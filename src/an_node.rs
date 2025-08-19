// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;

use crate::signals;
use futures_util::stream::StreamExt;
use lapin::{options::*, types::FieldTable, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{error, info};

#[derive(Serialize, Deserialize, Debug)]
struct TaskMessage {
    task_id: String,
    data: String,
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

    // Declare the queue for receiving tasks from the principal
    let queue_name = "an_task_queue";
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

    // Start consuming tasks from the queue
    let mut consumer = channel
        .basic_consume(
            queue_name,
            "an_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to start consuming: {:?}", e);
            e
        })?;

    info!("An node is running and waiting for tasks...");

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received, stopping An node...");
                break;
            }
            result = consumer.next() => {
                match result {
                    Some(Ok(delivery)) => {
                        match serde_json::from_slice::<TaskMessage>(&delivery.data) {
                            Ok(task_message) => {
                                info!("Received task: {:?}", task_message);

                                if let Err(e) = process_task(task_message).await {
                                    error!("Failed to process task: {:?}", e);
                                }

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
                    Some(Err(e)) => {
                        error!("Error in consumer: {:?}", e);
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

async fn process_task(task: TaskMessage) -> Result<(), Box<dyn Error>> {
    // Placeholder for task processing logic
    // This is where you would distribute tasks to Ki nodes or handle them locally
    info!("Processing task with ID: {}", task.task_id);

    // For now, we just log that the task is processed
    Ok(())
}
