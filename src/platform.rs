//! Where the app runs, and what that allows.
//!
//! * **Native** (the Pi, installed by `install.sh`): systemd service with a watchdog, narrow sudo
//!   rules for rfkill / Bluetooth recovery / bluez-alsa quality, and `update.sh` for updates.
//! * **Container** (`docker compose`, for a home server or cluster): no systemd or sudo; the image
//!   is updated by pulling a new one; Bluetooth only through the host's BlueZ (D-Bus socket
//!   mounted), otherwise off. Detected from `LIVEMASJID_PLATFORM=container` or `/.dockerenv`.
//!
//! Development: `LIVEMASJID_UI_DIR` serves the UI from disk with live reload (see `web::assets`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Native,
    Container,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Container => "container",
        }
    }
}

/// Decide from the environment (`LIVEMASJID_PLATFORM`) and whether `/.dockerenv` exists.
fn detect(var: Option<&str>, dockerenv: bool) -> Platform {
    match var {
        Some("container" | "docker") => Platform::Container,
        Some("native") => Platform::Native,
        _ if dockerenv => Platform::Container,
        _ => Platform::Native,
    }
}

pub fn get() -> Platform {
    static P: OnceLock<Platform> = OnceLock::new();
    *P.get_or_init(|| detect(std::env::var("LIVEMASJID_PLATFORM").ok().as_deref(), Path::new("/.dockerenv").exists()))
}

pub fn is_native() -> bool {
    get() == Platform::Native
}

/// Bluetooth management runs unless turned off (`LIVEMASJID_BLUETOOTH=off`); in a container it
/// also needs the host's D-Bus system socket.
pub fn bluetooth_enabled() -> bool {
    let off = std::env::var("LIVEMASJID_BLUETOOTH").is_ok_and(|v| matches!(v.as_str(), "off" | "0" | "false"));
    !off && (is_native() || Path::new("/run/dbus/system_bus_socket").exists())
}

/// Development: serve the UI from this folder instead of the copy built into the binary.
pub fn ui_dir() -> Option<&'static Path> {
    static D: OnceLock<Option<PathBuf>> = OnceLock::new();
    D.get_or_init(|| std::env::var_os("LIVEMASJID_UI_DIR").map(PathBuf::from).filter(|p| p.is_dir()))
        .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection() {
        assert_eq!(detect(None, false), Platform::Native);
        assert_eq!(detect(None, true), Platform::Container);
        assert_eq!(detect(Some("container"), false), Platform::Container);
        assert_eq!(detect(Some("native"), true), Platform::Native, "explicit setting wins");
        assert_eq!(detect(Some("other"), false), Platform::Native);
    }
}
