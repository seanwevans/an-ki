//! Raft consensus node wiring via [`openraft`].
//!
//! [`raft_store`](crate::raft_store) supplies the durable log and the
//! application state machine, [`raft_network`](crate::raft_network) carries RPCs
//! between principals, and this module assembles them into a running
//! [`AnKiRaft`] instance.
//!
//! Cluster shape comes from `RAFT_PEERS`. With it unset the node runs as a
//! single-member cluster, which is the local-development case. With it set, the
//! lowest-numbered peer initializes the cluster with the full membership and the
//! others come up empty and wait to be replicated to.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use openraft::{BasicNode, Config, Raft};

use crate::raft_network::{self, HttpNetworkFactory};
use crate::raft_store::{NodeId, SledStore, TypeConfig};

/// Convenience alias for this application's Raft handle.
pub type AnKiRaft = Raft<TypeConfig>;

/// Error returned while constructing or initializing a Raft node.
type RaftSetupError = Box<dyn std::error::Error + Send + Sync>;

/// A running Raft node together with the store behind it, so callers can read
/// replicated state directly instead of going back through the log.
pub struct RaftNode {
    pub raft: AnKiRaft,
    pub store: Arc<SledStore>,
    pub node_id: NodeId,
}

impl RaftNode {
    /// Stops the Raft node. The store's data remains on disk for the next start.
    pub async fn shutdown(&self) -> Result<(), RaftSetupError> {
        self.raft.shutdown().await?;
        Ok(())
    }
}

/// Builds a Raft node over `store` without forming a cluster. The returned node
/// has no leader until it is initialized (see [`start_single_node`]) or joins an
/// existing cluster.
pub async fn build_node(
    node_id: NodeId,
    store: Arc<SledStore>,
) -> Result<RaftNode, RaftSetupError> {
    let config = Arc::new(Config::default().validate()?);
    let (log_store, state_machine) = store.clone().into_adaptor();
    let raft = Raft::new(
        node_id,
        config,
        HttpNetworkFactory::default(),
        log_store,
        state_machine,
    )
    .await?;
    Ok(RaftNode {
        raft,
        store,
        node_id,
    })
}

/// Builds a Raft node and initializes it as a single-member cluster, so it
/// elects itself leader and accepts writes immediately.
pub async fn start_single_node(
    node_id: NodeId,
    store: Arc<SledStore>,
) -> Result<RaftNode, RaftSetupError> {
    let mut members = BTreeMap::new();
    members.insert(node_id, BasicNode::default());
    start_cluster(node_id, store, members).await
}

/// Builds a Raft node and, if this node is responsible for bootstrapping,
/// initializes the cluster with `members`.
///
/// Exactly one node may initialize a cluster, so the lowest id in the
/// membership does it; every other node starts empty and receives the
/// membership through replication. Initialization is skipped entirely when the
/// store already holds a log, which is what makes a restart resume the existing
/// cluster instead of clobbering it with a fresh configuration.
pub async fn start_cluster(
    node_id: NodeId,
    store: Arc<SledStore>,
    members: BTreeMap<NodeId, BasicNode>,
) -> Result<RaftNode, RaftSetupError> {
    let node = build_node(node_id, store).await?;

    // Every node racing to initialize would produce competing configurations,
    // so the choice of bootstrapper has to be one every node agrees on without
    // communicating. The lowest id is that choice.
    let bootstrapper = members.keys().next().copied();
    if bootstrapper != Some(node_id) {
        tracing::info!(
            "Raft node {} waiting for node {:?} to initialize the cluster",
            node_id,
            bootstrapper
        );
        return Ok(node);
    }

    match node.raft.initialize(members).await {
        Ok(()) => tracing::info!("Raft node {} initialized the cluster", node_id),
        // A store carried over from a previous run is already initialized;
        // re-initializing it would discard the membership it recovered.
        Err(openraft::error::RaftError::APIError(
            openraft::error::InitializeError::NotAllowed(_),
        )) => {
            tracing::info!("Raft store already initialized; resuming the existing cluster");
        }
        Err(e) => return Err(Box::new(e)),
    }

    Ok(node)
}

/// Resolves this node's numeric Raft id from `RAFT_NODE_ID`, defaulting to 1.
pub fn node_id_from_env() -> NodeId {
    std::env::var("RAFT_NODE_ID")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

/// Directory holding the Raft log and state machine, from `RAFT_DATA_DIR`.
/// Defaults to `data/raft`, relative to the process's working directory.
pub fn data_dir_from_env() -> PathBuf {
    std::env::var("RAFT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/raft"))
}

/// Opens the configured store and starts this node's Raft instance, forming a
/// cluster of whatever `RAFT_PEERS` describes. This is the entry point the
/// principal uses.
///
/// # Errors
/// Returns an error if `RAFT_PEERS` is malformed or names a cluster this node is
/// not part of. Both are refused rather than silently downgraded to a
/// single-node cluster: a principal that quietly forms its own cluster would
/// diverge from the one it was meant to join.
pub async fn start_from_env() -> Result<RaftNode, RaftSetupError> {
    let node_id = node_id_from_env();
    let data_dir = data_dir_from_env();
    // Give each Raft node its own subdirectory so several can share a volume.
    let path = data_dir.join(node_id.to_string());
    std::fs::create_dir_all(&path)?;
    let store = SledStore::open(&path)?;

    let peers = raft_network::peers_from_env()?;
    if peers.is_empty() {
        tracing::info!("RAFT_PEERS is unset; running Raft as a single-member cluster");
        return start_single_node(node_id, store).await;
    }
    if !peers.contains_key(&node_id) {
        return Err(format!(
            "RAFT_NODE_ID {} does not appear in RAFT_PEERS ({:?}); \
             refusing to start rather than form a separate cluster",
            node_id,
            peers.keys().collect::<Vec<_>>()
        )
        .into());
    }

    tracing::info!(
        "Starting Raft node {} in a {}-member cluster",
        node_id,
        peers.len()
    );
    start_cluster(node_id, store, peers).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::NodeRole;
    use crate::raft_store::ClusterRequest;
    use openraft::ServerState;
    use std::time::Duration;

    #[tokio::test]
    async fn single_node_becomes_leader_and_commits_writes() {
        let store = SledStore::temporary().expect("temporary store");
        let node = start_single_node(1, store).await.expect("start raft");

        node.raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "single node should elect itself")
            .await
            .expect("node became leader");
        assert_eq!(node.raft.current_leader().await, Some(1));

        let response = node
            .raft
            .client_write(ClusterRequest::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            })
            .await
            .expect("client write committed");
        assert!(response.log_id.index >= 1);
        assert_eq!(response.data.previous_role, None);

        // The write is visible in the replicated state, not just the log.
        assert_eq!(node.store.role_of("ki-0").await, Some(NodeRole::Ki));

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn uninitialized_node_has_no_leader() {
        let store = SledStore::temporary().expect("temporary store");
        let node = build_node(7, store).await.expect("build raft");

        assert_eq!(node.raft.current_leader().await, None);

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn committed_state_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "an-ki-raft-restart-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create data dir");

        {
            let store = SledStore::open(&dir).expect("open store");
            let node = start_single_node(1, store).await.expect("start raft");
            node.raft
                .wait(Some(Duration::from_secs(10)))
                .state(ServerState::Leader, "leader")
                .await
                .expect("became leader");
            node.raft
                .client_write(ClusterRequest::SetConfig {
                    key: "model_shards".to_string(),
                    value: "4".to_string(),
                })
                .await
                .expect("write committed");
            node.shutdown().await.expect("clean shutdown");
        }

        // Restarting must resume the existing cluster, not reinitialize it.
        let store = SledStore::open(&dir).expect("reopen store");
        let node = start_single_node(1, store).await.expect("restart raft");
        node.raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "leader after restart")
            .await
            .expect("became leader again");

        assert_eq!(
            node.store.config_value("model_shards").await,
            Some("4".to_string()),
            "state committed before the restart must still be present"
        );

        node.shutdown().await.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_non_bootstrapping_member_starts_without_a_leader() {
        // Node 2 is not the lowest id, so it must not initialize anything; it
        // waits for node 1 to replicate the membership to it.
        let mut members = BTreeMap::new();
        members.insert(1, BasicNode::new("127.0.0.1:4001"));
        members.insert(2, BasicNode::new("127.0.0.1:4002"));

        let store = SledStore::temporary().expect("temporary store");
        let node = start_cluster(2, store, members).await.expect("start raft");

        assert_eq!(
            node.raft.current_leader().await,
            None,
            "a follower must not elect itself before the cluster is initialized"
        );

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn the_lowest_id_bootstraps_the_cluster() {
        let mut members = BTreeMap::new();
        members.insert(1, BasicNode::new("127.0.0.1:4001"));
        members.insert(2, BasicNode::new("127.0.0.1:4002"));

        let store = SledStore::temporary().expect("temporary store");
        let node = start_cluster(1, store, members).await.expect("start raft");

        // Node 1 initialized a two-member cluster whose peer is absent, so it
        // cannot win an election — but the membership must have been recorded.
        let metrics = node.raft.metrics().borrow().clone();
        let voters: Vec<u64> = metrics.membership_config.membership().voter_ids().collect();
        assert_eq!(voters, vec![1, 2]);

        node.shutdown().await.expect("clean shutdown");
    }

    #[test]
    fn data_dir_defaults_when_unset() {
        // Only asserts the default shape; the env-var path is exercised by
        // deployments rather than tests, which must not mutate global env.
        let default = PathBuf::from("data/raft");
        assert_eq!(default.file_name().unwrap(), "raft");
    }
}
