// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;
use std::io::{Error as IoError, ErrorKind};

use crate::{
    api,
    common::{self, Task, TaskType},
    config::load_settings,
    health, messaging, security, signals,
};
use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicNackOptions},
    Channel,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryDisposition {
    Ack,
    Nack { requeue: bool },
}

fn nack_options(requeue: bool) -> BasicNackOptions {
    BasicNackOptions {
        multiple: false,
        requeue,
    }
}

fn is_invalid_payload_error(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(err) = current {
        if err.is::<serde_json::Error>() {
            return true;
        }
        if err
            .downcast_ref::<IoError>()
            .is_some_and(|io_error| io_error.kind() == ErrorKind::InvalidData)
        {
            return true;
        }
        current = err.source();
    }
    false
}

enum ProcessingOutcome<'a> {
    Succeeded,
    Failed(&'a (dyn Error + 'static)),
}

fn processing_disposition(outcome: ProcessingOutcome<'_>) -> DeliveryDisposition {
    match outcome {
        ProcessingOutcome::Succeeded => DeliveryDisposition::Ack,
        ProcessingOutcome::Failed(error) if is_invalid_payload_error(error) => {
            DeliveryDisposition::Nack { requeue: false }
        }
        ProcessingOutcome::Failed(_) => DeliveryDisposition::Nack { requeue: true },
    }
}

fn normalized_reconnect_attempts() -> u32 {
    let raw = std::env::var("AMQP_RECONNECT_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    normalize_reconnect_attempts(raw)
}

/// Ensures the configured retry count yields at least one attempt. A value of
/// zero would otherwise mean "never try", which is never the intent.
fn normalize_reconnect_attempts(raw: u32) -> u32 {
    if raw == 0 {
        error!(
            "AMQP_RECONNECT_ATTEMPTS=0 is invalid for retry semantics; normalizing to 1 total attempt."
        );
        1
    } else {
        raw
    }
}

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
        if shard_count == 0 {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "shard_count must be greater than 0",
            )
            .into());
        }

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
        // Encrypt the model parameters in transit with the shared node secret.
        let key = security::message_key()?;
        messaging::publish_encrypted(channel, "ki_model_queue", &task, &key).await?;
        info!("Broadcasted model update");
        Ok(())
    }
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if let Err(e) = signals::setup_unix_signal_handlers().await {
        error!("Failed to set up Unix signal handlers: {:?}", e);
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        if let Err(e) = signals::setup_signal_handler().await {
            error!("Signal handler error: {:?}", e);
        }
        let _ = shutdown_tx.send(true);
    });

    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;
    let amqp_addr = settings.amqp_addr.clone();
    let shard_count = settings.model_shards;
    let mut state = AnNodeState::new();

    // Serve the task REST API alongside the consumer loop. The server creates
    // its own database-backed task manager and shuts down on the same signal.
    let api_addr = settings.api_bind_addr().map_err(|e| {
        error!("Invalid api_addr configuration: {:?}", e);
        e
    })?;
    let mut api_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let shutdown = async move {
            let _ = api_shutdown.changed().await;
        };
        if let Err(e) = api::serve(api_addr, shutdown).await {
            error!("Task API server stopped: {:?}", e);
        }
    });

    // Emit heartbeats so the principal can monitor this node's health.
    let heartbeat_cancel = CancellationToken::new();
    {
        let trigger = heartbeat_cancel.clone();
        let mut hb_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let _ = hb_shutdown.changed().await;
            trigger.cancel();
        });
        tokio::spawn(health::publish_heartbeats(
            amqp_addr.clone(),
            common::node_id(),
            health::heartbeat_interval(),
            heartbeat_cancel,
        ));
    }

    let max_retries: u32 = normalized_reconnect_attempts();
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
                _ = shutdown_rx.changed() => {
                    info!("Shutdown signal received, stopping An node...");
                    return Ok(());
                }
                delivery = consumer.next() => {
                    match delivery {
                        Some(Ok(delivery)) => {
                            match serde_json::from_slice::<Task>(&delivery.data) {
                                Ok(task_message) => {
                                    info!("Received task: {:?}", task_message);
                                    match state
                                        .process_task(task_message, Some(&channel), shard_count)
                                        .await
                                    {
                                        Ok(()) => {
                                            match processing_disposition(ProcessingOutcome::Succeeded) {
                                                DeliveryDisposition::Ack => {
                                                    if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                                                        error!("Failed to acknowledge message: {:?}", e);
                                                    }
                                                }
                                                DeliveryDisposition::Nack { .. } => unreachable!(
                                                    "successful processing must acknowledge messages"
                                                ),
                                            }
                                        }
                                        Err(e) => {
                                            let disposition = processing_disposition(ProcessingOutcome::Failed(e.as_ref()));
                                            error!(
                                                "Failed to process task; applying {:?}: {:?}",
                                                disposition, e
                                            );
                                            match disposition {
                                                DeliveryDisposition::Ack => unreachable!(
                                                    "processing failures must not acknowledge messages"
                                                ),
                                                DeliveryDisposition::Nack { requeue } => {
                                                    if let Err(nack_error) = delivery
                                                        .nack(nack_options(requeue))
                                                        .await
                                                    {
                                                        error!(
                                                            "Failed to negatively acknowledge message: {:?}",
                                                            nack_error
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "Dropping malformed task message without requeue: {:?}",
                                        e
                                    );
                                    if let Err(e) = delivery.nack(nack_options(false)).await {
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
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn test_successful_processing_disposition_acknowledges() {
        assert_eq!(
            processing_disposition(ProcessingOutcome::Succeeded),
            DeliveryDisposition::Ack
        );
    }

    #[test]
    fn test_invalid_payload_disposition_drops_without_requeue() {
        let error = serde_json::from_str::<Vec<f32>>("not-json").unwrap_err();
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(&error)),
            DeliveryDisposition::Nack { requeue: false }
        );
    }

    #[test]
    fn test_invalid_data_disposition_drops_without_requeue() {
        let error = IoError::new(ErrorKind::InvalidData, "mismatched gradient length");
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(&error)),
            DeliveryDisposition::Nack { requeue: false }
        );
    }

    #[test]
    fn test_transient_processing_disposition_requeues() {
        let error = IoError::new(ErrorKind::TimedOut, "temporary broker timeout");
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(&error)),
            DeliveryDisposition::Nack { requeue: true }
        );
    }
    #[cfg(feature = "integration-tests")]
    use tokio::time::{timeout, Duration};

    #[test]
    fn zero_reconnect_attempts_normalizes_to_one() {
        assert_eq!(normalize_reconnect_attempts(0), 1);
    }

    #[test]
    fn one_reconnect_attempt_is_preserved() {
        assert_eq!(normalize_reconnect_attempts(1), 1);
    }

    #[test]
    fn high_reconnect_attempt_count_is_preserved() {
        assert_eq!(normalize_reconnect_attempts(100), 100);
    }

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

    #[tokio::test]
    async fn test_zero_shard_count_fails_without_mutation() {
        let mut state = AnNodeState {
            model_params: vec![1.0_f32, 2.0_f32],
            grad_accum: (vec![0.1_f32, 0.2_f32], 1),
        };
        let original_model = state.model_params.clone();
        let original_accum = state.grad_accum.clone();
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.3_f32, 0.4_f32]).unwrap(),
        };

        let err = state.process_task(task, None, 0).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("shard_count must be greater than 0"));
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
