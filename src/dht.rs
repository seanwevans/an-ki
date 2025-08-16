// dht.rs: Implements a distributed hash table (DHT) for node discovery and coordination.

use crate::common::NodeInfo;
use crate::database::Database;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct DHT {
    nodes: Arc<RwLock<HashMap<Uuid, NodeInfo>>>,
}

impl DHT {
    pub fn new() -> Self {
        DHT {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_node(&self, node_info: NodeInfo) {
        let mut nodes = self.nodes.write().await;
        nodes.insert(node_info.id, node_info.clone());
        info!("Added node to DHT: {:?}", node_info);
    }

    pub async fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if nodes.remove(node_id).is_some() {
            info!("Removed node from DHT: {:?}", node_id);
        } else {
            error!(
                "Failed to remove node from DHT: Node not found: {:?}",
                node_id
            );
        }
    }

    pub async fn get_node(&self, node_id: &Uuid) -> Option<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes.get(node_id).cloned()
    }

    pub async fn list_nodes(&self) -> Vec<NodeInfo> {
        let nodes = self.nodes.read().await;
        nodes.values().cloned().collect()
    }
}

// Store DHT in the database
pub async fn store_dht_in_db(dht: &DHT, db: &Database) -> Result<(), Box<dyn std::error::Error>> {
    let nodes = dht.list_nodes().await;
    for node in nodes {
        let node_id_str = node.id.to_string();
        let serialized_node = serde_json::to_string(&node)?;
        db.execute(
            "INSERT INTO dht (node_id, node_info) VALUES ($1, $2) ON CONFLICT (node_id) DO UPDATE SET node_info = $2",
            &[&node_id_str, &serialized_node],
        )
        .await
        .map_err(|e| {
            error!("Failed to store node in database: {:?}", e);
            e
        })?;
    }
    Ok(())
}

pub async fn load_dht_from_db(db: &Database) -> Result<DHT, Box<dyn std::error::Error>> {
    let dht = DHT::new();
    let rows = db
        .query("SELECT node_id, node_info FROM dht", &[])
        .await
        .map_err(|e| {
            error!("Failed to load DHT from database: {:?}", e);
            e
        })?;
    for row in rows {
        let _node_id: String = row.get(0);
        let node_info_str: String = row.get(1);
        let node_info: NodeInfo = serde_json::from_str(&node_info_str)?;
        dht.add_node(node_info).await;
    }
    Ok(dht)
}
