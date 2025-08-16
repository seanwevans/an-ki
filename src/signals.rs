// signals.rs: Handles signal handling, including graceful shutdown and CTRL+C handling.

use tokio::signal;
use std::error::Error;
use tracing::{error, info};

pub async fn setup_signal_handler() -> Result<(), Box<dyn Error>> {
    // Listen for CTRL+C (SIGINT) and other termination signals
    signal::ctrl_c().await.map_err(|e| {
        error!("Failed to listen for CTRL+C: {:?}", e);
        e
    })?;
    info!("Received termination signal, starting graceful shutdown...");
    Ok(())
}

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

#[cfg(unix)]
pub async fn setup_unix_signal_handlers() -> Result<(), Box<dyn Error>> {
    // Listen for SIGTERM
    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| {
        error!("Failed to register SIGTERM handler: {:?}", e);
        e
    })?;
    tokio::spawn(async move {
        sigterm.recv().await;
        info!("Received SIGTERM, starting graceful shutdown...");
    });

    // Listen for SIGHUP (optional)
    let mut sighup = signal(SignalKind::hangup()).map_err(|e| {
        error!("Failed to register SIGHUP handler: {:?}", e);
        e
    })?;
    tokio::spawn(async move {
        sighup.recv().await;
        info!("Received SIGHUP, reloading configuration...");
    });

    Ok(())
}
