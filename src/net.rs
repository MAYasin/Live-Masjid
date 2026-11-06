//! Network endpoints and a minimal blocking HTTP/1.0 client for Icecast audio streams.
//!
//! Audio uses raw `TcpStream`s on a dedicated thread: Icecast answers HTTP/1.0 with a
//! plain (non-chunked) body, so the socket *is* the audio byte stream – zero copies,
//! no async bridging, and the decoder reads straight from the kernel buffer.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Origin Icecast first (a new stream appears there immediately; the relay lags), then relay.
pub const STREAM_HOSTS: &[(&str, u16)] = &[("livemasjid.com", 8000), ("relay2.livemasjid.com", 8000)];

pub fn www_bases() -> &'static [&'static str] {
    if cfg!(feature = "tls") {
        &["https://www.livemasjid.com", "http://www.livemasjid.com"]
    } else {
        &["http://www.livemasjid.com"]
    }
}

/// Icecast status JSON sources, tried in order.
pub fn status_urls() -> Vec<String> {
    let mut v = vec![
        "http://livemasjid.com:8000/status-json.xsl".to_string(),
        "http://relay2.livemasjid.com:8000/status-json.xsl".to_string(),
    ];
    // (Each source is only used when the previous one fails.)
    v.extend(www_bases().iter().map(|b| format!("{b}/api/status-json.xsl")));
    v
}

pub const USER_AGENT: &str = concat!("livemasjid-pi/", env!("CARGO_PKG_VERSION"));

pub struct StreamConn {
    pub reader: BufReader<TcpStream>,
    pub content_type: String,
    pub url: String,
}

#[derive(Debug)]
pub enum OpenError {
    /// 404: the mount is not live.
    NotFound,
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotFound => write!(f, "not live (404)"),
            OpenError::Other(s) => f.write_str(s),
        }
    }
}

pub fn open_stream(host: &str, port: u16, path: &str) -> Result<StreamConn, OpenError> {
    let mut target = (host.to_string(), port, path.to_string());
    for _hop in 0..3 {
        let (h, p, path) = &target;
        let addrs: Vec<_> = (h.as_str(), *p)
            .to_socket_addrs()
            .map_err(|e| OpenError::Other(format!("dns {h}: {e}")))?
            .collect();
        let mut last = OpenError::Other(format!("no address for {h}"));
        let mut sock = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, Duration::from_secs(4)) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last = OpenError::Other(format!("connect {a}: {e}")),
            }
        }
        let sock = sock.ok_or(last)?;
        let io = |e: std::io::Error| OpenError::Other(format!("{h}: {e}"));
        sock.set_nodelay(true).map_err(io)?;
        sock.set_read_timeout(Some(Duration::from_secs(8))).map_err(io)?;
        sock.set_write_timeout(Some(Duration::from_secs(4))).map_err(io)?;
        // One write: Icecast resets connections whose request line arrives fragmented.
        let req = format!(
            "GET {path} HTTP/1.0\r\nHost: {h}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        (&sock).write_all(req.as_bytes()).map_err(io)?;
        let mut r = BufReader::with_capacity(32 * 1024, sock);
        let mut line = String::new();
        r.read_line(&mut line).map_err(io)?;
        let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut ctype = String::new();
        let mut location = None;
        let mut total = 0;
        loop {
            let mut hl = String::new();
            let n = r.read_line(&mut hl).map_err(io)?;
            total += n;
            if n == 0 || hl.trim().is_empty() || total > 16 * 1024 {
                break;
            }
            if let Some((k, v)) = hl.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-type" => ctype = v.trim().to_ascii_lowercase(),
                    "location" => location = Some(v.trim().to_string()),
                    _ => {}
                }
            }
        }
        match code {
            200 => {
                return Ok(StreamConn { reader: r, content_type: ctype, url: format!("http://{h}:{p}{path}") });
            }
            301 | 302 | 303 | 307 | 308 => {
                let loc = location.ok_or_else(|| OpenError::Other("redirect without location".into()))?;
                target = parse_http_url(&loc).ok_or_else(|| OpenError::Other(format!("unsupported redirect {loc}")))?;
            }
            404 => return Err(OpenError::NotFound),
            0 => return Err(OpenError::Other(format!("{h}: bad response"))),
            c => return Err(OpenError::Other(format!("{h}: HTTP {c}"))),
        }
    }
    Err(OpenError::Other("too many redirects".into()))
}

fn parse_http_url(u: &str) -> Option<(String, u16, String)> {
    let rest = u.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 80),
    };
    Some((host.to_string(), port, path.to_string()))
}

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(50))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .expect("http client")
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_urls() {
        assert_eq!(
            super::parse_http_url("http://relay2.livemasjid.com:8000/x.mp3"),
            Some(("relay2.livemasjid.com".into(), 8000, "/x.mp3".into()))
        );
        assert_eq!(super::parse_http_url("http://a.b"), Some(("a.b".into(), 80, "/".into())));
        assert_eq!(super::parse_http_url("https://a.b/"), None);
    }
}
