// principal.rs: Implements the specific responsibilities of the Principal, including role management and global coordination.

use crate::messaging::{consume_messages, declare_queue};
use crate::signals;

use crate::config::load_settings;
use futures_util::stream::StreamExt;
use lapin::{options::BasicAckOptions, Channel, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::sync::oneshot;
use tracing::{error, info};

#[derive(Serialize, Deserialize, Debug)]
struct RoleAssignment {
    node_id: String,
    role: String,
}

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
        .map_err(|e| Box::<dyn Error>::from(e))?;

    // Start consuming update requests from the queue
    let mut consumer = consume_messages(&channel, queue_name, "principal_consumer")
        .await
        .map_err(|e| Box::<dyn Error>::from(e))?;

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
                        let update_request: UpdateRequest = serde_json::from_slice(&delivery.data).map_err(|e| {
                            error!("Failed to deserialize update request: {:?}", e);
                            e
                        })?;

                        info!("Received update request: {:?}", update_request);

                        if let Err(e) = process_update_request(update_request).await {
                            error!("Failed to process update request: {:?}", e);
                        }

                        delivery
                            .ack(BasicAckOptions::default())
                            .await
                            .map_err(|e| {
                                error!("Failed to acknowledge message: {:?}", e);
                                e
                            })?;
                    }
                    Some(Err(e)) => {
                        error!("Failed to receive delivery: {:?}", e);
                    }
                    None => break,
                }
            }
        }
    }

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
        UpdateContent::Database { statement } => {
            if statement.trim().is_empty() {
                error!("Database update validation failed: empty statement");
                return Err("Invalid database statement".into());
            }
            apply_database_update(&statement)?;
            info!("Database update applied successfully");
        }
        UpdateContent::ConfigReload { key, value } => {
            if key.trim().is_empty() {
                error!("Config reload validation failed: empty key");
                return Err("Invalid config key".into());
            }
            broadcast_config_reload(&key, &value)?;
            info!("Configuration reload broadcast successfully");
        }
    }

    Ok(())
}

fn apply_database_update(statement: &str) -> Result<(), Box<dyn Error>> {
    info!("Applying database statement: {}", statement);
    // Placeholder for actual database interaction
    Ok(())
}

fn broadcast_config_reload(key: &str, value: &str) -> Result<(), Box<dyn Error>> {
    info!("Broadcasting config reload for {}={} ", key, value);
    // Placeholder for broadcasting configuration changes to nodes
    Ok(())
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
    async fn process_update_request_database_success() {
        let update = UpdateRequest {
            update_id: "1".into(),
            content: UpdateContent::Database {
                statement: "UPDATE table SET value = 1".into(),
            },
        };

        assert!(process_update_request(update).await.is_ok());
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
    async fn process_update_request_config_success() {
        let update = UpdateRequest {
            update_id: "3".into(),
            content: UpdateContent::ConfigReload {
                key: "threshold".into(),
                value: "10".into(),
            },
        };

        assert!(process_update_request(update).await.is_ok());
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
}
