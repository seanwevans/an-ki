// A model of the message broker the cluster runs on.
//
// Only the parts that change what the cluster does are modelled, but those are
// modelled properly, because they are the parts the system's fault tolerance
// rests on:
//
//   * Every Ki node consumes from the same `ki_task_queue`, so the broker —
//     not the An node — decides which worker gets which shard.
//   * A delivery is unacknowledged until the worker has published its reply.
//     A consumer that disappears takes no work with it: the broker requeues
//     everything it was holding, and another worker picks it up.
//   * A message rejected as malformed is dead-lettered rather than requeued,
//     since retrying a message that can only fail again just burns the queue.
//
// Messages travel over simulated links so the topology view has something real
// to animate: a packet's position is interpolated between the clock it was sent
// at and the clock it arrives at.

let nextPacketId = 1;
let nextDeliveryTag = 1;

/** One message in flight between two endpoints. */
class Packet {
  constructor({ from, to, queue, kind, body, sentAt, deliverAt }) {
    this.id = nextPacketId;
    nextPacketId += 1;
    this.from = from;
    this.to = to;
    this.queue = queue;
    this.kind = kind;
    this.body = body;
    this.sentAt = sentAt;
    this.deliverAt = deliverAt;
  }

  /** Fraction of the link traversed at `now`, for drawing. */
  progress(now) {
    const span = this.deliverAt - this.sentAt;
    if (span <= 0) {
      return 1;
    }
    return Math.min(Math.max((now - this.sentAt) / span, 0), 1);
  }
}

export class Broker {
  constructor({ linkLatencyMs = 45 } = {}) {
    this.linkLatencyMs = linkLatencyMs;
    this.queues = new Map();
    this.consumers = new Map();
    this.packets = [];
    this.deadLettered = 0;
    this.published = 0;
    this.delivered = 0;
    this.requeued = 0;
  }

  declareQueue(name) {
    if (!this.queues.has(name)) {
      this.queues.set(name, { name, ready: [], unacked: new Map(), roundRobin: 0 });
    }
    return this.queues.get(name);
  }

  queue(name) {
    return this.queues.get(name) ?? this.declareQueue(name);
  }

  /** Ready messages waiting on a queue — the depth a `rabbitmqctl` would show. */
  depth(name) {
    return this.queue(name).ready.length;
  }

  /** Messages delivered but not yet acknowledged. These survive a worker's death. */
  unackedCount(name) {
    return this.queue(name).unacked.size;
  }

  /**
   * Registers a consumer. Every consumer on a queue is equivalent as far as the
   * broker is concerned, which is what lets a worker join mid-round and start
   * taking shards without anyone reconfiguring anything.
   */
  consume(queueName, consumerId, onDelivery) {
    this.declareQueue(queueName);
    this.consumers.set(consumerId, { consumerId, queueName, onDelivery });
  }

  /**
   * Cancels a consumer and requeues everything it was holding.
   *
   * This is the broker behaviour that makes a worker disposable: the shards it
   * had in hand go back on the queue for whoever is still alive, so a round
   * that lost a worker finishes late rather than never.
   */
  cancel(consumerId) {
    const consumer = this.consumers.get(consumerId);
    if (!consumer) {
      return 0;
    }
    this.consumers.delete(consumerId);

    const queue = this.queue(consumer.queueName);
    let recovered = 0;
    for (const [tag, message] of [...queue.unacked]) {
      if (message.consumerId !== consumerId) {
        continue;
      }
      queue.unacked.delete(tag);
      queue.ready.unshift(message.body);
      recovered += 1;
    }

    // Packets still on the wire to a dead consumer never arrive; the broker
    // still has them unacknowledged, so they were already requeued above.
    this.packets = this.packets.filter((packet) => packet.to !== consumerId);
    this.requeued += recovered;
    return recovered;
  }

  /** Publishes a message, which crosses the link to the broker before enqueuing. */
  publish(queueName, body, from, now, kind = 'task') {
    this.declareQueue(queueName);
    this.published += 1;
    this.packets.push(
      new Packet({
        from,
        to: 'broker',
        queue: queueName,
        kind,
        body,
        sentAt: now,
        deliverAt: now + this.linkLatencyMs,
      }),
    );
  }

  ack(consumerId, deliveryTag) {
    const consumer = this.consumers.get(consumerId);
    if (!consumer) {
      return;
    }
    this.queue(consumer.queueName).unacked.delete(deliveryTag);
  }

  /**
   * Rejects a delivery. `requeue` decides whether it goes back for another
   * attempt or is dropped: a payload that cannot be parsed will not parse next
   * time either.
   */
  nack(consumerId, deliveryTag, requeue) {
    const consumer = this.consumers.get(consumerId);
    if (!consumer) {
      return;
    }
    const queue = this.queue(consumer.queueName);
    const message = queue.unacked.get(deliveryTag);
    if (!message) {
      return;
    }
    queue.unacked.delete(deliveryTag);
    if (requeue) {
      queue.ready.push(message.body);
      this.requeued += 1;
    } else {
      this.deadLettered += 1;
    }
  }

  consumersFor(queueName) {
    return [...this.consumers.values()].filter((consumer) => consumer.queueName === queueName);
  }

  /** Advances in-flight packets and hands ready messages to consumers. */
  tick(now) {
    const arrived = [];
    this.packets = this.packets.filter((packet) => {
      if (packet.deliverAt > now) {
        return true;
      }
      arrived.push(packet);
      return false;
    });

    for (const packet of arrived) {
      if (packet.to === 'broker') {
        this.queue(packet.queue).ready.push(packet.body);
      } else {
        const consumer = this.consumers.get(packet.to);
        if (consumer) {
          this.delivered += 1;
          consumer.onDelivery(packet.body, packet.deliveryTag, now);
        }
      }
    }

    for (const queue of this.queues.values()) {
      this.dispatch(queue, now);
    }
  }

  /**
   * Hands ready messages out round-robin across live consumers.
   *
   * No prefetch limit is configured on the real channels, so the broker pushes
   * as soon as a consumer exists; each node then works through what it was
   * given one delivery at a time.
   */
  dispatch(queue, now) {
    const consumers = this.consumersFor(queue.name);
    if (consumers.length === 0) {
      return;
    }

    while (queue.ready.length > 0) {
      const consumer = consumers[queue.roundRobin % consumers.length];
      queue.roundRobin += 1;
      const body = queue.ready.shift();
      const deliveryTag = nextDeliveryTag;
      nextDeliveryTag += 1;
      queue.unacked.set(deliveryTag, { consumerId: consumer.consumerId, body });

      const packet = new Packet({
        from: 'broker',
        to: consumer.consumerId,
        queue: queue.name,
        kind: body.kind ?? 'task',
        body,
        sentAt: now,
        deliverAt: now + this.linkLatencyMs,
      });
      packet.deliveryTag = deliveryTag;
      this.packets.push(packet);
    }
  }
}
