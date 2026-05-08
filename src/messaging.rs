// messaging.rs: Implements RabbitMQ messaging logic, including sending and receiving messages across the network.

use crate::error::AnKiError;
use lapin::{
    options::*,
    publisher_confirm::{Confirmation, PublisherConfirm},
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use std::error::Error;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

pub async fn establish_connection(amqp_addr: &str) -> Result<Channel, AnKiError> {
    let connection = Connection::connect(amqp_addr, ConnectionProperties::default())
        .await
        .map_err(|e| {
            error!("Failed to connect to RabbitMQ: {:?}", e);
            AnKiError::Messaging(e.to_string())
        })?;
    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);
        AnKiError::Messaging(e.to_string())
    })?;
    channel
        .confirm_select(ConfirmSelectOptions::default())
        .await
        .map_err(|e| {
            error!("Failed to enable publisher confirms: {:?}", e);
            AnKiError::Messaging(e.to_string())
        })?;
    info!("Established connection to RabbitMQ at: {}", amqp_addr);
    Ok(channel)
}

pub async fn declare_queue(channel: &Channel, queue_name: &str) -> Result<(), AnKiError> {
    channel
        .queue_declare(
            queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .map_err(|e| {
            error!("Failed to declare queue: {:?}", e);
            AnKiError::Messaging(e.to_string())
        })?;
    info!("Declared queue: {}", queue_name);
    Ok(())
}

pub async fn publish_message(
    channel: &Channel,
    queue_name: &str,
    payload: &[u8],
) -> Result<(), AnKiError> {
    let confirm: PublisherConfirm = channel
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
            AnKiError::Messaging(e.to_string())
        })?;

    match confirm.await.map_err(|e| {
        error!("Failed to confirm published message: {:?}", e);
        AnKiError::Messaging(e.to_string())
    })? {
        Confirmation::Ack(_) => {
            info!("Published message to queue: {}", queue_name);
            Ok(())
        }
        Confirmation::Nack(_) => {
            let msg = format!("RabbitMQ negatively acknowledged publish to queue: {queue_name}");
            error!("{}", msg);
            Err(AnKiError::Messaging(msg))
        }
        Confirmation::NotRequested => {
            let msg = format!(
                "RabbitMQ publisher confirms were not enabled for publish to queue: {queue_name}"
            );
            error!("{}", msg);
            Err(AnKiError::Messaging(msg))
        }
    }
}

pub async fn consume_messages(
    channel: &Channel,
    queue_name: &str,
    consumer_tag: &str,
) -> Result<lapin::Consumer, AnKiError> {
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
            AnKiError::Messaging(e.to_string())
        })?;
    info!("Started consuming messages from queue: {}", queue_name);
    Ok(consumer)
}

pub async fn connect_with_retries(
    amqp_addr: &str,
    queue_name: &str,
    consumer_tag: &str,
    max_retries: u32,
    backoff_ms: u64,
    max_delay_ms: u64,
) -> Result<(Channel, lapin::Consumer), Box<dyn Error>> {
    if max_retries == 0 {
        return Err(Box::new(AnKiError::Messaging(
            "AMQP reconnect attempts must be >= 1".to_string(),
        )));
    }

    for attempt in 1..=max_retries {
        let result = async {
            let channel = establish_connection(amqp_addr).await?;
            declare_queue(&channel, queue_name).await?;
            let consumer = consume_messages(&channel, queue_name, consumer_tag).await?;
            Ok::<_, Box<dyn Error>>((channel, consumer))
        }
        .await;

        match result {
            Ok(res) => return Ok(res),
            Err(e) => {
                error!(
                    "Failed to establish AMQP connection for queue '{}' (attempt {}/{}): {:?}",
                    queue_name, attempt, max_retries, e
                );
                if attempt == max_retries {
                    error!(
                        "Exhausted AMQP connection attempts for queue '{}' after {} total attempt(s).",
                        queue_name, max_retries
                    );
                    return Err(e);
                }
                let attempt_factor = attempt.saturating_sub(1);
                let multiplier = 2u64.saturating_pow(attempt_factor);
                let delay = backoff_ms.saturating_mul(multiplier).min(max_delay_ms);
                info!(
                    "Retrying AMQP connection for queue '{}' in {} ms (next attempt {}/{}).",
                    queue_name,
                    delay,
                    attempt + 1,
                    max_retries
                );
                sleep(Duration::from_millis(delay)).await;
            }
        }
    }

    Err(Box::new(AnKiError::Messaging(
        "AMQP connection retry loop exited unexpectedly".to_string(),
    )))
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

        delivery.ack(BasicAckOptions::default()).await.expect("ack");
    }

    #[tokio::test]
    async fn test_connection_failure() {
        let result = establish_connection("amqp://invalid:5672/%2f").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_with_retries_failure() {
        let result =
            connect_with_retries("amqp://invalid:5672/%2f", "queue", "tag", 3, 10, 20).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_with_retries_hits_max_delay_without_panic() {
        let result = connect_with_retries("amqp://invalid:5672/%2f", "queue", "tag", 5, 4, 8).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connect_with_retries_rejects_zero_attempts() {
        let result =
            connect_with_retries("amqp://127.0.0.1:1/%2f", "queue", "tag", 0, 10, 20).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("must be >= 1"));
    }

    #[tokio::test]
    async fn test_connect_with_retries_one_attempt_has_no_retry_sleep() {
        let start = std::time::Instant::now();
        let result =
            connect_with_retries("amqp://127.0.0.1:1/%2f", "queue", "tag", 1, 5_000, 5_000).await;
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn test_connect_with_retries_high_attempt_count_still_fails_cleanly() {
        let result = connect_with_retries("amqp://127.0.0.1:1/%2f", "queue", "tag", 20, 0, 0).await;
        assert!(result.is_err());
    }

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_connect_with_retries_success() {
        let res =
            connect_with_retries(AMQP_ADDR, "test_queue_retry", "test_tag", 1, 10, 10_000).await;
        match res {
            Ok((channel, consumer)) => {
                // drop consumer to close
                drop(consumer);
                drop(channel);
            }
            Err(_) => return, // skip if RabbitMQ not available
        }
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
