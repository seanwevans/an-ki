// Consensus between principals.
//
// The principal's authoritative state — which role each node is assigned, and
// cluster-wide configuration overrides — is not held in one process's memory.
// Every change is a `ClusterRequest` committed through Raft and then folded
// into a state machine, so each principal derives the same state from the same
// log. This is a compact implementation of the protocol `openraft` runs for
// real in `src/raft_node.rs`: terms, randomized election timeouts, the
// up-to-date check on votes, log matching, and commitment by majority.
//
// Two properties are modelled because the demo turns on them:
//
//   * A minority cannot commit. Kill two of three principals and the survivor
//     campaigns forever without ever becoming leader — which is why the README
//     tells you to run an odd number.
//   * The log is on disk. A principal that restarts reloads its term, vote and
//     entries and rejoins the cluster it was part of, instead of coming back as
//     a fresh single-member cluster.

/** Small deterministic generator for election timeouts, so runs reproduce. */
export function mulberry32(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = Math.imul(state ^ (state >>> 15), 1 | state);
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

export const FOLLOWER = 'follower';
export const CANDIDATE = 'candidate';
export const LEADER = 'leader';

/**
 * The state machine every principal folds the committed log into.
 *
 * Deliberately tiny, and deliberately not a general-purpose executor: an
 * earlier design accepted an arbitrary SQL statement from anyone able to
 * publish to the update queue, which is why the request type is a closed enum.
 */
export class ClusterStateMachine {
  constructor() {
    this.roles = new Map();
    this.config = new Map();
    this.applied = 0;
  }

  apply(request) {
    this.applied += 1;
    if (!request) {
      // The blank entry a new leader appends to commit its own term.
      return 'leader established';
    }
    switch (request.type) {
      case 'assign_role':
        this.roles.set(request.node_id, request.role);
        return `assigned ${request.role} to ${request.node_id}`;
      case 'clear_role':
        this.roles.delete(request.node_id);
        return `cleared role for ${request.node_id}`;
      case 'set_config':
        this.config.set(request.key, request.value);
        return `${request.key} = ${request.value}`;
      default:
        return 'unknown request';
    }
  }

  snapshot() {
    return {
      roles: [...this.roles.entries()],
      config: [...this.config.entries()],
      applied: this.applied,
    };
  }
}

class RaftNode {
  constructor(id, cluster) {
    this.id = id;
    this.cluster = cluster;
    this.alive = true;

    // Persisted to `sled` in the real principal: the vote and the log are
    // flushed before the storage call returns, so a crash cannot lose them.
    this.currentTerm = 0;
    this.votedFor = null;
    this.log = [{ index: 0, term: 0, request: null }];

    // Volatile. A restart rebuilds the state machine by replaying the log.
    this.commitIndex = 0;
    this.lastApplied = 0;
    this.state = FOLLOWER;
    this.leaderId = null;
    this.votes = new Set();
    this.nextIndex = new Map();
    this.matchIndex = new Map();
    this.machine = new ClusterStateMachine();
    this.electionDeadline = 0;
    this.nextHeartbeatAt = 0;
  }

  lastIndex() {
    return this.log[this.log.length - 1].index;
  }

  lastTerm() {
    return this.log[this.log.length - 1].term;
  }

  entryAt(index) {
    return this.log[index];
  }

  resetElectionTimer(now) {
    const [low, high] = this.cluster.electionTimeoutMs;
    this.electionDeadline = now + low + this.cluster.random() * (high - low);
  }

  becomeFollower(term, now) {
    this.state = FOLLOWER;
    this.currentTerm = term;
    this.votedFor = null;
    this.votes.clear();
    this.resetElectionTimer(now);
  }
}

export class RaftCluster {
  constructor({
    peers,
    rpcLatencyMs = 40,
    electionTimeoutMs = [900, 1800],
    heartbeatMs = 300,
    seed = 0x5eed,
    onEvent = () => {},
  }) {
    this.random = mulberry32(seed);
    this.rpcLatencyMs = rpcLatencyMs;
    this.electionTimeoutMs = electionTimeoutMs;
    this.heartbeatMs = heartbeatMs;
    this.onEvent = onEvent;
    this.nodes = new Map(peers.map((id) => [id, new RaftNode(id, this)]));
    this.packets = [];
    this.now = 0;
    for (const node of this.nodes.values()) {
      node.resetElectionTimer(0);
    }
  }

  peers() {
    return [...this.nodes.keys()];
  }

  node(id) {
    return this.nodes.get(id);
  }

  majority() {
    return Math.floor(this.nodes.size / 2) + 1;
  }

  leader() {
    for (const node of this.nodes.values()) {
      if (node.alive && node.state === LEADER) {
        return node;
      }
    }
    return null;
  }

  send(from, to, kind, body) {
    this.packets.push({
      id: `${from}-${to}-${this.now}-${this.packets.length}`,
      from,
      to,
      kind,
      body,
      sentAt: this.now,
      deliverAt: this.now + this.rpcLatencyMs,
    });
  }

  /** A principal that loses power keeps its disk, not its memory. */
  kill(id) {
    const node = this.nodes.get(id);
    if (!node || !node.alive) {
      return;
    }
    node.alive = false;
    node.state = FOLLOWER;
    node.leaderId = null;
    node.votes.clear();
    this.onEvent({ level: 'warn', node: id, message: 'principal stopped; Raft log left on disk' });
  }

  /**
   * Restarts a principal. It reloads its term, vote and entries, then replays
   * the log into a fresh state machine — the same recovery the `sled`-backed
   * store performs on startup.
   */
  revive(id, now) {
    const node = this.nodes.get(id);
    if (!node || node.alive) {
      return;
    }
    node.alive = true;
    node.commitIndex = 0;
    node.lastApplied = 0;
    node.machine = new ClusterStateMachine();
    node.state = FOLLOWER;
    node.resetElectionTimer(now);
    this.onEvent({
      level: 'info',
      node: id,
      message: `principal restarted; reloaded ${node.lastIndex()} log entr${
        node.lastIndex() === 1 ? 'y' : 'ies'
      } at term ${node.currentTerm}`,
    });
  }

  /**
   * Submits a cluster change. Only the leader may accept one; anyone else
   * reports who to try instead, which is how the principal knows to requeue a
   * message rather than acknowledge it.
   */
  clientWrite(request) {
    const leader = this.leader();
    if (!leader) {
      return { ok: false, reason: 'no leader' };
    }
    const index = leader.lastIndex() + 1;
    leader.log.push({ index, term: leader.currentTerm, request });
    return { ok: true, index, term: leader.currentTerm, leader: leader.id };
  }

  tick(now) {
    this.now = now;
    this.deliver(now);

    for (const node of this.nodes.values()) {
      if (!node.alive) {
        continue;
      }
      if (node.state === LEADER) {
        if (now >= node.nextHeartbeatAt) {
          node.nextHeartbeatAt = now + this.heartbeatMs;
          this.replicate(node);
        }
        this.advanceCommit(node);
      } else if (now >= node.electionDeadline) {
        this.startElection(node, now);
      }
      this.applyCommitted(node);
    }
  }

  deliver(now) {
    const arrived = [];
    this.packets = this.packets.filter((packet) => {
      if (packet.deliverAt > now) {
        return true;
      }
      arrived.push(packet);
      return false;
    });

    for (const packet of arrived) {
      const node = this.nodes.get(packet.to);
      // A packet to a stopped principal is simply lost, which is what makes
      // the election timeout the only thing that can break a stall.
      if (!node || !node.alive) {
        continue;
      }
      switch (packet.kind) {
        case 'vote':
          this.handleVoteRequest(node, packet.body, now);
          break;
        case 'vote-reply':
          this.handleVoteReply(node, packet.body, now);
          break;
        case 'append':
          this.handleAppend(node, packet.body, now);
          break;
        case 'append-reply':
          this.handleAppendReply(node, packet.body, now);
          break;
        default:
          break;
      }
    }
  }

  startElection(node, now) {
    node.currentTerm += 1;
    node.state = CANDIDATE;
    node.votedFor = node.id;
    node.votes = new Set([node.id]);
    node.leaderId = null;
    node.resetElectionTimer(now);
    this.onEvent({
      level: 'info',
      node: node.id,
      message: `election timeout; campaigning for term ${node.currentTerm}`,
    });

    for (const peer of this.peers()) {
      if (peer === node.id) {
        continue;
      }
      this.send(node.id, peer, 'vote', {
        term: node.currentTerm,
        candidateId: node.id,
        lastLogIndex: node.lastIndex(),
        lastLogTerm: node.lastTerm(),
      });
    }

    // Its own vote may already be a majority — it always is in the
    // single-member cluster that running without `RAFT_PEERS` produces, where
    // there is no reply coming to trigger the win.
    if (node.votes.size >= this.majority()) {
      this.becomeLeader(node, now);
    }
  }

  handleVoteRequest(node, body, now) {
    if (body.term > node.currentTerm) {
      node.becomeFollower(body.term, now);
    }

    // A vote is only given to a candidate whose log is at least as complete as
    // the voter's. Without this check a leader could be elected that is missing
    // committed entries, and the log would not be a log any more.
    const upToDate =
      body.lastLogTerm > node.lastTerm() ||
      (body.lastLogTerm === node.lastTerm() && body.lastLogIndex >= node.lastIndex());
    const granted =
      body.term === node.currentTerm &&
      (node.votedFor === null || node.votedFor === body.candidateId) &&
      upToDate;

    if (granted) {
      node.votedFor = body.candidateId;
      node.resetElectionTimer(now);
    }

    this.send(node.id, body.candidateId, 'vote-reply', {
      term: node.currentTerm,
      granted,
      from: node.id,
    });
  }

  handleVoteReply(node, body, now) {
    if (body.term > node.currentTerm) {
      node.becomeFollower(body.term, now);
      return;
    }
    if (node.state !== CANDIDATE || body.term !== node.currentTerm || !body.granted) {
      return;
    }

    node.votes.add(body.from);
    if (node.votes.size >= this.majority()) {
      this.becomeLeader(node, now);
    }
  }

  becomeLeader(node, now) {
    node.state = LEADER;
    node.leaderId = node.id;
    node.nextIndex = new Map(this.peers().map((peer) => [peer, node.lastIndex() + 1]));
    node.matchIndex = new Map(this.peers().map((peer) => [peer, 0]));
    // A blank entry at the new term. A leader may only commit entries from its
    // own term, so without one, entries inherited from a previous leader could
    // sit replicated but uncommitted indefinitely.
    node.log.push({ index: node.lastIndex() + 1, term: node.currentTerm, request: null });
    node.matchIndex.set(node.id, node.lastIndex());
    node.nextHeartbeatAt = now;
    this.onEvent({
      level: 'info',
      node: node.id,
      message: `elected leader for term ${node.currentTerm} with ${node.votes.size}/${this.nodes.size} votes`,
    });
  }

  replicate(leader) {
    for (const peer of this.peers()) {
      if (peer === leader.id) {
        continue;
      }
      const nextIndex = leader.nextIndex.get(peer) ?? leader.lastIndex() + 1;
      const prevIndex = Math.max(nextIndex - 1, 0);
      const previous = leader.entryAt(prevIndex) ?? leader.log[0];
      const entries = leader.log.slice(prevIndex + 1);
      this.send(leader.id, peer, 'append', {
        term: leader.currentTerm,
        leaderId: leader.id,
        prevLogIndex: previous.index,
        prevLogTerm: previous.term,
        entries,
        leaderCommit: leader.commitIndex,
      });
    }
  }

  handleAppend(node, body, now) {
    if (body.term < node.currentTerm) {
      this.send(node.id, body.leaderId, 'append-reply', {
        term: node.currentTerm,
        success: false,
        from: node.id,
        matchIndex: 0,
      });
      return;
    }

    if (body.term > node.currentTerm) {
      node.becomeFollower(body.term, now);
    }
    node.state = FOLLOWER;
    node.leaderId = body.leaderId;
    node.resetElectionTimer(now);

    // Log matching: refuse anything that does not follow from an entry we
    // already hold, and let the leader walk `nextIndex` back until it finds one.
    const previous = node.entryAt(body.prevLogIndex);
    if (!previous || previous.term !== body.prevLogTerm) {
      this.send(node.id, body.leaderId, 'append-reply', {
        term: node.currentTerm,
        success: false,
        from: node.id,
        matchIndex: 0,
      });
      return;
    }

    if (body.entries.length > 0) {
      node.log = node.log.slice(0, body.prevLogIndex + 1).concat(body.entries);
    }
    node.commitIndex = Math.min(body.leaderCommit, node.lastIndex());

    this.send(node.id, body.leaderId, 'append-reply', {
      term: node.currentTerm,
      success: true,
      from: node.id,
      matchIndex: node.lastIndex(),
    });
  }

  handleAppendReply(node, body, now) {
    if (body.term > node.currentTerm) {
      node.becomeFollower(body.term, now);
      return;
    }
    if (node.state !== LEADER || body.term !== node.currentTerm) {
      return;
    }

    if (body.success) {
      node.matchIndex.set(body.from, body.matchIndex);
      node.nextIndex.set(body.from, body.matchIndex + 1);
    } else {
      node.nextIndex.set(body.from, Math.max((node.nextIndex.get(body.from) ?? 1) - 1, 1));
    }
  }

  /**
   * Commits the highest index a majority holds — and only from the current
   * term, per the protocol's commitment rule.
   */
  advanceCommit(leader) {
    leader.matchIndex.set(leader.id, leader.lastIndex());
    const matches = [...leader.matchIndex.values()].sort((a, b) => b - a);
    const candidate = matches[this.majority() - 1] ?? 0;

    if (candidate > leader.commitIndex && leader.entryAt(candidate)?.term === leader.currentTerm) {
      leader.commitIndex = candidate;
    }
  }

  applyCommitted(node) {
    while (node.lastApplied < node.commitIndex) {
      node.lastApplied += 1;
      const entry = node.entryAt(node.lastApplied);
      if (!entry) {
        break;
      }
      const description = node.machine.apply(entry.request);
      if (entry.request && node.state === LEADER) {
        this.onEvent({
          level: 'info',
          node: node.id,
          message: `committed entry ${entry.index} at term ${entry.term}: ${description}`,
        });
      }
    }
  }
}
