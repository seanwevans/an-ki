// security.rs: Implements security mechanisms including encryption, decryption, and authentication for nodes.

use base64::{decode as base64_decode, encode as base64_encode};
use chrono::{Duration, Utc};
use hmac::{Hmac, Mac};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::env;
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
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())?;
    mac.update(message.as_bytes());
    let result = mac.finalize().into_bytes();
    Ok(base64_encode(&result))
}

pub fn decrypt_message(encoded_message: &str, key: &str) -> Result<String, Box<dyn Error>> {
    let decoded_bytes = base64_decode(encoded_message)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())?;
    mac.update(&decoded_bytes);
    let result = mac.finalize().into_bytes();
    let decrypted_message = String::from_utf8(result.to_vec())?;
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
        let decrypted_message = decrypt_message(&encrypted_message, key).unwrap();

        assert_eq!(decrypted_message, message);
    }
}
