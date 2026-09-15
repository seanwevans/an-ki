// The text side of the dashboard: queues, consensus, membership, metrics and
// the log tail. Canvas panels redraw every frame; these redraw a few times a
// second, which is as fast as anyone can read them.

const SOURCE = 'https://github.com/seanwevans/an-ki/blob/main/';

const MAPPING = [
  ['src/scheduler.rs', 'builds and publishes each epoch of gradient requests'],
  ['src/an_node.rs', 'aggregates gradients, steps the model, checkpoints it'],
  ['src/ki_node.rs', 'executes one shard and replies with its gradient'],
  ['src/model.rs', 'the MLP: forward pass, backpropagation, initialization'],
  ['src/dataset.rs', 'deterministic data and disjoint shard assignment'],
  ['src/raft_store.rs', 'replicated cluster state and its durable storage'],
  ['src/raft_node.rs', 'elections, leadership, and cluster bootstrap'],
  ['src/principal.rs', 'validates cluster updates and commits them through Raft'],
  ['src/node_registry.rs', 'membership derived from heartbeats alone'],
  ['src/health.rs', 'heartbeat publishing and the health monitor'],
  ['src/messaging.rs', 'queue declaration, publish, consume, encryption'],
  ['src/checkpoint.rs', 'encrypted model checkpoints, keyed by run fingerprint'],
];

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function formatSeconds(ms) {
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatNumber(value, digits = 0) {
  return value.toLocaleString(undefined, {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

export function renderQueues(element, snapshot) {
  const busiest = Math.max(
    8,
    ...snapshot.queues.map((queue) => queue.depth + queue.unacked),
  );
  element.innerHTML = snapshot.queues
    .map((queue) => {
      const ready = (queue.depth / busiest) * 100;
      const unacked = (queue.unacked / busiest) * 100;
      return `
        <div class="queue">
          <span class="queue-name">${escapeHtml(queue.name)}</span>
          <span class="queue-counts">${queue.depth} ready · ${queue.unacked} unacked</span>
          <span class="queue-bar">
            <i class="ready" style="width:${ready.toFixed(2)}%"></i>
            <i class="unacked" style="width:${unacked.toFixed(2)}%"></i>
          </span>
        </div>`;
    })
    .join('');
}

function statRows(rows) {
  return rows
    .map(
      ([label, value, tone]) =>
        `<dt>${escapeHtml(label)}</dt><dd class="${tone ?? ''}">${escapeHtml(value)}</dd>`,
    )
    .join('');
}

export function renderBrokerStats(element, snapshot) {
  const metrics = snapshot.metrics;
  element.innerHTML = statRows([
    ['tasks dispatched', formatNumber(metrics.dispatched)],
    ['requeued after a failure', formatNumber(metrics.requeuedTasks), metrics.requeuedTasks > 0 ? 'warn' : ''],
    ['dead-lettered', formatNumber(metrics.deadLettered), metrics.deadLettered > 0 ? 'bad' : ''],
    ['link latency', `${snapshot.settings.link_latency_ms}ms`],
  ]);
}

export function renderModelStats(element, snapshot) {
  element.innerHTML = statRows([
    ['run', snapshot.runId],
    ['shape', `${snapshot.spec.inputs}→${snapshot.spec.hidden}→${snapshot.spec.outputs}`],
    ['parameters', formatNumber(snapshot.parameterCount)],
    ['shards per epoch', formatNumber(snapshot.shards)],
    [
      'gradients this round',
      `${snapshot.pendingGradients}/${snapshot.shards}`,
      snapshot.pendingGradients > 0 ? 'warn' : '',
    ],
    ['checkpoints', formatNumber(snapshot.checkpoints.length)],
  ]);
}

export function renderMetrics(element, snapshot) {
  const metrics = snapshot.metrics;
  const leader = snapshot.raft.leader;
  element.innerHTML = statRows([
    ['tasks_processed_total', formatNumber(metrics.tasksProcessedTotal)],
    [
      'processing time (mean)',
      metrics.averageProcessingMs === null ? '—' : `${metrics.averageProcessingMs.toFixed(0)}ms`,
    ],
    [
      'node_status up',
      `${
        snapshot.nodes.ki.filter((node) => node.alive).length +
        (snapshot.nodes.an.alive ? 1 : 0) +
        snapshot.nodes.principals.filter((node) => node.alive).length
      }/${snapshot.nodes.ki.length + 1 + snapshot.nodes.principals.length}`,
    ],
    ['consensus_state = 1', leader ?? 'nobody', leader ? 'good' : 'bad'],
    ['epochs completed', formatNumber(snapshot.epochsCompleted)],
    ['epochs dispatched', formatNumber(snapshot.epochsDispatched)],
    [
      'training loss',
      snapshot.loss === null ? '—' : snapshot.loss.toFixed(5),
    ],
    [
      'held-out accuracy',
      snapshot.accuracy === null ? '—' : `${(snapshot.accuracy * 100).toFixed(2)}%`,
      snapshot.accuracy !== null && snapshot.accuracy > 0.95 ? 'good' : '',
    ],
  ]);
}

export function renderRaft(nodesElement, logElement, machineElement, snapshot) {
  const raft = snapshot.raft;

  nodesElement.innerHTML = raft.nodes
    .map((node) => {
      const classes = ['raft-node'];
      if (node.state === 'leader' && node.alive) {
        classes.push('leader');
      }
      if (!node.alive) {
        classes.push('down');
      }
      const dot = node.alive ? node.state : 'down';
      const state = node.alive ? node.state : 'stopped';
      return `
        <div class="${classes.join(' ')}">
          <span class="dot ${dot}"></span>
          <span>${escapeHtml(node.id)} <span style="color:var(--text-faint)">${escapeHtml(state)}</span></span>
          <span class="meta">term ${node.term} · log ${node.lastIndex} · commit ${node.commitIndex}</span>
        </div>`;
    })
    .join('');

  const commitIndex = Math.max(...raft.nodes.filter((node) => node.alive).map((node) => node.commitIndex), 0);
  const entries = raft.log.slice(-12);
  logElement.innerHTML =
    entries.length === 0
      ? '<li style="border:0;background:none">no entries yet</li>'
      : entries
          .map((entry) => {
            const description = entry.request
              ? `${entry.request.type} ${
                  entry.request.node_id ?? `${entry.request.key}=${entry.request.value}`
                }`
              : 'leader established';
            const committed = entry.index <= commitIndex;
            return `
              <li class="${committed ? '' : 'uncommitted'}">
                <span class="index">#${entry.index}</span>
                <span class="term">t${entry.term}</span>
                <span>${escapeHtml(description)}</span>
              </li>`;
          })
          .join('');

  const rows = [
    ['entries applied', formatNumber(raft.machine.applied)],
    ['majority needed', `${raft.majority} of ${raft.nodes.length}`],
  ];
  for (const [nodeId, role] of raft.machine.roles) {
    rows.push([`role: ${nodeId}`, role]);
  }
  for (const [key, value] of raft.machine.config) {
    rows.push([`config: ${key}`, value]);
  }
  machineElement.innerHTML = statRows(rows);
}

export function renderRegistry(tbody, snapshot) {
  if (snapshot.registry.length === 0) {
    tbody.innerHTML = '<tr class="empty"><td colspan="4">no node has heartbeat yet</td></tr>';
    return;
  }
  tbody.innerHTML = snapshot.registry
    .slice()
    .sort((left, right) => left.id.localeCompare(right.id))
    .map(
      (node) => `
        <tr>
          <td>${escapeHtml(node.id)}</td>
          <td>${escapeHtml(node.role)}</td>
          <td>${formatSeconds(node.age)} ago</td>
          <td class="${node.stale ? 'state-stale' : 'state-fresh'}">${
            node.stale ? 'stale' : 'live'
          }</td>
        </tr>`,
    )
    .join('');
}

/** The log tail. Appends rather than rebuilding, so scrolling back stays possible. */
export class EventLog {
  constructor(element) {
    this.element = element;
    this.lastId = 0;
    this.levels = new Set(['info', 'warn', 'error']);
  }

  setLevel(level, enabled) {
    if (enabled) {
      this.levels.add(level);
    } else {
      this.levels.delete(level);
    }
    this.reset();
  }

  reset() {
    this.element.innerHTML = '';
    this.lastId = 0;
  }

  render(snapshot) {
    const pinned =
      this.element.scrollTop + this.element.clientHeight >= this.element.scrollHeight - 24;

    const fresh = snapshot.events.filter(
      (event) => event.id > this.lastId && this.levels.has(event.level),
    );
    if (fresh.length > 0) {
      this.lastId = snapshot.events[snapshot.events.length - 1].id;
      this.element.insertAdjacentHTML(
        'beforeend',
        fresh
          .map(
            (event) => `
              <li>
                <span class="at">${formatSeconds(event.at)}</span>
                <span class="level-${event.level}">${event.level.toUpperCase()}</span>
                <span class="node">${escapeHtml(event.node)}</span>
                <span>${escapeHtml(event.message)}</span>
              </li>`,
          )
          .join(''),
      );

      while (this.element.childElementCount > 300) {
        this.element.removeChild(this.element.firstElementChild);
      }
      if (pinned) {
        this.element.scrollTop = this.element.scrollHeight;
      }
    }
  }
}

export function renderMapping(element) {
  element.innerHTML = MAPPING.map(
    ([path, description]) => `
      <li>
        <a href="${SOURCE}${path}">${escapeHtml(path.replace('src/', ''))}</a>
        <span>${escapeHtml(description)}</span>
      </li>`,
  ).join('');
}

export function renderLegend(element, entries) {
  element.innerHTML = entries
    .map(
      (entry) =>
        `<span class="key"><i style="background:${entry.color}"></i>${escapeHtml(entry.label)}</span>`,
    )
    .join('');
}
