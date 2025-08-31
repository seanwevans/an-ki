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

    /// Adds a task to the in-memory map and attempts to persist it to the database.
    ///
    /// Note: The database operation occurs after the task has been inserted into the
    /// in-memory map. If the persistence step fails, the task will remain only in
    /// memory. Consider implementing compensating logic (e.g. retrying or removing
    /// the task) to maintain consistency between memory and the database.
    pub async fn add_task(&self, task: Task) {
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.task_id, task.clone());
        info!("Added task to recovery manager: {:?}", task);

        // Release the write guard before attempting any database operations to avoid
        // blocking other readers/writers while awaiting a connection.
        drop(tasks);

        match self.pool.get().await {
            Ok(conn) => {
                let task_type = format!("{:?}", task.task_type);
                if let Err(e) = conn
                    .execute(
                        "UPSERT INTO tasks (task_id, task_type, data) VALUES ($1, $2, $3)",
                        &[&task.task_id, &task_type, &task.data],
                    )
                    .await
                {
                    error!("Failed to persist task: {:?}", e);
                    // At this point the task exists only in memory. A compensation
                    // strategy may be needed if persistence failures must be handled.
                }
            }
            Err(e) => {
                error!("Failed to acquire connection: {:?}", e);
                // The task remains in-memory only if the database connection
                // cannot be established.
            }
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
        let rows = conn
            .query("SELECT task_id, task_type, data FROM tasks", &[])
            .await?;
        let mut tasks = self.tasks.write().await;
        tasks.clear();
        for row in rows {
            let task_id: Uuid = row.get("task_id");
            let task_type_str: String = row.get("task_type");
            let data: String = row.get("data");
            let task_type = match task_type_str.as_str() {
                "GradientUpdate" => TaskType::GradientUpdate,
                "ParameterPull" => TaskType::ParameterPull,
                other => {
                    error!("Unknown task_type '{}' for task {}", other, task_id);
                    continue;
                }
            };
            tasks.insert(
                task_id,
                Task {
                    task_id,
                    task_type,
                    data,
                },
            );
        }
        info!("Recovered tasks from database.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::database::get_pool;
    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use std::str::FromStr;
    use tokio::time::{sleep, Duration};
    use tokio_postgres::{Config, NoTls};

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
        let tasks = recovery_manager.tasks.read().await;
        let recovered = tasks.get(&task.task_id).expect("task missing");
        assert_eq!(recovered.task_type, TaskType::ParameterPull);
        assert_eq!(recovered.data, "Test data");

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

    // Build a PgPool with a single connection to simulate a slow/stalled database
    // response when all connections are in use.
    async fn build_small_pool() -> PgPool {
        let settings = match config::load_settings() {
            Ok(settings) => settings,
            Err(e) => panic!("Failed to load settings: {}", e),
        };
        let pg_config = match Config::from_str(&settings.database_url) {
            Ok(cfg) => cfg,
            Err(e) => panic!("Invalid database config: {}", e),
        };
        let manager = PostgresConnectionManager::new(pg_config, NoTls);
        let pool = Pool::builder()
            .max_size(1)
            .build(manager)
            .await
            .expect("build pool");

        // Run minimal migrations for tasks table.
        let conn = pool.get().await.expect("conn");
        conn.batch_execute(include_str!("../migrations/001_create_tasks.sql"))
            .await
            .expect("migrate tasks");
        conn.batch_execute(include_str!(
            "../migrations/002_create_model_checkpoints.sql"
        ))
        .await
        .expect("migrate checkpoints");
        drop(conn);

        pool
    }

    #[tokio::test]
    async fn test_add_task_no_deadlock_on_slow_db() {
        let pool = build_small_pool().await;
        let recovery_manager = TaskRecoveryManager::new(pool.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Deadlock test".to_string(),
        };

        // Acquire the only connection to block pool.get() calls.
        let conn = pool.get().await.expect("conn");

        // Spawn add_task which will block on acquiring a DB connection.
        let manager_clone = recovery_manager.clone();
        let task_clone = task.clone();
        let handle = tokio::spawn(async move {
            manager_clone.add_task(task_clone).await;
        });

        // Give add_task a moment to insert the task and reach the awaiting connection.
        sleep(Duration::from_millis(100)).await;

        // We should be able to read the task while the DB operation is waiting, proving
        // the write lock was released.
        {
            let tasks = recovery_manager.tasks.read().await;
            assert!(tasks.contains_key(&task.task_id));
        }

        // Release the connection so add_task can complete.
        drop(conn);
        handle.await.unwrap();
    }
}
