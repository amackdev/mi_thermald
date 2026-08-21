use std::sync::atomic::Ordering;

use crate::types::Sensor;
use super::workload::WorkloadMode;

const MAX_CPU_TEMP_MC: i32 = 85000;

// Context-dependent battery thresholds
const BATTERY_TEMP_IDLE_MC: i32 = 38000;       // 38°C when idle/light
const BATTERY_TEMP_MODERATE_MC: i32 = 40000;   // 40°C moderate load
const BATTERY_TEMP_PERFGAMING_MC: i32 = 45000; // 45°C gaming
const BATTERY_TEMP_GAMING_MC: i32 = 44000;     // 44°C gaming
const BATTERY_TEMP_BENCHMARK_MC: i32 = 48000;  // 48°C benchmark/stress test

const BATTERY_WARN_IDLE_MC: i32 = 35000;       // 35°C warning (idle)
const BATTERY_WARN_MODERATE_MC: i32 = 37000;   // 37°C warning (moderate)
const BATTERY_WARN_PERFGAMING_MC: i32 = 44000; // 44°C warning (perfgaming)
const BATTERY_WARN_GAMING_MC: i32 = 42000;     // 42°C warning (gaming)
const BATTERY_WARN_BENCHMARK_MC: i32 = 45000;  // 45°C warning (benchmark)

const MAX_VIOLATIONS: u32 = 10;
const VIOLATION_WINDOW_TICKS: u64 = 3600;

pub struct SafetyMonitor {
    violations: u32,
    last_violation_tick: u64,
    current_workload: WorkloadMode,
}

impl SafetyMonitor {
    pub fn new() -> Self {
        SafetyMonitor {
            violations: 0,
            last_violation_tick: 0,
            current_workload: WorkloadMode::Light,
        }
    }

    pub fn set_workload_mode(&mut self, mode: WorkloadMode) {
        if mode != self.current_workload {
            log_debug!("Safety: workload mode → {:?}", mode);
            self.current_workload = mode;
        }
    }

    /// Pure safety predicate — does not record a violation. Actions 0/1 are
    /// the lowest-frequency cooling actions and are always safe; 6-9 are the
    /// high/max-performance actions (see the action→frequency mapping in the
    /// README) and must never bypass the cpu/battery checks, since those are
    /// exactly the actions that would make an active thermal emergency worse.
    pub fn is_action_safe(&self, action: u8, sensors: &[Sensor]) -> bool {
        let cpu_over = self.cpu_over_limit(sensors);
        let battery_over = self.battery_over_limit(sensors);
        let battery_warm = self.battery_warm(sensors);

        match action {
            0 | 1 => true,
            6 | 7 | 8 | 9 => !cpu_over && !battery_over,
            _ => {
                // Mid-range actions: block if battery warming
                if battery_warm && action >= 5 {
                    false
                } else {
                    !cpu_over && !battery_over
                }
            }
        }
    }

    pub fn record_check(&mut self, safe: bool, tick_count: u64) {
        if !safe {
            self.violations = self.violations.saturating_add(1);
            self.last_violation_tick = tick_count;
        } else if self.violations > 0 && tick_count - self.last_violation_tick > VIOLATION_WINDOW_TICKS {
            self.violations = 0;
        }
    }

    pub fn check_action(&mut self, action: u8, sensors: &[Sensor], tick_count: u64) -> bool {
        let safe = self.is_action_safe(action, sensors);
        self.record_check(safe, tick_count);
        safe
    }

    pub fn is_disabled(&self, tick_count: u64) -> bool {
        if self.violations >= MAX_VIOLATIONS {
            if tick_count - self.last_violation_tick < VIOLATION_WINDOW_TICKS {
                return true;
            }
        }
        false
    }

    pub fn violations(&self) -> u32 {
        self.violations
    }

    /// True only for a genuine thermal emergency (CPU actually near its
    /// redline) — as opposed to `is_action_safe` returning false, which also
    /// fires routinely whenever the battery is merely above its context
    /// warn/crit line and just means "the RL's proposed action got capped,"
    /// not that anything dangerous happened. Only this should count toward
    /// the violation counter that can permanently disable the controller;
    /// otherwise an ordinary idle battery temp a fraction of a degree over
    /// BATTERY_TEMP_IDLE_MC trips 10 "violations" in ~10 seconds even though
    /// the safety layer capped every action correctly the whole time.
    pub fn is_hazard(&self, sensors: &[Sensor]) -> bool {
        self.cpu_over_limit(sensors)
    }

    fn cpu_over_limit(&self, sensors: &[Sensor]) -> bool {
        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            (name.contains("cpu") || name.contains("tsens"))
                && s.last_temp_mc.load(Ordering::Relaxed) > MAX_CPU_TEMP_MC
        })
    }

    /// (warn_mc, crit_mc) battery temperature thresholds for the current workload.
    fn battery_thresholds(&self) -> (i32, i32) {
        match self.current_workload {
            WorkloadMode::Idle | WorkloadMode::Light => (BATTERY_WARN_IDLE_MC, BATTERY_TEMP_IDLE_MC),
            WorkloadMode::Moderate => (BATTERY_WARN_MODERATE_MC, BATTERY_TEMP_MODERATE_MC),
            WorkloadMode::Gaming => (BATTERY_WARN_GAMING_MC, BATTERY_TEMP_GAMING_MC),
            WorkloadMode::PerfGaming => (BATTERY_WARN_PERFGAMING_MC, BATTERY_TEMP_PERFGAMING_MC),
            WorkloadMode::Benchmark => (BATTERY_WARN_BENCHMARK_MC, BATTERY_TEMP_BENCHMARK_MC),
        }
    }

    fn battery_over_limit(&self, sensors: &[Sensor]) -> bool {
        let (_, crit) = self.battery_thresholds();
        Self::any_battery_sensor_over(sensors, crit)
    }

    fn battery_warm(&self, sensors: &[Sensor]) -> bool {
        let (warn, _) = self.battery_thresholds();
        Self::any_battery_sensor_over(sensors, warn)
    }

    fn any_battery_sensor_over(sensors: &[Sensor], threshold: i32) -> bool {
        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            name == "battery"
                && s.last_temp_mc.load(Ordering::Relaxed) > threshold
        })
    }
}
