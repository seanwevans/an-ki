// consensus.rs: Implements a consensus algorithm to ensure consistency across An nodes.

use crate::logging_metrics;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
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
/// Tracks proposals and votes in-memory.
#[derive(Default)]
struct ConsensusState {
    proposals: RwLock<HashMap<Uuid, ConsensusProposal>>,
    votes: RwLock<HashMap<Uuid, usize>>,
}

impl ConsensusState {
    fn new() -> Self {
        Self::default()
    }

    async fn add_proposal(&self, proposal: ConsensusProposal) {
        self.votes.write().await.insert(proposal.proposal_id, 0);
        self.proposals
            .write()
            .await
            .insert(proposal.proposal_id, proposal);
        info!("Received proposal: {}", proposal.proposal_id);
    }

    async fn cast_vote(&self, proposal_id: Uuid) {
        let mut votes = self.votes.write().await;
        if let Some(vote_count) = votes.get_mut(&proposal_id) {
            *vote_count += 1;
            info!(
                "Cast vote for proposal: {}. Current votes: {}",
                proposal_id, *vote_count
            );
        } else {
            error!("Proposal not found: {}", proposal_id);
        }
    }

    async fn has_consensus(&self, proposal_id: Uuid, threshold: usize) -> bool {
        let votes = self.votes.read().await;
        matches!(votes.get(&proposal_id), Some(&count) if count >= threshold)
    }
}

struct DummyNetwork;

#[async_trait]
impl RaftNetwork<TypeConfig> for DummyNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _opt: RPCOption,
    ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError<TypeConfig, RaftError<TypeConfig>>>
    {
        Ok(AppendEntriesResponse {
            vote: rpc.vote,
            success: true,
            conflict: None,
        })
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
        Ok(VoteResponse {
            vote: rpc.vote,
            vote_granted: true,
        })
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
    mut vote_rx: broadcast::Receiver<Uuid>,
    commit_tx: broadcast::Sender<String>,
    consensus_threshold: usize,
) -> Result<(), Box<dyn Error>> {
    let consensus_state = ConsensusState::new();

    let config = Arc::new(Config::default().validate()?);
    let (log_store, state_machine) = new_mem_store();
    let network = DummyNetworkFactory;
    let raft = Raft::new(node_id, config, network, log_store, state_machine).await?;
    let db: Db = sled::open(format!("raft-{}", node_id))?;

    loop {
        tokio::select! {
            Some(proposal) = proposal_rx.recv() => {
                consensus_state.add_proposal(proposal.clone()).await;

                let req = ClientRequest {
                    client: proposal.proposer_id.to_string(),
                    serial: 0,
                    status: proposal.content.clone(),
                };
                match raft.client_write(req).await {
                    Ok(res) => {
                        let _ = db.insert(
                            res.log_id.to_string().as_bytes(),
                            serde_json::to_vec(&res.data)?,
                        );
                        let _ = commit_tx.send(res.data.0.clone().unwrap_or_default());
                        info!("Committed proposal {}", proposal.proposal_id);
                    }
                    Err(e) => error!("raft write error: {:?}", e),
                }
            }
            Ok(proposal_id) = vote_rx.recv() => {
                consensus_state.cast_vote(proposal_id).await;
                if consensus_state.has_consensus(proposal_id, consensus_threshold).await {
                    info!("Consensus reached for proposal: {}", proposal_id);
                    logging_metrics::set_consensus_state(1.0);
                }
            }
            else => break,
        }
    }

    Ok(())
}
