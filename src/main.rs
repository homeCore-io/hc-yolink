mod auth;
mod bridge;
mod config;
mod devices;
mod homecore;
mod yolink;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use config::{Config, Endpoints};
use devices::DeviceKind;
use yolink::{api::YolinkApi, mqtt::YolinkMqtt, types::YolinkReport};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Logging — respects RUST_LOG; defaults to info for this crate
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hc_yolink=info".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-yolink plugin");
        match try_start(&cfg).await {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(
                        error = %e,
                        attempt,
                        "Startup failed; retrying in {RETRY_DELAY_SECS} s"
                    );
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Startup — everything that can fail (retried up to MAX_ATTEMPTS times)
// ---------------------------------------------------------------------------

async fn try_start(cfg: &Config) -> Result<()> {
    // Resolve mode-specific endpoints from config
    let ep = Endpoints::from_config(&cfg.yolink)?;

    // --- Auth -----------------------------------------------------------------
    let tokens = auth::TokenManager::new(
        ep.token_url.clone(),
        ep.client_id.clone(),
        ep.client_secret.clone(),
    );
    tokens.init().await?;
    tokens.clone().spawn_refresh_task();
    info!("YoLink authentication successful");

    // --- YoLink API client ----------------------------------------------------
    let yolink_api = Arc::new(YolinkApi::new(ep.api_base_url.clone(), tokens.clone()));

    // --- Build MQTT topic prefix (mode-specific) ------------------------------
    // Cloud: yl-home/{home_id}   — home_id fetched from the API
    // Local: ylsubnet/{net_id}   — net_id comes directly from config credentials
    let topic_prefix = match cfg.yolink.mode {
        config::Mode::Cloud => {
            let home_id = yolink_api.get_home_id().await?;
            info!(home_id = %home_id, "YoLink home ID obtained");
            format!("yl-home/{home_id}")
        }
        config::Mode::Local => {
            let net_id = &ep.net_id;
            info!(net_id = %net_id, "Using local hub Net ID for MQTT topics");
            format!("ylsubnet/{net_id}")
        }
    };

    // --- YoLink MQTT event stream ---------------------------------------------
    let (yolink_tx, yolink_rx) = mpsc::channel::<YolinkReport>(256);
    let yl_mqtt = YolinkMqtt::new(ep.mqtt_host.clone(), ep.mqtt_port, tokens.clone());
    tokio::spawn(yl_mqtt.run(topic_prefix, yolink_tx));

    // --- HomeCore MQTT --------------------------------------------------------
    let hc_client = homecore::HomecoreClient::connect(&cfg.homecore).await?;
    let publisher = hc_client.publisher();
    let (hc_tx, hc_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // --- Device discovery -----------------------------------------------------
    let raw_devices = yolink_api.get_device_list().await?;
    info!(count = raw_devices.len(), "Discovered YoLink devices");

    let mut bridged_devices = Vec::new();

    for info in raw_devices {
        let kind = DeviceKind::from_yolink_type(&info.device_type);

        if !kind.is_supported() {
            info!(
                device_id = %info.device_id,
                device_type = %info.device_type,
                "Skipping unsupported device type"
            );
            continue;
        }

        let hc_id = format!("yolink_{}", info.device_id);
        let hc_type = kind.homecore_device_type();

        // Register with HomeCore
        publisher
            .register_device(&hc_id, &info.name, hc_type, None)
            .await?;

        // Subscribe to commands for this device
        publisher.subscribe_commands(&hc_id).await?;

        // Fetch and publish initial state
        match yolink_api.get_device_state(&info).await {
            Ok(data) => {
                let online = data["online"].as_bool().unwrap_or(true);
                publisher.publish_availability(&hc_id, online).await?;

                if let Some(state) =
                    kind.translate_state(&data, &cfg.yolink.temperature_unit)
                {
                    publisher.publish_state(&hc_id, &state).await?;
                }
            }
            Err(e) => {
                // Non-fatal: mark offline, continue with other devices
                tracing::warn!(
                    device_id = %hc_id,
                    error = %e,
                    "Could not fetch initial state; marking offline"
                );
                publisher.publish_availability(&hc_id, false).await?;
            }
        }

        bridged_devices.push((info, kind));
    }

    info!(
        registered = bridged_devices.len(),
        "All devices registered with HomeCore"
    );

    // --- Start HomeCore event loop (spawned so bridge can run on current task) --
    tokio::spawn(hc_client.run(hc_tx));

    // --- Bridge event loop (runs until error / shutdown) ----------------------
    let bridge = bridge::Bridge::new(
        bridged_devices,
        yolink_api,
        publisher,
        cfg.yolink.temperature_unit.clone(),
        cfg.yolink.poll_interval_secs,
    );

    bridge.run(yolink_rx, hc_rx).await
}
