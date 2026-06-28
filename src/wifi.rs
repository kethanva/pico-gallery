//! Host Wi-Fi configuration (Linux / Raspberry Pi only).
//!
//! Applies the `[wifi]` config section to the operating system so the photo
//! frame can be moved to a new network from the on-screen settings menu without
//! re-imaging the SD card. Two backends, tried in order:
//!
//!   1. `nmcli` (NetworkManager) — the default on Raspberry Pi OS Bookworm.
//!   2. `wpa_supplicant.conf` + `wpa_cli reconfigure` — older dhcpcd images.
//!
//! Both require root; the Pi appliance service runs as root. On non-Linux
//! platforms (e.g. a macOS dev box) `apply` returns an error so the UI can
//! report "not supported here" instead of pretending it worked.
//!
//! Security: the pre-shared key is never written to the log, and the
//! wpa_supplicant file is created 0600.

use anyhow::Result;

use crate::config::WifiConfig;

/// Apply Wi-Fi settings to the host OS. Returns `Ok(())` once a backend accepts
/// the change — the link itself may take a few seconds to associate afterwards.
///
/// Never panics and never blocks the slideshow for long: it shells out to the
/// system Wi-Fi tools and reports their outcome.
pub async fn apply(cfg: &WifiConfig) -> Result<()> {
    if cfg.ssid.trim().is_empty() {
        return Err(anyhow::anyhow!("Wi-Fi SSID is empty"));
    }
    apply_impl(cfg).await
}

#[cfg(target_os = "linux")]
async fn apply_impl(cfg: &WifiConfig) -> Result<()> {
    // Prefer NetworkManager when present (Raspberry Pi OS Bookworm default).
    if nmcli_available().await {
        log::info!("Wi-Fi: applying via nmcli (SSID '{}')", cfg.ssid);
        return nmcli_connect(cfg).await;
    }
    log::info!(
        "Wi-Fi: nmcli not found — applying via wpa_supplicant (SSID '{}')",
        cfg.ssid
    );
    wpa_supplicant_connect(cfg).await
}

#[cfg(not(target_os = "linux"))]
async fn apply_impl(_cfg: &WifiConfig) -> Result<()> {
    Err(anyhow::anyhow!(
        "Wi-Fi configuration is only supported on Linux / Raspberry Pi"
    ))
}

#[cfg(target_os = "linux")]
async fn nmcli_available() -> bool {
    tokio::process::Command::new("nmcli")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
async fn nmcli_connect(cfg: &WifiConfig) -> Result<()> {
    // `nmcli device wifi connect <ssid> password <psk>` creates/updates a
    // connection profile and associates. The password is passed as an argument
    // to nmcli only — it is never logged here.
    let mut cmd = tokio::process::Command::new("nmcli");
    cmd.args(["device", "wifi", "connect", &cfg.ssid]);
    if !cfg.password.is_empty() {
        cmd.args(["password", &cfg.password]);
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running nmcli: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        // stderr names the failure (bad password, no Wi-Fi device, …) without
        // echoing the PSK back.
        let err = String::from_utf8_lossy(&out.stderr);
        Err(anyhow::anyhow!("nmcli failed: {}", err.trim()))
    }
}

#[cfg(target_os = "linux")]
async fn wpa_supplicant_connect(cfg: &WifiConfig) -> Result<()> {
    const CONF: &str = "/etc/wpa_supplicant/wpa_supplicant.conf";

    // Prefer `wpa_passphrase` so the PSK is hashed rather than stored in
    // plaintext; fall back to a quoted plaintext psk if the tool is absent.
    let network_block = match wpa_passphrase_block(cfg).await {
        Some(b) => b,
        None => format!(
            "network={{\n\tssid=\"{}\"\n\tpsk=\"{}\"\n}}\n",
            cfg.ssid, cfg.password
        ),
    };

    let mut contents = String::from("ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n");
    contents.push_str("update_config=1\n");
    if !cfg.country.trim().is_empty() {
        contents.push_str(&format!("country={}\n", cfg.country.trim()));
    }
    contents.push_str(&network_block);

    write_owner_only(CONF, &contents).map_err(|e| anyhow::anyhow!("writing {CONF}: {e}"))?;

    // Ask a running supplicant to reload. Non-fatal if it isn't up yet — the
    // file is in place and the next boot will pick it up.
    let _ = tokio::process::Command::new("wpa_cli")
        .arg("reconfigure")
        .output()
        .await;
    Ok(())
}

/// Run `wpa_passphrase <ssid> <psk>` and return its `network={…}` block (which
/// carries a hashed psk). `None` if there is no password or the tool is missing.
#[cfg(target_os = "linux")]
async fn wpa_passphrase_block(cfg: &WifiConfig) -> Option<String> {
    if cfg.password.is_empty() {
        return None;
    }
    let out = tokio::process::Command::new("wpa_passphrase")
        .arg(&cfg.ssid)
        .arg(&cfg.password)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Write `contents` to `path`, truncating, with 0600 perms — the file holds
/// credentials and must not be world-readable. Small one-shot write, so plain
/// `std::fs` is fine.
#[cfg(target_os = "linux")]
fn write_owner_only(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}
