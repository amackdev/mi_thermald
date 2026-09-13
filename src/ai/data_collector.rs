use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::features::StateVector;

const DATA_DIR: &str = "/data/vendor/thermal/ai_data";
const FLUSH_INTERVAL: usize = 100;
const MAX_EXPERIENCES: usize = 10000;
const RETENTION_DAYS: u64 = 7;

#[derive(Serialize, Deserialize)]
struct Experience {
    t_cpu_max: f32,
    t_cpu_avg: f32,
    t_board_max: f32,
    t_battery: f32,
    t_ambient: f32,
    dt_cpu: f32,
    dt_board: f32,
    dt_battery: f32,
    battery_soc: f32,
    is_charging: f32,
    screen_on: f32,
    time_of_day: f32,
    thermal_level_traditional: f32,
    t_cpu_ma_10: f32,
    t_board_ma_10: f32,
    thermal_level_ma_10: f32,
    cpu_freq_ratio: f32,
    temp_headroom_cpu: f32,
    temp_headroom_battery: f32,
    temp_variance: f32,
    last_action: f32,
    action_stability: f32,
    last_action_compute: f32,
    last_action_thermal: f32,
    last_action_charging: f32,
    last_action_display: f32,
    brightness_ratio: f32,
    charge_current_ratio: f32,
    t_charger: f32,
    battery_current: f32,
    cpu_load: f32,
    workload_mode: f32,
    action: u8,
    reward: f32,
}

impl Experience {
    fn csv_header() -> &'static str {
        "t_cpu_max,t_cpu_avg,t_board_max,t_battery,t_ambient,dt_cpu,dt_board,dt_battery,\
         battery_soc,is_charging,screen_on,time_of_day,thermal_level_traditional,\
         t_cpu_ma_10,t_board_ma_10,thermal_level_ma_10,cpu_freq_ratio,temp_headroom_cpu,\
         temp_headroom_battery,temp_variance,last_action,action_stability,\
         last_action_compute,last_action_thermal,last_action_charging,last_action_display,\
         brightness_ratio,charge_current_ratio,t_charger,battery_current,\
         cpu_load,workload_mode,action,reward"
    }

    fn to_csv_row(&self) -> String {
        format!("{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{:.4}",
            self.t_cpu_max, self.t_cpu_avg, self.t_board_max, self.t_battery, self.t_ambient,
            self.dt_cpu, self.dt_board, self.dt_battery, self.battery_soc, self.is_charging,
            self.screen_on, self.time_of_day, self.thermal_level_traditional, self.t_cpu_ma_10,
            self.t_board_ma_10, self.thermal_level_ma_10, self.cpu_freq_ratio, self.temp_headroom_cpu,
            self.temp_headroom_battery, self.temp_variance, self.last_action, self.action_stability,
            self.last_action_compute, self.last_action_thermal, self.last_action_charging, self.last_action_display,
            self.brightness_ratio, self.charge_current_ratio,
            self.t_charger, self.battery_current, self.cpu_load, self.workload_mode, self.action, self.reward)
    }
}

pub struct DataCollector {
    buffer: Vec<Experience>,
    data_dir: String,
    enabled: bool,
}

impl DataCollector {
    pub fn new() -> Self {
        let dir = DATA_DIR.to_string();
        let _ = fs::create_dir_all(&dir);

        DataCollector {
            buffer: Vec::with_capacity(FLUSH_INTERVAL),
            data_dir: dir,
            enabled: true,
        }
    }

    pub fn record(
        &mut self,
        state: &StateVector,
        action: u8,
        reward: f32,
    ) {
        if !self.enabled {
            return;
        }

        let exp = Experience {
            t_cpu_max: state.t_cpu_max,
            t_cpu_avg: state.t_cpu_avg,
            t_board_max: state.t_board_max,
            t_battery: state.t_battery,
            t_ambient: state.t_ambient,
            dt_cpu: state.dt_cpu,
            dt_board: state.dt_board,
            dt_battery: state.dt_battery,
            battery_soc: state.battery_soc,
            is_charging: state.is_charging,
            screen_on: state.screen_on,
            time_of_day: state.time_of_day,
            thermal_level_traditional: state.thermal_level_traditional,
            t_cpu_ma_10: state.t_cpu_ma_10,
            t_board_ma_10: state.t_board_ma_10,
            thermal_level_ma_10: state.thermal_level_ma_10,
            cpu_freq_ratio: state.cpu_freq_ratio,
            temp_headroom_cpu: state.temp_headroom_cpu,
            temp_headroom_battery: state.temp_headroom_battery,
            temp_variance: state.temp_variance,
            last_action: state.last_action,
            action_stability: state.action_stability,
            last_action_compute: state.last_action_compute,
            last_action_thermal: state.last_action_thermal,
            last_action_charging: state.last_action_charging,
            last_action_display: state.last_action_display,
            brightness_ratio: state.brightness_ratio,
            charge_current_ratio: state.charge_current_ratio,
            t_charger: state.t_charger,
            battery_current: state.battery_current,
            cpu_load: state.cpu_load,
            workload_mode: state.workload_mode,
            action,
            reward,
        };

        self.buffer.push(exp);

        if self.buffer.len() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        let date_str = self.current_date();
        let path = format!("{}/experiences_{}.csv", self.data_dir, date_str);

        let file_exists = Path::new(&path).exists();
        let mut data = String::new();
        
        if !file_exists {
            data.push_str(Experience::csv_header());
            data.push('\n');
        }

        for exp in &self.buffer {
            data.push_str(&exp.to_csv_row());
            data.push('\n');
        }

        // Use append mode instead of reading whole file
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = file.write_all(data.as_bytes());
        }

        self.buffer.clear();
        self.cleanup_old_logs();
    }

    pub fn save_qtable(&self, weights: &[f32]) {
        let path = format!("{}/q_table.json", self.data_dir);
        let backup = format!("{}/q_table_backup.json", self.data_dir);

        if Path::new(&path).exists() {
            let _ = fs::copy(&path, &backup);
        }

        if let Ok(json) = serde_json::to_string(weights) {
            let temp = format!("{}.tmp", path);
            if fs::write(&temp, &json).is_ok() {
                let _ = fs::rename(&temp, &path);
            }
        }
    }

    pub fn load_qtable(&self) -> Option<Vec<f32>> {
        let path = format!("{}/q_table.json", self.data_dir);
        let data = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn current_date(&self) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("{}", now / 86400)
    }

    fn cleanup_old_logs(&self) {
        let dir = match fs::read_dir(&self.data_dir) {
            Ok(d) => d,
            Err(_) => return,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cutoff = now - RETENTION_DAYS * 86400;

        for entry in dir.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy();
                if name.starts_with("experiences_") && name.ends_with(".csv") {
                    if let Ok(metadata) = fs::metadata(&path) {
                        if let Ok(mtime) = metadata.modified() {
                            if let Ok(mtime_secs) = mtime.duration_since(std::time::UNIX_EPOCH) {
                                if mtime_secs.as_secs() < cutoff {
                                    let _ = fs::remove_file(&path);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
