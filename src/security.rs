// security.rs: Implements security mechanisms including encryption, decryption, and authentication for nodes.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose, Engine as _};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use rand::rngs::OsRng;
use rand::RngCore;
use ring::pbkdf2::{self, PBKDF2_HMAC_SHA256};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::time::{SystemTime, UNIX_EPOCH};
use webpki::{EndEntityCert, Time, TrustAnchor, ECDSA_P256_SHA256};

use std::env;
use std::error::Error;
use tracing::info;

use crate::common::NodeRole;
use crate::error::AnKiError;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,    // Subject (usually the node ID)
    pub role: NodeRole, // Role of the node (e.g., principal, an, ki)
    pub exp: usize,     // Expiration time as a UNIX timestamp
}

fn get_secret_key() -> Result<String, env::VarError> {
    env::var("JWT_SECRET_KEY")
}

pub fn generate_token(
    node_id: &str,
    role: NodeRole,
    expiration_minutes: i64,
) -> Result<String, Box<dyn Error>> {
    let expiration = Utc::now() + Duration::minutes(expiration_minutes);
    let claims = Claims {
        sub: node_id.to_owned(),
        role,
        exp: expiration.timestamp() as usize,
    };

    let secret_key = get_secret_key()?;
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret_key.as_ref()),
    )?;
    info!(
        "Generated token for node_id: {} with role: {:?}",
        node_id, claims.role
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
        "Verified token for node_id: {} with role: {:?}",
        token_data.claims.sub, token_data.claims.role
    );
    Ok(token_data)
}

pub fn renew_token(token: &str, expiration_minutes: i64) -> Result<String, Box<dyn Error>> {
    let token_data = verify_token(token)?;
    let Claims { sub, role, .. } = &token_data.claims;
    generate_token(sub, role.clone(), expiration_minutes)
}

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const PBKDF2_ITERATIONS: u32 = 100_000;

pub fn encrypt_message(message: &str, key: &str) -> Result<String, Box<dyn Error>> {
    // Generate a random salt for key derivation using a secure RNG
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);

    // Derive a 256-bit key using PBKDF2 with the salt
    let mut derived_key = [0u8; 32];
    pbkdf2::derive(
        PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERATIONS).unwrap(),
        &salt,
        key.as_bytes(),
        &mut derived_key,
    );
    let cipher_key = Key::<Aes256Gcm>::from_slice(&derived_key);
    let cipher = Aes256Gcm::new(cipher_key);

    // Generate a random nonce using a secure RNG
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the message
    let ciphertext = cipher
        .encrypt(nonce, message.as_bytes())
        .map_err(|e| Box::<dyn Error>::from(e.to_string()))?;

    // Prepend salt and nonce to the ciphertext so they can be used for decryption
    let mut combined = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    combined.extend_from_slice(&salt);
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(general_purpose::STANDARD.encode(&combined))
}

pub fn decrypt_message(encoded_message: &str, key: &str) -> Result<String, AnKiError> {
    let decoded_bytes = general_purpose::STANDARD
        .decode(encoded_message)
        .map_err(|e| AnKiError::CryptoError(e.to_string()))?;

    if decoded_bytes.len() < SALT_LEN + NONCE_LEN {
        return Err(AnKiError::InvalidCiphertext);
    }

    // Split salt, nonce, and ciphertext
    let (salt, rest) = decoded_bytes.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);

    // Derive the same 256-bit key using PBKDF2 with the salt
    let mut derived_key = [0u8; 32];
    pbkdf2::derive(
        PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERATIONS).unwrap(),
        salt,
        key.as_bytes(),
        &mut derived_key,
    );
    let cipher_key = Key::<Aes256Gcm>::from_slice(&derived_key);
    let cipher = Aes256Gcm::new(cipher_key);

    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| AnKiError::CryptoError(e.to_string()))?;
    let decrypted_message = String::from_utf8(plaintext)
        .map_err(|e| AnKiError::CryptoError(e.to_string()))?;
    Ok(decrypted_message)
}

/// Validates `cert_der` against the provided CA certificate `ca_der`.
pub fn validate_certificate(cert_der: &[u8], ca_der: &[u8]) -> Result<(), Box<dyn Error>> {
    let anchor = TrustAnchor::try_from_cert_der(ca_der).map_err(|e| e.to_string())?;
    let anchors = [anchor];
    let trust_anchors = webpki::TlsServerTrustAnchors(&anchors);
    let cert = EndEntityCert::try_from(cert_der).map_err(|e| e.to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    cert.verify_is_valid_tls_server_cert(
        &[&ECDSA_P256_SHA256],
        &trust_anchors,
        &[],
        Time::from_seconds_since_unix_epoch(now.as_secs()),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Generates a random challenge to be signed by a peer.
pub fn generate_challenge() -> [u8; 32] {
    let mut challenge = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut challenge);
    challenge
}

/// Signs the `challenge` using the node's private key in DER format.
pub fn sign_challenge(challenge: &[u8], key_der: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key_der)?;
    let sig = key_pair.sign(&SystemRandom::new(), challenge)?;
    Ok(sig.as_ref().to_vec())
}

/// Verifies a signed challenge using the peer's certificate DER bytes.
pub fn verify_challenge(
    challenge: &[u8],
    signature: &[u8],
    cert_der: &[u8],
) -> Result<(), Box<dyn Error>> {
    let cert = EndEntityCert::try_from(cert_der).map_err(|e| e.to_string())?;
    cert.verify_signature(&ECDSA_P256_SHA256, challenge, signature)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decrypt_message, encrypt_message, generate_challenge, generate_token, renew_token,
        sign_challenge, validate_certificate, verify_challenge, verify_token, Claims,
        NONCE_LEN, SALT_LEN,
    };
    use crate::common::NodeRole;
    use crate::error::AnKiError;
    use base64::{engine::general_purpose, Engine as _};
    use rcgen::{
        BasicConstraints, Certificate as RcCertificate, CertificateParams, ExtendedKeyUsagePurpose,
        IsCa,
    };

    #[test]
    fn test_generate_and_verify_token() {
        std::env::set_var("JWT_SECRET_KEY", "test_secret_key");
        let node_id = "test_node";
        let role = NodeRole::An;
        let token = generate_token(node_id, role.clone(), 60).unwrap();
        let token_data = verify_token(&token).unwrap();
        let Claims {
            sub,
            role: claim_role,
            ..
        } = token_data.claims;

        assert_eq!(sub, node_id);
        assert_eq!(claim_role, role);
    }

    #[test]
    fn test_encrypt_and_decrypt_message() {
        let message = "This is a secret message.";
        let key = "encryption_key";
        let encrypted_message1 = encrypt_message(message, key).unwrap();
        let encrypted_message2 = encrypt_message(message, key).unwrap();

        // Different salts and nonces should produce different ciphertexts
        assert_ne!(encrypted_message1, encrypted_message2);

        let decrypted_message1 = decrypt_message(&encrypted_message1, key).unwrap();
        let decrypted_message2 = decrypt_message(&encrypted_message2, key).unwrap();

        assert_eq!(decrypted_message1, message);
        assert_eq!(decrypted_message2, message);

        // Decryption should fail with an incorrect key
        assert!(decrypt_message(&encrypted_message1, "wrong_key").is_err());

        // Truncated ciphertext should return InvalidCiphertext
        let mut truncated = general_purpose::STANDARD
            .decode(&encrypted_message1)
            .unwrap();
        truncated.truncate(SALT_LEN + NONCE_LEN - 1);
        let truncated = general_purpose::STANDARD.encode(&truncated);
        assert!(matches!(
            decrypt_message(&truncated, key),
            Err(AnKiError::InvalidCiphertext)
        ));
    }

    #[test]
    fn test_renew_token() {
        std::env::set_var("JWT_SECRET_KEY", "test_secret_key");
        let token = generate_token("node", NodeRole::Ki, 1).unwrap();
        let renewed = renew_token(&token, 60).unwrap();
        let data = verify_token(&renewed).unwrap();
        assert_eq!(data.claims.sub, "node");
    }

    #[test]
    fn test_certificate_validation_and_challenge() {
        // Generate a CA certificate
        let mut ca_params = CertificateParams::new(vec!["ca".into()]);
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = RcCertificate::from_params(ca_params).unwrap();
        let ca_der = ca.serialize_der().unwrap();

        // Generate node certificate signed by CA
        let mut node_params = CertificateParams::new(vec!["node".into()]);
        node_params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let node = RcCertificate::from_params(node_params).unwrap();
        let node_der = node.serialize_der_with_signer(&ca).unwrap();

        // Validate
        validate_certificate(&node_der, &ca_der).unwrap();

        // Challenge-response
        let challenge = generate_challenge();
        let key_der = node.serialize_private_key_der();
        let sig = sign_challenge(&challenge, &key_der).unwrap();
        verify_challenge(&challenge, &sig, &node_der).unwrap();
    }
}
