// health.rs: Implements health checks and a heartbeat mechanism for monitoring node health.

use std::time::Duration;
use tokio::time;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, error};
use std::error::Error;

#[derive(Clone, Debug)]
pub struct HealthCheck {
    pub node_id: String,
    pub is_healthy: bool,
}

pub async fn start_heartbeat(
    interval: Duration,
    tx: broadcast::Sender<HealthCheck>,
    node_id: String,
    cancel: CancellationToken,
) {
    let mut ticker = time::interval(interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let health_check = HealthCheck {
                    node_id: node_id.clone(),
                    is_healthy: true,
                };
                if let Err(e) = tx.send(health_check) {
                    error!("Failed to send heartbeat: {:?}", e);
                } else {
                    info!("Sent heartbeat for node: {}", node_id);
                }
            }
            _ = cancel.cancelled() => {
                info!("Heartbeat task for node {} cancelled", node_id);
                break;
            }
        }
    }
}

pub async fn monitor_health(
    mut rx: broadcast::Receiver<HealthCheck>,
    unhealthy_threshold: u32,
    cancel: CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let mut unhealthy_count = 0;

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(health_check) => {
                        if !health_check.is_healthy {
                            unhealthy_count += 1;
                            error!("Node {} is unhealthy. Unhealthy count: {}", health_check.node_id, unhealthy_count);
                        } else {
                            unhealthy_count = 0;
                            info!("Node {} is healthy.", health_check.node_id);
                        }

                        if unhealthy_count >= unhealthy_threshold {
                            error!(
                                "Node {} has been unhealthy for {} consecutive checks. Taking corrective action.",
                                health_check.node_id, unhealthy_threshold
                            );
                            // Add corrective actions here, such as restarting the node or notifying other services.
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Heartbeat channel closed");
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        error!("Missed {} heartbeat messages", skipped);
                        return Err(broadcast::error::RecvError::Lagged(skipped).into());
                    }
                }
            }
            _ = cancel.cancelled() => {
                info!("Health monitoring cancelled");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn monitor_health_returns_on_channel_close() {
        let (tx, rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(async move { monitor_health(rx, 1, cancel).await });

        // Drop the sender to close the channel
        drop(tx);

        let res = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("monitor_health did not return in time")
            .expect("task panicked");

        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn start_heartbeat_stops_on_cancel() {
        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let node_id = "test-node".to_string();
        let hb_cancel = cancel.clone();

        let handle = tokio::spawn(async move { start_heartbeat(Duration::from_millis(10), tx, node_id, hb_cancel).await });

        // Receive one heartbeat to ensure the task is running
        rx.recv().await.unwrap();
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("start_heartbeat did not stop in time")
            .expect("task panicked");
    }
}
