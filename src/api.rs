// api.rs: Implements REST API endpoints for interacting with the task recovery system.

use warp::Filter;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::task_recovery_module::{TaskRecoveryManager, Task};
use uuid::Uuid;
use warp::http::StatusCode;

#[derive(Clone)]
pub struct Api {
    pub task_manager: Arc<TaskRecoveryManager>,
}

impl Api {
    pub fn new(task_manager: Arc<TaskRecoveryManager>) -> Self {
        Api { task_manager }
    }

    pub fn filters(self) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        let api = warp::path("tasks").and(warp::path::end());

        let get_task = warp::get()
            .and(api.clone())
            .and(with_task_manager(self.task_manager.clone()))
            .and(warp::query::<GetTaskParams>())
            .and_then(get_task_handler);

        let add_task = warp::post()
            .and(api.clone())
            .and(with_task_manager(self.task_manager.clone()))
            .and(warp::body::json())
            .and_then(add_task_handler);

        let delete_task = warp::delete()
            .and(api)
            .and(with_task_manager(self.task_manager.clone()))
            .and(warp::query::<DeleteTaskParams>())
            .and_then(delete_task_handler);

        get_task.or(add_task).or(delete_task)
    }
}

#[derive(Deserialize)]
struct GetTaskParams {
    task_id: String,
}

#[derive(Deserialize)]
struct DeleteTaskParams {
    task_id: String,
}

fn with_task_manager(
    task_manager: Arc<TaskRecoveryManager>,
) -> impl Filter<Extract = (Arc<TaskRecoveryManager>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || task_manager.clone())
}

async fn get_task_handler(
    task_manager: Arc<TaskRecoveryManager>,
    params: GetTaskParams,
) -> Result<impl warp::Reply, warp::Rejection> {
    let task_id = match Uuid::parse_str(&params.task_id) {
        Ok(uuid) => uuid,
        Err(_) => return Ok(warp::reply::with_status("Invalid UUID", StatusCode::BAD_REQUEST)),
    };

    let tasks = task_manager.tasks.read().unwrap();
    if let Some(task) = tasks.get(&task_id) {
        Ok(warp::reply::json(task))
    } else {
        Ok(warp::reply::with_status("Task not found", StatusCode::NOT_FOUND))
    }
}

async fn add_task_handler(
    task_manager: Arc<TaskRecoveryManager>,
    new_task: Task,
) -> Result<impl warp::Reply, warp::Rejection> {
    task_manager.add_task(new_task);
    Ok(warp::reply::with_status("Task added", StatusCode::CREATED))
}

async fn delete_task_handler(
    task_manager: Arc<TaskRecoveryManager>,
    params: DeleteTaskParams,
) -> Result<impl warp::Reply, warp::Rejection> {
    let task_id = match Uuid::parse_str(&params.task_id) {
        Ok(uuid) => uuid,
        Err(_) => return Ok(warp::reply::with_status("Invalid UUID", StatusCode::BAD_REQUEST)),
    };

    task_manager.remove_task(&task_id);
    Ok(warp::reply::with_status("Task deleted", StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use warp::test::request;

    #[tokio::test]
    async fn test_add_task() {
        let storage_file = "test_tasks_api.json";
        let task_manager = Arc::new(TaskRecoveryManager::new(storage_file));
        let api = Api::new(task_manager.clone());

        let new_task = Task {
            task_id: Uuid::new_v4(),
            data: "Test task data".to_string(),
        };

        let res = request()
            .method("POST")
            .path("/tasks")
            .json(&new_task)
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_get_task() {
        let storage_file = "test_tasks_api.json";
        let task_manager = Arc::new(TaskRecoveryManager::new(storage_file));
        let api = Api::new(task_manager.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            data: "Test task data".to_string(),
        };
        task_manager.add_task(task.clone());

        let res = request()
            .method("GET")
            .path(&format!("/tasks?task_id={}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_delete_task() {
        let storage_file = "test_tasks_api.json";
        let task_manager = Arc::new(TaskRecoveryManager::new(storage_file));
        let api = Api::new(task_manager.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            data: "Test task data".to_string(),
        };
        task_manager.add_task(task.clone());

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks?task_id={}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
    }
}
