// backup.rs: Implements a backup mechanism for task persistence to enhance robustness and redundancy.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use tracing::{info, error};
use uuid::Uuid;
use chrono::Utc;
use std::error::Error;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub task_id: Uuid,
    pub data: String,
}

#[derive(Clone)]
pub struct BackupManager {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub backup_dir: String,
}

impl BackupManager {
    pub fn new(backup_dir: &str) -> Self {
        if !Path::new(backup_dir).exists() {
            fs::create_dir_all(backup_dir).expect("Failed to create backup directory");
        }

        BackupManager {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            backup_dir: backup_dir.to_string(),
        }
    }

    pub fn create_backup(&self) -> Result<(), Box<dyn Error>> {
        let timestamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
        let backup_file_path = format!("{}/backup_{}.json", self.backup_dir, timestamp);

        let tasks = self.tasks.read().unwrap();
        let content = serde_json::to_string(&*tasks)?;
        let mut file = OpenOptions::new().write(true).create(true).open(&backup_file_path)?;
        file.write_all(content.as_bytes())?;

        info!("Created backup at: {}", backup_file_path);
        Ok(())
    }

    pub fn restore_backup(&self, backup_file: &str) -> Result<(), Box<dyn Error>> {
        let content = fs::read_to_string(backup_file)?;
        let recovered_tasks: HashMap<Uuid, Task> = serde_json::from_str(&content)?;

        let mut tasks = self.tasks.write().unwrap();
        *tasks = recovered_tasks;
        info!("Restored tasks from backup file: {}", backup_file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_create_and_restore_backup() {
        let backup_dir = "test_backups";
        let backup_manager = BackupManager::new(backup_dir);

        let task = Task {
            task_id: Uuid::new_v4(),
            data: "Backup test task data".to_string(),
        };
        backup_manager.tasks.write().unwrap().insert(task.task_id, task.clone());

        // Create a backup
        backup_manager.create_backup().unwrap();

        // Find the backup file that was just created
        let backup_files: Vec<_> = fs::read_dir(backup_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert!(!backup_files.is_empty(), "Backup file was not created");
        let backup_file_path = backup_files[0].path().to_str().unwrap().to_string();

        // Clear the current tasks and restore from backup
        backup_manager.tasks.write().unwrap().clear();
        backup_manager.restore_backup(&backup_file_path).unwrap();
        assert!(backup_manager.tasks.read().unwrap().contains_key(&task.task_id));

        // Clean up backup files
        fs::remove_dir_all(backup_dir).unwrap();
    }
}
