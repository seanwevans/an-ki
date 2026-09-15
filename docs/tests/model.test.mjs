// Mirrors the load-bearing tests in `src/model.rs`. Backpropagation is easy to
// write in a way that looks reasonable and is subtly wrong — a transposed
// index, a missing activation derivative — and all of those produce gradients
// that still point somewhere, so training merely converges badly instead of
// failing loudly.

import test from 'node:test';
import assert from 'node:assert/strict';

import * as model from '../assets/sim/model.js';
import { StdRng } from '../assets/sim/rng.js';

const spec = model.mlpSpec(2, 4, 2);
const batch = [
  { features: [0.6, -0.2], label: 0 },
  { features: [-0.4, 0.9], label: 1 },
  { features: [0.1, 0.3], label: 1 },
];

test('the parameter count matches the layout', () => {
  const wide = model.mlpSpec(2, 4, 3);
  // W1 (4x2) + b1 (4) + W2 (3x4) + b2 (3)
  assert.equal(model.parameterCount(wide), 8 + 4 + 12 + 3);
});

test('initialization breaks symmetry between hidden units', () => {
  const parameters = model.initialize(spec, new StdRng(42));
  const rows = Array.from({ length: spec.hidden }, (_, unit) =>
    Array.from(parameters.slice(unit * spec.inputs, (unit + 1) * spec.inputs)).join(','),
  );
  assert.equal(new Set(rows).size, rows.length, 'two hidden units started identical');
  assert.ok(Array.from(parameters).some((value) => value !== 0));
});

test('probabilities are a distribution and survive large logits', () => {
  const parameters = model.initialize(spec, new StdRng(3));
  const probabilities = model.predict(spec, parameters, [0.4, -0.7]);
  const sum = probabilities.reduce((total, value) => total + value, 0);
  assert.ok(Math.abs(sum - 1) < 1e-5, `probabilities summed to ${sum}`);

  const large = model.mlpSpec(1, 1, 2);
  const huge = Float32Array.from([0, 0, 1000, 1000, 1000, 1001]);
  const extreme = model.predict(large, huge, [1]);
  assert.ok(extreme.every(Number.isFinite));
  assert.ok(extreme[1] > extreme[0]);
});

test('analytic gradients match central finite differences', () => {
  const parameters = model.initialize(spec, new StdRng(11));
  const { gradient } = model.lossAndGradient(spec, parameters, batch);
  const epsilon = 1e-2;

  for (let index = 0; index < parameters.length; index += 1) {
    const original = parameters[index];
    parameters[index] = Math.fround(original + epsilon);
    const up = model.loss(spec, parameters, batch);
    parameters[index] = Math.fround(original - epsilon);
    const down = model.loss(spec, parameters, batch);
    parameters[index] = original;

    const numerical = (up - down) / (2 * epsilon);
    const scale = Math.max(Math.abs(numerical), Math.abs(gradient[index]), 1);
    assert.ok(
      Math.abs(numerical - gradient[index]) / scale < 1e-2,
      `parameter ${index}: analytic ${gradient[index]} vs numerical ${numerical}`,
    );
  }
});

test('gradient descent on a batch reduces the loss', () => {
  const parameters = model.initialize(spec, new StdRng(13));
  const before = model.loss(spec, parameters, batch);
  for (let step = 0; step < 200; step += 1) {
    const { gradient } = model.lossAndGradient(spec, parameters, batch);
    for (let index = 0; index < parameters.length; index += 1) {
      parameters[index] = Math.fround(parameters[index] - Math.fround(0.5 * gradient[index]));
    }
  }
  const after = model.loss(spec, parameters, batch);
  assert.ok(after < before, `loss rose from ${before} to ${after}`);
  assert.ok(after < 0.1, `expected the batch to be fit, got ${after}`);
});

test('an empty batch is refused rather than answered with zeros', () => {
  const parameters = model.initialize(spec, new StdRng(1));
  assert.throws(() => model.lossAndGradient(spec, parameters, []));
});
