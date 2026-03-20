use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// BDDP — Basic Downlink Data Packet (client → YoLink)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Bddp<'a> {
    /// Current Unix timestamp in milliseconds
    pub time: u64,
    /// JSON-RPC method name, e.g. "Outlet.setState"
    pub method: &'a str,
    /// Caller-supplied message ID for correlation (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msgid: Option<String>,
    /// Target device ID (required for device-specific methods)
    #[serde(rename = "targetDevice", skip_serializing_if = "Option::is_none")]
    pub target_device: Option<&'a str>,
    /// Per-device auth token obtained from the device list
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<&'a str>,
    /// Method-specific parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// BUDP — Basic Uplink Data Packet (YoLink → client)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Budp {
    /// "000000" = success; any other value is an error
    pub code: String,
    /// Human-readable status description
    pub desc: Option<String>,
    /// Method-specific response payload
    pub data: Option<Value>,
}

impl Budp {
    /// Unwrap the response data, returning an error if `code != "000000"`.
    pub fn into_data(self) -> anyhow::Result<Value> {
        if self.code != "000000" {
            anyhow::bail!(
                "YoLink API error {} — {}",
                self.code,
                self.desc.unwrap_or_default()
            );
        }
        Ok(self.data.unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// DeviceInfo — entry in the device list response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceInfo {
    #[serde(rename = "deviceId")]
    pub device_id: String,

    pub name: String,

    /// YoLink device type string, e.g. "Outlet", "DoorSensor", "THSensor"
    #[serde(rename = "type")]
    pub device_type: String,

    /// Per-device network token; required for all device-specific API calls
    pub token: String,

    /// Hub / gateway this device is paired to
    #[serde(rename = "parentDeviceId")]
    #[allow(dead_code)]
    pub parent_device_id: Option<String>,
}

// ---------------------------------------------------------------------------
// YolinkReport — parsed real-time event from the MQTT broker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct YolinkReport {
    /// YoLink device ID
    pub device_id: String,
    /// Event type: "StatusChange", "Alert", "Report", etc.
    pub event: String,
    /// Raw event payload (device-type-specific)
    pub data: Value,
}
