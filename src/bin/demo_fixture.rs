//! Emits the reference values the browser demo checks itself against.
//!
//! `docs/` ships a port of [`dataset`] and [`model`] so the simulation on the
//! page trains on the same data the cluster would, from the same seeds. A port
//! is only worth anything if it stays a port, so this binary prints what the
//! real code produces and `docs/tests/rng.test.mjs` asserts the JavaScript
//! reproduces it exactly, down to the last bit of every `f32`.
//!
//! Regenerate the fixture after touching the dataset or the parameter
//! initialisation:
//!
//! ```text
//! cargo run --bin demo_fixture > docs/assets/sim/fixtures/reference.json
//! ```

use distributed_neural_network::dataset::{self, DatasetSpec};

const SAMPLES: usize = 512;
const DATASET_SEED: u64 = 20_260_814;
const VALIDATION_SAMPLES: usize = 102;
const HIDDEN: usize = 16;
const INIT_SEED: u64 = 7;

/// Enough leading samples to catch a generator that diverges after its first
/// buffer refill rather than on its first draw.
const PREVIEW: usize = 8;

fn sample_json(sample: &distributed_neural_network::model::Sample) -> String {
    format!(
        "{{ \"features\": [{}, {}], \"label\": {} }}",
        sample.features[0] as f64, sample.features[1] as f64, sample.label
    )
}

fn main() {
    let spec = DatasetSpec::with_validation(SAMPLES, DATASET_SEED, VALIDATION_SAMPLES);
    let samples = dataset::generate(spec);
    let model_spec = spec.model_spec(HIDDEN);
    let parameters = model_spec.initialize(INIT_SEED);

    let inside = samples.iter().filter(|sample| sample.label == 1).count();
    // Summed in f64 so the total is a function of the sample values alone, not
    // of the order f32 rounding happens to accumulate in.
    let checksum: f64 = samples
        .iter()
        .flat_map(|sample| sample.features.iter())
        .map(|&feature| feature as f64)
        .sum();

    let preview: Vec<String> = samples.iter().take(PREVIEW).map(sample_json).collect();
    let initial: Vec<String> = parameters
        .iter()
        .map(|&value| (value as f64).to_string())
        .collect();

    println!("{{");
    println!("  \"generated_by\": \"cargo run --bin demo_fixture\",");
    println!(
        "  \"dataset\": {{ \"samples\": {SAMPLES}, \"seed\": {DATASET_SEED}, \"validation_samples\": {VALIDATION_SAMPLES} }},"
    );
    println!(
        "  \"model\": {{ \"inputs\": {}, \"hidden\": {}, \"outputs\": {}, \"init_seed\": {INIT_SEED} }},",
        model_spec.inputs, model_spec.hidden, model_spec.outputs
    );
    println!("  \"first_samples\": [");
    println!("    {}", preview.join(",\n    "));
    println!("  ],");
    println!(
        "  \"last_sample\": {},",
        sample_json(&samples[samples.len() - 1])
    );
    println!("  \"inside_count\": {inside},");
    println!("  \"feature_checksum\": {checksum},");
    println!("  \"initial_parameters\": [{}]", initial.join(", "));
    println!("}}");
}
