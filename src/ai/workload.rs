// Workload detection for context-aware thermal management
// Detects gaming, benchmark, and normal usage patterns

use crate::thermal_profile::ThermalProfileManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadMode {
    Idle,       // Screen off or very low activity
    Light,      // Browsing, messaging
    Moderate,   // Video playback, light apps
    Gaming,     // High sustained load
    PerfGaming, // Perf High sustained load
    Benchmark,  // Extreme synthetic load
}

impl WorkloadMode {
    pub fn to_normalized(&self) -> f32 {
        match self {
            WorkloadMode::Idle => 0.0,
            WorkloadMode::Light => 0.25,
            WorkloadMode::Moderate => 0.5,
            WorkloadMode::Gaming => 0.65,
            WorkloadMode::PerfGaming => 0.75,
            WorkloadMode::Benchmark => 1.0,
        }
    }
}

pub struct WorkloadDetector {
    // Detection state
    high_load_ticks: u64,
    last_mode: WorkloadMode,

    // Hysteresis to prevent mode flapping
    ticks_in_current_mode: u64,
    mode_switch_threshold: u64,

    // NEW: Thermal profile manager for instant detection
    profile_manager: ThermalProfileManager,
    sconfig_available: bool,

    // Flag to bypass hysteresis after sconfig reset
    force_mode_update: bool,
}

impl WorkloadDetector {
    pub fn new() -> Self {
        let sconfig_available = ThermalProfileManager::is_sconfig_available();

        if sconfig_available {
            log_info!("Thermal sconfig node available - instant workload detection enabled");
        } else {
            log_info!("Thermal sconfig node not available - using sensor-based detection");
        }

        WorkloadDetector {
            high_load_ticks: 0,
            last_mode: WorkloadMode::Light,
            ticks_in_current_mode: 0,
            mode_switch_threshold: 10,  // 10 seconds before mode switch
            profile_manager: ThermalProfileManager::new(),
            sconfig_available,
            force_mode_update: false,
        }
    }

    /// Enhanced workload detection with thermal profile support
    /// Checks sconfig node first for instant detection, falls back to sensor-based
    pub fn detect_workload(
        &mut self,
        cpu_load: f32,
        cpu_freq_ratio: f32,
        temp_variance: f32,
        screen_on: bool,
    ) -> WorkloadMode {
        // Priority 1: Check thermal profile from sconfig (instant, 100% accurate)
        if self.sconfig_available {
            if let Some(profile_id) = self.profile_manager.read_sconfig() {
                // Profile 0 means no thermal profile active - reset to sensor detection
                if profile_id == 0 {
                    if self.last_mode == WorkloadMode::Gaming || self.last_mode == WorkloadMode::Benchmark {
                        log_debug!("Thermal profile 0: resetting {:?} -> sensor detection", self.last_mode);
                    }
                    // Reset counters to allow fresh sensor-based detection
                    self.high_load_ticks = 0;
                    self.ticks_in_current_mode = 0;
                    self.force_mode_update = true;  // Bypass hysteresis on next update
                    // Fall through to sensor detection
                } else if let Some(mode) = self.profile_manager.profile_to_workload(profile_id) {
                    log_debug!("Workload from thermal profile {}: {:?}", profile_id, mode);
                    // Trust sconfig immediately without hysteresis
                    if mode != self.last_mode {
                        log_debug!("Workload mode changed (sconfig): {:?} -> {:?}", self.last_mode, mode);
                        self.last_mode = mode;
                        self.ticks_in_current_mode = 0;
                    }
                    return mode;
                }
            }
        }

        // Priority 2: Fall back to sensor-based detection
        self.detect_workload_sensors(cpu_load, cpu_freq_ratio, temp_variance, screen_on)
    }

    /// Original sensor-based workload detection
    fn detect_workload_sensors(
        &mut self,
        cpu_load: f32,
        cpu_freq_ratio: f32,
        temp_variance: f32,
        screen_on: bool,
    ) -> WorkloadMode {
        // Reset counters if screen off
        if !screen_on {
            self.high_load_ticks = 0;
            return self.update_mode(WorkloadMode::Idle);
        }

        // Detect sustained high CPU demand (frequency + load)
        let high_cpu = cpu_load > 0.5 && cpu_freq_ratio > 0.7;
        let high_variance = temp_variance > 0.5;

        // Update counter
        if high_cpu {
            self.high_load_ticks += 1;
        } else {
            self.high_load_ticks = self.high_load_ticks.saturating_sub(2);
        }

        // Determine workload mode with hysteresis
        let detected_mode = if self.high_load_ticks > 30 && high_variance {
            // 30+ seconds of sustained high CPU → Gaming
            WorkloadMode::Gaming
        } else if self.high_load_ticks > 60 && cpu_load > 0.8 {
            // 60+ seconds extreme load → Benchmark
            WorkloadMode::Benchmark
        } else if cpu_load > 0.3 {
            // Moderate activity
            WorkloadMode::Moderate
        } else if cpu_load > 0.1 {
            // Light activity
            WorkloadMode::Light
        } else {
            // Very low activity
            WorkloadMode::Idle
        };

        self.update_mode(detected_mode)
    }

    fn update_mode(&mut self, new_mode: WorkloadMode) -> WorkloadMode {
        if new_mode == self.last_mode {
            self.ticks_in_current_mode += 1;
            self.force_mode_update = false;  // Clear force flag
            new_mode
        } else {
            // Force update if flag is set (after sconfig reset)
            if self.force_mode_update {
                self.ticks_in_current_mode = 0;
                self.last_mode = new_mode;
                self.force_mode_update = false;
                log_debug!("Workload mode changed (forced): {:?}", new_mode);
                return new_mode;
            }

            // Mode change detected, but wait for threshold (hysteresis)
            if self.ticks_in_current_mode < self.mode_switch_threshold {
                self.ticks_in_current_mode = 0;
                self.last_mode  // Stay in old mode (hysteresis)
            } else {
                // Confirmed mode change
                self.ticks_in_current_mode = 0;
                self.last_mode = new_mode;
                log_debug!("Workload mode changed: {:?}", new_mode);
                new_mode
            }
        }
    }
}
