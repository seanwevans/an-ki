// End-to-end properties of the simulated cluster: that sharding the gradient
// changes nothing about the answer, and that losing a worker changes only how
// long the round takes.

import test from 'node:test';
import assert from 'node:assert/strict';

import { Simulation, KI_TASK_QUEUE } from '../assets/sim/cluster.js';
import * as dataset from '../assets/sim/dataset.js';
import * as model from '../assets/sim/model.js';
import { StdRng } from '../assets/sim/rng.js';

/** Advances the simulation until `predicate` holds or the virtual clock runs out. */
function runUntil(simulation, predicate, limitMs = 600000) {
  while (simulation.now < limitMs) {
    simulation.step(10);
    if (predicate(simulation)) {
      return true;
    }
  }
  return false;
}

/** Ordinary full-batch gradient descent, for comparison with the cluster. */
function centralized(settings, epochs) {
  const heldOut = Math.round(settings.dataset_samples * settings.validation_fraction);
  const spec = dataset.datasetSpec(settings.dataset_samples, settings.dataset_seed, heldOut);
  const modelSpec = model.mlpSpec(dataset.INPUTS, settings.hidden_units, dataset.OUTPUTS);
  const samples = dataset.generate(spec);
  const training = dataset.training(spec, samples);
  const parameters = model.initialize(modelSpec, new StdRng(settings.init_seed));

  for (let epoch = 0; epoch < epochs; epoch += 1) {
    const { gradient } = model.lossAndGradient(modelSpec, parameters, training);
    for (let index = 0; index < parameters.length; index += 1) {
      parameters[index] = Math.fround(
        parameters[index] - Math.fround(settings.learning_rate * gradient[index]),
      );
    }
  }

  return { spec, modelSpec, parameters, validation: dataset.validation(spec, samples) };
}

test('training across shards lands exactly where one machine would', () => {
  // This is the whole claim of data-parallel training: the An node's
  // sample-weighted average of per-shard mean gradients *is* the mean gradient
  // over the dataset, so the distributed run is not an approximation of the
  // single-machine one — it is the same arithmetic, spread out.
  const settings = { training_epochs: 40, model_shards: 4 };
  const simulation = new Simulation(settings);
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= 40));

  const reference = centralized(simulation.settings, 40);
  const distributed = simulation.state.parameters;

  let worst = 0;
  for (let index = 0; index < distributed.length; index += 1) {
    worst = Math.max(worst, Math.abs(distributed[index] - reference.parameters[index]));
  }
  assert.ok(worst < 1e-5, `parameters diverged by ${worst}`);
});

test('the shard count changes the message flow, not the model', () => {
  const runs = [1, 4, 8].map((shards) => {
    const simulation = new Simulation({ training_epochs: 25, model_shards: shards });
    assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= 25));
    return simulation;
  });

  const [one, four, eight] = runs;
  for (let index = 0; index < one.state.parameters.length; index += 1) {
    assert.ok(Math.abs(one.state.parameters[index] - four.state.parameters[index]) < 1e-5);
    assert.ok(Math.abs(one.state.parameters[index] - eight.state.parameters[index]) < 1e-5);
  }
  assert.ok(eight.metrics.dispatched > four.metrics.dispatched);
});

test('the shipped settings reach the accuracy the README claims', () => {
  const simulation = new Simulation();
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= 400, 400000));
  assert.ok(
    simulation.state.lastValidationAccuracy > 0.97,
    `held-out accuracy was ${simulation.state.lastValidationAccuracy}`,
  );
});

test('a worker that dies mid-round has its shard requeued and the round still closes', () => {
  const simulation = new Simulation({ training_epochs: 40 });
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= 3));

  // Kill it while it is actually holding a delivery, which is the case that
  // needs the broker to put the work back.
  const victim = simulation.kiNodes.find((node) => node.id === 'ki-1');
  assert.ok(runUntil(simulation, () => victim.current !== null || victim.inbox.length > 0));

  const before = simulation.state.epochsCompleted;
  simulation.killKi('ki-1');
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= before + 3));

  assert.ok(simulation.broker.requeued > 0, 'nothing was requeued when the worker died');
  const survivors = simulation.snapshot().nodes.ki.filter((node) => node.alive);
  assert.equal(survivors.length, 3);
  assert.ok(survivors.every((node) => node.tasksProcessed > 0));
});

test('with every worker gone the queue grows and no epoch completes', () => {
  const simulation = new Simulation({ training_epochs: 40 });
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted >= 3));

  for (const node of simulation.kiNodes) {
    simulation.killKi(node.id);
  }
  const stalledAt = simulation.state.epochsCompleted;
  const stalledClock = simulation.now;
  runUntil(simulation, () => false, stalledClock + 4000);

  assert.equal(simulation.state.epochsCompleted, stalledAt, 'a round closed with no workers alive');
  assert.ok(simulation.broker.depth(KI_TASK_QUEUE) > 0, 'the queue should be backing up');

  // Bringing one back drains the backlog: the work was never lost, only parked.
  simulation.reviveKi('ki-0');
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted > stalledAt));
});

test('a restarted An node resumes from its checkpoint instead of from init_seed', () => {
  const simulation = new Simulation({ training_epochs: 200, checkpoint_interval_epochs: 10 });
  assert.ok(runUntil(simulation, (sim) => sim.checkpoints.length >= 2));

  const checkpoint = simulation.checkpoints[simulation.checkpoints.length - 1];
  const accuracyBefore = simulation.state.lastValidationAccuracy;

  simulation.killAn();
  runUntil(simulation, () => false, simulation.now + 2000);
  simulation.reviveAn();

  assert.equal(simulation.state.epochsCompleted, checkpoint.epoch);
  assert.deepEqual(
    Array.from(simulation.state.parameters),
    Array.from(checkpoint.parameters),
    'the restart did not restore the checkpointed parameters',
  );
  assert.equal(simulation.state.lastValidationAccuracy, accuracyBefore);

  // And it keeps training from there rather than from the beginning.
  assert.ok(runUntil(simulation, (sim) => sim.state.epochsCompleted > checkpoint.epoch + 5));
});

test('a cluster update is only acknowledged once a leader has committed it', () => {
  const simulation = new Simulation();
  assert.ok(runUntil(simulation, (sim) => sim.leaderId() !== null));

  simulation.submitUpdate({ type: 'set_config', key: 'model_shards', value: '8' });
  assert.ok(
    runUntil(simulation, (sim) => {
      const machine = sim.raft.leader()?.machine;
      return machine?.config.get('model_shards') === '8';
    }),
  );

  // Every principal ends up with the same state, because they all derive it
  // from the same committed log — a follower learns the commit index on the
  // leader's next heartbeat, so this lags the leader by one round trip.
  assert.ok(
    runUntil(simulation, (sim) =>
      sim.raft.peers().every((id) => sim.raft.node(id).machine.config.get('model_shards') === '8'),
    ),
  );
  for (const id of simulation.raft.peers()) {
    assert.equal(simulation.raft.node(id).machine.config.get('model_shards'), '8');
  }
});

test('heartbeats populate the registry, and silence empties it', () => {
  const simulation = new Simulation({ heartbeat_interval_ms: 1000, node_ttl_ms: 3000 });
  assert.ok(runUntil(simulation, (sim) => sim.registry.list().length === 5));

  simulation.killKi('ki-2');
  assert.ok(
    runUntil(simulation, (sim) => !sim.registry.nodes.has('ki-2'), simulation.now + 20000),
    'a silent node was never evicted',
  );
  assert.ok(simulation.registry.nodes.has('ki-0'), 'a live node was evicted');
});
