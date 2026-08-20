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
        let file = write_private_temp_secret(&cfg.password)
            .context("creating temporary NetworkManager password file")?;
        cmd.arg("--passwd-file").arg(file.path());
        Some(file)
    };
    cmd.args(["device", "wifi", "connect", &cfg.ssid]);
    let result = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running nmcli: {e}"))?;
    if let Some(file) = password_file {
        drop(file);
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
fn write_private_temp_secret(secret: &str) -> std::io::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file = tempfile::Builder::new()
        .prefix("picogallery-nmcli-")
        .tempfile()?;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(file.path(), perms)?;
    file.write_all(format!("{secret}\n").as_bytes())?;
    file.flush()?;
    Ok(file)
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
        existing.insert_str(
            0,
            "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n",
        );
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
    fn credentials_reject_empty_ssid() {
        let cfg = WifiConfig {
            ssid: "".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
    }

    #[test]
    fn credentials_reject_long_ssid() {
        let cfg = WifiConfig {
            ssid: "123456789012345678901234567890123".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
    }

    #[test]
    fn credentials_accept_open() {
        let cfg = WifiConfig {
            ssid: "open_network".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_ok());
    }

    #[test]
    fn credentials_accept_psk() {
        let cfg = WifiConfig {
            ssid: "secure_network".into(),
            password: "password123".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_ok());
    }

    #[test]
    fn credentials_reject_invalid_country() {
        let cfg = WifiConfig {
            ssid: "wifi".into(),
            country: "USA".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
        let cfg = WifiConfig {
            ssid: "wifi".into(),
            country: "U1".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_err());
    }

    #[test]
    fn credentials_accept_valid_country() {
        let cfg = WifiConfig {
            ssid: "wifi".into(),
            country: "US".into(),
            ..Default::default()
        };
        assert!(validate_credentials(&cfg).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn temp_secret_is_owner_only_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let file = write_private_temp_secret("psk-test").expect("temp secret");
        let path = file.path().to_path_buf();
        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(file);
        assert!(!path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn string_escape() {
        assert_eq!(escape_wpa_string("test\\string"), "test\\\\string");
        assert_eq!(escape_wpa_string("test\"string"), "test\\\"string");
    }
}

/// Write `contents` to `path`, truncating, with 0600 perms — the file holds
/// credentials and must not be world-readable. Small one-shot write, so plain
/// `std::fs` is fine.
#[cfg(target_os = "linux")]
fn write_owner_only(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let mut temp = tempfile::Builder::new()
        .prefix("wpa_supplicant.conf.")
        .tempfile_in(dir)?;

    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
    std::fs::set_permissions(temp.path(), perms)?;

    temp.write_all(contents.as_bytes())?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
