// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations, and sending outputs.

use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicPublishOptions},
    BasicProperties,
};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::messaging;

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

    let queue_name = "ki_task_queue";
    let consumer_tag = "ki_consumer";

    let mut attempts = 0u32;

    loop {
        // Establish connection and consumer using messaging helpers
        let (channel, mut consumer) =
            match setup_consumer(&amqp_addr, queue_name, consumer_tag).await {
                Ok(c) => {
                    attempts = 0; // reset attempts on success
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

        info!("Ki node is running and waiting for tasks...");

        loop {
            match consumer.next().await {
                Some(Ok(delivery)) => {
                    let task_message: TaskMessage = serde_json::from_slice(&delivery.data)
                        .map_err(|e| {
                            error!("Failed to deserialize task message: {:?}", e);
                            e
                        })?;

                    info!("Received task: {:?}", task_message);

                    // Perform computation and generate result
                    let result = perform_computation(task_message).await;

                    // Send the result back to the An node
                    if let Err(e) = send_result(result, &channel).await {
                        error!("Failed to send result: {:?}", e);
                    }

                    // Acknowledge the message
                    if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                        error!("Failed to acknowledge message: {:?}", e);
                    }
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

        // Consumer ended; attempt to reconnect
        attempts += 1;
        if attempts > max_retries {
            error!("Failed to reconnect after {} attempts", max_retries);
            return Err("reconnection attempts exceeded".into());
        }
        let delay = backoff_ms * 2u64.pow(attempts - 1);
        sleep(Duration::from_millis(delay)).await;
    }
}

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
