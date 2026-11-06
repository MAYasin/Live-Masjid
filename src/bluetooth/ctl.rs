//! Thin async wrapper around `bluetoothctl` plus parsers for its output.

use std::time::Duration;
use tokio::process::Command;

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

/// Same as [`btctl`] with owned arguments, for spawning as a background task.
pub async fn btctl_owned(args: Vec<String>, timeout: Duration) -> anyhow::Result<String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    btctl(&a, timeout).await
}

const SHORT: Duration = Duration::from_secs(5);

pub async fn quick(args: &[&str]) -> anyhow::Result<String> {
    btctl(args, SHORT).await
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

pub async fn devices(filter: Option<&str>) -> Vec<(String, String)> {
    let args: Vec<&str> = std::iter::once("devices").chain(filter).collect();
    parse_devices(&quick(&args).await.unwrap_or_default())
}

pub struct DeviceInfo {
    pub connected: bool,
    pub name: Option<String>,
    pub battery: Option<u8>,
}

pub async fn device_info(mac: &str) -> anyhow::Result<DeviceInfo> {
    let s = quick(&["info", mac]).await?;
    Ok(DeviceInfo {
        connected: field(&s, "Connected:") == Some("yes"),
        name: field(&s, "Name:").map(str::to_owned),
        battery: field(&s, "Battery Percentage:")
            .and_then(|v| v.split('(').nth(1))
            .and_then(|v| v.trim_end_matches(')').parse().ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
