// api.rs: Implements REST API endpoints for interacting with the task recovery system.

use crate::common::{NodeRole, Task};
use crate::database::get_pool;
use crate::security::{self, Claims};
use crate::task_recovery::TaskRecoveryManager;
use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};
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
    ) -> impl Filter<Extract = impl warp::Reply, Error = std::convert::Infallible> + Clone {
        let add_task = warp::post()
            .and(warp::path("tasks"))
            .and(warp::path::end())
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and(with_auth())
            .and(warp::body::json())
            .and_then(add_task_handler);

        let get_task = warp::get()
            .and(warp::path!("tasks" / Uuid))
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and(with_auth())
            .and_then(get_task_handler);

        let delete_task = warp::delete()
            .and(warp::path!("tasks" / Uuid))
            .and(with_task_manager(Arc::clone(&self.task_manager)))
            .and(with_auth())
            .and_then(delete_task_handler);

        add_task
            .or(get_task)
            .or(delete_task)
            .recover(handle_rejection)
    }
}

/// Marker rejection for requests that fail authentication.
#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

/// Roles permitted to create or delete tasks. Ki nodes are execution-only and
/// may read tasks but not mutate the task set.
fn can_mutate_tasks(role: &NodeRole) -> bool {
    matches!(role, NodeRole::Principal | NodeRole::An)
}

/// Extracts and verifies the `Authorization: Bearer <jwt>` header, yielding the
/// token [`Claims`] or rejecting with [`Unauthorized`].
fn with_auth() -> impl Filter<Extract = (Claims,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(
        |header: Option<String>| async move {
            let token = header
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or_else(|| warp::reject::custom(Unauthorized))?;
            security::verify_token(token)
                .map(|data| data.claims)
                .map_err(|_| warp::reject::custom(Unauthorized))
        },
    )
}

/// Converts rejections into responses, mapping authentication failures to 401
/// while preserving sensible status codes for other rejection kinds.
async fn handle_rejection(
    err: warp::Rejection,
) -> Result<impl warp::Reply, std::convert::Infallible> {
    let (message, status) = if err.is_not_found() {
        ("Not Found".to_string(), StatusCode::NOT_FOUND)
    } else if err.find::<Unauthorized>().is_some() {
        ("Unauthorized".to_string(), StatusCode::UNAUTHORIZED)
    } else if let Some(e) = err.find::<warp::filters::body::BodyDeserializeError>() {
        (
            format!("Invalid request body: {}", e),
            StatusCode::BAD_REQUEST,
        )
    } else if err.find::<warp::reject::MethodNotAllowed>().is_some() {
        (
            "Method Not Allowed".to_string(),
            StatusCode::METHOD_NOT_ALLOWED,
        )
    } else {
        error!("Unhandled rejection: {:?}", err);
        (
            "Internal Server Error".to_string(),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    };
    Ok(warp::reply::with_status(message, status))
}

fn with_task_manager(
    task_manager: Arc<TaskRecoveryManager>,
) -> impl Filter<Extract = (Arc<TaskRecoveryManager>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || task_manager.clone())
}

async fn get_task_handler(
    task_id: Uuid,
    task_manager: Arc<TaskRecoveryManager>,
    _claims: Claims,
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
    claims: Claims,
    new_task: Task,
) -> Result<impl warp::Reply, warp::Rejection> {
    if !can_mutate_tasks(&claims.role) {
        return Ok(warp::reply::with_status("Forbidden", StatusCode::FORBIDDEN));
    }
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
    claims: Claims,
) -> Result<impl warp::Reply, warp::Rejection> {
    if !can_mutate_tasks(&claims.role) {
        return Ok(warp::reply::with_status("Forbidden", StatusCode::FORBIDDEN));
    }
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

/// Serves the task REST API on `addr`, creating a fresh database-backed
/// [`TaskRecoveryManager`]. The server runs until `shutdown` resolves, then
/// drains in-flight requests and returns.
///
/// # Errors
/// Returns an error if the database connection pool cannot be established.
pub async fn serve<F>(addr: SocketAddr, shutdown: F) -> Result<(), Box<dyn Error + Send + Sync>>
where
    F: Future<Output = ()> + Send + 'static,
{
    let pool = get_pool().await?;
    let task_manager = Arc::new(TaskRecoveryManager::new(pool));
    serve_with_manager(task_manager, addr, shutdown).await;
    Ok(())
}

/// Serves the task REST API using a caller-supplied [`TaskRecoveryManager`],
/// shutting down gracefully when `shutdown` resolves. Kept separate from
/// [`serve`] so tests can supply their own manager.
pub async fn serve_with_manager<F>(
    task_manager: Arc<TaskRecoveryManager>,
    addr: SocketAddr,
    shutdown: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let api = Api::new(task_manager);
    let (bound_addr, server) =
        warp::serve(api.filters()).bind_with_graceful_shutdown(addr, shutdown);
    info!("Task API listening on {}", bound_addr);
    server.await;
}

// Lifecycle tests for the server itself. These use a pool built with
// `build_unchecked` (which never opens a connection) so they exercise binding
// and graceful shutdown without a database, and therefore run by default.
#[cfg(test)]
mod serve_tests {
    use super::*;
    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio_postgres::{Config, NoTls};

    fn offline_manager() -> Arc<TaskRecoveryManager> {
        let cfg = Config::from_str("postgresql://user@localhost/db").unwrap();
        let manager = PostgresConnectionManager::new(cfg, NoTls);
        // `build_unchecked` constructs the pool without contacting the database.
        let pool = Pool::builder().build_unchecked(manager);
        Arc::new(TaskRecoveryManager::new(pool))
    }

    #[tokio::test]
    async fn serve_binds_and_shuts_down_gracefully() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = tokio::spawn(serve_with_manager(offline_manager(), addr, async move {
            let _ = rx.await;
        }));

        // Let the server reach its serving state, then ask it to stop.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(()).expect("shutdown receiver should be alive");

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server did not shut down within the deadline")
            .expect("server task panicked");
    }
}

// The authentication gate runs before any database access, so these tests use
// an offline (`build_unchecked`) pool and run by default. They avoid generating
// real tokens (which would read the shared JWT secret) so they never race with
// the env-mutating tests in `security`.
#[cfg(test)]
mod auth_tests {
    use super::*;
    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use std::str::FromStr;
    use tokio_postgres::{Config, NoTls};
    use warp::test::request;

    fn offline_api() -> Api {
        let cfg = Config::from_str("postgresql://user@localhost/db").unwrap();
        let manager = PostgresConnectionManager::new(cfg, NoTls);
        let pool = Pool::builder().build_unchecked(manager);
        Api::new(Arc::new(TaskRecoveryManager::new(pool)))
    }

    #[tokio::test]
    async fn request_without_token_is_unauthorized() {
        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", Uuid::new_v4()))
            .reply(&offline_api().filters())
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn request_with_invalid_token_is_unauthorized() {
        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", Uuid::new_v4()))
            .header("authorization", "Bearer not-a-jwt")
            .reply(&offline_api().filters())
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_authorization_scheme_is_unauthorized() {
        let res = request()
            .method("GET")
            .path(&format!("/tasks/{}", Uuid::new_v4()))
            .header("authorization", "Basic abc123")
            .reply(&offline_api().filters())
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn only_principal_and_an_may_mutate_tasks() {
        assert!(can_mutate_tasks(&NodeRole::Principal));
        assert!(can_mutate_tasks(&NodeRole::An));
        assert!(!can_mutate_tasks(&NodeRole::Ki));
    }
}

// Every test here drives the API through a real `TaskRecoveryManager` backed by
// a live database connection, so they are gated behind the `integration-tests`
// feature and excluded from the default `cargo test` run.
#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;
    use crate::common::TaskType;
    use crate::database::get_pool;
    use warp::test::request;

    fn auth_header() -> String {
        let token = security::generate_token("test-node", NodeRole::An, 5).expect("token");
        format!("Bearer {}", token)
    }

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
            .header("authorization", auth_header())
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
            .header("authorization", auth_header())
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
            .header("authorization", auth_header())
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
        let task_manager = Arc::new(TaskRecoveryManager::new(pool.clone()));
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
            .header("authorization", auth_header())
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks/{}", task.task_id))
            .header("authorization", auth_header())
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let database_only_task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: "Database-only task data".to_string(),
        };
        let task_type = format!("{:?}", database_only_task.task_type);
        pool.get()
            .await
            .unwrap()
            .execute(
                "INSERT INTO tasks (task_id, task_type, data) VALUES ($1, $2, $3)",
                &[
                    &database_only_task.task_id,
                    &task_type,
                    &database_only_task.data,
                ],
            )
            .await
            .unwrap();
        assert!(!task_manager
            .tasks
            .read()
            .await
            .contains_key(&database_only_task.task_id));

        let res = request()
            .method("DELETE")
            .path(&format!("/tasks/{}", database_only_task.task_id))
            .header("authorization", auth_header())
            .reply(&api.filters())
            .await;
        assert_eq!(res.status(), StatusCode::OK);

        let remaining_rows = pool
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM tasks WHERE task_id = $1",
                &[&database_only_task.task_id],
            )
            .await
            .unwrap();
        let remaining_count: i64 = remaining_rows.get(0);
        assert_eq!(remaining_count, 0);
    }
}
