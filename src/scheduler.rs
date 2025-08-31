// scheduler.rs: Implements a task scheduler that assigns tasks to Ki nodes based on load and capacity.

use crate::load_balancer::LoadBalancer;
use crate::common::{Task, TaskType};
use tokio::sync::mpsc;
use uuid::Uuid;
use tracing::{info, error};
use std::time::Duration;
use tokio::time;
use std::error::Error;

/// Specifies how the training tasks should be coordinated across nodes.
#[derive(Clone, Copy)]
pub enum TrainingMode {
    /// A central parameter server collects gradients and distributes updated parameters.
    ParameterServer,
    /// Nodes perform an all-reduce operation to share gradients amongst themselves.
    AllReduce,
}

pub struct Scheduler {
    load_balancer: LoadBalancer,
    task_tx: mpsc::Sender<Task>,
    mode: TrainingMode,
    epochs: u32,
}

impl Scheduler {
    pub fn new(load_balancer: LoadBalancer, task_tx: mpsc::Sender<Task>, mode: TrainingMode, epochs: u32) -> Self {
        Scheduler {
            load_balancer,
            task_tx,
            mode,
            epochs,
        }
    }

    pub async fn schedule_task(&self, task: Task) -> Result<(), Box<dyn Error>> {
        if let Some(node_id) = self.load_balancer.assign_task().await {
            info!("Scheduling task {} to node {}", task.task_id, node_id);
            self.task_tx
                .send(task)
                .await
                .map_err(|e| {
                    error!("Failed to send task: {:?}", e);
                    e
                })?;
            Ok(())
        } else {
            error!("No available nodes to schedule task {}");
            Err("No available nodes".into())
        }
    }

    /// Runs the scheduler for a fixed number of epochs. In `ParameterServer` mode
    /// a `ParameterPull` task is broadcast to all Ki nodes so they can fetch the
    /// latest model from the An node. In `AllReduce` mode a `GradientUpdate` task is
    /// broadcast which instructs nodes to compute gradients on their shard.
    pub async fn run_scheduler(&self, interval: Duration) {
        let mut ticker = time::interval(interval);
        for _ in 0..self.epochs {
            ticker.tick().await;
            let task_type = match self.mode {
                TrainingMode::ParameterServer => TaskType::ParameterPull,
                TrainingMode::AllReduce => TaskType::GradientUpdate,
            };
            let task = Task {
                task_id: Uuid::new_v4(),
                task_type,
                data: String::new(),
            };
            if let Err(e) = self.broadcast_task(task).await {
                error!("Failed to dispatch task: {:?}", e);
            }
        }
    }

    /// Sends a task to every node managed by the scheduler.
    async fn broadcast_task(&self, task: Task) -> Result<(), Box<dyn Error>> {
        let nodes = self.load_balancer.nodes.read().await;
        for node_id in nodes.keys() {
            info!("Broadcasting task {} to node {}", task.task_id, node_id);
            self.task_tx
                .send(task.clone())
                .await
                .map_err(|e| {
                    error!("Failed to send task: {:?}", e);
                    e
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_balancer::LoadBalancer;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_schedule_task() {
        let load_balancer = LoadBalancer::new();
        let (task_tx, mut task_rx) = mpsc::channel(10);
        let scheduler = Scheduler::new(load_balancer.clone(), task_tx, TrainingMode::ParameterServer, 1);

        load_balancer.add_node(Uuid::new_v4()).await;
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Test data".to_string(),
        };

        scheduler.schedule_task(task.clone()).await.unwrap();
        let received_task = task_rx.recv().await.unwrap();
        assert_eq!(received_task.task_id, task.task_id);
    }
}
