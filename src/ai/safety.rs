use std::sync::atomic::Ordering;

use crate::types::Sensor;

const MAX_CPU_TEMP_MC: i32 = 85000;
const MAX_BATTERY_TEMP_MC: i32 = 45000;
const MAX_VIOLATIONS: u32 = 10;
const VIOLATION_WINDOW_TICKS: u64 = 3600;

pub struct SafetyMonitor {
    violations: u32,
    last_violation_tick: u64,
}

impl SafetyMonitor {
    pub fn new() -> Self {
        SafetyMonitor {
            violations: 0,
            last_violation_tick: 0,
        }
    }

    pub fn check_action(&mut self, action: u8, sensors: &[Sensor], tick_count: u64) -> bool {
        let cpu_over = self.cpu_over_limit(sensors);
        let battery_over = self.battery_over_limit(sensors);

        let safe = match action {
            0 | 1 | 8 | 9 => true,
            6 => !cpu_over,
            _ => !cpu_over && !battery_over,
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
        sensors.iter().any(|s| {
            let name = s.name.to_lowercase();
            name.contains("battery")
                && s.last_temp_mc.load(Ordering::Relaxed) > MAX_BATTERY_TEMP_MC
        })
    }
}
