// load_balancer.rs: Implements load balancing for An nodes to effectively distribute tasks.

use std::collections::BinaryHeap;
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
    pub nodes: Arc<RwLock<BinaryHeap<NodeLoadInfo>>>,
}

impl LoadBalancer {
    pub fn new() -> Self {
        LoadBalancer {
            nodes: Arc::new(RwLock::new(BinaryHeap::new())),
        }
    }

    pub async fn add_node(&self, node_id: Uuid) {
        let mut nodes = self.nodes.write().await;
        nodes.push(NodeLoadInfo { node_id, task_count: 0 });
        info!("Added node to load balancer: {}", node_id);
    }

    pub async fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        let mut vec: Vec<_> = nodes.drain().collect();
        let len_before = vec.len();
        vec.retain(|n| &n.node_id != node_id);
        if vec.len() < len_before {
            info!("Removed node from load balancer: {}", node_id);
        } else {
            error!("Failed to remove node from load balancer: Node not found: {}", node_id);
        }
        *nodes = BinaryHeap::from(vec);
    }

    pub async fn assign_task(&self) -> Option<Uuid> {
        let mut nodes = self.nodes.write().await;
        if let Some(mut node) = nodes.pop() {
            node.task_count += 1;
            let node_id = node.node_id;
            info!("Assigned task to node: {}. Task count: {}", node_id, node.task_count);
            nodes.push(node);
            Some(node_id)
        } else {
            error!("No nodes available to assign task.");
            None
        }
    }

    pub async fn complete_task(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        let mut vec: Vec<_> = nodes.drain().collect();
        let mut found = false;
        for node in vec.iter_mut() {
            if &node.node_id == node_id {
                if node.task_count > 0 {
                    node.task_count -= 1;
                    info!("Completed task on node: {}. Remaining task count: {}", node_id, node.task_count);
                }
                found = true;
                break;
            }
        }
        if !found {
            error!("Failed to complete task: Node not found: {}", node_id);
        }
        *nodes = BinaryHeap::from(vec);
    }

    pub async fn random_node(&self) -> Option<Uuid> {
        let nodes = self.nodes.read().await;
        let vec: Vec<_> = nodes.clone().into_vec();
        vec.iter().map(|n| n.node_id).choose(&mut rand::thread_rng())
    }
}

pub async fn monitor_node_load(mut rx: broadcast::Receiver<NodeLoadInfo>, load_balancer: LoadBalancer) {
    while let Ok(node_load) = rx.recv().await {
        let mut nodes = load_balancer.nodes.write().await;
        let mut vec: Vec<_> = nodes.drain().collect();
        let mut found = false;
        for node in vec.iter_mut() {
            if node.node_id == node_load.node_id {
                node.task_count = node_load.task_count;
                info!("Updated load info for node: {}. Task count: {}", node_load.node_id, node_load.task_count);
                found = true;
                break;
            }
        }
        if !found {
            error!("Node not found in load balancer for update: {}", node_load.node_id);
        }
        *nodes = BinaryHeap::from(vec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, BinaryHeap};
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
            let mut heap = load_balancer.nodes.write().await;
            let mut vec: Vec<_> = heap.drain().collect();
            for node in vec.iter_mut() {
                if node.node_id == node1 {
                    node.task_count = 5;
                } else if node.node_id == node2 {
                    node.task_count = 3;
                } else if node.node_id == node3 {
                    node.task_count = 1;
                }
            }
            *heap = BinaryHeap::from(vec);
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
            let mut heap = load_balancer.nodes.write().await;
            let mut vec: Vec<_> = heap.drain().collect();
            for node_info in vec.iter_mut() {
                if node_info.node_id == node {
                    node_info.task_count = 2;
                }
            }
            *heap = BinaryHeap::from(vec);
        }

        load_balancer.complete_task(&node).await;
        {
            let heap = load_balancer.nodes.read().await;
            let vec: Vec<_> = heap.clone().into_vec();
            let count = vec.iter().find(|n| n.node_id == node).unwrap().task_count;
            assert_eq!(count, 1);
        }

        load_balancer.complete_task(&node).await;
        {
            let heap = load_balancer.nodes.read().await;
            let vec: Vec<_> = heap.clone().into_vec();
            let count = vec.iter().find(|n| n.node_id == node).unwrap().task_count;
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

        let heap = load_balancer.nodes.read().await;
        let vec: Vec<_> = heap.clone().into_vec();
        let c1 = vec.iter().find(|n| n.node_id == node1).unwrap().task_count;
        let c2 = vec.iter().find(|n| n.node_id == node2).unwrap().task_count;
        let c3 = vec.iter().find(|n| n.node_id == node3).unwrap().task_count;
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
        let heap = lb.nodes.read().await;
        let vec: Vec<_> = heap.clone().into_vec();
        let count = vec.iter().find(|n| n.node_id == node).unwrap().task_count;
        assert_eq!(count, 4);
    }
}
