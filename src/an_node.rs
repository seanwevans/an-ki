// an_node.rs: Contains the logic for An nodes, including task distribution to Ki nodes and local database handling.

use std::error::Error;
use std::io::{Error as IoError, ErrorKind};

use crate::{
    api, checkpoint,
    common::{self, GradientReply, Task, TaskType},
    config::load_settings,
    database,
    dataset::{self, DatasetSpec},
    health, logging_metrics, messaging,
    model::{self, MlpSpec, Sample},
    scheduler, signals,
};
use futures_util::stream::StreamExt;
use lapin::options::{BasicAckOptions, BasicNackOptions};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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

/// The An node's view of training: the model, and the gradients accumulated
/// toward the current epoch.
pub struct AnNodeState {
    spec: MlpSpec,
    dataset: DatasetSpec,
    parameters: Vec<f32>,
    learning_rate: f32,
    shards: usize,
    /// Sum of each shard's mean gradient weighted by its sample count.
    accumulator: Vec<f32>,
    accumulated_samples: usize,
    replies: usize,
    /// Sum of each shard's mean loss weighted by its sample count.
    loss_accumulator: f64,
    last_epoch_loss: Option<f32>,
    /// Accuracy on the held-out set after the last completed epoch.
    last_validation_accuracy: Option<f32>,
    /// The held-out samples, generated once rather than per epoch.
    validation: Vec<Sample>,
    epochs_completed: u64,
}

impl AnNodeState {
    /// Builds the initial state, drawing starting parameters from `init_seed`.
    ///
    /// The parameters must not start at zero: identical hidden units receive
    /// identical gradients forever, so the network would never use more than one
    /// of them. See [`MlpSpec::initialize`].
    pub fn new(
        spec: MlpSpec,
        dataset: DatasetSpec,
        shards: usize,
        learning_rate: f32,
        init_seed: u64,
    ) -> Self {
        let parameters = spec.initialize(init_seed);
        let all_samples = dataset::generate(dataset);
        let validation = dataset::validation(&dataset, &all_samples).to_vec();
        Self {
            accumulator: vec![0.0; parameters.len()],
            last_validation_accuracy: None,
            validation,
            spec,
            dataset,
            parameters,
            learning_rate,
            shards,
            accumulated_samples: 0,
            replies: 0,
            loss_accumulator: 0.0,
            last_epoch_loss: None,
            epochs_completed: 0,
        }
    }

    /// Resumes from a previously saved checkpoint, replacing the freshly
    /// initialized parameters and continuing the epoch count.
    ///
    /// # Errors
    /// Returns an error if the checkpoint's parameter vector does not match the
    /// configured network shape, which would otherwise corrupt every subsequent
    /// gradient application.
    pub fn resume_from(
        &mut self,
        parameters: Vec<f32>,
        epochs_completed: u64,
    ) -> Result<(), Box<dyn Error>> {
        if parameters.len() != self.parameters.len() {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                format!(
                    "checkpoint has {} parameters, model expects {}",
                    parameters.len(),
                    self.parameters.len()
                ),
            )
            .into());
        }
        self.parameters = parameters;
        self.epochs_completed = epochs_completed;
        Ok(())
    }

    /// The current model parameters.
    pub fn parameters(&self) -> &[f32] {
        &self.parameters
    }

    pub fn spec(&self) -> MlpSpec {
        self.spec
    }

    pub fn dataset(&self) -> DatasetSpec {
        self.dataset
    }

    pub fn shards(&self) -> usize {
        self.shards
    }

    /// Gradients received toward the current epoch.
    pub fn pending_gradients(&self) -> usize {
        self.replies
    }

    /// Mean training loss of the last completed epoch, once one has completed.
    pub fn last_epoch_loss(&self) -> Option<f32> {
        self.last_epoch_loss
    }

    /// Accuracy on the held-out set after the last completed epoch.
    ///
    /// `None` when no epoch has completed, or when the dataset holds nothing
    /// back — in which case there is no honest number to report.
    pub fn last_validation_accuracy(&self) -> Option<f32> {
        self.last_validation_accuracy
    }

    /// The held-out samples this node evaluates against.
    pub fn validation_set(&self) -> &[Sample] {
        &self.validation
    }

    pub fn epochs_completed(&self) -> u64 {
        self.epochs_completed
    }

    /// Folds one worker's reply into the current epoch, applying the update once
    /// every shard has reported.
    pub async fn process_task(&mut self, task: Task) -> Result<(), Box<dyn Error>> {
        match task.task_type {
            TaskType::GradientUpdate => {
                let reply: GradientReply = serde_json::from_str(&task.data)?;
                self.accumulate(reply)?;

                if self.replies >= self.shards {
                    self.apply_epoch();
                }
            }
            TaskType::ParameterPull => {
                // Workers receive parameters in their gradient request, so there
                // is nothing to push. The variant remains a valid task type for
                // the REST task API, which is unrelated to training.
                warn!(
                    "Ignoring ParameterPull task {}: parameters travel with each \
                     gradient request",
                    task.task_id
                );
            }
        }
        Ok(())
    }

    /// Adds one shard's contribution to the epoch.
    ///
    /// Contributions are weighted by sample count rather than averaged evenly
    /// across shards. Both give the same answer for equal shards, but weighting
    /// stays correct when the dataset does not divide evenly, where an even
    /// average would quietly give samples in smaller shards more influence.
    fn accumulate(&mut self, reply: GradientReply) -> Result<(), Box<dyn Error>> {
        if reply.gradient.len() != self.parameters.len() {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                format!(
                    "Gradient length mismatch: expected {}, received {}",
                    self.parameters.len(),
                    reply.gradient.len()
                ),
            )
            .into());
        }
        if reply.samples == 0 {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "gradient reported over zero samples carries no information",
            )
            .into());
        }
        if !reply.loss.is_finite() || reply.gradient.iter().any(|g| !g.is_finite()) {
            // A non-finite gradient poisons every parameter it touches and can
            // never be recovered from, so reject it rather than folding it in.
            return Err(
                IoError::new(ErrorKind::InvalidData, "gradient or loss was not finite").into(),
            );
        }

        let weight = reply.samples as f32;
        for (slot, g) in self.accumulator.iter_mut().zip(&reply.gradient) {
            *slot += g * weight;
        }
        self.loss_accumulator += reply.loss as f64 * reply.samples as f64;
        self.accumulated_samples += reply.samples;
        self.replies += 1;
        Ok(())
    }

    /// Applies the accumulated gradient and resets for the next epoch.
    fn apply_epoch(&mut self) {
        let total = self.accumulated_samples as f32;
        for (parameter, accumulated) in self.parameters.iter_mut().zip(&self.accumulator) {
            *parameter -= self.learning_rate * (accumulated / total);
        }

        self.last_epoch_loss =
            Some((self.loss_accumulator / self.accumulated_samples as f64) as f32);
        self.epochs_completed += 1;

        // Evaluate on data no worker trained on. Accuracy over the training set
        // would measure how well the model memorized it, not whether it
        // generalized.
        self.last_validation_accuracy = if self.validation.is_empty() {
            None
        } else {
            model::accuracy(&self.spec, &self.parameters, &self.validation).ok()
        };

        info!(
            "Epoch {} over {} samples from {} shard(s): training loss {:.5}, validation accuracy {}",
            self.epochs_completed,
            self.accumulated_samples,
            self.replies,
            self.last_epoch_loss.unwrap_or(f32::NAN),
            self.last_validation_accuracy
                .map(|a| format!("{a:.4}"))
                .unwrap_or_else(|| "n/a".to_string())
        );

        self.accumulator.iter_mut().for_each(|value| *value = 0.0);
        self.accumulated_samples = 0;
        self.loss_accumulator = 0.0;
        self.replies = 0;
    }
}

/// Saves a checkpoint when one is due, at most once per completed epoch.
///
/// Failures are logged and swallowed: losing a checkpoint costs progress, but
/// aborting the training loop over it costs more.
async fn maybe_checkpoint(
    store: Option<&checkpoint::CheckpointStore>,
    run_id: &str,
    state: &Arc<Mutex<AnNodeState>>,
    interval: u64,
    last_saved: &mut u64,
) {
    let Some(store) = store else {
        return;
    };

    let (epoch, parameters, loss) = {
        let state = state.lock().await;
        (
            state.epochs_completed(),
            state.parameters().to_vec(),
            state.last_epoch_loss(),
        )
    };

    if epoch == *last_saved || !checkpoint::is_checkpoint_due(epoch, interval) {
        return;
    }

    match store.save(run_id, epoch, &parameters, loss).await {
        Ok(_) => *last_saved = epoch,
        // Do not advance `last_saved` on failure, so the next completed epoch
        // retries rather than waiting a full interval.
        Err(e) => error!("Failed to save checkpoint at epoch {}: {:?}", epoch, e),
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
    // The scheduler dispatches the current parameters each epoch, so the model
    // needs a shape before the first round rather than being inferred from the
    // first gradient to arrive.
    let model_spec = settings.model_spec();
    let dataset_spec = settings.dataset_spec();
    let run_id = checkpoint::model_id(&model_spec, &dataset_spec);
    let mut state_value = AnNodeState::new(
        model_spec,
        dataset_spec,
        shard_count,
        settings.learning_rate,
        settings.init_seed,
    );

    // Restore the last checkpoint so a restart resumes training rather than
    // discarding it. Checkpointing is best-effort: a database that is
    // unavailable must not stop the node from training, only from remembering.
    let checkpoints = match database::get_pool().await {
        Ok(pool) => match checkpoint::CheckpointStore::new(pool) {
            Ok(store) => {
                match store.latest(&run_id, model_spec.parameter_count()).await {
                    Ok(Some(saved)) => {
                        match state_value.resume_from(saved.parameters, saved.epoch) {
                            Ok(()) => info!(
                                "Resumed {} from checkpoint at epoch {} (loss {:?})",
                                run_id, saved.epoch, saved.loss
                            ),
                            Err(e) => error!("Ignoring incompatible checkpoint: {:?}", e),
                        }
                    }
                    Ok(None) => info!("No checkpoint for {}; starting from the seed", run_id),
                    Err(e) => error!("Could not read checkpoints: {:?}", e),
                }
                Some(store)
            }
            Err(e) => {
                error!("Checkpointing disabled: {:?}", e);
                None
            }
        },
        Err(e) => {
            error!("Checkpointing disabled, no database connection: {:?}", e);
            None
        }
    };

    let state = Arc::new(Mutex::new(state_value));

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
            common::NodeRole::An,
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
    let mut last_checkpointed_epoch = 0_u64;

    loop {
        let (_channel, mut consumer) = messaging::connect_with_retries(
            &amqp_addr,
            queue_name,
            consumer_tag,
            max_retries,
            backoff_ms,
            max_backoff_ms,
        )
        .await?;

        // Drive training rounds onto ki_task_queue. Without this nothing ever
        // publishes there, so the Ki nodes sit idle and no round completes.
        // The scheduler gets its own channel so a slow publish cannot block the
        // consumer loop that feeds it results.
        let scheduler_cancel = CancellationToken::new();
        match messaging::establish_connection(&amqp_addr).await {
            Ok(scheduler_channel) => {
                let scheduler = scheduler::Scheduler::new(
                    scheduler_channel,
                    Arc::clone(&state),
                    settings.training_epochs,
                    settings.epoch_interval(),
                );
                let token = scheduler_cancel.clone();
                let mut scheduler_shutdown = shutdown_rx.clone();
                tokio::spawn(async move {
                    let _ = scheduler_shutdown.changed().await;
                    token.cancel();
                });
                let token = scheduler_cancel.clone();
                tokio::spawn(async move { scheduler.run(token).await });
            }
            Err(e) => error!(
                "Scheduler could not open a publish channel; no training rounds \
                 will be dispatched from this node: {:?}",
                e
            ),
        }

        info!("An node is running and waiting for tasks...");

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    info!("Shutdown signal received, stopping An node...");
                    scheduler_cancel.cancel();
                    return Ok(());
                }
                delivery = consumer.next() => {
                    match delivery {
                        Some(Ok(delivery)) => {
                            match serde_json::from_slice::<Task>(&delivery.data) {
                                Ok(task_message) => {
                                    info!("Received task: {:?}", task_message);
                                    let started_at = Instant::now();
                                    let outcome = state
                                        .lock()
                                        .await
                                        .process_task(task_message)
                                        .await;
                                    if outcome.is_ok() {
                                        logging_metrics::record_task_processed(started_at);
                                        maybe_checkpoint(
                                            checkpoints.as_ref(),
                                            &run_id,
                                            &state,
                                            settings.checkpoint_interval_epochs,
                                            &mut last_checkpointed_epoch,
                                        )
                                        .await;
                                    }
                                    match outcome {
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
    use uuid::Uuid;

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

    fn test_spec() -> MlpSpec {
        MlpSpec::new(crate::dataset::INPUTS, 4, crate::dataset::OUTPUTS)
    }

    fn test_state(shards: usize) -> AnNodeState {
        AnNodeState::new(test_spec(), DatasetSpec::new(64, 1), shards, 0.5, 3)
    }

    fn reply_task(gradient: Vec<f32>, loss: f32, samples: usize) -> Task {
        Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: serde_json::to_string(&GradientReply {
                gradient,
                loss,
                samples,
            })
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn a_full_round_steps_the_parameters_down_the_gradient() {
        let mut state = test_state(1);
        let before = state.parameters().to_vec();
        let gradient = vec![1.0_f32; before.len()];

        state
            .process_task(reply_task(gradient, 0.5, 10))
            .await
            .expect("accepted");

        // One shard, uniform gradient of 1, learning rate 0.5.
        for (after, start) in state.parameters().iter().zip(&before) {
            assert!((after - (start - 0.5)).abs() < 1e-6);
        }
        assert_eq!(state.epochs_completed(), 1);
        assert_eq!(state.last_epoch_loss(), Some(0.5));
        assert_eq!(state.pending_gradients(), 0, "accumulator must reset");
    }

    #[tokio::test]
    async fn shards_are_weighted_by_their_sample_count() {
        // Two shards of unequal size: the mean gradient over all 30 samples is
        // (1*10 + 4*20)/30 = 3, not the unweighted shard average of 2.5.
        let mut state = test_state(2);
        let before = state.parameters().to_vec();
        let width = before.len();

        state
            .process_task(reply_task(vec![1.0; width], 1.0, 10))
            .await
            .expect("accepted");
        state
            .process_task(reply_task(vec![4.0; width], 4.0, 20))
            .await
            .expect("accepted");

        for (after, start) in state.parameters().iter().zip(&before) {
            assert!(
                (after - (start - 0.5 * 3.0)).abs() < 1e-5,
                "expected a sample-weighted mean of 3"
            );
        }
        assert_eq!(state.last_epoch_loss(), Some(3.0));
    }

    #[tokio::test]
    async fn a_mismatched_gradient_is_rejected_without_mutating_the_model() {
        let mut state = test_state(1);
        let before = state.parameters().to_vec();

        let err = state
            .process_task(reply_task(vec![1.0; 3], 0.1, 5))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Gradient length mismatch"));
        assert_eq!(state.parameters(), &before[..]);
        assert_eq!(state.pending_gradients(), 0);
    }

    #[tokio::test]
    async fn a_non_finite_gradient_is_rejected_by_the_accumulator() {
        // NaN poisons every parameter it touches and cannot be recovered from.
        // Constructed directly rather than through a task, because JSON cannot
        // even represent NaN — see the test below for that layer.
        let mut state = test_state(1);
        let before = state.parameters().to_vec();
        let width = before.len();

        let err = state
            .accumulate(GradientReply {
                gradient: vec![f32::NAN; width],
                loss: 0.1,
                samples: 5,
            })
            .unwrap_err();

        assert!(err.to_string().contains("not finite"), "got: {err}");
        assert_eq!(state.parameters(), &before[..]);
        assert_eq!(state.pending_gradients(), 0);
    }

    #[tokio::test]
    async fn a_non_finite_gradient_cannot_even_survive_the_wire() {
        // serde_json encodes NaN and infinity as `null`, which fails to decode
        // back into f32. So a corrupt worker cannot deliver one over AMQP at
        // all; the accumulator's guard covers the in-process path.
        let mut state = test_state(1);
        let before = state.parameters().to_vec();
        let width = before.len();

        let err = state
            .process_task(reply_task(vec![f32::INFINITY; width], 0.1, 5))
            .await
            .unwrap_err();

        assert!(
            err.is::<serde_json::Error>(),
            "expected a decode failure, got: {err}"
        );
        assert_eq!(state.parameters(), &before[..]);
    }

    #[tokio::test]
    async fn a_gradient_over_zero_samples_is_rejected() {
        let mut state = test_state(1);
        let width = state.parameters().len();

        let err = state
            .process_task(reply_task(vec![1.0; width], 0.1, 0))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("zero samples"));
    }

    #[tokio::test]
    async fn resuming_replaces_the_parameters_and_continues_the_epoch_count() {
        let mut state = test_state(1);
        let restored: Vec<f32> = (0..state.parameters().len()).map(|i| i as f32).collect();

        state.resume_from(restored.clone(), 125).expect("resume");

        assert_eq!(state.parameters(), &restored[..]);
        assert_eq!(state.epochs_completed(), 125);
    }

    #[tokio::test]
    async fn a_checkpoint_of_the_wrong_shape_is_refused() {
        // Loading a mismatched vector would corrupt every gradient applied to
        // it afterwards, so it must not silently succeed.
        let mut state = test_state(1);
        let before = state.parameters().to_vec();

        let err = state.resume_from(vec![0.0; 3], 10).unwrap_err();

        assert!(err.to_string().contains("checkpoint has 3 parameters"));
        assert_eq!(state.parameters(), &before[..]);
        assert_eq!(state.epochs_completed(), 0);
    }

    #[tokio::test]
    async fn initial_parameters_are_not_all_zero() {
        // Zero-initialised weights leave every hidden unit symmetric, so the
        // network could never use more than one of them.
        let state = test_state(1);
        assert!(state.parameters().iter().any(|&p| p != 0.0));
        assert_eq!(state.parameters().len(), test_spec().parameter_count());
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
        let (_channel, mut consumer) = messaging::connect_with_retries(
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
