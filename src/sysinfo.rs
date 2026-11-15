//! Live device health for the System page: network, Bluetooth, hardware resources and
//! the app's own usage. Everything is read from /proc, /sys and a few cheap commands, on
//! demand (only while someone has the page open).

use crate::sync::LockExt;
use crate::state::App;
use serde::Serialize;
use std::path::Path;
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
    /// Quran recitations stored on the Pi (all reciters), in MB.
    pub quran_mb: f32,
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
    let throttled = read_trim("/sys/devices/platform/soc/soc:firmware/get_throttled").as_deref().and_then(parse_throttled);
    // Newer Pi kernels expose under-voltage through the `rpi_volt` hwmon instead.
    let volt_alarm = hwmon_file("rpi_volt", "in0_lcrit_alarm").map(|v| v == "1");

    let device = Device {
        model: read_trim("/proc/device-tree/model").map_or_else(|| "Unknown board".into(), |s| s.trim_end_matches('\0').to_string()),
        os: os_name(),
        kernel: read_trim("/proc/sys/kernel/osrelease").unwrap_or_default(),
        hostname: read_trim("/proc/sys/kernel/hostname").unwrap_or_default(),
        uptime_s: read_trim("/proc/uptime").map_or(0, |s| parse_uptime(&s)),
        cpu_cores: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
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
        quran_mb: app.quran.stored_bytes() as f32 / 1_048_576.0,
    };

    SystemInfo { device, wifi: wifi().await, bluetooth, app: app_usage }
}

/// "0x50005" → the throttle flags.
fn parse_throttled(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// Read `<file>` from the hwmon device whose `name` is `name`.
fn hwmon_file(name: &str, file: &str) -> Option<String> {
    hwmon_file_in(Path::new("/sys/class/hwmon"), name, file)
}

fn hwmon_file_in(root: &Path, name: &str, file: &str) -> Option<String> {
    std::fs::read_dir(root).ok()?.flatten().find_map(|e| {
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
    std::fs::read_to_string("/etc/os-release").map(|s| parse_os_release(&s)).unwrap_or_default()
}

fn parse_os_release(s: &str) -> String {
    s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())).unwrap_or_default()
}

fn loadavg() -> [f32; 3] {
    parse_loadavg(&read_trim("/proc/loadavg").unwrap_or_default())
}

fn parse_loadavg(s: &str) -> [f32; 3] {
    let mut it = s.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
}

/// (total kB, available kB)
fn meminfo() -> (u64, u64) {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

fn parse_meminfo(s: &str) -> (u64, u64) {
    let field = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn parse_uptime(s: &str) -> u64 {
    s.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// (total bytes, available bytes) of the root filesystem via `df`.
async fn disk(paths: &[&str]) -> (u64, u64) {
    let mut args = vec!["df", "-B1", "--output=size,avail"];
    args.extend_from_slice(paths);
    parse_df(&run(&args).await.unwrap_or_default())
}

fn parse_df(out: &str) -> (u64, u64) {
    let mut nums = out.lines().nth(1).unwrap_or("").split_whitespace().map(|v| v.parse().unwrap_or(0));
    (nums.next().unwrap_or(0), nums.next().unwrap_or(0))
}

/// (total, idle) jiffies from the first line of /proc/stat.
fn parse_cpu_stat(s: &str) -> (u64, u64) {
    let nums: Vec<u64> = s.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let total: u64 = nums.iter().sum();
    let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
    (total, idle)
}

/// utime + stime from /proc/self/stat (fields 14 and 15).
fn parse_proc_ticks(s: &str) -> u64 {
    let after = s.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<u64> = after.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    f.get(10).copied().unwrap_or(0) + f.get(11).copied().unwrap_or(0)
}

/// System CPU % and this process's CPU % since the previous call.
fn cpu_usage() -> (Option<f32>, Option<f32>) {
    let (total, idle) = parse_cpu_stat(&std::fs::read_to_string("/proc/stat").unwrap_or_default());
    let proc_ticks = parse_proc_ticks(&std::fs::read_to_string("/proc/self/stat").unwrap_or_default());

    let mut last = LAST.locked();
    let now = CpuSample { at: Instant::now(), total, idle, proc_ticks };
    let result = last.as_ref().map_or((None, None), |prev| cpu_since(prev, &now));
    *last = Some(now);
    result
}

/// System and app CPU % between two samples; None when they are too close together to tell.
fn cpu_since(prev: &CpuSample, now: &CpuSample) -> (Option<f32>, Option<f32>) {
    if now.total <= prev.total || now.at.saturating_duration_since(prev.at) <= Duration::from_millis(500) {
        return (None, None);
    }
    let dt = (now.total - prev.total) as f32;
    let sys = 100.0 * (1.0 - (now.idle.saturating_sub(prev.idle)) as f32 / dt);
    // /proc/stat totals are summed over all cores; express the app relative to one core.
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f32;
    let me = 100.0 * now.proc_ticks.saturating_sub(prev.proc_ticks) as f32 / dt * cores;
    (Some(sys.clamp(0.0, 100.0)), Some(me.max(0.0)))
}

/// (VmRSS kB, threads) of this process.
fn self_status() -> (u64, u32) {
    parse_self_status(&std::fs::read_to_string("/proc/self/status").unwrap_or_default())
}

fn parse_self_status(s: &str) -> (u64, u32) {
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)?.split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0);
    (get("VmRSS:"), get("Threads:") as u32)
}

#[derive(Default)]
struct Link {
    ssid: Option<String>,
    signal_dbm: Option<i32>,
    frequency_mhz: Option<u32>,
    tx_bitrate: Option<String>,
}

fn parse_iw_link(link: &str) -> Link {
    let value = |k: &str| link.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().to_string()));
    let signal_dbm = value("signal:").and_then(|v| v.split_whitespace().next()?.parse::<i32>().ok());
    Link {
        ssid: value("SSID:"),
        signal_dbm,
        frequency_mhz: value("freq:").and_then(|v| v.split('.').next()?.parse().ok()),
        tx_bitrate: value("tx bitrate:").map(|v| v.split_whitespace().take(2).collect::<Vec<_>>().join(" ")),
    }
}

/// 0–100 from dBm (−50 dBm or better = 100, −100 = 0).
fn quality_from_dbm(dbm: i32) -> u8 {
    ((dbm + 100) * 2).clamp(0, 100) as u8
}

fn parse_ip_addr(out: &str) -> Option<String> {
    out.split_whitespace().skip_while(|w| *w != "inet").nth(1).map(|a| a.split('/').next().unwrap_or(a).to_string())
}

async fn wifi() -> Option<Wifi> {
    wifi_in(Path::new("/sys/class/net")).await
}

/// The first wireless interface listed in `net` (normally /sys/class/net).
async fn wifi_in(net: &Path) -> Option<Wifi> {
    let iface = std::fs::read_dir(net)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| net.join(n).join("wireless").exists())?;
    let link = parse_iw_link(&run(&["iw", "dev", &iface, "link"]).await.unwrap_or_default());
    let ip = run(&["ip", "-4", "-o", "addr", "show", &iface]).await.and_then(|s| parse_ip_addr(&s));
    let ps = run(&["iw", "dev", &iface, "get", "power_save"]).await;
    Some(Wifi {
        ssid: link.ssid,
        signal_dbm: link.signal_dbm,
        quality: link.signal_dbm.map(quality_from_dbm),
        frequency_mhz: link.frequency_mhz,
        tx_bitrate: link.tx_bitrate,
        ip,
        power_save: ps.map(|s| s.contains(": on")),
        interface: iface,
    })
}

fn bt_controller() -> Option<String> {
    bt_controller_in(Path::new("/sys/class/bluetooth"))
}

fn bt_controller_in(root: &Path) -> Option<String> {
    let dir = std::fs::read_dir(root).ok()?;
    dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.starts_with("hci") && !n.contains(':'))
}

async fn bt_transport(mac: &str) -> Option<String> {
    parse_bt_transport(&run(&["bluealsa-aplay", "-L"]).await?, mac)
}

fn parse_bt_transport(list: &str, mac: &str) -> Option<String> {
    let mut lines = list.lines();
    while let Some(l) = lines.next() {
        if l.contains(&format!("DEV={mac},PROFILE=a2dp")) {
            return lines.nth(1).map(|t| t.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let s = "NAME=\"Raspberry Pi OS\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_ID=\"13\"\n";
        assert_eq!(parse_os_release(s), "Debian GNU/Linux 13 (trixie)");
        assert_eq!(parse_os_release("NAME=x\n"), "");
    }

    #[test]
    fn parses_loadavg() {
        assert_eq!(parse_loadavg("0.52 0.31 0.28 1/212 3456\n"), [0.52, 0.31, 0.28]);
        assert_eq!(parse_loadavg("1.00"), [1.0, 0.0, 0.0]);
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
        assert_eq!(parse_loadavg("bad 2.5 3"), [0.0, 2.5, 3.0]);
    }

    #[test]
    fn parses_meminfo() {
        let s = "MemTotal:         415392 kB\nMemFree:           12345 kB\nMemAvailable:     256789 kB\n";
        assert_eq!(parse_meminfo(s), (415_392, 256_789));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), (1, 0));
    }

    #[test]
    fn parses_uptime() {
        assert_eq!(parse_uptime("12345.67 98765.43\n"), 12_345);
        assert_eq!(parse_uptime("0.00 0.00"), 0);
        assert_eq!(parse_uptime("nonsense"), 0);
    }

    #[test]
    fn parses_df() {
        let s = "       Size       Avail\n 31034548224 12345678901\n";
        assert_eq!(parse_df(s), (31_034_548_224, 12_345_678_901));
        assert_eq!(parse_df("only a header\n"), (0, 0));
    }

    #[test]
    fn parses_cpu_stat() {
        // user nice system idle iowait irq softirq steal ...
        let (total, idle) = parse_cpu_stat("cpu  100 20 30 500 50 10 5 0 0 0\ncpu0 1 2 3\n");
        assert_eq!(total, 715);
        assert_eq!(idle, 550);
        assert_eq!(parse_cpu_stat(""), (0, 0));
    }

    #[test]
    fn parses_proc_ticks() {
        // Fields after the name: state ppid pgrp session tty tpgid flags minflt cminflt
        // majflt cmajflt utime stime.
        let s = "1234 (my app) R 1 1234 1234 0 0 4194304 100 0 0 0 7 8 0 0 20 0 1 0 999 0 0\n";
        assert_eq!(parse_proc_ticks(s), 15);
        assert_eq!(parse_proc_ticks("garbage"), 0);
    }

    #[test]
    fn parses_self_status() {
        let s = "Name:\tlivemasjid\nVmRSS:\t   4096 kB\nThreads:\t9\n";
        assert_eq!(parse_self_status(s), (4096, 9));
        assert_eq!(parse_self_status(""), (0, 0));
    }

    #[test]
    fn parses_iw_link() {
        let s = "Connected to aa:bb:cc:dd:ee:ff (on wlan0)\n\tSSID: HomeNet\n\tfreq: 2437.0\n\tsignal: -64 dBm\n\ttx bitrate: 72.2 MBit/s\n\trx bitrate: 65.0 MBit/s\n";
        let l = parse_iw_link(s);
        assert_eq!(l.ssid.as_deref(), Some("HomeNet"));
        assert_eq!(l.signal_dbm, Some(-64));
        assert_eq!(l.frequency_mhz, Some(2437));
        assert_eq!(l.tx_bitrate.as_deref(), Some("72.2 MBit/s"));
        let empty = parse_iw_link("Not connected.\n");
        assert_eq!(empty.ssid, None);
        assert_eq!(empty.signal_dbm, None);
    }

    #[test]
    fn quality_curve() {
        assert_eq!(quality_from_dbm(-50), 100);
        assert_eq!(quality_from_dbm(-64), 72);
        assert_eq!(quality_from_dbm(-100), 0);
        assert_eq!(quality_from_dbm(-120), 0);
        assert_eq!(quality_from_dbm(0), 100);
    }

    #[test]
    fn parses_ip_addr() {
        let s = "2: wlan0    inet 192.168.1.101/24 brd 192.168.1.255 scope global wlan0\n";
        assert_eq!(parse_ip_addr(s).as_deref(), Some("192.168.1.101"));
        assert_eq!(parse_ip_addr("2: eth0    inet6 fe80::1/64 scope link\n"), None);
    }

    #[test]
    fn parses_bt_transport() {
        let s = "Device: bluealsa\n  bluealsa:DEV=AA:BB:CC:DD:EE:FF,PROFILE=a2dp\n\n    A2DP (SBC): S16_LE 2 channels 48000 Hz\n";
        assert_eq!(parse_bt_transport(s, "AA:BB:CC:DD:EE:FF").as_deref(), Some("A2DP (SBC): S16_LE 2 channels 48000 Hz"));
        assert_eq!(parse_bt_transport(s, "11:22:33:44:55:66"), None);
        assert_eq!(parse_bt_transport("bluealsa:DEV=AA:BB:CC:DD:EE:FF,PROFILE=a2dp\n", "AA:BB:CC:DD:EE:FF"), None);
    }

    #[tokio::test]
    async fn collects_from_proc_and_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("lm-sysinfo-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        app.update_settings(|s| s.bt_quality = "medium".into()).unwrap();
        app.status.send_modify(|s| {
            s.bt.device = Some("AA:BB:CC:DD:EE:FF".into());
            s.bt.name = Some("Speaker".into());
            s.bt.connected = true;
            s.bt.battery = Some(80);
            s.buffer_ms = 3_000;
            s.phase = crate::state::Phase::Playing;
        });

        let first = collect(&app).await;
        assert!(!first.device.kernel.is_empty());
        assert!(!first.device.hostname.is_empty());
        assert!(first.device.uptime_s > 0);
        assert!(first.device.cpu_cores >= 1);
        assert!(first.device.mem_total_mb > 0);
        assert!(first.device.disk_total_gb > 0.0);
        assert!(first.device.load[0] >= 0.0);
        assert_eq!(first.bluetooth.speaker.as_deref(), Some("Speaker"));
        assert!(first.bluetooth.connected);
        assert_eq!(first.bluetooth.quality, "medium");
        assert_eq!(first.app.phase, "playing");
        assert!((first.app.buffer_s - 3.0).abs() < 0.01);
        assert!(first.app.memory_mb > 0.0);
        assert!(first.app.threads > 0);

        // A second sample a moment later can turn the counters into percentages (unless another
        // test collected in between; the arithmetic is tested in `cpu_between_two_samples`).
        std::thread::sleep(Duration::from_millis(600));
        let second = collect(&app).await;
        assert!(second.device.cpu_percent.is_none_or(|p| (0.0..=100.0).contains(&p)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cpu_between_two_samples() {
        let t0 = Instant::now();
        let prev = CpuSample { at: t0, total: 1000, idle: 800, proc_ticks: 10 };
        let later = |total, idle, proc_ticks| CpuSample { at: t0 + Duration::from_secs(1), total, idle, proc_ticks };
        let (sys, me) = cpu_since(&prev, &later(1100, 850, 15));
        assert!((sys.unwrap() - 50.0).abs() < 0.01);
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f32;
        assert!((me.unwrap() - 5.0 * cores).abs() < 0.01);
        assert_eq!(cpu_since(&prev, &later(1000, 800, 10)), (None, None), "no ticks yet");
        let soon = CpuSample { at: t0 + Duration::from_millis(100), total: 2000, idle: 900, proc_ticks: 20 };
        assert_eq!(cpu_since(&prev, &soon), (None, None), "too close together");
        let (sys, me) = cpu_since(&prev, &later(1100, 2000, 0));
        assert_eq!((sys, me), (Some(0.0), Some(0.0)), "clamped");
    }

    #[test]
    fn throttle_flags() {
        assert_eq!(parse_throttled("0x50005"), Some(0x50005));
        assert_eq!(parse_throttled("0"), Some(0));
        assert_eq!(parse_throttled("nope"), None);
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lm-sys-{name}-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_hwmon_by_name() {
        let root = temp("hwmon");
        for (dir, name) in [("hwmon0", "cpu_thermal"), ("hwmon1", "rpi_volt")] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("name"), format!("{name}\n")).unwrap();
        }
        std::fs::write(root.join("hwmon1").join("in0_lcrit_alarm"), "1\n").unwrap();
        assert_eq!(hwmon_file_in(&root, "rpi_volt", "in0_lcrit_alarm").as_deref(), Some("1"));
        assert_eq!(hwmon_file_in(&root, "rpi_volt", "missing"), None);
        assert_eq!(hwmon_file_in(&root, "other", "in0_lcrit_alarm"), None);
        assert_eq!(hwmon_file_in(&root.join("nope"), "rpi_volt", "x"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn finds_the_bluetooth_controller() {
        let root = temp("bt");
        assert_eq!(bt_controller_in(&root), None);
        std::fs::create_dir_all(root.join("hci0:12")).unwrap();
        assert_eq!(bt_controller_in(&root), None, "a connection, not a controller");
        std::fs::create_dir_all(root.join("hci0")).unwrap();
        assert_eq!(bt_controller_in(&root).as_deref(), Some("hci0"));
        assert_eq!(bt_controller_in(&root.join("nope")), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finds_the_wireless_interface() {
        let root = temp("net");
        std::fs::create_dir_all(root.join("eth0")).unwrap();
        assert!(wifi_in(&root).await.is_none(), "no wireless interface");
        std::fs::create_dir_all(root.join("wlan-test").join("wireless")).unwrap();
        let w = wifi_in(&root).await.unwrap();
        assert_eq!(w.interface, "wlan-test");
        assert!(w.ssid.is_none() && w.quality.is_none(), "not associated");
        assert!(wifi_in(&root.join("nope")).await.is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
