// task_recovery.rs: Implements task persistence and recovery for robustness.

use crate::common::{Task, TaskType};
use crate::database::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct TaskRecoveryManager {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub pool: PgPool,
}

impl TaskRecoveryManager {
    pub fn new(pool: PgPool) -> Self {
        TaskRecoveryManager {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            pool,
        }
    }

    pub async fn add_task(&self, task: Task) {
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.task_id, task.clone());
        info!("Added task to recovery manager: {:?}", task);

        match self.pool.get().await {
            Ok(conn) => {
                if let Err(e) = conn
                    .execute(
                        "UPSERT INTO tasks (task_id, data) VALUES ($1, $2)",
                        &[&task.task_id, &task.data],
                    )
                    .await
                {
                    error!("Failed to persist task: {:?}", e);
                }
            }
            Err(e) => error!("Failed to acquire connection: {:?}", e),
        }
    }

    pub async fn remove_task(&self, task_id: &Uuid) {
        let mut tasks = self.tasks.write().await;
        if tasks.remove(task_id).is_some() {
            info!("Removed task from recovery manager: {}", task_id);
            match self.pool.get().await {
                Ok(conn) => {
                    if let Err(e) = conn
                        .execute("DELETE FROM tasks WHERE task_id = $1", &[task_id])
                        .await
                    {
                        error!("Failed to remove task from database: {:?}", e);
                    }
                }
                Err(e) => error!("Failed to acquire connection: {:?}", e),
            }
        } else {
            error!("Task not found for removal: {}", task_id);
        }
    }

    pub async fn recover_tasks(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.pool.get().await?;
        let rows = conn.query("SELECT task_id, data FROM tasks", &[]).await?;
        let mut tasks = self.tasks.write().await;
        tasks.clear();
        for row in rows {
            let task_id: Uuid = row.get("task_id");
            let data: String = row.get("data");
            tasks.insert(task_id, Task { task_id, data });
        }
        info!("Recovered tasks from database.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::get_pool;

    #[tokio::test]
    async fn test_task_recovery() {
        let pool = get_pool().await.expect("pool");
        let recovery_manager = TaskRecoveryManager::new(pool.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Test data".to_string(),
        };

        recovery_manager.add_task(task.clone()).await;
        recovery_manager.remove_task(&task.task_id).await;

        // Recover tasks from database
        recovery_manager.add_task(task.clone()).await;
        recovery_manager.recover_tasks().await.unwrap();
        assert!(recovery_manager
            .tasks
            .read()
            .await
            .contains_key(&task.task_id));

        // Clean up
        pool.get()
            .await
            .unwrap()
            .execute("DELETE FROM tasks WHERE task_id = $1", &[&task.task_id])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_recover_tasks_file_absent() {
        let pool = get_pool().await.expect("pool");
        // Ensure table is empty
        pool.get()
            .await
            .unwrap()
            .execute("DELETE FROM tasks", &[])
            .await
            .unwrap();
        let recovery_manager = TaskRecoveryManager::new(pool);
        assert!(recovery_manager.recover_tasks().await.is_ok());
        assert!(recovery_manager.tasks.read().await.is_empty());
    }
}
