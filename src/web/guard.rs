//! Protection for a page that lives on the home network, password or not.
//!
//! * **DNS rebinding:** a website could point its own domain at the Pi's address and then read and
//!   drive the API from the visitor's browser. Every request must therefore name the Pi by an IP
//!   address, `localhost`, a `.local` name, its host name (also on a private home suffix: `pi.lan`,
//!   `pi.home`, `pi.internal`, `pi.localdomain`, `pi.home.arpa`), or a name
//!   listed in `LIVEMASJID_ALLOWED_HOSTS` (comma-separated; e.g. a Tailscale or reverse-proxy name).
//! * **Cross-site requests:** a page on another site could make the browser POST to the Pi (form
//!   or `fetch`). Requests that change something must come from this page itself: a browser always
//!   sends `Origin` on cross-site POSTs, and it must match the `Host` the request was sent to.

use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::IpAddr;
use std::sync::OnceLock;

/// The host part of a `Host` header ("pi.local:8080" → "pi.local", "[::1]:80" → "::1"), lowercase.
fn host_name(host: &str) -> String {
    let host = host.trim();
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(n, _)| n),
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Suffixes home routers give local names. Never a public domain: `pi.example.com` could be an
/// attacker's name pointed at the Pi.
const HOME_SUFFIXES: [&str; 5] = ["lan", "home", "internal", "localdomain", "home.arpa"];

/// Is `host` (a `Host` header) one of the Pi's own names?
pub fn host_allowed(host: &str, hostname: &str, extra: &[String]) -> bool {
    let name = host_name(host);
    let hostname = hostname.to_ascii_lowercase();
    !name.is_empty()
        && (name.parse::<IpAddr>().is_ok()
            || name == "localhost"
            || name.ends_with(".localhost")
            || name.ends_with(".local")
            || (!hostname.is_empty() && (name == hostname || HOME_SUFFIXES.iter().any(|s| name == format!("{hostname}.{s}"))))
            || extra.contains(&name))
}

/// May a request that changes something proceed? Only from this page: no `Origin` (not a
/// cross-site browser request) or an `Origin` naming the same host and port it was sent to.
pub fn origin_allowed(origin: Option<&str>, host: &str) -> bool {
    let Some(origin) = origin else { return true };
    let rest = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://"));
    rest.is_some_and(|r| r.trim_end_matches('/').eq_ignore_ascii_case(host.trim()))
}

fn hostname() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default())
}

fn extra_hosts() -> &'static [String] {
    static E: OnceLock<Vec<String>> = OnceLock::new();
    E.get_or_init(|| {
        std::env::var("LIVEMASJID_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(host_name)
            .filter(|h| !h.is_empty())
            .collect()
    })
}

pub async fn check(req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !host_allowed(host, hostname(), extra_hosts()) {
        tracing::warn!("refused a request for host {host:?} (add it to LIVEMASJID_ALLOWED_HOSTS if it is yours)");
        let msg = format!("Live Masjid doesn't answer to \"{}\". Use the Pi's address, or add this name to LIVEMASJID_ALLOWED_HOSTS.", host_name(host));
        return (StatusCode::FORBIDDEN, msg).into_response();
    }
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if changes && !origin_allowed(origin, host) {
        tracing::warn!("refused a cross-site {} {} from {origin:?}", req.method(), req.uri().path());
        return (StatusCode::FORBIDDEN, "Requests from other websites are not allowed").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        let none: Vec<String> = Vec::new();
        for ok in ["192.168.1.101", "192.168.1.101:80", "[::1]:8080", "localhost:8080", "pi.local", "PI.local.", "pi", "pi.lan", "pi.home:80"] {
            assert!(host_allowed(ok, "pi", &none), "{ok}");
        }
        for bad in ["evil.example", "rebind.attacker.net:80", "pie.lan", "", "192.168.1.101.evil.com"] {
            assert!(!host_allowed(bad, "pi", &none), "{bad}");
        }
        assert!(!host_allowed("pi.attacker.com", "pi", &none), "the host name under a public domain");
        assert!(!host_allowed("pi.tail1234.ts.net", "pi", &none), "Tailscale names go in LIVEMASJID_ALLOWED_HOSTS");
        assert!(host_allowed("pi.home.arpa", "pi", &none));
        let extra = vec!["masjid.example.org".to_string()];
        assert!(host_allowed("masjid.example.org:443", "pi", &extra));
        assert!(!host_allowed("masjid.example.org", "", &none), "no host name known: only IPs and .local");
    }

    #[test]
    fn origins() {
        assert!(origin_allowed(None, "192.168.1.101"), "not a cross-site browser request");
        assert!(origin_allowed(Some("http://192.168.1.101"), "192.168.1.101"));
        assert!(origin_allowed(Some("http://pi.local:8080"), "pi.local:8080"));
        assert!(!origin_allowed(Some("http://evil.example"), "192.168.1.101"));
        assert!(!origin_allowed(Some("http://192.168.1.101:8080"), "192.168.1.101"), "another port is another site");
        assert!(!origin_allowed(Some("null"), "192.168.1.101"), "sandboxed or file pages");
    }
}
