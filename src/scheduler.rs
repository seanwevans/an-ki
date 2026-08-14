//! Drives training rounds by dispatching work to Ki nodes.
//!
//! Each epoch the scheduler publishes one task per model shard onto
//! `ki_task_queue`. Every Ki node consumes from that same queue, so the broker
//! distributes the shards across whichever workers are alive — there is no
//! routing decision to make here, only how much work to emit and when.
//!
//! The shard count matters for correctness, not just throughput: the An node
//! accumulates gradients and only publishes an updated model once it has
//! received `model_shards` of them. Emitting a different number per epoch would
//! leave the round half-finished.

use std::sync::Arc;
use std::time::Duration;

use lapin::Channel;
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

use crate::an_node::AnNodeState;
use crate::common::{GradientRequest, Task, TaskType};
use crate::dataset::DatasetSpec;
use crate::error::AnKiError;
use crate::messaging;
use crate::model::MlpSpec;

/// Queue Ki nodes take their work from.
pub const KI_TASK_QUEUE: &str = "ki_task_queue";

/// Builds the tasks that make up one epoch: one gradient request per shard,
/// each naming a different slice of the dataset.
///
/// The requests differ only in `shard`. That single field is what makes the
/// round data-parallel — with every worker handed the same slice, averaging
/// their gradients would return exactly one gradient's worth of signal.
///
/// # Errors
/// Returns an error if `shards` is zero: an epoch with no tasks would stall the
/// round rather than complete it.
pub fn epoch_tasks(
    spec: MlpSpec,
    dataset: DatasetSpec,
    shards: usize,
    parameters: &[f32],
) -> Result<Vec<Task>, AnKiError> {
    if shards == 0 {
        return Err(AnKiError::Scheduler(
            "model_shards must be greater than 0".into(),
        ));
    }

    (0..shards)
        .map(|shard| {
            let request = GradientRequest {
                spec,
                dataset,
                shard,
                shards,
                parameters: parameters.to_vec(),
            };
            let data = serde_json::to_string(&request).map_err(|e| {
                AnKiError::Scheduler(format!("failed to serialize gradient request: {e}"))
            })?;
            Ok(Task {
                task_id: Uuid::new_v4(),
                task_type: TaskType::GradientUpdate,
                data,
            })
        })
        .collect()
}

/// Publishes training rounds onto [`KI_TASK_QUEUE`].
pub struct Scheduler {
    channel: Channel,
    state: Arc<Mutex<AnNodeState>>,
    epochs: u32,
    interval: Duration,
}

impl Scheduler {
    pub fn new(
        channel: Channel,
        state: Arc<Mutex<AnNodeState>>,
        epochs: u32,
        interval: Duration,
    ) -> Self {
        Self {
            channel,
            state,
            epochs,
            interval,
        }
    }

    /// Publishes one epoch's worth of tasks, reading the current parameters so
    /// each round starts from the model the previous round produced.
    pub async fn dispatch_epoch(&self) -> Result<usize, AnKiError> {
        let (spec, dataset, shards, parameters) = {
            let state = self.state.lock().await;
            (
                state.spec(),
                state.dataset(),
                state.shards(),
                state.parameters().to_vec(),
            )
        };
        let tasks = epoch_tasks(spec, dataset, shards, &parameters)?;

        for task in &tasks {
            let payload = serde_json::to_vec(task).map_err(|e| {
                AnKiError::Scheduler(format!("failed to serialize task {}: {e}", task.task_id))
            })?;
            messaging::publish_message(&self.channel, KI_TASK_QUEUE, &payload).await?;
        }

        Ok(tasks.len())
    }

    /// Runs `epochs` rounds `interval` apart, stopping early if `cancel` fires.
    ///
    /// A failed epoch is logged and the round is skipped rather than aborting
    /// the run: a transient broker error should not end training, and the An
    /// node's accumulator tolerates a missed round because it only publishes a
    /// model once a full set of gradients has arrived.
    pub async fn run(&self, cancel: CancellationToken) {
        if self.epochs == 0 {
            info!("training_epochs is 0; scheduler has nothing to dispatch");
            return;
        }

        if let Err(e) = messaging::declare_queue(&self.channel, KI_TASK_QUEUE).await {
            error!("Scheduler cannot declare {}: {:?}", KI_TASK_QUEUE, e);
            return;
        }

        let shards = self.state.lock().await.shards();
        let mut ticker = time::interval(self.interval);
        info!(
            "Scheduler dispatching {} epoch(s) of {} shard(s) every {:?}",
            self.epochs, shards, self.interval
        );

        for epoch in 1..=self.epochs {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Scheduler stopped after {} epoch(s)", epoch - 1);
                    return;
                }
                _ = ticker.tick() => {
                    match self.dispatch_epoch().await {
                        Ok(count) => info!("Dispatched epoch {}/{} ({} tasks)", epoch, self.epochs, count),
                        Err(e) => error!("Epoch {} failed to dispatch: {:?}", epoch, e),
                    }
                }
            }
        }

        info!("Scheduler completed {} epoch(s)", self.epochs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset;
    use crate::ki_node;
    use crate::model;

    fn spec() -> MlpSpec {
        MlpSpec::new(dataset::INPUTS, 16, dataset::OUTPUTS)
    }

    fn data() -> DatasetSpec {
        DatasetSpec::new(256, 20_260_814)
    }

    #[test]
    fn an_epoch_emits_one_request_per_shard() {
        let spec = spec();
        let parameters = spec.initialize(1);
        let tasks = epoch_tasks(spec, data(), 3, &parameters).expect("tasks");

        assert_eq!(tasks.len(), 3);
        for (index, task) in tasks.iter().enumerate() {
            assert_eq!(task.task_type, TaskType::GradientUpdate);
            let request: GradientRequest =
                serde_json::from_str(&task.data).expect("request decodes");
            assert_eq!(request.shard, index);
            assert_eq!(request.shards, 3);
            assert_eq!(request.parameters, parameters);
        }
    }

    #[test]
    fn each_shard_is_asked_for_a_different_slice() {
        // This is what makes the round data-parallel. If every worker were sent
        // the same slice, averaging their gradients would return exactly one
        // gradient's worth of signal.
        let spec = spec();
        let tasks = epoch_tasks(spec, data(), 4, &spec.initialize(1)).expect("tasks");

        let mut shards: Vec<usize> = tasks
            .iter()
            .map(|task| {
                serde_json::from_str::<GradientRequest>(&task.data)
                    .expect("decodes")
                    .shard
            })
            .collect();
        shards.sort_unstable();
        assert_eq!(shards, vec![0, 1, 2, 3]);
    }

    #[test]
    fn requests_do_not_carry_the_dataset() {
        // Only the seed crosses the wire, so the payload stays proportional to
        // the model rather than to the data.
        let spec = spec();
        let dataset = DatasetSpec::new(100_000, 1);
        let tasks = epoch_tasks(spec, dataset, 1, &spec.initialize(1)).expect("tasks");

        assert!(
            tasks[0].data.len() < 4_096,
            "payload grew with the dataset: {} bytes",
            tasks[0].data.len()
        );
    }

    #[test]
    fn every_task_in_an_epoch_gets_its_own_id() {
        let spec = spec();
        let tasks = epoch_tasks(spec, data(), 4, &spec.initialize(1)).expect("tasks");

        let mut ids: Vec<_> = tasks.iter().map(|task| task.task_id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4, "duplicate task ids would confuse recovery");
    }

    #[test]
    fn zero_shards_is_refused_rather_than_producing_an_empty_epoch() {
        // An empty epoch would publish nothing, so the An node would wait
        // forever for gradients that were never requested.
        let spec = spec();
        let err = epoch_tasks(spec, data(), 0, &spec.initialize(1)).expect_err("must fail");
        assert!(err.to_string().contains("model_shards"));
    }

    /// Runs `epochs` complete rounds through the real functions of all three
    /// stages — scheduler, Ki compute, An aggregation — with only the broker
    /// left out. Returns the final state.
    async fn train(shards: usize, epochs: usize) -> AnNodeState {
        let spec = spec();
        let dataset = data();
        let mut state = AnNodeState::new(spec, dataset, shards, 1.0, 7);

        for _ in 0..epochs {
            let tasks =
                epoch_tasks(spec, dataset, shards, state.parameters()).expect("epoch tasks");
            for task in tasks {
                let reply = ki_node::perform_computation(task)
                    .await
                    .expect("ki computes a gradient");
                state
                    .process_task(reply, None)
                    .await
                    .expect("an node accepts the gradient");
            }
        }
        state
    }

    /// The claim this whole project makes: a network trained across several
    /// workers learns a function none of them could express linearly.
    ///
    /// The dataset is a circular decision boundary, so a model without a working
    /// hidden layer cannot exceed roughly chance here. Reaching high accuracy is
    /// evidence that the gradients were computed, shipped, averaged, and applied
    /// correctly at every step.
    #[tokio::test]
    async fn training_across_four_shards_learns_the_task() {
        let state = train(4, 400).await;

        let samples = dataset::generate(data());
        let accuracy =
            model::accuracy(&state.spec(), state.parameters(), &samples).expect("accuracy");
        let loss = state.last_epoch_loss().expect("an epoch completed");

        assert_eq!(state.epochs_completed(), 400);
        assert!(
            accuracy > 0.85,
            "expected the model to learn the circle, got accuracy {accuracy} (loss {loss})"
        );
        assert!(loss < 0.25, "loss stalled at {loss}");
    }

    #[tokio::test]
    async fn loss_falls_over_successive_epochs() {
        let early = train(4, 5).await.last_epoch_loss().expect("epoch");
        let late = train(4, 150).await.last_epoch_loss().expect("epoch");

        assert!(late < early, "loss did not fall: {early} then {late}");
    }

    /// Sharding must not change the answer. Four workers on quarter-shards
    /// compute the same averaged gradient as one worker on the whole dataset,
    /// because the An node weights each contribution by its sample count.
    #[tokio::test]
    async fn sharding_does_not_change_the_result() {
        let one = train(1, 20).await;
        let four = train(4, 20).await;

        for (a, b) in one.parameters().iter().zip(four.parameters()) {
            assert!(
                (a - b).abs() < 1e-3,
                "sharded training diverged: {a} vs {b}"
            );
        }
    }

    #[tokio::test]
    async fn a_partial_epoch_leaves_the_model_untouched() {
        let spec = spec();
        let dataset = data();
        let mut state = AnNodeState::new(spec, dataset, 3, 1.0, 7);
        let before = state.parameters().to_vec();

        // Deliver two of the three shards.
        for task in epoch_tasks(spec, dataset, 3, &before)
            .expect("tasks")
            .into_iter()
            .take(2)
        {
            let reply = ki_node::perform_computation(task).await.expect("gradient");
            state.process_task(reply, None).await.expect("accepted");
        }

        assert_eq!(state.parameters(), &before[..]);
        assert_eq!(state.pending_gradients(), 2);
        assert_eq!(state.epochs_completed(), 0);
    }
}
