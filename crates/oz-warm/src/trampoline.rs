// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable POSIX polyglot trampoline rewriting for Python `.venv/bin/*` scripts.

use std::path::{Component, Path};

/// Parses a hardcoded absolute Python shebang (`#!/.../python...`) into `(exe_name, args)`.
///
/// Returns `None` for non-Python shebangs, relative paths, or `#!/usr/bin/env ...`.
#[must_use]
pub fn parse_python_shebang(first_line: &str) -> Option<(&str, &str)> {
    let trimmed = first_line.trim_end_matches(['\r', '\n']);
    let after_bang = trimmed.strip_prefix("#!")?.trim_start();
    if !after_bang.starts_with('/') {
        return None;
    }

    // Fast path: standard shebang with no spaces in interpreter path.
    let (interp, rest) = match after_bang.find(char::is_whitespace) {
        Some(idx) => (&after_bang[..idx], after_bang[idx..].trim()),
        None => (after_bang, ""),
    };
    if let Some(exe) = interp.rsplit('/').next()
        && is_python_exe_name(exe)
    {
        return Some((exe, rest));
    }

    // Fallback for hardcoded paths containing spaces before `.../python...`.
    let mut search_from = 0;
    while let Some(rel_idx) = after_bang[search_from..].find("/python") {
        let slash_idx = search_from + rel_idx;
        let after_slash = &after_bang[slash_idx + 1..];
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

fn is_python_exe_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("python") else {
        return false;
    };
    suffix.is_empty()
        || suffix
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
}

/// Rewrites a script with a hardcoded absolute `#!/.../python...` shebang to a portable
/// relative POSIX polyglot trampoline:
///
/// ```sh
/// #!/bin/sh
/// '''exec' "$(dirname "$0")/python" "$0" "$@"
/// ' '''
/// ```
///
/// Returns `None` when the file is binary or does not begin with a hardcoded absolute Python shebang.
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
    let (exe_name, args) = parse_python_shebang(first_line)?;

    let header = if args.is_empty() {
        format!("#!/bin/sh\n'''exec' \"$(dirname \"$0\")/{exe_name}\" \"$0\" \"$@\"\n' '''\n")
    } else {
        format!(
            "#!/bin/sh\n'''exec' \"$(dirname \"$0\")/{exe_name}\" {args} \"$0\" \"$@\"\n' '''\n"
        )
    };

    let mut out = Vec::with_capacity(header.len() + rest_bytes.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(rest_bytes);
    Some(out)
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
        assert!(String::from_utf8(rewritten_py)
            .expect("utf8")
            .starts_with("#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python\" \"$0\" \"$@\"\n' '''\n"));
    }

    #[test]
    fn leaves_non_python_and_env_shebangs_untouched() {
        assert!(rewrite_python_trampoline_bytes(b"#!/bin/sh\necho hi\n").is_none());
        assert!(rewrite_python_trampoline_bytes(b"#!/usr/bin/env python3\n").is_none());
        assert!(rewrite_python_trampoline_bytes(b"\x7fELF\x02\x01\x01\x00").is_none());
    }
}
