//! The training data, and how it is divided between workers.
//!
//! Data is *generated*, not shipped. Every node derives the identical dataset
//! from a shared seed and then takes only its own shard, so the wire carries
//! parameters and gradients but never samples. That is how data-parallel
//! training is normally arranged — moving a dataset once per epoch would dwarf
//! the gradients in payload size — and it makes a shard assignment
//! reproducible from nothing but two integers.
//!
//! The task is deliberately not linearly separable: points inside a circle are
//! one class, points outside it the other. A model with no hidden layer cannot
//! draw that boundary, so a network that trains successfully here has genuinely
//! used its hidden units.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::model::{MlpSpec, Sample};

/// Feature count of the generated task.
pub const INPUTS: usize = 2;
/// Class count of the generated task.
pub const OUTPUTS: usize = 2;

/// Radius of the decision boundary.
///
/// For points drawn uniformly from `[-1, 1]^2`, the fraction inside a circle of
/// radius `r` is `pi * r^2 / 4`. This value is `sqrt(2 / pi)`, which puts about
/// half the points inside — a balanced problem, so accuracy is meaningful and a
/// model cannot score well by always guessing one class.
const RADIUS: f32 = 0.797_884_6;

/// Describes a dataset completely: same values, same samples, on any node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetSpec {
    pub samples: usize,
    pub seed: u64,
}

impl DatasetSpec {
    pub fn new(samples: usize, seed: u64) -> Self {
        Self { samples, seed }
    }

    /// The network shape this dataset calls for, given a hidden width.
    pub fn model_spec(&self, hidden: usize) -> MlpSpec {
        MlpSpec::new(INPUTS, hidden, OUTPUTS)
    }
}

/// Generates the dataset described by `spec`.
///
/// Deterministic in the seed, which is what allows every worker to reconstruct
/// the same data independently and take a disjoint slice of it.
pub fn generate(spec: DatasetSpec) -> Vec<Sample> {
    let mut rng = StdRng::seed_from_u64(spec.seed);
    (0..spec.samples)
        .map(|_| {
            let x: f32 = rng.gen_range(-1.0..1.0);
            let y: f32 = rng.gen_range(-1.0..1.0);
            let label = usize::from(x * x + y * y < RADIUS * RADIUS);
            Sample {
                features: vec![x, y],
                label,
            }
        })
        .collect()
}

/// The half-open range of `total` items belonging to shard `index` of `count`.
///
/// Shards are contiguous, disjoint, and cover the whole dataset. Sizes differ by
/// at most one: the first `total % count` shards take one extra item. Contiguity
/// is not important to correctness — the data is unordered — but it makes a
/// shard cheap to describe and to reason about.
///
/// A shard index at or beyond `count`, or a zero `count`, yields an empty range
/// rather than panicking: these arrive from configuration and from the wire.
pub fn shard_range(total: usize, index: usize, count: usize) -> std::ops::Range<usize> {
    if count == 0 || index >= count {
        return 0..0;
    }
    let base = total / count;
    let remainder = total % count;
    // Shards before `remainder` are one longer, so the start offset accumulates
    // one extra item for each of them.
    let start = index * base + index.min(remainder);
    let len = base + usize::from(index < remainder);
    start..(start + len)
}

/// Borrows shard `index` of `count` from `samples`.
pub fn shard(samples: &[Sample], index: usize, count: usize) -> &[Sample] {
    &samples[shard_range(samples.len(), index, count)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_reproducible_for_a_seed() {
        // Workers reconstruct the dataset independently rather than receiving
        // it, so two nodes disagreeing here would silently train on different
        // data while averaging their gradients together.
        let spec = DatasetSpec::new(64, 7);
        assert_eq!(generate(spec), generate(spec));
        assert_ne!(generate(spec), generate(DatasetSpec::new(64, 8)));
    }

    #[test]
    fn samples_have_the_declared_shape() {
        let samples = generate(DatasetSpec::new(50, 1));
        assert_eq!(samples.len(), 50);
        assert!(samples.iter().all(|s| s.features.len() == INPUTS));
        assert!(samples.iter().all(|s| s.label < OUTPUTS));
    }

    #[test]
    fn labels_follow_the_circle_rule() {
        for sample in generate(DatasetSpec::new(200, 3)) {
            let (x, y) = (sample.features[0], sample.features[1]);
            let inside = x * x + y * y < RADIUS * RADIUS;
            assert_eq!(sample.label, usize::from(inside));
        }
    }

    #[test]
    fn the_classes_are_roughly_balanced() {
        // A lopsided dataset would let a model score well by always guessing the
        // majority class, making accuracy useless as a signal that it learned.
        let samples = generate(DatasetSpec::new(2_000, 5));
        let inside = samples.iter().filter(|s| s.label == 1).count();
        let fraction = inside as f32 / samples.len() as f32;
        assert!(
            (0.45..=0.55).contains(&fraction),
            "class balance was {fraction}"
        );
    }

    #[test]
    fn the_task_needs_a_non_linear_boundary() {
        // Both classes appear on both sides of the midpoint of each feature, so
        // no single-feature threshold separates them. A model without a hidden
        // layer cannot draw this boundary, which is what makes success here
        // evidence that the hidden units did something.
        let samples = generate(DatasetSpec::new(500, 9));
        for feature in 0..INPUTS {
            for &label in &[0usize, 1usize] {
                assert!(
                    samples
                        .iter()
                        .any(|s| s.label == label && s.features[feature] < 0.0),
                    "no class {label} below zero on feature {feature}"
                );
                assert!(
                    samples
                        .iter()
                        .any(|s| s.label == label && s.features[feature] > 0.0),
                    "no class {label} above zero on feature {feature}"
                );
            }
        }
    }

    #[test]
    fn shards_are_disjoint_and_cover_everything() {
        for total in [0usize, 1, 7, 100] {
            for count in 1usize..=8 {
                let mut covered = Vec::new();
                for index in 0..count {
                    covered.extend(shard_range(total, index, count));
                }
                covered.sort_unstable();
                assert_eq!(
                    covered,
                    (0..total).collect::<Vec<_>>(),
                    "total={total} count={count}"
                );
            }
        }
    }

    #[test]
    fn shard_sizes_differ_by_at_most_one() {
        // Uneven shards would weight each worker's samples differently once the
        // An node averages their gradients.
        let total = 100;
        let count = 7;
        let sizes: Vec<usize> = (0..count)
            .map(|index| shard_range(total, index, count).len())
            .collect();
        let min = *sizes.iter().min().unwrap();
        let max = *sizes.iter().max().unwrap();
        assert!(max - min <= 1, "shard sizes were {sizes:?}");
        assert_eq!(sizes.iter().sum::<usize>(), total);
    }

    #[test]
    fn out_of_range_shards_are_empty_rather_than_panicking() {
        // Shard indices arrive from the wire, so a bad one must not take a node
        // down.
        assert!(shard_range(10, 3, 3).is_empty());
        assert!(shard_range(10, 99, 3).is_empty());
        assert!(shard_range(10, 0, 0).is_empty());
    }

    #[test]
    fn every_worker_sees_different_data() {
        // The point of sharding: averaging N copies of an identical gradient
        // yields exactly one gradient's worth of signal, which is not
        // distributed training.
        let samples = generate(DatasetSpec::new(60, 2));
        let first = shard(&samples, 0, 3);
        let second = shard(&samples, 1, 3);
        let third = shard(&samples, 2, 3);

        assert_eq!(first.len() + second.len() + third.len(), samples.len());
        assert_ne!(first, second);
        assert_ne!(second, third);
    }

    #[test]
    fn a_single_shard_is_the_whole_dataset() {
        let samples = generate(DatasetSpec::new(25, 4));
        assert_eq!(shard(&samples, 0, 1), &samples[..]);
    }
}
