use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
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
            devices.push(Device { info, kind, hc_id });
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
                    // Name sync runs after state poll; changes applied before next tick.
                    let name_changes = self.detect_name_changes().await;
                    for (idx, new_name) in name_changes {
                        let dev = &mut self.devices[idx];
                        let old_name = std::mem::replace(&mut dev.info.name, new_name.clone());
                        info!(
                            hc_id      = %dev.hc_id,
                            old_name   = %old_name,
                            new_name   = %new_name,
                            "Device name changed at source; re-registering with HomeCore"
                        );
                        // Re-registration triggers the upsert+DeviceNameChanged path in core.
                        let _ = self.publisher
                            .register_device(
                                &dev.hc_id.clone(),
                                &new_name,
                                dev.kind.homecore_device_type(),
                                None,
                            )
                            .await;
                    }
                }
            }
        }
    }

    /// Fetch the current device list from YoLink and return `(index, new_name)`
    /// for every device whose name differs from what is stored locally.
    async fn detect_name_changes(&self) -> Vec<(usize, String)> {
        let fresh = match self.yolink_api.get_device_list().await {
            Ok(list) => list,
            Err(e) => {
                warn!(error = %e, "Name sync: get_device_list failed");
                return vec![];
            }
        };

        // Build a lookup of YoLink device_id → current name from the fresh list.
        let name_map: std::collections::HashMap<&str, &str> = fresh
            .iter()
            .map(|d| (d.device_id.as_str(), d.name.as_str()))
            .collect();

        self.devices
            .iter()
            .enumerate()
            .filter_map(|(i, dev)| {
                let new_name = *name_map.get(dev.info.device_id.as_str())?;
                if new_name != dev.info.name.as_str() {
                    Some((i, new_name.to_string()))
                } else {
                    None
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Handlers
    // -----------------------------------------------------------------------

    async fn handle_yolink_report(&self, report: YolinkReport) {
        debug!(
            device_id = %report.device_id,
            event = %report.event,
            "YoLink report received"
        );

        let Some(dev) = self.find_by_yolink_id(&report.device_id) else {
            debug!(device_id = %report.device_id, "Report for unknown device, ignoring");
            return;
        };

        // Publish availability if present in the report
        if let Some(online) = report.data["online"].as_bool() {
            if let Err(e) = self.publisher.publish_availability(&dev.hc_id, online).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
            }
        }

        // Translate and publish state as a partial update (merge-patch)
        if let Some(patch) = dev.kind.translate_state(&report.data, &self.temp_unit) {
            if let Err(e) = self.publisher.publish_state_partial(&dev.hc_id, &patch).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state partial");
            }
        }
    }

    async fn handle_homecore_command(&self, hc_id: String, cmd: Value) {
        // HomeCore device IDs are "yolink_{yolink_device_id}"
        let yolink_id = hc_id.strip_prefix("yolink_").unwrap_or(&hc_id);

        let Some(dev) = self.find_by_yolink_id(yolink_id) else {
            warn!(hc_id = %hc_id, "Command for unknown device");
            return;
        };

        match dev.kind.translate_command(&cmd) {
            Ok((_method_suffix, params)) => {
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
            if !dev.kind.is_supported() {
                continue;
            }

            if !self.poll_device_delay.is_zero() {
                tokio::time::sleep(self.poll_device_delay).await;
            }

            match self.yolink_api.get_device_state(&dev.info).await {
                Ok(data) => {
                    // Publish availability
                    let online = data["online"].as_bool().unwrap_or(true);
                    let _ = self.publisher.publish_availability(&dev.hc_id, online).await;

                    // Publish full state (retained — this is a ground-truth refresh)
                    if let Some(state) = dev.kind.translate_state(&data, &self.temp_unit) {
                        if let Err(e) = self.publisher.publish_state(&dev.hc_id, &state).await {
                            warn!(hc_id = %dev.hc_id, error = %e, "Poll: failed to publish state");
                        }
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
