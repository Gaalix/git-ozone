// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Canonical `oz.v1` JSON output envelope, exit codes, and non-interactive detection.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::IsTerminal;

/// Version identifier for the canonical Ozone JSON response schema.
pub const OZ_SCHEMA_V1: &str = "oz.v1";

/// Standard Ozone exit code taxonomy:
/// - `0`: Success
/// - `1`: Merge conflict or verification/check failure
/// - `2`: CLI usage, missing argument, configuration, or guardrail error
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    ConflictOrCheckFail = 1,
    UsageOrGuardrail = 2,
}

impl ExitCode {
    /// Returns the numeric POSIX/Windows process exit code.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Converts into [`std::process::ExitCode`].
    #[must_use]
    pub fn to_process_exit_code(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self as u8)
    }

    /// Parses an integer exit code into the closest canonical variant.
    #[must_use]
    pub const fn from_i32(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1 => Self::ConflictOrCheckFail,
            _ => Self::UsageOrGuardrail,
        }
    }
}

impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value.as_i32()
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(value: ExitCode) -> Self {
        value.to_process_exit_code()
    }
}

/// Warning entry inside an [`Envelope`], supporting plain text or structured diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WarningEntry {
    Text(String),
    Structured {
        code: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

impl From<String> for WarningEntry {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for WarningEntry {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

/// Structured conflict entry returned in `oz.v1` envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    pub id: String,
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
    #[serde(default)]
    pub suggested_commands: Vec<String>,
}

/// Machine-readable error payload included when `ok == false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeError {
    pub code: String,
    pub exit_code: i32,
    pub message: String,
}

/// Canonical `oz.v1` JSON output envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T = Value> {
    pub schema: String,
    pub ok: bool,
    pub op_id: Option<String>,
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub data: T,
    #[serde(default)]
    pub warnings: Vec<WarningEntry>,
    #[serde(default)]
    pub conflicts: Vec<ConflictEntry>,
    #[serde(default)]
    pub recovery_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EnvelopeError>,
}

impl<T: Serialize> Envelope<T> {
    /// Creates a successful `oz.v1` envelope wrapping `data`.
    #[must_use]
    pub fn success(data: T) -> Self {
        Self {
            schema: OZ_SCHEMA_V1.to_string(),
            ok: true,
            op_id: None,
            workspace: None,
            branch: None,
            head_oid: None,
            data,
            warnings: Vec::new(),
            conflicts: Vec::new(),
            recovery_commands: Vec::new(),
            error: None,
        }
    }

    /// Sets the `op_id` field.
    #[must_use]
    pub fn with_op_id(mut self, op_id: impl Into<String>) -> Self {
        self.op_id = Some(op_id.into());
        self
    }

    /// Sets the active `workspace` name.
    #[must_use]
    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Sets the active `branch` name.
    #[must_use]
    pub fn with_branch(mut self, branch: Option<String>) -> Self {
        self.branch = branch;
        self
    }

    /// Sets the current `head_oid` commit SHA.
    #[must_use]
    pub fn with_head_oid(mut self, head_oid: Option<String>) -> Self {
        self.head_oid = head_oid;
        self
    }

    /// Appends a warning entry.
    #[must_use]
    pub fn with_warning(mut self, warning: impl Into<WarningEntry>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Appends a conflict entry.
    #[must_use]
    pub fn with_conflict(mut self, conflict: ConflictEntry) -> Self {
        self.conflicts.push(conflict);
        self
    }

    /// Appends a recovery command suggestion.
    #[must_use]
    pub fn with_recovery_command(mut self, command: impl Into<String>) -> Self {
        self.recovery_commands.push(command.into());
        self
    }

    /// Extends recovery commands.
    #[must_use]
    pub fn with_recovery_commands<I, S>(mut self, commands: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.recovery_commands
            .extend(commands.into_iter().map(Into::into));
        self
    }

    /// Renders the envelope as pretty-printed JSON.
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Renders the envelope as single-line compact JSON.
    pub fn to_json_compact(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

impl Envelope<Value> {
    /// Creates a failing `oz.v1` envelope with `data: {}`.
    #[must_use]
    pub fn failure(
        code: impl Into<String>,
        exit_code: ExitCode,
        message: impl Into<String>,
    ) -> Self {
        let msg = message.into();
        let code_str = code.into();
        Self {
            schema: OZ_SCHEMA_V1.to_string(),
            ok: false,
            op_id: None,
            workspace: None,
            branch: None,
            head_oid: None,
            data: serde_json::json!({}),
            warnings: Vec::new(),
            conflicts: Vec::new(),
            recovery_commands: Vec::new(),
            error: Some(EnvelopeError {
                code: code_str,
                exit_code: exit_code.as_i32(),
                message: msg,
            }),
        }
    }

    /// Converts an [`crate::error::OzError`] into a failing `oz.v1` envelope.
    #[must_use]
    pub fn from_error(err: &crate::error::OzError) -> Self {
        let mut env = Self::failure(err.error_code(), err.exit_code(), err.to_string());
        if let crate::error::OzError::Conflict { conflicts, .. } = err {
            env.conflicts = conflicts.clone();
        }
        env.recovery_commands = err.recovery_commands();
        env
    }
}

/// Inputs used to determine non-interactive / agent execution behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionInput<'a> {
    pub json_flag: bool,
    pub agent_flag: bool,
    pub oz_agent_env: Option<&'a str>,
    pub ci_env: Option<&'a str>,
    pub stdout_is_tty: bool,
    pub stdin_is_tty: bool,
}

/// Resolved non-interactive and prompt-suppression mode for the current invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionMode {
    pub non_interactive: bool,
    pub json: bool,
    pub agent: bool,
    pub ci: bool,
    pub stdout_is_tty: bool,
    pub stdin_is_tty: bool,
}

impl InteractionMode {
    /// Detects the active interaction mode from CLI flags, `OZ_AGENT`, `CI`, and TTY checks.
    #[must_use]
    pub fn detect(json_flag: bool, agent_flag: bool) -> Self {
        let oz_agent_env = std::env::var("OZ_AGENT").ok();
        let ci_env = std::env::var("CI").ok();
        let stdout_is_tty = std::io::stdout().is_terminal();
        let stdin_is_tty = std::io::stdin().is_terminal();

        Self::from_input(InteractionInput {
            json_flag,
            agent_flag,
            oz_agent_env: oz_agent_env.as_deref(),
            ci_env: ci_env.as_deref(),
            stdout_is_tty,
            stdin_is_tty,
        })
    }

    /// Pure deterministic constructor suitable for unit testing without mutating env vars.
    #[must_use]
    pub fn from_input(input: InteractionInput<'_>) -> Self {
        let agent_env_on = is_truthy_env(input.oz_agent_env);
        let ci_env_on = is_truthy_env(input.ci_env);
        let agent = input.agent_flag || agent_env_on;
        let non_interactive =
            input.json_flag || agent || ci_env_on || !input.stdout_is_tty || !input.stdin_is_tty;

        Self {
            non_interactive,
            json: input.json_flag,
            agent,
            ci: ci_env_on,
            stdout_is_tty: input.stdout_is_tty,
            stdin_is_tty: input.stdin_is_tty,
        }
    }

    /// Whether interactive confirmation or input prompts are permitted.
    #[must_use]
    pub const fn allow_prompts(&self) -> bool {
        !self.non_interactive
    }

    /// Whether spawning `$EDITOR` is permitted.
    #[must_use]
    pub const fn allow_editor(&self) -> bool {
        !self.non_interactive
    }

    /// Whether paging output through `$PAGER` / `less` is permitted.
    #[must_use]
    pub const fn allow_pager(&self) -> bool {
        !self.non_interactive && self.stdout_is_tty
    }

    /// Environment overrides applied when delegating to child processes in non-interactive mode.
    #[must_use]
    pub fn child_env_overrides(&self) -> &'static [(&'static str, &'static str)] {
        if self.non_interactive {
            &[
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_PAGER", "cat"),
                ("PAGER", "cat"),
                ("GCM_INTERACTIVE", "never"),
                ("OZ_AGENT", "1"),
            ]
        } else {
            &[]
        }
    }
}

fn is_truthy_env(val: Option<&str>) -> bool {
    let Some(raw) = val else {
        return false;
    };
    let normalized = raw.trim().to_ascii_lowercase();
    matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_serializes_canonical_oz_v1_fields() {
        let env = Envelope::success(serde_json::json!({ "status": "clean" }))
            .with_op_id("op_01JTEST")
            .with_workspace(Some("agent-auth".to_string()))
            .with_branch(Some("feat/pkce".to_string()))
            .with_head_oid(Some("abcdef123456".to_string()))
            .with_warning("minor note")
            .with_recovery_command("oz undo op_01JTEST");

        let json_str = env.to_json_pretty().expect("serialize envelope");
        let parsed: Value = serde_json::from_str(&json_str).expect("parse json");

        assert_eq!(parsed["schema"], "oz.v1");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["op_id"], "op_01JTEST");
        assert_eq!(parsed["workspace"], "agent-auth");
        assert_eq!(parsed["branch"], "feat/pkce");
        assert_eq!(parsed["head_oid"], "abcdef123456");
        assert_eq!(parsed["data"]["status"], "clean");
        assert_eq!(parsed["warnings"][0], "minor note");
        assert_eq!(parsed["conflicts"], serde_json::json!([]));
        assert_eq!(parsed["recovery_commands"][0], "oz undo op_01JTEST");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn failure_envelope_and_exit_codes() {
        assert_eq!(ExitCode::Success.as_i32(), 0);
        assert_eq!(ExitCode::ConflictOrCheckFail.as_i32(), 1);
        assert_eq!(ExitCode::UsageOrGuardrail.as_i32(), 2);

        let err = crate::error::OzError::InvalidUsage {
            message: "missing commit message".to_string(),
            recovery: vec!["oz commit -m \"feat: ...\"".to_string()],
        };
        assert_eq!(err.exit_code(), ExitCode::UsageOrGuardrail);

        let env = Envelope::from_error(&err);
        assert_eq!(env.schema, "oz.v1");
        assert!(!env.ok);
        assert_eq!(env.error.as_ref().map(|e| e.exit_code), Some(2));
        assert_eq!(env.recovery_commands, vec!["oz commit -m \"feat: ...\""]);

        let conflict_err = crate::error::OzError::Conflict {
            message: "merge conflict in lib.rs".to_string(),
            conflicts: vec![ConflictEntry {
                id: "c1".to_string(),
                path: "src/lib.rs".to_string(),
                message: "overlapping edit".to_string(),
                base: None,
                ours: None,
                theirs: None,
                suggested_commands: vec!["oz resolve c1 --take ours".to_string()],
            }],
            recovery: vec!["oz resolve c1 --take ours".to_string()],
        };
        assert_eq!(conflict_err.exit_code(), ExitCode::ConflictOrCheckFail);
    }

    #[test]
    fn non_interactive_detection_covers_tty_json_agent_and_env() {
        let interactive = InteractionMode::from_input(InteractionInput {
            json_flag: false,
            agent_flag: false,
            oz_agent_env: None,
            ci_env: None,
            stdout_is_tty: true,
            stdin_is_tty: true,
        });
        assert!(!interactive.non_interactive);
        assert!(interactive.allow_prompts());
        assert!(interactive.allow_editor());
        assert!(interactive.allow_pager());

        let non_tty = InteractionMode::from_input(InteractionInput {
            json_flag: false,
            agent_flag: false,
            oz_agent_env: Some("0"),
            ci_env: None,
            stdout_is_tty: false,
            stdin_is_tty: true,
        });
        assert!(non_tty.non_interactive);
        assert!(!non_tty.allow_prompts());
        assert!(!non_tty.allow_editor());
        assert!(!non_tty.allow_pager());

        let env_agent = InteractionMode::from_input(InteractionInput {
            json_flag: false,
            agent_flag: false,
            oz_agent_env: Some("1"),
            ci_env: None,
            stdout_is_tty: true,
            stdin_is_tty: true,
        });
        assert!(env_agent.non_interactive);
        assert!(env_agent.agent);
        assert!(!env_agent.child_env_overrides().is_empty());

        let flag_json = InteractionMode::from_input(InteractionInput {
            json_flag: true,
            agent_flag: false,
            oz_agent_env: None,
            ci_env: None,
            stdout_is_tty: true,
            stdin_is_tty: true,
        });
        assert!(flag_json.non_interactive);
        assert!(flag_json.json);
    }
}
