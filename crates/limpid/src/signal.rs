//! Signal handling for the daemon.
//!
//! - SIGTERM / SIGINT → graceful shutdown
//! - SIGHUP → configuration hot-reload

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

/// The action requested by a caught signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    Shutdown,
    Reload,
}

/// Wait for either SIGTERM/SIGINT (shutdown) or SIGHUP (reload).
/// Returns which signal was received.
pub async fn wait_for_signal() -> Result<SignalAction> {
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
    let mut sigint =
        signal(SignalKind::interrupt()).context("failed to register SIGINT handler")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to register SIGHUP handler")?;

    let action = tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM");
            SignalAction::Shutdown
        }
        _ = sigint.recv() => {
            info!("received SIGINT");
            SignalAction::Shutdown
        }
        _ = sighup.recv() => {
            info!("received SIGHUP");
            SignalAction::Reload
        }
    };

    Ok(action)
}
