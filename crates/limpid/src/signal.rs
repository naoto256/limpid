//! Signal handling for the daemon.
//!
//! - SIGTERM / SIGINT → graceful shutdown
//! - SIGHUP → configuration hot-reload

use anyhow::{Context, Result};
#[cfg(unix)]
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
#[cfg(windows)]
pub async fn wait_for_signal() -> Result<SignalAction> {
    if crate::service::active() {
        return crate::service::next_action().await;
    }
    let mut interrupt =
        tokio::signal::windows::ctrl_c().context("failed to register Ctrl-C handler")?;
    let mut break_signal =
        tokio::signal::windows::ctrl_break().context("failed to register Ctrl-Break handler")?;
    tokio::select! { _ = interrupt.recv() => {}, _ = break_signal.recv() => {} }
    info!("received Windows console shutdown signal");
    Ok(SignalAction::Shutdown)
}

#[cfg(unix)]
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
