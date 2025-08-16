// principal.rs: Implements the specific responsibilities of the Principal, including role management and global coordination.
use futures_util::stream::StreamExt;
use lapin::{options::*, types::FieldTable, BasicProperties, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};
use std::error::Error;
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
    // Establish connection to RabbitMQ
    let amqp_addr = std::env::var("AMQP_ADDR").map_err(|e| {
        error!("Failed to read AMQP_ADDR environment variable: {:?}", e);
        e
    })?;
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
    channel
        .queue_declare(
            queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to declare queue: {:?}", e);
            e
        })?;

    // Start consuming update requests from the queue
    let mut consumer = channel
        .basic_consume(
            queue_name,
            "principal_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to start consuming: {:?}", e);
            e
        })?;

    info!("Principal node is running and waiting for update requests...");

    while let Some(delivery_result) = consumer.next().await {
        let delivery = delivery_result.map_err(|e| {
            error!("Failed to receive delivery: {:?}", e);
            e
        })?;
        let update_request: UpdateRequest =
            serde_json::from_slice(&delivery.data).map_err(|e| {
                error!("Failed to deserialize update request: {:?}", e);
                e
            })?;

        info!("Received update request: {:?}", update_request);

        // Approve or reject the update request
        if let Err(e) = process_update_request(update_request).await {
            error!("Failed to process update request: {:?}", e);
        }

        // Acknowledge the message
        delivery
            .ack(BasicAckOptions::default())
            .await
            .map_err(|e| {
                error!("Failed to acknowledge message: {:?}", e);
                e
            })?;
    }

    Ok(())
}

async fn process_update_request(update: UpdateRequest) -> Result<(), Box<dyn Error>> {
    // Placeholder for update request approval logic
    // Validate the update and apply it to the master database if approved
    info!("Processing update request with ID: {}", update.update_id);

    // For now, we just log that the update was processed
    Ok(())
}

#[allow(dead_code)]
pub async fn assign_role(
    node_id: &str,
    role: &str,
    channel: &lapin::Channel,
) -> Result<(), Box<dyn Error>> {
    let role_assignment = RoleAssignment {
        node_id: node_id.to_string(),
        role: role.to_string(),
    };

    // Serialize the role assignment
    let payload = serde_json::to_vec(&role_assignment).map_err(|e| {
        error!("Failed to serialize role assignment: {:?}", e);
        e
    })?;

    // Publish the role assignment to the role management queue
    let role_queue = "role_assignment_queue";
    channel
        .basic_publish(
            "",
            role_queue,
            BasicPublishOptions::default(),
            &payload,
            BasicProperties::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to publish role assignment: {:?}", e);
            e
        })?;

    info!("Assigned role '{}' to node '{}'", role, node_id);
    Ok(())
}
