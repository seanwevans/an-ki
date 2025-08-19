// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations, and sending outputs.

use crate::messaging::{consume_messages, declare_queue, establish_connection, publish_message};
use futures_util::stream::StreamExt;
use lapin::options::BasicAckOptions;
use serde::{Deserialize, Serialize};
use std::error::Error;
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
    // Establish connection to RabbitMQ
    let amqp_addr = std::env::var("AMQP_ADDR").map_err(|e| {
        error!("Failed to read AMQP_ADDR environment variable: {:?}", e);
        e
    })?;
    let channel = establish_connection(&amqp_addr).await?;

    // Declare the queue for receiving tasks from the An node
    let queue_name = "ki_task_queue";
    declare_queue(&channel, queue_name).await?;

    // Start consuming tasks from the queue
    let mut consumer = consume_messages(&channel, queue_name, "ki_consumer").await?;

    info!("Ki node is running and waiting for tasks...");

    while let Some(delivery_result) = consumer.next().await {
        let delivery = delivery_result.map_err(|e| {
            error!("Failed to receive delivery: {:?}", e);
            e
        })?;
        let task_message: TaskMessage = serde_json::from_slice(&delivery.data).map_err(|e| {
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

    publish_message(channel, result_queue, &payload).await?;

    info!("Sent result for task ID: {}", result.task_id);
    Ok(())
}
