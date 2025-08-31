// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;

use crate::{common::{Task, TaskType}, config::load_settings, messaging, signals};
use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicNackOptions},
    Channel, Consumer,
};
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tracing::{error, info};
use lazy_static::lazy_static;
use std::sync::Mutex;
use uuid::Uuid;

lazy_static! {
    static ref MODEL_PARAMS: Mutex<Vec<f32>> = Mutex::new(Vec::new());
    static ref GRAD_ACCUM: Mutex<(Vec<f32>, usize)> = Mutex::new((Vec::new(), 0));
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

    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;
    let amqp_addr = settings.amqp_addr.clone();
    let shard_count = settings.model_shards;

    let max_retries: u32 = std::env::var("AMQP_RECONNECT_ATTEMPTS")
        .unwrap_or_else(|_| "5".into())
        .parse()
        .unwrap_or(5);
    let backoff_ms: u64 = std::env::var("AMQP_RECONNECT_BACKOFF_MS")
        .unwrap_or_else(|_| "500".into())
        .parse()
        .unwrap_or(500);

    let queue_name = "an_task_queue";
    let consumer_tag = "an_consumer";
    let mut attempts = 0u32;

    loop {
        let (channel, mut consumer) =
            match setup_consumer(&amqp_addr, queue_name, consumer_tag).await {
                Ok(c) => {
                    attempts = 0;
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

        info!("An node is running and waiting for tasks...");

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("Shutdown signal received, stopping An node...");
                    return Ok(());
                }
                delivery = consumer.next() => {
                    match delivery {
                        Some(Ok(delivery)) => {
                            match serde_json::from_slice::<Task>(&delivery.data) {
                                Ok(task_message) => {
                                    info!("Received task: {:?}", task_message);
                                    if let Err(e) = process_task(task_message, Some(&channel), shard_count).await {
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
                            error!("Error in consumer stream: {:?}", e);
                            break;
                        }
                        None => {
                            error!("Consumer stream closed");
                            break;
                        }
                    }
                }
            }
        }

        attempts += 1;
        if attempts >= max_retries {
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
    let channel = messaging::establish_connection(amqp_addr).await?;
    messaging::declare_queue(&channel, queue_name).await?;
    let consumer = messaging::consume_messages(&channel, queue_name, consumer_tag).await?;
    Ok((channel, consumer))
}

async fn process_task(task: Task, channel: Option<&Channel>, shard_count: usize) -> Result<(), Box<dyn Error>> {
    match task.task_type {
        TaskType::GradientUpdate => {
            let gradient: Vec<f32> = serde_json::from_str(&task.data)?;
            let mut acc = GRAD_ACCUM.lock().unwrap();
            if acc.0.is_empty() {
                acc.0 = vec![0.0; gradient.len()];
            }
            for (a, g) in acc.0.iter_mut().zip(&gradient) {
                *a += g;
            }
            acc.1 += 1;
            if acc.1 >= shard_count {
                let mut model = MODEL_PARAMS.lock().unwrap();
                if model.is_empty() {
                    model.resize(acc.0.len(), 0.0);
                }
                for (m, a) in model.iter_mut().zip(acc.0.iter()) {
                    *m -= *a / shard_count as f32;
                }
                acc.0.iter_mut().for_each(|v| *v = 0.0);
                acc.1 = 0;
                if let Some(ch) = channel {
                    broadcast_model(&model, ch).await?;
                }
            }
        }
        TaskType::ParameterPull => {
            if let Some(ch) = channel {
                let model = MODEL_PARAMS.lock().unwrap().clone();
                broadcast_model(&model, ch).await?;
            }
        }
    }
    Ok(())
}

async fn broadcast_model(model: &[f32], channel: &Channel) -> Result<(), Box<dyn Error>> {
    let task = Task {
        task_id: Uuid::new_v4(),
        task_type: TaskType::ParameterPull,
        data: serde_json::to_string(model)?,
    };
    messaging::declare_queue(channel, "ki_model_queue").await?;
    let payload = serde_json::to_vec(&task)?;
    messaging::publish_message(channel, "ki_model_queue", &payload).await?;
    info!("Broadcasted model update");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};
    use crate::common::{Task, TaskType};

    #[tokio::test]
    async fn test_process_task_ok() {
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.1_f32, 0.2_f32]).unwrap(),
        };
        assert!(process_task(task, None, 1).await.is_ok());
    }

    #[tokio::test]
    async fn test_setup_consumer_failure() {
        let result = setup_consumer("amqp://invalid:5672/%2f", "test_queue", "test_tag").await;
        assert!(result.is_err());
    }

    #[cfg(feature = "integration-tests")]
    use crate::messaging;
    #[cfg(feature = "integration-tests")]
    use futures_util::StreamExt;
    #[cfg(feature = "integration-tests")]
    use lapin::options::BasicAckOptions;
    #[cfg(feature = "integration-tests")]
    const AMQP_ADDR: &str = "amqp://127.0.0.1:5672/%2f";

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_setup_consumer_workflow() {
        let (channel, mut consumer) = setup_consumer(AMQP_ADDR, "test_queue", "test_consumer")
            .await
            .expect("setup");

        messaging::publish_message(&channel, "test_queue", b"hello")
            .await
            .expect("publish");

        let delivery = timeout(Duration::from_secs(5), consumer.next())
            .await
            .expect("timeout")
            .expect("consumer closed")
            .expect("delivery error");

        assert_eq!(delivery.data, b"hello");
        delivery.ack(BasicAckOptions::default()).await.expect("ack");
    }
}
