// api.rs: Implements REST API endpoints for interacting with the task recovery system.

use crate::common::{Task, TaskType};
use crate::task_recovery::TaskRecoveryManager;
use std::sync::Arc;
use uuid::Uuid;
use warp::http::StatusCode;
use warp::Filter;

#[derive(Clone)]
pub struct Api {
    pub task_manager: Arc<TaskRecoveryManager>,
}

impl Api {
    pub fn new(task_manager: Arc<TaskRecoveryManager>) -> Self {
        Api { task_manager }
    }

    pub fn filters(
        &self,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        let add_task = warp::post()
            .and(warp::path("tasks"))
            .and(warp::path::end())
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and(warp::body::json())
            .and_then(add_task_handler);

        let get_task = warp::get()
            .and(warp::path!("tasks" / Uuid))
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and_then(get_task_handler);

        let delete_task = warp::delete()
            .and(warp::path!("tasks" / Uuid))
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and_then(delete_task_handler);

        add_task.or(get_task).or(delete_task)
    }
}

fn with_task_manager(
    task_manager: Arc<TaskRecoveryManager>,
) -> impl Filter<Extract = (Arc<TaskRecoveryManager>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || task_manager.clone())
}

async fn get_task_handler(
    task_id: Uuid,
    task_manager: Arc<TaskRecoveryManager>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let tasks = task_manager.tasks.read().await;
    if let Some(task) = tasks.get(&task_id) {
        let body = serde_json::to_string(task).unwrap_or_default();
        Ok(warp::reply::with_status(body, StatusCode::OK))
    } else {
        Ok(warp::reply::with_status(
            "Task not found".to_string(),
            StatusCode::NOT_FOUND,
        ))
    }
}

async fn add_task_handler(
    task_manager: Arc<TaskRecoveryManager>,
    new_task: Task,
) -> Result<impl warp::Reply, warp::Rejection> {
    task_manager.add_task(new_task).await;
    Ok(warp::reply::with_status("Task added", StatusCode::CREATED))
}

async fn delete_task_handler(
    task_id: Uuid,
    task_manager: Arc<TaskRecoveryManager>,
) -> Result<impl warp::Reply, warp::Rejection> {
    task_manager.remove_task(&task_id).await;
    Ok(warp::reply::with_status("Task deleted", StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::get_pool;
    use warp::test::request;

    #[tokio::test]
    async fn test_add_task() {
        let pool = get_pool().await.expect("pool");
        let task_manager = Arc::new(TaskRecoveryManager::new(pool.clone()));
        let api = Api::new(task_manager.clone());

        let new_task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Test task data".to_string(),
        };

        let res = request()
            .method("POST")
            .path("/tasks")
            .json(&new_task)
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        task_manager.remove_task(&new_task.task_id).await;
    }

    #[tokio::test]
    async fn test_get_task() {
        let pool = get_pool().await.expect("pool");
        let task_manager = Arc::new(TaskRecoveryManager::new(pool));
        let api = Api::new(task_manager.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Test task data".to_string(),
        };
        task_manager.add_task(task.clone()).await;

        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        task_manager.remove_task(&task.task_id).await;
    }

    #[tokio::test]
    async fn test_delete_task() {
        let pool = get_pool().await.expect("pool");
        let task_manager = Arc::new(TaskRecoveryManager::new(pool));
        let api = Api::new(task_manager.clone());

        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::ParameterPull,
            data: "Test task data".to_string(),
        };
        task_manager.add_task(task.clone()).await;

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
    }
}
