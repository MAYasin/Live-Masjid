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

/// Icecast status JSON sources, tried in order (each only when the previous one fails).
/// Only the Icecast servers: the website (`www`) is never polled for status.
pub fn status_urls() -> Vec<String> {
    STREAM_HOSTS.iter().map(|(host, port)| format!("http://{host}:{port}/status-json.xsl")).collect()
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
        // Pages arrive about once a second; 6 s of silence means the connection stalled.
        sock.set_read_timeout(Some(Duration::from_secs(6))).map_err(io)?;
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

pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(50))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
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

    #[test]
    fn parses_more_urls() {
        assert_eq!(super::parse_http_url("http://a.b:80"), Some(("a.b".into(), 80, "/".into())));
        assert_eq!(super::parse_http_url("http://a.b/x/y?z=1"), Some(("a.b".into(), 80, "/x/y?z=1".into())));
        assert_eq!(super::parse_http_url("http://[::1]:8000/x"), Some(("[::1]".into(), 8000, "/x".into())));
        assert_eq!(super::parse_http_url("http://a.b:notaport/x"), None);
        assert_eq!(super::parse_http_url("a.b/x"), None);
    }

    #[test]
    fn endpoint_lists() {
        assert_eq!(
            super::status_urls(),
            ["http://livemasjid.com:8000/status-json.xsl", "http://relay2.livemasjid.com:8000/status-json.xsl"]
        );
        assert!(super::status_urls().iter().all(|u| !u.contains("www.")), "never the website");
        assert_eq!(super::www_bases().len(), if cfg!(feature = "tls") { 2 } else { 1 });
        assert!(super::www_bases().iter().all(|b| b.starts_with("http")));
        assert!(super::USER_AGENT.starts_with("livemasjid-pi/"));
        assert_eq!(super::STREAM_HOSTS.len(), 2);
    }

    #[test]
    fn open_errors_read_well() {
        assert_eq!(super::OpenError::NotFound.to_string(), "not live (404)");
        assert_eq!(super::OpenError::Other("boom".into()).to_string(), "boom");
    }

    use std::io::Read;
    use std::net::TcpListener;

    /// One-shot HTTP server on localhost: answers with `response`, then closes.
    fn serve(response: &[u8]) -> u16 {
        let response = response.to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req);
            use std::io::Write;
            sock.write_all(&response).unwrap();
        });
        port
    }

    #[test]
    fn opens_a_stream_and_reads_headers() {
        let port = serve(b"HTTP/1.0 200 OK\r\nContent-Type: audio/OGG\r\nX-Other: 1\r\n\r\nbody");
        let c = super::open_stream("127.0.0.1", port, "/makkah").unwrap();
        assert_eq!(c.content_type, "audio/ogg");
        assert_eq!(c.url, format!("http://127.0.0.1:{port}/makkah"));
    }

    #[test]
    fn maps_status_codes_to_open_errors() {
        let port = serve(b"HTTP/1.0 404 Not Found\r\n\r\n");
        assert!(matches!(super::open_stream("127.0.0.1", port, "/x"), Err(super::OpenError::NotFound)));

        let port = serve(b"HTTP/1.0 503 Busy\r\n\r\n");
        let err = super::open_stream("127.0.0.1", port, "/x").err().expect("an error").to_string();
        assert!(err.contains("HTTP 503"), "{err}");

        let port = serve(b"not http at all\r\n\r\n");
        let err = super::open_stream("127.0.0.1", port, "/x").err().expect("an error").to_string();
        assert!(err.contains("bad response"), "{err}");
    }

    #[test]
    fn follows_a_redirect() {
        let target = serve(b"HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\n\r\n");
        let location = format!("http://127.0.0.1:{target}/relay");
        let first = serve(format!("HTTP/1.0 302 Found\r\nLocation: {location}\r\n\r\n").as_bytes());
        let c = super::open_stream("127.0.0.1", first, "/start").unwrap();
        assert_eq!(c.content_type, "audio/mpeg");
        assert_eq!(c.url, location);
    }

    #[test]
    fn rejects_a_redirect_without_a_usable_location() {
        let port = serve(b"HTTP/1.0 301 Moved\r\n\r\n");
        let err = super::open_stream("127.0.0.1", port, "/x").err().expect("an error").to_string();
        assert!(err.contains("redirect without location"), "{err}");

        let port = serve(b"HTTP/1.0 302 Found\r\nLocation: https://elsewhere/x\r\n\r\n");
        let err = super::open_stream("127.0.0.1", port, "/x").err().expect("an error").to_string();
        assert!(err.contains("unsupported redirect"), "{err}");
    }

    #[test]
    fn a_refused_connection_is_reported() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = super::open_stream("127.0.0.1", port, "/x").err().expect("an error").to_string();
        assert!(err.contains("connect"), "{err}");
    }
}
