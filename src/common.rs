use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Represents the kind of task being dispatched between nodes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum TaskType {
    /// Contains gradient information produced by a Ki node.
    GradientUpdate,
    /// Request to pull the latest model parameters from an An node.
    ParameterPull,
}

/// Message structure exchanged between nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub task_id: Uuid,
    /// Nature of the task being transported.
    pub task_type: TaskType,
    /// Payload associated with the task. This may contain serialized gradients
    /// or model parameters depending on the [`TaskType`].
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum NodeRole {
    Principal,
    An,
    Ki,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: Uuid,
    pub address: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub role: NodeRole,
}

/// Returns this process's node identifier, taken from the `NODE_ID` environment
/// variable when set, or a freshly generated UUID otherwise.
pub fn node_id() -> String {
    std::env::var("NODE_ID").unwrap_or_else(|_| Uuid::new_v4().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_round_trips_through_json() {
        let task = Task {
            task_id: Uuid::new_v4(),
            task_type: TaskType::GradientUpdate,
            data: "payload".to_string(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let decoded: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.task_id, task.task_id);
        assert_eq!(decoded.task_type, task.task_type);
        assert_eq!(decoded.data, task.data);
    }

    #[test]
    fn node_role_serializes_to_expected_tokens() {
        assert_eq!(
            serde_json::to_string(&NodeRole::Principal).unwrap(),
            "\"Principal\""
        );
        assert_eq!(serde_json::to_string(&NodeRole::An).unwrap(), "\"An\"");
        assert_eq!(serde_json::to_string(&NodeRole::Ki).unwrap(), "\"Ki\"");
    }

    #[test]
    fn node_info_round_trips_through_json() {
        let now = Utc::now();
        let info = NodeInfo {
            id: Uuid::new_v4(),
            address: Some("127.0.0.1:3030".to_string()),
            last_seen: Some(now),
            role: NodeRole::An,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: NodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, info.id);
        assert_eq!(decoded.address, info.address);
        assert_eq!(decoded.last_seen, info.last_seen);
        assert_eq!(decoded.role, info.role);
    }
}
