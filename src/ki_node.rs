// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations, and sending outputs.

use crate::signals;
use futures_util::stream::StreamExt;
use lapin::{options::*, types::FieldTable, BasicProperties, Connection, ConnectionProperties};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::sync::oneshot;
use tracing::{error, info};

#[derive(Serialize, Deserialize, Debug)]
struct TaskMessage {
    task_id: String,
    data: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ResultMessage {
    task_id: String,
    result: String,
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

    // Declare the queue for receiving tasks from the An node
    let queue_name = "ki_task_queue";
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
            "ki_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to start consuming: {:?}", e);
            e
        })?;

    info!("Ki node is running and waiting for tasks...");

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received, stopping Ki node...");
                break;
            }
            delivery_result = consumer.next() => {
                match delivery_result {
                    Some(Ok(delivery)) => {
                        let task_message: TaskMessage = serde_json::from_slice(&delivery.data).map_err(|e| {
                            error!("Failed to deserialize task message: {:?}", e);
                            e
                        })?;

                        info!("Received task: {:?}", task_message);

                        let result = perform_computation(task_message).await;

                        if let Err(e) = send_result(result, &channel).await {
                            error!("Failed to send result: {:?}", e);
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

async fn perform_computation(task: TaskMessage) -> ResultMessage {
    // Placeholder for the computation logic
    // Simulate some processing based on the input data
    info!("Performing computation for task ID: {}", task.task_id);
    let computed_result = format!("Processed data: {}", task.data);

    ResultMessage {
        task_id: task.task_id,
        result: computed_result,
    }
}

async fn send_result(
    result: ResultMessage,
    channel: &lapin::Channel,
) -> Result<(), Box<dyn Error>> {
    let result_queue = "an_result_queue";

    // Serialize the result message
    let payload = serde_json::to_vec(&result).map_err(|e| {
        error!("Failed to serialize result: {:?}", e);
        e
    })?;

    // Publish the result to the An node
    channel
        .basic_publish(
            "",
            result_queue,
            BasicPublishOptions::default(),
            &payload,
            BasicProperties::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to publish result: {:?}", e);
            e
        })?;

    info!("Sent result for task ID: {}", result.task_id);
    Ok(())
}
