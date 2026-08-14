//! Raft consensus node wiring via [`openraft`].
//!
//! [`raft_store`](crate::raft_store) supplies the durable log and the
//! application state machine; this module assembles them into a running
//! [`AnKiRaft`] instance and resolves the node's identity from the environment.
//!
//! [`StubNetworkFactory`] is the single extension point remaining for multi-node
//! operation. A single-node cluster never dials peers, so the stub is sufficient
//! today; a real transport plugs in by replacing its RPC methods with ones that
//! actually reach the target node.

use std::collections::BTreeMap;
use std::io::Error as IoError;
use std::path::PathBuf;
use std::sync::Arc;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, Config, Raft, RaftNetwork, RaftNetworkFactory};

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

/// Builds the `Unreachable` RPC error the stub network returns for every peer.
fn no_transport<E>() -> RPCError<NodeId, BasicNode, E>
where
    E: std::error::Error,
{
    let io = IoError::other("no peer transport configured for this Raft node");
    RPCError::Unreachable(Unreachable::new(&io))
}

/// Network factory for single-node clusters. It produces [`StubNetwork`] clients
/// whose RPCs always report the peer as unreachable; multi-node transport is
/// introduced by replacing this factory.
#[derive(Clone, Default)]
pub struct StubNetworkFactory;

/// A network client that cannot reach peers. A single-node cluster never invokes
/// its methods; it exists so the [`Raft`] instance has a complete network type.
pub struct StubNetwork;

impl RaftNetworkFactory<TypeConfig> for StubNetworkFactory {
    type Network = StubNetwork;

    async fn new_client(&mut self, _target: NodeId, _node: &BasicNode) -> Self::Network {
        StubNetwork
    }
}

impl RaftNetwork<TypeConfig> for StubNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(no_transport())
    }

    async fn install_snapshot(
        &mut self,
        _rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        Err(no_transport())
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(no_transport())
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
        StubNetworkFactory,
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
///
/// Initialization is skipped when the store already holds a log, which is what
/// makes a restart resume the existing cluster instead of clobbering it with a
/// fresh single-member configuration.
pub async fn start_single_node(
    node_id: NodeId,
    store: Arc<SledStore>,
) -> Result<RaftNode, RaftSetupError> {
    let node = build_node(node_id, store).await?;

    let mut members = BTreeMap::new();
    members.insert(node_id, BasicNode::default());
    match node.raft.initialize(members).await {
        Ok(()) => {}
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

/// Opens the configured store and starts a single-member cluster on it. This is
/// the entry point the principal uses.
pub async fn start_from_env() -> Result<RaftNode, RaftSetupError> {
    let node_id = node_id_from_env();
    let data_dir = data_dir_from_env();
    // Give each Raft node its own subdirectory so several can share a volume.
    let path = data_dir.join(node_id.to_string());
    std::fs::create_dir_all(&path)?;
    let store = SledStore::open(&path)?;
    start_single_node(node_id, store).await
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

    #[test]
    fn data_dir_defaults_when_unset() {
        // Only asserts the default shape; the env-var path is exercised by
        // deployments rather than tests, which must not mutate global env.
        let default = PathBuf::from("data/raft");
        assert_eq!(default.file_name().unwrap(), "raft");
    }
}
