// principal.rs: Implements the specific responsibilities of the Principal, including role management and global coordination.

use crate::common::NodeRole;
use crate::health;
use crate::messaging::{consume_messages, declare_queue};
use crate::node_registry::{self, NodeRegistry};
use crate::raft_network;
use crate::raft_node::{self, AnKiRaft};
use crate::raft_store::ClusterRequest;
use crate::signals;

use crate::config::load_settings;
use futures_util::stream::StreamExt;
use lapin::{
    message::Delivery,
    options::{BasicAckOptions, BasicNackOptions},
    Connection, ConnectionProperties,
};
use openraft::error::{ClientWriteError, RaftError};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// A change to cluster-wide state, requested over `principal_update_queue`.
///
/// Each variant maps onto a [`ClusterRequest`] so that accepting one commits it
/// to the replicated log rather than mutating local state.
///
/// There is deliberately no variant for executing SQL. The previous
/// `Database { statement }` variant would have run an arbitrary statement
/// supplied by anyone able to publish to the queue, which is a
/// remote-code-execution path into the database dressed up as a coordination
/// message. Schema changes belong in `migrations/`, and data changes belong
/// behind the authenticated task API.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", content = "data")]
enum UpdateContent {
    /// Assign `role` to `node_id`.
    AssignRole { node_id: String, role: NodeRole },
    /// Remove any role assigned to `node_id`.
    ClearRole { node_id: String },
    /// Set a cluster-wide configuration value.
    SetConfig { key: String, value: String },
}

#[derive(Serialize, Deserialize, Debug)]
struct UpdateRequest {
    update_id: String,
    content: UpdateContent,
}

fn decode_update_request(payload: &[u8]) -> Option<UpdateRequest> {
    match serde_json::from_slice::<UpdateRequest>(payload) {
        Ok(update_request) => Some(update_request),
        Err(e) => {
            error!(
                "Failed to deserialize update request payload. error={:?}, payload={}",
                e,
                String::from_utf8_lossy(payload)
            );
            None
        }
    }
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if let Err(e) = signals::setup_unix_signal_handlers().await {
        error!("Failed to set up Unix signal handlers: {:?}", e);
    }

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(e) = signals::setup_signal_handler().await {
            error!("Signal handler error: {:?}", e);
        }
        let _ = shutdown_tx.send(());
    });

    // Establish connection to RabbitMQ
    // Load configuration and establish connection to RabbitMQ
    let settings = load_settings().map_err(|e| {
        error!("Failed to load settings: {:?}", e);
        e
    })?;

    let amqp_addr = std::env::var("AMQP_ADDR").unwrap_or(settings.amqp_addr.clone());

    let connection = Connection::connect(&amqp_addr, ConnectionProperties::default())
        .await
        .map_err(|e| {
            error!("Failed to connect to RabbitMQ: {:?}", e);
            e
        })?;
    let channel = connection.create_channel().await.map_err(|e| {
        error!("Failed to create channel: {:?}", e);
        e
    })?;

    // Declare the queue for receiving update requests from An nodes
    let queue_name = "principal_update_queue";
    declare_queue(&channel, queue_name)
        .await
        .map_err(Box::<dyn Error>::from)?;

    // Start consuming update requests from the queue
    let mut consumer = consume_messages(&channel, queue_name, "principal_consumer")
        .await
        .map_err(Box::<dyn Error>::from)?;

    // Monitor cluster health by consuming node heartbeats on a dedicated channel.
    // The same heartbeats populate the node registry, which is the principal's
    // live view of who is in the cluster and what role each node serves.
    let registry = NodeRegistry::new();
    let health_cancel = CancellationToken::new();
    match connection.create_channel().await {
        Ok(health_channel) => {
            let threshold = health::unhealthy_threshold();
            let ttl = node_registry::node_ttl();
            let token = health_cancel.clone();
            let registry = registry.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    health::run_health_monitor(health_channel, registry, threshold, ttl, token)
                        .await
                {
                    error!("Health monitor exited with error: {:?}", e);
                }
            });
        }
        Err(e) => error!("Failed to open health-monitor channel: {:?}", e),
    }

    // Start the Raft consensus node over its durable store, then serve the RPC
    // endpoints its peers dial. Cluster shape comes from RAFT_PEERS; unset means
    // a single-member cluster.
    let raft_cancel = CancellationToken::new();
    let raft = match raft_node::start_from_env().await {
        Ok(node) => {
            info!(
                "Raft consensus node started (id={}, data_dir={:?})",
                node.node_id,
                raft_node::data_dir_from_env()
            );

            match raft_network::bind_addr_from_env() {
                Ok(rpc_addr) => {
                    let handle = node.raft.clone();
                    let token = raft_cancel.clone();
                    tokio::spawn(async move {
                        raft_network::serve_raft_rpc(handle, rpc_addr, async move {
                            token.cancelled().await
                        })
                        .await;
                    });
                }
                // Without the RPC server this node can call peers but they
                // cannot call it, so it would never receive entries or votes.
                Err(e) => error!(
                    "Invalid RAFT_ADDR; peers will not be able to reach this node: {:?}",
                    e
                ),
            }

            // Report real leadership on the `consensus_state` gauge.
            raft_node::spawn_leadership_reporter(&node, raft_cancel.clone());

            Some(node)
        }
        Err(e) => {
            error!("Failed to start Raft consensus node: {:?}", e);
            None
        }
    };

    info!("Principal node is running and waiting for update requests...");

    let mut cluster_report = tokio::time::interval(node_registry::cluster_report_interval());

    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received, stopping Principal node...");
                break;
            }
            _ = cluster_report.tick() => {
                log_cluster_view(&registry).await;
            }
            delivery_result = consumer.next() => {
                match delivery_result {
                    Some(Ok(delivery)) => {
                        let Some(update_request) = decode_update_request(&delivery.data) else {
                            // Queue policy: negatively acknowledge malformed payloads without
                            // requeueing so the broker can dead-letter or drop poison messages.
                            nack_for_dead_letter(&delivery).await?;
                            continue;
                        };

                        info!("Received update request: {:?}", update_request);

                        let raft_handle = raft.as_ref().map(|node| &node.raft);
                        match process_update_request(update_request, raft_handle).await {
                            UpdateOutcome::Applied => {
                                delivery
                                    .ack(BasicAckOptions::default())
                                    .await
                                    .map_err(|e| {
                                        error!("Failed to acknowledge successful update: {:?}", e);
                                        e
                                    })?;
                            }
                            UpdateOutcome::Rejected(reason) => {
                                error!("Rejecting update request: {}", reason);
                                nack_for_dead_letter(&delivery).await?;
                            }
                            UpdateOutcome::Retry(reason) => {
                                // Requeue rather than dead-letter: the request is
                                // valid and the leader can still serve it.
                                warn!("Requeueing update request: {}", reason);
                                settle(&delivery, true).await?;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!("Failed to receive delivery (unrecoverable): {:?}", e);
                        return Err(Box::new(e));
                    }
                    None => break,
                }
            }
        }
    }

    // Stop the health monitor and Raft node before tearing down the connection.
    health_cancel.cancel();
    raft_cancel.cancel();
    if let Some(node) = raft {
        if let Err(e) = node.shutdown().await {
            error!("Failed to shut down Raft node: {:?}", e);
        }
    }

    if let Err(e) = channel.close(200, "Bye").await {
        error!("Failed to close channel: {:?}", e);
    }
    if let Err(e) = connection.close(200, "Bye").await {
        error!("Failed to close connection: {:?}", e);
    }

    Ok(())
}

/// Logs the current cluster composition so operators can see which nodes the
/// principal believes are alive without attaching a debugger.
async fn log_cluster_view(registry: &NodeRegistry) {
    let nodes = registry.list_nodes().await;
    if nodes.is_empty() {
        info!("Cluster view: no nodes have heartbeated yet");
        return;
    }

    let mut an = 0usize;
    let mut ki = 0usize;
    let mut principal = 0usize;
    for node in &nodes {
        match node.role {
            NodeRole::An => an += 1,
            NodeRole::Ki => ki += 1,
            NodeRole::Principal => principal += 1,
        }
    }
    info!(
        "Cluster view: {} node(s) alive ({} an, {} ki, {} principal)",
        nodes.len(),
        an,
        ki,
        principal
    );
}

/// What the principal decided to do with an update request, which determines
/// how the delivery is settled.
#[derive(Debug, PartialEq, Eq)]
enum UpdateOutcome {
    /// Committed through Raft. Acknowledge the delivery.
    Applied,
    /// The request itself is bad and will never succeed. Dead-letter it rather
    /// than requeueing a message that can only fail again.
    Rejected(String),
    /// The request is well-formed but this node cannot serve it right now,
    /// typically because it is not the Raft leader. Requeue so another
    /// principal — or this one, once it catches up — can take it.
    Retry(String),
}

/// Applies an update request by committing it to the Raft log.
///
/// Every accepted request becomes a [`ClusterRequest`], so the decision is
/// replicated to every principal instead of living in this process's memory.
/// Only the leader may write, which is why a follower answers [`UpdateOutcome::Retry`]
/// rather than applying anything locally.
async fn process_update_request(update: UpdateRequest, raft: Option<&AnKiRaft>) -> UpdateOutcome {
    info!("Processing update request with ID: {}", update.update_id);

    let request = match to_cluster_request(update.content) {
        Ok(request) => request,
        Err(reason) => return UpdateOutcome::Rejected(reason),
    };

    let Some(raft) = raft else {
        return UpdateOutcome::Retry(
            "Raft node is not running; cannot commit cluster updates".to_string(),
        );
    };

    match raft.client_write(request).await {
        Ok(response) => {
            info!(
                "Committed update {} at log index {}",
                update.update_id, response.log_id.index
            );
            UpdateOutcome::Applied
        }
        // Writes only succeed on the leader. This is the ordinary state of
        // affairs for a follower, not a failure of the request.
        Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
            UpdateOutcome::Retry(format!(
                "not the Raft leader (leader is {:?}); requeueing update {}",
                forward.leader_id, update.update_id
            ))
        }
        Err(e) => UpdateOutcome::Retry(format!("Raft write failed: {e}")),
    }
}

/// Validates an update request and translates it into the replicated
/// [`ClusterRequest`] vocabulary.
fn to_cluster_request(content: UpdateContent) -> Result<ClusterRequest, String> {
    match content {
        UpdateContent::AssignRole { node_id, role } => {
            let node_id = non_empty(node_id, "node_id")?;
            Ok(ClusterRequest::AssignRole { node_id, role })
        }
        UpdateContent::ClearRole { node_id } => {
            let node_id = non_empty(node_id, "node_id")?;
            Ok(ClusterRequest::ClearRole { node_id })
        }
        UpdateContent::SetConfig { key, value } => {
            let key = non_empty(key, "key")?;
            Ok(ClusterRequest::SetConfig { key, value })
        }
    }
}

/// Trims `value` and rejects it if nothing is left, so blank identifiers never
/// reach the replicated log.
fn non_empty(value: String, field: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Assigns `role` to `node_id` by committing it to the Raft log.
///
/// This is the programmatic entry point behind
/// [`UpdateContent::AssignRole`]; the assignment is readable afterwards from
/// any principal via [`SledStore::role_of`](crate::raft_store::SledStore::role_of).
pub async fn assign_role(
    raft: &AnKiRaft,
    node_id: &str,
    role: NodeRole,
) -> Result<Option<NodeRole>, Box<dyn Error>> {
    let node_id = non_empty(node_id.to_string(), "node_id")?;
    let response = raft
        .client_write(ClusterRequest::AssignRole {
            node_id: node_id.clone(),
            role: role.clone(),
        })
        .await?;
    info!("Assigned role {:?} to node {}", role, node_id);
    Ok(response.data.previous_role)
}

async fn nack_for_dead_letter(delivery: &Delivery) -> Result<(), lapin::Error> {
    settle(delivery, false).await
}

/// Negatively acknowledges a delivery, requeueing it only when retrying could
/// plausibly succeed.
async fn settle(delivery: &Delivery, requeue: bool) -> Result<(), lapin::Error> {
    delivery
        .nack(BasicNackOptions {
            multiple: false,
            requeue,
        })
        .await
        .map_err(|e| {
            error!("Failed to negatively acknowledge update: {:?}", e);
            e
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft_store::SledStore;
    use openraft::ServerState;
    use std::time::Duration;

    fn request(content: UpdateContent) -> UpdateRequest {
        UpdateRequest {
            update_id: "update-1".to_string(),
            content,
        }
    }

    /// A leader that accepts writes, for the happy-path cases.
    async fn leader() -> crate::raft_node::RaftNode {
        let store = SledStore::temporary().expect("temporary store");
        let node = raft_node::start_single_node(1, store)
            .await
            .expect("start raft");
        node.raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "leader")
            .await
            .expect("became leader");
        node
    }

    #[tokio::test]
    async fn assigning_a_role_commits_it_to_the_replicated_state() {
        let node = leader().await;

        let outcome = process_update_request(
            request(UpdateContent::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            }),
            Some(&node.raft),
        )
        .await;

        assert_eq!(outcome, UpdateOutcome::Applied);
        assert_eq!(
            node.store.role_of("ki-0").await,
            Some(NodeRole::Ki),
            "the assignment must be readable from the replicated state, not just logged"
        );

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn clearing_a_role_removes_it_from_the_replicated_state() {
        let node = leader().await;
        process_update_request(
            request(UpdateContent::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            }),
            Some(&node.raft),
        )
        .await;

        let outcome = process_update_request(
            request(UpdateContent::ClearRole {
                node_id: "ki-0".to_string(),
            }),
            Some(&node.raft),
        )
        .await;

        assert_eq!(outcome, UpdateOutcome::Applied);
        assert_eq!(node.store.role_of("ki-0").await, None);

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn setting_config_commits_it_to_the_replicated_state() {
        let node = leader().await;

        let outcome = process_update_request(
            request(UpdateContent::SetConfig {
                key: "model_shards".to_string(),
                value: "4".to_string(),
            }),
            Some(&node.raft),
        )
        .await;

        assert_eq!(outcome, UpdateOutcome::Applied);
        assert_eq!(
            node.store.config_value("model_shards").await,
            Some("4".to_string())
        );

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn assign_role_reports_the_role_it_replaced() {
        let node = leader().await;

        let first = assign_role(&node.raft, "node-0", NodeRole::Ki)
            .await
            .expect("first assignment");
        let second = assign_role(&node.raft, "node-0", NodeRole::An)
            .await
            .expect("second assignment");

        assert_eq!(first, None);
        assert_eq!(second, Some(NodeRole::Ki));
        assert_eq!(node.store.role_of("node-0").await, Some(NodeRole::An));

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn blank_identifiers_are_rejected_rather_than_replicated() {
        let node = leader().await;

        for content in [
            UpdateContent::AssignRole {
                node_id: "   ".to_string(),
                role: NodeRole::Ki,
            },
            UpdateContent::ClearRole {
                node_id: String::new(),
            },
            UpdateContent::SetConfig {
                key: " ".to_string(),
                value: "4".to_string(),
            },
        ] {
            let outcome = process_update_request(request(content), Some(&node.raft)).await;
            assert!(
                matches!(outcome, UpdateOutcome::Rejected(_)),
                "got {outcome:?}"
            );
        }

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn identifiers_are_trimmed_before_being_replicated() {
        let node = leader().await;

        process_update_request(
            request(UpdateContent::AssignRole {
                node_id: "  ki-0  ".to_string(),
                role: NodeRole::Ki,
            }),
            Some(&node.raft),
        )
        .await;

        assert_eq!(node.store.role_of("ki-0").await, Some(NodeRole::Ki));
        assert_eq!(node.store.role_of("  ki-0  ").await, None);

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn a_follower_requeues_rather_than_applying_locally() {
        // A node that never initialized has no leader, so it cannot write. It
        // must not apply anything locally — that would diverge from the cluster.
        let store = SledStore::temporary().expect("temporary store");
        let node = raft_node::build_node(2, store).await.expect("build raft");

        let outcome = process_update_request(
            request(UpdateContent::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            }),
            Some(&node.raft),
        )
        .await;

        assert!(
            matches!(outcome, UpdateOutcome::Retry(_)),
            "a non-leader must requeue, got {outcome:?}"
        );
        assert_eq!(node.store.role_of("ki-0").await, None);

        node.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn a_missing_raft_node_requeues_rather_than_dead_lettering() {
        let outcome = process_update_request(
            request(UpdateContent::SetConfig {
                key: "model_shards".to_string(),
                value: "4".to_string(),
            }),
            None,
        )
        .await;

        assert!(
            matches!(outcome, UpdateOutcome::Retry(_)),
            "a valid request must survive this node failing to start Raft, got {outcome:?}"
        );
    }

    #[test]
    fn update_requests_decode_from_their_wire_format() {
        let payload = br#"{"update_id":"1","content":{"type":"assign_role","data":{"node_id":"ki-0","role":"Ki"}}}"#;

        let decoded = decode_update_request(payload).expect("decode");

        assert_eq!(decoded.update_id, "1");
        assert_eq!(
            decoded.content,
            UpdateContent::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            }
        );
    }

    #[test]
    fn sql_statements_are_no_longer_a_valid_update() {
        // The `database` variant used to accept an arbitrary SQL statement from
        // anyone able to publish to the queue. It must not decode any more.
        let payload =
            br#"{"update_id":"1","content":{"type":"database","data":{"statement":"DROP TABLE tasks"}}}"#;

        assert!(decode_update_request(payload).is_none());
    }

    #[test]
    fn decode_update_request_invalid_payload_does_not_block_following_payload() {
        let invalid_payload = br#"{"update_id": "broken", "content": {"type":"clear_role"#;
        let valid_payload =
            br#"{"update_id":"5","content":{"type":"clear_role","data":{"node_id":"ki-0"}}}"#;

        assert!(
            decode_update_request(invalid_payload).is_none(),
            "invalid payload should be dropped"
        );
        assert!(
            decode_update_request(valid_payload).is_some(),
            "subsequent valid payload should still be decodable"
        );
    }
}
