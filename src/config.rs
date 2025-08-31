// config.rs: Centralizes configuration settings for the distributed neural network.

use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub amqp_addr: String,
    pub jwt_secret_key: String,
    pub database_url: String,
    /// Number of model shards expected when aggregating gradients.
    pub model_shards: usize,
    /// Number of training epochs the scheduler should execute.
    pub training_epochs: u32,
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

        s.try_into()
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
        assert!(settings.model_shards > 0);
        assert!(settings.training_epochs > 0);
    }
}
