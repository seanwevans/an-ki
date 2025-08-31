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
