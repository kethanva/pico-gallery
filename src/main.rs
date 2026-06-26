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
            plugins.push(Box::new(picogallery_directory::DirectoryPlugin::new(
                pcfg.clone(),
            )));
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

    #[cfg(feature = "plugin-webdav")]
    {
        if let Some(pcfg) = cfg.plugin_config("webdav") {
            info!("Registering plugin: webdav");
            plugins.push(Box::new(picogallery_webdav::WebDavPlugin::new(
                pcfg.clone(),
            )));
        }
    }

    #[cfg(feature = "plugin-photoprism")]
    {
        if let Some(pcfg) = cfg.plugin_config("photoprism") {
            info!("Registering plugin: photoprism");
            plugins.push(Box::new(picogallery_photoprism::PhotoPrismPlugin::new(
                pcfg.clone(),
            )));
        }
    }

    plugins
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")] // single-threaded — Pi Zero has 1 core
async fn main() -> Result<()> {
    let args = Args::parse();

    // Logging. clap has already resolved precedence (--log-level flag beats
    // the RUST_LOG env var beats the "info" default — see the `env` attr on
    // Args), so feed the resolved value straight to env_logger instead of
    // round-tripping through std::env::set_var, which is racy and unsafe as
    // of Rust 2024.
    env_logger::Builder::new()
        .parse_filters(&args.log_level)
        .init();

    // ── --print-default-config ───────────────────────────────────────────────
    if args.print_default_config {
        println!("{}", default_config());
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
        // exit(1) skips destructors, but nothing needing cleanup exists yet
        // (no cache, no SDL, no plugin state) — a plain exit is fine here.
        std::process::exit(1);
    };

    config.ensure_dirs()?;

    // ── Display schedule ──────────────────────────────────────────────────────
    if let Some(desc) = config.display.schedule_description() {
        info!("Display schedule active: on/off window = {desc}");
    }

    // ── Plugins ───────────────────────────────────────────────────────────────
    let mut plugins = build_plugins(&config);

    // Warn about plugins enabled in config that never registered — otherwise a
    // compiled-out Cargo feature (e.g. amazon-photos is not in the default
    // build) or a misspelled `name` silently drops the source with no clue why.
    let registered: std::collections::HashSet<&str> = plugins.iter().map(|p| p.name()).collect();
    for entry in &config.plugins {
        if entry.enabled && !registered.contains(entry.name.as_str()) {
            eprintln!(
                "Warning: plugin '{}' is enabled in config but not available in this build \
                 — unknown name, or its Cargo feature was not compiled in. Skipping.",
                entry.name
            );
        }
    }

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

    // ── HTTP remote (optional) ────────────────────────────────────────────────
    // Started before the slideshow so a bad bind (port in use, bad address)
    // fails fast at startup instead of surfacing mid-show.
    let (remote_rx, remote_status) = if config.remote.enabled {
        let status = picogallery::remote::SharedStatus::default();
        let rx = picogallery::remote::start(&config.remote, status.clone()).await?;
        (Some(rx), Some(status))
    } else {
        (None, None)
    };

    // Run the slideshow. The plugin factory lets the engine rebuild its photo
    // sources at runtime when the user switches source from the on-screen menu,
    // without the slideshow needing to know which plugins were compiled in.
    let slideshow = Slideshow::new(config, plugins, config_path, Box::new(build_plugins)).await?;
    slideshow.run(remote_rx, remote_status).await
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

    std::fs::write(path, default_config())
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
// The directory plugin is enabled by default and points at
// `<user's Pictures dir>/PicoGallery`, which is the same location install.sh
// populates with sample photos. Paths are computed at runtime so the config
// works for any user — not just `pi`.

/// Directory that the slideshow defaults to scanning. Matches install.sh's
/// `PHOTO_DIR` so a fresh install has content to display immediately.
fn default_photo_dir() -> PathBuf {
    dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("PicoGallery")
}

fn default_config() -> String {
    let photo_dir = default_photo_dir();
    let photo_dir_str = photo_dir.to_string_lossy();

    format!(
        r#"# PicoGallery configuration
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

# Photo order: "shuffle" (default) | "chronological" | "newest_first"
#              | "date_cluster" (small same-day/album runs, runs shuffled)
order             = "shuffle"
show_osd          = true   # show the album/date/filename pill + nav arrows
letterbox_blur    = true   # fill letterbox bars with a blurred copy, not black
ken_burns         = false  # slow zoom/pan per photo (more CPU; off on Pi Zero)
on_this_day_boost = true   # surface photos taken on today's date in past years

# ── Memory-safety limits (skip oversized photos instead of OOM) ──────────────
# max_image_mb   = 20    # raw file-size gate (0 = built-in 50 MB default)
# max_megapixels = 12    # decoded-pixel gate (0 = no limit); 12 MP ≈ 56 MB peak

# ── Optional display schedule (HH:MM, local time; both required) ─────────────
# Turn the HDMI output off overnight. Omit both = always on.
# on_time  = "07:00"
# off_time = "22:00"

# ── Optional night mode (HH:MM, local time; both required) ───────────────────
# Dim + warm-shift photos in a dark room. One cheap pixel pass per slide.
# night_start       = "21:00"
# night_end         = "07:00"
# night_dim_percent = 25     # brightness reduction (0–90)
# night_warmth      = 30     # warm tint strength (0–100)

# ─────────────────────────────────────────────────────────────────────────────
# Cache settings
# ─────────────────────────────────────────────────────────────────────────────
[cache]
max_mb         = 256  # maximum on-disk cache size in megabytes
prefetch_count = 3    # how many photos to pre-fetch ahead (keep low on Pi Zero)
# dir = "/tmp/picogallery-cache"   # override cache location (default: ~/.cache/picogallery)

# ─────────────────────────────────────────────────────────────────────────────
# HTTP remote control (optional)
# Phone-friendly next/prev/pause/favourite page + JSON status API. No
# authentication — only enable on a trusted LAN. Visit http://<pi-ip>:8188/
# once enabled. The ♥ button favourites the current photo (sources that
# support it, e.g. photoprism).
# ─────────────────────────────────────────────────────────────────────────────
[remote]
enabled = false
port    = 8188
bind    = "0.0.0.0"   # use "127.0.0.1" to restrict to local-only access

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
enabled = true

# Path to the root folder that contains your photos (required).
# Auto-resolved to the current user's Pictures directory — works for any
# user, not just `pi`. Plugin paths also accept a leading `~` which expands
# to $HOME, so you can edit this to e.g. "~/Photos" if preferred.
path = "{photo_dir}"

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
enabled = false

# One or more directories to scan for photos. `~` expands to $HOME.
paths = ["{photo_dir}", "/mnt/usb/photos"]

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
# NOTE: not in the default build — rebuild with
#       `cargo build --release --features plugin-amazon-photos` to include it.
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

# ─────────────────────────────────────────────────────────────────────────────
# [PLUGIN] photoprism  ★ another Raspberry Pi running PhotoPrism as a server
#
# Streams photos on demand from a PhotoPrism instance (https://photoprism.app)
# via its REST API. No local sync — the engine fetches the smallest pre-built
# thumbnail that fits the display, so this is light enough for a Pi Zero 2
# client talking to a Pi 4 / Pi 5 server.
#
# SETUP
# 1. Run PhotoPrism on the server Pi (Docker is easiest).
# 2. Note the URL (default http://<pi>.local:2342) and your admin password.
# 3. Optionally create an app password in PhotoPrism: Settings → Account →
#    App passwords. Use it instead of your real password.
# 4. Set enabled = true and restart picogallery.
# ─────────────────────────────────────────────────────────────────────────────
[[plugins]]
name    = "photoprism"
enabled = false

url      = "http://photoprism.local:2342"   # base URL of the PhotoPrism server
username = "admin"
password = "insecure"                       # or use app_password below
# app_password = "abcd-efgh-ijkl-mnop"      # PhotoPrism v0.10+ app password

# ── Filtering (all optional) ────────────────────────────────────────────────
# album       = "january-2024"     # album UID or slug
# albums      = ["trip", "family"] # OR several albums (album:trip|family)
# favorites   = true               # only favourites
# quality     = 3                  # 1=low … 5=excellent (drops anything lower)
# country     = "fr"               # ISO country code
# state       = "California"       # province / state name
# city        = "Paris"           # city name
# year        = 2024
# after       = "2020-06-01"       # only photos on/after this date (YYYY-MM-DD)
# before      = "2020-06-30"       # only photos on/before this date
# media_type  = "image"            # image | raw | live | animated | video
# color       = "blue"             # red|orange|gold|green|teal|blue|purple|pink|brown|white|grey|black
# mono        = true               # only black & white / monochrome photos
# panorama    = true               # only panoramas
# orientation = "portrait"         # portrait | landscape | square (good for a rotated frame)
# people      = ["Alice", "Bob"]   # only photos containing these subjects (faces)
# labels      = ["beach", "dog"]   # any of these labels (label:beach|dog)
# keywords    = ["sunset"]         # any of these keywords
# memories    = true               # only photos taken on today's date in any year
# query       = "label:beach keyword:sunset"   # raw PhotoPrism Q-language
#
# ── Privacy (a wall display should not surface these) ────────────────────────
# Private and archived photos are EXCLUDED by default. Opt back in:
# include_private  = false
# include_archived = false
#
# The configured album's real title is shown in the OSD pill, and PhotoPrism
# photo titles + "City, Country" location are added to the overlay automatically.
# Press F (or the ♥ button in the web remote) to favourite the on-screen photo.

# ── Ordering / paging ───────────────────────────────────────────────────────
# order    = "newest"   # newest | oldest | added | name | random | similar
# per_page = 100

# ── Thumbnail selection (saves Pi Zero RAM) ─────────────────────────────────
# Cap the largest thumbnail size requested. fit_1920 is plenty for 1080p panels.
# Sizes (longest edge px): tile_500, fit_720, fit_1280, fit_1920, fit_2048,
#                          fit_2560, fit_3840, fit_4096, fit_7680
# max_thumb      = "fit_1920"
# allow_original = true            # fall back to /dl/<hash> if no thumb fits

# ── Transport ───────────────────────────────────────────────────────────────
# skip_tls_verify       = false    # true for self-signed LAN certs
# request_timeout_secs  = 30
"#,
        photo_dir = photo_dir_str,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_template_parses_as_valid_config() {
        // The generated template is the first thing users edit — a typo in it
        // would make a fresh install fail to parse its own default config.
        let cfg: Config = toml::from_str(&default_config())
            .expect("generated default config must parse as valid Config");
        // Directory plugin ships enabled so a fresh install has photos to show.
        assert!(
            cfg.plugins
                .iter()
                .any(|p| p.name == "directory" && p.enabled),
            "directory plugin should be enabled by default"
        );
        // Remote section parses and is disabled out of the box (no-auth server).
        assert!(!cfg.remote.enabled);
    }
}
