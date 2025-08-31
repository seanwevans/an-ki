// security.rs: Implements security mechanisms including encryption, decryption, and authentication for nodes.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose, Engine as _};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, TokenData, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use webpki::{EndEntityCert, TrustAnchor, Time, ECDSA_P256_SHA256};
use std::time::{SystemTime, UNIX_EPOCH};

use std::env;
use std::error::Error;
use tracing::info;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,  // Subject (usually the node ID)
    pub role: String, // Role of the node (e.g., principal, teacher, ki)
    pub exp: usize,   // Expiration time as a UNIX timestamp
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

pub fn renew_token(token: &str, expiration_minutes: i64) -> Result<String, Box<dyn Error>> {
    let token_data = verify_token(token)?;
    let Claims { sub, role, .. } = &token_data.claims;
    generate_token(sub, role, expiration_minutes)
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
    let ciphertext = cipher
        .encrypt(nonce, message.as_bytes())
        .map_err(|e| Box::<dyn Error>::from(e.to_string()))?;

    // Prepend the nonce to the ciphertext so it can be used for decryption
    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(general_purpose::STANDARD.encode(&combined))
}

pub fn decrypt_message(encoded_message: &str, key: &str) -> Result<String, Box<dyn Error>> {
    let decoded_bytes = general_purpose::STANDARD.decode(encoded_message)?;

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

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| Box::<dyn Error>::from(e.to_string()))?;
    let decrypted_message = String::from_utf8(plaintext)?;
    Ok(decrypted_message)
}

/// Validates `cert_der` against the provided CA certificate `ca_der`.
pub fn validate_certificate(cert_der: &[u8], ca_der: &[u8]) -> Result<(), Box<dyn Error>> {
    let anchor = TrustAnchor::try_from_cert_der(ca_der).map_err(|e| e.to_string())?;
    let anchors = [anchor];
    let trust_anchors = webpki::TlsServerTrustAnchors(&anchors);
    let cert = EndEntityCert::try_from(cert_der).map_err(|e| e.to_string())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
    cert.verify_is_valid_tls_server_cert(&[&ECDSA_P256_SHA256], &trust_anchors, &[], Time::from_seconds_since_unix_epoch(now.as_secs()))
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
pub fn verify_challenge(challenge: &[u8], signature: &[u8], cert_der: &[u8]) -> Result<(), Box<dyn Error>> {
    let cert = EndEntityCert::try_from(cert_der).map_err(|e| e.to_string())?;
    cert.verify_signature(&ECDSA_P256_SHA256, challenge, signature)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        encrypt_message, decrypt_message, generate_token, verify_token, renew_token, Claims,
        validate_certificate, generate_challenge, sign_challenge, verify_challenge,
    };
    use rcgen::{Certificate as RcCertificate, CertificateParams, IsCa, BasicConstraints, ExtendedKeyUsagePurpose};

    #[test]
    fn test_generate_and_verify_token() {
        std::env::set_var("JWT_SECRET_KEY", "test_secret_key");
        let node_id = "test_node";
        let role = "teacher";
        let token = generate_token(node_id, role, 60).unwrap();
        let token_data = verify_token(&token).unwrap();
        let Claims { sub, role: claim_role, .. } = token_data.claims;

        assert_eq!(sub, node_id);
        assert_eq!(claim_role, role);
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

    #[test]
    fn test_renew_token() {
        std::env::set_var("JWT_SECRET_KEY", "test_secret_key");
        let token = generate_token("node", "role", 1).unwrap();
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
