// security.rs: Implements security mechanisms including encryption, decryption, and authentication for nodes.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{decode as base64_decode, encode as base64_encode};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::error::Error;
use tracing::{error, info};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,  // Subject (usually the node ID)
    role: String, // Role of the node (e.g., principal, teacher, ki)
    exp: usize,   // Expiration time as a UNIX timestamp
}

fn get_secret_key() -> Result<String, env::VarError> {
    env::var("JWT_SECRET_KEY")
}

pub fn generate_token(
    node_id: &str,
    role: &str,
    expiration_minutes: i64,
) -> Result<String, Box<dyn Error>> {
    let expiration = Utc::now() + Duration::minutes(expiration_minutes);
    let claims = Claims {
        sub: node_id.to_owned(),
        role: role.to_owned(),
        exp: expiration.timestamp() as usize,
    };

    let secret_key = get_secret_key()?;
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret_key.as_ref()),
    )?;
    info!(
        "Generated token for node_id: {} with role: {}",
        node_id, role
    );
    Ok(token)
}

pub fn verify_token(token: &str) -> Result<TokenData<Claims>, Box<dyn Error>> {
    let secret_key = get_secret_key()?;
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret_key.as_ref()),
        &Validation::default(),
    )?;
    info!(
        "Verified token for node_id: {} with role: {}",
        token_data.claims.sub, token_data.claims.role
    );
    Ok(token_data)
}

pub fn encrypt_message(message: &str, key: &str) -> Result<String, Box<dyn Error>> {
    // Derive a 256-bit key from the provided key string using SHA-256
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let key_hash = hasher.finalize();
    let cipher_key = Key::<Aes256Gcm>::from_slice(&key_hash);
    let cipher = Aes256Gcm::new(cipher_key);

    // Generate a random 96-bit nonce
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the message
    let ciphertext = cipher.encrypt(nonce, message.as_bytes())?;

    // Prepend the nonce to the ciphertext so it can be used for decryption
    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(base64_encode(&combined))
}

pub fn decrypt_message(encoded_message: &str, key: &str) -> Result<String, Box<dyn Error>> {
    let decoded_bytes = base64_decode(encoded_message)?;

    if decoded_bytes.len() < 12 {
        return Err("Invalid encrypted message".into());
    }

    // Derive the same 256-bit key from the provided key string
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let key_hash = hasher.finalize();
    let cipher_key = Key::<Aes256Gcm>::from_slice(&key_hash);
    let cipher = Aes256Gcm::new(cipher_key);

    // Split nonce and ciphertext
    let (nonce_bytes, ciphertext) = decoded_bytes.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher.decrypt(nonce, ciphertext)?;
    let decrypted_message = String::from_utf8(plaintext)?;
    Ok(decrypted_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_verify_token() {
        std::env::set_var("JWT_SECRET_KEY", "test_secret_key");
        let node_id = "test_node";
        let role = "teacher";
        let token = generate_token(node_id, role, 60).unwrap();
        let token_data = verify_token(&token).unwrap();

        assert_eq!(token_data.claims.sub, node_id);
        assert_eq!(token_data.claims.role, role);
    }

    #[test]
    fn test_encrypt_and_decrypt_message() {
        let message = "This is a secret message.";
        let key = "encryption_key";

        let encrypted_message = encrypt_message(message, key).unwrap();
        assert_ne!(encrypted_message, message);
        let decrypted_message = decrypt_message(&encrypted_message, key).unwrap();

        assert_eq!(decrypted_message, message);
    }
}
