// Mirrors the tests in `src/dataset.rs`: two nodes disagreeing about the data
// or about who owns which slice would silently average gradients taken over
// different samples.

import test from 'node:test';
import assert from 'node:assert/strict';

import * as dataset from '../assets/sim/dataset.js';

test('shards are contiguous, disjoint, and cover the whole set', () => {
  for (const [total, count] of [
    [10, 3],
    [512, 4],
    [7, 7],
    [5, 2],
  ]) {
    const ranges = Array.from({ length: count }, (_, index) =>
      dataset.shardRange(total, index, count),
    );
    assert.equal(ranges[0].start, 0);
    assert.equal(ranges[ranges.length - 1].end, total);
    for (let index = 1; index < ranges.length; index += 1) {
      assert.equal(ranges[index].start, ranges[index - 1].end, 'shards must not overlap or gap');
    }
    const sizes = ranges.map((range) => range.end - range.start);
    assert.ok(Math.max(...sizes) - Math.min(...sizes) <= 1, 'shard sizes differ by at most one');
  }
});

test('an out-of-range shard is empty rather than an error', () => {
  assert.deepEqual(dataset.shardRange(10, 4, 4), { start: 0, end: 0 });
  assert.deepEqual(dataset.shardRange(10, 0, 0), { start: 0, end: 0 });
});

test('labels follow the circle rule and the classes stay balanced', () => {
  const samples = dataset.generate(dataset.datasetSpec(2000, 5));
  for (const sample of samples) {
    const [x, y] = sample.features;
    const inside = Math.fround(Math.fround(x * x) + Math.fround(y * y)) < Math.fround(dataset.RADIUS * dataset.RADIUS);
    assert.equal(sample.label, inside ? 1 : 0);
  }
  const fraction = samples.filter((sample) => sample.label === 1).length / samples.length;
  assert.ok(fraction > 0.45 && fraction < 0.55, `class balance was ${fraction}`);
});

test('the hold-out comes off the end and never reaches a shard', () => {
  const spec = dataset.datasetSpec(100, 3, 20);
  const samples = dataset.generate(spec);
  const training = dataset.training(spec, samples);
  const validation = dataset.validation(spec, samples);

  assert.equal(training.length, 80);
  assert.equal(validation.length, 20);

  const held = new Set(validation.map((sample) => sample.features.join(',')));
  for (let index = 0; index < 4; index += 1) {
    for (const sample of dataset.shard(training, index, 4)) {
      assert.ok(!held.has(sample.features.join(',')), 'a validation sample reached a shard');
    }
  }
});

test('the task is not linearly separable', () => {
  // Both classes appear on both sides of the midpoint of each feature, so no
  // single threshold separates them and a model without a hidden layer cannot
  // draw the boundary.
  const samples = dataset.generate(dataset.datasetSpec(400, 11));
  for (const axis of [0, 1]) {
    for (const side of [-1, 1]) {
      const half = samples.filter((sample) => Math.sign(sample.features[axis]) === side);
      assert.ok(half.some((sample) => sample.label === 0));
      assert.ok(half.some((sample) => sample.label === 1));
    }
  }
});
