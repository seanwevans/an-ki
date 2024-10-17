// election.rs: Implements leader election for An nodes to ensure redundancy and high availability.

use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use tokio::time::{self, Duration};
use uuid::Uuid;
use tracing::{info, error};
use std::error::Error;

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

    pub fn start_election(&self) {
        let mut node_status = self.node_status.write().unwrap();
        node_status.is_candidate = true;
        info!("Node {} is starting an election as a candidate.", node_status.node_id);
    }

    pub fn set_leader(&self, leader_id: Uuid) {
        let mut current_leader = self.current_leader.write().unwrap();
        *current_leader = Some(leader_id);
        let mut node_status = self.node_status.write().unwrap();
        node_status.is_leader = node_status.node_id == leader_id;
        node_status.is_candidate = false;
        info!("Node {} is now the leader: {}", node_status.node_id, node_status.is_leader);
    }
}

pub async fn run_leader_election(election: Election, mut rx: broadcast::Receiver<NodeStatus>, election_interval: Duration) -> Result<(), Box<dyn Error>> {
    let mut ticker = time::interval(election_interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let current_leader = election.current_leader.read().unwrap();
                if current_leader.is_none() {
                    info!("No current leader. Initiating a new election.");
                    election.start_election();
                    // Broadcast the candidacy and handle the election logic (e.g., majority vote)
                }
            }
            Ok(node_status) = rx.recv() => {
                if node_status.is_leader {
                    election.set_leader(node_status.node_id);
                }
            }
            Err(e) => {
                error!("Failed to receive node status: {:?}", e);
            }
        }
    }
}
