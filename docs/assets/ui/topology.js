// The cluster diagram: who is alive, who is busy, and what is on the wire.
//
// Positions are computed from the canvas size rather than fixed, so the same
// drawing works on a phone and on a wide monitor. Every packet drawn is a
// message the simulation is actually carrying — the renderer interpolates
// between the clock it was sent at and the clock it arrives at, and knows
// nothing else about it.

const COLORS = {
  task: '#2dd4bf',
  result: '#38bdf8',
  heartbeat: '#64748b',
  update: '#a78bfa',
  append: '#7c6bd6',
  'append-reply': '#5b5a8c',
  vote: '#fbbf24',
  'vote-reply': '#b08a2a',
};

const LABELS = [
  ['task', 'gradient request'],
  ['result', 'gradient reply'],
  ['heartbeat', 'heartbeat'],
  ['update', 'cluster update'],
  ['append', 'raft append'],
  ['vote', 'raft vote'],
];

function roundedRect(context, x, y, width, height, radius) {
  context.beginPath();
  context.moveTo(x + radius, y);
  context.arcTo(x + width, y, x + width, y + height, radius);
  context.arcTo(x + width, y + height, x, y + height, radius);
  context.arcTo(x, y + height, x, y, radius);
  context.arcTo(x, y, x + width, y, radius);
  context.closePath();
}

export class TopologyView {
  constructor(canvas, { onNodeClick } = {}) {
    this.canvas = canvas;
    this.context = canvas.getContext('2d');
    this.boxes = new Map();
    this.hover = null;
    this.onNodeClick = onNodeClick ?? (() => {});

    canvas.addEventListener('click', (event) => {
      const hit = this.hitTest(event);
      if (hit && hit.clickable) {
        this.onNodeClick(hit.id, hit.kind);
      }
    });
    canvas.addEventListener('mousemove', (event) => {
      const hit = this.hitTest(event);
      this.hover = hit?.clickable ? hit.id : null;
      canvas.style.cursor = this.hover ? 'pointer' : 'default';
    });
    canvas.addEventListener('mouseleave', () => {
      this.hover = null;
    });
  }

  hitTest(event) {
    const rect = this.canvas.getBoundingClientRect();
    const x = event.clientX - rect.left;
    const y = event.clientY - rect.top;
    for (const box of this.boxes.values()) {
      if (x >= box.x && x <= box.x + box.width && y >= box.y && y <= box.y + box.height) {
        return box;
      }
    }
    return null;
  }

  /** Where a packet endpoint sits. Consumer tags carry a `:suffix`. */
  endpoint(id) {
    if (this.boxes.has(id)) {
      const box = this.boxes.get(id);
      return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    }
    const base = String(id).split(':')[0];
    if (this.boxes.has(base)) {
      const box = this.boxes.get(base);
      return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    }
    return null;
  }

  layout(snapshot, width, height) {
    this.boxes.clear();

    // Two arrangements: side by side when there is room, and stacked into
    // bands when there is not. Both keep the broker in the middle, because
    // everything except Raft goes through it.
    const compact = width < 640;
    const nodeWidth = Math.max(Math.min(width / 8.5, 124), compact ? 96 : 92);
    const nodeHeight = compact ? 40 : 50;
    const brokerWidth = compact ? width - 24 : Math.min(width * 0.36, 320);
    const brokerHeight = compact ? 70 : 88;
    const brokerY = compact ? height * 0.42 : height * 0.44;
    const principalY = compact ? 4 : height * 0.1;

    const principals = snapshot.nodes.principals;
    const principalColumns = compact ? Math.min(principals.length, 3) : principals.length;
    const principalRows = Math.ceil(principals.length / principalColumns);
    const principalGap = compact ? 8 : 22;
    principals.forEach((principal, index) => {
      const row = Math.floor(index / principalColumns);
      const column = index % principalColumns;
      const inRow = Math.min(principalColumns, principals.length - row * principalColumns);
      const rowWidth = inRow * nodeWidth + (inRow - 1) * principalGap;
      const start = width * 0.5 - rowWidth / 2;
      this.boxes.set(principal.id, {
        id: principal.id,
        kind: 'principal',
        clickable: true,
        label: principal.id.replace('principal-', 'principal '),
        sub: principal.alive
          ? compact
            ? `${principal.state} t${principal.term}`
            : `${principal.state} · term ${principal.term}`
          : 'stopped',
        alive: principal.alive,
        accent: principal.state === 'leader' ? '#2dd4bf' : '#a78bfa',
        x: start + column * (nodeWidth + principalGap),
        y: principalY + row * (nodeHeight + 8),
        width: nodeWidth,
        height: nodeHeight,
      });
    });

    this.boxes.set('broker', {
      id: 'broker',
      kind: 'broker',
      clickable: false,
      label: 'broker',
      alive: true,
      accent: '#94a3b8',
      x: width * 0.5 - brokerWidth / 2,
      y: brokerY,
      width: brokerWidth,
      height: brokerHeight,
    });

    // The An node publishes work and the operator publishes cluster updates;
    // both sit beside the broker when there is width for it, above it when not.
    const sideY = compact
      ? brokerY - nodeHeight - 12
      : brokerY + brokerHeight / 2 - nodeHeight / 2;
    const sideInset = compact ? 10 : Math.max(width * 0.02, 8);

    this.boxes.set('an-0', {
      id: 'an-0',
      kind: 'an',
      clickable: true,
      label: snapshot.nodes.an.id,
      sub: snapshot.nodes.an.alive ? `${snapshot.nodes.an.pending} gradients` : 'stopped',
      alive: snapshot.nodes.an.alive,
      accent: '#38bdf8',
      x: sideInset,
      y: sideY,
      width: nodeWidth,
      height: nodeHeight,
    });

    this.boxes.set('operator', {
      id: 'operator',
      kind: 'operator',
      clickable: false,
      label: 'operator',
      sub: 'update queue',
      alive: true,
      accent: '#a78bfa',
      x: width - nodeWidth - sideInset,
      y: sideY,
      width: nodeWidth,
      height: nodeHeight,
    });

    const workers = snapshot.nodes.ki;
    const columns = Math.max(Math.min(workers.length, compact ? 3 : 8), 1);
    const rows = Math.ceil(workers.length / columns);
    const gap = compact ? 8 : 12;
    const rowHeight = nodeHeight + 8;
    const workerTop = Math.max(
      height - rows * rowHeight - (compact ? 4 : 12),
      brokerY + brokerHeight + 10,
    );
    workers.forEach((worker, index) => {
      const row = Math.floor(index / columns);
      const column = index % columns;
      const inRow = Math.min(columns, workers.length - row * columns);
      const rowWidth = inRow * nodeWidth + (inRow - 1) * gap;
      const start = width * 0.5 - rowWidth / 2;
      this.boxes.set(worker.id, {
        id: worker.id,
        kind: 'ki',
        clickable: true,
        label: worker.id,
        sub: worker.alive ? `${worker.tasksProcessed} tasks` : 'stopped',
        alive: worker.alive,
        busy: worker.busy,
        queued: worker.queued,
        accent: '#2dd4bf',
        x: start + column * (nodeWidth + gap),
        y: workerTop + row * rowHeight,
        width: nodeWidth,
        height: nodeHeight,
      });
    });
  }

  drawLinks(context, snapshot) {
    const broker = this.boxes.get('broker');
    const center = { x: broker.x + broker.width / 2, y: broker.y + broker.height / 2 };

    context.save();
    context.strokeStyle = 'rgba(148, 163, 184, 0.16)';
    context.lineWidth = 1;

    for (const box of this.boxes.values()) {
      if (box.kind === 'broker') {
        continue;
      }
      context.beginPath();
      context.moveTo(box.x + box.width / 2, box.y + box.height / 2);
      context.lineTo(center.x, center.y);
      context.stroke();
    }

    // Principals talk to each other directly over HTTP for Raft RPCs, not
    // through the broker.
    const principals = snapshot.nodes.principals.map((principal) => this.boxes.get(principal.id));
    context.strokeStyle = 'rgba(167, 139, 250, 0.2)';
    context.setLineDash([4, 4]);
    for (let index = 0; index < principals.length; index += 1) {
      for (let other = index + 1; other < principals.length; other += 1) {
        context.beginPath();
        context.moveTo(principals[index].x + principals[index].width / 2, principals[index].y + 10);
        context.lineTo(principals[other].x + principals[other].width / 2, principals[other].y + 10);
        context.stroke();
      }
    }
    context.setLineDash([]);
    context.restore();
  }

  drawBroker(context, snapshot) {
    const box = this.boxes.get('broker');
    context.save();
    roundedRect(context, box.x, box.y, box.width, box.height, 12);
    context.fillStyle = 'rgba(15, 22, 38, 0.95)';
    context.fill();
    context.strokeStyle = 'rgba(148, 163, 184, 0.35)';
    context.lineWidth = 1;
    context.stroke();

    context.fillStyle = '#94a3b8';
    context.font = '600 11px ui-monospace, monospace';
    context.textAlign = 'center';
    context.fillText('message broker', box.x + box.width / 2, box.y + 17);

    const queues = snapshot.queues.filter((queue) => queue.name !== 'heartbeat_queue');
    const verbose = box.width > 260;
    const rowHeight = 15;
    context.textAlign = 'left';
    context.font = '10px ui-monospace, monospace';
    queues.forEach((queue, index) => {
      const y = box.y + 34 + index * rowHeight;
      context.fillStyle = queue.depth > 0 ? '#fbbf24' : '#64748b';
      context.fillText(queue.name.replace('_queue', ''), box.x + 14, y);
      context.textAlign = 'right';
      context.fillStyle = queue.depth > 0 ? '#fbbf24' : '#475569';
      context.fillText(
        verbose
          ? `${queue.depth} ready · ${queue.unacked} unacked`
          : `${queue.depth} / ${queue.unacked}`,
        box.x + box.width - 14,
        y,
      );
      context.textAlign = 'left';
    });
    context.restore();
  }

  drawNode(context, box, now) {
    context.save();
    roundedRect(context, box.x, box.y, box.width, box.height, 10);
    context.fillStyle = box.alive ? 'rgba(15, 22, 38, 0.95)' : 'rgba(15, 22, 38, 0.55)';
    context.fill();

    if (!box.alive) {
      context.setLineDash([3, 3]);
    }
    const highlighted = this.hover === box.id;
    context.strokeStyle = box.alive
      ? highlighted
        ? '#e6ecf7'
        : `${box.accent}88`
      : 'rgba(251, 113, 133, 0.65)';
    context.lineWidth = highlighted ? 1.6 : 1;
    context.stroke();
    context.setLineDash([]);

    context.textAlign = 'center';
    context.fillStyle = box.alive ? '#e6ecf7' : 'rgba(230, 236, 247, 0.5)';
    context.font = '600 11.5px ui-monospace, monospace';
    context.fillText(box.label, box.x + box.width / 2, box.y + box.height / 2 - 1);

    context.fillStyle = box.alive ? '#64748b' : 'rgba(251, 113, 133, 0.8)';
    context.font = '10px ui-monospace, monospace';
    context.fillText(box.sub ?? '', box.x + box.width / 2, box.y + box.height / 2 + 13);

    // A worker mid-computation gets a bar; the queue it has locally gets a
    // count, because a node holding four deliveries is a node whose death
    // requeues four of them.
    if (box.busy) {
      context.fillStyle = box.accent;
      context.fillRect(box.x + 8, box.y + box.height - 5, (box.width - 16) * 0.999, 2);
      context.globalAlpha = 0.35 + 0.35 * Math.sin(now / 160);
      context.fillRect(box.x + 8, box.y + box.height - 5, box.width - 16, 2);
      context.globalAlpha = 1;
    }
    if (box.queued > 0) {
      context.fillStyle = '#fbbf24';
      context.textAlign = 'right';
      context.font = '9.5px ui-monospace, monospace';
      context.fillText(`+${box.queued}`, box.x + box.width - 6, box.y + 12);
    }
    context.restore();
  }

  drawPacket(context, from, to, progress, color) {
    if (!from || !to) {
      return;
    }
    const x = from.x + (to.x - from.x) * progress;
    const y = from.y + (to.y - from.y) * progress;
    const trail = Math.max(progress - 0.08, 0);
    const trailX = from.x + (to.x - from.x) * trail;
    const trailY = from.y + (to.y - from.y) * trail;

    context.save();
    context.strokeStyle = color;
    context.globalAlpha = 0.5;
    context.lineWidth = 1.6;
    context.beginPath();
    context.moveTo(trailX, trailY);
    context.lineTo(x, y);
    context.stroke();

    context.globalAlpha = 1;
    context.fillStyle = color;
    context.beginPath();
    context.arc(x, y, 3, 0, Math.PI * 2);
    context.fill();
    context.restore();
  }

  draw(snapshot, simulation) {
    const canvas = this.canvas;
    const ratio = window.devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;

    if (canvas.width !== Math.round(width * ratio) || canvas.height !== Math.round(height * ratio)) {
      canvas.width = Math.round(width * ratio);
      canvas.height = Math.round(height * ratio);
    }

    const context = this.context;
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.clearRect(0, 0, width, height);

    this.layout(snapshot, width, height);
    this.drawLinks(context, snapshot);
    this.drawBroker(context, snapshot);

    for (const box of this.boxes.values()) {
      if (box.kind !== 'broker') {
        this.drawNode(context, box, snapshot.now);
      }
    }

    const now = snapshot.now;
    for (const packet of simulation.broker.packets) {
      this.drawPacket(
        context,
        this.endpoint(packet.from),
        this.endpoint(packet.to),
        packet.progress(now),
        COLORS[packet.kind] ?? '#94a3b8',
      );
    }
    for (const packet of simulation.raft.packets) {
      const span = packet.deliverAt - packet.sentAt;
      const progress = span <= 0 ? 1 : Math.min(Math.max((now - packet.sentAt) / span, 0), 1);
      this.drawPacket(
        context,
        this.endpoint(packet.from),
        this.endpoint(packet.to),
        progress,
        COLORS[packet.kind] ?? '#a78bfa',
      );
    }
  }

  static legend() {
    return LABELS.map(([kind, label]) => ({ color: COLORS[kind], label }));
  }
}
