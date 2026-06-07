// Workload detection for context-aware thermal management
// Detects gaming, benchmark, and normal usage patterns

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadMode {
    Idle,       // Screen off or very low activity
    Light,      // Browsing, messaging
    Moderate,   // Video playback, light apps
    Gaming,     // High sustained load
    Benchmark,  // Extreme synthetic load
}

impl WorkloadMode {
    pub fn to_normalized(&self) -> f32 {
        match self {
            WorkloadMode::Idle => 0.0,
            WorkloadMode::Light => 0.25,
            WorkloadMode::Moderate => 0.5,
            WorkloadMode::Gaming => 0.75,
            WorkloadMode::Benchmark => 1.0,
        }
    }
}

pub struct WorkloadDetector {
    // Detection state
    high_load_ticks: u64,
    sustained_gpu_ticks: u64,
    last_mode: WorkloadMode,

    // Hysteresis to prevent mode flapping
    ticks_in_current_mode: u64,
    mode_switch_threshold: u64,
}

impl WorkloadDetector {
    pub fn new() -> Self {
        WorkloadDetector {
            high_load_ticks: 0,
            sustained_gpu_ticks: 0,
            last_mode: WorkloadMode::Light,
            ticks_in_current_mode: 0,
            mode_switch_threshold: 10,  // 10 seconds before mode switch
        }
    }

    pub fn detect_workload(
        &mut self,
        cpu_load: f32,
        cpu_freq_ratio: f32,
        gpu_freq_ratio: f32,
        temp_variance: f32,
        screen_on: bool,
    ) -> WorkloadMode {
        // Reset counters if screen off
        if !screen_on {
            self.high_load_ticks = 0;
            self.sustained_gpu_ticks = 0;
            return self.update_mode(WorkloadMode::Idle);
        }

        // Detect gaming patterns
        let high_cpu = cpu_load > 0.5 && cpu_freq_ratio > 0.7;
        let high_gpu = gpu_freq_ratio > 0.7;
        let high_variance = temp_variance > 0.5;

        // Update counters
        if high_cpu && high_gpu {
            self.high_load_ticks += 1;
            self.sustained_gpu_ticks += 1;
        } else {
            self.high_load_ticks = self.high_load_ticks.saturating_sub(2);
            self.sustained_gpu_ticks = self.sustained_gpu_ticks.saturating_sub(1);
        }

        // Determine workload mode with hysteresis
        let detected_mode = if self.sustained_gpu_ticks > 30 && high_variance {
            // 30+ seconds of sustained GPU + CPU → Gaming
            WorkloadMode::Gaming
        } else if self.high_load_ticks > 60 && cpu_load > 0.8 {
            // 60+ seconds extreme load → Benchmark
            WorkloadMode::Benchmark
        } else if cpu_load > 0.3 || gpu_freq_ratio > 0.5 {
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
            new_mode
        } else {
            // Mode change detected, but wait for threshold
            if self.ticks_in_current_mode < self.mode_switch_threshold {
                self.ticks_in_current_mode = 0;
                self.last_mode  // Stay in old mode (hysteresis)
            } else {
                // Confirmed mode change
                self.ticks_in_current_mode = 0;
                self.last_mode = new_mode;
                log_info!("Workload mode changed: {:?}", new_mode);
                new_mode
            }
        }
    }

    pub fn current_mode(&self) -> WorkloadMode {
        self.last_mode
    }

    pub fn read_gpu_freq_ratio() -> f32 {
        let cur = crate::sensor::sysfs::read_int(
            "/sys/class/kgsl/kgsl-3d0/devfreq/cur_freq"
        );
        let max = crate::sensor::sysfs::read_int(
            "/sys/class/kgsl/kgsl-3d0/devfreq/max_freq"
        );

        if cur > 0 && max > 0 {
            (cur as f32 / max as f32).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}
