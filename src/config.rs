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

    fn validate(self) -> Result<Self, ConfigError> {
        if self.model_shards == 0 {
            return Err(ConfigError::Message(
                "model_shards must be greater than 0".into(),
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
