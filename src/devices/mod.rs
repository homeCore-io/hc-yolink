use anyhow::{bail, Result};
use serde_json::Value;

use crate::config::TemperatureUnit;

// ---------------------------------------------------------------------------
// DeviceKind — maps YoLink device types to HomeCore device types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceKind {
    /// Smart outlet (relay + optional power monitoring)
    Outlet,
    /// Same hardware class as Outlet
    SmartPlug,
    /// In-wall switch
    Switch,
    /// Multi-outlet power strip
    MultiOutlet,
    /// Door/window contact sensor
    DoorSensor,
    /// PIR motion sensor
    MotionSensor,
    /// Water leak sensor
    LeakSensor,
    /// Temperature + humidity sensor
    THSensor,
    /// Vibration / shock sensor
    VibrationSensor,
    /// Smart lock
    Lock,
    /// Smart lock v2 variant
    LockV2,
    /// Siren / alarm
    Siren,
    /// Hub itself — not bridged as a device
    Hub,
    /// Anything not yet recognised
    Unknown(String),
}

impl DeviceKind {
    pub fn from_yolink_type(s: &str) -> Self {
        match s {
            "Outlet"          => Self::Outlet,
            "SmartPlug"       => Self::SmartPlug,
            "Switch"          => Self::Switch,
            "MultiOutlet"     => Self::MultiOutlet,
            "DoorSensor"      => Self::DoorSensor,
            "MotionSensor"    => Self::MotionSensor,
            "LeakSensor"      => Self::LeakSensor,
            "THSensor"        => Self::THSensor,
            "VibrationSensor" => Self::VibrationSensor,
            "Lock"            => Self::Lock,
            "LockV2"          => Self::LockV2,
            "Siren"           => Self::Siren,
            "Hub"             => Self::Hub,
            other             => Self::Unknown(other.to_string()),
        }
    }

    /// HomeCore device_type string (matched against the device-types catalog).
    pub fn homecore_device_type(&self) -> &str {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet => "switch",
            Self::DoorSensor | Self::MotionSensor | Self::LeakSensor | Self::VibrationSensor => {
                "binary_sensor"
            }
            Self::THSensor => "temperature_sensor",
            Self::Lock | Self::LockV2 => "lock",
            Self::Siren => "switch",
            Self::Hub | Self::Unknown(_) => "unknown",
        }
    }

    /// Whether this device kind should be registered and bridged.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Hub | Self::Unknown(_))
    }

    // -----------------------------------------------------------------------
    // State translation: YoLink report/state data → HomeCore state JSON
    // -----------------------------------------------------------------------

    /// Translate device data from a YoLink report or getState response into a
    /// HomeCore-compatible state JSON object.
    ///
    /// Returns `None` if the data cannot be translated (e.g. unrecognised
    /// payload shape).
    pub fn translate_state(&self, data: &Value, temp_unit: &TemperatureUnit) -> Option<Value> {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch => translate_switch(data),
            Self::MultiOutlet => translate_multi_outlet(data),
            Self::DoorSensor => translate_door_sensor(data),
            Self::MotionSensor => translate_motion_sensor(data),
            Self::LeakSensor => translate_leak_sensor(data),
            Self::THSensor => translate_th_sensor(data, temp_unit),
            Self::VibrationSensor => translate_vibration_sensor(data),
            Self::Lock | Self::LockV2 => translate_lock(data),
            Self::Siren => translate_siren(data),
            Self::Hub | Self::Unknown(_) => None,
        }
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore cmd JSON → (YoLink method suffix, params)
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into a YoLink API method suffix
    /// (appended to the device type, e.g. "setState") and its params object.
    pub fn translate_command(&self, cmd: &Value) -> Result<(&'static str, Value)> {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet => {
                let on = cmd["on"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'on' field"))?;
                // YoLink uses "open" for on and "close" for off
                Ok(("setState", serde_json::json!({ "state": if on { "open" } else { "close" } })))
            }
            Self::Siren => {
                let on = cmd["on"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'on' field"))?;
                Ok(("setState", serde_json::json!({ "state": if on { "open" } else { "close" } })))
            }
            Self::Lock | Self::LockV2 => {
                let locked = cmd["locked"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'locked' field"))?;
                Ok((
                    "setState",
                    serde_json::json!({ "state": if locked { "lock" } else { "unlock" } }),
                ))
            }
            Self::DoorSensor
            | Self::MotionSensor
            | Self::LeakSensor
            | Self::THSensor
            | Self::VibrationSensor => {
                bail!("{:?} is read-only; commands are not supported", self)
            }
            Self::Hub | Self::Unknown(_) => {
                bail!("{:?} is not a supported device kind", self)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-type translation helpers
// ---------------------------------------------------------------------------

/// YoLink uses "open" for on/active and "close" for off/inactive in relay devices.
fn relay_is_on(state_str: &str) -> bool {
    matches!(state_str, "open" | "on")
}

/// Extract `data["state"]` as a string, or fall back to the top-level `data`
/// string (some devices nest, some don't).
fn state_str(data: &Value) -> Option<&str> {
    data["state"].as_str()
}

/// Battery level as a number (0–4 in YoLink, 4 = full).
fn battery(data: &Value) -> Option<Value> {
    // Prefer data["state"]["battery"], then data["battery"]
    let b = data["state"]["battery"]
        .as_u64()
        .or_else(|| data["battery"].as_u64())?;
    Some(Value::Number(b.into()))
}

fn translate_switch(data: &Value) -> Option<Value> {
    let on = relay_is_on(state_str(data)?);
    let mut out = serde_json::json!({ "on": on });

    // Power monitoring fields (Outlet with power meter)
    if let Some(w) = data["power"].as_f64().or_else(|| data["watt"].as_f64()) {
        out["power_w"] = serde_json::json!(round1(w));
    }
    if let Some(kwh) = data["electricity"].as_f64() {
        out["energy_kwh"] = serde_json::json!(round3(kwh));
    }

    Some(out)
}

fn translate_multi_outlet(data: &Value) -> Option<Value> {
    // MultiOutlet state may be an array of per-outlet states.
    // For now expose the overall state as a single on/off.
    translate_switch(data)
}

fn translate_door_sensor(data: &Value) -> Option<Value> {
    // state == "open" → door open, "close" or "closed" → closed
    let open = data["state"].as_str().map(|s| s == "open")?;
    let mut out = serde_json::json!({ "open": open });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_motion_sensor(data: &Value) -> Option<Value> {
    // YoLink MotionSensor: `alarm: true` = motion detected
    let motion = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .unwrap_or(false);
    let mut out = serde_json::json!({ "motion": motion });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_leak_sensor(data: &Value) -> Option<Value> {
    // `alarm: true` or state == "alert" → leak detected
    let leak = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .or_else(|| data["state"].as_str().map(|s| s == "alert"))
        .unwrap_or(false);
    let mut out = serde_json::json!({ "leak": leak });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_th_sensor(data: &Value, unit: &TemperatureUnit) -> Option<Value> {
    // Temperature value (raw)
    let raw_temp = data["temperature"]
        .as_f64()
        .or_else(|| data["state"]["temperature"].as_f64())?;

    let humidity = data["humidity"]
        .as_f64()
        .or_else(|| data["state"]["humidity"].as_f64())?;

    // Device reports its own unit in "tempUnit": "℃" or "℉"
    // Check both data-level and state-level.
    let raw_unit = data["tempUnit"]
        .as_str()
        .or_else(|| data["state"]["tempUnit"].as_str())
        .unwrap_or("℃");

    // Determine raw unit and convert to the configured output unit.
    let temp_out = if raw_unit.contains('C') || raw_unit == "℃" {
        unit.from_celsius(raw_temp)
    } else {
        // Assume Fahrenheit
        unit.from_fahrenheit(raw_temp)
    };

    let mut out = serde_json::json!({
        "temperature":      round1(temp_out),
        "temperature_unit": unit.label(),
        "humidity_pct":     round1(humidity),
    });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_vibration_sensor(data: &Value) -> Option<Value> {
    let vibration = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .unwrap_or(false);
    let mut out = serde_json::json!({ "vibration": vibration });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_lock(data: &Value) -> Option<Value> {
    let locked = data["state"].as_str().map(|s| s == "locked")?;
    let mut out = serde_json::json!({ "locked": locked });
    if let Some(b) = battery(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_siren(data: &Value) -> Option<Value> {
    let on = state_str(data).map(relay_is_on).unwrap_or(false);
    let alarm = data["alarm"].as_bool().unwrap_or(on);
    Some(serde_json::json!({ "on": on, "alarm": alarm }))
}

// ---------------------------------------------------------------------------
// Rounding helpers
// ---------------------------------------------------------------------------

/// Round to 1 decimal place.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Round to 3 decimal places.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outlet_on() {
        let data = json!({ "state": "open", "power": 12.5, "electricity": 0.031 });
        let state = DeviceKind::Outlet.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["on"], json!(true));
        assert_eq!(state["power_w"], json!(12.5));
        assert_eq!(state["energy_kwh"], json!(0.031));
    }

    #[test]
    fn outlet_off() {
        let data = json!({ "state": "close" });
        let state = DeviceKind::Outlet.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["on"], json!(false));
    }

    #[test]
    fn door_open() {
        let data = json!({ "state": "open", "battery": 3 });
        let state = DeviceKind::DoorSensor.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["open"], json!(true));
        assert_eq!(state["battery"], json!(3));
    }

    #[test]
    fn door_closed() {
        let data = json!({ "state": "close", "battery": 4 });
        let state = DeviceKind::DoorSensor.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["open"], json!(false));
    }

    #[test]
    fn motion_detected() {
        let data = json!({ "alarm": true, "battery": 2 });
        let state = DeviceKind::MotionSensor.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["motion"], json!(true));
    }

    #[test]
    fn th_celsius_device_to_fahrenheit_output() {
        let data = json!({ "temperature": 22.5, "humidity": 65.2, "tempUnit": "℃", "battery": 4 });
        let state = DeviceKind::THSensor.translate_state(&data, &TemperatureUnit::F).unwrap();
        // 22.5 °C → 72.5 °F
        assert_eq!(state["temperature"], json!(72.5));
        assert_eq!(state["temperature_unit"], json!("F"));
        assert_eq!(state["humidity_pct"], json!(65.2));
    }

    #[test]
    fn th_celsius_device_to_celsius_output() {
        let data = json!({ "temperature": 22.5, "humidity": 65.2, "tempUnit": "℃", "battery": 4 });
        let state = DeviceKind::THSensor.translate_state(&data, &TemperatureUnit::C).unwrap();
        assert_eq!(state["temperature"], json!(22.5));
        assert_eq!(state["temperature_unit"], json!("C"));
    }

    #[test]
    fn th_fahrenheit_device_to_fahrenheit_output() {
        let data = json!({ "temperature": 72.5, "humidity": 50.0, "tempUnit": "℉" });
        let state = DeviceKind::THSensor.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["temperature"], json!(72.5));
    }

    #[test]
    fn th_fahrenheit_device_to_celsius_output() {
        let data = json!({ "temperature": 72.5, "humidity": 50.0, "tempUnit": "℉" });
        let state = DeviceKind::THSensor.translate_state(&data, &TemperatureUnit::C).unwrap();
        // 72.5 °F → 22.5 °C
        assert_eq!(state["temperature"], json!(22.5));
    }

    #[test]
    fn lock_locked() {
        let data = json!({ "state": "locked", "battery": 3 });
        let state = DeviceKind::Lock.translate_state(&data, &TemperatureUnit::F).unwrap();
        assert_eq!(state["locked"], json!(true));
    }

    #[test]
    fn switch_cmd_on() {
        let cmd = json!({ "on": true });
        let (method, params) = DeviceKind::Switch.translate_command(&cmd).unwrap();
        assert_eq!(method, "setState");
        assert_eq!(params["state"], json!("open"));
    }

    #[test]
    fn switch_cmd_off() {
        let cmd = json!({ "on": false });
        let (_method, params) = DeviceKind::Switch.translate_command(&cmd).unwrap();
        assert_eq!(params["state"], json!("close"));
    }

    #[test]
    fn sensor_cmd_rejected() {
        let cmd = json!({});
        assert!(DeviceKind::DoorSensor.translate_command(&cmd).is_err());
    }
}
