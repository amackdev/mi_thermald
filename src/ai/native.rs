use std::collections::HashMap;
use std::fs;

use crate::types::{Instance, Sensor, Threshold, AlgoType, ActionType, SicState};

use super::data_collector::DataCollector;
use super::features::{FeatureExtractor, StateVector};
use super::qtable::QTable;
use super::rewards::RewardCalculator;
use super::safety::SafetyMonitor;
use super::workload::WorkloadDetector;
use super::WorkloadMode;

const SAVE_INTERVAL_TICKS: u64 = 1000;
const DATA_DIR: &str = "/data/vendor/thermal/ai_data";

/// Battery-temp throttle ladder: ascending (upper_bound_c, action) pairs.
/// The first entry whose bound the temperature is still under wins; the
/// last entry's bound must be f32::MAX so it always matches as a fallback.
type TempLadder = &'static [(f32, u8)];

// Benchmark allows the highest temps (up to 46°C).
const LADDER_BENCHMARK: TempLadder = &[(41.5, 9), (43.5, 8), (46.0, 6), (f32::MAX, 5)];
// PerfGaming: high temps allowed (up to 44°C).
const LADDER_PERFGAMING: TempLadder = &[(38.0, 9), (40.0, 8), (42.0, 7), (44.0, 6), (f32::MAX, 5)];
// Gaming: high temps allowed (up to 42°C).
const LADDER_GAMING: TempLadder = &[(36.0, 9), (38.0, 8), (40.0, 7), (42.0, 6), (f32::MAX, 5)];
// Moderate: sustained mixed load rarely reaches these temps (higher freq
// finishes micro-tasks faster, so heating is self-limiting) — the tail
// fallback stays a mild throttle rather than dropping straight to 0.
const LADDER_MODERATE: TempLadder =
    &[(36.0, 9), (38.0, 8), (40.0, 7), (41.0, 6), (42.0, 4), (45.0, 3), (f32::MAX, 2)];
// Idle, Light: conservative thresholds.
const LADDER_DEFAULT: TempLadder =
    &[(34.0, 9), (36.0, 8), (38.0, 7), (40.0, 6), (43.0, 4), (45.0, 2), (f32::MAX, 0)];

fn action_for_battery_temp(workload_mode: WorkloadMode, batt_temp_c: f32) -> u8 {
    let ladder = match workload_mode {
        WorkloadMode::Benchmark => LADDER_BENCHMARK,
        WorkloadMode::PerfGaming => LADDER_PERFGAMING,
        WorkloadMode::Gaming => LADDER_GAMING,
        WorkloadMode::Moderate => LADDER_MODERATE,
        _ => LADDER_DEFAULT,
    };
    ladder.iter()
        .find(|&&(bound, _)| batt_temp_c < bound)
        .map_or(0, |&(_, action)| action)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelGroup {
    Compute,
    Thermal,
    Charging,
    Display,
}

const ALL_CHANNEL_GROUPS: [ChannelGroup; 4] = [
    ChannelGroup::Compute,
    ChannelGroup::Thermal,
    ChannelGroup::Charging,
    ChannelGroup::Display,
];

impl ChannelGroup {
    fn name(&self) -> &'static str {
        match self {
            ChannelGroup::Compute => "compute",
            ChannelGroup::Thermal => "thermal",
            ChannelGroup::Charging => "charging",
            ChannelGroup::Display => "display",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        match s {
            "compute" => Some(ChannelGroup::Compute),
            "thermal" => Some(ChannelGroup::Thermal),
            "charging" => Some(ChannelGroup::Charging),
            "display" => Some(ChannelGroup::Display),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PidConfig {
    pub ks: Vec<i32>,
    pub ki: Vec<i32>,
    pub kc: Vec<i32>,
    pub max_out: Vec<i32>,
    pub min_out: Vec<i32>,
    pub targets: Vec<i32>,
    pub triggers: Vec<i32>,
}

pub struct CoolingChannel {
    pub name: String,
    pub path: String,
    pub action_type: ActionType,
    pub group: ChannelGroup,
    pub value: i32,
    pub min_val: i32,
    pub max_val: i32,
    pub pid: Option<PidConfig>,
    pub pid_state: SicState,
    pub sensor_name: Option<String>,
}

struct ScenarioModel {
    q_tables: HashMap<ChannelGroup, QTable>,
    tick_count: u64,
    last_state: Option<StateVector>,
    last_actions: HashMap<ChannelGroup, u8>,
    action_stability: f32,
    ticks_same_action: u64,
}

impl ScenarioModel {
    fn new() -> Self {
        ScenarioModel {
            q_tables: HashMap::new(),
            tick_count: 0,
            last_state: None,
            last_actions: HashMap::new(),
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
    // Monotonic tick counter shared across scenario switches — the
    // per-scenario ScenarioModel::tick_count resets when switching to a
    // fresh/less-used scenario, which would make the shared
    // SafetyMonitor's tick-windowed violation tracking underflow.
    global_tick_count: u64,
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
            global_tick_count: 0,
        };
        ctrl.init_channels();
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
        for (&scenario, model) in &mut self.scenario_models {
            let mut groups = serde_json::Map::new();
            for &group in &ALL_CHANNEL_GROUPS {
                let q_table = model.q_tables.entry(group).or_insert_with(QTable::new);
                groups.insert(group.name().to_string(), serde_json::json!({
                    "epsilon": q_table.epsilon(),
                    "weights": q_table.weights().to_vec(),
                }));
            }
            let cp = serde_json::json!({
                "scenario": scenario,
                "tick_count": model.tick_count,
                "groups": groups,
            });
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

        self.global_tick_count += 1;
        let global_tick_count = self.global_tick_count;

        let (group_actions, state, prev_state, prev_actions) = {
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

            state.last_action_compute = model.last_actions.get(&ChannelGroup::Compute).copied().map(|a| a as f32 / 9.0).unwrap_or(0.0);
            state.last_action_thermal = model.last_actions.get(&ChannelGroup::Thermal).copied().map(|a| a as f32 / 9.0).unwrap_or(0.0);
            state.last_action_charging = model.last_actions.get(&ChannelGroup::Charging).copied().map(|a| a as f32 / 9.0).unwrap_or(0.0);
            state.last_action_display = model.last_actions.get(&ChannelGroup::Display).copied().map(|a| a as f32 / 9.0).unwrap_or(0.0);
            state.last_action = state.last_action_compute;
            state.action_stability = model.action_stability;

            let gpu_freq_ratio = crate::ai::WorkloadDetector::read_gpu_freq_ratio();
            state.gpu_freq_ratio = gpu_freq_ratio;

            if let Some(ch) = self.channels.iter().find(|c| c.group == ChannelGroup::Display) {
                let actual_brightness = crate::sensor::sysfs::read_int(&ch.path).max(0);
                state.brightness_ratio = actual_brightness as f32 / ch.max_val.max(1) as f32;
            }
            if let Some(ch) = self.channels.iter().find(|c| c.name == "charge_current") {
                state.charge_current_ratio = ch.value as f32 / ch.max_val.max(1) as f32;
            }

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

            let workload_mode = self.workload_detector.detect_workload(
                state.cpu_load,
                state.cpu_freq_ratio,
                gpu_freq_ratio,
                state.temp_variance,
                state.screen_on > 0.5,
            );

            state.workload_mode = match workload_mode {
                crate::ai::WorkloadMode::Idle => 0.0,
                crate::ai::WorkloadMode::Light => 0.25,
                crate::ai::WorkloadMode::Moderate => 0.5,
                crate::ai::WorkloadMode::Gaming | crate::ai::WorkloadMode::PerfGaming => 0.75,
                crate::ai::WorkloadMode::Benchmark => 1.0,
            };

            let state_array = self.feature_extractor.to_array(&state);

            if self.safety_monitor.is_disabled(global_tick_count) {
                self.enabled = false;
                self.disabled_reason = Some(format!(
                    "disabled after {} safety violations",
                    self.safety_monitor.violations()
                ));
                log_warn!("AI-native: {}", self.disabled_reason.as_ref().unwrap());
                return false;
            }

            // Heavy load is true if:
            // 1. Sustained CPU load >= 3 ticks (sensor-based), OR
            // 2. Workload mode is Gaming/Benchmark (sconfig instant detection)
            let is_heavy_load = self.sustained_load_ticks >= 3
                || matches!(workload_mode, crate::ai::WorkloadMode::Gaming | crate::ai::WorkloadMode::PerfGaming | crate::ai::WorkloadMode::Benchmark);

            let batt_temp = crate::sensor::sysfs::read_int(
                "/sys/class/power_supply/battery/temp"
            );
            let batt_temp_c = batt_temp as f32 / 10.0;

            let mut group_actions = HashMap::new();
            for &group in &[ChannelGroup::Compute, ChannelGroup::Thermal, ChannelGroup::Charging, ChannelGroup::Display] {
                let q_table = model.q_tables.entry(group).or_insert_with(QTable::new);
                let action = q_table.select_action(&state_array);
                let safe = self.safety_monitor.check_action(action, sensor_readings, global_tick_count);
                let mut final_action = if safe { action } else { 3u8 };

                if group == ChannelGroup::Compute {
                    if is_heavy_load && final_action < 9 {
                        final_action = 9;
                    }
                }

                if batt_temp >= 0 {
                    let temp_action = action_for_battery_temp(workload_mode, batt_temp_c);
                    if temp_action < final_action {
                        final_action = temp_action;
                    }
                }
                
                group_actions.insert(group, final_action);
            }

            let prev_state = model.last_state.clone();
            let prev_actions = model.last_actions.clone();
            
            let prev_action_compute = prev_actions.get(&ChannelGroup::Compute).copied();
            let final_action_compute = group_actions.get(&ChannelGroup::Compute).copied().unwrap_or(9);
            if prev_action_compute == Some(final_action_compute) {
                model.ticks_same_action += 1;
            } else {
                model.ticks_same_action = 0;
            }
            model.action_stability = (model.ticks_same_action as f32 / 100.0).min(1.0);
            model.last_state = Some(state.clone());
            for (&k, &v) in &group_actions {
                model.last_actions.insert(k, v);
            }

            (group_actions, state, prev_state, prev_actions)
        };

        self.apply_grouped_actions(&group_actions, sensor_readings);

        if let Some(ref ls) = prev_state {
            let state_array = self.feature_extractor.to_array(ls);
            let next_array = self.feature_extractor.to_array(&state);

            let model = self.scenario_models
                .get_mut(&self.current_scenario)
                .unwrap();
                
            for &group in group_actions.keys() {
                if let Some(&la) = prev_actions.get(&group) {
                    // Credit/blame the action that actually produced this
                    // transition (`la`, taken last tick), not the new action
                    // just selected for the next tick.
                    let reward = self.reward_calculator.compute_group_reward(ls, group, la as i32);
                    let q_table = model.q_tables.entry(group).or_insert_with(QTable::new);
                    q_table.update(&state_array, la, reward, &next_array);
                    if group == ChannelGroup::Compute {
                        self.data_collector.record(ls, la, reward);
                    }
                }
            }
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

    pub fn init_channels(&mut self) {
        self.channels.clear();
        self.discover_hardware_channels();
        self.enrich_from_config();
        
        log_info!("AI-native: discovered {} writable cooling channels", self.channels.len());
        for ch in &self.channels {
            log_info!("AI-native: channel {} group={:?} min={} max={} cur={}",
                ch.name, ch.group, ch.min_val, ch.max_val, ch.value);
        }
    }

    fn discover_hardware_channels(&mut self) {
        // balance_mode: 0 = max cooling, higher = more performance
        if std::path::Path::new("/sys/class/thermal/thermal_message/balance_mode").exists() {
            let cur = crate::sensor::sysfs::read_int(
                "/sys/class/thermal/thermal_message/balance_mode"
            );
            self.channels.push(CoolingChannel {
                name: "balance_mode".into(),
                path: "/sys/class/thermal/thermal_message/balance_mode".into(),
                action_type: ActionType::None,
                group: ChannelGroup::Compute,
                value: cur.max(0).min(9),
                min_val: 0,
                max_val: 9,
                pid: None,
                pid_state: SicState::default(),
                sensor_name: None,
            });
        }

        // boost: 0 = disabled, 1 = enabled
        if std::path::Path::new("/sys/class/thermal/thermal_message/boost").exists() {
            let cur = crate::sensor::sysfs::read_int("/sys/class/thermal/thermal_message/boost");
            self.channels.push(CoolingChannel {
                name: "boost".into(),
                path: "/sys/class/thermal/thermal_message/boost".into(),
                action_type: ActionType::None,
                group: ChannelGroup::Compute,
                value: cur.max(0),
                min_val: 0,
                max_val: 1,
                pid: None,
                pid_state: SicState::default(),
                sensor_name: None,
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
                action_type: ActionType::None,
                group: ChannelGroup::Charging,
                value: if cur > 0 { cur } else { max_val },
                min_val: 500000,
                max_val,
                pid: None,
                pid_state: SicState::default(),
                sensor_name: None,
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
                    action_type: ActionType::CpuFreq,
                    group: ChannelGroup::Compute,
                    value: cur,
                    min_val: hw_max * 3 / 10,
                    max_val: hw_max,
                    pid: None,
                    pid_state: SicState::default(),
                    sensor_name: None,
                });
            }
        }

        // GPU devfreq
        let gpu_path = "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq";
        if std::path::Path::new(gpu_path).exists() {
            let cur = crate::sensor::sysfs::read_int(gpu_path).max(0);
            let available = crate::sensor::sysfs::read_string("/sys/class/kgsl/kgsl-3d0/devfreq/available_frequencies").unwrap_or_default();
            let freqs: Vec<i32> = available.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if !freqs.is_empty() {
                let max_val = *freqs.iter().max().unwrap();
                let min_val = *freqs.iter().min().unwrap();
                self.channels.push(CoolingChannel {
                    name: "gpu".into(),
                    path: gpu_path.into(),
                    action_type: ActionType::GpuBoost,
                    group: ChannelGroup::Compute,
                    value: if cur > 0 { cur } else { max_val },
                    min_val,
                    max_val,
                    pid: None,
                    pid_state: SicState::default(),
                    sensor_name: None,
                });
            }
        }

        // Backlight
        let bl_path = "/sys/class/backlight/panel0-backlight/brightness";
        if std::path::Path::new(bl_path).exists() {
            let cur = crate::sensor::sysfs::read_int(bl_path).max(0);
            let max_val = crate::sensor::sysfs::read_int("/sys/class/backlight/panel0-backlight/max_brightness");
            let max_val = if max_val > 0 { max_val } else { 4095 };
            self.channels.push(CoolingChannel {
                name: "backlight".into(),
                path: bl_path.into(),
                action_type: ActionType::None,
                group: ChannelGroup::Display,
                value: if cur > 0 { cur } else { max_val },
                min_val: 0,
                max_val,
                pid: None,
                pid_state: SicState::default(),
                sensor_name: None,
            });
        }
    }

    fn enrich_from_config(&mut self) {
        let map_content = match crate::config::get_scenario_map_content() {
            Some(c) => c,
            None => return,
        };
        let fname = crate::config::find_scenario_name(&map_content, self.current_scenario);
        let path = match crate::config::resolve_scenario_path(&fname) {
            Some(p) => p,
            None => return,
        };
        
        let blocks = crate::config::parse_config_blocks(&path);
        for b in blocks {
            if b.algo_str.to_lowercase() == "sic" && !b.devices.is_empty() {
                let target_dev = &b.devices[0];
                if let Some(ch) = self.channels.iter_mut().find(|c| &c.name == target_dev) {
                    if !b.threshold.trig.is_empty() {
                        ch.pid = Some(PidConfig {
                            ks: b.threshold.ks.clone(),
                            ki: b.threshold.ki.clone(),
                            kc: b.threshold.kc.clone(),
                            max_out: b.threshold.max_out.clone(),
                            min_out: b.threshold.min_out.clone(),
                            targets: b.threshold.target.clone(),
                            triggers: b.threshold.trig.clone(),
                        });
                        ch.sensor_name = Some(b.sensor_name.clone());
                        if ch.group == ChannelGroup::Compute && ch.name != "gpu" {
                            ch.group = ChannelGroup::Thermal;
                        }
                    }
                }
            }
        }
    }

    fn apply_grouped_actions(&mut self, actions: &HashMap<ChannelGroup, u8>, sensors: &[Sensor]) {
        self.apply_temp_based_charge();

        for ch in &mut self.channels {
            let action = *actions.get(&ch.group).unwrap_or(&9);

            if let Some(ref pid) = ch.pid {
                let setpoint = Self::action_to_setpoint(action, pid);
                let sensor_temp = if let Some(ref name) = ch.sensor_name {
                    sensors.iter().find(|s| &s.name == name).map(|s| s.last_temp_mc.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0)
                } else { 0 };
                
                let output = Self::algo_sic_channel(setpoint, sensor_temp, pid, &mut ch.pid_state, ch.value);
                if output != ch.value {
                    if crate::sensor::sysfs::write_int(&ch.path, output) {
                        ch.value = output;
                        log_debug!("AI-native [PID]: {} = {} (setpoint={} temp={})", ch.name, output, setpoint, sensor_temp);
                    }
                }
                continue;
            }

            if ch.name == "charge_current" || ch.name == "backlight" {
                continue;
            }
            if ch.name == "boost" {
                let boost_val = Self::action_to_channel_value(action, ch);
                crate::sensor::sysfs::write_int(&ch.path, boost_val);
                ch.value = boost_val;
                continue;
            }
            if ch.name.starts_with("cpu_freq") {
                let value = Self::action_to_channel_value(action, ch);
                if let Some(t) = Self::cpu_freq_target_var(&ch.name) {
                    t.store(value, std::sync::atomic::Ordering::Release);
                }
                if crate::sensor::sysfs::write_int(&ch.path, value) {
                    ch.value = value;
                }
                continue;
            }

            let value = Self::action_to_channel_value(action, ch);
            if value != ch.value {
                let ok = crate::sensor::sysfs::write_int(&ch.path, value);
                if ok {
                    ch.value = value;
                }
            }
        }
    }

    fn action_to_setpoint(action: u8, pid: &PidConfig) -> i32 {
        if pid.targets.is_empty() { return 0; }
        // Scale action 0-9 to targets array length
        let idx = (action as usize * pid.targets.len()) / 10;
        let idx = idx.clamp(0, pid.targets.len().saturating_sub(1));
        pid.targets[idx]
    }

    fn algo_sic_channel(setpoint: i32, sensor_temp: i32, pid: &PidConfig, state: &mut SicState, current_val: i32) -> i32 {
        let ek = setpoint - sensor_temp;
        let output_now = if !state.initialized {
            state.ek_1 = ek;
            state.ek_2 = ek;
            state.initialized = true;
            state.initial_value
        } else {
            current_val
        };

        // Determine active segment based on temperature vs triggers
        let mut seg = 0;
        for (i, &trig) in pid.triggers.iter().enumerate() {
            if sensor_temp >= trig {
                seg = i;
            }
        }

        let ks_val = pid.ks.get(seg).copied().unwrap_or(0) as i64;
        let ki_val = pid.ki.get(seg).copied().unwrap_or(0) as i64;
        let kc_val = pid.kc.get(seg).copied().unwrap_or(0) as i64;

        let ek_1 = state.ek_1 as i64;
        let ek_2 = state.ek_2 as i64;
        let ek_i = ek as i64;

        let delta_num = ks_val * (ek_i - 2 * ek_1 + ek_2)
                      + kc_val * (ek_i - ek_1)
                      + ki_val * ek_i;
        let delta = (delta_num as f64 / 1000.0).round() as i64;

        state.ek_2 = state.ek_1;
        state.ek_1 = ek;

        let mut output = (output_now as i64) + delta;
        let max_o = pid.max_out.get(seg).copied().unwrap_or(i32::MAX) as i64 * 1000;
        let min_o = pid.min_out.get(seg).copied().unwrap_or(0) as i64 * 1000;
        output = output.clamp(min_o, max_o);

        output as i32
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
                if action > 9 {
                    return ch.value;
                }
                let range = (ch.max_val - ch.min_val).max(1);
                super::lerp_by_action(action, ch.min_val, range, true)
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
                    struct GroupCheckpoint {
                        weights: Vec<f32>,
                    }
                    #[derive(serde::Deserialize)]
                    struct Checkpoint {
                        #[serde(default)]
                        tick_count: u64,
                        groups: HashMap<String, GroupCheckpoint>,
                    }
                    if let Ok(cp) = serde_json::from_str::<Checkpoint>(&json) {
                        let mut model = ScenarioModel::new();
                        model.tick_count = cp.tick_count;
                        let mut loaded = 0usize;
                        for (name, gcp) in &cp.groups {
                            if let Some(group) = ChannelGroup::from_name(name) {
                                let mut q_table = QTable::new();
                                if q_table.set_weights(&gcp.weights).is_some() {
                                    model.q_tables.insert(group, q_table);
                                    loaded += 1;
                                }
                            }
                        }
                        if loaded > 0 {
                            self.scenario_models.insert(scenario_id, model);
                            log_info!("AI-native: loaded {} Q-table(s) for scenario {}",
                                loaded, scenario_id);
                        }
                    }
                }
            }
        }
    }
}
