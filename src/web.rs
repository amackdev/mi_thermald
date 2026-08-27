use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Snapshot: serialisable snapshot of daemon state, sent as JSON
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub(crate) struct WebSnapshot {
    ts: u64,
    scenario: i32,
    mode: String,
    sensors: Vec<SensorInfo>,
    instances: Vec<InstanceInfo>,
    battery: BatteryInfo,
    cpu_load: f32,
    cpu_freq_mhz: Vec<i32>,
    gpu_freq_mhz: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    native: Option<NativeInfo>,
}

#[derive(serde::Serialize, Default)]
struct SensorInfo {
    name: String,
    c: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    grp: Option<String>,
}

#[derive(serde::Serialize)]
struct InstanceInfo {
    name: String,
    algo: String,
    level: i32,
}

#[derive(serde::Serialize, Default)]
struct BatteryInfo {
    temp_c: f64,
    soc: i32,
    current_ma: i32,
    charging: bool,
    health: String,
}

#[derive(serde::Serialize)]
struct NativeInfo {
    enabled: bool,
    reason: Option<String>,
    channels: Vec<ChannelInfo>,
}

#[derive(serde::Serialize)]
struct ChannelInfo {
    name: String,
    group: String,
    value: i32,
    min: i32,
    max: i32,
    pct: f64,
}

// ---------------------------------------------------------------------------
// CPU load: delta-based instantaneous measurement across calls
// ---------------------------------------------------------------------------

struct CpuLoadTracker {
    prev_idle: u64,
    prev_total: u64,
}

static mut CPU_TRACKER: CpuLoadTracker = CpuLoadTracker { prev_idle: 0, prev_total: 0 };

fn read_cpu_load() -> f32 {
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

    unsafe {
        let prev_total = CPU_TRACKER.prev_total;
        let prev_idle = CPU_TRACKER.prev_idle;
        CPU_TRACKER.prev_idle = idle;
        CPU_TRACKER.prev_total = total;

        if prev_total == 0 || total <= prev_total {
            return 0.0;
        }
        let dt = total - prev_total;
        let di = idle - prev_idle;
        if dt == 0 { 0.0 } else { ((dt - di) as f64 / dt as f64) as f32 }
    }
}

fn read_cpu_freqs() -> Vec<i32> {
    let policies = [
        "/sys/devices/system/cpu/cpufreq/policy0",
        "/sys/devices/system/cpu/cpufreq/policy3",
        "/sys/devices/system/cpu/cpufreq/policy7",
    ];
    policies.iter().map(|base| {
        let cur = crate::sensor::sysfs::read_int(&format!("{}/scaling_cur_freq", base));
        if cur > 0 { cur / 1000 } else { 0 }
    }).collect()
}

fn read_gpu_freq_mhz() -> i32 {
    let cur = crate::sensor::sysfs::read_int(
        "/sys/class/kgsl/kgsl-3d0/devfreq/cur_freq"
    );
    if cur > 0 { cur / 1_000_000 } else { 0 }
}

fn read_battery_info() -> BatteryInfo {
    let temp_raw = crate::sensor::sysfs::read_int("/sys/class/power_supply/battery/temp");
    let temp_c = if temp_raw >= 0 { temp_raw as f64 / 10.0 } else { 0.0 };
    let soc = crate::sensor::sysfs::read_int("/sys/class/power_supply/battery/capacity").max(0);
    let current = crate::sensor::sysfs::read_int("/sys/class/power_supply/battery/current_now");
    let current_ma = current.abs() / 1000;
    let status = crate::sensor::sysfs::read_string("/sys/class/power_supply/battery/status")
        .unwrap_or_default();
    let charging = status.trim() == "Charging" || status.trim() == "Full";
    let health = crate::sensor::sysfs::read_string("/sys/class/power_supply/battery/health")
        .unwrap_or_else(|| "Unknown".into());
    BatteryInfo { temp_c, soc, current_ma, charging, health }
}

// ---------------------------------------------------------------------------
// Collect snapshot from Engine (called under lock)
// ---------------------------------------------------------------------------

pub fn collect_snapshot(engine: &crate::Engine) -> WebSnapshot {
    let scenario = engine.current_scenario_idx;
    let mode = if engine.native_controller.is_some() {
        "native"
    } else if engine.ai_engine.is_some() {
        "engine"
    } else {
        "traditional"
    }.to_string();

    let sensor_groups: &[(&[&str], &str)] = &[
        (&["cpu", "tsens"], "CPU"),
        (&["gpu", "kgsl"], "GPU"),
        (&["battery_temp"], "Battery"),
        (&["board", "skin"], "Board"),
        (&["charger", "connector"], "Charger"),
    ];

    let non_thermal = ["BAT_SOC", "battery_current", "battery_voltage"];
    let sensors: Vec<SensorInfo> = engine.sensors.iter().filter(|s| !non_thermal.contains(&s.name.as_str())).map(|s| {
        let mc = s.last_temp_mc.load(Ordering::Relaxed);
        let c = (mc as f64) / 1000.0;
        let low = s.name.to_lowercase();
        let grp = sensor_groups.iter()
            .find(|(kws, _)| kws.iter().any(|kw| low.contains(kw)))
            .map(|(_, g)| g.to_string());
        SensorInfo { name: s.name.clone(), c, grp }
    }).collect();

    let instances: Vec<InstanceInfo> = engine.instances.iter().map(|inst| {
        InstanceInfo {
            name: inst.name.clone(),
            algo: format!("{:?}", inst.algo),
            level: inst.current_level,
        }
    }).collect();

    let cpu_freq_mhz = read_cpu_freqs();
    let gpu_freq_mhz = read_gpu_freq_mhz();
    let cpu_load = read_cpu_load();
    let battery = read_battery_info();

    let native = engine.native_controller.as_ref().map(|nc| {
        let channels: Vec<ChannelInfo> = nc.channels.iter().map(|ch| {
            let live_value = match ch.name.as_str() {
                "charge_current" | "backlight" => {
                    crate::sensor::sysfs::read_int(&ch.path).max(0)
                }
                _ => ch.value,
            };
            let range = (ch.max_val - ch.min_val).max(1);
            let pct = ((live_value - ch.min_val) as f64 / range as f64 * 100.0).clamp(0.0, 100.0);
            ChannelInfo {
                name: ch.name.clone(),
                group: format!("{:?}", ch.group),
                value: live_value,
                min: ch.min_val,
                max: ch.max_val,
                pct,
            }
        }).collect();

        NativeInfo {
            enabled: nc.is_enabled(),
            reason: nc.disabled_reason().map(|s| s.to_string()),
            channels,
        }
    });

    WebSnapshot {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        scenario,
        mode,
        sensors,
        instances,
        battery,
        cpu_load,
        cpu_freq_mhz,
        gpu_freq_mhz,
        native,
    }
}

// ---------------------------------------------------------------------------
// Web server thread
// ---------------------------------------------------------------------------

pub fn thread_web_server(
    engine: Arc<Mutex<crate::Engine>>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let port: u16 = std::env::var("THERMALD_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);

    let server = match tiny_http::Server::http(format!("0.0.0.0:{}", port)) {
        Ok(s) => {
            log_info!("web server listening on port {}", port);
            s
        }
        Err(e) => {
            log_err!("web server failed to bind port {}: {}", port, e);
            return;
        }
    };

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let request = match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(_) => continue,
        };

        let url = request.url().to_string();

        match url.as_str() {
            "/api/snapshot" => {
                let body = if let Ok(eng) = engine.lock() {
                    serde_json::to_string(&collect_snapshot(&eng))
                        .unwrap_or_else(|_| "{}".into())
                } else {
                    "{}".into()
                };
                let r = tiny_http::Response::from_string(body)
                    .with_header(
                        tiny_http::Header::from_bytes(b"Content-Type", b"application/json").unwrap()
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(b"Access-Control-Allow-Origin", b"*").unwrap()
                    );
                let _ = request.respond(r);
            }
            "/" | "/index.html" => {
                let r = tiny_http::Response::from_string(DASHBOARD_HTML)
                    .with_header(
                        tiny_http::Header::from_bytes(b"Content-Type", b"text/html; charset=utf-8").unwrap()
                    );
                let _ = request.respond(r);
            }
            _ => {
                let _ = request.respond(
                    tiny_http::Response::from_string("Not Found").with_status_code(404)
                );
            }
        }
    }

    log_info!("web server stopped");
}

// ---------------------------------------------------------------------------
// Embedded HTML dashboard
// ---------------------------------------------------------------------------

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<meta name="theme-color" content="#080c12">
<title>mi_thermald</title>
<style>
:root{
  --bg:#080c12;--surface:#0e1219;--card:#111820;--border:#1a2233;
  --text:#d0d8e4;--dim:#506070;--accent:#00e5b0;--accent2:#00b4d8;
  --red:#ff4757;--orange:#ff9f43;--yellow:#ffd93d;--green:#26de81;--blue:#45aaf2;
  --purple:#a55eea;--pink:#fd79a8;--cyan:#0abde3;
  --cpu0:#45aaf2;--cpu3:#a55eea;--cpu7:#ff6b6b;--gpu:#feca57;
  --radius:10px;
}
*{margin:0;padding:0;box-sizing:border-box}
body{background:var(--bg);color:var(--text);font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;font-size:13px;line-height:1.5;padding:10px;max-width:1440px;margin:0 auto;-webkit-font-smoothing:antialiased}

.hdr{display:flex;justify-content:space-between;align-items:center;padding:10px 4px;border-bottom:1px solid var(--border);margin-bottom:14px}
.hdr h1{font-size:15px;font-weight:700;color:var(--accent);letter-spacing:.5px}
.hdr .sub{color:var(--dim);font-size:11px;margin-top:1px}
.hdr .rt{text-align:right}
.hdr .live{display:inline-flex;align-items:center;gap:5px;font-size:11px;color:var(--dim)}
.hdr .dot{width:7px;height:7px;border-radius:50%;background:var(--green);animation:blink 2s infinite}
.hdr .dot.off{background:var(--red);animation:none}
@keyframes blink{0%,100%{opacity:1}50%{opacity:.3}}

/* ---- Top gauges row ---- */
.gauges{display:flex;gap:10px;margin-bottom:12px;flex-wrap:wrap}
.gauge-card{flex:1;min-width:140px;background:var(--card);border:1px solid var(--border);border-radius:var(--radius);padding:12px;text-align:center}
.gauge-card .label{font-size:10px;color:var(--dim);text-transform:uppercase;letter-spacing:1.2px;margin-bottom:6px}
.gauge-ring{position:relative;width:90px;height:90px;margin:0 auto 6px}
.gauge-ring svg{transform:rotate(-90deg)}
.gauge-ring .val{position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);font-size:18px;font-weight:700;font-variant-numeric:tabular-nums}
.gauge-card .sub-val{font-size:11px;color:var(--dim)}

/* ---- Grid ---- */
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:10px;margin-bottom:12px}
.card{background:var(--card);border:1px solid var(--border);border-radius:var(--radius);padding:12px}
.card h2{font-size:10px;color:var(--dim);text-transform:uppercase;letter-spacing:1.5px;margin-bottom:8px;padding-bottom:5px;border-bottom:1px solid var(--border)}
.full{grid-column:1/-1}

/* ---- Sensors ---- */
.s-group{margin-bottom:6px}
.s-group-title{font-size:10px;color:var(--accent2);letter-spacing:1px;margin:6px 0 3px;text-transform:uppercase}
.s-row{display:flex;align-items:center;padding:2px 0;font-size:12px}
.s-row .sn{color:var(--dim);flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:11px}
.s-row .sv{font-weight:600;min-width:60px;text-align:right;font-variant-numeric:tabular-nums;font-size:12px}
.s-row .sb{width:50px;height:5px;background:#151d27;border-radius:3px;margin:0 8px;overflow:hidden;flex-shrink:0}
.s-row .sf{height:100%;border-radius:3px;transition:width .6s ease}
.t-cold{color:#60a5fa}.t-warm{color:#fbbf24}.t-hot{color:#fb923c}.t-crit{color:#ef4444}

/* ---- Freq bars ---- */
.fb{margin:5px 0}
.fb-hd{font-size:11px;color:var(--dim);display:flex;justify-content:space-between}
.fb-hd b{color:var(--text);font-weight:600}
.fb-tr{height:7px;background:#151d27;border-radius:4px;overflow:hidden;margin-top:2px}
.fb-fl{height:100%;border-radius:4px;transition:width .5s ease}

/* ---- Metrics ---- */
.m{display:flex;justify-content:space-between;padding:3px 0;font-size:12px}
.m .k{color:var(--dim)}.m .v{font-weight:600;font-variant-numeric:tabular-nums}

/* ---- Battery ---- */
.bat-bar{height:20px;background:#151d27;border-radius:6px;overflow:hidden;margin:8px 0;position:relative}
.bat-fill{height:100%;border-radius:6px;transition:width .5s ease;display:flex;align-items:center;justify-content:center;font-size:10px;font-weight:700;color:#fff;text-shadow:0 1px 2px rgba(0,0,0,.5)}

/* ---- Table ---- */
table{width:100%;border-collapse:collapse;font-size:11px}
th{text-align:left;color:var(--dim);font-weight:500;padding:4px 6px;border-bottom:1px solid var(--border);text-transform:uppercase;letter-spacing:1px;font-size:10px}
td{padding:4px 6px;border-bottom:1px solid #131b25}
.lvl{display:inline-block;padding:1px 7px;border-radius:4px;font-weight:600;font-size:11px}
.l0{background:#0d2818;color:#26de81}.l1{background:#1a2f1a;color:#7bed9f}
.l2{background:#2f2a0a;color:#ffd93d}.l3{background:#2f1e0a;color:#ff9f43}
.l4{background:#2f0a0a;color:#ff6b6b}.l5{background:#3a0a0a;color:#ff4757}

/* ---- Channels ---- */
.ch-sec{margin-bottom:8px}
.ch-sec-title{font-size:10px;letter-spacing:1px;margin:8px 0 4px;text-transform:uppercase;font-weight:600}
.ch-sec-title.gc{color:var(--cpu0)}.ch-sec-title.gt{color:var(--red)}
.ch-sec-title.gg{color:var(--green)}.ch-sec-title.gd{color:var(--yellow)}
.ch-r{display:flex;align-items:center;gap:6px;padding:2px 0;font-size:12px}
.ch-r .cn{color:var(--dim);width:110px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:11px}
.ch-r .cbg{flex:1;height:5px;background:#151d27;border-radius:3px;overflow:hidden}
.ch-r .cb{height:100%;border-radius:3px;transition:width .4s ease}
.ch-r .cv{min-width:75px;text-align:right;font-variant-numeric:tabular-nums;font-size:11px}

/* ---- Chart ---- */
.cw{position:relative;height:180px;background:#0a0f16;border-radius:8px;overflow:hidden;margin-top:6px;border:1px solid var(--border)}
canvas{width:100%;height:100%}
.lgd{display:flex;flex-wrap:wrap;gap:8px;margin-top:8px;font-size:10px}
.lgd span{display:flex;align-items:center;gap:4px;color:var(--dim)}
.lgd .ld{width:10px;height:3px;border-radius:2px;flex-shrink:0}

/* ---- Empty state ---- */
.empty{color:var(--dim);text-align:center;padding:16px;font-size:12px;font-style:italic}

/* ---- Responsive ---- */
@media(max-width:600px){
  .gauges{flex-direction:column}
  .gauge-card{flex:none}
  .grid{grid-template-columns:1fr}
}
.togglable{cursor:pointer;user-select:none}
.arrow{font-size:12px;margin-left:6px;transition:transform .2s}
</style>
</head>
<body>

<div class="hdr">
  <div>
    <h1>MI_THERMALD</h1>
    <div class="sub" id="sub">loading...</div>
  </div>
  <div class="rt">
    <div class="live"><span class="dot off" id="dot"></span><span id="status">connecting</span></div>
    <div class="sub" id="latency"></div>
  </div>
</div>

<div class="gauges" id="gauges">
  <div class="gauge-card"><div class="label">CPU Temp</div><div class="gauge-ring" id="g-cpu"></div><div class="sub-val" id="g-cpu-sub"></div></div>
  <div class="gauge-card"><div class="label">Battery</div><div class="gauge-ring" id="g-bat"></div><div class="sub-val" id="g-bat-sub"></div></div>
  <div class="gauge-card"><div class="label">CPU Load</div><div class="gauge-ring" id="g-load"></div><div class="sub-val" id="g-load-sub"></div></div>
  <div class="gauge-card"><div class="label">Charge</div><div class="gauge-ring" id="g-charge"></div><div class="sub-val" id="g-charge-sub"></div></div>
</div>

<div class="grid">
  <div class="card"><h2 class="togglable" onclick="toggleSensors()">Thermal Sensors <span id="sens-arrow" class="arrow">&#9654;</span></h2><div id="sensors" style="display:none"></div></div>
  <div class="card"><h2>CPU / GPU</h2><div id="freqs"></div></div>
  <div class="card"><h2>Battery</h2><div id="battery"></div></div>
  <div class="card"><h2>AI Controller</h2><div id="ai"></div></div>
  <div class="card full" id="ch-card" style="display:none"><h2>Cooling Channels</h2><div id="channels"></div></div>
  <div class="card full"><h2>Thermal Instances</h2><table><thead><tr><th>Name</th><th>Algorithm</th><th>Level</th></tr></thead><tbody id="inst"></tbody></table></div>
  <div class="card full"><h2>Temperature History</h2><div class="cw"><canvas id="chart"></canvas></div><div class="lgd" id="legend"></div></div>
</div>

<script>
const HMAX=120, COLORS=['#45aaf2','#ff6b6b','#feca57','#26de81','#a55eea','#fd79a8','#0abde3','#ff9f43'];
let hist={},fc=0,st=0,abortCtrl=null,pollTimer=null,pollDelay=1000;

function $(id){return document.getElementById(id)}
function esc(s){const d=document.createElement('div');d.textContent=s;return d.innerHTML}

function toggleSensors(){
  const el=$('sensors');
  const arrow=$('sens-arrow');
  const show=el.style.display==='none';
  el.style.display=show?'':'none';
  arrow.textContent=show?'\u25bc':'\u25b6';
}

function ring(el,pct,color,valText,subText){
  const r=38,c=2*Math.PI*r,off=c*(1-Math.min(pct,100)/100);
  el.innerHTML=`<svg width="90" height="90"><circle cx="45" cy="45" r="${r}" fill="none" stroke="#151d27" stroke-width="6"/><circle cx="45" cy="45" r="${r}" fill="none" stroke="${color}" stroke-width="6" stroke-linecap="round" stroke-dasharray="${c}" stroke-dashoffset="${off}" style="transition:stroke-dashoffset .6s ease"/></svg><div class="val" style="color:${color}">${valText}</div>`;
}

function tc(c){return c<40?'t-cold':c<55?'t-warm':c<75?'t-hot':'t-crit'}
function bc(c){return c<40?'#45aaf2':c<55?'#fbbf24':c<75?'#fb923c':'#ef4444'}
function lc(l){return l<=0?'l0':l<=1?'l1':l<=2?'l2':l<=3?'l3':l<=4?'l4':'l5'}
function wc(w){w=w.toLowerCase();if(w.includes('idle'))return'w-idle';if(w.includes('light'))return'w-light';if(w.includes('moderate'))return'w-moderate';if(w.includes('perf'))return'w-perfgaming';if(w.includes('gaming'))return'w-gaming';if(w.includes('benchmark'))return'w-benchmark';return'w-moderate'}
function fmtT(c){return c.toFixed(1)+'\u00b0C'}

// ---- Gauges ----
function renderGauges(d){
  const s=d.sensors||[];
  const cpuMax=s.filter(x=>x.grp==='CPU').reduce((m,x)=>Math.max(m,x.c),0);
  const batC=d.battery.temp_c;
  const load=(d.cpu_load*100);
  const batSoc=d.battery.soc;

  ring($('g-cpu'),cpuMax/100*100,bc(cpuMax),fmtT(cpuMax),'max sensor');
  $('g-cpu-sub').textContent=s.filter(x=>x.grp==='CPU').length+' sensors';

  const batColor=batC<35?'#26de81':batC<42?'#feca57':'#ff4757';
  ring($('g-bat'),batSoc,batColor,batSoc+'%',fmtT(batC));
  $('g-bat-sub').textContent=d.battery.charging?'charging':'discharging';

  const loadColor=load<30?'#26de81':load<70?'#feca57':'#ff4757';
  ring($('g-load'),load,loadColor,load.toFixed(0)+'%','utilization');

  const chMax=d.native?d.native.channels.find(c=>c.name==='charge_current'):null;
  const chPct=chMax?chMax.pct:0;
  const chMa=d.battery.current_ma;
  const chColor=chPct>80?'#26de81':chPct>40?'#feca57':'#ff9f43';
  ring($('g-charge'),chPct,chColor,(chMa/1000).toFixed(1)+'A',chPct.toFixed(0)+'% of max');
  $('g-charge-sub').textContent=d.battery.charging?`${chMa} mA`:''; 
}

// ---- Sensors ----
function renderSensors(sensors){
  const el=$('sensors');
  if(!sensors.length){el.innerHTML='<div class="empty">No sensors discovered</div>';return}
  const groups={};
  for(const s of sensors){
    const g=s.grp||'Other';
    if(!groups[g])groups[g]=[];
    groups[g].push(s);
  }
  const order=['CPU','GPU','Board','Charger','Battery','Other'];
  let h='';
  for(const g of order){
    const items=groups[g];
    if(!items||!items.length)continue;
    items.sort((a,b)=>b.c-a.c);
    h+=`<div class="s-group"><div class="s-group-title">${esc(g)}</div>`;
    for(const s of items){
      const pct=Math.min(s.c/100*100,100);
      h+=`<div class="s-row"><span class="sn">${esc(s.name)}</span><div class="sb"><div class="sf" style="width:${pct}%;background:${bc(s.c)}"></div></div><span class="sv ${tc(s.c)}">${fmtT(s.c)}</span></div>`;
    }
    h+='</div>';
  }
  el.innerHTML=h;
}

// ---- Freqs ----
function renderFreqs(d){
  const el=$('freqs');
  const freqs=d.cpu_freq_mhz||[];
  const maxes=[2016,2803,3014];
  const names=['Little (0)','Mid (3)','Big (7)'];
  const cols=['var(--cpu0)','var(--cpu3)','var(--cpu7)'];
  let h='';
  for(let i=0;i<3;i++){
    const mhz=freqs[i]||0;
    const pct=Math.min(mhz/maxes[i]*100,100);
    h+=`<div class="fb"><div class="fb-hd"><span>CPU ${names[i]}</span><b>${mhz} MHz</b></div><div class="fb-tr"><div class="fb-fl" style="width:${pct}%;background:${cols[i]}"></div></div></div>`;
  }
  const gpu=d.gpu_freq_mhz||0;
  if(gpu>0){
    const gp=Math.min(gpu/1100*100,100);
    h+=`<div class="fb"><div class="fb-hd"><span>GPU</span><b>${gpu} MHz</b></div><div class="fb-tr"><div class="fb-fl" style="width:${gp}%;background:var(--gpu)"></div></div></div>`;
  }
  if(!h)h='<div class="empty">N/A (traditional mode)</div>';
  el.innerHTML=h;
}

// ---- Battery ----
function renderBattery(d){
  const el=$('battery');
  const b=d.battery;
  const tc2=b.temp_c<35?'var(--green)':b.temp_c<42?'var(--yellow)':'var(--red)';
  const sc=b.soc>50?'var(--green)':b.soc>20?'var(--yellow)':'var(--red)';
  const pct=Math.min(b.soc,100);
  el.innerHTML=`
    <div class="bat-bar"><div class="bat-fill" style="width:${pct}%;background:${sc}">${b.soc}%</div></div>
    <div class="m"><span class="k">Temperature</span><span class="v" style="color:${tc2}">${fmtT(b.temp_c)}</span></div>
    <div class="m"><span class="k">Current</span><span class="v">${(b.current_ma/1000).toFixed(2)} A</span></div>
    <div class="m"><span class="k">Status</span><span class="v" style="color:${b.charging?'var(--green)':'var(--dim)'}">${b.charging?'Charging':'Discharging'}</span></div>
    <div class="m"><span class="k">Health</span><span class="v">${esc(b.health)}</span></div>`;
}

// ---- AI ----
function renderAI(d){
  const el=$('ai');
  const m=d.mode||'?';
  const n=d.native;
  const en=n?n.enabled:(m!=='traditional');
  el.innerHTML=`
    <div class="m"><span class="k">Mode</span><span class="v" style="color:var(--accent)">${esc(m)}</span></div>
    <div class="m"><span class="k">Status</span><span class="v" style="color:${en?'var(--green)':'var(--red)'}">${en?'Active':'Disabled'}</span></div>
    <div class="m"><span class="k">Scenario</span><span class="v">${d.scenario}</span></div>
    ${n&&n.reason?`<div class="m"><span class="k">Disable Reason</span><span class="v" style="color:var(--red);font-size:11px">${esc(n.reason)}</span></div>`:''}`;
}

// ---- Channels ----
function renderChannels(channels){
  const card=$('ch-card');
  if(!channels||!channels.length){card.style.display='none';return}
  card.style.display='';
  const groups={};
  const gOrder=['Compute','Thermal','Charging','Display'];
  const gClass={Compute:'gc',Thermal:'gt',Charging:'gg',Display:'gd'};
  for(const ch of channels){
    const g=ch.group;
    if(!groups[g])groups[g]=[];
    groups[g].push(ch);
  }
  let h='';
  for(const g of gOrder){
    const items=groups[g];
    if(!items||!items.length)continue;
    h+=`<div class="ch-sec"><div class="ch-sec-title ${gClass[g]||''}">${esc(g)}</div>`;
    for(const ch of items){
      h+=`<div class="ch-r"><span class="cn" title="${esc(ch.name)}">${esc(ch.name)}</span><div class="cbg"><div class="cb" style="width:${ch.pct.toFixed(1)}%"></div></div><span class="cv">${fmtChVal(ch)}</span></div>`;
    }
    h+='</div>';
  }
  $('channels').innerHTML=h;
}

function fmtChVal(ch){
  if(ch.name.includes('freq'))return(ch.value/1000).toFixed(0)+' MHz';
  if(ch.name==='gpu')return(ch.value/1000000).toFixed(0)+' MHz';
  if(ch.name==='charge_current')return(ch.value/1000).toFixed(0)+' mA';
  if(ch.name==='backlight')return ch.value+'/'+ch.max;
  return ch.value.toString();
}

// ---- Instances ----
function renderInst(inst,mode){
  const el=$('inst');
  if(!inst||!inst.length){
    el.innerHTML=`<tr><td colspan="3" class="empty">${mode==='native'?'AI-native mode — cooling channels controlled directly (see above)':'No instances loaded'}</td></tr>`;
    return
  }
  let h='';
  for(const i of inst){
    h+=`<tr><td>${esc(i.name)}</td><td style="color:var(--dim)">${esc(i.algo)}</td><td><span class="lvl ${lc(i.level)}">L${i.level}</span></td></tr>`;
  }
  el.innerHTML=h;
}

// ---- Chart ----
function updateChart(sensors){
  const canvas=$('chart');
  const ctx=canvas.getContext('2d');
  const dpr=window.devicePixelRatio||1;
  const rect=canvas.parentElement.getBoundingClientRect();
  canvas.width=rect.width*dpr;
  canvas.height=rect.height*dpr;
  ctx.scale(dpr,dpr);
  const W=rect.width,H=rect.height;

  const chartGroups=new Set(['CPU','GPU','Battery']);
  const candidates=sensors
    .filter(s=>chartGroups.has(s.grp))
    .sort((a,b)=>b.c-a.c)
    .slice(0,8);
  const names=candidates.map(s=>s.name);

  for(const s of candidates){
    if(!hist[s.name])hist[s.name]=[];
    hist[s.name].push(s.c);
    if(hist[s.name].length>HMAX)hist[s.name].shift();
  }

  let all=[];
  for(const n of names)if(hist[n])all.push(...hist[n]);
  if(!all.length)return;
  let mn=Math.min(...all)-2,mx=Math.max(...all)+2;
  if(mx-mn<5){mn-=3;mx+=3}

  ctx.clearRect(0,0,W,H);

  // Y-axis labels
  ctx.fillStyle='#304050';
  ctx.font='10px monospace';
  ctx.textAlign='left';
  for(let i=0;i<=4;i++){
    const y=(i/4)*H;
    const v=mx-(mx-mn)*(i/4);
    ctx.fillText(v.toFixed(0)+'\u00b0',4,y+10);
  }

  // Grid
  ctx.strokeStyle='#151d27';
  ctx.lineWidth=0.5;
  for(let i=0;i<=4;i++){
    const y=(i/4)*H;
    ctx.beginPath();ctx.moveTo(24,y);ctx.lineTo(W,y);ctx.stroke();
  }

  // Lines with area fill
  for(let ni=0;ni<names.length;ni++){
    const data=hist[names[ni]];
    if(!data||data.length<2)continue;
    const color=COLORS[ni%COLORS.length];

    // Area fill
    ctx.fillStyle=color+'15';
    ctx.beginPath();
    ctx.moveTo(24+0/(HMAX-1)*(W-24),H);
    for(let i=0;i<data.length;i++){
      const x=24+(i/(HMAX-1))*(W-24);
      const y=H-((data[i]-mn)/(mx-mn))*H;
      ctx.lineTo(x,y);
    }
    ctx.lineTo(24+((data.length-1)/(HMAX-1))*(W-24),H);
    ctx.closePath();
    ctx.fill();

    // Line
    ctx.strokeStyle=color;
    ctx.lineWidth=1.5;
    ctx.beginPath();
    for(let i=0;i<data.length;i++){
      const x=24+(i/(HMAX-1))*(W-24);
      const y=H-((data[i]-mn)/(mx-mn))*H;
      if(i===0)ctx.moveTo(x,y);else ctx.lineTo(x,y);
    }
    ctx.stroke();

    // Dot at end
    if(data.length>0){
      const lx=24+((data.length-1)/(HMAX-1))*(W-24);
      const ly=H-((data[data.length-1]-mn)/(mx-mn))*H;
      ctx.fillStyle=color;
      ctx.beginPath();ctx.arc(lx,ly,3,0,Math.PI*2);ctx.fill();
    }
  }

  $('legend').innerHTML=names.map((n,i)=>
    `<span><span class="ld" style="background:${COLORS[i%COLORS.length]}"></span>${esc(n)}</span>`
  ).join('');
}

// ---- Update ----
function update(d){
  renderGauges(d);
  renderSensors(d.sensors||[]);
  renderFreqs(d);
  renderBattery(d);
  renderAI(d);
  renderChannels(d.native?d.native.channels:[]);
  renderInst(d.instances||[],d.mode||'');
  updateChart(d.sensors||[]);
  $('sub').textContent=`AI: ${d.mode} \u00b7 Scenario ${d.scenario} \u00b7 ${(d.sensors||[]).length} sensors`;
  $('status').textContent='live';
  $('dot').className='dot';
  fc=0;
  pollDelay=1000;
}

async function poll(){
  if(abortCtrl)abortCtrl.abort();
  abortCtrl=new AbortController();
  const t0=performance.now();
  try{
    const r=await fetch('/api/snapshot',{signal:AbortSignal.timeout(5000),headers:{'Cache-Control':'no-cache'}});
    if(!r.ok)throw new Error(r.status);
    const d=await r.json();
    const ms=(performance.now()-t0).toFixed(0);
    update(d);
    $('latency').textContent=ms+'ms';
  }catch(e){
    if(e.name==='AbortError')return;
    fc++;
    if(fc<=2)pollDelay=1000;
    else if(fc<=5)pollDelay=2000;
    else if(fc<=10)pollDelay=5000;
    else pollDelay=10000;
    if(fc>3){$('status').textContent='disconnected';$('dot').className='dot off';$('latency').textContent='--'}
  }
  pollTimer=setTimeout(poll,pollDelay);
}

pollTimer=setTimeout(poll,1000);

window.addEventListener('resize',()=>{
  const sensors=Object.keys(hist).map(n=>({name:n,c:hist[n].length?hist[n][hist[n].length-1]:0,grp:null}));
  if(sensors.length)updateChart(sensors);
});
</script>
</body>
</html>"##;
