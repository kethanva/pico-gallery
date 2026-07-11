//! Fullscreen viewer controller — current photo, return-to-gallery index.

use crate::mode::Mode;
use picogallery_core::PhotoMeta;

/// Fullscreen slideshow / viewer state shared with the display loop.
pub struct FullscreenController {
    pub current_queue_idx: usize,
    pub current_meta: Option<(usize, PhotoMeta)>,
    /// When set, the next advance should open this exact queue index (cut).
    pub pending_open: Option<usize>,
}

impl FullscreenController {
    pub fn new() -> Self {
        Self {
            current_queue_idx: 0,
            current_meta: None,
            pending_open: None,
        }
    }

    pub fn open_at(&mut self, queue_idx: usize) -> Mode {
        self.pending_open = Some(queue_idx);
        Mode::Fullscreen
    }

    pub fn return_to_gallery(&mut self) -> Mode {
        self.pending_open = None;
        Mode::Gallery
    }
}

impl Default for FullscreenController {
    fn default() -> Self {
        Self::new()
    }
}
