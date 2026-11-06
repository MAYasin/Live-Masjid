//! Bluetooth speaker management via `bluetoothctl` (BlueZ) and audio via bluez-alsa.
//!
//! A supervisor loop keeps the adapter powered and the preferred speaker connected
//! (reconnecting with back-off after power cuts, speaker restarts or range drops), and
//! recomputes the ordered list of audio outputs for the sink:
//! Bluetooth speaker (if connected) -> wired fallback.

use crate::sink::Target;
use crate::state::App;
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub icon: Option<String>,
    pub preferred: bool,
}

pub async fn btctl(args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    let fut = Command::new("bluetoothctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("bluetoothctl {} timed out", args.join(" ")))??;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(strip_ansi(&s))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c != '\r' && c != '\x01' && c != '\x02' {
            out.push(c);
        }
    }
    out
}

pub fn valid_mac(mac: &str) -> bool {
    mac.len() == 17
        && mac.split(':').count() == 6
        && mac.split(':').all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim()))
}

struct Info {
    connected: bool,
    name: Option<String>,
    battery: Option<u8>,
}

async fn device_info(mac: &str) -> anyhow::Result<Info> {
    let s = btctl(&["info", mac], Duration::from_secs(5)).await?;
    Ok(Info {
        connected: field(&s, "Connected:") == Some("yes"),
        name: field(&s, "Name:").map(str::to_owned),
        battery: field(&s, "Battery Percentage:")
            .and_then(|v| v.split('(').nth(1))
            .and_then(|v| v.trim_end_matches(')').parse().ok()),
    })
}

fn parse_devices(s: &str) -> Vec<(String, String)> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            let i = l.find("Device ")?;
            let rest = &l[i + 7..];
            let (mac, name) = rest.split_once(' ').unwrap_or((rest, ""));
            valid_mac(mac).then(|| (mac.to_string(), name.trim().to_string()))
        })
        .collect()
}

pub async fn list(app: &App) -> anyhow::Result<Vec<Device>> {
    let all = parse_devices(&btctl(&["devices"], Duration::from_secs(5)).await?);
    let paired = parse_devices(&btctl(&["devices", "Paired"], Duration::from_secs(5)).await.unwrap_or_default());
    let connected = parse_devices(&btctl(&["devices", "Connected"], Duration::from_secs(5)).await.unwrap_or_default());
    let trusted = parse_devices(&btctl(&["devices", "Trusted"], Duration::from_secs(5)).await.unwrap_or_default());
    let pref = app.settings().bt_device;
    let has = |v: &[(String, String)], m: &str| v.iter().any(|(x, _)| x == m);
    let mut out: Vec<Device> = all
        .iter()
        .map(|(mac, name)| Device {
            mac: mac.clone(),
            // Unnamed devices show their MAC with dashes as name.
            name: if name.is_empty() || name.replace('-', ":") == *mac { String::new() } else { name.clone() },
            paired: has(&paired, mac),
            connected: has(&connected, mac),
            trusted: has(&trusted, mac),
            icon: None,
            preferred: pref.as_deref() == Some(mac),
        })
        .collect();
    out.sort_by_key(|d| (!d.preferred, !d.connected, !d.paired, d.name.is_empty(), d.name.to_lowercase()));
    Ok(out)
}

pub async fn scan(app: App, secs: u64) {
    if app.status.borrow().bt.scanning {
        return;
    }
    app.status.send_modify(|s| s.bt.scanning = true);
    let _ = btctl(&["power", "on"], Duration::from_secs(5)).await;
    let t = secs.to_string();
    let _ = btctl(&["--timeout", &t, "scan", "on"], Duration::from_secs(secs + 5)).await;
    app.status.send_modify(|s| s.bt.scanning = false);
}

/// Pair (if needed), trust and connect; then make it the preferred speaker.
pub async fn use_device(app: &App, mac: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    let _ = btctl(&["power", "on"], Duration::from_secs(5)).await;
    let _ = btctl(&["pairable", "on"], Duration::from_secs(5)).await;
    let paired = parse_devices(&btctl(&["devices", "Paired"], Duration::from_secs(5)).await.unwrap_or_default());
    if !paired.iter().any(|(m, _)| m == mac) {
        // Keep discovery running while pairing so BlueZ doesn't expire the device entry.
        let scan = tokio::spawn(btctl_owned(vec!["--timeout".into(), "25".into(), "scan".into(), "on".into()], 30));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let out = btctl(&["--agent", "NoInputNoOutput", "--timeout", "20", "pair", mac], Duration::from_secs(25)).await;
        scan.abort();
        let out = out?;
        if !(out.contains("Pairing successful") || out.contains("AlreadyExists") || out.contains("Paired: yes")) {
            let reason = out.lines().rev().find(|l| l.contains("Failed") || l.contains("Error")).unwrap_or("pairing failed");
            anyhow::bail!("{}", reason.trim());
        }
    }
    let _ = btctl(&["trust", mac], Duration::from_secs(5)).await;
    let out = btctl(&["connect", mac], Duration::from_secs(20)).await?;
    let ok = out.contains("Connection successful") || device_info(mac).await.map(|i| i.connected).unwrap_or(false);
    let name = device_info(mac).await.ok().and_then(|i| i.name).unwrap_or_else(|| mac.to_string());
    app.update_settings(|s| {
        s.bt_device = Some(mac.to_string());
        s.bt_name = Some(name.clone());
    })?;
    app.log("info", format!("Bluetooth speaker set: {name}"));
    if !ok {
        anyhow::bail!("Paired, but could not connect yet. Make sure the speaker is on; it will auto-connect.");
    }
    refresh(app).await;
    Ok(format!("Connected to {name}"))
}

async fn btctl_owned(args: Vec<String>, timeout: u64) -> anyhow::Result<String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    btctl(&a, Duration::from_secs(timeout)).await
}

pub async fn stop_using(app: &App, mac: &str, forget: bool) -> anyhow::Result<()> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    if app.settings().bt_device.as_deref() == Some(mac) {
        app.update_settings(|s| {
            s.bt_device = None;
            s.bt_name = None;
        })?;
    }
    let _ = btctl(&["disconnect", mac], Duration::from_secs(10)).await;
    if forget {
        btctl(&["remove", mac], Duration::from_secs(10)).await?;
    }
    refresh(app).await;
    Ok(())
}

pub fn bluealsa_available() -> bool {
    ["/usr/bin/bluealsad", "/usr/bin/bluealsa"].iter().any(|p| std::path::Path::new(p).exists())
}

/// Wired ALSA cards from /proc/asound/cards: (id, description). USB first, HDMI last.
pub fn wired_cards() -> Vec<(String, String)> {
    let s = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let mut v: Vec<(String, String)> = s
        .lines()
        .filter_map(|l| {
            let open = l.find('[')?;
            let close = l.find(']')?;
            let id = l[open + 1..close].trim().to_string();
            let desc = l[close + 1..].trim_start_matches([' ', ':']).split(" - ").nth(1).unwrap_or(&id).trim().to_string();
            (!id.is_empty()).then_some((id, desc))
        })
        .collect();
    // HDMI audio only works with a display attached (otherwise ALSA open fails with error 524).
    if !hdmi_connected() {
        v.retain(|(id, _)| !id.to_ascii_lowercase().contains("hdmi"));
    }
    v.sort_by_key(|(id, _)| id.to_ascii_lowercase().contains("hdmi"));
    v
}

fn hdmi_connected() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return true };
    let mut seen = false;
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.contains("HDMI") {
            seen = true;
            if std::fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected") {
                return true;
            }
        }
    }
    !seen
}

pub fn compute_targets(app: &App) -> Vec<Target> {
    let s = app.settings();
    let st = app.status.borrow().bt.clone();
    let mut t = Vec::new();
    if let (Some(mac), true, true) = (&s.bt_device, st.connected, st.bluealsa) {
        t.push(Target {
            kind: "bluetooth",
            device: format!("bluealsa:DEV={mac},PROFILE=a2dp"),
            label: s.bt_name.clone().unwrap_or_else(|| mac.clone()),
        });
    }
    match s.fallback_output.as_str() {
        "none" => {}
        "auto" => {
            for (id, desc) in wired_cards() {
                let label = if id.to_ascii_lowercase().contains("hdmi") { "HDMI".to_string() } else { desc };
                t.push(Target { kind: "wired", device: format!("default:CARD={id}"), label });
            }
        }
        dev => t.push(Target { kind: "wired", device: dev.to_string(), label: dev.to_string() }),
    }
    t
}

/// Update status for the preferred device and retarget audio.
pub async fn refresh(app: &App) {
    let s = app.settings();
    let mut info = None;
    if let Some(mac) = &s.bt_device {
        info = device_info(mac).await.ok();
    }
    app.status.send_modify(|st| {
        st.bt.device = s.bt_device.clone();
        st.bt.name = s.bt_name.clone();
        st.bt.connected = info.as_ref().is_some_and(|i| i.connected);
        st.bt.battery = info.as_ref().and_then(|i| i.battery);
    });
    app.sink.retarget(compute_targets(app));
}

pub async fn run(app: App) {
    let bluealsa = bluealsa_available();
    let has_ctl = btctl(&["--version"], Duration::from_secs(5)).await.is_ok();
    app.status.send_modify(|s| {
        s.bt.available = has_ctl;
        s.bt.bluealsa = bluealsa;
        if has_ctl && !bluealsa {
            s.bt.error = Some("bluez-alsa is not installed; Bluetooth audio unavailable".into());
        }
    });
    if !has_ctl {
        app.log("warn", "bluetoothctl not found; Bluetooth disabled");
    }
    let mut next_connect = Instant::now();
    let mut backoff = 5u64;
    let mut was_connected = false;
    let mut last_reopen = Instant::now();
    loop {
        if has_ctl {
            let show = btctl(&["show"], Duration::from_secs(5)).await.unwrap_or_default();
            let mut powered = field(&show, "Powered:") == Some("yes");
            if !powered && !show.is_empty() {
                if show.contains("blocked") {
                    // Radio soft-blocked by rfkill (Pi OS default after boot): unblock via the
                    // narrow sudo rule installed by install.sh.
                    let ok = tokio::process::Command::new("sudo")
                        .args(["-n", "/usr/sbin/rfkill", "unblock", "bluetooth"])
                        .status()
                        .await
                        .is_ok_and(|s| s.success());
                    app.log(if ok { "info" } else { "warn" }, if ok { "Bluetooth radio was blocked; unblocked it" } else { "Bluetooth radio is blocked (rfkill) and could not be unblocked" });
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                let _ = btctl(&["power", "on"], Duration::from_secs(5)).await;
                powered = btctl(&["show"], Duration::from_secs(5))
                    .await
                    .map(|s| field(&s, "Powered:") == Some("yes"))
                    .unwrap_or(false);
                if !powered {
                    app.status.send_modify(|s| s.bt.error = Some("Bluetooth adapter is off or blocked (rfkill)".into()));
                }
            }
            app.status.send_modify(|s| {
                s.bt.powered = powered;
                if powered && s.bt.error.as_deref().is_some_and(|e| e.contains("adapter")) {
                    s.bt.error = None;
                }
            });

            let s = app.settings();
            if let Some(mac) = s.bt_device.clone() {
                let info = device_info(&mac).await.ok();
                let connected = info.as_ref().is_some_and(|i| i.connected);
                if !connected && powered && Instant::now() >= next_connect {
                    tracing::info!("connecting to Bluetooth speaker {mac}");
                    let _ = btctl(&["trust", &mac], Duration::from_secs(5)).await;
                    let ok = btctl(&["connect", &mac], Duration::from_secs(15))
                        .await
                        .map(|o| o.contains("Connection successful"))
                        .unwrap_or(false);
                    if ok {
                        backoff = 5;
                    } else {
                        next_connect = Instant::now() + Duration::from_secs(backoff);
                        backoff = (backoff * 2).min(60);
                    }
                }
                refresh(&app).await;
                let now_connected = app.status.borrow().bt.connected;
                if now_connected != was_connected {
                    let name = s.bt_name.clone().unwrap_or(mac.clone());
                    if now_connected {
                        app.log("info", format!("Bluetooth speaker connected: {name}"));
                        backoff = 5;
                    } else {
                        app.log("warn", format!("Bluetooth speaker disconnected: {name}"));
                        next_connect = Instant::now();
                    }
                    was_connected = now_connected;
                }
                // Connected but audio isn't on it (e.g. transport wasn't ready on first try): retry.
                let out_kind = app.status.borrow().output.kind.clone();
                if now_connected && bluealsa && out_kind != "bluetooth" && last_reopen.elapsed() > Duration::from_secs(12) {
                    last_reopen = Instant::now();
                    app.sink.reopen();
                }
            } else {
                was_connected = false;
                refresh(&app).await;
            }
        } else {
            app.sink.retarget(compute_targets(&app));
        }
        tokio::time::sleep(Duration::from_secs(if was_connected { 8 } else { 5 })).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let s = "Device AA:BB:CC:DD:EE:FF JBL Flip 5\n\x1b[0;94m[bluetooth]\x1b[0m# Device 11:22:33:44:55:66 11-22-33-44-55-66\nnoise";
        let d = super::parse_devices(&super::strip_ansi(s));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], ("AA:BB:CC:DD:EE:FF".into(), "JBL Flip 5".into()));
        assert!(super::valid_mac("AA:BB:CC:DD:EE:FF"));
        assert!(!super::valid_mac("AA:BB:CC:DD:EE;FF"));
    }
}
