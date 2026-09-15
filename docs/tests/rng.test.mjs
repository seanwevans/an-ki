// The port only means anything if it stays a port: these compare the
// JavaScript generator against values printed by the Rust crate itself
// (`cargo run --bin demo_fixture`), to the last bit of every f32.

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { StdRng } from '../assets/sim/rng.js';
import * as dataset from '../assets/sim/dataset.js';
import * as model from '../assets/sim/model.js';

const reference = JSON.parse(
  readFileSync(new URL('../assets/sim/fixtures/reference.json', import.meta.url), 'utf8'),
);

const spec = dataset.datasetSpec(
  reference.dataset.samples,
  reference.dataset.seed,
  reference.dataset.validation_samples,
);

test('the generated dataset matches the one the Rust nodes generate', () => {
  const samples = dataset.generate(spec);
  assert.equal(samples.length, reference.dataset.samples);

  reference.first_samples.forEach((expected, index) => {
    assert.equal(samples[index].features[0], Math.fround(expected.features[0]), `sample ${index} x`);
    assert.equal(samples[index].features[1], Math.fround(expected.features[1]), `sample ${index} y`);
    assert.equal(samples[index].label, expected.label, `sample ${index} label`);
  });

  const last = samples[samples.length - 1];
  assert.equal(last.features[0], Math.fround(reference.last_sample.features[0]));
  assert.equal(last.features[1], Math.fround(reference.last_sample.features[1]));
  assert.equal(last.label, reference.last_sample.label);
});

test('every sample in the dataset matches, not just the first few', () => {
  const samples = dataset.generate(spec);
  const inside = samples.filter((sample) => sample.label === 1).length;
  assert.equal(inside, reference.inside_count);

  // Summed in double precision, so the total depends on the sample values and
  // not on how f32 rounding accumulates.
  const checksum = samples.reduce(
    (total, sample) => total + sample.features[0] + sample.features[1],
    0,
  );
  assert.equal(checksum, reference.feature_checksum);
});

test('initial parameters match the ones the An node starts from', () => {
  const modelSpec = model.mlpSpec(
    reference.model.inputs,
    reference.model.hidden,
    reference.model.outputs,
  );
  const parameters = model.initialize(modelSpec, new StdRng(reference.model.init_seed));

  assert.equal(parameters.length, reference.initial_parameters.length);
  reference.initial_parameters.forEach((expected, index) => {
    assert.equal(parameters[index], Math.fround(expected), `parameter ${index}`);
  });
});

test('a seed reproduces its stream, and different seeds diverge', () => {
  const draw = (seed) =>
    Array.from({ length: 16 }, () => new StdRng(seed).genRangeF32(-1, 1)).join(',');
  assert.equal(draw(1), draw(1));
  assert.notEqual(draw(1), draw(2));
});
