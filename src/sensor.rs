use crate::types::*;
use std::sync::atomic::Ordering;

pub mod sysfs {
    use std::fs::File;
    use std::io::{Read, Write};

    pub fn read_int(path: &str) -> i32 {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(_) => return -1,
        };
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return -1;
        }
        buf.trim().parse().unwrap_or(-1)
    }

    pub fn read_string(path: &str) -> Option<String> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(_) => return None,
        };
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return None;
        }
        Some(buf.trim().to_string())
    }

    pub fn write_int(path: &str, value: i32) -> bool {
        let mut f = match File::create(path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        write!(f, "{}", value).is_ok()
    }
}

impl Sensor {
    pub fn poll(&self) -> i32 {
        let v = sysfs::read_int(&self.path);
        if v >= 0 {
            use std::sync::atomic::Ordering;
            self.last_temp_mc.store(v, Ordering::Relaxed);
        }
        v
    }
}

pub struct EngineDiscovery;

impl EngineDiscovery {
    pub fn sensor_init(sensors: &mut Vec<Sensor>) {
        for z in 0..MI_MAX_SENSORS as i32 {
            let p = format!("/sys/class/thermal/thermal_zone{}/type", z);
            let name = match sysfs::read_string(&p) {
                Some(n) => n,
                None => continue,
            };
            if sensors.len() >= MI_MAX_SENSORS {
                break;
            }
            let mut s = Sensor::new(&name);
            s.zone_id = z;
            s.type_ = SensorType::ThermalZone;
            s.path = format!("/sys/class/thermal/thermal_zone{}/temp", z);
            s.poll_ms = MI_POLL_INTERVAL_MS_DEFAULT;
            log_info!("discovered thermal_zone{} type={}", z, name);
            sensors.push(s);
        }

        struct ExtraSensor {
            name: &'static str,
            path: &'static str,
        }
        const EXTRA: &[ExtraSensor] = &[
            ExtraSensor { name: "ambient_sensor_temp", path: "/sys/class/thermal/thermal_message/ambient_sensor_temp" },
            ExtraSensor { name: "connector_temp", path: "/sys/class/qcom-battery/connector_temp" },
            ExtraSensor { name: "BAT_SOC", path: "/sys/class/power_supply/battery/capacity" },
            ExtraSensor { name: "battery_current", path: "/sys/class/power_supply/battery/current_now" },
            ExtraSensor { name: "battery_temp", path: "/sys/class/power_supply/battery/temp" },
            ExtraSensor { name: "battery_voltage", path: "/sys/class/power_supply/battery/voltage_now" },
        ];

        for es in EXTRA {
            if sensors.len() >= MI_MAX_SENSORS {
                break;
            }
            let mut s = Sensor::new(es.name);
            s.path = es.path.to_string();
            s.type_ = SensorType::ThermalZone;
            s.poll_ms = MI_POLL_INTERVAL_MS_DEFAULT;
            let init_v = sysfs::read_int(&es.path);
            if init_v >= 0 {
                s.last_temp_mc.store(init_v, Ordering::Relaxed);
            }
            log_info!("discovered extra sensor {} initial={}", es.name, init_v);
            sensors.push(s);
        }
    }

    pub fn scan_cooling_devices() -> Vec<CoolingDevice> {
        let mut cdevs = Vec::new();
        let dir = match std::fs::read_dir("/sys/class/thermal") {
            Ok(d) => d,
            Err(_) => return cdevs,
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if !name_str.starts_with("cooling_device") {
                continue;
            }
            if cdevs.len() >= MI_MAX_CDEV {
                break;
            }
            let base = format!("/sys/class/thermal/{}", name_str);
            let mut cdev = CoolingDevice {
                name: name_str,
                path: base.clone(),
                max_state: 0,
                cur_state: 0,
            };
            let type_path = format!("{}/type", base);
            if let Some(tname) = sysfs::read_string(&type_path) {
                cdev.name = tname;
            }
            cdev.max_state = sysfs::read_int(&format!("{}/max_state", base));
            cdev.cur_state = sysfs::read_int(&format!("{}/cur_state", base));
            log_info!("cdev {} max={} cur={}", cdev.name, cdev.max_state, cdev.cur_state);
            cdevs.push(cdev);
        }
        cdevs
    }

    pub fn vsns_init(sensors: &mut Vec<Sensor>) -> Vec<usize> {
        let mut virtual_sensors = Vec::new();
        const VSN_PATHS: &[&str] = &[
            "/sys/class/thermal/thermal_message/board_sensor_temp",
            "/sys/class/thermal/thermal_message/board_sensor_second_temp",
            "/sys/class/thermal/thermal_message/board_sensor_charge_temp",
            "/sys/class/thermal/thermal_message/board_sensor_other_temp",
            "/sys/class/thermal/thermal_message/ambient_sensor",
            "/sys/class/thermal/thermal_message/charger_temp",
            "/sys/class/thermal/thermal_message/display_therm_temp",
            "/sys/class/thermal/thermal_message/dynamic_tj",
        ];
        for vp in VSN_PATHS {
            if virtual_sensors.len() >= MI_MAX_VSNS || sensors.len() >= MI_MAX_SENSORS {
                break;
            }
            let base = match vp.rsplit('/').next() {
                Some(b) => b,
                None => "vsn",
            };
            let mut s = Sensor::new(base);
            s.path = vp.to_string();
            s.type_ = SensorType::Virtual;
            s.poll_ms = MI_POLL_INTERVAL_MS_DEFAULT;
            virtual_sensors.push(sensors.len());
            sensors.push(s);
        }
        virtual_sensors
    }

    pub fn formula_init() {}

    pub fn find_sensor(sensors: &[Sensor], virtual_sensors: &[usize], name: &str) -> Option<usize> {
        sensors.iter().position(|s| s.name == name)
            .or_else(|| {
                virtual_sensors.iter().copied().find(|&idx| sensors[idx].name == name)
            })
    }

    pub fn read_sconfig_idx() -> i32 {
        sysfs::read_int("/sys/class/thermal/thermal_message/sconfig")
    }
}
