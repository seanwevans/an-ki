// ki_node.rs: Manages the Ki node behavior, including fetching inputs, running computations,
// and sending outputs.

use crate::common::{Task, TaskType};
use crate::config::load_settings;
use crate::messaging;
use crate::signals;
use futures_util::stream::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicNackOptions},
    Channel,
};
use std::error::Error;
use std::io::{Error as IoError, ErrorKind};
use tokio::sync::oneshot;
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
        .unwrap_or_else(|_| "5".into())
        .parse()
        .unwrap_or(5);
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
struct KiNodeState {
    model_params: Vec<f32>,
}

impl KiNodeState {
    fn new() -> Self {
        Self::default()
    }

    fn update_model(&mut self, task: &Task) -> Result<(), serde_json::Error> {
        let params: Vec<f32> = serde_json::from_str(&task.data)?;
        self.model_params = params;
        info!("Updated local model parameters");
        Ok(())
    }

    #[cfg(test)]
    fn parameters(&self) -> &[f32] {
        &self.model_params
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
    let model_queue_name = "ki_model_queue";
    let model_consumer_tag = "ki_model_consumer";

    let mut state = KiNodeState::new();

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

        let (_model_channel, mut model_consumer) = messaging::connect_with_retries(
            &amqp_addr,
            model_queue_name,
            model_consumer_tag,
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
                model_delivery = model_consumer.next() => {
                    match model_delivery {
                        Some(Ok(delivery)) => {
                            match serde_json::from_slice::<Task>(&delivery.data) {
                                Ok(task_message) => match state.update_model(&task_message) {
                                    Ok(()) => {
                                        if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                                            error!("Failed to acknowledge model update: {:?}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "Dropping invalid model update without requeue: {:?}",
                                            e
                                        );
                                        if let Err(e) = delivery.nack(nack_options(false)).await {
                                            error!(
                                                "Failed to negatively acknowledge model update: {:?}",
                                                e
                                            );
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!(
                                        "Dropping malformed model update without requeue: {:?}",
                                        e
                                    );
                                    if let Err(e) = delivery.nack(nack_options(false)).await {
                                        error!(
                                            "Failed to negatively acknowledge model update: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("Error in model consumer stream: {:?}", e);
                            break;
                        }
                        None => {
                            error!("Model consumer stream closed");
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn process_and_send_task(task: Task, channel: &Channel) -> Result<(), Box<dyn Error>> {
    let result = perform_computation(task).await?;
    send_result(result, channel).await
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
                let input: Vec<f32> = serde_json::from_str(&task.data)?;
                let grad: Vec<f32> = input.into_iter().map(|v| v * 2.0).collect();
                let data = serde_json::to_string(&grad)?;
                Ok(Task {
                    task_id: task.task_id,
                    task_type: TaskType::GradientUpdate,
                    data,
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
    #[cfg(not(feature = "tch"))]
    use crate::an_node::AnNodeState;
    use crate::common::{Task, TaskType};
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
    fn test_normalized_reconnect_attempts_zero_is_one() {
        std::env::set_var("AMQP_RECONNECT_ATTEMPTS", "0");
        assert_eq!(normalized_reconnect_attempts(), 1);
        std::env::remove_var("AMQP_RECONNECT_ATTEMPTS");
    }

    #[test]
    fn test_normalized_reconnect_attempts_one_is_one() {
        std::env::set_var("AMQP_RECONNECT_ATTEMPTS", "1");
        assert_eq!(normalized_reconnect_attempts(), 1);
        std::env::remove_var("AMQP_RECONNECT_ATTEMPTS");
    }

    #[test]
    fn test_normalized_reconnect_attempts_high_value_is_preserved() {
        std::env::set_var("AMQP_RECONNECT_ATTEMPTS", "100");
        assert_eq!(normalized_reconnect_attempts(), 100);
        std::env::remove_var("AMQP_RECONNECT_ATTEMPTS");
    }

    #[test]
    fn test_update_model_parameters() {
        let mut state = KiNodeState::new();
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: serde_json::to_string(&vec![1.0_f32, 2.5_f32]).unwrap(),
        };

        state.update_model(&task).unwrap();
        assert_eq!(state.parameters(), &[1.0_f32, 2.5_f32]);
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

    #[cfg(not(feature = "tch"))]
    #[tokio::test]
    async fn test_perform_computation_gradient_update_non_tch() {
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![1.0_f32, -0.5_f32]).unwrap(),
        };
        let result = perform_computation(task.clone()).await.unwrap();
        assert_eq!(result.task_id, task.task_id);
        assert_eq!(result.task_type, TaskType::GradientUpdate);
        let gradient: Vec<f32> = serde_json::from_str(&result.data).unwrap();
        assert_eq!(gradient, vec![2.0_f32, -1.0_f32]);
    }

    #[cfg(not(feature = "tch"))]
    #[tokio::test]
    async fn test_an_node_processes_ki_payload() {
        let task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&vec![0.25_f32, 0.5_f32]).unwrap(),
        };
        let result = perform_computation(task).await.unwrap();
        let mut state = AnNodeState::new();
        assert!(state.process_task(result, None, 1).await.is_ok());
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

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_model_update_consumption() {
        use futures_util::StreamExt;
        use tokio::time::{timeout, Duration};

        const AMQP_ADDR: &str = "amqp://127.0.0.1:5672/%2f";

        let channel = match messaging::establish_connection(AMQP_ADDR).await {
            Ok(ch) => ch,
            Err(_) => return, // RabbitMQ not available
        };

        messaging::declare_queue(&channel, "ki_model_queue")
            .await
            .expect("declare model queue");

        let payload_task = Task {
            task_id: uuid::Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: serde_json::to_string(&vec![0.5_f32, -1.0_f32]).unwrap(),
        };

        let payload = serde_json::to_vec(&payload_task).expect("serialize task");
        messaging::publish_message(&channel, "ki_model_queue", &payload)
            .await
            .expect("publish model update");

        let mut consumer =
            messaging::consume_messages(&channel, "ki_model_queue", "test_model_consumer")
                .await
                .expect("consume model queue");

        if let Ok(Some(Ok(delivery))) = timeout(Duration::from_secs(5), consumer.next()).await {
            let task: Task =
                serde_json::from_slice(&delivery.data).expect("deserialize model task");
            let mut state = KiNodeState::new();
            state.update_model(&task).expect("update model");
            assert_eq!(state.parameters(), &[0.5_f32, -1.0_f32]);

            delivery
                .ack(BasicAckOptions::default())
                .await
                .expect("ack model update");
        }
    }
}
