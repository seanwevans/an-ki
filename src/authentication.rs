// authentication.rs: Handles authentication and authorization using JWT tokens for role-based access control.

use jsonwebtoken::{encode, decode, Header, Validation, EncodingKey, DecodingKey, TokenData};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tracing::{info, error};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,  // Subject (usually the node ID)
    role: String, // Role of the node (e.g., principal, teacher, ki)
    exp: usize,   // Expiration time as a UNIX timestamp
}

const SECRET_KEY: &str = env!("JWT_SECRET_KEY");

pub fn generate_token(node_id: &str, role: &str, expiration: usize) -> Result<String, Box<dyn Error>> {
    let claims = Claims {
        sub: node_id.to_owned(),
        role: role.to_owned(),
        exp: expiration,
    };

    let encoding_key = EncodingKey::from_secret(SECRET_KEY.as_ref());
    let mut retries = 3;
    let mut token_result;
    loop {
        token_result = encode(&Header::default(), &claims, &encoding_key);
        if token_result.is_ok() || retries == 0 {
            break;
        }
        retries -= 1;
        error!("Retrying to generate token...");
    }
    let token = token_result?;
    info!("Generated token for node_id: {} with role: {}", node_id, role);
    Ok(token)
}

pub fn verify_token(token: &str) -> Result<TokenData<Claims>, Box<dyn Error>> {
    let decoding_key = DecodingKey::from_secret(SECRET_KEY.as_ref());
    let mut retries = 3;
    let mut token_data_result;
    loop {
        token_data_result = decode::<Claims>(token, &decoding_key, &Validation::default());
        if token_data_result.is_ok() || retries == 0 {
            break;
        }
        retries -= 1;
        error!("Retrying to verify token...");
    }
    let token_data = token_data_result?;
    info!("Verified token for node_id: {} with role: {}", token_data.claims.sub, token_data.claims.role);
    Ok(token_data)
}

pub fn renew_token(token: &str, new_expiration: usize) -> Result<String, Box<dyn Error>> {
    let mut token_data = verify_token(token)?.claims;
    token_data.exp = new_expiration;

    let new_token = encode(&Header::default(), &token_data, &EncodingKey::from_secret(SECRET_KEY.as_ref()))?;
    info!("Renewed token for node_id: {} with new expiration: {}", token_data.sub, new_expiration);
    Ok(new_token)
}
