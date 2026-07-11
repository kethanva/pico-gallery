//! Plugin-agnostic photo queue access for gallery/fullscreen controllers.

use anyhow::Result;
use picogallery_core::{BoxedPlugin, PhotoMeta};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cache::ImageCache;

/// Narrow fetch interface: controllers never see plugin names.
pub struct QueueSource {
    plugins: Vec<BoxedPlugin>,
    cache: Arc<Mutex<ImageCache>>,
}

impl QueueSource {
    pub fn new(plugins: Vec<BoxedPlugin>, cache: Arc<Mutex<ImageCache>>) -> Self {
        Self { plugins, cache }
    }

    pub fn plugins(&self) -> &[BoxedPlugin] {
        &self.plugins
    }

    pub fn plugins_mut(&mut self) -> &mut Vec<BoxedPlugin> {
        &mut self.plugins
    }

    pub fn into_plugins(self) -> Vec<BoxedPlugin> {
        self.plugins
    }

    pub fn cache(&self) -> &Arc<Mutex<ImageCache>> {
        &self.cache
    }

    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    pub async fn list_page(
        &self,
        plugin_idx: usize,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<PhotoMeta>> {
        self.plugins
            .get(plugin_idx)
            .ok_or_else(|| anyhow::anyhow!("plugin index {plugin_idx} out of range"))?
            .list_photos(limit, offset)
            .await
    }

    pub async fn get_bytes(
        &self,
        plugin_idx: usize,
        meta: &PhotoMeta,
        dw: u32,
        dh: u32,
    ) -> Result<Vec<u8>> {
        let plugin = self
            .plugins
            .get(plugin_idx)
            .ok_or_else(|| anyhow::anyhow!("plugin index {plugin_idx} out of range"))?;
        let key = meta.cache_key(plugin.name());
        {
            let mut cache = self.cache.lock().await;
            if let Some(bytes) = cache.get(&key).await {
                return Ok(bytes);
            }
        }
        let bytes = plugin.get_photo_bytes(meta, dw, dh).await?;
        let _ = self.cache.lock().await.put(&key, &bytes).await;
        Ok(bytes)
    }
}
