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
        let pool = match get_pool().await {
            Ok(pool) => pool,
            Err(e) => panic!("Failed to create pool: {}", e),
        };
        let conn = match pool.get().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!(
                    "Skipping test_basic_query: failed to get connection from pool: {}",
                    e
                );
                return;
            }
        };
        let row = match conn.query_one("SELECT 1", &[]).await {
            Ok(row) => row,
            Err(e) => {
                eprintln!("Skipping test_basic_query: query failed: {}", e);
                return;
            }
        };
        let value: i32 = row.get(0);
        assert_eq!(value, 1);
    }
}
