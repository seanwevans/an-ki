//! The neural network being trained: a multi-layer perceptron with a `tanh`
//! hidden layer and a softmax output, trained with cross-entropy loss.
//!
//! Parameters are held as one flat `Vec<f32>` rather than as structured
//! matrices. That is deliberate: parameters and gradients are exchanged between
//! An and Ki nodes as JSON payloads, and the An node averages gradients
//! element-wise, so a single contiguous vector is both the wire format and the
//! aggregation format. [`MlpSpec`] describes how to interpret it.
//!
//! Everything here is a pure function of `(spec, parameters, samples)`. Nothing
//! in this module knows about queues, nodes, or the cluster — which is what lets
//! the gradients be checked directly against finite differences.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

/// One training example: a feature vector and the index of its correct class.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub features: Vec<f32>,
    pub label: usize,
}

/// Shape of the network. The parameter vector is meaningless without it, so it
/// travels with the model wherever the model goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MlpSpec {
    pub inputs: usize,
    pub hidden: usize,
    pub outputs: usize,
}

impl MlpSpec {
    pub fn new(inputs: usize, hidden: usize, outputs: usize) -> Self {
        Self {
            inputs,
            hidden,
            outputs,
        }
    }

    /// Length of the flat parameter vector: `W1`, `b1`, `W2`, `b2` end to end.
    pub fn parameter_count(&self) -> usize {
        self.hidden * self.inputs + self.hidden + self.outputs * self.hidden + self.outputs
    }

    // Offsets of each block within the flat vector. Kept as functions so the
    // layout is defined in exactly one place; a mismatch between the forward and
    // backward passes would show up as a plausible-looking but wrong gradient.
    fn w1_offset(&self) -> usize {
        0
    }
    fn b1_offset(&self) -> usize {
        self.hidden * self.inputs
    }
    fn w2_offset(&self) -> usize {
        self.b1_offset() + self.hidden
    }
    fn b2_offset(&self) -> usize {
        self.w2_offset() + self.outputs * self.hidden
    }

    /// Draws initial parameters from a seeded generator.
    ///
    /// Weights use Xavier-uniform bounds; biases start at zero.
    ///
    /// The seeding is not merely for reproducibility. Zero-initialised weights
    /// would make every hidden unit compute the same function and receive the
    /// same gradient forever, so the network could never use more than one
    /// effective hidden unit no matter how correct the backward pass is. The
    /// asymmetry has to come from somewhere, and it comes from here.
    pub fn initialize(&self, seed: u64) -> Vec<f32> {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut parameters = vec![0.0_f32; self.parameter_count()];

        let fill = |slice: &mut [f32], fan_in: usize, fan_out: usize, rng: &mut StdRng| {
            let limit = (6.0_f32 / (fan_in + fan_out) as f32).sqrt();
            for value in slice.iter_mut() {
                *value = rng.gen_range(-limit..limit);
            }
        };

        let b1 = self.b1_offset();
        let w2 = self.w2_offset();
        let b2 = self.b2_offset();
        fill(
            &mut parameters[self.w1_offset()..b1],
            self.inputs,
            self.hidden,
            &mut rng,
        );
        fill(&mut parameters[w2..b2], self.hidden, self.outputs, &mut rng);

        parameters
    }

    /// Splits a flat parameter vector into its four blocks.
    fn split<'a>(&self, parameters: &'a [f32]) -> Option<Blocks<'a>> {
        if parameters.len() != self.parameter_count() {
            return None;
        }
        let b1 = self.b1_offset();
        let w2 = self.w2_offset();
        let b2 = self.b2_offset();
        Some(Blocks {
            w1: &parameters[..b1],
            b1: &parameters[b1..w2],
            w2: &parameters[w2..b2],
            b2: &parameters[b2..],
        })
    }
}

struct Blocks<'a> {
    w1: &'a [f32],
    b1: &'a [f32],
    w2: &'a [f32],
    b2: &'a [f32],
}

/// Something that went wrong evaluating the model, always a shape mismatch
/// between the spec, the parameters, and the data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    ParameterCount { expected: usize, got: usize },
    FeatureCount { expected: usize, got: usize },
    LabelOutOfRange { label: usize, outputs: usize },
    EmptyBatch,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParameterCount { expected, got } => {
                write!(f, "expected {expected} parameters, got {got}")
            }
            Self::FeatureCount { expected, got } => {
                write!(f, "expected {expected} features, got {got}")
            }
            Self::LabelOutOfRange { label, outputs } => {
                write!(f, "label {label} is out of range for {outputs} outputs")
            }
            Self::EmptyBatch => write!(f, "cannot compute a gradient over an empty batch"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Numerically stable softmax: subtracting the maximum before exponentiating
/// keeps `exp` from overflowing on confident predictions, which is exactly when
/// the logits get large.
fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut out: Vec<f32> = logits.iter().map(|z| (z - max).exp()).collect();
    let sum: f32 = out.iter().sum();
    for value in out.iter_mut() {
        *value /= sum;
    }
    out
}

/// Dot product of two equal-length slices.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Runs the forward pass for one sample, returning the hidden activations and
/// the output probabilities. The activations are kept because the backward pass
/// needs them.
fn forward(spec: &MlpSpec, blocks: &Blocks<'_>, features: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut hidden = vec![0.0_f32; spec.hidden];
    for (h, activation) in hidden.iter_mut().enumerate() {
        let row = &blocks.w1[h * spec.inputs..(h + 1) * spec.inputs];
        let z = blocks.b1[h] + dot(row, features);
        *activation = z.tanh();
    }

    let mut logits = vec![0.0_f32; spec.outputs];
    for (o, logit) in logits.iter_mut().enumerate() {
        let row = &blocks.w2[o * spec.hidden..(o + 1) * spec.hidden];
        *logit = blocks.b2[o] + dot(row, &hidden);
    }

    (hidden, softmax(&logits))
}

/// Class probabilities for one input.
pub fn predict(
    spec: &MlpSpec,
    parameters: &[f32],
    features: &[f32],
) -> Result<Vec<f32>, ModelError> {
    let blocks = spec.split(parameters).ok_or(ModelError::ParameterCount {
        expected: spec.parameter_count(),
        got: parameters.len(),
    })?;
    if features.len() != spec.inputs {
        return Err(ModelError::FeatureCount {
            expected: spec.inputs,
            got: features.len(),
        });
    }
    Ok(forward(spec, &blocks, features).1)
}

/// Mean cross-entropy loss over a batch.
pub fn loss(spec: &MlpSpec, parameters: &[f32], batch: &[Sample]) -> Result<f32, ModelError> {
    Ok(loss_and_gradient(spec, parameters, batch)?.0)
}

/// Fraction of the batch the model classifies correctly.
pub fn accuracy(spec: &MlpSpec, parameters: &[f32], batch: &[Sample]) -> Result<f32, ModelError> {
    if batch.is_empty() {
        return Err(ModelError::EmptyBatch);
    }
    let mut correct = 0usize;
    for sample in batch {
        let probabilities = predict(spec, parameters, &sample.features)?;
        let predicted = probabilities
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
            .unwrap_or(0);
        if predicted == sample.label {
            correct += 1;
        }
    }
    Ok(correct as f32 / batch.len() as f32)
}

/// Mean cross-entropy loss and its gradient with respect to every parameter.
///
/// The gradient comes back in the same flat layout as `parameters`, so an An
/// node can average gradients from several Ki nodes by adding them element-wise
/// without knowing anything about the network's shape.
///
/// Both loss and gradient are means over the batch, not sums. That matters for
/// distributed training: shards may hold different numbers of samples, and
/// averaging per-shard means keeps each *sample* equally weighted only when the
/// shards are equal-sized — see [`crate::dataset`] for how shards are cut.
pub fn loss_and_gradient(
    spec: &MlpSpec,
    parameters: &[f32],
    batch: &[Sample],
) -> Result<(f32, Vec<f32>), ModelError> {
    let blocks = spec.split(parameters).ok_or(ModelError::ParameterCount {
        expected: spec.parameter_count(),
        got: parameters.len(),
    })?;
    if batch.is_empty() {
        return Err(ModelError::EmptyBatch);
    }

    let mut gradient = vec![0.0_f32; spec.parameter_count()];
    // Accumulate the loss in f64. Summing many small f32 values loses low bits
    // as the running total grows, and the loss is what we use to judge whether
    // training is working.
    let mut total_loss = 0.0_f64;

    let b1_offset = spec.b1_offset();
    let w2_offset = spec.w2_offset();
    let b2_offset = spec.b2_offset();

    for sample in batch {
        if sample.features.len() != spec.inputs {
            return Err(ModelError::FeatureCount {
                expected: spec.inputs,
                got: sample.features.len(),
            });
        }
        if sample.label >= spec.outputs {
            return Err(ModelError::LabelOutOfRange {
                label: sample.label,
                outputs: spec.outputs,
            });
        }

        let (hidden, probabilities) = forward(spec, &blocks, &sample.features);

        // Clamp before the log so a probability that underflows to zero yields a
        // large finite loss instead of infinity, which would poison the average
        // and every gradient derived from it.
        let correct = probabilities[sample.label].max(f32::MIN_POSITIVE);
        total_loss -= (correct as f64).ln();

        // d(loss)/d(logits) for softmax + cross-entropy is simply p - onehot(y).
        let mut dz2 = probabilities;
        dz2[sample.label] -= 1.0;

        for (o, &delta) in dz2.iter().enumerate() {
            gradient[b2_offset + o] += delta;
            let row = &mut gradient[w2_offset + o * spec.hidden..w2_offset + (o + 1) * spec.hidden];
            for (slot, &activation) in row.iter_mut().zip(&hidden) {
                *slot += delta * activation;
            }
        }

        for (h, &activation) in hidden.iter().enumerate() {
            let da1: f32 = dz2
                .iter()
                .enumerate()
                .map(|(o, &delta)| blocks.w2[o * spec.hidden + h] * delta)
                .sum();
            // tanh'(z) = 1 - tanh(z)^2, and `hidden` already holds tanh(z).
            let dz1 = da1 * (1.0 - activation * activation);

            gradient[b1_offset + h] += dz1;
            let row = &mut gradient[h * spec.inputs..(h + 1) * spec.inputs];
            for (slot, &feature) in row.iter_mut().zip(&sample.features) {
                *slot += dz1 * feature;
            }
        }
    }

    let scale = 1.0 / batch.len() as f32;
    for value in gradient.iter_mut() {
        *value *= scale;
    }

    Ok(((total_loss / batch.len() as f64) as f32, gradient))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MlpSpec {
        MlpSpec::new(2, 4, 2)
    }

    fn batch() -> Vec<Sample> {
        vec![
            Sample {
                features: vec![0.6, -0.2],
                label: 0,
            },
            Sample {
                features: vec![-0.4, 0.9],
                label: 1,
            },
            Sample {
                features: vec![0.1, 0.3],
                label: 1,
            },
        ]
    }

    #[test]
    fn parameter_count_matches_the_layout() {
        let spec = MlpSpec::new(2, 4, 3);
        // W1 (4x2) + b1 (4) + W2 (3x4) + b2 (3)
        assert_eq!(spec.parameter_count(), 8 + 4 + 12 + 3);
        assert_eq!(spec.initialize(7).len(), spec.parameter_count());
    }

    #[test]
    fn initialization_breaks_symmetry_between_hidden_units() {
        // Identical hidden rows would receive identical gradients forever, so
        // the network could never use more than one effective hidden unit.
        let spec = spec();
        let parameters = spec.initialize(42);
        let rows: Vec<&[f32]> = (0..spec.hidden)
            .map(|h| &parameters[h * spec.inputs..(h + 1) * spec.inputs])
            .collect();

        for (i, a) in rows.iter().enumerate() {
            for b in rows.iter().skip(i + 1) {
                assert_ne!(a, b, "two hidden units started identical");
            }
        }
        assert!(
            parameters.iter().any(|&value| value != 0.0),
            "an all-zero initialization cannot learn"
        );
    }

    #[test]
    fn initialization_is_reproducible_for_a_seed() {
        assert_eq!(spec().initialize(1), spec().initialize(1));
        assert_ne!(spec().initialize(1), spec().initialize(2));
    }

    #[test]
    fn probabilities_are_a_distribution() {
        let spec = spec();
        let parameters = spec.initialize(3);
        let probabilities = predict(&spec, &parameters, &[0.4, -0.7]).expect("predict");

        let sum: f32 = probabilities.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5, "probabilities summed to {sum}");
        assert!(probabilities.iter().all(|&p| (0.0..=1.0).contains(&p)));
    }

    #[test]
    fn softmax_survives_large_logits() {
        // Without max-subtraction this overflows to NaN.
        let probabilities = softmax(&[1000.0, 1001.0]);
        assert!(probabilities.iter().all(|p| p.is_finite()));
        assert!((probabilities.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(probabilities[1] > probabilities[0]);
    }

    /// The load-bearing test for this module: compare every analytic gradient
    /// against a central finite difference of the loss.
    ///
    /// Backpropagation is easy to write in a way that looks reasonable and is
    /// subtly wrong — a transposed index, a missing activation derivative, a
    /// block offset off by one. All of those produce gradients that still point
    /// somewhere, so training merely converges badly instead of failing loudly.
    /// Finite differences catch them immediately.
    #[test]
    fn analytic_gradients_match_finite_differences() {
        let spec = spec();
        let mut parameters = spec.initialize(11);
        let batch = batch();

        let (_, analytic) = loss_and_gradient(&spec, &parameters, &batch).expect("gradient");

        const EPS: f32 = 1e-2;
        for index in 0..spec.parameter_count() {
            let original = parameters[index];

            parameters[index] = original + EPS;
            let up = loss(&spec, &parameters, &batch).expect("loss");
            parameters[index] = original - EPS;
            let down = loss(&spec, &parameters, &batch).expect("loss");
            parameters[index] = original;

            let numerical = (up - down) / (2.0 * EPS);
            let difference = (numerical - analytic[index]).abs();
            let scale = numerical.abs().max(analytic[index].abs()).max(1.0);

            assert!(
                difference / scale < 1e-2,
                "parameter {index}: analytic {} vs numerical {} (relative {})",
                analytic[index],
                numerical,
                difference / scale
            );
        }
    }

    #[test]
    fn a_confident_correct_prediction_has_lower_loss_than_a_wrong_one() {
        let spec = spec();
        let parameters = spec.initialize(5);
        let features = vec![0.3, 0.8];

        let probabilities = predict(&spec, &parameters, &features).expect("predict");
        let best = if probabilities[0] > probabilities[1] {
            0
        } else {
            1
        };
        let worst = 1 - best;

        let confident = loss(
            &spec,
            &parameters,
            &[Sample {
                features: features.clone(),
                label: best,
            }],
        )
        .expect("loss");
        let wrong = loss(
            &spec,
            &parameters,
            &[Sample {
                features,
                label: worst,
            }],
        )
        .expect("loss");

        assert!(confident < wrong, "{confident} should be below {wrong}");
    }

    /// Gradient descent on a single batch must reduce the loss. This is the
    /// smallest possible end-to-end check that the gradient points downhill —
    /// a sign error would pass the shape tests and fail here.
    #[test]
    fn gradient_descent_reduces_the_loss() {
        let spec = spec();
        let mut parameters = spec.initialize(13);
        let batch = batch();

        let before = loss(&spec, &parameters, &batch).expect("loss");
        for _ in 0..200 {
            let (_, gradient) = loss_and_gradient(&spec, &parameters, &batch).expect("gradient");
            for (p, g) in parameters.iter_mut().zip(&gradient) {
                *p -= 0.5 * g;
            }
        }
        let after = loss(&spec, &parameters, &batch).expect("loss");

        assert!(after < before, "loss rose from {before} to {after}");
        assert!(after < 0.1, "expected the batch to be fit, got {after}");
    }

    #[test]
    fn shape_mismatches_are_reported_rather_than_panicking() {
        let spec = spec();
        let parameters = spec.initialize(1);

        assert_eq!(
            loss_and_gradient(&spec, &parameters[..3], &batch()),
            Err(ModelError::ParameterCount {
                expected: spec.parameter_count(),
                got: 3
            })
        );
        assert_eq!(
            loss_and_gradient(
                &spec,
                &parameters,
                &[Sample {
                    features: vec![1.0],
                    label: 0
                }]
            ),
            Err(ModelError::FeatureCount {
                expected: 2,
                got: 1
            })
        );
        assert_eq!(
            loss_and_gradient(
                &spec,
                &parameters,
                &[Sample {
                    features: vec![1.0, 2.0],
                    label: 9
                }]
            ),
            Err(ModelError::LabelOutOfRange {
                label: 9,
                outputs: 2
            })
        );
        assert_eq!(
            loss_and_gradient(&spec, &parameters, &[]),
            Err(ModelError::EmptyBatch)
        );
    }
}
