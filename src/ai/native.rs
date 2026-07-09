use std::collections::HashMap;
use std::fs;

use crate::types::{Instance, Sensor, Threshold, AlgoType};

use super::data_collector::DataCollector;
use super::features::{FeatureExtractor, StateVector};
use super::qtable::QTable;
use super::rewards::RewardCalculator;
use super::safety::SafetyMonitor;
use super::workload::WorkloadDetector;

const SAVE_INTERVAL_TICKS: u64 = 1000;
const DATA_DIR: &str = "/data/vendor/thermal/ai_data";

pub struct CoolingChannel {
    pub name: String,
    pub path: String,
    pub value: i32,
    pub min_val: i32,
    pub max_val: i32,
}

struct ScenarioModel {
    q_table: QTable,
    tick_count: u64,
    last_state: Option<StateVector>,
    last_action: Option<u8>,
    action_stability: f32,
    ticks_same_action: u64,
}

impl ScenarioModel {
    fn new() -> Self {
        ScenarioModel {
            q_table: QTable::new(),
            tick_count: 0,
            last_state: None,
            last_action: None,
            action_stability: 0.0,
            ticks_same_action: 0,
        }
    }
}

pub struct NativeController {
    pub channels: Vec<CoolingChannel>,
    scenario_models: HashMap<i32, ScenarioModel>,
    current_scenario: i32,
    feature_extractor: FeatureExtractor,
    reward_calculator: RewardCalculator,
    safety_monitor: SafetyMonitor,
    data_collector: DataCollector,
    workload_detector: WorkloadDetector,
    enabled: bool,
    disabled_reason: Option<String>,
    last_charge_temp: i32,
    charge_max_val: i32,
    sustained_load_ticks: u64,
    idle_load_ticks: u64,
}

impl NativeController {
    pub fn new() -> Self {
        let mut ctrl = NativeController {
            channels: Vec::new(),
            scenario_models: HashMap::new(),
            current_scenario: 0,
            feature_extractor: FeatureExtractor::new(),
            reward_calculator: RewardCalculator::new(),
            safety_monitor: SafetyMonitor::new(),
            data_collector: DataCollector::new(),
            workload_detector: WorkloadDetector::new(),
            sustained_load_ticks: 0,
            idle_load_ticks: 0,
            enabled: true,
            disabled_reason: None,
            last_charge_temp: -999,
            charge_max_val: 12000000,
        };
        ctrl.discover_known_writable_channels();
        ctrl.load_persisted_tables();
        if ctrl.channels.is_empty() {
            log_warn!("AI-native: no writable cooling channels, disabling");
            ctrl.enabled = false;
            ctrl.disabled_reason = Some("no writable channels".into());
        }
        ctrl
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn disabled_reason(&self) -> Option<&str> {
        self.disabled_reason.as_deref()
    }

    pub fn save_checkpoint(&mut self) {
        self.data_collector.flush();
        for (&scenario, model) in &self.scenario_models {
            #[derive(serde::Serialize)]
            struct Checkpoint {
                scenario: i32,
                tick_count: u64,
                epsilon: f32,
                weights: Vec<f32>,
            }
            let cp = Checkpoint {
                scenario,
                tick_count: model.tick_count,
                epsilon: model.q_table.epsilon(),
                weights: model.q_table.weights().to_vec(),
            };
            let path = format!("{}/q_scenario_{}.json", DATA_DIR, scenario);
            if let Ok(json) = serde_json::to_string(&cp) {
                let temp = format!("{}.tmp", path);
                if fs::write(&temp, &json).is_ok() {
                    let _ = fs::rename(&temp, &path);
                }
            }
        }
    }

    pub fn switch_scenario(&mut self, new_idx: i32) {
        if new_idx == self.current_scenario || !self.enabled {
            return;
        }
        self.save_checkpoint();
        self.current_scenario = new_idx;
        log_debug!("AI-native: switched to scenario {}", new_idx);
    }

    pub fn tick(&mut self, sensor_readings: &[Sensor]) -> bool {
        if !self.enabled {
            return false;
        }

        let (final_action, state, prev_state, prev_action) = {
            let model = self.scenario_models
                .entry(self.current_scenario)
                .or_insert_with(ScenarioModel::new);

            model.tick_count += 1;

            let dummy_instance = Instance {
                name: String::new(),
                sensor_idx: None,
                algo: AlgoType::Monitor,
                sample_ms: 0,
                reverse: false,
                threshold: Threshold::default(),
                actions: Vec::new(),
                current_level: 0,
                current_value: 0,
            };

            let mut state = self.feature_extractor.extract(
                sensor_readings,
                &dummy_instance,
                0,
            );

            state.last_action = model.last_action.map(|a| a as f32 / 9.0).unwrap_or(0.0);
            state.action_stability = model.action_stability;

            let state_array = self.feature_extractor.to_array(&state);

            if self.safety_monitor.is_disabled(model.tick_count) {
                self.enabled = false;
                self.disabled_reason = Some(format!(
                    "disabled after {} safety violations",
                    self.safety_monitor.violations()
                ));
                log_warn!("AI-native: {}", self.disabled_reason.as_ref().unwrap());
                return false;
            }

            let action = model.q_table.select_action(&state_array);
            let safe = self.safety_monitor.check_action(action, &[], model.tick_count);
            let mut final_action = if safe { action } else { 3u8 };

            // Sustained load detection: cpu_load is scheduler busyness,
            // independent of frequency — works even if we're capping.
            if state.cpu_load > 0.44 {
                self.sustained_load_ticks = self.sustained_load_ticks.saturating_add(1).min(100);
                self.idle_load_ticks = 0;
            } else {
                self.idle_load_ticks += 1;
                if self.idle_load_ticks >= 3 {
                    self.sustained_load_ticks = 0;
                }
            }

            // Detect current workload mode using sconfig or sensors
            let gpu_freq_ratio = crate::ai::WorkloadDetector::read_gpu_freq_ratio();
            let workload_mode = self.workload_detector.detect_workload(
                state.cpu_load,
                state.cpu_freq_ratio,
                gpu_freq_ratio,
                state.temp_variance,
                state.screen_on > 0.5,
            );

            // Heavy load is true if:
            // 1. Sustained CPU load >= 3 ticks (sensor-based), OR
            // 2. Workload mode is Gaming/Benchmark (sconfig instant detection)
            let is_heavy_load = self.sustained_load_ticks >= 3
                || matches!(workload_mode, crate::ai::WorkloadMode::Gaming | crate::ai::WorkloadMode::PerfGaming | crate::ai::WorkloadMode::Benchmark);

            log_debug!("AI-native: load={:.2} sustained={} workload={:?} heavy={} action={}",
                state.cpu_load, self.sustained_load_ticks, workload_mode, is_heavy_load, final_action);

            if is_heavy_load && final_action < 9 {
                log_debug!("AI-native: sustained override {} -> 9", final_action);
                final_action = 9;
            }

            // Battery temp throttle: clamp action based on battery temperature
            // Separate thresholds for Gaming, Benchmark, and light loads
            let batt_temp = crate::sensor::sysfs::read_int(
                "/sys/class/power_supply/battery/temp"
            );
            let batt_temp_c = batt_temp as f32 / 10.0;
            if batt_temp >= 0 {
                let temp_action = match workload_mode {
                    // Benchmark - allows highest temps (up to 46°C)
                    crate::ai::WorkloadMode::Benchmark => {
                        if batt_temp_c < 41.5 {
                            9
                        } else if batt_temp_c < 43.5 {
                            8
                        } else if batt_temp_c < 46.0 {
                            6
                        } else {
                            5
                        }
                    },
                    // Gaming - high temps allowed (up to 44°C)
                    crate::ai::WorkloadMode::PerfGaming => {
                        if batt_temp_c < 38.0 {
                            9
                        } else if batt_temp_c < 40.0 {
                            8
                        } else if batt_temp_c < 42.0 {
                            7
                        } else if batt_temp_c < 44.0 {
                            6
                        } else {
                            5
                        }
                    },
                    // Gaming - high temps allowed (up to 44°C)
                    crate::ai::WorkloadMode::Gaming => {
                        if batt_temp_c < 36.0 {
                            9
                        } else if batt_temp_c < 38.0 {
                            8
                        } else if batt_temp_c < 40.0 {
                            7
                        } else if batt_temp_c < 42.0 {
                            6
                        } else {
                            5
                        }
                    },
                    // Idle, Light, Moderate - conservative thresholds
                    _ => {
                        if batt_temp_c < 34.0 {
                            9
                        } else if batt_temp_c < 36.0 {
                            8
                        } else if batt_temp_c < 38.0 {
                            7
                        } else if batt_temp_c < 40.0 {
                            6
                        } else if batt_temp_c < 43.0 {
                            4
                        } else if batt_temp_c < 45.0 {
                            2
                        } else {
                            0
                        }
                    }
                };
                if temp_action < final_action {
                    log_debug!("AI-native: batt={:.1}°C override action {} -> {}", batt_temp_c, final_action, temp_action);
                    final_action = temp_action;
                }
            }

            let prev_state = model.last_state.clone();
            let prev_action = model.last_action;
            if model.last_action == Some(final_action) {
                model.ticks_same_action += 1;
            } else {
                model.ticks_same_action = 0;
            }
            model.action_stability = (model.ticks_same_action as f32 / 100.0).min(1.0);
            model.last_state = Some(state.clone());
            model.last_action = Some(final_action);

            (final_action, state, prev_state, prev_action)
        };

        self.apply_action(final_action);

        if let (Some(ref ls), Some(la)) = (prev_state, prev_action) {
            let reward = self.reward_calculator.compute_reward(ls, la as i32, final_action as i32);
            let state_array = self.feature_extractor.to_array(ls);
            let next_array = self.feature_extractor.to_array(&state);

            let model = self.scenario_models
                .get_mut(&self.current_scenario)
                .unwrap();
            model.q_table.update(&state_array, la, reward, &next_array);
            self.data_collector.record(ls, la, reward);
        }

        {
            let model = self.scenario_models
                .get_mut(&self.current_scenario)
                .unwrap();
            if model.tick_count % SAVE_INTERVAL_TICKS == 0 {
                self.save_checkpoint();
            }
        }

        true
    }

    fn discover_known_writable_channels(&mut self) {
        // balance_mode: 0 = max cooling, higher = more performance
        if std::path::Path::new("/sys/class/thermal/thermal_message/balance_mode").exists() {
            let cur = crate::sensor::sysfs::read_int(
                "/sys/class/thermal/thermal_message/balance_mode"
            );
            self.channels.push(CoolingChannel {
                name: "balance_mode".into(),
                path: "/sys/class/thermal/thermal_message/balance_mode".into(),
                value: cur.max(0).min(9),
                min_val: 0,
                max_val: 9,
            });
        }

        // boost: 0 = disabled, 1 = enabled
        if std::path::Path::new("/sys/class/thermal/thermal_message/boost").exists() {
            let cur = crate::sensor::sysfs::read_int("/sys/class/thermal/thermal_message/boost");
            self.channels.push(CoolingChannel {
                name: "boost".into(),
                path: "/sys/class/thermal/thermal_message/boost".into(),
                value: cur.max(0),
                min_val: 0,
                max_val: 1,
            });
        }

        // constant_charge_current: battery charging current in uA
        let batt_path = "/sys/class/power_supply/battery/constant_charge_current";
        if std::path::Path::new(batt_path).exists() {
            let cur = crate::sensor::sysfs::read_int(batt_path).max(0);
            let max_val = crate::sensor::sysfs::read_int(
                "/sys/class/power_supply/battery/constant_charge_current_max"
            );
            let max_val = if max_val > 0 { max_val } else { 12000000 };
            self.charge_max_val = max_val;
            self.channels.push(CoolingChannel {
                name: "charge_current".into(),
                path: batt_path.into(),
                value: if cur > 0 { cur } else { max_val },
                min_val: 500000,
                max_val,
            });
        }

        // scaling_max_freq for each CPU cluster (direct frequency cap)
        for &(policy, hw_max, var) in &[
            (0, 2016000, &crate::CPU_FREQ0_TARGET),
            (3, 2803200, &crate::CPU_FREQ3_TARGET),
            (7, 3014400, &crate::CPU_FREQ7_TARGET),
        ] {
            let path = format!("/sys/devices/system/cpu/cpufreq/policy{}/scaling_max_freq", policy);
            if std::path::Path::new(&path).exists() {
                let cur = crate::sensor::sysfs::read_int(&path).max(0);
                let cur = if cur > 0 { cur } else { hw_max };
                // FIX BUG-004: Use Release ordering for cross-thread visibility
                var.store(cur, std::sync::atomic::Ordering::Release);
                self.channels.push(CoolingChannel {
                    name: format!("cpu_freq{}", policy),
                    path,
                    value: cur,
                    min_val: hw_max * 3 / 10,  // 30% floor
                    max_val: hw_max,
                });
            }
        }

        log_info!("AI-native: discovered {} writable cooling channels", self.channels.len());
        for ch in &self.channels {
            log_info!("AI-native: channel {} min={} max={} cur={}",
                ch.name, ch.min_val, ch.max_val, ch.value);
        }
    }

    fn apply_action(&mut self, action: u8) {
        // Charge current is temperature-governed, not AI-controlled
        self.apply_temp_based_charge();

        let mut boost_val = 0i32;
        for ch in &mut self.channels {
            if ch.name == "charge_current" {
                continue;
            }
            if ch.name == "boost" {
                boost_val = Self::action_to_channel_value(action, ch);
                continue; // write boost last
            }
            if ch.name.starts_with("cpu_freq") {
                let value = Self::action_to_channel_value(action, ch);
                // Set atomic for continuous writer thread
                if let Some(t) = Self::cpu_freq_target_var(&ch.name) {
                    // FIX BUG-004: Use Release ordering for cross-thread visibility
                    t.store(value, std::sync::atomic::Ordering::Release);
                }
                // Also write directly for immediate effect
                if crate::sensor::sysfs::write_int(&ch.path, value) {
                    ch.value = value;
                    log_debug!("AI-native: {} = {} (action {})", ch.name, value, action);
                } else {
                    log_warn!("AI-native: write failed on {}", ch.name);
                }
                continue;
            }
            let value = Self::action_to_channel_value(action, ch);
            if value != ch.value {
                let ok = crate::sensor::sysfs::write_int(&ch.path, value);
                if ok {
                    log_debug!("AI-native: {} = {} (action {})", ch.name, value, action);
                    ch.value = value;
                } else {
                    log_warn!("AI-native: write failed on {}", ch.name);
                }
            }
        }
        // Write boost last — balance_mode ≥8 can reset it, so we always re-assert
        if let Some(ch) = self.channels.iter_mut().find(|c| c.name == "boost") {
            if crate::sensor::sysfs::write_int(&ch.path, boost_val) {
                ch.value = boost_val;
                log_debug!("AI-native: boost = {} (action {})", boost_val, action);
            }
        }
    }

    fn cpu_freq_target_var(name: &str) -> Option<&'static std::sync::atomic::AtomicI32> {
        match name {
            "cpu_freq0" => Some(&crate::CPU_FREQ0_TARGET),
            "cpu_freq3" => Some(&crate::CPU_FREQ3_TARGET),
            "cpu_freq7" => Some(&crate::CPU_FREQ7_TARGET),
            _ => None,
        }
    }

    fn apply_temp_based_charge(&mut self) {
        let charging = if let Some(s) = crate::sensor::sysfs::read_string(
            "/sys/class/power_supply/battery/status"
        ) {
            s.trim() == "Charging" || s.trim() == "Full"
        } else {
            false
        };
        if !charging {
            return;
        }

        // Detect charger type
        let usb_type = crate::sensor::sysfs::read_string(
            "/sys/class/power_supply/usb/type"
        ).unwrap_or_default();
        let fast_charger = usb_type.trim() == "USB_PD" || usb_type.trim() == "USB_HVDCP";

        // Non-fast chargers (DCP, regular USB): fixed slow rate, no temp reduction needed
        if !fast_charger {
            let slow_rate = 3000000; // 3A constant
            // FIX BUG-004: Use Release ordering for cross-thread visibility
            crate::FCC_VALUE.store(slow_rate, std::sync::atomic::Ordering::Release);
            log_debug!("AI-native: charge_current = {} (slow charger type={})", slow_rate, usb_type.trim());
            let path = "/sys/class/power_supply/battery/constant_charge_current";
            let _ = crate::sensor::sysfs::write_int(path, slow_rate);
            return;
        }

        let batt_temp = crate::sensor::sysfs::read_int(
            "/sys/class/power_supply/battery/temp"
        );
        if batt_temp < 0 {
            return;
        }

        // Update every tick for smooth control (removed hysteresis check)
        self.last_charge_temp = batt_temp;

        let max = self.charge_max_val;

        // Smooth linear transitions instead of step function
        let temp_c = batt_temp as f32 / 10.0;
        let current = if temp_c >= 48.0 {
            max / 10  // Emergency: 10%
        } else if temp_c >= 45.0 {
            // 45-48°C: linear ramp from 30% to 10%
            let t = ((temp_c - 45.0) / 3.0).clamp(0.0, 1.0);
            (max as f32 * (0.3 - 0.2 * t)) as i32
        } else if temp_c >= 42.0 {
            // 42-45°C: linear ramp from 60% to 30%
            let t = ((temp_c - 42.0) / 3.0).clamp(0.0, 1.0);
            (max as f32 * (0.6 - 0.3 * t)) as i32
        } else if temp_c >= 39.0 {
            // 39-42°C: linear ramp from 85% to 60%
            let t = ((temp_c - 39.0) / 3.0).clamp(0.0, 1.0);
            (max as f32 * (0.85 - 0.25 * t)) as i32
        } else if temp_c >= 35.0 {
            // 35-39°C: linear ramp from 100% to 85%
            let t = ((temp_c - 35.0) / 4.0).clamp(0.0, 1.0);
            (max as f32 * (1.0 - 0.15 * t)) as i32
        } else {
            max  // <35°C: full speed
        };

        // FIX BUG-004: Use Release ordering for cross-thread visibility
        crate::FCC_VALUE.store(current, std::sync::atomic::Ordering::Release);
        log_debug!("AI-native: charge_current = {} (battery {:.1}°C)", current, temp_c);

        let path = "/sys/class/power_supply/battery/constant_charge_current";
        let _ = crate::sensor::sysfs::write_int(path, current);
    }

    fn action_to_channel_value(action: u8, ch: &CoolingChannel) -> i32 {
        match ch.name.as_str() {
            "balance_mode" => (action as i32 * 7 / 9).min(7),
            "boost" => 1, // always on — balance_mode handles cooling
            _ => {
                let range = (ch.max_val - ch.min_val).max(1);
                match action {
                    0 => ch.min_val,
                    1 => ch.min_val + range / 9,
                    2 => ch.min_val + range * 2 / 9,
                    3 => ch.min_val + range * 3 / 9,
                    4 => ch.min_val + range * 4 / 9,
                    5 => ch.min_val + range * 5 / 9,
                    6 => ch.min_val + range * 6 / 9,
                    7 => ch.min_val + range * 7 / 9,
                    8 => ch.min_val + range * 8 / 9,
                    9 => ch.max_val,
                    _ => ch.value,
                }
            }
        }
    }

    fn load_persisted_tables(&mut self) {
        let dir = std::path::Path::new(DATA_DIR);
        if !dir.exists() {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.starts_with("q_scenario_") || !fname.ends_with(".json") {
                continue;
            }
            let num_part = &fname["q_scenario_".len()..fname.len() - ".json".len()];
            if let Ok(scenario_id) = num_part.parse::<i32>() {
                if let Ok(json) = fs::read_to_string(entry.path()) {
                    #[derive(serde::Deserialize)]
                    struct Checkpoint {
                        weights: Vec<f32>,
                    }
                    if let Ok(cp) = serde_json::from_str::<Checkpoint>(&json) {
                        let mut q_table = QTable::new();
                        if cp.weights.len() == q_table.weights().len() {
                            if q_table.set_weights(&cp.weights).is_some() {
                                let mut model = ScenarioModel::new();
                                model.q_table = q_table;
                                self.scenario_models.insert(scenario_id, model);
                                log_info!("AI-native: loaded Q-table for scenario {} ({} weights)",
                                    scenario_id, cp.weights.len());
                            }
                        }
                    }
                }
            }
        }
    }
}
