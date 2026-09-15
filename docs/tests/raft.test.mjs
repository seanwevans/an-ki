// The properties the cluster's consistency rests on. Each test drives the same
// implementation the page runs, on its virtual clock.

import test from 'node:test';
import assert from 'node:assert/strict';

import { RaftCluster, LEADER } from '../assets/sim/raft.js';

/** Runs the cluster forward by `ms`, in the same 5ms slices the page uses. */
function run(cluster, ms, now = cluster.now) {
  let clock = now;
  const end = now + ms;
  while (clock < end) {
    clock += 5;
    cluster.tick(clock);
  }
  return clock;
}

function cluster(size = 3, seed = 7) {
  return new RaftCluster({
    peers: Array.from({ length: size }, (_, index) => `principal-${index}`),
    seed,
  });
}

test('a quorum elects exactly one leader', () => {
  const raft = cluster();
  run(raft, 6000);

  const leaders = raft.peers().filter((id) => raft.node(id).state === LEADER);
  assert.equal(leaders.length, 1, `expected one leader, got ${leaders}`);
});

test('a single-member cluster elects itself', () => {
  // Leaving `RAFT_PEERS` unset runs one principal, which is the local
  // development default. Its own vote is already a majority, and no reply is
  // coming to tell it so.
  const raft = cluster(1);
  run(raft, 4000);

  const leader = raft.leader();
  assert.ok(leader, 'a lone principal never took leadership');
  assert.equal(leader.id, 'principal-0');

  const write = raft.clientWrite({ type: 'set_config', key: 'model_shards', value: '2' });
  assert.ok(write.ok);
  run(raft, 2000, raft.now);
  assert.equal(raft.node('principal-0').machine.config.get('model_shards'), '2');
});

test('losing the leader elects a new one', () => {
  const raft = cluster();
  let clock = run(raft, 6000);
  const first = raft.leader();
  assert.ok(first);

  raft.kill(first.id);
  clock = run(raft, 8000, clock);

  const second = raft.leader();
  assert.ok(second, 'the survivors never elected a replacement');
  assert.notEqual(second.id, first.id);
  assert.ok(second.currentTerm > first.currentTerm, 'a new term should have opened');
});

test('a minority cannot elect a leader', () => {
  const raft = cluster();
  let clock = run(raft, 6000);
  raft.kill('principal-0');
  raft.kill('principal-1');
  clock = run(raft, 12000, clock);

  assert.equal(raft.leader(), null, 'two of three down must leave the cluster unable to commit');
  assert.ok(raft.node('principal-2').currentTerm > 1, 'the survivor should keep campaigning');
});

test('committed entries reach every principal and apply once', () => {
  const raft = cluster();
  let clock = run(raft, 6000);

  const write = raft.clientWrite({ type: 'assign_role', node_id: 'ki-0', role: 'Ki' });
  assert.ok(write.ok);
  raft.clientWrite({ type: 'set_config', key: 'model_shards', value: '8' });
  clock = run(raft, 4000, clock);

  for (const id of raft.peers()) {
    const node = raft.node(id);
    assert.equal(node.machine.roles.get('ki-0'), 'Ki', `${id} missing the role assignment`);
    assert.equal(node.machine.config.get('model_shards'), '8', `${id} missing the config entry`);
  }
});

test('a request to a follower is refused rather than applied locally', () => {
  const raft = cluster();
  run(raft, 6000);
  const leader = raft.leader();
  raft.kill(leader.id);

  // Between the leader's death and the next election there is nobody to accept
  // a write; the principal requeues the message rather than acknowledging it.
  const rejected = raft.clientWrite({ type: 'clear_role', node_id: 'ki-0' });
  assert.equal(rejected.ok, false);
});

test('a restarted principal reloads its log rather than starting empty', () => {
  const raft = cluster();
  let clock = run(raft, 6000);
  raft.clientWrite({ type: 'set_config', key: 'model_shards', value: '4' });
  clock = run(raft, 3000, clock);

  const follower = raft.peers().find((id) => raft.node(id).state !== LEADER);
  const before = raft.node(follower).lastIndex();
  assert.ok(before > 0);

  raft.kill(follower);
  raft.revive(follower, clock);
  assert.equal(raft.node(follower).lastIndex(), before, 'the log did not survive the restart');

  clock = run(raft, 3000, clock);
  assert.equal(raft.node(follower).machine.config.get('model_shards'), '4');
});
