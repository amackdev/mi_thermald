use std::sync::atomic::AtomicI32;

pub const MI_THERMALD_VERSION_STRING: &str = "thermald";
pub const MI_THERMALD_GLOBAL_MODE_FILE: &str = "/data/vendor/thermal/thermal-global-mode";
pub const MI_THERMALD_DUMP_FILE: &str = "/data/vendor/thermal/thermal.dump";
pub const MI_THERMALD_LAST_DUMP_FILE: &str = "/data/vendor/thermal/last_thermal.dump";
pub const MI_THERMALD_CONFIG_DIR: &str = "/data/vendor/thermal/config";
pub const MI_THERMALD_DECRYPT_DIR: &str = "/data/local/tmp/thermald_decrypt";

pub const MI_PROP_SOC_MODEL: &str = "ro.soc.model";
pub const MI_PROP_THERMAL_DATA_PATH: &str = "vendor.sys.thermal.data.path";

pub const MI_THERMALD_AES_KEY_STR: &[u8; 16] = b"thermalopenssl.h";

pub const MI_MAX_SENSORS: usize = 128;
pub const MI_MAX_CDEV: usize = 32;
pub const MI_MAX_VSNS: usize = 16;
pub const MI_MAX_CONFIGS: usize = 256;
pub const MI_MAX_LEVELS: usize = 16;
pub const MI_MAX_DEVICES_PER_BLOCK: usize = 16;
pub const MI_MAX_INPUTS_PER_VSN: usize = 8;
pub const MI_POLL_INTERVAL_MS_DEFAULT: i32 = 1000;
pub const MI_EPOLL_MAX_EVENTS: usize = 16;

pub const LOG_PID: libc::c_int = 0x01;
pub const LOG_NDELAY: libc::c_int = 0x08;
pub const LOG_DAEMON: libc::c_int = 3 << 3;
pub const LOG_INFO: libc::c_int = 6;
pub const LOG_WARNING: libc::c_int = 4;
pub const LOG_ERR: libc::c_int = 3;
pub const LOG_DEBUG: libc::c_int = 7;



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorType {
    ThermalZone = 0,
    Virtual = 1,
    Formula = 2,
}

impl SensorType {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => SensorType::ThermalZone,
            1 => SensorType::Virtual,
            2 => SensorType::Formula,
            _ => SensorType::ThermalZone,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgoType {
    Simulated = 0,
    Virtual = 1,
    Monitor = 2,
    Ss = 3,
    Sic = 4,
}

impl AlgoType {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "simulated" => AlgoType::Simulated,
            "virtual" => AlgoType::Virtual,
            "monitor" => AlgoType::Monitor,
            "ss" => AlgoType::Ss,
            "sic" => AlgoType::Sic,
            _ => AlgoType::Monitor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    None = 0,
    CpuFreq = 1,
    CpuHotplug = 2,
    GpuBoost = 3,
    IpaBoost = 4,
    Bcl = 5,
    TempAware = 6,
    PowersaveBoost = 7,
    Brightness = 8,
    Shutdown = 9,
    Fcc = 10,
    BoostLimit = 11,
    TempState = 12,
    MaxBrightness = 13,
    ModemLimit = 14,
    ModemLevel = 15,
    WifiLimit = 16,
    VoiceLimit = 17,
    TorchLevel = 18,
    Cdsp = 19,
    MarketLimit = 20,
    ScreenState = 21,
    Boost = 22,
    SpecialCpuLimit = 23,
    CpuLimits = 24,
}

#[derive(Debug)]
pub struct Sensor {
    pub name: String,
    pub path: String,
    pub type_: SensorType,
    pub zone_id: i32,
    pub cdev_id: i32,
    pub poll_ms: i32,
    pub last_temp_mc: AtomicI32,
    pub inputs: Vec<usize>,
    pub weights: Vec<i32>,
    pub weight_sum: i32,
    pub compensation: i32,
}

impl Sensor {
    pub fn new(name: &str) -> Self {
        Sensor {
            name: name.to_string(),
            path: String::new(),
            type_: SensorType::ThermalZone,
            zone_id: -1,
            cdev_id: -1,
            poll_ms: MI_POLL_INTERVAL_MS_DEFAULT,
            last_temp_mc: AtomicI32::new(0),
            inputs: Vec::new(),
            weights: Vec::new(),
            weight_sum: 0,
            compensation: 0,
        }
    }
}

#[derive(Debug)]
pub struct CoolingDevice {
    pub name: String,
    pub path: String,
    pub max_state: i32,
    pub cur_state: i32,
}

#[derive(Debug, Default, Clone)]
pub struct Threshold {
    pub trig: Vec<i32>,
    pub clr: Vec<i32>,
    pub target: Vec<i32>,
    pub ks: Vec<i32>,
    pub ki: Vec<i32>,
    pub kc: Vec<i32>,
    pub max_out: Vec<i32>,
    pub min_out: Vec<i32>,
    pub proportion: i32,
}

impl Threshold {
    pub fn n_levels(&self) -> usize {
        self.trig.len()
    }
}

#[derive(Debug, Clone)]
pub struct Action {
    pub type_: ActionType,
    pub target: String,
    pub value: i32,
}

#[derive(Debug, Clone)]
pub struct SicState {
    pub ek_1: i32,
    pub ek_2: i32,
    pub initialized: bool,
    pub initial_value: i32,
}

impl Default for SicState {
    fn default() -> Self {
        Self {
            ek_1: 0,
            ek_2: 0,
            initialized: false,
            initial_value: 6000000,
        }
    }
}

#[derive(Debug)]
pub struct Instance {
    pub name: String,
    pub sensor_idx: Option<usize>,
    pub algo: AlgoType,
    pub sample_ms: i32,
    pub reverse: bool,
    pub threshold: Threshold,
    pub actions: Vec<Action>,
    pub current_level: i32,
    pub current_value: i32,
}
