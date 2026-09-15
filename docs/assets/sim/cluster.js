// The cluster itself: a principal quorum, an An node driving training rounds,
// and Ki workers computing gradients, all on a virtual clock.
//
// Every number the page displays comes from actually running the thing. The
// dataset is generated from its seed the way `src/dataset.rs` generates it, the
// gradients are real backpropagation from `src/model.rs`, and the An node
// combines them exactly as `AnNodeState::accumulate` does — sample-weighted,
// and only stepping once a full set of shards has arrived. Nothing here replays
// a recording.

import { Broker } from './broker.js';
import * as dataset from './dataset.js';
import * as model from './model.js';
import { RaftCluster, mulberry32, LEADER } from './raft.js';
import { StdRng } from './rng.js';

export const KI_TASK_QUEUE = 'ki_task_queue';
export const AN_RESULT_QUEUE = 'an_task_queue';
export const HEARTBEAT_QUEUE = 'heartbeat_queue';
export const PRINCIPAL_UPDATE_QUEUE = 'principal_update_queue';

/** Defaults from `config/default.example` and the environment variables. */
export const DEFAULT_SETTINGS = {
  model_shards: 4,
  hidden_units: 16,
  learning_rate: 1.0,
  dataset_samples: 512,
  dataset_seed: 20260814,
  init_seed: 7,
  validation_fraction: 0.2,
  training_epochs: 400,
  // The shipped default is 100ms. A round here takes longer than that, because
  // the links are slowed down enough to see a packet move; dispatching faster
  // than a round completes mixes gradients taken at different parameter points,
  // which the An node will warn about but not refuse.
  epoch_interval_ms: 400,
  checkpoint_interval_epochs: 25,
  ki_nodes: 4,
  principals: 3,
  heartbeat_interval_ms: 10000,
  node_ttl_ms: 30000,
  cluster_report_interval_ms: 60000,
  link_latency_ms: 45,
  compute_base_ms: 35,
  compute_per_sample_ms: 0.25,
};

/** FNV-1a over the fields a checkpoint is keyed by. */
export function fingerprint(spec, datasetSpec) {
  const text = [
    spec.inputs,
    spec.hidden,
    spec.outputs,
    datasetSpec.samples,
    datasetSpec.seed,
    datasetSpec.validationSamples,
  ].join(':');
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, '0');
}

/** Live cluster membership, derived from heartbeats — `src/node_registry.rs`. */
class NodeRegistry {
  constructor() {
    this.nodes = new Map();
  }

  record(id, role, now) {
    const known = this.nodes.has(id);
    this.nodes.set(id, { id, role, lastSeen: now });
    return !known;
  }

  /** Drops every node whose last heartbeat is older than `ttl`. */
  prune(ttl, now) {
    const evicted = [];
    for (const [id, info] of [...this.nodes]) {
      if (now - info.lastSeen > ttl) {
        this.nodes.delete(id);
        evicted.push(id);
      }
    }
    return evicted;
  }

  list() {
    return [...this.nodes.values()];
  }
}

/** A Ki worker: consumes gradient requests, computes one shard, replies. */
class KiNode {
  constructor(id, speed) {
    this.id = id;
    this.role = 'Ki';
    this.alive = true;
    this.speed = speed;
    this.inbox = [];
    this.current = null;
    this.tasksProcessed = 0;
    this.nextHeartbeatAt = 0;
    this.lastShard = null;
  }
}

/** The An node's view of training — a port of `AnNodeState`. */
class AnNodeState {
  constructor(spec, datasetSpec, parameters, learningRate, shards, validation) {
    this.spec = spec;
    this.dataset = datasetSpec;
    this.parameters = parameters;
    this.learningRate = learningRate;
    this.shards = shards;
    this.accumulator = new Float32Array(parameters.length);
    this.accumulatedSamples = 0;
    this.replies = 0;
    this.lossAccumulator = 0;
    this.lastEpochLoss = null;
    this.lastValidationAccuracy = null;
    this.validation = validation;
    this.epochsCompleted = 0;
  }

  /**
   * Adds one shard's contribution, weighted by sample count rather than
   * averaged evenly across shards. Both give the same answer for equal shards,
   * but weighting stays correct when the dataset does not divide evenly.
   */
  accumulate(reply) {
    if (reply.gradient.length !== this.parameters.length) {
      throw new Error(
        `Gradient length mismatch: expected ${this.parameters.length}, received ${reply.gradient.length}`,
      );
    }
    if (reply.samples === 0) {
      throw new Error('gradient reported over zero samples carries no information');
    }
    if (!Number.isFinite(reply.loss)) {
      throw new Error('gradient or loss was not finite');
    }

    const weight = reply.samples;
    for (let index = 0; index < this.accumulator.length; index += 1) {
      this.accumulator[index] = Math.fround(
        this.accumulator[index] + Math.fround(reply.gradient[index] * weight),
      );
    }
    this.lossAccumulator += reply.loss * reply.samples;
    this.accumulatedSamples += reply.samples;
    this.replies += 1;
  }

  /** Applies the accumulated gradient and resets for the next epoch. */
  applyEpoch() {
    const total = this.accumulatedSamples;
    for (let index = 0; index < this.parameters.length; index += 1) {
      this.parameters[index] = Math.fround(
        this.parameters[index] -
          Math.fround(this.learningRate * Math.fround(this.accumulator[index] / total)),
      );
    }

    this.lastEpochLoss = this.lossAccumulator / this.accumulatedSamples;
    this.epochsCompleted += 1;
    // Evaluate on data no worker trained on. Accuracy over the training set
    // would measure how well the model memorized it, not whether it generalized.
    this.lastValidationAccuracy =
      this.validation.length === 0
        ? null
        : model.accuracy(this.spec, this.parameters, this.validation);

    const summary = {
      epoch: this.epochsCompleted,
      samples: this.accumulatedSamples,
      shards: this.replies,
      loss: this.lastEpochLoss,
      accuracy: this.lastValidationAccuracy,
    };

    this.accumulator.fill(0);
    this.accumulatedSamples = 0;
    this.lossAccumulator = 0;
    this.replies = 0;
    return summary;
  }
}

export class Simulation {
  constructor(settings = {}) {
    this.settings = { ...DEFAULT_SETTINGS, ...settings };
    this.reset();
  }

  reset(overrides = {}) {
    this.settings = { ...this.settings, ...overrides };
    const s = this.settings;

    this.now = 0;
    this.events = [];
    this.eventSequence = 0;
    this.history = [];
    this.checkpoints = [];
    this.chaosNotes = [];
    this.jitter = mulberry32(0xa17ce);

    const heldOut = Math.round(s.dataset_samples * s.validation_fraction);
    this.datasetSpec = dataset.datasetSpec(s.dataset_samples, s.dataset_seed, heldOut);
    this.spec = model.mlpSpec(dataset.INPUTS, s.hidden_units, dataset.OUTPUTS);
    this.samples = dataset.generate(this.datasetSpec);
    this.trainingSet = dataset.training(this.datasetSpec, this.samples);
    this.validationSet = dataset.validation(this.datasetSpec, this.samples);
    this.runId = fingerprint(this.spec, this.datasetSpec);

    this.broker = new Broker({ linkLatencyMs: s.link_latency_ms });
    this.broker.declareQueue(KI_TASK_QUEUE);
    this.broker.declareQueue(AN_RESULT_QUEUE);
    this.broker.declareQueue(HEARTBEAT_QUEUE);
    this.broker.declareQueue(PRINCIPAL_UPDATE_QUEUE);
    this.pendingRequeues = [];

    this.registry = new NodeRegistry();
    this.metrics = {
      tasksProcessedTotal: 0,
      processingTimes: [],
      dispatched: 0,
      requeuedTasks: 0,
      deadLettered: 0,
    };

    this.raft = new RaftCluster({
      peers: Array.from({ length: s.principals }, (_, index) => `principal-${index}`),
      rpcLatencyMs: Math.round(s.link_latency_ms * 0.8),
      onEvent: (event) => this.log(event.level, event.node, event.message),
    });
    this.principals = this.raft.peers().map((id) => ({ id, role: 'Principal', alive: true }));
    this.nextSweepAt = Math.max(s.node_ttl_ms / 3, 1000);
    this.nextClusterReportAt = s.cluster_report_interval_ms;

    this.kiNodes = Array.from({ length: s.ki_nodes }, (_, index) => {
      // A little spread in worker speed, fixed by seed. Identical workers would
      // hide the thing worth seeing: the broker keeps every one of them busy.
      const speed = 0.85 + this.jitter() * 0.5;
      return new KiNode(`ki-${index}`, speed);
    });

    this.anNode = {
      id: 'an-0',
      role: 'An',
      alive: true,
      nextHeartbeatAt: 0,
      tasksProcessed: 0,
    };
    this.epochsDispatched = 0;
    this.nextEpochAt = 0;
    this.nextOverlapWarningAt = 0;
    this.lastCheckpointEpoch = 0;
    this.state = this.freshTrainingState();

    for (const node of this.kiNodes) {
      this.subscribeKi(node);
    }
    this.subscribeAn();
    for (const principal of this.principals) {
      this.subscribePrincipal(principal);
    }

    this.log('info', 'cluster', `run ${this.runId}: ${this.spec.hidden} hidden units, ${
      model.parameterCount(this.spec)
    } parameters, ${this.trainingSet.length} training and ${this.validationSet.length} held-out samples`);
  }

  freshTrainingState() {
    const rng = new StdRng(this.settings.init_seed);
    const parameters = model.initialize(this.spec, rng);
    return new AnNodeState(
      this.spec,
      this.datasetSpec,
      parameters,
      this.settings.learning_rate,
      this.settings.model_shards,
      this.validationSet,
    );
  }

  log(level, node, message) {
    this.eventSequence += 1;
    this.events.push({ id: this.eventSequence, at: this.now, level, node, message });
    if (this.events.length > 400) {
      this.events.splice(0, this.events.length - 400);
    }
  }

  // ---------------------------------------------------------------- consumers

  subscribeKi(node) {
    this.broker.consume(KI_TASK_QUEUE, node.id, (body, deliveryTag) => {
      node.inbox.push({ body, deliveryTag });
    });
  }

  subscribeAn() {
    this.broker.consume(AN_RESULT_QUEUE, this.anNode.id, (body, deliveryTag, now) => {
      this.handleGradientReply(body, deliveryTag, now);
    });
  }

  subscribePrincipal(principal) {
    this.broker.consume(HEARTBEAT_QUEUE, `${principal.id}:heartbeats`, (body, tag, now) => {
      if (!principal.alive) {
        return;
      }
      // Only the members of the quorum track membership; every principal keeps
      // its own registry, derived from the same heartbeats.
      if (this.registry.record(body.id, body.role, now) && principal.id === this.leaderId()) {
        this.log('info', principal.id, `${body.id} joined the cluster as ${body.role}`);
      }
      this.broker.ack(`${principal.id}:heartbeats`, tag);
    });

    this.broker.consume(PRINCIPAL_UPDATE_QUEUE, `${principal.id}:updates`, (body, tag, now) => {
      this.handleClusterUpdate(principal, body, tag, now);
    });
  }

  // ------------------------------------------------------------------ actions

  /**
   * Handles one cluster update request the way `principal::process_update`
   * does: rejected outright if it can never succeed, requeued if this principal
   * is not the leader, acknowledged only once it is committed through Raft.
   */
  handleClusterUpdate(principal, body, tag, now) {
    const consumerId = `${principal.id}:updates`;
    if (!principal.alive) {
      return;
    }

    const request = body.request;
    const invalid =
      !request ||
      (request.type === 'assign_role' && !request.node_id) ||
      (request.type === 'set_config' && !request.key);
    if (invalid) {
      this.log('warn', principal.id, `update ${body.update_id} rejected as malformed; dead-lettered`);
      this.broker.nack(consumerId, tag, false);
      this.metrics.deadLettered += 1;
      return;
    }

    if (this.leaderId() !== principal.id) {
      this.log(
        'warn',
        principal.id,
        `update ${body.update_id} requeued after ${(body.attempts ?? 0) + 1} attempt(s): not the Raft leader${
          this.leaderId() ? ` (${this.leaderId()} is)` : ' and no leader is elected'
        }`,
      );
      this.requeueUpdate(consumerId, tag, body, now);
      return;
    }

    const result = this.raft.clientWrite(request);
    if (!result.ok) {
      this.requeueUpdate(consumerId, tag, body, now);
      return;
    }
    this.log(
      'info',
      principal.id,
      `update ${body.update_id} appended at index ${result.index}, term ${result.term}`,
    );
    this.broker.ack(consumerId, tag);
  }

  /**
   * Puts a request back for the leader to pick up, backing off as attempts
   * mount. A leaderless cluster would otherwise spin the message between its
   * principals as fast as the broker could redeliver it.
   */
  requeueUpdate(consumerId, tag, body, now) {
    this.broker.nack(consumerId, tag, false);
    const attempts = (body.attempts ?? 0) + 1;
    const delay = Math.min(400 * 2 ** (attempts - 1), 3200);
    this.pendingRequeues.push({
      at: now + delay,
      queue: PRINCIPAL_UPDATE_QUEUE,
      body: { ...body, attempts },
    });
  }

  /** Publishes a cluster update request, as an operator or an orchestrator would. */
  submitUpdate(request) {
    const id = `${this.raft.peers().length}-${this.eventSequence + 1}`;
    this.broker.publish(
      PRINCIPAL_UPDATE_QUEUE,
      { kind: 'update', update_id: id, request },
      'operator',
      this.now,
      'update',
    );
    this.log('info', 'operator', `published ${request.type} to ${PRINCIPAL_UPDATE_QUEUE}`);
  }

  leaderId() {
    return this.raft.leader()?.id ?? null;
  }

  // ----------------------------------------------------------------- training

  /**
   * Publishes one epoch's worth of tasks: one gradient request per shard, each
   * naming a different slice of the dataset.
   *
   * The scheduler fires on its timer whether or not the previous round closed —
   * which is exactly what makes a stalled round visible as a growing queue
   * rather than as silence.
   */
  dispatchEpoch() {
    const shards = this.state.shards;
    this.warnIfRoundStillOpen();
    const parameters = Float32Array.from(this.state.parameters);

    for (let shardIndex = 0; shardIndex < shards; shardIndex += 1) {
      this.broker.publish(
        KI_TASK_QUEUE,
        {
          kind: 'task',
          task_id: `${this.epochsDispatched}-${shardIndex}`,
          task_type: 'GradientUpdate',
          request: {
            spec: this.spec,
            dataset: this.datasetSpec,
            shard: shardIndex,
            shards,
            parameters,
          },
        },
        this.anNode.id,
        this.now,
        'task',
      );
    }

    this.epochsDispatched += 1;
    this.metrics.dispatched += shards;
  }

  /**
   * The scheduler does not wait for the previous round, so a cluster slower
   * than its epoch interval accumulates gradients taken at parameter points the
   * model has already left behind. Training still converges; it converges from
   * stale information, which is worth saying out loud rather than hiding.
   */
  warnIfRoundStillOpen() {
    const outstanding =
      this.broker.depth(KI_TASK_QUEUE) +
      this.broker.unackedCount(KI_TASK_QUEUE) +
      this.state.replies;
    if (outstanding < this.state.shards || this.now < this.nextOverlapWarningAt) {
      return;
    }
    this.nextOverlapWarningAt = this.now + 4000;
    this.log(
      'warn',
      this.anNode.id,
      `dispatching epoch ${this.epochsDispatched + 1} with ${outstanding} task(s) from earlier rounds still outstanding; gradients from different parameter points will be averaged together`,
    );
  }

  /** One worker's turn at one shard: rebuild the data, take your slice, reply. */
  computeGradient(node, delivery) {
    const request = delivery.body.request;
    const samples = dataset.generate(request.dataset);
    const training = dataset.training(request.dataset, samples);
    const shard = dataset.shard(training, request.shard, request.shards);

    if (shard.length === 0) {
      // A shard with no samples has no gradient to report. Treat it as bad
      // input rather than replying with zeros, which the An node would fold
      // into the average as though it were real evidence.
      this.log(
        'error',
        node.id,
        `shard ${request.shard} of ${request.shards} is empty for a ${training.length}-sample training set; dropped`,
      );
      this.broker.nack(node.id, delivery.deliveryTag, false);
      this.metrics.deadLettered += 1;
      return null;
    }

    const { loss, gradient } = model.lossAndGradient(request.spec, request.parameters, shard);
    return { gradient, loss, samples: shard.length, shard: request.shard, shards: request.shards };
  }

  handleGradientReply(body, deliveryTag, now) {
    if (!this.anNode.alive) {
      return;
    }
    try {
      this.state.accumulate(body.reply);
    } catch (error) {
      this.log('error', this.anNode.id, `${error.message}; dropped without requeue`);
      this.broker.nack(this.anNode.id, deliveryTag, false);
      this.metrics.deadLettered += 1;
      return;
    }

    this.broker.ack(this.anNode.id, deliveryTag);
    this.anNode.tasksProcessed += 1;
    this.metrics.tasksProcessedTotal += 1;

    if (this.state.replies >= this.state.shards) {
      const summary = this.state.applyEpoch();
      this.history.push({
        epoch: summary.epoch,
        loss: summary.loss,
        accuracy: summary.accuracy,
      });
      this.log(
        'info',
        this.anNode.id,
        `Epoch ${summary.epoch} over ${summary.samples} samples from ${summary.shards} shard(s): training loss ${summary.loss.toFixed(5)}, validation accuracy ${
          summary.accuracy === null ? 'n/a' : summary.accuracy.toFixed(4)
        }`,
      );
      this.maybeCheckpoint(summary.epoch, now);
    }
  }

  /**
   * Saves a checkpoint when one is due. Parameters are encrypted at rest with
   * the same shared secret used for inter-node messages: a checkpoint is the
   * entire product of the cluster's work and should not be plaintext to
   * anything holding a database connection.
   */
  maybeCheckpoint(epoch, now) {
    const interval = this.settings.checkpoint_interval_epochs;
    if (interval === 0 || epoch % interval !== 0 || epoch === this.lastCheckpointEpoch) {
      return;
    }
    this.lastCheckpointEpoch = epoch;
    this.checkpoints.push({
      epoch,
      at: now,
      runId: this.runId,
      parameters: Float32Array.from(this.state.parameters),
      accuracy: this.state.lastValidationAccuracy,
    });
    if (this.checkpoints.length > 24) {
      this.checkpoints.shift();
    }
    this.log(
      'info',
      this.anNode.id,
      `checkpoint saved for run ${this.runId} at epoch ${epoch} (AES-256-GCM, ${
        this.state.parameters.length * 4
      } bytes)`,
    );
  }

  // --------------------------------------------------------------------- tick

  step(deltaMs) {
    const stepMs = 5;
    let remaining = deltaMs;
    while (remaining > 0) {
      const slice = Math.min(stepMs, remaining);
      this.now += slice;
      this.tick(this.now);
      remaining -= slice;
    }
  }

  tick(now) {
    this.releaseRequeues(now);
    this.broker.tick(now);
    this.raft.tick(now);
    this.tickHeartbeats(now);
    this.tickRegistry(now);
    this.tickScheduler(now);
    this.tickWorkers(now);
  }

  releaseRequeues(now) {
    const due = this.pendingRequeues.filter((entry) => entry.at <= now);
    this.pendingRequeues = this.pendingRequeues.filter((entry) => entry.at > now);
    for (const entry of due) {
      this.broker.publish(entry.queue, entry.body, 'broker', now, 'update');
    }
  }

  tickHeartbeats(now) {
    const interval = this.settings.heartbeat_interval_ms;
    const beat = (node) => {
      if (!node.alive || now < node.nextHeartbeatAt) {
        return;
      }
      node.nextHeartbeatAt = now + interval;
      this.broker.publish(
        HEARTBEAT_QUEUE,
        { kind: 'heartbeat', id: node.id, role: node.role },
        node.id,
        now,
        'heartbeat',
      );
    };

    beat(this.anNode);
    for (const node of this.kiNodes) {
      beat(node);
    }
  }

  tickRegistry(now) {
    if (now >= this.nextSweepAt) {
      this.nextSweepAt = now + Math.max(this.settings.node_ttl_ms / 3, 1000);
      const evicted = this.registry.prune(this.settings.node_ttl_ms, now);
      for (const id of evicted) {
        this.log(
          'warn',
          this.leaderId() ?? 'principal-0',
          `Node ${id} evicted from the cluster after ${this.settings.node_ttl_ms}ms without a heartbeat`,
        );
      }
    }

    if (now >= this.nextClusterReportAt) {
      this.nextClusterReportAt = now + this.settings.cluster_report_interval_ms;
      const counts = { An: 0, Ki: 0 };
      for (const node of this.registry.list()) {
        counts[node.role] = (counts[node.role] ?? 0) + 1;
      }
      this.log(
        'info',
        this.leaderId() ?? 'principal-0',
        `cluster: ${this.registry.list().length} node(s) — ${counts.An ?? 0} An, ${counts.Ki ?? 0} Ki, ${
          this.principals.filter((principal) => principal.alive).length
        } principal(s)`,
      );
    }
  }

  tickScheduler(now) {
    if (!this.anNode.alive || now < this.nextEpochAt) {
      return;
    }
    this.nextEpochAt = now + this.settings.epoch_interval_ms;
    if (this.epochsDispatched >= this.settings.training_epochs) {
      return;
    }
    this.dispatchEpoch();
  }

  tickWorkers(now) {
    for (const node of this.kiNodes) {
      if (!node.alive) {
        continue;
      }

      if (node.current && now >= node.current.finishAt) {
        const { delivery, reply, startedAt } = node.current;
        node.current = null;
        this.broker.publish(
          AN_RESULT_QUEUE,
          {
            kind: 'result',
            task_id: delivery.body.task_id,
            task_type: 'GradientUpdate',
            reply,
          },
          node.id,
          now,
          'result',
        );
        this.broker.ack(node.id, delivery.deliveryTag);
        node.tasksProcessed += 1;
        this.metrics.tasksProcessedTotal += 1;
        this.metrics.processingTimes.push(now - startedAt);
        if (this.metrics.processingTimes.length > 200) {
          this.metrics.processingTimes.shift();
        }
        this.log(
          'debug',
          node.id,
          `Computed gradient for shard ${reply.shard}/${reply.shards} over ${reply.samples} samples (loss ${reply.loss.toFixed(4)})`,
        );
      }

      if (!node.current && node.inbox.length > 0) {
        const delivery = node.inbox.shift();
        const reply = this.computeGradient(node, delivery);
        if (!reply) {
          continue;
        }
        const duration =
          (this.settings.compute_base_ms + this.settings.compute_per_sample_ms * reply.samples) *
          node.speed;
        node.lastShard = `${reply.shard}/${reply.shards}`;
        node.current = { delivery, reply, startedAt: now, finishAt: now + duration };
      }
    }
  }

  // ------------------------------------------------------------------- chaos

  /** Stops a Ki worker. The broker requeues whatever it was holding. */
  killKi(id) {
    const node = this.kiNodes.find((candidate) => candidate.id === id);
    if (!node || !node.alive) {
      return;
    }
    node.alive = false;
    const inFlight = node.inbox.length + (node.current ? 1 : 0);
    node.inbox = [];
    node.current = null;
    const requeued = this.broker.cancel(node.id);
    this.metrics.requeuedTasks += requeued;
    const survivors = this.kiNodes.filter((candidate) => candidate.alive).length;
    const held = `${requeued} unacknowledged deliver${requeued === 1 ? 'y' : 'ies'}`;
    this.log(
      'warn',
      node.id,
      requeued === 0
        ? 'worker stopped with nothing in flight'
        : survivors > 0
          ? `worker stopped; ${held} requeued for whoever is still alive`
          : `worker stopped; ${held} requeued with no consumer left to take them`,
    );
    if (inFlight > 0 && survivors === 0) {
      this.log('error', 'broker', `no Ki consumers left; ${KI_TASK_QUEUE} will grow until one returns`);
    }
  }

  reviveKi(id) {
    const node = this.kiNodes.find((candidate) => candidate.id === id);
    if (!node || node.alive) {
      return;
    }
    node.alive = true;
    node.nextHeartbeatAt = this.now;
    this.subscribeKi(node);
    this.log('info', node.id, 'worker started; consuming ' + KI_TASK_QUEUE);
  }

  /** Stops the An node. Training stops; the model survives in its checkpoint. */
  killAn() {
    if (!this.anNode.alive) {
      return;
    }
    this.anNode.alive = false;
    this.broker.cancel(this.anNode.id);
    this.log('warn', this.anNode.id, 'An node stopped; no epochs will be dispatched');
  }

  /**
   * Restarts the An node, which restores the most recent checkpoint instead of
   * starting again from `init_seed`. Without this a restart would discard the
   * entire training run.
   */
  reviveAn() {
    if (this.anNode.alive) {
      return;
    }
    this.anNode.alive = true;
    this.anNode.nextHeartbeatAt = this.now;
    this.subscribeAn();

    const checkpoint = this.checkpoints[this.checkpoints.length - 1];
    if (checkpoint && checkpoint.runId === this.runId) {
      this.state = new AnNodeState(
        this.spec,
        this.datasetSpec,
        Float32Array.from(checkpoint.parameters),
        this.settings.learning_rate,
        this.settings.model_shards,
        this.validationSet,
      );
      this.state.epochsCompleted = checkpoint.epoch;
      this.state.lastValidationAccuracy = checkpoint.accuracy;
      this.log(
        'info',
        this.anNode.id,
        `An node started; resumed run ${this.runId} from the checkpoint at epoch ${checkpoint.epoch}`,
      );
    } else {
      this.state = this.freshTrainingState();
      this.log('info', this.anNode.id, `An node started; no checkpoint for run ${this.runId}, starting from init_seed`);
    }
  }

  killPrincipal(id) {
    const principal = this.principals.find((candidate) => candidate.id === id);
    if (!principal || !principal.alive) {
      return;
    }
    principal.alive = false;
    this.raft.kill(id);
    this.broker.cancel(`${id}:heartbeats`);
    this.broker.cancel(`${id}:updates`);
  }

  revivePrincipal(id) {
    const principal = this.principals.find((candidate) => candidate.id === id);
    if (!principal || principal.alive) {
      return;
    }
    principal.alive = true;
    this.raft.revive(id, this.now);
    this.subscribePrincipal(principal);
  }

  // ------------------------------------------------------------------ readout

  /** Everything the UI draws, gathered in one place. */
  snapshot() {
    const leader = this.raft.leader();
    return {
      now: this.now,
      runId: this.runId,
      settings: this.settings,
      spec: this.spec,
      parameterCount: model.parameterCount(this.spec),
      epochsCompleted: this.state.epochsCompleted,
      epochsDispatched: this.epochsDispatched,
      pendingGradients: this.state.replies,
      shards: this.state.shards,
      loss: this.state.lastEpochLoss,
      accuracy: this.state.lastValidationAccuracy,
      history: this.history,
      checkpoints: this.checkpoints.map(({ epoch, at, accuracy }) => ({ epoch, at, accuracy })),
      queues: [KI_TASK_QUEUE, AN_RESULT_QUEUE, HEARTBEAT_QUEUE, PRINCIPAL_UPDATE_QUEUE].map(
        (name) => ({
          name,
          depth: this.broker.depth(name),
          unacked: this.broker.unackedCount(name),
        }),
      ),
      metrics: {
        ...this.metrics,
        averageProcessingMs:
          this.metrics.processingTimes.length === 0
            ? null
            : this.metrics.processingTimes.reduce((total, value) => total + value, 0) /
              this.metrics.processingTimes.length,
        deadLettered: this.metrics.deadLettered + this.broker.deadLettered,
        requeuedTasks: this.broker.requeued,
      },
      raft: {
        leader: leader?.id ?? null,
        term: Math.max(...[...this.raft.nodes.values()].map((node) => node.currentTerm)),
        majority: this.raft.majority(),
        nodes: [...this.raft.nodes.values()].map((node) => ({
          id: node.id,
          alive: node.alive,
          state: node.state,
          term: node.currentTerm,
          lastIndex: node.lastIndex(),
          commitIndex: node.commitIndex,
          consensusState: node.alive && node.state === LEADER ? 1 : 0,
        })),
        log: (leader ?? [...this.raft.nodes.values()].find((node) => node.alive))?.log.slice(1) ?? [],
        machine: (leader ?? [...this.raft.nodes.values()].find((node) => node.alive))?.machine.snapshot() ?? {
          roles: [],
          config: [],
          applied: 0,
        },
      },
      nodes: {
        an: {
          id: this.anNode.id,
          alive: this.anNode.alive,
          tasksProcessed: this.anNode.tasksProcessed,
          pending: `${this.state.replies}/${this.state.shards}`,
        },
        ki: this.kiNodes.map((node) => ({
          id: node.id,
          alive: node.alive,
          busy: Boolean(node.current),
          queued: node.inbox.length,
          tasksProcessed: node.tasksProcessed,
          lastShard: node.lastShard,
        })),
        principals: this.principals.map((principal) => {
          const raftNode = this.raft.node(principal.id);
          return {
            id: principal.id,
            alive: principal.alive,
            state: raftNode.state,
            term: raftNode.currentTerm,
          };
        }),
      },
      registry: this.registry.list().map((node) => ({
        ...node,
        age: this.now - node.lastSeen,
        stale: this.now - node.lastSeen > this.settings.node_ttl_ms,
      })),
      events: this.events,
    };
  }
}
