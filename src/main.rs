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
    /// Path to config.toml.  Defaults to ~/.config/picogallery/config.toml
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run at a specific log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info", env = "RUST_LOG")]
    log_level: String,

    /// Print the generated default config and exit (useful for first-time setup).
    #[arg(long)]
    print_default_config: bool,
}

// ── Plugin registry ───────────────────────────────────────────────────────────
//
// Each plugin is conditionally compiled in via a feature flag.
// Add more plugins here as they are created.

fn build_plugins(cfg: &Config) -> Vec<BoxedPlugin> {
    let mut plugins: Vec<BoxedPlugin> = Vec::new();

    #[cfg(feature = "plugin-google-photos")]
    {
        if let Some(pcfg) = cfg.plugin_config("google-photos") {
            info!("Registering plugin: google-photos");
            plugins.push(Box::new(picogallery_google_photos::GooglePhotosPlugin::new(pcfg.clone())));
        }
    }

    #[cfg(feature = "plugin-amazon-photos")]
    {
        if let Some(pcfg) = cfg.plugin_config("amazon-photos") {
            info!("Registering plugin: amazon-photos");
            plugins.push(Box::new(picogallery_amazon_photos::AmazonPhotosPlugin::new(pcfg.clone())));
        }
    }

    #[cfg(feature = "plugin-local")]
    {
        if let Some(pcfg) = cfg.plugin_config("local") {
            info!("Registering plugin: local");
            plugins.push(Box::new(picogallery_local::LocalPlugin::new(pcfg.clone())));
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

    if args.print_default_config {
        println!("{}", DEFAULT_CONFIG);
        return Ok(());
    }

    // Config.
    let config_path = args.config.unwrap_or_else(Config::default_path);
    info!("Loading config from {}", config_path.display());

    let config = if config_path.exists() {
        Config::from_file(&config_path)?
    } else {
        eprintln!("Config not found at {}.", config_path.display());
        eprintln!("Run `picogallery --print-default-config > ~/.config/picogallery/config.toml`");
        eprintln!("then edit it with your credentials.");
        std::process::exit(1);
    };
    config.ensure_dirs()?;

    // Plugins.
    let mut plugins = build_plugins(&config);
    if plugins.is_empty() {
        eprintln!("No plugins enabled. Check that at least one [[plugins]] entry has enabled=true.");
        std::process::exit(1);
    }

    // Initialise plugins.
    for plugin in &mut plugins {
        let pcfg = config
            .plugin_config(plugin.name())
            .cloned()
            .unwrap_or_default();
        plugin.init(&pcfg).await
            .with_context(|| format!("initialising plugin {}", plugin.name()))?;
    }

    // Run.
    let slideshow = Slideshow::new(config, plugins).await?;
    slideshow.run().await
}

// ── Default config template ───────────────────────────────────────────────────

const DEFAULT_CONFIG: &str = r#"
# PicoGallery configuration — copy to ~/.config/picogallery/config.toml

[display]
slide_duration_secs = 10     # seconds each photo is shown
transition          = "fade" # "cut" | "fade" | "slide_left" | "slide_right"
transition_ms       = 800    # transition duration (ms); set 0 to disable
fill_screen         = false  # true = crop to fill; false = letterbox
fps                 = 15     # max FPS — lower saves CPU on Pi Zero
# width  = 1920             # uncomment to force resolution
# height = 1080

[cache]
max_mb        = 256   # disk cache ceiling in megabytes
prefetch_count = 3    # photos to pre-fetch ahead (keep low on Pi Zero)

# ─────────────────────────────────────────────────────────────
# Google Photos Plugin  (uses rclone — no API key needed)
# ─────────────────────────────────────────────────────────────
# One-time setup (run once in a terminal, then never again):
#   rclone config
#   → n  (new remote)
#   → name: gphotos
#   → type: google photos
#   → leave client_id / client_secret blank  (use rclone's built-in)
#   → read_only: true
#   → browser opens → sign in to Google → approve
#   → q  (quit)
[[plugins]]
name           = "google-photos"
enabled        = true
rclone_remote  = "gphotos"                    # must match the name you gave in rclone config
sync_dir       = "/tmp/picogallery-gphotos"   # local cache of synced photos
# album        = "Favourites"                 # optional: sync one album only
# max_transfer = "500"                        # MB cap per sync run

# ─────────────────────────────────────────────────────────────
# Amazon Photos Plugin (optional)
# ─────────────────────────────────────────────────────────────
# [[plugins]]
# name          = "amazon-photos"
# enabled       = false
# client_id     = "YOUR_LWA_CLIENT_ID"
# client_secret = "YOUR_LWA_CLIENT_SECRET"

# ─────────────────────────────────────────────────────────────
# Local filesystem plugin (optional)
# ─────────────────────────────────────────────────────────────
# [[plugins]]
# name    = "local"
# enabled = false
# paths   = ["/mnt/photos", "/home/pi/Pictures"]
"#;
