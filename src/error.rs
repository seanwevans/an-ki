use std::fmt;

#[derive(Debug, PartialEq)]
pub enum AnKiError {
    InvalidCiphertext,
    CryptoError(String),
}

impl fmt::Display for AnKiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnKiError::InvalidCiphertext => write!(f, "Invalid ciphertext"),
            AnKiError::CryptoError(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for AnKiError {}
