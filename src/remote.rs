//! Tiny built-in HTTP remote control.
//!
//! Serves a single phone-friendly page with Prev / Pause / Next buttons and
//! a JSON status endpoint. Implemented on a raw `TcpListener` — no HTTP
//! framework dependency, near-zero idle cost (one parked accept task).
//!
//! Endpoints:
//!   GET  /             → control page (HTML)
//!   POST /api/next     → advance to next photo
//!   POST /api/prev     → go to previous photo
//!   POST /api/pause    → toggle pause
//!   POST /api/favorite → favourite/un-favourite the current photo
//!   GET  /api/status   → {"paused":…,"index":…,"total":…,"filename":…,"album":…,"favorite":…}
//!   GET  /api/health   → process/readiness status for local supervision
//!
//! Security:
//!   - A bearer token is mandatory whenever the remote is enabled.
//!   - Commands are display-control only; no photo bytes or filesystem paths.
//!   - `/api/status` returns [`Status`] only — no Wi-Fi, PhotoPrism, or other
//!     credentials are ever included in the JSON payload.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{channel, error::TrySendError, Receiver, Sender};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::config::RemoteConfig;
use crate::renderer::SlideshowCmd;

/// Command queue depth shared with the display loop. Deliberately small:
/// remote taps should act "now" — when the loop is busy (mid-transition) a
/// short backlog is fine, but past that we tell the phone to retry (429)
/// rather than queue up a pile of stale button presses.
const CMD_QUEUE_CAP: usize = 16;

/// Cap concurrent remote HTTP handlers. Without this, a LAN flood of TCP
/// connections can spawn unbounded tasks and OOM a Pi Zero.
const MAX_CONCURRENT_CONNS: usize = 32;

/// Snapshot of what the slideshow is currently showing, shared with the
/// HTTP server and serialised by `/api/status`.
///
/// Deliberately excludes credentials, filesystem paths, and photo bytes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub paused: bool,
    pub index: usize,
    pub total: usize,
    pub providers: usize,
    pub filename: String,
    pub album: String,
    pub favorite: bool,
}

pub type SharedStatus = Arc<Mutex<Status>>;

/// Bind the listener and spawn the accept loop. Returns the command channel
/// the display loop drains. Fails fast on bind errors (port in use, bad
/// address) so misconfiguration is visible at startup.
pub async fn start(cfg: &RemoteConfig, status: SharedStatus) -> Result<Receiver<SlideshowCmd>> {
    let token = Arc::new(
        cfg.token
            .clone()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("remote: token is required"))?,
    );
    let bind_ip = cfg
        .bind
        .parse::<IpAddr>()
        .context("remote: bind must be a literal IP address")?;
    let addr = SocketAddr::new(bind_ip, cfg.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("remote: binding {addr}"))?;
    info!("Remote control: http://{addr}/");

    let (tx, rx) = channel(CMD_QUEUE_CAP);
    let conn_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));

    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = tx.closed() => return,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, peer)) => {
                    let Ok(permit) = conn_limit.clone().try_acquire_owned() else {
                        debug!("remote: rejecting {peer} — connection limit reached");
                        // Drop the stream without spawning work.
                        drop(stream);
                        continue;
                    };
                    debug!("remote: connection from {peer}");
                    let tx = tx.clone();
                    let status = status.clone();
                    let token = token.clone();
                    let bind_ip_str = Arc::new(bind_ip.to_string());
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_conn(stream, peer, tx, status, token, bind_ip_str, permit).await
                        {
                            debug!("remote: connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("remote: accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(rx)
}

/// Index just past the first CRLF-CRLF in `buf`, if present.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == *b"\r\n\r\n")
        .map(|i| i + 4)
}

async fn handle_conn(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    tx: Sender<SlideshowCmd>,
    status: SharedStatus,
    token: Arc<String>,
    bind_ip: Arc<String>,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    // Read until end-of-headers or the buffer fills. A single `read` can
    // return a partial request when TCP fragments; looping keeps large
    // browser headers and split packets from becoming spurious 404s.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if buf.len() >= 8192 {
            break;
        }
        let mut chunk = [0u8; 1024];
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!("remote: read timeout"));
        }
        let n = tokio::time::timeout(remaining, stream.read(&mut chunk))
            .await
            .context("remote: read timeout")??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if find_header_end(&buf).is_some() {
            break;
        }
    }

    let req = String::from_utf8_lossy(&buf);
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("");
    // Browsers / proxies may append `?…` — match on the path only.
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");

    // The control shell has no data or side effects. It remains readable so a
    // user can open `http://host:8188/#TOKEN`; the fragment never traverses
    // the network and JavaScript sends it only in API request headers.
    if !validate_host(&req, &bind_ip) {
        let response = http_response("400 Bad Request", "text/plain", "invalid host");
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await.ok();
        return Ok(());
    }

    if !(authorized(&req, &token) || endpoint_is_public(method, path, peer.ip().is_loopback())) {
        drop(permit);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let response = http_response(
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"unauthorized\"}",
        );
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await.ok();
        return Ok(());
    }

    let response = match (method, path) {
        ("GET", "/") => http_response("200 OK", "text/html; charset=utf-8", CONTROL_PAGE),
        ("GET", "/api/status") => {
            let body = {
                let s = status.lock().await.clone();
                serde_json::to_string(&s).unwrap_or_else(|_| "{}".to_string())
            };
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/health") => {
            let snapshot = status.lock().await.clone();
            if snapshot.total > 0 && snapshot.providers > 0 {
                http_response(
                    "200 OK",
                    "application/json",
                    &format!(
                        "{{\"status\":\"ready\",\"photos\":{},\"providers\":{}}}",
                        snapshot.total, snapshot.providers
                    ),
                )
            } else {
                http_response(
                    "503 Service Unavailable",
                    "application/json",
                    "{\"status\":\"starting\"}",
                )
            }
        }
        ("POST", "/api/next") => command(&tx, SlideshowCmd::Next),
        ("POST", "/api/prev") => command(&tx, SlideshowCmd::Prev),
        ("POST", "/api/pause") => command(&tx, SlideshowCmd::TogglePause),
        ("POST", "/api/favorite") => command(&tx, SlideshowCmd::ToggleFavorite),
        _ => http_response("404 Not Found", "text/plain", "not found"),
    };

    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await.ok();
    Ok(())
}

fn authorized(req: &str, token: &str) -> bool {
    let Some(value) = req.lines().skip(1).find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            if !name.eq_ignore_ascii_case("authorization") {
                return None;
            }
            let value = value.trim();
            value
                .split_once(' ')
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, rest)| rest.trim())
        })
    }) else {
        return false;
    };
    constant_time_eq(value.as_bytes(), token.as_bytes())
}

/// Endpoints that may be read without a bearer token.
///
/// `GET /` is the phone shell (token lives in the URL fragment). `GET
/// /api/health` is public only from loopback so a local supervisor can probe
/// readiness without embedding the token in a unit file.
fn endpoint_is_public(method: &str, path: &str, peer_is_loopback: bool) -> bool {
    method == "GET" && (path == "/" || (path == "/api/health" && peer_is_loopback))
}

fn validate_host(req: &str, bind_ip: &str) -> bool {
    let Some(value) = req.lines().skip(1).find_map(|line| {
        line.split_once(':')
            .and_then(|(name, value)| name.eq_ignore_ascii_case("host").then(|| value.trim()))
    }) else {
        return false;
    };
    // The Host header might be `IP:port`, `[IPv6]:port`, or `domain:port`.
    let host_no_port = if value.starts_with('[') {
        value
            .split(']')
            .next()
            .unwrap_or(value)
            .trim_start_matches('[')
    } else {
        value.split(':').next().unwrap_or(value)
    };

    if host_no_port.eq_ignore_ascii_case("localhost")
        || host_no_port == bind_ip
        || host_no_port == "127.0.0.1"
        || host_no_port == "::1"
    {
        return true;
    }

    // Binding 0.0.0.0 / :: means "any local address"; accept a literal IP
    // Host so phones can use the Pi's LAN address without DNS.
    if (bind_ip == "0.0.0.0" || bind_ip == "::") && host_no_port.parse::<IpAddr>().is_ok() {
        return true;
    }

    // Single-label mDNS (e.g. `myframe.local`). Nested suffixes like
    // `attacker.com.local` are rejected so a rebinding hostname cannot
    // hide behind the `.local` allowance.
    host_no_port
        .strip_suffix(".local")
        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for i in 0..left.len().max(right.len()) {
        diff |= usize::from(*left.get(i).unwrap_or(&0) ^ *right.get(i).unwrap_or(&0));
    }
    diff == 0
}

fn command(tx: &Sender<SlideshowCmd>, cmd: SlideshowCmd) -> String {
    match tx.try_send(cmd) {
        Ok(()) => http_response("200 OK", "application/json", "{\"ok\":true}"),
        // Queue full — the display loop is behind. Drop this press instead of
        // letting stale taps pile up; the phone can simply tap again.
        Err(TrySendError::Full(_)) => http_response(
            "429 Too Many Requests",
            "application/json",
            "{\"ok\":false,\"error\":\"busy\"}",
        ),
        // Receiver dropped — the slideshow is shutting down.
        Err(TrySendError::Closed(_)) => http_response(
            "503 Service Unavailable",
            "application/json",
            "{\"ok\":false}",
        ),
    }
}

fn http_response(code: &str, content_type: &str, body: &str) -> String {
    let csp = if content_type.starts_with("text/html") {
        "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'\r\n"
    } else {
        ""
    };
    format!(
        "HTTP/1.1 {code}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Robots-Tag: noindex, nofollow\r\n\
         Referrer-Policy: no-referrer\r\n\
         {csp}\
         \r\n\
         {body}",
        body.len(),
    )
}

/// Single-file control page: three big buttons + a status line that polls
/// every 2 s. Dark theme so a phone in a dark room doesn't blind anyone.
const CONTROL_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>PicoGallery Remote</title>
<style>
  :root { color-scheme: dark; }
  body  { margin:0; min-height:100vh; display:flex; flex-direction:column;
          align-items:center; justify-content:center; gap:1.5rem;
          background:#101014; color:#e8e8ea;
          font-family:system-ui, -apple-system, sans-serif; }
  h1    { font-size:1rem; font-weight:500; letter-spacing:.2em;
          text-transform:uppercase; color:#9a9aa4; margin:0; }
  .row  { display:flex; gap:1rem; flex-wrap:wrap; justify-content:center; }
  button{ border:1px solid #33333d; border-radius:14px; background:#1b1b22;
          color:#e8e8ea; font-size:1.6rem; width:5.5rem; height:5.5rem;
          cursor:pointer; transition:background .15s, transform .05s; }
  button:hover  { background:#26262f; }
  button:active { transform:scale(.95); background:#30303b; }
  #status { font-size:.85rem; color:#9a9aa4; text-align:center;
            min-height:2.4em; max-width:80vw; overflow-wrap:anywhere; }
</style>
</head>
<body>
<h1>PicoGallery</h1>
<div class="row">
  <button onclick="cmd('prev')"     aria-label="Previous">&#9664;</button>
  <button onclick="cmd('pause')"    aria-label="Pause" id="pp">&#10073;&#10073;</button>
  <button onclick="cmd('next')"     aria-label="Next">&#9654;</button>
  <button onclick="cmd('favorite')" aria-label="Favourite" id="fav">&#9825;</button>
</div>
<div id="status">…</div>
<script>
const token = decodeURIComponent(location.hash.slice(1));
const auth = token ? {'Authorization':'Bearer '+token} : {};
async function cmd(c){ try{ await fetch('/api/'+c,{method:'POST',headers:auth}); }catch(e){}
                       setTimeout(poll, 300); }
async function poll(){
  try{
    const s = await (await fetch('/api/status',{headers:auth})).json();
    document.getElementById('pp').innerHTML = s.paused ? '&#9654;' : '&#10073;&#10073;';
    const fav = document.getElementById('fav');
    fav.innerHTML = s.favorite ? '&#9829;' : '&#9825;';   // filled vs outline heart
    fav.style.color = s.favorite ? '#ff5a6e' : '';
    const album = s.album ? s.album + ' — ' : '';
    document.getElementById('status').textContent =
      (s.paused ? '⏸ paused · ' : '') + album + s.filename +
      ' (' + (s.index + 1) + '/' + s.total + ')';
  }catch(e){
    document.getElementById('status').textContent = 'disconnected';
  }
}
poll(); setInterval(poll, 2000);
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_header_end_detects_crlf_crlf() {
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(find_header_end(buf), Some(buf.len()));
    }

    #[test]
    fn find_header_end_none_when_incomplete() {
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(find_header_end(buf), None);
    }

    #[test]
    fn request_path_strips_query_string() {
        let raw = "GET /api/status?x=1 HTTP/1.1";
        let mut parts = raw.split_whitespace();
        let _method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        assert_eq!(path, "/api/status");
    }

    #[test]
    fn authorization_requires_exact_bearer_token() {
        assert!(authorized(
            "GET /api/status HTTP/1.1\r\nAuthorization: Bearer abc\r\n",
            "abc"
        ));
        assert!(authorized(
            "GET /api/status HTTP/1.1\r\nAuthorization: bearer abc\r\n",
            "abc"
        ));
        assert!(!authorized(
            "GET /api/status HTTP/1.1\r\nAuthorization: Bearer ab\r\n",
            "abc"
        ));
        assert!(!authorized("GET /api/status HTTP/1.1\r\n", "abc"));
    }

    #[test]
    fn validate_host_allows_bind_ip() {
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n",
            "127.0.0.1"
        ));
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: 192.168.1.100:8080\r\n",
            "192.168.1.100"
        ));
        assert!(!validate_host(
            "GET / HTTP/1.1\r\nHost: 10.0.0.1\r\n",
            "192.168.1.100"
        ));
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: 192.168.1.50:8188\r\n",
            "0.0.0.0"
        ));
        assert!(!validate_host(
            "GET / HTTP/1.1\r\nHost: evil.example\r\n",
            "0.0.0.0"
        ));
    }

    #[test]
    fn validate_host_allows_localhost_and_mdns() {
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: localhost\r\n",
            "192.168.1.100"
        ));
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: myframe.local\r\n",
            "192.168.1.100"
        ));
        assert!(validate_host(
            "GET / HTTP/1.1\r\nHost: myframe.local:8188\r\n",
            "192.168.1.100"
        ));
        assert!(!validate_host(
            "GET / HTTP/1.1\r\nHost: attacker.com.local\r\n",
            "192.168.1.100"
        ));
    }

    #[test]
    fn validate_host_blocks_dns_rebinding() {
        assert!(!validate_host(
            "GET / HTTP/1.1\r\nHost: attacker.com\r\n",
            "192.168.1.100"
        ));
        assert!(!validate_host(
            "GET / HTTP/1.1\r\nHost: attacker.com:8188\r\n",
            "192.168.1.100"
        ));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"password", b"password"));
        assert!(!constant_time_eq(b"password", b"passwOrd"));
        assert!(!constant_time_eq(b"password", b"pass"));
        assert!(!constant_time_eq(b"pass", b"password"));
    }

    #[test]
    fn test_http_response_format() {
        let resp = http_response("200 OK", "text/plain", "hello");
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.contains("Content-Type: text/plain\r\n"));
        assert!(resp.contains("Content-Length: 5\r\n"));
        assert!(resp.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(resp.contains("X-Robots-Tag: noindex, nofollow\r\n"));
        assert!(resp.ends_with("\r\n\r\nhello"));
    }

    #[test]
    fn html_response_carries_csp_and_robots_headers() {
        let resp = http_response("200 OK", "text/html; charset=utf-8", "<p>x</p>");
        assert!(resp.contains("Content-Security-Policy:"));
        assert!(resp.contains("X-Robots-Tag: noindex, nofollow"));
        assert!(resp.contains("Referrer-Policy: no-referrer"));
    }

    #[test]
    fn health_is_public_from_loopback_only() {
        assert!(endpoint_is_public("GET", "/", false));
        assert!(endpoint_is_public("GET", "/api/health", true));
        assert!(!endpoint_is_public("GET", "/api/health", false));
        assert!(!endpoint_is_public("POST", "/api/next", true));
        assert!(!endpoint_is_public("GET", "/api/status", true));
    }

    #[tokio::test]
    async fn test_remote_integration() {
        let status = Arc::new(Mutex::new(Status {
            paused: false,
            index: 1,
            total: 10,
            providers: 1,
            filename: "test.jpg".to_string(),
            album: "test_album".to_string(),
            favorite: false,
        }));

        let mut config = RemoteConfig {
            enabled: true,
            port: 0,
            bind: "127.0.0.1".to_string(),
            token_file: None,
            token: Some("1234567890123456".to_string()),
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut rx = start(&config, status.clone()).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", config.port))
            .await
            .unwrap();
        let req = "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer 1234567890123456\r\n\r\n";
        stream.write_all(req.as_bytes()).await.unwrap();

        let mut buf = vec![0; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("test.jpg"));

        let mut stream2 = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", config.port))
            .await
            .unwrap();
        let req2 = "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream2.write_all(req2.as_bytes()).await.unwrap();
        let n2 = stream2.read(&mut buf).await.unwrap();
        let resp2 = String::from_utf8_lossy(&buf[..n2]);
        assert!(resp2.contains("401 Unauthorized"));

        let mut stream3 = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", config.port))
            .await
            .unwrap();
        let req3 = "POST /api/next HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer 1234567890123456\r\n\r\n";
        stream3.write_all(req3.as_bytes()).await.unwrap();
        let n3 = stream3.read(&mut buf).await.unwrap();
        let resp3 = String::from_utf8_lossy(&buf[..n3]);
        assert!(resp3.contains("200 OK"));

        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, SlideshowCmd::Next));

        // Test root page with no auth token
        let mut stream4 = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", config.port))
            .await
            .unwrap();
        let req4 = "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream4.write_all(req4.as_bytes()).await.unwrap();
        let n4 = stream4.read(&mut buf).await.unwrap();
        let resp4 = String::from_utf8_lossy(&buf[..n4]);
        assert!(resp4.contains("200 OK"));
        assert!(resp4.contains("PicoGallery Remote"));

        // Test health endpoint from loopback without auth
        let mut stream5 = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", config.port))
            .await
            .unwrap();
        let req5 = "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        stream5.write_all(req5.as_bytes()).await.unwrap();
        let n5 = stream5.read(&mut buf).await.unwrap();
        let resp5 = String::from_utf8_lossy(&buf[..n5]);
        assert!(resp5.contains("200 OK"));
        assert!(resp5.contains("ready"));
    }
}
