// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;

use crate::signals;
use crate::messaging::{consume_messages, declare_queue, establish_connection};
use futures_util::stream::StreamExt;
use lapin::options::{BasicAckOptions, BasicNackOptions};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::config::load_settings;
use crate::messaging;

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


    // Establish connection to RabbitMQ using configuration settings
    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;
    let connection =
        Connection::connect(&settings.amqp_addr, ConnectionProperties::default())
            .await
            .map_err(|e| {
                error!("Failed to connect to RabbitMQ: {:?}", e);
                e
            })?;

    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);

    // Read configuration values
    let amqp_addr = std::env::var("AMQP_ADDR").map_err(|e| {
        error!("Failed to read AMQP_ADDR environment variable: {:?}", e);
        e
    })?;

    let max_retries: u32 = std::env::var("AMQP_RECONNECT_ATTEMPTS")
        .unwrap_or_else(|_| "5".into())
        .parse()
        .unwrap_or(5);
    let backoff_ms: u64 = std::env::var("AMQP_RECONNECT_BACKOFF_MS")
        .unwrap_or_else(|_| "500".into())
        .parse()
        .unwrap_or(500);

    let channel = establish_connection(&amqp_addr).await?;
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

    let consumer_tag = "an_consumer";

    let mut attempts = 0u32;

    loop {
        // Establish connection and consumer using messaging helpers
        let (_, mut consumer) = match setup_consumer(&amqp_addr, queue_name, consumer_tag).await {
            Ok(c) => {
                attempts = 0; // reset attempts after successful connection
                c
            }
            Err(e) => {
                attempts += 1;
                error!(
                    "Failed to establish connection: {:?}. Attempt {}/{}",
                    e, attempts, max_retries
                );
                if attempts >= max_retries {
                    error!("Exceeded maximum reconnection attempts. Exiting.");
                    return Err(e);
                }
                let delay = backoff_ms * 2u64.pow(attempts - 1);
                sleep(Duration::from_millis(delay)).await;
                continue;
            }
        };
    declare_queue(&channel, queue_name).await?;

    // Start consuming tasks from the queue
    let mut consumer = consume_messages(&channel, queue_name, "an_consumer").await?;
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
                        // Process the task (distribute to Ki nodes or handle locally)
                        if let Err(e) = process_task(task_message).await {
                            error!("Failed to process task: {:?}", e);
                        }
        info!("An node is running and waiting for tasks...");

        loop {
            match consumer.next().await {
                Some(Ok(delivery)) => {
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
                    None => break,
                }
                Some(Err(e)) => {
                    error!("Error in consumer stream: {:?}", e);
                    break;
                }
                None => {
                    error!("Consumer stream closed");
                    break;
                }
            }
        }

        // If we reach here, consumer encountered an error or closed; retry connection
        attempts += 1;
        if attempts > max_retries {
            error!("Failed to reconnect after {} attempts", max_retries);
            return Err("reconnection attempts exceeded".into());
        }
        let delay = backoff_ms * 2u64.pow(attempts - 1);
        sleep(Duration::from_millis(delay)).await;
    }
}


    if let Err(e) = channel.close(200, "Bye").await {
        error!("Failed to close channel: {:?}", e);
    }
    if let Err(e) = connection.close(200, "Bye").await {
        error!("Failed to close connection: {:?}", e);
    }

    Ok(())

async fn setup_consumer(
    amqp_addr: &str,
    queue_name: &str,
    consumer_tag: &str,
) -> Result<(lapin::Channel, lapin::Consumer), Box<dyn Error>> {
    let channel = messaging::establish_connection(amqp_addr).await?;
    messaging::declare_queue(&channel, queue_name).await?;
    let consumer = messaging::consume_messages(&channel, queue_name, consumer_tag).await?;
    Ok((channel, consumer))
}

async fn process_task(task: TaskMessage) -> Result<(), Box<dyn Error>> {
    // Placeholder for task processing logic
    // This is where you would distribute tasks to Ki nodes or handle them locally
    info!("Processing task with ID: {}", task.task_id);

    // For now, we just log that the task is processed
    Ok(())
}
