// consensus.rs: Implements a consensus algorithm to ensure consistency across An nodes.

use crate::logging_metrics;
use serde::{Deserialize, Serialize};
use sled::Db;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusProposal {
    pub proposal_id: Uuid,
    pub content: String,
    pub proposer_id: Uuid,
}

pub type NodeId = u64;

/// Tracks proposals, vote counts, and committed proposals in memory.
#[derive(Default)]
struct ConsensusState {
    proposals: RwLock<HashMap<Uuid, ConsensusProposal>>,
    votes: RwLock<HashMap<Uuid, usize>>,
    committed: RwLock<HashSet<Uuid>>,
}

impl ConsensusState {
    fn new() -> Self {
        Self::default()
    }

    async fn add_proposal(&self, proposal: ConsensusProposal) {
        self.votes
            .write()
            .await
            .entry(proposal.proposal_id)
            .or_insert(0);
        self.proposals
            .write()
            .await
            .insert(proposal.proposal_id, proposal.clone());
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

    async fn proposal(&self, proposal_id: Uuid) -> Option<ConsensusProposal> {
        self.proposals.read().await.get(&proposal_id).cloned()
    }

    async fn mark_committed(&self, proposal_id: Uuid) -> bool {
        self.committed.write().await.insert(proposal_id)
    }
}

async fn commit_proposal(
    state: &ConsensusState,
    db: &Db,
    commit_tx: &broadcast::Sender<String>,
    proposal_id: Uuid,
) -> Result<(), Box<dyn Error>> {
    let Some(proposal) = state.proposal(proposal_id).await else {
        error!("Cannot commit unknown proposal: {}", proposal_id);
        return Ok(());
    };

    if !state.mark_committed(proposal_id).await {
        info!("Proposal {} was already committed", proposal_id);
        return Ok(());
    }

    db.insert(proposal_id.as_bytes(), serde_json::to_vec(&proposal)?)?;
    db.flush_async().await?;
    let _ = commit_tx.send(proposal.content.clone());
    logging_metrics::set_consensus_state(1.0);
    info!("Committed proposal {}", proposal_id);
    Ok(())
}

pub async fn run_consensus_protocol(
    node_id: NodeId,
    mut proposal_rx: mpsc::Receiver<ConsensusProposal>,
    mut vote_rx: broadcast::Receiver<Uuid>,
    commit_tx: broadcast::Sender<String>,
    consensus_threshold: usize,
) -> Result<(), Box<dyn Error>> {
    let consensus_state = ConsensusState::new();
    let db: Db = sled::open(format!("consensus-{}", node_id))?;
    let threshold = consensus_threshold.max(1);

    loop {
        tokio::select! {
            Some(proposal) = proposal_rx.recv() => {
                let proposal_id = proposal.proposal_id;
                consensus_state.add_proposal(proposal).await;

                if threshold == 1 {
                    commit_proposal(&consensus_state, &db, &commit_tx, proposal_id).await?;
                }
            }
            vote_result = vote_rx.recv() => {
                match vote_result {
                    Ok(proposal_id) => {
                        consensus_state.cast_vote(proposal_id).await;
                        if consensus_state.has_consensus(proposal_id, threshold).await {
                            info!("Consensus reached for proposal: {}", proposal_id);
                            commit_proposal(&consensus_state, &db, &commit_tx, proposal_id).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        error!("Missed {} consensus vote messages", skipped);
                    }
                }
            }
            else => break,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(content: &str) -> ConsensusProposal {
        ConsensusProposal {
            proposal_id: Uuid::new_v4(),
            content: content.to_string(),
            proposer_id: Uuid::new_v4(),
        }
    }

    #[tokio::test]
    async fn add_proposal_records_it_with_zero_votes() {
        let state = ConsensusState::new();
        let p = proposal("hello");
        let id = p.proposal_id;
        state.add_proposal(p).await;

        assert!(state.proposal(id).await.is_some());
        assert!(!state.has_consensus(id, 1).await);
    }

    #[tokio::test]
    async fn votes_accumulate_until_threshold_is_met() {
        let state = ConsensusState::new();
        let p = proposal("c");
        let id = p.proposal_id;
        state.add_proposal(p).await;

        assert!(!state.has_consensus(id, 2).await);
        state.cast_vote(id).await;
        assert!(!state.has_consensus(id, 2).await);
        state.cast_vote(id).await;
        assert!(state.has_consensus(id, 2).await);
    }

    #[tokio::test]
    async fn voting_for_unknown_proposal_is_a_noop() {
        let state = ConsensusState::new();
        let id = Uuid::new_v4();
        // Must not panic and must not create a phantom tally.
        state.cast_vote(id).await;
        assert!(!state.has_consensus(id, 1).await);
    }

    #[tokio::test]
    async fn mark_committed_is_idempotent() {
        let state = ConsensusState::new();
        let id = Uuid::new_v4();
        assert!(state.mark_committed(id).await);
        assert!(!state.mark_committed(id).await);
    }

    #[tokio::test]
    async fn commit_proposal_persists_once_and_broadcasts_content() {
        let state = ConsensusState::new();
        let dir = std::env::temp_dir().join(format!("an-ki-consensus-test-{}", Uuid::new_v4()));
        let db: Db = sled::open(&dir).unwrap();
        let (commit_tx, mut commit_rx) = broadcast::channel::<String>(4);

        let p = proposal("commit-me");
        let id = p.proposal_id;
        state.add_proposal(p).await;

        commit_proposal(&state, &db, &commit_tx, id).await.unwrap();
        assert_eq!(commit_rx.recv().await.unwrap(), "commit-me");
        assert!(db.contains_key(id.as_bytes()).unwrap());

        // A second commit of the same proposal is a no-op and must not re-broadcast.
        commit_proposal(&state, &db, &commit_tx, id).await.unwrap();
        assert!(commit_rx.try_recv().is_err());

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_proposal_ignores_unknown_proposal() {
        let state = ConsensusState::new();
        let dir = std::env::temp_dir().join(format!("an-ki-consensus-test-{}", Uuid::new_v4()));
        let db: Db = sled::open(&dir).unwrap();
        let (commit_tx, _commit_rx) = broadcast::channel::<String>(4);

        let unknown = Uuid::new_v4();
        commit_proposal(&state, &db, &commit_tx, unknown)
            .await
            .unwrap();
        assert!(!db.contains_key(unknown.as_bytes()).unwrap());

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
