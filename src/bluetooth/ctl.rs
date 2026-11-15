//! Thin async wrapper around `bluetoothctl` plus parsers for its output.

use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::Command;

/// The Bluetooth tools the speaker and device rules use, behind a trait so the rules can be
/// tested with a fake ([`System`] runs the real programs).
pub trait Bt: Clone + Send + Sync + 'static {
    /// Run `bluetoothctl` with `args`: its stdout and stderr, without escape codes.
    fn ctl(&self, args: &[&str], timeout: Duration) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// The `bluealsa-aplay -L` listing (empty when it fails).
    fn pcms(&self) -> impl Future<Output = String> + Send;
    /// Wait (a fake doesn't).
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
}

/// The real tools.
#[derive(Clone, Copy)]
pub struct System;

impl Bt for System {
    async fn ctl(&self, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        btctl(args, timeout).await
    }

    async fn pcms(&self) -> String {
        output("bluealsa-aplay", &["-L"], Duration::from_secs(5))
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Run `program` (stdin closed), killing it after `timeout`.
async fn output(program: &str, args: &[&str], timeout: Duration) -> anyhow::Result<Output> {
    let fut = Command::new(program).args(args).stdin(Stdio::null()).kill_on_drop(true).output();
    Ok(tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("{program} {} timed out", args.join(" ")))??)
}

/// stdout then stderr, without escape codes.
fn text(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    strip_ansi(&s)
}

pub async fn btctl(args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    Ok(text(&output("bluetoothctl", args, timeout).await?))
}

const SHORT: Duration = Duration::from_secs(5);

pub async fn quick(args: &[&str]) -> anyhow::Result<String> {
    btctl(args, SHORT).await
}

/// [`quick`] through `bt`.
pub async fn quick_with<B: Bt>(bt: &B, args: &[&str]) -> anyhow::Result<String> {
    bt.ctl(args, SHORT).await
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if !matches!(c, '\r' | '\x01' | '\x02') {
            out.push(c);
        }
    }
    out
}

pub fn valid_mac(mac: &str) -> bool {
    mac.len() == 17 && mac.split(':').all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Value of a `Key: value` line in `bluetoothctl show/info` output.
pub fn field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.lines().find_map(|l| l.trim().strip_prefix(key).map(str::trim))
}

/// `Device AA:BB:.. Name` lines → (mac, name).
pub fn parse_devices(s: &str) -> Vec<(String, String)> {
    s.lines()
        .filter_map(|l| {
            let rest = &l[l.find("Device ")? + 7..];
            let (mac, name) = rest.split_once(' ').unwrap_or((rest, ""));
            valid_mac(mac).then(|| (mac.to_string(), name.trim().to_string()))
        })
        .collect()
}

/// Known devices, optionally only those with a property (`Paired`, `Connected`).
pub async fn devices_with<B: Bt>(bt: &B, filter: Option<&str>) -> Vec<(String, String)> {
    let args: Vec<&str> = std::iter::once("devices").chain(filter).collect();
    parse_devices(&quick_with(bt, &args).await.unwrap_or_default())
}

pub struct DeviceInfo {
    pub connected: bool,
    pub name: Option<String>,
    pub battery: Option<u8>,
}

pub async fn device_info(mac: &str) -> anyhow::Result<DeviceInfo> {
    device_info_with(&System, mac).await
}

pub async fn device_info_with<B: Bt>(bt: &B, mac: &str) -> anyhow::Result<DeviceInfo> {
    let s = quick_with(bt, &["info", mac]).await?;
    Ok(DeviceInfo {
        connected: field(&s, "Connected:") == Some("yes"),
        name: field(&s, "Name:").map(str::to_owned),
        battery: field(&s, "Battery Percentage:").and_then(battery_percent),
    })
}

/// "0x5a (90)" → 90; None when BlueZ reports no battery.
fn battery_percent(v: &str) -> Option<u8> {
    v.split('(').nth(1)?.trim_end_matches(')').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::fake::FakeBt;

    #[tokio::test]
    async fn runs_a_program_and_keeps_stdout_then_stderr_without_escapes() {
        let out = output("sh", &["-c", "printf 'a\\033[0mb\\r\\n'; printf 'err' >&2"], Duration::from_secs(5)).await.unwrap();
        assert_eq!(text(&out), "ab\nerr");
    }

    #[tokio::test]
    async fn a_slow_program_times_out() {
        let err = output("sleep", &["5"], Duration::from_millis(50)).await.unwrap_err();
        assert_eq!(err.to_string(), "sleep 5 timed out");
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error() {
        assert!(output("no-such-program-livemasjid", &[], Duration::from_secs(5)).await.is_err());
        // The real tools (not installed where the tests run): only that they return.
        let _ = quick(&["--version"]).await;
        let _ = System.pcms().await;
        let _ = System.ctl(&["--version"], SHORT).await;
        let t = std::time::Instant::now();
        System.sleep(Duration::from_millis(5)).await;
        assert!(t.elapsed() >= Duration::from_millis(5));
    }

    #[tokio::test]
    async fn lists_devices_through_bt() {
        let bt = FakeBt::default();
        bt.reply("devices", Ok("Device AA:BB:CC:DD:EE:FF One\nDevice 11:22:33:44:55:66 Two"))
            .reply("devices Paired", Ok("Device 11:22:33:44:55:66 Two"))
            .reply("devices Connected", Err("timed out"));
        assert_eq!(devices_with(&bt, None).await.len(), 2);
        assert_eq!(devices_with(&bt, Some("Paired")).await, [("11:22:33:44:55:66".into(), "Two".into())]);
        assert!(devices_with(&bt, Some("Connected")).await.is_empty(), "a failure lists nothing");
        assert_eq!(bt.calls(), ["devices", "devices Paired", "devices Connected"]);
    }

    #[tokio::test]
    async fn reads_device_info() {
        let bt = FakeBt::default();
        bt.reply("info AA:BB:CC:DD:EE:FF", Ok("Device AA:BB:CC:DD:EE:FF\n\tName: VSW07\n\tConnected: yes\n\tBattery Percentage: 0x5a (90)\n"));
        let i = device_info_with(&bt, "AA:BB:CC:DD:EE:FF").await.unwrap();
        assert!(i.connected);
        assert_eq!(i.name.as_deref(), Some("VSW07"));
        assert_eq!(i.battery, Some(90));

        let i = device_info_with(&bt, "11:22:33:44:55:66").await.unwrap();
        assert!(!i.connected && i.name.is_none() && i.battery.is_none());

        bt.reply("info 00", Err("no device"));
        assert!(device_info_with(&bt, "00:00:00:00:00:00").await.is_err());
        // The real tool isn't installed in tests.
        let _ = device_info("00:00:00:00:00:00").await;
    }

    #[tokio::test]
    async fn the_fake_repeats_its_last_listing() {
        let bt = FakeBt::default();
        assert_eq!(bt.pcms().await, "");
        bt.ready("AA:BB:CC:DD:EE:FF", &[false, true]);
        assert_eq!(bt.pcms().await, "");
        assert!(bt.pcms().await.contains("DEV=AA:BB:CC:DD:EE:FF,PROFILE=a2dp"));
        assert!(bt.pcms().await.contains("PROFILE=a2dp"));
    }

    #[test]
    fn parses_device_lines() {
        let s = "Device AA:BB:CC:DD:EE:FF JBL Flip 5\n\x1b[0;94m[bluetooth]\x1b[0m# Device 11:22:33:44:55:66 11-22-33-44-55-66\nnoise";
        let d = parse_devices(&strip_ansi(s));
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], ("AA:BB:CC:DD:EE:FF".into(), "JBL Flip 5".into()));
        assert!(valid_mac("AA:BB:CC:DD:EE:FF"));
        assert!(!valid_mac("AA:BB:CC:DD:EE;FF"));
    }

    #[test]
    fn reads_fields() {
        let info = "\tName: VSW07\n\tConnected: yes\n\tBattery Percentage: 0x5a (90)";
        assert_eq!(field(info, "Name:"), Some("VSW07"));
        assert_eq!(field(info, "Connected:"), Some("yes"));
    }

    #[test]
    fn strips_escapes_and_control_bytes() {
        assert_eq!(strip_ansi("\x1b[0;94m[bluetooth]\x1b[0m# prompt"), "[bluetooth]# prompt");
        assert_eq!(strip_ansi("a\rb\x01c\x02d"), "abcd");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn mac_shapes() {
        assert!(valid_mac("aa:bb:cc:dd:ee:ff"));
        assert!(!valid_mac("aa:bb:cc:dd:ee"));
        assert!(!valid_mac("aa:bb:cc:dd:ee:ff:00"));
        assert!(!valid_mac("aa-bb-cc-dd-ee-ff"));
        assert!(!valid_mac(""));
    }

    #[test]
    fn device_lines_need_a_valid_mac() {
        let s = "Device AA:BB:CC:DD:EE:FF\nDevice nope Broken\nDevice 11:22:33:44:55:66";
        let d = parse_devices(s);
        assert_eq!(d, [("AA:BB:CC:DD:EE:FF".into(), String::new()), ("11:22:33:44:55:66".into(), String::new())]);
        assert!(parse_devices("").is_empty());
    }

    #[test]
    fn battery_values() {
        assert_eq!(battery_percent("0x5a (90)"), Some(90));
        assert_eq!(battery_percent("0x64 (100)"), Some(100));
        assert_eq!(battery_percent("0x00 (0)"), Some(0));
        assert_eq!(battery_percent("unknown"), None);
        assert_eq!(battery_percent("(abc)"), None);
    }
}

/// A scripted [`Bt`] for tests: replies by command prefix, a queue of `bluealsa-aplay -L`
/// listings (the last one repeats), and a log of every command.
#[cfg(test)]
pub mod fake {
    use crate::sync::LockExt;
    use super::Bt;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct State {
        replies: Vec<(String, Result<String, String>)>,
        pcms: VecDeque<String>,
        calls: Vec<String>,
    }

    #[derive(Clone, Default)]
    pub struct FakeBt(Arc<Mutex<State>>);

    /// A `bluealsa-aplay -L` listing in which `mac` is audio-ready.
    pub fn a2dp(mac: &str) -> String {
        format!("bluealsa:DEV={mac},PROFILE=a2dp,SRV=org.bluealsa\n    Speaker, trusted audio-card, playback\n")
    }

    impl FakeBt {
        /// Answer commands starting with `prefix` (the longest matching prefix wins).
        pub fn reply(&self, prefix: &str, out: Result<&str, &str>) -> &Self {
            let out = out.map(str::to_owned).map_err(str::to_owned);
            self.0.locked().replies.push((prefix.to_owned(), out));
            self
        }

        /// Whether `mac` is audio-ready on each successive check (the last answer repeats).
        pub fn ready(&self, mac: &str, seq: &[bool]) -> &Self {
            let mut s = self.0.locked();
            s.pcms = seq.iter().map(|&r| if r { a2dp(mac) } else { String::new() }).collect();
            self
        }

        pub fn calls(&self) -> Vec<String> {
            self.0.locked().calls.clone()
        }

        pub fn called(&self, cmd: &str) -> bool {
            self.calls().iter().any(|c| c == cmd)
        }
    }

    impl Bt for FakeBt {
        async fn ctl(&self, args: &[&str], _timeout: Duration) -> anyhow::Result<String> {
            let cmd = args.join(" ");
            let mut s = self.0.locked();
            s.calls.push(cmd.clone());
            let best = s.replies.iter().filter(|(p, _)| cmd.starts_with(p.as_str())).max_by_key(|(p, _)| p.len());
            match best {
                Some((_, Ok(out))) => Ok(out.clone()),
                Some((_, Err(e))) => Err(anyhow::anyhow!("{e}")),
                None => Ok(String::new()),
            }
        }

        async fn pcms(&self) -> String {
            let mut s = self.0.locked();
            if s.pcms.len() > 1 { s.pcms.pop_front().unwrap_or_default() } else { s.pcms.front().cloned().unwrap_or_default() }
        }

        async fn sleep(&self, _d: Duration) {
            tokio::task::yield_now().await;
        }
    }
}
