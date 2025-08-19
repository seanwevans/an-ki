// signals.rs: Handles signal handling, including graceful shutdown and CTRL+C handling.

use tokio::signal;
use std::error::Error;
use tracing::{error, info};

pub async fn setup_signal_handler() -> Result<(), Box<dyn Error>> {
    setup_signal_handler_internal(None).await
}

async fn setup_signal_handler_internal(
    tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), Box<dyn Error>> {
    // Listen for CTRL+C (SIGINT) and other termination signals
    signal::ctrl_c().await.map_err(|e| {
        error!("Failed to listen for CTRL+C: {:?}", e);
        e
    })?;
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
    info!("Received termination signal, starting graceful shutdown...");
    Ok(())
}

#[cfg(test)]
pub async fn setup_signal_handler_with_tx(
    tx: tokio::sync::oneshot::Sender<()>,
) -> Result<(), Box<dyn Error>> {
    setup_signal_handler_internal(Some(tx)).await
}

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

#[cfg(unix)]
pub async fn setup_unix_signal_handlers() -> Result<(), Box<dyn Error>> {
    setup_unix_signal_handlers_internal(None).await
}

#[cfg(unix)]
async fn setup_unix_signal_handlers_internal(
    tx: Option<tokio::sync::mpsc::Sender<&'static str>>,
) -> Result<(), Box<dyn Error>> {
    // Listen for SIGTERM
    let mut sigterm = signal(SignalKind::terminate()).map_err(|e| {
        error!("Failed to register SIGTERM handler: {:?}", e);
        e
    })?;
    let tx_term = tx.clone();
    tokio::spawn(async move {
        sigterm.recv().await;
        if let Some(tx) = tx_term {
            let _ = tx.send("SIGTERM").await;
        }
        info!("Received SIGTERM, starting graceful shutdown...");
    });

    // Listen for SIGHUP (optional)
    let mut sighup = signal(SignalKind::hangup()).map_err(|e| {
        error!("Failed to register SIGHUP handler: {:?}", e);
        e
    })?;
    tokio::spawn(async move {
        sighup.recv().await;
        if let Some(tx) = tx {
            let _ = tx.send("SIGHUP").await;
        }
        info!("Received SIGHUP, reloading configuration...");
    });

    Ok(())
}

#[cfg(all(test, unix))]
pub async fn setup_unix_signal_handlers_with_tx(
    tx: tokio::sync::mpsc::Sender<&'static str>,
) -> Result<(), Box<dyn Error>> {
    setup_unix_signal_handlers_internal(Some(tx)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration, sleep};
    #[cfg(unix)]
    use nix::sys::signal::{self, Signal};
    #[cfg(unix)]
    use nix::unistd::Pid;

    #[cfg(unix)]
    #[tokio::test]
    async fn test_setup_signal_handler_ctrl_c() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handler = tokio::spawn(async move {
            setup_signal_handler_with_tx(tx).await.unwrap();
        });

        sleep(Duration::from_millis(100)).await;
        signal::kill(Pid::this(), Signal::SIGINT).unwrap();

        timeout(Duration::from_secs(1), rx).await.expect("signal wait").unwrap();
        handler.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_setup_unix_signal_handlers() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        setup_unix_signal_handlers_with_tx(tx).await.unwrap();

        sleep(Duration::from_millis(100)).await;
        signal::kill(Pid::this(), Signal::SIGTERM).unwrap();
        signal::kill(Pid::this(), Signal::SIGHUP).unwrap();

        let mut signals = vec![];
        signals.push(
            timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("sigterm wait")
                .unwrap(),
        );
        signals.push(
            timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("sighup wait")
                .unwrap(),
        );
        signals.sort();
        assert_eq!(signals, vec!["SIGHUP", "SIGTERM"]);
    }
}
