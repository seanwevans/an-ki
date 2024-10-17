// config.rs: Centralizes configuration settings for the distributed neural network.

use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub amqp_addr: String,
    pub jwt_secret_key: String,
    pub database_url: String,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let mut s = Config::new();

        // Start with a default configuration file
        s.merge(File::with_name("config/default"))?;

        // Add in environment-specific settings
        let env = env::var("RUN_ENV").unwrap_or_else(|_| "development".into());
        s.merge(File::with_name(&format!("config/{}", env)).required(false))?;

        // Add in settings from environment variables (with a prefix of "APP")
        s.merge(Environment::with_prefix("APP"))?;

        s.try_into()
    }
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
    }
}
