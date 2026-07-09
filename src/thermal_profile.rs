// Thermal profile manager - reads sconfig node written by Android framework
// The framework reads thermal_list XML and writes profile ID to sysfs

use std::fs;

const THERMAL_SCONFIG_NODE: &str = "/sys/class/thermal/thermal_message/sconfig";

pub struct ThermalProfileManager {
    current_profile_id: u32,
    last_check_time: std::time::Instant,
    check_interval: std::time::Duration,
}

impl ThermalProfileManager {
    pub fn new() -> Self {
        ThermalProfileManager {
            current_profile_id: 0,
            last_check_time: std::time::Instant::now(),
            check_interval: std::time::Duration::from_secs(1),  // Check every 1s
        }
    }

    /// Read current thermal profile ID from sconfig node
    /// Returns Some(profile_id) if successful, None if node doesn't exist or error
    pub fn read_sconfig(&mut self) -> Option<u32> {
        // Rate limiting: only check once per second to reduce I/O
        if self.last_check_time.elapsed() < self.check_interval {
            return if self.current_profile_id > 0 {
                Some(self.current_profile_id)
            } else {
                None
            };
        }

        self.last_check_time = std::time::Instant::now();

        // Read sysfs node
        match fs::read_to_string(THERMAL_SCONFIG_NODE) {
            Ok(content) => {
                match content.trim().parse::<u32>() {
                    Ok(profile_id) => {
                        if profile_id != self.current_profile_id {
                            log_debug!("Thermal profile changed: {} → {}",
                                      self.current_profile_id, profile_id);
                            self.current_profile_id = profile_id;
                        }
                        Some(profile_id)  // Return profile_id including 0 (reset)
                    }
                    Err(e) => {
                        log_debug!("Failed to parse sconfig value '{}': {}", content.trim(), e);
                        None
                    }
                }
            }
            Err(e) => {
                // Node might not exist on all devices - this is normal
                // Only log once when first detected
                if self.current_profile_id == 0 {
                    log_debug!("Thermal sconfig node not available: {}", e);
                }
                None
            }
        }
    }

    /// Get current profile ID
    pub fn current_profile(&self) -> u32 {
        self.current_profile_id
    }

    /// Map thermal profile ID to workload mode
    /// Based on thermal_list_marble.xml and thermal_list_peridot.xml analysis
    pub fn profile_to_workload(&self, profile_id: u32) -> Option<crate::ai::WorkloadMode> {
        use crate::ai::WorkloadMode;

        match profile_id {
            0 => None,  // Default/no profile, use sensor detection

            // Benchmarks (Geekbench, AnTuTu, 3DMark, etc.)
            6 | 10 | 40 => Some(WorkloadMode::Benchmark),

            // heavy Gaming
            18 | 39 => Some(WorkloadMode::PerfGaming),

            // Gaming (Genshin, PUBG, COD, Fortnite, etc.)
            19 | 20 => Some(WorkloadMode::Gaming),

            // Camera apps (burst load)
            12 | 15 | 42 => Some(WorkloadMode::Moderate),

            // Video players (VLC, YouTube, sustained decode)
            11 | 21 | 51 | 61 => Some(WorkloadMode::Moderate),

            // Streaming/social media (Netflix, Instagram, light load)
            7 | 41 => Some(WorkloadMode::Light),

            // Unknown profile - fall back to sensor detection
            _ => {
                log_debug!("Unknown thermal profile ID: {}, using sensor detection", profile_id);
                None
            }
        }
    }

    /// Check if sconfig node is available on this device
    pub fn is_sconfig_available() -> bool {
        std::path::Path::new(THERMAL_SCONFIG_NODE).exists()
    }
}
