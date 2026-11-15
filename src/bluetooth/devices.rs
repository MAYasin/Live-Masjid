//! User-facing device operations: list, scan, pair/use, release, forget.

use super::ctl::{Bt, System, devices_with, device_info_with, parse_devices, quick_with, valid_mac};
use crate::state::App;
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub preferred: bool,
}

pub async fn list(app: &App) -> anyhow::Result<Vec<Device>> {
    list_with(&System, app).await
}

async fn list_with<B: Bt>(bt: &B, app: &App) -> anyhow::Result<Vec<Device>> {
    let all = parse_devices(&quick_with(bt, &["devices"]).await?);
    let paired = devices_with(bt, Some("Paired")).await;
    let connected = devices_with(bt, Some("Connected")).await;
    let preferred = app.settings().bt_device;
    let has = |v: &[(String, String)], m: &str| v.iter().any(|(x, _)| x == m);
    let mut out: Vec<Device> = all
        .into_iter()
        .map(|(mac, name)| Device {
            name: display_name(name, &mac),
            paired: has(&paired, &mac),
            connected: has(&connected, &mac),
            preferred: preferred.as_deref() == Some(mac.as_str()),
            mac,
        })
        .collect();
    sort_devices(&mut out);
    Ok(out)
}

/// Unnamed devices report their MAC (with dashes) as the name; show nothing instead.
fn display_name(name: String, mac: &str) -> String {
    if name.is_empty() || name.replace('-', ":") == mac { String::new() } else { name }
}

/// Preferred first, then connected, then paired, then named, then alphabetical.
fn sort_devices(out: &mut [Device]) {
    out.sort_by_key(|d| (!d.preferred, !d.connected, !d.paired, d.name.is_empty(), d.name.to_lowercase()));
}

pub async fn scan(app: App, secs: u64) {
    scan_with(&System, &app, secs).await;
}

async fn scan_with<B: Bt>(bt: &B, app: &App, secs: u64) {
    if app.status.borrow().bt.scanning {
        return;
    }
    app.status.send_modify(|s| s.bt.scanning = true);
    let _ = quick_with(bt, &["power", "on"]).await;
    let t = secs.to_string();
    let _ = bt.ctl(&["--timeout", &t, "scan", "on"], Duration::from_secs(secs + 5)).await;
    app.status.send_modify(|s| s.bt.scanning = false);
}

/// Pair (if needed), trust and connect; then make it the preferred speaker.
pub async fn use_device(app: &App, mac: &str) -> anyhow::Result<String> {
    use_device_with(&System, app, mac).await
}

async fn use_device_with<B: Bt>(bt: &B, app: &App, mac: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    let _ = quick_with(bt, &["power", "on"]).await;
    let _ = quick_with(bt, &["pairable", "on"]).await;
    if !devices_with(bt, Some("Paired")).await.iter().any(|(m, _)| m == mac) {
        // Keep discovery running while pairing so BlueZ doesn't expire the device entry.
        let b = bt.clone();
        let scan = tokio::spawn(async move { b.ctl(&["--timeout", "25", "scan", "on"], Duration::from_secs(30)).await });
        bt.sleep(Duration::from_millis(1500)).await;
        let out = bt.ctl(&["--agent", "NoInputNoOutput", "--timeout", "20", "pair", mac], Duration::from_secs(25)).await;
        scan.abort();
        let out = out?;
        pairing_result(&out)?;
    }
    let _ = quick_with(bt, &["pairable", "off"]).await;
    let _ = quick_with(bt, &["trust", mac]).await;
    let _ = bt.ctl(&["connect", mac], Duration::from_secs(20)).await?;
    // Success means the audio channel is up, not merely the radio link.
    let mut ok = false;
    for _ in 0..8 {
        if super::speaker::audio_ready_with(bt, mac).await {
            ok = true;
            break;
        }
        bt.sleep(Duration::from_secs(1)).await;
    }
    let name = device_info_with(bt, mac).await.ok().and_then(|i| i.name).unwrap_or_else(|| mac.to_string());
    app.update_settings(|s| {
        s.bt_device = Some(mac.to_string());
        s.bt_name = Some(name.clone());
    })?;
    app.log("info", format!("Bluetooth speaker set: {name}"));
    anyhow::ensure!(ok, "Paired, but not connected yet. Make sure the speaker is on; it will connect automatically.");
    super::refresh(app).await;
    Ok(format!("Connected to {name}"))
}

/// Did `bluetoothctl pair` succeed? If not, its last failure line is the reason.
fn pairing_result(out: &str) -> anyhow::Result<()> {
    if !["Pairing successful", "AlreadyExists", "Paired: yes"].iter().any(|k| out.contains(k)) {
        let reason = out.lines().rev().find(|l| l.contains("Failed") || l.contains("Error")).unwrap_or("Pairing failed");
        anyhow::bail!("{}", reason.trim());
    }
    Ok(())
}

/// Forget the old (broken) bond and pair again. The speaker must be in pairing mode.
pub async fn repair(app: &App, mac: &str) -> anyhow::Result<String> {
    repair_with(&System, app, mac).await
}

pub(super) async fn repair_with<B: Bt>(bt: &B, app: &App, mac: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    let _ = bt.ctl(&["disconnect", mac], Duration::from_secs(8)).await;
    let _ = bt.ctl(&["remove", mac], Duration::from_secs(8)).await;
    use_device_with(bt, app, mac).await
}

/// Stop using a speaker (disconnect), optionally forgetting the pairing.
pub async fn release(app: &App, mac: &str, forget: bool) -> anyhow::Result<()> {
    release_with(&System, app, mac, forget).await
}

async fn release_with<B: Bt>(bt: &B, app: &App, mac: &str, forget: bool) -> anyhow::Result<()> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    if app.settings().bt_device.as_deref() == Some(mac) {
        app.update_settings(|s| {
            s.bt_device = None;
            s.bt_name = None;
        })?;
    }
    let _ = bt.ctl(&["disconnect", mac], Duration::from_secs(10)).await;
    if forget {
        bt.ctl(&["remove", mac], Duration::from_secs(10)).await?;
    }
    super::refresh(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(mac: &str, name: &str, paired: bool, connected: bool, preferred: bool) -> Device {
        Device { mac: mac.into(), name: name.into(), paired, connected, preferred }
    }

    #[test]
    fn hides_mac_as_name() {
        let mac = "AA:BB:CC:DD:EE:FF";
        assert_eq!(display_name(String::new(), mac), "");
        assert_eq!(display_name("AA-BB-CC-DD-EE-FF".into(), mac), "");
        assert_eq!(display_name("VSW07".into(), mac), "VSW07");
    }

    #[test]
    fn sorts_by_usefulness() {
        let mut out = vec![
            dev("1", "Zeta", false, false, false),
            dev("2", "", false, false, false),
            dev("3", "Beta", true, false, false),
            dev("4", "Alpha", true, true, false),
            dev("5", "Preferred", false, false, true),
        ];
        sort_devices(&mut out);
        let order: Vec<_> = out.iter().map(|d| d.mac.as_str()).collect();
        assert_eq!(order, vec!["5", "4", "3", "1", "2"]);
    }

    #[test]
    fn sorts_names_case_insensitively() {
        let mut out = vec![dev("1", "beta", false, false, false), dev("2", "Alpha", false, false, false)];
        sort_devices(&mut out);
        assert_eq!(out[0].mac, "2");
        assert_eq!(out[1].mac, "1");
    }

    use crate::bluetooth::ctl::fake::FakeBt;

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    fn app(name: &str) -> (std::path::PathBuf, App) {
        let dir = std::env::temp_dir().join(format!("lm-devices-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    fn prefer(app: &App, mac: &str) {
        app.update_settings(|s| {
            s.bt_device = Some(mac.into());
            s.bt_name = Some("VSW07".into());
        })
        .unwrap();
    }

    #[test]
    fn pairing_output() {
        assert!(pairing_result("Attempting to pair\nPairing successful").is_ok());
        assert!(pairing_result("Failed to pair: org.bluez.Error.AlreadyExists").is_ok());
        assert!(pairing_result("\tPaired: yes").is_ok());
        let e = pairing_result("Attempting\n  Failed to pair: org.bluez.Error.AuthenticationFailed  \nbye").unwrap_err();
        assert_eq!(e.to_string(), "Failed to pair: org.bluez.Error.AuthenticationFailed");
        assert_eq!(pairing_result("Device not available").unwrap_err().to_string(), "Pairing failed");
    }

    #[tokio::test]
    async fn lists_devices_with_their_state() {
        let (dir, app) = app("list");
        prefer(&app, "11:22:33:44:55:66");
        let bt = FakeBt::default();
        bt.reply("devices", Ok(&format!("Device {MAC} VSW07\nDevice 11:22:33:44:55:66 11-22-33-44-55-66\nDevice 22:22:22:22:22:22 Phone")))
            .reply("devices Paired", Ok(&format!("Device {MAC} VSW07")))
            .reply("devices Connected", Ok(&format!("Device {MAC} VSW07")));
        let out = list_with(&bt, &app).await.unwrap();
        let macs: Vec<_> = out.iter().map(|d| d.mac.as_str()).collect();
        assert_eq!(macs, ["11:22:33:44:55:66", MAC, "22:22:22:22:22:22"]);
        assert!(out[0].preferred && out[0].name.is_empty());
        assert!(out[1].paired && out[1].connected && !out[1].preferred);
        assert!(!out[2].paired && !out[2].connected);

        bt.reply("devices", Err("bluetoothctl devices timed out"));
        assert!(list_with(&bt, &app).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scans_once_at_a_time() {
        let (dir, app) = app("scan");
        let bt = FakeBt::default();
        scan_with(&bt, &app, 7).await;
        assert_eq!(bt.calls(), ["power on", "--timeout 7 scan on"]);
        assert!(!app.status.borrow().bt.scanning);

        app.status.send_modify(|s| s.bt.scanning = true);
        scan_with(&bt, &app, 7).await;
        assert_eq!(bt.calls().len(), 2, "a scan already running: nothing new");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn uses_a_paired_speaker() {
        let (dir, app) = app("use");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false, true])
            .reply("devices Paired", Ok(&format!("Device {MAC} VSW07")))
            .reply("info", Ok("\tName: VSW07\n"));
        assert_eq!(use_device_with(&bt, &app, MAC).await.unwrap(), "Connected to VSW07");
        assert!(!bt.calls().iter().any(|c| c.contains("pair ")), "already paired: no pairing");
        assert!(bt.called(&format!("trust {MAC}")) && bt.called("pairable off"));
        let s = app.settings();
        assert_eq!((s.bt_device.as_deref(), s.bt_name.as_deref()), (Some(MAC), Some("VSW07")));
        assert!(app.logs.lock().unwrap().iter().any(|l| l.msg == "Bluetooth speaker set: VSW07"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pairs_a_new_speaker() {
        let (dir, app) = app("pair");
        let bt = FakeBt::default();
        bt.ready(MAC, &[true]).reply("--agent", Ok("Attempting to pair\nPairing successful"));
        // No name in the info: the address stands in.
        assert_eq!(use_device_with(&bt, &app, MAC).await.unwrap(), format!("Connected to {MAC}"));
        assert!(bt.called(&format!("--agent NoInputNoOutput --timeout 20 pair {MAC}")));
        assert_eq!(app.settings().bt_name.as_deref(), Some(MAC));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pairing_failures_are_reported() {
        let (dir, app) = app("pairfail");
        let bt = FakeBt::default();
        bt.reply("--agent", Ok("Failed to pair: org.bluez.Error.ConnectionAttemptFailed"));
        let e = use_device_with(&bt, &app, MAC).await.unwrap_err();
        assert_eq!(e.to_string(), "Failed to pair: org.bluez.Error.ConnectionAttemptFailed");
        assert!(!bt.called(&format!("connect {MAC}")));

        bt.reply("--agent", Err("bluetoothctl pair timed out"));
        assert!(use_device_with(&bt, &app, MAC).await.unwrap_err().to_string().contains("timed out"));
        assert!(app.settings().bt_device.is_none(), "nothing saved");

        assert_eq!(use_device_with(&bt, &app, "nope").await.unwrap_err().to_string(), "invalid address");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_speaker_without_audio_is_saved_but_reported() {
        let (dir, app) = app("noaudio");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false]).reply("devices Paired", Ok(&format!("Device {MAC} VSW07")));
        let e = use_device_with(&bt, &app, MAC).await.unwrap_err();
        assert!(e.to_string().starts_with("Paired, but not connected yet"));
        assert_eq!(app.settings().bt_device.as_deref(), Some(MAC), "kept: it connects by itself later");

        bt.reply("connect", Err("bluetoothctl connect timed out"));
        assert!(use_device_with(&bt, &app, MAC).await.unwrap_err().to_string().contains("connect timed out"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn re_pairing_removes_the_old_bond_first() {
        let (dir, app) = app("repairdev");
        let bt = FakeBt::default();
        bt.ready(MAC, &[true]).reply("--agent", Ok("Pairing successful"));
        assert!(repair_with(&bt, &app, MAC).await.is_ok());
        let calls = bt.calls();
        assert_eq!(&calls[..2], [format!("disconnect {MAC}"), format!("remove {MAC}")]);
        assert_eq!(repair_with(&bt, &app, "x").await.unwrap_err().to_string(), "invalid address");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn releasing_and_forgetting() {
        let (dir, app) = app("release");
        prefer(&app, MAC);
        let bt = FakeBt::default();
        release_with(&bt, &app, "11:22:33:44:55:66", false).await.unwrap();
        assert_eq!(app.settings().bt_device.as_deref(), Some(MAC), "another device: the preferred one stays");
        assert!(!bt.called("remove 11:22:33:44:55:66"));

        release_with(&bt, &app, MAC, true).await.unwrap();
        assert!(app.settings().bt_device.is_none() && app.settings().bt_name.is_none());
        assert!(bt.called(&format!("disconnect {MAC}")) && bt.called(&format!("remove {MAC}")));

        bt.reply("remove", Err("not available"));
        assert!(release_with(&bt, &app, MAC, true).await.is_err());
        assert_eq!(release_with(&bt, &app, "bad", false).await.unwrap_err().to_string(), "invalid address");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_public_entry_points_refuse_a_bad_address() {
        // Only inputs refused before any tool runs: on a Pi, bluetoothctl would reach the real adapter.
        let (dir, app) = app("real");
        assert!(use_device(&app, "bad").await.is_err());
        assert!(repair(&app, "bad").await.is_err());
        assert!(release(&app, "bad", false).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
