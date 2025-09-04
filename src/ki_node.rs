// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations,
// and sending outputs.

use crate::common::{Task, TaskType};
use crate::config::load_settings;
use crate::messaging;
use crate::signals;
use futures_util::stream::StreamExt;
use lapin::{options::BasicAckOptions, Channel, Consumer};
use std::error::Error;
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

#[cfg(feature = "tch")]
use tch::Tensor;

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

    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;

    let amqp_addr = settings.amqp_addr;
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
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("Shutdown signal received, stopping Ki node...");
                    return Ok(());
                }
                delivery = consumer.next() => {
                    match delivery {
                        Some(Ok(delivery)) => {
                            let task_message: Task = serde_json::from_slice(&delivery.data).map_err(|e| {
                                error!("Failed to deserialize task message: {:?}", e);
                                e
                            })?;

                            info!("Received task: {:?}", task_message);

                            match perform_computation(task_message).await {
                                Ok(result) => {
                                    if let Err(e) = send_result(result, &channel).await {
                                        error!("Failed to send result: {:?}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Computation failed: {:?}", e);
                                }
                            }

                            if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                                error!("Failed to acknowledge message: {:?}", e);
                            }
                        }
                        Some(Err(e)) => {
                            error!("Error in consumer stream: {:?}", e);
                            break; // reconnect
                        }
                        None => {
                            error!("Consumer stream closed");
                            break; // reconnect
                        }
                    }
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
) -> Result<(Channel, Consumer), Box<dyn Error>> {
    let channel = messaging::establish_connection(amqp_addr)
        .await
        .map_err(|e| Box::<dyn Error>::from(e))?;
    messaging::declare_queue(&channel, queue_name)
        .await
        .map_err(|e| Box::<dyn Error>::from(e))?;
    let consumer = messaging::consume_messages(&channel, queue_name, consumer_tag)
        .await
        .map_err(|e| Box::<dyn Error>::from(e))?;
    Ok((channel, consumer))
}

async fn perform_computation(task: Task) -> Result<Task, Box<dyn Error>> {
    match task.task_type {
        TaskType::GradientUpdate => {
            info!("Performing computation for task ID: {}", task.task_id);
            #[cfg(feature = "tch")]
            {
                let input: Vec<f32> = serde_json::from_str(&task.data)?;
                let tensor = Tensor::of_slice(&input);
                let grad_tensor = &tensor * 2.0;
                let grad: Vec<f32> = Vec::<f32>::from(grad_tensor);
                let data = serde_json::to_string(&grad)?;
                Ok(Task {
                    task_id: task.task_id,
                    task_type: TaskType::GradientUpdate,
                    data,
                })
            }
            #[cfg(not(feature = "tch"))]
            {
                Ok(Task {
                    task_id: task.task_id,
                    task_type: TaskType::GradientUpdate,
                    data: format!("Processed data: {}", task.data),
                })
            }
        }
        TaskType::ParameterPull => {
            info!("Received parameter pull task ID: {}", task.task_id);
            Ok(Task {
                task_id: task.task_id,
                task_type: TaskType::ParameterPull,
                data: task.data,
            })
        }
    }
}

async fn send_result(result: Task, channel: &Channel) -> Result<(), Box<dyn Error>> {
    let result_queue = "an_task_queue";
    let payload = serde_json::to_vec(&result).map_err(|e| {
        error!("Failed to serialize result: {:?}", e);
        e
    })?;

    messaging::publish_message(channel, result_queue, &payload)
        .await
        .map_err(|e| Box::<dyn Error>::from(e))?;
    info!("Sent result for task ID: {}", result.task_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Task, TaskType};

    #[tokio::test]
    async fn test_setup_consumer_failure() {
        let result = setup_consumer("amqp://invalid:5672/%2f", "queue", "tag").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_perform_computation_parameter_pull() {
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: String::new(),
        };
        let result = perform_computation(task.clone()).await.unwrap();
        assert_eq!(result.task_type, TaskType::ParameterPull);
        assert_eq!(result.task_id, task.task_id);
    }

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_send_result_publishes_message() {
        use futures_util::StreamExt;
        use tokio::time::{timeout, Duration};

        const AMQP_ADDR: &str = "amqp://127.0.0.1:5672/%2f";

        let channel = match messaging::establish_connection(AMQP_ADDR).await {
            Ok(ch) => ch,
            Err(_) => return, // RabbitMQ not available
        };

        messaging::declare_queue(&channel, "an_task_queue")
            .await
            .expect("declare result queue");

        let result = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: "ok".into(),
        };
        send_result(result.clone(), &channel)
            .await
            .expect("send result");

        let mut consumer = messaging::consume_messages(&channel, "an_task_queue", "test_cons")
            .await
            .expect("consume result queue");

        let delivery = timeout(Duration::from_secs(5), consumer.next())
            .await
            .expect("timeout")
            .expect("consumer closed")
            .expect("delivery");

        let received: Task = serde_json::from_slice(&delivery.data).unwrap();
        assert_eq!(received.task_id, result.task_id);

        delivery
            .ack(BasicAckOptions::default())
            .await
            .expect("ack result");
    }
}
