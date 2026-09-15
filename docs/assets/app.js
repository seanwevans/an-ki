// Wiring: one simulation, one animation loop, and the controls that poke at it.

import { Simulation, DEFAULT_SETTINGS } from './sim/cluster.js';
import * as dataset from './sim/dataset.js';
import * as model from './sim/model.js';
import { StdRng } from './sim/rng.js';
import { TopologyView } from './ui/topology.js';
import { TrainingChart, BoundaryView } from './ui/charts.js';
import {
  EventLog,
  renderBrokerStats,
  renderLegend,
  renderMapping,
  renderMetrics,
  renderModelStats,
  renderQueues,
  renderRaft,
  renderRegistry,
} from './ui/panels.js';

const SPEEDS = [0.5, 1, 2, 5, 10];

/** The settings worth exposing, with the reason each one matters. */
const FIELDS = [
  {
    key: 'model_shards',
    label: 'model_shards',
    min: 1,
    max: 12,
    step: 1,
    hint: 'Tasks dispatched per epoch, and gradients required to close a round.',
  },
  {
    key: 'ki_nodes',
    label: 'ki_nodes',
    min: 1,
    max: 8,
    step: 1,
    hint: 'Workers consuming the same queue. The broker spreads shards across them.',
  },
  {
    key: 'principals',
    label: 'principals',
    min: 1,
    max: 7,
    step: 2,
    hint: 'Raft members. Odd numbers only: 3 tolerate one failure, 5 tolerate two.',
  },
  {
    key: 'hidden_units',
    label: 'hidden_units',
    min: 2,
    max: 32,
    step: 2,
    hint: 'Hidden layer width. The parameter count follows from it.',
  },
  {
    key: 'learning_rate',
    label: 'learning_rate',
    min: 0.1,
    max: 3,
    step: 0.1,
    hint: 'Step size applied to the averaged gradient each epoch.',
  },
  {
    key: 'dataset_samples',
    label: 'dataset_samples',
    min: 128,
    max: 2048,
    step: 128,
    hint: 'Samples generated from the seed. A fifth is held out for evaluation.',
  },
  {
    key: 'epoch_interval_ms',
    label: 'epoch_interval_ms',
    min: 150,
    max: 1200,
    step: 50,
    hint: 'Delay between epochs. Dispatch faster than a round closes and gradients mix.',
  },
  {
    key: 'training_epochs',
    label: 'training_epochs',
    min: 50,
    max: 800,
    step: 50,
    hint: 'Epochs to dispatch before the scheduler stops.',
  },
  {
    key: 'link_latency_ms',
    label: 'link latency',
    min: 5,
    max: 140,
    step: 5,
    hint: 'Simulated network delay each way. Not a setting the cluster has — it is how fast packets move here.',
  },
];

const elements = {
  play: document.getElementById('play'),
  reset: document.getElementById('reset'),
  speed: document.getElementById('speed'),
  epoch: document.getElementById('stat-epoch'),
  loss: document.getElementById('stat-loss'),
  accuracy: document.getElementById('stat-accuracy'),
  clock: document.getElementById('stat-clock'),
  topology: document.getElementById('topology'),
  legend: document.getElementById('topology-legend'),
  boundary: document.getElementById('boundary'),
  chart: document.getElementById('chart'),
  queues: document.getElementById('queues'),
  brokerStats: document.getElementById('broker-stats'),
  modelStats: document.getElementById('model-stats'),
  metrics: document.getElementById('metrics'),
  raftNodes: document.getElementById('raft-nodes'),
  raftLog: document.getElementById('raft-log'),
  raftMachine: document.getElementById('raft-machine'),
  registry: document.querySelector('#registry tbody'),
  ttlLabel: document.getElementById('ttl-label'),
  events: document.getElementById('events'),
  logFilter: document.getElementById('log-filter'),
  chaos: document.getElementById('chaos'),
  settings: document.getElementById('settings'),
  mapping: document.getElementById('mapping'),
  conformance: document.getElementById('conformance'),
};

const settings = { ...DEFAULT_SETTINGS };
let simulation = new Simulation(settings);
let running = true;
let speed = 2;
let lastFrame = performance.now();
let lastPanelRender = 0;
const scheduled = [];

const topology = new TopologyView(elements.topology, {
  onNodeClick: (id) => toggleNode(id),
});
const chart = new TrainingChart(elements.chart);
const boundary = new BoundaryView(elements.boundary);
const eventLog = new EventLog(elements.events);

function toggleNode(id) {
  if (id === 'an-0') {
    if (simulation.anNode.alive) {
      simulation.killAn();
    } else {
      simulation.reviveAn();
    }
    return;
  }
  if (id.startsWith('principal-')) {
    const principal = simulation.principals.find((candidate) => candidate.id === id);
    if (principal?.alive) {
      simulation.killPrincipal(id);
    } else {
      simulation.revivePrincipal(id);
    }
    return;
  }
  const worker = simulation.kiNodes.find((candidate) => candidate.id === id);
  if (worker?.alive) {
    simulation.killKi(id);
  } else if (worker) {
    simulation.reviveKi(id);
  }
}

/** Chaos buttons. Each one is a failure the system claims to survive. */
const SCENARIOS = [
  {
    title: 'Stop a worker mid-round',
    detail:
      'The broker requeues the delivery it was holding and another worker picks up that shard. The round finishes late, not never.',
    run() {
      const busy =
        simulation.kiNodes.find((node) => node.alive && node.current) ??
        simulation.kiNodes.find((node) => node.alive);
      if (busy) {
        simulation.killKi(busy.id);
      }
    },
  },
  {
    title: 'Stop every worker',
    detail:
      'Nothing consumes ki_task_queue, so it grows while the An node waits on a set of gradients that will never be complete.',
    run() {
      for (const node of simulation.kiNodes) {
        simulation.killKi(node.id);
      }
    },
  },
  {
    title: 'Stop the Raft leader',
    detail:
      'The survivors notice the missing heartbeats, hold an election, and a new term opens. Cluster updates requeue until it does.',
    run() {
      const leader = simulation.leaderId();
      if (leader) {
        simulation.killPrincipal(leader);
      }
    },
  },
  {
    title: 'Take out the quorum',
    detail:
      'With a minority left, the survivor campaigns and never wins. Nothing can be committed until a majority returns.',
    run() {
      const alive = simulation.principals.filter((principal) => principal.alive);
      const target = alive.length - (Math.floor(simulation.principals.length / 2) + 1) + 1;
      for (let index = 0; index < target; index += 1) {
        simulation.killPrincipal(alive[index].id);
      }
    },
  },
  {
    title: 'Restart the An node',
    detail:
      'It comes back on the most recent checkpoint rather than from init_seed, so the run keeps its progress.',
    run() {
      simulation.killAn();
      scheduled.push({ at: simulation.now + 2500, action: () => simulation.reviveAn() });
    },
  },
  {
    title: 'Publish a cluster update',
    detail:
      'set_config goes onto principal_update_queue. Only the leader may accept it; anyone else requeues it.',
    run() {
      const value = String(1 + Math.floor(Math.random() * 8));
      simulation.submitUpdate({ type: 'set_config', key: 'model_shards', value });
    },
  },
  {
    title: 'Publish a malformed update',
    detail:
      'A request that can never succeed is dead-lettered rather than requeued — retrying it would only burn the queue.',
    run() {
      simulation.submitUpdate({ type: 'set_config', key: '', value: 'nonsense' });
    },
  },
  {
    title: 'Bring everything back',
    detail: 'Every stopped node restarts. Workers rejoin the queue; principals reload their logs.',
    run() {
      for (const node of simulation.kiNodes) {
        simulation.reviveKi(node.id);
      }
      for (const principal of simulation.principals) {
        simulation.revivePrincipal(principal.id);
      }
      simulation.reviveAn();
    },
  },
];

function buildSpeedControl() {
  elements.speed.innerHTML = SPEEDS.map(
    (value) =>
      `<button type="button" data-speed="${value}" aria-pressed="${value === speed}">${value}×</button>`,
  ).join('');
  elements.speed.querySelectorAll('button').forEach((button) => {
    button.addEventListener('click', () => {
      speed = Number(button.dataset.speed);
      elements.speed
        .querySelectorAll('button')
        .forEach((other) => other.setAttribute('aria-pressed', String(Number(other.dataset.speed) === speed)));
    });
  });
}

function buildChaosControls() {
  elements.chaos.innerHTML = SCENARIOS.map(
    (scenario, index) =>
      `<button type="button" data-scenario="${index}">
        <strong>${scenario.title}</strong>
        <small>${scenario.detail}</small>
      </button>`,
  ).join('');
  elements.chaos.querySelectorAll('button').forEach((button) => {
    button.addEventListener('click', () => {
      SCENARIOS[Number(button.dataset.scenario)].run();
    });
  });
}

function buildSettingsControls() {
  elements.settings.innerHTML = FIELDS.map(
    (field) => `
      <div class="field">
        <label for="field-${field.key}">
          ${field.label}
          <span id="value-${field.key}">${settings[field.key]}</span>
        </label>
        <input
          id="field-${field.key}"
          type="range"
          min="${field.min}"
          max="${field.max}"
          step="${field.step}"
          value="${settings[field.key]}"
        />
        <small>${field.hint}</small>
      </div>`,
  ).join('');

  for (const field of FIELDS) {
    const input = document.getElementById(`field-${field.key}`);
    const output = document.getElementById(`value-${field.key}`);
    input.addEventListener('input', () => {
      output.textContent = input.value;
    });
    input.addEventListener('change', () => {
      settings[field.key] = Number(input.value);
      restart();
    });
  }
}

function buildLogFilter() {
  const levels = ['debug', 'info', 'warn', 'error'];
  elements.logFilter.innerHTML = levels
    .map(
      (level) =>
        `<button type="button" data-level="${level}" aria-pressed="${level !== 'debug'}">${level}</button>`,
    )
    .join('');
  elements.logFilter.querySelectorAll('button').forEach((button) => {
    button.addEventListener('click', () => {
      const enabled = button.getAttribute('aria-pressed') !== 'true';
      button.setAttribute('aria-pressed', String(enabled));
      eventLog.setLevel(button.dataset.level, enabled);
    });
  });
}

function restart() {
  simulation = new Simulation(settings);
  scheduled.length = 0;
  eventLog.reset();
  boundary.lastDrawnEpoch = -1;
  elements.ttlLabel.textContent = `${settings.node_ttl_ms}ms`;
}

function setRunning(next) {
  running = next;
  elements.play.textContent = running ? 'Pause' : 'Play';
  elements.play.setAttribute('aria-pressed', String(running));
}

/**
 * Checks the browser's port of the generator against values printed by the
 * Rust crate. If this badge ever goes red, the page is training on data the
 * cluster would not recognise.
 */
async function verifyAgainstRust() {
  const badge = elements.conformance;
  try {
    const response = await fetch('assets/sim/fixtures/reference.json', { cache: 'no-store' });
    if (!response.ok) {
      throw new Error(`fixture returned ${response.status}`);
    }
    const reference = await response.json();

    const spec = dataset.datasetSpec(
      reference.dataset.samples,
      reference.dataset.seed,
      reference.dataset.validation_samples,
    );
    const samples = dataset.generate(spec);
    const checksum = samples.reduce(
      (total, sample) => total + sample.features[0] + sample.features[1],
      0,
    );
    const modelSpec = model.mlpSpec(
      reference.model.inputs,
      reference.model.hidden,
      reference.model.outputs,
    );
    const parameters = model.initialize(modelSpec, new StdRng(reference.model.init_seed));

    const matches =
      checksum === reference.feature_checksum &&
      samples.filter((sample) => sample.label === 1).length === reference.inside_count &&
      reference.initial_parameters.every(
        (value, index) => parameters[index] === Math.fround(value),
      );

    badge.classList.remove('pill-quiet');
    badge.classList.add(matches ? 'pill-ok' : 'pill-bad');
    badge.textContent = matches
      ? 'dataset + weights match the Rust reference'
      : 'diverges from the Rust reference';
  } catch (error) {
    badge.textContent = 'reference check needs an http server';
    badge.title = String(error);
  }
}

function renderPanels(snapshot) {
  elements.epoch.textContent = `${snapshot.epochsCompleted} / ${snapshot.settings.training_epochs}`;
  elements.loss.textContent = snapshot.loss === null ? '—' : snapshot.loss.toFixed(4);
  elements.accuracy.textContent =
    snapshot.accuracy === null ? '—' : `${(snapshot.accuracy * 100).toFixed(1)}%`;
  elements.clock.textContent = `${(snapshot.now / 1000).toFixed(1)}s`;

  renderQueues(elements.queues, snapshot);
  renderBrokerStats(elements.brokerStats, snapshot);
  renderModelStats(elements.modelStats, snapshot);
  renderMetrics(elements.metrics, snapshot);
  renderRaft(elements.raftNodes, elements.raftLog, elements.raftMachine, snapshot);
  renderRegistry(elements.registry, snapshot);
  eventLog.render(snapshot);
}

function frame(timestamp) {
  const elapsed = Math.min(timestamp - lastFrame, 120);
  lastFrame = timestamp;

  if (running) {
    simulation.step(elapsed * speed);
    while (scheduled.length > 0 && scheduled[0].at <= simulation.now) {
      scheduled.shift().action();
    }
  }

  const snapshot = simulation.snapshot();
  topology.draw(snapshot, simulation);
  boundary.draw(snapshot, simulation);
  chart.draw(snapshot);

  // The text panels cost layout, so they update at reading speed rather than
  // at frame rate.
  if (timestamp - lastPanelRender > 120) {
    lastPanelRender = timestamp;
    renderPanels(snapshot);
  }

  requestAnimationFrame(frame);
}

elements.play.addEventListener('click', () => setRunning(!running));
elements.reset.addEventListener('click', () => restart());
document.addEventListener('keydown', (event) => {
  if (event.code === 'Space' && !['INPUT', 'BUTTON', 'TEXTAREA'].includes(event.target.tagName)) {
    event.preventDefault();
    setRunning(!running);
  }
});

buildSpeedControl();
buildChaosControls();
buildSettingsControls();
buildLogFilter();
renderMapping(elements.mapping);
renderLegend(elements.legend, TopologyView.legend());
elements.ttlLabel.textContent = `${settings.node_ttl_ms}ms`;
setRunning(true);
verifyAgainstRust();
requestAnimationFrame(frame);
