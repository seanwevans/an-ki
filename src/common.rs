use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dataset::DatasetSpec;
use crate::model::MlpSpec;

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

/// What an An node asks a Ki node to compute: the gradient of one shard of the
/// dataset at a given point in parameter space.
///
/// The dataset itself is not included. Every node reconstructs it from
/// [`DatasetSpec`], so the payload stays proportional to the model rather than
/// to the data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradientRequest {
    /// Shape of the network the parameters describe.
    pub spec: MlpSpec,
    /// Dataset every worker rebuilds locally.
    pub dataset: DatasetSpec,
    /// Which shard this worker is responsible for.
    pub shard: usize,
    /// How many shards the dataset is divided into.
    pub shards: usize,
    /// Parameters to evaluate the gradient at.
    pub parameters: Vec<f32>,
}

/// What a Ki node returns: the mean gradient over its shard, with enough
/// context for the An node to combine it correctly with other shards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradientReply {
    /// Mean gradient over the shard, in the model's flat parameter layout.
    pub gradient: Vec<f32>,
    /// Mean loss over the shard, for monitoring convergence.
    pub loss: f32,
    /// Samples the mean was taken over. The An node weights by this so shards
    /// of unequal size still give every sample the same influence.
    pub samples: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeRole {
    Principal,
    An,
    Ki,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Identity the node heartbeats under — see [`node_id`]. This is a free-form
    /// string rather than a [`Uuid`] because `NODE_ID` is typically set to
    /// something meaningful to the orchestrator, such as a Kubernetes pod name.
    pub id: String,
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
            id: "an-0".to_string(),
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
