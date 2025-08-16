#![allow(dead_code)]

use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::types::ToSql;
use tokio_postgres::{NoTls, Row};
use tracing::error;

pub struct Database {
    pool: Pool<PostgresConnectionManager<NoTls>>,
}

impl Database {
    pub async fn connect(conn_str: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config = conn_str.parse().map_err(|e| {
            error!("Failed to parse database connection string: {:?}", e);
            e
        })?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = Pool::builder().build(manager).await.map_err(|e| {
            error!("Failed to build connection pool: {:?}", e);
            e
        })?;
        Ok(Database { pool })
    }

    pub async fn execute(
        &self,
        query: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let conn = self.pool.get().await.map_err(|e| {
            error!("Failed to get database connection: {:?}", e);
            Box::<dyn std::error::Error>::from(e)
        })?;
        let result = conn.execute(query, params).await.map_err(|e| {
            error!("Failed to execute query: {:?}", e);
            Box::<dyn std::error::Error>::from(e)
        })?;
        Ok(result)
    }

    pub async fn query(
        &self,
        query: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, Box<dyn std::error::Error>> {
        let conn = self.pool.get().await.map_err(|e| {
            error!("Failed to get database connection: {:?}", e);
            Box::<dyn std::error::Error>::from(e)
        })?;
        let rows = conn.query(query, params).await.map_err(|e| {
            error!("Failed to execute query: {:?}", e);
            Box::<dyn std::error::Error>::from(e)
        })?;
        Ok(rows)
    }
}
