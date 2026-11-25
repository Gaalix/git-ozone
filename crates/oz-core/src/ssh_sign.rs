// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust constant-time Ed25519 (RFC 8032 via `ed25519-dalek`), SHA-512 / SHA-256
//! (FIPS 180-4 via `sha2`), Base64 (RFC 4648), OpenSSH key parsing (`openssh-key-v1` &
//! `ssh-ed25519`), OpenSSH `SSHSIG` v1 signing and verification (`namespace = "git"`),
//! and SSH signing readiness diagnostics.
//!
//! Zero C or OpenSSL dependencies are used anywhere in this module.

use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::open_gix_for_workspace;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use gix::bstr::ByteSlice;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// OpenSSH signature armor begin marker.
pub const SSHSIG_ARMOR_HEADER: &str = "-----BEGIN SSH SIGNATURE-----";
/// OpenSSH signature armor end marker.
pub const SSHSIG_ARMOR_FOOTER: &str = "-----END SSH SIGNATURE-----";
/// Standard OpenSSH `SSHSIG` namespace used by Git commit and tag signatures.
pub const GIT_SSHSIG_NAMESPACE: &str = "git";

const OPENSSH_KEY_V1_MAGIC: &[u8] = b"openssh-key-v1\0";
const PRIVKEY_ARMOR_HEADER: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const PRIVKEY_ARMOR_FOOTER: &str = "-----END OPENSSH PRIVATE KEY-----";
const SSHSIG_MAGIC_PREAMBLE: &[u8; 6] = b"SSHSIG";
const SSHSIG_VERSION_V1: u32 = 1;
const SSHSIG_WRAP_COLUMNS: usize = 70;
const ALG_ED25519: &str = "ssh-ed25519";

// ============================================================================
// FIPS 180-4 SHA-256 & SHA-512 (via audited `sha2`)
// ============================================================================

/// Computes the FIPS 180-4 SHA-256 digest of `data`.
#[must_use]
fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Computes the FIPS 180-4 SHA-512 digest of `data`.
#[must_use]
fn sha512(data: &[u8]) -> [u8; 64] {
    Sha512::digest(data).into()
}

// ============================================================================
// RFC 4648 Base64 Encode & Decode
// ============================================================================

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `data` as standard RFC 4648 Base64 with `=` padding.
#[must_use]
fn base64_encode(data: &[u8]) -> String {
    base64_encode_inner(data, true)
}

/// Encodes `data` as standard RFC 4648 Base64 without trailing `=` padding
/// (used for OpenSSH `SHA256:<fingerprint>` strings).
#[must_use]
fn base64_encode_no_pad(data: &[u8]) -> String {
    base64_encode_inner(data, false)
}

fn base64_encode_inner(data: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let (triples, rem) = data.as_chunks::<3>();
    for c in triples {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
    }
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            if pad {
                out.push('=');
                out.push('=');
            }
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            if pad {
                out.push('=');
            }
        }
        _ => {}
    }
    out
}

/// Decodes standard Base64 input, ignoring ASCII whitespace (`' '`, `'\t'`, `'\r'`, `'\n'`).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let clean = Zeroizing::new(
        input
            .bytes()
            .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            .collect::<Vec<u8>>(),
    );

    if clean.is_empty() {
        return Ok(Vec::new());
    }
    if !clean.len().is_multiple_of(4) {
        return Err(OzError::InvalidUsage {
            message: "invalid base64 payload length (not a multiple of 4)".to_string(),
            recovery: Vec::new(),
        });
    }

    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    for (chunk_idx, quad) in clean.as_chunks::<4>().0.iter().enumerate() {
        let is_last = (chunk_idx + 1) * 4 == clean.len();
        let pad_count = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if pad_count > 2 || (pad_count > 0 && !is_last) {
            out.zeroize();
            return Err(OzError::InvalidUsage {
                message: "invalid base64 padding".to_string(),
                recovery: Vec::new(),
            });
        }
        if quad[..4 - pad_count].contains(&b'=') {
            out.zeroize();
            return Err(OzError::InvalidUsage {
                message: "unexpected '=' inside base64 quantum".to_string(),
                recovery: Vec::new(),
            });
        }

        let decode_sextet = |b: u8| -> Result<u32> {
            match b {
                b'A'..=b'Z' => Ok((b - b'A') as u32),
                b'a'..=b'z' => Ok((b - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((b - b'0' + 52) as u32),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err(OzError::InvalidUsage {
                    message: format!("invalid base64 character 0x{b:02x}"),
                    recovery: Vec::new(),
                }),
            }
        };

        let s0 = match decode_sextet(quad[0]) {
            Ok(v) => v,
            Err(e) => {
                out.zeroize();
                return Err(e);
            }
        };
        let s1 = match decode_sextet(quad[1]) {
            Ok(v) => v,
            Err(e) => {
                out.zeroize();
                return Err(e);
            }
        };
        let s2 = if pad_count >= 2 {
            0
        } else {
            match decode_sextet(quad[2]) {
                Ok(v) => v,
                Err(e) => {
                    out.zeroize();
                    return Err(e);
                }
            }
        };
        let s3 = if pad_count >= 1 {
            0
        } else {
            match decode_sextet(quad[3]) {
                Ok(v) => v,
                Err(e) => {
                    out.zeroize();
                    return Err(e);
                }
            }
        };

        let triple = (s0 << 18) | (s1 << 12) | (s2 << 6) | s3;
        out.push(((triple >> 16) & 0xff) as u8);
        if pad_count < 2 {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if pad_count == 0 {
            out.push((triple & 0xff) as u8);
        }
    }

    Ok(out)
}

/// Decodes standard Base64 input into a [`Zeroizing`] buffer for secret key material.
fn base64_decode_zeroizing(input: &str) -> Result<Zeroizing<Vec<u8>>> {
    base64_decode(input).map(Zeroizing::new)
}

// ============================================================================
// Constant-Time RFC 8032 Ed25519 (via audited `ed25519-dalek` + `zeroize`)
// ============================================================================

/// Checks that the 255-bit little-endian y-coordinate is strictly canonical (`< 2^255 - 19`).
fn is_canonical_ed25519_y(bytes: &[u8; 32]) -> bool {
    let top = bytes[31] & 0x7f;
    if top < 0x7f {
        return true;
    }
    if bytes[1..31].iter().any(|&b| b != 0xff) {
        return true;
    }
    bytes[0] < 0xed
}

/// Pure-Rust constant-time RFC 8032 Ed25519 signing key backed by `ed25519-dalek`.
///
/// Secret key material is zeroized automatically on [`Drop`].
pub struct Ed25519SigningKey {
    inner: SigningKey,
}

impl ZeroizeOnDrop for Ed25519SigningKey {}

impl Drop for Ed25519SigningKey {
    fn drop(&mut self) {
        // `ed25519_dalek::SigningKey` (compiled with the `zeroize` feature) zeroes its
        // secret key material inside its own `Drop` implementation immediately after
        // this destructor returns.
    }
}

impl std::fmt::Debug for Ed25519SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519SigningKey")
            .field("public_key", &self.verifying_key_bytes())
            .finish_non_exhaustive()
    }
}

impl Ed25519SigningKey {
    /// Derives an Ed25519 keypair from a 32-byte secret seed per RFC 8032 §5.1.5.
    #[must_use]
    pub fn from_seed(mut seed: [u8; 32]) -> Self {
        let inner = SigningKey::from_bytes(&seed);
        seed.zeroize();
        Self { inner }
    }

    /// Derives an Ed25519 keypair from a borrowed 32-byte seed without creating an
    /// unzeroized stack copy.
    #[must_use]
    fn from_seed_ref(seed: &[u8; 32]) -> Self {
        Self {
            inner: SigningKey::from_bytes(seed),
        }
    }

    /// Returns a zeroizing copy of the 32-byte secret seed for internal OpenSSH PEM encoding.
    #[must_use]
    fn seed_zeroizing(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.inner.to_bytes())
    }

    /// Returns the 32-byte compressed Ed25519 public key.
    #[must_use]
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.inner.verifying_key().to_bytes()
    }

    /// Returns the corresponding [`Ed25519VerifyingKey`].
    #[must_use]
    pub fn verifying_key(&self) -> Ed25519VerifyingKey {
        Ed25519VerifyingKey {
            inner: self.inner.verifying_key(),
        }
    }

    /// Deterministically signs `message` per RFC 8032 §5.1.6, returning a 64-byte signature.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.inner.sign(message).to_bytes()
    }
}

/// Pure-Rust RFC 8032 Ed25519 public verifying key backed by `ed25519-dalek`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ed25519VerifyingKey {
    inner: VerifyingKey,
}

impl Ed25519VerifyingKey {
    /// Parses and validates a canonical 32-byte Ed25519 public key.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        if !is_canonical_ed25519_y(&bytes) {
            return Err(OzError::InvalidUsage {
                message: "invalid Ed25519 public key (non-canonical field element)".to_string(),
                recovery: Vec::new(),
            });
        }
        let inner = VerifyingKey::from_bytes(&bytes).map_err(|_| OzError::InvalidUsage {
            message: "invalid Ed25519 public key (point decompression failed)".to_string(),
            recovery: Vec::new(),
        })?;
        Ok(Self { inner })
    }

    /// Returns the 32-byte compressed public key encoding.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.inner.as_bytes()
    }

    /// Verifies a 64-byte Ed25519 `signature` over `message` per RFC 8032 §5.1.7 using
    /// strict verification (rejecting non-canonical `S >= L`, non-canonical `R`, and
    /// small-order public keys/commitments).
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<()> {
        let sig = Signature::from_bytes(signature);
        self.inner
            .verify_strict(message, &sig)
            .map_err(|err| OzError::CheckFailed {
                message: format!("Ed25519 signature verification failed: {err}"),
                recovery: Vec::new(),
            })
    }
}

// ============================================================================
// OpenSSH Wire Helpers, Public/Private Key Parsing & Fingerprints
// ============================================================================

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

struct SshReader<'a> {
    buf: &'a [u8],
}

impl<'a> SshReader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn read_exact(&mut self, len: usize, context: &str) -> Result<&'a [u8]> {
        if self.buf.len() < len {
            return Err(OzError::InvalidUsage {
                message: format!("truncated OpenSSH {context} (needed {len} bytes)"),
                recovery: Vec::new(),
            });
        }
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(head)
    }

    fn read_u32(&mut self, context: &str) -> Result<u32> {
        let b = self.read_exact(4, context)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_ssh_string(&mut self, context: &str) -> Result<&'a [u8]> {
        let len = self.read_u32(context)? as usize;
        self.read_exact(len, context)
    }
}

fn unsupported_algorithm_error(alg: &str) -> OzError {
    OzError::InvalidUsage {
        message: format!(
            "unsupported SSH key algorithm '{alg}'; pure-Rust signing supports only 'ssh-ed25519'"
        ),
        recovery: vec![
            "ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519".to_string(),
            "git config gpg.format ssh && git config user.signingkey ~/.ssh/id_ed25519".to_string(),
        ],
    }
}

/// Parsed OpenSSH `ssh-ed25519` public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSshPublicKey {
    verifying_key: Ed25519VerifyingKey,
    comment: Option<String>,
}

impl OpenSshPublicKey {
    /// Creates an [`OpenSshPublicKey`] from an [`Ed25519VerifyingKey`] and optional comment.
    #[must_use]
    pub fn new(verifying_key: Ed25519VerifyingKey, comment: Option<String>) -> Self {
        let comment = comment.filter(|c| !c.is_empty());
        Self {
            verifying_key,
            comment,
        }
    }

    /// Parses an OpenSSH public key line (`ssh-ed25519 AAAA... [comment]`) or Git inline
    /// `key::ssh-ed25519 AAAA... [comment]` syntax.
    pub fn parse_openssh(line: &str) -> Result<Self> {
        let trimmed = line.trim();
        let stripped = trimmed.strip_prefix("key::").unwrap_or(trimmed).trim();

        let mut parts = stripped.split_whitespace();
        let Some(alg) = parts.next() else {
            return Err(OzError::InvalidUsage {
                message: "empty OpenSSH public key".to_string(),
                recovery: Vec::new(),
            });
        };
        if alg != ALG_ED25519 {
            return Err(unsupported_algorithm_error(alg));
        }
        let Some(b64_blob) = parts.next() else {
            return Err(OzError::InvalidUsage {
                message: "missing base64 payload in OpenSSH public key".to_string(),
                recovery: Vec::new(),
            });
        };
        let comment_parts: Vec<&str> = parts.collect();
        let comment = (!comment_parts.is_empty()).then(|| comment_parts.join(" "));

        let blob = base64_decode(b64_blob)?;
        let verifying_key = Self::decode_wire_blob(&blob)?;
        Ok(Self::new(verifying_key, comment))
    }

    /// Parses a binary SSH public key wire blob (`string("ssh-ed25519") || string(pubkey_32)`).
    pub fn from_wire_blob(blob: &[u8]) -> Result<Self> {
        let vk = Self::decode_wire_blob(blob)?;
        Ok(Self::new(vk, None))
    }

    fn decode_wire_blob(blob: &[u8]) -> Result<Ed25519VerifyingKey> {
        let mut reader = SshReader::new(blob);
        let key_type = reader.read_ssh_string("public key type")?;
        let key_type_str = String::from_utf8_lossy(key_type);
        if key_type != ALG_ED25519.as_bytes() {
            return Err(unsupported_algorithm_error(&key_type_str));
        }
        let raw_pk = reader.read_ssh_string("ed25519 public key bytes")?;
        if !reader.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "unexpected trailing bytes in OpenSSH ed25519 public key blob".to_string(),
                recovery: Vec::new(),
            });
        }
        let pk_arr: [u8; 32] = raw_pk.try_into().map_err(|_| OzError::InvalidUsage {
            message: format!(
                "invalid OpenSSH ed25519 public key length {} (expected 32)",
                raw_pk.len()
            ),
            recovery: Vec::new(),
        })?;
        Ed25519VerifyingKey::from_bytes(pk_arr)
    }

    /// Serializes the binary OpenSSH wire public key blob (`"ssh-ed25519"` + 32-byte key).
    #[must_use]
    pub fn to_wire_blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(51);
        put_ssh_string(&mut out, ALG_ED25519.as_bytes());
        put_ssh_string(&mut out, self.verifying_key.as_bytes());
        out
    }

    /// Computes the canonical OpenSSH `SHA256:<base64-no-pad>` public key fingerprint.
    #[must_use]
    pub fn fingerprint_sha256(&self) -> String {
        let digest = sha256(&self.to_wire_blob());
        format!("SHA256:{}", base64_encode_no_pad(&digest))
    }

    /// Formats as an OpenSSH `.pub` line (`ssh-ed25519 AAAA... [comment]`).
    #[must_use]
    pub fn to_openssh_line(&self) -> String {
        let b64 = base64_encode(&self.to_wire_blob());
        match self.comment {
            Some(ref c) if !c.is_empty() => format!("{ALG_ED25519} {b64} {c}"),
            _ => format!("{ALG_ED25519} {b64}"),
        }
    }

    /// Returns the underlying [`Ed25519VerifyingKey`].
    #[must_use]
    pub const fn verifying_key(&self) -> &Ed25519VerifyingKey {
        &self.verifying_key
    }

    /// Returns the optional key comment.
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// Parsed unencrypted OpenSSH `openssh-key-v1` Ed25519 private key.
///
/// Secret key material is zeroized automatically when dropped.
#[derive(Debug)]
pub struct OpenSshPrivateKey {
    signing_key: Ed25519SigningKey,
    comment: Option<String>,
}

impl ZeroizeOnDrop for OpenSshPrivateKey {}

impl OpenSshPrivateKey {
    /// Wraps an [`Ed25519SigningKey`] with an optional OpenSSH key comment.
    #[must_use]
    pub fn from_signing_key(signing_key: Ed25519SigningKey, comment: Option<String>) -> Self {
        Self {
            signing_key,
            comment: comment.filter(|c| !c.is_empty()),
        }
    }

    /// Parses an OpenSSH `-----BEGIN OPENSSH PRIVATE KEY-----` PEM string.
    ///
    /// Returns actionable [`OzError`] diagnostics when a key is passphrase-encrypted
    /// (`cipher != "none"` / `kdf != "none"`) or uses a non-Ed25519 algorithm.
    pub fn parse_openssh(pem_text: &str) -> Result<Self> {
        let trimmed = pem_text.trim();
        if trimmed.contains("ENCRYPTED") && !trimmed.contains(PRIVKEY_ARMOR_HEADER) {
            return Err(OzError::InvalidUsage {
                message: "SSH private key is encrypted; pure-Rust commit signing requires an unencrypted OpenSSH Ed25519 key".to_string(),
                recovery: vec![
                    "ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519_oz_signing".to_string(),
                    "oz commit --signing-key ~/.ssh/id_ed25519_oz_signing -m \"...\"".to_string(),
                ],
            });
        }
        if trimmed.contains("BEGIN RSA PRIVATE KEY") {
            return Err(unsupported_algorithm_error("ssh-rsa"));
        }
        if trimmed.contains("BEGIN EC PRIVATE KEY") {
            return Err(unsupported_algorithm_error("ecdsa"));
        }

        let body_b64 = Zeroizing::new(extract_armor_body(
            trimmed,
            PRIVKEY_ARMOR_HEADER,
            PRIVKEY_ARMOR_FOOTER,
            "OpenSSH private key",
        )?);
        let raw = base64_decode_zeroizing(&body_b64)?;
        let mut reader = SshReader::new(&raw);

        let magic = reader.read_exact(OPENSSH_KEY_V1_MAGIC.len(), "openssh-key-v1 magic")?;
        if magic != OPENSSH_KEY_V1_MAGIC {
            return Err(OzError::InvalidUsage {
                message: "unrecognized OpenSSH private key magic (expected 'openssh-key-v1')"
                    .to_string(),
                recovery: Vec::new(),
            });
        }

        let ciphername =
            String::from_utf8_lossy(reader.read_ssh_string("ciphername")?).into_owned();
        let kdfname = String::from_utf8_lossy(reader.read_ssh_string("kdfname")?).into_owned();
        let _kdfoptions = reader.read_ssh_string("kdfoptions")?;
        let num_keys = reader.read_u32("number of keys")?;

        if num_keys == 0 {
            return Err(OzError::InvalidUsage {
                message: "OpenSSH private key contains 0 keys".to_string(),
                recovery: Vec::new(),
            });
        }

        let mut first_pub_blob: Option<&[u8]> = None;
        for idx in 0..num_keys {
            let pub_blob = reader.read_ssh_string("public key entry")?;
            if idx == 0 {
                first_pub_blob = Some(pub_blob);
            }
        }

        // Check public key algorithm even BEFORE cipher check if possible, or check cipher first
        // so encrypted RSA keys report encryption/algorithm clearly.
        if let Some(pub_blob) = first_pub_blob {
            let mut pub_reader = SshReader::new(pub_blob);
            if let Ok(kt) = pub_reader.read_ssh_string("public key type")
                && kt != ALG_ED25519.as_bytes()
            {
                return Err(unsupported_algorithm_error(&String::from_utf8_lossy(kt)));
            }
        }

        if ciphername != "none" || kdfname != "none" {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "OpenSSH private key is passphrase-encrypted (cipher='{ciphername}', kdf='{kdfname}'); pure-Rust signing requires an unencrypted Ed25519 key"
                ),
                recovery: vec![
                    "ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519_oz_signing".to_string(),
                    "git config user.signingkey ~/.ssh/id_ed25519_oz_signing".to_string(),
                ],
            });
        }

        let priv_section = reader.read_ssh_string("private key section")?;
        let mut priv_reader = SshReader::new(priv_section);
        let check1 = priv_reader.read_u32("checkint1")?;
        let check2 = priv_reader.read_u32("checkint2")?;
        if check1 != check2 {
            return Err(OzError::InvalidUsage {
                message: "corrupt OpenSSH private key (checkint mismatch)".to_string(),
                recovery: Vec::new(),
            });
        }

        let key_type = priv_reader.read_ssh_string("private key type")?;
        if key_type != ALG_ED25519.as_bytes() {
            return Err(unsupported_algorithm_error(&String::from_utf8_lossy(
                key_type,
            )));
        }

        let pub_bytes = priv_reader.read_ssh_string("ed25519 public key")?;
        let priv_bytes = priv_reader.read_ssh_string("ed25519 private key")?;
        let comment_bytes = priv_reader.read_ssh_string("key comment")?;

        if pub_bytes.len() != 32 || priv_bytes.len() != 64 {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "invalid OpenSSH ed25519 key lengths (pub={}, priv={})",
                    pub_bytes.len(),
                    priv_bytes.len()
                ),
                recovery: Vec::new(),
            });
        }
        if &priv_bytes[32..64] != pub_bytes {
            return Err(OzError::InvalidUsage {
                message: "OpenSSH ed25519 private key public-half mismatch".to_string(),
                recovery: Vec::new(),
            });
        }

        let mut seed = Zeroizing::new([0u8; 32]);
        seed.copy_from_slice(&priv_bytes[..32]);
        let signing_key = Ed25519SigningKey::from_seed_ref(&seed);
        if signing_key.verifying_key_bytes().as_slice() != pub_bytes {
            return Err(OzError::InvalidUsage {
                message: "derived Ed25519 public key does not match stored public key".to_string(),
                recovery: Vec::new(),
            });
        }

        let comment_str = String::from_utf8_lossy(comment_bytes).into_owned();
        let comment = (!comment_str.is_empty()).then_some(comment_str);

        Ok(Self {
            signing_key,
            comment,
        })
    }

    /// Loads and parses an OpenSSH Ed25519 private key from disk.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw =
            Zeroizing::new(
                fs::read_to_string(path).map_err(|err| OzError::InvalidUsage {
                    message: format!(
                        "failed to read SSH signing key at {}: {err}",
                        path.display()
                    ),
                    recovery: vec![
                        "ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519".to_string(),
                        format!(
                            "Verify signing key permissions and path: {}",
                            path.display()
                        ),
                    ],
                })?,
            );
        Self::parse_openssh(&raw)
    }

    /// Encodes this unencrypted Ed25519 private key in standard `openssh-key-v1` PEM format.
    #[must_use]
    pub fn to_openssh_pem(&self) -> String {
        let pub_key = self.public_key();
        let pub_blob = pub_key.to_wire_blob();
        let pub_bytes = self.signing_key.verifying_key_bytes();

        let mut priv_sec = Zeroizing::new(Vec::with_capacity(160));
        let checkint: u32 = 0x5a5a_a5a5;
        put_u32(&mut priv_sec, checkint);
        put_u32(&mut priv_sec, checkint);
        put_ssh_string(&mut priv_sec, ALG_ED25519.as_bytes());
        put_ssh_string(&mut priv_sec, &pub_bytes);

        let seed = self.signing_key.seed_zeroizing();
        let mut seed_and_pub = Zeroizing::new([0u8; 64]);
        seed_and_pub[..32].copy_from_slice(&*seed);
        seed_and_pub[32..64].copy_from_slice(&pub_bytes);
        put_ssh_string(&mut priv_sec, &*seed_and_pub);

        let comment_bytes = self.comment.as_deref().unwrap_or("").as_bytes();
        put_ssh_string(&mut priv_sec, comment_bytes);

        let pad_len = (8 - (priv_sec.len() % 8)) % 8;
        for i in 1..=pad_len {
            priv_sec.push(i as u8);
        }

        let mut outer = Zeroizing::new(Vec::with_capacity(256));
        outer.extend_from_slice(OPENSSH_KEY_V1_MAGIC);
        put_ssh_string(&mut outer, b"none");
        put_ssh_string(&mut outer, b"none");
        put_ssh_string(&mut outer, b"");
        put_u32(&mut outer, 1);
        put_ssh_string(&mut outer, &pub_blob);
        put_ssh_string(&mut outer, &priv_sec);

        let outer_b64 = Zeroizing::new(base64_encode(&outer));
        wrap_armor(PRIVKEY_ARMOR_HEADER, PRIVKEY_ARMOR_FOOTER, &outer_b64, 70)
    }

    /// Returns the corresponding [`OpenSshPublicKey`].
    #[must_use]
    pub fn public_key(&self) -> OpenSshPublicKey {
        OpenSshPublicKey::new(self.signing_key.verifying_key(), self.comment.clone())
    }

    /// Returns the underlying [`Ed25519SigningKey`].
    #[must_use]
    pub const fn signing_key(&self) -> &Ed25519SigningKey {
        &self.signing_key
    }

    /// Returns the optional key comment.
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Produces an OpenSSH `SSHSIG` v1 armored signature over `message` in `namespace`.
    pub fn sign_sshsig(&self, message: &[u8], namespace: &str) -> Result<String> {
        sign_sshsig(&self.signing_key, message, namespace)
    }
}

// ============================================================================
// OpenSSH SSHSIG v1 Signing & Verification
// ============================================================================

/// Parsed and verified OpenSSH `SSHSIG` v1 signature metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSshSignature {
    pub version: u32,
    pub public_key: OpenSshPublicKey,
    pub namespace: String,
    pub reserved: Vec<u8>,
    pub hash_algorithm: String,
    pub signature_bytes: [u8; 64],
}

fn build_sshsig_signed_data(
    namespace: &str,
    reserved: &[u8],
    hash_algorithm: &str,
    message_digest: &[u8],
) -> Vec<u8> {
    let mut signed_data = Vec::with_capacity(
        6 + 4
            + namespace.len()
            + 4
            + reserved.len()
            + 4
            + hash_algorithm.len()
            + 4
            + message_digest.len(),
    );
    signed_data.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
    put_ssh_string(&mut signed_data, namespace.as_bytes());
    put_ssh_string(&mut signed_data, reserved);
    put_ssh_string(&mut signed_data, hash_algorithm.as_bytes());
    put_ssh_string(&mut signed_data, message_digest);
    signed_data
}

/// Produces an OpenSSH `SSHSIG` v1 armored signature (`-----BEGIN SSH SIGNATURE-----`,
/// 70-column Base64 wrapping) over `message` for `namespace` (e.g. `"git"`).
pub fn sign_sshsig(
    signing_key: &Ed25519SigningKey,
    message: &[u8],
    namespace: &str,
) -> Result<String> {
    if namespace.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "SSHSIG namespace must not be empty".to_string(),
            recovery: Vec::new(),
        });
    }

    let hash_alg = "sha512";
    let msg_hash = sha512(message);
    let signed_data = build_sshsig_signed_data(namespace, b"", hash_alg, &msg_hash);
    let ed_sig = signing_key.sign(&signed_data);

    let pubkey_blob = OpenSshPublicKey::new(signing_key.verifying_key(), None).to_wire_blob();

    let mut sig_blob = Vec::with_capacity(4 + ALG_ED25519.len() + 4 + 64);
    put_ssh_string(&mut sig_blob, ALG_ED25519.as_bytes());
    put_ssh_string(&mut sig_blob, &ed_sig);

    let mut raw = Vec::with_capacity(180);
    raw.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
    put_u32(&mut raw, SSHSIG_VERSION_V1);
    put_ssh_string(&mut raw, &pubkey_blob);
    put_ssh_string(&mut raw, namespace.as_bytes());
    put_ssh_string(&mut raw, b"");
    put_ssh_string(&mut raw, hash_alg.as_bytes());
    put_ssh_string(&mut raw, &sig_blob);

    let b64 = base64_encode(&raw);
    Ok(wrap_armor(
        SSHSIG_ARMOR_HEADER,
        SSHSIG_ARMOR_FOOTER,
        &b64,
        SSHSIG_WRAP_COLUMNS,
    ))
}

/// Parses and cryptographically verifies an OpenSSH `SSHSIG` v1 armored signature
/// over `message` in `expected_namespace`.
pub fn verify_sshsig(
    message: &[u8],
    armored_sig: &str,
    expected_namespace: &str,
) -> Result<ParsedSshSignature> {
    let b64 = extract_armor_body(
        armored_sig.trim(),
        SSHSIG_ARMOR_HEADER,
        SSHSIG_ARMOR_FOOTER,
        "SSH signature",
    )
    .map_err(|err| OzError::CheckFailed {
        message: err.to_string(),
        recovery: Vec::new(),
    })?;

    let raw = base64_decode(&b64).map_err(|err| OzError::CheckFailed {
        message: format!("invalid base64 in SSH signature: {err}"),
        recovery: Vec::new(),
    })?;

    let mut reader = SshReader::new(&raw);
    let magic = reader
        .read_exact(SSHSIG_MAGIC_PREAMBLE.len(), "SSHSIG magic")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    if magic != SSHSIG_MAGIC_PREAMBLE {
        return Err(OzError::CheckFailed {
            message: "invalid SSH signature magic preamble (expected 'SSHSIG')".to_string(),
            recovery: Vec::new(),
        });
    }

    let version = reader
        .read_u32("SSHSIG version")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    if version != SSHSIG_VERSION_V1 {
        return Err(OzError::CheckFailed {
            message: format!("unsupported SSHSIG version {version} (expected 1)"),
            recovery: Vec::new(),
        });
    }

    let pubkey_blob =
        reader
            .read_ssh_string("SSHSIG public key")
            .map_err(|err| OzError::CheckFailed {
                message: err.to_string(),
                recovery: Vec::new(),
            })?;
    let public_key =
        OpenSshPublicKey::from_wire_blob(pubkey_blob).map_err(|err| OzError::CheckFailed {
            message: format!("invalid public key in SSH signature: {err}"),
            recovery: Vec::new(),
        })?;

    let namespace_bytes =
        reader
            .read_ssh_string("SSHSIG namespace")
            .map_err(|err| OzError::CheckFailed {
                message: err.to_string(),
                recovery: Vec::new(),
            })?;
    let namespace = std::str::from_utf8(namespace_bytes)
        .map_err(|_| OzError::CheckFailed {
            message: "non-UTF-8 namespace in SSH signature".to_string(),
            recovery: Vec::new(),
        })?
        .to_string();
    if namespace.is_empty() || namespace != expected_namespace {
        return Err(OzError::CheckFailed {
            message: format!(
                "SSH signature namespace mismatch: got '{namespace}', expected '{expected_namespace}'"
            ),
            recovery: Vec::new(),
        });
    }

    let reserved = reader
        .read_ssh_string("SSHSIG reserved")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?
        .to_vec();

    let hash_alg_bytes = reader
        .read_ssh_string("SSHSIG hash algorithm")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    let hash_algorithm = std::str::from_utf8(hash_alg_bytes)
        .map_err(|_| OzError::CheckFailed {
            message: "non-UTF-8 hash algorithm in SSH signature".to_string(),
            recovery: Vec::new(),
        })?
        .to_string();

    let sig_blob = reader
        .read_ssh_string("SSHSIG signature blob")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    if !reader.is_empty() {
        return Err(OzError::CheckFailed {
            message: "trailing bytes after SSHSIG signature payload".to_string(),
            recovery: Vec::new(),
        });
    }

    let mut sig_reader = SshReader::new(sig_blob);
    let sig_alg = sig_reader
        .read_ssh_string("signature algorithm")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    if sig_alg != ALG_ED25519.as_bytes() {
        return Err(OzError::CheckFailed {
            message: format!(
                "unsupported signature algorithm '{}'",
                String::from_utf8_lossy(sig_alg)
            ),
            recovery: Vec::new(),
        });
    }
    let raw_sig = sig_reader
        .read_ssh_string("ed25519 signature bytes")
        .map_err(|err| OzError::CheckFailed {
            message: err.to_string(),
            recovery: Vec::new(),
        })?;
    if !sig_reader.is_empty() {
        return Err(OzError::CheckFailed {
            message: "trailing bytes inside SSHSIG ed25519 signature blob".to_string(),
            recovery: Vec::new(),
        });
    }
    let signature_bytes: [u8; 64] = raw_sig.try_into().map_err(|_| OzError::CheckFailed {
        message: format!(
            "invalid Ed25519 signature length {} (expected 64)",
            raw_sig.len()
        ),
        recovery: Vec::new(),
    })?;

    let msg_digest: Vec<u8> = match hash_algorithm.as_str() {
        "sha512" => sha512(message).to_vec(),
        "sha256" => sha256(message).to_vec(),
        other => {
            return Err(OzError::CheckFailed {
                message: format!("unsupported SSHSIG hash algorithm '{other}'"),
                recovery: Vec::new(),
            });
        }
    };

    let signed_data = build_sshsig_signed_data(&namespace, &reserved, &hash_algorithm, &msg_digest);
    public_key
        .verifying_key()
        .verify(&signed_data, &signature_bytes)?;

    Ok(ParsedSshSignature {
        version,
        public_key,
        namespace,
        reserved,
        hash_algorithm,
        signature_bytes,
    })
}

fn wrap_armor(header: &str, footer: &str, b64: &str, cols: usize) -> String {
    let mut out = String::with_capacity(header.len() + footer.len() + b64.len() + 32);
    out.push_str(header);
    out.push('\n');
    for chunk in b64.as_bytes().chunks(cols) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(footer);
    out
}

fn extract_armor_body(input: &str, header: &str, footer: &str, label: &str) -> Result<String> {
    let Some(start) = input.find(header) else {
        return Err(OzError::InvalidUsage {
            message: format!("missing '{header}' in {label}"),
            recovery: Vec::new(),
        });
    };
    let after_header = &input[start + header.len()..];
    let Some(end) = after_header.find(footer) else {
        return Err(OzError::InvalidUsage {
            message: format!("missing '{footer}' in {label}"),
            recovery: Vec::new(),
        });
    };
    Ok(after_header[..end].trim().to_string())
}

// ============================================================================
// Key Path Resolution, Allowed Signers & `inspect_ssh_signing_config` Doctor
// ============================================================================

/// Expands leading `~` or `~/` (and `~\` on Windows) against `HOME` / `USERPROFILE`.
#[must_use]
pub fn expand_tilde_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~"
        && let Some(home) = user_home_dir()
    {
        return home;
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        && let Some(home) = user_home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn default_ed25519_key_path() -> PathBuf {
    match user_home_dir() {
        Some(home) => home.join(".ssh").join("id_ed25519"),
        None => PathBuf::from("~/.ssh/id_ed25519"),
    }
}

/// Resolves and loads an [`OpenSshPrivateKey`] from a key specification:
/// - inline OpenSSH private key PEM (`-----BEGIN OPENSSH PRIVATE KEY-----`)
/// - inline public key (`key::ssh-ed25519 ...` or `ssh-ed25519 ...`) matched against `~/.ssh/id_ed25519`
/// - file path (with `~/` expansion and `repo_root` fallback; if pointing at `<key>.pub`,
///   automatically loads sibling `<key>` and verifies its public key matches `<key>.pub`).
pub fn resolve_signing_private_key(
    key_spec: &str,
    repo_root: Option<&Path>,
) -> Result<(OpenSshPrivateKey, Option<PathBuf>)> {
    let trimmed = key_spec.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "SSH signing key specification is empty".to_string(),
            recovery: vec!["ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519".to_string()],
        });
    }

    // Direct inline private key PEM.
    if trimmed.contains("-----BEGIN ") {
        let key = OpenSshPrivateKey::parse_openssh(trimmed)?;
        return Ok((key, None));
    }

    // Inline public key (`key::ssh-ed25519 ...` or `ssh-ed25519 AAAA...`).
    if trimmed.starts_with("key::")
        || trimmed.starts_with("ssh-")
        || trimmed.starts_with("ecdsa-")
        || trimmed.starts_with("sk-")
    {
        let wanted_pub = OpenSshPublicKey::parse_openssh(trimmed)?;
        let default_priv = default_ed25519_key_path();
        if default_priv.is_file()
            && let Ok(priv_key) = OpenSshPrivateKey::from_file(&default_priv)
            && priv_key.public_key().verifying_key() == wanted_pub.verifying_key()
        {
            return Ok((priv_key, Some(default_priv)));
        }
        return Err(OzError::InvalidUsage {
            message: format!(
                "inline SSH public key ({}) cannot sign directly without a matching unencrypted private key file",
                wanted_pub.fingerprint_sha256()
            ),
            recovery: vec![
                "Configure `[signing].key = \"~/.ssh/id_ed25519\"` in `.ozone.toml` or pass `--signing-key <private-key-path>`.".to_string(),
            ],
        });
    }

    let mut path = expand_tilde_path(trimmed);
    if !path.is_absolute()
        && !path.exists()
        && let Some(root) = repo_root
    {
        let candidate = root.join(&path);
        if candidate.exists() {
            path = candidate;
        }
    }

    if !path.is_file() {
        return Err(OzError::InvalidUsage {
            message: format!("SSH signing key file not found at {}", path.display()),
            recovery: vec![
                format!("ssh-keygen -t ed25519 -N \"\" -f {}", path.display()),
                "git config gpg.format ssh".to_string(),
            ],
        });
    }

    let content =
        Zeroizing::new(
            fs::read_to_string(&path).map_err(|err| OzError::InvalidUsage {
                message: format!(
                    "failed to read SSH signing key at {}: {err}",
                    path.display()
                ),
                recovery: Vec::new(),
            })?,
        );
    let content_trimmed = content.trim();

    // If `user.signingkey` points to `<name>.pub`, parse the `.pub` file and load sibling `<name>`.
    if content_trimmed.starts_with("ssh-")
        || content_trimmed.starts_with("ecdsa-")
        || content_trimmed.starts_with("sk-")
        || path.extension().and_then(|e| e.to_str()) == Some("pub")
            && !content_trimmed.contains("-----BEGIN ")
    {
        let wanted_pub = OpenSshPublicKey::parse_openssh(content_trimmed)?;
        let priv_candidate = path.with_extension("");
        if priv_candidate != path && priv_candidate.is_file() {
            let priv_key = OpenSshPrivateKey::from_file(&priv_candidate)?;
            if priv_key.public_key().verifying_key() != wanted_pub.verifying_key() {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "private key {} does not match public key {}",
                        priv_candidate.display(),
                        path.display()
                    ),
                    recovery: Vec::new(),
                });
            }
            return Ok((priv_key, Some(priv_candidate)));
        }
        return Err(OzError::InvalidUsage {
            message: format!(
                "'{}' is a public key file and sibling private key '{}' was not found",
                path.display(),
                priv_candidate.display()
            ),
            recovery: vec![format!(
                "Point `[signing].key` or `user.signingkey` at the unencrypted private key file '{}'.",
                priv_candidate.display()
            )],
        });
    }

    let priv_key = OpenSshPrivateKey::parse_openssh(content_trimmed)?;
    Ok((priv_key, Some(path)))
}

/// Checks an OpenSSH `allowed_signers` file for an entry matching `public_key` (and optionally
/// `signer_email` if provided), returning `(true, Some(principal))` on match.
pub fn match_allowed_signers_file(
    allowed_signers_path: &Path,
    public_key: &OpenSshPublicKey,
    signer_email: Option<&str>,
) -> Result<(bool, Option<String>)> {
    let raw = fs::read_to_string(allowed_signers_path)?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Format: `<principals> [options...] <key-type> <base64-key> [comment]`
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        let principals = tokens[0];
        // Locate the key type token (`ssh-ed25519`, `ssh-rsa`, etc.).
        let Some(kt_idx) = tokens[1..]
            .iter()
            .position(|t| {
                *t == ALG_ED25519
                    || t.starts_with("ssh-")
                    || t.starts_with("ecdsa-")
                    || t.starts_with("sk-")
            })
            .map(|p| p + 1)
        else {
            continue;
        };
        if tokens[kt_idx] != ALG_ED25519 || kt_idx + 1 >= tokens.len() {
            continue;
        }
        let key_line = format!("{} {}", tokens[kt_idx], tokens[kt_idx + 1]);
        let Ok(entry_pub) = OpenSshPublicKey::parse_openssh(&key_line) else {
            continue;
        };
        if entry_pub.verifying_key() != public_key.verifying_key() {
            continue;
        }

        // Check principal match against signer_email (supports comma-separated principals and `*` globs).
        if let Some(email) = signer_email {
            for principal in principals.split(',') {
                let p = principal.trim();
                if principal_matches(p, email) {
                    return Ok((true, Some(p.to_string())));
                }
            }
        } else {
            let first = principals.split(',').next().unwrap_or(principals).trim();
            return Ok((true, Some(first.to_string())));
        }
    }
    Ok((false, None))
}

fn principal_matches(pattern: &str, identity: &str) -> bool {
    if pattern == "*" || pattern.eq_ignore_ascii_case(identity) {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return identity
            .to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase());
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return identity
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase());
    }
    false
}

/// Structured SSH signing readiness & configuration diagnostic report (`oz doctor` / commit preflight).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSigningReport {
    /// Whether commits will be SSH-signed automatically when `CommitOptions::sign` is `None`.
    pub enabled_by_default: bool,
    /// Whether a valid, unencrypted Ed25519 signing key was resolved and verified ready.
    pub ready: bool,
    /// Source of the active signing key (`"ozone_config"`, `"git_config"`, `"default_ssh_dir"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_source: Option<String>,
    /// Raw configured signing key value before path expansion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_key: Option<String>,
    /// Expanded filesystem path to the private signing key, if file-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_key_path: Option<PathBuf>,
    /// Detected SSH key algorithm (e.g. `"ssh-ed25519"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_algorithm: Option<String>,
    /// `true` if a key file exists at the target location but is passphrase-encrypted.
    pub key_encrypted: bool,
    /// `SHA256:<base64-no-pad>` fingerprint of the resolved signing key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// OpenSSH `ssh-ed25519 AAAA...` public key line suitable for GitHub / `allowed_signers`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Effective `gpg.format` from repo/global Git config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_gpg_format: Option<String>,
    /// Effective `user.signingkey` from repo/global Git config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_user_signingkey: Option<String>,
    /// Effective `commit.gpgsign` from repo/global Git config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit_gpgsign: Option<bool>,
    /// Effective `gpg.ssh.allowedSignersFile` from repo/global Git config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_allowed_signers_file: Option<PathBuf>,
    /// Effective `.ozone.toml` / `.git/ozone/config.toml` `[signing].enabled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ozone_signing_enabled: Option<bool>,
    /// Effective `.ozone.toml` / `.git/ozone/config.toml` `[signing].key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ozone_signing_key: Option<String>,
    /// Diagnostic warnings or errors found during inspection.
    #[serde(default)]
    pub issues: Vec<String>,
    /// Actionable setup or remediation commands.
    #[serde(default)]
    pub recovery_commands: Vec<String>,
}

/// Result of verifying a commit's cryptographic SSH signature via [`crate::verify_commit_signature`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSignatureVerification {
    pub commit_oid: String,
    pub valid: bool,
    pub namespace: String,
    pub hash_algorithm: String,
    pub key_algorithm: String,
    pub fingerprint: String,
    pub public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_signer_matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_principal: Option<String>,
}

/// Inspects repository/global Git configuration (`gpg.format`, `user.signingkey`,
/// `commit.gpgsign`, `gpg.ssh.allowedSignersFile`) and `.ozone.toml` `[signing]`
/// to report SSH commit signing readiness and actionable diagnostics.
pub fn inspect_ssh_signing_config(repo: &OzoneRepo) -> Result<SshSigningReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    Ok(inspect_ssh_signing_with_gix(repo, &gix_repo))
}

pub(crate) fn inspect_ssh_signing_with_gix(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> SshSigningReport {
    let snap = gix_repo.config_snapshot();
    let git_gpg_format = snap
        .string("gpg.format")
        .map(|v| v.to_str_lossy().trim().to_string())
        .filter(|s| !s.is_empty());
    let git_user_signingkey = snap
        .string("user.signingkey")
        .map(|v| v.to_str_lossy().trim().to_string())
        .filter(|s| !s.is_empty());
    let git_commit_gpgsign = snap.boolean("commit.gpgsign");
    let git_allowed_signers_file = snap
        .string("gpg.ssh.allowedSignersFile")
        .map(|v| expand_tilde_path(&v.to_str_lossy()));

    let ozone_signing_enabled = repo.config.signing.enabled;
    let ozone_signing_key = repo.config.signing.key.clone();

    let enabled_by_default = match ozone_signing_enabled {
        Some(v) => v,
        None => {
            git_commit_gpgsign == Some(true)
                && git_gpg_format
                    .as_deref()
                    .is_some_and(|f| f.eq_ignore_ascii_case("ssh"))
        }
    };

    let mut issues = Vec::new();
    let mut recovery_commands = Vec::new();

    if ozone_signing_enabled.is_none()
        && git_commit_gpgsign == Some(true)
        && !git_gpg_format
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case("ssh"))
    {
        issues.push(format!(
            "git commit.gpgsign is true, but gpg.format is '{}' (set gpg.format=ssh or [signing].enabled=true for pure-Rust SSH signing)",
            git_gpg_format.as_deref().unwrap_or("openpgp")
        ));
        recovery_commands.push("git config gpg.format ssh".to_string());
    }

    let (key_source, configured_key) = if let Some(ref k) = ozone_signing_key {
        (Some("ozone_config".to_string()), Some(k.clone()))
    } else if let Some(ref k) = git_user_signingkey {
        (Some("git_config".to_string()), Some(k.clone()))
    } else {
        let def = default_ed25519_key_path();
        (
            Some("default_ssh_dir".to_string()),
            Some(def.to_string_lossy().into_owned()),
        )
    };

    let mut ready = false;
    let mut resolved_key_path = None;
    let mut key_algorithm = None;
    let mut key_encrypted = false;
    let mut fingerprint = None;
    let mut public_key = None;

    if let Some(ref spec) = configured_key {
        match resolve_signing_private_key(spec, Some(&repo.repo_root)) {
            Ok((priv_key, path_opt)) => {
                let pub_key = priv_key.public_key();
                ready = true;
                resolved_key_path = path_opt;
                key_algorithm = Some(ALG_ED25519.to_string());
                fingerprint = Some(pub_key.fingerprint_sha256());
                public_key = Some(pub_key.to_openssh_line());
            }
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("encrypted") {
                    key_encrypted = true;
                    key_algorithm = Some(ALG_ED25519.to_string());
                } else if let Some(rest) = msg.strip_prefix("unsupported SSH key algorithm '")
                    && let Some((alg, _)) = rest.split_once('\'')
                {
                    key_algorithm = Some(alg.to_string());
                }
                let candidate = expand_tilde_path(spec);
                if candidate.exists() {
                    resolved_key_path = Some(candidate);
                }
                issues.push(msg);
                for cmd in err.recovery_commands() {
                    if !recovery_commands.contains(&cmd) {
                        recovery_commands.push(cmd);
                    }
                }
            }
        }
    }

    SshSigningReport {
        enabled_by_default,
        ready,
        key_source,
        configured_key,
        resolved_key_path,
        key_algorithm,
        key_encrypted,
        fingerprint,
        public_key,
        git_gpg_format,
        git_user_signingkey,
        git_commit_gpgsign,
        git_allowed_signers_file,
        ozone_signing_enabled,
        ozone_signing_key,
        issues,
        recovery_commands,
    }
}

// ============================================================================
// Unit Tests (FIPS 180-4, RFC 4648, RFC 8032 Vectors, OpenSSH Keys & SSHSIG)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(hex: &str) -> Vec<u8> {
        let clean: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        assert!(clean.len().is_multiple_of(2), "hex length must be even");
        clean
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let s = std::str::from_utf8(pair).expect("ascii hex");
                u8::from_str_radix(s, 16).expect("valid hex byte")
            })
            .collect()
    }

    fn hex_encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    #[test]
    fn fips_180_4_sha256_and_sha512_test_vectors() {
        // SHA-256 NIST vectors
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex_encode(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );

        // SHA-512 NIST vectors
        assert_eq!(
            hex_encode(&sha512(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            hex_encode(&sha512(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        let multi_block = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        assert_eq!(
            hex_encode(&sha512(multi_block)),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );

        // Streaming chunked update consistency across block boundaries
        let big: Vec<u8> = (0..500usize).map(|i| (i * 37 % 251) as u8).collect();
        let mut streaming = Sha512::new();
        for chunk in big.chunks(13) {
            streaming.update(chunk);
        }
        let streaming_digest: [u8; 64] = streaming.finalize().into();
        assert_eq!(streaming_digest, sha512(&big));
    }

    #[test]
    fn rfc_4648_base64_vectors_and_validation() {
        let cases = [
            ("", "", ""),
            ("f", "Zg==", "Zg"),
            ("fo", "Zm8=", "Zm8"),
            ("foo", "Zm9v", "Zm9v"),
            ("foob", "Zm9vYg==", "Zm9vYg"),
            ("fooba", "Zm9vYmE=", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy", "Zm9vYmFy"),
        ];
        for (plain, padded, unpadded) in cases {
            assert_eq!(base64_encode(plain.as_bytes()), padded);
            assert_eq!(base64_encode_no_pad(plain.as_bytes()), unpadded);
            assert_eq!(base64_decode(padded).expect("decode"), plain.as_bytes());
        }
        assert_eq!(
            base64_decode("  Zm9v\n  YmFy \r\n").expect("whitespace decode"),
            b"foobar"
        );
        assert!(base64_decode("abc").is_err());
        assert!(base64_decode("Zm=v").is_err());
        assert!(base64_decode("Zm9$").is_err());
    }

    #[test]
    fn rfc_8032_ed25519_standard_test_vectors_and_malleability_guards() {
        // Compile-time check that secret key types implement ZeroizeOnDrop.
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<Ed25519SigningKey>();
        assert_zeroize_on_drop::<OpenSshPrivateKey>();

        // Group order L = 2^252 + 27742317777372353535851937790883648493 in 32 LE bytes.
        let l_low: u128 = 27_742_317_777_372_353_535_851_937_790_883_648_493_u128;
        let l_words: [u64; 4] = [l_low as u64, (l_low >> 64) as u64, 0, 1u64 << 60];
        let mut l_le = [0u8; 32];
        for (i, w) in l_words.iter().enumerate() {
            l_le[i * 8..(i + 1) * 8].copy_from_slice(&w.to_le_bytes());
        }

        // RFC 8032 §7.1 TEST 1 (empty message), TEST 2 (0x72), TEST 3 (0xaf82)
        let vectors: &[(&str, &str, &[u8], &str)] = &[
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                b"",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                &[0x72],
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            ),
            (
                "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
                "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
                &[0xaf, 0x82],
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
            ),
        ];

        for (seed_hex, pub_hex, msg, sig_hex) in vectors {
            let seed: [u8; 32] = hex_decode(seed_hex).try_into().unwrap();
            let expected_pub: [u8; 32] = hex_decode(pub_hex).try_into().unwrap();
            let expected_sig: [u8; 64] = hex_decode(sig_hex).try_into().unwrap();

            let sk = Ed25519SigningKey::from_seed(seed);
            assert_eq!(sk.verifying_key_bytes(), expected_pub);

            let sig = sk.sign(msg);
            assert_eq!(sig, expected_sig);

            let vk = sk.verifying_key();
            vk.verify(msg, &sig)
                .expect("RFC 8032 signature must verify");

            // Tampered message must fail
            assert!(vk.verify(b"tampered", &sig).is_err());

            // Tampered signature must fail
            let mut bad_sig = sig;
            bad_sig[63] ^= 0x01;
            assert!(vk.verify(msg, &bad_sig).is_err());
        }

        // RFC 8032 §7.1 SHA(abc) test vector
        let seed_sha: [u8; 32] =
            hex_decode("833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42")
                .try_into()
                .unwrap();
        let pub_sha: [u8; 32] =
            hex_decode("ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf")
                .try_into()
                .unwrap();
        let msg_sha = hex_decode(
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        );
        let sig_sha: [u8; 64] = hex_decode(
            "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
        )
        .try_into()
        .unwrap();
        let sk_sha = Ed25519SigningKey::from_seed(seed_sha);
        assert_eq!(sk_sha.verifying_key_bytes(), pub_sha);
        assert_eq!(sk_sha.sign(&msg_sha), sig_sha);
        sk_sha.verifying_key().verify(&msg_sha, &sig_sha).unwrap();

        // Reject non-canonical S >= L (set S = L)
        let mut non_canonical_sig = sig_sha;
        non_canonical_sig[32..64].copy_from_slice(&l_le);
        assert!(
            sk_sha
                .verifying_key()
                .verify(&msg_sha, &non_canonical_sig)
                .is_err()
        );

        // Reject non-canonical public key y = p (0x7fff...ffed)
        let mut p_bytes = [0xffu8; 32];
        p_bytes[0] = 0xed;
        p_bytes[31] = 0x7f;
        assert!(Ed25519VerifyingKey::from_bytes(p_bytes).is_err());

        // Reject weak small-order public key (identity point) during strict verification
        let mut identity_bytes = [0u8; 32];
        identity_bytes[0] = 1;
        if let Ok(weak_vk) = Ed25519VerifyingKey::from_bytes(identity_bytes) {
            let mut weak_sig = [0u8; 64];
            weak_sig[0] = 1;
            assert!(weak_vk.verify(b"anything", &weak_sig).is_err());
        }
    }

    #[test]
    fn openssh_key_roundtrip_sshsig_v1_and_encrypted_or_rsa_diagnostics() {
        let sk = Ed25519SigningKey::from_seed([0x42; 32]);
        let priv_key =
            OpenSshPrivateKey::from_signing_key(sk, Some("signer@ozone.dev".to_string()));
        let pem = priv_key.to_openssh_pem();
        assert!(pem.starts_with(PRIVKEY_ARMOR_HEADER));
        assert!(pem.ends_with(PRIVKEY_ARMOR_FOOTER));

        let parsed_priv = OpenSshPrivateKey::parse_openssh(&pem).expect("parse private key");
        assert_eq!(parsed_priv.comment(), Some("signer@ozone.dev"));

        let pub_line = parsed_priv.public_key().to_openssh_line();
        let parsed_pub = OpenSshPublicKey::parse_openssh(&pub_line).expect("parse pubkey");
        let parsed_inline =
            OpenSshPublicKey::parse_openssh(&format!("key::{pub_line}")).expect("parse key::");
        assert_eq!(parsed_pub, parsed_inline);
        assert!(parsed_pub.fingerprint_sha256().starts_with("SHA256:"));
        assert!(!parsed_pub.fingerprint_sha256().ends_with('='));

        // SSHSIG v1 sign + verify + 70-col wrapping check
        let message = b"tree 0000000000000000000000000000000000000000\n\ntest commit\n";
        let armored = parsed_priv
            .sign_sshsig(message, GIT_SSHSIG_NAMESPACE)
            .expect("sign sshsig");
        let lines: Vec<&str> = armored.lines().collect();
        assert_eq!(lines.first().copied(), Some(SSHSIG_ARMOR_HEADER));
        assert_eq!(lines.last().copied(), Some(SSHSIG_ARMOR_FOOTER));
        for body_line in &lines[1..lines.len() - 1] {
            assert!(body_line.len() <= SSHSIG_WRAP_COLUMNS);
        }
        assert_eq!(lines[1].len(), SSHSIG_WRAP_COLUMNS);

        let verified =
            verify_sshsig(message, &armored, GIT_SSHSIG_NAMESPACE).expect("verify sshsig");
        assert_eq!(verified.version, 1);
        assert_eq!(verified.namespace, "git");
        assert_eq!(verified.hash_algorithm, "sha512");
        assert_eq!(
            verified.public_key.fingerprint_sha256(),
            parsed_pub.fingerprint_sha256()
        );

        // Wrong namespace and tampered payload must fail verification
        assert!(verify_sshsig(message, &armored, "file").is_err());
        assert!(verify_sshsig(b"modified", &armored, GIT_SSHSIG_NAMESPACE).is_err());

        // Unsupported algorithm diagnostic
        let rsa_err = OpenSshPublicKey::parse_openssh("ssh-rsa AAAAB3NzaC1yc2E= user@host")
            .expect_err("reject rsa");
        assert!(rsa_err.to_string().contains("ssh-rsa"));

        // Encrypted openssh-key-v1 diagnostic
        let mut enc_outer = Vec::new();
        enc_outer.extend_from_slice(OPENSSH_KEY_V1_MAGIC);
        put_ssh_string(&mut enc_outer, b"aes256-ctr");
        put_ssh_string(&mut enc_outer, b"bcrypt");
        put_ssh_string(&mut enc_outer, b"salt");
        put_u32(&mut enc_outer, 1);
        put_ssh_string(&mut enc_outer, &parsed_pub.to_wire_blob());
        put_ssh_string(&mut enc_outer, b"ciphertext");
        let enc_pem = wrap_armor(
            PRIVKEY_ARMOR_HEADER,
            PRIVKEY_ARMOR_FOOTER,
            &base64_encode(&enc_outer),
            70,
        );
        let enc_err = OpenSshPrivateKey::parse_openssh(&enc_pem).expect_err("reject encrypted");
        assert!(enc_err.to_string().contains("encrypted"));
        assert!(!enc_err.recovery_commands().is_empty());
    }
}
