//! Immich plugin for PicoGallery.
//!
//! Streams photos from a self-hosted Immich server (local LAN or remote)
//! using its REST API. No local sync directory — bytes are fetched on demand
//! per slide via the `/assets/{id}/thumbnail` or `/assets/{id}/original`
//! endpoint, so the Pi's SD card stays small.
//!
//! Authentication uses an Immich API key (`x-api-key` header). Generate one
//! from the web UI under **Account Settings → API Keys**.
//!
//! Config keys (in `[[plugins]]`):
//!
//! ```toml
//! [[plugins]]
//! name             = "immich"
//! enabled          = true
//! url              = "http://immich.lan:2283"          # base URL of the Immich server
//! api_key          = "YOUR_IMMICH_API_KEY"
//! page_size        = 250                                # assets per metadata page
//! image_size       = "preview"                          # "preview" | "thumbnail" | "original"
//! album_id         = ""                                 # optional: only photos in this album
//! favorites_only   = false                              # only assets marked favourite
//! skip_tls_verify  = false                              # true for self-signed certs
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use reqwest::{Client, ClientBuilder};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_PAGE_SIZE: u64 = 250;
const DEFAULT_IMAGE_SIZE: &str = "preview";

// ── Immich API DTOs ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExifInfo {
    #[serde(default)]
    #[serde(rename = "exifImageWidth")]
    exif_image_width: Option<u32>,
    #[serde(default)]
    #[serde(rename = "exifImageHeight")]
    exif_image_height: Option<u32>,
    #[serde(default)]
    #[serde(rename = "dateTimeOriginal")]
    date_time_original: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImmichAsset {
    id: String,
    #[serde(default, rename = "originalFileName")]
    original_filename: Option<String>,
    #[serde(default, rename = "type")]
    asset_type: Option<String>,
    #[serde(default, rename = "fileCreatedAt")]
    file_created_at: Option<String>,
    #[serde(default, rename = "exifInfo")]
    exif_info: Option<ExifInfo>,
}

#[derive(Debug, Deserialize)]
struct AssetsPage {
    items: Vec<ImmichAsset>,
    #[serde(default, rename = "nextPage")]
    next_page: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    assets: AssetsPage,
}

// ── Plugin ───────────────────────────────────────────────────────────────────

pub struct ImmichPlugin {
    cfg:     PluginConfig,
    client:  Client,
    /// Lazily-populated metadata cache shared across calls.
    cache:   Arc<RwLock<CacheState>>,
}

#[derive(Default)]
struct CacheState {
    items:    Vec<PhotoMeta>,
    next_pg:  Option<u64>,
    finished: bool,
}

impl ImmichPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            client: Client::new(),
            cache:  Arc::new(RwLock::new(CacheState { next_pg: Some(1), ..Default::default() })),
        }
    }

    // ── Config helpers ─────────────────────────────────────────────────────

    fn base_url(&self) -> Result<String> {
        let raw = self.cfg.require_str("url")?;
        Ok(raw.trim_end_matches('/').to_string())
    }

    fn api_key(&self) -> Result<&str> { self.cfg.require_str("api_key") }

    fn page_size(&self) -> u64 {
        self.cfg.values.get("page_size")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0 && n <= 1000)
            .unwrap_or(DEFAULT_PAGE_SIZE)
    }

    fn image_size(&self) -> String {
        let raw = self.cfg.get_str("image_size").unwrap_or(DEFAULT_IMAGE_SIZE).to_lowercase();
        match raw.as_str() {
            "preview" | "thumbnail" | "original" => raw,
            other => {
                warn!("immich: unknown image_size '{other}', falling back to 'preview'");
                DEFAULT_IMAGE_SIZE.to_string()
            }
        }
    }

    fn album_id(&self) -> Option<&str> {
        self.cfg.get_str("album_id").map(str::trim).filter(|s| !s.is_empty())
    }

    fn favorites_only(&self) -> bool {
        self.cfg.values.get("favorites_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    // ── HTTP ───────────────────────────────────────────────────────────────

    /// POST `/api/search/metadata` for one page of image assets.
    async fn fetch_page(&self, page: u64) -> Result<AssetsPage> {
        let base = self.base_url()?;
        let url  = format!("{base}/api/search/metadata");

        let mut body = serde_json::json!({
            "page": page,
            "size": self.page_size(),
            "type": "IMAGE",
            "withExif": true,
        });
        if let Some(album) = self.album_id() {
            body.as_object_mut().unwrap().insert("albumIds".to_string(), serde_json::json!([album]));
        }
        if self.favorites_only() {
            body.as_object_mut().unwrap().insert("isFavorite".to_string(), serde_json::json!(true));
        }

        let resp = self.client
            .post(&url)
            .header("x-api-key", self.api_key()?)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("POST {url}: server error"))?;

        let parsed: SearchResponse = resp.json().await
            .context("parsing Immich search response")?;
        Ok(parsed.assets)
    }

    /// Extend the metadata cache until it covers `needed` items or the server
    /// has no more pages.
    async fn ensure_cache(&self, needed: usize) -> Result<()> {
        loop {
            {
                let state = self.cache.read().await;
                if state.finished || state.items.len() >= needed {
                    return Ok(());
                }
            }

            let page = {
                let state = self.cache.read().await;
                state.next_pg.unwrap_or(1)
            };

            let assets_page = self.fetch_page(page).await?;
            let items: Vec<PhotoMeta> = assets_page.items
                .into_iter()
                .filter(|a| a.asset_type.as_deref().unwrap_or("IMAGE") == "IMAGE")
                .map(to_photo_meta)
                .collect();
            let count = items.len();

            let next_pg = assets_page.next_page
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok());

            let mut state = self.cache.write().await;
            state.items.extend(items);
            if let Some(np) = next_pg {
                if np <= page {
                    warn!("immich: API returned invalid next_page {np} (current: {page}), stopping pagination");
                    state.finished = true;
                    state.next_pg = None;
                } else {
                    state.next_pg = Some(np);
                }
            } else {
                state.finished = true;
                state.next_pg = None;
            }
            debug!("immich: fetched page {page} ({count} items); cache={}", state.items.len());

            if state.finished {
                return Ok(());
            }
        }
    }
}

// ── DTO → PhotoMeta ──────────────────────────────────────────────────────────

fn to_photo_meta(a: ImmichAsset) -> PhotoMeta {
    let (mut w, mut h) = a.exif_info.as_ref()
        .map(|e| (e.exif_image_width.unwrap_or(0), e.exif_image_height.unwrap_or(0)))
        .unwrap_or((0, 0));

    if w == 0 || h == 0 {
        w = 1;
        h = 1;
    }

    let taken_at = a.exif_info.as_ref()
        .and_then(|e| e.date_time_original.as_deref())
        .or(a.file_created_at.as_deref())
        .and_then(parse_immich_timestamp);

    PhotoMeta {
        id:           a.id,
        filename:     a.original_filename.unwrap_or_else(|| "image.jpg".to_string()),
        width:        w,
        height:       h,
        taken_at,
        download_url: None,
        extra:        Default::default(),
    }
}

/// Parse Immich timestamps (ISO 8601 / RFC 3339).
fn parse_immich_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// ── PhotoPlugin impl ─────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for ImmichPlugin {
    fn name(&self)         -> &str { "immich" }
    fn display_name(&self) -> &str { "Immich" }
    fn version(&self)      -> &str { "0.1.0" }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        let skip_tls = self.cfg.values
            .get("skip_tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        self.client = ClientBuilder::new()
            .danger_accept_invalid_certs(skip_tls)
            .build()
            .context("building Immich HTTP client")?;

        let _ = self.base_url()?;
        let _ = self.api_key()?;

        // Probe server reachability — non-fatal warning on failure so the
        // engine can still retry later (e.g. flaky LAN at boot).
        let base = self.base_url()?;
        let ping_url = format!("{base}/api/server/ping");
        match self.client
            .get(&ping_url)
            .header("x-api-key", self.api_key()?)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                info!("immich: connected to {base}");
            }
            Ok(r) => warn!("immich: ping returned HTTP {}", r.status()),
            Err(e) => warn!("immich: ping failed: {e}"),
        }

        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus { AuthStatus::Authenticated }
    async fn authenticate(&mut self) -> Result<AuthStatus> { Ok(AuthStatus::Authenticated) }
    async fn refresh_auth(&mut self)  -> Result<()>        { Ok(()) }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let needed = offset.saturating_add(limit);
        self.ensure_cache(needed).await?;

        let state = self.cache.read().await;
        if state.items.is_empty() {
            warn!("immich: no image assets returned. Check api_key/album_id/url.");
            return Ok(vec![]);
        }
        Ok(state.items.iter().skip(offset).take(limit).cloned().collect())
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _dw: u32,
        _dh: u32,
    ) -> Result<Vec<u8>> {
        let base = self.base_url()?;
        let size = self.image_size();

        let url = if size == "original" {
            format!("{base}/api/assets/{}/original", meta.id)
        } else {
            format!("{base}/api/assets/{}/thumbnail?size={size}", meta.id)
        };

        let resp = self.client
            .get(&url)
            .header("x-api-key", self.api_key()?)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        let bytes = resp.bytes().await.context("reading Immich asset body")?;

        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "immich asset too large ({} MB): {}",
                bytes.len() / 1_048_576,
                meta.id
            ));
        }
        if !is_image_magic(&bytes) {
            return Err(anyhow::anyhow!(
                "immich asset {} did not return a recognised image", meta.id
            ));
        }

        Ok(bytes.to_vec())
    }
}

// ── Image magic ──────────────────────────────────────────────────────────────

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(values: serde_json::Value) -> PluginConfig {
        serde_json::from_value(values).unwrap()
    }

    #[test]
    fn page_size_defaults_when_unset() {
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k" })));
        assert_eq!(p.page_size(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn page_size_clamps_invalid() {
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k", "page_size": 0 })));
        assert_eq!(p.page_size(), DEFAULT_PAGE_SIZE);
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k", "page_size": 5000 })));
        assert_eq!(p.page_size(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn image_size_validates() {
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k", "image_size": "original" })));
        assert_eq!(p.image_size(), "original");
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k", "image_size": "bogus" })));
        assert_eq!(p.image_size(), "preview");
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x:2283/", "api_key": "k" })));
        assert_eq!(p.base_url().unwrap(), "http://x:2283");
    }

    #[test]
    fn album_id_blank_is_none() {
        let p = ImmichPlugin::new(cfg(json!({ "url": "http://x", "api_key": "k", "album_id": "  " })));
        assert!(p.album_id().is_none());
    }

    #[test]
    fn to_photo_meta_extracts_exif() {
        let asset = ImmichAsset {
            id: "abc".to_string(),
            original_filename: Some("IMG_0001.jpg".to_string()),
            asset_type: Some("IMAGE".to_string()),
            file_created_at: Some("2024-08-15T12:00:00Z".to_string()),
            exif_info: Some(ExifInfo {
                exif_image_width:  Some(4032),
                exif_image_height: Some(3024),
                date_time_original: Some("2024-08-15T11:59:30Z".to_string()),
            }),
        };
        let m = to_photo_meta(asset);
        assert_eq!(m.id, "abc");
        assert_eq!(m.filename, "IMG_0001.jpg");
        assert_eq!(m.width, 4032);
        assert_eq!(m.height, 3024);
        assert!(m.taken_at.is_some());
    }

    #[test]
    fn is_image_magic_signatures() {
        assert!(is_image_magic(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_image_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A]));
        assert!(is_image_magic(b"RIFF\x00\x00\x00\x00WEBP"));
        assert!(!is_image_magic(b"not an image"));
        assert!(!is_image_magic(b""));
    }
}
