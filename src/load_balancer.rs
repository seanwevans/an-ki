//! Load balancing for assigning work to nodes.
//!
//! The [`LoadBalancer`] maintains a set of active nodes and distributes work to the
//! least loaded node. It uses a binary heap to efficiently choose candidates and
//! exposes helpers for updating load information.

use rand::seq::IteratorRandom;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

/// Stores the current task load for a node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeLoadInfo {
    /// Unique identifier of the node.
    pub node_id: Uuid,
    /// Number of tasks currently assigned to the node.
    pub task_count: usize,
}

impl Ord for NodeLoadInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the comparison for a min-heap based on task_count
        other
            .task_count
            .cmp(&self.task_count)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

impl PartialOrd for NodeLoadInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Coordinates task distribution among nodes.
#[derive(Clone)]
pub struct LoadBalancer {
    /// Mapping of node identifiers to their load information.
    pub nodes: Arc<RwLock<HashMap<Uuid, NodeLoadInfo>>>,
    /// Min-heap of nodes ordered by [`NodeLoadInfo::task_count`].
    pub heap: Arc<RwLock<BinaryHeap<NodeLoadInfo>>>,
}

impl Default for LoadBalancer {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadBalancer {
    /// Creates an empty [`LoadBalancer`].
    pub fn new() -> Self {
        LoadBalancer {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            heap: Arc::new(RwLock::new(BinaryHeap::new())),
        }
    }

    /// Registers a new node with zero initial load.
    ///
    /// # Parameters
    /// * `node_id` - Identifier of the node to add.
    pub async fn add_node(&self, node_id: Uuid) {
        let mut nodes = self.nodes.write().await;
        let info = NodeLoadInfo {
            node_id,
            task_count: 0,
        };
        nodes.insert(node_id, info.clone());
        drop(nodes);
        let mut heap = self.heap.write().await;
        heap.push(info);
        info!("Added node to load balancer: {}", node_id);
    }

    /// Removes a node from the balancer.
    ///
    /// # Parameters
    /// * `node_id` - Identifier of the node to remove.
    pub async fn remove_node(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if nodes.remove(node_id).is_some() {
            info!("Removed node from load balancer: {}", node_id);
            drop(nodes);
            let mut heap = self.heap.write().await;
            let rebuilt: BinaryHeap<_> = heap.drain().filter(|n| &n.node_id != node_id).collect();
            *heap = rebuilt;
        } else {
            error!(
                "Failed to remove node from load balancer: Node not found: {}",
                node_id
            );
        }
    }

    /// Assigns a task to the least-loaded node.
    ///
    /// # Returns
    /// * `Some(Uuid)` - Identifier of the chosen node.
    /// * `None` - No nodes were available for assignment.
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
                        info!(
                            "Assigned task to node: {}. Task count: {}",
                            updated.node_id, updated.task_count
                        );
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

    /// Decrements the load count for a node after task completion.
    ///
    /// # Parameters
    /// * `node_id` - Identifier of the node whose load should decrease.
    pub async fn complete_task(&self, node_id: &Uuid) {
        let mut nodes = self.nodes.write().await;
        if let Some(entry) = nodes.get_mut(node_id) {
            if entry.task_count > 0 {
                entry.task_count -= 1;
                let updated = entry.clone();
                drop(nodes);
                let mut heap = self.heap.write().await;
                let rebuilt: BinaryHeap<_> = heap
                    .drain()
                    .filter(|n| n.node_id != updated.node_id)
                    .collect();
                *heap = rebuilt;
                heap.push(updated.clone());
                info!(
                    "Completed task on node: {}. Remaining task count: {}",
                    node_id, updated.task_count
                );
            } else {
                info!(
                    "Completed task on node: {}. Remaining task count: 0",
                    node_id
                );
            }
        } else {
            error!("Failed to complete task: Node not found: {}", node_id);
        }
    }

    /// Returns a random node identifier or `None` if the balancer is empty.
    pub async fn random_node(&self) -> Option<Uuid> {
        let nodes = self.nodes.read().await;
        nodes.keys().copied().choose(&mut rand::thread_rng())
    }
}

/// Updates the load balancer using load reports from nodes.
///
/// The function listens on `rx` for [`NodeLoadInfo`] messages and updates the
/// balancer to reflect the reported task counts.
///
/// # Parameters
/// * `rx` - Channel receiving load updates from nodes.
/// * `load_balancer` - Shared load balancer to update.
pub async fn monitor_node_load(
    mut rx: broadcast::Receiver<NodeLoadInfo>,
    load_balancer: LoadBalancer,
) {
    while let Ok(node_load) = rx.recv().await {
        let mut nodes = load_balancer.nodes.write().await;
        if let Some(entry) = nodes.get_mut(&node_load.node_id) {
            entry.task_count = node_load.task_count;
            let updated = entry.clone();
            drop(nodes);
            let mut heap = load_balancer.heap.write().await;
            let rebuilt: BinaryHeap<_> = heap
                .drain()
                .filter(|n| n.node_id != updated.node_id)
                .collect();
            *heap = rebuilt;
            heap.push(updated);
            info!(
                "Updated load info for node: {}. Task count: {}",
                node_load.node_id, node_load.task_count
            );
        } else {
            error!(
                "Node not found in load balancer for update: {}",
                node_load.node_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tokio::time::{timeout, Duration};
    use tokio::task::yield_now;

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
            if let Some(n) = nodes.get_mut(&node1) {
                n.task_count = 5;
            }
            if let Some(n) = nodes.get_mut(&node2) {
                n.task_count = 3;
            }
            if let Some(n) = nodes.get_mut(&node3) {
                n.task_count = 1;
            }
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
    async fn test_complete_task_no_duplicate_heap_entries() {
        let lb = LoadBalancer::new();
        let node = Uuid::new_v4();

        lb.add_node(node).await;

        {
            let mut nodes = lb.nodes.write().await;
            if let Some(info) = nodes.get_mut(&node) {
                info.task_count = 3;
            }
            let n = nodes.get(&node).cloned().unwrap();
            drop(nodes);
            let mut heap = lb.heap.write().await;
            heap.clear();
            heap.push(n);
        }

        for _ in 0..5 {
            lb.complete_task(&node).await;
        }

        let heap = lb.heap.read().await;
        assert_eq!(heap.len(), 1);
        let entry = heap.peek().unwrap();
        assert_eq!(entry.task_count, 0);
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
    async fn test_remove_node_clears_heap_entries() {
        let lb = LoadBalancer::new();
        let node1 = Uuid::new_v4();
        let node2 = Uuid::new_v4();
        lb.add_node(node1).await;
        lb.add_node(node2).await;
        {
            let mut heap = lb.heap.write().await;
            if let Some(entry) = heap.iter().find(|n| n.node_id == node1).cloned() {
                heap.push(entry);
            }
        }
        lb.remove_node(&node1).await;
        let heap = lb.heap.read().await;
        assert!(heap.iter().all(|n| n.node_id != node1));
        assert_eq!(heap.len(), 1);
    }

    #[tokio::test]
    async fn test_monitor_node_load_updates() {
        let lb = LoadBalancer::new();
        let node = Uuid::new_v4();
        lb.add_node(node).await;
        let (tx, rx) = broadcast::channel(1);
        let lb_clone = lb.clone();
        tokio::spawn(async move {
            monitor_node_load(rx, lb_clone).await;
        });
        tx.send(NodeLoadInfo {
            node_id: node,
            task_count: 4,
        })
        .unwrap();

        // Wait until the update is processed
        timeout(Duration::from_secs(1), async {
            loop {
                if lb
                    .nodes
                    .read()
                    .await
                    .get(&node)
                    .map(|n| n.task_count)
                    == Some(4)
                {
                    break;
                }
                yield_now().await;
            }
        })
        .await
        .expect("load update");
        let nodes = lb.nodes.read().await;
        let count = nodes.get(&node).unwrap().task_count;
        assert_eq!(count, 4);
    }

    #[tokio::test]
    async fn test_monitor_node_load_no_duplicates() {
        let lb = LoadBalancer::new();
        let node = Uuid::new_v4();
        lb.add_node(node).await;
        let (tx, rx) = broadcast::channel(2);
        let lb_clone = lb.clone();
        tokio::spawn(async move {
            monitor_node_load(rx, lb_clone).await;
        });
        tx.send(NodeLoadInfo {
            node_id: node,
            task_count: 1,
        })
        .unwrap();
        tx.send(NodeLoadInfo {
            node_id: node,
            task_count: 3,
        })
        .unwrap();

        // Wait for heap to reflect the latest update
        timeout(Duration::from_secs(1), async {
            loop {
                let heap = lb.heap.read().await;
                if heap.len() == 1 && heap.peek().unwrap().task_count == 3 {
                    break;
                }
                drop(heap);
                yield_now().await;
            }
        })
        .await
        .expect("heap update");
        let heap = lb.heap.read().await;
        assert_eq!(heap.len(), 1);
        let entry = heap.peek().unwrap();
        assert_eq!(entry.task_count, 3);
    }
}
