// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust OpenSSH client configuration parser (`~/.ssh/config` and `/etc/ssh/ssh_config`)
//! and transport option builder for `oz clone` / `oz fetch` / `oz push` over SSH.

use std::fs;
use std::path::{Path, PathBuf};

use gix_transport::client::blocking_io::ssh::{self, ProgramKind};

const MAX_SSH_INCLUDE_DEPTH: usize = 8;

/// Resolved OpenSSH configuration options for a target SSH host alias.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedSshHost {
    /// Original target host string (e.g., `work-gh` or `github.com`).
    pub target_host: String,
    /// Effective `HostName` (defaults to `target_host` if unspecified in SSH config).
    pub hostname: Option<String>,
    /// Effective `User` from SSH config.
    pub user: Option<String>,
    /// Effective `Port` from SSH config.
    pub port: Option<u16>,
    /// Expanded `IdentityFile` paths accumulated in match order.
    pub identity_files: Vec<PathBuf>,
    /// `IdentitiesOnly` setting.
    pub identities_only: Option<bool>,
    /// `ProxyJump` hop(s) (`user@host[:port],...` or `"none"`).
    pub proxy_jump: Option<String>,
    /// `ProxyCommand` string.
    pub proxy_command: Option<String>,
    /// `IdentityAgent` path or `"SSH_AUTH_SOCK"` / `"none"`.
    pub identity_agent: Option<String>,
    /// `ForwardAgent` setting.
    pub forward_agent: Option<bool>,
    /// `PKCS11Provider` shared library path (hardware smartcards / YubiKey PIV).
    pub pkcs11_provider: Option<String>,
    /// `SecurityKeyProvider` library/internal path (FIDO2 `sk-` hardware keys).
    pub security_key_provider: Option<String>,
    /// User SSH config file path loaded (`$OZ_SSH_CONFIG` or `~/.ssh/config`), if any.
    pub user_config_file: Option<PathBuf>,
    /// Effective agent socket path resolved from `IdentityAgent` and `SSH_AUTH_SOCK`.
    pub effective_agent_sock: Option<PathBuf>,
}

impl ResolvedSshHost {
    /// Returns the effective target hostname (`HostName` if configured, else `target_host`).
    #[must_use]
    pub fn effective_hostname(&self) -> &str {
        self.hostname.as_deref().unwrap_or(&self.target_host)
    }
}

/// Optional paths/env overrides for deterministic unit testing of SSH config resolution.
#[derive(Debug, Clone, Default)]
pub struct SshConfigSearchContext {
    pub home_dir: Option<PathBuf>,
    pub user_config: Option<PathBuf>,
    pub system_config: Option<PathBuf>,
    pub ssh_auth_sock: Option<String>,
    pub skip_system_config: bool,
}

impl SshConfigSearchContext {
    fn from_env() -> Self {
        let home_dir = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            });
        let explicit_cfg = std::env::var_os("OZ_SSH_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let user_config = explicit_cfg.or_else(|| home_dir.as_ref().map(|h| h.join(".ssh/config")));
        let ssh_auth_sock = std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|v| !v.is_empty());

        Self {
            home_dir,
            user_config,
            system_config: Some(PathBuf::from("/etc/ssh/ssh_config")),
            ssh_auth_sock,
            skip_system_config: false,
        }
    }
}

/// Resolves OpenSSH host directives from `~/.ssh/config` and `/etc/ssh/ssh_config`
/// using OpenSSH first-match-wins precedence.
#[must_use]
pub fn resolve_ssh_host(target_host: &str) -> ResolvedSshHost {
    let ctx = SshConfigSearchContext::from_env();
    resolve_ssh_host_with_context(target_host, &ctx)
}

/// Deterministic variant of [`resolve_ssh_host`] accepting explicit config paths/environment.
#[must_use]
pub fn resolve_ssh_host_with_context(
    target_host: &str,
    ctx: &SshConfigSearchContext,
) -> ResolvedSshHost {
    let mut resolved = ResolvedSshHost {
        target_host: target_host.to_string(),
        ..Default::default()
    };

    if let Some(ref user_cfg) = ctx.user_config
        && user_cfg.is_file()
    {
        resolved.user_config_file = Some(user_cfg.clone());
        let ssh_dir = user_cfg.parent().unwrap_or_else(|| Path::new("/"));
        let _ = parse_ssh_config_file(
            user_cfg,
            ssh_dir,
            target_host,
            ctx.home_dir.as_deref(),
            &mut resolved,
            0,
        );
    }

    if !ctx.skip_system_config
        && let Some(ref sys_cfg) = ctx.system_config
        && sys_cfg.is_file()
    {
        let sys_dir = sys_cfg.parent().unwrap_or_else(|| Path::new("/etc/ssh"));
        let _ = parse_ssh_config_file(
            sys_cfg,
            sys_dir,
            target_host,
            ctx.home_dir.as_deref(),
            &mut resolved,
            0,
        );
    }

    // Expand SSH % tokens (%h, %p, %r, %%) and ~/ after primary HostName/User/Port settle.
    finalize_ssh_tokens(
        &mut resolved,
        ctx.home_dir.as_deref(),
        ctx.ssh_auth_sock.as_deref(),
    );
    resolved
}

/// Builds `gix_transport` SSH connection options honoring:
/// 1. `GIT_SSH_COMMAND` env var
/// 2. `core.sshCommand` Git config
/// 3. `GIT_SSH` env var
/// 4. OpenSSH (`ssh`) with resolved `~/.ssh/config` options (`-F`, `-o HostName=`,
///    `-o User=`, `-o Port=`, `-i`, `-o IdentitiesOnly=`, `-o ProxyJump=`,
///    `-o IdentityAgent=`, `-o PKCS11Provider=`, `-o SecurityKeyProvider=`).
#[must_use]
pub fn build_gix_ssh_options(
    url: &gix::url::Url,
    core_ssh_command: Option<&str>,
) -> ssh::connect::Options {
    if let Ok(env_cmd) = std::env::var("GIT_SSH_COMMAND")
        && !env_cmd.trim().is_empty()
    {
        return ssh::connect::Options {
            command: Some(env_cmd.trim().into()),
            disallow_shell: false,
            kind: infer_ssh_program_kind(env_cmd.trim()),
        };
    }

    if let Some(cfg_cmd) = core_ssh_command.map(str::trim).filter(|s| !s.is_empty()) {
        return ssh::connect::Options {
            command: Some(cfg_cmd.into()),
            disallow_shell: false,
            kind: infer_ssh_program_kind(cfg_cmd),
        };
    }

    if let Ok(git_ssh) = std::env::var("GIT_SSH")
        && !git_ssh.trim().is_empty()
    {
        return ssh::connect::Options {
            command: Some(git_ssh.trim().into()),
            disallow_shell: true,
            kind: infer_ssh_program_kind(git_ssh.trim()),
        };
    }

    let host = url.host().unwrap_or("");
    let resolved = resolve_ssh_host(host);
    let url_has_user = url.user().is_some();
    let url_has_port = url.port.is_some();

    let cmd = format_openssh_command(&resolved, url_has_user, url_has_port);
    ssh::connect::Options {
        command: Some(cmd.into()),
        disallow_shell: false,
        kind: Some(ProgramKind::Ssh),
    }
}

/// Formats an OpenSSH CLI invocation string (`ssh [-F ...] [-o ...] [-i ...]`) reflecting
/// resolved `~/.ssh/config` settings without overriding explicit URL `user`/`port`.
#[must_use]
pub fn format_openssh_command(
    resolved: &ResolvedSshHost,
    url_has_user: bool,
    url_has_port: bool,
) -> String {
    let mut parts = vec!["ssh".to_string()];

    if let Some(ref cfg_file) = resolved.user_config_file {
        parts.push("-F".to_string());
        parts.push(shell_quote(&cfg_file.to_string_lossy()));
    }
    if let Some(ref hn) = resolved.hostname {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("HostName={hn}")));
    }
    if !url_has_user && let Some(ref u) = resolved.user {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("User={u}")));
    }
    if !url_has_port && let Some(p) = resolved.port {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("Port={p}")));
    }
    if let Some(io) = resolved.identities_only {
        parts.push("-o".to_string());
        parts.push(format!("IdentitiesOnly={}", if io { "yes" } else { "no" }));
    }
    for id_path in &resolved.identity_files {
        parts.push("-i".to_string());
        parts.push(shell_quote(&id_path.to_string_lossy()));
    }
    if let Some(ref pj) = resolved.proxy_jump {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("ProxyJump={pj}")));
    }
    if let Some(ref pc) = resolved.proxy_command {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("ProxyCommand={pc}")));
    }
    if let Some(ref agent) = resolved.identity_agent {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("IdentityAgent={agent}")));
    }
    if let Some(fa) = resolved.forward_agent {
        parts.push("-o".to_string());
        parts.push(format!("ForwardAgent={}", if fa { "yes" } else { "no" }));
    }
    if let Some(ref pkcs11) = resolved.pkcs11_provider {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("PKCS11Provider={pkcs11}")));
    }
    if let Some(ref sk) = resolved.security_key_provider {
        parts.push("-o".to_string());
        parts.push(shell_quote(&format!("SecurityKeyProvider={sk}")));
    }

    parts.join(" ")
}

fn infer_ssh_program_kind(cmd: &str) -> Option<ProgramKind> {
    let first = cmd.split_ascii_whitespace().next()?;
    let base = Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    if base.eq_ignore_ascii_case("ssh") || base.eq_ignore_ascii_case("ssh.exe") {
        Some(ProgramKind::Ssh)
    } else if base.eq_ignore_ascii_case("plink") || base.eq_ignore_ascii_case("plink.exe") {
        Some(ProgramKind::Plink)
    } else if base.eq_ignore_ascii_case("putty") || base.eq_ignore_ascii_case("putty.exe") {
        Some(ProgramKind::Putty)
    } else if base.eq_ignore_ascii_case("tortoiseplink.exe") {
        Some(ProgramKind::TortoisePlink)
    } else {
        None
    }
}

fn parse_ssh_config_file(
    path: &Path,
    include_base_dir: &Path,
    target_host: &str,
    home_dir: Option<&Path>,
    out: &mut ResolvedSshHost,
    depth: usize,
) -> std::io::Result<()> {
    if depth > MAX_SSH_INCLUDE_DEPTH || !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    parse_ssh_config_str(
        &content,
        include_base_dir,
        target_host,
        home_dir,
        out,
        depth,
    );
    Ok(())
}

/// Parses an in-memory SSH config string into `out` (exposed for direct unit testing).
pub fn parse_ssh_config_str(
    content: &str,
    include_base_dir: &Path,
    target_host: &str,
    home_dir: Option<&Path>,
    out: &mut ResolvedSshHost,
    depth: usize,
) {
    let mut active = true;
    for raw_line in content.lines() {
        let line = strip_ssh_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((keyword, rest)) = split_ssh_keyword_arg(line) else {
            continue;
        };

        if keyword.eq_ignore_ascii_case("Host") {
            let patterns = tokenize_ssh_args(rest);
            active = ssh_host_patterns_match(&patterns, target_host);
            continue;
        }

        if keyword.eq_ignore_ascii_case("Match") {
            let args = tokenize_ssh_args(rest);
            active = args.first().is_some_and(|a| a.eq_ignore_ascii_case("all"));
            continue;
        }

        if !active {
            continue;
        }

        if keyword.eq_ignore_ascii_case("Include") {
            if depth < MAX_SSH_INCLUDE_DEPTH {
                for pat in tokenize_ssh_args(rest) {
                    for inc_path in resolve_ssh_include_paths(&pat, include_base_dir, home_dir) {
                        let _ = parse_ssh_config_file(
                            &inc_path,
                            include_base_dir,
                            target_host,
                            home_dir,
                            out,
                            depth + 1,
                        );
                    }
                }
            }
            continue;
        }

        let value = unquote_single_arg(rest);
        if value.is_empty() {
            continue;
        }

        if keyword.eq_ignore_ascii_case("HostName") {
            if out.hostname.is_none() {
                out.hostname = Some(value);
            }
        } else if keyword.eq_ignore_ascii_case("User") {
            if out.user.is_none() {
                out.user = Some(value);
            }
        } else if keyword.eq_ignore_ascii_case("Port") {
            if out.port.is_none()
                && let Ok(p) = value.parse::<u16>()
            {
                out.port = Some(p);
            }
        } else if keyword.eq_ignore_ascii_case("IdentityFile") {
            let expanded = expand_ssh_tilde(&value, home_dir);
            out.identity_files.push(PathBuf::from(expanded));
        } else if keyword.eq_ignore_ascii_case("IdentitiesOnly") {
            if out.identities_only.is_none() {
                out.identities_only = parse_ssh_yes_no(&value);
            }
        } else if keyword.eq_ignore_ascii_case("ProxyJump") {
            if out.proxy_jump.is_none() {
                out.proxy_jump = Some(value);
            }
        } else if keyword.eq_ignore_ascii_case("ProxyCommand") {
            if out.proxy_command.is_none() {
                out.proxy_command = Some(rest.trim().to_string());
            }
        } else if keyword.eq_ignore_ascii_case("IdentityAgent") {
            if out.identity_agent.is_none() {
                out.identity_agent = Some(expand_ssh_tilde(&value, home_dir));
            }
        } else if keyword.eq_ignore_ascii_case("ForwardAgent") {
            if out.forward_agent.is_none() {
                out.forward_agent = parse_ssh_yes_no(&value);
            }
        } else if keyword.eq_ignore_ascii_case("PKCS11Provider") {
            if out.pkcs11_provider.is_none() {
                out.pkcs11_provider = Some(expand_ssh_tilde(&value, home_dir));
            }
        } else if keyword.eq_ignore_ascii_case("SecurityKeyProvider")
            && out.security_key_provider.is_none()
        {
            out.security_key_provider = Some(expand_ssh_tilde(&value, home_dir));
        }
    }
}

fn finalize_ssh_tokens(
    out: &mut ResolvedSshHost,
    home_dir: Option<&Path>,
    ssh_auth_sock: Option<&str>,
) {
    let eff_host = out
        .hostname
        .clone()
        .unwrap_or_else(|| out.target_host.clone());
    let eff_port = out.port.unwrap_or(22).to_string();
    let eff_user = out
        .user
        .clone()
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "git".to_string());

    if let Some(ref mut hn) = out.hostname {
        *hn = expand_ssh_tokens(hn, &out.target_host, &eff_host, &eff_port, &eff_user);
    }
    for id in &mut out.identity_files {
        let s = id.to_string_lossy().into_owned();
        let expanded = expand_ssh_tokens(&s, &out.target_host, &eff_host, &eff_port, &eff_user);
        *id = PathBuf::from(expand_ssh_tilde(&expanded, home_dir));
    }
    if let Some(ref mut pj) = out.proxy_jump {
        *pj = expand_ssh_tokens(pj, &out.target_host, &eff_host, &eff_port, &eff_user);
    }
    if let Some(ref mut pc) = out.proxy_command {
        *pc = expand_ssh_tokens(pc, &out.target_host, &eff_host, &eff_port, &eff_user);
    }

    out.effective_agent_sock = match out.identity_agent.as_deref() {
        Some(val) if val.eq_ignore_ascii_case("none") => None,
        Some(val) if val.eq_ignore_ascii_case("SSH_AUTH_SOCK") => ssh_auth_sock.map(PathBuf::from),
        Some(env_ref) if env_ref.starts_with('$') => std::env::var(&env_ref[1..])
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        Some(explicit) => Some(PathBuf::from(explicit)),
        None => ssh_auth_sock.map(PathBuf::from),
    };
}

fn expand_ssh_tokens(raw: &str, orig_host: &str, hostname: &str, port: &str, user: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.next() {
                Some('%') => out.push('%'),
                Some('h') => out.push_str(hostname),
                Some('n') => out.push_str(orig_host),
                Some('p') => out.push_str(port),
                Some('r') => out.push_str(user),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn expand_ssh_tilde(raw: &str, home_dir: Option<&Path>) -> String {
    if raw == "~"
        && let Some(h) = home_dir
    {
        return h.to_string_lossy().into_owned();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home_dir
    {
        return h.join(rest).to_string_lossy().into_owned();
    }
    raw.to_string()
}

fn resolve_ssh_include_paths(
    pattern: &str,
    base_dir: &Path,
    home_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let expanded = expand_ssh_tilde(pattern, home_dir);
    let abs_pattern = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        base_dir.join(expanded)
    };
    let pat_str = abs_pattern.to_string_lossy();
    if !pat_str.contains('*') && !pat_str.contains('?') {
        return if abs_pattern.is_file() {
            vec![abs_pattern]
        } else {
            Vec::new()
        };
    }

    let Some(search_root) = abs_pattern.parent() else {
        return Vec::new();
    };
    let Some(file_pat) = abs_pattern.file_name().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(search_root) else {
        return Vec::new();
    };
    let mut matched: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| ssh_wildcard_match(file_pat, name))
        })
        .collect();
    matched.sort();
    matched
}

fn ssh_host_patterns_match(patterns: &[String], target_host: &str) -> bool {
    let mut matched_positive = false;
    for pat in patterns {
        if let Some(neg) = pat.strip_prefix('!') {
            if ssh_wildcard_match(neg, target_host) {
                return false;
            }
        } else if ssh_wildcard_match(pat, target_host) {
            matched_positive = true;
        }
    }
    matched_positive
}

fn ssh_wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    wildcard_dp(&p, &t)
}

fn wildcard_dp(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn strip_ssh_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == '#' && !in_quotes {
            return &line[..idx];
        }
    }
    line
}

fn split_ssh_keyword_arg(line: &str) -> Option<(&str, &str)> {
    let mut sep_idx = None;
    let mut eq_seen = false;
    for (idx, ch) in line.char_indices() {
        if ch == '=' {
            sep_idx = Some(idx);
            eq_seen = true;
            break;
        }
        if ch.is_ascii_whitespace() {
            sep_idx = Some(idx);
            break;
        }
    }
    let idx = sep_idx?;
    let key = line[..idx].trim();
    let mut rest = line[idx + 1..].trim_start();
    if !eq_seen {
        rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    }
    if key.is_empty() {
        None
    } else {
        Some((key, rest))
    }
}

fn tokenize_ssh_args(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in rest.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch.is_ascii_whitespace() && !in_quotes {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote_single_arg(rest: &str) -> String {
    let t = rest.trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn parse_ssh_yes_no(raw: &str) -> Option<bool> {
    let lower = raw.trim().to_ascii_lowercase();
    match lower.as_str() {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn shell_quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolves_ssh_config_wildcards_negation_first_match_and_proxy_jump() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let ssh_dir = home.join(".ssh");
        fs::create_dir_all(ssh_dir.join("conf.d")).unwrap();

        fs::write(
            ssh_dir.join("conf.d/10-corp.conf"),
            r#"
Host *.corp.internal !bastion.corp.internal
    User corp-git
    Port 2222
    IdentityFile ~/.ssh/id_corp_%r
    ProxyJump bastion.corp.internal:2200
"#,
        )
        .unwrap();

        fs::write(
            ssh_dir.join("config"),
            r#"
Include conf.d/*.conf

Host ghe-alias
    HostName git.corp.internal
    User alias-user
    Port 2202
    IdentityFile ~/.ssh/id_alias_sk
    IdentitiesOnly yes
    SecurityKeyProvider /usr/lib/libsk-libfido2.so
    ProxyJump jump@bastion:2201

Host *
    User fallback-user
    Port 22
    IdentityFile ~/.ssh/id_fallback
"#,
        )
        .unwrap();

        let ctx = SshConfigSearchContext {
            home_dir: Some(home.clone()),
            user_config: Some(ssh_dir.join("config")),
            ssh_auth_sock: Some("/tmp/agent.sock".to_string()),
            skip_system_config: true,
            ..Default::default()
        };

        // 1. Exact alias first-match-wins over `Host *`
        let ghe = resolve_ssh_host_with_context("ghe-alias", &ctx);
        assert_eq!(ghe.hostname.as_deref(), Some("git.corp.internal"));
        assert_eq!(ghe.user.as_deref(), Some("alias-user"));
        assert_eq!(ghe.port, Some(2202));
        assert_eq!(ghe.identities_only, Some(true));
        assert_eq!(ghe.proxy_jump.as_deref(), Some("jump@bastion:2201"));
        assert_eq!(
            ghe.security_key_provider.as_deref(),
            Some("/usr/lib/libsk-libfido2.so")
        );
        assert_eq!(
            ghe.identity_files,
            vec![home.join(".ssh/id_alias_sk"), home.join(".ssh/id_fallback")]
        );
        assert_eq!(
            ghe.effective_agent_sock,
            Some(PathBuf::from("/tmp/agent.sock"))
        );

        // 2. Wildcard + Include + %r token expansion
        let corp = resolve_ssh_host_with_context("code.corp.internal", &ctx);
        assert_eq!(corp.user.as_deref(), Some("corp-git"));
        assert_eq!(corp.port, Some(2222));
        assert_eq!(
            corp.proxy_jump.as_deref(),
            Some("bastion.corp.internal:2200")
        );
        assert_eq!(corp.identity_files[0], home.join(".ssh/id_corp_corp-git"));

        // 3. Negated `!bastion.corp.internal` skips wildcard rule and hits `Host *`
        let bastion = resolve_ssh_host_with_context("bastion.corp.internal", &ctx);
        assert_eq!(bastion.user.as_deref(), Some("fallback-user"));
        assert_eq!(bastion.port, Some(22));
        assert_eq!(bastion.proxy_jump, None);
    }

    #[test]
    fn formats_openssh_command_without_clobbering_explicit_url_user_or_port() {
        let mut resolved = ResolvedSshHost {
            target_host: "work-gh".to_string(),
            hostname: Some("github.com".to_string()),
            user: Some("git".to_string()),
            port: Some(2222),
            identity_files: vec![PathBuf::from("/home/ada/.ssh/id_ed25519")],
            identities_only: Some(true),
            proxy_jump: Some("bastion".to_string()),
            user_config_file: Some(PathBuf::from("/home/ada/.ssh/config")),
            ..Default::default()
        };
        let cmd_defaults = format_openssh_command(&resolved, false, false);
        assert!(
            cmd_defaults.contains("HostName=github.com"),
            "{cmd_defaults}"
        );
        assert!(cmd_defaults.contains("User=git"), "{cmd_defaults}");
        assert!(cmd_defaults.contains("Port=2222"), "{cmd_defaults}");
        assert!(cmd_defaults.contains("ProxyJump=bastion"), "{cmd_defaults}");

        let cmd_url_overrides = format_openssh_command(&resolved, true, true);
        assert!(
            !cmd_url_overrides.contains("User=git"),
            "{cmd_url_overrides}"
        );
        assert!(
            !cmd_url_overrides.contains("Port=2222"),
            "{cmd_url_overrides}"
        );

        resolved.pkcs11_provider = Some("/usr/lib/opensc-pkcs11.so".to_string());
        let cmd_hw = format_openssh_command(&resolved, false, false);
        assert!(cmd_hw.contains("PKCS11Provider=/usr/lib/opensc-pkcs11.so"));
    }
}
