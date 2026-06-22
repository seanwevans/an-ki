// validation.rs: Implements data validation logic for messages exchanged between nodes.

use lazy_static::lazy_static;
use regex::Regex;
use std::error::Error;
use tracing::{error, info};
use uuid::Uuid;

lazy_static! {
    static ref IP_REGEX: Regex = Regex::new(
        r"^((25[0-5]|2[0-4][0-9]|[0-1]?[0-9][0-9]?)\.){3}(25[0-5]|2[0-4][0-9]|[0-1]?[0-9][0-9]?)$",
    )
    .expect("Failed to compile IP regex");
    static ref MESSAGE_REGEX: Regex =
        Regex::new(r"^[a-zA-Z0-9]+$").expect("Failed to compile message format regex");
}

#[derive(Debug)]
pub struct Validator;

impl Validator {
    pub fn validate_uuid(uuid_str: &str) -> Result<Uuid, Box<dyn Error>> {
        match Uuid::parse_str(uuid_str) {
            Ok(uuid) => {
                info!("Valid UUID: {}", uuid);
                Ok(uuid)
            }
            Err(e) => {
                error!("Invalid UUID: {}. Error: {}", uuid_str, e);
                Err(Box::new(e))
            }
        }
    }

    pub fn validate_ip_address(ip_address: &str) -> Result<(), Box<dyn Error>> {
        if IP_REGEX.is_match(ip_address) {
            info!("Valid IP address: {}", ip_address);
            Ok(())
        } else {
            error!("Invalid IP address: {}", ip_address);
            Err("Invalid IP address format".into())
        }
    }

    pub fn validate_message_format(message: &str) -> Result<(), Box<dyn Error>> {
        if MESSAGE_REGEX.is_match(message) {
            info!("Message format is valid: {}", message);
            Ok(())
        } else {
            error!("Message format is invalid: {}", message);
            Err("Invalid message format".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_uuid() {
        let valid_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let invalid_uuid = "invalid-uuid";
        assert!(Validator::validate_uuid(valid_uuid).is_ok());
        assert!(Validator::validate_uuid(invalid_uuid).is_err());
    }

    #[test]
    fn test_validate_ip_address() {
        let valid_ip = "192.168.1.1";
        let invalid_ip = "999.999.999.999";
        assert!(Validator::validate_ip_address(valid_ip).is_ok());
        assert!(Validator::validate_ip_address(invalid_ip).is_err());
    }

    #[test]
    fn test_validate_message_format() {
        let valid_message = "Hello123"; // contains only alphanumeric characters
        let invalid_message = "Hello_123"; // contains an underscore, which is invalid
        assert!(Validator::validate_message_format(valid_message).is_ok());
        assert!(Validator::validate_message_format(invalid_message).is_err());
    }
}
