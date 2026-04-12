/// Google Photos plugin for PicoGallery.
///
/// Authentication uses the OAuth 2.0 Device Authorization Grant
/// (RFC 8628).  This is the right flow for a headless Pi: the user
/// is shown a short URL and code, visits it on their phone or PC,
/// approves, and the Pi polls until the token arrives.
///
/// Required config keys:
///   client_id     — OAuth2 client ID (TV/Limited-Input type)
///   client_secret — OAuth2 client secret
///   album_id      — (optional) restrict to a single album
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::fs;

// Re-export the plugin trait from the main crate.
use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

// ── Token storage ─────────────────────────────────────────────────────────────

const TOKEN_FILE: &str = "google-photos-token.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token:  String,
    refresh_token: Option<String>,
    expires_at:    DateTime<Utc>,
}

impl StoredToken {
    fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at - chrono::Duration::minutes(5)
    }
}

// ── Google API types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code:      String,
    user_code:        String,
    verification_url: String,
    interval:         u64,
    expires_in:       u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token:  String,
    token_type:    String,
    expires_in:    Option<u64>,
    refresh_token: Option<String>,
    #[serde(default)]
    error:         Option<String>,
}

#[derive(Deserialize)]
struct MediaItemsResponse {
    #[serde(rename = "mediaItems", default)]
    media_items:    Vec<GMediaItem>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GMediaItem {
    id:       String,
    filename: String,
    #[serde(rename = "mediaMetadata")]
    metadata: GMediaMetadata,
    #[serde(rename = "baseUrl")]
    base_url: String,
}

#[derive(Deserialize)]
struct GMediaMetadata {
    width:            Option<String>,
    height:           Option<String>,
    #[serde(rename = "creationTime")]
    creation_time:    Option<String>,
}

// ── Plugin struct ─────────────────────────────────────────────────────────────

pub struct GooglePhotosPlugin {
    cfg:           PluginConfig,
    client:        reqwest::Client,
    token:         Option<StoredToken>,
    token_dir:     PathBuf,
    device_code:   Option<String>,
    poll_interval: u64,
}

impl GooglePhotosPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            client:        reqwest::Client::new(),
            token:         None,
            token_dir:     dirs::config_dir()
                               .unwrap_or_default()
                               .join("picogallery"),
            device_code:   None,
            poll_interval: 5,
        }
    }

    // ── Config helpers ────────────────────────────────────────────────────

    fn client_id(&self)     -> Result<&str> { self.cfg.require_str("client_id") }
    fn client_secret(&self) -> Result<&str> { self.cfg.require_str("client_secret") }
    fn album_id(&self)      -> Option<&str> { self.cfg.get_str("album_id") }

    // ── Token persistence ─────────────────────────────────────────────────

    fn token_path(&self) -> PathBuf { self.token_dir.join(TOKEN_FILE) }

    async fn save_token(&self, t: &StoredToken) {
        let p = self.token_path();
        let _ = fs::create_dir_all(p.parent().unwrap()).await;
        let _ = fs::write(&p, serde_json::to_vec(t).unwrap_or_default()).await;
    }

    async fn load_token(&mut self) {
        let p = self.token_path();
        if let Ok(data) = fs::read(&p).await {
            if let Ok(t) = serde_json::from_slice::<StoredToken>(&data) {
                self.token = Some(t);
            }
        }
    }

    // ── OAuth2 device flow ────────────────────────────────────────────────

    async fn request_device_code(&mut self) -> Result<DeviceCodeResponse> {
        let res = self.client
            .post("https://oauth2.googleapis.com/device/code")
            .form(&[
                ("client_id", self.client_id()?),
                ("scope",     "https://www.googleapis.com/auth/photoslibrary.readonly"),
            ])
            .send().await?
            .error_for_status()?
            .json::<DeviceCodeResponse>().await?;
        Ok(res)
    }

    async fn poll_token(&self, device_code: &str) -> Result<Option<StoredToken>> {
        let res = self.client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id",     self.client_id()?),
                ("client_secret", self.client_secret()?),
                ("device_code",   device_code),
                ("grant_type",    "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send().await?
            .json::<TokenResponse>().await?;

        if let Some(err) = &res.error {
            if err == "authorization_pending" || err == "slow_down" {
                return Ok(None); // not yet — keep polling
            }
            anyhow::bail!("Token error: {}", err);
        }

        let expires_in = res.expires_in.unwrap_or(3600);
        let token = StoredToken {
            access_token:  res.access_token,
            refresh_token: res.refresh_token,
            expires_at:    Utc::now() + chrono::Duration::seconds(expires_in as i64),
        };
        Ok(Some(token))
    }

    async fn refresh_token_now(&mut self) -> Result<()> {
        let refresh_token = match &self.token {
            Some(t) => t.refresh_token.clone().context("no refresh token")?,
            None    => anyhow::bail!("not authenticated"),
        };

        let res = self.client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id",     self.client_id()?),
                ("client_secret", self.client_secret()?),
                ("refresh_token", &refresh_token as &str),
                ("grant_type",    "refresh_token"),
            ])
            .send().await?
            .json::<TokenResponse>().await?;

        let expires_in = res.expires_in.unwrap_or(3600);
        let token = StoredToken {
            access_token:  res.access_token,
            refresh_token: Some(refresh_token),
            expires_at:    Utc::now() + chrono::Duration::seconds(expires_in as i64),
        };
        self.save_token(&token).await;
        self.token = Some(token);
        Ok(())
    }

    fn access_token(&self) -> Result<&str> {
        self.token.as_ref()
            .map(|t| t.access_token.as_str())
            .ok_or_else(|| anyhow::anyhow!("not authenticated"))
    }

    // ── Photos API ────────────────────────────────────────────────────────

    async fn list_page(&self, page_token: Option<&str>, page_size: usize) -> Result<MediaItemsResponse> {
        let token = self.access_token()?;

        let url = match self.album_id() {
            Some(album) => {
                // Album-scoped search.
                let body = serde_json::json!({
                    "albumId":   album,
                    "pageSize":  page_size,
                    "pageToken": page_token.unwrap_or(""),
                });
                let res = self.client
                    .post("https://photoslibrary.googleapis.com/v1/mediaItems:search")
                    .bearer_auth(token)
                    .json(&body)
                    .send().await?
                    .error_for_status()?
                    .json::<MediaItemsResponse>().await?;
                return Ok(res);
            }
            None => format!(
                "https://photoslibrary.googleapis.com/v1/mediaItems?pageSize={}&pageToken={}",
                page_size, page_token.unwrap_or("")
            ),
        };

        let res = self.client
            .get(&url)
            .bearer_auth(token)
            .send().await?
            .error_for_status()?
            .json::<MediaItemsResponse>().await?;
        Ok(res)
    }
}

// ── PhotoPlugin impl ──────────────────────────────────────────────────────────

#[async_trait]
impl PhotoPlugin for GooglePhotosPlugin {
    fn name(&self)         -> &str { "google-photos"  }
    fn display_name(&self) -> &str { "Google Photos"  }
    fn version(&self)      -> &str { "0.1.0"          }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        self.load_token().await;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        match &self.token {
            None    => AuthStatus::NotAuthenticated,
            Some(t) if t.is_expired() && t.refresh_token.is_none() => AuthStatus::NotAuthenticated,
            Some(_) => AuthStatus::Authenticated,
        }
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        // Already authenticated?
        if let Some(t) = &self.token {
            if !t.is_expired() { return Ok(AuthStatus::Authenticated); }
            if t.refresh_token.is_some() {
                self.refresh_token_now().await?;
                return Ok(AuthStatus::Authenticated);
            }
        }

        // Start the device flow.
        let dc = self.request_device_code().await?;
        self.device_code   = Some(dc.device_code.clone());
        self.poll_interval = dc.interval;

        let message = format!(
            "Open this URL on any device:\n\n  {}\n\nEnter code: {}\n\n(expires in {} seconds)",
            dc.verification_url, dc.user_code, dc.expires_in
        );

        Ok(AuthStatus::PendingUserAction {
            message,
            poll_interval_secs: dc.interval,
        })
    }

    async fn refresh_auth(&mut self) -> Result<()> {
        // Called from main loop: check token expiry and try to refresh.
        if let Some(t) = &self.token {
            if t.is_expired() { self.refresh_token_now().await?; }
        }

        // Also poll device code if we're in the middle of auth.
        if let Some(code) = self.device_code.clone() {
            if let Ok(Some(token)) = self.poll_token(&code).await {
                info!("Google Photos: device authorized!");
                self.save_token(&token).await;
                self.token       = Some(token);
                self.device_code = None;
            }
        }
        Ok(())
    }

    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        // Google Photos API doesn't support arbitrary offsets; we page through
        // sequentially and track the page token internally.
        // For simplicity, we fetch one full page of `limit` items.
        // A production implementation would cache page tokens.
        let response = self.list_page(None, limit).await?;

        let photos: Vec<PhotoMeta> = response.media_items.into_iter().map(|item| {
            let w = item.metadata.width.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
            let h = item.metadata.height.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
            let taken_at = item.metadata.creation_time.as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            PhotoMeta {
                id:           item.id,
                filename:     item.filename,
                width:        w,
                height:       h,
                taken_at,
                download_url: Some(item.base_url),
                extra:        Default::default(),
            }
        }).collect();

        Ok(photos)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        display_width: u32,
        display_height: u32,
    ) -> Result<Vec<u8>> {
        // Google Photos base URL + `=w{W}-h{H}` suffix requests closest CDN size.
        let base = meta.download_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("no download URL for {}", meta.id))?;
        let url = format!("{}=w{}-h{}", base, display_width * 2, display_height * 2);

        let bytes = self.client
            .get(&url)
            .bearer_auth(self.access_token()?)
            .send().await?
            .error_for_status()?
            .bytes().await?
            .to_vec();

        debug!("Fetched {} bytes for {}", bytes.len(), meta.filename);
        Ok(bytes)
    }
}
