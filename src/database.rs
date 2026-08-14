use crate::config;
use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use std::error::Error;
use std::str::FromStr;
use tokio_postgres::{Config, NoTls};

/// Run database migrations to ensure required tables exist and configure
/// basic replication settings. These statements are idempotent so calling
/// the function multiple times is safe.
async fn run_migrations(pool: &PgPool) -> Result<(), Box<dyn Error + Send + Sync>> {
    let conn = pool.get().await?;

    // Task metadata table.
    conn.batch_execute(include_str!("../migrations/001_create_tasks.sql"))
        .await?;

    // Model checkpoint storage table.
    conn.batch_execute(include_str!(
        "../migrations/002_create_model_checkpoints.sql"
    ))
    .await?;

    // Reshaped to fit training checkpoints rather than REST API tasks.
    conn.batch_execute(include_str!(
        "../migrations/003_reshape_model_checkpoints.sql"
    ))
    .await?;

    Ok(())
}

/// Type alias for a Postgres connection pool.
pub type PgPool = Pool<PostgresConnectionManager<NoTls>>;

/// Create a connection pool to the PostgreSQL database using settings
/// loaded from the configuration.
///
/// # Errors
/// Returns any error encountered while loading configuration, parsing the
/// connection string, or building the pool.
pub async fn get_pool() -> Result<PgPool, Box<dyn Error + Send + Sync>> {
    let settings = config::load_settings()?;
    // The connection string may contain multiple hosts for multi-region clusters.
    // `tokio_postgres` will automatically balance between them.
    let pg_config = Config::from_str(&settings.database_url)?;
    let manager = PostgresConnectionManager::new(pg_config, NoTls);
    let pool = Pool::builder().build(manager).await?;

    // Ensure schema exists before returning the pool.
    run_migrations(&pool).await?;

    Ok(pool)
}

// These tests require a live PostgreSQL/CockroachDB instance reachable via the
// configured `database_url`, so they are gated behind the `integration-tests`
// feature and excluded from the default `cargo test` run.
#[cfg(all(test, feature = "integration-tests"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = get_pool().await;
        assert!(pool.is_ok(), "pool creation failed: {:?}", pool.err());
    }

    #[tokio::test]
    async fn test_basic_query() {
        // Previously this skipped itself when the database was unreachable,
        // which made an unreachable database indistinguishable from a passing
        // test. The suite is gated behind `integration-tests` precisely because
        // it requires a database, so not having one is a failure.
        let pool = get_pool().await.expect("connect to the database");
        let conn = pool.get().await.expect("take a connection from the pool");
        let row = conn.query_one("SELECT 1", &[]).await.expect("run a query");

        let value: i32 = row.get(0);
        assert_eq!(value, 1);
    }

    #[tokio::test]
    async fn migrations_create_every_table_the_code_reads() {
        // `get_pool` runs the migrations, so reaching these tables proves they
        // exist with the columns the queries name.
        let pool = get_pool().await.expect("connect to the database");
        let conn = pool.get().await.expect("connection");

        conn.query_opt("SELECT task_id, task_type, data FROM tasks LIMIT 1", &[])
            .await
            .expect("tasks table is queryable");
        conn.query_opt(
            "SELECT checkpoint_id, model_id, epoch, parameters, loss \
             FROM model_checkpoints LIMIT 1",
            &[],
        )
        .await
        .expect("model_checkpoints table is queryable");
    }
}
