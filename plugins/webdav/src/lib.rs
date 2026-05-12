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
use tokio::fs;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

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

    // Mutable builder state for the entry currently being parsed.
    let mut href = String::new();
    let mut is_collection = false;
    let mut content_type: Option<String> = None;
    let mut last_modified: Option<DateTime<Utc>> = None;
    let mut in_response = false;
    let mut in_resourcetype = false;
    let mut current_tag = String::new();

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                match local {
                    "response" => {
                        in_response = true;
                        href.clear();
                        is_collection = false;
                        content_type = None;
                        last_modified = None;
                    }
                    "resourcetype" => in_resourcetype = true,
                    _ => {}
                }
                if in_response {
                    current_tag = local.to_string();
                }
            }
            Ok(Event::Empty(ref e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "collection" && in_resourcetype {
                    is_collection = true;
                }
            }
            Ok(Event::Text(ref e)) => {
                if !in_response {
                    continue;
                }
                let text = match e.unescape() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "href" => href = text.to_string(),
                    "getcontenttype" => {
                        content_type = Some(text.split(';').next().unwrap_or(text).trim().to_string());
                    }
                    "getlastmodified" => {
                        last_modified = DateTime::parse_from_rfc2822(text)
                            .ok()
                            .map(|d| d.with_timezone(&Utc));
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                match local {
                    "response" if in_response => {
                        if !href.is_empty() {
                            entries.push(DavEntry {
                                href: href.clone(),
                                is_collection,
                                content_type: content_type.clone(),
                                last_modified,
                            });
                        }
                        in_response = false;
                    }
                    "resourcetype" => in_resourcetype = false,
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    entries
}

/// Extract the local (non-namespace) part of an XML element name.
fn local_name(qname: &[u8]) -> &str {
    let s = std::str::from_utf8(qname).unwrap_or("");
    s.rfind(':').map(|i| &s[i + 1..]).unwrap_or(s)
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
/// Path traversal components (`..`, `.`) are rejected.
#[allow(dead_code)]
fn decode_relative_path(encoded_rel: &str) -> Option<PathBuf> {
    let components: Vec<String> = encoded_rel
        .split('/')
        .filter(|s| !s.is_empty())
        .map(url_decode)
        .collect();

    // Security: reject traversal components.
    if components.iter().any(|c| c == ".." || c == ".") {
        return None;
    }

    Some(components.iter().fold(PathBuf::new(), |acc, c| acc.join(c)))
}

// ── Image type detection ──────────────────────────────────────────────────────

fn is_image_content_type(ct: &Option<String>) -> bool {
    matches!(
        ct.as_deref(),
        Some(
            "image/jpeg"
                | "image/jpg"
                | "image/png"
                | "image/gif"
                | "image/webp"
                | "image/bmp"
        )
    )
}

fn is_image_href(href: &str) -> bool {
    let lower = href.split('?').next().unwrap_or(href).to_lowercase();
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
    )
}

fn is_image_magic(bytes: &[u8]) -> bool {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => true,
        [0x89, b'P', b'N', b'G', ..] => true,
        [b'G', b'I', b'F', b'8', ..] => true,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => true,
        [b'B', b'M', ..] => true,
        _ => false,
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct WebDavPlugin {
    cfg:    PluginConfig,
    client: Client,
}

impl WebDavPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self { cfg, client: Client::new() }
    }

    // ── Config helpers ─────────────────────────────────────────────────────

    fn base_url(&self) -> Result<String> {
        self.cfg.require_str("url").map(|s| s.trim_end_matches('/').to_string())
    }

    fn remote_path(&self) -> String {
        let raw = self.cfg.get_str("remote_path").unwrap_or("/");
        let p = raw.trim_end_matches('/');
        if p.is_empty() { "/".to_string() } else { p.to_string() }
    }

    fn sync_dir(&self) -> PathBuf {
        PathBuf::from(self.cfg.get_str("sync_dir").unwrap_or("/tmp/picogallery-webdav"))
    }

    fn username(&self) -> Result<&str> { self.cfg.require_str("username") }
    fn password(&self) -> Result<&str> { self.cfg.require_str("password") }

    fn sync_interval_secs(&self) -> u64 {
        self.cfg.values.get("sync_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600)
    }

    // ── PROPFIND ───────────────────────────────────────────────────────────

    /// Issue a `PROPFIND Depth: 1` request and parse the response.
    async fn propfind(&self, url: &str) -> Result<Vec<DavEntry>> {
        let method = Method::from_bytes(b"PROPFIND")
            .expect("PROPFIND is a valid HTTP method");

        let response = self.client
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

        let text = response.text().await.context("reading PROPFIND body")?;
        debug!("PROPFIND {url}: {} bytes", text.len());

        Ok(parse_propfind(&text))
    }

    // ── Discovery ──────────────────────────────────────────────────────────

    /// Walk the DAV tree breadth-first (Depth: 1 per level) and collect all
    /// image file entries together with their full download URLs.
    async fn discover_images(&self) -> Result<Vec<(String, Option<DateTime<Utc>>)>> {
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

        let mut images: Vec<(String, Option<DateTime<Utc>>)> = Vec::new();
        let mut queue: Vec<String> = vec![start_url];

        while let Some(dir_url) = queue.pop() {
            let entries = match self.propfind(&dir_url).await {
                Ok(e)  => e,
                Err(e) => { warn!("WebDAV: PROPFIND {dir_url} failed: {e}"); continue; }
            };

            // The first entry is the directory itself — skip it.
            let skip_href = dir_url.strip_prefix(origin).unwrap_or(&dir_url);

            for entry in entries {
                let entry_href = entry.href.trim_end_matches('/');
                if entry_href == skip_href.trim_end_matches('/') {
                    continue;
                }

                let full_url = if entry.href.starts_with("http") {
                    entry.href.clone()
                } else {
                    format!("{origin}{}", entry.href)
                };

                if entry.is_collection {
                    queue.push(full_url);
                } else if is_image_content_type(&entry.content_type)
                    || is_image_href(&entry.href)
                {
                    images.push((full_url, entry.last_modified));
                }
            }
        }

        info!("WebDAV: discovered {} images under {base_href_path}", images.len());
        Ok(images)
    }

    // ── Sync ───────────────────────────────────────────────────────────────

    async fn sync_images(&self) -> Result<usize> {
        let sync_dir = self.sync_dir();
        fs::create_dir_all(&sync_dir).await
            .with_context(|| format!("creating sync_dir {}", sync_dir.display()))?;

        let images = self.discover_images().await?;
        let mut synced = 0usize;

        for (download_url, _modified) in &images {
            let filename = filename_from_url(download_url);
            if filename.is_empty() {
                continue;
            }

            let local_path = sync_dir.join(&filename);
            if local_path.exists() {
                debug!("WebDAV: already cached {filename}");
                continue;
            }

            match self.download_file(download_url, &local_path).await {
                Ok(()) => {
                    synced += 1;
                    debug!("WebDAV: synced {filename}");
                }
                Err(e) => {
                    warn!("WebDAV: failed to download {filename}: {e}");
                    // Remove partial file if it was created.
                    let _ = fs::remove_file(&local_path).await;
                }
            }
        }

        if synced > 0 {
            info!("WebDAV: sync complete — {synced} new files");
        }
        Ok(synced)
    }

    async fn download_file(&self, url: &str, dest: &Path) -> Result<()> {
        let response = self.client
            .get(url)
            .basic_auth(self.username()?, Some(self.password()?))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        let bytes = response.bytes().await.context("reading image body")?;

        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "image too large ({} MB): {url}",
                bytes.len() / 1_048_576
            ));
        }

        if !is_image_magic(&bytes) {
            return Err(anyhow::anyhow!("response is not a recognised image: {url}"));
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(dest, &bytes)
            .await
            .with_context(|| format!("writing {}", dest.display()))
    }

    // ── Local file listing ──────────────────────────────────────────────────

    async fn list_local_images(&self) -> Vec<PathBuf> {
        let sync_dir = self.sync_dir();
        let mut out = Vec::new();
        let Ok(mut rd) = fs::read_dir(&sync_dir).await else { return out };

        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if let Ok(ft) = entry.file_type().await {
                if ft.is_file() && is_image_href(path.to_str().unwrap_or("")) {
                    out.push(path);
                }
            }
        }
        out
    }

    // ── Background sync loop ────────────────────────────────────────────────

    fn spawn_sync_loop(client: Client, cfg: PluginConfig, interval: u64) {
        #[allow(unused_variables)]
        tokio::spawn(async move {
            // Wait one interval before the first background sync so the initial
            // foreground sync has time to finish before we pile on.
            if interval > 0 {
                tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
            } else {
                return; // interval = 0 means no periodic background sync
            }

            loop {
                let plugin = WebDavPlugin { cfg: cfg.clone(), client: client.clone() };
                info!("WebDAV: background sync started");
                match plugin.sync_images().await {
                    Ok(n) if n > 0 => info!("WebDAV: background sync — {n} new files"),
                    Ok(_)          => debug!("WebDAV: background sync — no new files"),
                    Err(e)         => warn!("WebDAV: background sync error: {e}"),
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
            }
        });
    }
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for WebDavPlugin {
    fn name(&self)         -> &str { "webdav"          }
    fn display_name(&self) -> &str { "WebDAV / Nextcloud" }
    fn version(&self)      -> &str { "0.1.0"           }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        // Build the HTTP client — supports custom TLS for self-signed certs.
        let skip_tls = self.cfg.values
            .get("skip_tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        self.client = ClientBuilder::new()
            .danger_accept_invalid_certs(skip_tls)
            .build()
            .context("building WebDAV HTTP client")?;

        fs::create_dir_all(self.sync_dir()).await
            .with_context(|| format!("creating sync_dir {}", self.sync_dir().display()))?;

        // Validate credentials are present before the first sync attempt.
        let _ = self.username()?;
        let _ = self.password()?;

        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus { AuthStatus::Authenticated }
    async fn authenticate(&mut self) -> Result<AuthStatus> { Ok(AuthStatus::Authenticated) }
    async fn refresh_auth(&mut self)  -> Result<()>         { Ok(()) }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let local = self.list_local_images().await;

        if local.is_empty() {
            // No cached photos yet — run a blocking foreground sync.
            info!("WebDAV: no local cache found — running initial sync…");
            if let Err(e) = self.sync_images().await {
                warn!("WebDAV: initial sync failed: {e}");
            }
        }

        if offset == 0 {
            // Kick off the background periodic sync on the first page request.
            Self::spawn_sync_loop(
                self.client.clone(),
                self.cfg.clone(),
                self.sync_interval_secs(),
            );
        }

        let paths = self.list_local_images().await;
        if paths.is_empty() {
            warn!("WebDAV: no images available after sync. Check url/username/password/remote_path.");
            return Ok(vec![]);
        }

        let photos: Vec<PhotoMeta> = paths
            .into_iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, path)| {
                let filename = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                PhotoMeta {
                    id:           (offset + i).to_string(),
                    filename,
                    width:        0,
                    height:       0,
                    taken_at:     None,
                    download_url: Some(path.to_string_lossy().to_string()),
                    extra:        Default::default(),
                }
            })
            .collect();

        Ok(photos)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _dw: u32,
        _dh: u32,
    ) -> Result<Vec<u8>> {
        let path_str = meta.download_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("webdav: no local path for '{}'", meta.filename))?;

        let path = PathBuf::from(path_str);

        // Re-canonicalize at read time: guards against symlink swap attacks.
        let canonical = path.canonicalize()
            .with_context(|| format!("resolving '{path_str}'"))?;

        // Must still live under the configured sync_dir.
        let sync_dir = self.sync_dir().canonicalize().unwrap_or(self.sync_dir());
        if !canonical.starts_with(&sync_dir) {
            return Err(anyhow::anyhow!(
                "security: '{}' is outside sync_dir",
                canonical.display()
            ));
        }

        let file_size = fs::metadata(&canonical).await
            .with_context(|| format!("stat '{path_str}'"))?
            .len();
        if file_size > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "file too large ({} MB): '{}'",
                file_size / 1_048_576,
                canonical.display()
            ));
        }

        let bytes = fs::read(&canonical).await
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
fn filename_from_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    url_decode(last)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

        let file = entries.iter().find(|e| e.href.ends_with("sunset.jpg")).unwrap();
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
    fn filename_from_url_basic() {
        assert_eq!(filename_from_url("https://example.com/dav/Photos/my%20pic.jpg"), "my pic.jpg");
        assert_eq!(filename_from_url("https://example.com/dav/Photos/sunset.jpg?token=abc"), "sunset.jpg");
        assert_eq!(filename_from_url(""), "");
    }

    #[test]
    fn url_origin_extraction() {
        assert_eq!(url_origin("https://example.com/dav/files/user"), "https://example.com");
        assert_eq!(url_origin("https://example.com:8443/dav"), "https://example.com:8443");
        assert_eq!(url_origin("http://nas.local/photos"), "http://nas.local");
    }

    #[test]
    fn is_image_href_extensions() {
        assert!(is_image_href("/photos/a.jpg"));
        assert!(is_image_href("/photos/A.JPEG"));
        assert!(is_image_href("/photos/b.png"));
        assert!(is_image_href("/photos/c.gif"));
        assert!(is_image_href("/photos/d.webp"));
        assert!(!is_image_href("/photos/e.mp4"));
        assert!(!is_image_href("/photos/f.txt"));
    }

    #[test]
    fn is_image_magic_signatures() {
        assert!(is_image_magic(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_image_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A]));
        assert!(is_image_magic(&[b'G', b'I', b'F', b'8', b'9', b'a']));
        assert!(is_image_magic(b"RIFF\x00\x00\x00\x00WEBP"));
        assert!(!is_image_magic(b"Not an image"));
        assert!(!is_image_magic(b""));
    }

    #[test]
    fn local_name_strips_namespace() {
        assert_eq!(local_name(b"d:href"), "href");
        assert_eq!(local_name(b"href"), "href");
        assert_eq!(local_name(b"oc:fileid"), "fileid");
    }
}
