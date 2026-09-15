// The neural network being trained — a port of `src/model.rs`.
//
// A multi-layer perceptron with a `tanh` hidden layer and a softmax output,
// trained with cross-entropy. Parameters are one flat `f32` vector rather than
// structured matrices, because that vector is both the wire format between An
// and Ki nodes and the format gradients are averaged in.
//
// Arithmetic is kept in `f32` (`Float32Array` plus `Math.fround`) so the
// browser's numbers track the cluster's. `tanh`, `exp` and `log` are the
// platform's own, so results can differ from Rust's in the last bit or two —
// enough to matter for a checksum, not enough to matter for a decision
// boundary.

/** Shape of the network. The parameter vector is meaningless without it. */
export function mlpSpec(inputs, hidden, outputs) {
  return { inputs, hidden, outputs };
}

/** Length of the flat parameter vector: `W1`, `b1`, `W2`, `b2` end to end. */
export function parameterCount(spec) {
  return spec.hidden * spec.inputs + spec.hidden + spec.outputs * spec.hidden + spec.outputs;
}

// Offsets of each block within the flat vector, defined in one place so the
// forward and backward passes cannot disagree about the layout.
const w1Offset = () => 0;
const b1Offset = (spec) => spec.hidden * spec.inputs;
const w2Offset = (spec) => b1Offset(spec) + spec.hidden;
const b2Offset = (spec) => w2Offset(spec) + spec.outputs * spec.hidden;

export const offsets = { w1Offset, b1Offset, w2Offset, b2Offset };

/**
 * Draws initial parameters from a seeded generator: Xavier-uniform weights,
 * zero biases.
 *
 * The seeding is not merely for reproducibility. Zero-initialised weights would
 * make every hidden unit compute the same function and receive the same
 * gradient forever, so the network could never use more than one effective
 * hidden unit no matter how correct the backward pass is.
 */
export function initialize(spec, rng) {
  const parameters = new Float32Array(parameterCount(spec));

  const fill = (start, end, fanIn, fanOut) => {
    const limit = Math.fround(Math.sqrt(Math.fround(6.0 / (fanIn + fanOut))));
    for (let index = start; index < end; index += 1) {
      parameters[index] = rng.genRangeF32(-limit, limit);
    }
  };

  fill(w1Offset(), b1Offset(spec), spec.inputs, spec.hidden);
  fill(w2Offset(spec), b2Offset(spec), spec.hidden, spec.outputs);

  return parameters;
}

/**
 * Numerically stable softmax: subtracting the maximum before exponentiating
 * keeps `exp` from overflowing on confident predictions, which is exactly when
 * the logits get large.
 */
function softmax(logits) {
  let max = -Infinity;
  for (let index = 0; index < logits.length; index += 1) {
    if (logits[index] > max) {
      max = logits[index];
    }
  }

  let sum = 0;
  for (let index = 0; index < logits.length; index += 1) {
    logits[index] = Math.fround(Math.exp(Math.fround(logits[index] - max)));
    sum = Math.fround(sum + logits[index]);
  }
  for (let index = 0; index < logits.length; index += 1) {
    logits[index] = Math.fround(logits[index] / sum);
  }

  return logits;
}

/**
 * Runs the forward pass for one sample. The hidden activations come back
 * alongside the probabilities because the backward pass needs them.
 */
export function forward(spec, parameters, features, hidden, probabilities) {
  const b1 = b1Offset(spec);
  const w2 = w2Offset(spec);
  const b2 = b2Offset(spec);

  for (let unit = 0; unit < spec.hidden; unit += 1) {
    let sum = parameters[b1 + unit];
    const row = unit * spec.inputs;
    for (let input = 0; input < spec.inputs; input += 1) {
      sum = Math.fround(sum + Math.fround(parameters[row + input] * features[input]));
    }
    hidden[unit] = Math.fround(Math.tanh(sum));
  }

  for (let output = 0; output < spec.outputs; output += 1) {
    let sum = parameters[b2 + output];
    const row = w2 + output * spec.hidden;
    for (let unit = 0; unit < spec.hidden; unit += 1) {
      sum = Math.fround(sum + Math.fround(parameters[row + unit] * hidden[unit]));
    }
    probabilities[output] = sum;
  }

  return softmax(probabilities);
}

/** Class probabilities for one input. */
export function predict(spec, parameters, features) {
  const hidden = new Float32Array(spec.hidden);
  const probabilities = new Float32Array(spec.outputs);
  return Array.from(forward(spec, parameters, features, hidden, probabilities));
}

/** Fraction of the batch the model classifies correctly. */
export function accuracy(spec, parameters, batch) {
  if (batch.length === 0) {
    return null;
  }

  const hidden = new Float32Array(spec.hidden);
  const probabilities = new Float32Array(spec.outputs);
  let correct = 0;

  for (const sample of batch) {
    forward(spec, parameters, sample.features, hidden, probabilities);
    let best = 0;
    for (let output = 1; output < spec.outputs; output += 1) {
      if (probabilities[output] > probabilities[best]) {
        best = output;
      }
    }
    if (best === sample.label) {
      correct += 1;
    }
  }

  return correct / batch.length;
}

/** Smallest positive normal `f32`, the clamp applied before taking a log. */
const F32_MIN_POSITIVE = 1.1754943508222875e-38;

/**
 * Mean cross-entropy loss and its gradient with respect to every parameter.
 *
 * The gradient comes back in the same flat layout as `parameters`, so an An
 * node can combine gradients from several Ki nodes element-wise without knowing
 * anything about the network's shape.
 *
 * Both loss and gradient are means over the batch, not sums — which is why the
 * An node weights each shard's reply by its sample count.
 */
export function lossAndGradient(spec, parameters, batch) {
  if (batch.length === 0) {
    throw new Error('cannot compute a gradient over an empty batch');
  }

  const gradient = new Float32Array(parameterCount(spec));
  // Accumulate the loss in double precision. Summing many small f32 values
  // loses low bits as the running total grows, and the loss is what we use to
  // judge whether training is working.
  let totalLoss = 0;

  const b1 = b1Offset(spec);
  const w2 = w2Offset(spec);
  const b2 = b2Offset(spec);
  const hidden = new Float32Array(spec.hidden);
  const probabilities = new Float32Array(spec.outputs);

  for (const sample of batch) {
    forward(spec, parameters, sample.features, hidden, probabilities);

    // Clamp before the log so a probability that underflows to zero yields a
    // large finite loss instead of infinity, which would poison the average and
    // every gradient derived from it.
    const correct = Math.max(probabilities[sample.label], F32_MIN_POSITIVE);
    totalLoss -= Math.log(correct);

    // d(loss)/d(logits) for softmax + cross-entropy is simply p - onehot(y).
    probabilities[sample.label] = Math.fround(probabilities[sample.label] - 1.0);

    for (let output = 0; output < spec.outputs; output += 1) {
      const delta = probabilities[output];
      gradient[b2 + output] = Math.fround(gradient[b2 + output] + delta);
      const row = w2 + output * spec.hidden;
      for (let unit = 0; unit < spec.hidden; unit += 1) {
        gradient[row + unit] = Math.fround(
          gradient[row + unit] + Math.fround(delta * hidden[unit]),
        );
      }
    }

    for (let unit = 0; unit < spec.hidden; unit += 1) {
      const activation = hidden[unit];
      let upstream = 0;
      for (let output = 0; output < spec.outputs; output += 1) {
        upstream = Math.fround(
          upstream +
            Math.fround(parameters[w2 + output * spec.hidden + unit] * probabilities[output]),
        );
      }
      // tanh'(z) = 1 - tanh(z)^2, and `hidden` already holds tanh(z).
      const delta = Math.fround(upstream * Math.fround(1.0 - Math.fround(activation * activation)));

      gradient[b1 + unit] = Math.fround(gradient[b1 + unit] + delta);
      const row = unit * spec.inputs;
      for (let input = 0; input < spec.inputs; input += 1) {
        gradient[row + input] = Math.fround(
          gradient[row + input] + Math.fround(delta * sample.features[input]),
        );
      }
    }
  }

  const scale = Math.fround(1.0 / batch.length);
  for (let index = 0; index < gradient.length; index += 1) {
    gradient[index] = Math.fround(gradient[index] * scale);
  }

  return { loss: Math.fround(totalLoss / batch.length), gradient, samples: batch.length };
}

/** Mean cross-entropy loss over a batch. */
export function loss(spec, parameters, batch) {
  return lossAndGradient(spec, parameters, batch).loss;
}
