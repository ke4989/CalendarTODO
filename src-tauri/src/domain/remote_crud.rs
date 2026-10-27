//! Bounded product admission/status DTOs. No execution capability.
use super::remote_mutation::{
    ConflictResolutionChoice, ConflictResolutionState, RemoteMutationState,
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteCrudKind {
    CreateUtcV2,
    UpdateEventV3,
    Delete,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCrudReceipt {
    pub kind: RemoteCrudKind,
    pub operation_id: String,
    pub account_id: String,
    pub calendar_id: Option<String>,
    pub resource_id: Option<String>,
    pub created_at_ms: i64,
    pub production_execution_available: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCrudOperation {
    pub operation_id: String,
    pub calendar_id: String,
    pub resource_id: String,
    pub state: RemoteMutationState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub resolution_state: Option<ConflictResolutionState>,
    pub retry_operation_id: Option<String>,
    pub revision: i64,
    pub predecessor_operation_id: Option<String>,
    pub stage: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCrudStatus {
    pub account_id: String,
    pub kind: RemoteCrudKind,
    pub operation_count: u32,
    pub operations: Vec<RemoteCrudOperation>,
    pub production_execution_available: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCrudConflictReceipt {
    pub kind: RemoteCrudKind,
    pub operation_id: String,
    pub account_id: String,
    pub choice: ConflictResolutionChoice,
    pub retry_operation_id: Option<String>,
    pub production_execution_available: bool,
}
