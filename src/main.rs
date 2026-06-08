// Allow dead code — this is a direct port; many items are used only
// via match arms or config-driven dispatch invisible to the compiler.
#![allow(dead_code)]
#![allow(unused_unsafe)]

#[macro_use]
mod log_macros;

mod types;
mod sensor;
mod config;
mod algorithm;
mod action;
mod ai;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::ffi::CString;
use std::fs;
use std::os::unix::io::RawFd;

use libc;

use crate::types::*;
use crate::sensor::{sysfs, EngineDiscovery};
use crate::algorithm::{evaluate_virtual_sensor, evaluate_instance};
use crate::action::{action_apply_multi, cpuinfo_max_path, set_bcl};

// ------------------------------------------------------------------
// Logging + Android properties FFI
// ------------------------------------------------------------------

#[cfg(not(target_os = "android"))]
pub fn android_log_write(priority: libc::c_int, msg: *const libc::c_char) {
    unsafe { crate::syslog_ffi(priority, msg); }
}

#[cfg(target_os = "android")]
pub fn android_log_write(priority: libc::c_int, msg: *const libc::c_char) {
    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(
            prio: libc::c_int,
            tag: *const libc::c_char,
            text: *const libc::c_char,
        ) -> libc::c_int;
    }
    let tag = CString::new(crate::MI_THERMALD_VERSION_STRING).unwrap();
    unsafe { __android_log_write(priority, tag.as_ptr(), msg); }
}

// The syslog family is in libc on Linux (glibc) — no explicit link attribute needed.
#[cfg(not(target_os = "android"))]
extern "C" {
    fn openlog(ident: *const libc::c_char, option: libc::c_int, facility: libc::c_int);
    #[link_name = "syslog"]
    pub(crate) fn syslog_ffi(priority: libc::c_int, message: *const libc::c_char);
    fn closelog();
    fn setlogmask(mask: libc::c_int) -> libc::c_int;
}

// Android property access via getprop command (portable, no libcutils dependency)
pub fn property_get_str(key: &str, default: &str) -> String {
    use std::process::Command;

    // Execute getprop command to read property
    let output = Command::new("getprop")
        .arg(key.trim_end_matches('\0'))
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let value = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }

    default.to_string()
}

// ------------------------------------------------------------------
// Signal handling
// ------------------------------------------------------------------

static G_TERM: AtomicBool = AtomicBool::new(false);
static G_USR1: AtomicBool = AtomicBool::new(false);
pub(crate) static FCC_VALUE: AtomicI32 = AtomicI32::new(0);
pub(crate) static CPU_FREQ0_TARGET: AtomicI32 = AtomicI32::new(0);
pub(crate) static CPU_FREQ3_TARGET: AtomicI32 = AtomicI32::new(0);
pub(crate) static CPU_FREQ7_TARGET: AtomicI32 = AtomicI32::new(0);

extern "C" fn signal_term(_: libc::c_int) {
    G_TERM.store(true, Ordering::SeqCst);
}

extern "C" fn signal_usr1(_: libc::c_int) {
    G_USR1.store(true, Ordering::SeqCst);
}

// ------------------------------------------------------------------
// Engine
// ------------------------------------------------------------------

struct Engine {
    sensors: Vec<Sensor>,
    virtual_sensors: Vec<usize>,
    cooling_devices: Vec<CoolingDevice>,
    instances: Vec<Instance>,
    sic_states: Vec<SicState>,

    epoll_fd: RawFd,
    inotify_fd: RawFd,
    timer_fd: RawFd,

    charger_only: bool,
    boot_completed: bool,
    current_scenario_idx: i32,

    log_level: i32,
    ai_engine: Option<ai::AIEngine>,
    native_controller: Option<ai::NativeController>,
}

impl Engine {
    fn new() -> Self {
        Engine {
            sensors: Vec::new(),
            virtual_sensors: Vec::new(),
            cooling_devices: Vec::new(),
            instances: Vec::new(),
            sic_states: Vec::new(),
            epoll_fd: -1,
            inotify_fd: -1,
            timer_fd: -1,
            charger_only: false,
            boot_completed: false,
            current_scenario_idx: 0,
            log_level: 6,
            ai_engine: None,
            native_controller: None,
        }
    }

    fn init(&mut self) -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let mut log_level = LOG_INFO;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-d" => {}
                "-l" => {
                    i += 1;
                    if i < args.len() {
                        log_level = args[i].parse().unwrap_or(LOG_INFO);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.log_level = log_level;

        #[cfg(not(target_os = "android"))]
        unsafe {
            let ident = CString::new(MI_THERMALD_VERSION_STRING).unwrap();
            openlog(ident.as_ptr(), LOG_PID | LOG_NDELAY, LOG_DAEMON);
            setlogmask(log::log_upto(log_level));
        }

        unsafe {
            let rc = libc::setpriority(libc::PRIO_PROCESS, 0, -19);
            if rc != 0 {
                log_warn!("setpriority failed");
            }
        }

        unsafe {
            let mut cset = std::mem::zeroed::<libc::cpu_set_t>();
            libc::CPU_ZERO(&mut cset);
            for cpu in 0..4 {
                libc::CPU_SET(cpu, &mut cset);
            }
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cset);
        }

        self.setup_signals();

        let soc = property_get_str(MI_PROP_SOC_MODEL, "default");
        log_info!("thermald start soc={}", soc);

        let ai_prop = property_get_str("ro.vendor.mi_thermal_ai", "false");
        let ai_enabled = ai_prop == "true" || ai_prop == "1";

        if ai_enabled {
            log_info!("AI-native mode: skipping OEM config, discovering hardware");

            EngineDiscovery::sensor_init(&mut self.sensors);
            self.virtual_sensors = EngineDiscovery::vsns_init(&mut self.sensors);

            let mut nc = ai::NativeController::new();
            nc.switch_scenario(self.current_scenario_idx);
            let n_channels = nc.channels.len();
            self.native_controller = Some(nc);

            log_info!("AI-native started ({} sensors, {} cooling channels)",
                self.sensors.len(), n_channels);
        } else {
            if self.load_thermal_map_impl(&soc) != 0 {
                log_err!("no thermal config — exiting");
                return -1;
            }
            log_info!("thermald started ({} sensors, {} instances)",
                self.sensors.len(), self.instances.len());
        }

        0
    }

    fn setup_signals(&self) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = signal_term as libc::sighandler_t;
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
            sa.sa_sigaction = signal_usr1 as libc::sighandler_t;
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
    }

    fn load_thermal_map_impl(&mut self, soc: &str) -> i32 {
        crate::config::load_thermal_map(
            &mut self.sensors,
            &mut self.virtual_sensors,
            &mut self.cooling_devices,
            &mut self.instances,
            &mut self.sic_states,
            soc,
        )
    }

    fn tick(&mut self) -> i32 {
        if self.native_controller.is_some() {
            return self.tick_native();
        }

        let new_idx = EngineDiscovery::read_sconfig_idx();
        if new_idx >= 0 && new_idx != self.current_scenario_idx {
            log_info!("sconfig changed {} -> {}, reloading scenario",
                self.current_scenario_idx, new_idx);
            let soc = property_get_str(MI_PROP_SOC_MODEL, "default");
            self.load_thermal_map_impl(&soc);
            self.current_scenario_idx = new_idx;
            return 0;
        }

        // Always evaluate all Virtual-type sensors (they may be defined in a
        // base config and referenced by scenarios that lack a Virtual block).
        for sensor_idx in 0..self.sensors.len() {
            if self.sensors[sensor_idx].type_ == SensorType::Virtual
                && !self.sensors[sensor_idx].inputs.is_empty()
            {
                let v = evaluate_virtual_sensor(&self.sensors, sensor_idx);
                if self.sensors[sensor_idx].name == "VIRTUAL-SENSOR0" {
                    log_debug!("VIRTUAL-SENSOR0 = {} mC", v);
                }
            }
        }

        let mut ai_engine = self.ai_engine.take();
        let ai_enabled = ai_engine.as_ref()
            .map(|ai| ai.is_enabled())
            .unwrap_or(false);

        for i in (0..self.instances.len()).rev() {
            if self.instances[i].algo == AlgoType::Virtual {
                continue;
            }
            let sensor_idx = match self.instances[i].sensor_idx {
                Some(idx) => idx,
                None => continue,
            };

            let t = self.sensors[sensor_idx].last_temp_mc.load(Ordering::Relaxed);
            let prev_level = self.instances[i].current_level;
            let traditional_level = evaluate_instance(
                &mut self.instances[i],
                t,
                &mut self.sic_states[i],
            );

            let (level, ai_actions) = if ai_enabled {
                let engine = ai_engine.as_mut().unwrap();
                let state = engine.collect_state(&self.sensors, &self.instances[i], traditional_level);
                let directive = engine.decide_action(&state, traditional_level, &self.instances[i]);
                if directive.level != traditional_level {
                    log_debug!("AI: instance {} trad={} ai={} temp={}",
                        self.instances[i].name, traditional_level, directive.level, t);
                }
                (directive.level, Some(directive.actions))
            } else {
                (traditional_level, None)
            };

            if level != traditional_level {
                self.instances[i].current_level = level;
            }

            if !self.instances[i].actions.is_empty() {
                if let Some(ref ai_acts) = ai_actions {
                    for a in ai_acts {
                        if level != prev_level {
                            log_info!("apply {} level={} {:?}[{}] = {}",
                                self.instances[i].name, level, a.type_, a.target, a.value);
                        }
                    }
                    action_apply_multi(ai_acts);
                } else {
                    let n_levels = self.instances[i].threshold.n_levels();
                    let effective_levels = if n_levels > 0 { n_levels + 1 } else { 1 };
                    let per_dev = self.instances[i].actions.len() / effective_levels;
                    let start = (level as usize) * per_dev;
                    let mut end = start + per_dev;
                    log_debug!("tick instance {} sensor={} temp={} level={}->{} n_actions={}/{}/{}",
                        self.instances[i].name,
                        sensor_idx, t, prev_level, level,
                        self.instances[i].actions.len(), n_levels, effective_levels);
                    if start < self.instances[i].actions.len() {
                        if end > self.instances[i].actions.len() {
                            end = self.instances[i].actions.len();
                        }

                        if level == 0 {
                            for j in start..end {
                                if self.instances[i].actions[j].type_ == ActionType::CpuFreq
                                    && self.instances[i].actions[j].value == 0
                                {
                                    let path = cpuinfo_max_path(&self.instances[i].actions[j].target);
                                    let v = sysfs::read_int(&path);
                                    self.instances[i].actions[j].value = if v > 0 { v } else { 3000000 };
                                }
                                if self.instances[i].actions[j].type_ == ActionType::CpuHotplug
                                    && self.instances[i].actions[j].value == 0
                                {
                                    let cpu = &self.instances[i].actions[j].target;
                                    let n = if cpu.starts_with("hotplug_cpu") {
                                        &cpu[11..]
                                    } else if cpu.starts_with("cpu") {
                                        &cpu[3..]
                                    } else {
                                        cpu
                                    };
                                    let path = format!("/sys/devices/system/cpu/cpu{}/online", n);
                                    let v = sysfs::read_int(&path);
                                    self.instances[i].actions[j].value = if v > 0 { v } else { 1 };
                                }
                                if self.instances[i].actions[j].type_ == ActionType::GpuBoost
                                    && self.instances[i].actions[j].value == 0
                                {
                                    let table = sysfs::read_string(
                                        "/sys/class/kgsl/kgsl-3d0/freq_table_mhz"
                                    ).unwrap_or_default();
                                    let max_mhz: i32 = table.split_whitespace()
                                        .next()
                                        .and_then(|s| s.parse().ok())
                                        .unwrap_or(0);
                                    self.instances[i].actions[j].value =
                                        if max_mhz > 0 { max_mhz * 1_000_000 } else { 1_100_000_000 };
                                }
                                if self.instances[i].actions[j].type_ == ActionType::Bcl
                                    && self.instances[i].actions[j].value == 0
                                {
                                    let v = sysfs::read_int(
                                        "/sys/class/power_supply/battery/constant_charge_current"
                                    );
                                    self.instances[i].actions[j].value = if v > 0 { v } else { 5000000 };
                                }
                                if self.instances[i].actions[j].type_ == ActionType::Fcc
                                    && self.instances[i].actions[j].value == 0
                                {
                                    let mut v = sysfs::read_int(
                                        "/sys/class/power_supply/battery/constant_charge_current_max"
                                    );
                                    if v <= 0 {
                                        v = sysfs::read_int(
                                            "/sys/class/power_supply/battery/constant_charge_current"
                                        );
                                    }
                                    self.instances[i].actions[j].value = if v > 0 { v } else { 6000000 };
                                }
                            }
                        }

                        if self.instances[i].algo == AlgoType::Sic {
                            let pid_v = self.instances[i].current_value;
                            for j in start..end {
                                self.instances[i].actions[j].value = pid_v;
                            }
                            FCC_VALUE.store(pid_v, Ordering::Relaxed);
                        }

                        for j in start..end {
                            let a = &self.instances[i].actions[j];
                            if level != prev_level {
                                log_info!("apply {} level={} {:?}[{}] = {}",
                                    self.instances[i].name, level, a.type_, a.target, a.value);
                            }
                        }
                        action_apply_multi(&self.instances[i].actions[start..end]);
                    }
                }
            }
        }

        if let Some(mut engine) = ai_engine {
            engine.end_tick(&self.sensors);
            self.ai_engine = Some(engine);
        }
        0
    }

    fn tick_native(&mut self) -> i32 {
        let new_idx = EngineDiscovery::read_sconfig_idx();
        if new_idx >= 0 {
            if let Some(ref mut nc) = self.native_controller {
                nc.switch_scenario(new_idx);
                self.current_scenario_idx = new_idx;
            }
        }

        if let Some(ref mut nc) = self.native_controller {
            if nc.is_enabled() {
                nc.tick(&self.sensors);
            } else if let Some(reason) = nc.disabled_reason() {
                log_warn!("AI-native controller disabled: {}", reason);
            }
        }
        0
    }

    fn handle_config_change(&mut self) {
        if self.native_controller.is_some() {
            log_info!("config change ignored (AI-native mode)");
            return;
        }
        log_info!("thermal config changed, reloading");
        let soc = property_get_str(MI_PROP_SOC_MODEL, "default");
        self.load_thermal_map_impl(&soc);
    }

    fn dump_state(&self, path: &str) {
        let mut content = String::from("# mi_thermald v2.1 dump\n");
        content.push_str(&format!("n_sensors={} n_instances={}\n",
            self.sensors.len(), self.instances.len()));
        for s in &self.sensors {
            content.push_str(&format!("sensor {}={} mC\n",
                s.name, s.last_temp_mc.load(Ordering::Relaxed)));
        }
        for inst in &self.instances {
            content.push_str(&format!("instance {} algo={:?} level={} actions={}\n",
                inst.name, inst.algo as i32, inst.current_level, inst.actions.len()));
        }
        let _ = fs::write(path, &content);
    }
}

// ------------------------------------------------------------------
// Thread workers
// ------------------------------------------------------------------

fn thread_poll_sensors(engine: Arc<Mutex<Engine>>, shutdown: Arc<AtomicBool>) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 1, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 1, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            if let Ok(guard) = engine.lock() {
                for s in &guard.sensors {
                    s.poll();
                }
            }
        }
        libc::close(tfd);
    }
}

fn thread_board_sensor(shutdown: Arc<AtomicBool>) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 2, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 2, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            let v = sysfs::read_int("/sys/class/thermal/thermal_message/board_sensor_temp");
            log_debug!("board sensor temp = {} mC", v);
        }
        libc::close(tfd);
    }
}

fn thread_second_board() {
    let _v = sysfs::read_int("/sys/class/thermal/thermal_message/board_sensor_second_temp");
}

fn thread_second_display() {
    let _v = sysfs::read_int("/sys/class/thermal/thermal_message/display_therm_temp");
}

fn thread_cpu_freq_writer() {
    let paths = [
        (0, "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq", &CPU_FREQ0_TARGET),
        (3, "/sys/devices/system/cpu/cpufreq/policy3/scaling_max_freq", &CPU_FREQ3_TARGET),
        (7, "/sys/devices/system/cpu/cpufreq/policy7/scaling_max_freq", &CPU_FREQ7_TARGET),
    ];
    loop {
        for &(_, path, target) in &paths {
            let val = target.load(std::sync::atomic::Ordering::Relaxed);
            if val > 0 {
                crate::sensor::sysfs::write_int(path, val);
            }
        }
        thread::sleep(Duration::from_millis(50));
        if G_TERM.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }
}

fn thread_fcc_writer() {
    let path = "/sys/class/power_supply/battery/constant_charge_current";
    let mut last_fcc = 0;
    loop {
        let fcc = FCC_VALUE.load(Ordering::Relaxed);
        if fcc > 0 && fcc != last_fcc {
            sysfs::write_int(path, fcc);
            last_fcc = fcc;
        }
        thread::sleep(Duration::from_millis(200));
        if G_TERM.load(Ordering::Relaxed) {
            break;
        }
    }
}

fn thread_charger_temp(shutdown: Arc<AtomicBool>) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 1, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 1, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            let v = sysfs::read_int("/sys/class/thermal/thermal_message/charger_temp");
            log_debug!("[charger_temp {}]", v);
        }
        libc::close(tfd);
    }
}

fn thread_cpu_nolimit(shutdown: Arc<AtomicBool>) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 5, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 5, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            let _v = sysfs::read_int("/sys/class/thermal/thermal_message/cpu_nolimit_temp");
        }
        libc::close(tfd);
    }
}

fn thread_dynamic_ttj(shutdown: Arc<AtomicBool>) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 1, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 1, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            let _v = sysfs::read_int("/sys/kernel/thermal/ttj");
        }
        libc::close(tfd);
    }
}

fn thread_bcl_init(shutdown: Arc<AtomicBool>, boot_completed: Arc<AtomicBool>) {
    // Wait a few seconds for boot to complete, then initialise BCL.
    // (property_get_str is stubbed — the original Android property is
    //  unavailable without libcutils.)
    for _ in 0..5 {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
    boot_completed.store(true, Ordering::Relaxed);
    let _ = set_bcl(0);
}

fn thread_config_watch(
    shutdown: Arc<AtomicBool>,
    reload_tx: std::sync::mpsc::Sender<()>,
) {
    unsafe {
        let ifd = libc::inotify_init1(libc::IN_CLOEXEC);
        if ifd < 0 {
            return;
        }
        let cpath = CString::new(MI_THERMALD_CONFIG_DIR).unwrap();
        let wd = libc::inotify_add_watch(
            ifd,
            cpath.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        );
        if wd < 0 {
            libc::close(ifd);
            return;
        }

        let mut buf = [0u8; 4096];
        while !shutdown.load(Ordering::Relaxed) {
            let len = libc::read(
                ifd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            );
            if len < 0 {
                continue;
            }
            let _ = reload_tx.send(());
        }
        libc::close(ifd);
    }
}

// ------------------------------------------------------------------
// log_upto helper
// ------------------------------------------------------------------

mod log {
    pub fn log_upto(level: i32) -> i32 {
        (1 << (level + 1)) - 1
    }
}

// ------------------------------------------------------------------
// Main
// ------------------------------------------------------------------

fn main() {
    let mut engine = Engine::new();
    if engine.init() != 0 {
        std::process::exit(1);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let boot_completed = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(Mutex::new(engine));
    let (reload_tx, reload_rx) = std::sync::mpsc::channel::<()>();

    let mut threads: Vec<JoinHandle<()>> = Vec::new();

    let e = engine.clone();
    let s = shutdown.clone();
    threads.push(thread::spawn(move || thread_poll_sensors(e, s)));

    // Disabled: Unnecessary polling threads that waste CPU
    // let s = shutdown.clone();
    // threads.push(thread::spawn(move || thread_board_sensor(s)));
    // threads.push(thread::spawn(move || thread_second_board()));
    // threads.push(thread::spawn(move || thread_second_display()));

    threads.push(thread::spawn(|| thread_cpu_freq_writer()));
    threads.push(thread::spawn(|| thread_fcc_writer()));

    // Disabled: Charger temp polling - useless node
    // let s = shutdown.clone();
    // threads.push(thread::spawn(move || thread_charger_temp(s)));

    // Disabled: Unnecessary monitoring threads
    // let s = shutdown.clone();
    // threads.push(thread::spawn(move || thread_cpu_nolimit(s)));
    // let s = shutdown.clone();
    // threads.push(thread::spawn(move || thread_dynamic_ttj(s)));

    let s = shutdown.clone();
    let tx = reload_tx;
    threads.push(thread::spawn(move || thread_config_watch(s, tx)));

    let s = shutdown.clone();
    let bc = boot_completed.clone();
    threads.push(thread::spawn(move || thread_bcl_init(s, bc)));

    // Setup epoll
    let epoll_fd: RawFd;
    let timer_fd: RawFd;
    unsafe {
        epoll_fd = libc::epoll_create1(libc::EPOLL_CLOEXEC);
        if epoll_fd < 0 {
            log_err!("epoll_create: failed");
            return;
        }

        timer_fd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if timer_fd < 0 {
            log_err!("timerfd_create: failed");
            libc::close(epoll_fd);
            return;
        }
        let ts = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 1, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 1, tv_nsec: 0 },
        };
        libc::timerfd_settime(timer_fd, 0, &ts, std::ptr::null_mut());

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN) as u32,
            u64: timer_fd as u64,
        };
        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, timer_fd, &mut ev);
    }

    // Store fds in engine for later use
    {
        let mut eng = engine.lock().unwrap();
        eng.epoll_fd = epoll_fd;
        eng.timer_fd = timer_fd;
    }

    let mut events: Vec<libc::epoll_event> = vec![
        libc::epoll_event { events: 0, u64: 0 };
        MI_EPOLL_MAX_EVENTS
    ];

    loop {
        if let Ok(()) = reload_rx.try_recv() {
            if let Ok(mut eng) = engine.lock() {
                eng.handle_config_change();
            }
        }

        unsafe {
            let n = libc::epoll_wait(
                epoll_fd,
                events.as_mut_ptr(),
                MI_EPOLL_MAX_EVENTS as i32,
                500,
            );
            if n < 0 {
                continue;
            }
            for i in 0..n as usize {
                if events[i].u64 == timer_fd as u64 {
                    let mut exp: u64 = 0;
                    libc::read(timer_fd, &mut exp as *mut _ as *mut libc::c_void, 8);
                    if let Ok(mut eng) = engine.lock() {
                        eng.tick();
                    }
                }
            }
        }

        if G_TERM.load(Ordering::Relaxed) {
            break;
        }
        if G_USR1.load(Ordering::Relaxed) {
            G_USR1.store(false, Ordering::Relaxed);
            if let Ok(eng) = engine.lock() {
                eng.dump_state(MI_THERMALD_DUMP_FILE);
            }
        }
    }

    log_info!("thermald shutdown begin");
    shutdown.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    {
        if let Ok(mut eng) = engine.lock() {
            eng.dump_state(MI_THERMALD_LAST_DUMP_FILE);
            if let Some(ref mut ai) = eng.ai_engine {
                ai.save_checkpoint();
                log_info!("AI: checkpoint saved on shutdown");
            }
            if let Some(ref mut nc) = eng.native_controller {
                nc.save_checkpoint();
                log_info!("AI-native: checkpoint saved on shutdown");
            }
        };
    }
    #[cfg(not(target_os = "android"))]
    unsafe { closelog(); }
}
