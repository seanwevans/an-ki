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
use crate::common::{Task, TaskType};
use crate::error::AnKiError;
use crate::messaging;

/// Queue Ki nodes take their work from.
pub const KI_TASK_QUEUE: &str = "ki_task_queue";

/// How training rounds are coordinated across nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrainingMode {
    /// An nodes hold the parameters, Ki nodes return gradients against them.
    #[default]
    ParameterServer,
}

/// Builds the tasks that make up one epoch.
///
/// Every shard receives the same starting parameters and returns a gradient; the
/// An node averages them. Splitting the parameter vector per shard would be the
/// data-parallel alternative, but the aggregation in
/// [`AnNodeState::process_task`](crate::an_node::AnNodeState::process_task)
/// averages equal-length gradients, so shards must span the whole vector.
///
/// # Errors
/// Returns an error if `parameters` cannot be serialized, or if `shards` is zero
/// — an epoch with no tasks would stall the round rather than complete it.
pub fn epoch_tasks(
    mode: TrainingMode,
    shards: usize,
    parameters: &[f32],
) -> Result<Vec<Task>, AnKiError> {
    if shards == 0 {
        return Err(AnKiError::Scheduler(
            "model_shards must be greater than 0".into(),
        ));
    }

    let data = serde_json::to_string(parameters)
        .map_err(|e| AnKiError::Scheduler(format!("failed to serialize parameters: {e}")))?;

    let task_type = match mode {
        TrainingMode::ParameterServer => TaskType::GradientUpdate,
    };

    Ok((0..shards)
        .map(|_| Task {
            task_id: Uuid::new_v4(),
            task_type: task_type.clone(),
            data: data.clone(),
        })
        .collect())
}

/// Publishes training rounds onto [`KI_TASK_QUEUE`].
pub struct Scheduler {
    channel: Channel,
    state: Arc<Mutex<AnNodeState>>,
    mode: TrainingMode,
    shards: usize,
    epochs: u32,
    interval: Duration,
}

impl Scheduler {
    pub fn new(
        channel: Channel,
        state: Arc<Mutex<AnNodeState>>,
        mode: TrainingMode,
        shards: usize,
        epochs: u32,
        interval: Duration,
    ) -> Self {
        Self {
            channel,
            state,
            mode,
            shards,
            epochs,
            interval,
        }
    }

    /// Publishes one epoch's worth of tasks, reading the current parameters so
    /// each round starts from the model the previous round produced.
    pub async fn dispatch_epoch(&self) -> Result<usize, AnKiError> {
        let parameters = self.state.lock().await.parameters().to_vec();
        let tasks = epoch_tasks(self.mode, self.shards, &parameters)?;

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

        let mut ticker = time::interval(self.interval);
        info!(
            "Scheduler dispatching {} epoch(s) of {} shard(s) every {:?}",
            self.epochs, self.shards, self.interval
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

    #[test]
    fn an_epoch_emits_one_task_per_shard() {
        let tasks = epoch_tasks(TrainingMode::ParameterServer, 3, &[0.5, 1.5]).expect("tasks");

        assert_eq!(tasks.len(), 3);
        for task in &tasks {
            assert_eq!(task.task_type, TaskType::GradientUpdate);
            let payload: Vec<f32> = serde_json::from_str(&task.data).expect("payload decodes");
            assert_eq!(payload, vec![0.5, 1.5]);
        }
    }

    #[test]
    fn every_task_in_an_epoch_gets_its_own_id() {
        let tasks = epoch_tasks(TrainingMode::ParameterServer, 4, &[0.0]).expect("tasks");

        let mut ids: Vec<_> = tasks.iter().map(|task| task.task_id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4, "duplicate task ids would confuse recovery");
    }

    #[test]
    fn zero_shards_is_refused_rather_than_producing_an_empty_epoch() {
        // An empty epoch would publish nothing, so the An node would wait
        // forever for gradients that were never requested.
        let err = epoch_tasks(TrainingMode::ParameterServer, 0, &[1.0]).expect_err("must fail");
        assert!(err.to_string().contains("model_shards"));
    }

    #[test]
    fn an_empty_parameter_vector_still_produces_decodable_tasks() {
        let tasks = epoch_tasks(TrainingMode::ParameterServer, 1, &[]).expect("tasks");
        let payload: Vec<f32> = serde_json::from_str(&tasks[0].data).expect("payload decodes");
        assert!(payload.is_empty());
    }

    /// The whole point of the scheduler: a round it dispatches must be
    /// consumable by a Ki node and aggregate into a model update on the An node.
    ///
    /// This drives the real functions from all three stages — scheduler,
    /// `ki_node::perform_computation`, `AnNodeState::process_task` — and only
    /// leaves out the broker in the middle, so it catches payload-shape
    /// mismatches between the stages. Those are exactly the breakages that kept
    /// this loop from ever running: the previous scheduler emitted
    /// `data: String::new()`, which `perform_computation` cannot parse.
    #[tokio::test]
    async fn a_dispatched_epoch_flows_through_ki_and_updates_the_model() {
        use crate::ki_node;

        let shards = 3;
        let initial = vec![1.0_f32, -2.0_f32];
        let mut an_state = AnNodeState::with_parameters(initial.clone());

        let tasks = epoch_tasks(TrainingMode::ParameterServer, shards, &initial).expect("tasks");
        assert_eq!(tasks.len(), shards);

        for task in tasks {
            // Ki side: compute the gradient for this shard.
            let result = ki_node::perform_computation(task)
                .await
                .expect("ki computes a gradient");
            // An side: accumulate it. No channel, so no broadcast is attempted.
            an_state
                .process_task(result, None, shards)
                .await
                .expect("an node accepts the gradient");
        }

        // Placeholder compute returns 2x the input, so each shard's gradient is
        // [2, -4]; averaging `shards` of them and subtracting leaves [-1, 2].
        assert_eq!(an_state.parameters(), &[-1.0_f32, 2.0_f32]);
        assert_eq!(
            an_state.pending_gradients(),
            0,
            "a completed round must reset the accumulator for the next epoch"
        );
    }

    #[tokio::test]
    async fn a_partial_epoch_leaves_the_model_untouched() {
        use crate::ki_node;

        let shards = 3;
        let initial = vec![1.0_f32];
        let mut an_state = AnNodeState::with_parameters(initial.clone());
        let tasks = epoch_tasks(TrainingMode::ParameterServer, shards, &initial).expect("tasks");

        // Deliver only two of the three shards.
        for task in tasks.into_iter().take(2) {
            let result = ki_node::perform_computation(task).await.expect("gradient");
            an_state
                .process_task(result, None, shards)
                .await
                .expect("ok");
        }

        assert_eq!(
            an_state.parameters(),
            &initial[..],
            "the model must not move until a full round has arrived"
        );
        assert_eq!(an_state.pending_gradients(), 2);
    }
}
