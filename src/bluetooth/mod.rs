//! Bluetooth speaker management (BlueZ via `bluetoothctl`, audio via bluez-alsa).
//!
//! * [`agent`]   — persistent pairing/authorization agent (lets the speaker reconnect itself)
//! * [`adapter`] — keep the controller powered; lift rfkill; reset a hung chip
//! * [`speaker`] — keep the chosen speaker audio-ready without flapping; detect lost pairing
//! * [`devices`] — user actions: scan, pair/use, re-pair, release, forget
//! * [`outputs`] — ordered ALSA outputs for the sink (Bluetooth first, then wired)

mod adapter;
mod agent;
mod ctl;
mod devices;
mod outputs;
mod speaker;

pub use devices::{list, release, repair, scan, use_device};
pub use outputs::{compute_targets, wired_cards};

use crate::state::App;
use adapter::{AdapterState, Recovery};
use std::time::Duration;

/// Refresh the preferred device's display fields and retarget audio.
pub async fn refresh(app: &App) {
    let s = app.settings();
    let info = match &s.bt_device {
        Some(mac) => ctl::device_info(mac).await.ok(),
        None => None,
    };
    let ready = match &s.bt_device {
        Some(mac) => speaker::audio_ready(mac).await,
        None => false,
    };
    app.status.send_modify(|st| {
        st.bt.device = s.bt_device.clone();
        st.bt.name = s.bt_name.clone();
        st.bt.connected = ready;
        st.bt.battery = info.as_ref().and_then(|i| i.battery);
    });
    app.sink.retarget(compute_targets(app));
}

/// Apply a Bluetooth SBC quality ("medium" | "high") by reconfiguring bluez-alsa.
/// Restarting bluez-alsa drops the audio link for a few seconds; the supervisor reconnects.
/// Runs as a systemd one-shot unit: the service itself is sandboxed (`ProtectSystem=full`).
pub async fn set_quality(app: &App, quality: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(quality, "medium" | "high"), "invalid quality");
    let fut = tokio::process::Command::new("sudo")
        .args(["-n", "/usr/bin/systemctl", "start", &format!("livemasjid-bt-audio@{quality}.service")])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .map_err(|_| anyhow::anyhow!("timed out reconfiguring Bluetooth audio"))??;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    app.log("info", format!("Bluetooth audio quality set to {quality}"));
    Ok(())
}

/// Make the system bluez-alsa configuration match the saved setting (e.g. after a restore).
async fn sync_quality(app: &App) {
    let wanted = app.settings().bt_quality;
    let current = std::fs::read_to_string("/etc/livemasjid/bt-quality").unwrap_or_default();
    if !current.trim().is_empty() && current.trim() != wanted
        && let Err(e) = set_quality(app, &wanted).await {
            tracing::warn!("could not apply Bluetooth quality: {e}");
        }
}

pub async fn run(app: App) {
    let bluealsa = outputs::bluealsa_available();
    let has_ctl = ctl::quick(&["--version"]).await.is_ok();
    app.status.send_modify(|s| {
        s.bt.available = has_ctl;
        s.bt.bluealsa = bluealsa;
    });
    if !has_ctl {
        app.log("warn", "bluetoothctl not found; Bluetooth disabled");
        loop {
            app.sink.retarget(compute_targets(&app));
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    if !bluealsa {
        app.status.send_modify(|s| s.bt.error = Some("bluez-alsa is not installed; Bluetooth audio unavailable".into()));
    }
    tokio::spawn(agent::run());
    sync_quality(&app).await;
    let mut battery_warned = false;

    let mut recovery = Recovery::new();
    let mut last_state = AdapterState::Powered;
    let mut sp = speaker::Speaker::new();
    let mut current: Option<String> = None;
    loop {
        let state = adapter::ensure_powered().await;
        supervise_adapter(&app, state, &mut last_state, &mut recovery).await;

        let s = app.settings();
        if s.bt_device != current {
            sp.reset(); // a different (or no) speaker was chosen
            current = s.bt_device.clone();
        }
        match (&s.bt_device, state) {
            (Some(mac), AdapterState::Powered) => {
                let name = s.bt_name.clone().unwrap_or_else(|| mac.clone());
                speaker::supervise(&app, &mut sp, mac, &name).await;
                let battery = ctl::device_info(mac).await.ok().and_then(|i| i.battery);
                match battery {
                    Some(b) if b <= 20 && !battery_warned => {
                        app.log("warn", format!("{name} battery is low ({b}%). Plug it in to keep the radio playing."));
                        battery_warned = true;
                    }
                    Some(b) if b >= 30 => battery_warned = false,
                    _ => {}
                }
                app.status.send_if_modified(|st| std::mem::replace(&mut st.bt.battery, battery) != battery);
            }
            _ => refresh(&app).await,
        }
        let secs = if app.status.borrow().bt.connected { 10 } else { 5 };
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

async fn supervise_adapter(app: &App, state: AdapterState, last: &mut AdapterState, recovery: &mut Recovery) {
    if state != *last {
        match state.message() {
            Some(msg) => app.log("warn", msg),
            None => app.log("info", "Bluetooth adapter is on"),
        }
        if state == AdapterState::Powered {
            // Only the UI pairing flow makes us pairable; nobody else can pair to the Pi.
            let _ = ctl::quick(&["pairable", "off"]).await;
            let _ = ctl::quick(&["discoverable", "off"]).await;
        }
        *last = state;
    }
    app.status.send_modify(|s| {
        s.bt.powered = state == AdapterState::Powered;
        s.bt.error = state.message().map(str::to_owned).or_else(|| s.bt.error.take().filter(|e| e.contains("bluez-alsa")));
    });
    if state == AdapterState::Powered {
        recovery.healthy();
        app.status.send_modify(|s| s.bt.recovering = false);
    } else if recovery.should_reset(state) {
        app.status.send_modify(|s| s.bt.recovering = true);
        app.log("warn", "Resetting the Bluetooth controller");
        let ok = recovery.reset().await;
        app.log(if ok { "info" } else { "error" }, if ok { "Bluetooth controller reset" } else { "Bluetooth controller reset failed" });
        app.status.send_modify(|s| s.bt.recovering = false);
    }
}
