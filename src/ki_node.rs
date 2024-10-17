// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations, and sending outputs.

use lapin::{options::*, types::FieldTable, BasicProperties, Connection, ConnectionProperties};
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
    let amqp_addr = std::env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into());
    let connection = Connection::connect(&amqp_addr, ConnectionProperties::default()).await?;
    let channel = connection.create_channel().await?;

    // Declare the queue for receiving tasks from the An node
    let queue_name = "ki_task_queue";
    channel
        .queue_declare(
            queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await?;

    // Start consuming tasks from the queue
    let mut consumer = channel
        .basic_consume(
            queue_name,
            "ki_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    info!("Ki node is running and waiting for tasks...");

    while let Some(delivery) = consumer.next().await {
        let (_, delivery) = delivery.expect("error in consumer");
        let task_message: TaskMessage = serde_json::from_slice(&delivery.data)?;

        info!("Received task: {:?}", task_message);

        // Perform computation and generate result
        let result = perform_computation(task_message).await;

        // Send the result back to the An node
        if let Err(e) = send_result(result, &channel).await {
            error!("Failed to send result: {:?}", e);
        }

        // Acknowledge the message
        delivery.ack(BasicAckOptions::default()).await?;
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

async fn send_result(result: ResultMessage, channel: &lapin::Channel) -> Result<(), Box<dyn Error>> {
    let result_queue = "an_result_queue";

    // Serialize the result message
    let payload = serde_json::to_vec(&result)?;

    // Publish the result to the An node
    channel
        .basic_publish(
            "",
            result_queue,
            BasicPublishOptions::default(),
            &payload,
            BasicProperties::default(),
        )
        .await?;

    info!("Sent result for task ID: {}", result.task_id);
    Ok(())
}
