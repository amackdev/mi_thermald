use std::fs;

use crate::types::*;
use crate::sensor::sysfs;

pub fn cpuinfo_max_path(cpu: &str) -> String {
    if cpu.starts_with("policy") || cpu.starts_with("cpu-cluster") {
        let suffix = if let Some(p) = cpu.rfind('-') {
            &cpu[p + 1..]
        } else {
            cpu
        };
        format!(
            "/sys/devices/system/cpu/cpufreq/policy{}/cpuinfo_max_freq",
            suffix
        )
    } else {
        format!(
            "/sys/devices/system/cpu/{}/cpufreq/cpuinfo_max_freq",
            cpu
        )
    }
}

pub fn cpuinfo_min_path(cpu: &str) -> String {
    if cpu.starts_with("policy") || cpu.starts_with("cpu-cluster") {
        let suffix = if let Some(p) = cpu.rfind('-') {
            &cpu[p + 1..]
        } else {
            cpu
        };
        format!(
            "/sys/devices/system/cpu/cpufreq/policy{}/cpuinfo_min_freq",
            suffix
        )
    } else {
        format!(
            "/sys/devices/system/cpu/{}/cpufreq/cpuinfo_min_freq",
            cpu
        )
    }
}

pub fn action_apply(a: &Action) -> i32 {
    match a.type_ {
        ActionType::CpuFreq => set_cpu_freq(&a.target, a.value),
        ActionType::CpuHotplug => set_cpu_hotplug(&a.target, a.value != 0),
        ActionType::GpuBoost => set_gpu_boost(a.value),
        ActionType::IpaBoost => set_ipa_boost(a.value),
        ActionType::Bcl => set_bcl(a.value),
        ActionType::TempAware => set_temp_aware(a.value != 0),
        ActionType::PowersaveBoost => set_powersave_boost(a.value != 0),
        ActionType::Brightness => set_brightness(a.value),
        ActionType::MaxBrightness => set_max_brightness(a.value),
        ActionType::Shutdown => set_shutdown_policy(a.value != 0),
        ActionType::Fcc => set_fcc(a.value),
        ActionType::BoostLimit => set_boost_limit(a.value != 0),
        ActionType::TempState => set_temp_state(a.value),
        ActionType::ModemLimit => set_modem_limit(a.value != 0),
        ActionType::ModemLevel => set_modem_level(a.value),
        ActionType::WifiLimit => set_wifi_limit(a.value),
        ActionType::VoiceLimit => set_voice_limit(a.value != 0),
        ActionType::TorchLevel => set_torch_level(a.value),
        ActionType::Cdsp => set_cdsp(a.value),
        ActionType::MarketLimit => set_market_limit(a.value != 0),
        ActionType::ScreenState => set_screen_state(a.value),
        ActionType::Boost => set_boost_state(a.value),
        ActionType::SpecialCpuLimit => set_special_cpu_limit(a.value),
        ActionType::CpuLimits => set_cpu_limits(a.value),
        _ => 0,
    }
}

pub fn action_apply_multi(actions: &[Action]) -> i32 {
    for a in actions {
        action_apply(a);
    }
    0
}

pub fn set_cpu_freq(cpu: &str, freq_khz: i32) -> i32 {
    let path = if cpu.starts_with("policy") || cpu.starts_with("cpu-cluster") {
        let suffix = if let Some(p) = cpu.rfind('-') {
            &cpu[p + 1..]
        } else {
            cpu
        };
        format!(
            "/sys/devices/system/cpu/cpufreq/policy{}/scaling_max_freq",
            suffix
        )
    } else {
        format!(
            "/sys/devices/system/cpu/{}/cpufreq/scaling_max_freq",
            cpu
        )
    };
    let ok = sysfs::write_int(&path, freq_khz);
    if !ok {
        log_warn!("set cpu freq of {} to {} failed", cpu, freq_khz);
    } else {
        log_info!("set cpu freq of {} to {}", cpu, freq_khz);
    }
    if ok { 0 } else { -1 }
}

pub fn set_cpu_hotplug(cpu: &str, online: bool) -> i32 {
    let n = if cpu.starts_with("hotplug_cpu") {
        &cpu[11..]
    } else if cpu.starts_with("cpu") {
        &cpu[3..]
    } else {
        cpu
    };
    let path = format!("/sys/devices/system/cpu/cpu{}/online", n);
    let ok = sysfs::write_int(&path, if online { 1 } else { 0 });
    if !ok {
        log_warn!("set cpu{} online to {} failed", n, online);
    } else {
        log_info!("set cpu{} online to {}", n, online);
    }
    if ok { 0 } else { -1 }
}

pub fn set_gpu_boost(level: i32) -> i32 {
    let mut rc = 0;
    let devfreq_dir = "/sys/class/devfreq/";
    if let Ok(entries) = fs::read_dir(devfreq_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if name_str.contains("kgsl-busmon") {
                continue;
            }
            if name_str.contains("kgsl")
                || name_str.starts_with("gpu-")
                || name_str.starts_with("gpuss-")
            {
                let max_path = entry.path().join("max_freq");
                if !sysfs::write_int(max_path.to_str().unwrap_or(""), level) {
                    rc = -1;
                }
            }
        }
    }
    rc
}

pub fn set_ipa_boost(level: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/ipa_cdev_limits", level) {
        0
    } else {
        -1
    }
}

pub fn set_bcl(ma: i32) -> i32 {
    log_info!("set bcl to {}", ma);
    let ok = sysfs::write_int("/sys/class/power_supply/battery/constant_charge_current", ma)
        || sysfs::write_int("/sys/class/power_supply/battery/current_max", ma);
    if ok { 0 } else { -1 }
}

pub fn set_fcc(ma: i32) -> i32 {
    log_info!("set fcc to {}", ma);
    if sysfs::write_int("/sys/class/power_supply/battery/constant_charge_current", ma) {
        0
    } else {
        -1
    }
}

pub fn set_boost_limit(on: bool) -> i32 {
    let ok = sysfs::write_int("/sys/devices/system/cpu/cpufreq/boost", if on { 1 } else { 0 });
    if !ok {
        log_warn!("set boost_limit to {} failed", on);
    }
    if ok { 0 } else { -1 }
}

pub fn set_temp_state(state: i32) -> i32 {
    let ok = sysfs::write_int("/sys/class/thermal/thermal_message/temp_state", state)
        || sysfs::write_int("/sys/kernel/thermal/temp_state", state);
    if ok { 0 } else { -1 }
}

pub fn set_brightness(level: i32) -> i32 {
    let ok = sysfs::write_int("/sys/class/backlight/panel0-backlight/brightness", level)
        || sysfs::write_int("/sys/class/leds/lcd-backlight/brightness", level)
        || sysfs::write_int("/sys/class/mi_display/disp-DSI-0/brightness_clone", level);
    if ok { 0 } else { -1 }
}

pub fn set_max_brightness(level: i32) -> i32 {
    let ok = sysfs::write_int("/sys/class/backlight/panel0-backlight/max_brightness", level)
        || sysfs::write_int("/sys/class/thermal/thermal_message/thermal_max_brightness", level);
    if ok { 0 } else { -1 }
}

pub fn set_modem_limit(on: bool) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/modem_limit", if on { 1 } else { 0 }) {
        0
    } else {
        -1
    }
}

pub fn set_modem_level(level: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/modem_level", level) { 0 } else { -1 }
}

pub fn set_wifi_limit(limit: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/wifi_limit", limit) { 0 } else { -1 }
}

pub fn set_voice_limit(on: bool) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/voice_limit", if on { 1 } else { 0 }) {
        0
    } else {
        -1
    }
}

pub fn set_torch_level(level: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/torch_level", level) { 0 } else { -1 }
}

pub fn set_cdsp(level: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/cdsp", level) { 0 } else { -1 }
}

pub fn set_market_limit(on: bool) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/market_download_limit", if on { 1 } else { 0 }) {
        0
    } else {
        -1
    }
}

pub fn set_temp_aware(on: bool) -> i32 {
    let ok = sysfs::write_int("/sys/class/power_debug/temp_aware", if on { 1 } else { 0 })
        || sysfs::write_int("/sys/class/thermal/thermal_message/temp_aware", if on { 1 } else { 0 });
    if !ok {
        log_warn!("set temp_aware {} failed", on);
    }
    if ok { 0 } else { -1 }
}

pub fn set_powersave_boost(on: bool) -> i32 {
    let ok = sysfs::write_int("/sys/powersave/boost", if on { 1 } else { 0 })
        || sysfs::write_int("/sys/class/thermal/power_save/powersave_mode", if on { 1 } else { 0 });
    if !ok {
        log_warn!("set powersave boost to {} failed", on);
    }
    if ok { 0 } else { -1 }
}

pub fn set_shutdown_policy(on: bool) -> i32 {
    let ok = sysfs::write_int("/data/vendor/thermal/shutdown_policy0", if on { 1 } else { 0 });
    if !ok {
        log_warn!("write shutdown policy0 file failed {}", on);
    } else {
        log_info!("write shutdown policy0 file success");
    }
    if ok { 0 } else { -1 }
}

pub fn set_screen_state(state: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/screen_state", state) { 0 } else { -1 }
}

pub fn set_boost_state(level: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/boost", level) { 0 } else { -1 }
}

pub fn set_special_cpu_limit(limit: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/special_cpu_limit", limit) { 0 } else { -1 }
}

pub fn set_cpu_limits(limit: i32) -> i32 {
    if sysfs::write_int("/sys/class/thermal/thermal_message/cpu_limits", limit) { 0 } else { -1 }
}
