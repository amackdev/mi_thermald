use std::sync::atomic::Ordering;

use crate::types::Sensor;
use super::workload::WorkloadMode;

const MAX_CPU_TEMP_MC: i32 = 85000;

// Context-dependent battery thresholds
const BATTERY_TEMP_IDLE_MC: i32 = 38000;       // 38°C when idle/light
const BATTERY_TEMP_MODERATE_MC: i32 = 40000;   // 40°C moderate load
const BATTERY_TEMP_GAMING_MC: i32 = 45000;     // 45°C gaming
const BATTERY_TEMP_BENCHMARK_MC: i32 = 48000;  // 48°C benchmark/stress test

const BATTERY_WARN_IDLE_MC: i32 = 35000;       // 35°C warning (idle)
const BATTERY_WARN_MODERATE_MC: i32 = 37000;   // 37°C warning (moderate)
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

    pub fn check_action(&mut self, action: u8, sensors: &[Sensor], tick_count: u64) -> bool {
        let cpu_over = self.cpu_over_limit(sensors);
        let battery_over = self.battery_over_limit(sensors);
        let battery_warm = self.battery_warm(sensors);

        let safe = match action {
            0 | 1 | 8 | 9 => true,  // Cooling/emergency always OK
            6 | 7 => !cpu_over && !battery_over,  // High perf needs both safe
            _ => {
                // Mid-range actions: block if battery warming
                if battery_warm && action >= 5 {
                    false
                } else {
                    !cpu_over && !battery_over
                }
            }
        };

        if !safe {
            self.violations = self.violations.saturating_add(1);
            self.last_violation_tick = tick_count;
        } else if self.violations > 0 && tick_count - self.last_violation_tick > VIOLATION_WINDOW_TICKS {
            self.violations = 0;
        }

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

    fn cpu_over_limit(&self, sensors: &[Sensor]) -> bool {
        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            (name.contains("cpu") || name.contains("tsens"))
                && s.last_temp_mc.load(Ordering::Relaxed) > MAX_CPU_TEMP_MC
        })
    }

    fn battery_over_limit(&self, sensors: &[Sensor]) -> bool {
        let threshold = match self.current_workload {
            WorkloadMode::Idle => BATTERY_TEMP_IDLE_MC,
            WorkloadMode::Light => BATTERY_TEMP_IDLE_MC,
            WorkloadMode::Moderate => BATTERY_TEMP_MODERATE_MC,
            WorkloadMode::Gaming => BATTERY_TEMP_GAMING_MC,
            WorkloadMode::Benchmark => BATTERY_TEMP_BENCHMARK_MC,
        };

        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            name.contains("battery")
                && s.last_temp_mc.load(Ordering::Relaxed) > threshold
        })
    }

    fn battery_warm(&self, sensors: &[Sensor]) -> bool {
        let threshold = match self.current_workload {
            WorkloadMode::Idle => BATTERY_WARN_IDLE_MC,
            WorkloadMode::Light => BATTERY_WARN_IDLE_MC,
            WorkloadMode::Moderate => BATTERY_WARN_MODERATE_MC,
            WorkloadMode::Gaming => BATTERY_WARN_GAMING_MC,
            WorkloadMode::Benchmark => BATTERY_WARN_BENCHMARK_MC,
        };

        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            name.contains("battery")
                && s.last_temp_mc.load(Ordering::Relaxed) > threshold
        })
    }

    pub fn get_battery_threshold(&self) -> i32 {
        match self.current_workload {
            WorkloadMode::Idle | WorkloadMode::Light => BATTERY_TEMP_IDLE_MC,
            WorkloadMode::Moderate => BATTERY_TEMP_MODERATE_MC,
            WorkloadMode::Gaming => BATTERY_TEMP_GAMING_MC,
            WorkloadMode::Benchmark => BATTERY_TEMP_BENCHMARK_MC,
        }
    }
}
