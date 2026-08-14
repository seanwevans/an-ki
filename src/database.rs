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

    // Reshaped to fit training checkpoints rather than REST API tasks. This one
    // opens with `DROP TABLE IF EXISTS model_checkpoints`, so it must run once
    // and never again: `get_pool` runs the migrations on every call, and every
    // An node calls it at startup. Applying the drop unconditionally deleted
    // every saved checkpoint each time a node came back up — precisely the
    // state checkpointing exists to preserve.
    if has_legacy_checkpoint_shape(&conn).await? {
        conn.batch_execute(include_str!(
            "../migrations/003_reshape_model_checkpoints.sql"
        ))
        .await?;
    }

    Ok(())
}

/// Reports whether `model_checkpoints` still has the pre-003 shape.
///
/// The `task_id` column is the distinguishing feature: it exists only in the
/// original table, which referenced the REST API's `tasks` rows. Its absence
/// means 003 has already been applied and re-running it would destroy data.
async fn has_legacy_checkpoint_shape(
    conn: &bb8::PooledConnection<'_, PostgresConnectionManager<NoTls>>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let row = conn
        .query_opt(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name = 'model_checkpoints' \
               AND column_name = 'task_id'",
            &[],
        )
        .await?;
    Ok(row.is_some())
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

    #[tokio::test]
    async fn a_restart_does_not_drop_saved_checkpoints() {
        // Every An node calls `get_pool` at startup, which runs the migrations.
        // Migration 003 opens with a DROP, so applying it unconditionally wiped
        // every checkpoint on each restart — the one moment checkpoints are
        // supposed to matter. A second `get_pool` stands in for that restart.
        let pool = get_pool().await.expect("connect to the database");
        let conn = pool.get().await.expect("connection");

        let checkpoint_id = uuid::Uuid::new_v4();
        let model_id = format!("restart-{}", uuid::Uuid::new_v4());
        conn.execute(
            "INSERT INTO model_checkpoints (checkpoint_id, model_id, epoch, parameters, loss) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &checkpoint_id,
                &model_id,
                &7_i64,
                &b"parameters".to_vec(),
                &Some(0.5_f32),
            ],
        )
        .await
        .expect("insert a checkpoint");

        let _restarted = get_pool().await.expect("reconnect as a restarted node");

        let survivor = conn
            .query_opt(
                "SELECT epoch FROM model_checkpoints WHERE checkpoint_id = $1",
                &[&checkpoint_id],
            )
            .await
            .expect("read the checkpoint back");
        let epoch: i64 = survivor
            .expect("the checkpoint is still there after a restart")
            .get(0);
        assert_eq!(epoch, 7);

        conn.execute(
            "DELETE FROM model_checkpoints WHERE checkpoint_id = $1",
            &[&checkpoint_id],
        )
        .await
        .expect("clean up");
    }
}
