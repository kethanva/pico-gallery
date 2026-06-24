/// Amazon Photos plugin for PicoGallery.
///
/// Authentication: Login with Amazon (LWA) device flow — same concept as
/// Google's device flow, no browser on the Pi required.
///
/// Amazon Photos API root:  https://drive.amazonaws.com/drive/v1
///
/// Config keys:
///   client_id     — LWA client ID
///   client_secret — LWA client secret
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const TOKEN_FILE: &str = "amazon-photos-token.json";
const LWA_AUTH_URL: &str = "https://api.amazon.com/auth/o2/create/codepair";
const LWA_TOKEN_URL: &str = "https://api.amazon.com/auth/o2/token";
const DRIVE_API: &str = "https://drive.amazonaws.com/drive/v1";
/// Reject images larger than this before buffering into memory (Pi Zero RAM guard).
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
/// HTTP timeouts — a stalled request would otherwise hang the
/// single-threaded executor indefinitely.
const REQUEST_TIMEOUT_SECS: u64 = 30;
const CONNECT_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: DateTime<Utc>,
}

impl StoredToken {
    fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at - chrono::Duration::minutes(5)
    }
}

#[derive(Deserialize)]
struct LwaDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

#[derive(Deserialize)]
struct LwaToken {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct NodeList {
    data: Vec<Node>,
    /// Opaque continuation token. Unused until token-based paging is wired up.
    #[allow(dead_code)]
    #[serde(rename = "nextToken")]
    next_token: Option<String>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    name: String,
    #[serde(rename = "contentProperties")]
    content: Option<ContentProps>,
}

#[derive(Deserialize)]
struct ContentProps {
    #[allow(dead_code)]
    size: Option<u64>,
    image: Option<ImageInfo>,
}

#[derive(Deserialize)]
struct ImageInfo {
    width: Option<u32>,
    height: Option<u32>,
    #[serde(rename = "dateTimeOriginal")]
    taken: Option<String>,
}

/// In-flight LWA device-code grant. Kept so repeated `authenticate()` calls
/// poll the token endpoint instead of requesting a fresh code each time.
struct PendingAuth {
    device_code: String,
    message: String,
    interval: u64,
}

pub struct AmazonPhotosPlugin {
    cfg: PluginConfig,
    client: reqwest::Client,
    token: Option<StoredToken>,
    token_dir: PathBuf,
    pending: Option<PendingAuth>,
}

impl AmazonPhotosPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .build()
            .expect("building Amazon Photos HTTP client");
        Self {
            cfg,
            client,
            token: None,
            token_dir: dirs::config_dir().unwrap_or_default().join("picogallery"),
            pending: None,
        }
    }

    fn client_id(&self) -> Result<&str> {
        self.cfg.require_str("client_id")
    }
    fn client_secret(&self) -> Result<&str> {
        self.cfg.require_str("client_secret")
    }
    fn token_path(&self) -> PathBuf {
        self.token_dir.join(TOKEN_FILE)
    }

    /// Persist the token so auth survives restarts. Failure is non-fatal:
    /// callers log a warning and keep using the in-memory token.
    async fn save_token(&self, t: &StoredToken) -> Result<()> {
        let path = self.token_path();
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("token path has no parent dir: {}", path.display()))?;
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating token dir {}", dir.display()))?;
        let data = serde_json::to_vec(t).context("serializing token")?;
        fs::write(&path, data)
            .await
            .with_context(|| format!("writing token file {}", path.display()))?;
        // Restrict token file to owner-only — contains refresh token.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("setting permissions on {}", path.display()))?;
        }
        Ok(())
    }

    async fn load_token(&mut self) {
        if let Ok(data) = fs::read(self.token_path()).await {
            if let Ok(t) = serde_json::from_slice::<StoredToken>(&data) {
                self.token = Some(t);
            }
        }
    }

    async fn refresh_token_now(&mut self) -> Result<()> {
        let rt = self
            .token
            .as_ref()
            .and_then(|t| t.refresh_token.clone())
            .context("no refresh token")?;

        let res = self
            .client
            .post(LWA_TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &rt),
                ("client_id", self.client_id()?),
                ("client_secret", self.client_secret()?),
            ])
            .send()
            .await?
            .json::<LwaToken>()
            .await?;

        if let Some(at) = res.access_token {
            let token = StoredToken {
                access_token: at,
                refresh_token: res.refresh_token.or(Some(rt)),
                expires_at: Utc::now()
                    + chrono::Duration::seconds(res.expires_in.unwrap_or(3600) as i64),
            };
            if let Err(e) = self.save_token(&token).await {
                warn!("Amazon Photos: failed to persist refreshed token (auth still valid this session): {e:#}");
            }
            self.token = Some(token);
        }
        Ok(())
    }

    fn access_token(&self) -> Result<&str> {
        self.token
            .as_ref()
            .map(|t| t.access_token.as_str())
            .ok_or_else(|| anyhow::anyhow!("amazon-photos: not authenticated"))
    }
}

#[async_trait]
impl PhotoPlugin for AmazonPhotosPlugin {
    fn name(&self) -> &str {
        "amazon-photos"
    }
    fn display_name(&self) -> &str {
        "Amazon Photos"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }

    async fn init(&mut self, _config: &PluginConfig) -> Result<()> {
        self.load_token().await;
        Ok(())
    }

    async fn auth_status(&self) -> AuthStatus {
        match &self.token {
            None => AuthStatus::NotAuthenticated,
            // Any expired token reports NotAuthenticated so the engine calls
            // authenticate(), which performs the refresh when a refresh token
            // exists. Reporting Authenticated here would mean every API call
            // 401s until something else triggers a refresh.
            Some(t) if t.is_expired() => AuthStatus::NotAuthenticated,
            Some(_) => AuthStatus::Authenticated,
        }
    }

    async fn authenticate(&mut self) -> Result<AuthStatus> {
        if let Some(t) = &self.token {
            if !t.is_expired() {
                return Ok(AuthStatus::Authenticated);
            }
            if t.refresh_token.is_some() {
                self.refresh_token_now().await?;
                return Ok(AuthStatus::Authenticated);
            }
        }

        // A device-code grant is already in flight — poll the token endpoint
        // instead of requesting a new code (which would invalidate the code
        // the user is currently typing in).
        if let Some(pending) = self.pending.take() {
            let res = self
                .client
                .post(LWA_TOKEN_URL)
                .form(&[
                    ("grant_type", "device_code"),
                    ("device_code", pending.device_code.as_str()),
                    ("client_id", self.client_id()?),
                    ("client_secret", self.client_secret()?),
                ])
                .send()
                .await?
                .json::<LwaToken>()
                .await?;

            if let Some(at) = res.access_token {
                let t = StoredToken {
                    access_token: at,
                    refresh_token: res.refresh_token,
                    expires_at: Utc::now()
                        + chrono::Duration::seconds(res.expires_in.unwrap_or(3600) as i64),
                };
                if let Err(e) = self.save_token(&t).await {
                    warn!("Amazon Photos: failed to persist token (auth still valid this session): {e:#}");
                }
                self.token = Some(t);
                info!("Amazon Photos: device authorized!");
                return Ok(AuthStatus::Authenticated);
            }

            match res.error.as_deref() {
                Some("authorization_pending") => {
                    let message = pending.message.clone();
                    let interval = pending.interval;
                    self.pending = Some(pending);
                    return Ok(AuthStatus::PendingUserAction {
                        message,
                        poll_interval_secs: interval,
                    });
                }
                Some("slow_down") => {
                    // LWA asks us to back off — add 5 s as the spec requires.
                    let message = pending.message.clone();
                    let interval = pending.interval + 5;
                    self.pending = Some(PendingAuth {
                        interval,
                        ..pending
                    });
                    return Ok(AuthStatus::PendingUserAction {
                        message,
                        poll_interval_secs: interval,
                    });
                }
                other => {
                    // expired_token / access_denied / unknown — start over below.
                    warn!(
                        "Amazon Photos: device code no longer valid ({}) — requesting a new one",
                        other.unwrap_or("no error field")
                    );
                }
            }
        }

        // LWA device code request.
        let res = self
            .client
            .post(LWA_AUTH_URL)
            .form(&[
                ("response_type", "device_code"),
                ("client_id", self.client_id()?),
                ("scope", "profile:amazon_photos"),
            ])
            .send()
            .await?
            .json::<LwaDeviceCode>()
            .await?;

        let message = format!(
            "Open this URL on any device:\n\n  {}\n\nEnter code: {}\n\n(expires in {} seconds)",
            res.verification_uri, res.user_code, res.expires_in
        );
        self.pending = Some(PendingAuth {
            device_code: res.device_code,
            message: message.clone(),
            interval: res.interval,
        });

        Ok(AuthStatus::PendingUserAction {
            message,
            poll_interval_secs: res.interval,
        })
    }

    async fn refresh_auth(&mut self) -> Result<()> {
        if let Some(t) = &self.token {
            if t.is_expired() && t.refresh_token.is_some() {
                self.refresh_token_now().await?;
            }
        }
        Ok(())
    }

    /// Emulates offset paging over the Drive API's token-based paging by
    /// over-fetching a single page. NOTE: this silently caps the library at
    /// `API_MAX_LIMIT` (200) photos total — offsets at or past the cap return
    /// an empty page so the engine stops paging.
    async fn list_photos(&self, limit: usize, offset: usize) -> Result<Vec<PhotoMeta>> {
        // The Drive API pages with an opaque nextToken, not a numeric offset.
        // To honour `offset` statelessly, request `offset + limit` items in one
        // call (capped at the API maximum of 200) and skip the first `offset`.
        // Past the cap we report exhaustion so the engine stops paging instead
        // of receiving the same first page forever.
        const API_MAX_LIMIT: usize = 200;
        let want = offset.saturating_add(limit).min(API_MAX_LIMIT);
        if offset >= want {
            return Ok(vec![]);
        }

        let url = format!(
            "{}/nodes?filters=kind:PHOTOS&limit={}&asset=ALL&tempLink=true",
            DRIVE_API, want
        );

        let res = self
            .client
            .get(&url)
            .bearer_auth(self.access_token()?)
            .send()
            .await?
            .error_for_status()?
            .json::<NodeList>()
            .await?;

        let photos = res
            .data
            .into_iter()
            .skip(offset)
            .map(|node| {
                let (w, h, taken) = node
                    .content
                    .as_ref()
                    .and_then(|c| c.image.as_ref())
                    .map(|img| {
                        (
                            img.width.unwrap_or(0),
                            img.height.unwrap_or(0),
                            img.taken
                                .as_deref()
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                .map(|d| d.with_timezone(&Utc)),
                        )
                    })
                    .unwrap_or((0, 0, None));

                PhotoMeta {
                    id: node.id.clone(),
                    filename: node.name,
                    width: w,
                    height: h,
                    taken_at: taken,
                    download_url: Some(format!("{}/nodes/{}/content", DRIVE_API, node.id)),
                    extra: Default::default(),
                }
            })
            .collect();

        Ok(photos)
    }

    async fn get_photo_bytes(&self, meta: &PhotoMeta, _dw: u32, _dh: u32) -> Result<Vec<u8>> {
        let url = meta
            .download_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no URL for {}", meta.id))?;

        let resp = self
            .client
            .get(url)
            .bearer_auth(self.access_token()?)
            .send()
            .await?
            .error_for_status()?;

        // Size guard before buffering the body — protects Pi Zero RAM.
        if let Some(len) = resp.content_length() {
            if len > MAX_IMAGE_BYTES {
                return Err(anyhow::anyhow!(
                    "image too large ({} MB): {}",
                    len / 1_048_576,
                    meta.filename
                ));
            }
        }

        let bytes = resp.bytes().await?.to_vec();
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(anyhow::anyhow!(
                "image too large ({} MB): {}",
                bytes.len() / 1_048_576,
                meta.filename
            ));
        }

        debug!("Fetched {} bytes for {}", bytes.len(), meta.filename);
        Ok(bytes)
    }
}
