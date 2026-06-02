/// PhotoPrism plugin for PicoGallery.
///
/// Talks to a PhotoPrism server (https://www.photoprism.app) — typically another
/// Raspberry Pi (4 / 5) on the LAN running PhotoPrism — via its REST API at
/// `/api/v1`. A single session is opened at startup with username + password
/// (or an app password), and the returned session ID + preview/download tokens
/// are reused for all subsequent calls.
///
/// Photos are streamed on demand. The plugin picks the smallest pre-generated
/// PhotoPrism thumbnail size that is ≥ the display dimensions, which keeps
/// bandwidth and Pi Zero decode RAM low. Falls back to the original via the
/// `/dl/{hash}` endpoint when no thumbnail is large enough.
///
/// Supported PhotoPrism features exposed through plugin config:
///
///   * username / password OR app password authentication
///   * session reuse with automatic re-login on 401
///   * arbitrary search query via PhotoPrism's `q=` Q-language
///     (`album:`, `keyword:`, `label:`, `country:`, `year:`, `favorite:true`,
///     `panorama:true`, `video:false`, `quality:3` …)
///   * filter by album UID (uid or slug), favourites only, quality floor,
///     country, year, type (image/raw/live/animated/panorama)
///   * server-side ordering: newest, oldest, name, similar, random, …
///   * thumbnail size auto-selection (tile_500 … fit_7680)
///   * skip TLS verify for self-signed LAN certificates
///
/// Config keys (in `[[plugins]]`):
///
/// ```toml
/// [[plugins]]
/// name     = "photoprism"
/// enabled  = true
/// url      = "http://photoprism.local:2342"   # base URL, no trailing /api
/// username = "admin"                          # or use app_password below
/// password = "insecure"
/// # app_password = "abcd-efgh-ijkl-mnop"     # PhotoPrism v0.10+ app password
///
/// # ── Filtering ──────────────────────────────────────────────────────────
/// # album       = "january-2024"     # album UID or slug
/// # favorites   = true               # only favourites
/// # quality     = 3                  # 1=low … 5=excellent (drops lower)
/// # country     = "fr"               # ISO country code
/// # year        = 2024
/// # media_type  = "image"            # image | raw | live | animated | video
/// # query       = "label:beach keyword:sunset"   # raw PhotoPrism Q
///
/// # ── Ordering / paging ─────────────────────────────────────────────────
/// # order       = "newest"           # newest | oldest | added | name | random | similar
/// # per_page    = 100
///
/// # ── Thumbnail selection ───────────────────────────────────────────────
/// # max_thumb   = "fit_1920"         # cap requested size; saves Pi Zero RAM
/// # allow_original = true            # fall back to original if no thumb fits
///
/// # ── Transport ─────────────────────────────────────────────────────────
/// # skip_tls_verify = false
/// # request_timeout_secs = 30
/// ```
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use log::{debug, info, warn};
use reqwest::{header, Client, ClientBuilder, StatusCode};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_PER_PAGE: u32 = 100;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

// PhotoPrism thumbnail size names, ordered smallest → largest.
// Each tuple = (server name, longest-edge pixels).
const THUMB_SIZES: &[(&str, u32)] = &[
    ("tile_500",  500),
    ("fit_720",   720),
    ("fit_1280", 1280),
    ("fit_1920", 1920),
    ("fit_2048", 2048),
    ("fit_2560", 2560),
    ("fit_3840", 3840),
    ("fit_4096", 4096),
    ("fit_7680", 7680),
];

// ── PhotoPrism API response shapes ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SessionResponse {
    #[serde(default, alias = "id", alias = "session_id")]
    session_id: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    preview_token: Option<String>,
    #[serde(default)]
    download_token: Option<String>,
    #[serde(default)]
    config: Option<ServerConfig>,
}

#[derive(Debug, Deserialize)]
struct ServerConfig {
    #[serde(default)]
    preview_token: Option<String>,
    #[serde(default)]
    download_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PpPhoto {
    #[serde(default, rename = "UID")]
    uid: String,
    #[serde(default, rename = "FileName")]
    file_name: String,
    #[serde(default, rename = "Name")]
    name: String,
    #[serde(default, rename = "OriginalName")]
    original_name: String,
    #[serde(default, rename = "Title")]
    title: String,
    #[serde(default, rename = "Width")]
    width: u32,
    #[serde(default, rename = "Height")]
    height: u32,
    #[serde(default, rename = "TakenAt")]
    taken_at: Option<String>,
    #[serde(default, rename = "TakenAtLocal")]
    taken_at_local: Option<String>,
    #[serde(default, rename = "Type")]
    media_type: String,
    #[serde(default, rename = "Favorite")]
    favorite: bool,
    #[serde(default, rename = "Files")]
    files: Vec<PpFile>,
}

#[derive(Debug, Deserialize)]
struct PpFile {
    #[serde(default, rename = "Hash")]
    hash: String,
    #[serde(default, rename = "Primary")]
    primary: bool,
    #[serde(default, rename = "Width")]
    width: u32,
    #[serde(default, rename = "Height")]
    height: u32,
    #[serde(default, rename = "Video")]
    video: bool,
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct PhotoPrismPlugin {
    cfg:    PluginConfig,
    client: Client,
    state:  Mutex<State>,
}

#[derive(Default)]
struct Session {
    session_id:     String,
    preview_token:  String,
    download_token: String,
}

#[derive(Default)]
struct State {
    cached:    Vec<PhotoMeta>,
    next_page: u32,
    exhausted: bool,
    session:   Option<Session>,
}

impl PhotoPrismPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            client: Client::new(),
            state:  Mutex::new(State::default()),
        }
    }

    // ── Config helpers ─────────────────────────────────────────────────────

    fn base_url(&self) -> Result<String> {
        self.cfg
            .require_str("url")
            .map(|s| s.trim_end_matches('/').to_string())
    }

    fn api_url(&self, path: &str) -> Result<String> {
        Ok(format!("{}/api/v1{path}", self.base_url()?))
    }

    fn username(&self)     -> Option<&str> { self.cfg.get_str("username") }
    fn password(&self)     -> Option<&str> { self.cfg.get_str("password") }
    fn app_password(&self) -> Option<&str> { self.cfg.get_str("app_password") }

    fn per_page(&self) -> u32 {
        self.cfg.values.get("per_page")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PER_PAGE as u64)
            .clamp(1, 1000) as u32
    }

    fn order(&self) -> &str {
        self.cfg.get_str("order").unwrap_or("newest")
    }

    fn allow_original(&self) -> bool {
        self.cfg.values.get("allow_original")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn max_thumb_cap(&self) -> u32 {
        let name = self.cfg.get_str("max_thumb").unwrap_or("fit_7680");
        THUMB_SIZES.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, px)| *px)
            .unwrap_or(7680)
    }

    /// Build the server-side search query from typed config fields plus an
    /// optional raw `query =` string. Plain-key fields are translated to
    /// PhotoPrism's `key:value` Q-syntax.
    fn build_query(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(album) = self.cfg.get_str("album") {
            parts.push(format!("album:{album}"));
        }
        if self.cfg.values.get("favorites").and_then(|v| v.as_bool()).unwrap_or(false) {
            parts.push("favorite:true".into());
        }
        if let Some(q) = self.cfg.values.get("quality").and_then(|v| v.as_u64()) {
            parts.push(format!("quality:{q}"));
        }
        if let Some(c) = self.cfg.get_str("country") {
            parts.push(format!("country:{c}"));
        }
        if let Some(y) = self.cfg.values.get("year").and_then(|v| v.as_u64()) {
            parts.push(format!("year:{y}"));
        }
        if let Some(t) = self.cfg.get_str("media_type") {
            parts.push(format!("type:{t}"));
        }
        if let Some(raw) = self.cfg.get_str("query") {
            parts.push(raw.to_string());
        }
        parts.join(" ")
    }

    // ── Auth ───────────────────────────────────────────────────────────────

    /// POST /api/v1/session — open a fresh session and capture the tokens
    /// needed for thumbnail and download URLs.
    async fn login(&self) -> Result<Session> {
        let url = self.api_url("/session")?;

        // App password takes precedence (PhotoPrism v0.10+): sent as the
        // password with an empty/admin username.
        let (user, pass) = if let Some(app) = self.app_password() {
            (self.username().unwrap_or("admin"), app)
        } else {
            let user = self.username()
                .ok_or_else(|| anyhow!("photoprism: `username` (or `app_password`) required"))?;
            let pass = self.password()
                .ok_or_else(|| anyhow!("photoprism: `password` (or `app_password`) required"))?;
            (user, pass)
        };

        let body = serde_json::json!({
            "username": user,
            "password": pass,
        });

        let resp = self.client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url} (login)"))?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "photoprism: login failed (HTTP {}): check username / password",
                resp.status()
            ));
        }

        // PhotoPrism returns the session id both in the JSON body and the
        // `X-Session-ID` header; grab whichever is present.
        let header_sid = resp.headers()
            .get("X-Session-ID")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let parsed: SessionResponse = resp.json().await
            .context("parsing PhotoPrism session JSON")?;

        let sid = if !parsed.session_id.is_empty() {
            parsed.session_id
        } else if let Some(at) = parsed.access_token {
            at
        } else if let Some(h) = header_sid {
            h
        } else {
            return Err(anyhow!("photoprism: session response missing session id"));
        };

        let preview_token  = parsed.preview_token
            .or_else(|| parsed.config.as_ref().and_then(|c| c.preview_token.clone()))
            .unwrap_or_else(|| "public".to_string());
        let download_token = parsed.download_token
            .or_else(|| parsed.config.as_ref().and_then(|c| c.download_token.clone()))
            .unwrap_or_else(|| "public".to_string());

        info!("PhotoPrism: logged in as {user} (session {}…)", &sid[..sid.len().min(8)]);

        Ok(Session { session_id: sid, preview_token, download_token })
    }

    async fn ensure_session(&self, state: &mut State) -> Result<()> {
        if state.session.is_some() {
            return Ok(());
        }
        state.session = Some(self.login().await?);
        Ok(())
    }

    fn auth_headers(sess: &Session) -> header::HeaderMap {
        let mut h = header::HeaderMap::new();
        if let Ok(v) = header::HeaderValue::from_str(&sess.session_id) {
            h.insert("X-Session-ID", v.clone());
            // Older PhotoPrism builds expect X-Auth-Token; newer ones accept
            // either. Sending both is harmless.
            h.insert("X-Auth-Token", v);
        }
        h
    }

    // ── API: GET /api/v1/photos ────────────────────────────────────────────

    async fn fetch_page(&self, sess: &Session, page: u32) -> Result<Vec<PpPhoto>> {
        let url = self.api_url("/photos")?;
        let per_page = self.per_page();
        let offset   = page.saturating_mul(per_page);
        let query    = self.build_query();
        let order    = self.order().to_string();

        let mut params: Vec<(&str, String)> = vec![
            ("count",   per_page.to_string()),
            ("offset",  offset.to_string()),
            ("order",   order),
            ("merged",  "true".into()),
        ];
        if !query.is_empty() {
            params.push(("q", query));
        }

        let resp = self.client
            .get(&url)
            .headers(Self::auth_headers(sess))
            .query(&params)
            .send()
            .await
            .with_context(|| format!("GET {url} (photos page={page})"))?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("photoprism: session expired (HTTP 401)"));
        }
        let resp = resp.error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        let photos: Vec<PpPhoto> = resp.json().await
            .context("parsing /photos JSON")?;
        debug!("PhotoPrism: page {page} returned {} photos", photos.len());
        Ok(photos)
    }

    async fn populate_until(&self, state: &mut State, needed: usize) -> Result<()> {
        let mut reauth_attempts = 0u32;
        while !state.exhausted && state.cached.len() < needed {
            // Re-login transparently if the session was invalidated mid-run.
            self.ensure_session(state).await?;
            let sess = state.session.as_ref().expect("session set above");

            let page = state.next_page;
            let photos = match self.fetch_page(sess, page).await {
                Ok(p) => p,
                Err(e) if e.to_string().contains("session expired") => {
                    reauth_attempts += 1;
                    if reauth_attempts > 3 {
                        return Err(anyhow!("photoprism: repeated 401 after {reauth_attempts} re-auth attempts — check credentials"));
                    }
                    warn!("PhotoPrism: re-authenticating after 401 (attempt {reauth_attempts})");
                    state.session = None;
                    continue;
                }
                Err(e) => return Err(e),
            };

            let returned = photos.len() as u32;
            for p in photos {
                if let Some(meta) = photo_to_meta(p, sess) {
                    state.cached.push(meta);
                }
            }

            if returned == 0 || returned < self.per_page() {
                state.exhausted = true;
            }
            state.next_page = page + 1;
        }
        Ok(())
    }
}

// ── Conversion: PpPhoto → PhotoMeta ───────────────────────────────────────────

fn photo_to_meta(p: PpPhoto, sess: &Session) -> Option<PhotoMeta> {
    // Pick the primary file; fall back to the first non-video file.
    let file = p.files.iter()
        .find(|f| f.primary && !f.video)
        .or_else(|| p.files.iter().find(|f| !f.video))?;

    if file.hash.is_empty() {
        return None;
    }

    let filename = if !p.file_name.is_empty() {
        p.file_name.split('/').next_back().unwrap_or(&p.file_name).to_string()
    } else if !p.original_name.is_empty() {
        p.original_name
    } else if !p.title.is_empty() {
        format!("{}.jpg", p.title)
    } else {
        format!("{}.jpg", p.name)
    };

    let taken_at = p.taken_at.as_deref()
        .or(p.taken_at_local.as_deref())
        .and_then(parse_pp_date);

    let mut extra: HashMap<String, String> = HashMap::new();
    extra.insert("hash".into(),           file.hash.clone());
    extra.insert("uid".into(),            p.uid.clone());
    extra.insert("preview_token".into(),  sess.preview_token.clone());
    extra.insert("download_token".into(), sess.download_token.clone());
    extra.insert("media_type".into(),     p.media_type);
    if p.favorite {
        extra.insert("favorite".into(), "true".into());
    }

    let width  = if file.width  > 0 { file.width  } else { p.width  };
    let height = if file.height > 0 { file.height } else { p.height };

    Some(PhotoMeta {
        id: if !p.uid.is_empty() { p.uid } else { file.hash.clone() },
        filename,
        width,
        height,
        taken_at,
        download_url: None, // resolved at fetch time using hash + token
        extra,
    })
}

/// PhotoPrism returns ISO-8601 timestamps. Some endpoints include the trailing
/// `Z`, some use a `+00:00` offset. Try the most common shapes.
fn parse_pp_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDateTime};
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|ndt| ndt.and_utc())
}

/// Pick the smallest pre-generated PhotoPrism thumbnail size whose longest
/// edge is ≥ `max(dw, dh)`, clamped by `max_thumb` from config.
fn pick_thumb_size(dw: u32, dh: u32, cap_px: u32) -> &'static str {
    let target = dw.max(dh);
    for (name, px) in THUMB_SIZES {
        if *px >= target && *px <= cap_px {
            return name;
        }
    }
    // Nothing in-range — use the largest allowed size.
    THUMB_SIZES.iter()
        .rfind(|(_, px)| *px <= cap_px)
        .map(|(n, _)| *n)
        .unwrap_or("fit_1920")
}

fn is_image_magic(bytes: &[u8]) -> bool {
    matches!(bytes,
        [0xFF, 0xD8, 0xFF, ..] |
        [0x89, b'P', b'N', b'G', ..] |
        [b'G', b'I', b'F', b'8', ..] |
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] |
        [b'B', b'M', ..]
    )
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for PhotoPrismPlugin {
    fn name(&self)         -> &str { "photoprism" }
    fn display_name(&self) -> &str { "PhotoPrism" }
    fn version(&self)      -> &str { "0.1.0" }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        let skip_tls = self.cfg.values
            .get("skip_tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = self.cfg.values
            .get("request_timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        self.client = ClientBuilder::new()
            .danger_accept_invalid_certs(skip_tls)
            .timeout(Duration::from_secs(timeout))
            .user_agent(concat!("picogallery-photoprism/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building PhotoPrism HTTP client")?;

        // Validate base URL up-front so misconfiguration fails fast.
        let _ = self.base_url()?;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        if self.state.try_lock().map(|s| s.session.is_some()).unwrap_or(false) {
            AuthStatus::Authenticated
        } else {
            AuthStatus::NotAuthenticated
        }
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        let mut state = self.state.lock().await;
        self.ensure_session(&mut state).await?;
        Ok(AuthStatus::Authenticated)
    }

    async fn refresh_auth(&mut self) -> Result<()> {
        // Drop session AND cache: preview_token / download_token are embedded
        // in every cached PhotoMeta::extra, so they'd be stale after re-login.
        let mut state = self.state.lock().await;
        state.session   = None;
        state.cached    = Vec::new();
        state.next_page = 0;
        state.exhausted = false;
        Ok(())
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        let mut state = self.state.lock().await;
        self.populate_until(&mut state, offset + limit).await?;

        if offset >= state.cached.len() {
            return Ok(vec![]);
        }
        let end = (offset + limit).min(state.cached.len());
        Ok(state.cached[offset..end].to_vec())
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        dw: u32,
        dh: u32,
    ) -> Result<Vec<u8>> {
        let hash = meta.extra.get("hash")
            .ok_or_else(|| anyhow!("photoprism: meta missing `hash` for '{}'", meta.filename))?;
        let preview_token = meta.extra.get("preview_token")
            .map(String::as_str)
            .unwrap_or("public");
        let download_token = meta.extra.get("download_token")
            .map(String::as_str)
            .unwrap_or("public");

        let cap = self.max_thumb_cap();
        let size = pick_thumb_size(dw, dh, cap);

        // Decide: thumbnail or original?
        // If the largest allowed thumb is still smaller than the display, and
        // the user permits originals, fetch the original instead.
        let largest_thumb_px = THUMB_SIZES.iter()
            .filter(|(_, px)| *px <= cap)
            .map(|(_, px)| *px)
            .max()
            .unwrap_or(0);
        let need_original = self.allow_original()
            && dw.max(dh) > largest_thumb_px
            && meta.width.max(meta.height) > largest_thumb_px;

        let url = if need_original {
            self.api_url(&format!("/dl/{hash}?t={download_token}"))?
        } else {
            self.api_url(&format!("/t/{hash}/{preview_token}/{size}"))?
        };

        // Attach session headers if we have one (some PhotoPrism deployments
        // require auth even for token-signed URLs).
        let headers = {
            let state = self.state.lock().await;
            state.session.as_ref().map(Self::auth_headers).unwrap_or_default()
        };

        let resp = self.client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        if let Some(len) = resp.content_length() {
            if len > MAX_IMAGE_BYTES {
                return Err(anyhow!(
                    "photoprism: image too large ({} MB): {url}",
                    len / 1_048_576
                ));
            }
        }

        let bytes = resp.bytes().await.context("reading PhotoPrism image body")?;

        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(anyhow!(
                "photoprism: image too large ({} MB): {url}",
                bytes.len() / 1_048_576
            ));
        }
        if !is_image_magic(&bytes) {
            warn!("PhotoPrism: response from {url} is not a recognised image format");
            return Err(anyhow!("photoprism: not a recognised image format: {url}"));
        }
        Ok(bytes.to_vec())
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Best-effort logout — DELETE /api/v1/session/{id} — ignore errors.
        let state = self.state.lock().await;
        if let Some(sess) = state.session.as_ref() {
            if let Ok(url) = self.api_url(&format!("/session/{}", sess.session_id)) {
                let _ = self.client
                    .delete(&url)
                    .headers(Self::auth_headers(sess))
                    .send()
                    .await;
            }
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_thumb_size_returns_smallest_above_target() {
        assert_eq!(pick_thumb_size(800,  600, 7680), "fit_1280");
        assert_eq!(pick_thumb_size(1920, 1080, 7680), "fit_1920");
        assert_eq!(pick_thumb_size(400,  300, 7680), "tile_500");
    }

    #[test]
    fn pick_thumb_size_respects_cap() {
        // Cap at fit_1920 even when display is bigger.
        assert_eq!(pick_thumb_size(3840, 2160, 1920), "fit_1920");
    }

    #[test]
    fn pick_thumb_size_picks_largest_when_target_exceeds_all() {
        assert_eq!(pick_thumb_size(9000, 6000, 7680), "fit_7680");
    }

    #[test]
    fn parse_pp_date_rfc3339() {
        let dt = parse_pp_date("2024-01-15T18:30:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-15T18:30:00+00:00");
    }

    #[test]
    fn parse_pp_date_naive() {
        let dt = parse_pp_date("2024-01-15T18:30:00").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-01-15T18:30:00+00:00");
    }

    #[test]
    fn parse_pp_date_invalid_returns_none() {
        assert!(parse_pp_date("not-a-date").is_none());
        assert!(parse_pp_date("").is_none());
    }

    #[test]
    fn is_image_magic_signatures() {
        assert!(is_image_magic(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_image_magic(&[0x89, b'P', b'N', b'G', 0x0D]));
        assert!(!is_image_magic(b"<html>"));
        assert!(!is_image_magic(b""));
    }

    fn sess() -> Session {
        Session {
            session_id:     "sid123".into(),
            preview_token:  "ptok".into(),
            download_token: "dtok".into(),
        }
    }

    #[test]
    fn photo_to_meta_picks_primary_file() {
        let p = PpPhoto {
            uid: "uid42".into(),
            file_name: "2024/01/IMG_42.jpg".into(),
            name: "IMG_42".into(),
            original_name: "IMG_42.jpg".into(),
            title: "Sunset".into(),
            width: 4000, height: 3000,
            taken_at: Some("2024-01-15T18:30:00Z".into()),
            taken_at_local: None,
            media_type: "image".into(),
            favorite: true,
            files: vec![
                PpFile { hash: "vidhash".into(), primary: false, width: 1920, height: 1080, video: true },
                PpFile { hash: "primaryhash".into(), primary: true, width: 4000, height: 3000, video: false },
            ],
        };
        let m = photo_to_meta(p, &sess()).unwrap();
        assert_eq!(m.id, "uid42");
        assert_eq!(m.filename, "IMG_42.jpg");
        assert_eq!(m.width, 4000);
        assert_eq!(m.extra.get("hash").unwrap(), "primaryhash");
        assert_eq!(m.extra.get("preview_token").unwrap(), "ptok");
        assert_eq!(m.extra.get("favorite").unwrap(), "true");
        assert!(m.taken_at.is_some());
    }

    #[test]
    fn photo_to_meta_skips_video_only() {
        let p = PpPhoto {
            uid: "u".into(), file_name: "v.mp4".into(),
            name: "".into(), original_name: "".into(), title: "".into(),
            width: 0, height: 0, taken_at: None, taken_at_local: None,
            media_type: "video".into(), favorite: false,
            files: vec![PpFile { hash: "vh".into(), primary: true, width: 0, height: 0, video: true }],
        };
        assert!(photo_to_meta(p, &sess()).is_none());
    }

    #[test]
    fn build_query_assembles_pp_q_syntax() {
        let cfg = PluginConfig {
            values: [
                ("album".to_string(),     serde_json::json!("january-2024")),
                ("favorites".to_string(), serde_json::json!(true)),
                ("quality".to_string(),   serde_json::json!(3)),
                ("year".to_string(),      serde_json::json!(2024)),
                ("query".to_string(),     serde_json::json!("label:beach")),
            ].into_iter().collect(),
        };
        let p = PhotoPrismPlugin::new(cfg);
        let q = p.build_query();
        assert!(q.contains("album:january-2024"));
        assert!(q.contains("favorite:true"));
        assert!(q.contains("quality:3"));
        assert!(q.contains("year:2024"));
        assert!(q.contains("label:beach"));
    }

    #[test]
    fn build_query_empty_when_no_filters() {
        let p = PhotoPrismPlugin::new(PluginConfig::default());
        assert_eq!(p.build_query(), "");
    }
}
