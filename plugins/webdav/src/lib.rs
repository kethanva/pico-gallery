/// WebDAV/Nextcloud plugin for PicoGallery.
///
/// Syncs photos from any WebDAV server (Nextcloud, ownCloud, Synology NAS,
/// Apache mod_dav, nginx-dav-ext) to a local directory and serves them from
/// disk, matching photOS's WebDAV→rsync→framebuffer design — but entirely in
/// Rust, with no shell tools or mounted filesystems.
///
/// Offline operation: once synced, the slideshow works without a network
/// connection. New and changed files are pulled on the next sync cycle.
///
/// Config keys (in `[[plugins]]`):
///
/// ```toml
/// [[plugins]]
/// name     = "webdav"
/// enabled  = true
/// url      = "https://nextcloud.example.com/remote.php/dav/files/USERNAME"
/// username = "alice"
/// password = "secret"
/// remote_path      = "/Photos"              # sub-path on server; default "/"
/// sync_dir         = "/tmp/picogallery-webdav"
/// sync_interval_secs = 3600                 # 0 = startup only
/// skip_tls_verify  = false                  # true for self-signed certs
/// ```
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use reqwest::{Client, ClientBuilder, Method};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncReadExt;

use picogallery_core::{
    exif_thumb_from_head, AuthStatus, FetchIntent, PhotoMeta, PhotoPlugin, PluginConfig,
    EXIF_HEAD_SCAN_BYTES,
};

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
/// Cap on a single PROPFIND response body — a buggy or hostile server must
/// not be able to balloon RAM on a 512 MB Pi.
const MAX_PROPFIND_BYTES: u64 = 4 * 1024 * 1024;
/// Hard limits on tree discovery so a pathological server (or one that keeps
/// listing the same collection) can't keep the BFS running forever.
const MAX_DIRS: usize = 10_000;
const MAX_IMAGES: usize = 100_000;
/// Network timeouts — a dead server must not hang a request indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

async fn read_bounded(mut response: reqwest::Response, max: u64, label: &str) -> Result<Vec<u8>> {
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max).min(1 << 20) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading {label}"))?
    {
        if body.len().saturating_add(chunk.len()) as u64 > max {
            return Err(anyhow::anyhow!("{label}: response exceeds {max} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:">"#,
    r#"<d:prop>"#,
    r#"<d:displayname/>"#,
    r#"<d:getcontenttype/>"#,
    r#"<d:getcontentlength/>"#,
    r#"<d:resourcetype/>"#,
    r#"<d:getlastmodified/>"#,
    r#"</d:prop>"#,
    r#"</d:propfind>"#,
);

// ── PROPFIND result entry ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DavEntry {
    /// Server-relative URL path (e.g. `/remote.php/dav/files/user/Photos/img.jpg`).
    href: String,
    is_collection: bool,
    content_type: Option<String>,
    last_modified: Option<DateTime<Utc>>,
}

// ── XML parser for PROPFIND multistatus responses ─────────────────────────────

fn parse_propfind(xml: &str) -> Vec<DavEntry> {
    use quick_xml::{events::Event, Reader};

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::with_capacity(256);
    let mut entries: Vec<DavEntry> = Vec::new();

    // Mutable builder state for the entry currently being parsed. Text fields
    // accumulate via push_str because quick-xml may split one text node into
    // several `Text` events; the raw strings are post-processed when the
    // enclosing `response` element closes.
    let mut href = String::new();
    let mut is_collection = false;
    let mut content_type_raw = String::new();
    let mut last_modified_raw = String::new();
    let mut in_response = false;
    let mut in_resourcetype = false;
    let mut current_tag = String::new();
    let mut warned_bad_name = false;

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.local_name();
                let local = element_name(name.as_ref(), &mut warned_bad_name);
                match local {
                    "response" => {
                        in_response = true;
                        href.clear();
                        is_collection = false;
                        content_type_raw.clear();
                        last_modified_raw.clear();
                    }
                    "resourcetype" => in_resourcetype = true,
                    _ => {}
                }
                if in_response {
                    current_tag = local.to_string();
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let local = element_name(name.as_ref(), &mut warned_bad_name);
                if local == "collection" && in_resourcetype {
                    is_collection = true;
                }
            }
            Ok(Event::Text(ref e)) => {
                if !in_response {
                    continue;
                }
                let decoded = match e.decode() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let text = match quick_xml::escape::unescape(&decoded) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "href" => href.push_str(text),
                    "getcontenttype" => content_type_raw.push_str(text),
                    "getlastmodified" => last_modified_raw.push_str(text),
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.local_name();
                let local = element_name(name.as_ref(), &mut warned_bad_name);
                match local {
                    "response" if in_response => {
                        if !href.is_empty() {
                            let content_type = if content_type_raw.is_empty() {
                                None
                            } else {
                                Some(
                                    content_type_raw
                                        .split(';')
                                        .next()
                                        .unwrap_or(&content_type_raw)
                                        .trim()
                                        .to_string(),
                                )
                            };
                            let last_modified = DateTime::parse_from_rfc2822(&last_modified_raw)
                                .ok()
                                .map(|d| d.with_timezone(&Utc));
                            entries.push(DavEntry {
                                href: href.clone(),
                                is_collection,
                                content_type,
                                last_modified,
                            });
                        }
                        in_response = false;
                    }
                    "resourcetype" => in_resourcetype = false,
                    _ => {}
                }
                // Clear only when this End matches the tag we're collecting
                // text for — a nested close (e.g. `</d:prop>`) must not drop
                // text accumulated for an outer element.
                if local == current_tag {
                    current_tag.clear();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    entries
}

/// Convert an element's local name (namespace already stripped by quick-xml)
/// to `&str`. Invalid UTF-8 is warned once per document and treated as empty.
fn element_name<'n>(name: &'n [u8], warned: &mut bool) -> &'n str {
    match std::str::from_utf8(name) {
        Ok(s) => s,
        Err(_) => {
            if !*warned {
                warn!("WebDAV: non-UTF-8 element name in PROPFIND response");
                *warned = true;
            }
            ""
        }
    }
}

// ── URL helpers ───────────────────────────────────────────────────────────────

/// Extract `scheme://host[:port]` from a URL string.
fn url_origin(url: &str) -> &str {
    if let Some(after_scheme) = url.find("://").map(|i| i + 3) {
        let rest = &url[after_scheme..];
        let path_start = rest.find('/').unwrap_or(rest.len());
        &url[..after_scheme + path_start]
    } else {
        url
    }
}

/// True when `path` is exactly `base` or a descendant (`base/...`).
/// Plain `starts_with` is wrong: `/dav/photos` would also match `/dav/photosEvil`.
fn path_under_base(path: &str, base: &str) -> bool {
    let path = url_decode(path);
    let base = url_decode(base);
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return true;
    }
    path == base || path.starts_with(&(base.to_owned() + "/"))
}

/// True when `url` is safe to send the configured Basic-Auth credentials to:
/// it must be on the configured `origin` (scheme://host[:port]) AND its path
/// must sit under the configured `base_path`. A malicious or compromised WebDAV
/// server can return absolute hrefs in a PROPFIND response (e.g.
/// `https://evil.example/steal`); without this gate the follow-up PROPFIND or
/// GET would attach the username/password to that arbitrary URL. Both path and
/// base are percent-decoded before comparison so encoding differences don't
/// reject a legitimate in-tree href.
fn href_in_scope(url: &str, origin: &str, base_path: &str) -> bool {
    if url_origin(url) != origin {
        return false;
    }
    let path = url.strip_prefix(origin).unwrap_or("");
    path_under_base(path, base_path)
}

/// Decode `%XX` percent-encoding. Invalid sequences pass through unchanged.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = from_hex_nibble(bytes[i + 1]);
            let l = from_hex_nibble(bytes[i + 2]);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn from_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a URL-encoded path and return a safe `PathBuf` relative to some root.
/// Path traversal components (`..`, `.`) are rejected, as are components that
/// *decode* to contain a path separator (e.g. `a%2F..` → `a/..`) — those would
/// otherwise smuggle traversal past the component check.
fn decode_relative_path(encoded_rel: &str) -> Option<PathBuf> {
    let components: Vec<String> = encoded_rel
        .split('/')
        .filter(|s| !s.is_empty())
        .map(url_decode)
        .collect();

    if components.is_empty() {
        return None;
    }

    // Security: reject traversal and separator-smuggling components.
    if components
        .iter()
        .any(|c| c == ".." || c == "." || c.contains('/') || c.contains('\\'))
    {
        return None;
    }

    Some(components.iter().fold(PathBuf::new(), |acc, c| acc.join(c)))
}

// ── Image type detection ──────────────────────────────────────────────────────

// JPEG only — the slideshow decoder is built with JPEG support alone.

fn is_image_content_type(ct: &Option<String>) -> bool {
    matches!(ct.as_deref(), Some("image/jpeg" | "image/jpg"))
}

fn is_image_href(href: &str) -> bool {
    let lower = href.split('?').next().unwrap_or(href).to_lowercase();
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "jpg" | "jpeg"
    )
}

fn is_image_magic(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, 0xD8, 0xFF, ..])
}

/// Gallery-thumb shortcut: read the JPEG head and return an embedded EXIF
/// thumbnail when one is large enough. `None` means "read the whole file".
async fn try_gallery_exif_thumb(
    path: &Path,
    file_size: u64,
    intent: FetchIntent,
) -> Result<Option<Vec<u8>>> {
    if !intent.is_thumb() || file_size < 3 {
        return Ok(None);
    }
    let n = (EXIF_HEAD_SCAN_BYTES as u64).min(file_size) as usize;
    let mut file = match fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mut head = vec![0u8; n];
    if file.read_exact(&mut head).await.is_err() {
        return Ok(None);
    }
    if !is_image_magic(&head) {
        return Ok(None);
    }
    Ok(exif_thumb_from_head(&head, intent.target_edge()))
}

async fn file_mtime_utc(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0))
}

/// Return whether an existing local copy is older than the server's
/// `getlastmodified` value.  A missing/unreadable local mtime is treated as a
/// cache miss; when the server omits the timestamp we retain the safe
/// historical behaviour of not re-downloading every sync interval.
async fn local_needs_download(path: &Path, remote_modified: Option<DateTime<Utc>>) -> Result<bool> {
    let metadata = match fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error).with_context(|| format!("stat local WebDAV copy {}", path.display()))
        }
    };
    let Some(remote_modified) = remote_modified else {
        return Ok(false);
    };
    let Ok(local_modified) = metadata.modified() else {
        return Ok(true);
    };
    let Ok(local_secs) = local_modified.duration_since(std::time::UNIX_EPOCH) else {
        return Ok(true);
    };
    Ok(remote_modified.timestamp() >= 0
        && remote_modified.timestamp() as u64 > local_secs.as_secs())
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct WebDavPlugin {
    cfg: PluginConfig,
    client: Option<Client>,
    /// Guards the background sync loop so it is spawned at most once, no
    /// matter how many times `list_photos(offset == 0)` is called.
    sync_started: Arc<AtomicBool>,
    sync_abort: Arc<StdMutex<Option<tokio::task::AbortHandle>>>,
}

impl WebDavPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            client: None,
            sync_started: Arc::new(AtomicBool::new(false)),
            sync_abort: Arc::new(StdMutex::new(None)),
        }
    }

    fn client(&self) -> Result<&Client> {
        self.client
            .as_ref()
            .context("webdav: plugin has not been initialized")
    }

    // ── Config helpers ─────────────────────────────────────────────────────

    fn base_url(&self) -> Result<String> {
        self.cfg
            .require_str("url")
            .map(|s| s.trim_end_matches('/').to_string())
    }

    fn validate_tls_policy(&self, skip_tls: bool) -> Result<()> {
        if !skip_tls {
            return Ok(());
        }
        let base = reqwest::Url::parse(&self.base_url()?).context("parsing WebDAV url")?;
        let host = base
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("WebDAV url has no host"))?;
        let allowed = self
            .cfg
            .values
            .get("allowed_hosts")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str());
        if !allowed
            .into_iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(host))
        {
            return Err(anyhow::anyhow!(
                "WebDAV skip_tls_verify=true requires the URL host `{host}` in allowed_hosts"
            ));
        }
        Ok(())
    }

    fn remote_path(&self) -> String {
        let raw = self.cfg.get_str("remote_path").unwrap_or("/");
        let p = raw.trim_end_matches('/');
        if p.is_empty() {
            "/".to_string()
        } else {
            p.to_string()
        }
    }

    fn sync_dir(&self) -> PathBuf {
        PathBuf::from(
            self.cfg
                .get_str("sync_dir")
                .unwrap_or("/tmp/picogallery-webdav"),
        )
    }

    fn username(&self) -> Result<&str> {
        self.cfg.require_str("username")
    }
    fn password(&self) -> Result<&str> {
        self.cfg.require_str("password")
    }

    fn sync_interval_secs(&self) -> u64 {
        self.cfg
            .values
            .get("sync_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600)
    }

    // ── PROPFIND ───────────────────────────────────────────────────────────

    /// Issue a `PROPFIND Depth: 1` request and parse the response.
    async fn propfind(&self, url: &str) -> Result<Vec<DavEntry>> {
        let method = Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid HTTP method");

        let response = self
            .client()?
            .request(method, url)
            .basic_auth(self.username()?, Some(self.password()?))
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(PROPFIND_BODY)
            .send()
            .await
            .with_context(|| format!("PROPFIND {url}"))?
            .error_for_status()
            .with_context(|| format!("PROPFIND {url}: server error"))?;

        // Cap the body size before buffering it into RAM.
        if let Some(len) = response.content_length() {
            if len > MAX_PROPFIND_BYTES {
                return Err(anyhow::anyhow!(
                    "PROPFIND {url}: response too large ({len} bytes, max {MAX_PROPFIND_BYTES})"
                ));
            }
        }

        let bytes = read_bounded(response, MAX_PROPFIND_BYTES, "PROPFIND body").await?;

        let text = String::from_utf8_lossy(&bytes);
        debug!("PROPFIND {url}: {} bytes", text.len());

        Ok(parse_propfind(&text))
    }

    // ── Discovery ──────────────────────────────────────────────────────────

    /// Walk the DAV tree breadth-first (Depth: 1 per level) and collect all
    /// image file entries: `(download_url, local_relative_path, last_modified)`.
    ///
    /// The relative path mirrors the remote directory structure under
    /// `sync_dir`, so two remote files with the same name in different
    /// folders no longer collide locally.
    async fn discover_images(&self) -> Result<Vec<(String, PathBuf, Option<DateTime<Utc>>)>> {
        let base_url = self.base_url()?;
        let remote_path = self.remote_path();
        let start_url = if remote_path == "/" {
            base_url.clone()
        } else {
            format!("{base_url}{remote_path}")
        };

        let origin = url_origin(&base_url);

        // Base HREF path used to compute relative paths for local storage.
        // Extracted from the URL path component (everything after origin).
        let base_href_path = base_url.strip_prefix(origin).unwrap_or("").to_string()
            + if remote_path == "/" { "" } else { &remote_path };

        let mut images: Vec<(String, PathBuf, Option<DateTime<Utc>>)> = Vec::new();
        let mut queue: Vec<String> = vec![start_url];
        let mut dirs_walked = 0usize;

        while let Some(dir_url) = queue.pop() {
            if dirs_walked >= MAX_DIRS {
                warn!("WebDAV: stopping discovery — directory limit ({MAX_DIRS}) reached");
                break;
            }
            dirs_walked += 1;

            let entries = match self.propfind(&dir_url).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("WebDAV: PROPFIND {dir_url} failed: {e}");
                    continue;
                }
            };

            // The listing includes the directory itself — skip it (RFC 4918
            // doesn't guarantee it comes first). Servers may percent-encode
            // hrefs differently from our constructed URL, so compare decoded
            // forms: an unskipped self entry is a collection and would be
            // re-queued, PROPFINDing the same directory forever.
            let self_href = url_decode(dir_url.strip_prefix(origin).unwrap_or(&dir_url));
            let self_href = self_href.trim_end_matches('/');

            for entry in entries {
                let entry_href = entry.href.trim_end_matches('/');
                if url_decode(entry_href).trim_end_matches('/') == self_href {
                    continue;
                }

                let full_url = if entry.href.starts_with("http") {
                    entry.href.clone()
                } else {
                    format!("{origin}{}", entry.href)
                };

                // Security: never send Basic-Auth credentials off the configured
                // origin / base path. A hostile or compromised server can return
                // absolute hrefs pointing elsewhere; drop them before any PROPFIND
                // (collections) or download (files) attaches the credentials.
                if !href_in_scope(&full_url, origin, &base_href_path) {
                    warn!("WebDAV: skipping out-of-scope href {}", entry.href);
                    continue;
                }

                if entry.is_collection {
                    queue.push(full_url);
                } else if is_image_content_type(&entry.content_type) || is_image_href(&entry.href) {
                    // Relative path = href minus the base path prefix. If the
                    // server encodes hrefs differently from our config-derived
                    // prefix, fall back to the bare filename (old behaviour).
                    let rel = entry
                        .href
                        .strip_prefix(&base_href_path)
                        .and_then(decode_relative_path)
                        .or_else(|| {
                            // Still-encoded last path component → single decode.
                            let name = entry_href.rsplit('/').next().unwrap_or("");
                            decode_relative_path(name)
                        });
                    if let Some(rel) = rel {
                        images.push((full_url, rel, entry.last_modified));
                    } else {
                        warn!("WebDAV: skipping unsafe/empty path for {}", entry.href);
                    }
                }
            }

            if images.len() >= MAX_IMAGES {
                warn!("WebDAV: stopping discovery — image limit ({MAX_IMAGES}) reached");
                break;
            }
        }

        info!(
            "WebDAV: discovered {} images under {base_href_path}",
            images.len()
        );
        Ok(images)
    }

    // ── Sync ───────────────────────────────────────────────────────────────

    async fn sync_images(&self) -> Result<usize> {
        let sync_dir = self.sync_dir();
        fs::create_dir_all(&sync_dir)
            .await
            .with_context(|| format!("creating sync_dir {}", sync_dir.display()))?;

        let images = self.discover_images().await?;
        let mut synced = 0usize;

        for (download_url, rel_path, modified) in &images {
            // Mirror the remote directory structure under sync_dir so files
            // with the same name in different remote folders don't collide.
            let local_path = sync_dir.join(rel_path);
            let exists = fs::try_exists(&local_path).await.unwrap_or(false);
            if exists && !local_needs_download(&local_path, *modified).await? {
                debug!("WebDAV: already cached {}", rel_path.display());
                continue;
            }

            match self.download_file(download_url, &local_path).await {
                Ok(()) => {
                    synced += 1;
                    debug!(
                        "WebDAV: {} {}",
                        if exists { "updated" } else { "synced" },
                        rel_path.display()
                    );
                }
                Err(e) => {
                    warn!("WebDAV: failed to download {}: {e}", rel_path.display());
                }
            }
        }

        if synced > 0 {
            info!("WebDAV: sync complete — {synced} new files");
        }
        Ok(synced)
    }

    async fn download_file(&self, url: &str, dest: &Path) -> Result<()> {
        let response = self
            .client()?
            .get(url)
            .basic_auth(self.username()?, Some(self.password()?))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        // Reject oversized files before buffering the body into RAM.
        if let Some(len) = response.content_length() {
            if len > MAX_IMAGE_BYTES {
                return Err(anyhow::anyhow!(
                    "image too large ({} MB): {url}",
                    len / 1_048_576
                ));
            }
        }

        let bytes = read_bounded(response, MAX_IMAGE_BYTES, "WebDAV image body").await?;

        if !is_image_magic(&bytes) {
            return Err(anyhow::anyhow!("response is not a recognised image: {url}"));
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let ext = dest
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();
        let temp_path = dest.with_extension(format!("{ext}.part"));

        if let Err(error) = fs::write(&temp_path, &bytes).await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(error).with_context(|| format!("writing {}", temp_path.display()));
        }

        if let Err(error) = fs::rename(&temp_path, dest).await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(error).with_context(|| {
                format!("renaming {} to {}", temp_path.display(), dest.display())
            });
        }
        Ok(())
    }

    // ── Local file listing ──────────────────────────────────────────────────

    /// Recursively walk `sync_dir` (it now mirrors the remote tree) and
    /// collect image files.
    async fn list_local_images(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.sync_dir()).await else {
            return out;
        };
        let mut stack = vec![rd];

        while let Some(rd) = stack.last_mut() {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let path = entry.path();
                    match entry.file_type().await {
                        Ok(ft) if ft.is_dir() => {
                            if let Ok(sub) = fs::read_dir(&path).await {
                                stack.push(sub);
                            }
                        }
                        Ok(ft) if ft.is_file() && is_image_href(path.to_str().unwrap_or("")) => {
                            out.push(path);
                        }
                        _ => {}
                    }
                }
                Ok(None) | Err(_) => {
                    stack.pop();
                }
            }
        }
        out
    }

    // ── Background sync loop ────────────────────────────────────────────────

    fn spawn_sync_loop(
        client: Client,
        cfg: PluginConfig,
        interval: u64,
    ) -> tokio::task::AbortHandle {
        #[allow(unused_variables)]
        let task = tokio::spawn(async move {
            // Wait one interval before the first background sync so the initial
            // foreground sync has time to finish before we pile on.
            if interval > 0 {
                tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
            } else {
                return; // interval = 0 means no periodic background sync
            }

            loop {
                let plugin = WebDavPlugin {
                    cfg: cfg.clone(),
                    client: Some(client.clone()),
                    // Already running inside the loop — never spawn another.
                    sync_started: Arc::new(AtomicBool::new(true)),
                    sync_abort: Arc::new(StdMutex::new(None)),
                };
                info!("WebDAV: background sync started");
                match plugin.sync_images().await {
                    Ok(n) if n > 0 => info!("WebDAV: background sync — {n} new files"),
                    Ok(_) => debug!("WebDAV: background sync — no new files"),
                    Err(e) => warn!("WebDAV: background sync error: {e}"),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
            }
        });
        task.abort_handle()
    }
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for WebDavPlugin {
    fn name(&self) -> &str {
        "webdav"
    }
    fn uses_engine_image_cache(&self) -> bool {
        false
    }
    fn display_name(&self) -> &str {
        "WebDAV / Nextcloud"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }

    async fn init(&mut self, config: &PluginConfig) -> Result<()> {
        let _ = self.shutdown().await;
        self.cfg = config.clone();
        if self.cfg.get_str("url").is_none() {
            return Err(anyhow::anyhow!("webdav: missing required config key 'url'"));
        }
        // Build the HTTP client — supports custom TLS for self-signed certs.
        let skip_tls = self
            .cfg
            .values
            .get("skip_tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.validate_tls_policy(skip_tls)?;

        let builder = ClientBuilder::new()
            .danger_accept_invalid_certs(skip_tls)
            // Without timeouts a dead/unreachable server hangs requests
            // forever and the slideshow freezes on the current frame.
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT);
        #[cfg(target_os = "macos")]
        let builder = builder.no_proxy();
        self.client = Some(builder.build().context("building WebDAV HTTP client")?);

        fs::create_dir_all(self.sync_dir())
            .await
            .with_context(|| format!("creating sync_dir {}", self.sync_dir().display()))?;

        // Validate credentials are present before the first sync attempt.
        let _ = self.username()?;
        let _ = self.password()?;

        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::Authenticated
    }
    async fn authenticate(&mut self) -> Result<AuthStatus> {
        Ok(AuthStatus::Authenticated)
    }
    async fn refresh_auth(&mut self) -> Result<()> {
        Ok(())
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let mut paths = self.list_local_images().await;

        if paths.is_empty() {
            // No cached photos yet — run a blocking foreground sync, then
            // re-walk the sync dir to pick up whatever it fetched.
            info!("WebDAV: no local cache found — running initial sync…");
            if let Err(e) = self.sync_images().await {
                warn!("WebDAV: initial sync failed: {e}");
            }
            paths = self.list_local_images().await;
        }

        if offset == 0 && !self.sync_started.load(Ordering::Relaxed) {
            // Resolve the client before flipping the flag so a missing URL
            // cannot permanently skip background sync.
            let client = self.client()?.clone();
            if !self.sync_started.swap(true, Ordering::Relaxed) {
                let abort =
                    Self::spawn_sync_loop(client, self.cfg.clone(), self.sync_interval_secs());
                *self.sync_abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(abort);
            }
        }

        if paths.is_empty() {
            warn!(
                "WebDAV: no images available after sync. Check url/username/password/remote_path."
            );
            return Ok(vec![]);
        }

        // read_dir order is filesystem-dependent — sort so paging is stable
        // and the same offset never skips or duplicates files between calls.
        paths.sort();

        let sync_dir = self.sync_dir();
        let mut photos = Vec::new();
        for path in paths.into_iter().skip(offset).take(limit) {
            let rel = path.strip_prefix(&sync_dir).unwrap_or(&path);
            let Some(id) = rel.to_str().map(str::to_string) else {
                continue;
            };
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&id)
                .to_string();
            let Some(download) = path.to_str().map(str::to_string) else {
                continue;
            };
            let album = path
                .strip_prefix(&sync_dir)
                .ok()
                .and_then(|rel| rel.parent())
                .and_then(|p| p.iter().next())
                .and_then(|c| c.to_str())
                .map(|s| s.to_string());
            let taken_at = file_mtime_utc(&path).await;
            photos.push(PhotoMeta {
                id,
                filename,
                width: 0,
                height: 0,
                taken_at,
                download_url: Some(download),
                album,
                title: None,
                location: None,
                is_favorite: false,
                extra: Default::default(),
            });
        }

        Ok(photos)
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(abort) = self
            .sync_abort
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            abort.abort();
        }
        self.sync_started.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, intent: FetchIntent) -> Result<Vec<u8>> {
        let path_str = meta
            .download_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("webdav: no local path for '{}'", meta.filename))?;

        let path = PathBuf::from(path_str);

        // Re-canonicalize at read time: guards against symlink swap attacks.
        let canonical = fs::canonicalize(&path)
            .await
            .with_context(|| format!("resolving '{path_str}'"))?;

        // Must still live under the configured sync_dir. Fail closed if the
        // sync_dir itself can't be canonicalized — comparing a canonical path
        // against a non-canonical root would defeat the traversal guard.
        let sync_dir = fs::canonicalize(self.sync_dir())
            .await
            .with_context(|| format!("resolving sync_dir '{}'", self.sync_dir().display()))?;
        if !canonical.starts_with(&sync_dir) {
            return Err(anyhow::anyhow!(
                "security: '{}' is outside sync_dir",
                canonical.display()
            ));
        }

        let file_size = fs::metadata(&canonical)
            .await
            .with_context(|| format!("stat '{path_str}'"))?
            .len();
        if file_size > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "file too large ({} MB): '{}'",
                file_size / 1_048_576,
                canonical.display()
            ));
        }

        if let Some(thumb) = try_gallery_exif_thumb(&canonical, file_size, intent).await? {
            return Ok(thumb);
        }

        let bytes = fs::read(&canonical)
            .await
            .with_context(|| format!("reading '{path_str}'"))?;

        if !is_image_magic(&bytes) {
            return Err(anyhow::anyhow!(
                "not a recognised image format: '{}'",
                canonical.display()
            ));
        }

        Ok(bytes)
    }
}

// ── Misc helpers ──────────────────────────────────────────────────────────────

/// Extract and URL-decode the last path component of a URL.
/// Only used by tests since sync switched to structure-preserving paths.
#[cfg(test)]
fn filename_from_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    url_decode(last)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_copy_is_refreshed_when_remote_mtime_is_newer() {
        let dir =
            std::env::temp_dir().join(format!("picogallery-webdav-mtime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("photo.jpg");
        fs::write(&path, b"old").await.unwrap();
        let newer = DateTime::<Utc>::from_timestamp(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 60,
            0,
        )
        .unwrap();
        assert!(local_needs_download(&path, Some(newer)).await.unwrap());
        assert!(!local_needs_download(&path, None).await.unwrap());
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn missing_local_copy_needs_download() {
        let dir =
            std::env::temp_dir().join(format!("picogallery-webdav-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("missing.jpg");
        assert!(local_needs_download(&path, None).await.unwrap());
        let _ = fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn parse_propfind_finds_files_and_dirs() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/Photos/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>Photos</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/Photos/sunset.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:displayname>sunset.jpg</d:displayname>
        <d:getcontenttype>image/jpeg</d:getcontenttype>
        <d:getcontentlength>123456</d:getcontentlength>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

        let entries = parse_propfind(xml);
        assert_eq!(entries.len(), 2);

        let dir = entries.iter().find(|e| e.href.ends_with('/')).unwrap();
        assert!(dir.is_collection);

        let file = entries
            .iter()
            .find(|e| e.href.ends_with("sunset.jpg"))
            .unwrap();
        assert!(!file.is_collection);
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn parse_propfind_empty_xml() {
        assert!(parse_propfind("").is_empty());
        assert!(parse_propfind("<d:multistatus/>").is_empty());
    }

    #[test]
    fn url_decode_basic() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("no_encoding"), "no_encoding");
        assert_eq!(url_decode("%2F"), "/");
        assert_eq!(url_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn url_decode_invalid_sequence_passthrough() {
        assert_eq!(url_decode("bad%GG"), "bad%GG");
    }

    #[test]
    fn decode_relative_path_rejects_traversal() {
        assert!(decode_relative_path("../secret").is_none());
        assert!(decode_relative_path("a/../../b").is_none());
        assert!(decode_relative_path("./a").is_none());
    }

    #[test]
    fn decode_relative_path_normal() {
        let p = decode_relative_path("Photos/Vacation/img.jpg").unwrap();
        assert_eq!(p, PathBuf::from("Photos/Vacation/img.jpg"));
    }

    #[test]
    fn decode_relative_path_rejects_encoded_separator_smuggling() {
        // %2F decodes to '/', %2E%2E to ".." — must not create traversal.
        assert!(decode_relative_path("a%2F..%2Fb").is_none());
        assert!(decode_relative_path("%2E%2E/secret").is_none());
        assert!(decode_relative_path("a%5C..%5Cb").is_none()); // backslash
        assert!(decode_relative_path("").is_none());
    }

    #[test]
    fn filename_from_url_basic() {
        assert_eq!(
            filename_from_url("https://example.com/dav/Photos/my%20pic.jpg"),
            "my pic.jpg"
        );
        assert_eq!(
            filename_from_url("https://example.com/dav/Photos/sunset.jpg?token=abc"),
            "sunset.jpg"
        );
        assert_eq!(filename_from_url(""), "");
    }

    #[test]
    fn url_origin_extraction() {
        assert_eq!(
            url_origin("https://example.com/dav/files/user"),
            "https://example.com"
        );
        assert_eq!(
            url_origin("https://example.com:8443/dav"),
            "https://example.com:8443"
        );
        assert_eq!(url_origin("http://nas.local/photos"), "http://nas.local");
    }

    #[test]
    fn href_in_scope_allows_same_origin_under_base() {
        let origin = "https://nas.local";
        let base = "/dav/photos";
        assert!(href_in_scope(
            "https://nas.local/dav/photos/trip/a.jpg",
            origin,
            base
        ));
        // Encoding differences between href and base must not reject in-tree URLs.
        assert!(href_in_scope(
            "https://nas.local/dav/photos/My%20Album/a.jpg",
            origin,
            base
        ));
    }

    #[test]
    fn href_in_scope_rejects_foreign_origin() {
        // The credential-leak case: server returns an absolute href elsewhere.
        assert!(!href_in_scope(
            "https://evil.example/steal",
            "https://nas.local",
            "/dav/photos"
        ));
        // Same host, different scheme/port is still a different origin.
        assert!(!href_in_scope(
            "http://nas.local/dav/photos/a.jpg",
            "https://nas.local",
            "/dav/photos"
        ));
        assert!(!href_in_scope(
            "https://nas.local:8443/dav/photos/a.jpg",
            "https://nas.local",
            "/dav/photos"
        ));
    }

    #[test]
    fn href_in_scope_rejects_outside_base_path() {
        assert!(!href_in_scope(
            "https://nas.local/etc/passwd",
            "https://nas.local",
            "/dav/photos"
        ));
        // Empty base path = whole origin in scope (base_url has no path component).
        assert!(href_in_scope(
            "https://nas.local/anything/a.jpg",
            "https://nas.local",
            ""
        ));
    }

    #[test]
    fn href_in_scope_rejects_prefix_sibling_paths() {
        // Classic starts_with footgun: base `/dav/photos` must NOT allow
        // `/dav/photosEvil/...` (same origin, credentials would still attach).
        assert!(!href_in_scope(
            "https://nas.local/dav/photosEvil/a.jpg",
            "https://nas.local",
            "/dav/photos"
        ));
        assert!(!href_in_scope(
            "https://nas.local/dav/photos-backup/a.jpg",
            "https://nas.local",
            "/dav/photos"
        ));
        // Exact base and true descendants remain allowed.
        assert!(href_in_scope(
            "https://nas.local/dav/photos",
            "https://nas.local",
            "/dav/photos"
        ));
        assert!(href_in_scope(
            "https://nas.local/dav/photos/",
            "https://nas.local",
            "/dav/photos"
        ));
        assert!(href_in_scope(
            "https://nas.local/dav/photos/trip/a.jpg",
            "https://nas.local",
            "/dav/photos"
        ));
    }

    #[test]
    fn is_image_href_extensions() {
        assert!(is_image_href("/photos/a.jpg"));
        assert!(is_image_href("/photos/A.JPEG"));
        // JPEG only — other formats are rejected.
        assert!(!is_image_href("/photos/b.png"));
        assert!(!is_image_href("/photos/c.gif"));
        assert!(!is_image_href("/photos/d.webp"));
        assert!(!is_image_href("/photos/e.mp4"));
        assert!(!is_image_href("/photos/f.txt"));
    }

    #[test]
    fn is_image_magic_signatures() {
        assert!(is_image_magic(&[0xFF, 0xD8, 0xFF, 0xE0]));
        // JPEG only — PNG/GIF/WebP are rejected.
        assert!(!is_image_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A]));
        assert!(!is_image_magic(b"GIF89a"));
        assert!(!is_image_magic(b"RIFF\x00\x00\x00\x00WEBP"));
        assert!(!is_image_magic(b"Not an image"));
        assert!(!is_image_magic(b""));
    }

    #[test]
    fn parse_propfind_handles_alternate_namespace_prefix() {
        // quick-xml's local_name() must strip whatever prefix the server uses.
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/Photos/a.jpg</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontenttype>image/jpeg</D:getcontenttype>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

        let entries = parse_propfind(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].href, "/dav/Photos/a.jpg");
        assert!(!entries[0].is_collection);
        assert_eq!(entries[0].content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn parse_propfind_appends_multi_chunk_text() {
        // A comment splits the href text node into two Text events — the
        // parser must append the chunks instead of keeping only the last one.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/Photos/sun<!-- split -->set.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype/>
        <d:getcontenttype>image/jpeg</d:getcontenttype>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

        let entries = parse_propfind(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].href, "/dav/Photos/sunset.jpg");
        assert_eq!(entries[0].content_type.as_deref(), Some("image/jpeg"));
    }

    #[tokio::test]
    async fn test_webdav_propfind_mock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/folder/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/folder/image.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:getcontenttype>image/jpeg</d:getcontenttype>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

        Mock::given(method("PROPFIND"))
            .and(path("/dav/folder/"))
            .respond_with(ResponseTemplate::new(207).set_body_string(xml))
            .mount(&server)
            .await;

        let mut cfg = picogallery_core::PluginConfig::default();
        cfg.values.insert(
            "url".to_string(),
            serde_json::Value::String(format!("{}/dav/folder/", server.uri())),
        );
        cfg.values.insert(
            "username".to_string(),
            serde_json::Value::String("u".into()),
        );
        cfg.values.insert(
            "password".to_string(),
            serde_json::Value::String("p".into()),
        );

        let mut plugin = WebDavPlugin::new(cfg);
        plugin.client = Some(reqwest::Client::new());

        let entries = plugin
            .propfind(&format!("{}/dav/folder/", server.uri()))
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].href, "/dav/folder/");
        assert!(entries[0].is_collection);
        assert_eq!(entries[1].href, "/dav/folder/image.jpg");
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].content_type.as_deref(), Some("image/jpeg"));
    }
}

#[tokio::test]
async fn test_webdav_propfind_and_sync() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    let propfind_resp = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/remote.php/dav/files/user/Photos/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/user/Photos/img.jpg</d:href>
    <d:propstat>
      <d:prop>
        <d:getcontenttype>image/jpeg</d:getcontenttype>
        <d:getlastmodified>Sun, 01 Jan 2023 12:00:00 GMT</d:getlastmodified>
        <d:resourcetype/>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;

    Mock::given(method("PROPFIND"))
        .and(path("/remote.php/dav/files/user/Photos"))
        .respond_with(ResponseTemplate::new(207).set_body_string(propfind_resp))
        .mount(&mock_server)
        .await;

    // Mock the GET request for the file
    Mock::given(method("GET"))
        .and(path("/remote.php/dav/files/user/Photos/img.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF, 0x00, 0x00, 0x00]),
        )
        .mount(&mock_server)
        .await;

    let mut values = std::collections::HashMap::new();
    values.insert(
        "url".into(),
        serde_json::Value::String(format!("{}/remote.php/dav/files/user", mock_server.uri())),
    );
    values.insert("username".into(), serde_json::Value::String("test".into()));
    values.insert("password".into(), serde_json::Value::String("test".into()));
    values.insert(
        "remote_path".into(),
        serde_json::Value::String("/Photos".into()),
    );
    let dir = std::env::temp_dir().join(format!("picogallery-webdav-mock-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir).await;
    values.insert(
        "sync_dir".into(),
        serde_json::Value::String(dir.to_string_lossy().to_string()),
    );

    let cfg = PluginConfig { values };

    let mut plugin = WebDavPlugin::new(cfg.clone());
    plugin.init(&cfg).await.unwrap();

    // Call sync images
    let synced = plugin.sync_images().await.unwrap();
    assert_eq!(synced, 1);

    let _ = fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn test_webdav_malformed_xml() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock_server = MockServer::start().await;

    Mock::given(method("PROPFIND"))
        .and(path("/remote.php/dav/files/user/Photos"))
        .respond_with(ResponseTemplate::new(207).set_body_string("INVALID XML <d:response>"))
        .mount(&mock_server)
        .await;

    let mut values = std::collections::HashMap::new();
    values.insert(
        "url".into(),
        serde_json::Value::String(format!("{}/remote.php/dav/files/user", mock_server.uri())),
    );
    values.insert("username".into(), serde_json::Value::String("test".into()));
    values.insert("password".into(), serde_json::Value::String("test".into()));
    values.insert(
        "remote_path".into(),
        serde_json::Value::String("/Photos".into()),
    );

    let cfg = PluginConfig { values };

    let mut plugin = WebDavPlugin::new(cfg.clone());
    plugin.init(&cfg).await.unwrap();

    // This shouldn't panic, but might result in empty or error
    let result = plugin
        .propfind(&format!(
            "{}/remote.php/dav/files/user/Photos",
            mock_server.uri()
        ))
        .await
        .unwrap();
    assert_eq!(result.len(), 0);
}
