// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Coding agent harness installers (`oz agent setup`), `oz mcp` server, `refs/notes/ai`
//! provenance engine, and `oz blame` human vs. agent line attribution.

pub mod blame;
pub mod mcp;
pub mod notes;
pub mod setup;

pub use blame::{BlameLine, BlameOptions, BlameReport, blame_file, format_blame_report};
pub use mcp::{
    CiLogEntry, CiLogProvider, CiLogsReport, CiLogsRequest, JSONRPC_INTERNAL_ERROR,
    JSONRPC_INVALID_PARAMS, JSONRPC_INVALID_REQUEST, JSONRPC_METHOD_NOT_FOUND, JSONRPC_PARSE_ERROR,
    MCP_PROTOCOL_VERSION_2024, MCP_PROTOCOL_VERSION_2025, McpCallToolResult, McpServer,
    McpToolContent, MessageFraming, mcp_tool_definitions,
};
pub use notes::{
    AI_NOTES_REF, AI_PROVENANCE_SCHEMA_VERSION, DetectedAgentSession, ProvenanceOptions,
    compute_commit_line_attributions, detect_agent_session, detect_agent_session_from_map,
    list_provenance_notes, read_provenance_note, record_commit_provenance, write_provenance_note,
};
pub use setup::{
    AGENTS_MD_END_MARKER, AGENTS_MD_START_MARKER, AgentSetupOptions, AgentSetupReport,
    OZ_CHECKPOINT_HOOK_CMD, detect_env_harnesses, detect_harnesses, detect_harnesses_with_context,
    detect_repo_harnesses, render_ozone_workflow_body, setup_agent_harnesses,
};

use chrono::{DateTime, Utc};
use oz_core::OzError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Supported AI coding agent harnesses configured by `oz agent setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarness {
    ClaudeCode,
    Cursor,
    CodexCli,
    Windsurf,
    Antigravity,
}

impl AgentHarness {
    /// All supported harnesses in canonical deterministic order.
    pub const ALL: [Self; 5] = [
        Self::ClaudeCode,
        Self::Cursor,
        Self::CodexCli,
        Self::Windsurf,
        Self::Antigravity,
    ];

    /// Canonical CLI slug (`claude-code`, `cursor`, `codex-cli`, `windsurf`, `antigravity`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::CodexCli => "codex-cli",
            Self::Windsurf => "windsurf",
            Self::Antigravity => "antigravity",
        }
    }

    /// Human-readable harness display name.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::CodexCli => "Codex CLI",
            Self::Windsurf => "Windsurf",
            Self::Antigravity => "Antigravity",
        }
    }
}

impl fmt::Display for AgentHarness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentHarness {
    type Err = OzError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase();
        match norm.as_str() {
            "claude-code" | "claude_code" | "claude" => Ok(Self::ClaudeCode),
            "cursor" => Ok(Self::Cursor),
            "codex-cli" | "codex_cli" | "codex" => Ok(Self::CodexCli),
            "windsurf" | "codeium" => Ok(Self::Windsurf),
            "antigravity" | "agy" | "gemini" => Ok(Self::Antigravity),
            _ => Err(OzError::InvalidUsage {
                message: format!(
                    "unknown agent harness '{s}'; expected one of: claude-code, cursor, codex-cli, windsurf, antigravity"
                ),
                recovery: vec!["oz agent setup --help".to_string()],
            }),
        }
    }
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
