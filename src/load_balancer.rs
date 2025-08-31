// load_balancer.rs: Implements load balancing for An nodes to effectively distribute tasks.

use std::collections::{BinaryHeap, HashMap};
use std::cmp::Ordering;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;
use tokio::sync::broadcast;
use tracing::{info, error};
use rand::seq::IteratorRandom;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeLoadInfo {
    pub node_id: Uuid,
    pub task_count: usize,
}

impl Ord for NodeLoadInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the comparison for a min-heap based on task_count
        other.task_count.cmp(&self.task_count)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

impl PartialOrd for NodeLoadInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone)]
pub struct LoadBalancer {
    pub nodes: Arc<RwLock<HashMap<Uuid, NodeLoadInfo>>>,
    pub heap: Arc<RwLock<BinaryHeap<NodeLoadInfo>>>,
}

impl LoadBalancer {
    pub fn new() -> Self {
        LoadBalancer {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            heap: Arc::new(RwLock::new(BinaryHeap::new())),
        }
    }

    pub async fn add_node(&self, node_id: Uuid) {
        let mut nodes = self.nodes.write().await;
        let info = NodeLoadInfo { node_id, task_count: 0 };
        nodes.insert(node_id, info.clone());
        drop(nodes);
        let mut heap = self.heap.write().await;
        heap.push(info);
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
        loop {
            let candidate = {
                let mut heap = self.heap.write().await;
                heap.pop()
            };

            if let Some(node_info) = candidate {
                let mut nodes = self.nodes.write().await;
                if let Some(entry) = nodes.get_mut(&node_info.node_id) {
                    if entry.task_count == node_info.task_count {
                        entry.task_count += 1;
                        let updated = entry.clone();
                        drop(nodes);
                        let mut heap = self.heap.write().await;
                        heap.push(updated.clone());
                        info!("Assigned task to node: {}. Task count: {}", updated.node_id, updated.task_count);
                        return Some(updated.node_id);
                    } else {
                        let updated = entry.clone();
                        drop(nodes);
                        let mut heap = self.heap.write().await;
                        heap.push(updated);
                    }
                }
                // Node might have been removed; continue loop
            } else {
                error!("No nodes available to assign task.");
                return None;
            }
        }
    }

    pub async fn complete_task(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if let Some(entry) = nodes.get_mut(node_id) {
            if entry.task_count > 0 {
                entry.task_count -= 1;
                let updated = entry.clone();
                drop(nodes);
                let mut heap = self.heap.write().await;
                heap.push(updated.clone());
                info!("Completed task on node: {}. Remaining task count: {}", node_id, updated.task_count);
            } else {
                info!("Completed task on node: {}. Remaining task count: 0", node_id);
            }
        } else {
            error!("Failed to complete task: Node not found: {}", node_id);
        }
    }

    pub async fn random_node(&self) -> Option<Uuid> {
        let nodes = self.nodes.read().await;
        nodes.keys().copied().choose(&mut rand::thread_rng())
    }
}

pub async fn monitor_node_load(mut rx: broadcast::Receiver<NodeLoadInfo>, load_balancer: LoadBalancer) {
    while let Ok(node_load) = rx.recv().await {
        let mut nodes = load_balancer.nodes.write().await;
        if let Some(entry) = nodes.get_mut(&node_load.node_id) {
            entry.task_count = node_load.task_count;
            let updated = entry.clone();
            drop(nodes);
            let mut heap = load_balancer.heap.write().await;
            heap.push(updated);
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
    use tokio::time::{sleep, Duration};

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
            if let Some(n) = nodes.get_mut(&node1) { n.task_count = 5; }
            if let Some(n) = nodes.get_mut(&node2) { n.task_count = 3; }
            if let Some(n) = nodes.get_mut(&node3) { n.task_count = 1; }
            let n1 = nodes.get(&node1).cloned().unwrap();
            let n2 = nodes.get(&node2).cloned().unwrap();
            let n3 = nodes.get(&node3).cloned().unwrap();
            drop(nodes);
            let mut heap = load_balancer.heap.write().await;
            heap.clear();
            heap.push(n1);
            heap.push(n2);
            heap.push(n3);
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
            if let Some(info) = nodes.get_mut(&node) {
                info.task_count = 2;
            }
            let n = nodes.get(&node).cloned().unwrap();
            drop(nodes);
            let mut heap = load_balancer.heap.write().await;
            heap.clear();
            heap.push(n);
        }

        load_balancer.complete_task(&node).await;
        {
            let nodes = load_balancer.nodes.read().await;
            let count = nodes.get(&node).unwrap().task_count;
            assert_eq!(count, 1);
        }

        load_balancer.complete_task(&node).await;
        {
            let nodes = load_balancer.nodes.read().await;
            let count = nodes.get(&node).unwrap().task_count;
            assert_eq!(count, 0);
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
        let c1 = nodes.get(&node1).unwrap().task_count;
        let c2 = nodes.get(&node2).unwrap().task_count;
        let c3 = nodes.get(&node3).unwrap().task_count;
        assert_eq!(c1, 1);
        assert_eq!(c2, 1);
        assert_eq!(c3, 1);
    }

    #[tokio::test]
    async fn test_remove_and_random_node() {
        let lb = LoadBalancer::new();
        let node = Uuid::new_v4();
        lb.add_node(node).await;
        assert_eq!(lb.random_node().await, Some(node));
        lb.remove_node(&node).await;
        assert!(lb.random_node().await.is_none());
    }

    #[tokio::test]
    async fn test_monitor_node_load_updates() {
        let lb = LoadBalancer::new();
        let node = Uuid::new_v4();
        lb.add_node(node).await;
        let (tx, rx) = broadcast::channel(1);
        let lb_clone = lb.clone();
        tokio::spawn(async move { monitor_node_load(rx, lb_clone).await; });
        tx.send(NodeLoadInfo { node_id: node, task_count: 4 }).unwrap();
        sleep(Duration::from_millis(50)).await;
        let nodes = lb.nodes.read().await;
        let count = nodes.get(&node).unwrap().task_count;
        assert_eq!(count, 4);
    }
}
