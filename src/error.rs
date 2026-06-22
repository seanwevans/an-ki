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
