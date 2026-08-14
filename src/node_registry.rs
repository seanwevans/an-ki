//! Live view of the nodes making up the cluster.
//!
//! The principal builds this registry from the heartbeats it consumes on
//! [`HEARTBEAT_QUEUE`](crate::health::HEARTBEAT_QUEUE): a node appears the first
//! time it is heard from and disappears once it has been silent for longer than
//! the configured time-to-live. There is no separate registration handshake, so
//! a node that restarts under the same identity simply re-appears.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::common::{NodeInfo, NodeRole};

/// How long a node may go without heartbeating before [`NodeRegistry::prune_stale`]
/// evicts it. Overridable with `NODE_TTL_MS`; defaults to 30s, which tolerates two
/// missed beats at the default 10s heartbeat interval.
pub fn node_ttl() -> Duration {
    let ms = std::env::var("NODE_TTL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(30_000);
    Duration::from_millis(ms)
}

/// How often the principal logs its view of the cluster. Overridable with
/// `CLUSTER_REPORT_INTERVAL_MS`; defaults to 60s.
pub fn cluster_report_interval() -> Duration {
    let ms = std::env::var("CLUSTER_REPORT_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(60_000);
    Duration::from_millis(ms)
}

/// Tracks which nodes are currently part of the cluster, keyed by the node
/// identity carried in heartbeats (`NODE_ID`, or a generated UUID string).
#[derive(Clone, Default)]
pub struct NodeRegistry {
    nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a heartbeat from `node_id`, inserting the node if this is the
    /// first time it has been heard from and refreshing `last_seen` otherwise.
    ///
    /// Returns `true` when the node was newly added, so callers can distinguish
    /// a node joining from routine liveness traffic.
    pub async fn record_heartbeat(&self, node_id: &str, role: NodeRole) -> bool {
        self.record_heartbeat_at(node_id, role, Utc::now()).await
    }

    /// [`record_heartbeat`](Self::record_heartbeat) with an explicit timestamp,
    /// so tests can age entries without sleeping.
    pub async fn record_heartbeat_at(
        &self,
        node_id: &str,
        role: NodeRole,
        seen_at: DateTime<Utc>,
    ) -> bool {
        let mut nodes = self.nodes.write().await;
        match nodes.get_mut(node_id) {
            Some(existing) => {
                existing.last_seen = Some(seen_at);
                // A node that comes back under a different role (a redeploy that
                // changed the container's command, say) should not keep the old one.
                if existing.role != role {
                    info!(
                        "Node {} changed role from {:?} to {:?}",
                        node_id, existing.role, role
                    );
                    existing.role = role;
                }
                debug!("Refreshed heartbeat for node {}", node_id);
                false
            }
            None => {
                info!("Node {} joined the cluster as {:?}", node_id, role);
                nodes.insert(
                    node_id.to_owned(),
                    NodeInfo {
                        id: node_id.to_owned(),
                        address: None,
                        last_seen: Some(seen_at),
                        role,
                    },
                );
                true
            }
        }
    }

    /// Drops every node whose last heartbeat is older than `ttl`, returning the
    /// identities that were evicted. Nodes with no recorded `last_seen` are
    /// treated as stale.
    pub async fn prune_stale(&self, ttl: Duration) -> Vec<String> {
        self.prune_stale_at(ttl, Utc::now()).await
    }

    /// [`prune_stale`](Self::prune_stale) evaluated against an explicit "now".
    pub async fn prune_stale_at(&self, ttl: Duration, now: DateTime<Utc>) -> Vec<String> {
        let cutoff = match chrono::Duration::from_std(ttl) {
            Ok(ttl) => now - ttl,
            // A TTL too large to represent means nothing can be stale.
            Err(_) => return Vec::new(),
        };

        let mut nodes = self.nodes.write().await;
        let mut evicted = Vec::new();
        nodes.retain(|node_id, info| {
            let fresh = info.last_seen.is_some_and(|seen| seen > cutoff);
            if !fresh {
                evicted.push(node_id.clone());
            }
            fresh
        });

        for node_id in &evicted {
            info!(
                "Node {} evicted from the cluster after {:?} without a heartbeat",
                node_id, ttl
            );
        }
        evicted
    }

    /// Removes a single node regardless of how recently it was seen.
    pub async fn remove_node(&self, node_id: &str) -> bool {
        let removed = self.nodes.write().await.remove(node_id).is_some();
        if removed {
            info!("Removed node {} from the registry", node_id);
        }
        removed
    }

    pub async fn get_node(&self, node_id: &str) -> Option<NodeInfo> {
        self.nodes.read().await.get(node_id).cloned()
    }

    /// Every node currently considered part of the cluster.
    pub async fn list_nodes(&self) -> Vec<NodeInfo> {
        self.nodes.read().await.values().cloned().collect()
    }

    /// The subset of the cluster serving `role`, which is how schedulers find
    /// the Ki nodes eligible to receive work.
    pub async fn list_by_role(&self, role: NodeRole) -> Vec<NodeInfo> {
        self.nodes
            .read()
            .await
            .values()
            .filter(|info| info.role == role)
            .cloned()
            .collect()
    }

    pub async fn len(&self) -> usize {
        self.nodes.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.nodes.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_heartbeat_adds_the_node() {
        let registry = NodeRegistry::new();

        assert!(registry.record_heartbeat("an-0", NodeRole::An).await);

        let info = registry.get_node("an-0").await.expect("node registered");
        assert_eq!(info.id, "an-0");
        assert_eq!(info.role, NodeRole::An);
        assert!(info.last_seen.is_some());
    }

    #[tokio::test]
    async fn repeat_heartbeat_refreshes_rather_than_duplicates() {
        let registry = NodeRegistry::new();
        let first = Utc::now() - chrono::Duration::seconds(5);

        registry
            .record_heartbeat_at("ki-0", NodeRole::Ki, first)
            .await;
        let added_again = registry.record_heartbeat("ki-0", NodeRole::Ki).await;

        assert!(!added_again, "a known node must not be reported as joining");
        assert_eq!(registry.len().await, 1);
        let info = registry.get_node("ki-0").await.unwrap();
        assert!(info.last_seen.unwrap() > first);
    }

    #[tokio::test]
    async fn heartbeat_with_a_new_role_updates_the_entry() {
        let registry = NodeRegistry::new();
        registry.record_heartbeat("node-0", NodeRole::Ki).await;

        registry.record_heartbeat("node-0", NodeRole::An).await;

        assert_eq!(
            registry.get_node("node-0").await.unwrap().role,
            NodeRole::An
        );
        assert_eq!(registry.len().await, 1);
    }

    #[tokio::test]
    async fn prune_evicts_only_nodes_past_the_ttl() {
        let registry = NodeRegistry::new();
        let now = Utc::now();
        registry
            .record_heartbeat_at("fresh", NodeRole::Ki, now - chrono::Duration::seconds(5))
            .await;
        registry
            .record_heartbeat_at("stale", NodeRole::Ki, now - chrono::Duration::seconds(60))
            .await;

        let evicted = registry.prune_stale_at(Duration::from_secs(30), now).await;

        assert_eq!(evicted, vec!["stale".to_string()]);
        assert!(registry.get_node("fresh").await.is_some());
        assert!(registry.get_node("stale").await.is_none());
    }

    #[tokio::test]
    async fn prune_keeps_a_node_that_just_heartbeat() {
        let registry = NodeRegistry::new();
        registry.record_heartbeat("an-0", NodeRole::An).await;

        assert!(registry
            .prune_stale(Duration::from_secs(30))
            .await
            .is_empty());
        assert_eq!(registry.len().await, 1);
    }

    #[tokio::test]
    async fn list_by_role_partitions_the_cluster() {
        let registry = NodeRegistry::new();
        registry.record_heartbeat("an-0", NodeRole::An).await;
        registry.record_heartbeat("ki-0", NodeRole::Ki).await;
        registry.record_heartbeat("ki-1", NodeRole::Ki).await;

        let mut ki: Vec<String> = registry
            .list_by_role(NodeRole::Ki)
            .await
            .into_iter()
            .map(|info| info.id)
            .collect();
        ki.sort();

        assert_eq!(ki, vec!["ki-0".to_string(), "ki-1".to_string()]);
        assert_eq!(registry.list_by_role(NodeRole::An).await.len(), 1);
        assert_eq!(registry.list_nodes().await.len(), 3);
    }

    #[tokio::test]
    async fn remove_node_reports_whether_it_was_present() {
        let registry = NodeRegistry::new();
        registry.record_heartbeat("an-0", NodeRole::An).await;

        assert!(registry.remove_node("an-0").await);
        assert!(!registry.remove_node("an-0").await);
        assert!(registry.is_empty().await);
    }

    #[tokio::test]
    async fn registry_clones_share_one_view() {
        let registry = NodeRegistry::new();
        let clone = registry.clone();

        clone.record_heartbeat("ki-0", NodeRole::Ki).await;

        assert_eq!(registry.len().await, 1);
    }
}
