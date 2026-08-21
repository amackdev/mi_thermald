use std::fs;
use std::path::Path;

use aes::Aes128;
use cbc::Decryptor;
use cbc::cipher::{KeyIvInit, BlockDecryptMut, block_padding::NoPadding};

use crate::types::*;
use crate::sensor::EngineDiscovery;



fn device_to_action(dev: &str) -> ActionType {
    if dev.len() >= 4
        && dev.as_bytes()[0] == b'c'
        && dev.as_bytes()[1] == b'p'
        && dev.as_bytes()[2] == b'u'
        && dev.as_bytes()[3] >= b'0'
        && dev.as_bytes()[3] <= b'7'
        && dev.len() == 4
    {
        return ActionType::CpuFreq;
    }
    if dev.starts_with("hotplug_cpu") {
        return ActionType::CpuHotplug;
    }
    if dev.starts_with("policy") || dev.starts_with("cpu-cluster") {
        return ActionType::CpuFreq;
    }
    if dev.starts_with("VIRTUAL") {
        return ActionType::None;
    }
    match dev {
        "gpu" => ActionType::GpuBoost,
        "battery" => ActionType::Bcl,
        "thermal_fcc_override" => ActionType::Fcc,
        "boost_limit" => ActionType::BoostLimit,
        "temp_state" => ActionType::TempState,
        "thermal_max_brightness" => ActionType::MaxBrightness,
        "modem_limit" => ActionType::ModemLimit,
        "modem_level" => ActionType::ModemLevel,
        "wifi_limit" => ActionType::WifiLimit,
        "voice_limit" => ActionType::VoiceLimit,
        "torch_level" => ActionType::TorchLevel,
        "cdsp" => ActionType::Cdsp,
        "market_download_limit" => ActionType::MarketLimit,
        "screen_state" => ActionType::ScreenState,
        "boost" => ActionType::Boost,
        "special_cpu_limit" => ActionType::SpecialCpuLimit,
        "cpu_limits" => ActionType::CpuLimits,
        _ => ActionType::CpuFreq,
    }
}

fn parse_int_array(s: &str, max: usize) -> Vec<i32> {
    s.split_whitespace()
        .filter_map(|tok| tok.parse::<i32>().ok())
        .take(max)
        .collect()
}

fn split_devices(s: &str, max: usize) -> Vec<String> {
    s.split('+')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .take(max)
        .collect()
}

#[derive(Debug, Clone)]
pub struct ConfigBlock {
    pub block_name: String,
    pub algo_str: String,
    pub sensor_name: String,
    pub sensor_list: Vec<String>,
    pub devices: Vec<String>,
    pub polling: i32,
    pub weight_sum: i32,
    pub compensation: i32,
    pub weights: Vec<i32>,
    pub threshold: Threshold,
    pub proportion: i32,
    pub reverse: i32,
}

impl Default for ConfigBlock {
    fn default() -> Self {
        ConfigBlock {
            block_name: String::new(),
            algo_str: String::new(),
            sensor_name: String::new(),
            sensor_list: Vec::new(),
            devices: Vec::new(),
            polling: 1000,
            weight_sum: 0,
            compensation: 0,
            weights: Vec::new(),
            threshold: Threshold::default(),
            proportion: 0,
            reverse: 0,
        }
    }
}

fn resolve_block(
    sensors: &mut Vec<Sensor>,
    virtual_sensors: &[usize],
    instances: &mut Vec<Instance>,
    sic_states: &mut Vec<SicState>,
    b: &ConfigBlock,
) -> i32 {
    if instances.len() >= MI_MAX_CONFIGS {
        log_warn!("too many instances, dropping block {}", b.block_name);
        return -1;
    }

    let algo = AlgoType::from_str(&b.algo_str);
    let mut inst = Instance {
        name: b.block_name.clone(),
        sensor_idx: None,
        algo,
        sample_ms: b.polling,
        reverse: b.reverse != 0,
        threshold: Threshold {
            proportion: b.proportion,
            ..b.threshold.clone()
        },
        actions: Vec::new(),
        current_level: 0,
        current_value: 0,
    };

    log_debug!("resolve block {} algo={:?} sensor={} reverse={} n_trig={} n_clr={} n_tgt={} devices={:?}",
        b.block_name, algo, b.sensor_name, b.reverse,
        b.threshold.trig.len(), b.threshold.clr.len(),
        b.threshold.target.len(), b.devices);

    if !b.sensor_name.is_empty() {
        let idx = EngineDiscovery::find_sensor(sensors, virtual_sensors, &b.sensor_name);
        if let Some(idx) = idx {
            inst.sensor_idx = Some(idx);
        } else if inst.algo != AlgoType::Simulated && sensors.len() < MI_MAX_SENSORS {
            let mut s = Sensor::new(&b.sensor_name);
            s.type_ = SensorType::Virtual;
            inst.sensor_idx = Some(sensors.len());
            sensors.push(s);
        }
    }

    if inst.algo == AlgoType::Virtual {
        let sensor_idx = if let Some(idx) = inst.sensor_idx {
            idx
        } else if !b.sensor_list.is_empty() {
            // Reuse existing virtual sensor definition (e.g. VIRTUAL-SENSOR0
            // defined by thermal-normal.conf but not by the mgame scenario).
            let existing = EngineDiscovery::find_sensor(sensors, virtual_sensors, &b.block_name);
            if let Some(idx) = existing {
                inst.sensor_idx = Some(idx);
                idx
            } else if sensors.len() < MI_MAX_SENSORS {
                let mut s = Sensor::new(&b.block_name);
                s.type_ = SensorType::Virtual;
                let idx = sensors.len();
                sensors.push(s);
                inst.sensor_idx = Some(idx);
                idx
            } else {
                usize::MAX
            }
        } else {
            usize::MAX
        };
        if sensor_idx != usize::MAX && sensors[sensor_idx].inputs.is_empty() {
            for (i, sname) in b.sensor_list.iter().enumerate().take(MI_MAX_INPUTS_PER_VSN) {
                let input_idx = EngineDiscovery::find_sensor(sensors, virtual_sensors, sname);
                if let Some(input_idx) = input_idx {
                    sensors[sensor_idx].inputs.push(input_idx);
                    sensors[sensor_idx].weights.push(
                        if i < b.weights.len() { b.weights[i] } else { 0 },
                    );
                } else if sensors.len() < MI_MAX_SENSORS {
                    let mut s = Sensor::new(sname);
                    s.type_ = SensorType::ThermalZone;
                    s.path = format!("/sys/class/thermal/thermal_message/{}", sname);
                    let input_idx = sensors.len();
                    sensors.push(s);
                    sensors[sensor_idx].inputs.push(input_idx);
                    sensors[sensor_idx].weights.push(
                        if i < b.weights.len() { b.weights[i] } else { 0 },
                    );
                }
            }
            sensors[sensor_idx].weight_sum = b.weight_sum;
            sensors[sensor_idx].compensation = b.compensation;
        }
    }

    let n_dev = if b.devices.is_empty() { 1 } else { b.devices.len() };
    if n_dev == 1 && b.devices.is_empty() {
        instances.push(inst);
        sic_states.push(SicState::default());
        return 0;
    }

    let n_lvl = if b.threshold.n_levels() == 0 { 1 } else { b.threshold.n_levels() + 1 };
    let n_tgt = b.threshold.target.len();
    for lvl in 0..n_lvl {
        for d in 0..n_dev {
            if inst.actions.len() >= MI_MAX_DEVICES_PER_BLOCK * MI_MAX_LEVELS {
                break;
            }
            let idx = lvl * n_dev + d;
            let a = Action {
                type_: device_to_action(&b.devices[d]),
                target: b.devices[d].clone(),
                value: if idx < n_tgt { b.threshold.target[idx] } else { 0 },
            };
            inst.actions.push(a);
        }
    }

    instances.push(inst);
    sic_states.push(SicState::default());
    0
}

pub fn parse_config_blocks(path: &str) -> Vec<ConfigBlock> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            log_err!("open {}: {}", path, e);
            return Vec::new();
        }
    };
    log_info!("parsing scenario config blocks {}", path);

    let mut blocks = Vec::new();
    let mut b = ConfigBlock::default();
    let mut in_block = false;

    for line in content.lines() {
        let line = line.trim();
        let line = if let Some(hash) = line.find('#') {
            &line[..hash]
        } else {
            line
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') {
            if in_block {
                blocks.push(b);
            }
            b = ConfigBlock::default();
            in_block = true;
            let end = line.find(']').unwrap_or(line.len());
            b.block_name = line[1..end].to_string();
            continue;
        }

        if !in_block {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, |c: char| c == ' ' || c == '\t')
            .map(|s| s.trim())
            .collect();
        if parts.len() < 2 {
            continue;
        }
        let key = parts[0].to_string();
        let value = parts[1..].join(" ");

        match key.as_str() {
            "algo_type" => b.algo_str = value,
            "sensor" => b.sensor_name = value,
            "sensors" => {
                b.sensor_list = value.split_whitespace()
                    .map(|s| s.to_string())
                    .take(MI_MAX_INPUTS_PER_VSN)
                    .collect();
            }
            "weight" => {
                b.weights = parse_int_array(&value, MI_MAX_INPUTS_PER_VSN);
            }
            "weight_sum" => b.weight_sum = value.parse().unwrap_or(0),
            "compensation" => b.compensation = value.parse().unwrap_or(0),
            "device" => b.devices = split_devices(&value, MI_MAX_DEVICES_PER_BLOCK),
            "polling" => b.polling = value.parse().unwrap_or(1000),
            "trig" => b.threshold.trig = parse_int_array(&value, MI_MAX_LEVELS),
            "clr" => b.threshold.clr = parse_int_array(&value, MI_MAX_LEVELS),
            "target" => {
                b.threshold.target = value.split_whitespace()
                    .flat_map(|tok| {
                        if tok.contains('+') {
                            tok.split('+').filter_map(|s| s.trim().parse::<i32>().ok())
                                .collect::<Vec<_>>()
                        } else {
                            vec![tok.parse::<i32>().unwrap_or(0)]
                        }
                    })
                    .take(MI_MAX_LEVELS * MI_MAX_DEVICES_PER_BLOCK)
                    .collect();
            }
            "ks" => b.threshold.ks = parse_int_array(&value, MI_MAX_LEVELS),
            "ki" => b.threshold.ki = parse_int_array(&value, MI_MAX_LEVELS),
            "kc" => b.threshold.kc = parse_int_array(&value, MI_MAX_LEVELS),
            "max" => b.threshold.max_out = parse_int_array(&value, MI_MAX_LEVELS),
            "min" => b.threshold.min_out = parse_int_array(&value, MI_MAX_LEVELS),
            "proportion" => b.proportion = value.parse().unwrap_or(0),
            "reverse" => b.reverse = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if in_block {
        blocks.push(b);
    }
    blocks
}

pub fn load_scenario_config(
    sensors: &mut Vec<Sensor>,
    virtual_sensors: &[usize],
    instances: &mut Vec<Instance>,
    sic_states: &mut Vec<SicState>,
    path: &str,
) -> i32 {
    let blocks = parse_config_blocks(path);
    if blocks.is_empty() {
        return -1;
    }
    
    for b in blocks {
        resolve_block(sensors, virtual_sensors, instances, sic_states, &b);
    }
    0
}


pub fn decrypt_config_file(in_path: &str, out_path: &str) -> i32 {
    let encrypted = match fs::read(in_path) {
        Ok(d) => d,
        Err(e) => {
            log_err!("decrypt: open {}: {}", in_path, e);
            return -1;
        }
    };

    if encrypted.len() % 16 != 0 {
        log_warn!("decrypt: {} not aligned to 16 (size={}) — plaintext?", in_path, encrypted.len());
        return -1;
    }

    use cbc::cipher::generic_array::GenericArray;
    let key = GenericArray::from_slice(MI_THERMALD_AES_KEY_STR);
    let iv = GenericArray::from_slice(MI_THERMALD_AES_KEY_STR);
    let mut plaintext = encrypted.clone();

    if let Err(e) = Decryptor::<Aes128>::new(key, iv)
        .decrypt_padded_mut::<NoPadding>(&mut plaintext)
    {
        log_err!("decrypt: AES-CBC decrypt failed: {:?}", e);
        return -1;
    }

    // Strip PKCS7 padding
    let pad_len = plaintext.last().copied().unwrap_or(0) as usize;
    if pad_len > 0 && pad_len <= 16 && plaintext.len() > pad_len {
        if plaintext[plaintext.len() - pad_len..].iter().all(|&b| b == pad_len as u8) {
            plaintext.truncate(plaintext.len() - pad_len);
        }
    }

    if plaintext.len() < 2 || plaintext[0] != b'[' {
        log_err!("decrypt: {} plaintext doesn't look like config", in_path);
        return -1;
    }

    if let Some(parent) = Path::new(out_path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(out_path, &plaintext) {
        Ok(_) => {
            log_info!("decrypted {} -> {} ({} bytes)", in_path, out_path, plaintext.len());
            0
        }
        Err(e) => {
            log_err!("decrypt: write {}: {}", out_path, e);
            -1
        }
    }
}

pub fn find_scenario_name(map_content: &str, idx: i32) -> String {
    for line in map_content.lines() {
        if line.starts_with('[') {
            if let Some(end) = line.find(']') {
                let inner = &line[1..end];
                if let Some(colon) = inner.find(':') {
                    let entry_idx: i32 = inner[..colon].parse().unwrap_or(-1);
                    let fname = inner[colon + 1..].to_string();
                    if entry_idx == idx {
                        return fname;
                    }
                }
            }
        }
    }
    "thermal-normal.conf".to_string()
}

pub fn get_scenario_map_content() -> Option<String> {
    let data_path = crate::property_get_str(MI_PROP_THERMAL_DATA_PATH, "/data/vendor/thermal");
    let map_candidates = [
        format!("{}/thermal-map.conf", MI_THERMALD_CONFIG_DIR),
        "/odm/etc/thermal-map.conf".to_string(),
        "/vendor/etc/thermal-map.conf".to_string(),
        format!("{}/thermal-map.conf", data_path),
    ];
    let map_path = map_candidates.iter().find(|p| Path::new(p).exists())
        .map(|s| s.as_str())
        .unwrap_or(&map_candidates[0]);
    let plain_path = format!("{}/thermal-map.txt", MI_THERMALD_DECRYPT_DIR);
    if decrypt_config_file(map_path, &plain_path) != 0 {
        log_err!("cannot decrypt dispatch table {}", map_path);
        return None;
    }
    fs::read_to_string(&plain_path).ok()
}

pub fn resolve_scenario_path(fname: &str) -> Option<String> {
    let data_path = crate::property_get_str(MI_PROP_THERMAL_DATA_PATH, "/data/vendor/thermal");
    let vendor_path = format!("/vendor/etc/{}", fname);
    let odm_path = format!("/odm/etc/{}", fname);
    let scen_enc = format!("{}/config/{}", data_path, fname);

    let chosen = if Path::new(&odm_path).exists() { &odm_path }
        else if Path::new(&vendor_path).exists() { &vendor_path }
        else if Path::new(&scen_enc).exists() { &scen_enc }
        else { "" };

    if chosen.is_empty() {
        return None;
    }

    let scen_plain = format!("{}/{}.txt", MI_THERMALD_DECRYPT_DIR, fname);
    if decrypt_config_file(chosen, &scen_plain) != 0 {
        return Some(chosen.to_string());
    }
    Some(scen_plain)
}

fn load_single_scenario(
    sensors: &mut Vec<Sensor>,
    virtual_sensors: &mut Vec<usize>,
    instances: &mut Vec<Instance>,
    sic_states: &mut Vec<SicState>,
    fname: &str,
) -> i32 {
    let path = match resolve_scenario_path(fname) {
        Some(p) => p,
        None => {
            log_err!("no scenario config found for {}", fname);
            return -1;
        }
    };
    load_scenario_config(sensors, virtual_sensors, instances, sic_states, &path)
}

pub fn load_thermal_map(
    sensors: &mut Vec<Sensor>,
    virtual_sensors: &mut Vec<usize>,
    _cooling_devices: &mut Vec<CoolingDevice>,
    instances: &mut Vec<Instance>,
    sic_states: &mut Vec<SicState>,
    _soc: &str,
) -> i32 {
    let map_content = match get_scenario_map_content() {
        Some(c) => c,
        None => return -1,
    };

    let target_idx = {
        let idx = EngineDiscovery::read_sconfig_idx();
        if idx < 0 { 0 } else { idx }
    };

    let target_scenario = find_scenario_name(&map_content, target_idx);
    log_info!("sconfig index = {}", target_idx);

    instances.clear();
    sic_states.clear();

    if sensors.is_empty() {
        EngineDiscovery::sensor_init(sensors);

        // First call: if the target scenario doesn't define VIRTUAL-SENSOR0
        // (e.g. mgame), pre-load the default (sconfig=0) scenario first so
        // virtual sensor definitions persist across reloads.
        let default_scenario = find_scenario_name(&map_content, 0);
        if default_scenario != target_scenario {
            log_info!("first call with sconfig={} != 0 — pre-loading {} first",
                target_idx, default_scenario);
            load_single_scenario(sensors, virtual_sensors, instances, sic_states, &default_scenario);
            instances.clear();
            sic_states.clear();
        }
    }

    // Rebuild virtual_sensors index from existing sensors
    *virtual_sensors = sensors.iter()
        .enumerate()
        .filter(|(_, s)| s.type_ == SensorType::Virtual)
        .map(|(i, _)| i)
        .collect();

    load_single_scenario(sensors, virtual_sensors, instances, sic_states, &target_scenario)
}
