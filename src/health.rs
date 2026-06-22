// health.rs: Implements health checks and a heartbeat mechanism for monitoring node health.

use crate::error::AnKiError;
use crate::messaging::{consume_messages, declare_queue, establish_connection, publish_message};
use futures_util::stream::StreamExt;
use lapin::{options::BasicAckOptions, Channel};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Queue that nodes publish heartbeats to and the principal consumes from.
pub const HEARTBEAT_QUEUE: &str = "heartbeat_queue";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    pub node_id: String,
    pub is_healthy: bool,
}

/// Heartbeat publish interval, overridable with `HEARTBEAT_INTERVAL_MS`
/// (default 10s).
pub fn heartbeat_interval() -> Duration {
    let ms = std::env::var("HEARTBEAT_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    Duration::from_millis(ms)
}

/// Consecutive missed/unhealthy heartbeats before the principal alerts,
/// overridable with `HEALTH_UNHEALTHY_THRESHOLD` (default 3).
pub fn unhealthy_threshold() -> u32 {
    std::env::var("HEALTH_UNHEALTHY_THRESHOLD")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&threshold| threshold > 0)
        .unwrap_or(3)
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
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut unhealthy_counts: HashMap<String, u32> = HashMap::new();

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(health_check) => {
                        let threshold_reached = update_unhealthy_counts(&mut unhealthy_counts, &health_check, unhealthy_threshold);

                        if health_check.is_healthy {
                            info!("Node {} is healthy.", health_check.node_id);
                        } else if threshold_reached {
                            error!(
                                "Node {} has been unhealthy for {} consecutive checks. Taking corrective action.",
                                health_check.node_id, unhealthy_threshold
                            );
                            // Add corrective actions here, such as restarting the node or notifying other services.
                        } else {
                            if let Some(count) = unhealthy_counts.get(&health_check.node_id) {
                                error!("Node {} is unhealthy. Unhealthy count: {}", health_check.node_id, count);
                            } else {
                                error!("Node {} is unhealthy.", health_check.node_id);
                            }
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

fn update_unhealthy_counts(
    unhealthy_counts: &mut HashMap<String, u32>,
    health_check: &HealthCheck,
    unhealthy_threshold: u32,
) -> bool {
    if health_check.is_healthy {
        unhealthy_counts.remove(&health_check.node_id);
        return false;
    }

    let count = unhealthy_counts
        .entry(health_check.node_id.clone())
        .and_modify(|counter| *counter = counter.saturating_add(1))
        .or_insert(1);

    *count >= unhealthy_threshold
}

/// Periodically publishes this node's heartbeat to [`HEARTBEAT_QUEUE`] until
/// `cancel` fires. The publisher owns its own AMQP connection and transparently
/// reconnects if a publish fails, so it is independent of any consumer loop.
pub async fn publish_heartbeats(
    amqp_addr: String,
    node_id: String,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = time::interval(interval);
    let mut channel: Option<Channel> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Heartbeat publisher for node {} stopped", node_id);
                break;
            }
            _ = ticker.tick() => {
                if channel.is_none() {
                    match establish_connection(&amqp_addr).await {
                        Ok(new_channel) => {
                            if let Err(e) = declare_queue(&new_channel, HEARTBEAT_QUEUE).await {
                                error!("Failed to declare heartbeat queue: {:?}", e);
                                continue;
                            }
                            channel = Some(new_channel);
                        }
                        Err(e) => {
                            error!("Heartbeat publisher cannot reach broker: {:?}", e);
                            continue;
                        }
                    }
                }

                let Some(active) = channel.as_ref() else {
                    continue;
                };
                let health_check = HealthCheck {
                    node_id: node_id.clone(),
                    is_healthy: true,
                };
                match serde_json::to_vec(&health_check) {
                    Ok(payload) => {
                        if let Err(e) = publish_message(active, HEARTBEAT_QUEUE, &payload).await {
                            error!("Heartbeat publish failed; reconnecting: {:?}", e);
                            channel = None;
                        }
                    }
                    Err(e) => error!("Failed to serialize heartbeat: {:?}", e),
                }
            }
        }
    }
}

/// Consumes heartbeats from [`HEARTBEAT_QUEUE`] and tracks per-node health,
/// alerting when a node exceeds `unhealthy_threshold` consecutive bad reports.
/// Runs until `cancel` fires or the consumer stream closes.
pub async fn run_health_monitor(
    channel: Channel,
    unhealthy_threshold: u32,
    cancel: CancellationToken,
) -> Result<(), AnKiError> {
    declare_queue(&channel, HEARTBEAT_QUEUE).await?;
    let mut consumer =
        consume_messages(&channel, HEARTBEAT_QUEUE, "principal_health_consumer").await?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    info!(
        "Principal health monitor consuming heartbeats (unhealthy_threshold={})",
        unhealthy_threshold
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Health monitor cancelled");
                break;
            }
            delivery = consumer.next() => {
                match delivery {
                    Some(Ok(delivery)) => {
                        match serde_json::from_slice::<HealthCheck>(&delivery.data) {
                            Ok(health_check) => {
                                let alert = update_unhealthy_counts(
                                    &mut counts,
                                    &health_check,
                                    unhealthy_threshold,
                                );
                                if health_check.is_healthy {
                                    info!("Heartbeat: node {} healthy", health_check.node_id);
                                } else if alert {
                                    error!(
                                        "Node {} unhealthy for {} consecutive checks; corrective action needed",
                                        health_check.node_id, unhealthy_threshold
                                    );
                                } else {
                                    error!("Node {} reported unhealthy", health_check.node_id);
                                }
                            }
                            Err(e) => error!("Dropping malformed heartbeat: {:?}", e),
                        }
                        if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                            error!("Failed to acknowledge heartbeat: {:?}", e);
                        }
                    }
                    Some(Err(e)) => {
                        error!("Heartbeat consumer error: {:?}", e);
                        break;
                    }
                    None => {
                        info!("Heartbeat consumer stream closed");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
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

        let handle = tokio::spawn(async move {
            start_heartbeat(Duration::from_millis(10), tx, node_id, hb_cancel).await
        });

        // Receive one heartbeat to ensure the task is running
        rx.recv().await.unwrap();
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("start_heartbeat did not stop in time")
            .expect("task panicked");
    }

    #[test]
    fn update_unhealthy_counts_tracks_per_node_thresholds() {
        let mut counts = HashMap::new();
        let threshold = 2;

        let node_a = "node-a".to_string();
        let node_b = "node-b".to_string();

        // First unhealthy report for node A.
        let alert = update_unhealthy_counts(
            &mut counts,
            &HealthCheck {
                node_id: node_a.clone(),
                is_healthy: false,
            },
            threshold,
        );
        assert!(!alert);
        assert_eq!(counts.get(&node_a), Some(&1));

        // First unhealthy report for node B.
        let alert = update_unhealthy_counts(
            &mut counts,
            &HealthCheck {
                node_id: node_b.clone(),
                is_healthy: false,
            },
            threshold,
        );
        assert!(!alert);
        assert_eq!(counts.get(&node_b), Some(&1));

        // Second unhealthy report for node A should trigger the alert for node A only.
        let alert = update_unhealthy_counts(
            &mut counts,
            &HealthCheck {
                node_id: node_a.clone(),
                is_healthy: false,
            },
            threshold,
        );
        assert!(alert);
        assert_eq!(counts.get(&node_a), Some(&2));
        assert_eq!(counts.get(&node_b), Some(&1));

        // Healthy report for node B should reset its counter without affecting node A.
        let alert = update_unhealthy_counts(
            &mut counts,
            &HealthCheck {
                node_id: node_b.clone(),
                is_healthy: true,
            },
            threshold,
        );
        assert!(!alert);
        assert!(!counts.contains_key(&node_b));
        assert_eq!(counts.get(&node_a), Some(&2));
    }

    #[test]
    fn health_check_round_trips_through_json() {
        let health_check = HealthCheck {
            node_id: "node-1".to_string(),
            is_healthy: false,
        };
        let json = serde_json::to_string(&health_check).unwrap();
        let decoded: HealthCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.node_id, health_check.node_id);
        assert_eq!(decoded.is_healthy, health_check.is_healthy);
    }

    #[test]
    fn unhealthy_threshold_is_always_positive() {
        // A threshold of zero would alert on the very first report; the resolver
        // must clamp invalid/zero configuration up to the default.
        assert!(unhealthy_threshold() >= 1);
    }

    #[tokio::test]
    async fn publish_heartbeats_stops_promptly_when_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Already cancelled, so the publisher must return without ever dialing
        // the (unreachable) broker.
        tokio::time::timeout(
            Duration::from_secs(1),
            publish_heartbeats(
                "amqp://127.0.0.1:1/%2f".to_string(),
                "node-1".to_string(),
                Duration::from_millis(10),
                cancel,
            ),
        )
        .await
        .expect("cancelled publisher should return promptly");
    }
}
