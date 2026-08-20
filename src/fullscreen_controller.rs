//! Fullscreen viewer controller — pending open-at-index request.
//!
//! The live viewer state (current queue index + on-screen photo metadata) is
//! owned by the display loop; this type only carries the deferred "open this
//! exact index next" request raised from gallery input.

/// Fullscreen open request shared with the display loop.
pub struct FullscreenController {
    /// When set, the display loop should spawn a priority fetch for this index.
    pub pending_open: Option<usize>,
    /// Index whose priority fetch is in flight. Mode switch happens when the
    /// `Fetcher` result arrives, so the display loop never awaits the JPEG.
    pub awaiting_open: Option<usize>,
}

impl FullscreenController {
    pub fn new() -> Self {
        Self {
            pending_open: None,
            awaiting_open: None,
        }
    }
}

impl Default for FullscreenController {
    fn default() -> Self {
        Self::new()
    }
}
