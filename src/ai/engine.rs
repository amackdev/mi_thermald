use crate::action::{cpuinfo_max_path, cpuinfo_min_path};
use crate::types::{Action, ActionType, Instance, Sensor};

use super::data_collector::DataCollector;
use super::features::{FeatureExtractor, StateVector};
use super::qtable::QTable;
use super::rewards::RewardCalculator;
use super::safety::SafetyMonitor;
use super::workload::WorkloadDetector;

const SAVE_INTERVAL_TICKS: u64 = 1000;

pub struct AIDirective {
    pub level: i32,
    pub actions: Vec<Action>,
}

pub struct AIEngine {
    q_table: QTable,
    feature_extractor: FeatureExtractor,
    reward_calculator: RewardCalculator,
    safety_monitor: SafetyMonitor,
    data_collector: DataCollector,
    workload_detector: WorkloadDetector,

    last_state: Option<StateVector>,
    last_directive: Option<AIDirective>,
    last_action: Option<u8>,
    last_traditional_level: i32,
    last_max_level: i32,
    enabled: bool,
    tick_count: u64,
    disabled_reason: Option<String>,
}

impl AIEngine {
    pub fn new() -> Result<Self, String> {
        let mut engine = AIEngine {
            q_table: QTable::new(),
            feature_extractor: FeatureExtractor::new(),
            reward_calculator: RewardCalculator::new(),
            safety_monitor: SafetyMonitor::new(),
            data_collector: DataCollector::new(),
            workload_detector: WorkloadDetector::new(),

            last_state: None,
            last_directive: None,
            last_action: None,
            last_traditional_level: 0,
            last_max_level: 0,
            enabled: true,
            tick_count: 0,
            disabled_reason: None,
        };

        if let Some(weights) = engine.data_collector.load_qtable() {
            if weights.len() == engine.q_table.weights().len() {
                let _ = engine.q_table.set_weights(&weights);
                log_info!("AI: loaded Q-table from disk ({} weights)", weights.len());
            } else {
                log_warn!("AI: saved Q-table size mismatch, starting fresh");
            }
        } else {
            log_info!("AI: no saved Q-table found, starting fresh");
        }

        Ok(engine)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn disabled_reason(&self) -> Option<&str> {
        self.disabled_reason.as_deref()
    }

    pub fn collect_state(
        &mut self,
        sensors: &[Sensor],
        instance: &Instance,
        traditional_level: i32,
    ) -> StateVector {
        self.feature_extractor.extract(sensors, instance, traditional_level)
    }

    pub fn decide_action(
        &mut self,
        state: &StateVector,
        traditional_level: i32,
        instance: &Instance,
        sensors: &[Sensor],
    ) -> AIDirective {
        if !self.enabled {
            return AIDirective {
                level: traditional_level,
                actions: instance.actions.clone(),
            };
        }

        // Detect workload mode
        let gpu_freq_ratio = WorkloadDetector::read_gpu_freq_ratio();
        let workload_mode = self.workload_detector.detect_workload(
            state.cpu_load,
            state.cpu_freq_ratio,
            gpu_freq_ratio,
            state.temp_variance,
            state.screen_on > 0.5,
        );

        // Update safety and reward calculators with current workload
        self.safety_monitor.set_workload_mode(workload_mode);
        self.reward_calculator.set_workload_mode(workload_mode);

        // Update state vector with workload mode
        let mut state = state.clone();
        state.workload_mode = workload_mode.to_normalized();

        let state_array = self.feature_extractor.to_array(&state);
        let action = self.q_table.select_action(&state_array);

        let safe = self
            .safety_monitor
            .check_action(action, sensors, self.tick_count);

        let final_action = if safe {
            action
        } else {
            log_warn!("AI: action {} rejected by safety monitor, falling back", action);
            let traditional_map = 3u8;
            let _ = self.safety_monitor.check_action(traditional_map, sensors, self.tick_count);
            traditional_map
        };

        let max_level = instance.threshold.n_levels() as i32;

        if self.safety_monitor.is_disabled(self.tick_count) {
            self.enabled = false;
            self.disabled_reason = Some(format!(
                "disabled after {} safety violations",
                self.safety_monitor.violations()
            ));
            log_warn!("AI: {}", self.disabled_reason.as_ref().unwrap());
            return AIDirective {
                level: traditional_level,
                actions: instance.actions.clone(),
            };
        }

        let ai_level = self.action_to_level(final_action, traditional_level, max_level);

        if ai_level != traditional_level {
            log_debug!(
                "AI: action={} trad={} ai={} epsilon={:.3}",
                final_action, traditional_level, ai_level, self.q_table.epsilon()
            );
        }

        let direct_actions = self.build_direct_actions(final_action, ai_level, instance);

        self.last_state = Some(state);
        self.last_directive = Some(AIDirective {
            level: ai_level,
            actions: direct_actions.clone(),
        });
        self.last_action = Some(final_action);
        self.last_traditional_level = traditional_level;
        self.last_max_level = max_level;

        AIDirective {
            level: ai_level,
            actions: direct_actions,
        }
    }

    fn build_direct_actions(&self, action: u8, ai_level: i32, instance: &Instance) -> Vec<Action> {
        let n_levels = instance.threshold.n_levels();
        let effective_levels = if n_levels > 0 { n_levels + 1 } else { 1 };
        let per_dev = if effective_levels > 0 {
            instance.actions.len() / effective_levels
        } else {
            0
        };
        let start = (ai_level as usize) * per_dev;
        let mut end = start + per_dev;
        if end > instance.actions.len() {
            end = instance.actions.len();
        }

        let mut out = Vec::with_capacity(end.saturating_sub(start));
        for cfg_action in &instance.actions[start..end] {
            let value = match cfg_action.type_ {
                ActionType::CpuFreq => self.action_to_cpu_freq(action, &cfg_action.target),
                ActionType::CpuHotplug => {
                    if action == 0 { 0 } else { 1 }
                }
                ActionType::GpuBoost => {
                    if action <= 1 { 0 } else { 1_100_000_000 }
                }
                _ => {
                    if cfg_action.value == 0 {
                        self.resolve_zero_value(cfg_action)
                    } else {
                        cfg_action.value
                    }
                }
            };
            out.push(Action {
                type_: cfg_action.type_,
                target: cfg_action.target.clone(),
                value,
            });
        }
        out
    }

    fn action_to_cpu_freq(&self, action: u8, target: &str) -> i32 {
        let max_freq = {
            let v = crate::sensor::sysfs::read_int(&cpuinfo_max_path(target));
            if v > 0 { v } else { 3000000 }
        };
        let min_freq = {
            let v = crate::sensor::sysfs::read_int(&cpuinfo_min_path(target));
            if v > 0 { v } else { max_freq / 10 }
        };
        let range = (max_freq - min_freq).max(1);
        // action 0 = most cooling = min_freq
        // action 9 = performance = max_freq
        let result = min_freq + (action as i64 * range as i64 / 9) as i32;
        result.clamp(min_freq, max_freq)
    }

    fn resolve_zero_value(&self, action: &Action) -> i32 {
        match action.type_ {
            ActionType::CpuFreq => {
                let v = crate::sensor::sysfs::read_int(&cpuinfo_max_path(&action.target));
                if v > 0 { v } else { 3000000 }
            }
            ActionType::Bcl => {
                let v = crate::sensor::sysfs::read_int(
                    "/sys/class/power_supply/battery/constant_charge_current"
                );
                if v > 0 { v } else { 5000000 }
            }
            ActionType::Fcc => {
                let v = crate::sensor::sysfs::read_int(
                    "/sys/class/power_supply/battery/constant_charge_current_max"
                );
                if v > 0 { v } else { 6000000 }
            }
            _ => action.value,
        }
    }

    pub fn end_tick(&mut self, sensors: &[Sensor]) {
        if !self.enabled {
            return;
        }

        if let (Some(ref last_state), Some(last_action)) =
            (self.last_state.clone(), self.last_action)
        {
            let prev_level = self.last_traditional_level;
            let max_level = self.last_max_level.max(1);

            let next_state = self.build_state_from_sensors(sensors, prev_level, max_level);
            let new_level = self.last_directive.as_ref().map_or(
                self.action_to_level(last_action, prev_level, max_level),
                |d| d.level,
            );

            let reward = self
                .reward_calculator
                .compute_reward(last_state, prev_level, new_level);

            let state_array = self.feature_extractor.to_array(last_state);
            let next_state_array = self.feature_extractor.to_array(&next_state);

            self.q_table.update(&state_array, last_action, reward, &next_state_array);
            self.data_collector.record(last_state, last_action, reward);

            self.tick_count += 1;

            if self.tick_count % SAVE_INTERVAL_TICKS == 0 {
                self.data_collector.save_qtable(self.q_table.weights());
            }
        }

        self.last_state = None;
        self.last_directive = None;
        self.last_action = None;
    }

    pub fn save_checkpoint(&mut self) {
        self.data_collector.flush();
        self.data_collector.save_qtable(self.q_table.weights());
    }

    fn build_state_from_sensors(
        &mut self,
        sensors: &[Sensor],
        traditional_level: i32,
        _max_level: i32,
    ) -> StateVector {
        let instance = Instance {
            name: String::new(),
            sensor_idx: None,
            algo: crate::types::AlgoType::Monitor,
            sample_ms: 0,
            reverse: false,
            threshold: crate::types::Threshold::default(),
            actions: Vec::new(),
            current_level: 0,
            current_value: 0,
        };

        self.feature_extractor.extract(sensors, &instance, traditional_level)
    }

    fn action_to_level(&self, action: u8, trad_level: i32, max_level: i32) -> i32 {
        let max_level = max_level.max(1);
        if action > 9 {
            return trad_level.max(0).min(max_level);
        }
        super::lerp_by_action(action, 0, max_level, false)
    }
}
