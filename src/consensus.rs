// consensus.rs: Implements a consensus algorithm to ensure consistency across An nodes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;
use tracing::{info, error};
use std::error::Error;
use serde::{Serialize, Deserialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusProposal {
    pub proposal_id: Uuid,
    pub content: String,
    pub proposer_id: Uuid,
}

#[derive(Clone, Debug)]
pub struct ConsensusState {
    pub proposals: Arc<RwLock<HashMap<Uuid, ConsensusProposal>>>,
    pub votes: Arc<RwLock<HashMap<Uuid, usize>>>,
}

impl ConsensusState {
    pub fn new() -> Self {
        ConsensusState {
            proposals: Arc::new(RwLock::new(HashMap::new())),
            votes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn add_proposal(&self, proposal: ConsensusProposal) {
        let mut proposals = self.proposals.write().unwrap();
        proposals.insert(proposal.proposal_id, proposal.clone());
        self.votes.write().unwrap().insert(proposal.proposal_id, 0);
        info!("Added new proposal: {:?}", proposal);
    }

    pub fn cast_vote(&self, proposal_id: Uuid) {
        let mut votes = self.votes.write().unwrap();
        if let Some(vote_count) = votes.get_mut(&proposal_id) {
            *vote_count += 1;
            info!("Cast vote for proposal: {}. Current votes: {}", proposal_id, *vote_count);
        } else {
            error!("Proposal not found: {}", proposal_id);
        }
    }

    pub fn has_consensus(&self, proposal_id: Uuid, threshold: usize) -> bool {
        let votes = self.votes.read().unwrap();
        if let Some(&vote_count) = votes.get(&proposal_id) {
            vote_count >= threshold
        } else {
            false
        }
    }
}

pub async fn run_consensus_protocol(
    consensus_state: ConsensusState,
    mut proposal_rx: mpsc::Receiver<ConsensusProposal>,
    mut vote_rx: broadcast::Receiver<Uuid>,
    consensus_threshold: usize,
) -> Result<(), Box<dyn Error>> {
    loop {
        tokio::select! {
            Some(proposal) = proposal_rx.recv() => {
                consensus_state.add_proposal(proposal);
            }
            Ok(proposal_id) = vote_rx.recv() => {
                consensus_state.cast_vote(proposal_id);
                if consensus_state.has_consensus(proposal_id, consensus_threshold) {
                    info!("Consensus reached for proposal: {}", proposal_id);
                    // Handle the proposal that reached consensus (e.g., apply an update to the database)
                }
            }
            Err(e) => {
                error!("Failed to receive proposal or vote: {:?}", e);
            }
        }
    }
}
