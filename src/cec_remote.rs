//! HDMI CEC remote control receiver (Linux).
//!
//! Listens for `UserControlPressed` CEC frames and maps common TV remote keys
//! to slideshow commands:
//! - next: Right / ChannelUp / SkipForward
//! - previous: Left / ChannelDown / SkipBackward
//! - pause toggle: Play/Pause/PausePlayFunction
//! - favourite toggle: FavoriteMenu / F1Blue
//!
//! Best-effort and isolated: failures are surfaced when starting the receiver,
//! but runtime poll errors are logged and retried so slideshow rendering keeps
//! running.

use crate::config::CecConfig;
use crate::renderer::SlideshowCmd;
use anyhow::Result;
use tokio::sync::mpsc::Receiver;

#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use log::{debug, info};
#[cfg(target_os = "linux")]
use tokio::sync::mpsc::{channel, Sender};

#[cfg(target_os = "linux")]
use linux_cec::async_support::AsyncDevice;
#[cfg(target_os = "linux")]
use linux_cec::device::PollResult;
#[cfg(target_os = "linux")]
use linux_cec::message::Message;
#[cfg(target_os = "linux")]
use linux_cec::operand::UiCommand;
#[cfg(target_os = "linux")]
use linux_cec::{FollowerMode, InitiatorMode, PollTimeout};

/// Start CEC key receiver and return a command channel consumed by slideshow.
pub async fn start(cfg: &CecConfig) -> Result<Receiver<SlideshowCmd>> {
    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = channel::<SlideshowCmd>(32);
        start_linux(cfg, tx).await?;
        return Ok(rx);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        anyhow::bail!("CEC remote is only supported on Linux");
    }
}

#[cfg(target_os = "linux")]
async fn start_linux(cfg: &CecConfig, tx: Sender<SlideshowCmd>) -> Result<()> {
    let mut dev = AsyncDevice::open(&cfg.device)
        .await
        .with_context(|| format!("CEC open {}", cfg.device))?;
    dev.set_initiator_mode(InitiatorMode::Disabled)
        .await
        .context("CEC set initiator mode disabled")?;
    dev.set_follower_mode(FollowerMode::Monitor)
        .await
        .context("CEC set follower mode monitor")?;

    info!("CEC remote: listening on {}", cfg.device);
    let poll_timeout = PollTimeout::from(cfg.poll_ms.min(2_000) as i32);

    tokio::spawn(async move {
        loop {
            if tx.is_closed() {
                return;
            }
            let polled = dev.poll(poll_timeout).await;
            let results = match polled {
                Ok(r) => r,
                Err(e) => {
                    debug!("CEC poll error: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };

            for result in results {
                let PollResult::Message(envelope) = result else {
                    continue;
                };
                let linux_cec::device::MessageData::Valid(Message::UserControlPressed {
                    ui_command,
                }) = envelope.message
                else {
                    continue;
                };
                if let Some(cmd) = map_ui_command(ui_command) {
                    match tx.try_send(cmd) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            debug!("CEC command queue full; dropping keypress");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn map_ui_command(cmd: UiCommand) -> Option<SlideshowCmd> {
    match cmd {
        UiCommand::Right | UiCommand::ChannelUp | UiCommand::SkipForward | UiCommand::PageDown => {
            Some(SlideshowCmd::Next)
        }
        UiCommand::Left | UiCommand::ChannelDown | UiCommand::SkipBackward | UiCommand::PageUp => {
            Some(SlideshowCmd::Prev)
        }
        UiCommand::Pause | UiCommand::Play | UiCommand::PausePlayFunction => {
            Some(SlideshowCmd::TogglePause)
        }
        UiCommand::FavoriteMenu | UiCommand::F1Blue => Some(SlideshowCmd::ToggleFavorite),
        UiCommand::Back => Some(SlideshowCmd::BackToGallery),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn map_ui_command(_: ()) -> Option<SlideshowCmd> {
    None
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn maps_core_transport_keys() {
        assert!(matches!(
            map_ui_command(UiCommand::Right),
            Some(SlideshowCmd::Next)
        ));
        assert!(matches!(
            map_ui_command(UiCommand::Left),
            Some(SlideshowCmd::Prev)
        ));
        assert!(matches!(
            map_ui_command(UiCommand::Pause),
            Some(SlideshowCmd::TogglePause)
        ));
        assert!(matches!(
            map_ui_command(UiCommand::FavoriteMenu),
            Some(SlideshowCmd::ToggleFavorite)
        ));
    }
}
