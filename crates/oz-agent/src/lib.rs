// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Coding agent harness installers (`oz agent setup`), `oz mcp` server, and `refs/notes/ai`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Supported AI coding agent harnesses configured by `oz agent setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarness {
    ClaudeCode,
    Cursor,
    CodexCli,
    Windsurf,
    Antigravity,
}

/// Line-range attribution written under `refs/notes/ai`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRangeAttribution {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub author_kind: String,
}

/// Structured provenance payload persisted to `refs/notes/ai`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiProvenanceNote {
    pub schema_version: u32,
    pub session_id: String,
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_summary: Option<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub line_ranges: Vec<LineRangeAttribution>,
}
