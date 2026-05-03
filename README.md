# hc-yolink

[![CI](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-yolink/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore-io.github.io/ci-glance/)

Bridges YoLink smart home devices into HomeCore via the YS1606 local hub (LAN) or cloud MQTT.

## Supported device types

| YoLink Device | HomeCore device_type |
|---|---|
| DoorSensor | `contact_sensor` |
| MotionSensor | `motion_sensor` |
| LeakSensor | `water_sensor` |
| VibrationSensor | `vibration_sensor` |
| THSensor | `temperature_sensor` |
| Outlet / SmartPlug / Switch | `switch` |
| MultiOutlet | `switch` (per-outlet) |
| Lock (v1/v2) | `lock` |
| Siren | `switch` |

## Setup

1. Copy `config/config.toml.example` to `config/config.toml`
2. Set mode (`"local"` for YS1606 hub or `"cloud"` for cloud MQTT)
3. Fill in credentials (`client_id`, `client_secret`, `net_id` for local; `uaid`, `secret_key` for cloud)
4. Add a `[[plugins]]` entry in `homecore.toml`

## Configuration highlights

- `mode` — `"local"` (recommended, requires YS1606) or `"cloud"`
- `hub_ip` — YS1606 hub IP (local mode only)
- `poll_interval_secs` — background state refresh interval
- `temperature_unit` — `"c"` or `"f"`
