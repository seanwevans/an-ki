// messaging.rs: Implements RabbitMQ messaging logic, including sending and receiving messages across the network.

use lapin::{options::*, types::FieldTable, BasicProperties, Channel, Connection, ConnectionProperties};
use std::error::Error;
use tracing::info;

pub async fn establish_connection(amqp_addr: &str) -> Result<Channel, Box<dyn Error>> {
    let connection = Connection::connect(amqp_addr, ConnectionProperties::default()).await?;
    let channel = connection.create_channel().await?;
    info!("Established connection to RabbitMQ at: {}", amqp_addr);
    Ok(channel)
}

pub async fn declare_queue(channel: &Channel, queue_name: &str) -> Result<(), Box<dyn Error>> {
    channel
        .queue_declare(
            queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await?;
    info!("Declared queue: {}", queue_name);
    Ok(())
}

pub async fn publish_message(channel: &Channel, queue_name: &str, payload: &[u8]) -> Result<(), Box<dyn Error>> {
    channel
        .basic_publish(
            "",
            queue_name,
            BasicPublishOptions::default(),
            payload,
            BasicProperties::default(),
        )
        .await?;
    info!("Published message to queue: {}", queue_name);
    Ok(())
}

pub async fn consume_messages(channel: &Channel, queue_name: &str, consumer_tag: &str) -> Result<lapin::Consumer, Box<dyn Error>> {
    let consumer = channel
        .basic_consume(
            queue_name,
            consumer_tag,
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    info!("Started consuming messages from queue: {}", queue_name);
    Ok(consumer)
}
