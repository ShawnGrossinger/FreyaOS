//! sha1.rs — SHA-1, for checking a TPM 1.2's PCRs (Entry 33).
//!
//! A TPM 1.2 keeps its Platform Configuration Registers as SHA-1 values:
//! extending PCR p with a 20-byte digest d makes it SHA-1(p || d). To check
//! the TPM's arithmetic, and to recompute a PCR from the seals the kernel
//! printed, the kernel, the programs' library and the build machine's
//! audit_verify need the same function. Nothing here relies on SHA-1 for
//! anything else: it has been broken for collisions since 2017, and the
//! chain itself is BLAKE3. It is what a TPM 1.2 does, so it is what a check
//! of one must do.
//!
//! Plain Rust, no unsafe, nothing from the kernel (as blake3.rs). FIPS
//! 180-4, one 64-byte block at a time.

// Built into three crates, and not every one uses every part.
#![allow(dead_code)]

pub const OUT_LEN: usize = 20;

const H0: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];

fn block(h: &mut [u32; 5], b: &[u8; 64]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b_, mut c, mut d, mut e] = *h;
    for (i, &wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b_ & c) | (!b_ & d), 0x5A82_7999),
            20..=39 => (b_ ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b_ & c) | (b_ & d) | (c & d), 0x8F1B_BCDC),
            _ => (b_ ^ c ^ d, 0xCA62_C1D6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
        e = d;
        d = c;
        c = b_.rotate_left(30);
        b_ = a;
        a = t;
    }
    for (x, y) in h.iter_mut().zip([a, b_, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

/// SHA-1 over input given in pieces.
#[derive(Clone, Copy)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Sha1 {
    pub const fn new() -> Self {
        Sha1 { h: H0, buf: [0; 64], buf_len: 0, total: 0 }
    }

    pub fn update(&mut self, mut input: &[u8]) {
        self.total += input.len() as u64;
        while !input.is_empty() {
            let take = (64 - self.buf_len).min(input.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&input[..take]);
            self.buf_len += take;
            input = &input[take..];
            if self.buf_len == 64 {
                let b = self.buf;
                block(&mut self.h, &b);
                self.buf_len = 0;
            }
        }
    }

    pub fn finalize(mut self) -> [u8; OUT_LEN] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; OUT_LEN];
        for (i, x) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
        }
        out
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn sha1(input: &[u8]) -> [u8; OUT_LEN] {
    let mut s = Sha1::new();
    s.update(input);
    s.finalize()
}

/// What a TPM 1.2 makes a PCR holding `pcr` when it is extended with
/// `digest`: SHA-1(pcr || digest).
pub fn extend(pcr: &[u8; OUT_LEN], digest: &[u8; OUT_LEN]) -> [u8; OUT_LEN] {
    let mut s = Sha1::new();
    s.update(pcr);
    s.update(digest);
    s.finalize()
}

/// FIPS 180's examples (and the empty string): "abc", the 448-bit two-block
/// message, and a million 'a's, fed a block at a time.
pub fn test_vectors_passed() -> usize {
    let mut passed = 0;
    let hex = |h: [u8; OUT_LEN]| {
        let mut s = [0u8; 40];
        for (i, b) in h.iter().enumerate() {
            s[2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
            s[2 * i + 1] = b"0123456789abcdef"[(b & 15) as usize];
        }
        s
    };
    let cases: [(&[u8], &[u8; 40]); 3] = [
        (b"", b"da39a3ee5e6b4b0d3255bfef95601890afd80709"),
        (b"abc", b"a9993e364706816aba3e25717850c26c9cd0d89d"),
        (b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq", b"84983e441c3bd26ebaae4aa1f95129e5e54670f1"),
    ];
    for (input, want) in cases {
        if &hex(sha1(input)) == want {
            passed += 1;
        }
    }
    let mut s = Sha1::new();
    for _ in 0..(1_000_000 / 64) {
        s.update(&[b'a'; 64]);
    }
    if &hex(s.finalize()) == b"34aa973cd4c4daa4f61eeb2bdbad27316534016f" {
        passed += 1;
    }
    passed
}

pub const TEST_VECTORS: usize = 4;
