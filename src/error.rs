use thiserror::Error;

#[derive(Debug, Error)]
pub enum AnKiError {
    #[error("Messaging error: {0}")]
    Messaging(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Security error: {0}")]
    Security(String),
    #[error("Scheduler error: {0}")]
    Scheduler(String),
    #[error("Task recovery error: {0}")]
    TaskRecovery(String),
    #[error("Invalid ciphertext")]
    InvalidCiphertext,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_variant_context() {
        assert_eq!(
            AnKiError::Messaging("boom".into()).to_string(),
            "Messaging error: boom"
        );
        assert_eq!(
            AnKiError::Network("down".into()).to_string(),
            "Network error: down"
        );
        assert_eq!(
            AnKiError::Config("bad".into()).to_string(),
            "Config error: bad"
        );
        assert_eq!(
            AnKiError::Security("nope".into()).to_string(),
            "Security error: nope"
        );
        assert_eq!(
            AnKiError::Scheduler("late".into()).to_string(),
            "Scheduler error: late"
        );
        assert_eq!(
            AnKiError::TaskRecovery("lost".into()).to_string(),
            "Task recovery error: lost"
        );
        assert_eq!(
            AnKiError::InvalidCiphertext.to_string(),
            "Invalid ciphertext"
        );
    }
}
