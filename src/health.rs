// health.rs: Implements health checks and a heartbeat mechanism for monitoring node health.

use std::time::Duration;
use tokio::time;
use tokio::sync::broadcast;
use tracing::{info, error};
use std::error::Error;

#[derive(Clone, Debug)]
pub struct HealthCheck {
    pub node_id: String,
    pub is_healthy: bool,
}

pub async fn start_heartbeat(interval: Duration, tx: broadcast::Sender<HealthCheck>, node_id: String) {
    let mut ticker = time::interval(interval);

    loop {
        ticker.tick().await;
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
}

pub async fn monitor_health(mut rx: broadcast::Receiver<HealthCheck>, unhealthy_threshold: u32) -> Result<(), Box<dyn Error>> {
    let mut unhealthy_count = 0;

    loop {
        match rx.recv().await {
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
            Err(e) => {
                error!("Failed to receive heartbeat: {:?}", e);
            }
        }
    }
}
