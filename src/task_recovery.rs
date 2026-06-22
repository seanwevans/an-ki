//! Task persistence and recovery utilities.
//!
//! The [`TaskRecoveryManager`] persists tasks to a PostgreSQL database so they can
//! be restored after failures and tracks them in an in-memory map.

use crate::common::{Task, TaskType};
use crate::database::PgPool;
use crate::error::AnKiError;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use uuid::Uuid;

fn task_type_from_str(task_type: &str, task_id: &Uuid) -> Option<TaskType> {
    match task_type {
        "GradientUpdate" => Some(TaskType::GradientUpdate),
        "ParameterPull" => Some(TaskType::ParameterPull),
        other => {
            error!("Unknown task_type '{}' for task {}", other, task_id);
            None
        }
    }
}

#[derive(Clone)]
pub struct TaskRecoveryManager {
    /// In-memory cache of tasks keyed by ID.
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    /// Database connection pool used for persistence operations.
    pub pool: PgPool,
}

impl TaskRecoveryManager {
    /// Creates a new [`TaskRecoveryManager`].
    ///
    /// # Parameters
    /// * `pool` - Connection pool for the tasks database.
    pub fn new(pool: PgPool) -> Self {
        TaskRecoveryManager {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            pool,
        }
    }

    /// Adds a task to the in-memory map and attempts to persist it to the database.
    ///
    /// The task is inserted into the in-memory map before contacting the database so
    /// the write lock is not held across an await. If persistence fails at any stage,
    /// the in-memory insertion is rolled back and an error is returned so callers can
    /// react accordingly.
    pub async fn add_task(&self, task: Task) -> Result<(), AnKiError> {
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.task_id, task.clone());
        info!("Added task to recovery manager: {:?}", task);

        // Release the write guard before attempting any database operations to avoid
        // blocking other readers/writers while awaiting a connection.
        drop(tasks);

        let conn = match self.pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Failed to acquire connection: {:?}", e);
                let mut tasks = self.tasks.write().await;
                tasks.remove(&task.task_id);
                return Err(AnKiError::TaskRecovery(e.to_string()));
            }
        };

        let task_type = format!("{:?}", task.task_type);
        if let Err(e) = conn
            .execute(
                "INSERT INTO tasks (task_id, task_type, data) VALUES ($1,$2,$3) \
ON CONFLICT (task_id) DO UPDATE SET task_type=$2, data=$3",
                &[&task.task_id, &task_type, &task.data],
            )
            .await
        {
            error!("Failed to persist task: {:?}", e);
            let mut tasks = self.tasks.write().await;
            tasks.remove(&task.task_id);
            return Err(AnKiError::TaskRecovery(e.to_string()));
        }

        Ok(())
    }

    /// Looks up a task by ID, first checking memory and then falling back to the database.
    ///
    /// When the task is found in the database, the in-memory cache is repopulated
    /// before returning a clone of the recovered task.
    ///
    /// # Parameters
    /// * `task_id` - Identifier of the task to retrieve.
    pub async fn get_task(&self, task_id: &Uuid) -> Result<Option<Task>, AnKiError> {
        if let Some(task) = self.tasks.read().await.get(task_id).cloned() {
            return Ok(Some(task));
        }

        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;
        let row = conn
            .query_opt(
                "SELECT task_id, task_type, data FROM tasks WHERE task_id = $1",
                &[task_id],
            )
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;

        let Some(row) = row else {
            return Ok(None);
        };

        let task_id: Uuid = row.get("task_id");
        let task_type_str: String = row.get("task_type");
        let data: String = row.get("data");
        let Some(task_type) = task_type_from_str(&task_type_str, &task_id) else {
            return Ok(None);
        };
        let task = Task {
            task_id,
            task_type,
            data,
        };

        self.tasks.write().await.insert(task.task_id, task.clone());

        Ok(Some(task))
    }

    /// Removes a task from both memory and the database.
    ///
    /// # Parameters
    /// * `task_id` - Identifier of the task to remove.
    pub async fn remove_task(&self, task_id: &Uuid) -> Result<bool, AnKiError> {
        let removed_task = {
            let mut tasks = self.tasks.write().await;
            tasks.remove(task_id)
        };

        if removed_task.is_some() {
            info!("Removed task from recovery manager: {}", task_id);
        } else {
            debug!("Task not found in recovery manager cache: {}", task_id);
        }

        let conn = match self.pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Failed to acquire connection: {:?}", e);
                if let Some(task) = removed_task.as_ref() {
                    let mut tasks = self.tasks.write().await;
                    tasks.insert(task.task_id, task.clone());
                }
                return Err(AnKiError::TaskRecovery(e.to_string()));
            }
        };

        let deleted_rows = match conn
            .execute("DELETE FROM tasks WHERE task_id = $1", &[task_id])
            .await
        {
            Ok(count) => count,
            Err(e) => {
                error!("Failed to remove task from database: {:?}", e);
                if let Some(task) = removed_task.as_ref() {
                    let mut tasks = self.tasks.write().await;
                    tasks.insert(task.task_id, task.clone());
                }
                return Err(AnKiError::TaskRecovery(e.to_string()));
            }
        };

        if deleted_rows > 0 {
            debug!("Removed persistent task from database: {}", task_id);
        }

        Ok(removed_task.is_some() || deleted_rows > 0)
    }

    pub async fn recover_tasks(&self) -> Result<(), AnKiError> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;
        let rows = conn
            .query("SELECT task_id, task_type, data FROM tasks", &[])
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;
        let mut tasks = self.tasks.write().await;
        tasks.clear();
        for row in rows {
            let task_id: Uuid = row.get("task_id");
            let task_type_str: String = row.get("task_type");
            let data: String = row.get("data");
            let Some(task_type) = task_type_from_str(&task_type_str, &task_id) else {
                continue;
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

// These tests exercise persistence against a live database (via `get_pool`), so
// they are gated behind the `integration-tests` feature and excluded from the
// default `cargo test` run.
#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;
    use crate::config;
    use crate::database::get_pool;
    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use std::str::FromStr;
    use tokio::task::yield_now;
    use tokio::time::{timeout, Duration};
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

        recovery_manager.add_task(task.clone()).await.unwrap();
        assert!(recovery_manager.remove_task(&task.task_id).await.unwrap());

        // Recover tasks from database
        recovery_manager.add_task(task.clone()).await.unwrap();
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
    async fn test_get_task_falls_back_to_database_and_repopulates_cache() {
        let pool = get_pool().await.expect("pool");
        let recovery_manager = TaskRecoveryManager::new(pool.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: "Database-backed task".to_string(),
        };
        let task_type = format!("{:?}", task.task_type);

        pool.get()
            .await
            .unwrap()
            .execute(
                "INSERT INTO tasks (task_id, task_type, data) VALUES ($1,$2,$3)",
                &[&task.task_id, &task_type, &task.data],
            )
            .await
            .unwrap();

        assert!(!recovery_manager
            .tasks
            .read()
            .await
            .contains_key(&task.task_id));

        let recovered = recovery_manager
            .get_task(&task.task_id)
            .await
            .unwrap()
            .expect("task missing");
        assert_eq!(recovered.task_type, TaskType::GradientUpdate);
        assert_eq!(recovered.data, task.data);
        assert!(recovery_manager
            .tasks
            .read()
            .await
            .contains_key(&task.task_id));

        pool.get()
            .await
            .unwrap()
            .execute("DELETE FROM tasks WHERE task_id = $1", &[&task.task_id])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_add_and_update_task() {
        let pool = get_pool().await.expect("pool");
        let recovery_manager = TaskRecoveryManager::new(pool.clone());

        let task_id = Uuid::new_v4();
        let task = Task {
            task_id,
            task_type: TaskType::ParameterPull,
            data: "one".to_string(),
        };

        recovery_manager.add_task(task.clone()).await.unwrap();

        let updated_task = Task {
            task_id,
            task_type: TaskType::GradientUpdate,
            data: "two".to_string(),
        };

        recovery_manager
            .add_task(updated_task.clone())
            .await
            .unwrap();

        let row = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT task_type, data FROM tasks WHERE task_id = $1",
                &[&task_id],
            )
            .await
            .unwrap();
        let task_type: String = row.get(0);
        let data: String = row.get(1);
        assert_eq!(task_type, format!("{:?}", updated_task.task_type));
        assert_eq!(data, updated_task.data);

        pool.get()
            .await
            .unwrap()
            .execute("DELETE FROM tasks WHERE task_id = $1", &[&task_id])
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
            manager_clone.add_task(task_clone).await.unwrap();
        });

        // Wait for the task to be inserted before proceeding.
        let task_id = task.task_id;
        timeout(Duration::from_secs(1), async {
            loop {
                if recovery_manager.tasks.read().await.contains_key(&task_id) {
                    break;
                }
                yield_now().await;
            }
        })
        .await
        .expect("task inserted");

        // We should be able to read the task while the DB operation is waiting, proving
        // the write lock was released.
        {
            let tasks = recovery_manager.tasks.read().await;
            assert!(tasks.contains_key(&task_id));
        }

        // Release the connection so add_task can complete.
        drop(conn);
        handle.await.unwrap();
    }
}
