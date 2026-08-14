// config.rs: Centralizes configuration settings for the distributed neural network.

use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub amqp_addr: String,
    pub jwt_secret_key: String,
    pub database_url: String,
    /// OTLP endpoint for exporting traces.
    pub otlp_endpoint: Option<String>,
    /// Address the task REST API binds to. Defaults to `0.0.0.0:3030` when unset.
    pub api_addr: Option<String>,
    /// Number of model shards expected when aggregating gradients.
    pub model_shards: usize,
    /// Number of training epochs the scheduler should execute.
    pub training_epochs: u32,
    /// Hidden units in the network. The parameter count follows from this and
    /// the dataset's input and output widths, so it is not configured directly.
    #[serde(default = "default_hidden_units")]
    pub hidden_units: usize,
    /// Step size applied to the averaged gradient each epoch.
    #[serde(default = "default_learning_rate")]
    pub learning_rate: f32,
    /// Samples in the generated training set.
    #[serde(default = "default_dataset_samples")]
    pub dataset_samples: usize,
    /// Seed for dataset generation. Every node must agree on this, or workers
    /// would train on different data while averaging their gradients together.
    #[serde(default = "default_dataset_seed")]
    pub dataset_seed: u64,
    /// Seed for the initial parameters.
    #[serde(default = "default_init_seed")]
    pub init_seed: u64,
    /// Fraction of the dataset held out for evaluation rather than training.
    #[serde(default = "default_validation_fraction")]
    pub validation_fraction: f32,
    /// Epochs between model checkpoints. Zero disables checkpointing.
    #[serde(default = "default_checkpoint_interval_epochs")]
    pub checkpoint_interval_epochs: u64,
    /// Milliseconds between training epochs.
    #[serde(default = "default_epoch_interval_ms")]
    pub epoch_interval_ms: u64,
}

fn default_hidden_units() -> usize {
    16
}

fn default_learning_rate() -> f32 {
    1.0
}

fn default_dataset_samples() -> usize {
    512
}

fn default_dataset_seed() -> u64 {
    20_260_814
}

fn default_init_seed() -> u64 {
    7
}

fn default_checkpoint_interval_epochs() -> u64 {
    25
}

fn default_validation_fraction() -> f32 {
    0.2
}

fn default_epoch_interval_ms() -> u64 {
    100
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let mut s = Config::new();

        // Start with a default configuration file. Prefer `default.toml`,
        // but fall back to `default.example` if the former is missing.
        if Path::new("config/default.toml").exists() {
            s.merge(File::with_name("config/default"))?;
        } else {
            s.merge(File::new("config/default.example", FileFormat::Toml))?;
        }

        // Add in environment-specific settings
        let env = env::var("RUN_ENV").unwrap_or_else(|_| "development".into());
        s.merge(File::with_name(&format!("config/{}", env)).required(false))?;

        // Add in settings from environment variables (with a prefix of "APP")
        s.merge(Environment::with_prefix("APP"))?;

        // Allow overrides from unprefixed env vars or file-based entries for ConfigMaps
        override_from_env_or_file(&mut s, "amqp_addr", "AMQP_ADDR")?;
        override_from_env_or_file(&mut s, "jwt_secret_key", "JWT_SECRET_KEY")?;
        override_from_env_or_file(&mut s, "database_url", "DATABASE_URL")?;
        override_from_env_or_file(&mut s, "otlp_endpoint", "OTLP_ENDPOINT")?;
        override_from_env_or_file(&mut s, "api_addr", "API_ADDR")?;

        Self::from_config(s)
    }

    /// Resolves the socket address the task REST API should bind to, falling
    /// back to `0.0.0.0:3030` when `api_addr` is not configured.
    pub fn api_bind_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        self.api_addr.as_deref().unwrap_or("0.0.0.0:3030").parse()
    }

    fn from_config(s: Config) -> Result<Self, ConfigError> {
        let settings: Settings = s.try_into()?;
        settings.validate()
    }

    /// Interval between training epochs.
    pub fn epoch_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.epoch_interval_ms)
    }

    /// Dataset every node reconstructs locally.
    ///
    /// The hold-out is resolved to an exact sample count here rather than
    /// carried as a fraction, so every node divides the data at the same index
    /// no matter how it rounds.
    pub fn dataset_spec(&self) -> crate::dataset::DatasetSpec {
        let held_out = (self.dataset_samples as f32 * self.validation_fraction).round() as usize;
        crate::dataset::DatasetSpec::with_validation(
            self.dataset_samples,
            self.dataset_seed,
            held_out,
        )
    }

    /// Network shape implied by the dataset and the configured hidden width.
    pub fn model_spec(&self) -> crate::model::MlpSpec {
        self.dataset_spec().model_spec(self.hidden_units)
    }

    fn validate(self) -> Result<Self, ConfigError> {
        if self.model_shards == 0 {
            return Err(ConfigError::Message(
                "model_shards must be greater than 0".into(),
            ));
        }
        if self.hidden_units == 0 {
            // With no hidden layer the network is a linear classifier, and the
            // training task is deliberately not linearly separable.
            return Err(ConfigError::Message(
                "hidden_units must be greater than 0".into(),
            ));
        }
        if !(self.learning_rate.is_finite() && self.learning_rate > 0.0) {
            return Err(ConfigError::Message(
                "learning_rate must be a positive, finite number".into(),
            ));
        }
        if !(0.0..1.0).contains(&self.validation_fraction) {
            // A fraction of 1.0 or more would leave nothing to train on.
            return Err(ConfigError::Message(
                "validation_fraction must be at least 0 and below 1".into(),
            ));
        }
        if self.dataset_spec().training_samples() < self.model_shards {
            // Otherwise some shard is empty and its worker has no gradient to
            // return, so the round can never collect a full set.
            return Err(ConfigError::Message(
                "training samples (after the validation hold-out) must be at least \
                 model_shards"
                    .into(),
            ));
        }
        // A zero interval would spin the scheduler as fast as the broker
        // accepts publishes, drowning the Ki nodes in a round they cannot
        // finish before the next one starts.
        if self.epoch_interval_ms == 0 {
            return Err(ConfigError::Message(
                "epoch_interval_ms must be greater than 0".into(),
            ));
        }
        Ok(self)
    }
}

fn override_from_env_or_file(s: &mut Config, key: &str, env_var: &str) -> Result<(), ConfigError> {
    if let Ok(val) = env::var(env_var) {
        s.set(key, val)?;
    } else if let Ok(path) = env::var(format!("{}_FILE", env_var)) {
        if let Ok(contents) = fs::read_to_string(path) {
            s.set(key, contents.trim())?;
        }
    }
    Ok(())
}

/// Convenience function to load [`Settings`] using the default configuration
/// sources. This allows callers to access configuration without needing to
/// instantiate `Settings` directly.
pub fn load_settings() -> Result<Settings, ConfigError> {
    Settings::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_settings() {
        let settings = Settings::new();
        assert!(settings.is_ok());
        let settings = settings.unwrap();
        assert!(!settings.amqp_addr.is_empty());
        assert!(!settings.jwt_secret_key.is_empty());
        assert!(!settings.database_url.is_empty());
        assert!(settings.otlp_endpoint.is_some());
        assert!(settings.model_shards > 0);
        assert!(settings.training_epochs > 0);
    }

    #[test]
    fn test_model_shards_zero_is_invalid() {
        let mut config = Config::new();
        config
            .set("amqp_addr", "amqp://127.0.0.1:5672/%2f")
            .unwrap();
        config.set("jwt_secret_key", "test-secret").unwrap();
        config
            .set(
                "database_url",
                "postgresql://root@localhost:26257/defaultdb?sslmode=disable",
            )
            .unwrap();
        config
            .set("otlp_endpoint", "http://localhost:4317")
            .unwrap();
        config.set("model_shards", 0).unwrap();
        config.set("training_epochs", 10).unwrap();

        let err = Settings::from_config(config).unwrap_err();
        assert!(err
            .to_string()
            .contains("model_shards must be greater than 0"));
    }

    fn settings_with_api_addr(api_addr: Option<&str>) -> Settings {
        Settings {
            amqp_addr: "amqp://127.0.0.1:5672/%2f".to_string(),
            jwt_secret_key: "test-secret".to_string(),
            database_url: "postgresql://root@localhost:26257/defaultdb".to_string(),
            otlp_endpoint: None,
            api_addr: api_addr.map(str::to_string),
            model_shards: 1,
            training_epochs: 1,
            hidden_units: 8,
            learning_rate: 0.5,
            dataset_samples: 64,
            dataset_seed: 1,
            init_seed: 1,
            checkpoint_interval_epochs: 25,
            validation_fraction: 0.2,
            epoch_interval_ms: 250,
        }
    }

    #[test]
    fn api_bind_addr_defaults_to_all_interfaces() {
        let settings = settings_with_api_addr(None);
        assert_eq!(
            settings.api_bind_addr().unwrap(),
            "0.0.0.0:3030".parse().unwrap()
        );
    }

    #[test]
    fn api_bind_addr_uses_configured_value() {
        let settings = settings_with_api_addr(Some("127.0.0.1:8080"));
        assert_eq!(
            settings.api_bind_addr().unwrap(),
            "127.0.0.1:8080".parse().unwrap()
        );
    }

    #[test]
    fn api_bind_addr_rejects_garbage() {
        let settings = settings_with_api_addr(Some("not-an-address"));
        assert!(settings.api_bind_addr().is_err());
    }
}
