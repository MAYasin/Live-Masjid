//! User-facing device operations: list, scan, pair/use, release, forget.

use super::ctl::{btctl, btctl_owned, device_info, devices, quick, valid_mac};
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
    let all = super::ctl::parse_devices(&quick(&["devices"]).await?);
    let paired = devices(Some("Paired")).await;
    let connected = devices(Some("Connected")).await;
    let preferred = app.settings().bt_device;
    let has = |v: &[(String, String)], m: &str| v.iter().any(|(x, _)| x == m);
    let mut out: Vec<Device> = all
        .into_iter()
        .map(|(mac, name)| Device {
            // Unnamed devices report their MAC (with dashes) as the name.
            name: if name.is_empty() || name.replace('-', ":") == mac { String::new() } else { name },
            paired: has(&paired, &mac),
            connected: has(&connected, &mac),
            preferred: preferred.as_deref() == Some(mac.as_str()),
            mac,
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
    let _ = quick(&["power", "on"]).await;
    let t = secs.to_string();
    let _ = btctl(&["--timeout", &t, "scan", "on"], Duration::from_secs(secs + 5)).await;
    app.status.send_modify(|s| s.bt.scanning = false);
}

/// Pair (if needed), trust and connect; then make it the preferred speaker.
pub async fn use_device(app: &App, mac: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_mac(mac), "invalid address");
    let _ = quick(&["power", "on"]).await;
    let _ = quick(&["pairable", "on"]).await;
    if !devices(Some("Paired")).await.iter().any(|(m, _)| m == mac) {
        // Keep discovery running while pairing so BlueZ doesn't expire the device entry.
        let args = ["--timeout", "25", "scan", "on"].map(String::from).to_vec();
        let scan = tokio::spawn(btctl_owned(args, Duration::from_secs(30)));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let out = btctl(&["--agent", "NoInputNoOutput", "--timeout", "20", "pair", mac], Duration::from_secs(25)).await;
        scan.abort();
        let out = out?;
        if !["Pairing successful", "AlreadyExists", "Paired: yes"].iter().any(|k| out.contains(k)) {
            let reason = out.lines().rev().find(|l| l.contains("Failed") || l.contains("Error")).unwrap_or("Pairing failed");
            anyhow::bail!("{}", reason.trim());
        }
    }
    let _ = quick(&["trust", mac]).await;
    let out = btctl(&["connect", mac], Duration::from_secs(20)).await?;
    let info = device_info(mac).await.ok();
    let ok = out.contains("Connection successful") || info.as_ref().is_some_and(|i| i.connected);
    let name = info.and_then(|i| i.name).unwrap_or_else(|| mac.to_string());
    app.update_settings(|s| {
        s.bt_device = Some(mac.to_string());
        s.bt_name = Some(name.clone());
    })?;
    app.log("info", format!("Bluetooth speaker set: {name}"));
    anyhow::ensure!(ok, "Paired, but not connected yet. Make sure the speaker is on; it will connect automatically.");
    super::refresh(app).await;
    Ok(format!("Connected to {name}"))
}

/// Stop using a speaker (disconnect), optionally forgetting the pairing.
pub async fn release(app: &App, mac: &str, forget: bool) -> anyhow::Result<()> {
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
    super::refresh(app).await;
    Ok(())
}
