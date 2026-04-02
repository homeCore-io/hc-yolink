use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::TemperatureUnit;
use crate::devices::DeviceKind;
use crate::homecore::HomecorePublisher;
use crate::yolink::{api::YolinkApi, types::{DeviceInfo, YolinkReport}};

// ---------------------------------------------------------------------------
// Device record — pairs the YoLink device info with its resolved kind
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Device {
    info: DeviceInfo,
    kind: DeviceKind,
    /// HomeCore device ID: "yolink_{yolink_device_id}"
    hc_id: String,
    retired: bool,
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    devices: Vec<Device>,
    /// YoLink device_id → index in `devices`, for O(1) lookup on MQTT events
    index: HashMap<String, usize>,
    yolink_api: Arc<YolinkApi>,
    publisher: HomecorePublisher,
    temp_unit: TemperatureUnit,
    poll_interval: Duration,
    /// Delay between successive per-device getState calls to avoid hub rate limits.
    poll_device_delay: Duration,
}

impl Bridge {
    pub fn new(
        raw: Vec<(DeviceInfo, DeviceKind)>,
        yolink_api: Arc<YolinkApi>,
        publisher: HomecorePublisher,
        temp_unit: TemperatureUnit,
        poll_interval_secs: u64,
        poll_device_delay_ms: u64,
    ) -> Self {
        let mut devices = Vec::with_capacity(raw.len());
        let mut index = HashMap::new();

        for (info, kind) in raw {
            let hc_id = format!("yolink_{}", info.device_id);
            index.insert(info.device_id.clone(), devices.len());
            devices.push(Device {
                info,
                kind,
                hc_id,
                retired: false,
            });
        }

        Self {
            devices,
            index,
            yolink_api,
            publisher,
            temp_unit,
            poll_interval: Duration::from_secs(poll_interval_secs),
            poll_device_delay: Duration::from_millis(poll_device_delay_ms),
        }
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    pub async fn run(
        mut self,
        mut yolink_rx: mpsc::Receiver<YolinkReport>,
        mut homecore_rx: mpsc::Receiver<(String, Value)>,
    ) -> Result<()> {
        // Startup poll — get fresh state for all devices immediately so HomeCore
        // has current attributes before the first periodic interval fires.
        info!("Bridge startup: polling {} devices for initial state", self.devices.len());
        self.poll_all_devices().await;

        let mut poll_timer = tokio::time::interval(self.poll_interval);
        // Skip the immediate first tick (we just polled above).
        poll_timer.tick().await;

        info!("Bridge event loop running ({} devices)", self.devices.len());

        loop {
            tokio::select! {
                // Real-time device report from YoLink MQTT
                Some(report) = yolink_rx.recv() => {
                    self.handle_yolink_report(report).await;
                }

                // Command from HomeCore (rule engine / user API)
                Some((hc_id, cmd)) = homecore_rx.recv() => {
                    self.handle_homecore_command(hc_id, cmd).await;
                }

                // Periodic true-up: state refresh + device name sync
                _ = poll_timer.tick() => {
                    self.poll_all_devices().await;
                    self.sync_inventory().await;
                }
            }
        }
    }

    async fn sync_inventory(&mut self) {
        let fresh = match self.yolink_api.get_device_list().await {
            Ok(list) => list,
            Err(e) => {
                warn!(error = %e, "Inventory sync: get_device_list failed");
                return;
            }
        };

        let mut seen = HashSet::new();

        for info in fresh {
            let kind = DeviceKind::from_yolink_type(&info.device_type);
            if !kind.is_supported() {
                continue;
            }

            let device_id = info.device_id.clone();
            seen.insert(device_id.clone());

            if let Some(&idx) = self.index.get(&device_id) {
                let (hc_id, old_name, needs_reregister) = {
                    let dev = &self.devices[idx];
                    (
                        dev.hc_id.clone(),
                        dev.info.name.clone(),
                        dev.info.name != info.name
                            || dev.kind.homecore_device_type() != kind.homecore_device_type(),
                    )
                };

                if needs_reregister {
                    info!(
                        hc_id = %hc_id,
                        old_name = %old_name,
                        new_name = %info.name,
                        "YoLink device metadata changed; re-registering with HomeCore"
                    );
                    if let Err(e) = self
                        .publisher
                        .register_device(&hc_id, &info.name, kind.homecore_device_type(), None)
                        .await
                    {
                        warn!(
                            hc_id = %hc_id,
                            error = %e,
                            "Inventory sync: failed to re-register device metadata"
                        );
                        continue;
                    }
                }

                let dev = &mut self.devices[idx];
                dev.info = info;
                dev.kind = kind;
                dev.retired = false;
                continue;
            }

            let hc_id = format!("yolink_{device_id}");
            info!(hc_id = %hc_id, name = %info.name, "New YoLink device discovered; registering");
            if let Err(e) = self
                .publisher
                .register_device(&hc_id, &info.name, kind.homecore_device_type(), None)
                .await
            {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: register_device failed");
                continue;
            }
            if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: subscribe_commands failed");
            }

            match self.yolink_api.get_device_state(&info).await {
                Ok(data) => {
                    let online = data["online"].as_bool().unwrap_or(true);
                    let _ = self.publisher.publish_availability(&hc_id, online).await;
                    if let Some(state) = kind.translate_state(&data, &self.temp_unit) {
                        let _ = self.publisher.publish_state(&hc_id, &state).await;
                    }
                }
                Err(e) => {
                    warn!(hc_id = %hc_id, error = %e, "Inventory sync: initial state fetch failed");
                    let _ = self.publisher.publish_availability(&hc_id, false).await;
                }
            }

            self.index.insert(device_id, self.devices.len());
            self.devices.push(Device {
                info,
                kind,
                hc_id,
                retired: false,
            });
        }

        let missing: Vec<(String, usize)> = self
            .index
            .iter()
            .filter_map(|(device_id, &idx)| {
                (!seen.contains(device_id.as_str())).then_some((device_id.clone(), idx))
            })
            .collect();

        for (device_id, idx) in missing {
            self.index.remove(&device_id);
            if self.devices[idx].retired {
                continue;
            }
            let hc_id = self.devices[idx].hc_id.clone();
            info!(hc_id = %hc_id, "YoLink device missing from inventory; unregistering");
            if let Err(e) = self.publisher.unregister_device(&hc_id).await {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: unregister_device failed");
                self.index.insert(device_id, idx);
                continue;
            }
            self.devices[idx].retired = true;
        }
    }

    // -----------------------------------------------------------------------
    // Handlers
    // -----------------------------------------------------------------------

    async fn handle_yolink_report(&self, report: YolinkReport) {
        let Some(dev) = self.find_by_yolink_id(&report.device_id) else {
            debug!(device_id = %report.device_id, "Report for unknown device, ignoring");
            return;
        };

        debug!(
            hc_id = %dev.hc_id,
            yolink_device_id = %report.device_id,
            event = %report.event,
            kind = ?dev.kind,
            raw = %report.data,
            "YoLink report received"
        );

        // Publish availability if present in the report
        if let Some(online) = report.data["online"].as_bool() {
            debug!(
                hc_id = %dev.hc_id,
                yolink_device_id = %report.device_id,
                event = %report.event,
                online,
                "Publishing availability from YoLink report"
            );
            if let Err(e) = self.publisher.publish_availability(&dev.hc_id, online).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
            }
        }

        // Translate and publish state as a partial update (merge-patch)
        if let Some(patch) = dev.kind.translate_state(&report.data, &self.temp_unit) {
            debug!(
                hc_id = %dev.hc_id,
                yolink_device_id = %report.device_id,
                event = %report.event,
                kind = ?dev.kind,
                patch = %patch,
                "Publishing state patch from YoLink report"
            );
            if let Err(e) = self.publisher.publish_state_partial(&dev.hc_id, &patch).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state partial");
            }
        } else {
            warn!(
                hc_id = %dev.hc_id,
                event = %report.event,
                kind  = ?dev.kind,
                raw   = %report.data,
                "MQTT report: translate_state returned None — raw report data logged above"
            );
        }
    }

    async fn handle_homecore_command(&self, hc_id: String, cmd: Value) {
        // HomeCore device IDs are "yolink_{yolink_device_id}"
        let yolink_id = hc_id.strip_prefix("yolink_").unwrap_or(&hc_id);

        let Some(dev) = self.find_by_yolink_id(yolink_id) else {
            warn!(hc_id = %hc_id, "Command for unknown device");
            return;
        };

        debug!(
            hc_id = %hc_id,
            yolink_device_id = %yolink_id,
            kind = ?dev.kind,
            command = %cmd,
            "HomeCore command received for YoLink device"
        );

        match dev.kind.translate_command(&cmd) {
            Ok((method_suffix, params)) => {
                debug!(
                    hc_id = %hc_id,
                    yolink_device_id = %yolink_id,
                    kind = ?dev.kind,
                    method_suffix,
                    params = %params,
                    "Translated HomeCore command to YoLink request"
                );
                // All current controllable types use setState
                if let Err(e) = self.yolink_api.set_device_state(&dev.info, params).await {
                    warn!(hc_id = %hc_id, error = %e, "YoLink command failed");
                } else {
                    debug!(hc_id = %hc_id, "Command sent to YoLink");
                }
            }
            Err(e) => {
                warn!(hc_id = %hc_id, error = %e, "Cannot translate HomeCore command");
            }
        }
    }

    async fn poll_all_devices(&self) {
        info!("Polling {} devices for state true-up", self.devices.len());

        for dev in &self.devices {
            if dev.retired {
                continue;
            }
            if !dev.kind.is_supported() {
                continue;
            }

            if !self.poll_device_delay.is_zero() {
                tokio::time::sleep(self.poll_device_delay).await;
            }

            match self.yolink_api.get_device_state(&dev.info).await {
                Ok(data) => {
                    debug!(
                        hc_id = %dev.hc_id,
                        yolink_device_id = %dev.info.device_id,
                        kind = ?dev.kind,
                        raw = %data,
                        "YoLink getState snapshot received"
                    );

                    // Publish availability
                    let online = data["online"].as_bool().unwrap_or(true);
                    debug!(
                        hc_id = %dev.hc_id,
                        yolink_device_id = %dev.info.device_id,
                        online,
                        "Publishing availability from YoLink getState snapshot"
                    );
                    let _ = self.publisher.publish_availability(&dev.hc_id, online).await;

                    // Publish full state (retained — this is a ground-truth refresh)
                    if let Some(state) = dev.kind.translate_state(&data, &self.temp_unit) {
                        debug!(
                            hc_id = %dev.hc_id,
                            yolink_device_id = %dev.info.device_id,
                            kind = ?dev.kind,
                            state = %state,
                            "Publishing full state snapshot from YoLink getState"
                        );
                        if let Err(e) = self.publisher.publish_state(&dev.hc_id, &state).await {
                            warn!(hc_id = %dev.hc_id, error = %e, "Poll: failed to publish state");
                        }
                    } else {
                        warn!(
                            hc_id = %dev.hc_id,
                            kind  = ?dev.kind,
                            raw   = %data,
                            "Poll: translate_state returned None — raw getState response logged above"
                        );
                    }
                }
                Err(e) => {
                    warn!(hc_id = %dev.hc_id, error = %e, "Poll: getState failed");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lookup helpers
    // -----------------------------------------------------------------------

    fn find_by_yolink_id(&self, yolink_id: &str) -> Option<&Device> {
        self.index.get(yolink_id).map(|&i| &self.devices[i])
    }
}
