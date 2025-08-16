// task_recovery.rs: Implements task persistence and recovery for robustness.
#![allow(dead_code)]

use crate::common::Task;
use std::collections::HashMap;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct TaskRecoveryManager {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub storage_file: String,
}

impl TaskRecoveryManager {
    pub fn new(storage_file: &str) -> Self {
        TaskRecoveryManager {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            storage_file: storage_file.to_string(),
        }
    }

    pub async fn add_task(&self, task: Task) {
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.task_id, task.clone());
        info!("Added task to recovery manager: {:?}", task);
        if let Err(e) = self.persist_tasks().await {
            error!("Failed to persist tasks: {:?}", e);
        }
    }

    pub async fn remove_task(&self, task_id: &Uuid) {
        let mut tasks = self.tasks.write().await;
        if tasks.remove(task_id).is_some() {
            info!("Removed task from recovery manager: {}", task_id);
            if let Err(e) = self.persist_tasks().await {
                error!("Failed to persist tasks: {:?}", e);
            }
        } else {
            error!("Task not found for removal: {}", task_id);
        }
    }

    pub async fn recover_tasks(&self) -> Result<(), Box<dyn Error>> {
        let mut file = match OpenOptions::new().read(true).open(&self.storage_file) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open(&self.storage_file)?;
                return Ok(());
            }
            Err(e) => return Err(Box::new(e)),
        };

        let mut content = String::new();
        file.read_to_string(&mut content)?;

        if !content.is_empty() {
            let recovered_tasks: HashMap<Uuid, Task> = serde_json::from_str(&content)?;
            let mut tasks = self.tasks.write().await;
            *tasks = recovered_tasks;
            info!("Recovered tasks from storage file.");
        }
        Ok(())
    }

    async fn persist_tasks(&self) -> Result<(), Box<dyn Error>> {
        let tasks = self.tasks.read().await;
        let content = serde_json::to_string(&*tasks)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.storage_file)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_task_recovery() {
        let storage_file = "test_tasks.json";
        let recovery_manager = TaskRecoveryManager::new(storage_file);

        let task = Task {
            task_id: Uuid::new_v4(),
            data: "Test data".to_string(),
        };

        recovery_manager.add_task(task.clone()).await;
        recovery_manager.remove_task(&task.task_id).await;

        // Recover tasks from file
        recovery_manager.add_task(task.clone()).await;
        recovery_manager.persist_tasks().await.unwrap();
        let new_recovery_manager = TaskRecoveryManager::new(storage_file);
        new_recovery_manager.recover_tasks().await.unwrap();
        assert!(new_recovery_manager
            .tasks
            .read()
            .await
            .contains_key(&task.task_id));

        // Clean up test file
        fs::remove_file(storage_file).unwrap();
    }

    #[tokio::test]
    async fn test_recover_tasks_file_absent() {
        let storage_file = "missing_tasks.json";
        if fs::metadata(storage_file).is_ok() {
            fs::remove_file(storage_file).unwrap();
        }
        let recovery_manager = TaskRecoveryManager::new(storage_file);
        assert!(recovery_manager.recover_tasks().await.is_ok());
        assert!(fs::metadata(storage_file).is_ok());
        assert!(recovery_manager.tasks.read().await.is_empty());
        fs::remove_file(storage_file).unwrap();
    }
}
