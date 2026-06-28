/// Best-effort display power control.
///
/// On Raspberry Pi (Linux), attempts `vcgencmd display_power 0/1`.
/// If `vcgencmd` is unavailable (Pi 4/5 with full KMS, or non-Pi Linux),
/// the call is a silent no-op — the caller is expected to render a black
/// frame as the primary power-saving mechanism on those platforms.
///
/// All failures are logged at debug level and silently ignored; this must
/// never block or crash the slideshow.
pub async fn set_power(on: bool) {
    #[cfg(target_os = "linux")]
    {
        use log::debug;
        let val = if on { "1" } else { "0" };
        match tokio::process::Command::new("vcgencmd")
            .args(["display_power", val])
            .output()
            .await
        {
            Ok(o) if o.status.success() => {
                debug!("vcgencmd display_power {val}: ok");
            }
            Ok(o) => {
                debug!(
                    "vcgencmd display_power {val}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(_) => {
                debug!("vcgencmd not available — relying on black-frame fallback");
            }
        }

        // Send standby / wake commands over HDMI CEC
        let cec_cmd = if on { "on 0\n" } else { "standby 0\n" };
        let cec_task = async {
            match tokio::process::Command::new("cec-client")
                .args(["-s", "-d", "1"])
                // Ensure the child is killed if the timeout drops this future,
                // otherwise a hung CEC bus would leak an orphaned cec-client.
                .kill_on_drop(true)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        use tokio::io::AsyncWriteExt;
                        let _ = stdin.write_all(cec_cmd.as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                    child.wait().await
                }
                Err(e) => Err(std::io::Error::new(std::io::ErrorKind::NotFound, e)),
            }
        };

        let cec_label = cec_cmd.trim();
        match tokio::time::timeout(std::time::Duration::from_secs(3), cec_task).await {
            Ok(Ok(status)) if status.success() => {
                debug!("cec-client {cec_label}: ok");
            }
            Ok(Ok(status)) => {
                debug!("cec-client {cec_label} failed: status {status}");
            }
            Ok(Err(e)) => {
                debug!("cec-client {cec_label} failed to execute: {e}");
            }
            Err(_) => {
                debug!("cec-client {cec_label} timed out after 3 seconds");
            }
        }
    }

    // On non-Linux platforms (macOS/dev), do nothing.
    // The renderer shows a black frame when the schedule says off.
    #[cfg(not(target_os = "linux"))]
    let _ = on;
}
