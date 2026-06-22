// dht.rs: Implements a distributed hash table (DHT) for node discovery and coordination.

use crate::common::NodeInfo;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone)]
pub struct Dht {
    nodes: Arc<RwLock<HashMap<Uuid, NodeInfo>>>,
}

impl Default for Dht {
    fn default() -> Self {
        Self::new()
    }
}

impl Dht {
    pub fn new() -> Self {
        Dht {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::NodeRole;
    use chrono::Utc;

    fn sample_node(role: NodeRole) -> NodeInfo {
        NodeInfo {
            id: Uuid::new_v4(),
            address: None,
            last_seen: Some(Utc::now()),
            role,
        }
    }

    #[tokio::test]
    async fn test_add_and_get_node() {
        let dht = Dht::new();
        let node = sample_node(NodeRole::An);
        let id = node.id;
        dht.add_node(node).await;
        let retrieved = dht.get_node(&id).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, id);
    }

    #[tokio::test]
    async fn test_remove_node() {
        let dht = Dht::new();
        let node = sample_node(NodeRole::An);
        let id = node.id;
        dht.add_node(node).await;
        dht.remove_node(&id).await;
        assert!(dht.get_node(&id).await.is_none());
    }

    #[tokio::test]
    async fn test_list_nodes() {
        let dht = Dht::new();
        dht.add_node(sample_node(NodeRole::An)).await;
        dht.add_node(sample_node(NodeRole::Ki)).await;
        let nodes = dht.list_nodes().await;
        assert_eq!(nodes.len(), 2);
    }
}
