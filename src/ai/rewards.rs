use super::features::StateVector;

const W_TEMP: f32 = 5.0;
const W_PERF: f32 = 1.0;
const W_BATT: f32 = 0.5;
const W_STAB: f32 = 0.3;

pub struct RewardCalculator {
    w_temp: f32,
    w_perf: f32,
    w_batt: f32,
    w_stab: f32,
}

impl RewardCalculator {
    pub fn new() -> Self {
        RewardCalculator {
            w_temp: W_TEMP,
            w_perf: W_PERF,
            w_batt: W_BATT,
            w_stab: W_STAB,
        }
    }

    pub fn compute_reward(&self, state: &StateVector, prev_level: i32, new_level: i32) -> f32 {
        let temp_reward = self.compute_temp_reward(state.t_cpu_max);
        let perf_reward = state.cpu_freq_ratio;
        let batt_reward = if state.is_charging > 0.5 {
            state.battery_current
        } else {
            0.0
        };
        let stab_penalty = (new_level - prev_level).abs() as f32;

        self.w_temp * temp_reward
            + self.w_perf * perf_reward
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
}
