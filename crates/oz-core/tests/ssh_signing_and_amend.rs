// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for Milestone 3 Pure-Rust Ed25519 SSH Commit Signing (`SSHSIG` v1)
//! and `--amend` enhancements, including byte-level interoperability against system
//! `/usr/bin/ssh-keygen` (`-Y verify` & `-Y sign`) and `/usr/bin/git` (`verify-commit`).

use oz_core::{
    CommitOptions, CommitOutcome, ExitCode, GIT_SSHSIG_NAMESPACE, OpenSshPrivateKey,
    OpenSshPublicKey, OzoneRepo, SSHSIG_ARMOR_FOOTER, SSHSIG_ARMOR_HEADER, commit_changes,
    compute_status, inspect_ssh_signing_config, verify_commit_signature,
    verify_commit_signature_with_allowed_signers, verify_sshsig,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run /usr/bin/git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn ssh_keygen(args: &[&str]) -> String {
    let out = Command::new("/usr/bin/ssh-keygen")
        .args(args)
        .output()
        .expect("run /usr/bin/ssh-keygen");
    assert!(
        out.status.success(),
        "ssh-keygen {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo_with_user(dir: &Path, email: &str) {
    fs::create_dir_all(dir).expect("mkdir repo");
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Ozone Signer"]);
    git(dir, &["config", "user.email", email]);
}

fn generate_ed25519_keypair(dir: &Path, filename: &str, passphrase: &str, comment: &str) -> (PathBuf, PathBuf) {
    let priv_path = dir.join(filename);
    let pub_path = dir.join(format!("{filename}.pub"));
    ssh_keygen(&[
        "-q",
        "-t",
        "ed25519",
        "-N",
        passphrase,
        "-C",
        comment,
        "-f",
        priv_path.to_str().unwrap(),
    ]);
    (priv_path, pub_path)
}

/// Extracts `(unsigned_commit_bytes, armored_gpgsig)` directly from `git cat-file commit <rev>`
/// so `/usr/bin/ssh-keygen -Y verify -n git` can verify the exact commit payload independently.
fn extract_raw_commit_and_gpgsig(repo_dir: &Path, rev: &str) -> (Vec<u8>, String) {
    let out = Command::new("/usr/bin/git")
        .args(["cat-file", "commit", rev])
        .current_dir(repo_dir)
        .output()
        .expect("git cat-file commit");
    assert!(out.status.success());
    let raw = &out.stdout;

    // Split headers and body at the first `\n\n`.
    let sep = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .expect("commit header/body separator");
    let header_block = std::str::from_utf8(&raw[..sep]).expect("utf8 headers");
    let body_with_leading_nl = &raw[sep + 1..]; // starts with `\n` before message

    let mut unsigned_headers = String::new();
    let mut sig_lines: Vec<String> = Vec::new();
    let mut in_gpgsig = false;

    for line in header_block.lines() {
        if let Some(first_sig_line) = line.strip_prefix("gpgsig ") {
            in_gpgsig = true;
            sig_lines.push(first_sig_line.to_string());
        } else if in_gpgsig && let Some(cont) = line.strip_prefix(' ') {
            sig_lines.push(cont.to_string());
        } else {
            in_gpgsig = false;
            unsigned_headers.push_str(line);
            unsigned_headers.push('\n');
        }
    }

    assert!(!sig_lines.is_empty(), "commit must contain a gpgsig header");
    let mut unsigned_payload = unsigned_headers.into_bytes();
    unsigned_payload.extend_from_slice(body_with_leading_nl);

    let mut armored_sig = sig_lines.join("\n");
    armored_sig.push('\n');
    (unsigned_payload, armored_sig)
}

#[test]
fn pure_rust_signed_commits_pass_both_ssh_keygen_y_verify_and_git_verify_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let key_dir = tmp.path().join("keys");
    fs::create_dir_all(&key_dir).expect("mkdir keys");

    let signer_email = "signer@ozone.dev";
    let (priv_path, pub_path) =
        generate_ed25519_keypair(&key_dir, "id_ed25519", "", signer_email);

    // 1. Verify OpenSSH key parsing and SHA256:<base64-no-pad> fingerprint match `ssh-keygen -lf`
    let parsed_priv = OpenSshPrivateKey::from_file(&priv_path).expect("parse ssh-keygen privkey");
    let pub_text = fs::read_to_string(&pub_path).expect("read .pub");
    let parsed_pub = OpenSshPublicKey::parse_openssh(&pub_text).expect("parse .pub");
    assert_eq!(parsed_priv.public_key(), parsed_pub);

    let keygen_fp_out = ssh_keygen(&["-lf", pub_path.to_str().unwrap(), "-E", "sha256"]);
    // Output format: `256 SHA256:<b64> signer@ozone.dev (ED25519)`
    let expected_fp = keygen_fp_out
        .split_whitespace()
        .nth(1)
        .expect("extract SHA256 fingerprint");
    assert_eq!(parsed_pub.fingerprint_sha256(), expected_fp);

    // 2. Also verify `oz-core` verifies signatures created by `/usr/bin/ssh-keygen -Y sign`
    let msg_file = tmp.path().join("sample_payload.txt");
    fs::write(&msg_file, b"cross-check payload from ssh-keygen -Y sign\n").unwrap();
    ssh_keygen(&[
        "-Y",
        "sign",
        "-f",
        priv_path.to_str().unwrap(),
        "-n",
        GIT_SSHSIG_NAMESPACE,
        msg_file.to_str().unwrap(),
    ]);
    let keygen_sig = fs::read_to_string(tmp.path().join("sample_payload.txt.sig")).unwrap();
    let cross_verified = verify_sshsig(
        b"cross-check payload from ssh-keygen -Y sign\n",
        &keygen_sig,
        GIT_SSHSIG_NAMESPACE,
    )
    .expect("oz-core must verify ssh-keygen -Y sign output");
    assert_eq!(cross_verified.public_key.fingerprint_sha256(), expected_fp);

    // 3. Create repository, configure allowed_signers, and author a pure-Rust SSH-signed commit
    let repo_root = tmp.path().join("repo");
    init_repo_with_user(&repo_root, signer_email);

    let allowed_signers = tmp.path().join("allowed_signers");
    fs::write(
        &allowed_signers,
        format!("{signer_email} {}\n", pub_text.trim()),
    )
    .expect("write allowed_signers");

    git(&repo_root, &["config", "gpg.format", "ssh"]);
    git(
        &repo_root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );

    fs::write(repo_root.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover repo");

    let out = commit_changes(
        &repo,
        &CommitOptions::new("feat(m3): pure-rust ssh signed commit")
            .with_sign(true)
            .with_signing_key(priv_path.to_string_lossy().to_string()),
    )
    .expect("commit signed");

    assert!(out.signed);
    assert!(!out.amended);
    assert_eq!(out.signature_fingerprint.as_deref(), Some(expected_fp));

    // 4. Verify raw commit signature with `/usr/bin/ssh-keygen -Y verify -n git`!
    let (unsigned_bytes, gpgsig_armored) = extract_raw_commit_and_gpgsig(&repo_root, "HEAD");
    assert!(gpgsig_armored.starts_with(SSHSIG_ARMOR_HEADER));
    assert!(gpgsig_armored.trim_end().ends_with(SSHSIG_ARMOR_FOOTER));

    let sig_path = tmp.path().join("commit_gpgsig.sig");
    fs::write(&sig_path, &gpgsig_armored).expect("write extracted sig");

    let mut child = Command::new("/usr/bin/ssh-keygen")
        .args([
            "-Y",
            "verify",
            "-f",
            allowed_signers.to_str().unwrap(),
            "-I",
            signer_email,
            "-n",
            "git",
            "-s",
            sig_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ssh-keygen -Y verify");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&unsigned_bytes)
        .expect("pipe unsigned commit bytes");
    let verify_out = child.wait_with_output().expect("wait ssh-keygen -Y verify");
    assert!(
        verify_out.status.success(),
        "ssh-keygen -Y verify failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&verify_out.stdout),
        String::from_utf8_lossy(&verify_out.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&verify_out.stdout),
        String::from_utf8_lossy(&verify_out.stderr)
    );
    assert!(combined.contains("Good \"git\" signature"));

    // 5. Verify commit directly with `/usr/bin/git verify-commit` AND `git fsck` AND `%G?`!
    let git_verify = Command::new("/usr/bin/git")
        .args(["verify-commit", "-v", &out.commit_oid])
        .current_dir(&repo_root)
        .output()
        .expect("run git verify-commit");
    assert!(
        git_verify.status.success(),
        "git verify-commit failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&git_verify.stdout),
        String::from_utf8_lossy(&git_verify.stderr)
    );
    let stderr_str = String::from_utf8_lossy(&git_verify.stderr);
    assert!(stderr_str.contains("Good \"git\" signature"));
    assert!(stderr_str.contains(expected_fp));

    let sig_status = git(&repo_root, &["log", "-1", "--format=%G?"]);
    assert_eq!(sig_status.trim(), "G");
    git(&repo_root, &["fsck", "--strict"]);

    // 6. Verify using pure-Rust `verify_commit_signature` & `verify_commit_signature_with_allowed_signers`
    let repo_after = OzoneRepo::discover(Some(&repo_root)).expect("re-discover");
    let oz_ver = verify_commit_signature(&repo_after, &out.commit_oid).expect("oz verify");
    assert!(oz_ver.valid);
    assert_eq!(oz_ver.namespace, "git");
    assert_eq!(oz_ver.hash_algorithm, "sha512");
    assert_eq!(oz_ver.fingerprint, expected_fp);
    assert_eq!(oz_ver.allowed_signer_matched, Some(true));
    assert_eq!(oz_ver.matched_principal.as_deref(), Some(signer_email));

    let oz_ver_head = verify_commit_signature_with_allowed_signers(
        &repo_after,
        "HEAD",
        &allowed_signers,
    )
    .expect("oz verify HEAD with allowed_signers");
    assert_eq!(oz_ver_head.commit_oid, out.commit_oid);

    // 7. Bidirectional check: C `git commit -S` signed commit verified by pure-Rust `oz-core`!
    git(
        &repo_root,
        &["config", "user.signingkey", priv_path.to_str().unwrap()],
    );
    fs::write(repo_root.join("cgit.rs"), "pub fn from_c_git() {}\n").unwrap();
    git(&repo_root, &["add", "cgit.rs"]);
    git(&repo_root, &["commit", "-S", "-m", "feat: signed by system git"]);
    let cgit_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    let repo_cgit = OzoneRepo::discover(Some(&repo_root)).expect("discover after c git");
    let cgit_ver = verify_commit_signature_with_allowed_signers(
        &repo_cgit,
        &cgit_oid,
        &allowed_signers,
    )
    .expect("pure-Rust oz-core must verify commits signed by system git");
    assert!(cgit_ver.valid);
    assert_eq!(cgit_ver.fingerprint, expected_fp);
}

#[test]
fn auto_signing_config_precedence_doctor_helper_and_encrypted_or_rsa_errors() {
    let tmp = TempDir::new().expect("tempdir");
    let key_dir = tmp.path().join("keys");
    fs::create_dir_all(&key_dir).unwrap();

    let email = "dev@ozone.dev";
    let (ed_priv, ed_pub) = generate_ed25519_keypair(&key_dir, "ed_unenc", "", email);
    let (enc_priv, _) = generate_ed25519_keypair(&key_dir, "ed_encrypted", "secret-passphrase", email);

    let rsa_priv = key_dir.join("id_rsa");
    ssh_keygen(&[
        "-q",
        "-t",
        "rsa",
        "-b",
        "2048",
        "-N",
        "",
        "-C",
        email,
        "-f",
        rsa_priv.to_str().unwrap(),
    ]);

    let allowed_signers = tmp.path().join("allowed_signers");
    fs::write(
        &allowed_signers,
        format!("{email} {}\n", fs::read_to_string(&ed_pub).unwrap().trim()),
    )
    .unwrap();

    let repo_root = tmp.path().join("cfg-repo");
    init_repo_with_user(&repo_root, email);
    git(&repo_root, &["config", "gpg.format", "ssh"]);
    git(
        &repo_root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );
    // Point `user.signingkey` at the `.pub` file to verify automatic sibling private key lookup!
    git(
        &repo_root,
        &["config", "user.signingkey", ed_pub.to_str().unwrap()],
    );
    git(&repo_root, &["config", "commit.gpgsign", "true"]);

    // 1. Inspect doctor helper when git config enables SSH signing
    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let doc = inspect_ssh_signing_config(&repo).expect("inspect signing");
    assert!(doc.enabled_by_default);
    assert!(doc.ready);
    assert!(!doc.key_encrypted);
    assert_eq!(doc.key_source.as_deref(), Some("git_config"));
    assert_eq!(doc.key_algorithm.as_deref(), Some("ssh-ed25519"));
    assert_eq!(doc.resolved_key_path.as_deref(), Some(ed_priv.as_path()));

    // `sign: None` auto-signs because `commit.gpgsign=true` and `gpg.format=ssh`!
    fs::write(repo_root.join("auto.txt"), "auto-signed via git config\n").unwrap();
    let auto_out =
        commit_changes(&repo, &CommitOptions::new("feat: auto sign via git config")).unwrap();
    assert!(auto_out.signed);
    git(&repo_root, &["verify-commit", &auto_out.commit_oid]);

    // `sign: Some(false)` explicitly disables signing even when auto-signing is active!
    fs::write(repo_root.join("unsigned.txt"), "explicitly unsigned\n").unwrap();
    let repo2 = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let nosign_out = commit_changes(
        &repo2,
        &CommitOptions::new("chore: unsigned override").with_sign(false),
    )
    .unwrap();
    assert!(!nosign_out.signed);
    assert!(nosign_out.signature_fingerprint.is_none());
    assert!(verify_commit_signature(&repo2, &nosign_out.commit_oid).is_err());

    // 2. `.ozone.toml` `[signing]` overrides git config
    git(&repo_root, &["config", "commit.gpgsign", "false"]);
    fs::write(
        repo_root.join(".ozone.toml"),
        format!(
            "[signing]\nenabled = true\nkey = {:?}\n",
            ed_priv.to_string_lossy()
        ),
    )
    .unwrap();
    let repo_toml = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let doc_toml = inspect_ssh_signing_config(&repo_toml).unwrap();
    assert!(doc_toml.enabled_by_default);
    assert!(doc_toml.ready);
    assert_eq!(doc_toml.key_source.as_deref(), Some("ozone_config"));

    fs::write(repo_root.join("from_toml.txt"), "signed via .ozone.toml\n").unwrap();
    let toml_out =
        commit_changes(&repo_toml, &CommitOptions::new("feat: signed via ozone.toml")).unwrap();
    assert!(toml_out.signed);
    git(&repo_root, &["verify-commit", &toml_out.commit_oid]);

    // 3. Encrypted key produces clear diagnostic and aborts commit cleanly without advancing HEAD
    fs::write(repo_root.join("blocked.txt"), "should not commit\n").unwrap();
    let head_before = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();
    let enc_err = commit_changes(
        &repo_toml,
        &CommitOptions::new("fail encrypted")
            .with_sign(true)
            .with_signing_key(enc_priv.to_string_lossy().to_string()),
    )
    .expect_err("encrypted key must fail");
    assert_eq!(enc_err.exit_code(), ExitCode::UsageOrGuardrail);
    assert!(enc_err.to_string().contains("encrypted"));
    assert_eq!(
        git(&repo_root, &["rev-parse", "HEAD"]).trim(),
        head_before,
        "HEAD must not advance when signing key fails preflight"
    );

    // Doctor reports `key_encrypted = true` when `.ozone.toml` points at encrypted key
    fs::write(
        repo_root.join(".ozone.toml"),
        format!(
            "[signing]\nenabled = true\nkey = {:?}\n",
            enc_priv.to_string_lossy()
        ),
    )
    .unwrap();
    let repo_enc = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let doc_enc = inspect_ssh_signing_config(&repo_enc).unwrap();
    assert!(!doc_enc.ready);
    assert!(doc_enc.key_encrypted);

    // 4. Unsupported RSA key algorithm produces clear diagnostic
    let rsa_err = commit_changes(
        &repo_toml,
        &CommitOptions::new("fail rsa")
            .with_sign(true)
            .with_signing_key(rsa_priv.to_string_lossy().to_string()),
    )
    .expect_err("rsa key must fail");
    assert!(rsa_err.to_string().contains("ssh-rsa"));
}

#[test]
fn amend_root_child_merge_partial_pick_exclude_change_id_and_signed_amend() {
    let tmp = TempDir::new().expect("tempdir");
    let key_dir = tmp.path().join("keys");
    fs::create_dir_all(&key_dir).unwrap();
    let email = "amend@ozone.dev";
    let (priv_path, pub_path) = generate_ed25519_keypair(&key_dir, "id_ed25519", "", email);

    let allowed_signers = tmp.path().join("allowed_signers");
    fs::write(
        &allowed_signers,
        format!("{email} {}\n", fs::read_to_string(&pub_path).unwrap().trim()),
    )
    .unwrap();

    let repo_root = tmp.path().join("amend-repo");
    init_repo_with_user(&repo_root, email);
    git(&repo_root, &["config", "gpg.format", "ssh"]);
    git(
        &repo_root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );

    // 0. Amending an unborn branch must return a clear InvalidUsage error
    let unborn_repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    fs::write(repo_root.join("a.rs"), "fn a() {}\n").unwrap();
    let unborn_err = commit_changes(
        &unborn_repo,
        &CommitOptions::new("cannot amend unborn").with_amend(true),
    )
    .expect_err("amend on unborn HEAD must fail");
    assert_eq!(unborn_err.exit_code(), ExitCode::UsageOrGuardrail);

    // 1. Create initial root commit and amend it (root commit has 0 parents -> amended still has 0 parents!)
    let c1 = commit_changes(&unborn_repo, &CommitOptions::new("feat: initial root commit"))
        .expect("initial commit");
    assert!(c1.parent_oid.is_none());
    assert!(!c1.amended);
    let root_cid = c1.change_id.clone();

    fs::write(repo_root.join("a.rs"), "fn a() -> i32 { 1 }\n").unwrap();
    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let c1_amended = commit_changes(
        &repo,
        &CommitOptions::new("feat: amended root commit").with_amend(true),
    )
    .expect("amend root commit");
    assert!(c1_amended.amended);
    assert!(c1_amended.parent_oid.is_none());
    assert_eq!(
        c1_amended.change_id, root_cid,
        "amending root commit must preserve Change-Id"
    );
    assert_eq!(git(&repo_root, &["rev-list", "--count", "HEAD"]).trim(), "1");

    // 2. Create second commit with multi-section file so we can test surgical hunk `--pick` & `--exclude` amend
    let base_file = (1..=20)
        .map(|i| format!("line {i:02}\n"))
        .collect::<String>();
    fs::write(repo_root.join("multi.txt"), &base_file).unwrap();
    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let c2 = commit_changes(&repo, &CommitOptions::new("feat: add multi.txt")).unwrap();
    let c2_cid = c2.change_id.clone();
    assert_eq!(c2.parent_oid.as_deref(), Some(c1_amended.commit_oid.as_str()));

    // 2a. Clean-tree message-only amend + empty-message amend preserve Change-Id and tree
    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let c2_msg_amend = commit_changes(
        &repo,
        &CommitOptions::new("feat: add multi.txt (refined)").with_amend(true),
    )
    .unwrap();
    assert_eq!(c2_msg_amend.tree_oid, c2.tree_oid);
    assert_eq!(c2_msg_amend.change_id, c2_cid);
    assert_eq!(
        c2_msg_amend.parent_oid.as_deref(),
        Some(c1_amended.commit_oid.as_str())
    );

    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let c2_reuse_msg =
        commit_changes(&repo, &CommitOptions::new("").with_amend(true)).expect("reuse msg amend");
    assert_eq!(c2_reuse_msg.message, c2_msg_amend.message);
    assert_eq!(c2_reuse_msg.change_id, c2_cid);

    // 2b. Edit two distant hunks in `multi.txt` (`h2a` at line 2, `h2b` at line 19), plus `extra.txt` and `unwanted.txt`
    let mut lines: Vec<String> = (1..=20).map(|i| format!("line {i:02}")).collect();
    lines[1] = "line 02 AMENDED HUNK A".to_string();
    lines[18] = "line 19 UNCOMMITTED HUNK B".to_string();
    fs::write(repo_root.join("multi.txt"), format!("{}\n", lines.join("\n"))).unwrap();
    fs::write(repo_root.join("extra.txt"), "extra file included in amend\n").unwrap();
    fs::write(repo_root.join("unwanted.txt"), "leave uncommitted\n").unwrap();

    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let st = compute_status(&repo).unwrap();
    let multi_change = st.find_by_target("multi.txt").expect("multi.txt dirty");
    assert_eq!(multi_change.hunks.len(), 2);
    let first_hunk_id = multi_change.hunks[0].id.clone();

    // Surgically amend `HEAD` picking ONLY `first_hunk_id` + `extra.txt`, AND SSH-sign the amended commit!
    let c2_surgical = commit_changes(
        &repo,
        &CommitOptions::new("feat: surgical hunk + file signed amend")
            .with_amend(true)
            .with_pick([first_hunk_id.as_str(), "extra.txt"])
            .with_sign(true)
            .with_signing_key(priv_path.to_string_lossy().to_string()),
    )
    .expect("surgical signed amend");

    assert!(c2_surgical.amended);
    assert!(c2_surgical.signed);
    assert_eq!(c2_surgical.change_id, c2_cid);
    assert_eq!(
        c2_surgical.parent_oid.as_deref(),
        Some(c1_amended.commit_oid.as_str())
    );
    assert_eq!(c2_surgical.remaining_dirty_count, 2);
    assert_eq!(git(&repo_root, &["rev-list", "--count", "HEAD"]).trim(), "2");

    // Verify HEAD tree contains hunk A + extra.txt + a.rs, NOT hunk B or unwanted.txt
    let committed_multi = git(&repo_root, &["show", "HEAD:multi.txt"]);
    assert!(committed_multi.contains("AMENDED HUNK A"));
    assert!(!committed_multi.contains("UNCOMMITTED HUNK B"));
    assert_eq!(
        git(&repo_root, &["show", "HEAD:extra.txt"]),
        "extra file included in amend\n"
    );

    // Working tree still has hunk B and unwanted.txt untouched!
    let disk_multi = fs::read_to_string(repo_root.join("multi.txt")).unwrap();
    assert!(disk_multi.contains("UNCOMMITTED HUNK B"));
    assert!(repo_root.join("unwanted.txt").is_file());

    // Verify both `git verify-commit` and `verify_commit_signature` pass on the amended commit!
    git(&repo_root, &["verify-commit", &c2_surgical.commit_oid]);
    let repo = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let ver = verify_commit_signature(&repo, &c2_surgical.commit_oid).unwrap();
    assert!(ver.valid);

    // 2c. Amend with `--exclude` (commit remaining `multi.txt` hunk while excluding `unwanted.txt`)
    // AND verify explicit `Change-Id:` trailer overrides HEAD's Change-Id when explicitly provided!
    let explicit_cid = "I0123456789abcdef0123456789abcdef01234567";
    let c2_excl = commit_changes(
        &repo,
        &CommitOptions::new(format!(
            "feat: complete multi.txt amend\n\nChange-Id: {explicit_cid}"
        ))
        .with_amend(true)
        .with_exclude(["unwanted.txt"]),
    )
    .expect("amend with exclude and explicit Change-Id");
    assert_eq!(c2_excl.change_id, explicit_cid);
    assert_eq!(c2_excl.remaining_dirty_count, 1);
    let committed_multi_full = git(&repo_root, &["show", "HEAD:multi.txt"]);
    assert!(committed_multi_full.contains("AMENDED HUNK A"));
    assert!(committed_multi_full.contains("UNCOMMITTED HUNK B"));
    fs::remove_file(repo_root.join("unwanted.txt")).unwrap();

    // 3. Amend a 2-parent merge commit and verify both parents are preserved!
    git(&repo_root, &["checkout", "-b", "side", &c1_amended.commit_oid]);
    fs::write(repo_root.join("side.rs"), "pub fn side() {}\n").unwrap();
    git(&repo_root, &["add", "side.rs"]);
    git(&repo_root, &["commit", "-m", "feat: side branch"]);
    let side_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    git(&repo_root, &["checkout", "main"]);
    let main_before_merge = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();
    git(&repo_root, &["merge", "--no-ff", "side", "-m", "Merge branch 'side'"]);

    let repo_merge = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let amended_merge = commit_changes(
        &repo_merge,
        &CommitOptions::new("Merge branch 'side' (signed amend)")
            .with_amend(true)
            .with_sign(true)
            .with_signing_key(priv_path.to_string_lossy().to_string()),
    )
    .expect("amend merge commit");
    assert!(amended_merge.amended);
    assert!(amended_merge.signed);

    let parents_line = git(&repo_root, &["log", "-1", "--format=%P", "HEAD"]);
    let parents: Vec<&str> = parents_line.split_whitespace().collect();
    assert_eq!(parents, vec![main_before_merge.as_str(), side_oid.as_str()]);
    git(&repo_root, &["verify-commit", &amended_merge.commit_oid]);

    // Verify file deletion during `--amend` keeps `.git/index` and `git status --porcelain` 100% clean
    fs::remove_file(repo_root.join("extra.txt")).unwrap();
    let repo_del = OzoneRepo::discover(Some(&repo_root)).unwrap();
    let del_amend = commit_changes(
        &repo_del,
        &CommitOptions::new("Merge branch 'side' (drop extra.txt)")
            .with_amend(true)
            .with_sign(true)
            .with_signing_key(priv_path.to_string_lossy().to_string()),
    )
    .expect("amend removing tracked file");
    assert_eq!(del_amend.committed_paths, vec!["extra.txt"]);
    // Untracked `.ozone` isn't present; git status --porcelain must be completely empty
    assert_eq!(git(&repo_root, &["status", "--porcelain"]).trim(), "");
    git(&repo_root, &["verify-commit", &del_amend.commit_oid]);

    // Verify `~/` tilde path expansion and `key::ssh-ed25519 ...` inline public key handling
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        assert_eq!(
            oz_core::expand_tilde_path("~/.ssh/custom_ed25519"),
            PathBuf::from(home).join(".ssh/custom_ed25519")
        );
    }
    let inline_key_spec = format!("key::{}", fs::read_to_string(&pub_path).unwrap().trim());
    let parsed_inline = OpenSshPublicKey::parse_openssh(&inline_key_spec).expect("parse inline key::");
    assert!(parsed_inline.fingerprint_sha256().starts_with("SHA256:"));

    // 4. Legacy M1/M2 JSON deserialization backwards-compatibility check for `CommitOutcome`
    let legacy_json = r#"{
        "commit_oid": "0123456789abcdef0123456789abcdef01234567",
        "tree_oid": "abcdef0123456789abcdef0123456789abcdef01",
        "parent_oid": null,
        "branch": "main",
        "change_id": "I1111111111111111111111111111111111111111",
        "message": "legacy\n",
        "committed_paths": ["a.rs"],
        "remaining_dirty_count": 0
    }"#;
    let legacy: CommitOutcome = serde_json::from_str(legacy_json).expect("deserialize M1/M2 outcome");
    assert!(!legacy.signed);
    assert!(legacy.signature_fingerprint.is_none());
    assert!(!legacy.amended);
}
