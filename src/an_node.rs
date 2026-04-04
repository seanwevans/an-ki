// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;
use std::io::{Error as IoError, ErrorKind};

use crate::{
    common::{Task, TaskType},
    config::load_settings,
    messaging, signals,
};
use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicNackOptions},
    Channel,
};
use tokio::sync::oneshot;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Default)]
pub struct AnNodeState {
    model_params: Vec<f32>,
    grad_accum: (Vec<f32>, usize),
}

impl AnNodeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn process_task(
        &mut self,
        task: Task,
        channel: Option<&Channel>,
        shard_count: usize,
    ) -> Result<(), Box<dyn Error>> {
        match task.task_type {
            TaskType::GradientUpdate => {
                let gradient: Vec<f32> = serde_json::from_str(&task.data)?;
                if !self.grad_accum.0.is_empty() && self.grad_accum.0.len() != gradient.len() {
                    return Err(IoError::new(
                        ErrorKind::InvalidData,
                        format!(
                            "Gradient length mismatch for accumulator: expected {}, received {}",
                            self.grad_accum.0.len(),
                            gradient.len()
                        ),
                    )
                    .into());
                }
                if !self.model_params.is_empty() && self.model_params.len() != gradient.len() {
                    return Err(IoError::new(
                        ErrorKind::InvalidData,
                        format!(
                            "Gradient length mismatch for model parameters: expected {}, received {}",
                            self.model_params.len(),
                            gradient.len()
                        ),
                    )
                    .into());
                }
                let broadcast_data = {
                    if self.grad_accum.0.is_empty() {
                        self.grad_accum.0 = vec![0.0; gradient.len()];
                    }
                    for (a, g) in self.grad_accum.0.iter_mut().zip(&gradient) {
                        *a += g;
                    }
                    self.grad_accum.1 += 1;
                    if self.grad_accum.1 >= shard_count {
                        if self.model_params.is_empty() {
                            self.model_params.resize(self.grad_accum.0.len(), 0.0);
                        }
                        for (m, a) in self.model_params.iter_mut().zip(self.grad_accum.0.iter()) {
                            *m -= *a / shard_count as f32;
                        }
                        let model_clone = self.model_params.clone();
                        self.grad_accum.0.iter_mut().for_each(|v| *v = 0.0);
                        self.grad_accum.1 = 0;
                        Some(model_clone)
                    } else {
                        None
                    }
                };
                if let (Some(ch), Some(model)) = (channel, broadcast_data) {
                    self.broadcast_model(&model, ch).await?;
                }
            }
            TaskType::ParameterPull => {
                if let Some(ch) = channel {
                    let model = self.model_params.clone();
                    self.broadcast_model(&model, ch).await?;
                }
            }
        }
        Ok(())
    }

    async fn broadcast_model(
        &self,
        model: &[f32],
        channel: &Channel,
    ) -> Result<(), Box<dyn Error>> {
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
    let mut state = AnNodeState::new();

    let max_retries: u32 = std::env::var("AMQP_RECONNECT_ATTEMPTS")
        .unwrap_or_else(|_| "5".into())
        .parse()
        .unwrap_or(5);
    let backoff_ms: u64 = std::env::var("AMQP_RECONNECT_BACKOFF_MS")
        .unwrap_or_else(|_| "500".into())
        .parse()
        .unwrap_or(500);
    let max_backoff_ms: u64 = std::env::var("AMQP_RECONNECT_MAX_BACKOFF_MS")
        .unwrap_or_else(|_| "5000".into())
        .parse()
        .unwrap_or(5_000);

    let queue_name = "an_task_queue";
    let consumer_tag = "an_consumer";

    loop {
        let (channel, mut consumer) = messaging::connect_with_retries(
            &amqp_addr,
            queue_name,
            consumer_tag,
            max_retries,
            backoff_ms,
            max_backoff_ms,
        )
        .await?;

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
                                    if let Err(e) = state
                                        .process_task(task_message, Some(&channel), shard_count)
                                        .await
                                    {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Task, TaskType};
    #[cfg(feature = "integration-tests")]
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_first_gradient_initializes_state() {
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.1_f32, 0.2_f32]).unwrap(),
        };
        let mut state = AnNodeState::new();
        assert!(state.process_task(task, None, 1).await.is_ok());
        assert_eq!(state.model_params, vec![-0.1_f32, -0.2_f32]);
        assert_eq!(state.grad_accum.0, vec![0.0_f32, 0.0_f32]);
        assert_eq!(state.grad_accum.1, 0);
    }

    #[tokio::test]
    async fn test_matching_lengths_succeed() {
        let mut state = AnNodeState {
            model_params: vec![1.0_f32, 2.0_f32],
            grad_accum: (vec![0.0_f32, 0.0_f32], 0),
        };
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.5_f32, 1.5_f32]).unwrap(),
        };

        assert!(state.process_task(task, None, 1).await.is_ok());
        assert_eq!(state.model_params, vec![0.5_f32, 0.5_f32]);
        assert_eq!(state.grad_accum.0, vec![0.0_f32, 0.0_f32]);
        assert_eq!(state.grad_accum.1, 0);
    }

    #[tokio::test]
    async fn test_mismatched_lengths_fail_without_mutation() {
        let mut state = AnNodeState {
            model_params: vec![1.0_f32, 2.0_f32],
            grad_accum: (vec![0.1_f32, 0.2_f32], 1),
        };
        let original_model = state.model_params.clone();
        let original_accum = state.grad_accum.clone();
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.3_f32, 0.4_f32, 0.5_f32]).unwrap(),
        };

        let err = state.process_task(task, None, 1).await.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("expected 2, received 3"));
        assert_eq!(state.model_params, original_model);
        assert_eq!(state.grad_accum, original_accum);
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
        let (channel, mut consumer) = messaging::connect_with_retries(
            AMQP_ADDR,
            "test_queue",
            "test_consumer",
            1,
            10,
            10_000,
        )
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
