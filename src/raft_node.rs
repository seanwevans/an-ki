//! Real Raft consensus via [`openraft`].
//!
//! This replaces the earlier hand-rolled vote-counting protocol with a proper
//! replicated log. It builds on the in-memory store and type configuration from
//! `openraft-memstore`: a node runs an [`AnKiRaft`] instance whose state machine
//! is kept consistent through Raft.
//!
//! [`StubNetworkFactory`] is the single extension point for multi-node
//! operation. A single-node cluster never dials peers, so the stub is
//! sufficient today; a real transport (over RabbitMQ or gRPC, say) plugs in by
//! replacing its RPC methods with ones that actually reach the target node.

use std::collections::BTreeMap;
use std::io::Error as IoError;
use std::sync::Arc;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::Adaptor;
use openraft::{Config, Raft, RaftNetwork, RaftNetworkFactory};
use openraft_memstore::{MemNodeId, MemStore, TypeConfig};

/// Convenience alias for this application's Raft handle.
pub type AnKiRaft = Raft<TypeConfig>;

/// Error returned while constructing or initializing a Raft node.
type RaftSetupError = Box<dyn std::error::Error + Send + Sync>;

/// Builds the `Unreachable` RPC error the stub network returns for every peer.
fn no_transport<E>() -> RPCError<MemNodeId, (), E>
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

    async fn new_client(&mut self, _target: MemNodeId, _node: &()) -> Self::Network {
        StubNetwork
    }
}

impl RaftNetwork<TypeConfig> for StubNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<MemNodeId>, RPCError<MemNodeId, (), RaftError<MemNodeId>>>
    {
        Err(no_transport())
    }

    async fn install_snapshot(
        &mut self,
        _rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MemNodeId>,
        RPCError<MemNodeId, (), RaftError<MemNodeId, InstallSnapshotError>>,
    > {
        Err(no_transport())
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<MemNodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<MemNodeId>, RPCError<MemNodeId, (), RaftError<MemNodeId>>> {
        Err(no_transport())
    }
}

/// Builds a Raft node backed by an in-memory store without forming a cluster.
/// The returned node has no leader until it is initialized (see
/// [`start_single_node`]) or joins an existing cluster.
pub async fn build_node(node_id: MemNodeId) -> Result<AnKiRaft, RaftSetupError> {
    let config = Arc::new(Config::default().validate()?);
    let store = Arc::new(MemStore::new());
    let (log_store, state_machine) = Adaptor::new(store);
    let raft = Raft::new(
        node_id,
        config,
        StubNetworkFactory,
        log_store,
        state_machine,
    )
    .await?;
    Ok(raft)
}

/// Builds a Raft node and initializes it as a single-member cluster, so it
/// elects itself leader and accepts writes immediately.
pub async fn start_single_node(node_id: MemNodeId) -> Result<AnKiRaft, RaftSetupError> {
    let raft = build_node(node_id).await?;
    let mut members = BTreeMap::new();
    members.insert(node_id, ());
    raft.initialize(members).await?;
    Ok(raft)
}

/// Resolves this node's numeric Raft id from `RAFT_NODE_ID`, defaulting to 1.
pub fn node_id_from_env() -> MemNodeId {
    std::env::var("RAFT_NODE_ID")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::ServerState;
    use openraft_memstore::ClientRequest;
    use std::time::Duration;

    #[tokio::test]
    async fn single_node_becomes_leader_and_commits_writes() {
        let raft = start_single_node(1).await.expect("start single-node raft");

        raft.wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "single node should elect itself")
            .await
            .expect("node became leader");
        assert_eq!(raft.current_leader().await, Some(1));

        let response = raft
            .client_write(ClientRequest {
                client: "client-1".to_string(),
                serial: 1,
                status: "online".to_string(),
            })
            .await
            .expect("client write committed");
        assert!(response.log_id.index >= 1);

        raft.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn uninitialized_node_has_no_leader() {
        let raft = build_node(7).await.expect("build raft");
        assert_eq!(raft.current_leader().await, None);
        raft.shutdown().await.expect("clean shutdown");
    }
}
