//! Which ALSA devices the sink should try, in order: Bluetooth speaker, then wired.

use crate::sink::Target;
use crate::state::App;

pub fn bluealsa_available() -> bool {
    ["/usr/bin/bluealsad", "/usr/bin/bluealsa"].iter().any(|p| std::path::Path::new(p).exists())
}

/// Wired ALSA cards from /proc/asound/cards as (id, description); USB before HDMI.
pub fn wired_cards() -> Vec<(String, String)> {
    let s = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    order_cards(parse_cards(&s), hdmi_connected(std::path::Path::new("/sys/class/drm")))
}

/// Parse `/proc/asound/cards` lines such as ` 0 [PCH ]: HDA-Intel - HDA Intel PCH`.
fn parse_cards(s: &str) -> Vec<(String, String)> {
    s.lines()
        .filter_map(|l| {
            let id = l.get(l.find('[')? + 1..l.find(']')?)?.trim().to_string();
            let rest = &l[l.find(']')? + 1..];
            let desc = rest.trim_start_matches([' ', ':']).split(" - ").nth(1).unwrap_or(&id).trim().to_string();
            (!id.is_empty()).then_some((id, desc))
        })
        .collect()
}

/// Drop HDMI cards when no display is attached, then put HDMI last.
fn order_cards(mut cards: Vec<(String, String)>, hdmi_ok: bool) -> Vec<(String, String)> {
    let is_hdmi = |id: &str| id.to_ascii_lowercase().contains("hdmi");
    // HDMI audio only works with a display attached (otherwise ALSA open fails, error 524).
    if !hdmi_ok {
        cards.retain(|(id, _)| !is_hdmi(id));
    }
    cards.sort_by_key(|(id, _)| is_hdmi(id));
    cards
}

/// Is a display on an HDMI connector under `drm` (`/sys/class/drm`)? True when there is no HDMI at all.
fn hdmi_connected(drm: &std::path::Path) -> bool {
    let Ok(dir) = std::fs::read_dir(drm) else { return true };
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

/// Ordered outputs: the preferred Bluetooth speaker (when audio-ready), then the fallback.
fn build_targets(
    bt_device: Option<&str>,
    bt_name: Option<&str>,
    bt_connected: bool,
    bt_bluealsa: bool,
    fallback: &str,
    cards: &[(String, String)],
) -> Vec<Target> {
    let mut targets = Vec::new();
    if let (Some(mac), true, true) = (bt_device, bt_connected, bt_bluealsa) {
        targets.push(Target {
            kind: "bluetooth",
            device: format!("bluealsa:DEV={mac},PROFILE=a2dp"),
            label: bt_name.unwrap_or(mac).to_string(),
        });
    }
    match fallback {
        "none" => {}
        "auto" => {
            for (id, desc) in cards {
                let label = if id.to_ascii_lowercase().contains("hdmi") { "HDMI".to_string() } else { desc.clone() };
                targets.push(Target { kind: "wired", device: format!("default:CARD={id}"), label });
            }
        }
        dev => targets.push(Target { kind: "wired", device: dev.to_string(), label: dev.to_string() }),
    }
    targets
}

pub fn compute_targets(app: &App) -> Vec<Target> {
    let s = app.settings();
    let bt = app.status.borrow().bt.clone();
    build_targets(s.bt_device.as_deref(), s.bt_name.as_deref(), bt.connected, bt.bluealsa, &s.fallback_output, &wired_cards())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, desc: &str) -> (String, String) {
        (id.into(), desc.into())
    }

    #[test]
    fn parses_cards() {
        let s = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0x1234 irq 5\n 1 [HDMI           ]: HDA-Intel - HDA Intel HDMI\n 2 [Device         ]: USB-Audio - USB Speaker\n";
        assert_eq!(
            parse_cards(s),
            vec![card("PCH", "HDA Intel PCH"), card("HDMI", "HDA Intel HDMI"), card("Device", "USB Speaker")]
        );
        assert!(parse_cards("no cards here\n").is_empty());
    }

    #[test]
    fn drops_hdmi_without_a_display() {
        let cards = vec![card("HDMI", "HDMI Audio"), card("Device", "USB Speaker")];
        assert_eq!(order_cards(cards.clone(), false), vec![card("Device", "USB Speaker")]);
        assert_eq!(order_cards(cards, true), vec![card("Device", "USB Speaker"), card("HDMI", "HDMI Audio")]);
    }

    #[test]
    fn bluetooth_first_then_fallback() {
        let cards = vec![card("Device", "USB Speaker")];
        let t = build_targets(Some("AA:BB:CC:DD:EE:FF"), Some("VSW07"), true, true, "auto", &cards);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].kind, "bluetooth");
        assert_eq!(t[0].device, "bluealsa:DEV=AA:BB:CC:DD:EE:FF,PROFILE=a2dp");
        assert_eq!(t[0].label, "VSW07");
        assert_eq!(t[1], Target { kind: "wired", device: "default:CARD=Device".into(), label: "USB Speaker".into() });
    }

    #[test]
    fn bluetooth_needs_audio_ready() {
        for (connected, bluealsa) in [(false, true), (true, false)] {
            let t = build_targets(Some("AA:BB:CC:DD:EE:FF"), None, connected, bluealsa, "none", &[]);
            assert!(t.is_empty(), "connected={connected} bluealsa={bluealsa}");
        }
    }

    #[test]
    fn bluetooth_label_falls_back_to_mac() {
        let t = build_targets(Some("AA:BB:CC:DD:EE:FF"), None, true, true, "none", &[]);
        assert_eq!(t[0].label, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn explicit_fallback_device_and_none() {
        let t = build_targets(None, None, false, false, "hw:2,0", &[]);
        assert_eq!(t, vec![Target { kind: "wired", device: "hw:2,0".into(), label: "hw:2,0".into() }]);
        assert!(build_targets(None, None, false, false, "none", &[card("Device", "USB Speaker")]).is_empty());
    }

    #[test]
    fn auto_labels_hdmi() {
        let cards = vec![card("HDMI", "HDA Intel HDMI")];
        let t = build_targets(None, None, false, false, "auto", &cards);
        assert_eq!(t[0].label, "HDMI");
        assert_eq!(t[0].device, "default:CARD=HDMI");
    }

    #[test]
    fn hdmi_needs_a_connected_display() {
        let dir = std::env::temp_dir().join(format!("lm-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(hdmi_connected(&dir), "no drm folder: assume yes");
        std::fs::create_dir_all(dir.join("card0")).unwrap();
        assert!(hdmi_connected(&dir), "no HDMI connector at all");
        let port = dir.join("card0-HDMI-A-1");
        std::fs::create_dir_all(&port).unwrap();
        assert!(!hdmi_connected(&dir), "HDMI without a status");
        std::fs::write(port.join("status"), "disconnected\n").unwrap();
        assert!(!hdmi_connected(&dir));
        std::fs::write(port.join("status"), "connected\n").unwrap();
        assert!(hdmi_connected(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn targets_from_the_app() {
        let dir = std::env::temp_dir().join(format!("lm-outputs-{}", std::process::id()));
        let app = crate::state::test_app(&dir);
        app.update_settings(|s| {
            s.bt_device = Some("AA:BB:CC:DD:EE:FF".into());
            s.bt_name = Some("VSW07".into());
            s.fallback_output = "hw:1,0".into();
        })
        .unwrap();
        app.status.send_modify(|s| {
            s.bt.connected = true;
            s.bt.bluealsa = true;
        });
        let t = compute_targets(&app);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].label, "VSW07");
        assert_eq!(t[1].device, "hw:1,0");
        // The machine's own cards and bluez-alsa: whatever they are, they don't fail.
        let _ = wired_cards();
        let _ = bluealsa_available();
        let _ = std::fs::remove_dir_all(dir);
    }
}
