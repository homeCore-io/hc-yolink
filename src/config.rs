use anyhow::{bail, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homecore: HomecoreConfig,
    pub yolink: YolinkConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config file {path}: {e}"))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        match self.yolink.mode {
            Mode::Cloud if self.yolink.cloud.is_none() => {
                bail!("[yolink.cloud] section is required when mode = \"cloud\"");
            }
            Mode::Local if self.yolink.local.is_none() => {
                bail!("[yolink.local] section is required when mode = \"local\"");
            }
            _ => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HomeCore broker connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct HomecoreConfig {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    pub password: String,
}

fn default_broker_host() -> String { "127.0.0.1".into() }
fn default_broker_port() -> u16    { 1883 }
fn default_plugin_id()   -> String { "plugin.yolink".into() }

// ---------------------------------------------------------------------------
// YoLink settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Cloud,
    Local,
}

/// Display unit for temperatures reported to HomeCore.
///
/// YoLink devices report their own unit in the `tempUnit` field; this plugin
/// always converts to the configured unit before publishing.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub enum TemperatureUnit {
    /// Degrees Celsius
    #[serde(rename = "C")]
    C,
    /// Degrees Fahrenheit (default)
    #[serde(rename = "F")]
    #[default]
    F,
}

impl TemperatureUnit {
    /// Convert a Celsius value to the target unit.
    pub fn from_celsius(&self, c: f64) -> f64 {
        match self {
            TemperatureUnit::C => c,
            TemperatureUnit::F => c * 9.0 / 5.0 + 32.0,
        }
    }

    /// Convert a Fahrenheit value to the target unit.
    pub fn from_fahrenheit(&self, f: f64) -> f64 {
        match self {
            TemperatureUnit::F => f,
            TemperatureUnit::C => (f - 32.0) * 5.0 / 9.0,
        }
    }

    /// Short label used in published state JSON, e.g. `"F"` or `"C"`.
    pub fn label(&self) -> &'static str {
        match self {
            TemperatureUnit::C => "C",
            TemperatureUnit::F => "F",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct YolinkConfig {
    pub mode: Mode,

    /// How often to poll all devices for a full state refresh (seconds).
    /// MQTT events are the primary delivery mechanism; this is a safety true-up.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Delay between individual device polls (milliseconds).
    /// Prevents 000201 "Cannot connect to device" rate-limit errors from the hub.
    #[serde(default = "default_poll_device_delay_ms")]
    pub poll_device_delay_ms: u64,

    /// Delay before starting the initial background state fetch (seconds).
    /// Gives the YoLink MQTT connection time to stabilize before adding HTTP
    /// load to the hub.  Set to 0 to disable (rely on MQTT reports + periodic poll).
    #[serde(default = "default_initial_fetch_delay_secs")]
    pub initial_fetch_delay_secs: u64,

    /// Unit used when publishing temperature values to HomeCore.
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,

    pub cloud: Option<CloudConfig>,
    pub local: Option<LocalConfig>,
}

fn default_poll_interval() -> u64 { 3600 }
fn default_poll_device_delay_ms() -> u64 { 1000 }
fn default_initial_fetch_delay_secs() -> u64 { 10 }

// ---------------------------------------------------------------------------
// Cloud mode (YS1603 / YS1605)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CloudConfig {
    /// User Access Credential ID (from YoLink App → Account → Personal Access Credentials)
    pub uaid: String,
    /// UAC secret key
    pub secret_key: String,

    #[serde(default = "default_cloud_api_url")]
    pub api_url: String,
    #[serde(default = "default_cloud_mqtt_host")]
    pub mqtt_host: String,
    #[serde(default = "default_cloud_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_cloud_api_url()  -> String { "https://api.yosmart.com".into() }
fn default_cloud_mqtt_host() -> String { "mqtt.api.yosmart.com".into() }
fn default_cloud_mqtt_port() -> u16   { 8003 }

// ---------------------------------------------------------------------------
// Local mode (YS1606 Local Hub only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct LocalConfig {
    /// Local IP address (or hostname) of the YS1606 hub on the LAN
    pub hub_ip: String,

    /// Client ID from YoLink App → Local Hub → Local Network → Integrations → Local API
    pub client_id: String,
    /// Client secret from the same screen
    pub client_secret: String,
    /// Network ID from the same credentials screen — required for local hub access
    pub net_id: String,

    /// HTTP port of the local hub's API server
    #[serde(default = "default_local_api_port")]
    pub api_port: u16,

    /// MQTT port of the local hub's broker
    #[serde(default = "default_local_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_local_api_port()  -> u16 { 1080 }
fn default_local_mqtt_port() -> u16 { 18080 }

// ---------------------------------------------------------------------------
// Resolved endpoint bundle (computed once in main, passed around)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub token_url: String,
    pub api_base_url: String,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub client_id: String,
    pub client_secret: String,
    /// Local hub Net ID (local mode only; empty string for cloud mode).
    /// Used to build the MQTT topic prefix: `ylsubnet/{net_id}/+/report`
    pub net_id: String,
}

impl Endpoints {
    pub fn from_config(cfg: &YolinkConfig) -> Result<Self> {
        match cfg.mode {
            Mode::Cloud => {
                let c = cfg.cloud.as_ref().unwrap();
                Ok(Self {
                    token_url:      format!("{}/open/yolink/token", c.api_url),
                    api_base_url:   c.api_url.clone(),
                    mqtt_host:      c.mqtt_host.clone(),
                    mqtt_port:      c.mqtt_port,
                    client_id:      c.uaid.clone(),
                    client_secret:  c.secret_key.clone(),
                    net_id:         String::new(),
                })
            }
            Mode::Local => {
                let l = cfg.local.as_ref().unwrap();
                let base = format!("http://{}:{}", l.hub_ip, l.api_port);
                Ok(Self {
                    token_url:      format!("{}/open/yolink/token", base),
                    api_base_url:   base,
                    mqtt_host:      l.hub_ip.clone(),
                    mqtt_port:      l.mqtt_port,
                    client_id:      l.client_id.clone(),
                    client_secret:  l.client_secret.clone(),
                    net_id:         l.net_id.clone(),
                })
            }
        }
    }
}
