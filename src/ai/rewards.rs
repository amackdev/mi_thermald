use super::features::StateVector;
use super::workload::WorkloadMode;

const W_TEMP: f32 = 5.0;
const W_PERF: f32 = 1.0;
const W_BATT: f32 = 0.5;
const W_STAB: f32 = 0.3;
const W_BATT_TEMP: f32 = 4.0;

pub struct RewardCalculator {
    w_temp: f32,
    w_perf: f32,
    w_batt: f32,
    w_stab: f32,
    w_batt_temp: f32,
    current_workload: WorkloadMode,
}

impl RewardCalculator {
    pub fn new() -> Self {
        RewardCalculator {
            w_temp: W_TEMP,
            w_perf: W_PERF,
            w_batt: W_BATT,
            w_stab: W_STAB,
            w_batt_temp: W_BATT_TEMP,
            current_workload: WorkloadMode::Light,
        }
    }

    pub fn set_workload_mode(&mut self, mode: WorkloadMode) {
        self.current_workload = mode;
    }

    pub fn compute_group_reward(&self, state: &StateVector, group: crate::ai::native::ChannelGroup, action: i32) -> f32 {
        let temp_penalty = self.compute_temp_reward(state.t_cpu_max);
        let batt_temp_penalty = self.compute_batt_temp_reward(state.t_battery, state.dt_battery);
        
        let prev_action = match group {
            crate::ai::native::ChannelGroup::Compute => state.last_action_compute * 9.0,
            crate::ai::native::ChannelGroup::Thermal => state.last_action_thermal * 9.0,
            crate::ai::native::ChannelGroup::Charging => state.last_action_charging * 9.0,
            crate::ai::native::ChannelGroup::Display => state.last_action_display * 9.0,
        };
        let stab_penalty = (action as f32 - prev_action).abs();

        let (perf_weight, batt_temp_weight) = match self.current_workload {
            WorkloadMode::Idle => (0.5, 5.0),
            WorkloadMode::Light => (1.0, 4.0),
            WorkloadMode::Moderate => (1.5, 3.5),
            WorkloadMode::Gaming => (3.0, 2.0),
            WorkloadMode::PerfGaming => (3.5, 1.75),
            WorkloadMode::Benchmark => (4.0, 1.5),
        };

        match group {
            crate::ai::native::ChannelGroup::Compute => {
                let perf_reward = state.cpu_freq_ratio;
                self.w_temp * temp_penalty + perf_weight * perf_reward - self.w_stab * stab_penalty
            }
            crate::ai::native::ChannelGroup::Thermal => {
                self.w_temp * temp_penalty - self.w_stab * stab_penalty
            }
            crate::ai::native::ChannelGroup::Charging => {
                let charge_speed_reward = state.charge_current_ratio;
                batt_temp_weight * batt_temp_penalty + self.w_batt * charge_speed_reward - self.w_stab * stab_penalty
            }
            crate::ai::native::ChannelGroup::Display => {
                let brightness_reward = state.brightness_ratio;
                self.w_temp * temp_penalty * 0.5 + 2.0 * brightness_reward - self.w_stab * stab_penalty
            }
        }
    }

    pub fn compute_reward(&self, state: &StateVector, prev_level: i32, new_level: i32) -> f32 {
        let temp_reward = self.compute_temp_reward(state.t_cpu_max);
        let batt_temp_reward = self.compute_batt_temp_reward(state.t_battery, state.dt_battery);
        let perf_reward = state.cpu_freq_ratio;
        let batt_reward = if state.is_charging > 0.5 {
            state.battery_current
        } else {
            0.0
        };
        let stab_penalty = (new_level - prev_level).abs() as f32;

        let (perf_weight, batt_temp_weight) = match self.current_workload {
            WorkloadMode::Idle => (0.5, 5.0),
            WorkloadMode::Light => (1.0, 4.0),
            WorkloadMode::Moderate => (1.5, 3.5),
            WorkloadMode::Gaming => (3.0, 2.0),
            WorkloadMode::PerfGaming => (3.5, 1.75),
            WorkloadMode::Benchmark => (4.0, 1.5),
        };

        self.w_temp * temp_reward
            + batt_temp_weight * batt_temp_reward
            + perf_weight * perf_reward
            + self.w_batt * batt_reward
            - self.w_stab * stab_penalty
    }



    fn compute_temp_reward(&self, t_cpu: f32) -> f32 {
        let temp_c = t_cpu * 100.0;
        if temp_c > 85.0 {
            -10.0
        } else if temp_c > 75.0 {
            -0.1 * (temp_c - 75.0)
        } else {
            0.0
        }
    }

    fn compute_batt_temp_reward(&self, t_battery: f32, dt_battery: f32) -> f32 {
        let temp_c = t_battery * 100.0;

        // Context-dependent thresholds
        let (warn_temp, crit_temp) = match self.current_workload {
            WorkloadMode::Idle | WorkloadMode::Light => (35.0, 38.0),
            WorkloadMode::Moderate => (37.0, 40.0),
            WorkloadMode::Gaming => (42.0, 44.0),
            WorkloadMode::PerfGaming => (44.0, 45.0),
            WorkloadMode::Benchmark => (45.0, 48.0),
        };

        // Penalize high battery temps
        let static_penalty = if temp_c > crit_temp {
            -10.0  // Critical
        } else if temp_c > warn_temp {
            -0.2 * (temp_c - warn_temp)  // Warning zone
        } else if temp_c > warn_temp - 3.0 {
            -0.05 * (temp_c - (warn_temp - 3.0))  // Slight penalty
        } else {
            0.0
        };

        // Heating penalty - stronger for non-gaming workloads
        let heating_penalty = if dt_battery > 0.0 {
            let penalty_scale = match self.current_workload {
                WorkloadMode::Gaming | WorkloadMode::PerfGaming | WorkloadMode::Benchmark => 1.0,
                _ => 2.0,  // Double penalty for heating during light loads
            };
            -penalty_scale * 2.0 * dt_battery
        } else {
            0.5 * dt_battery.abs()  // Small reward for cooling
        };

        static_penalty + heating_penalty
    }
}
