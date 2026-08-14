// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations,
// and sending outputs.

use crate::common::{self, GradientReply, GradientRequest, Task, TaskType};
use crate::config::load_settings;
use crate::dataset;
use crate::health;
use crate::logging_metrics;
use crate::messaging;
use crate::model;
use crate::signals;
use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicNackOptions},
    Channel,
};
use std::error::Error;
use std::io::{Error as IoError, ErrorKind};
use std::time::Instant;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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

fn deserialize_task_message(queue_name: &str, payload: &[u8]) -> Result<Task, serde_json::Error> {
    serde_json::from_slice::<Task>(payload).map_err(|e| {
        error!(
            "Failed to deserialize task message from queue '{}' (payload_len={}): {:?}",
            queue_name,
            payload.len(),
            e
        );
        e
    })
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if let Err(e) = signals::setup_unix_signal_handlers().await {
        error!("Failed to set up Unix signal handlers: {:?}", e);
    }

    let heartbeat_cancel = CancellationToken::new();
    let heartbeat_trigger = heartbeat_cancel.clone();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(e) = signals::setup_signal_handler().await {
            error!("Signal handler error: {:?}", e);
        }
        let _ = shutdown_tx.send(());
        heartbeat_trigger.cancel();
    });

    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;

    let amqp_addr = settings.amqp_addr;

    // Emit heartbeats so the principal can monitor this node's health.
    tokio::spawn(health::publish_heartbeats(
        amqp_addr.clone(),
        common::node_id(),
        common::NodeRole::Ki,
        health::heartbeat_interval(),
        heartbeat_cancel,
    ));
    let max_retries: u32 = normalized_reconnect_attempts();
    let backoff_ms: u64 = std::env::var("AMQP_RECONNECT_BACKOFF_MS")
        .unwrap_or_else(|_| "500".into())
        .parse()
        .unwrap_or(500);
    let max_backoff_ms: u64 = std::env::var("AMQP_RECONNECT_MAX_BACKOFF_MS")
        .unwrap_or_else(|_| "5000".into())
        .parse()
        .unwrap_or(5_000);

    let queue_name = "ki_task_queue";
    let consumer_tag = "ki_consumer";

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
                            match deserialize_task_message(queue_name, &delivery.data) {
                                Ok(task_message) => {
                                    info!("Received task: {:?}", task_message);

                                    match process_and_send_task(task_message, &channel).await {
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

async fn process_and_send_task(task: Task, channel: &Channel) -> Result<(), Box<dyn Error>> {
    let started_at = Instant::now();
    let result = perform_computation(task).await?;
    send_result(result, channel).await?;
    // Only successful tasks are counted: a task that failed was not processed,
    // and counting it would inflate the throughput panel during an outage.
    logging_metrics::record_task_processed(started_at);
    Ok(())
}

/// Computes the gradient this node is responsible for.
///
/// For a [`TaskType::GradientUpdate`] the payload is a [`GradientRequest`]: the
/// node rebuilds the shared dataset from its spec, takes only its own shard, and
/// returns the mean gradient and loss over that shard. The dataset never crosses
/// the wire — only the seed does.
pub async fn perform_computation(task: Task) -> Result<Task, Box<dyn Error>> {
    match task.task_type {
        TaskType::GradientUpdate => {
            let request: GradientRequest = serde_json::from_str(&task.data)?;

            let samples = dataset::generate(request.dataset);
            // Shard the training portion only. A validation sample that reached
            // a gradient would make the evaluation meaningless.
            let training = dataset::training(&request.dataset, &samples);
            let shard = dataset::shard(training, request.shard, request.shards);
            if shard.is_empty() {
                // A shard with no samples has no gradient to report. Treat it as
                // bad input rather than replying with zeros, which the An node
                // would fold into the average as though it were real evidence.
                return Err(IoError::new(
                    ErrorKind::InvalidData,
                    format!(
                        "shard {} of {} is empty for a {}-sample training set",
                        request.shard,
                        request.shards,
                        training.len()
                    ),
                )
                .into());
            }

            let (loss, gradient) =
                model::loss_and_gradient(&request.spec, &request.parameters, shard)
                    .map_err(|e| IoError::new(ErrorKind::InvalidData, e.to_string()))?;

            info!(
                "Computed gradient for shard {}/{} over {} samples (loss {:.4})",
                request.shard,
                request.shards,
                shard.len(),
                loss
            );

            let reply = GradientReply {
                gradient,
                loss,
                samples: shard.len(),
            };
            Ok(Task {
                task_id: task.task_id,
                task_type: TaskType::GradientUpdate,
                data: serde_json::to_string(&reply)?,
            })
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
        .map_err(Box::<dyn Error>::from)?;
    info!("Sent result for task ID: {}", result.task_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Task, TaskType};
    use crate::model::MlpSpec;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn test_successful_task_disposition_acknowledges() {
        assert_eq!(
            processing_disposition(ProcessingOutcome::Succeeded),
            DeliveryDisposition::Ack
        );
    }

    #[test]
    fn test_invalid_task_payload_disposition_drops_without_requeue() {
        let error = serde_json::from_str::<Vec<f32>>("not-json").unwrap_err();
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(&error)),
            DeliveryDisposition::Nack { requeue: false }
        );
    }

    #[test]
    fn test_send_result_failure_disposition_requeues_without_ack() {
        let error = IoError::new(ErrorKind::ConnectionReset, "result publish failed");
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(&error)),
            DeliveryDisposition::Nack { requeue: true }
        );
    }

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

    #[tokio::test]
    async fn a_gradient_request_returns_a_gradient_over_its_own_shard() {
        let spec = MlpSpec::new(crate::dataset::INPUTS, 4, crate::dataset::OUTPUTS);
        let data = crate::dataset::DatasetSpec::new(40, 5);
        let request = GradientRequest {
            spec,
            dataset: data,
            shard: 1,
            shards: 4,
            parameters: spec.initialize(2),
        };
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&request).unwrap(),
        };

        let result = perform_computation(task.clone()).await.expect("gradient");

        assert_eq!(result.task_id, task.task_id);
        let reply: GradientReply = serde_json::from_str(&result.data).unwrap();
        assert_eq!(reply.gradient.len(), spec.parameter_count());
        assert_eq!(reply.samples, 10, "shard 1 of 4 over 40 samples");
        assert!(reply.loss.is_finite() && reply.loss > 0.0);
        assert!(reply.gradient.iter().all(|g| g.is_finite()));
    }

    #[tokio::test]
    async fn different_shards_produce_different_gradients() {
        // If they matched, the workers would be duplicating each other's work
        // and the cluster would learn no faster than one node.
        let spec = MlpSpec::new(crate::dataset::INPUTS, 4, crate::dataset::OUTPUTS);
        let data = crate::dataset::DatasetSpec::new(64, 11);
        let parameters = spec.initialize(2);

        let gradient_for = |shard: usize| {
            let request = GradientRequest {
                spec,
                dataset: data,
                shard,
                shards: 4,
                parameters: parameters.clone(),
            };
            Task {
                task_id: uuid::Uuid::new_v4(),
                task_type: TaskType::GradientUpdate,
                data: serde_json::to_string(&request).unwrap(),
            }
        };

        let first: GradientReply =
            serde_json::from_str(&perform_computation(gradient_for(0)).await.unwrap().data)
                .unwrap();
        let second: GradientReply =
            serde_json::from_str(&perform_computation(gradient_for(1)).await.unwrap().data)
                .unwrap();

        assert_ne!(first.gradient, second.gradient);
    }

    #[tokio::test]
    async fn an_empty_shard_is_reported_rather_than_answered_with_zeros() {
        // Replying with zeros would fold into the An node's average as though
        // it were real evidence, quietly damping the update.
        let spec = MlpSpec::new(crate::dataset::INPUTS, 4, crate::dataset::OUTPUTS);
        let request = GradientRequest {
            spec,
            dataset: crate::dataset::DatasetSpec::new(2, 1),
            shard: 5,
            shards: 8,
            parameters: spec.initialize(1),
        };
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&request).unwrap(),
        };

        let err = perform_computation(task).await.expect_err("must fail");
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

    #[tokio::test]
    async fn a_malformed_gradient_request_is_dropped_without_requeue() {
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: "not-a-request".to_string(),
        };

        let err = perform_computation(task).await.expect_err("must fail");
        assert_eq!(
            processing_disposition(ProcessingOutcome::Failed(err.as_ref())),
            DeliveryDisposition::Nack { requeue: false }
        );
    }

    #[test]
    fn test_deserialize_task_message_ok() {
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![1.0_f32]).unwrap(),
        };
        let payload = serde_json::to_vec(&task).expect("serialize task");
        let parsed = deserialize_task_message("ki_task_queue", &payload).expect("deserialize task");
        assert_eq!(parsed.task_id, task.task_id);
        assert_eq!(parsed.task_type, task.task_type);
    }

    #[test]
    fn test_deserialize_task_message_malformed_payload() {
        let err = deserialize_task_message("ki_task_queue", b"{not-json")
            .expect_err("malformed payload should fail to deserialize");
        assert!(err.is_syntax());
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
