// load_balancer.rs: Implements load balancing for An nodes to effectively distribute tasks.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
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

    pub async fn add_node(&self, node_id: Uuid) {
        let mut nodes = self.nodes.write().await;
        nodes.insert(node_id, NodeLoadInfo { node_id, task_count: 0 });
        info!("Added node to load balancer: {}", node_id);
    }

    pub async fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if nodes.remove(node_id).is_some() {
            info!("Removed node from load balancer: {}", node_id);
        } else {
            error!("Failed to remove node from load balancer: Node not found: {}", node_id);
        }
    }

    pub async fn assign_task(&self) -> Option<Uuid> {
        let mut nodes = self.nodes.write().await;
        if nodes.is_empty() {
            error!("No nodes available to assign task.");
            return None;
        }

        // Find the node with the least tasks
        if let Some((node_id, node_info)) = nodes.iter_mut().min_by_key(|(_, n)| n.task_count) {
            node_info.task_count += 1;
            info!(
                "Assigned task to node: {}. Task count: {}",
                node_id,
                node_info.task_count
            );
            Some(*node_id)
        } else {
            None
        }
    }

    pub async fn complete_task(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if let Some(node_info) = nodes.get_mut(node_id) {
            if node_info.task_count > 0 {
                node_info.task_count -= 1;
                info!("Completed task on node: {}. Remaining task count: {}", node_id, node_info.task_count);
            }
        } else {
            error!("Failed to complete task: Node not found: {}", node_id);
        }
    }

    pub async fn random_node(&self) -> Option<Uuid> {
        let nodes = self.nodes.read().await;
        nodes.keys().cloned().choose(&mut rand::thread_rng())
    }
}

pub async fn monitor_node_load(mut rx: broadcast::Receiver<NodeLoadInfo>, load_balancer: LoadBalancer) {
    while let Ok(node_load) = rx.recv().await {
        let mut nodes = load_balancer.nodes.write().await;
        if let Some(node_info) = nodes.get_mut(&node_load.node_id) {
            node_info.task_count = node_load.task_count;
            info!("Updated load info for node: {}. Task count: {}", node_load.node_id, node_load.task_count);
        } else {
            error!("Node not found in load balancer for update: {}", node_load.node_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn test_assign_task_chooses_least_loaded() {
        let load_balancer = LoadBalancer::new();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();
        let node3 = Uuid::new_v4();

        load_balancer.add_node(node1).await;
        load_balancer.add_node(node2).await;
        load_balancer.add_node(node3).await;

        {
            let mut nodes = load_balancer.nodes.write().await;
            nodes.get_mut(&node1).unwrap().task_count = 5;
            nodes.get_mut(&node2).unwrap().task_count = 3;
            nodes.get_mut(&node3).unwrap().task_count = 1;
        }

        let assigned = load_balancer.assign_task().await.unwrap();
        assert_eq!(assigned, node3);
    }

    #[tokio::test]
    async fn test_complete_task_decrements() {
        let load_balancer = LoadBalancer::new();
        let node = Uuid::new_v4();

        load_balancer.add_node(node).await;

        {
            let mut nodes = load_balancer.nodes.write().await;
            nodes.get_mut(&node).unwrap().task_count = 2;
        }

        load_balancer.complete_task(&node).await;
        {
            let nodes = load_balancer.nodes.read().await;
            assert_eq!(nodes.get(&node).unwrap().task_count, 1);
        }

        load_balancer.complete_task(&node).await;
        {
            let nodes = load_balancer.nodes.read().await;
            assert_eq!(nodes.get(&node).unwrap().task_count, 0);
        }
    }

    #[tokio::test]
    async fn test_assign_task_distributes_when_equal_loads() {
        let load_balancer = LoadBalancer::new();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();
        let node3 = Uuid::new_v4();

        load_balancer.add_node(node1).await;
        load_balancer.add_node(node2).await;
        load_balancer.add_node(node3).await;

        let mut assigned = HashSet::new();
        assigned.insert(load_balancer.assign_task().await.unwrap());
        assigned.insert(load_balancer.assign_task().await.unwrap());
        assigned.insert(load_balancer.assign_task().await.unwrap());

        assert_eq!(assigned.len(), 3);

        let nodes = load_balancer.nodes.read().await;
        assert_eq!(nodes.get(&node1).unwrap().task_count, 1);
        assert_eq!(nodes.get(&node2).unwrap().task_count, 1);
        assert_eq!(nodes.get(&node3).unwrap().task_count, 1);
    }
}
