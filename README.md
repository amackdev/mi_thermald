# mi_thermald

Rust port of Xiaomi's userspace thermal daemon (`thermald`) — the thermal management engine found on Xiaomi Android devices. Includes a **Q-learning AI layer** that adaptively manages CPU/GPU frequency, charging current, and display brightness based on live thermal state and detected workload.

## Architecture

```
mi_thermald
├── src/
│   ├── main.rs            # Main loop, signal handling, thread workers
│   ├── types.rs           # Core types: Sensor, Instance, Action, Threshold
│   ├── sensor.rs          # Thermal zone / sensor discovery via sysfs
│   ├── config.rs          # OEM thermal config loading (AES-CBC decryption)
│   ├── algorithm.rs       # Traditional threshold-based thermal evaluation
│   ├── action.rs          # Action sysfs writers (cpufreq, GPU, BCL, FCC, ...)
│   ├── thermal_profile.rs # sconfig-node reader, profile ID → workload mode
│   ├── log_macros.rs      # Logging macros (log_info!, log_debug!, etc.)
│   └── ai/
│       ├── mod.rs            # Module re-exports + shared action-interpolation helper
│       ├── engine.rs         # AIEngine — single Q-table, augments traditional config mode
│       ├── native.rs         # NativeController — per-channel-group Q-tables, pure AI mode
│       ├── features.rs       # 34-dim state vector + feature extraction
│       ├── workload.rs       # Workload classification (Idle→Benchmark)
│       ├── qtable.rs         # Q-learning with tile coding
│       ├── tile_coding.rs    # Hash-based tile coding for continuous state
│       ├── rewards.rs        # Multi-objective reward calculation
│       ├── safety.rs         # Hardware-safe action gating + violation circuit breaker
│       └── data_collector.rs # Experience replay logging + Q-table checkpoints
```

## Operation Modes

Two mutually exclusive modes, selected by the Android property `ro.vendor.mi_thermal_ai`:

### 1. Traditional + AI mode (`false` / absent, or `engine`)
- Loads Xiaomi's OEM thermal config files (AES-CBC encrypted) from the scenario selected by the `sconfig` sysfs node
- Evaluates threshold-based instances per sensor (Monitor/SS/SIC/Simulated algorithms, `algorithm.rs`)
- If `ro.vendor.mi_thermal_ai=engine`, an `AIEngine` (single Q-table) can override the traditional thermal level per-instance
- Actions applied: cpufreq scaling_max, GPU boost, BCL current, FCC, hotplug, brightness, etc.

### 2. Pure AI-native mode (`true` / `1`)
- Bypasses all OEM config files entirely
- Self-discovers thermal zones and writable cooling channels via sysfs at startup
- `NativeController` groups channels into four independent control domains — `Compute`, `Thermal`, `Charging`, `Display` — each with its **own** Q-table per scenario (scenario = device mode reported by the `sconfig` sysfs node)
- Directly writes CPU frequency targets, GPU max frequency, `balance_mode`/`boost`, backlight, and battery charge current
- Dedicated 50ms CPU-frequency writer thread and 200ms charge-current writer thread for low-latency, lock-free updates (via shared atomics), independent of the 1s decision tick

## AI Module: Deep Dive

### Q-Learning with Tile Coding

Both controllers implement **Q-learning with tile-coding function approximation**. `AIEngine` runs one Q-table; `NativeController` runs one Q-table *per channel group per scenario* (so up to 4 tables per active scenario, persisted/loaded independently).

| Parameter | Value |
|-----------|-------|
| Actions | 10 (0–9: min cooling → max performance) |
| Learning rate (α) | 0.1 |
| Discount factor (γ) | 0.95 |
| Epsilon (exploration) | 0.3 → 0.05 (decays over ~7 days of ticks) |
| Tile tilings | 8 |
| Tiles per dimension | 4 |
| Hash table size | 262,144 (2¹⁸) |

### State Space (34 dimensions)

| # | Feature | Description |
|---|---------|-------------|
| 1–5 | `t_cpu_max`, `t_cpu_avg`, `t_board_max`, `t_battery`, `t_ambient` | Raw temperatures |
| 6–8 | `dt_cpu`, `dt_board`, `dt_battery` | Temperature derivatives |
| 9–12 | `battery_soc`, `is_charging`, `screen_on`, `time_of_day` | Device context |
| 13 | `thermal_level_traditional` | Current traditional thermal level |
| 14–16 | `t_cpu_ma_10`, `t_board_ma_10`, `thermal_level_ma_10` | 10-tick moving averages |
| 17 | `cpu_freq_ratio` | Normalized CPU frequency |
| 18–19 | `temp_headroom_cpu`, `temp_headroom_battery` | Headroom to throttle thresholds |
| 20 | `temp_variance` | Temperature volatility (workload proxy) |
| 21–22 | `last_action`, `action_stability` | Action history (compute channel) |
| 23–24 | `t_gpu`, `t_charger` | Additional temperatures |
| 25 | `battery_current` | Charge/discharge current |
| 26 | `cpu_load` | CPU utilization from `/proc/stat` |
| 27 | `workload_mode` | Workload classification, 0 (Idle) – 1 (Benchmark) |
| 28–31 | `last_action_compute/thermal/charging/display` | Per-group action history (native mode) |
| 32 | `gpu_freq_ratio` | Current / max GPU devfreq |
| 33 | `brightness_ratio` | Current / max backlight |
| 34 | `charge_current_ratio` | Current / max charge current |

Temperatures are matched by **exact sensor name**, not substring: `t_battery` only accepts a sensor literally named `battery` (thermal zone type) or `battery_temp` (the synthesized `/sys/class/power_supply/battery/temp` sensor). A `.contains("battery")` match would also catch `battery_current` (µA) and `battery_voltage` (µV) — both numerically in the millions — and silently corrupt the feature. The same two sensors, plus `BAT_SOC` (a percentage, not a temperature), are excluded from the `temp_variance` calculation for the same reason.

### Cooling Channels (NativeController)

Discovered at startup via `discover_hardware_channels()`, each assigned to a `ChannelGroup`:

| Channel | Group | Action mapping |
|---------|-------|-----------------|
| `balance_mode` | Compute | `action * 7 / 9`, clamped to [0,7] |
| `boost` | Compute | always written `1` — `balance_mode` handles cooling |
| `cpu_freq0/3/7` (scaling_max_freq) | Compute | `lerp_by_action` across [30% of hw max, hw max] |
| `gpu` (devfreq max_freq) | Compute | `lerp_by_action` across [min, max available freq] |
| `backlight` | Display | not directly written (display level driven by config/PID only) |
| `charge_current` | Charging | not directly written by the Q-table — see [Charging Current Control](#charging-current-control-nativecontroller-only) |

If the active scenario's config defines a `sic` (PID) block targeting one of these channels (e.g. a skin-temperature-controlled thermal channel), `enrich_from_config()` attaches a `PidConfig` to it and reassigns it from `Compute` to `Thermal`. For a PID-backed channel the Q-table's action no longer maps directly to a device value — instead it selects a **setpoint** (`action_to_setpoint`, action 0–9 → an index into the config's `target` array), and a discrete PID controller (`algo_sic_channel`, the same second-order incremental algorithm used by the traditional `Sic` algorithm in `algorithm.rs`) drives the channel toward that setpoint every tick.

### Action → Frequency Mapping

CPU/GPU frequency channels are evenly distributed across their range:

```
freq = min_freq + (action * (max_freq - min_freq) / 9)

Action 0 → min_freq (aggressive cooling)
Action 4 → ~44% of range
Action 9 → max_freq (full performance)
```

Example for a typical Xiaomi device:

| Action | Policy0 (little) | Policy3 (mid) | Policy7 (big) |
|--------|-----------------|---------------|---------------|
| 0 | 604.8 MHz | 841.0 MHz | 904.3 MHz |
| 3 | 1,075.2 MHz | 1,494.4 MHz | 1,607.7 MHz |
| 6 | 1,545.6 MHz | 2,147.8 MHz | 2,310.4 MHz |
| 9 | **2,016.0 MHz** | **2,803.2 MHz** | **3,014.4 MHz** |

When the device is idle (low CPU load, cool temperatures), the agent tends toward **action 9** — the hardware's maximum frequency — since there's no thermal reason to constrain performance.

### Workload Classification (`WorkloadDetector`)

Detection is two-tiered: if the device exposes the `sconfig` thermal-profile
sysfs node, `ThermalProfileManager` maps its profile ID straight to a
`WorkloadMode` (instant, no hysteresis — e.g. profiles 18/39 → PerfGaming,
19/20 → Gaming, 6/10/40 → Benchmark). Profile 0 or a missing node falls back
to sensor-based detection below, which uses hysteresis (~10 ticks) to avoid
mode flapping.

| Mode | Criteria | perf_weight | batt_temp_weight |
|------|----------|-------------|-------------------|
| Idle | Screen off OR `cpu_load < 0.1` | 0.5 | 5.0 |
| Light | `cpu_load > 0.1` | 1.0 | 4.0 |
| Moderate | `cpu_load > 0.3` or GPU > 0.5 | 1.5 | 3.5 |
| Gaming | 30s sustained CPU > 0.5 + GPU > 0.7 | 3.0 | 2.0 |
| PerfGaming | sconfig profile 18/39 (perf-heavy gaming) | 3.5 | 1.75 |
| Benchmark | 60s sustained CPU > 0.8, or sconfig profile 6/10/40 | 4.0 | 1.5 |

`NativeController` additionally tracks sustained scheduler load independent of frequency (`cpu_load > 0.44` for ≥3 consecutive ticks, cleared after 3 idle ticks). When that holds, or the detected mode is Gaming/PerfGaming/Benchmark, the **Compute** group's action is forced to 9 regardless of what its Q-table selected — this only overrides Compute, not Thermal/Charging/Display.

### Reward Function

`AIEngine` (single Q-table, traditional mode):

```
reward = temp_penalty + batt_temp_penalty
       + perf_weight * cpu_freq_ratio
       + battery_current_term (while charging)
       - stability_penalty * |Δlevel|
```

`NativeController` computes a separate reward per channel group (`compute_group_reward`), all sharing the same temperature penalty terms but weighted toward what that group actually controls:

| Group | Reward terms |
|-------|--------------|
| Compute | `temp_penalty + perf_weight*(cpu_freq_ratio + 0.5*gpu_freq_ratio) - stability_penalty` |
| Thermal | `temp_penalty - stability_penalty` |
| Charging | `batt_temp_weight*batt_temp_penalty + battery_weight*charge_current_ratio - stability_penalty` |
| Display | `0.5*temp_penalty + 2.0*brightness_ratio - stability_penalty` |

Temperature penalty: -10 if CPU > 85°C, -0.1×(T-75) if > 75°C, else 0.
Battery temp penalty: context-dependent thresholds (idle/light warn at 35°C/crit 38°C, moderate 37/40°C, gaming 42/44°C, perfgaming 44/45°C, benchmark 45/48°C), plus a separate penalty/reward on the rate of change (heating penalized 1–2×, cooling lightly rewarded).

Each group is credited/blamed using the action it actually took *last* tick against the transition that action produced, not the action just selected for next tick.

### Safety Monitor

`SafetyMonitor` (`ai/safety.rs`) gates every proposed action before it's applied:

- **`is_action_safe(action, sensors)`** — pure predicate, no side effects. Actions 0/1 are always safe. Actions 6–9 (and action 5, if the battery is in its "warm" pre-threshold band) require both `!cpu_over` (no sensor named `cpu*`/`tsens*` above 85°C) and `!battery_over` (the exact `battery`/`battery_temp` sensor above the current workload's crit threshold). An unsafe action is replaced with a fixed fallback (`3`) before being applied.
- **`is_hazard(sensors)`** — true only for a genuine emergency (CPU actually over 85°C). This, not every safety-capped action, is what feeds the violation counter: an ordinary warm-idle battery a few tenths of a degree over its crit line gets capped to a conservative action every tick as designed, but that's the safety layer working correctly, not a violation of it.
- **Circuit breaker** — 10 hazard violations within a rolling 3600-tick (~1 hour) window disables the Q-learning action-selection path entirely (`NativeController.enabled = false` / `AIEngine.enabled = false`), falling back to the traditional/default level. `NativeController::tick()` keeps advancing its tick counter and re-checking `is_disabled()` even while disabled, so it re-enables itself automatically once an hour passes with no fresh hazard — no daemon restart required.
- **Charge protection is exempt from the breaker.** `apply_charge_protection()` (temperature/derivative-based charge-current ramping, below) is called unconditionally every tick from `main.rs`, regardless of whether the Q-learning half of the controller is currently disabled — it's pure safety logic, not a learned decision.

### Battery Temperature Override (NativeController only)

Each tick, the current battery temperature is looked up against a
per-workload-mode throttle ladder (`action_for_battery_temp()` in
`ai/native.rs`) and every group's action is clamped down if the ladder's value is
lower than what its Q-table picked:

| Workload | Ladder (temp °C → action) |
|----------|----------------------------|
| Benchmark | <41.5→9, <43.5→8, <46.0→6, else 5 |
| PerfGaming | <38.0→9, <40.0→8, <42.0→7, <44.0→6, else 5 |
| Gaming | <36.0→9, <38.0→8, <40.0→7, <42.0→6, else 5 |
| Moderate | <36.0→9, <38.0→8, <40.0→7, <41.0→6, <42.0→4, <45.0→3, else 2 |
| Idle/Light | <34.0→9, <36.0→8, <38.0→7, <40.0→6, <43.0→4, <45.0→2, else 0 |

Moderate's tail stays a mild throttle (2) rather than dropping to 0 like
Idle/Light — sustained mixed load rarely reaches these temps in practice
(higher frequency finishes micro-tasks faster, so heating tends to be
self-limiting), so the fallback doesn't need to be as punitive.

### Charging Current Control (NativeController only)

The `charge_current` channel is never driven by its Q-table's raw action value — it's controlled entirely by `apply_temp_based_charge()`, called from `apply_charge_protection()` every tick regardless of AI enable state:

1. **Charger-type gate**: non-fast chargers (not `USB_PD`/`USB_HVDCP`) get a fixed 3A rate; no temperature curve needed.
2. **Absolute-temperature ladder**: full rate below 32°C, then linearly ramped down through several bands (100%→85% at 32–35°C, down through 85%→65%, 65%→45%, 45%→25%, 25%→10%) to a hard stop (0) at ≥52°C.
3. **Derivative-aware ceiling**: the per-tick battery-temp delta is smoothed into a °C/s EMA (α=0.3). Below 0.03°C/s the ladder above is untouched; at/above 0.12°C/s an additional ceiling of 50% of max current is applied on top of whatever the ladder allows — so a battery heating up fast gets capped *before* it crosses into the next absolute-temp band, not just after. This only ever tightens the cap (`.min()`), never loosens it, and only applies while temperature is rising.

The Charging group's Q-table still learns and selects actions — its reward (`charge_current_ratio` term) reflects the *outcome* of this protection logic, giving the RL a signal about charging speed without it ever directly controlling the current.

## Compilation

### Prerequisites

- Rust toolchain (install via [rustup](https://rustup.rs/))
- For Android target: Android NDK with `aarch64-linux-android30-clang`

### Native (Linux / Testing)

```bash
# Build for host
cargo build --release

# Run directly (test mode, no Android sysfs required)
./target/release/mi_thermald
```

### Cross-compile for Android (aarch64)

1. Install the Android NDK and set up the linker in `.cargo/config.toml`:

```toml
# .cargo/config.toml
[target.aarch64-linux-android]
linker = "/path/to/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android30-clang"
```

2. Add the Android target and build:

```bash
rustup target add aarch64-linux-android
cargo build --release --target aarch64-linux-android
```

3. The binary will be at `target/aarch64-linux-android/release/mi_thermald`.

### Optimize for size

Release builds already enable `strip = true` and `lto = true` (see `Cargo.toml`). To strip an existing binary further:

```bash
llvm-strip --strip-all target/release/mi_thermald
```

### Deploying to a device (`deploy.sh`)

On a rooted device, `deploy.sh` builds, then replaces the OEM `thermald` vendor binary in place and restarts it under its original service name/SELinux context:

```bash
cargo build --release --target aarch64-linux-android

adb shell "su -c stop mi_thermald"
adb push target/aarch64-linux-android/release/mi_thermald /sdcard/
adb shell "su -c mount -o remount,rw /vendor"
adb shell "su -c cp /sdcard/mi_thermald /vendor/bin/mi_thermald"
adb shell "su -c chcon u:object_r:mi_thermald_exec:s0 /vendor/bin/mi_thermald"
adb shell "su -c chown root:shell /vendor/bin/mi_thermald"
adb shell "su -c chmod 0755 /vendor/bin/mi_thermald"
adb shell "su -c setprop persist.mithermal.debug 1"
adb shell "su -c start mi_thermald"
```

This requires root and a writable `/vendor` partition, and assumes the OEM `mi_thermald` init service already exists (the binary is being swapped, not newly registered). Use `adb logcat -s thermald` (or `adb shell su -c logcat` filtered similarly) to watch the daemon after `start`.

## Usage

```
mi_thermald [-l <log_level>]
```

- `-l <level>`: Set log level (3=ERR, 4=WARN, 6=INFO, 7=DEBUG). Default: 6.

`log_debug!` calls are additionally gated at runtime by the
`persist.mithermal.debug` Android property (`1`/`true` to enable), checked
once via `OnceLock` on first use — set it before the daemon starts if you
want debug-level tracing without changing `-l`.

The daemon runs as a background service on Android. It uses:
- `timerfd` / `epoll` for 1-second decision tick intervals
- `inotify` to watch for config file changes and reload (ignored in AI-native mode)
- Background threads for sensor polling (1s), CPU freq writing (50ms), FCC/charge-current writing (200ms), and delayed BCL init (5s after boot)

### AI Data Persistence

Q-tables are saved to `/data/vendor/thermal/ai_data/q_scenario_<id>.json` (native mode — one file per scenario, containing per-channel-group `epsilon` and `weights`) or `q_table.json` (traditional+AI mode) every 1000 ticks and on shutdown (`SIGTERM`/`SIGINT`). The same directory holds `experiences_<day>.csv` state/action/reward logs, flushed every 100 records and pruned after 7 days.

### Config File Format (Traditional mode)

OEM thermal configs live in `/data/vendor/thermal/config/` (or `/vendor/etc/`, `/odm/etc/`). They are AES-CBC encrypted files with a fixed key, decrypted at startup/reload into `MI_THERMALD_DECRYPT_DIR`. Each config is a set of `[block_name]`-delimited sections with `key value` lines (`algo_type`, `sensor`, `sensors`, `weight`, `device`, `trig`, `clr`, `target`, `ks`/`ki`/`kc`/`max`/`min` for PID segments, etc.) — see `config::parse_config_blocks`.

## Filesystem Paths

| Path | Purpose |
|------|---------|
| `/data/vendor/thermal/config/` | OEM thermal config directory |
| `/data/vendor/thermal/thermal-global-mode` | Global mode file |
| `/data/vendor/thermal/thermal.dump` | State dump (SIGUSR1) |
| `/data/vendor/thermal/last_thermal.dump` | Last state dump (shutdown) |
| `/data/vendor/thermal/thermald_decrypt/` | Decrypted config dump |
| `/data/vendor/thermal/ai_data/` | AI Q-table checkpoints + experience logs |
| `/sys/devices/system/cpu/cpufreq/policy{0,3,7}/scaling_max_freq` | CPU freq targets |
| `/sys/class/power_supply/battery/constant_charge_current` | Charge current target |
| `/sys/class/kgsl/kgsl-3d0/devfreq/max_freq` | GPU max frequency target |
| `/sys/class/thermal/thermal_message/sconfig` | Thermal profile ID (instant workload detection + scenario selection) |
