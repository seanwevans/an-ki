// testing.rs: Implements integration tests for the distributed neural network system.

use crate::task_recovery_module::{TaskRecoveryManager, Task};
use crate::api_module::Api;
use std::sync::Arc;
use uuid::Uuid;
use warp::http::StatusCode;
use warp::test::request;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_task_recovery_persistence() {
    let storage_file = "test_tasks_persistence.json";
    let recovery_manager = Arc::new(TaskRecoveryManager::new(storage_file));

    let task = Task {
        task_id: Uuid::new_v4(),
        data: "Persistent task data".to_string(),
    };

    // Add task and persist to file
    recovery_manager.add_task(task.clone());
    recovery_manager.persist_tasks().unwrap();

    // Create a new recovery manager to test loading from the persisted file
    let new_recovery_manager = TaskRecoveryManager::new(storage_file);
    new_recovery_manager.recover_tasks().unwrap();
    assert!(new_recovery_manager.tasks.read().unwrap().contains_key(&task.task_id));

    // Clean up test file
    std::fs::remove_file(storage_file).unwrap();
}

#[tokio::test]
async fn test_task_api_add_and_get() {
    let storage_file = "test_tasks_api_integration.json";
    let task_manager = Arc::new(TaskRecoveryManager::new(storage_file));
    let api = Api::new(task_manager.clone());

    let new_task = Task {
        task_id: Uuid::new_v4(),
        data: "Integration test task data".to_string(),
    };

    // Test adding a task
    let res = request()
        .method("POST")
        .path("/tasks")
        .json(&new_task)
        .reply(&api.filters())
        .await;
    assert_eq!(res.status(), StatusCode::CREATED);

    // Test retrieving the added task
    let res = request()
        .method("GET")
        .path(&format!("/tasks?task_id={}", new_task.task_id))
        .reply(&api.filters())
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Clean up test file
    std::fs::remove_file(storage_file).unwrap();
}

#[tokio::test]
async fn test_task_api_delete() {
    let storage_file = "test_tasks_api_delete.json";
    let task_manager = Arc::new(TaskRecoveryManager::new(storage_file));
    let api = Api::new(task_manager.clone());

    let task = Task {
        task_id: Uuid::new_v4(),
        data: "Delete test task data".to_string(),
    };
    task_manager.add_task(task.clone());

    // Test deleting the task
    let res = request()
        .method("DELETE")
        .path(&format!("/tasks?task_id={}", task.task_id))
        .reply(&api.filters())
        .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Verify the task is no longer available
    let res = request()
        .method("GET")
        .path(&format!("/tasks?task_id={}", task.task_id))
        .reply(&api.filters())
        .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Clean up test file
    std::fs::remove_file(storage_file).unwrap();
}

#[tokio::test]
async fn test_recovery_after_crash() {
    let storage_file = "test_recovery_after_crash.json";
    let recovery_manager = Arc::new(TaskRecoveryManager::new(storage_file));

    let task = Task {
        task_id: Uuid::new_v4(),
        data: "Recovery after crash task data".to_string(),
    };

    // Simulate adding a task and persisting before a crash
    recovery_manager.add_task(task.clone());
    recovery_manager.persist_tasks().unwrap();

    // Simulate a delay (representing downtime)
    sleep(Duration::from_secs(1)).await;

    // Recover tasks after "crash"
    let new_recovery_manager = TaskRecoveryManager::new(storage_file);
    new_recovery_manager.recover_tasks().unwrap();
    assert!(new_recovery_manager.tasks.read().unwrap().contains_key(&task.task_id));

    // Clean up test file
    std::fs::remove_file(storage_file).unwrap();
}
