use tokio_postgres::{NoTls, Row, Error};
use tokio_postgres::types::ToSql;
use bb8::{Pool};
use bb8_postgres::PostgresConnectionManager;

pub struct Database {
    pool: Pool<PostgresConnectionManager<NoTls>>,
}

impl Database {
    pub async fn connect(conn_str: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config = conn_str.parse()?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        let pool = Pool::builder().build(manager).await?;
        Ok(Database { pool })
    }

    pub async fn execute(&self, query: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Box<dyn std::error::Error>> {
        let conn = self.pool.get().await.map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
        let result = conn.execute(query, params).await?;
        Ok(result)
    }

    pub async fn query(&self, query: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Box<dyn std::error::Error>> {
        let conn = self.pool.get().await.map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
        let rows = conn.query(query, params).await?;
        Ok(rows)
    }
}
