// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable POSIX polyglot trampoline, activation, `pyvenv.cfg`, and `.pth` rewriting
//! for Python virtual environments (`.venv/`).

use std::path::{Component, Path};

/// Parses a hardcoded absolute Python shebang (`#!/.../python...`) into `(exe_name, args)`.
///
/// Returns `None` for non-Python shebangs, relative paths, or `#!/usr/bin/env ...`.
#[must_use]
pub fn parse_python_shebang(first_line: &str) -> Option<(&str, &str)> {
    let trimmed = first_line.trim_end_matches(['\r', '\n']);
    let after_bang = trimmed.strip_prefix("#!")?.trim_start();
    parse_abs_python_invocation(after_bang)
}

fn parse_abs_python_invocation(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim_start();
    if let Some(quoted) = trimmed.strip_prefix('\'')
        && let Some(end_q) = quoted.find('\'')
    {
        let interp = &quoted[..end_q];
        let rest = quoted[end_q + 1..].trim();
        return extract_abs_python_exe(interp).map(|exe| (exe, rest));
    }
    if let Some(quoted) = trimmed.strip_prefix('"')
        && let Some(end_q) = quoted.find('"')
    {
        let interp = &quoted[..end_q];
        let rest = quoted[end_q + 1..].trim();
        return extract_abs_python_exe(interp).map(|exe| (exe, rest));
    }
    if !trimmed.starts_with('/') {
        return None;
    }

    // Fast path: standard invocation with no spaces in interpreter path.
    let (interp, rest) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    };
    if let Some(exe) = extract_abs_python_exe(interp) {
        return Some((exe, rest));
    }

    // Fallback for unquoted hardcoded paths containing spaces before `.../python...`.
    let mut search_from = 0;
    while let Some(rel_idx) = trimmed[search_from..].find("/python") {
        let slash_idx = search_from + rel_idx;
        let after_slash = &trimmed[slash_idx + 1..];
        let exe_end = after_slash
            .find(char::is_whitespace)
            .unwrap_or(after_slash.len());
        let exe = &after_slash[..exe_end];
        if is_python_exe_name(exe) {
            let args = after_slash[exe_end..].trim();
            return Some((exe, args));
        }
        search_from = slash_idx + 1;
    }

    None
}

fn extract_abs_python_exe(interp: &str) -> Option<&str> {
    if !interp.starts_with('/') {
        return None;
    }
    let exe = interp.rsplit('/').next()?;
    is_python_exe_name(exe).then_some(exe)
}

fn is_python_exe_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("python") else {
        return false;
    };
    if suffix.is_empty() {
        return true;
    }
    // Accept versioned names like `python3`, `python3.14`, and ABI suffixes like `python3.14t` / `python3.13d`.
    let core = suffix.strip_suffix(['t', 'd']).unwrap_or(suffix);
    !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn format_trampoline_header(exe_name: &str, args: &str) -> String {
    if args.is_empty() {
        format!("#!/bin/sh\n'''exec' \"$(dirname \"$0\")/{exe_name}\" \"$0\" \"$@\"\n' '''\n")
    } else {
        format!(
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/{exe_name}\" {args} \"$0\" \"$@\"\n' '''\n"
        )
    }
}

fn parse_long_shebang_trampoline(content_str: &str) -> Option<(&str, &str, &str)> {
    let mut lines = content_str.split_inclusive('\n');
    let line1 = lines.next()?;
    let line2 = lines.next()?;
    let line3 = lines.next()?;

    let shebang = line1.trim_end_matches(['\r', '\n']).trim();
    if shebang != "#!/bin/sh" && shebang != "#!/usr/bin/env sh" {
        return None;
    }
    if line3.trim_end_matches(['\r', '\n']).trim() != "' '''" {
        return None;
    }

    let exec_body = line2
        .trim_end_matches(['\r', '\n'])
        .trim_start()
        .strip_prefix("'''exec'")?
        .trim_start();
    // Already portable relative trampoline.
    if exec_body.contains("$(dirname \"$0\")") || exec_body.contains("$(dirname '$0')") {
        return None;
    }

    let (exe_name, trailing) = parse_abs_python_invocation(exec_body)?;
    let args = trailing
        .strip_suffix("\"$0\" \"$@\"")
        .or_else(|| trailing.strip_suffix("'$0' '$@'"))?
        .trim();

    let header_len = line1.len() + line2.len() + line3.len();
    Some((exe_name, args, &content_str[header_len..]))
}

/// Rewrites a script with a hardcoded absolute `#!/.../python...` shebang (or `pip`/`uv` long-path
/// `#!/bin/sh\n'''exec' /abs/path/.../python... "$0" "$@"\n' '''` polyglot header) to a portable
/// relative POSIX polyglot trampoline:
///
/// ```sh
/// #!/bin/sh
/// '''exec' "$(dirname "$0")/python" "$0" "$@"
/// ' '''
/// ```
///
/// Returns `None` when the file is binary or does not contain a hardcoded absolute Python invocation.
#[must_use]
pub fn rewrite_python_trampoline_bytes(content: &[u8]) -> Option<Vec<u8>> {
    if !content.starts_with(b"#!") {
        return None;
    }

    let (first_line_bytes, rest_bytes) = match content.iter().position(|&b| b == b'\n') {
        Some(nl) => (&content[..nl], &content[nl + 1..]),
        None => (content, &[][..]),
    };
    if first_line_bytes.contains(&0) {
        return None;
    }

    let first_line = std::str::from_utf8(first_line_bytes).ok()?;
    if let Some((exe_name, args)) = parse_python_shebang(first_line) {
        let header = format_trampoline_header(exe_name, args);
        let mut out = Vec::with_capacity(header.len() + rest_bytes.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(rest_bytes);
        return Some(out);
    }

    let prefix_scan_len = content.len().min(4096);
    if content[..prefix_scan_len].contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let (exe_name, args, tail) = parse_long_shebang_trampoline(text)?;
    let header = format_trampoline_header(exe_name, args);
    let mut out = Vec::with_capacity(header.len() + tail.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(tail.as_bytes());
    Some(out)
}

/// Rewrites `.venv/bin/activate` and `.venv/bin/activate.fish` so `VIRTUAL_ENV` resolves
/// dynamically relative to the workspace's `.venv/bin/` directory instead of hardcoding the
/// original repository's `.venv` path.
#[must_use]
pub fn rewrite_venv_activate_bytes(
    file_name: &str,
    content: &[u8],
    orig_target_root: &Path,
) -> Option<Vec<u8>> {
    if content.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let orig_str = orig_target_root.to_string_lossy();
    let canon_str = std::fs::canonicalize(orig_target_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    let matches_orig = |val: &str| -> bool {
        let unquoted = val.trim().trim_matches('\'').trim_matches('"');
        unquoted == orig_str.as_ref() || canon_str.as_deref().is_some_and(|c| unquoted == c)
    };

    match file_name {
        "activate" => {
            if text.contains("BASH_SOURCE[0]") {
                return None;
            }
            let mut changed = false;
            let mut out = String::with_capacity(text.len() + 256);
            for line in text.split_inclusive('\n') {
                let raw = line.trim_end_matches(['\r', '\n']);
                let indent_len = raw.len() - raw.trim_start().len();
                let indent = &raw[..indent_len];
                let trimmed = &raw[indent_len..];
                let eol = &line[raw.len()..];

                if let Some(rhs) = trimmed
                    .strip_prefix("export VIRTUAL_ENV=")
                    .or_else(|| trimmed.strip_prefix("VIRTUAL_ENV="))
                    && matches_orig(rhs)
                {
                    let export_kw = if trimmed.starts_with("export ") {
                        "export VIRTUAL_ENV\n"
                    } else {
                        ""
                    };
                    let fallback = rhs.trim();
                    out.push_str(&format!(
                        "{indent}if [ -n \"${{BASH_VERSION:-}}\" ]; then\n\
                         {indent}    _OZ_VENV_BIN_DIR=\"$(cd \"$(dirname \"${{BASH_SOURCE[0]}}\")\" && pwd)\"\n\
                         {indent}    VIRTUAL_ENV=\"$(dirname \"$_OZ_VENV_BIN_DIR\")\"\n\
                         {indent}elif [ -n \"${{ZSH_VERSION:-}}\" ]; then\n\
                         {indent}    eval '_OZ_VENV_SRC=\"${{(%):-%x}}\"'\n\
                         {indent}    _OZ_VENV_BIN_DIR=\"$(cd \"$(dirname \"$_OZ_VENV_SRC\")\" && pwd)\"\n\
                         {indent}    VIRTUAL_ENV=\"$(dirname \"$_OZ_VENV_BIN_DIR\")\"\n\
                         {indent}else\n\
                         {indent}    VIRTUAL_ENV={fallback}\n\
                         {indent}fi\n"
                    ));
                    if !export_kw.is_empty() {
                        out.push_str(indent);
                        out.push_str(export_kw);
                    }
                    changed = true;
                    continue;
                }

                if let Some(inner) = trimmed.strip_prefix("VIRTUAL_ENV=$(cygpath ")
                    && let Some(path_arg) = inner.strip_suffix(')')
                    && matches_orig(path_arg)
                {
                    out.push_str(&format!(
                        "{indent}if [ -n \"${{BASH_VERSION:-}}\" ]; then\n\
                         {indent}    _OZ_VENV_BIN_DIR=\"$(cd \"$(dirname \"${{BASH_SOURCE[0]}}\")\" && pwd)\"\n\
                         {indent}    VIRTUAL_ENV=\"$(dirname \"$_OZ_VENV_BIN_DIR\")\"\n\
                         {indent}else\n\
                         {indent}    VIRTUAL_ENV=$(cygpath {path_arg})\n\
                         {indent}fi{eol}"
                    ));
                    changed = true;
                    continue;
                }

                out.push_str(line);
            }
            changed.then(|| out.into_bytes())
        }
        "activate.fish" => {
            if text.contains("status filename") {
                return None;
            }
            let mut changed = false;
            let mut out = String::with_capacity(text.len() + 64);
            for line in text.split_inclusive('\n') {
                let raw = line.trim_end_matches(['\r', '\n']);
                let indent_len = raw.len() - raw.trim_start().len();
                let indent = &raw[..indent_len];
                let trimmed = &raw[indent_len..];
                let eol = &line[raw.len()..];

                if let Some(rhs) = trimmed.strip_prefix("set -gx VIRTUAL_ENV ")
                    && matches_orig(rhs)
                {
                    out.push_str(&format!(
                        "{indent}set -gx VIRTUAL_ENV (dirname (dirname (status filename))){eol}"
                    ));
                    changed = true;
                    continue;
                }
                out.push_str(line);
            }
            changed.then(|| out.into_bytes())
        }
        _ => None,
    }
}

/// Normalizes `pyvenv.cfg` so `command = ...` (or other lines containing the original `.venv`
/// or repository root path) never leaks the original repository's filesystem location across
/// spawned workspaces.
#[must_use]
pub fn rewrite_pyvenv_cfg_bytes(
    content: &[u8],
    repo_root: &Path,
    orig_target_root: &Path,
    target_rel_path: &str,
) -> Option<Vec<u8>> {
    if content.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let target_str = orig_target_root.to_string_lossy();
    let repo_str = repo_root.to_string_lossy();
    let canon_target = std::fs::canonicalize(orig_target_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let canon_repo = std::fs::canonicalize(repo_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut updated = line.to_string();
        for pat in [Some(target_str.as_ref()), canon_target.as_deref()]
            .into_iter()
            .flatten()
        {
            if !pat.is_empty() && updated.contains(pat) {
                updated = updated.replace(pat, target_rel_path);
                changed = true;
            }
        }
        for pat in [Some(repo_str.as_ref()), canon_repo.as_deref()]
            .into_iter()
            .flatten()
        {
            if !pat.is_empty() && updated.contains(pat) {
                updated = updated.replace(pat, ".");
                changed = true;
            }
        }
        out.push_str(&updated);
    }

    changed.then(|| out.into_bytes())
}

/// Rewrites Python `site-packages/*.pth` editable-install path entries that point inside
/// `repo_root` into workspace-relative `import os, sys; ...` lines anchored at `sys.prefix`.
#[must_use]
pub fn rewrite_python_pth_bytes(content: &[u8], repo_root: &Path) -> Option<Vec<u8>> {
    if content.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(content).ok()?;
    let repo_canon = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    let mut changed = false;
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let raw = line.trim_end_matches(['\r', '\n']);
        let eol = &line[raw.len()..];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("import ") {
            out.push_str(line);
            continue;
        }
        let p = Path::new(trimmed);
        let rel = p
            .strip_prefix(repo_root)
            .ok()
            .or_else(|| p.strip_prefix(&repo_canon).ok());
        if let Some(rel_path) = rel {
            let rel_slash = rel_path
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            if rel_slash.is_empty() {
                out.push_str(&format!(
                    "import os, sys; sys.path.append(os.path.dirname(os.path.abspath(sys.prefix))){eol}"
                ));
            } else {
                let escaped = rel_slash.replace('\\', "\\\\").replace('"', "\\\"");
                out.push_str(&format!(
                    "import os, sys; sys.path.append(os.path.join(os.path.dirname(os.path.abspath(sys.prefix)), \"{escaped}\")){eol}"
                ));
            }
            changed = true;
        } else {
            out.push_str(line);
        }
    }

    changed.then(|| out.into_bytes())
}

/// Returns `true` if `rel_path` points to a direct script inside `.venv/bin/` (or `<target>/bin/`
/// for a Python target).
#[must_use]
pub fn is_venv_bin_script(rel_path: &Path) -> bool {
    let comps: Vec<&str> = rel_path
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => os.to_str(),
            _ => None,
        })
        .collect();
    comps.len() >= 2 && comps[comps.len() - 2] == "bin"
}

/// Returns `true` if `rel_path` points to a `.pth` file inside `site-packages/`.
#[must_use]
pub fn is_venv_pth_file(rel_path: &Path) -> bool {
    rel_path.extension().and_then(|s| s.to_str()) == Some("pth")
        && rel_path
            .components()
            .any(|c| matches!(c, Component::Normal(s) if s == "site-packages"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_python_and_python3_hardcoded_shebangs() {
        let src = b"#!/old/workspace/.venv/bin/python3\nimport sys\nprint(sys.executable)\n";
        let rewritten = rewrite_python_trampoline_bytes(src).expect("should rewrite");
        let text = String::from_utf8(rewritten).expect("utf8");
        assert_eq!(
            text,
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python3\" \"$0\" \"$@\"\n' '''\nimport sys\nprint(sys.executable)\n"
        );

        let src_py = b"#!/opt/repo/.venv/bin/python\nprint('ok')\n";
        let rewritten_py = rewrite_python_trampoline_bytes(src_py).expect("should rewrite");
        assert!(String::from_utf8(rewritten_py).expect("utf8").starts_with(
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python\" \"$0\" \"$@\"\n' '''\n"
        ));
    }

    #[test]
    fn rewrites_long_path_pip_trampoline_headers() {
        let long_path = format!("/{}/repo/.venv/bin/python3.14", "deep".repeat(35));
        let src = format!(
            "#!/bin/sh\n'''exec' {long_path} \"$0\" \"$@\"\n' '''\nimport sys\nprint('LONG_OK')\n"
        );
        let rewritten = rewrite_python_trampoline_bytes(src.as_bytes()).expect("rewrite long");
        let text = String::from_utf8(rewritten).expect("utf8");
        assert_eq!(
            text,
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python3.14\" \"$0\" \"$@\"\n' '''\nimport sys\nprint('LONG_OK')\n"
        );
        // Re-running on an already-portable trampoline is a clean no-op.
        assert!(rewrite_python_trampoline_bytes(text.as_bytes()).is_none());
    }

    #[test]
    fn rewrites_quoted_long_path_and_free_threaded_python_exe() {
        let src = b"#!/bin/sh\n'''exec' '/path with spaces/.venv/bin/python3.14t' \"$0\" \"$@\"\n' '''\nprint(1)\n";
        let rewritten = rewrite_python_trampoline_bytes(src).expect("rewrite quoted");
        let text = String::from_utf8(rewritten).expect("utf8");
        assert_eq!(
            text,
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python3.14t\" \"$0\" \"$@\"\n' '''\nprint(1)\n"
        );
    }

    #[test]
    fn rewrites_activate_pyvenv_cfg_and_editable_pth() {
        let repo = Path::new("/old/repo");
        let venv = Path::new("/old/repo/.venv");

        let cfg = b"home = /usr/bin\ninclude-system-site-packages = false\ncommand = /usr/bin/python3 -m venv /old/repo/.venv\n";
        let new_cfg = rewrite_pyvenv_cfg_bytes(cfg, repo, venv, ".venv").expect("cfg rewrite");
        assert_eq!(
            String::from_utf8(new_cfg).expect("utf8"),
            "home = /usr/bin\ninclude-system-site-packages = false\ncommand = /usr/bin/python3 -m venv .venv\n"
        );

        let activate = b"deactivate nondestructive\nexport VIRTUAL_ENV=/old/repo/.venv\nPATH=\"$VIRTUAL_ENV/bin:$PATH\"\n";
        let new_act = rewrite_venv_activate_bytes("activate", activate, venv).expect("act rewrite");
        let act_str = String::from_utf8(new_act).expect("utf8");
        assert!(act_str.contains("BASH_SOURCE[0]"));
        assert!(!act_str.contains("export VIRTUAL_ENV=/old/repo/.venv"));

        let pth = b"/old/repo/src\n/usr/lib/python3/dist-packages\n";
        let new_pth = rewrite_python_pth_bytes(pth, repo).expect("pth rewrite");
        let pth_str = String::from_utf8(new_pth).expect("utf8");
        assert!(pth_str.contains("os.path.dirname(os.path.abspath(sys.prefix)), \"src\""));
        assert!(pth_str.contains("/usr/lib/python3/dist-packages"));
    }

    #[test]
    fn leaves_non_python_and_env_shebangs_untouched() {
        assert!(rewrite_python_trampoline_bytes(b"#!/bin/sh\necho hi\n").is_none());
        assert!(rewrite_python_trampoline_bytes(b"#!/usr/bin/env python3\n").is_none());
        assert!(rewrite_python_trampoline_bytes(b"\x7fELF\x02\x01\x01\x00").is_none());
    }
}
