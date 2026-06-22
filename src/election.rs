// election.rs: Implements leader election for An nodes to ensure redundancy and high availability.

use std::error::Error;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{self, Duration};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct NodeStatus {
    pub node_id: Uuid,
    pub is_leader: bool,
    pub is_candidate: bool,
}

#[derive(Clone)]
pub struct Election {
    pub current_leader: Arc<RwLock<Option<Uuid>>>,
    pub node_status: Arc<RwLock<NodeStatus>>,
}

impl Election {
    pub fn new(node_id: Uuid) -> Self {
        Election {
            current_leader: Arc::new(RwLock::new(None)),
            node_status: Arc::new(RwLock::new(NodeStatus {
                node_id,
                is_leader: false,
                is_candidate: false,
            })),
        }
    }

    pub async fn start_election(&self) {
        let mut node_status = self.node_status.write().await;
        node_status.is_candidate = true;
        info!(
            "Node {} is starting an election as a candidate.",
            node_status.node_id
        );
    }

    pub async fn set_leader(&self, leader_id: Uuid) {
        let mut current_leader = self.current_leader.write().await;
        *current_leader = Some(leader_id);
        let mut node_status = self.node_status.write().await;
        node_status.is_leader = node_status.node_id == leader_id;
        node_status.is_candidate = false;
        info!(
            "Node {} is now the leader: {}",
            node_status.node_id, node_status.is_leader
        );
    }
}

pub async fn run_leader_election(
    election: Election,
    mut rx: broadcast::Receiver<NodeStatus>,
    election_interval: Duration,
) -> Result<(), Box<dyn Error>> {
    let mut ticker = time::interval(election_interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let current_leader = election.current_leader.read().await;
                if current_leader.is_none() {
                    info!("No current leader. Initiating a new election.");
                    election.start_election().await;
                    // Broadcast the candidacy and handle the election logic (e.g., majority vote)
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(node_status) => {
                        if node_status.is_leader {
                            election.set_leader(node_status.node_id).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Leader election channel closed");
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        error!("Missed {} leader election messages", skipped);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn new_election_starts_with_no_leader() {
        let node_id = Uuid::new_v4();
        let election = Election::new(node_id);

        assert!(election.current_leader.read().await.is_none());
        let status = election.node_status.read().await;
        assert_eq!(status.node_id, node_id);
        assert!(!status.is_leader);
        assert!(!status.is_candidate);
    }

    #[tokio::test]
    async fn start_election_marks_node_as_candidate() {
        let election = Election::new(Uuid::new_v4());
        election.start_election().await;
        assert!(election.node_status.read().await.is_candidate);
    }

    #[tokio::test]
    async fn set_leader_promotes_self_and_clears_candidacy() {
        let node_id = Uuid::new_v4();
        let election = Election::new(node_id);
        election.start_election().await;

        election.set_leader(node_id).await;

        assert_eq!(*election.current_leader.read().await, Some(node_id));
        let status = election.node_status.read().await;
        assert!(status.is_leader);
        assert!(!status.is_candidate);
    }

    #[tokio::test]
    async fn set_leader_for_other_node_remains_follower() {
        let node_id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let election = Election::new(node_id);

        election.set_leader(other).await;

        assert_eq!(*election.current_leader.read().await, Some(other));
        assert!(!election.node_status.read().await.is_leader);
    }

    #[tokio::test]
    async fn run_leader_election_returns_when_channel_closes() {
        let election = Election::new(Uuid::new_v4());
        let (tx, rx) = broadcast::channel(1);
        drop(tx); // Closing the sender should terminate the loop cleanly.

        let result = run_leader_election(election, rx, Duration::from_secs(60)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_leader_election_adopts_announced_leader() {
        let leader_id = Uuid::new_v4();
        let election = Election::new(leader_id);
        let (tx, rx) = broadcast::channel(4);
        tx.send(NodeStatus {
            node_id: leader_id,
            is_leader: true,
            is_candidate: false,
        })
        .unwrap();
        drop(tx);

        run_leader_election(election.clone(), rx, Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(*election.current_leader.read().await, Some(leader_id));
    }
}
