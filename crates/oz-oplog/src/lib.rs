// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lock-free append-only MVCC operation log (`.git/ozone/oplog/`), checkpoints, and `oz undo`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ulid::Ulid;

/// Generates a new lexicographically sortable Ozone operation ID (`op_<ULID>`).
#[must_use]
pub fn new_op_id() -> String {
    format!("op_{}", Ulid::generate())
}

/// Originator of an Ozone operation recorded in the OpLog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    System,
}

/// Actor metadata attached to an [`OpLogEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorInfo {
    pub kind: ActorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Reference transition recorded for lock-free CAS undo/redo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefDelta {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Per-workspace tree snapshot linked to an OpLog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsSnapshot {
    pub base_tree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_tree: Option<String>,
}

/// Content-addressed operation log record stored under `.git/ozone/oplog/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogEntry {
    pub op_id: String,
    #[serde(default)]
    pub parents: Vec<String>,
    pub timestamp: DateTime<Utc>,
    pub actor: ActorInfo,
    pub command: String,
    #[serde(default)]
    pub ref_deltas: BTreeMap<String, RefDelta>,
    #[serde(default)]
    pub ws_snapshots: BTreeMap<String, WsSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_sortable_op_ids_and_serializes_entry() {
        let id1 = new_op_id();
        let id2 = new_op_id();
        assert!(id1.starts_with("op_"));
        assert_eq!(id1.len(), 29);
        assert_ne!(id1, id2);

        let entry = OpLogEntry {
            op_id: id1.clone(),
            parents: Vec::new(),
            timestamp: Utc::now(),
            actor: ActorInfo {
                kind: ActorKind::Agent,
                client: Some("ozone-test".to_string()),
                workspace: Some("agent-auth".to_string()),
            },
            command: "oz commit -m test".to_string(),
            ref_deltas: BTreeMap::new(),
            ws_snapshots: BTreeMap::new(),
        };
        let raw = serde_json::to_string(&entry).expect("serialize");
        let parsed: OpLogEntry = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(parsed.op_id, id1);
    }
}
