use uuid::Uuid;
use serde::{Serialize, Deserialize};
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: Uuid,
    pub address: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub role: String,
}
