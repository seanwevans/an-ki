// health.rs: Implements health checks and a heartbeat mechanism for monitoring node health.

use crate::common::NodeRole;
use crate::error::AnKiError;
use crate::messaging::{consume_messages, declare_queue, establish_connection, publish_message};
use crate::node_registry::NodeRegistry;
use futures_util::stream::StreamExt;
use lapin::{options::BasicAckOptions, Channel};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Queue that nodes publish heartbeats to and the principal consumes from.
pub const HEARTBEAT_QUEUE: &str = "heartbeat_queue";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    pub node_id: String,
    /// Role the sending node is serving. The principal uses this to build its
    /// view of the cluster without a separate registration handshake.
    pub role: NodeRole,
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
    role: NodeRole,
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
                    role: role.clone(),
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

/// Consumes heartbeats from [`HEARTBEAT_QUEUE`], maintains the cluster view in
/// `registry`, and tracks per-node health, alerting when a node exceeds
/// `unhealthy_threshold` consecutive bad reports.
///
/// Each heartbeat both refreshes the sender's entry in the registry and feeds
/// the unhealthy-streak counter. A node that stops heartbeating is evicted from
/// the registry once it has been silent for `ttl`; a node that keeps
/// heartbeating but reports itself unhealthy stays registered and alerts
/// instead, since it is still reachable.
///
/// Runs until `cancel` fires or the consumer stream closes.
pub async fn run_health_monitor(
    channel: Channel,
    registry: NodeRegistry,
    unhealthy_threshold: u32,
    ttl: Duration,
    cancel: CancellationToken,
) -> Result<(), AnKiError> {
    declare_queue(&channel, HEARTBEAT_QUEUE).await?;
    let mut consumer =
        consume_messages(&channel, HEARTBEAT_QUEUE, "principal_health_consumer").await?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    // Sweep often enough that an evicted node is noticed well within its TTL
    // rather than up to a full TTL late.
    let mut sweep = time::interval(sweep_interval(ttl));
    info!(
        "Principal health monitor consuming heartbeats (unhealthy_threshold={}, node_ttl={:?})",
        unhealthy_threshold, ttl
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Health monitor cancelled");
                break;
            }
            _ = sweep.tick() => {
                for node_id in registry.prune_stale(ttl).await {
                    // The node is gone, so its unhealthy streak is meaningless;
                    // if it comes back it should start from a clean slate.
                    counts.remove(&node_id);
                }
            }
            delivery = consumer.next() => {
                match delivery {
                    Some(Ok(delivery)) => {
                        match serde_json::from_slice::<HealthCheck>(&delivery.data) {
                            Ok(health_check) => {
                                registry
                                    .record_heartbeat(&health_check.node_id, health_check.role.clone())
                                    .await;
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

/// How often the monitor sweeps for stale nodes: a third of the TTL, clamped to
/// at least a second so a tiny configured TTL cannot spin the loop.
fn sweep_interval(ttl: Duration) -> Duration {
    (ttl / 3).max(Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::time::Duration;

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
                role: NodeRole::Ki,
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
                role: NodeRole::Ki,
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
                role: NodeRole::Ki,
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
                role: NodeRole::Ki,
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
            role: NodeRole::An,
            is_healthy: false,
        };
        let json = serde_json::to_string(&health_check).unwrap();
        let decoded: HealthCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.node_id, health_check.node_id);
        assert_eq!(decoded.role, health_check.role);
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
                NodeRole::Ki,
                Duration::from_millis(10),
                cancel,
            ),
        )
        .await
        .expect("cancelled publisher should return promptly");
    }
}
