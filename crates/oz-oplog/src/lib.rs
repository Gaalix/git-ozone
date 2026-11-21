// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lock-free append-only MVCC operation log (`.git/ozone/oplog/`), `< 10 ms` dirty
//! working-tree checkpointing, mutation transaction wrappers, and sub-10 ms `oz undo` / `oz redo`.

pub mod checkpoint;
pub mod dirty;
pub mod fsck;
pub mod gc;
pub mod mutation;
pub mod store;
pub mod types;
pub mod undo;

pub use checkpoint::checkpoint;
pub use dirty::workspace_key;
pub use fsck::fsck_repo;
pub use gc::compact_oplog;
pub use mutation::{
    MutationTransaction, record_mutation, record_mutation_all_branches, record_mutation_with_refs,
};
pub use store::{
    append_entry, ensure_oplog_dirs, entry_path, head_lock_path, head_path, list_entries,
    load_entry, ops_dir, read_head_op_id,
};
pub use types::{
    ActorInfo, ActorKind, CheckpointOutcome, DEFAULT_OPLOG_RETENTION_DAYS, DEFAULT_WORKSPACE_KEY,
    DirtyFileSnapshot, DirtyFileState, FsckCategory, FsckCheckStatus, FsckCheckSummary, FsckIssue,
    FsckOptions, FsckReport, FsckSeverity, OpKind, OpLogEntry, OpLogGcOptions, OpLogGcOutcome,
    RedoOutcome, RefDelta, UndoOutcome, UndoTarget, WsSnapshot,
};
pub use undo::{redo, undo, undo_with_dirty};

use std::sync::{Mutex, OnceLock};
use ulid::{Generator, Ulid};

static ULID_GENERATOR: OnceLock<Mutex<Generator>> = OnceLock::new();

/// Generates a new strictly monotonic, lexicographically sortable Ozone operation ID (`op_<ULID>`).
#[must_use]
pub fn new_op_id() -> String {
    let mutex = ULID_GENERATOR.get_or_init(|| Mutex::new(Generator::new()));
    let ulid = {
        let mut guard = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.generate().unwrap_or_else(|_| Ulid::generate())
    };
    format!("op_{ulid}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeMap;

    #[test]
    fn generates_sortable_op_ids_and_serializes_entry() {
        let id1 = new_op_id();
        let id2 = new_op_id();
        assert!(id1.starts_with("op_"));
        assert_eq!(id1.len(), 29);
        assert_ne!(id1, id2);
        assert!(id1 < id2);

        let entry = OpLogEntry {
            op_id: id1.clone(),
            parents: Vec::new(),
            timestamp: Utc::now(),
            actor: ActorInfo {
                kind: ActorKind::Agent,
                client: Some("ozone-test".to_string()),
                workspace: Some("agent-auth".to_string()),
            },
            kind: OpKind::Mutate,
            command: "oz commit -m test".to_string(),
            message: None,
            inverse_of: None,
            undone_ops: Vec::new(),
            rewound_uncheckpointed: false,
            ref_deltas: BTreeMap::new(),
            ws_snapshots: BTreeMap::new(),
        };
        let raw = serde_json::to_string(&entry).expect("serialize");
        let parsed: OpLogEntry = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(parsed.op_id, id1);
        assert_eq!(parsed.kind, OpKind::Mutate);
    }

    #[test]
    fn deserializes_legacy_m0_oplog_entry_without_new_fields() {
        let legacy_json = r#"{
            "op_id": "op_01J00000000000000000000001",
            "parents": [],
            "timestamp": "2026-11-16T21:40:12Z",
            "actor": { "kind": "agent", "client": "claude-code", "workspace": "agent-auth" },
            "command": "oz commit -m init",
            "ref_deltas": { "refs/heads/main": { "old": null, "new": "abc123" } },
            "ws_snapshots": { "agent-auth": { "base_tree": "111aaa", "dirty_tree": "222bbb" } }
        }"#;

        let parsed: OpLogEntry = serde_json::from_str(legacy_json).expect("deserialize legacy");
        assert_eq!(parsed.kind, OpKind::Mutate);
        assert_eq!(parsed.inverse_of, None);
        assert!(!parsed.rewound_uncheckpointed);
        let snap = &parsed.ws_snapshots["agent-auth"];
        assert_eq!(snap.base_tree, "111aaa");
        assert_eq!(snap.dirty_tree.as_deref(), Some("222bbb"));
        assert!(snap.dirty_files.is_empty());
        assert_eq!(snap.pre_base_tree, None);
    }
}
