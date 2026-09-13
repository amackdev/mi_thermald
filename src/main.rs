// Allow dead code for config-driven dispatch types invisible to the compiler.
#![allow(dead_code)]

#[macro_use]
mod log_macros;

mod types;
mod sensor;
mod config;
mod algorithm;
mod action;
mod ai;
mod thermal_profile;
mod web;

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

// The log_* macros pass Android log-priority numbers (VERBOSE=2, DEBUG=3,
// INFO=4, WARN=5, ERROR=6 — see android_LogPriority) since that's what the
// real __android_log_write() below expects. POSIX syslog() uses a different,
// inverted numbering (LOG_EMERG=0 .. LOG_DEBUG=7 — see LOG_* in types.rs), so
// on non-Android builds we must translate or every severity comes out wrong
// (e.g. errors logged as LOG_INFO, debug spam logged as LOG_ERR).
#[cfg(not(target_os = "android"))]
fn android_prio_to_syslog(priority: libc::c_int) -> libc::c_int {
    match priority {
        2 => LOG_DEBUG,   // VERBOSE
        3 => LOG_DEBUG,   // DEBUG
        4 => LOG_INFO,    // INFO
        5 => LOG_WARNING, // WARN
        6 => LOG_ERR,     // ERROR
        7 => LOG_ERR,     // FATAL
        _ => LOG_INFO,
    }
}

#[cfg(not(target_os = "android"))]
pub fn android_log_write(priority: libc::c_int, msg: *const libc::c_char) {
    unsafe { crate::syslog_ffi(android_prio_to_syslog(priority), msg); }
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

// Check if debug logging is enabled via persist.mithermal.debug property
pub fn is_debug_enabled() -> bool {
    use std::sync::OnceLock;
    static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

    *DEBUG_ENABLED.get_or_init(|| {
        let debug_prop = property_get_str("persist.mithermal.debug", "0");
        debug_prop == "1" || debug_prop == "true"
    })
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
// Action default-value resolution
// ------------------------------------------------------------------

/// A level-0 ("off"/no-throttle) action configured with value=0 means "leave
/// the current hardware state alone" rather than literally writing 0 (which
/// would e.g. cap CPU freq to zero). Probe the live sysfs state so it gets
/// re-asserted instead of clobbered.
fn resolve_default_action_value(action: &mut Action) {
    if action.value != 0 {
        return;
    }
    match action.type_ {
        ActionType::CpuFreq => {
            let path = cpuinfo_max_path(&action.target);
            let v = sysfs::read_int(&path);
            action.value = if v > 0 { v } else { 3000000 };
        }
        ActionType::CpuHotplug => {
            let n = action.target.strip_prefix("hotplug_cpu")
                .or_else(|| action.target.strip_prefix("cpu"))
                .unwrap_or(&action.target);
            let path = format!("/sys/devices/system/cpu/cpu{}/online", n);
            let v = sysfs::read_int(&path);
            action.value = if v > 0 { v } else { 1 };
        }
        ActionType::Bcl => {
            let v = sysfs::read_int(
                "/sys/class/power_supply/battery/constant_charge_current"
            );
            action.value = if v > 0 { v } else { 5000000 };
        }
        ActionType::Fcc => {
            let mut v = sysfs::read_int(
                "/sys/class/power_supply/battery/constant_charge_current_max"
            );
            if v <= 0 {
                v = sysfs::read_int(
                    "/sys/class/power_supply/battery/constant_charge_current"
                );
            }
            action.value = if v > 0 { v } else { 6000000 };
        }
        _ => {}
    }
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
    timer_fd: RawFd,

    current_scenario_idx: i32,

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
            timer_fd: -1,
            current_scenario_idx: 0,
            ai_engine: None,
            native_controller: None,
        }
    }

    fn init(&mut self) -> i32 {
        let args: Vec<String> = std::env::args().collect();
        #[allow(unused)]
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

        #[cfg(not(target_os = "android"))]
        unsafe {
            fn log_upto(level: i32) -> i32 {
                (1 << (level + 1)) - 1
            }
            let ident = CString::new(MI_THERMALD_VERSION_STRING).unwrap();
            openlog(ident.as_ptr(), LOG_PID | LOG_NDELAY, LOG_DAEMON);
            setlogmask(log_upto(log_level));
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
        let ai_engine_enabled = ai_prop == "engine";

        if ai_enabled {
            log_info!("AI-native mode: skipping OEM config, discovering hardware");

            EngineDiscovery::sensor_init(&mut self.sensors);

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

            // Apply conservative CPU freq caps immediately at boot so the
            // writer thread enforces them before the first thermal tick fires
            // (~1s later). Without this, the CPU runs at hardware-max for
            // the entire first second and temperatures spike to 90°C+.
            Self::apply_boot_freq_caps();

            if ai_engine_enabled {
                match ai::AIEngine::new() {
                    Ok(engine) => {
                        self.ai_engine = Some(engine);
                        log_info!("AI Engine enabled (augmenting traditional config)");
                    }
                    Err(e) => {
                        log_warn!("AI Engine failed to initialize: {}", e);
                    }
                }
            }
        }

        0
    }

    /// Write conservative CPU frequency caps immediately at startup.
    /// The `thread_cpu_freq_writer` loop reads these atomics every 50ms,
    /// so the caps take effect well before the first thermal tick (~1s).
    fn apply_boot_freq_caps() {
        // 60% of each cluster's max — enough for a fast boot, cool enough
        // to avoid the 90°C+ spike that occurs when the CPU runs at full
        // speed for the entire first second before thermal control kicks in.
        let caps = [
            ("/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq", 1209600, &CPU_FREQ0_TARGET),
            ("/sys/devices/system/cpu/cpufreq/policy3/scaling_max_freq", 1681920, &CPU_FREQ3_TARGET),
            ("/sys/devices/system/cpu/cpufreq/policy7/scaling_max_freq", 1808640, &CPU_FREQ7_TARGET),
        ];
        for (path, val, atomic) in &caps {
            atomic.store(*val, Ordering::Relaxed);
            sysfs::write_int(path, *val);
        }
        log_info!("boot freq caps applied: policy0=1209 policy3=1681 policy7=1808 MHz");
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
            log_debug!("sconfig changed {} -> {}, reloading scenario",
                self.current_scenario_idx, new_idx);
            let soc = property_get_str(MI_PROP_SOC_MODEL, "default");
            self.load_thermal_map_impl(&soc);
            self.current_scenario_idx = new_idx;
            return 0;
        }

        self.evaluate_virtual_sensors();

        let mut ai_engine = self.ai_engine.take();
        let ai_enabled = ai_engine.as_ref()
            .map(|ai| ai.is_enabled())
            .unwrap_or(false);

        for i in (0..self.instances.len()).rev() {
            self.tick_instance(i, ai_engine.as_mut(), ai_enabled);
        }

        if let Some(mut engine) = ai_engine {
            engine.end_tick(&self.sensors);
            self.ai_engine = Some(engine);
        }
        0
    }

    // Always evaluate all Virtual-type sensors (they may be defined in a
    // base config and referenced by scenarios that lack a Virtual block).
    fn evaluate_virtual_sensors(&self) {
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
    }

    fn tick_instance(&mut self, i: usize, ai_engine: Option<&mut ai::AIEngine>, ai_enabled: bool) {
        if self.instances[i].algo == AlgoType::Virtual {
            return;
        }
        let sensor_idx = match self.instances[i].sensor_idx {
            Some(idx) => idx,
            None => return,
        };

        let t = self.sensors[sensor_idx].last_temp_mc.load(Ordering::Relaxed);
        let prev_level = self.instances[i].current_level;
        let traditional_level = evaluate_instance(
            &mut self.instances[i],
            t,
            &mut self.sic_states[i],
        );

        let (level, ai_actions) = if ai_enabled {
            let engine = ai_engine.unwrap();
            let state = engine.collect_state(&self.sensors, &self.instances[i], traditional_level);
            let directive = engine.decide_action(&state, traditional_level, &self.instances[i], &self.sensors);
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

        if self.instances[i].actions.is_empty() {
            return;
        }

        if let Some(ref ai_acts) = ai_actions {
            for a in ai_acts {
                if level != prev_level {
                    log_debug!("apply {} level={} {:?}[{}] = {}",
                        self.instances[i].name, level, a.type_, a.target, a.value);
                }
            }
            action_apply_multi(ai_acts);
            return;
        }

        self.apply_traditional_level(i, sensor_idx, t, prev_level, level);
    }

    fn apply_traditional_level(&mut self, i: usize, sensor_idx: usize, t: i32, prev_level: i32, level: i32) {
        let n_levels = self.instances[i].threshold.n_levels();
        let effective_levels = if n_levels > 0 { n_levels + 1 } else { 1 };
        let per_dev = self.instances[i].actions.len() / effective_levels;
        let start = (level as usize) * per_dev;
        let mut end = start + per_dev;
        log_debug!("tick instance {} sensor={} temp={} level={}->{} n_actions={}/{}/{}",
            self.instances[i].name,
            sensor_idx, t, prev_level, level,
            self.instances[i].actions.len(), n_levels, effective_levels);
        if start >= self.instances[i].actions.len() {
            return;
        }
        if end > self.instances[i].actions.len() {
            end = self.instances[i].actions.len();
        }

        if level == 0 {
            for j in start..end {
                resolve_default_action_value(&mut self.instances[i].actions[j]);
            }
        }

        if self.instances[i].algo == AlgoType::Sic {
            let pid_v = self.instances[i].current_value;
            for j in start..end {
                self.instances[i].actions[j].value = pid_v;
            }
            // FIX BUG-004: Use Release ordering for cross-thread visibility
            FCC_VALUE.store(pid_v, Ordering::Release);
        }

        for j in start..end {
            let a = &self.instances[i].actions[j];
            if level != prev_level {
                log_debug!("apply {} level={} {:?}[{}] = {}",
                    self.instances[i].name, level, a.type_, a.target, a.value);
            }
        }
        action_apply_multi(&self.instances[i].actions[start..end]);
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
            // Charge-current thermal protection must run every tick
            // regardless of whether the Q-learning action-selection path
            // has safety-disabled itself.
            nc.apply_charge_protection();

            // Always call tick(), even while disabled: it owns the tick
            // counter that SafetyMonitor's timed recovery depends on, and it
            // re-enables itself internally once that window elapses. Gating
            // this call on is_enabled() would freeze that counter forever,
            // making the "temporary" safety disable permanent.
            nc.tick(&self.sensors);
            if !nc.is_enabled() {
                if let Some(reason) = nc.disabled_reason() {
                    log_warn!("AI-native controller disabled: {}", reason);
                }
            }
        }
        0
    }

    fn handle_config_change(&mut self) {
        if self.native_controller.is_some() {
            log_debug!("config change ignored (AI-native mode)");
            return;
        }
        log_debug!("thermal config changed, reloading");
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

/// Create a periodic timerfd and run `body` each tick until `shutdown` is set.
fn run_periodic_timer(
    interval_secs: i64,
    shutdown: &AtomicBool,
    mut body: impl FnMut(),
) {
    unsafe {
        let tfd = libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC);
        if tfd < 0 {
            return;
        }
        let its = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: interval_secs, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: interval_secs, tv_nsec: 0 },
        };
        libc::timerfd_settime(tfd, 0, &its, std::ptr::null_mut());
        while !shutdown.load(Ordering::Relaxed) {
            let mut exp: u64 = 0;
            libc::read(tfd, &mut exp as *mut _ as *mut libc::c_void, 8);
            body();
        }
        libc::close(tfd);
    }
}

fn thread_poll_sensors(engine: Arc<Mutex<Engine>>, shutdown: Arc<AtomicBool>) {
    run_periodic_timer(1, &shutdown, || {
        if let Ok(guard) = engine.lock() {
            for s in &guard.sensors {
                s.poll();
            }
        }
    });
}

fn thread_cpu_freq_writer() {
    let paths = [
        (0, "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq", &CPU_FREQ0_TARGET),
        (3, "/sys/devices/system/cpu/cpufreq/policy3/scaling_max_freq", &CPU_FREQ3_TARGET),
        (7, "/sys/devices/system/cpu/cpufreq/policy7/scaling_max_freq", &CPU_FREQ7_TARGET),
    ];
    loop {
        for &(_, path, target) in &paths {
            // FIX BUG-004: Use Acquire ordering to ensure visibility of writes
            let val = target.load(std::sync::atomic::Ordering::Acquire);
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
        // FIX BUG-004: Use Acquire ordering to ensure visibility of writes
        let fcc = FCC_VALUE.load(Ordering::Acquire);
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

    threads.push(thread::spawn(|| thread_cpu_freq_writer()));
    threads.push(thread::spawn(|| thread_fcc_writer()));

    let s = shutdown.clone();
    let tx = reload_tx;
    threads.push(thread::spawn(move || thread_config_watch(s, tx)));

    let s = shutdown.clone();
    let bc = boot_completed.clone();
    threads.push(thread::spawn(move || thread_bcl_init(s, bc)));

    // Web server thread (only when debug property is set)
    if is_debug_enabled() {
        let e = engine.clone();
        let s = shutdown.clone();
        threads.push(thread::spawn(move || web::thread_web_server(e, s)));
    }

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
