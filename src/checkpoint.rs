//! Persistence for trained model parameters.
//!
//! Training that is not checkpointed is thrown away: an An node that restarts
//! after four hundred epochs would otherwise begin again from its initial seed.
//! This stores the parameter vector to the database periodically and restores
//! the most recent one at startup.
//!
//! Parameters are encrypted at rest with the same shared secret used for
//! inter-node messages. A checkpoint is the model — the entire product of the
//! cluster's work — so it should not sit in the database as plaintext readable
//! by anything holding a connection.

use crate::database::PgPool;
use crate::dataset::DatasetSpec;
use crate::error::AnKiError;
use crate::model::MlpSpec;
use crate::security;
use tracing::{info, warn};
use uuid::Uuid;

/// A restored checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    pub epoch: u64,
    pub parameters: Vec<f32>,
    pub loss: Option<f32>,
}

/// Fingerprint of a training run: the network shape and the dataset it is
/// trained on.
///
/// Parameters are only meaningful for the model that produced them, so this is
/// what keeps a configuration change from resuming into a mismatched vector.
/// Changing the hidden width, the dataset size, or the dataset seed all start a
/// new run rather than silently loading incompatible weights.
pub fn model_id(spec: &MlpSpec, dataset: &DatasetSpec) -> String {
    format!(
        "in{}-h{}-out{}-n{}-val{}-seed{}",
        spec.inputs,
        spec.hidden,
        spec.outputs,
        dataset.samples,
        dataset.validation_samples,
        dataset.seed
    )
}

/// Whether a checkpoint is due after completing `epoch`.
///
/// Epoch zero means nothing has completed yet. An interval of zero disables
/// periodic checkpointing rather than dividing by zero.
pub fn is_checkpoint_due(epoch: u64, interval: u64) -> bool {
    interval != 0 && epoch != 0 && epoch.is_multiple_of(interval)
}

/// Encrypts a parameter vector for storage.
pub fn encode(parameters: &[f32], key: &str) -> Result<Vec<u8>, AnKiError> {
    let json = serde_json::to_string(parameters)
        .map_err(|e| AnKiError::TaskRecovery(format!("failed to serialize parameters: {e}")))?;
    Ok(security::encrypt_message(&json, key)?.into_bytes())
}

/// Decrypts a stored parameter vector.
pub fn decode(stored: &[u8], key: &str) -> Result<Vec<f32>, AnKiError> {
    let encoded = std::str::from_utf8(stored).map_err(|_| AnKiError::InvalidCiphertext)?;
    let json = security::decrypt_message(encoded, key)?;
    serde_json::from_str(&json)
        .map_err(|e| AnKiError::TaskRecovery(format!("failed to decode parameters: {e}")))
}

/// Stores and retrieves model checkpoints.
#[derive(Clone)]
pub struct CheckpointStore {
    pool: PgPool,
    key: String,
}

impl CheckpointStore {
    /// Builds a store, taking the encryption key from the shared node secret.
    pub fn new(pool: PgPool) -> Result<Self, AnKiError> {
        Ok(Self {
            pool,
            key: security::message_key()?,
        })
    }

    /// Writes a checkpoint for `model_id` at `epoch`.
    pub async fn save(
        &self,
        model_id: &str,
        epoch: u64,
        parameters: &[f32],
        loss: Option<f32>,
    ) -> Result<Uuid, AnKiError> {
        let ciphertext = encode(parameters, &self.key)?;
        let checkpoint_id = Uuid::new_v4();

        let connection = self
            .pool
            .get()
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;
        connection
            .execute(
                "INSERT INTO model_checkpoints \
                 (checkpoint_id, model_id, epoch, parameters, loss) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &checkpoint_id,
                    &model_id,
                    &(epoch as i64),
                    &ciphertext,
                    &loss,
                ],
            )
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;

        info!(
            "Saved checkpoint {} for {} at epoch {}",
            checkpoint_id, model_id, epoch
        );
        Ok(checkpoint_id)
    }

    /// Reads the most recent checkpoint for `model_id`, if any.
    ///
    /// `expected_parameters` guards against restoring a vector of the wrong
    /// length. The model id already encodes the shape, so a mismatch means the
    /// stored row is corrupt or was written by an incompatible version; either
    /// way it is discarded rather than loaded.
    pub async fn latest(
        &self,
        model_id: &str,
        expected_parameters: usize,
    ) -> Result<Option<Checkpoint>, AnKiError> {
        let connection = self
            .pool
            .get()
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;
        let row = connection
            .query_opt(
                "SELECT epoch, parameters, loss FROM model_checkpoints \
                 WHERE model_id = $1 ORDER BY epoch DESC LIMIT 1",
                &[&model_id],
            )
            .await
            .map_err(|e| AnKiError::TaskRecovery(e.to_string()))?;

        let Some(row) = row else {
            return Ok(None);
        };

        let epoch: i64 = row.get("epoch");
        let stored: Vec<u8> = row.get("parameters");
        let loss: Option<f32> = row.get("loss");
        let parameters = decode(&stored, &self.key)?;

        if parameters.len() != expected_parameters {
            warn!(
                "Discarding checkpoint for {}: expected {} parameters, stored row has {}",
                model_id,
                expected_parameters,
                parameters.len()
            );
            return Ok(None);
        }

        Ok(Some(Checkpoint {
            epoch: epoch.max(0) as u64,
            parameters,
            loss,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MlpSpec {
        MlpSpec::new(2, 16, 2)
    }

    fn dataset() -> DatasetSpec {
        DatasetSpec::new(512, 20_260_814)
    }

    #[test]
    fn the_model_id_changes_with_anything_that_changes_the_parameters() {
        let base = model_id(&spec(), &dataset());

        assert_eq!(base, model_id(&spec(), &dataset()), "must be stable");
        assert_ne!(
            base,
            model_id(&MlpSpec::new(2, 8, 2), &dataset()),
            "a different hidden width is a different parameter vector"
        );
        assert_ne!(
            base,
            model_id(&spec(), &DatasetSpec::new(512, 1)),
            "a different dataset seed is a different training run"
        );
        assert_ne!(
            base,
            model_id(&spec(), &DatasetSpec::new(256, 20_260_814)),
            "a different dataset size is a different training run"
        );
        assert_ne!(
            base,
            model_id(&spec(), &DatasetSpec::with_validation(512, 20_260_814, 100)),
            "a different hold-out means the model trained on different samples"
        );
    }

    #[test]
    fn parameters_round_trip_through_encryption() {
        let parameters = spec().initialize(3);
        let key = "checkpoint-test-key";

        let stored = encode(&parameters, key).expect("encode");
        let restored = decode(&stored, key).expect("decode");

        assert_eq!(restored, parameters);
    }

    #[test]
    fn stored_parameters_are_not_readable_without_the_key() {
        // A checkpoint is the entire product of the cluster's work; it should
        // not sit in the database as plaintext.
        let parameters = vec![1.5_f32, -2.25, 3.0];
        let stored = encode(&parameters, "right-key").expect("encode");

        assert!(decode(&stored, "wrong-key").is_err());
        let as_text = String::from_utf8_lossy(&stored);
        assert!(!as_text.contains("1.5"), "parameters appeared in plaintext");
    }

    #[test]
    fn corrupt_ciphertext_is_an_error_rather_than_a_panic() {
        assert!(decode(b"not-ciphertext", "key").is_err());
        assert!(decode(&[0xff, 0xfe], "key").is_err());
    }

    #[test]
    fn checkpoints_are_due_on_the_interval_only() {
        assert!(!is_checkpoint_due(0, 25), "nothing has completed yet");
        assert!(!is_checkpoint_due(24, 25));
        assert!(is_checkpoint_due(25, 25));
        assert!(is_checkpoint_due(50, 25));
        assert!(!is_checkpoint_due(51, 25));
    }

    #[test]
    fn a_zero_interval_disables_checkpointing_rather_than_dividing_by_zero() {
        assert!(!is_checkpoint_due(10, 0));
    }

    // These need a live database, so they are gated behind the same feature as
    // the rest of the database-backed tests.
    #[cfg(feature = "integration-tests")]
    mod database_tests {
        use super::*;
        use crate::database::get_pool;

        async fn store() -> CheckpointStore {
            CheckpointStore::new(get_pool().await.expect("pool")).expect("store")
        }

        #[tokio::test]
        async fn a_saved_checkpoint_comes_back() {
            let store = store().await;
            let run = format!("test-{}", Uuid::new_v4());
            let parameters = spec().initialize(5);

            store
                .save(&run, 42, &parameters, Some(0.25))
                .await
                .expect("save");
            let restored = store
                .latest(&run, parameters.len())
                .await
                .expect("read")
                .expect("checkpoint present");

            assert_eq!(restored.epoch, 42);
            assert_eq!(restored.parameters, parameters);
            assert_eq!(restored.loss, Some(0.25));
        }

        #[tokio::test]
        async fn the_most_recent_epoch_wins() {
            let store = store().await;
            let run = format!("test-{}", Uuid::new_v4());
            let early = spec().initialize(1);
            let late = spec().initialize(2);

            store.save(&run, 10, &early, None).await.expect("save");
            store.save(&run, 20, &late, None).await.expect("save");

            let restored = store
                .latest(&run, late.len())
                .await
                .expect("read")
                .expect("present");
            assert_eq!(restored.epoch, 20);
            assert_eq!(restored.parameters, late);
        }

        #[tokio::test]
        async fn an_unknown_run_has_no_checkpoint() {
            let store = store().await;
            let run = format!("never-saved-{}", Uuid::new_v4());
            assert!(store.latest(&run, 8).await.expect("read").is_none());
        }

        #[tokio::test]
        async fn a_row_of_the_wrong_shape_is_discarded_rather_than_returned() {
            let store = store().await;
            let run = format!("test-{}", Uuid::new_v4());
            store
                .save(&run, 1, &[1.0, 2.0, 3.0], None)
                .await
                .expect("save");

            // Ask for a model that expects a different parameter count.
            assert!(store.latest(&run, 99).await.expect("read").is_none());
        }
    }
}
