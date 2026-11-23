// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust JSON-RPC 2.0 Model Context Protocol (`oz mcp`) stdio server supporting both
//! newline-delimited JSON-RPC lines and LSP-style `Content-Length` framing across all 11
//! Ozone agent tools (`oz_status`, `oz_diff`, `oz_commit`, `oz_ws_spawn`, `oz_radar`,
//! `oz_checkpoint`, `oz_undo`, `oz_ci_logs`, `oz_log`, `oz_restore`, `oz_show`).

use crate::AiProvenanceNote;
use crate::notes::{ProvenanceOptions, read_provenance_note, record_commit_provenance};
use oz_core::{
    BackendKind, ChangeKind, CommitOptions, CommitOutcome, DiffReport, FileChange, LogOptions,
    OzError, OzoneRepo, RestoreOptions, RestoreOutcome, Result, RevisionDiffOptions, ScopePolicy,
    ShowOptions, StatusReport, WarningEntry, WorkspaceState, compute_diff, compute_revision_diff,
    compute_status, log_commits, render_unified_diff, restore_paths, show_commit,
};
use oz_cow::{SpawnWorkspaceOptions, spawn_workspace};
use oz_integrate::{RadarOptions, RadarReport, scan_radar};
use oz_oplog::{
    ActorInfo, ActorKind, CheckpointOutcome, MutationTransaction, UndoOutcome, UndoTarget,
    checkpoint, new_op_id, undo_with_dirty,
};
use oz_warm::BaselineSource;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default MCP protocol version negotiated when client requests an unknown or older version.
pub const MCP_PROTOCOL_VERSION_2024: &str = "2024-11-05";
/// Newer MCP streamable/stdio protocol revision supported seamlessly by `oz mcp`.
pub const MCP_PROTOCOL_VERSION_2025: &str = "2025-03-26";

/// JSON-RPC 2.0 standard error code: Invalid JSON was received by the server.
pub const JSONRPC_PARSE_ERROR: i32 = -32700;
/// JSON-RPC 2.0 standard error code: The JSON sent is not a valid Request object.
pub const JSONRPC_INVALID_REQUEST: i32 = -32600;
/// JSON-RPC 2.0 standard error code: The method does not exist / is not available.
pub const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC 2.0 standard error code: Invalid method parameter(s).
pub const JSONRPC_INVALID_PARAMS: i32 = -32602;
/// JSON-RPC 2.0 standard error code: Internal JSON-RPC error.
pub const JSONRPC_INTERNAL_ERROR: i32 = -32603;

/// Wire framing auto-detected per incoming MCP message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageFraming {
    /// Newline-delimited JSON-RPC (`<json>\n`).
    JsonLine,
    /// `Content-Length: N\r\n\r\n<N bytes>` framing.
    ContentLength,
}

/// Request parameters passed to [`CiLogProvider`] when handling `oz_ci_logs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLogsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default)]
    pub failing_only: bool,
}

/// Single CI check or job log excerpt returned by `oz_ci_logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLogEntry {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
}

/// Structured CI check & failing log report returned by [`CiLogProvider`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLogsReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    pub all_passed: bool,
    pub checks: Vec<CiLogEntry>,
}

/// Injectable CI log provider trait allowing `oz-cli` to wire `oz-github` directly into
/// [`McpServer`] without introducing a circular dependency between `oz-agent` and `oz-github`.
pub trait CiLogProvider: Send + Sync {
    fn fetch_ci_logs(&self, repo: &OzoneRepo, req: &CiLogsRequest) -> Result<CiLogsReport>;
}

impl<F> CiLogProvider for F
where
    F: Fn(&OzoneRepo, &CiLogsRequest) -> Result<CiLogsReport> + Send + Sync,
{
    fn fetch_ci_logs(&self, repo: &OzoneRepo, req: &CiLogsRequest) -> Result<CiLogsReport> {
        (self)(repo, req)
    }
}

/// MCP tool output block (`{ "type": "text", "text": "..." }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolContent {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

impl McpToolContent {
    /// Creates a text content block containing serialized JSON.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".to_string(),
            text: text.into(),
        }
    }
}

/// Structured MCP `CallToolResult` payload returned by `tools/call`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallToolResult {
    pub content: Vec<McpToolContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// Built-in Ozone Model Context Protocol (`oz mcp`) server instance.
pub struct McpServer {
    root_dir: Option<PathBuf>,
    session_id: String,
    client_name: Option<String>,
    protocol_version: String,
    initialized: bool,
    ci_provider: Option<Arc<dyn CiLogProvider>>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl McpServer {
    /// Creates a new `oz mcp` server anchored at `root_dir` (or current working directory when `None`).
    #[must_use]
    pub fn new(root_dir: Option<PathBuf>) -> Self {
        Self {
            root_dir,
            session_id: format!("mcp_{}", new_op_id()),
            client_name: None,
            protocol_version: MCP_PROTOCOL_VERSION_2024.to_string(),
            initialized: false,
            ci_provider: None,
        }
    }

    /// Attaches a [`CiLogProvider`] implementation used to fulfill `oz_ci_logs` tool calls.
    #[must_use]
    pub fn with_ci_provider<P>(mut self, provider: P) -> Self
    where
        P: CiLogProvider + 'static,
    {
        self.ci_provider = Some(Arc::new(provider));
        self
    }

    /// Attaches a closure used to fulfill `oz_ci_logs` tool calls with full parameter type inference.
    #[must_use]
    pub fn with_ci_provider_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&OzoneRepo, &CiLogsRequest) -> Result<CiLogsReport> + Send + Sync + 'static,
    {
        self.ci_provider = Some(Arc::new(f));
        self
    }

    /// Overrides the default MCP session identifier recorded in `refs/notes/ai`.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Runs the MCP server synchronously over any `BufRead` + `Write` streams until EOF.
    ///
    /// Auto-detects newline-delimited JSON-RPC vs `Content-Length` framing per message and
    /// replies using the exact matching framing.
    pub fn serve_stdio<R: BufRead, W: Write>(
        &mut self,
        mut reader: R,
        mut writer: W,
    ) -> Result<()> {
        while let Some((framing, raw_payload)) = read_framed_message_sync(&mut reader)? {
            if let Some(resp_json) = self.process_raw_payload(&raw_payload)? {
                write_framed_message_sync(&mut writer, framing, &resp_json)?;
            }
        }
        Ok(())
    }

    /// Runs the MCP server synchronously over `std::io::stdin()` and `std::io::stdout()`.
    pub fn serve_process_stdio(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve_stdio(stdin.lock(), stdout.lock())
    }

    /// Runs the MCP server asynchronously over `tokio::io::stdin()` and `tokio::io::stdout()`.
    pub async fn serve_tokio_stdio(&mut self) -> Result<()> {
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        self.serve_async(stdin, stdout).await
    }

    /// Runs the MCP server asynchronously over any `AsyncBufRead` + `AsyncWrite` streams until EOF.
    pub async fn serve_async<R, W>(&mut self, mut reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        while let Some((framing, raw_payload)) = read_framed_message_async(&mut reader).await? {
            if let Some(resp_json) = self.process_raw_payload(&raw_payload)? {
                write_framed_message_async(&mut writer, framing, &resp_json).await?;
            }
        }
        Ok(())
    }

    /// Processes a single raw JSON-RPC payload string and returns the serialized JSON-RPC response
    /// (or `Ok(None)` for JSON-RPC notifications).
    pub fn process_raw_payload(&mut self, raw: &str) -> Result<Option<String>> {
        let parsed: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(err) => {
                let err_resp = jsonrpc_error_response(
                    Value::Null,
                    JSONRPC_PARSE_ERROR,
                    format!("Parse error: {err}"),
                );
                return Ok(Some(serde_json::to_string(&err_resp)?));
            }
        };

        match parsed {
            Value::Object(obj) => {
                let maybe_resp = self.handle_single_message(obj);
                match maybe_resp {
                    Some(val) => Ok(Some(serde_json::to_string(&val)?)),
                    None => Ok(None),
                }
            }
            Value::Array(batch) => {
                if batch.is_empty() {
                    let err_resp = jsonrpc_error_response(
                        Value::Null,
                        JSONRPC_INVALID_REQUEST,
                        "Invalid Request: batch array must not be empty",
                    );
                    return Ok(Some(serde_json::to_string(&err_resp)?));
                }
                let mut responses = Vec::new();
                for item in batch {
                    match item {
                        Value::Object(obj) => {
                            if let Some(r) = self.handle_single_message(obj) {
                                responses.push(r);
                            }
                        }
                        _ => responses.push(jsonrpc_error_response(
                            Value::Null,
                            JSONRPC_INVALID_REQUEST,
                            "Invalid Request: batch element must be an object",
                        )),
                    }
                }
                if responses.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(serde_json::to_string(&Value::Array(responses))?))
                }
            }
            _ => {
                let err_resp = jsonrpc_error_response(
                    Value::Null,
                    JSONRPC_INVALID_REQUEST,
                    "Invalid Request: top-level JSON value must be an object",
                );
                Ok(Some(serde_json::to_string(&err_resp)?))
            }
        }
    }

    fn handle_single_message(&mut self, mut obj: Map<String, Value>) -> Option<Value> {
        let has_id = obj.contains_key("id");
        let raw_id = obj.remove("id").unwrap_or(Value::Null);

        if has_id && !matches!(raw_id, Value::Null | Value::String(_) | Value::Number(_)) {
            return Some(jsonrpc_error_response(
                Value::Null,
                JSONRPC_INVALID_REQUEST,
                "Invalid Request: 'id' must be a string, number, or null",
            ));
        }

        match obj.get("jsonrpc").and_then(Value::as_str) {
            Some("2.0") => {}
            _ => {
                return has_id.then(|| {
                    jsonrpc_error_response(
                        raw_id,
                        JSONRPC_INVALID_REQUEST,
                        "Invalid Request: 'jsonrpc' must equal '2.0'",
                    )
                });
            }
        }

        let Some(method) = obj
            .get("method")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
        else {
            return has_id.then(|| {
                jsonrpc_error_response(
                    raw_id,
                    JSONRPC_INVALID_REQUEST,
                    "Invalid Request: 'method' must be a non-empty string",
                )
            });
        };

        let params = obj.remove("params");

        // JSON-RPC notifications (no `id` key) never emit a response on the wire.
        if !has_id {
            if method == "notifications/initialized" || method == "initialized" {
                self.initialized = true;
            }
            return None;
        }

        let dispatch_res = match method.as_str() {
            "initialize" => self.handle_initialize(params),
            "notifications/initialized" | "initialized" => {
                self.initialized = true;
                Ok(json!({}))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": mcp_tool_definitions() })),
            "tools/call" => self.handle_tools_call(params),
            _ => Err((
                JSONRPC_METHOD_NOT_FOUND,
                format!("Method not found: '{method}'"),
            )),
        };

        Some(match dispatch_res {
            Ok(result_val) => jsonrpc_success_response(raw_id, result_val),
            Err((code, msg)) => jsonrpc_error_response(raw_id, code, msg),
        })
    }

    fn handle_initialize(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (i32, String)> {
        if let Some(ref p) = params {
            let Some(map) = p.as_object() else {
                return Err((
                    JSONRPC_INVALID_PARAMS,
                    "Invalid params: 'initialize' params must be an object".to_string(),
                ));
            };
            if let Some(req_ver) = map.get("protocolVersion").and_then(Value::as_str) {
                self.protocol_version = match req_ver {
                    MCP_PROTOCOL_VERSION_2025 => MCP_PROTOCOL_VERSION_2025.to_string(),
                    MCP_PROTOCOL_VERSION_2024 => MCP_PROTOCOL_VERSION_2024.to_string(),
                    _ => MCP_PROTOCOL_VERSION_2024.to_string(),
                };
            }
            if let Some(client_name) = map
                .get("clientInfo")
                .and_then(Value::as_object)
                .and_then(|ci| ci.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                self.client_name = Some(client_name.trim().to_string());
            }
        }

        Ok(json!({
            "protocolVersion": self.protocol_version,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": "oz-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    fn handle_tools_call(
        &mut self,
        params: Option<Value>,
    ) -> std::result::Result<Value, (i32, String)> {
        let Some(Value::Object(mut map)) = params else {
            return Err((
                JSONRPC_INVALID_PARAMS,
                "Invalid params: 'tools/call' requires a params object".to_string(),
            ));
        };

        let Some(tool_name) = map.remove("name").and_then(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s),
            _ => None,
        }) else {
            return Err((
                JSONRPC_INVALID_PARAMS,
                "Invalid params: missing non-empty string 'name' in 'tools/call'".to_string(),
            ));
        };

        let args_val = match map.remove("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(Value::Object(obj)) => Value::Object(obj),
            Some(_) => {
                return Err((
                    JSONRPC_INVALID_PARAMS,
                    "Invalid params: 'arguments' must be a JSON object".to_string(),
                ));
            }
        };

        let tool_outcome: Result<Value> = match tool_name.as_str() {
            "oz_status" => {
                let args: StatusToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_status(args)
            }
            "oz_diff" => {
                let args: DiffToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_diff(args)
            }
            "oz_commit" => {
                let args: CommitToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_commit(args)
            }
            "oz_ws_spawn" => {
                let args: WsSpawnToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_ws_spawn(args)
            }
            "oz_radar" => {
                let args: RadarToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_radar(args)
            }
            "oz_checkpoint" => {
                let args: CheckpointToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_checkpoint(args)
            }
            "oz_undo" => {
                let args: UndoToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_undo(args)
            }
            "oz_ci_logs" => {
                let args: CiLogsToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_ci_logs(args)
            }
            "oz_log" => {
                let args: LogToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_log(args)
            }
            "oz_restore" => {
                let args: RestoreToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_restore(args)
            }
            "oz_show" => {
                let args: ShowToolArgs = parse_tool_args(&tool_name, args_val)?;
                self.exec_oz_show(args)
            }
            _ => {
                return Err((
                    JSONRPC_INVALID_PARAMS,
                    format!("Invalid params: unknown tool '{tool_name}'"),
                ));
            }
        };

        let call_result = match tool_outcome {
            Ok(payload) => {
                let text = serde_json::to_string_pretty(&payload)
                    .map_err(|e| (JSONRPC_INTERNAL_ERROR, e.to_string()))?;
                McpCallToolResult {
                    content: vec![McpToolContent::text(text)],
                    is_error: false,
                }
            }
            Err(oz_err) => {
                let err_payload = json!({
                    "ok": false,
                    "code": oz_err.error_code(),
                    "message": oz_err.to_string(),
                    "recovery_commands": oz_err.recovery_commands(),
                });
                let text = serde_json::to_string_pretty(&err_payload)
                    .map_err(|e| (JSONRPC_INTERNAL_ERROR, e.to_string()))?;
                McpCallToolResult {
                    content: vec![McpToolContent::text(text)],
                    is_error: true,
                }
            }
        };

        serde_json::to_value(call_result).map_err(|e| (JSONRPC_INTERNAL_ERROR, e.to_string()))
    }

    fn discover_target_repo(
        &self,
        directory: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<OzoneRepo> {
        if let Some(dir) = directory.map(str::trim).filter(|s| !s.is_empty()) {
            return OzoneRepo::discover(Some(Path::new(dir)));
        }

        let base_repo = OzoneRepo::discover(self.root_dir.as_deref())?;
        if let Some(ws_name) = workspace.map(str::trim).filter(|s| !s.is_empty()) {
            if base_repo.workspace.as_deref() == Some(ws_name) {
                return Ok(base_repo);
            }
            let state_path = base_repo.layout.workspace_state_path(ws_name);
            if !state_path.is_file() {
                return Err(OzError::WorkspaceNotFound {
                    name: ws_name.to_string(),
                });
            }
            let raw = std::fs::read_to_string(&state_path)?;
            let state: WorkspaceState = serde_json::from_str(&raw)?;
            return OzoneRepo::discover(Some(&state.worktree_path));
        }

        Ok(base_repo)
    }

    fn mcp_actor(&self, repo: &OzoneRepo) -> ActorInfo {
        ActorInfo {
            kind: ActorKind::Agent,
            client: Some(
                self.client_name
                    .clone()
                    .unwrap_or_else(|| "oz-mcp".to_string()),
            ),
            workspace: repo.workspace.clone(),
        }
    }

    fn exec_oz_status(&self, args: StatusToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let report: StatusReport = compute_status(&repo)?;
        let policy = ScopePolicy::from_repo(&repo).ok();
        let warnings: Vec<WarningEntry> = report
            .changes
            .iter()
            .filter(|c| c.out_of_scope)
            .map(|c| {
                let message = policy
                    .as_ref()
                    .and_then(|p| p.violation_reason(&c.path))
                    .unwrap_or_else(|| {
                        format!(
                            "'{}' is outside workspace allowed scope or matches deny pattern",
                            c.path
                        )
                    });
                WarningEntry::Structured {
                    code: "out_of_scope".to_string(),
                    message,
                    path: Some(c.path.clone()),
                }
            })
            .collect();

        let changes_val = if args.include_lines {
            serde_json::to_value(&report.changes)?
        } else {
            serde_json::to_value(summarize_files_compact(&report.changes))?
        };

        Ok(json!({
            "ok": true,
            "workspace": report.workspace,
            "branch": report.branch,
            "head_oid": report.head_oid,
            "changes": changes_val,
            "warnings": warnings,
        }))
    }

    fn exec_oz_diff(&self, args: DiffToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let is_range = args.range.is_some() || args.base.is_some() || args.rev.is_some();
        let report: DiffReport = if is_range {
            compute_revision_diff(
                &repo,
                &RevisionDiffOptions {
                    base: args.base.clone(),
                    rev: args.rev.clone(),
                    range: args.range.clone(),
                    targets: args.targets.clone(),
                },
            )?
        } else if let Some(first) = args.targets.first()
            && first.contains("..")
        {
            compute_revision_diff(
                &repo,
                &RevisionDiffOptions {
                    base: None,
                    rev: None,
                    range: Some(first.clone()),
                    targets: args.targets[1..].to_vec(),
                },
            )?
        } else {
            compute_diff(&repo, &args.targets)?
        };
        let unified = if args.stat_only {
            None
        } else if args.include_unified.unwrap_or(true) {
            Some(render_unified_diff(&report))
        } else {
            None
        };
        let files_val = if args.stat_only {
            serde_json::to_value(summarize_files_compact(&report.files))?
        } else {
            serde_json::to_value(&report.files)?
        };

        Ok(json!({
            "ok": true,
            "workspace": report.workspace,
            "branch": report.branch,
            "head_oid": report.head_oid,
            "base_oid": report.base_oid,
            "target_oid": report.target_oid,
            "files": files_val,
            "total_additions": report.total_additions,
            "total_deletions": report.total_deletions,
            "unified_diff": unified,
        }))
    }

    fn exec_oz_commit(&self, args: CommitToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let pre_status = compute_status(&repo)?;
        let actor = self.mcp_actor(&repo);

        let tx =
            MutationTransaction::begin(&repo, "oz mcp commit", actor, Some(&pre_status.changes))?;
        let mut commit_opts = CommitOptions::new(args.message.clone());
        commit_opts.pick = args.pick;
        commit_opts.exclude = args.exclude;
        commit_opts.force = args.force;
        commit_opts.allow_out_of_scope = args.allow_out_of_scope;
        commit_opts.allow_empty = args.allow_empty;
        let outcome: CommitOutcome = oz_core::commit_changes(&repo, &commit_opts)?;

        let entry = if outcome.remaining_dirty_count == 0 {
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&[]))?
        } else {
            let post_status = compute_status(&repo)?;
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&post_status.changes))?
        };

        let prov_opts = ProvenanceOptions {
            session_id: args.session_id.or_else(|| Some(self.session_id.clone())),
            client: args
                .client
                .or_else(|| self.client_name.clone())
                .or_else(|| Some("oz-mcp".to_string())),
            model: args.model,
            prompt_summary: args.prompt_summary,
            force_agent: true,
            inspect_env: true,
            env_overrides: None,
        };
        let provenance: Option<AiProvenanceNote> =
            record_commit_provenance(&repo, &outcome.commit_oid, &prov_opts)?;

        #[derive(Serialize)]
        struct McpCommitResponse {
            ok: bool,
            op_id: String,
            #[serde(flatten)]
            outcome: CommitOutcome,
            #[serde(skip_serializing_if = "Option::is_none")]
            provenance: Option<AiProvenanceNote>,
        }

        Ok(serde_json::to_value(McpCommitResponse {
            ok: true,
            op_id: entry.op_id,
            outcome,
            provenance,
        })?)
    }

    fn exec_oz_ws_spawn(&self, args: WsSpawnToolArgs) -> Result<Value> {
        let repo = self.discover_target_repo(args.directory.as_deref(), None)?;
        let pre_status = compute_status(&repo)?;
        let actor = self.mcp_actor(&repo);
        let cmd_str = format!("oz ws spawn {}", args.name);
        let tx = MutationTransaction::begin(&repo, cmd_str, actor, Some(&pre_status.changes))?;

        let mut opts = SpawnWorkspaceOptions::new(args.name);
        opts.branch = args.branch;
        opts.from_ref = args.from;
        opts.backend = args.backend;
        opts.scope = args.scope;
        opts.deny = args.deny;
        opts.warm = args.warm.unwrap_or(true);
        let info = spawn_workspace(&repo, &opts)?;
        let entry = tx.finish(&repo, Some(&pre_status.changes))?;

        let baseline_source = info.baseline_source.map(|src| match src {
            BaselineSource::Existing => "existing",
            BaselineSource::Captured => "captured",
            BaselineSource::FallbackPrior => "fallback_prior",
        });

        #[derive(Serialize)]
        struct McpSpawnResponse {
            ok: bool,
            op_id: String,
            workspace: String,
            branch: String,
            worktree_path: PathBuf,
            git_worktree_dir: PathBuf,
            backend: BackendKind,
            base_commit: Option<String>,
            base_tree: String,
            baseline_id: Option<String>,
            baseline_source: Option<&'static str>,
            scope: Vec<String>,
            deny: Vec<String>,
        }

        Ok(serde_json::to_value(McpSpawnResponse {
            ok: true,
            op_id: entry.op_id,
            workspace: info.state.name,
            branch: info.state.branch,
            worktree_path: info.workspace.workspace_path,
            git_worktree_dir: info.git_worktree_dir,
            backend: info.state.backend,
            base_commit: info.state.base_commit,
            base_tree: info.state.base_tree,
            baseline_id: info.state.baseline_id,
            baseline_source,
            scope: info.state.scope,
            deny: info.state.deny,
        })?)
    }

    fn exec_oz_radar(&self, args: RadarToolArgs) -> Result<Value> {
        let repo = self.discover_target_repo(args.directory.as_deref(), None)?;
        let report: RadarReport = scan_radar(
            &repo,
            &RadarOptions {
                base_branch: args.base,
                include_committed: args.include_committed.unwrap_or(true),
                workspaces: args.workspaces,
            },
        )?;

        #[derive(Serialize)]
        struct McpRadarResponse {
            ok: bool,
            #[serde(flatten)]
            report: RadarReport,
        }

        Ok(serde_json::to_value(McpRadarResponse { ok: true, report })?)
    }

    fn exec_oz_checkpoint(&self, args: CheckpointToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let status = compute_status(&repo)?;
        let actor = self.mcp_actor(&repo);
        let outcome: CheckpointOutcome = checkpoint(
            &repo,
            "oz mcp checkpoint",
            args.message.as_deref(),
            actor,
            Some(&status.changes),
        )?;

        #[derive(Serialize)]
        struct McpCheckpointResponse {
            ok: bool,
            #[serde(flatten)]
            outcome: CheckpointOutcome,
        }

        Ok(serde_json::to_value(McpCheckpointResponse {
            ok: true,
            outcome,
        })?)
    }

    fn exec_oz_undo(&self, args: UndoToolArgs) -> Result<Value> {
        if args.op_id.is_some() && args.steps.is_some() {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both 'op_id' and 'steps' to oz_undo".to_string(),
                recovery: vec!["Pass either 'op_id' or 'steps'.".to_string()],
            });
        }
        let target = match args.op_id {
            Some(id) => UndoTarget::OpId(id),
            None => UndoTarget::Steps(args.steps.unwrap_or(1)),
        };

        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let status = compute_status(&repo)?;
        let actor = self.mcp_actor(&repo);
        let outcome: UndoOutcome = undo_with_dirty(&repo, target, actor, Some(&status.changes))?;

        #[derive(Serialize)]
        struct McpUndoResponse {
            ok: bool,
            #[serde(flatten)]
            outcome: UndoOutcome,
        }

        Ok(serde_json::to_value(McpUndoResponse { ok: true, outcome })?)
    }

    fn exec_oz_ci_logs(&self, args: CiLogsToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let Some(ref provider) = self.ci_provider else {
            return Err(OzError::CheckFailed {
                message: "CI log provider is not configured for this MCP server instance"
                    .to_string(),
                recovery: vec!["oz ci --json".to_string()],
            });
        };

        let req = CiLogsRequest {
            branch: args.branch.or_else(|| repo.branch.clone()),
            commit_oid: args.commit_oid.or_else(|| repo.head_oid.clone()),
            pr_number: args.pr_number,
            run_id: args.run_id,
            failing_only: args.failing_only.unwrap_or(false),
        };
        let report = provider.fetch_ci_logs(&repo, &req)?;

        #[derive(Serialize)]
        struct McpCiResponse {
            ok: bool,
            #[serde(flatten)]
            report: CiLogsReport,
        }

        Ok(serde_json::to_value(McpCiResponse { ok: true, report })?)
    }

    fn exec_oz_log(&self, args: LogToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let opts = LogOptions {
            rev: args.rev,
            all: false,
            limit: Some(args.limit.unwrap_or(10)),
            author: args.author,
            grep: args.grep,
            search_string: args.pickaxe,
            search_diff_regex: args.grep_diff,
            since: args.since,
            graph: false,
        };
        let report = log_commits(&repo, &opts)?;
        let commits_val = if args.oneline.unwrap_or(true) {
            let compact: Vec<McpLogCommitSummary> = report
                .commits
                .into_iter()
                .map(|c| McpLogCommitSummary {
                    oid: c.oid,
                    short_oid: c.short_oid,
                    subject: c.subject,
                    author_name: c.author_name,
                    author_time: c.author_time.to_rfc3339(),
                    change_id: c.change_id,
                    refs: c.refs,
                })
                .collect();
            serde_json::to_value(compact)?
        } else {
            serde_json::to_value(report.commits)?
        };

        Ok(json!({
            "ok": true,
            "target_rev": report.target_rev,
            "unborn": report.unborn,
            "total_returned": report.total_returned,
            "commits": commits_val,
        }))
    }

    fn exec_oz_restore(&self, args: RestoreToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let pre_status = compute_status(&repo)?;
        let actor = self.mcp_actor(&repo);

        let tx = MutationTransaction::begin(
            &repo,
            "oz mcp restore",
            actor,
            Some(&pre_status.changes),
        )?;
        let outcome: RestoreOutcome = restore_paths(
            &repo,
            &RestoreOptions {
                targets: args.targets,
                source: args.source,
                remove_untracked: args.remove_untracked,
            },
        )?;
        let post_status = compute_status(&repo)?;
        let entry = tx.finish(&repo, Some(&post_status.changes))?;

        #[derive(Serialize)]
        struct McpRestoreResponse {
            ok: bool,
            op_id: String,
            #[serde(flatten)]
            outcome: RestoreOutcome,
        }

        Ok(serde_json::to_value(McpRestoreResponse {
            ok: true,
            op_id: entry.op_id,
            outcome,
        })?)
    }

    fn exec_oz_show(&self, args: ShowToolArgs) -> Result<Value> {
        let repo =
            self.discover_target_repo(args.directory.as_deref(), args.workspace.as_deref())?;
        let report = show_commit(
            &repo,
            &ShowOptions {
                rev: args.rev,
                stat_only: args.stat_only,
            },
        )?;
        let provenance = read_provenance_note(&repo, &report.commit_oid).unwrap_or(None);
        Ok(json!({
            "ok": true,
            "commit_oid": report.commit_oid,
            "short_oid": report.short_oid,
            "tree_oid": report.tree_oid,
            "parents": report.parents,
            "tag_name": report.tag_name,
            "tag_message": report.tag_message,
            "author_name": report.author_name,
            "author_email": report.author_email,
            "author_time": report.author_time,
            "committer_name": report.committer_name,
            "committer_email": report.committer_email,
            "committer_time": report.committer_time,
            "subject": report.subject,
            "body": report.body,
            "message": report.message,
            "change_id": report.change_id,
            "signed": report.signed,
            "signature_valid": report.signature_valid,
            "signature_fingerprint": report.signature_fingerprint,
            "provenance": provenance,
            "stat": report.stat,
            "files": if args.include_files { Some(report.files) } else { None },
            "unified_diff": report.unified_diff,
        }))
    }
}

fn summarize_files_compact(changes: &[FileChange]) -> Vec<McpStatusFileSummary> {
    changes
        .iter()
        .map(|c| McpStatusFileSummary {
            id: c.id.clone(),
            path: c.path.clone(),
            old_path: c.old_path.clone(),
            kind: c.kind,
            additions: c.additions,
            deletions: c.deletions,
            is_binary: c.is_binary,
            pinned: c.pinned,
            out_of_scope: c.out_of_scope,
            hunks: c
                .hunks
                .iter()
                .map(|h| McpHunkSummary {
                    id: h.id.clone(),
                    file_id: h.file_id.clone(),
                    path: h.path.clone(),
                    old_start: h.old_start,
                    old_lines: h.old_lines,
                    new_start: h.new_start,
                    new_lines: h.new_lines,
                    header: h.header.clone(),
                })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct McpHunkSummary {
    id: String,
    file_id: String,
    path: String,
    old_start: u32,
    old_lines: u32,
    new_start: u32,
    new_lines: u32,
    header: String,
}

#[derive(Debug, Serialize)]
struct McpStatusFileSummary {
    id: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    kind: ChangeKind,
    additions: u32,
    deletions: u32,
    is_binary: bool,
    pinned: bool,
    out_of_scope: bool,
    hunks: Vec<McpHunkSummary>,
}

#[derive(Debug, Serialize)]
struct McpLogCommitSummary {
    oid: String,
    short_oid: String,
    subject: String,
    author_name: String,
    author_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_id: Option<String>,
    refs: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StatusToolArgs {
    #[serde(default)]
    include_lines: bool,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LogToolArgs {
    rev: Option<String>,
    limit: Option<usize>,
    author: Option<String>,
    grep: Option<String>,
    pickaxe: Option<String>,
    grep_diff: Option<String>,
    since: Option<String>,
    oneline: Option<bool>,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RestoreToolArgs {
    targets: Vec<String>,
    source: Option<String>,
    #[serde(default)]
    remove_untracked: bool,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DiffToolArgs {
    #[serde(default)]
    targets: Vec<String>,
    base: Option<String>,
    rev: Option<String>,
    range: Option<String>,
    #[serde(default)]
    stat_only: bool,
    include_unified: Option<bool>,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ShowToolArgs {
    rev: Option<String>,
    #[serde(default)]
    stat_only: bool,
    #[serde(default)]
    include_files: bool,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitToolArgs {
    message: String,
    #[serde(default)]
    pick: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    allow_empty: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    allow_out_of_scope: bool,
    session_id: Option<String>,
    client: Option<String>,
    model: Option<String>,
    prompt_summary: Option<String>,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WsSpawnToolArgs {
    name: String,
    from: Option<String>,
    branch: Option<String>,
    backend: Option<BackendKind>,
    #[serde(default)]
    scope: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    warm: Option<bool>,
    directory: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RadarToolArgs {
    base: Option<String>,
    include_committed: Option<bool>,
    #[serde(default)]
    workspaces: Vec<String>,
    directory: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CheckpointToolArgs {
    message: Option<String>,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UndoToolArgs {
    op_id: Option<String>,
    steps: Option<usize>,
    directory: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CiLogsToolArgs {
    branch: Option<String>,
    commit_oid: Option<String>,
    pr_number: Option<u64>,
    run_id: Option<u64>,
    failing_only: Option<bool>,
    directory: Option<String>,
    workspace: Option<String>,
}

fn parse_tool_args<T: for<'de> Deserialize<'de>>(
    tool_name: &str,
    val: Value,
) -> std::result::Result<T, (i32, String)> {
    serde_json::from_value(val).map_err(|err| {
        (
            JSONRPC_INVALID_PARAMS,
            format!("Invalid params for tool '{tool_name}': {err}"),
        )
    })
}

fn jsonrpc_success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error_response(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

fn looks_like_header_line(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("content-length:") || lower.starts_with("content-type:")
}

fn parse_content_length_value(trimmed: &str) -> Option<Result<usize>> {
    let (name, rest) = trimmed.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    Some(
        rest.trim()
            .parse::<usize>()
            .map_err(|err| OzError::InvalidUsage {
                message: format!("invalid MCP Content-Length header '{}': {err}", rest.trim()),
                recovery: Vec::new(),
            }),
    )
}

fn read_framed_message_sync<R: BufRead>(
    reader: &mut R,
) -> Result<Option<(MessageFraming, String)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }

        if looks_like_header_line(trimmed) {
            let mut content_len = parse_content_length_value(trimmed).transpose()?;
            loop {
                line.clear();
                let hn = reader.read_line(&mut line)?;
                if hn == 0 {
                    return Err(OzError::InvalidUsage {
                        message: "unexpected EOF while reading MCP Content-Length headers"
                            .to_string(),
                        recovery: Vec::new(),
                    });
                }
                let h_trimmed = line.trim_end_matches(['\r', '\n']);
                if h_trimmed.is_empty() {
                    break;
                }
                if let Some(parsed_len) = parse_content_length_value(h_trimmed) {
                    content_len = Some(parsed_len?);
                }
            }

            let len = content_len.ok_or_else(|| OzError::InvalidUsage {
                message: "missing Content-Length header in header-framed MCP message".to_string(),
                recovery: Vec::new(),
            })?;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            let payload = String::from_utf8(buf).map_err(|err| OzError::InvalidUsage {
                message: format!("MCP payload is not valid UTF-8: {err}"),
                recovery: Vec::new(),
            })?;
            return Ok(Some((MessageFraming::ContentLength, payload)));
        }

        return Ok(Some((MessageFraming::JsonLine, trimmed.to_string())));
    }
}

fn write_framed_message_sync<W: Write>(
    writer: &mut W,
    framing: MessageFraming,
    json_payload: &str,
) -> Result<()> {
    match framing {
        MessageFraming::JsonLine => {
            writer.write_all(json_payload.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        MessageFraming::ContentLength => {
            let bytes = json_payload.as_bytes();
            write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
            writer.write_all(bytes)?;
        }
    }
    writer.flush()?;
    Ok(())
}

async fn read_framed_message_async<R>(reader: &mut R) -> Result<Option<(MessageFraming, String)>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }

        if looks_like_header_line(trimmed) {
            let mut content_len = parse_content_length_value(trimmed).transpose()?;
            loop {
                line.clear();
                let hn = reader.read_line(&mut line).await?;
                if hn == 0 {
                    return Err(OzError::InvalidUsage {
                        message: "unexpected EOF while reading MCP Content-Length headers"
                            .to_string(),
                        recovery: Vec::new(),
                    });
                }
                let h_trimmed = line.trim_end_matches(['\r', '\n']);
                if h_trimmed.is_empty() {
                    break;
                }
                if let Some(parsed_len) = parse_content_length_value(h_trimmed) {
                    content_len = Some(parsed_len?);
                }
            }

            let len = content_len.ok_or_else(|| OzError::InvalidUsage {
                message: "missing Content-Length header in header-framed MCP message".to_string(),
                recovery: Vec::new(),
            })?;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            let payload = String::from_utf8(buf).map_err(|err| OzError::InvalidUsage {
                message: format!("MCP payload is not valid UTF-8: {err}"),
                recovery: Vec::new(),
            })?;
            return Ok(Some((MessageFraming::ContentLength, payload)));
        }

        return Ok(Some((MessageFraming::JsonLine, trimmed.to_string())));
    }
}

async fn write_framed_message_async<W>(
    writer: &mut W,
    framing: MessageFraming,
    json_payload: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match framing {
        MessageFraming::JsonLine => {
            writer.write_all(json_payload.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        MessageFraming::ContentLength => {
            let bytes = json_payload.as_bytes();
            let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(bytes).await?;
        }
    }
    writer.flush().await?;
    Ok(())
}

/// Returns the canonical MCP `tools/list` schema descriptors for all 11 Ozone MCP tools.
#[must_use]
pub fn mcp_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "oz_status",
            "description": "Show indexless O(dirty_files) working-tree status with deterministic file IDs (f1, f2) and hunk IDs (h1a, h1b). Omits full diff lines by default (use oz_diff for line-level diffs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_lines": { "type": "boolean", "description": "Include full hunk diff lines (default: false)." },
                    "workspace": { "type": "string", "description": "Optional spawned Ozone workspace name." },
                    "directory": { "type": "string", "description": "Optional explicit directory path." }
                }
            }
        }),
        json!({
            "name": "oz_diff",
            "description": "Compute Histogram diffs and addressable hunk IDs (h1a, h1b) for dirty files or revision ranges (A..B, A...B, or base/rev).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "targets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional revision range (A..B) as first item, or file IDs (f1), hunk IDs (h1a), or relative file paths."
                    },
                    "base": { "type": "string", "description": "Optional base commit/branch revision (defaults to comparing against HEAD unless rev is set)." },
                    "rev": { "type": "string", "description": "Optional target commit/branch revision (defaults to HEAD when base is set)." },
                    "range": { "type": "string", "description": "Optional Git revision range expression (`A..B` or `A...B`)." },
                    "stat_only": { "type": "boolean", "description": "Omit hunk diff lines and unified diff text, returning compact file/hunk summaries (default: false)." },
                    "include_unified": { "type": "boolean", "description": "Include rendered unified diff text (default: true)." },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_commit",
            "description": "Create a stage-free commit (whole tree or surgical --pick / --exclude hunks), recording MVCC OpLog state and refs/notes/ai provenance.",
            "inputSchema": {
                "type": "object",
                "required": ["message"],
                "properties": {
                    "message": { "type": "string", "description": "Commit message (Change-Id trailer added automatically)." },
                    "pick": { "type": "array", "items": { "type": "string" }, "description": "File IDs (f1), hunk IDs (h1a), or paths to include." },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "File IDs (f2), hunk IDs (h1b), or paths to exclude." },
                    "allow_empty": { "type": "boolean" },
                    "force": { "type": "boolean" },
                    "allow_out_of_scope": { "type": "boolean" },
                    "session_id": { "type": "string" },
                    "client": { "type": "string" },
                    "model": { "type": "string" },
                    "prompt_summary": { "type": "string" },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_ws_spawn",
            "description": "Spawn an isolated Copy-on-Write workspace on a real branch with warm caches and optional --scope / --deny guardrails.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "Workspace name." },
                    "from": { "type": "string", "description": "Base branch or commit revision (defaults to HEAD)." },
                    "branch": { "type": "string", "description": "Branch name to create/check out (defaults to workspace name)." },
                    "backend": { "type": "string", "description": "Optional explicit CoW backend override." },
                    "scope": { "type": "array", "items": { "type": "string" }, "description": "Allowed path globs." },
                    "deny": { "type": "array", "items": { "type": "string" }, "description": "Denied path globs." },
                    "warm": { "type": "boolean", "description": "Enable warm cache baseline & mtime preservation (default: true)." },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_radar",
            "description": "Scan active workspaces for overlapping hunk edits, same-file edits, and out-of-scope changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Base branch to project edits onto (defaults to main)." },
                    "include_committed": { "type": "boolean" },
                    "workspaces": { "type": "array", "items": { "type": "string" } },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_checkpoint",
            "description": "Capture a < 10 ms MVCC working-tree checkpoint in refs/ozone/checkpoints/<ws> and the OpLog.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_undo",
            "description": "Rewind recent OpLog mutations or checkpoints in < 10 ms via atomic CAS ref updates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op_id": { "type": "string", "description": "Specific operation ID (op_...) to undo." },
                    "steps": { "type": "integer", "minimum": 1, "description": "Number of sequential steps to undo (default: 1)." },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_ci_logs",
            "description": "Fetch CI check status and compact failing step logs for agent self-healing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch": { "type": "string" },
                    "commit_oid": { "type": "string" },
                    "pr_number": { "type": "integer" },
                    "run_id": { "type": "integer" },
                    "failing_only": { "type": "boolean" },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_log",
            "description": "Inspect compact commit history (defaults to 10 oneline entries without diff bodies).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rev": { "type": "string", "description": "Optional starting commit/branch/tag revision (defaults to HEAD)." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of commits to return (default: 10)." },
                    "author": { "type": "string", "description": "Filter by author name or email substring." },
                    "grep": { "type": "string", "description": "Filter commit messages by regex." },
                    "pickaxe": { "type": "string", "description": "Filter commits changing occurrences of a literal string (-S)." },
                    "grep_diff": { "type": "string", "description": "Filter commits whose diff lines match a regex (-G)." },
                    "since": { "type": "string", "description": "Show commits more recent than date/duration (e.g. '2 days ago', '2026-11-01')." },
                    "oneline": { "type": "boolean", "description": "Emit compact commit summaries (default: true; false for full commit bodies)." },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_restore",
            "description": "Selectively restore working-tree files or file IDs (f1) from HEAD or --source while preserving other dirty edits (recorded in OpLog for oz_undo).",
            "inputSchema": {
                "type": "object",
                "required": ["targets"],
                "properties": {
                    "targets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File paths, directory prefixes, or file IDs (f1) to restore."
                    },
                    "source": { "type": "string", "description": "Optional source revision (defaults to HEAD)." },
                    "remove_untracked": { "type": "boolean", "description": "Delete specified untracked paths not present in source (default: false)." },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "oz_show",
            "description": "Inspect a commit or tag: metadata, Change-Id, SSH signature verification, refs/notes/ai provenance, diffstat, and unified diff.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rev": { "type": "string", "description": "Revision, commit OID, branch, or tag to inspect (defaults to HEAD)." },
                    "stat_only": { "type": "boolean", "description": "Omit unified diff text and return metadata + diffstat only (default: false)." },
                    "include_files": { "type": "boolean", "description": "Include structured per-file hunk array alongside unified_diff (default: false)." },
                    "workspace": { "type": "string" },
                    "directory": { "type": "string" }
                }
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::read_provenance_note;
    use oz_core::{CommitOptions, commit_changes};
    use std::fs;
    use std::io::Cursor;
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_mcp_repo() -> (TempDir, PathBuf) {
        let parent = TempDir::new().expect("tempdir");
        let repo_dir = parent.path().join("demo-repo");
        fs::create_dir_all(&repo_dir).expect("mkdir repo");

        let s = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo_dir)
            .status()
            .expect("git init");
        assert!(s.success());
        for (k, v) in [
            ("user.name", "MCP Tester"),
            ("user.email", "mcp@example.com"),
        ] {
            let c = Command::new("git")
                .args(["config", k, v])
                .current_dir(&repo_dir)
                .status()
                .expect("git config");
            assert!(c.success());
        }

        fs::create_dir_all(repo_dir.join(".git/ozone")).expect("mkdir .git/ozone");
        fs::write(
            repo_dir.join(".git/ozone/config.toml"),
            "backend = \"cas_hardlink\"\n",
        )
        .expect("write local ozone config");

        fs::write(
            repo_dir.join("calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\n\n\n\n\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("write calc.rs");
        let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
        commit_changes(&repo, &CommitOptions::new("chore: initial commit")).expect("init commit");

        (parent, repo_dir)
    }

    fn encode_msg(framing: MessageFraming, v: &Value) -> Vec<u8> {
        let s = serde_json::to_string(v).expect("serialize req");
        match framing {
            MessageFraming::JsonLine => format!("{s}\n").into_bytes(),
            MessageFraming::ContentLength => {
                format!("Content-Length: {}\r\n\r\n{s}", s.len()).into_bytes()
            }
        }
    }

    fn decode_all_responses(framing: MessageFraming, raw_out: &[u8]) -> Vec<Value> {
        let mut cursor = Cursor::new(raw_out);
        let mut out = Vec::new();
        while let Some((det_framing, payload)) =
            read_framed_message_sync(&mut cursor).expect("read framed response")
        {
            assert_eq!(det_framing, framing);
            let val: Value = serde_json::from_str(&payload).expect("parse response json");
            out.push(val);
        }
        out
    }

    fn extract_tool_json(resp: &Value) -> (bool, Value) {
        let is_err = resp["result"]["isError"].as_bool().expect("isError bool");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("content[0].text");
        let parsed: Value = serde_json::from_str(text).expect("tool json text");
        (is_err, parsed)
    }

    fn run_full_lifecycle_test(framing: MessageFraming) {
        let (_parent, repo_dir) = setup_mcp_repo();

        // Create two independent hunks in `calc.rs` + a second file `extra.rs` so we can test `--pick`!
        fs::write(
            repo_dir.join("calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.saturating_add(b)\n}\n\n\n\n\n\n\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a.saturating_sub(b)\n}\n",
        )
        .expect("modify calc.rs");
        fs::write(repo_dir.join("extra.rs"), "pub const VERSION: u32 = 1;\n")
            .expect("write extra.rs");

        let mut server = McpServer::new(Some(repo_dir.clone()))
            .with_session_id("mcp-test-session")
            .with_ci_provider_fn(|_repo, req| {
                Ok(CiLogsReport {
                    branch: req.branch.clone(),
                    commit_oid: req.commit_oid.clone(),
                    all_passed: false,
                    checks: vec![CiLogEntry {
                        name: "cargo-test".to_string(),
                        status: "completed".to_string(),
                        conclusion: Some("failure".to_string()),
                        failing_step: Some("Run tests".to_string()),
                        log_excerpt: Some(
                            "assertion `left == right` failed at src/lib.rs:42".to_string(),
                        ),
                    }],
                })
            });

        let requests = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "clientInfo": { "name": "claude-code", "version": "1.0.0" }
                }
            }),
            // Notification (no `id`) -> must NOT produce a response!
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "ping"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/list"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": "oz_status", "arguments": {} }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": "oz_diff", "arguments": { "targets": ["f1"] } }
            }),
            // Commit ONLY `h1a` via `pick`, leaving `h1b` and `extra.rs` uncommitted!
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {
                    "name": "oz_commit",
                    "arguments": {
                        "message": "feat(calc): use saturating_add",
                        "pick": ["h1a"],
                        "model": "claude-sonnet-4-5",
                        "prompt_summary": "harden integer addition"
                    }
                }
            }),
            // Spawn a CoW workspace (`oz_ws_spawn`)
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "oz_ws_spawn",
                    "arguments": {
                        "name": "agent-sub",
                        "from": "main",
                        "scope": ["calc.rs", "extra.rs"],
                        "deny": ["secrets/**"],
                        "warm": false
                    }
                }
            }),
            // Radar check (`oz_radar`)
            json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": { "name": "oz_radar", "arguments": {} }
            }),
            // Checkpoint (`oz_checkpoint`)
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": {
                    "name": "oz_checkpoint",
                    "arguments": { "message": "before experiment" }
                }
            }),
            // Undo (`oz_undo`)
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {
                    "name": "oz_undo",
                    "arguments": { "steps": 1 }
                }
            }),
            // CI logs (`oz_ci_logs`)
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": {
                    "name": "oz_ci_logs",
                    "arguments": { "failing_only": true }
                }
            }),
        ];

        let mut input_bytes = Vec::new();
        for req in &requests {
            input_bytes.extend_from_slice(&encode_msg(framing, req));
        }

        let mut output_bytes = Vec::new();
        server
            .serve_stdio(Cursor::new(input_bytes), &mut output_bytes)
            .expect("serve_stdio");

        let responses = decode_all_responses(framing, &output_bytes);
        // 12 messages sent, 1 was `notifications/initialized` -> 11 responses!
        assert_eq!(responses.len(), 11);

        // 1: initialize
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], "oz-mcp");

        // 2: ping
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[1]["result"], json!({}));

        // 3: tools/list -> all 11 tools
        let tools = responses[2]["result"]["tools"]
            .as_array()
            .expect("tools array");
        assert_eq!(tools.len(), 11);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().expect("tool name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "oz_status",
                "oz_diff",
                "oz_commit",
                "oz_ws_spawn",
                "oz_radar",
                "oz_checkpoint",
                "oz_undo",
                "oz_ci_logs",
                "oz_log",
                "oz_restore",
                "oz_show",
            ]
        );

        // 4: oz_status (compact by default: omits hunk lines)
        let (is_err, status_json) = extract_tool_json(&responses[3]);
        assert!(!is_err);
        assert_eq!(status_json["changes"].as_array().unwrap().len(), 2);
        assert!(
            status_json["changes"][0]["hunks"][0].get("lines").is_none(),
            "oz_status must omit hunks[].lines by default"
        );

        // 5: oz_diff
        let (is_err, diff_json) = extract_tool_json(&responses[4]);
        assert!(!is_err);
        assert_eq!(diff_json["files"].as_array().unwrap().len(), 1);
        assert!(
            diff_json["unified_diff"]
                .as_str()
                .unwrap()
                .contains("saturating_add")
        );

        // 6: oz_commit with pick=["h1a"] -> records OpLog + refs/notes/ai!
        let (is_err, commit_json) = extract_tool_json(&responses[5]);
        assert!(!is_err);
        let commit_oid = commit_json["commit_oid"].as_str().expect("commit_oid");
        assert!(commit_json["op_id"].as_str().unwrap().starts_with("op_"));
        assert_eq!(commit_json["remaining_dirty_count"], 2);
        assert_eq!(commit_json["provenance"]["client"], "claude-code");
        assert_eq!(commit_json["provenance"]["session_id"], "mcp-test-session");

        let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
        let stored_note = read_provenance_note(&repo, commit_oid)
            .expect("read note")
            .expect("note exists in refs/notes/ai");
        assert_eq!(stored_note.client, "claude-code");
        assert_eq!(stored_note.line_ranges.len(), 1);
        assert_eq!(stored_note.line_ranges[0].path, "calc.rs");
        assert_eq!(stored_note.line_ranges[0].start_line, 2);
        assert_eq!(stored_note.line_ranges[0].end_line, 2);

        // 7: oz_ws_spawn
        let (is_err, spawn_json) = extract_tool_json(&responses[6]);
        assert!(!is_err, "spawn error: {spawn_json}");
        assert_eq!(spawn_json["workspace"], "agent-sub");

        // 8: oz_radar
        let (is_err, radar_json) = extract_tool_json(&responses[7]);
        assert!(!is_err);
        assert_eq!(radar_json["scanned_workspaces"], json!(["agent-sub"]));

        // 9: oz_checkpoint
        let (is_err, cp_json) = extract_tool_json(&responses[8]);
        assert!(!is_err);
        assert!(cp_json["op_id"].as_str().unwrap().starts_with("op_"));

        // 10: oz_undo
        let (is_err, undo_json) = extract_tool_json(&responses[9]);
        assert!(!is_err, "undo error: {undo_json}");
        assert!(undo_json["op_id"].as_str().unwrap().starts_with("op_"));
        assert_eq!(undo_json["undone_op_ids"][0], cp_json["op_id"]);

        // 11: oz_ci_logs
        let (is_err, ci_json) = extract_tool_json(&responses[10]);
        assert!(!is_err);
        assert_eq!(ci_json["all_passed"], false);
        assert_eq!(ci_json["checks"][0]["name"], "cargo-test");
    }

    #[test]
    fn mcp_full_lifecycle_over_json_lines() {
        run_full_lifecycle_test(MessageFraming::JsonLine);
    }

    #[test]
    fn mcp_full_lifecycle_over_content_length_headers() {
        run_full_lifecycle_test(MessageFraming::ContentLength);
    }

    #[test]
    fn mcp_returns_standard_jsonrpc_error_codes() {
        let mut server = McpServer::new(None);

        // -32700 Parse error
        let r1: Value = serde_json::from_str(
            &server
                .process_raw_payload("{not valid json")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(r1["error"]["code"], JSONRPC_PARSE_ERROR);

        // -32600 Invalid Request
        let r2: Value = serde_json::from_str(
            &server
                .process_raw_payload(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(r2["error"]["code"], JSONRPC_INVALID_REQUEST);

        // -32601 Method not found
        let r3: Value = serde_json::from_str(
            &server
                .process_raw_payload(r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent/method"}"#)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(r3["error"]["code"], JSONRPC_METHOD_NOT_FOUND);

        // -32602 Invalid params (unknown tool & missing required parameter)
        let r4: Value = serde_json::from_str(
            &server
                .process_raw_payload(
                    r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
                )
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(r4["error"]["code"], JSONRPC_INVALID_PARAMS);

        let r5: Value = serde_json::from_str(
            &server
                .process_raw_payload(
                    r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"oz_commit","arguments":{}}}"#,
                )
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(r5["error"]["code"], JSONRPC_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn mcp_async_transport_supports_mixed_framing() {
        let (_parent, repo_dir) = setup_mcp_repo();
        let mut server = McpServer::new(Some(repo_dir));

        let mut wire_in = Vec::new();
        wire_in.extend_from_slice(&encode_msg(
            MessageFraming::JsonLine,
            &json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
        ));
        wire_in.extend_from_slice(&encode_msg(
            MessageFraming::ContentLength,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oz_status","arguments":{}}}),
        ));

        let reader = tokio::io::BufReader::new(std::io::Cursor::new(wire_in));
        let mut wire_out: Vec<u8> = Vec::new();
        server
            .serve_async(reader, &mut wire_out)
            .await
            .expect("serve_async");

        let mut cursor = Cursor::new(wire_out);
        let (f1, p1) = read_framed_message_sync(&mut cursor)
            .unwrap()
            .expect("first msg");
        assert_eq!(f1, MessageFraming::JsonLine);
        let v1: Value = serde_json::from_str(&p1).unwrap();
        assert_eq!(v1["id"], 1);

        let (f2, p2) = read_framed_message_sync(&mut cursor)
            .unwrap()
            .expect("second msg");
        assert_eq!(f2, MessageFraming::ContentLength);
        let v2: Value = serde_json::from_str(&p2).unwrap();
        assert_eq!(v2["id"], 2);
        assert_eq!(v2["result"]["isError"], false);
    }

    #[test]
    fn mcp_status_omits_lines_by_default_and_includes_on_request() {
        let (_parent, repo_dir) = setup_mcp_repo();
        let big_body: String = (0..400).map(|i| format!("line_{i} = true\n")).collect();
        fs::write(repo_dir.join("large.txt"), &big_body).expect("write large");

        let mut server = McpServer::new(Some(repo_dir));
        let reqs = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"oz_status","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oz_status","arguments":{"include_lines":true}}}),
        ];
        let mut input = Vec::new();
        for req in &reqs {
            input.extend_from_slice(&encode_msg(MessageFraming::JsonLine, req));
        }
        let mut out = Vec::new();
        server
            .serve_stdio(Cursor::new(input), &mut out)
            .expect("serve_stdio");
        let resps = decode_all_responses(MessageFraming::JsonLine, &out);

        let (_, compact) = extract_tool_json(&resps[0]);
        let large_compact = compact["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "large.txt")
            .expect("large.txt entry");
        assert!(large_compact["hunks"][0].get("lines").is_none());
        assert_eq!(large_compact["additions"], 400);

        let (_, verbose) = extract_tool_json(&resps[1]);
        let large_verbose = verbose["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "large.txt")
            .expect("large.txt entry");
        assert_eq!(
            large_verbose["hunks"][0]["lines"]
                .as_array()
                .unwrap()
                .len(),
            400
        );
    }

    #[test]
    fn mcp_log_and_restore_with_undo_roundtrip() {
        let (_parent, repo_dir) = setup_mcp_repo();
        let original_calc = fs::read_to_string(repo_dir.join("calc.rs")).expect("read orig calc");
        fs::write(repo_dir.join("calc.rs"), "pub fn add(a: i32, b: i32) -> i32 { 999 }\n")
            .expect("modify calc");
        fs::write(repo_dir.join("other.txt"), "keep dirty\n").expect("write other");

        let mut server = McpServer::new(Some(repo_dir.clone()));
        let reqs_part1 = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"oz_log","arguments":{"limit":5}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oz_log","arguments":{"limit":1,"oneline":false}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"oz_restore","arguments":{"targets":["calc.rs"]}}}),
        ];
        let mut input1 = Vec::new();
        for req in &reqs_part1 {
            input1.extend_from_slice(&encode_msg(MessageFraming::JsonLine, req));
        }
        let mut out1 = Vec::new();
        server
            .serve_stdio(Cursor::new(input1), &mut out1)
            .expect("serve_stdio part1");
        let resps1 = decode_all_responses(MessageFraming::JsonLine, &out1);

        let (err1, log_compact) = extract_tool_json(&resps1[0]);
        assert!(!err1);
        assert_eq!(log_compact["total_returned"], 1);
        assert!(log_compact["commits"][0].get("message").is_none());
        assert_eq!(log_compact["commits"][0]["subject"], "chore: initial commit");

        let (err2, log_full) = extract_tool_json(&resps1[1]);
        assert!(!err2);
        assert!(log_full["commits"][0].get("message").is_some());

        let (err3, restore_json) = extract_tool_json(&resps1[2]);
        assert!(!err3, "restore err: {restore_json}");
        assert_eq!(restore_json["restored_paths"], json!(["calc.rs"]));
        assert_eq!(
            fs::read_to_string(repo_dir.join("calc.rs")).unwrap(),
            original_calc
        );
        assert_eq!(
            fs::read_to_string(repo_dir.join("other.txt")).unwrap(),
            "keep dirty\n"
        );

        let req4 = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"oz_undo","arguments":{"steps":1}}});
        let input2 = encode_msg(MessageFraming::JsonLine, &req4);
        let mut out2 = Vec::new();
        server
            .serve_stdio(Cursor::new(input2), &mut out2)
            .expect("serve_stdio part2");
        let resps2 = decode_all_responses(MessageFraming::JsonLine, &out2);

        let (err4, undo_json) = extract_tool_json(&resps2[0]);
        assert!(!err4, "undo err: {undo_json}");
        assert_eq!(undo_json["undone_op_ids"][0], restore_json["op_id"]);
        assert_eq!(
            fs::read_to_string(repo_dir.join("calc.rs")).unwrap(),
            "pub fn add(a: i32, b: i32) -> i32 { 999 }\n"
        );
        assert_eq!(
            fs::read_to_string(repo_dir.join("other.txt")).unwrap(),
            "keep dirty\n"
        );
    }

    #[test]
    fn mcp_show_and_revision_range_diff_roundtrip() {
        let (_parent, repo_dir) = setup_mcp_repo();
        let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");

        fs::write(repo_dir.join("calc.rs"), "pub fn add(a: i32, b: i32) -> i32 { a + b + 1 }\n")
            .unwrap();
        fs::write(repo_dir.join("extra.rs"), "pub const X: u32 = 42;\n").unwrap();
        let commit_out = commit_changes(&repo, &CommitOptions::new("feat: second commit"))
            .expect("commit second");

        let mut server = McpServer::new(Some(repo_dir));
        let reqs = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"oz_show","arguments":{"rev":"HEAD"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oz_show","arguments":{"rev":&commit_out.commit_oid,"stat_only":true,"include_files":true}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"oz_diff","arguments":{"range":"HEAD~1..HEAD","targets":["calc.rs"]}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"oz_diff","arguments":{"base":"HEAD~1","stat_only":true}}}),
        ];
        let mut input = Vec::new();
        for req in &reqs {
            input.extend_from_slice(&encode_msg(MessageFraming::JsonLine, req));
        }
        let mut out = Vec::new();
        server
            .serve_stdio(Cursor::new(input), &mut out)
            .expect("serve_stdio");
        let resps = decode_all_responses(MessageFraming::JsonLine, &out);

        let (err1, show_default) = extract_tool_json(&resps[0]);
        assert!(!err1, "show default err: {show_default}");
        assert_eq!(show_default["commit_oid"], commit_out.commit_oid);
        assert_eq!(show_default["subject"], "feat: second commit");
        assert!(show_default["unified_diff"].as_str().unwrap().contains("a + b + 1"));
        assert!(show_default["files"].is_null());

        let (err2, show_stat) = extract_tool_json(&resps[1]);
        assert!(!err2, "show stat err: {show_stat}");
        assert!(show_stat["unified_diff"].is_null());
        assert_eq!(show_stat["stat"]["files_changed"], 2);
        assert_eq!(show_stat["files"].as_array().unwrap().len(), 2);

        let (err3, diff_range) = extract_tool_json(&resps[2]);
        assert!(!err3, "diff range err: {diff_range}");
        assert!(diff_range["base_oid"].is_string());
        assert_eq!(diff_range["target_oid"], commit_out.commit_oid);
        assert_eq!(diff_range["files"].as_array().unwrap().len(), 1);
        assert_eq!(diff_range["files"][0]["path"], "calc.rs");

        let (err4, diff_stat) = extract_tool_json(&resps[3]);
        assert!(!err4, "diff stat err: {diff_stat}");
        assert_eq!(diff_stat["files"].as_array().unwrap().len(), 2);
        assert!(diff_stat["unified_diff"].is_null());
        for f in diff_stat["files"].as_array().unwrap() {
            for h in f["hunks"].as_array().unwrap() {
                assert!(h.get("lines").is_none());
            }
        }
    }
}
