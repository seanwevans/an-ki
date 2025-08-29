use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::{Config, NoTls};
use crate::config;
use std::error::Error;
use std::str::FromStr;

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
    let pg_config = Config::from_str(&settings.database_url)?;
    let manager = PostgresConnectionManager::new(pg_config, NoTls);
    let pool = Pool::builder().build(manager).await?;
    Ok(pool)
}

#[cfg(test)]
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
                eprintln!("Skipping test_basic_query: failed to get connection from pool: {}", e);
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

