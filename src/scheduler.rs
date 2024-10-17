// scheduler.rs: Implements a task scheduler that assigns tasks to Ki nodes based on load and capacity.

use crate::load_balancer::LoadBalancer;
use tokio::sync::mpsc;
use uuid::Uuid;
use tracing::{info, error};
use std::time::Duration;
use tokio::time;
use std::error::Error;

#[derive(Clone, Debug)]
pub struct Task {
    pub task_id: Uuid,
    pub data: String,
}

pub struct Scheduler {
    load_balancer: LoadBalancer,
    task_tx: mpsc::Sender<Task>,
}

impl Scheduler {
    pub fn new(load_balancer: LoadBalancer, task_tx: mpsc::Sender<Task>) -> Self {
        Scheduler {
            load_balancer,
            task_tx,
        }
    }

    pub async fn schedule_task(&self, task: Task) -> Result<(), Box<dyn Error>> {
        if let Some(node_id) = self.load_balancer.assign_task() {
            info!("Scheduling task {} to node {}", task.task_id, node_id);
            self.task_tx.send(task).await?;
            Ok(())
        } else {
            error!("No available nodes to schedule task {}");
            Err("No available nodes".into())
        }
    }

    pub async fn run_scheduler(&self, interval: Duration) {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;
            // Placeholder for actual task generation or retrieval logic
            let task = Task {
                task_id: Uuid::new_v4(),
                data: "Sample task data".to_string(),
            };

            if let Err(e) = self.schedule_task(task).await {
                error!("Failed to schedule task: {:?}", e);
            }
        }
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
        let scheduler = Scheduler::new(load_balancer.clone(), task_tx);

        load_balancer.add_node(Uuid::new_v4());
        let task = Task {
            task_id: Uuid::new_v4(),
            data: "Test data".to_string(),
        };

        scheduler.schedule_task(task.clone()).await.unwrap();
        let received_task = task_rx.recv().await.unwrap();
        assert_eq!(received_task.task_id, task.task_id);
    }
}
