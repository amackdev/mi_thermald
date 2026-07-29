// Feature extraction for thermal state representation
// Converts raw sensor data into normalized state vector

use crate::types::{Sensor, Instance};
use std::sync::atomic::Ordering;

/// 27-dimensional state vector for Q-learning
#[derive(Clone, Debug)]
pub struct StateVector {
    // Temperature features (normalized 0-1, where 1 = 100°C)
    pub t_cpu_max: f32,      // Maximum CPU temperature
    pub t_cpu_avg: f32,      // Average CPU temperature
    pub t_board_max: f32,    // Maximum board temperature
    pub t_battery: f32,      // Battery temperature
    pub t_ambient: f32,      // Ambient temperature

    // Temperature derivatives (°C per second, normalized -1 to 1)
    pub dt_cpu: f32,         // CPU temperature rate of change
    pub dt_board: f32,       // Board temperature rate of change
    pub dt_battery: f32,     // Battery temperature rate of change

    // Device context (normalized 0-1)
    pub battery_soc: f32,    // Battery state of charge (0-100%)
    pub is_charging: f32,    // 1.0 if charging, 0.0 otherwise
    pub screen_on: f32,      // 1.0 if screen on, 0.0 otherwise
    pub time_of_day: f32,    // Hour of day / 24

    // Traditional controller baseline
    pub thermal_level_traditional: f32, // What traditional algo chose (normalized)

    // Historical features (10-tick moving averages)
    pub t_cpu_ma_10: f32,    // CPU temp moving average
    pub t_board_ma_10: f32,  // Board temp moving average
    pub thermal_level_ma_10: f32, // Thermal level moving average

    // Performance indicators
    pub cpu_freq_ratio: f32, // Current freq / max freq

    // Thermal headroom
    pub temp_headroom_cpu: f32,   // (85°C - current) / 85°C
    pub temp_headroom_battery: f32, // (45°C - current) / 45°C

    // Workload proxy
    pub temp_variance: f32,  // Temperature variance across sensors

    // Action history
    pub last_action: f32,    // Previous action taken (normalized)
    pub action_stability: f32, // How long at current level

    // Additional sensors
    pub t_gpu: f32,          // GPU temperature if available
    pub t_charger: f32,      // Charger temperature if available

    // Power metrics
    pub battery_current: f32, // Battery current (normalized)

    // CPU utilization
    pub cpu_load: f32,       // CPU utilization 0-1 from /proc/stat

    // Workload context
    pub workload_mode: f32,  // 0=idle, 0.25=light, 0.5=moderate, 0.75=gaming, 1.0=benchmark

    // Multi-channel action history (per-group)
    pub last_action_compute: f32,
    pub last_action_thermal: f32,
    pub last_action_charging: f32,
    pub last_action_display: f32,

    // Channel-specific readings
    pub gpu_freq_ratio: f32,
    pub brightness_ratio: f32,
    pub charge_current_ratio: f32,
}

pub struct FeatureExtractor {
    // History buffers for moving averages
    cpu_temp_history: Vec<f32>,
    board_temp_history: Vec<f32>,
    level_history: Vec<f32>,

    // Previous state for derivatives
    prev_cpu_temp: f32,
    prev_board_temp: f32,
    prev_battery_temp: f32,

    history_size: usize,

    // CPU utilization tracking
    prev_cpu_idle: u64,
    prev_cpu_total: u64,
}

impl FeatureExtractor {
    pub fn new() -> Self {
        FeatureExtractor {
            cpu_temp_history: Vec::new(),
            board_temp_history: Vec::new(),
            level_history: Vec::new(),
            prev_cpu_temp: 0.0,
            prev_board_temp: 0.0,
            prev_battery_temp: 0.0,
            history_size: 10,
            prev_cpu_idle: 0,
            prev_cpu_total: 0,
        }
    }

    pub fn extract(&mut self, sensors: &[Sensor], instance: &Instance, traditional_level: i32) -> StateVector {
        // Extract temperature values
        let (t_cpu_max, t_cpu_avg, t_board_max, t_battery, t_ambient, t_gpu, t_charger) =
            self.extract_temperatures(sensors);

        // Compute derivatives
        let dt_cpu = (t_cpu_max - self.prev_cpu_temp).clamp(-10.0, 10.0) / 10.0;
        let dt_board = (t_board_max - self.prev_board_temp).clamp(-10.0, 10.0) / 10.0;
        let dt_battery = (t_battery - self.prev_battery_temp).clamp(-5.0, 5.0) / 5.0;
        self.prev_cpu_temp = t_cpu_max;
        self.prev_board_temp = t_board_max;
        self.prev_battery_temp = t_battery;

        // Update history buffers
        self.cpu_temp_history.push(t_cpu_max);
        self.board_temp_history.push(t_board_max);
        self.level_history.push(traditional_level as f32);

        if self.cpu_temp_history.len() > self.history_size {
            self.cpu_temp_history.remove(0);
            self.board_temp_history.remove(0);
            self.level_history.remove(0);
        }

        // Compute moving averages
        let t_cpu_ma_10 = self.moving_average(&self.cpu_temp_history);
        let t_board_ma_10 = self.moving_average(&self.board_temp_history);
        let thermal_level_ma_10 = self.moving_average(&self.level_history);

        // Extract device context
        let (battery_soc, is_charging, battery_current) = self.extract_battery_info(sensors);
        let screen_on = 1.0; // TODO: Could be extracted from sensor if available
        let time_of_day = self.get_time_of_day();

        // Compute headroom
        let temp_headroom_cpu = ((85.0 - t_cpu_max * 100.0) / 85.0).clamp(0.0, 1.0);
        let temp_headroom_battery = ((45.0 - t_battery * 100.0) / 45.0).clamp(0.0, 1.0);

        // Compute variance as workload proxy
        let temp_variance = self.compute_temp_variance(sensors);

        // Action history
        let max_level = instance.threshold.n_levels() as f32;
        let thermal_level_normalized = if max_level > 0.0 {
            traditional_level as f32 / max_level
        } else {
            0.0
        };

        let cpu_load = self.read_cpu_load();

        StateVector {
            t_cpu_max,
            t_cpu_avg,
            t_board_max,
            t_battery,
            t_ambient,
            dt_cpu,
            dt_board,
            dt_battery,
            battery_soc,
            is_charging,
            screen_on,
            time_of_day,
            thermal_level_traditional: thermal_level_normalized,
            t_cpu_ma_10,
            t_board_ma_10,
            thermal_level_ma_10: thermal_level_ma_10 / max_level.max(1.0),
            cpu_freq_ratio: Self::read_avg_cpu_freq(),
            temp_headroom_cpu,
            temp_headroom_battery,
            temp_variance,
            last_action: 0.0, // TODO: Track from previous tick
            action_stability: 0.0, // TODO: Track ticks at current level
            t_gpu,
            t_charger,
            battery_current,
            cpu_load,
            workload_mode: 0.5, // Will be updated by engine
            last_action_compute: 0.0,
            last_action_thermal: 0.0,
            last_action_charging: 0.0,
            last_action_display: 0.0,
            gpu_freq_ratio: 0.0,
            brightness_ratio: 0.0,
            charge_current_ratio: 0.0,
        }
    }

    fn extract_temperatures(&self, sensors: &[Sensor]) -> (f32, f32, f32, f32, f32, f32, f32) {
        let mut cpu_temps = Vec::new();
        let mut board_temps = Vec::new();
        let mut battery_temp = 0.0;
        let mut ambient_temp = 0.0;
        let mut gpu_temp = 0.0;
        let mut charger_temp = 0.0;

        for sensor in sensors {
            let temp_mc = sensor.last_temp_mc.load(Ordering::Relaxed);
            let temp_normalized = (temp_mc as f32 / 100000.0).clamp(0.0, 1.0);

            if sensor.name.contains("cpu") || sensor.name.contains("tsens") {
                cpu_temps.push(temp_normalized);
            } else if sensor.name.contains("board") {
                board_temps.push(temp_normalized);
            } else if sensor.name.contains("battery") {
                battery_temp = temp_normalized;
            } else if sensor.name.contains("ambient") {
                ambient_temp = temp_normalized;
            } else if sensor.name.contains("gpu") || sensor.name.contains("kgsl") {
                gpu_temp = temp_normalized;
            } else if sensor.name.contains("charger") {
                charger_temp = temp_normalized;
            }
        }

        let t_cpu_max = cpu_temps.iter().cloned().fold(0.0f32, f32::max);
        let t_cpu_avg = if !cpu_temps.is_empty() {
            cpu_temps.iter().sum::<f32>() / cpu_temps.len() as f32
        } else {
            0.0
        };
        let t_board_max = board_temps.iter().cloned().fold(0.0f32, f32::max);

        (t_cpu_max, t_cpu_avg, t_board_max, battery_temp, ambient_temp, gpu_temp, charger_temp)
    }

    fn extract_battery_info(&self, sensors: &[Sensor]) -> (f32, f32, f32) {
        let mut soc = 0.5; // Default 50%
        let mut is_charging = 0.0;
        let mut current = 0.0;

        for sensor in sensors {
            let value = sensor.last_temp_mc.load(Ordering::Relaxed);

            if sensor.name == "BAT_SOC" {
                soc = (value as f32 / 100.0).clamp(0.0, 1.0);
            } else if sensor.name.contains("battery_current") {
                // Xiaomi reports negative µA when charging
                let abs_val = value.abs();
                current = (abs_val as f32 / 6000000.0).clamp(0.0, 1.0);
                is_charging = if value < 0 { 1.0 } else { 0.0 };
            }
        }

        (soc, is_charging, current)
    }

    fn compute_temp_variance(&self, sensors: &[Sensor]) -> f32 {
        let temps: Vec<f32> = sensors
            .iter()
            .map(|s| s.last_temp_mc.load(Ordering::Relaxed) as f32 / 100000.0)
            .collect();

        if temps.is_empty() {
            return 0.0;
        }

        let mean = temps.iter().sum::<f32>() / temps.len() as f32;
        let variance = temps.iter()
            .map(|&t| (t - mean).powi(2))
            .sum::<f32>() / temps.len() as f32;

        (variance.sqrt() / 0.2).clamp(0.0, 1.0) // Normalize, assume max std dev of 20°C
    }

    fn moving_average(&self, history: &[f32]) -> f32 {
        if history.is_empty() {
            0.0
        } else {
            history.iter().sum::<f32>() / history.len() as f32
        }
    }

    fn get_time_of_day(&self) -> f32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let hour = (now / 3600) % 24;
        hour as f32 / 24.0
    }

    fn read_cpu_load(&mut self) -> f32 {
        let data = match std::fs::read_to_string("/proc/stat") {
            Ok(s) => s,
            Err(_) => return 0.0,
        };
        let first_line = match data.lines().next() {
            Some(l) => l,
            None => return 0.0,
        };
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 5 {
            return 0.0;
        }
        let user: u64 = parts[1].parse().unwrap_or(0);
        let nice: u64 = parts[2].parse().unwrap_or(0);
        let system: u64 = parts[3].parse().unwrap_or(0);
        let idle: u64 = parts[4].parse().unwrap_or(0);

        let total = user + nice + system + idle;
        if self.prev_cpu_total == 0 || total <= self.prev_cpu_total {
            self.prev_cpu_idle = idle;
            self.prev_cpu_total = total;
            return 0.0;
        }
        let total_delta = total - self.prev_cpu_total;
        let idle_delta = idle - self.prev_cpu_idle;
        self.prev_cpu_idle = idle;
        self.prev_cpu_total = total;

        if total_delta == 0 {
            return 0.0;
        }
        let load = 1.0 - (idle_delta as f32 / total_delta as f32);
        load.clamp(0.0, 1.0)
    }

    fn read_avg_cpu_freq() -> f32 {
        let policies = ["/sys/devices/system/cpu/cpufreq/policy0",
                        "/sys/devices/system/cpu/cpufreq/policy3",
                        "/sys/devices/system/cpu/cpufreq/policy7"];
        let mut sum = 0.0f64;
        let mut count = 0;
        for base in &policies {
            let cur = crate::sensor::sysfs::read_int(
                &format!("{}/scaling_cur_freq", base)
            );
            let max = crate::sensor::sysfs::read_int(
                &format!("{}/cpuinfo_max_freq", base)
            );
            if cur > 0 && max > 0 {
                sum += cur as f64 / max as f64;
                count += 1;
            }
        }
        if count > 0 {
            (sum / count as f64) as f32
        } else {
            0.8
        }
    }

    /// Convert state vector to array for tile coding
    pub fn to_array(&self, state: &StateVector) -> Vec<f32> {
        vec![
            state.t_cpu_max,
            state.t_cpu_avg,
            state.t_board_max,
            state.t_battery,
            state.t_ambient,
            state.dt_cpu,
            state.dt_board,
            state.dt_battery,
            state.battery_soc,
            state.is_charging,
            state.screen_on,
            state.time_of_day,
            state.thermal_level_traditional,
            state.t_cpu_ma_10,
            state.t_board_ma_10,
            state.thermal_level_ma_10,
            state.cpu_freq_ratio,
            state.temp_headroom_cpu,
            state.temp_headroom_battery,
            state.temp_variance,
            state.last_action,
            state.action_stability,
            state.t_gpu,
            state.t_charger,
            state.battery_current,
            state.cpu_load,
            state.workload_mode,
            state.last_action_compute,
            state.last_action_thermal,
            state.last_action_charging,
            state.last_action_display,
            state.gpu_freq_ratio,
            state.brightness_ratio,
            state.charge_current_ratio,
        ]
    }
}
