// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use lapin::{options::*, types::FieldTable, BasicProperties, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tracing::{error, info};
use futures_util::stream::StreamExt;

#[derive(Serialize, Deserialize, Debug)]
struct TaskMessage {
    task_id: String,
    data: String,
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    // Establish connection to RabbitMQ
    let amqp_addr = std::env::var("AMQP_ADDR").map_err(|e| {
        error!("Failed to read AMQP_ADDR environment variable: {:?}", e);
        e
    })?;
    let connection = Connection::connect(&amqp_addr, ConnectionProperties::default()).await.map_err(|e| {
        error!("Failed to connect to RabbitMQ: {:?}", e);
        e
    })?;
    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);
        e
    })?;

    // Declare the queue for receiving tasks from the principal
    let queue_name = "an_task_queue";
    channel
        .queue_declare(
            queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await.map_err(|e| {
            error!("Failed to declare queue: {:?}", e);
            e
        })?;

    // Start consuming tasks from the queue
    let mut consumer = channel
        .basic_consume(
            queue_name,
            "an_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await.map_err(|e| {
            error!("Failed to start consuming: {:?}", e);
            e
        })?;

    info!("An node is running and waiting for tasks...");

    while let Some(result) = consumer.next().await {
        match result {
            Ok((_, delivery)) => {
                match serde_json::from_slice::<TaskMessage>(&delivery.data) {
                    Ok(task_message) => {
                        info!("Received task: {:?}", task_message);

                        // Process the task (distribute to Ki nodes or handle locally)
                        if let Err(e) = process_task(task_message).await {
                            error!("Failed to process task: {:?}", e);
                        }

                        // Acknowledge the message
                        if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                            error!("Failed to acknowledge message: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to deserialize task message: {:?}", e);
                        if let Err(e) = delivery.nack(BasicNackOptions::default()).await {
                            error!("Failed to negatively acknowledge message: {:?}", e);
                        }
                    }
                }
            }
            Err(e) => {
                error!("Error in consumer: {:?}", e);
            }
        }
    }

    Ok(())
}

async fn process_task(task: TaskMessage) -> Result<(), Box<dyn Error>> {
    // Placeholder for task processing logic
    // This is where you would distribute tasks to Ki nodes or handle them locally
    info!("Processing task with ID: {}", task.task_id);

    // For now, we just log that the task is processed
    Ok(())
}
