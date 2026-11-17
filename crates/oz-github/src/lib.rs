// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Git wire protocol client, SSH commit signing, and GitHub PR/CI workflows.

use serde::{Deserialize, Serialize};

/// Supported Git remote transport protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportKind {
    Https,
    Ssh,
}

/// Pull request creation parameters for `oz pr create` / `oz pr stack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSpec {
    pub title: String,
    pub body: String,
    pub head_branch: String,
    pub base_branch: String,
    pub draft: bool,
}

/// Compact CI check summary extracted by `oz ci --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCheckSummary {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub failing_step: Option<String>,
    pub log_excerpt: Option<String>,
}
