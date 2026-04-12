/// PicoGallery — public API surface for plugins.
///
/// Plugins depend on this crate as `picogallery` and import from here:
///   use picogallery::plugin::{AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig};
pub mod cache;
pub mod config;
pub mod plugin;
pub mod renderer;
pub mod slideshow;
