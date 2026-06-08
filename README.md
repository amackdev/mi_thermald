# mi_thermald

Rust port of Xiaomi's userspace thermal daemon (`thermald`) — the thermal management engine found on Xiaomi Android devices. Includes a **Q-learning AI module** that adaptively manages CPU frequencies, cooling devices, and battery current based on thermal state and workload.

## Architecture

```
mi_thermald
├── src/
│   ├── main.rs           # Main loop, signal handling, thread workers
│   ├── types.rs          # Core types: Sensor, Instance, Action, Threshold
│   ├── sensor.rs         # Thermal zone / sensor discovery via sysfs
│   ├── config.rs         # OEM thermal config loading (AES-CBC decryption)
│   ├── algorithm.rs      # Traditional threshold-based thermal evaluation
│   ├── action.rs         # Action sysfs writers (cpufreq, GPU, BCL, FCC)
│   ├── log_macros.rs     # Logging macros (log_info!, log_debug!, etc.)
│   └── ai/
│       ├── mod.rs        # Module re-exports (AIEngine, NativeController)
│       ├── engine.rs     # AIEngine — Q-learning for traditional config mode
│       ├── native.rs     # NativeController — Q-learning for pure AI mode
│       ├── features.rs   # 27-dim state vector + feature extraction
│       ├── workload.rs   # Workload classification (Idle→Benchmark)
│       ├── qtable.rs     # Q-learning with tile coding
│       ├── tile_coding.rs # Hash-based tile coding for continuous state
│       ├── rewards.rs    # Multi-objective reward calculation
│       ├── safety.rs     # Hardware-safe action gating
│       └── data_collector.rs # Experience replay logging
```

## Operation Modes

Two mutually exclusive modes, selected by the Android property `ro.vendor.mi_thermal_ai`:

### 1. Traditional + AI mode (`false` / absent)
- Loads Xiaomi's OEM thermal config XML files (AES-CBC encrypted)
- Evaluates threshold-based instances per sensor (Monitor/SS/SIC/Simulated algorithms)
- A `AIEngine` Q-learning agent can override the traditional thermal level per-instance
- Actions applied: cpufreq scaling_max, GPU boost, BCL current, FCC, hotplug, etc.

### 2. Pure AI-native mode (`true` / `1`)
- Bypasses all OEM config files entirely
- Self-discovers thermal zones and cooling channels via sysfs
- `NativeController` runs per-scenario Q-tables (scenario = device mode from sconfig sysfs node)
- Directly writes CPU frequency targets, balance_mode, boost, and battery current
- Dedicated 50ms CPU frequency writer thread for low-latency freq updates

## AI Module: Deep Dive

### Q-Learning with Tile Coding

Both controllers implement **SARSA/Q-learning with tile-coding function approximation**:

| Parameter | Value |
|-----------|-------|
| Actions | 10 (0–9: min cooling → max performance) |
| Learning rate (α) | 0.1 |
| Discount factor (γ) | 0.95 |
| Epsilon (exploration) | 0.3 → 0.05 (decays over ~7 days) |
| Tile tilings | 8 |
| Tiles per dimension | 4 |
| Hash table size | 262,144 (2¹⁸) |

### State Space (27 dimensions)

| # | Feature | Description |
|---|---------|-------------|
| 1–5 | `t_cpu_max`, `t_cpu_avg`, `t_board_max`, `t_battery`, `t_ambient` | Raw temperatures |
| 6–8 | `dt_cpu`, `dt_board`, `dt_battery` | Temperature derivatives |
| 9–12 | `battery_soc`, `is_charging`, `screen_on`, `time_of_day` | Device context |
| 13 | `thermal_level_traditional` | Current traditional thermal level |
| 14–16 | `t_cpu_ma_10`, `t_board_ma_10`, `thermal_level_ma_10` | 10-tick moving averages |
| 17 | `cpu_freq_ratio` | Normalized CPU frequency |
| 18–19 | `temp_headroom_cpu`, `temp_headroom_battery` | Headroom to throttle thresholds |
| 20 | `temp_variance` | Temperature volatility |
| 21–22 | `last_action`, `action_stability` | Action history |
| 23–24 | `t_gpu`, `t_charger` | Additional temperatures |
| 25 | `battery_current` | Charge/discharge current |
| 26 | `cpu_load` | CPU utilization |
| 27 | `workload_mode` | Workload classification |

### Action → Frequency Mapping

Actions are evenly distributed across the CPU's frequency range:

```
freq = min_freq + (action * (max_freq - min_freq) / 9)

Action 0 → min_freq (aggressive cooling)
Action 4 → ~66% of max
Action 9 → max_freq (full performance)
```

Example for a typical Xiaomi device:

| Action | Policy0 (little) | Policy3 (mid) | Policy7 (big) |
|--------|-----------------|---------------|---------------|
| 0 | 604.8 MHz | 841.0 MHz | 904.3 MHz |
| 3 | 1,075.2 MHz | 1,494.4 MHz | 1,607.7 MHz |
| 6 | 1,545.6 MHz | 2,147.8 MHz | 2,310.4 MHz |
| 9 | **2,016.0 MHz** | **2,803.2 MHz** | **3,014.4 MHz** |

When the device is idle (low CPU load, cool temperatures), the agent selects **action 9** — the hardware's maximum frequency — since there's no thermal reason to constrain performance.

### Workload Classification (`WorkloadDetector`)

| Mode | Criteria | perf_weight | batt_temp_weight |
|------|----------|-------------|-----------------|
| Idle | Screen off OR `cpu_load < 0.1` | 0.5 | 5.0 |
| Light | `cpu_load > 0.1` | 1.0 | 4.0 |
| Moderate | `cpu_load > 0.3` or GPU > 0.5 | 1.5 | 3.5 |
| Gaming | 30s sustained CPU > 0.5 + GPU > 0.7 | 3.0 | 2.0 |
| Benchmark | 60s sustained CPU > 0.8 | 4.0 | 1.5 |

### Reward Function

```
reward = temp_penalty + batt_temp_penalty
       + perf_weight * cpu_freq_ratio
       + battery_current_term
       - stability_penalty * |Δaction|
```

Temperature penalty: -10 if CPU > 85°C, -0.1×(T-75) if > 75°C, else 0.
Battery temp penalty: context-dependent thresholds (idle warn at 35°C, gaming warn at 42°C).

### Safety Monitor

Blocks actions that would violate hardware limits:
- CPU > 85°C → only actions 0,1,8,9 allowed
- Battery thresholds vary by workload mode
- After 10 violations in 1 hour → AI permanently disables itself

### Sustained Load Override (NativeController only)

If `cpu_load > 0.5` for 3+ consecutive ticks, forces action 9 (max performance) regardless of what Q-table selects — prevents thermal throttling during sustained bursts.

### Battery Temperature Override (NativeController only)

If battery > 42°C and not in heavy load, clamps action to ≤ 4 (moderate throttling) to protect the battery.

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

4. Push to device:

```bash
adb push target/aarch64-linux-android/release/mi_thermald /data/local/tmp/
adb shell chmod +x /data/local/tmp/mi_thermald
```

### Optimize for size

```bash
cargo build --release
# Strip debug symbols
llvm-strip --strip-all target/release/mi_thermald
```

## Usage

```
mi_thermald [-l <log_level>]
```

- `-l <level>`: Set log level (3=ERR, 4=WARN, 6=INFO, 7=DEBUG). Default: 6.

The daemon runs as a background service on Android. It uses:
- `timerfd` / `epoll` for 10-second tick intervals
- `inotify` to watch for config file changes and reload
- Background threads for sensor polling (10s), CPU freq writing (50ms), FCC writing (200ms)

### AI Data Persistence

Q-tables are saved to `/data/local/tmp/ai_data/q_scenario_*.json` every 1000 ticks and on shutdown.

### Config File Format (Traditional mode)

OEM thermal configs live in `/data/vendor/thermal/config/`. They are AES-CBC encrypted XML files with the key `thermalopenssl.h`. The daemon decrypts them at startup.

## Filesystem Paths

| Path | Purpose |
|------|---------|
| `/data/vendor/thermal/config/` | OEM thermal config directory |
| `/data/vendor/thermal/thermal-global-mode` | Global mode file |
| `/data/vendor/thermal/thermal.dump` | State dump (SIGUSR1) |
| `/data/vendor/thermal/last_thermal.dump` | Last state dump (shutdown) |
| `/data/local/tmp/thermald_decrypt/` | Decrypted config dump |
| `/data/local/tmp/ai_data/` | AI Q-table checkpoints |
| `/sys/devices/system/cpu/cpufreq/policy{0,3,7}/scaling_max_freq` | CPU freq targets |
| `/sys/class/thermal/thermal_message/board_sensor_temp` | Board temperature |
