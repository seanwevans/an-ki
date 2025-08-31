use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};
use uuid::Uuid;

use async_trait::async_trait;
use openraft::error::{InstallSnapshotError, RPCError, RaftError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, Config, Raft};
use openraft_memstore::{new_mem_store, ClientRequest, TypeConfig};
use sled::Db;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusProposal {
    pub proposal_id: Uuid,
    pub content: String,
    pub proposer_id: Uuid,
}

pub type NodeId = u64;

struct DummyNetwork;

#[async_trait]
impl RaftNetwork<TypeConfig> for DummyNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError<TypeConfig, RaftError<TypeConfig>>>
    {
        Ok(AppendEntriesResponse::Success)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<TypeConfig>,
        RPCError<TypeConfig, RaftError<TypeConfig, InstallSnapshotError>>,
    > {
        Ok(InstallSnapshotResponse { vote: rpc.vote })
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig, RaftError<TypeConfig>>> {
        Ok(VoteResponse::new(rpc.vote, rpc.last_log_id, true))
    }
}

struct DummyNetworkFactory;

#[async_trait]
impl RaftNetworkFactory<TypeConfig> for DummyNetworkFactory {
    type Network = DummyNetwork;

    async fn new_client(&mut self, _target: NodeId, _node: &BasicNode) -> Self::Network {
        DummyNetwork
    }
}

pub async fn run_consensus_protocol(
    node_id: NodeId,
    mut proposal_rx: mpsc::Receiver<ConsensusProposal>,
    commit_tx: broadcast::Sender<String>,
) -> Result<(), Box<dyn Error>> {
    let config = Arc::new(Config::default().validate()?);
    let (log_store, state_machine) = new_mem_store();
    let network = DummyNetworkFactory;
    let raft = Raft::new(node_id, config, network, log_store, state_machine).await?;
    let db: Db = sled::open(format!("raft-{}", node_id))?;

    while let Some(p) = proposal_rx.recv().await {
        let req = ClientRequest {
            client: p.proposer_id.to_string(),
            serial: 0,
            status: p.content.clone(),
        };
        match raft.client_write(req).await {
            Ok(res) => {
                let _ = db.insert(
                    res.log_id.to_string().as_bytes(),
                    serde_json::to_vec(&res.data)?,
                );
                let _ = commit_tx.send(res.data.0.clone().unwrap_or_default());
                info!("Committed proposal {}", p.proposal_id);
            }
            Err(e) => error!("raft write error: {:?}", e),
        }
    }

    Ok(())
}
