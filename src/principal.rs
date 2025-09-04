// principal.rs: Implements the specific responsibilities of the Principal, including role management and global coordination.

use crate::messaging::{consume_messages, declare_queue, publish_message};
use crate::signals;

use crate::config::load_settings;
use futures_util::stream::StreamExt;
use lapin::{options::BasicAckOptions, Channel, Connection, ConnectionProperties};
use std::error::Error;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{error, info};

#[derive(Serialize, Deserialize, Debug)]
struct RoleAssignment {
    node_id: String,
    role: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct UpdateRequest {
    update_id: String,
    content: String,
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
    // Placeholder for update request approval logic
    // Validate the update and apply it to the master database if approved
    info!("Processing update request with ID: {}", update.update_id);    
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
