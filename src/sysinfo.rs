//! Live device health for the System page: network, Bluetooth, hardware resources and
//! the app's own usage. Everything is read from /proc, /sys and a few cheap commands, on
//! demand (only while someone has the page open).

use crate::state::App;
use serde::Serialize;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Serialize, Default)]
pub struct SystemInfo {
    pub device: Device,
    pub wifi: Option<Wifi>,
    pub bluetooth: Bluetooth,
    pub app: AppUsage,
}

#[derive(Serialize, Default)]
pub struct Device {
    pub model: String,
    pub os: String,
    pub kernel: String,
    pub hostname: String,
    pub uptime_s: u64,
    pub cpu_cores: usize,
    pub cpu_percent: Option<f32>,
    pub load: [f32; 3],
    pub temp_c: Option<f32>,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub disk_total_gb: f32,
    pub disk_used_gb: f32,
    /// Raspberry Pi power/thermal flags (from the firmware), if available.
    pub undervoltage_now: Option<bool>,
    pub throttled_now: Option<bool>,
    pub undervoltage_since_boot: Option<bool>,
}

#[derive(Serialize, Default)]
pub struct Wifi {
    pub interface: String,
    pub ssid: Option<String>,
    pub signal_dbm: Option<i32>,
    /// 0–100, derived from dBm (−50 dBm or better = 100, −100 = 0).
    pub quality: Option<u8>,
    pub frequency_mhz: Option<u32>,
    pub tx_bitrate: Option<String>,
    pub ip: Option<String>,
    pub power_save: Option<bool>,
}

#[derive(Serialize, Default)]
pub struct Bluetooth {
    pub controller: Option<String>,
    pub powered: bool,
    pub speaker: Option<String>,
    pub connected: bool,
    pub battery: Option<u8>,
    /// e.g. "A2DP (SBC): S16_LE 2 channels 48000 Hz"
    pub transport: Option<String>,
    pub quality: String,
}

#[derive(Serialize, Default)]
pub struct AppUsage {
    pub version: String,
    pub uptime_s: u64,
    pub memory_mb: f32,
    pub cpu_percent: Option<f32>,
    pub threads: u32,
    pub restarts: Option<u32>,
    pub data_received_mb: f32,
    pub buffer_s: f32,
    pub phase: String,
}

/// CPU counters from the previous sample, to turn cumulative ticks into percentages.
struct CpuSample {
    at: Instant,
    total: u64,
    idle: u64,
    proc_ticks: u64,
}
static LAST: Mutex<Option<CpuSample>> = Mutex::new(None);

pub async fn collect(app: &App) -> SystemInfo {
    let st = app.status.borrow().clone();
    let settings = app.settings();
    let (cpu_percent, app_cpu) = cpu_usage();
    let (mem_total, mem_avail) = meminfo();
    let (disk_total, disk_avail) = disk(&["/"]).await;
    let throttled = read_trim("/sys/devices/platform/soc/soc:firmware/get_throttled")
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    // Newer Pi kernels expose under-voltage through the `rpi_volt` hwmon instead.
    let volt_alarm = hwmon_file("rpi_volt", "in0_lcrit_alarm").map(|v| v == "1");

    let device = Device {
        model: read_trim("/proc/device-tree/model").map(|s| s.trim_end_matches('\0').to_string()).unwrap_or_else(|| "Unknown board".into()),
        os: os_name(),
        kernel: read_trim("/proc/sys/kernel/osrelease").unwrap_or_default(),
        hostname: read_trim("/proc/sys/kernel/hostname").unwrap_or_default(),
        uptime_s: read_trim("/proc/uptime").and_then(|s| s.split('.').next()?.parse().ok()).unwrap_or(0),
        cpu_cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        cpu_percent,
        load: loadavg(),
        temp_c: read_trim("/sys/class/thermal/thermal_zone0/temp").and_then(|s| s.parse::<f32>().ok()).map(|m| m / 1000.0),
        mem_total_mb: mem_total / 1024,
        mem_used_mb: mem_total.saturating_sub(mem_avail) / 1024,
        disk_total_gb: disk_total as f32 / 1e9,
        disk_used_gb: disk_total.saturating_sub(disk_avail) as f32 / 1e9,
        undervoltage_now: throttled.map(|t| t & 0x1 != 0).or(volt_alarm),
        throttled_now: throttled.map(|t| t & 0x4 != 0),
        undervoltage_since_boot: throttled.map(|t| t & 0x10000 != 0),
    };

    let bluetooth = Bluetooth {
        controller: bt_controller(),
        powered: st.bt.powered,
        speaker: st.bt.name.clone(),
        connected: st.bt.connected,
        battery: st.bt.battery,
        transport: match &st.bt.device {
            Some(mac) if st.bt.connected => bt_transport(mac).await,
            _ => None,
        },
        quality: settings.bt_quality.clone(),
    };

    let (rss_kb, threads) = self_status();
    let app_usage = AppUsage {
        version: crate::VERSION.to_string(),
        uptime_s: crate::state::now_ms().saturating_sub(st.started_ms) / 1000,
        memory_mb: rss_kb as f32 / 1024.0,
        cpu_percent: app_cpu,
        threads,
        restarts: run(&["systemctl", "show", "-p", "NRestarts", "--value", "livemasjid"]).await.and_then(|s| s.parse().ok()),
        data_received_mb: crate::stream::BYTES_RECEIVED.load(Ordering::Relaxed) as f32 / 1_048_576.0,
        buffer_s: st.buffer_ms as f32 / 1000.0,
        phase: format!("{:?}", st.phase).to_lowercase(),
    };

    SystemInfo { device, wifi: wifi().await, bluetooth, app: app_usage }
}

/// Read `<file>` from the hwmon device whose `name` is `name`.
fn hwmon_file(name: &str, file: &str) -> Option<String> {
    std::fs::read_dir("/sys/class/hwmon").ok()?.flatten().find_map(|e| {
        let dir = e.path();
        (std::fs::read_to_string(dir.join("name")).ok()?.trim() == name)
            .then(|| std::fs::read_to_string(dir.join(file)).ok().map(|s| s.trim().to_string()))
            .flatten()
    })
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

async fn run(args: &[&str]) -> Option<String> {
    let fut = Command::new(args[0]).args(&args[1..]).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(3), fut).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_default()
}

fn loadavg() -> [f32; 3] {
    let s = read_trim("/proc/loadavg").unwrap_or_default();
    let mut it = s.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
}

/// (total kB, available kB)
fn meminfo() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

/// (total bytes, available bytes) of the root filesystem via `df`.
async fn disk(paths: &[&str]) -> (u64, u64) {
    let mut args = vec!["df", "-B1", "--output=size,avail"];
    args.extend_from_slice(paths);
    let out = run(&args).await.unwrap_or_default();
    let mut nums = out.lines().nth(1).unwrap_or("").split_whitespace().map(|v| v.parse().unwrap_or(0));
    (nums.next().unwrap_or(0), nums.next().unwrap_or(0))
}

/// System CPU % and this process's CPU % since the previous call.
fn cpu_usage() -> (Option<f32>, Option<f32>) {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let nums: Vec<u64> = stat.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let total: u64 = nums.iter().sum();
    let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
    // utime + stime are fields 14 and 15 of /proc/self/stat (after the parenthesised name).
    let me = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = me.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<u64> = after.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    let proc_ticks = f.get(10).copied().unwrap_or(0) + f.get(11).copied().unwrap_or(0);

    let mut last = LAST.lock().unwrap();
    let result = match last.as_ref() {
        Some(prev) if total > prev.total && prev.at.elapsed() > Duration::from_millis(500) => {
            let dt = (total - prev.total) as f32;
            let sys = 100.0 * (1.0 - (idle.saturating_sub(prev.idle)) as f32 / dt);
            // /proc/stat totals are summed over all cores; express the app relative to one core.
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
            let me = 100.0 * proc_ticks.saturating_sub(prev.proc_ticks) as f32 / dt * cores;
            (Some(sys.clamp(0.0, 100.0)), Some(me.max(0.0)))
        }
        _ => (None, None),
    };
    *last = Some(CpuSample { at: Instant::now(), total, idle, proc_ticks });
    result
}

/// (VmRSS kB, threads) of this process.
fn self_status() -> (u64, u32) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0);
    (get("VmRSS:"), get("Threads:") as u32)
}

async fn wifi() -> Option<Wifi> {
    let iface = std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| std::path::Path::new(&format!("/sys/class/net/{n}/wireless")).exists())?;
    let link = run(&["iw", "dev", &iface, "link"]).await.unwrap_or_default();
    let value = |k: &str| link.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().to_string()));
    let signal_dbm = value("signal:").and_then(|v| v.split_whitespace().next()?.parse::<i32>().ok());
    let ip = run(&["ip", "-4", "-o", "addr", "show", &iface])
        .await
        .and_then(|s| s.split_whitespace().skip_while(|w| *w != "inet").nth(1).map(|a| a.split('/').next().unwrap_or(a).to_string()));
    let ps = run(&["iw", "dev", &iface, "get", "power_save"]).await;
    Some(Wifi {
        ssid: value("SSID:"),
        signal_dbm,
        quality: signal_dbm.map(|d| ((d + 100) * 2).clamp(0, 100) as u8),
        frequency_mhz: value("freq:").and_then(|v| v.split('.').next()?.parse().ok()),
        tx_bitrate: value("tx bitrate:").map(|v| v.split_whitespace().take(2).collect::<Vec<_>>().join(" ")),
        ip,
        power_save: ps.map(|s| s.contains(": on")),
        interface: iface,
    })
}

fn bt_controller() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/bluetooth").ok()?;
    dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.starts_with("hci") && !n.contains(':'))
}

async fn bt_transport(mac: &str) -> Option<String> {
    let list = run(&["bluealsa-aplay", "-L"]).await?;
    let mut lines = list.lines();
    while let Some(l) = lines.next() {
        if l.contains(&format!("DEV={mac},PROFILE=a2dp")) {
            return lines.nth(1).map(|t| t.trim().to_string());
        }
    }
    None
}
