// signals.rs: Handles signal handling, including graceful shutdown and CTRL+C handling.

use tokio::signal;
use std::error::Error;
use tracing::{error, info};

use tokio::sync::oneshot;

pub async fn setup_signal_handler() -> Result<(), Box<dyn Error>> {
    setup_signal_handler_internal(None, None).await
}

async fn setup_signal_handler_internal(
    tx: Option<tokio::sync::oneshot::Sender<()>>,
    ready: Option<oneshot::Sender<()>>,
) -> Result<(), Box<dyn Error>> {
    // Register for CTRL+C (SIGINT)
    let ctrl_c = signal::ctrl_c();
    if let Some(r) = ready {
        let _ = r.send(());
    }
    ctrl_c.await.map_err(|e| {
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
    ready: oneshot::Sender<()>,
) -> Result<(), Box<dyn Error>> {
    setup_signal_handler_internal(Some(tx), Some(ready)).await
}

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

#[cfg(unix)]
pub async fn setup_unix_signal_handlers() -> Result<(), Box<dyn Error>> {
    setup_unix_signal_handlers_internal(None, None).await
}

#[cfg(unix)]
async fn setup_unix_signal_handlers_internal(
    tx: Option<tokio::sync::mpsc::Sender<&'static str>>,
    ready: Option<oneshot::Sender<()>>,
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

    if let Some(r) = ready {
        let _ = r.send(());
    }

    Ok(())
}

#[cfg(all(test, unix))]
pub async fn setup_unix_signal_handlers_with_tx(
    tx: tokio::sync::mpsc::Sender<&'static str>,
    ready: oneshot::Sender<()>,
) -> Result<(), Box<dyn Error>> {
    setup_unix_signal_handlers_internal(Some(tx), Some(ready)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};
    #[cfg(unix)]
    use nix::sys::signal::{self, Signal};
    #[cfg(unix)]
    use nix::unistd::Pid;

    #[cfg(unix)]
    #[tokio::test]
    async fn test_setup_signal_handler_ctrl_c() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let handler = tokio::spawn(async move {
            setup_signal_handler_with_tx(tx, ready_tx).await.unwrap();
        });

        // Ensure the handler has been registered before sending the signal
        ready_rx.await.unwrap();
        signal::kill(Pid::this(), Signal::SIGINT).unwrap();

        timeout(Duration::from_secs(1), rx).await.expect("signal wait").unwrap();
        handler.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_setup_unix_signal_handlers() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        setup_unix_signal_handlers_with_tx(tx, ready_tx).await.unwrap();

        // Wait for handlers to be ready before sending signals
        ready_rx.await.unwrap();
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
