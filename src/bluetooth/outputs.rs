//! Which ALSA devices the sink should try, in order: Bluetooth speaker, then wired.

use crate::sink::Target;
use crate::state::App;

pub fn bluealsa_available() -> bool {
    ["/usr/bin/bluealsad", "/usr/bin/bluealsa"].iter().any(|p| std::path::Path::new(p).exists())
}

/// Wired ALSA cards from /proc/asound/cards as (id, description); USB before HDMI.
pub fn wired_cards() -> Vec<(String, String)> {
    let s = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let mut cards: Vec<(String, String)> = s
        .lines()
        .filter_map(|l| {
            let id = l.get(l.find('[')? + 1..l.find(']')?)?.trim().to_string();
            let rest = &l[l.find(']')? + 1..];
            let desc = rest.trim_start_matches([' ', ':']).split(" - ").nth(1).unwrap_or(&id).trim().to_string();
            (!id.is_empty()).then_some((id, desc))
        })
        .collect();
    let is_hdmi = |id: &str| id.to_ascii_lowercase().contains("hdmi");
    // HDMI audio only works with a display attached (otherwise ALSA open fails, error 524).
    if !hdmi_connected() {
        cards.retain(|(id, _)| !is_hdmi(id));
    }
    cards.sort_by_key(|(id, _)| is_hdmi(id));
    cards
}

fn hdmi_connected() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return true };
    let mut seen = false;
    for e in dir.flatten() {
        if e.file_name().to_string_lossy().contains("HDMI") {
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
    let bt = app.status.borrow().bt.clone();
    let mut targets = Vec::new();
    if let (Some(mac), true, true) = (&s.bt_device, bt.connected, bt.bluealsa) {
        targets.push(Target {
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
                targets.push(Target { kind: "wired", device: format!("default:CARD={id}"), label });
            }
        }
        dev => targets.push(Target { kind: "wired", device: dev.to_string(), label: dev.to_string() }),
    }
    targets
}
