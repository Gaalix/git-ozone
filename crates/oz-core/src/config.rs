// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration parser and layered defaults for `.ozone.toml` and `.git/ozone/config.toml`.

use crate::error::{OzError, Result};
use crate::types::BackendKind;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Default maximum disk budget (in GiB) retained for warm baseline caches.
pub const DEFAULT_MAX_BASELINE_GB: f64 = 20.0;
/// Default maximum single-blob size (in MiB) allowed by pre-commit guardrails.
pub const DEFAULT_MAX_BLOB_MB: u64 = 50;
/// Default sibling container template for spawned workspaces.
pub const DEFAULT_WORKSPACE_TEMPLATE: &str = "../{repo}.oz/{ws}";

/// Resolved repository and local Ozone configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OzoneConfig {
    /// Maximum disk budget in GiB for `.git/ozone/baselines/`.
    pub max_baseline_gb: f64,
    /// Pre-commit binary blob size limit in MiB (default `50`).
    pub max_blob_mb: u64,
    /// Workspace path template supporting `{repo}` and `{ws}` placeholders.
    pub workspace_template: String,
    /// Warm cache detection and mtime preservation settings.
    pub warm: WarmConfig,
    /// Pre-commit and workspace scope guardrail settings.
    pub guardrails: GuardrailsConfig,
    /// Optional repo-local backend override (`linux_fuse3`, `apfs_clonefile`, etc.).
    pub backend: Option<BackendKind>,
    /// Background `oz-vfsd` lifecycle settings.
    pub daemon: DaemonConfig,
    /// Optional SSH commit signing settings (`[signing]`).
    #[serde(default)]
    pub signing: SigningConfig,
    /// Configurable CLI subcommand aliases (`[aliases]`).
    #[serde(default = "default_aliases")]
    pub aliases: BTreeMap<String, String>,
}

/// Returns the built-in default CLI aliases (`st`, `ci`, `co`, `lg`, `br`).
#[must_use]
pub fn default_aliases() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("st".to_string(), "status".to_string());
    map.insert("ci".to_string(), "commit".to_string());
    map.insert("co".to_string(), "git checkout".to_string());
    map.insert("lg".to_string(), "log".to_string());
    map.insert("br".to_string(), "branch".to_string());
    map
}

impl Default for OzoneConfig {
    fn default() -> Self {
        Self {
            max_baseline_gb: DEFAULT_MAX_BASELINE_GB,
            max_blob_mb: DEFAULT_MAX_BLOB_MB,
            workspace_template: DEFAULT_WORKSPACE_TEMPLATE.to_string(),
            warm: WarmConfig::default(),
            guardrails: GuardrailsConfig::default(),
            backend: None,
            daemon: DaemonConfig::default(),
            signing: SigningConfig::default(),
            aliases: default_aliases(),
        }
    }
}

/// Warm build/dependency cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarmConfig {
    /// Automatically detect Cargo (`target/`), Node (`node_modules/`), and Python (`.venv/`).
    pub auto_detect: bool,
    /// Additional or custom relative cache directory paths to snapshot and clone.
    pub paths: Vec<String>,
    /// Relative paths excluded from warm baseline capture.
    pub exclude_paths: Vec<String>,
    /// Preserve baseline `mtime` on unchanged workspace files to keep build caches hot.
    pub preserve_mtime: bool,
}

impl Default for WarmConfig {
    fn default() -> Self {
        Self {
            auto_detect: true,
            paths: Vec::new(),
            exclude_paths: Vec::new(),
            preserve_mtime: true,
        }
    }
}

/// Pre-commit and workspace path guardrail settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailsConfig {
    /// Abort commits containing suspected secrets or unignored `.env` / private keys.
    pub check_secrets: bool,
    /// Abort commits containing unresolved conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`).
    pub check_conflict_markers: bool,
    /// Default allowed path glob patterns when spawning workspaces.
    pub scope: Vec<String>,
    /// Default denied path glob patterns across the repository.
    pub deny: Vec<String>,
}

impl Default for GuardrailsConfig {
    fn default() -> Self {
        Self {
            check_secrets: true,
            check_conflict_markers: true,
            scope: Vec::new(),
            deny: Vec::new(),
        }
    }
}

/// Daemon lifecycle settings for `oz-vfsd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Automatically spawn/re-attach `oz-vfsd` when accessing a FUSE3/ProjFS workspace.
    pub auto_start: bool,
    /// Use `allow_other` FUSE option when `/etc/fuse.conf` permits it (disabled by default).
    pub allow_other: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            auto_start: true,
            allow_other: false,
        }
    }
}

/// SSH commit signing settings (`[signing]` in `.ozone.toml` or `.git/ozone/config.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningConfig {
    /// Whether commits should be signed by default (`Some(true)` forces signing,
    /// `Some(false)` disables signing, `None` defers to Git `commit.gpgsign` when `gpg.format = ssh`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Optional signing key path (`~/.ssh/id_ed25519`, relative path) or inline `key::ssh-ed25519 ...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialOzoneConfig {
    #[serde(deserialize_with = "de_opt_f64")]
    max_baseline_gb: Option<f64>,
    max_blob_mb: Option<u64>,
    workspace_template: Option<String>,
    warm: Option<PartialWarmConfig>,
    baselines: Option<PartialBaselinesConfig>,
    workspace: Option<PartialWorkspaceSection>,
    workspaces: Option<PartialWorkspaceSection>,
    guardrails: Option<PartialGuardrailsConfig>,
    backend: Option<BackendKind>,
    daemon: Option<PartialDaemonConfig>,
    signing: Option<PartialSigningConfig>,
    aliases: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialWarmConfig {
    auto_detect: Option<bool>,
    paths: Option<Vec<String>>,
    custom_paths: Option<Vec<String>>,
    exclude_paths: Option<Vec<String>>,
    preserve_mtime: Option<bool>,
    #[serde(deserialize_with = "de_opt_f64")]
    max_baseline_gb: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialBaselinesConfig {
    #[serde(deserialize_with = "de_opt_f64")]
    max_baseline_gb: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialWorkspaceSection {
    template: Option<String>,
    container_template: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialGuardrailsConfig {
    check_secrets: Option<bool>,
    check_conflict_markers: Option<bool>,
    max_blob_mb: Option<u64>,
    scope: Option<Vec<String>>,
    deny: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialDaemonConfig {
    auto_start: Option<bool>,
    allow_other: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialSigningConfig {
    enabled: Option<bool>,
    key: Option<String>,
}

fn de_opt_f64<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrNull {
        Float(f64),
        Int(i64),
    }

    let opt = Option::<NumOrNull>::deserialize(deserializer)?;
    Ok(opt.map(|n| match n {
        NumOrNull::Float(f) => f,
        NumOrNull::Int(i) => i as f64,
    }))
}

impl OzoneConfig {
    /// Loads configuration with precedence: `defaults < .ozone.toml < .git/ozone/config.toml`.
    pub fn load_for_repo(repo_root: &Path, ozone_dir: &Path) -> Result<Self> {
        let repo_cfg = repo_root.join(".ozone.toml");
        let local_cfg = ozone_dir.join("config.toml");
        Self::load_from_paths(
            repo_cfg.is_file().then_some(repo_cfg.as_path()),
            local_cfg.is_file().then_some(local_cfg.as_path()),
        )
    }

    /// Loads and layers optional repo-committed and repo-local config files.
    pub fn load_from_paths(
        repo_config_path: Option<&Path>,
        local_config_path: Option<&Path>,
    ) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(path) = repo_config_path {
            let content = std::fs::read_to_string(path)?;
            cfg.apply_toml_str(&content, path)?;
        }
        if let Some(path) = local_config_path {
            let content = std::fs::read_to_string(path)?;
            cfg.apply_toml_str(&content, path)?;
        }
        cfg.validate(local_config_path.or(repo_config_path))?;
        Ok(cfg)
    }

    /// Parses a standalone TOML string on top of defaults (convenient for tests).
    pub fn parse_str(toml_str: &str) -> Result<Self> {
        let mut cfg = Self::default();
        let source = Path::new("<inline .ozone.toml>");
        cfg.apply_toml_str(toml_str, source)?;
        cfg.validate(Some(source))?;
        Ok(cfg)
    }

    fn apply_toml_str(&mut self, toml_str: &str, source_path: &Path) -> Result<()> {
        let layer: PartialOzoneConfig =
            toml::from_str(toml_str).map_err(|err| OzError::InvalidConfig {
                path: source_path.to_path_buf(),
                message: err.to_string(),
            })?;

        if let Some(v) = layer.max_baseline_gb {
            self.max_baseline_gb = v;
        }
        if let Some(v) = layer.max_blob_mb {
            self.max_blob_mb = v;
        }
        if let Some(v) = layer.workspace_template {
            self.workspace_template = v;
        }
        if let Some(baselines) = layer.baselines
            && let Some(v) = baselines.max_baseline_gb
        {
            self.max_baseline_gb = v;
        }
        for ws_section in [layer.workspace, layer.workspaces].into_iter().flatten() {
            if let Some(t) = ws_section.container_template.or(ws_section.template) {
                self.workspace_template = t;
            }
        }
        if let Some(warm) = layer.warm {
            if let Some(v) = warm.auto_detect {
                self.warm.auto_detect = v;
            }
            if let Some(v) = warm.paths {
                self.warm.paths = v;
            } else if let Some(v) = warm.custom_paths {
                self.warm.paths = v;
            }
            if let Some(v) = warm.exclude_paths {
                self.warm.exclude_paths = v;
            }
            if let Some(v) = warm.preserve_mtime {
                self.warm.preserve_mtime = v;
            }
            if let Some(v) = warm.max_baseline_gb {
                self.max_baseline_gb = v;
            }
        }
        if let Some(guardrails) = layer.guardrails {
            if let Some(v) = guardrails.check_secrets {
                self.guardrails.check_secrets = v;
            }
            if let Some(v) = guardrails.check_conflict_markers {
                self.guardrails.check_conflict_markers = v;
            }
            if let Some(v) = guardrails.max_blob_mb {
                self.max_blob_mb = v;
            }
            if let Some(v) = guardrails.scope {
                self.guardrails.scope = v;
            }
            if let Some(v) = guardrails.deny {
                self.guardrails.deny = v;
            }
        }
        if let Some(backend) = layer.backend {
            self.backend = Some(backend);
        }
        if let Some(daemon) = layer.daemon {
            if let Some(v) = daemon.auto_start {
                self.daemon.auto_start = v;
            }
            if let Some(v) = daemon.allow_other {
                self.daemon.allow_other = v;
            }
        }
        if let Some(signing) = layer.signing {
            if let Some(v) = signing.enabled {
                self.signing.enabled = Some(v);
            }
            if let Some(v) = signing.key {
                self.signing.key = Some(v);
            }
        }
        if let Some(aliases) = layer.aliases {
            for (name, expansion) in aliases {
                let key = name.trim().to_string();
                if expansion.trim().is_empty() {
                    self.aliases.remove(&key);
                } else {
                    self.aliases.insert(key, expansion);
                }
            }
        }
        Ok(())
    }

    fn validate(&self, source_path: Option<&Path>) -> Result<()> {
        let path = source_path.unwrap_or_else(|| Path::new(".ozone.toml"));
        if !self.max_baseline_gb.is_finite() || self.max_baseline_gb <= 0.0 {
            return Err(OzError::InvalidConfig {
                path: path.to_path_buf(),
                message: "max_baseline_gb must be a positive finite number".to_string(),
            });
        }
        if self.max_blob_mb == 0 {
            return Err(OzError::InvalidConfig {
                path: path.to_path_buf(),
                message: "max_blob_mb must be greater than 0".to_string(),
            });
        }
        if !self.workspace_template.contains("{ws}") {
            return Err(OzError::InvalidConfig {
                path: path.to_path_buf(),
                message: "workspace_template must contain the '{ws}' placeholder".to_string(),
            });
        }
        if let Some(ref key) = self.signing.key
            && key.trim().is_empty()
        {
            return Err(OzError::InvalidConfig {
                path: path.to_path_buf(),
                message: "signing.key must not be empty".to_string(),
            });
        }
        for key in self.aliases.keys() {
            if key.is_empty() || key.starts_with('-') || key.chars().any(char::is_whitespace) {
                return Err(OzError::InvalidConfig {
                    path: path.to_path_buf(),
                    message: format!(
                        "invalid alias name '{key}': must be non-empty, not start with '-', and contain no whitespace"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Expands `workspace_template` (default `../{repo}.oz/{ws}`) relative to `repo_root`.
    pub fn render_workspace_path(&self, repo_root: &Path, workspace: &str) -> Result<PathBuf> {
        validate_workspace_name(workspace)?;
        let repo_name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| OzError::InvalidUsage {
                message: format!(
                    "cannot determine repository directory name from {}",
                    repo_root.display()
                ),
                recovery: Vec::new(),
            })?;

        let rendered = self
            .workspace_template
            .replace("{repo}", repo_name)
            .replace("{ws}", workspace);
        let rel_or_abs = PathBuf::from(rendered);
        let joined = if rel_or_abs.is_absolute() {
            rel_or_abs
        } else {
            repo_root.join(rel_or_abs)
        };
        Ok(normalize_lexical(&joined))
    }

    /// Returns the grouped workspace container directory (e.g. `../<repo>.oz`).
    pub fn render_container_dir(&self, repo_root: &Path) -> Result<PathBuf> {
        let probe = self.render_workspace_path(repo_root, "__probe__")?;
        probe
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| OzError::InvalidConfig {
                path: repo_root.join(".ozone.toml"),
                message: "workspace_template has no parent container directory".to_string(),
            })
    }
}

/// Validates a workspace name against path traversal or empty segments.
pub fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('-')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid workspace name '{name}'"),
            recovery: vec!["Use a simple workspace name such as 'agent-auth'.".to_string()],
        });
    }
    Ok(())
}

/// Lexically normalizes `.` and `..` path components without hitting the filesystem.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_) | Component::ParentDir)
                ) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_match_architecture_spec() {
        let cfg = OzoneConfig::default();
        assert!((cfg.max_baseline_gb - 20.0).abs() < f64::EPSILON);
        assert_eq!(cfg.max_blob_mb, 50);
        assert_eq!(cfg.workspace_template, "../{repo}.oz/{ws}");
        assert!(cfg.warm.auto_detect);
        assert!(cfg.warm.preserve_mtime);
        assert!(cfg.guardrails.check_secrets);
        assert!(cfg.guardrails.check_conflict_markers);
    }

    #[test]
    fn layered_repo_and_local_config_merging() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("my-app");
        let ozone_dir = repo_root.join(".git/ozone");
        std::fs::create_dir_all(&ozone_dir).expect("mkdir");

        std::fs::write(
            repo_root.join(".ozone.toml"),
            r#"
max_baseline_gb = 12
max_blob_mb = 25
workspace_template = "../{repo}.oz/{ws}"

[warm]
auto_detect = true
paths = ["target", "node_modules", ".custom-cache"]

[guardrails]
deny = ["Cargo.lock", "migrations/**"]
"#,
        )
        .expect("write .ozone.toml");

        std::fs::write(
            ozone_dir.join("config.toml"),
            r#"
max_baseline_gb = 8.5
backend = "linux_fuse3"
"#,
        )
        .expect("write config.toml");

        let cfg = OzoneConfig::load_for_repo(&repo_root, &ozone_dir).expect("load config");
        assert!((cfg.max_baseline_gb - 8.5).abs() < f64::EPSILON);
        // Preserved from .ozone.toml when not overridden in config.toml:
        assert_eq!(cfg.max_blob_mb, 25);
        assert_eq!(
            cfg.warm.paths,
            vec!["target", "node_modules", ".custom-cache"]
        );
        assert_eq!(cfg.guardrails.deny, vec!["Cargo.lock", "migrations/**"]);
        assert_eq!(cfg.backend, Some(BackendKind::LinuxFuse3));
    }

    #[test]
    fn renders_grouped_sibling_workspace_path() {
        let cfg = OzoneConfig::default();
        let repo_root = Path::new("/home/dev/git-ozone");
        let ws = cfg
            .render_workspace_path(repo_root, "agent-auth")
            .expect("render workspace path");
        assert_eq!(ws, PathBuf::from("/home/dev/git-ozone.oz/agent-auth"));

        let container = cfg
            .render_container_dir(repo_root)
            .expect("render container dir");
        assert_eq!(container, PathBuf::from("/home/dev/git-ozone.oz"));
    }

    #[test]
    fn rejects_invalid_config_and_workspace_names() {
        assert!(OzoneConfig::parse_str("max_blob_mb = 0").is_err());
        assert!(OzoneConfig::parse_str("max_baseline_gb = -1").is_err());
        assert!(OzoneConfig::parse_str("workspace_template = \"../bad\"").is_err());
        assert!(validate_workspace_name("../escape").is_err());
        assert!(validate_workspace_name("a/b").is_err());
        assert!(validate_workspace_name("-flag").is_err());
        assert!(OzoneConfig::parse_str("[signing]\nkey = \"   \"").is_err());
    }

    #[test]
    fn parses_and_layers_signing_section() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("signed-app");
        let ozone_dir = repo_root.join(".git/ozone");
        std::fs::create_dir_all(&ozone_dir).expect("mkdir");

        std::fs::write(
            repo_root.join(".ozone.toml"),
            r#"
[signing]
enabled = true
key = "~/.ssh/team_ed25519"
"#,
        )
        .expect("write .ozone.toml");

        let cfg1 = OzoneConfig::load_for_repo(&repo_root, &ozone_dir).expect("load repo config");
        assert_eq!(cfg1.signing.enabled, Some(true));
        assert_eq!(cfg1.signing.key.as_deref(), Some("~/.ssh/team_ed25519"));

        std::fs::write(
            ozone_dir.join("config.toml"),
            r#"
[signing]
key = "~/.ssh/local_ed25519"
"#,
        )
        .expect("write local config");

        let cfg2 = OzoneConfig::load_for_repo(&repo_root, &ozone_dir).expect("load layered config");
        assert_eq!(cfg2.signing.enabled, Some(true));
        assert_eq!(cfg2.signing.key.as_deref(), Some("~/.ssh/local_ed25519"));
    }

    #[test]
    fn default_and_layered_custom_aliases() {
        let defaults = OzoneConfig::default();
        assert_eq!(defaults.aliases.get("st").map(String::as_str), Some("status"));
        assert_eq!(defaults.aliases.get("ci").map(String::as_str), Some("commit"));
        assert_eq!(
            defaults.aliases.get("co").map(String::as_str),
            Some("git checkout")
        );
        assert_eq!(defaults.aliases.get("lg").map(String::as_str), Some("log"));
        assert_eq!(defaults.aliases.get("br").map(String::as_str), Some("branch"));

        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("aliased-app");
        let ozone_dir = repo_root.join(".git/ozone");
        std::fs::create_dir_all(&ozone_dir).expect("mkdir");

        std::fs::write(
            repo_root.join(".ozone.toml"),
            r#"
[aliases]
lg = "log --oneline -n 10"
cm = "commit -m"
co = ""
"#,
        )
        .expect("write .ozone.toml");

        std::fs::write(
            ozone_dir.join("config.toml"),
            r#"
[aliases]
lg = "log --oneline -n 5"
"#,
        )
        .expect("write local config");

        let cfg = OzoneConfig::load_for_repo(&repo_root, &ozone_dir).expect("load aliases");
        assert_eq!(cfg.aliases.get("st").map(String::as_str), Some("status"));
        assert_eq!(
            cfg.aliases.get("lg").map(String::as_str),
            Some("log --oneline -n 5")
        );
        assert_eq!(cfg.aliases.get("cm").map(String::as_str), Some("commit -m"));
        assert!(!cfg.aliases.contains_key("co"));

        assert!(OzoneConfig::parse_str("[aliases]\n\"-bad\" = \"status\"").is_err());
        assert!(OzoneConfig::parse_str("[aliases]\n\"bad name\" = \"status\"").is_err());
    }
}
