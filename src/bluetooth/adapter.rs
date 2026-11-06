//! Adapter health: keep the controller powered and recover it when it wedges.
//!
//! Two different failure modes look alike from `bluetoothctl` ("Powered: no"):
//! * **rfkill soft-block** (Pi OS default after boot) — fixed by `rfkill unblock`.
//! * **Hung controller** — the UART-attached BCM43430 stops answering HCI commands
//!   (kernel: `hci0: Opcode 0x0c03 failed: -110`). Only re-binding the `hci_uart_bcm`
//!   driver (which re-uploads the firmware) brings it back; `bt-recover.sh` does that.

use super::ctl::{field, quick};
use std::time::{Duration, Instant};
use tokio::process::Command;

const RECOVER_SCRIPT: &str = "/opt/livemasjid/bt-recover.sh";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterState {
    Powered,
    /// rfkill block we could not lift.
    Blocked,
    /// Controller present but not responding to power-on.
    Unresponsive,
    /// No controller visible to BlueZ at all.
    Missing,
}

impl AdapterState {
    pub fn message(self) -> Option<&'static str> {
        match self {
            AdapterState::Powered => None,
            AdapterState::Blocked => Some("Bluetooth radio is switched off (rfkill) and could not be switched on"),
            AdapterState::Unresponsive => Some("Bluetooth chip is not responding; resetting it automatically"),
            AdapterState::Missing => Some("No Bluetooth controller found; resetting it automatically"),
        }
    }
}

/// True if any Bluetooth rfkill switch is soft- or hard-blocked.
pub fn rfkill_blocked() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/rfkill") else { return false };
    dir.flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("type").trim() == "bluetooth" && (read("soft").trim() == "1" || read("hard").trim() == "1")
    })
}

async fn sudo(args: &[&str], timeout: Duration) -> bool {
    let fut = Command::new("sudo").arg("-n").args(args).kill_on_drop(true).status();
    matches!(tokio::time::timeout(timeout, fut).await, Ok(Ok(s)) if s.success())
}

async fn is_powered() -> Option<bool> {
    let show = quick(&["show"]).await.ok()?;
    if show.trim().is_empty() || show.contains("No default controller") {
        return None;
    }
    Some(field(&show, "Powered:") == Some("yes"))
}

/// Make sure the adapter is powered, lifting rfkill blocks on the way.
pub async fn ensure_powered() -> AdapterState {
    if rfkill_blocked() {
        sudo(&["/usr/sbin/rfkill", "unblock", "bluetooth"], Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    match is_powered().await {
        Some(true) => return AdapterState::Powered,
        None => return AdapterState::Missing,
        Some(false) => {}
    }
    let _ = quick(&["power", "on"]).await;
    match is_powered().await {
        Some(true) => AdapterState::Powered,
        None => AdapterState::Missing,
        Some(false) if rfkill_blocked() => AdapterState::Blocked,
        Some(false) => AdapterState::Unresponsive,
    }
}

/// Decides when to run the (disruptive) controller reset, with back-off.
pub struct Recovery {
    unhealthy_since: Option<Instant>,
    next_attempt: Instant,
    attempts: u32,
}

impl Recovery {
    pub fn new() -> Self {
        Self { unhealthy_since: None, next_attempt: Instant::now(), attempts: 0 }
    }

    pub fn healthy(&mut self) {
        self.unhealthy_since = None;
        self.attempts = 0;
    }

    /// Returns true when a reset should be attempted now.
    pub fn should_reset(&mut self, state: AdapterState) -> bool {
        if !matches!(state, AdapterState::Unresponsive | AdapterState::Missing) {
            return false;
        }
        let since = *self.unhealthy_since.get_or_insert_with(Instant::now);
        // Give BlueZ a moment first (it may just be restarting).
        if since.elapsed() < Duration::from_secs(20) || Instant::now() < self.next_attempt {
            return false;
        }
        self.attempts += 1;
        let wait_min = [1u64, 5, 15, 30][(self.attempts as usize - 1).min(3)];
        self.next_attempt = Instant::now() + Duration::from_secs(wait_min * 60);
        true
    }

    pub async fn reset(&self) -> bool {
        sudo(&[RECOVER_SCRIPT], Duration::from_secs(60)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_before_resetting_and_backs_off() {
        let mut r = Recovery::new();
        assert!(!r.should_reset(AdapterState::Unresponsive), "grace period first");
        r.unhealthy_since = Some(Instant::now() - Duration::from_secs(30));
        assert!(r.should_reset(AdapterState::Unresponsive));
        assert!(!r.should_reset(AdapterState::Unresponsive), "backed off");
        assert!(!r.should_reset(AdapterState::Blocked));
        r.healthy();
        assert_eq!(r.attempts, 0);
    }
}
