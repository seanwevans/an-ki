// The training data, and how it is divided between workers — a port of
// `src/dataset.rs`.
//
// Data is generated, never shipped. Every node derives the identical dataset
// from a shared seed and takes only its own shard, so the wire carries
// parameters and gradients but never samples. The browser does the same thing,
// from the same seed, and gets the same points.

import { StdRng } from './rng.js';

/** Feature count of the generated task. */
export const INPUTS = 2;

/** Class count of the generated task. */
export const OUTPUTS = 2;

/**
 * Radius of the decision boundary, `sqrt(2 / pi)`.
 *
 * For points drawn uniformly from [-1, 1]^2 that puts about half of them
 * inside, so accuracy means something: a model cannot score well by always
 * guessing one class.
 */
export const RADIUS = Math.fround(0.7978846);

/**
 * Describes a dataset completely: same values, same samples, same split, on any
 * node. The hold-out is a count rather than a fraction so every node divides the
 * data at exactly the same index.
 */
export function datasetSpec(samples, seed, validationSamples = 0) {
  return {
    samples,
    seed,
    validationSamples: Math.min(validationSamples, Math.max(samples - 1, 0)),
  };
}

export function trainingSamples(spec) {
  return Math.max(spec.samples - spec.validationSamples, 0);
}

/** Generates the dataset described by `spec`. Deterministic in the seed. */
export function generate(spec) {
  const rng = new StdRng(spec.seed);
  const samples = new Array(spec.samples);
  const radiusSquared = Math.fround(RADIUS * RADIUS);

  for (let index = 0; index < spec.samples; index += 1) {
    const x = rng.genRangeF32(-1.0, 1.0);
    const y = rng.genRangeF32(-1.0, 1.0);
    const distance = Math.fround(Math.fround(x * x) + Math.fround(y * y));
    samples[index] = { features: [x, y], label: distance < radiusSquared ? 1 : 0 };
  }

  return samples;
}

/**
 * The half-open range of `total` items belonging to shard `index` of `count`.
 *
 * Shards are contiguous, disjoint, and cover the whole set; sizes differ by at
 * most one, with the first `total % count` shards taking the extra item. An
 * index at or beyond `count` yields an empty range rather than throwing —
 * these arrive from configuration and from the wire.
 */
export function shardRange(total, index, count) {
  if (count === 0 || index >= count) {
    return { start: 0, end: 0 };
  }
  const base = Math.floor(total / count);
  const remainder = total % count;
  const start = index * base + Math.min(index, remainder);
  const length = base + (index < remainder ? 1 : 0);
  return { start, end: start + length };
}

/** Shard `index` of `count` from `samples`. */
export function shard(samples, index, count) {
  const range = shardRange(samples.length, index, count);
  return samples.slice(range.start, range.end);
}

/**
 * The portion of `samples` available for training. Shards are cut from this
 * slice only, so no validation sample can reach a worker's gradient.
 */
export function training(spec, samples) {
  return samples.slice(0, Math.min(trainingSamples(spec), samples.length));
}

/** The held-out portion of `samples`, taken from the end. */
export function validation(spec, samples) {
  return samples.slice(Math.min(trainingSamples(spec), samples.length));
}
