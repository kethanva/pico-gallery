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
///   * filter by person/subject name (`people = ["Alice", "Bob"]`),
///     labels, keywords, multiple albums, colour, monochrome, panorama,
///     orientation (portrait/landscape/square), state/city, and a
///     date range (`after` / `before`)
///   * privacy guard: private and archived photos are excluded by default
///     (each independently opt-in) — safe for an always-on display
///   * photos only — video items are skipped; only JPEG stills are decoded
///   * "memories" mode: restrict the feed to photos taken on today's
///     calendar day across all years (server-side `month` + `day`)
///   * favourite / un-favourite the on-screen photo (`set_favorite`)
///   * album title resolution: the configured album slug/UID is resolved to
///     its human title for the on-screen OSD pill
///   * richer per-photo metadata (title, city/country) surfaced to the OSD
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
/// # albums      = ["trip", "family"] # OR several albums (album:trip|family)
/// # state       = "California"        # state / province
/// # city        = "Paris"            # city
/// # after       = "2020-06-01"        # date range (YYYY-MM-DD)
/// # before      = "2020-06-30"
/// # media_type  = "image"            # image | raw | live | animated | video
/// # color       = "blue"             # red|orange|gold|green|teal|blue|purple|pink|brown|white|grey|black
/// # mono        = true               # only monochrome
/// # panorama    = true               # only panoramas
/// # orientation = "portrait"         # portrait | landscape | square
/// # people      = ["Alice", "Bob"]   # only photos containing these subjects
/// # labels      = ["beach", "dog"]   # any of these labels
/// # keywords    = ["sunset"]         # any of these keywords
/// # memories    = true               # only photos taken on today's date (any year)
/// # query       = "label:beach keyword:sunset"   # raw PhotoPrism Q
///
/// # ── Privacy (excluded by default; each independently opt-in) ────────────
/// # include_private  = false
/// # include_archived = false
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
use chrono::Datelike;
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
const CONNECT_TIMEOUT_SECS: u64 = 10;

// PhotoPrism thumbnail size names, ordered smallest → largest.
// Each tuple = (server name, longest-edge pixels).
const THUMB_SIZES: &[(&str, u32)] = &[
    ("tile_500", 500),
    ("fit_720", 720),
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
    // Place fields are present in the merged /photos search result, so the
    // OSD gets location text without a second per-photo detail fetch.
    #[serde(default, rename = "PlaceCity")]
    place_city: String,
    #[serde(default, rename = "PlaceState")]
    place_state: String,
    #[serde(default, rename = "PlaceCountry")]
    place_country: String,
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
struct PpAlbum {
    #[serde(default, rename = "UID")]
    uid: String,
    #[serde(default, rename = "Slug")]
    slug: String,
    #[serde(default, rename = "Title")]
    title: String,
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

// ── Errors ────────────────────────────────────────────────────────────────────

/// Marker error for an invalidated session (HTTP 401). `populate_until`
/// detects it via the error chain's root cause, so wrapping it in
/// `.context(...)` layers does not break detection.
#[derive(Debug)]
struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session expired (HTTP 401)")
    }
}

impl std::error::Error for SessionExpired {}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct PhotoPrismPlugin {
    cfg: PluginConfig,
    client: Client,
    state: Mutex<State>,
}

#[derive(Default)]
struct Session {
    session_id: String,
    preview_token: String,
    download_token: String,
}

#[derive(Default)]
struct State {
    cached: Vec<PhotoMeta>,
    next_page: u32,
    exhausted: bool,
    session: Option<Session>,
    /// Maps album slug *and* UID → human title, fetched once when an `album`
    /// filter is configured. Used to show a real album name in the OSD.
    albums: HashMap<String, String>,
    /// True once the (lazy, one-shot) album fetch has been attempted, so a
    /// server with no albums or a fetch error doesn't retry every page.
    albums_loaded: bool,
}

impl PhotoPrismPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        // Fully configured client from the start — a bare Client::new() has no
        // timeout, and a stalled request would hang the single-threaded
        // executor. init() rebuilds it once config overrides are known.
        let client = Self::build_client(false, DEFAULT_TIMEOUT_SECS)
            .expect("building default PhotoPrism HTTP client");
        Self {
            cfg,
            client,
            state: Mutex::new(State::default()),
        }
    }

    /// Build the HTTP client. Called from `new()` with defaults and from
    /// `init()` with config overrides (skip_tls_verify, request_timeout_secs).
    fn build_client(skip_tls: bool, timeout_secs: u64) -> Result<Client> {
        ClientBuilder::new()
            .danger_accept_invalid_certs(skip_tls)
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(concat!(
                "picogallery-photoprism/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .context("building PhotoPrism HTTP client")
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

    fn username(&self) -> Option<&str> {
        self.cfg.get_str("username")
    }
    fn password(&self) -> Option<&str> {
        self.cfg.get_str("password")
    }
    fn app_password(&self) -> Option<&str> {
        self.cfg.get_str("app_password")
    }

    fn per_page(&self) -> u32 {
        self.cfg
            .values
            .get("per_page")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PER_PAGE as u64)
            .clamp(1, 1000) as u32
    }

    fn order(&self) -> &str {
        self.cfg.get_str("order").unwrap_or("newest")
    }

    fn allow_original(&self) -> bool {
        self.cfg
            .values
            .get("allow_original")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn max_thumb_cap(&self) -> u32 {
        let name = self.cfg.get_str("max_thumb").unwrap_or("fit_7680");
        THUMB_SIZES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, px)| *px)
            .unwrap_or(7680)
    }

    /// Read a boolean config flag, with a default when absent or wrong-typed.
    fn flag(&self, key: &str, default: bool) -> bool {
        self.cfg
            .values
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    /// Read a config value that may be either a single string or an array of
    /// strings (used for `people`, `labels`, `keywords`, `albums`).
    fn str_list(&self, key: &str) -> Vec<String> {
        match self.cfg.values.get(key) {
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Sanitize + OR-join a string list into a `a|b|c` value (PhotoPrism's OR
    /// syntax within one filter). Strips `"` and `|` from each entry so a
    /// config value can't inject extra terms. Returns None when empty.
    fn or_join(values: &[String]) -> Option<String> {
        let cleaned: Vec<String> = values
            .iter()
            .map(|v| v.replace(['"', '|'], "").trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.join("|"))
        }
    }

    /// Build the typed `/photos` search parameters from config.
    ///
    /// Each filter is sent as its own query parameter whose key matches a
    /// PhotoPrism `SearchPhotos` form field exactly (verified against the
    /// server source) — more robust than concatenating a `q=` mini-language
    /// string, which relies on a separate inline parser. The free-form `query`
    /// escape hatch is still sent as `q=` via `raw_query`.
    fn search_params(&self) -> Vec<(&'static str, String)> {
        let mut p: Vec<(&'static str, String)> = Vec::new();

        // ── Privacy guard ─────────────────────────────────────────────────────
        // A wall display must not surface private/archived photos. PhotoPrism
        // already hides archived results by default; we add it explicitly and
        // also require public unless opted in. Each is independently re-enablable.
        if !self.flag("include_private", false) {
            p.push(("public", "true".into()));
        }
        if !self.flag("include_archived", false) {
            p.push(("archived", "false".into()));
        }

        // ── Album(s) ──────────────────────────────────────────────────────────
        if let Some(a) = self.cfg.get_str("album") {
            p.push(("album", a.to_string()));
        }
        if let Some(v) = Self::or_join(&self.str_list("albums")) {
            p.push(("albums", v)); // any of several albums
        }

        // ── Boolean flags ─────────────────────────────────────────────────────
        if self.flag("favorites", false) {
            p.push(("favorite", "true".into()));
        }
        if self.flag("mono", false) {
            p.push(("mono", "true".into())); // black & white / monochrome
        }
        if self.flag("panorama", false) {
            p.push(("panorama", "true".into()));
        }

        // ── Orientation (handy for a rotated/portrait frame) ──────────────────
        match self.cfg.get_str("orientation") {
            Some("portrait") => p.push(("portrait", "true".into())),
            Some("landscape") => p.push(("landscape", "true".into())),
            Some("square") => p.push(("square", "true".into())),
            _ => {}
        }

        // ── Quality floor (1–5) ───────────────────────────────────────────────
        if let Some(q) = self.cfg.values.get("quality").and_then(|v| v.as_u64()) {
            p.push(("quality", q.to_string()));
        }

        // ── Colour ────────────────────────────────────────────────────────────
        if let Some(c) = self.cfg.get_str("color") {
            p.push(("color", c.to_string())); // red, blue, gold, …
        }

        // ── Geography ─────────────────────────────────────────────────────────
        if let Some(c) = self.cfg.get_str("country") {
            p.push(("country", c.to_string()));
        }
        if let Some(s) = self.cfg.get_str("state") {
            p.push(("state", s.to_string()));
        }
        if let Some(c) = self.cfg.get_str("city") {
            p.push(("city", c.to_string()));
        }

        // ── Time ──────────────────────────────────────────────────────────────
        if let Some(y) = self.cfg.values.get("year").and_then(|v| v.as_u64()) {
            p.push(("year", y.to_string()));
        }
        if let Some(a) = self.cfg.get_str("after") {
            p.push(("after", a.to_string())); // YYYY-MM-DD
        }
        if let Some(b) = self.cfg.get_str("before") {
            p.push(("before", b.to_string()));
        }

        // ── Type / labels / keywords / subjects ───────────────────────────────
        // Field names match the form tags exactly: `label`, `keywords`
        // (plural), `subject`.
        if let Some(t) = self.cfg.get_str("media_type") {
            p.push(("type", t.to_string()));
        }
        if let Some(v) = Self::or_join(&self.str_list("labels")) {
            p.push(("label", v)); // label=beach|dog — any of several labels
        }
        if let Some(v) = Self::or_join(&self.str_list("keywords")) {
            p.push(("keywords", v));
        }
        if let Some(v) = Self::or_join(&self.people()) {
            p.push(("subject", v)); // people by subject name (OR)
        }

        p
    }

    /// The raw `query =` escape hatch, sent verbatim as `q=`.
    fn raw_query(&self) -> Option<&str> {
        self.cfg.get_str("query")
    }

    /// Subject/person names from the `people` config key (string or array).
    fn people(&self) -> Vec<String> {
        self.str_list("people")
    }

    fn memories(&self) -> bool {
        self.cfg
            .values
            .get("memories")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Extra `/photos` query params for "memories" mode: restrict results to
    /// today's calendar month + day across every year. Empty when disabled.
    fn memories_params(&self) -> Vec<(&'static str, String)> {
        if !self.memories() {
            return Vec::new();
        }
        let now = chrono::Local::now();
        vec![
            ("month", now.month().to_string()),
            ("day", now.day().to_string()),
        ]
    }

    /// Resolve the configured album slug/UID to its human title (falling back
    /// to the raw config value when no album list match is found).
    fn resolve_album_title(&self, albums: &HashMap<String, String>) -> Option<String> {
        self.cfg.get_str("album").map(|cfg_album| {
            albums
                .get(cfg_album)
                .cloned()
                .unwrap_or_else(|| cfg_album.to_string())
        })
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
            let user = self
                .username()
                .ok_or_else(|| anyhow!("photoprism: `username` (or `app_password`) required"))?;
            let pass = self
                .password()
                .ok_or_else(|| anyhow!("photoprism: `password` (or `app_password`) required"))?;
            (user, pass)
        };

        let body = serde_json::json!({
            "username": user,
            "password": pass,
        });

        let resp = self
            .client
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
        let header_sid = resp
            .headers()
            .get("X-Session-ID")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let parsed: SessionResponse = resp
            .json()
            .await
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

        let preview_token = parsed
            .preview_token
            .or_else(|| parsed.config.as_ref().and_then(|c| c.preview_token.clone()))
            .unwrap_or_else(|| "public".to_string());
        let download_token = parsed
            .download_token
            .or_else(|| {
                parsed
                    .config
                    .as_ref()
                    .and_then(|c| c.download_token.clone())
            })
            .unwrap_or_else(|| "public".to_string());

        // chars() (not byte slicing) — a multibyte id must not panic the log line.
        let sid_prefix: String = sid.chars().take(8).collect();
        info!("PhotoPrism: logged in as {user} (session {sid_prefix}…)");

        Ok(Session {
            session_id: sid,
            preview_token,
            download_token,
        })
    }

    async fn ensure_session(&self, state: &mut State) -> Result<()> {
        if state.session.is_some() {
            return Ok(());
        }
        state.session = Some(self.login().await?);
        Ok(())
    }

    fn auth_headers(sess: &Session) -> header::HeaderMap {
        Self::session_headers(&sess.session_id)
    }

    /// Build the auth headers from a bare session id. Split out so callers that
    /// only hold the id (favourites, album fetch) don't need the whole Session.
    fn session_headers(sid: &str) -> header::HeaderMap {
        let mut h = header::HeaderMap::new();
        if let Ok(v) = header::HeaderValue::from_str(sid) {
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
        let offset = page.saturating_mul(per_page);
        let order = self.order().to_string();

        let mut params: Vec<(&'static str, String)> = vec![
            ("count", per_page.to_string()),
            ("offset", offset.to_string()),
            ("order", order),
            ("merged", "true".into()),
        ];
        // Typed filters, each bound to an exact PhotoPrism form field.
        params.extend(self.search_params());
        // "Memories": today's month + day across all years (server-side filter).
        params.extend(self.memories_params());
        // Free-form Q-language escape hatch, last so it can't be clobbered.
        if let Some(raw) = self.raw_query() {
            params.push(("q", raw.to_string()));
        }

        let resp = self
            .client
            .get(&url)
            .headers(Self::auth_headers(sess))
            .query(&params)
            .send()
            .await
            .with_context(|| format!("GET {url} (photos page={page})"))?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(anyhow::Error::new(SessionExpired)
                .context(format!("GET {url} (photos page={page})")));
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        let photos: Vec<PpPhoto> = resp.json().await.context("parsing /photos JSON")?;
        debug!("PhotoPrism: page {page} returned {} photos", photos.len());
        Ok(photos)
    }

    // ── API: GET /api/v1/albums ──────────────────────────────────────────────

    /// Fetch the album list once (only when an `album` filter is configured),
    /// building a slug→title and uid→title lookup for the OSD. Best-effort:
    /// a failure logs a warning and leaves the map empty so the raw config
    /// value is shown instead.
    async fn ensure_albums(&self, state: &mut State) {
        if state.albums_loaded {
            return;
        }
        state.albums_loaded = true; // attempt once, regardless of outcome
        if self.cfg.get_str("album").is_none() {
            return; // no album filter — nothing to resolve
        }
        let Some(sid) = state.session.as_ref().map(|s| s.session_id.clone()) else {
            return; // called before a session exists
        };
        match self.fetch_albums(&sid).await {
            Ok(map) => {
                debug!("PhotoPrism: loaded {} album title(s)", map.len() / 2);
                state.albums = map;
            }
            Err(e) => warn!("PhotoPrism: album list fetch failed: {e}"),
        }
    }

    async fn fetch_albums(&self, sid: &str) -> Result<HashMap<String, String>> {
        let url = self.api_url("/albums")?;
        let params = [("count", "1000"), ("offset", "0"), ("type", "album")];
        let resp = self
            .client
            .get(&url)
            .headers(Self::session_headers(sid))
            .query(&params)
            .send()
            .await
            .with_context(|| format!("GET {url} (albums)"))?
            .error_for_status()
            .with_context(|| format!("GET {url}: HTTP error"))?;

        let albums: Vec<PpAlbum> = resp.json().await.context("parsing /albums JSON")?;

        // Index by both slug and UID so the configured value matches either.
        let mut map = HashMap::with_capacity(albums.len() * 2);
        for a in albums {
            if a.title.is_empty() {
                continue;
            }
            if !a.slug.is_empty() {
                map.insert(a.slug, a.title.clone());
            }
            if !a.uid.is_empty() {
                map.insert(a.uid, a.title);
            }
        }
        Ok(map)
    }

    async fn populate_until(&self, state: &mut State, needed: usize) -> Result<()> {
        let mut reauth_attempts = 0u32;
        while !state.exhausted && state.cached.len() < needed {
            // Re-login transparently if the session was invalidated mid-run.
            self.ensure_session(state).await?;
            // One-shot album-title fetch (no-op unless an album filter is set
            // and it hasn't run yet). Done after ensure_session so a session
            // exists; cheap thereafter.
            self.ensure_albums(state).await;
            // Resolve the configured album to its human title once per page —
            // a String clone, so it doesn't borrow `state` across the push below.
            let album_title = self.resolve_album_title(&state.albums);
            let sess = state.session.as_ref().expect("session set above");

            let page = state.next_page;
            let photos = match self.fetch_page(sess, page).await {
                Ok(p) => p,
                Err(e) if e.root_cause().is::<SessionExpired>() => {
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
                if let Some(meta) = photo_to_meta(p, sess, album_title.as_deref()) {
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

fn photo_to_meta(p: PpPhoto, sess: &Session, album_title: Option<&str>) -> Option<PhotoMeta> {
    // Photos only — pick the primary still, falling back to the first non-video
    // file. Video items (no still file) are skipped entirely: this is a photo
    // frame and a Pi Zero can't decode video. Live Photos keep their JPEG
    // primary, so they still show as stills.
    let file = p
        .files
        .iter()
        .find(|f| f.primary && !f.video)
        .or_else(|| p.files.iter().find(|f| !f.video))?;

    if file.hash.is_empty() {
        return None;
    }

    let filename = if !p.file_name.is_empty() {
        p.file_name
            .split('/')
            .next_back()
            .unwrap_or(&p.file_name)
            .to_string()
    } else if !p.original_name.is_empty() {
        p.original_name
    } else if !p.title.is_empty() {
        format!("{}.jpg", p.title)
    } else {
        format!("{}.jpg", p.name)
    };

    let taken_at = p
        .taken_at
        .as_deref()
        .or(p.taken_at_local.as_deref())
        .and_then(parse_pp_date);

    let mut extra: HashMap<String, String> = HashMap::new();
    extra.insert("hash".into(), file.hash.clone());
    extra.insert("uid".into(), p.uid.clone());
    extra.insert("preview_token".into(), sess.preview_token.clone());
    extra.insert("download_token".into(), sess.download_token.clone());
    extra.insert("media_type".into(), p.media_type);
    if p.favorite {
        extra.insert("favorite".into(), "true".into());
    }

    // Human album title (resolved from the configured slug/UID) for the OSD.
    if let Some(title) = album_title {
        if !title.is_empty() {
            extra.insert("album".into(), title.to_string());
        }
    }
    // Per-photo title — only when it adds information beyond the filename.
    if !p.title.is_empty() && p.title != filename {
        extra.insert("title".into(), p.title);
    }
    // "City, Country" location line. PhotoPrism uses "Unknown" / "zz" as
    // placeholders for unresolved places — drop those so the OSD stays clean.
    if let Some(loc) = format_location(&p.place_city, &p.place_state, &p.place_country) {
        extra.insert("location".into(), loc);
    }

    let width = if file.width > 0 { file.width } else { p.width };
    let height = if file.height > 0 {
        file.height
    } else {
        p.height
    };

    Some(PhotoMeta {
        id: if !p.uid.is_empty() {
            p.uid
        } else {
            file.hash.clone()
        },
        filename,
        width,
        height,
        taken_at,
        download_url: None, // resolved at fetch time using hash + token
        extra,
    })
}

/// Build a short "City, Country" (or "City, State") location string from the
/// PhotoPrism place fields, skipping empty and placeholder values. Returns
/// `None` when nothing usable remains.
fn format_location(city: &str, state: &str, country: &str) -> Option<String> {
    fn usable(s: &str) -> Option<&str> {
        let t = s.trim();
        // PhotoPrism placeholders for an unresolved place.
        if t.is_empty() || t.eq_ignore_ascii_case("unknown") || t.eq_ignore_ascii_case("zz") {
            None
        } else {
            Some(t)
        }
    }
    let primary = usable(city);
    // Prefer country as the second part; fall back to state when no country.
    let secondary = usable(country).or_else(|| usable(state));
    let parts: Vec<&str> = [primary, secondary].into_iter().flatten().collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
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
    THUMB_SIZES
        .iter()
        .rfind(|(_, px)| *px <= cap_px)
        .map(|(n, _)| *n)
        .unwrap_or("fit_1920")
}

/// JPEG-only: this frame decodes nothing else. PhotoPrism thumbnails are
/// always JPEG, so a non-JPEG body means a misconfigured request or an HTML
/// error page — reject it.
fn is_image_magic(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, 0xD8, 0xFF, ..])
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for PhotoPrismPlugin {
    fn name(&self) -> &str {
        "photoprism"
    }
    fn display_name(&self) -> &str {
        "PhotoPrism"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        let skip_tls = self
            .cfg
            .values
            .get("skip_tls_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = self
            .cfg
            .values
            .get("request_timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        self.client = Self::build_client(skip_tls, timeout)?;

        // Validate base URL up-front so misconfiguration fails fast.
        let _ = self.base_url()?;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        // Await the lock — try_lock would misreport NotAuthenticated whenever
        // another task briefly holds the state mutex.
        if self.state.lock().await.session.is_some() {
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
        state.session = None;
        state.cached = Vec::new();
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

    async fn get_photo_bytes(&self, meta: &PhotoMeta, dw: u32, dh: u32) -> Result<Vec<u8>> {
        let hash = meta
            .extra
            .get("hash")
            .ok_or_else(|| anyhow!("photoprism: meta missing `hash` for '{}'", meta.filename))?;
        let preview_token = meta
            .extra
            .get("preview_token")
            .map(String::as_str)
            .unwrap_or("public");
        let download_token = meta
            .extra
            .get("download_token")
            .map(String::as_str)
            .unwrap_or("public");

        let cap = self.max_thumb_cap();
        let size = pick_thumb_size(dw, dh, cap);

        // Decide: thumbnail or original?
        // If the largest allowed thumb is still smaller than the display, and
        // the user permits originals, fetch the original instead.
        let largest_thumb_px = THUMB_SIZES
            .iter()
            .filter(|(_, px)| *px <= cap)
            .map(|(_, px)| *px)
            .max()
            .unwrap_or(0);
        let need_original = self.allow_original()
            && dw.max(dh) > largest_thumb_px
            && meta.width.max(meta.height) > largest_thumb_px;

        // Attach session headers if we have one (some PhotoPrism deployments
        // require auth even for token-signed URLs).
        let headers = {
            let state = self.state.lock().await;
            state
                .session
                .as_ref()
                .map(Self::auth_headers)
                .unwrap_or_default()
        };

        // The download token rides in the query string so reqwest encodes it.
        // The preview token is path-embedded; PhotoPrism issues alphanumeric
        // tokens, so reject anything else rather than percent-encode (avoids a
        // new dependency, and a hostile value can't smuggle path segments).
        let request = if need_original {
            self.client
                .get(self.api_url(&format!("/dl/{hash}"))?)
                .query(&[("t", download_token)])
        } else {
            if !preview_token.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(anyhow!(
                    "photoprism: preview token contains unexpected characters"
                ));
            }
            self.client
                .get(self.api_url(&format!("/t/{hash}/{preview_token}/{size}"))?)
        };

        // Errors and logs carry the file hash, never the token-bearing URL.
        let resp = request
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("photoprism: fetching image (hash {hash})"))?
            .error_for_status()
            .with_context(|| format!("photoprism: HTTP error fetching image (hash {hash})"))?;

        if let Some(len) = resp.content_length() {
            if len > MAX_IMAGE_BYTES {
                return Err(anyhow!(
                    "photoprism: image too large ({} MB, hash {hash})",
                    len / 1_048_576
                ));
            }
        }

        let bytes = resp
            .bytes()
            .await
            .context("reading PhotoPrism image body")?;

        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(anyhow!(
                "photoprism: image too large ({} MB, hash {hash})",
                bytes.len() / 1_048_576
            ));
        }
        if !is_image_magic(&bytes) {
            warn!(
                "PhotoPrism: response for hash {hash} ({} bytes) is not a recognised image format",
                bytes.len()
            );
            return Err(anyhow!(
                "photoprism: not a recognised image format (hash {hash}, {} bytes)",
                bytes.len()
            ));
        }
        Ok(bytes.to_vec())
    }

    async fn set_favorite(&self, meta: &PhotoMeta, favorite: bool) -> Result<()> {
        let uid = meta
            .extra
            .get("uid")
            .filter(|u| !u.is_empty())
            .ok_or_else(|| anyhow!("photoprism: photo has no UID; cannot set favourite"))?;
        // The UID is path-embedded; PhotoPrism issues alphanumeric UIDs, so
        // reject anything else rather than smuggle path segments into the URL.
        if !uid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(anyhow!("photoprism: UID contains unexpected characters"));
        }
        let url = self.api_url(&format!("/photos/{uid}/like"))?;

        let mut state = self.state.lock().await;
        let mut attempts = 0u32;
        loop {
            self.ensure_session(&mut state).await?;
            let sid = state
                .session
                .as_ref()
                .expect("session set above")
                .session_id
                .clone();

            // POST /like favourites; DELETE /like clears it.
            let req = if favorite {
                self.client.post(&url)
            } else {
                self.client.delete(&url)
            };
            let resp = req
                .headers(Self::session_headers(&sid))
                .send()
                .await
                .with_context(|| format!("photoprism: updating favourite (uid {uid})"))?;

            if resp.status() == StatusCode::UNAUTHORIZED {
                attempts += 1;
                if attempts > 2 {
                    return Err(anyhow!(
                        "photoprism: favourite failed after re-auth (uid {uid})"
                    ));
                }
                state.session = None;
                continue;
            }
            resp.error_for_status()
                .with_context(|| format!("photoprism: HTTP error setting favourite (uid {uid})"))?;
            info!(
                "PhotoPrism: {} favourite for uid {uid}",
                if favorite { "set" } else { "cleared" }
            );
            return Ok(());
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Best-effort logout — DELETE /api/v1/session/{id} — ignore errors.
        let state = self.state.lock().await;
        if let Some(sess) = state.session.as_ref() {
            if let Ok(url) = self.api_url(&format!("/session/{}", sess.session_id)) {
                let _ = self
                    .client
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
        assert_eq!(pick_thumb_size(800, 600, 7680), "fit_1280");
        assert_eq!(pick_thumb_size(1920, 1080, 7680), "fit_1920");
        assert_eq!(pick_thumb_size(400, 300, 7680), "tile_500");
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
        // JPEG only — PNG and others are now rejected.
        assert!(!is_image_magic(&[0x89, b'P', b'N', b'G', 0x0D]));
        assert!(!is_image_magic(b"<html>"));
        assert!(!is_image_magic(b""));
    }

    fn sess() -> Session {
        Session {
            session_id: "sid123".into(),
            preview_token: "ptok".into(),
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
            place_city: "Paris".into(),
            place_state: "Île-de-France".into(),
            place_country: "France".into(),
            width: 4000,
            height: 3000,
            taken_at: Some("2024-01-15T18:30:00Z".into()),
            taken_at_local: None,
            media_type: "image".into(),
            favorite: true,
            files: vec![
                PpFile {
                    hash: "vidhash".into(),
                    primary: false,
                    width: 1920,
                    height: 1080,
                    video: true,
                },
                PpFile {
                    hash: "primaryhash".into(),
                    primary: true,
                    width: 4000,
                    height: 3000,
                    video: false,
                },
            ],
        };
        let m = photo_to_meta(p, &sess(), Some("January 2024")).unwrap();
        assert_eq!(m.id, "uid42");
        assert_eq!(m.filename, "IMG_42.jpg");
        assert_eq!(m.width, 4000);
        assert_eq!(m.extra.get("hash").unwrap(), "primaryhash");
        assert_eq!(m.extra.get("preview_token").unwrap(), "ptok");
        assert_eq!(m.extra.get("favorite").unwrap(), "true");
        assert_eq!(m.extra.get("album").unwrap(), "January 2024");
        assert_eq!(m.extra.get("title").unwrap(), "Sunset");
        assert_eq!(m.extra.get("location").unwrap(), "Paris, France");
        assert!(m.taken_at.is_some());
    }

    #[test]
    fn photo_to_meta_skips_video_only() {
        let p = PpPhoto {
            uid: "u".into(),
            file_name: "v.mp4".into(),
            name: "".into(),
            original_name: "".into(),
            title: "".into(),
            place_city: "".into(),
            place_state: "".into(),
            place_country: "".into(),
            width: 0,
            height: 0,
            taken_at: None,
            taken_at_local: None,
            media_type: "video".into(),
            favorite: false,
            files: vec![PpFile {
                hash: "vh".into(),
                primary: true,
                width: 0,
                height: 0,
                video: true,
            }],
        };
        // Video-only photos are always skipped — this is a photo frame.
        assert!(photo_to_meta(p, &sess(), None).is_none());
    }

    /// Look up a single search-param value by key.
    fn pval<'a>(params: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    fn params_for(values: &[(&str, serde_json::Value)]) -> Vec<(&'static str, String)> {
        let cfg = PluginConfig {
            values: values
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        };
        PhotoPrismPlugin::new(cfg).search_params()
    }

    #[test]
    fn search_params_maps_typed_filters_to_form_fields() {
        let p = params_for(&[
            ("album", serde_json::json!("january-2024")),
            ("favorites", serde_json::json!(true)),
            ("quality", serde_json::json!(3)),
            ("year", serde_json::json!(2024)),
        ]);
        assert_eq!(pval(&p, "album"), Some("january-2024"));
        assert_eq!(pval(&p, "favorite"), Some("true"));
        assert_eq!(pval(&p, "quality"), Some("3"));
        assert_eq!(pval(&p, "year"), Some("2024"));
    }

    #[test]
    fn raw_query_is_passed_through_verbatim() {
        let cfg = PluginConfig {
            values: [(
                "query".to_string(),
                serde_json::json!("label:beach color:gold"),
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            PhotoPrismPlugin::new(cfg).raw_query(),
            Some("label:beach color:gold")
        );
    }

    #[test]
    fn search_params_applies_privacy_guard_by_default() {
        // A bare config still excludes private + archived photos.
        let p = PhotoPrismPlugin::new(PluginConfig::default()).search_params();
        assert_eq!(pval(&p, "public"), Some("true"));
        assert_eq!(pval(&p, "archived"), Some("false"));
    }

    #[test]
    fn search_params_privacy_guard_can_be_opted_out() {
        let p = params_for(&[
            ("include_private", serde_json::json!(true)),
            ("include_archived", serde_json::json!(true)),
        ]);
        assert_eq!(pval(&p, "public"), None);
        assert_eq!(pval(&p, "archived"), None);
    }

    #[test]
    fn search_params_color_mono_panorama_orientation() {
        let p = params_for(&[
            ("color", serde_json::json!("blue")),
            ("mono", serde_json::json!(true)),
            ("panorama", serde_json::json!(true)),
            ("orientation", serde_json::json!("portrait")),
        ]);
        assert_eq!(pval(&p, "color"), Some("blue"));
        assert_eq!(pval(&p, "mono"), Some("true"));
        assert_eq!(pval(&p, "panorama"), Some("true"));
        assert_eq!(pval(&p, "portrait"), Some("true"));
    }

    #[test]
    fn search_params_geo_and_date_range() {
        let p = params_for(&[
            ("state", serde_json::json!("California")),
            ("city", serde_json::json!("San Francisco")),
            ("after", serde_json::json!("2020-01-01")),
            ("before", serde_json::json!("2020-12-31")),
        ]);
        assert_eq!(pval(&p, "state"), Some("California"));
        assert_eq!(pval(&p, "city"), Some("San Francisco"));
        assert_eq!(pval(&p, "after"), Some("2020-01-01"));
        assert_eq!(pval(&p, "before"), Some("2020-12-31"));
    }

    #[test]
    fn search_params_labels_keywords_albums_use_correct_fields() {
        let p = params_for(&[
            ("labels", serde_json::json!(["beach", "dog"])),
            ("keywords", serde_json::json!("sunset")),
            ("albums", serde_json::json!(["trip", "family"])),
        ]);
        assert_eq!(pval(&p, "label"), Some("beach|dog"));
        // `keywords` is the exact PhotoPrism form field (plural), not `keyword`.
        assert_eq!(pval(&p, "keywords"), Some("sunset"));
        assert_eq!(pval(&p, "albums"), Some("trip|family"));
    }

    #[test]
    fn or_join_sanitizes_and_handles_empty() {
        assert_eq!(
            PhotoPrismPlugin::or_join(&["a\"|b".to_string(), "c".to_string()]).as_deref(),
            Some("ab|c")
        );
        assert_eq!(PhotoPrismPlugin::or_join(&[]), None);
        assert_eq!(PhotoPrismPlugin::or_join(&["   ".to_string()]), None);
    }

    #[test]
    fn search_params_baseline_is_just_privacy_guard() {
        let p = PhotoPrismPlugin::new(PluginConfig::default()).search_params();
        assert_eq!(
            p,
            vec![
                ("public", "true".to_string()),
                ("archived", "false".to_string()),
            ]
        );
    }

    #[test]
    fn search_params_people_become_subject_or_value() {
        let p = params_for(&[("people", serde_json::json!(["Alice", "Bob Smith"]))]);
        assert_eq!(pval(&p, "subject"), Some("Alice|Bob Smith"));
    }

    #[test]
    fn search_params_people_single_string() {
        let p = params_for(&[("people", serde_json::json!("Alice"))]);
        assert_eq!(pval(&p, "subject"), Some("Alice"));
    }

    #[test]
    fn search_params_people_strips_embedded_quotes_and_pipes() {
        let p = params_for(&[("people", serde_json::json!("Al\"ice|Bob"))]);
        // Quotes + pipes stripped from a single entry so it can't inject an OR.
        assert_eq!(pval(&p, "subject"), Some("AliceBob"));
    }

    #[test]
    fn memories_params_present_only_when_enabled() {
        let off = PhotoPrismPlugin::new(PluginConfig::default());
        assert!(off.memories_params().is_empty());

        let cfg = PluginConfig {
            values: [("memories".to_string(), serde_json::json!(true))]
                .into_iter()
                .collect(),
        };
        let on = PhotoPrismPlugin::new(cfg);
        let params = on.memories_params();
        assert_eq!(params.len(), 2);
        let keys: Vec<&str> = params.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"month"));
        assert!(keys.contains(&"day"));
    }

    #[test]
    fn resolve_album_title_uses_lookup_then_falls_back() {
        let cfg = PluginConfig {
            values: [("album".to_string(), serde_json::json!("jan-2024"))]
                .into_iter()
                .collect(),
        };
        let p = PhotoPrismPlugin::new(cfg);

        let mut albums = HashMap::new();
        albums.insert("jan-2024".to_string(), "January 2024".to_string());
        assert_eq!(
            p.resolve_album_title(&albums).as_deref(),
            Some("January 2024")
        );

        // No match → raw config value is shown.
        assert_eq!(
            p.resolve_album_title(&HashMap::new()).as_deref(),
            Some("jan-2024")
        );
    }

    #[test]
    fn resolve_album_title_none_without_album_config() {
        let p = PhotoPrismPlugin::new(PluginConfig::default());
        assert!(p.resolve_album_title(&HashMap::new()).is_none());
    }

    #[test]
    fn format_location_composes_city_country() {
        assert_eq!(
            format_location("Paris", "", "France").as_deref(),
            Some("Paris, France")
        );
    }

    #[test]
    fn format_location_falls_back_to_state_without_country() {
        assert_eq!(
            format_location("Austin", "Texas", "").as_deref(),
            Some("Austin, Texas")
        );
    }

    #[test]
    fn format_location_skips_placeholders() {
        assert_eq!(format_location("Unknown", "", "zz"), None);
        assert_eq!(format_location("", "", ""), None);
        assert_eq!(
            format_location("Tokyo", "Unknown", "Unknown").as_deref(),
            Some("Tokyo")
        );
    }
}
