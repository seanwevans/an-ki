// api.rs: Implements REST API endpoints for interacting with the task recovery system.

use crate::common::{Task, TaskType};
use crate::task_recovery::TaskRecoveryManager;
use std::sync::Arc;
use tracing::error;
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
    match task_manager.get_task(&task_id).await {
        Ok(Some(task)) => match serde_json::to_string(&task) {
            Ok(body) => Ok(warp::reply::with_status(body, StatusCode::OK)),
            Err(e) => {
                error!("Failed to serialize task: {}", e);
                Ok(warp::reply::with_status(
                    "Internal server error".to_string(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ))
            }
        },
        Ok(None) => Ok(warp::reply::with_status(
            "Task not found".to_string(),
            StatusCode::NOT_FOUND,
        )),
        Err(e) => {
            error!("Failed to get task: {:?}", e);
            Ok(warp::reply::with_status(
                "Internal server error".to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

async fn add_task_handler(
    task_manager: Arc<TaskRecoveryManager>,
    new_task: Task,
) -> Result<impl warp::Reply, warp::Rejection> {
    match task_manager.add_task(new_task).await {
        Ok(_) => Ok(warp::reply::with_status("Task added", StatusCode::CREATED)),
        Err(e) => {
            error!("Failed to add task: {:?}", e);
            Ok(warp::reply::with_status(
                "Internal server error",
                StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

async fn delete_task_handler(
    task_id: Uuid,
    task_manager: Arc<TaskRecoveryManager>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match task_manager.remove_task(&task_id).await {
        Ok(true) => Ok(warp::reply::with_status("Task deleted", StatusCode::OK)),
        Ok(false) => Ok(warp::reply::with_status(
            "Task not found",
            StatusCode::NOT_FOUND,
        )),
        Err(e) => {
            error!("Failed to delete task: {:?}", e);
            Ok(warp::reply::with_status(
                "Internal server error",
                StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
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
        assert!(task_manager.remove_task(&new_task.task_id).await.unwrap());
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
        task_manager.add_task(task.clone()).await.unwrap();

        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(task_manager.remove_task(&task.task_id).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_task_from_database_on_cache_miss() {
        let pool = get_pool().await.expect("pool");
        let task_manager = Arc::new(TaskRecoveryManager::new(pool.clone()));
        let api = Api::new(task_manager.clone());

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

        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(task_manager.tasks.read().await.contains_key(&task.task_id));

        pool.get()
            .await
            .unwrap()
            .execute("DELETE FROM tasks WHERE task_id = $1", &[&task.task_id])
            .await
            .unwrap();
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
        task_manager.add_task(task.clone()).await.unwrap();

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks/{}", task.task_id))
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
