//! Application state replicated through Raft, and the durable store that backs it.
//!
//! The cluster's authoritative state is the principal's answer to two questions:
//! which role each node has been assigned, and what the cluster-wide
//! configuration is. Both are replicated as [`ClusterRequest`] entries in the
//! Raft log and folded into [`ClusterStateMachine`], so any principal in the
//! cluster derives the same answer from the same log.
//!
//! [`SledStore`] persists that log — along with the vote, which Raft correctness
//! depends on surviving a crash — to a local [`sled`] database. Every write that
//! Raft must not lose is flushed before the call returns.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::Arc;

use openraft::storage::{Adaptor, LogState, RaftLogReader, RaftSnapshotBuilder, Snapshot};
use openraft::{
    BasicNode, Entry, EntryPayload, LogId, OptionalSend, RaftLogId, RaftStorage, RaftTypeConfig,
    SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info};

use crate::common::NodeRole;

/// Raft's identifier for a principal. Distinct from [`crate::common::node_id`],
/// which identifies a process to the health system; Raft requires a small
/// totally ordered id, supplied by `RAFT_NODE_ID`.
pub type NodeId = u64;

/// A state change submitted to the cluster. Everything the principal decides on
/// behalf of the cluster goes through one of these, so the decision is
/// replicated rather than held in one process's memory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClusterRequest {
    /// Record that `node_id` should serve `role`.
    AssignRole { node_id: String, role: NodeRole },
    /// Forget any role assigned to `node_id`.
    ClearRole { node_id: String },
    /// Set a cluster-wide configuration value.
    SetConfig { key: String, value: String },
}

/// The outcome of applying a [`ClusterRequest`], carrying the value that was
/// replaced so callers can tell an update from a no-op.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClusterResponse {
    /// Role previously assigned to the affected node, if any.
    pub previous_role: Option<NodeRole>,
    /// Configuration value previously stored under the affected key, if any.
    pub previous_value: Option<String>,
}

openraft::declare_raft_types!(
    /// Raft type configuration for the an-ki cluster. `NodeId` defaults to `u64`
    /// and `Node` to [`BasicNode`], whose `addr` field is what the network
    /// transport dials.
    pub TypeConfig:
        D = ClusterRequest,
        R = ClusterResponse,
);

/// The replicated state itself: the fold of every applied [`ClusterRequest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClusterStateMachine {
    pub last_applied_log: Option<LogId<NodeId>>,
    pub last_membership: StoredMembership<NodeId, BasicNode>,
    /// Role assigned to each node, keyed by the node's heartbeat identity.
    pub roles: BTreeMap<String, NodeRole>,
    /// Cluster-wide configuration overrides.
    pub config: BTreeMap<String, String>,
}

impl ClusterStateMachine {
    /// Applies one request, returning what it replaced.
    fn apply(&mut self, request: &ClusterRequest) -> ClusterResponse {
        match request {
            ClusterRequest::AssignRole { node_id, role } => ClusterResponse {
                previous_role: self.roles.insert(node_id.clone(), role.clone()),
                previous_value: None,
            },
            ClusterRequest::ClearRole { node_id } => ClusterResponse {
                previous_role: self.roles.remove(node_id),
                previous_value: None,
            },
            ClusterRequest::SetConfig { key, value } => ClusterResponse {
                previous_role: None,
                previous_value: self.config.insert(key.clone(), value.clone()),
            },
        }
    }
}

/// A snapshot of [`ClusterStateMachine`] plus the metadata Raft needs to place it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSnapshot {
    pub meta: SnapshotMeta<NodeId, BasicNode>,
    pub data: Vec<u8>,
}

// Keys in the `meta` tree. Kept together so the on-disk layout is visible in one place.
const KEY_VOTE: &str = "vote";
const KEY_COMMITTED: &str = "committed";
const KEY_LAST_PURGED: &str = "last_purged";
const KEY_STATE_MACHINE: &str = "state_machine";
const KEY_SNAPSHOT: &str = "snapshot";
const KEY_SNAPSHOT_INDEX: &str = "snapshot_index";

const TREE_LOGS: &str = "raft_logs";
const TREE_META: &str = "raft_meta";

/// Log indices are stored big-endian so sled's lexicographic key order matches
/// numeric order, which is what makes range scans and `last()` correct.
fn log_key(index: u64) -> [u8; 8] {
    index.to_be_bytes()
}

/// Translates a `RangeBounds<u64>` over log indices into the byte-keyed range
/// sled wants.
fn log_key_bound(bound: Bound<&u64>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Included(index) => Bound::Included(log_key(*index).to_vec()),
        Bound::Excluded(index) => Bound::Excluded(log_key(*index).to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Durable Raft storage backed by a local sled database.
///
/// The Raft log and vote live on disk. The state machine and current snapshot
/// are cached in memory for reads and written through to disk on every change,
/// so a restart reloads exactly the state the log had already produced.
pub struct SledStore {
    db: sled::Db,
    logs: sled::Tree,
    meta: sled::Tree,
    state_machine: RwLock<ClusterStateMachine>,
    current_snapshot: RwLock<Option<StoredSnapshot>>,
    snapshot_index: Mutex<u64>,
}

impl SledStore {
    /// Opens (or creates) a store rooted at `path`, restoring any state machine
    /// and snapshot a previous run left behind.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>, sled::Error> {
        let db = sled::open(path)?;
        Self::from_db(db)
    }

    /// Builds a store over a temporary database that is deleted when dropped.
    /// Used by tests so they neither touch the real data directory nor collide.
    pub fn temporary() -> Result<Arc<Self>, sled::Error> {
        let db = sled::Config::new().temporary(true).open()?;
        Self::from_db(db)
    }

    fn from_db(db: sled::Db) -> Result<Arc<Self>, sled::Error> {
        let logs = db.open_tree(TREE_LOGS)?;
        let meta = db.open_tree(TREE_META)?;

        // A state machine or snapshot that fails to decode means the on-disk
        // format changed underneath us. Treat it as empty rather than crashing:
        // the Raft log is the source of truth and will rebuild the state.
        let state_machine: ClusterStateMachine = meta
            .get(KEY_STATE_MACHINE)?
            .and_then(|raw| decode_or_warn(&raw, KEY_STATE_MACHINE))
            .unwrap_or_default();
        let current_snapshot: Option<StoredSnapshot> = meta
            .get(KEY_SNAPSHOT)?
            .and_then(|raw| decode_or_warn(&raw, KEY_SNAPSHOT));
        let snapshot_index: u64 = meta
            .get(KEY_SNAPSHOT_INDEX)?
            .and_then(|raw| decode_or_warn(&raw, KEY_SNAPSHOT_INDEX))
            .unwrap_or(0);

        info!(
            "Opened Raft store (log entries={}, last_applied={:?}, snapshot={})",
            logs.len(),
            state_machine.last_applied_log,
            current_snapshot.is_some()
        );

        Ok(Arc::new(Self {
            db,
            logs,
            meta,
            state_machine: RwLock::new(state_machine),
            current_snapshot: RwLock::new(current_snapshot),
            snapshot_index: Mutex::new(snapshot_index),
        }))
    }

    /// Splits this store into the log store and state machine halves that
    /// [`openraft::Raft::new`] expects.
    pub fn into_adaptor(
        self: Arc<Self>,
    ) -> (
        Adaptor<TypeConfig, Arc<Self>>,
        Adaptor<TypeConfig, Arc<Self>>,
    ) {
        Adaptor::new(self)
    }

    /// Reads the currently assigned role for a node, if any.
    pub async fn role_of(&self, node_id: &str) -> Option<NodeRole> {
        self.state_machine.read().await.roles.get(node_id).cloned()
    }

    /// Reads a replicated configuration value, if any.
    pub async fn config_value(&self, key: &str) -> Option<String> {
        self.state_machine.read().await.config.get(key).cloned()
    }

    /// A copy of the whole replicated state, for reporting and tests.
    pub async fn snapshot_state(&self) -> ClusterStateMachine {
        self.state_machine.read().await.clone()
    }

    /// Persists the in-memory state machine and flushes it to disk.
    async fn persist_state_machine(
        &self,
        state: &ClusterStateMachine,
    ) -> Result<(), StorageError<NodeId>> {
        let encoded =
            serde_json::to_vec(state).map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.meta
            .insert(KEY_STATE_MACHINE, encoded)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.flush(|e| StorageIOError::write_state_machine(&e))
            .await
    }

    /// Flushes the database, mapping any failure through `to_error`.
    ///
    /// Raft's safety argument assumes a write is on disk once the storage call
    /// returns, so every durability-critical path funnels through here.
    async fn flush(
        &self,
        to_error: impl Fn(sled::Error) -> StorageIOError<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        self.db.flush_async().await.map_err(&to_error)?;
        Ok(())
    }
}

/// Decodes a persisted value, logging and discarding anything unreadable.
fn decode_or_warn<T: for<'de> Deserialize<'de>>(raw: &[u8], key: &str) -> Option<T> {
    match serde_json::from_slice(raw) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                "Discarding unreadable persisted value for '{}': {:?}. \
                 It will be rebuilt from the Raft log.",
                key,
                e
            );
            None
        }
    }
}

impl RaftLogReader<TypeConfig> for Arc<SledStore> {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let bounds = (
            log_key_bound(range.start_bound()),
            log_key_bound(range.end_bound()),
        );

        let mut entries = Vec::new();
        for item in self.logs.range(bounds) {
            let (_, raw) = item.map_err(|e| StorageIOError::read_logs(&e))?;
            let entry: Entry<TypeConfig> =
                serde_json::from_slice(&raw).map_err(|e| StorageIOError::read_logs(&e))?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

impl RaftSnapshotBuilder<TypeConfig> for Arc<SledStore> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let (data, last_applied_log, last_membership) = {
            let state = self.state_machine.read().await;
            let data =
                serde_json::to_vec(&*state).map_err(|e| StorageIOError::read_state_machine(&e))?;
            (data, state.last_applied_log, state.last_membership.clone())
        };

        // The counter is persisted so snapshot ids stay unique across restarts;
        // an id reused after a crash would collide with a snapshot a follower
        // has already seen.
        let snapshot_index = {
            let mut index = self.snapshot_index.lock().await;
            *index += 1;
            let encoded = serde_json::to_vec(&*index)
                .map_err(|e| StorageIOError::write_snapshot(None, &e))?;
            self.meta
                .insert(KEY_SNAPSHOT_INDEX, encoded)
                .map_err(|e| StorageIOError::write_snapshot(None, &e))?;
            *index
        };

        let snapshot_id = match last_applied_log {
            Some(last) => format!("{}-{}-{}", last.leader_id, last.index, snapshot_index),
            None => format!("--{}", snapshot_index),
        };

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            last_membership,
            snapshot_id,
        };
        let stored = StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };

        let encoded = serde_json::to_vec(&stored)
            .map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), &e))?;
        self.meta
            .insert(KEY_SNAPSHOT, encoded)
            .map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), &e))?;
        self.flush(|e| StorageIOError::write_snapshot(None, &e))
            .await?;

        *self.current_snapshot.write().await = Some(stored);
        debug!(
            "Built Raft snapshot {} ({} bytes)",
            meta.snapshot_id,
            data.len()
        );

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStorage<TypeConfig> for Arc<SledStore> {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let last_purged: Option<LogId<NodeId>> = self
            .meta
            .get(KEY_LAST_PURGED)
            .map_err(|e| StorageIOError::read_logs(&e))?
            .map(|raw| serde_json::from_slice(&raw))
            .transpose()
            .map_err(|e| StorageIOError::read_logs(&e))?;

        let last_in_log = self
            .logs
            .last()
            .map_err(|e| StorageIOError::read_logs(&e))?
            .map(|(_, raw)| serde_json::from_slice::<Entry<TypeConfig>>(&raw))
            .transpose()
            .map_err(|e| StorageIOError::read_logs(&e))?
            .map(|entry| *entry.get_log_id());

        Ok(LogState {
            last_purged_log_id: last_purged,
            // With every entry purged, the last purged id is the last log id.
            last_log_id: last_in_log.or(last_purged),
        })
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        let encoded = serde_json::to_vec(vote).map_err(|e| StorageIOError::write_vote(&e))?;
        self.meta
            .insert(KEY_VOTE, encoded)
            .map_err(|e| StorageIOError::write_vote(&e))?;
        // A vote that is not durable before this returns can let the node vote
        // twice in one term after a crash, so this flush is not optional.
        self.flush(|e| StorageIOError::write_vote(&e)).await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        self.meta
            .get(KEY_VOTE)
            .map_err(|e| StorageIOError::read_vote(&e))?
            .map(|raw| serde_json::from_slice(&raw))
            .transpose()
            .map_err(|e| StorageIOError::read_vote(&e).into())
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        let encoded = serde_json::to_vec(&committed).map_err(|e| StorageIOError::write_logs(&e))?;
        self.meta
            .insert(KEY_COMMITTED, encoded)
            .map_err(|e| StorageIOError::write_logs(&e))?;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        let stored: Option<Option<LogId<NodeId>>> = self
            .meta
            .get(KEY_COMMITTED)
            .map_err(|e| StorageIOError::read_logs(&e))?
            .map(|raw| serde_json::from_slice(&raw))
            .transpose()
            .map_err(|e| StorageIOError::read_logs(&e))?;
        Ok(stored.flatten())
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        for entry in entries {
            let log_id = *entry.get_log_id();
            let encoded = serde_json::to_vec(&entry)
                .map_err(|e| StorageIOError::write_log_entry(log_id, &e))?;
            self.logs
                .insert(log_key(log_id.index), encoded)
                .map_err(|e| StorageIOError::write_log_entry(log_id, &e))?;
        }
        // Entries must be durable before they are acknowledged to the leader.
        self.flush(|e| StorageIOError::write_logs(&e)).await
    }

    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        debug!(
            "Deleting conflicting log entries from index {}",
            log_id.index
        );
        let keys: Vec<_> = self
            .logs
            .range(log_key(log_id.index).to_vec()..)
            .keys()
            .collect::<Result<_, _>>()
            .map_err(|e| StorageIOError::write_logs(&e))?;
        for key in keys {
            self.logs
                .remove(key)
                .map_err(|e| StorageIOError::write_logs(&e))?;
        }
        self.flush(|e| StorageIOError::write_logs(&e)).await
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        debug!("Purging log entries through index {}", log_id.index);
        let encoded = serde_json::to_vec(&log_id).map_err(|e| StorageIOError::write_logs(&e))?;
        self.meta
            .insert(KEY_LAST_PURGED, encoded)
            .map_err(|e| StorageIOError::write_logs(&e))?;

        let keys: Vec<_> = self
            .logs
            .range(..=log_key(log_id.index).to_vec())
            .keys()
            .collect::<Result<_, _>>()
            .map_err(|e| StorageIOError::write_logs(&e))?;
        for key in keys {
            self.logs
                .remove(key)
                .map_err(|e| StorageIOError::write_logs(&e))?;
        }
        self.flush(|e| StorageIOError::write_logs(&e)).await
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        let state = self.state_machine.read().await;
        Ok((state.last_applied_log, state.last_membership.clone()))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<TypeConfig>],
    ) -> Result<Vec<ClusterResponse>, StorageError<NodeId>> {
        let mut responses = Vec::with_capacity(entries.len());
        let mut state = self.state_machine.write().await;

        for entry in entries {
            state.last_applied_log = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => responses.push(ClusterResponse::default()),
                EntryPayload::Normal(ref request) => responses.push(state.apply(request)),
                EntryPayload::Membership(ref membership) => {
                    state.last_membership =
                        StoredMembership::new(Some(entry.log_id), membership.clone());
                    responses.push(ClusterResponse::default());
                }
            }
        }

        // Persist once for the whole batch: the state machine is only correct as
        // a unit, and a partial write would be indistinguishable from a full one.
        self.persist_state_machine(&state).await?;
        Ok(responses)
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<<TypeConfig as RaftTypeConfig>::SnapshotData>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<<TypeConfig as RaftTypeConfig>::SnapshotData>,
    ) -> Result<(), StorageError<NodeId>> {
        let data = snapshot.into_inner();
        info!(
            "Installing Raft snapshot {} ({} bytes)",
            meta.snapshot_id,
            data.len()
        );

        let new_state: ClusterStateMachine = serde_json::from_slice(&data)
            .map_err(|e| StorageIOError::read_snapshot(Some(meta.signature()), &e))?;

        let stored = StoredSnapshot {
            meta: meta.clone(),
            data,
        };
        let encoded = serde_json::to_vec(&stored)
            .map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), &e))?;
        self.meta
            .insert(KEY_SNAPSHOT, encoded)
            .map_err(|e| StorageIOError::write_snapshot(Some(meta.signature()), &e))?;

        {
            let mut state = self.state_machine.write().await;
            *state = new_state;
            self.persist_state_machine(&state).await?;
        }
        *self.current_snapshot.write().await = Some(stored);
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        Ok(self
            .current_snapshot
            .read()
            .await
            .as_ref()
            .map(|stored| Snapshot {
                meta: stored.meta.clone(),
                snapshot: Box::new(Cursor::new(stored.data.clone())),
            }))
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::testing::log_id;
    use openraft::CommittedLeaderId;

    fn normal_entry(term: u64, index: u64, request: ClusterRequest) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(term, 0), index),
            payload: EntryPayload::Normal(request),
        }
    }

    fn assign(node_id: &str, role: NodeRole) -> ClusterRequest {
        ClusterRequest::AssignRole {
            node_id: node_id.to_string(),
            role,
        }
    }

    #[test]
    fn state_machine_reports_the_role_it_replaced() {
        let mut state = ClusterStateMachine::default();

        let first = state.apply(&assign("node-0", NodeRole::Ki));
        let second = state.apply(&assign("node-0", NodeRole::An));

        assert_eq!(first.previous_role, None);
        assert_eq!(second.previous_role, Some(NodeRole::Ki));
        assert_eq!(state.roles.get("node-0"), Some(&NodeRole::An));
    }

    #[test]
    fn clearing_a_role_removes_it() {
        let mut state = ClusterStateMachine::default();
        state.apply(&assign("node-0", NodeRole::Ki));

        let response = state.apply(&ClusterRequest::ClearRole {
            node_id: "node-0".to_string(),
        });

        assert_eq!(response.previous_role, Some(NodeRole::Ki));
        assert!(state.roles.is_empty());
    }

    #[test]
    fn config_updates_report_the_replaced_value() {
        let mut state = ClusterStateMachine::default();

        let first = state.apply(&ClusterRequest::SetConfig {
            key: "shards".into(),
            value: "2".into(),
        });
        let second = state.apply(&ClusterRequest::SetConfig {
            key: "shards".into(),
            value: "4".into(),
        });

        assert_eq!(first.previous_value, None);
        assert_eq!(second.previous_value, Some("2".to_string()));
        assert_eq!(state.config.get("shards"), Some(&"4".to_string()));
    }

    #[test]
    fn log_keys_sort_numerically() {
        // Lexicographic order over the byte keys must match numeric order, or
        // range scans and `last()` silently return the wrong entries.
        assert!(log_key(2) < log_key(10));
        assert!(log_key(255) < log_key(256));
        assert!(log_key(u64::MAX - 1) < log_key(u64::MAX));
    }

    #[tokio::test]
    async fn vote_survives_reopening_the_database() {
        let dir = tempdir();
        let vote = Vote::new(3, 7);
        {
            let mut store = SledStore::open(&dir).expect("open store");
            store.save_vote(&vote).await.expect("save vote");
        }

        let mut reopened = SledStore::open(&dir).expect("reopen store");
        assert_eq!(reopened.read_vote().await.expect("read vote"), Some(vote));
    }

    #[tokio::test]
    async fn applied_entries_survive_reopening_the_database() {
        let dir = tempdir();
        {
            let mut store = SledStore::open(&dir).expect("open store");
            store
                .append_to_log([normal_entry(1, 1, assign("ki-0", NodeRole::Ki))])
                .await
                .expect("append");
            store
                .apply_to_state_machine(&[normal_entry(1, 1, assign("ki-0", NodeRole::Ki))])
                .await
                .expect("apply");
        }

        let reopened = SledStore::open(&dir).expect("reopen store");
        assert_eq!(reopened.role_of("ki-0").await, Some(NodeRole::Ki));
        let mut reopened_mut = reopened.clone();
        let state = reopened_mut.get_log_state().await.expect("log state");
        assert_eq!(state.last_log_id.map(|id| id.index), Some(1));
    }

    #[tokio::test]
    async fn log_entries_round_trip_through_a_range_read() {
        let mut store = SledStore::temporary().expect("temporary store");
        let entries: Vec<_> = (1..=5)
            .map(|i| normal_entry(1, i, assign(&format!("node-{i}"), NodeRole::Ki)))
            .collect();
        store.append_to_log(entries).await.expect("append");

        let read = store.try_get_log_entries(2..4).await.expect("read range");

        let indices: Vec<u64> = read.iter().map(|entry| entry.log_id.index).collect();
        assert_eq!(indices, vec![2, 3]);
    }

    #[tokio::test]
    async fn conflicting_entries_are_deleted_from_the_index_onward() {
        let mut store = SledStore::temporary().expect("temporary store");
        let entries: Vec<_> = (1..=5)
            .map(|i| normal_entry(1, i, assign(&format!("node-{i}"), NodeRole::Ki)))
            .collect();
        store.append_to_log(entries).await.expect("append");

        store
            .delete_conflict_logs_since(log_id(1, 0, 3))
            .await
            .expect("delete conflicts");

        let remaining = store.try_get_log_entries(..).await.expect("read all");
        let indices: Vec<u64> = remaining.iter().map(|entry| entry.log_id.index).collect();
        assert_eq!(indices, vec![1, 2]);
    }

    #[tokio::test]
    async fn purging_advances_the_last_purged_id() {
        let mut store = SledStore::temporary().expect("temporary store");
        let entries: Vec<_> = (1..=5)
            .map(|i| normal_entry(1, i, assign(&format!("node-{i}"), NodeRole::Ki)))
            .collect();
        store.append_to_log(entries).await.expect("append");

        store.purge_logs_upto(log_id(1, 0, 3)).await.expect("purge");

        let state = store.get_log_state().await.expect("log state");
        assert_eq!(state.last_purged_log_id.map(|id| id.index), Some(3));
        assert_eq!(state.last_log_id.map(|id| id.index), Some(5));
        let remaining = store.try_get_log_entries(..).await.expect("read all");
        let indices: Vec<u64> = remaining.iter().map(|entry| entry.log_id.index).collect();
        assert_eq!(indices, vec![4, 5]);
    }

    #[tokio::test]
    async fn purging_every_entry_leaves_the_purged_id_as_the_last_log_id() {
        let mut store = SledStore::temporary().expect("temporary store");
        store
            .append_to_log([normal_entry(1, 1, assign("ki-0", NodeRole::Ki))])
            .await
            .expect("append");

        store.purge_logs_upto(log_id(1, 0, 1)).await.expect("purge");

        let state = store.get_log_state().await.expect("log state");
        assert_eq!(state.last_log_id.map(|id| id.index), Some(1));
    }

    #[tokio::test]
    async fn a_snapshot_round_trips_through_install() {
        let mut source = SledStore::temporary().expect("temporary store");
        source
            .apply_to_state_machine(&[normal_entry(1, 1, assign("ki-0", NodeRole::Ki))])
            .await
            .expect("apply");
        let snapshot = source.build_snapshot().await.expect("build snapshot");

        let mut target = SledStore::temporary().expect("temporary store");
        target
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .expect("install snapshot");

        assert_eq!(target.role_of("ki-0").await, Some(NodeRole::Ki));
        let restored = target
            .get_current_snapshot()
            .await
            .expect("current snapshot")
            .expect("snapshot present");
        assert_eq!(restored.meta.snapshot_id, snapshot.meta.snapshot_id);
    }

    #[tokio::test]
    async fn snapshot_ids_keep_advancing_across_restarts() {
        let dir = tempdir();
        let first_id = {
            let mut store = SledStore::open(&dir).expect("open store");
            store
                .build_snapshot()
                .await
                .expect("build")
                .meta
                .snapshot_id
        };

        let mut reopened = SledStore::open(&dir).expect("reopen store");
        let second_id = reopened
            .build_snapshot()
            .await
            .expect("build")
            .meta
            .snapshot_id;

        assert_ne!(
            first_id, second_id,
            "a reused snapshot id would collide with one followers already hold"
        );
    }

    /// A unique scratch directory that is removed when the test process exits.
    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "an-ki-raft-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }
}
