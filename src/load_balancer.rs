// load_balancer.rs: Implements load balancing for An nodes to effectively distribute tasks.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use tokio::sync::broadcast;
use tracing::{info, error};
use rand::seq::IteratorRandom;

#[derive(Clone, Debug)]
pub struct NodeLoadInfo {
    pub node_id: Uuid,
    pub task_count: usize,
}

#[derive(Clone)]
pub struct LoadBalancer {
    pub nodes: Arc<RwLock<HashMap<Uuid, NodeLoadInfo>>>,
}

impl LoadBalancer {
    pub fn new() -> Self {
        LoadBalancer {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn add_node(&self, node_id: Uuid) {
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(node_id, NodeLoadInfo { node_id, task_count: 0 });
        info!("Added node to load balancer: {}", node_id);
    }

    pub fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().unwrap();
        if nodes.remove(node_id).is_some() {
            info!("Removed node from load balancer: {}", node_id);
        } else {
            error!("Failed to remove node from load balancer: Node not found: {}", node_id);
        }
    }

    pub fn assign_task(&self) -> Option<Uuid> {
        let mut nodes = self.nodes.write().unwrap();
        if nodes.is_empty() {
            error!("No nodes available to assign task.");
            return None;
        }

        // Find the node with the least tasks
        if let Some((node_id, node_info)) = nodes.values_mut().min_by_key(|n| n.task_count) {
            node_info.task_count += 1;
            info!("Assigned task to node: {}. Task count: {}", node_id, node_info.task_count);
            Some(*node_id)
        } else {
            None
        }
    }

    pub fn complete_task(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().unwrap();
        if let Some(node_info) = nodes.get_mut(node_id) {
            if node_info.task_count > 0 {
                node_info.task_count -= 1;
                info!("Completed task on node: {}. Remaining task count: {}", node_id, node_info.task_count);
            }
        } else {
            error!("Failed to complete task: Node not found: {}", node_id);
        }
    }

    pub fn random_node(&self) -> Option<Uuid> {
        let nodes = self.nodes.read().unwrap();
        nodes.keys().cloned().choose(&mut rand::thread_rng())
    }
}

pub async fn monitor_node_load(mut rx: broadcast::Receiver<NodeLoadInfo>, load_balancer: LoadBalancer) {
    while let Ok(node_load) = rx.recv().await {
        let mut nodes = load_balancer.nodes.write().unwrap();
        if let Some(node_info) = nodes.get_mut(&node_load.node_id) {
            node_info.task_count = node_load.task_count;
            info!("Updated load info for node: {}. Task count: {}", node_load.node_id, node_load.task_count);
        } else {
            error!("Node not found in load balancer for update: {}", node_load.node_id);
        }
    }
}
