//! Host Wi-Fi configuration (Linux / Raspberry Pi only).
//!
//! Applies the `[wifi]` config section to the operating system so the photo
//! frame can be moved to a new network from the on-screen settings menu without
//! re-imaging the SD card. Two backends, tried in order:
//!
//!   1. `nmcli` (NetworkManager) — the default on Raspberry Pi OS Bookworm.
//!   2. `wpa_supplicant.conf` + `wpa_cli reconfigure` — older dhcpcd images.
//!
//! Both require host authorization. Standard appliance installs run as an
//! unprivileged service account, so the OS must grant NetworkManager/udisks
//! access or Wi-Fi should be provisioned outside the application.
//!
//! Security: the pre-shared key is never written to the log, and the
//! wpa_supplicant file is created 0600.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;

use crate::config::WifiConfig;

/// Apply Wi-Fi settings to the host OS. Returns `Ok(())` once a backend accepts
/// the change — the link itself may take a few seconds to associate afterwards.
///
/// Never panics and never blocks the slideshow for long: it shells out to the
/// system Wi-Fi tools and reports their outcome.
pub async fn apply(cfg: &WifiConfig) -> Result<()> {
    validate_credentials(cfg)?;
    apply_impl(cfg).await
}

fn validate_credentials(cfg: &WifiConfig) -> Result<()> {
    if cfg.ssid.trim().is_empty() {
        return Err(anyhow::anyhow!("Wi-Fi SSID is empty"));
    }
    if cfg.ssid.chars().any(char::is_control) || cfg.password.chars().any(char::is_control) {
        return Err(anyhow::anyhow!(
            "Wi-Fi SSID and password must not contain control characters"
        ));
    }
    if cfg.ssid.len() > 32 {
        return Err(anyhow::anyhow!("Wi-Fi SSID exceeds 32 bytes"));
    }
    if !cfg.password.is_empty() && !(8..=63).contains(&cfg.password.len()) {
        return Err(anyhow::anyhow!(
            "Wi-Fi password must be 8 to 63 bytes, or empty for an open network"
        ));
    }
    if !cfg.country.trim().is_empty() {
        let country = cfg.country.trim();
        if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(anyhow::anyhow!("Wi-Fi country must be exactly 2 letters"));
        }
    }
    Ok(())
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
    // NetworkManager accepts secrets from a private password file. This avoids
    // exposing the PSK through argv, where local process inspection can read it.
    let mut cmd = tokio::process::Command::new("nmcli");
    let password_file = if cfg.password.is_empty() {
        None
    } else {
        let path = write_private_temp_secret(&cfg.password)
            .context("creating temporary NetworkManager password file")?;
        cmd.arg("--passwd-file").arg(&path);
        Some(path)
    };
    cmd.args(["device", "wifi", "connect", &cfg.ssid]);
    let result = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running nmcli: {e}"))?;
    if let Some(path) = password_file {
        let _ = std::fs::remove_file(path);
    }
    if result.status.success() {
        Ok(())
    } else {
        // stderr names the failure (bad password, no Wi-Fi device, …) without
        // echoing the PSK back.
        let err = String::from_utf8_lossy(&result.stderr);
        Err(anyhow::anyhow!("nmcli failed: {}", err.trim()))
    }
}

#[cfg(target_os = "linux")]
fn write_private_temp_secret(secret: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    for attempt in 0..3 {
        let path = dir.join(format!(
            "picogallery-nmcli-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(format!("{secret}\n").as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate temporary password file",
    ))
}

#[cfg(target_os = "linux")]
async fn wpa_supplicant_connect(cfg: &WifiConfig) -> Result<()> {
    const CONF: &str = "/etc/wpa_supplicant/wpa_supplicant.conf";

    let network_block = if cfg.password.is_empty() {
        format!(
            "network={{\n\tssid=\"{}\"\n\tkey_mgmt=NONE\n}}\n",
            escape_wpa_string(&cfg.ssid)
        )
    } else {
        wpa_passphrase_block(cfg).await?
    };

    let mut existing = std::fs::read_to_string(CONF).unwrap_or_default();
    
    // Ensure preamble exists
    if !existing.contains("ctrl_interface=") {
        existing.insert_str(0, "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n");
    }
    if !existing.contains("update_config=1") {
        existing.push_str("update_config=1\n");
    }
    if !cfg.country.trim().is_empty() {
        let country_line = format!("country={}\n", cfg.country.trim());
        if !existing.contains(&country_line) {
            existing.push_str(&country_line);
        }
    }

    // Filter out existing network block for the same SSID
    let mut out = String::new();
    let mut in_block = false;
    let mut current_block = String::new();
    let ssid_line = format!("ssid=\"{}\"", escape_wpa_string(&cfg.ssid));
    
    for line in existing.lines() {
        if !in_block && line.trim().starts_with("network={") {
            in_block = true;
            current_block.push_str(line);
            current_block.push('\n');
        } else if in_block {
            current_block.push_str(line);
            current_block.push('\n');
            if line.trim() == "}" {
                in_block = false;
                if !current_block.contains(&ssid_line) {
                    out.push_str(&current_block);
                }
                current_block.clear();
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if in_block {
        out.push_str(&current_block);
    }
    
    out.push_str(&network_block);

    write_owner_only(CONF, &out).map_err(|e| anyhow::anyhow!("writing {CONF}: {e}"))?;

    // Ask a running supplicant to reload. Non-fatal if it isn't up yet — the
    // file is in place and the next boot will pick it up.
    let _ = tokio::process::Command::new("wpa_cli")
        .arg("reconfigure")
        .output()
        .await;
    Ok(())
}

/// Run `wpa_passphrase <ssid> <psk>` and return its `network={…}` block (which
/// carries a hashed psk). The password is supplied on stdin so it is not exposed
/// in the process list, and the plaintext comment emitted by the tool is removed.
#[cfg(target_os = "linux")]
async fn wpa_passphrase_block(cfg: &WifiConfig) -> Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new("wpa_passphrase")
        .arg(&cfg.ssid)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .await
        .context("starting wpa_passphrase")?;
    child
        .stdin
        .take()
        .context("opening wpa_passphrase stdin")?
        .write_all(format!("{}\n", cfg.password).as_bytes())
        .await
        .context("writing wpa_passphrase stdin")?;
    let out = child
        .wait_with_output()
        .await
        .context("waiting for wpa_passphrase")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "wpa_passphrase failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let block = String::from_utf8(out.stdout).context("wpa_passphrase returned invalid UTF-8")?;
    Ok(block
        .lines()
        .filter(|line| !line.trim_start().starts_with("#psk="))
        .map(|line| format!("{line}\n"))
        .collect())
}

#[cfg(target_os = "linux")]
fn escape_wpa_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_reject_control_characters() {
        let cfg = WifiConfig {
            ssid: "safe\nnetwork={".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
    }

    #[test]
    fn credentials_enforce_protocol_lengths() {
        let cfg = WifiConfig {
            ssid: "wifi".into(),
            password: "short".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
    }
}

/// Write `contents` to `path`, truncating, with 0600 perms — the file holds
/// credentials and must not be world-readable. Small one-shot write, so plain
/// `std::fs` is fine.
#[cfg(target_os = "linux")]
fn write_owner_only(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    
    // Explicitly chmod the file if it exists so permissions are fixed even if created world-readable
    if std::path::Path::new(path).exists() {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(mut perms) = std::fs::metadata(path).map(|m| m.permissions()) {
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}
