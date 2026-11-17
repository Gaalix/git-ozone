// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path guardrails (`--scope`/`--deny`), `oz radar` collision detection, atomic in-memory
//! merge (`oz integrate`), conflict sandbox resolution (`oz resolve`), and stacked rebasing.

pub mod guardrails;
pub mod merge;
pub mod radar;
pub mod resolve;
pub(crate) mod sandbox;
pub mod stack;

pub use guardrails::{
    IntegrationGuardrailOptions, check_branch_guardrails_for_integration,
    check_workspace_scope_for_integration,
};
pub use merge::{
    IntegrateOptions, IntegrateOutcome, continue_integration, continue_integration_with_verify,
    integrate,
};
pub use radar::{RadarOptions, RadarReport, scan_radar};
pub use resolve::{
    AbortOutcome, ContinueOutcome, ResolveOptions, ResolveOutcome, abort_integration,
    active_session, continue_session, resolve_conflict,
};
pub use sandbox::{
    ConflictSideRef, IntegrateSessionState, IntegrationMode, IntegrationSession,
    RebasedCommitInfo, ResolvedIntegrateSource, StackRebaseSessionState, StackedBranchPlan,
    StoredConflict, TakeSide,
};
pub use stack::{
    RebasedBranchOutcome, StackBranchStatus, StackCommitSummary, StackRebaseOptions,
    StackRebaseOutcome, StackStatusReport, continue_stack_rebase, stack_rebase, stack_status,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use oz_core::{OzError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Severity level for a cross-workspace collision detected by `oz radar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionSeverity {
    OverlappingHunks,
    SameFileDifferentLines,
    OutOfScope,
}

/// Workspace line range involved in a collision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLineSpan {
    pub ws: String,
    pub lines: String,
}

/// Single collision entry emitted by `oz radar --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollisionReportItem {
    pub path: String,
    pub severity: CollisionSeverity,
    pub workspaces: Vec<WorkspaceLineSpan>,
    pub suggested_action: String,
}

/// Compiled `--scope` and `--deny` glob matcher for workspace path guardrails.
#[derive(Debug, Clone)]
pub struct ScopeFilter {
    allow: Option<GlobSet>,
    deny: GlobSet,
}

impl ScopeFilter {
    /// Compiles optional allow-list (`scope`) and deny-list (`deny`) glob patterns.
    pub fn compile(scope: &[String], deny: &[String]) -> Result<Self> {
        let allow = if scope.is_empty() {
            None
        } else {
            Some(build_globset(scope)?)
        };
        let deny = build_globset(deny)?;
        Ok(Self { allow, deny })
    }

    /// Returns `true` when `rel_path` complies with both `--scope` and `--deny` rules.
    #[must_use]
    pub fn is_allowed(&self, rel_path: &Path) -> bool {
        if self.deny.is_match(rel_path) {
            return false;
        }
        match &self.allow {
            Some(allow) => allow.is_match(rel_path),
            None => true,
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|err| OzError::InvalidUsage {
            message: format!("invalid path guardrail glob '{pat}': {err}"),
            recovery: Vec::new(),
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| OzError::Other(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_scope_and_deny_globs() {
        let filter = ScopeFilter::compile(
            &["crates/auth/**".to_string()],
            &[
                "Cargo.lock".to_string(),
                "crates/auth/secrets/**".to_string(),
            ],
        )
        .expect("compile scope filter");

        assert!(filter.is_allowed(Path::new("crates/auth/src/lib.rs")));
        assert!(!filter.is_allowed(Path::new("crates/auth/secrets/key.pem")));
        assert!(!filter.is_allowed(Path::new("crates/core/src/lib.rs")));
        assert!(!filter.is_allowed(Path::new("Cargo.lock")));
    }
}
