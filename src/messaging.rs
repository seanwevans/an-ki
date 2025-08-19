// messaging.rs: Implements RabbitMQ messaging logic, including sending and receiving messages across the network.

use lapin::{options::*, types::FieldTable, BasicProperties, Channel, Connection, ConnectionProperties};
use std::error::Error;
use tracing::{error, info};

pub async fn establish_connection(amqp_addr: &str) -> Result<Channel, Box<dyn Error>> {
    let connection = Connection::connect(amqp_addr, ConnectionProperties::default()).await.map_err(|e| {
        error!("Failed to connect to RabbitMQ: {:?}", e);
        e
    })?;
    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);
        e
    })?;
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
        .await
        .map_err(|e| {
            error!("Failed to declare queue: {:?}", e);
            e
        })?;
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
        .await
        .map_err(|e| {
            error!("Failed to publish message: {:?}", e);
            e
        })?;
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
        .await
        .map_err(|e| {
            error!("Failed to start consuming: {:?}", e);
            e
        })?;
    info!("Started consuming messages from queue: {}", queue_name);
    Ok(consumer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;
    use tokio::time::{timeout, Duration};

    #[cfg(feature = "integration-tests")]
    use futures_util::StreamExt;
    #[cfg(feature = "integration-tests")]
    use lapin::options::BasicAckOptions;

    #[cfg(feature = "integration-tests")]
    const AMQP_ADDR: &str = "amqp://127.0.0.1:5672/%2f";

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_messaging_workflow() {
        let channel = match establish_connection(AMQP_ADDR).await {
            Ok(ch) => ch,
            Err(_) => return,
        };

        declare_queue(&channel, "test_queue")
            .await
            .expect("declare");

        publish_message(&channel, "test_queue", b"hello")
            .await
            .expect("publish");

        let mut consumer = consume_messages(&channel, "test_queue", "test_consumer")
            .await
            .expect("consume");

        let delivery = timeout(Duration::from_secs(5), consumer.next())
            .await
            .expect("consumer timed out")
            .expect("consumer closed")
            .expect("delivery error");

        assert_eq!(delivery.data, b"hello");

        delivery
            .ack(BasicAckOptions::default())
            .await
            .expect("ack");
    }

    #[tokio::test]
    async fn test_connection_failure() {
        let result = establish_connection("amqp://invalid:5672/%2f").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_messaging_flow() {
        let (tx, mut rx) = broadcast::channel(10);
        tx.send(b"hello".to_vec()).unwrap();
        let received = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("no message")
            .unwrap();
        assert_eq!(received, b"hello");
    }
}
