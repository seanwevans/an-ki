// principal.rs: Implements the specific responsibilities of the Principal, including role management and global coordination.

use crate::health;
use crate::messaging::{consume_messages, declare_queue};
use crate::signals;

use crate::config::load_settings;
use futures_util::stream::StreamExt;
use lapin::{
    message::Delivery,
    options::{BasicAckOptions, BasicNackOptions},
    Channel, Connection, ConnectionProperties,
};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case", content = "data")]
enum UpdateContent {
    Database { statement: String },
    ConfigReload { key: String, value: String },
}

#[derive(Serialize, Deserialize, Debug)]
struct UpdateRequest {
    update_id: String,
    content: UpdateContent,
}

fn decode_update_request(payload: &[u8]) -> Option<UpdateRequest> {
    match serde_json::from_slice::<UpdateRequest>(payload) {
        Ok(update_request) => Some(update_request),
        Err(e) => {
            error!(
                "Failed to deserialize update request payload. error={:?}, payload={}",
                e,
                String::from_utf8_lossy(payload)
            );
            None
        }
    }
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if let Err(e) = signals::setup_unix_signal_handlers().await {
        error!("Failed to set up Unix signal handlers: {:?}", e);
    }

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(e) = signals::setup_signal_handler().await {
            error!("Signal handler error: {:?}", e);
        }
        let _ = shutdown_tx.send(());
    });

    // Establish connection to RabbitMQ
    // Load configuration and establish connection to RabbitMQ
    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;

    let amqp_addr = std::env::var("AMQP_ADDR").unwrap_or(settings.amqp_addr.clone());

    let connection = Connection::connect(&amqp_addr, ConnectionProperties::default())
        .await
        .map_err(|e| {
            error!("Failed to connect to RabbitMQ: {:?}", e);
            e
        })?;
    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);
        e
    })?;

    // Declare the queue for receiving update requests from An nodes
    let queue_name = "principal_update_queue";
    declare_queue(&channel, queue_name)
        .await
        .map_err(Box::<dyn Error>::from)?;

    // Start consuming update requests from the queue
    let mut consumer = consume_messages(&channel, queue_name, "principal_consumer")
        .await
        .map_err(Box::<dyn Error>::from)?;

    // Monitor cluster health by consuming node heartbeats on a dedicated channel.
    let health_cancel = CancellationToken::new();
    match connection.create_channel().await {
        Ok(health_channel) => {
            let threshold = health::unhealthy_threshold();
            let token = health_cancel.clone();
            tokio::spawn(async move {
                if let Err(e) = health::run_health_monitor(health_channel, threshold, token).await {
                    error!("Health monitor exited with error: {:?}", e);
                }
            });
        }
        Err(e) => error!("Failed to open health-monitor channel: {:?}", e),
    }

    info!("Principal node is running and waiting for update requests...");

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received, stopping Principal node...");
                break;
            }
            delivery_result = consumer.next() => {
                match delivery_result {
                    Some(Ok(delivery)) => {
                        let Some(update_request) = decode_update_request(&delivery.data) else {
                            // Queue policy: negatively acknowledge malformed payloads without
                            // requeueing so the broker can dead-letter or drop poison messages.
                            nack_for_dead_letter(&delivery).await?;
                            continue;
                        };

                        info!("Received update request: {:?}", update_request);

                        match process_update_request(update_request).await {
                            Ok(()) => {
                                delivery
                                    .ack(BasicAckOptions::default())
                                    .await
                                    .map_err(|e| {
                                        error!("Failed to acknowledge successful update: {:?}", e);
                                        e
                                    })?;
                            }
                            Err(e) => {
                                error!("Failed to process update request: {:?}", e);
                                nack_for_dead_letter(&delivery).await?;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!("Failed to receive delivery (unrecoverable): {:?}", e);
                        return Err(Box::new(e));
                    }
                    None => break,
                }
            }
        }
    }

    // Stop the health monitor before tearing down the connection.
    health_cancel.cancel();

    if let Err(e) = channel.close(200, "Bye").await {
        error!("Failed to close channel: {:?}", e);
    }
    if let Err(e) = connection.close(200, "Bye").await {
        error!("Failed to close connection: {:?}", e);
    }

    Ok(())
}

async fn process_update_request(update: UpdateRequest) -> Result<(), Box<dyn Error>> {
    info!("Processing update request with ID: {}", update.update_id);

    match update.content {
        UpdateContent::Database { statement } => reject_database_update(&statement),
        UpdateContent::ConfigReload { key, value } => reject_config_reload(&key, &value),
    }
}

async fn nack_for_dead_letter(delivery: &Delivery) -> Result<(), lapin::Error> {
    delivery
        .nack(BasicNackOptions {
            multiple: false,
            requeue: false,
        })
        .await
        .map_err(|e| {
            error!("Failed to negatively acknowledge failed update: {:?}", e);
            e
        })
}

fn reject_database_update(statement: &str) -> Result<(), Box<dyn Error>> {
    if statement.trim().is_empty() {
        error!("Database update validation failed: empty statement");
        return Err("Invalid database statement".into());
    }

    error!(
        "Database update requests are unsupported until principal database execution is implemented"
    );
    Err("Database update requests are unsupported".into())
}

fn reject_config_reload(key: &str, value: &str) -> Result<(), Box<dyn Error>> {
    if key.trim().is_empty() {
        error!("Config reload validation failed: empty key");
        return Err("Invalid config key".into());
    }

    error!(
        "Config reload request for {}={} is unsupported until node queue publication is implemented",
        key, value
    );
    Err("Config reload requests are unsupported".into())
}

#[allow(dead_code)]
pub async fn assign_role(
    node_id: &str,
    role: &str,
    _channel: &Channel,
) -> Result<(), Box<dyn Error>> {
    info!("Assigned role '{}' to node '{}'", role, node_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_update_request_database_unsupported_is_not_successful() {
        let update = UpdateRequest {
            update_id: "1".into(),
            content: UpdateContent::Database {
                statement: "UPDATE table SET value = 1".into(),
            },
        };

        let result = process_update_request(update).await;

        assert!(
            result.is_err(),
            "unsupported database updates must not be reported as successful"
        );
    }

    #[tokio::test]
    async fn process_update_request_database_failure() {
        let update = UpdateRequest {
            update_id: "2".into(),
            content: UpdateContent::Database {
                statement: "   ".into(),
            },
        };

        assert!(process_update_request(update).await.is_err());
    }

    #[tokio::test]
    async fn process_update_request_config_unsupported_is_not_successful() {
        let update = UpdateRequest {
            update_id: "3".into(),
            content: UpdateContent::ConfigReload {
                key: "threshold".into(),
                value: "10".into(),
            },
        };

        let result = process_update_request(update).await;

        assert!(
            result.is_err(),
            "unsupported config reloads must not be reported as successful"
        );
    }

    #[tokio::test]
    async fn process_update_request_config_failure() {
        let update = UpdateRequest {
            update_id: "4".into(),
            content: UpdateContent::ConfigReload {
                key: "".into(),
                value: "10".into(),
            },
        };

        assert!(process_update_request(update).await.is_err());
    }

    #[test]
    fn decode_update_request_invalid_payload_does_not_block_following_payload() {
        let invalid_payload =
            br#"{"update_id": "broken", "content": {"type":"database","data":{"statement":"X"}}"#;
        let valid_payload =
            br#"{"update_id":"5","content":{"type":"database","data":{"statement":"UPDATE t SET v=2"}}}"#;

        let first = decode_update_request(invalid_payload);
        let second = decode_update_request(valid_payload);

        assert!(first.is_none(), "invalid payload should be dropped");
        assert!(
            second.is_some(),
            "subsequent valid payload should still be decodable"
        );
    }
}
