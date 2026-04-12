/// PicoGallery library crate.
///
/// The `plugin` module is re-exported from `picogallery-core` so that
/// external code can use `picogallery::plugin::PhotoPlugin` etc.
/// Plugins themselves depend on `picogallery-core` directly to avoid a
/// cyclic dependency with this crate.

pub mod plugin {
    pub use picogallery_core::*;
}
pub mod cache;
pub mod config;
pub mod renderer;
pub mod slideshow;
