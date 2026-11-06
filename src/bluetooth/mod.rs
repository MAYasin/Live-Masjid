//! Bluetooth speaker management (BlueZ via `bluetoothctl`, audio via bluez-alsa).
//!
//! The supervisor loop keeps the adapter healthy and the preferred speaker connected —
//! reconnecting with back-off after power cuts, speaker restarts or range drops, and
//! resetting the controller if it wedges — and keeps the sink's output list current.

mod adapter;
mod ctl;
mod devices;
mod outputs;

pub use devices::{list, release, scan, use_device};
pub use outputs::{compute_targets, wired_cards};

use crate::state::App;
use adapter::{AdapterState, Recovery};
use std::time::{Duration, Instant};

/// Refresh the preferred device's status and retarget audio.
pub async fn refresh(app: &App) {
    let s = app.settings();
    let info = match &s.bt_device {
        Some(mac) => ctl::device_info(mac).await.ok(),
        None => None,
    };
    app.status.send_modify(|st| {
        st.bt.device = s.bt_device.clone();
        st.bt.name = s.bt_name.clone();
        st.bt.connected = info.as_ref().is_some_and(|i| i.connected);
        st.bt.battery = info.as_ref().and_then(|i| i.battery);
    });
    app.sink.retarget(compute_targets(app));
}

/// Reconnect bookkeeping for the preferred speaker.
struct Reconnect {
    next: Instant,
    backoff: u64,
    /// Consecutive failed connects; after a few we tell the user what to do.
    failures: u32,
    was_connected: bool,
    last_reopen: Instant,
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

    let mut recovery = Recovery::new();
    let mut rc = Reconnect { next: Instant::now(), backoff: 5, failures: 0, was_connected: false, last_reopen: Instant::now() };
    let mut last_state = AdapterState::Powered;
    loop {
        let state = adapter::ensure_powered().await;
        supervise_adapter(&app, state, &mut last_state, &mut recovery).await;
        if state == AdapterState::Powered {
            supervise_speaker(&app, &mut rc, bluealsa).await;
        } else {
            refresh(&app).await;
        }
        let secs = if rc.was_connected { 8 } else { 5 };
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

async fn supervise_adapter(app: &App, state: AdapterState, last: &mut AdapterState, recovery: &mut Recovery) {
    if state != *last {
        match state.message() {
            Some(msg) => app.log("warn", msg),
            None => app.log("info", "Bluetooth adapter is on"),
        }
        *last = state;
    }
    app.status.send_modify(|s| {
        s.bt.powered = state == AdapterState::Powered;
        s.bt.error = state.message().map(str::to_owned).or_else(|| s.bt.error.take().filter(|e| e.contains("bluez-alsa") || e.contains("keeps refusing")));
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

async fn supervise_speaker(app: &App, rc: &mut Reconnect, bluealsa: bool) {
    let s = app.settings();
    let Some(mac) = s.bt_device.clone() else {
        rc.was_connected = false;
        return refresh(app).await;
    };
    let connected = ctl::device_info(&mac).await.is_ok_and(|i| i.connected);
    if !connected && Instant::now() >= rc.next {
        tracing::info!("connecting to Bluetooth speaker {mac}");
        let _ = ctl::quick(&["trust", &mac]).await;
        let ok = ctl::btctl(&["connect", &mac], Duration::from_secs(15))
            .await
            .is_ok_and(|o| o.contains("Connection successful"));
        if ok {
            rc.backoff = 5;
            rc.failures = 0;
        } else {
            rc.next = Instant::now() + Duration::from_secs(rc.backoff);
            rc.backoff = (rc.backoff * 2).min(60);
            rc.failures += 1;
        }
    }
    if connected {
        rc.failures = 0;
    }
    // A speaker that keeps refusing usually lost our pairing (re-paired with a phone) or is
    // busy with another device. Only a person can fix that, so say so.
    const HINT: &str = "The speaker keeps refusing the connection. Switch it off and on and make sure no phone is connected to it. If that doesn't help, Forget it and pair again.";
    if rc.failures >= 5 {
        app.status.send_modify(|st| st.bt.error = Some(HINT.into()));
    } else {
        app.status.send_if_modified(|st| {
            let clear = st.bt.error.as_deref() == Some(HINT);
            if clear {
                st.bt.error = None;
            }
            clear
        });
    }
    refresh(app).await;

    let now_connected = app.status.borrow().bt.connected;
    if now_connected != rc.was_connected {
        let name = s.bt_name.clone().unwrap_or_else(|| mac.clone());
        if now_connected {
            app.log("info", format!("Bluetooth speaker connected: {name}"));
            rc.backoff = 5;
        } else {
            app.log("warn", format!("Bluetooth speaker disconnected: {name}"));
            rc.next = Instant::now();
        }
        rc.was_connected = now_connected;
    }
    // Connected but audio isn't routed to it yet (A2DP transport not ready on first try).
    let on_bt = app.status.borrow().output.kind == "bluetooth";
    if now_connected && bluealsa && !on_bt && rc.last_reopen.elapsed() > Duration::from_secs(12) {
        rc.last_reopen = Instant::now();
        app.sink.reopen();
    }
}
