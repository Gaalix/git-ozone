// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Ed25519 (RFC 8032), SHA-512 / SHA-256 (FIPS 180-4), Base64 (RFC 4648),
//! OpenSSH key parsing (`openssh-key-v1` & `ssh-ed25519`), OpenSSH `SSHSIG` v1 signing
//! and verification (`namespace = "git"`), and SSH signing readiness diagnostics.
//!
//! Zero C or OpenSSL dependencies are used anywhere in this module.

use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::open_gix_for_workspace;
use gix::bstr::ByteSlice;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
// FIPS 180-4 SHA-256 & SHA-512
// ============================================================================

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const K512: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

/// Computes the FIPS 180-4 SHA-256 digest of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        sha256_compress(&mut state, block);
    }

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;

    if rem.len() < 56 {
        tail[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let mut blk = [0u8; 64];
        blk.copy_from_slice(&tail[..64]);
        sha256_compress(&mut state, &blk);
    } else {
        tail[120..128].copy_from_slice(&bit_len.to_be_bytes());
        let mut blk0 = [0u8; 64];
        let mut blk1 = [0u8; 64];
        blk0.copy_from_slice(&tail[..64]);
        blk1.copy_from_slice(&tail[64..128]);
        sha256_compress(&mut state, &blk0);
        sha256_compress(&mut state, &blk1);
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes(*chunk);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K256[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Streaming FIPS 180-4 SHA-512 hasher.
#[derive(Clone)]
pub struct Sha512 {
    state: [u64; 8],
    buf: [u8; 128],
    buf_len: usize,
    total_bytes: u128,
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    /// Creates a new SHA-512 hasher initialized to the standard IV.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: [
                0x6a09e667f3bcc908,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ],
            buf: [0u8; 128],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    /// Feeds bytes into the SHA-512 state.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u128);

        if self.buf_len > 0 {
            let needed = 128 - self.buf_len;
            if data.len() < needed {
                self.buf[self.buf_len..self.buf_len + data.len()].copy_from_slice(data);
                self.buf_len += data.len();
                return;
            }
            self.buf[self.buf_len..128].copy_from_slice(&data[..needed]);
            let block = self.buf;
            sha512_compress(&mut self.state, &block);
            self.buf_len = 0;
            data = &data[needed..];
        }

        let (blocks, rem) = data.as_chunks::<128>();
        for block in blocks {
            sha512_compress(&mut self.state, block);
        }

        if !rem.is_empty() {
            self.buf[..rem.len()].copy_from_slice(rem);
            self.buf_len = rem.len();
        }
    }

    /// Finalizes the SHA-512 calculation and returns the 64-byte digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 64] {
        let bit_len = self.total_bytes.wrapping_mul(8);
        self.buf[self.buf_len] = 0x80;
        for b in &mut self.buf[self.buf_len + 1..] {
            *b = 0;
        }

        if self.buf_len >= 112 {
            let block = self.buf;
            sha512_compress(&mut self.state, &block);
            self.buf = [0u8; 128];
        }

        self.buf[112..128].copy_from_slice(&bit_len.to_be_bytes());
        let final_block = self.buf;
        sha512_compress(&mut self.state, &final_block);

        let mut out = [0u8; 64];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Computes the FIPS 180-4 SHA-512 digest of `data`.
#[must_use]
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(data);
    h.finalize()
}

fn sha512_compress(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 80];
    for (i, chunk) in block.as_chunks::<8>().0.iter().enumerate() {
        w[i] = u64::from_be_bytes(*chunk);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K512[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

// ============================================================================
// RFC 4648 Base64 Encode & Decode
// ============================================================================

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `data` as standard RFC 4648 Base64 with `=` padding.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    base64_encode_inner(data, true)
}

/// Encodes `data` as standard RFC 4648 Base64 without trailing `=` padding
/// (used for OpenSSH `SHA256:<fingerprint>` strings).
#[must_use]
pub fn base64_encode_no_pad(data: &[u8]) -> String {
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
pub fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();

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
            return Err(OzError::InvalidUsage {
                message: "invalid base64 padding".to_string(),
                recovery: Vec::new(),
            });
        }
        if quad[..4 - pad_count].contains(&b'=') {
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

        let s0 = decode_sextet(quad[0])?;
        let s1 = decode_sextet(quad[1])?;
        let s2 = if pad_count >= 2 { 0 } else { decode_sextet(quad[2])? };
        let s3 = if pad_count >= 1 { 0 } else { decode_sextet(quad[3])? };

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

// ============================================================================
// Pure-Rust RFC 8032 Ed25519
// ============================================================================

const MASK51: u64 = (1u64 << 51) - 1;

/// Element of GF(2^255 - 19) represented in radix 2^51.
#[derive(Clone, Copy, Debug)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Self = Self([0, 0, 0, 0, 0]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    const fn from_u64(v: u64) -> Self {
        Self([v & MASK51, v >> 51, 0, 0, 0])
    }

    /// Decodes a 255-bit integer from 32 little-endian bytes (ignoring bit 255).
    fn from_bytes_unchecked(s: &[u8; 32]) -> Self {
        let load8 = |offset: usize| -> u64 {
            u64::from_le_bytes([
                s[offset],
                s[offset + 1],
                s[offset + 2],
                s[offset + 3],
                s[offset + 4],
                s[offset + 5],
                s[offset + 6],
                s[offset + 7],
            ])
        };
        let l0 = load8(0) & MASK51;
        let l1 = (load8(6) >> 3) & MASK51;
        let l2 = (load8(12) >> 6) & MASK51;
        let l3 = (load8(19) >> 1) & MASK51;
        let l4 = (load8(24) >> 12) & MASK51;
        Self([l0, l1, l2, l3, l4])
    }

    /// Strictly decodes a canonical field element `< p` (with top sign bit masked by caller).
    fn from_bytes_canonical(s_masked: &[u8; 32]) -> Option<Self> {
        let fe = Self::from_bytes_unchecked(s_masked);
        if &fe.to_bytes() == s_masked {
            Some(fe)
        } else {
            None
        }
    }

    /// Reduces modulo `p = 2^255 - 19` to canonical 32 little-endian bytes.
    fn to_bytes(self) -> [u8; 32] {
        let mut l = self.0;
        for _ in 0..2 {
            let c0 = l[0] >> 51;
            l[0] &= MASK51;
            l[1] += c0;
            let c1 = l[1] >> 51;
            l[1] &= MASK51;
            l[2] += c1;
            let c2 = l[2] >> 51;
            l[2] &= MASK51;
            l[3] += c2;
            let c3 = l[3] >> 51;
            l[3] &= MASK51;
            l[4] += c3;
            let c4 = l[4] >> 51;
            l[4] &= MASK51;
            l[0] += c4 * 19;
        }

        // Compute q = 1 iff value >= 2^255 - 19, else 0.
        let mut q = (l[0] + 19) >> 51;
        q = (l[1] + q) >> 51;
        q = (l[2] + q) >> 51;
        q = (l[3] + q) >> 51;
        q = (l[4] + q) >> 51;

        l[0] += 19 * q;
        let c0 = l[0] >> 51;
        l[0] &= MASK51;
        l[1] += c0;
        let c1 = l[1] >> 51;
        l[1] &= MASK51;
        l[2] += c1;
        let c2 = l[2] >> 51;
        l[2] &= MASK51;
        l[3] += c2;
        let c3 = l[3] >> 51;
        l[3] &= MASK51;
        l[4] = (l[4] + c3) & MASK51;

        // Pack 5 x 51-bit limbs into 32 little-endian bytes.
        let mut out = [0u8; 32];
        let mut acc: u128 = 0;
        let mut bits: usize = 0;
        let mut limb_idx = 0;
        for out_byte in &mut out {
            if bits < 8 && limb_idx < 5 {
                acc |= (l[limb_idx] as u128) << bits;
                bits += 51;
                limb_idx += 1;
            }
            *out_byte = (acc & 0xff) as u8;
            acc >>= 8;
            bits = bits.saturating_sub(8);
        }
        out
    }

    fn carry_reduce(mut l: [u64; 5]) -> Self {
        let c0 = l[0] >> 51;
        l[0] &= MASK51;
        l[1] += c0;
        let c1 = l[1] >> 51;
        l[1] &= MASK51;
        l[2] += c1;
        let c2 = l[2] >> 51;
        l[2] &= MASK51;
        l[3] += c2;
        let c3 = l[3] >> 51;
        l[3] &= MASK51;
        l[4] += c3;
        let c4 = l[4] >> 51;
        l[4] &= MASK51;
        l[0] += c4 * 19;
        Self(l)
    }

    fn add(&self, rhs: &Self) -> Self {
        Self::carry_reduce([
            self.0[0] + rhs.0[0],
            self.0[1] + rhs.0[1],
            self.0[2] + rhs.0[2],
            self.0[3] + rhs.0[3],
            self.0[4] + rhs.0[4],
        ])
    }

    fn sub(&self, rhs: &Self) -> Self {
        // Add 4*p before subtracting so limbs never underflow even on unreduced inputs.
        const FOUR_P0: u64 = 4 * ((1u64 << 51) - 19);
        const FOUR_PN: u64 = 4 * ((1u64 << 51) - 1);
        let mut l = [
            (self.0[0] + FOUR_P0) - rhs.0[0],
            (self.0[1] + FOUR_PN) - rhs.0[1],
            (self.0[2] + FOUR_PN) - rhs.0[2],
            (self.0[3] + FOUR_PN) - rhs.0[3],
            (self.0[4] + FOUR_PN) - rhs.0[4],
        ];
        let c0 = l[0] >> 51;
        l[0] &= MASK51;
        l[1] += c0;
        let c1 = l[1] >> 51;
        l[1] &= MASK51;
        l[2] += c1;
        let c2 = l[2] >> 51;
        l[2] &= MASK51;
        l[3] += c2;
        let c3 = l[3] >> 51;
        l[3] &= MASK51;
        l[4] += c3;
        let c4 = l[4] >> 51;
        l[4] &= MASK51;
        l[0] += c4 * 19;
        Self(l)
    }

    fn neg(&self) -> Self {
        Self::ZERO.sub(self)
    }

    fn mul(&self, rhs: &Self) -> Self {
        let a = &self.0;
        let b = &rhs.0;
        let a0 = a[0] as u128;
        let a1 = a[1] as u128;
        let a2 = a[2] as u128;
        let a3 = a[3] as u128;
        let a4 = a[4] as u128;

        let b0 = b[0] as u128;
        let b1 = b[1] as u128;
        let b2 = b[2] as u128;
        let b3 = b[3] as u128;
        let b4 = b[4] as u128;

        let b1_19 = 19 * b1;
        let b2_19 = 19 * b2;
        let b3_19 = 19 * b3;
        let b4_19 = 19 * b4;

        let mut t0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let mut t1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let mut t2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let mut t3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let mut t4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

        let mask = MASK51 as u128;
        let c0 = t0 >> 51;
        t0 &= mask;
        t1 += c0;
        let c1 = t1 >> 51;
        t1 &= mask;
        t2 += c1;
        let c2 = t2 >> 51;
        t2 &= mask;
        t3 += c2;
        let c3 = t3 >> 51;
        t3 &= mask;
        t4 += c3;
        let c4 = t4 >> 51;
        t4 &= mask;
        t0 += c4 * 19;
        let c0b = t0 >> 51;
        t0 &= mask;
        t1 += c0b;

        Self([t0 as u64, t1 as u64, t2 as u64, t3 as u64, t4 as u64])
    }

    fn sq(&self) -> Self {
        self.mul(self)
    }

    fn pow_le(&self, exp_le: &[u8; 32]) -> Self {
        let mut res = Self::ONE;
        for byte_idx in (0..32).rev() {
            let byte = exp_le[byte_idx];
            for bit in (0..8).rev() {
                res = res.sq();
                if ((byte >> bit) & 1) == 1 {
                    res = res.mul(self);
                }
            }
        }
        res
    }

    /// Computes `self^(p - 2) mod p`.
    fn inv(&self) -> Self {
        let mut exp = [0xffu8; 32];
        exp[0] = 0xeb; // 2^255 - 21
        exp[31] = 0x7f;
        self.pow_le(&exp)
    }

    /// Computes `self^((p - 5) / 8) mod p = self^(2^252 - 3) mod p`.
    fn pow_p_minus_5_div_8(&self) -> Self {
        let mut exp = [0xffu8; 32];
        exp[0] = 0xfd;
        exp[31] = 0x0f;
        self.pow_le(&exp)
    }

    /// Computes `sqrt(-1) = 2^((p - 1) / 4) mod p = 2^(2^253 - 5) mod p`.
    fn sqrt_m1() -> Self {
        let mut exp = [0xffu8; 32];
        exp[0] = 0xfb;
        exp[31] = 0x1f;
        Self::from_u64(2).pow_le(&exp)
    }

    /// Curve constant `d = -121665 / 121666 mod p`.
    fn edwards_d() -> Self {
        Self::from_u64(121_665)
            .neg()
            .mul(&Self::from_u64(121_666).inv())
    }
}

impl PartialEq for Fe {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes() == other.to_bytes()
    }
}
impl Eq for Fe {}

/// Point on the Ed25519 twisted Edwards curve in extended coordinates `(X : Y : Z : T)`.
#[derive(Clone, Copy, Debug)]
struct EdwardsPoint {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl EdwardsPoint {
    const IDENTITY: Self = Self {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    fn basepoint() -> Self {
        // B_y = 4 / 5 mod p, x_0 = 0 (positive/even x) per RFC 8032 §5.1.
        let by = Fe::from_u64(4).mul(&Fe::from_u64(5).inv());
        let compressed = by.to_bytes();
        Self::decompress(&compressed).expect("RFC 8032 Ed25519 basepoint must decompress")
    }

    /// Decompresses a canonical 32-byte Ed25519 point encoding per RFC 8032 §5.1.3.
    fn decompress(bytes: &[u8; 32]) -> Option<Self> {
        let x_sign = (bytes[31] >> 7) & 1;
        let mut y_bytes = *bytes;
        y_bytes[31] &= 0x7f;

        let y = Fe::from_bytes_canonical(&y_bytes)?;
        let d = Fe::edwards_d();
        let y2 = y.sq();
        let u = y2.sub(&Fe::ONE);
        let v = d.mul(&y2).add(&Fe::ONE);

        // Candidate root x = u * v^3 * (u * v^7)^((p - 5)/8) mod p
        let v2 = v.sq();
        let v3 = v2.mul(&v);
        let v7 = v3.sq().mul(&v);
        let uv7 = u.mul(&v7);
        let mut x = u.mul(&v3).mul(&uv7.pow_p_minus_5_div_8());

        let vx2 = v.mul(&x.sq());
        if vx2 != u {
            if vx2 == u.neg() {
                x = x.mul(&Fe::sqrt_m1());
            } else {
                return None;
            }
        }

        let x_bytes = x.to_bytes();
        if x_bytes == [0u8; 32] && x_sign == 1 {
            return None;
        }
        if (x_bytes[0] & 1) != x_sign {
            x = x.neg();
        }

        let t = x.mul(&y);
        Some(Self {
            x,
            y,
            z: Fe::ONE,
            t,
        })
    }

    /// Compresses this point to 32 bytes per RFC 8032 §5.1.2.
    fn compress(&self) -> [u8; 32] {
        let z_inv = self.z.inv();
        let x = self.x.mul(&z_inv).to_bytes();
        let mut y = self.y.mul(&z_inv).to_bytes();
        y[31] |= (x[0] & 1) << 7;
        y
    }

    /// Complete extended twisted Edwards addition per RFC 8032 §5.1.4.
    fn add(&self, rhs: &Self, two_d: &Fe) -> Self {
        let a = self.y.sub(&self.x).mul(&rhs.y.sub(&rhs.x));
        let b = self.y.add(&self.x).mul(&rhs.y.add(&rhs.x));
        let c = self.t.mul(two_d).mul(&rhs.t);
        let d = self.z.add(&self.z).mul(&rhs.z);
        let e = b.sub(&a);
        let f = d.sub(&c);
        let g = d.add(&c);
        let h = b.add(&a);
        Self {
            x: e.mul(&f),
            y: g.mul(&h),
            t: e.mul(&h),
            z: f.mul(&g),
        }
    }

    fn scalar_mul(&self, scalar_le: &[u8; 32]) -> Self {
        let d = Fe::edwards_d();
        let two_d = d.add(&d);
        let mut acc = Self::IDENTITY;
        for byte_idx in (0..32).rev() {
            let byte = scalar_le[byte_idx];
            for bit in (0..8).rev() {
                acc = acc.add(&acc, &two_d);
                if ((byte >> bit) & 1) == 1 {
                    acc = acc.add(self, &two_d);
                }
            }
        }
        acc
    }
}

impl PartialEq for EdwardsPoint {
    fn eq(&self, other: &Self) -> bool {
        // X1 * Z2 == X2 * Z1 && Y1 * Z2 == Y2 * Z1
        self.x.mul(&other.z) == other.x.mul(&self.z)
            && self.y.mul(&other.z) == other.y.mul(&self.z)
    }
}
impl Eq for EdwardsPoint {}

// Group order L = 2^252 + 27742317777372353535851937790883648493.
const L_LOW: u128 = 27_742_317_777_372_353_535_851_937_790_883_648_493_u128;
const L_WORDS: [u64; 4] = [
    L_LOW as u64,
    (L_LOW >> 64) as u64,
    0,
    1u64 << 60,
];

fn words_ge_l(a: &[u64; 4]) -> bool {
    for i in (0..4).rev() {
        if a[i] != L_WORDS[i] {
            return a[i] > L_WORDS[i];
        }
    }
    true
}

fn sub_l_in_place(a: &mut [u64; 4]) {
    let mut borrow: u128 = 0;
    for i in 0..4 {
        let sub = (L_WORDS[i] as u128) + borrow;
        if (a[i] as u128) >= sub {
            a[i] = ((a[i] as u128) - sub) as u64;
            borrow = 0;
        } else {
            a[i] = (((1u128 << 64) + (a[i] as u128)) - sub) as u64;
            borrow = 1;
        }
    }
}

/// Checks whether a 32-byte little-endian scalar is strictly canonical (`< L`).
#[must_use]
fn is_canonical_scalar(s: &[u8; 32]) -> bool {
    let mut w = [0u64; 4];
    for (i, chunk) in s.as_chunks::<8>().0.iter().enumerate() {
        w[i] = u64::from_le_bytes(*chunk);
    }
    !words_ge_l(&w)
}

/// Reduces an arbitrary-length little-endian unsigned integer modulo `L` into 32 LE bytes.
fn reduce_mod_l(le_bytes: &[u8]) -> [u8; 32] {
    let mut acc = [0u64; 4];
    for &byte in le_bytes.iter().rev() {
        for bit in (0..8).rev() {
            let mut carry = ((byte >> bit) & 1) as u64;
            for word in &mut acc {
                let next_carry = *word >> 63;
                *word = (*word << 1) | carry;
                carry = next_carry;
            }
            if carry != 0 || words_ge_l(&acc) {
                sub_l_in_place(&mut acc);
            }
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in acc.iter().enumerate() {
        out[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// Computes `(r + k * s) mod L` in little-endian 32-byte representation.
fn scalar_mul_add_mod_l(r: &[u8; 32], k: &[u8; 32], s: &[u8; 32]) -> [u8; 32] {
    let mut rw = [0u64; 4];
    let mut kw = [0u64; 4];
    let mut sw = [0u64; 4];
    for i in 0..4 {
        let sl = i * 8;
        rw[i] = u64::from_le_bytes(r[sl..sl + 8].try_into().expect("8 bytes"));
        kw[i] = u64::from_le_bytes(k[sl..sl + 8].try_into().expect("8 bytes"));
        sw[i] = u64::from_le_bytes(s[sl..sl + 8].try_into().expect("8 bytes"));
    }

    let mut prod = [0u64; 8];
    prod[..4].copy_from_slice(&rw);

    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in 0..4 {
            let cur = (prod[i + j] as u128) + (kw[i] as u128) * (sw[j] as u128) + carry;
            prod[i + j] = cur as u64;
            carry = cur >> 64;
        }
        let mut idx = i + 4;
        while carry > 0 && idx < 8 {
            let cur = (prod[idx] as u128) + carry;
            prod[idx] = cur as u64;
            carry = cur >> 64;
            idx += 1;
        }
    }

    let mut prod_le = [0u8; 64];
    for (i, word) in prod.iter().enumerate() {
        prod_le[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
    }
    reduce_mod_l(&prod_le)
}

/// Pure-Rust RFC 8032 Ed25519 signing key derived from a 32-byte secret seed.
#[derive(Clone)]
pub struct Ed25519SigningKey {
    seed: [u8; 32],
    clamped_scalar: [u8; 32],
    nonce_prefix: [u8; 32],
    public_key: [u8; 32],
}

impl std::fmt::Debug for Ed25519SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519SigningKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl Ed25519SigningKey {
    /// Derives an Ed25519 keypair from a 32-byte secret seed per RFC 8032 §5.1.5.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let digest = sha512(&seed);
        let mut clamped_scalar = [0u8; 32];
        clamped_scalar.copy_from_slice(&digest[..32]);
        clamped_scalar[0] &= 248;
        clamped_scalar[31] &= 127;
        clamped_scalar[31] |= 64;

        let mut nonce_prefix = [0u8; 32];
        nonce_prefix.copy_from_slice(&digest[32..64]);

        let base = EdwardsPoint::basepoint();
        let public_key = base.scalar_mul(&clamped_scalar).compress();

        Self {
            seed,
            clamped_scalar,
            nonce_prefix,
            public_key,
        }
    }

    /// Returns the 32-byte secret seed.
    #[must_use]
    pub const fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    /// Returns the 32-byte compressed Ed25519 public key.
    #[must_use]
    pub const fn verifying_key_bytes(&self) -> [u8; 32] {
        self.public_key
    }

    /// Returns the corresponding [`Ed25519VerifyingKey`].
    #[must_use]
    pub fn verifying_key(&self) -> Ed25519VerifyingKey {
        Ed25519VerifyingKey::from_bytes(self.public_key)
            .expect("derived Ed25519 public key is always canonical")
    }

    /// Deterministically signs `message` per RFC 8032 §5.1.6, returning a 64-byte signature.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let mut r_hasher = Sha512::new();
        r_hasher.update(&self.nonce_prefix);
        r_hasher.update(message);
        let r = reduce_mod_l(&r_hasher.finalize());

        let base = EdwardsPoint::basepoint();
        let cap_r = base.scalar_mul(&r).compress();

        let mut k_hasher = Sha512::new();
        k_hasher.update(&cap_r);
        k_hasher.update(&self.public_key);
        k_hasher.update(message);
        let k = reduce_mod_l(&k_hasher.finalize());

        let cap_s = scalar_mul_add_mod_l(&r, &k, &self.clamped_scalar);

        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&cap_r);
        sig[32..64].copy_from_slice(&cap_s);
        sig
    }
}

/// Pure-Rust RFC 8032 Ed25519 public verifying key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ed25519VerifyingKey {
    bytes: [u8; 32],
    point: EdwardsPoint,
}

impl Ed25519VerifyingKey {
    /// Parses and validates a canonical 32-byte Ed25519 public key.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        let point = EdwardsPoint::decompress(&bytes).ok_or_else(|| OzError::InvalidUsage {
            message: "invalid Ed25519 public key (point decompression failed)".to_string(),
            recovery: Vec::new(),
        })?;
        Ok(Self { bytes, point })
    }

    /// Returns the 32-byte compressed public key encoding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Verifies a 64-byte Ed25519 `signature` over `message` per RFC 8032 §5.1.7.
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<()> {
        let mut r_bytes = [0u8; 32];
        let mut s_bytes = [0u8; 32];
        r_bytes.copy_from_slice(&signature[..32]);
        s_bytes.copy_from_slice(&signature[32..64]);

        if !is_canonical_scalar(&s_bytes) {
            return Err(OzError::CheckFailed {
                message: "invalid Ed25519 signature (scalar S >= L)".to_string(),
                recovery: Vec::new(),
            });
        }

        let Some(r_point) = EdwardsPoint::decompress(&r_bytes) else {
            return Err(OzError::CheckFailed {
                message: "invalid Ed25519 signature (point R decompression failed)".to_string(),
                recovery: Vec::new(),
            });
        };

        let mut k_hasher = Sha512::new();
        k_hasher.update(&r_bytes);
        k_hasher.update(&self.bytes);
        k_hasher.update(message);
        let k = reduce_mod_l(&k_hasher.finalize());

        let d = Fe::edwards_d();
        let two_d = d.add(&d);
        let lhs = EdwardsPoint::basepoint().scalar_mul(&s_bytes);
        let rhs = r_point.add(&self.point.scalar_mul(&k), &two_d);

        if lhs == rhs {
            Ok(())
        } else {
            Err(OzError::CheckFailed {
                message: "Ed25519 signature verification equation failed".to_string(),
                recovery: Vec::new(),
            })
        }
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
#[derive(Clone, Debug)]
pub struct OpenSshPrivateKey {
    signing_key: Ed25519SigningKey,
    comment: Option<String>,
}

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
        if trimmed.contains("ENCRYPTED")
            && !trimmed.contains(PRIVKEY_ARMOR_HEADER)
        {
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

        let body_b64 = extract_armor_body(
            trimmed,
            PRIVKEY_ARMOR_HEADER,
            PRIVKEY_ARMOR_FOOTER,
            "OpenSSH private key",
        )?;
        let raw = base64_decode(&body_b64)?;
        let mut reader = SshReader::new(&raw);

        let magic = reader.read_exact(OPENSSH_KEY_V1_MAGIC.len(), "openssh-key-v1 magic")?;
        if magic != OPENSSH_KEY_V1_MAGIC {
            return Err(OzError::InvalidUsage {
                message: "unrecognized OpenSSH private key magic (expected 'openssh-key-v1')"
                    .to_string(),
                recovery: Vec::new(),
            });
        }

        let ciphername = String::from_utf8_lossy(reader.read_ssh_string("ciphername")?).into_owned();
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

        let mut seed = [0u8; 32];
        seed.copy_from_slice(&priv_bytes[..32]);
        let signing_key = Ed25519SigningKey::from_seed(seed);
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
        let raw = fs::read_to_string(path).map_err(|err| OzError::InvalidUsage {
            message: format!(
                "failed to read SSH signing key at {}: {err}",
                path.display()
            ),
            recovery: vec![
                "ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519".to_string(),
                format!("Verify signing key permissions and path: {}", path.display()),
            ],
        })?;
        Self::parse_openssh(&raw)
    }

    /// Encodes this unencrypted Ed25519 private key in standard `openssh-key-v1` PEM format.
    #[must_use]
    pub fn to_openssh_pem(&self) -> String {
        let pub_key = self.public_key();
        let pub_blob = pub_key.to_wire_blob();

        let mut priv_sec = Vec::with_capacity(160);
        let checkint: u32 = 0x5a5a_a5a5;
        put_u32(&mut priv_sec, checkint);
        put_u32(&mut priv_sec, checkint);
        put_ssh_string(&mut priv_sec, ALG_ED25519.as_bytes());
        put_ssh_string(&mut priv_sec, &self.signing_key.verifying_key_bytes());

        let mut seed_and_pub = [0u8; 64];
        seed_and_pub[..32].copy_from_slice(self.signing_key.seed());
        seed_and_pub[32..64].copy_from_slice(&self.signing_key.verifying_key_bytes());
        put_ssh_string(&mut priv_sec, &seed_and_pub);

        let comment_bytes = self.comment.as_deref().unwrap_or("").as_bytes();
        put_ssh_string(&mut priv_sec, comment_bytes);

        let pad_len = (8 - (priv_sec.len() % 8)) % 8;
        for i in 1..=pad_len {
            priv_sec.push(i as u8);
        }

        let mut outer = Vec::with_capacity(256);
        outer.extend_from_slice(OPENSSH_KEY_V1_MAGIC);
        put_ssh_string(&mut outer, b"none");
        put_ssh_string(&mut outer, b"none");
        put_ssh_string(&mut outer, b"");
        put_u32(&mut outer, 1);
        put_ssh_string(&mut outer, &pub_blob);
        put_ssh_string(&mut outer, &priv_sec);

        wrap_armor(PRIVKEY_ARMOR_HEADER, PRIVKEY_ARMOR_FOOTER, &base64_encode(&outer), 70)
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
    let mut signed_data = Vec::with_capacity(6 + 4 + namespace.len() + 4 + reserved.len() + 4 + hash_algorithm.len() + 4 + message_digest.len());
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

    let version = reader.read_u32("SSHSIG version").map_err(|err| OzError::CheckFailed {
        message: err.to_string(),
        recovery: Vec::new(),
    })?;
    if version != SSHSIG_VERSION_V1 {
        return Err(OzError::CheckFailed {
            message: format!("unsupported SSHSIG version {version} (expected 1)"),
            recovery: Vec::new(),
        });
    }

    let pubkey_blob = reader
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

    let namespace_bytes = reader
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

fn extract_armor_body(
    input: &str,
    header: &str,
    footer: &str,
    label: &str,
) -> Result<String> {
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
    if trimmed.starts_with("key::") || trimmed.starts_with("ssh-") || trimmed.starts_with("ecdsa-") || trimmed.starts_with("sk-") {
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

    let content = fs::read_to_string(&path).map_err(|err| OzError::InvalidUsage {
        message: format!("failed to read SSH signing key at {}: {err}", path.display()),
        recovery: Vec::new(),
    })?;
    let content_trimmed = content.trim();

    // If `user.signingkey` points to `<name>.pub`, parse the `.pub` file and load sibling `<name>`.
    if content_trimmed.starts_with("ssh-")
        || content_trimmed.starts_with("ecdsa-")
        || content_trimmed.starts_with("sk-")
        || path.extension().and_then(|e| e.to_str()) == Some("pub") && !content_trimmed.contains("-----BEGIN ")
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
            .position(|t| *t == ALG_ED25519 || t.starts_with("ssh-") || t.starts_with("ecdsa-") || t.starts_with("sk-"))
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
        let clean: Vec<u8> = hex
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
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
        assert_eq!(streaming.finalize(), sha512(&big));
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
        // Group order sanity check: [L] * B == Identity
        let mut l_le = [0u8; 32];
        for (i, w) in L_WORDS.iter().enumerate() {
            l_le[i * 8..(i + 1) * 8].copy_from_slice(&w.to_le_bytes());
        }
        assert_eq!(
            EdwardsPoint::basepoint().scalar_mul(&l_le),
            EdwardsPoint::IDENTITY
        );

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
            vk.verify(msg, &sig).expect("RFC 8032 signature must verify");

            // Tampered message must fail
            assert!(vk.verify(b"tampered", &sig).is_err());

            // Tampered signature must fail
            let mut bad_sig = sig;
            bad_sig[63] ^= 0x01;
            assert!(vk.verify(msg, &bad_sig).is_err());
        }

        // RFC 8032 §7.1 SHA(abc) test vector
        let seed_sha: [u8; 32] = hex_decode(
            "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
        )
        .try_into()
        .unwrap();
        let pub_sha: [u8; 32] = hex_decode(
            "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        )
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
        assert!(sk_sha.verifying_key().verify(&msg_sha, &non_canonical_sig).is_err());

        // Reject non-canonical public key y = p (0x7fff...ffed)
        let mut p_bytes = [0xffu8; 32];
        p_bytes[0] = 0xed;
        p_bytes[31] = 0x7f;
        assert!(Ed25519VerifyingKey::from_bytes(p_bytes).is_err());
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
