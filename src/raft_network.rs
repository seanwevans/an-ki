//! HTTP transport carrying Raft RPCs between principals.
//!
//! Each principal serves three endpoints — `/raft/append-entries`, `/raft/vote`
//! and `/raft/install-snapshot` — and dials its peers at the same paths. The
//! address a peer is reached on comes from its [`BasicNode::addr`], which is
//! recorded in the cluster membership, so the transport needs no configuration
//! of its own beyond the peer list used to bootstrap.
//!
//! Errors are deliberately split. A failure to reach the peer at all becomes
//! [`RPCError::Unreachable`], which tells openraft to back off before retrying.
//! An error the peer itself reported is carried through as a
//! [`RemoteError`] so the caller sees the remote's actual [`RaftError`] rather
//! than a generic transport failure.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use hyper::{Body, Client, Method, Request};
use openraft::error::{
    InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable,
};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, RaftNetwork, RaftNetworkFactory};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::{debug, error, info};
use warp::Filter;

use crate::raft_node::AnKiRaft;
use crate::raft_store::{NodeId, TypeConfig};

pub const PATH_APPEND_ENTRIES: &str = "append-entries";
pub const PATH_VOTE: &str = "vote";
pub const PATH_INSTALL_SNAPSHOT: &str = "install-snapshot";

/// Default address the Raft RPC server binds to.
pub const DEFAULT_RAFT_ADDR: &str = "0.0.0.0:4001";

/// Address this node's Raft RPC server binds to, from `RAFT_ADDR`.
pub fn bind_addr_from_env() -> Result<SocketAddr, std::net::AddrParseError> {
    std::env::var("RAFT_ADDR")
        .unwrap_or_else(|_| DEFAULT_RAFT_ADDR.to_string())
        .parse()
}

/// How long a single Raft RPC may take before the peer is treated as
/// unreachable, from `RAFT_RPC_TIMEOUT_MS` (default 2s). Kept well under
/// openraft's election timeout so a slow peer does not stall an election.
pub fn rpc_timeout_from_env() -> Duration {
    let ms = std::env::var("RAFT_RPC_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(2_000);
    Duration::from_millis(ms)
}

/// Parses the `RAFT_PEERS` cluster description: a comma-separated list of
/// `id=host:port` entries naming every principal in the cluster, including this
/// one. Returns an empty map when unset, which means "run as a single node".
///
/// ```text
/// RAFT_PEERS=1=principal-0.principal-headless:4001,2=principal-1.principal-headless:4001
/// ```
pub fn parse_peers(raw: &str) -> Result<BTreeMap<NodeId, BasicNode>, PeerParseError> {
    let mut peers = BTreeMap::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (id, addr) = entry
            .split_once('=')
            .ok_or_else(|| PeerParseError::new(entry, "expected `id=host:port`"))?;
        let id: NodeId = id
            .trim()
            .parse()
            .map_err(|_| PeerParseError::new(entry, "node id must be a number"))?;
        let addr = addr.trim();
        if addr.is_empty() {
            return Err(PeerParseError::new(entry, "address must not be empty"));
        }
        if peers.insert(id, BasicNode::new(addr)).is_some() {
            return Err(PeerParseError::new(entry, "duplicate node id"));
        }
    }
    Ok(peers)
}

/// Reads and parses `RAFT_PEERS`.
pub fn peers_from_env() -> Result<BTreeMap<NodeId, BasicNode>, PeerParseError> {
    match std::env::var("RAFT_PEERS") {
        Ok(raw) => parse_peers(&raw),
        Err(_) => Ok(BTreeMap::new()),
    }
}

/// A malformed entry in `RAFT_PEERS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerParseError {
    entry: String,
    reason: &'static str,
}

impl PeerParseError {
    fn new(entry: &str, reason: &'static str) -> Self {
        Self {
            entry: entry.to_string(),
            reason,
        }
    }
}

impl fmt::Display for PeerParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid RAFT_PEERS entry {:?}: {}",
            self.entry, self.reason
        )
    }
}

impl StdError for PeerParseError {}

/// Builds [`HttpNetwork`] clients addressed by each peer's [`BasicNode::addr`].
#[derive(Clone)]
pub struct HttpNetworkFactory {
    timeout: Duration,
}

impl HttpNetworkFactory {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for HttpNetworkFactory {
    fn default() -> Self {
        Self::new(rpc_timeout_from_env())
    }
}

impl RaftNetworkFactory<TypeConfig> for HttpNetworkFactory {
    type Network = HttpNetwork;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        HttpNetwork {
            target,
            addr: node.addr.clone(),
            // hyper's Client pools connections internally, so one per peer
            // keeps a warm connection for the steady stream of heartbeats.
            client: Client::new(),
            timeout: self.timeout,
        }
    }
}

/// An RPC client for one peer.
pub struct HttpNetwork {
    target: NodeId,
    addr: String,
    client: Client<hyper::client::HttpConnector, Body>,
    timeout: Duration,
}

impl HttpNetwork {
    /// POSTs `request` as JSON and decodes the peer's `Result` reply.
    ///
    /// The reply body is the peer's own `Result<Resp, RaftError>`, so a remote
    /// failure arrives here as data rather than as an HTTP error status.
    async fn post<Req, Resp, E>(
        &self,
        path: &str,
        request: &Req,
    ) -> Result<Resp, RPCError<NodeId, BasicNode, RaftError<NodeId, E>>>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
        E: StdError + DeserializeOwned,
    {
        let uri = format!("http://{}/raft/{}", self.addr, path);
        let body = serde_json::to_vec(request).map_err(|e| NetworkError::new(&e))?;

        let http_request = Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .map_err(|e| NetworkError::new(&e))?;

        // A peer that cannot be dialled or does not answer in time is
        // `Unreachable`, not `Network`: openraft backs off on the former and
        // retries the latter immediately, and hammering a down peer is waste.
        let response = tokio::time::timeout(self.timeout, self.client.request(http_request))
            .await
            .map_err(|_| {
                debug!("Raft RPC to {} ({}) timed out", self.target, uri);
                Unreachable::new(&TimedOut(self.timeout))
            })?
            .map_err(|e| {
                debug!("Raft RPC to {} ({}) failed: {}", self.target, uri, e);
                Unreachable::new(&e)
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(Unreachable::new(&UnexpectedStatus(status)).into());
        }

        let bytes = hyper::body::to_bytes(response.into_body())
            .await
            .map_err(|e| NetworkError::new(&e))?;
        let decoded: Result<Resp, RaftError<NodeId, E>> =
            serde_json::from_slice(&bytes).map_err(|e| NetworkError::new(&e))?;

        decoded.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}

/// Reported when a peer does not answer within the RPC timeout.
#[derive(Debug)]
struct TimedOut(Duration);

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no response within {:?}", self.0)
    }
}

impl StdError for TimedOut {}

/// Reported when a peer answers with a non-success HTTP status, which means it
/// is not serving the Raft API we expect.
#[derive(Debug)]
struct UnexpectedStatus(hyper::StatusCode);

impl fmt::Display for UnexpectedStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer answered with HTTP {}", self.0)
    }
}

impl StdError for UnexpectedStatus {}

impl RaftNetwork<TypeConfig> for HttpNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.post(PATH_APPEND_ENTRIES, &rpc).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.post(PATH_VOTE, &rpc).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        self.post(PATH_INSTALL_SNAPSHOT, &rpc).await
    }
}

/// The warp filters serving this node's Raft RPC endpoints.
///
/// Each handler hands the decoded request straight to the local Raft instance
/// and serialises the resulting `Result` as the response body, so the caller
/// receives the same value a local call would have produced.
pub fn raft_rpc_filters(
    raft: AnKiRaft,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let append = {
        let raft = raft.clone();
        warp::path!("raft" / "append-entries")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |rpc: AppendEntriesRequest<TypeConfig>| {
                let raft = raft.clone();
                async move { reply_with(raft.append_entries(rpc).await) }
            })
    };

    let vote = {
        let raft = raft.clone();
        warp::path!("raft" / "vote")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |rpc: VoteRequest<NodeId>| {
                let raft = raft.clone();
                async move { reply_with(raft.vote(rpc).await) }
            })
    };

    let snapshot = {
        warp::path!("raft" / "install-snapshot")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(move |rpc: InstallSnapshotRequest<TypeConfig>| {
                let raft = raft.clone();
                async move { reply_with(raft.install_snapshot(rpc).await) }
            })
    };

    append.or(vote).unify().or(snapshot).unify()
}

/// Serialises a Raft `Result` into the response body.
///
/// Both arms are 200 responses: the `Result` itself is the payload, and the
/// client reconstructs it verbatim. Reserving non-success statuses for
/// transport-level problems keeps "the peer is unreachable" distinguishable
/// from "the peer answered, and its answer was an error".
fn reply_with<T, E>(result: Result<T, E>) -> Result<warp::reply::Response, warp::Rejection>
where
    T: Serialize,
    E: Serialize,
{
    let payload: Result<&T, &E> = match &result {
        Ok(value) => Ok(value),
        Err(e) => Err(e),
    };
    match serde_json::to_vec(&payload) {
        Ok(body) => Ok(warp::http::Response::builder()
            .status(warp::http::StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.into())
            .expect("a serialized body is always a valid response")),
        Err(e) => {
            error!("Failed to serialize Raft RPC reply: {:?}", e);
            Ok(warp::http::Response::builder()
                .status(warp::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("an empty body is always a valid response"))
        }
    }
}

/// Serves this node's Raft RPC endpoints on `addr` until `shutdown` resolves.
pub async fn serve_raft_rpc<F>(raft: AnKiRaft, addr: SocketAddr, shutdown: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let (bound, server) =
        warp::serve(raft_rpc_filters(raft)).bind_with_graceful_shutdown(addr, shutdown);
    info!("Raft RPC server listening on {}", bound);
    server.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_parse_into_addressed_nodes() {
        let peers = parse_peers("1=principal-0:4001,2=principal-1:4001").expect("parse");

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[&1].addr, "principal-0:4001");
        assert_eq!(peers[&2].addr, "principal-1:4001");
    }

    #[test]
    fn peers_tolerate_whitespace_and_trailing_commas() {
        let peers = parse_peers(" 1 = a:1 , 2 = b:2 , ").expect("parse");

        assert_eq!(peers.len(), 2);
        assert_eq!(peers[&1].addr, "a:1");
    }

    #[test]
    fn an_empty_peer_list_means_single_node() {
        assert!(parse_peers("").expect("parse").is_empty());
        assert!(parse_peers("   ").expect("parse").is_empty());
    }

    #[test]
    fn malformed_peers_are_rejected_rather_than_skipped() {
        // Silently dropping a bad entry would form a cluster with the wrong
        // membership, which is far worse than refusing to start.
        assert!(parse_peers("principal-0:4001").is_err(), "missing id");
        assert!(
            parse_peers("one=principal-0:4001").is_err(),
            "id not a number"
        );
        assert!(parse_peers("1=").is_err(), "empty address");
        assert!(parse_peers("1=a:1,1=b:2").is_err(), "duplicate id");
    }

    #[test]
    fn peer_parse_errors_name_the_offending_entry() {
        let err = parse_peers("1=a:1,oops").expect_err("should fail");
        assert!(err.to_string().contains("oops"), "got: {err}");
    }

    #[test]
    fn default_bind_address_is_parseable() {
        assert!(DEFAULT_RAFT_ADDR.parse::<SocketAddr>().is_ok());
    }

    /// Pins the contract between the Helm chart and this parser. The chart
    /// builds `RAFT_PEERS` from StatefulSet ordinals as `ordinal+1=pod-DNS`,
    /// matching the `RAFT_NODE_ID` each container derives from its hostname. If
    /// either side's numbering drifts, principals form the wrong cluster — so
    /// the exact string the chart emits is asserted here.
    #[test]
    fn the_peer_string_the_chart_emits_parses_as_expected() {
        let two_replicas = "1=principal-0.principal-headless:4001,\
                            2=principal-1.principal-headless:4001";
        let peers = parse_peers(two_replicas).expect("parse");
        assert_eq!(peers.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(peers[&1].addr, "principal-0.principal-headless:4001");
        assert_eq!(peers[&2].addr, "principal-1.principal-headless:4001");

        let three_replicas = "1=principal-0.principal-headless:4001,\
                              2=principal-1.principal-headless:4001,\
                              3=principal-2.principal-headless:4001";
        let peers = parse_peers(three_replicas).expect("parse");
        assert_eq!(peers.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
        // The bootstrapper is the lowest id, which is always ordinal 0.
        assert_eq!(
            peers.keys().next().copied(),
            Some(1),
            "pod ordinal 0 must be the bootstrapping node"
        );

        // A single replica must still name itself, so the node finds its own id.
        let one_replica = "1=principal-0.principal-headless:4001";
        assert_eq!(parse_peers(one_replica).expect("parse").len(), 1);
    }

    #[tokio::test]
    async fn unreachable_peers_report_unreachable_not_remote_error() {
        // Port 1 on loopback refuses connections, standing in for a peer that
        // is down. openraft backs off on Unreachable, so misclassifying this as
        // a remote error would turn a dead peer into a hot retry loop.
        let mut factory = HttpNetworkFactory::new(Duration::from_millis(200));
        let mut client = factory.new_client(2, &BasicNode::new("127.0.0.1:1")).await;

        let result = client
            .vote(
                VoteRequest::new(openraft::Vote::new(1, 1), None),
                RPCOption::new(Duration::from_millis(200)),
            )
            .await;

        match result {
            Err(RPCError::Unreachable(_)) => {}
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rpc_endpoints_answer_a_live_raft_node() {
        use crate::raft_node;
        use crate::raft_store::SledStore;
        use openraft::ServerState;

        let store = SledStore::temporary().expect("temporary store");
        let node = raft_node::start_single_node(1, store)
            .await
            .expect("start raft");
        node.raft
            .wait(Some(Duration::from_secs(10)))
            .state(ServerState::Leader, "leader")
            .await
            .expect("became leader");

        // Drive the filters directly rather than binding a port, so the test
        // exercises the routing and encoding without racing on a socket.
        let filters = raft_rpc_filters(node.raft.clone());
        let expected_last_log_index = node.raft.metrics().borrow().last_log_index;
        assert!(
            expected_last_log_index.is_some(),
            "the leader should have committed its initial entries"
        );

        let response = warp::test::request()
            .method("POST")
            .path("/raft/vote")
            .json(&VoteRequest::new(openraft::Vote::new(99, 2), None))
            .reply(&filters)
            .await;

        assert_eq!(response.status(), 200);
        let decoded: Result<VoteResponse<NodeId>, RaftError<NodeId>> =
            serde_json::from_slice(response.body()).expect("decode reply");
        let vote_response = decoded.expect("raft answered");

        // The assertion is about the transport, not about Raft's voting policy:
        // the reply must carry this node's real log position and its real
        // current vote, which a canned or stubbed responder could not produce.
        assert_eq!(
            vote_response.last_log_id.map(|id| id.index),
            expected_last_log_index,
            "the reply must reflect the live node's log"
        );
        assert_eq!(
            vote_response.vote.leader_id().voted_for(),
            Some(1),
            "the reply must carry the live node's own vote"
        );
        // A live leader within its lease refuses to be disrupted, which is the
        // correct answer here and further confirms real Raft logic ran.
        assert!(!vote_response.vote_granted);

        node.shutdown().await.expect("clean shutdown");
    }

    /// The end-to-end proof that this transport works: two Raft nodes, each
    /// serving its RPC endpoints on a real socket, forming a cluster, electing a
    /// leader (which requires the vote RPC to cross the wire) and replicating a
    /// committed write to the follower's state machine (which requires
    /// append_entries to cross it too).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_nodes_elect_a_leader_and_replicate_over_http() {
        use crate::common::NodeRole;
        use crate::raft_node;
        use crate::raft_store::{ClusterRequest, SledStore};
        use openraft::ServerState;

        let store_a = SledStore::temporary().expect("temporary store");
        let store_b = SledStore::temporary().expect("temporary store");

        // Build both nodes before initializing either: the membership needs the
        // addresses, and the addresses are only known once the servers bind.
        let node_a = raft_node::build_node(1, store_a)
            .await
            .expect("build node 1");
        let node_b = raft_node::build_node(2, store_b)
            .await
            .expect("build node 2");

        let (stop_a, rx_a) = tokio::sync::oneshot::channel::<()>();
        let (stop_b, rx_b) = tokio::sync::oneshot::channel::<()>();
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let (addr_a, server_a) = warp::serve(raft_rpc_filters(node_a.raft.clone()))
            .bind_with_graceful_shutdown(loopback, async move {
                let _ = rx_a.await;
            });
        let (addr_b, server_b) = warp::serve(raft_rpc_filters(node_b.raft.clone()))
            .bind_with_graceful_shutdown(loopback, async move {
                let _ = rx_b.await;
            });
        tokio::spawn(server_a);
        tokio::spawn(server_b);

        let mut members = BTreeMap::new();
        members.insert(1, BasicNode::new(addr_a.to_string()));
        members.insert(2, BasicNode::new(addr_b.to_string()));
        node_a
            .raft
            .initialize(members)
            .await
            .expect("initialize the two-member cluster");

        // A two-member cluster needs both votes, so becoming leader is only
        // possible if node 2 answered the vote RPC over HTTP.
        node_a
            .raft
            .wait(Some(Duration::from_secs(30)))
            .state(ServerState::Leader, "node 1 should win the election")
            .await
            .expect("node 1 became leader");
        node_b
            .raft
            .wait(Some(Duration::from_secs(30)))
            .state(ServerState::Follower, "node 2 should follow")
            .await
            .expect("node 2 became follower");

        node_a
            .raft
            .client_write(ClusterRequest::AssignRole {
                node_id: "ki-0".to_string(),
                role: NodeRole::Ki,
            })
            .await
            .expect("write committed by the leader");

        // Committing required node 2 to accept the entry; wait for it to apply.
        node_b
            .raft
            .wait(Some(Duration::from_secs(30)))
            .applied_index_at_least(Some(2), "node 2 applied the replicated write")
            .await
            .expect("node 2 applied the entry");

        assert_eq!(
            node_b.store.role_of("ki-0").await,
            Some(NodeRole::Ki),
            "the follower's state machine must reflect the leader's committed write"
        );

        let _ = stop_a.send(());
        let _ = stop_b.send(());
        node_a.shutdown().await.expect("clean shutdown");
        node_b.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn unknown_raft_paths_are_not_routed() {
        use crate::raft_node;
        use crate::raft_store::SledStore;

        let store = SledStore::temporary().expect("temporary store");
        let node = raft_node::build_node(1, store).await.expect("build raft");
        let filters = raft_rpc_filters(node.raft.clone());

        let response = warp::test::request()
            .method("POST")
            .path("/raft/not-a-method")
            .json(&serde_json::json!({}))
            .reply(&filters)
            .await;

        assert_eq!(response.status(), 404);
        node.shutdown().await.expect("clean shutdown");
    }
}
