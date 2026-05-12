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
    }

    // On non-Linux platforms (macOS/dev), do nothing.
    // The renderer shows a black frame when the schedule says off.
    #[cfg(not(target_os = "linux"))]
    let _ = on;
}
