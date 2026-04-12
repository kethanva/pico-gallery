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
use tokio::fs;

use picogallery_core::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};

const TOKEN_FILE: &str = "amazon-photos-token.json";
const LWA_AUTH_URL: &str = "https://api.amazon.com/auth/o2/create/codepair";
const LWA_TOKEN_URL: &str = "https://api.amazon.com/auth/o2/token";
const DRIVE_API: &str = "https://drive.amazonaws.com/drive/v1";

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

#[derive(Deserialize)]
struct LwaDeviceCode {
    device_code: String,
    user_code:   String,
    verification_uri: String,
    interval:    u64,
    expires_in:  u64,
}

#[derive(Deserialize)]
struct LwaToken {
    access_token:  Option<String>,
    refresh_token: Option<String>,
    expires_in:    Option<u64>,
    error:         Option<String>,
}

#[derive(Deserialize)]
struct NodeList {
    data: Vec<Node>,
    #[serde(rename = "nextToken")]
    next_token: Option<String>,
}

#[derive(Deserialize)]
struct Node {
    id:          String,
    name:        String,
    #[serde(rename = "contentProperties")]
    content:     Option<ContentProps>,
}

#[derive(Deserialize)]
struct ContentProps {
    size: Option<u64>,
    image: Option<ImageInfo>,
}

#[derive(Deserialize)]
struct ImageInfo {
    width:  Option<u32>,
    height: Option<u32>,
    #[serde(rename = "dateTimeOriginal")]
    taken:  Option<String>,
}

pub struct AmazonPhotosPlugin {
    cfg:      PluginConfig,
    client:   reqwest::Client,
    token:    Option<StoredToken>,
    token_dir: PathBuf,
    device_code: Option<String>,
    poll_interval: u64,
}

impl AmazonPhotosPlugin {
    pub fn new(cfg: PluginConfig) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
            token: None,
            token_dir: dirs::config_dir().unwrap_or_default().join("picogallery"),
            device_code: None,
            poll_interval: 5,
        }
    }

    fn client_id(&self)     -> Result<&str> { self.cfg.require_str("client_id") }
    fn client_secret(&self) -> Result<&str> { self.cfg.require_str("client_secret") }
    fn token_path(&self) -> PathBuf { self.token_dir.join(TOKEN_FILE) }

    async fn save_token(&self, t: &StoredToken) {
        let _ = fs::create_dir_all(self.token_path().parent().unwrap()).await;
        let _ = fs::write(self.token_path(), serde_json::to_vec(t).unwrap_or_default()).await;
    }

    async fn load_token(&mut self) {
        if let Ok(data) = fs::read(self.token_path()).await {
            if let Ok(t) = serde_json::from_slice::<StoredToken>(&data) {
                self.token = Some(t);
            }
        }
    }

    async fn refresh_token_now(&mut self) -> Result<()> {
        let rt = self.token.as_ref()
            .and_then(|t| t.refresh_token.clone())
            .context("no refresh token")?;

        let res = self.client
            .post(LWA_TOKEN_URL)
            .form(&[
                ("grant_type",    "refresh_token"),
                ("refresh_token", &rt),
                ("client_id",     self.client_id()?),
                ("client_secret", self.client_secret()?),
            ])
            .send().await?
            .json::<LwaToken>().await?;

        if let Some(at) = res.access_token {
            let token = StoredToken {
                access_token:  at,
                refresh_token: res.refresh_token.or(Some(rt)),
                expires_at: Utc::now() + chrono::Duration::seconds(res.expires_in.unwrap_or(3600) as i64),
            };
            self.save_token(&token).await;
            self.token = Some(token);
        }
        Ok(())
    }

    fn access_token(&self) -> Result<&str> {
        self.token.as_ref()
            .map(|t| t.access_token.as_str())
            .ok_or_else(|| anyhow::anyhow!("amazon-photos: not authenticated"))
    }
}

#[async_trait]
impl PhotoPlugin for AmazonPhotosPlugin {
    fn name(&self)         -> &str { "amazon-photos"  }
    fn display_name(&self) -> &str { "Amazon Photos"  }
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
        if let Some(t) = &self.token {
            if !t.is_expired() { return Ok(AuthStatus::Authenticated); }
            if t.refresh_token.is_some() {
                self.refresh_token_now().await?;
                return Ok(AuthStatus::Authenticated);
            }
        }

        // LWA device code request.
        let res = self.client
            .post(LWA_AUTH_URL)
            .form(&[
                ("response_type", "device_code"),
                ("client_id",     self.client_id()?),
                ("scope",         "profile:amazon_photos"),
            ])
            .send().await?
            .json::<LwaDeviceCode>().await?;

        self.device_code = Some(res.device_code);
        self.poll_interval = res.interval;

        Ok(AuthStatus::PendingUserAction {
            message: format!(
                "Open this URL on any device:\n\n  {}\n\nEnter code: {}\n\n(expires in {} seconds)",
                res.verification_uri, res.user_code, res.expires_in
            ),
            poll_interval_secs: res.interval,
        })
    }

    async fn refresh_auth(&mut self) -> Result<()> {
        if let Some(t) = &self.token {
            if t.is_expired() { self.refresh_token_now().await?; }
        }

        if let Some(dc) = self.device_code.clone() {
            let res = self.client
                .post(LWA_TOKEN_URL)
                .form(&[
                    ("grant_type",   "device_code"),
                    ("device_code",  &dc),
                    ("client_id",    self.client_id()?),
                    ("client_secret", self.client_secret()?),
                ])
                .send().await?
                .json::<LwaToken>().await?;

            if let Some(at) = res.access_token {
                let t = StoredToken {
                    access_token:  at,
                    refresh_token: res.refresh_token,
                    expires_at: Utc::now() + chrono::Duration::seconds(res.expires_in.unwrap_or(3600) as i64),
                };
                self.save_token(&t).await;
                self.token = Some(t);
                self.device_code = None;
                info!("Amazon Photos: device authorized!");
            }
        }
        Ok(())
    }

    async fn list_photos(&self, limit: usize, _offset: usize) -> Result<Vec<PhotoMeta>> {
        let url = format!(
            "{}/nodes?filters=kind:PHOTOS&limit={}&asset=ALL&tempLink=true",
            DRIVE_API, limit
        );

        let res = self.client
            .get(&url)
            .bearer_auth(self.access_token()?)
            .send().await?
            .error_for_status()?
            .json::<NodeList>().await?;

        let photos = res.data.into_iter().map(|node| {
            let (w, h, taken) = node.content
                .as_ref()
                .and_then(|c| c.image.as_ref())
                .map(|img| (
                    img.width.unwrap_or(0),
                    img.height.unwrap_or(0),
                    img.taken.as_deref()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                ))
                .unwrap_or((0, 0, None));

            PhotoMeta {
                id:           node.id.clone(),
                filename:     node.name,
                width:        w,
                height:       h,
                taken_at:     taken,
                download_url: Some(format!("{}/nodes/{}/content", DRIVE_API, node.id)),
                extra:        Default::default(),
            }
        }).collect();

        Ok(photos)
    }

    async fn get_photo_bytes(
        &self,
        meta: &PhotoMeta,
        _dw: u32,
        _dh: u32,
    ) -> Result<Vec<u8>> {
        let url = meta.download_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("no URL for {}", meta.id))?;

        let bytes = self.client
            .get(url)
            .bearer_auth(self.access_token()?)
            .send().await?
            .error_for_status()?
            .bytes().await?
            .to_vec();

        debug!("Fetched {} bytes for {}", bytes.len(), meta.filename);
        Ok(bytes)
    }
}
