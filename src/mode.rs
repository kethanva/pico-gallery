//! Top-level UI mode for the display loop.
//!
//! Pause and display-power remain orthogonal flags; this enum only tracks
//! which primary surface owns input and rendering.

/// Which primary surface is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Scrollable thumbnail grid.
    Gallery,
    /// Fullscreen slideshow / single-photo viewer.
    Fullscreen,
}

impl Mode {
    pub fn is_gallery(self) -> bool {
        matches!(self, Mode::Gallery)
    }

    pub fn is_fullscreen(self) -> bool {
        matches!(self, Mode::Fullscreen)
    }
}
