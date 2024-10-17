// node_registry.rs: Implements a node registry for keeping track of active nodes in the distributed neural network system.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use chrono::{Utc, DateTime};
use tracing::{info, error};
use std::error::Error;

#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub node_id: Uuid,
    pub last_seen: DateTime<Utc>,
    pub role: String,
}

#[derive(Clone)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<Uuid, NodeInfo>>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        NodeRegistry {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_node(&self, node_id: Uuid, role: String) {
        let node_info = NodeInfo {
            node_id,
            last_seen: Utc::now(),
            role,
        };
        self.nodes.write().unwrap().insert(node_id, node_info.clone());
        info!("Registered new node: {:?}", node_info);
    }

    pub fn update_last_seen(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().unwrap();
        if let Some(node_info) = nodes.get_mut(node_id) {
            node_info.last_seen = Utc::now();
            info!("Updated last seen for node: {}", node_id);
        } else {
            error!("Attempted to update non-existent node: {}", node_id);
        }
    }

    pub fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().unwrap();
        if nodes.remove(node_id).is_some() {
            info!("Removed node from registry: {}", node_id);
        } else {
            error!("Attempted to remove non-existent node: {}", node_id);
        }
    }

    pub fn list_active_nodes(&self) -> Vec<NodeInfo> {
        let nodes = self.nodes.read().unwrap();
        nodes.values().cloned().collect()
    }

    pub fn get_node_info(&self, node_id: &Uuid) -> Option<NodeInfo> {
        let nodes = self.nodes.read().unwrap();
        nodes.get(node_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_update_node() {
        let registry = NodeRegistry::new();
        let node_id = Uuid::new_v4();
        let role = "teacher".to_string();

        registry.register_node(node_id, role.clone());
        assert!(registry.get_node_info(&node_id).is_some());

        registry.update_last_seen(&node_id);
        let node_info = registry.get_node_info(&node_id).unwrap();
        assert_eq!(node_info.role, role);
    }

    #[test]
    fn test_remove_node() {
        let registry = NodeRegistry::new();
        let node_id = Uuid::new_v4();
        let role = "ki".to_string();

        registry.register_node(node_id, role);
        assert!(registry.get_node_info(&node_id).is_some());

        registry.remove_node(&node_id);
        assert!(registry.get_node_info(&node_id).is_none());
    }

    #[test]
    fn test_list_active_nodes() {
        let registry = NodeRegistry::new();
        let node_id_1 = Uuid::new_v4();
        let node_id_2 = Uuid::new_v4();

        registry.register_node(node_id_1, "teacher".to_string());
        registry.register_node(node_id_2, "ki".to_string());

        let active_nodes = registry.list_active_nodes();
        assert_eq!(active_nodes.len(), 2);
    }
}
