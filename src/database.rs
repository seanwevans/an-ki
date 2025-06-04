use tokio_postgres::{Client, NoTls, Row, Error};
use tokio_postgres::types::ToSql;
use tokio::runtime::Runtime;

pub struct Database {
    client: Client,
    rt: Runtime,
}

impl Database {
    pub fn connect(conn_str: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = Runtime::new()?;
        let (client, connection) = rt.block_on(tokio_postgres::connect(conn_str, NoTls))?;
        rt.spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });
        Ok(Database { client, rt })
    }

    pub fn execute(&self, query: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error> {
        self.rt.block_on(self.client.execute(query, params))
    }

    pub fn query(&self, query: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error> {
        self.rt.block_on(self.client.query(query, params))
    }
}
