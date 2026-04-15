use anyhow::{Context, Result};
use clap::Parser;
use log::info;
use std::path::PathBuf;

use picogallery::config::Config;
use picogallery::plugin::BoxedPlugin;
use picogallery::slideshow::Slideshow;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name        = "picogallery",
    version     = env!("CARGO_PKG_VERSION"),
    about       = "Lightweight plugin-based photo slideshow for Raspberry Pi",
    long_about  = None,
)]
struct Args {
    /// Path to config.toml.  Defaults to ~/.config/picogallery/config.toml.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    log_level: String,

    /// Print the default config template to stdout and exit.
    #[arg(long)]
    print_default_config: bool,

    /// Write the default config template to the config path and exit.
    /// Skips writing if the file already exists (use --force to overwrite).
    #[arg(long)]
    generate_config: bool,

    /// Together with --generate-config: overwrite an existing config file.
    #[arg(long)]
    force: bool,
}

// ── Plugin registry ───────────────────────────────────────────────────────────
//
// Plugins are conditionally compiled via Cargo features.
// At runtime, only plugins with `enabled = true` in [[plugins]] are loaded.

fn build_plugins(cfg: &Config) -> Vec<BoxedPlugin> {
    let mut plugins: Vec<BoxedPlugin> = Vec::new();

    #[cfg(feature = "plugin-directory")]
    {
        if let Some(pcfg) = cfg.plugin_config("directory") {
            info!("Registering plugin: directory");
            plugins.push(Box::new(
                picogallery_directory::DirectoryPlugin::new(pcfg.clone()),
            ));
        }
    }

    #[cfg(feature = "plugin-local")]
    {
        if let Some(pcfg) = cfg.plugin_config("local") {
            info!("Registering plugin: local");
            plugins.push(Box::new(picogallery_local::LocalPlugin::new(pcfg.clone())));
        }
    }

    #[cfg(feature = "plugin-google-photos")]
    {
        if let Some(pcfg) = cfg.plugin_config("google-photos") {
            info!("Registering plugin: google-photos");
            plugins.push(Box::new(
                picogallery_google_photos::GooglePhotosPlugin::new(pcfg.clone()),
            ));
        }
    }

    #[cfg(feature = "plugin-amazon-photos")]
    {
        if let Some(pcfg) = cfg.plugin_config("amazon-photos") {
            info!("Registering plugin: amazon-photos");
            plugins.push(Box::new(
                picogallery_amazon_photos::AmazonPhotosPlugin::new(pcfg.clone()),
            ));
        }
    }

    plugins
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")] // single-threaded — Pi Zero has 1 core
async fn main() -> Result<()> {
    let args = Args::parse();

    // Logging.
    std::env::set_var("RUST_LOG", &args.log_level);
    env_logger::init();

    // ── --print-default-config ───────────────────────────────────────────────
    if args.print_default_config {
        println!("{}", DEFAULT_CONFIG);
        return Ok(());
    }

    let config_path = args.config.unwrap_or_else(Config::default_path);

    // ── --generate-config ────────────────────────────────────────────────────
    if args.generate_config {
        return generate_config(&config_path, args.force);
    }

    // ── Normal startup ────────────────────────────────────────────────────────
    info!("Loading config from {}", config_path.display());

    let config = if config_path.exists() {
        Config::from_file(&config_path)?
    } else {
        // No config found — write a default and exit with instructions.
        eprintln!(
            "Config not found at {}.\n\
             Generating a default config — edit it to enable a plugin, then restart.",
            config_path.display()
        );
        generate_config(&config_path, false)?;
        std::process::exit(1);
    };

    config.ensure_dirs()?;

    // ── Plugins ───────────────────────────────────────────────────────────────
    let mut plugins = build_plugins(&config);
    if plugins.is_empty() {
        eprintln!(
            "No plugins enabled.\n\
             Edit {} and set `enabled = true` on at least one [[plugins]] entry.",
            config_path.display()
        );
        std::process::exit(1);
    }

    // Initialise each enabled plugin.
    for plugin in &mut plugins {
        let pcfg = config
            .plugin_config(plugin.name())
            .cloned()
            .unwrap_or_default();
        plugin
            .init(&pcfg)
            .await
            .with_context(|| format!("initialising plugin '{}'", plugin.name()))?;
    }

    // Run the slideshow.
    let slideshow = Slideshow::new(config, plugins).await?;
    slideshow.run().await
}

// ── Config generation ─────────────────────────────────────────────────────────

fn generate_config(path: &std::path::Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        eprintln!(
            "Config already exists at {}.\n\
             Use --force to overwrite, or edit it directly.",
            path.display()
        );
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {}", parent.display()))?;
    }

    std::fs::write(path, DEFAULT_CONFIG)
        .with_context(|| format!("writing config to {}", path.display()))?;

    eprintln!(
        "Config written to {}.\n\
         Edit it to enable a plugin (set `enabled = true`) then run picogallery again.",
        path.display()
    );
    Ok(())
}

// ── Default config template ───────────────────────────────────────────────────
//
// Shown by --print-default-config and written by --generate-config.
// All plugins are disabled by default — the user opts in.

const DEFAULT_CONFIG: &str = r#"# PicoGallery configuration
# Generated by: picogallery --generate-config
# Location:     ~/.config/picogallery/config.toml
#
# HOW TO USE
# ----------
# 1. Enable one or more plugins below by setting  enabled = true
# 2. Fill in the required fields for each plugin you enable
# 3. Run:  picogallery
#
# You can enable multiple plugins at once — photos from all enabled sources
# are merged into a single shuffled play queue.

# ─────────────────────────────────────────────────────────────────────────────
# Display settings
# ─────────────────────────────────────────────────────────────────────────────
[display]
slide_duration_secs = 10      # seconds each photo is shown
transition          = "fade"  # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800     # transition animation duration (ms); 0 = instant
fill_screen         = false   # true = crop to fill screen; false = letterbox
fps                 = 15      # frame-rate cap — lower saves CPU on Pi Zero
# width  = 1920               # uncomment to force a specific resolution
# height = 1080

# ─────────────────────────────────────────────────────────────────────────────
# Cache settings
# ─────────────────────────────────────────────────────────────────────────────
[cache]
max_mb         = 256  # maximum on-disk cache size in megabytes
prefetch_count = 3    # how many photos to pre-fetch ahead (keep low on Pi Zero)
# dir = "/tmp/picogallery-cache"   # override cache location (default: ~/.cache/picogallery)

# ═════════════════════════════════════════════════════════════════════════════
# PLUGINS
# Set  enabled = true  on the plugin(s) you want to use.
# ═════════════════════════════════════════════════════════════════════════════

# ─────────────────────────────────────────────────────────────────────────────
# Directory plugin  ★ RECOMMENDED — simplest setup
# Displays photos from a local folder, with sub-folder "album" support.
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name    = "directory"
enabled = false

# Path to the root folder that contains your photos (required).
path = "/home/pi/Photos"

# How to order photos:
#   "shuffle"      — random order (default)
#   "alphabetical" — sort by filename
#   "date_modified"— newest files first
order = "shuffle"

# Scan sub-directories and treat them as albums (default: true).
recursive = true

# Optional: only show photos from these sub-directories (album names).
# Leave empty or omit to show all albums.
# allowed_albums = ["Vacation 2024", "Family"]

# Re-scan the directory every N seconds (0 = only at startup).
# rescan_interval_secs = 3600

# ─────────────────────────────────────────────────────────────────────────────
# Local filesystem plugin
# Like "directory" but accepts multiple root paths.
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name    = "local"
enabled = true

# One or more directories to scan for photos.
paths = ["/home/pi/Pictures", "/mnt/usb/photos"]

# Scan sub-directories (default: true).
recursive = true

# ─────────────────────────────────────────────────────────────────────────────
# Google Drive plugin  (requires rclone)
# ─────────────────────────────────────────────────────────────────────────────
# NOTE (March 2025): Google removed the photoslibrary.readonly API scope.
# Direct Google Photos API access is no longer possible for third-party apps.
# This plugin uses Google Drive via rclone instead.
#
# SETUP
# 1. Install rclone:  brew install rclone  /  sudo apt install rclone
# 2. Enable this plugin and run picogallery — a browser sign-in will open.
# 3. Once authenticated the token is saved; subsequent runs are automatic.
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name    = "google-photos"
enabled = false

# Local directory where synced photos are cached (created automatically).
sync_dir = "/tmp/picogallery-gdrive"

# Optional: Google Drive folder ID to sync from (leave blank for root).
# Find the folder ID in the Drive URL: …/folders/<FOLDER_ID>
# drive_folder_id = ""

# Maximum data transferred per sync run (default: 500 MB).
# max_transfer = "500"

# ─────────────────────────────────────────────────────────────────────────────
# Amazon Photos plugin
# ─────────────────────────────────────────────────────────────────────────────
# SETUP
# 1. Create a Login with Amazon (LWA) app at developer.amazon.com
# 2. Add  http://localhost  as an allowed redirect URL
# 3. Copy the client_id and client_secret below
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name    = "amazon-photos"
enabled = false

client_id     = "YOUR_LWA_CLIENT_ID"
client_secret = "YOUR_LWA_CLIENT_SECRET"
"#;
