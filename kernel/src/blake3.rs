//! blake3.rs — the BLAKE3 hash, for the audit chain (Entry 32).
//!
//! The manifesto's default hash (Section V, 5.1). Written here rather than
//! taken from a crate, since the kernel and the programs take nothing from
//! outside; it follows BLAKE3's reference implementation (by its authors,
//! public domain), which favours being plainly right over being fast: one
//! block at a time, no SIMD. The self-test checks it against BLAKE3's
//! official test vectors.
//!
//! Plain Rust with no unsafe and nothing from the kernel, so that the same
//! file builds three times: into the kernel, into the user programs'
//! library (the shell verifies the chain itself), and into the host tool
//! that verifies a chain the kernel printed (build/audit_verify.rs).
//!
//! Only what the chain needs: the plain hash with 32 bytes of output, at
//! once (hash) or in pieces (Hasher). No keyed hashing, no key derivation,
//! no longer output.

// Built into three crates (above), and not every one uses every part.
#![allow(dead_code)]

/// The output: 32 bytes.
pub const OUT_LEN: usize = 32;
const BLOCK_LEN: usize = 64;
const CHUNK_LEN: usize = 1024;

// Domain flags: which kind of compression this is.
const CHUNK_START: u32 = 1 << 0;
const CHUNK_END: u32 = 1 << 1;
const PARENT: u32 = 1 << 2;
const ROOT: u32 = 1 << 3;

/// SHA-256's initial values, as BLAKE3's.
const IV: [u32; 8] = [0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19];

/// How the message words are reordered between rounds.
const PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

/// The quarter-round, mixing two message words into one column or
/// diagonal of the state.
fn g(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(x);
    s[d] = (s[d] ^ s[a]).rotate_right(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(12);
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(y);
    s[d] = (s[d] ^ s[a]).rotate_right(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(7);
}

fn round(s: &mut [u32; 16], m: &[u32; 16]) {
    // The columns, then the diagonals.
    g(s, 0, 4, 8, 12, m[0], m[1]);
    g(s, 1, 5, 9, 13, m[2], m[3]);
    g(s, 2, 6, 10, 14, m[4], m[5]);
    g(s, 3, 7, 11, 15, m[6], m[7]);
    g(s, 0, 5, 10, 15, m[8], m[9]);
    g(s, 1, 6, 11, 12, m[10], m[11]);
    g(s, 2, 7, 8, 13, m[12], m[13]);
    g(s, 3, 4, 9, 14, m[14], m[15]);
}

/// The compression function: a 64-byte block into a chaining value, in
/// seven rounds. Returns the whole 16-word state; the first 8 words are
/// the next chaining value. Never inlined: one copy, whoever calls it.
#[inline(never)]
fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    let mut s = [
        cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7],
        IV[0], IV[1], IV[2], IV[3],
        counter as u32, (counter >> 32) as u32, block_len, flags,
    ];
    let mut m = *block;
    for r in 0..7 {
        round(&mut s, &m);
        if r < 6 {
            let mut p = [0u32; 16];
            for (i, &from) in PERMUTATION.iter().enumerate() {
                p[i] = m[from];
            }
            m = p;
        }
    }
    for i in 0..8 {
        s[i] ^= s[i + 8];
        s[i + 8] ^= cv[i];
    }
    s
}

fn first_8(words: [u32; 16]) -> [u32; 8] {
    let mut out = [0u32; 8];
    out.copy_from_slice(&words[..8]);
    out
}

/// A block's bytes as 16 little-endian words (the block is zero-padded).
fn words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    let mut w = [0u32; 16];
    for (i, word) in w.iter_mut().enumerate() {
        *word = u32::from_le_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
    }
    w
}

/// What is left to compress at the end of a chunk or a parent node: as a
/// chaining value inside the tree, or with ROOT as the hash itself.
#[derive(Clone, Copy)]
struct Output {
    cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    block_len: u32,
    flags: u32,
}

impl Output {
    fn chaining_value(&self) -> [u32; 8] {
        first_8(compress(&self.cv, &self.block, self.counter, self.block_len, self.flags))
    }

    fn root(&self) -> [u8; OUT_LEN] {
        let s = compress(&self.cv, &self.block, 0, self.block_len, self.flags | ROOT);
        let mut out = [0u8; OUT_LEN];
        for (i, word) in s[..8].iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// One 1 KiB chunk, filled a block at a time.
#[derive(Clone, Copy)]
struct Chunk {
    cv: [u32; 8],
    counter: u64,
    block: [u8; BLOCK_LEN],
    block_len: u8,
    blocks_compressed: u8,
}

impl Chunk {
    const fn new(counter: u64) -> Self {
        Chunk { cv: IV, counter, block: [0; BLOCK_LEN], block_len: 0, blocks_compressed: 0 }
    }

    fn len(&self) -> usize {
        BLOCK_LEN * self.blocks_compressed as usize + self.block_len as usize
    }

    fn start_flag(&self) -> u32 {
        if self.blocks_compressed == 0 { CHUNK_START } else { 0 }
    }

    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            // A full block with more input to come: compress it (not the
            // chunk's last, so no CHUNK_END) and start the next.
            if self.block_len as usize == BLOCK_LEN {
                let s = compress(&self.cv, &words(&self.block), self.counter, BLOCK_LEN as u32, self.start_flag());
                self.cv = first_8(s);
                self.blocks_compressed += 1;
                self.block = [0; BLOCK_LEN];
                self.block_len = 0;
            }
            let at = self.block_len as usize;
            let take = (BLOCK_LEN - at).min(input.len());
            self.block[at..at + take].copy_from_slice(&input[..take]);
            self.block_len += take as u8;
            input = &input[take..];
        }
    }

    fn output(&self) -> Output {
        Output {
            cv: self.cv,
            block: words(&self.block),
            counter: self.counter,
            block_len: self.block_len as u32,
            flags: self.start_flag() | CHUNK_END,
        }
    }
}

fn parent(left: [u32; 8], right: [u32; 8]) -> Output {
    let mut block = [0u32; 16];
    block[..8].copy_from_slice(&left);
    block[8..].copy_from_slice(&right);
    Output { cv: IV, block, counter: 0, block_len: BLOCK_LEN as u32, flags: PARENT }
}

/// BLAKE3 over input given in pieces: update as often as needed, then
/// finalize. Pieces of any size give the same hash as the whole at once.
#[derive(Clone, Copy)]
pub struct Hasher {
    chunk: Chunk,
    /// The chaining values of the completed subtrees on the tree's left
    /// edge, one per bit set in the number of chunks done: 54 is enough
    /// for 2^64 bytes.
    stack: [[u32; 8]; 54],
    stack_len: u8,
}

impl Hasher {
    pub const fn new() -> Self {
        Hasher { chunk: Chunk::new(0), stack: [[0; 8]; 54], stack_len: 0 }
    }

    pub fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            // A full chunk with more input to come: fold it into the tree.
            if self.chunk.len() == CHUNK_LEN {
                let mut cv = self.chunk.output().chaining_value();
                let mut chunks = self.chunk.counter + 1;
                // Each trailing 0 bit in the new number of chunks is a
                // subtree this chunk completes: merge it with its left half.
                while chunks & 1 == 0 {
                    self.stack_len -= 1;
                    cv = parent(self.stack[self.stack_len as usize], cv).chaining_value();
                    chunks >>= 1;
                }
                self.stack[self.stack_len as usize] = cv;
                self.stack_len += 1;
                self.chunk = Chunk::new(self.chunk.counter + 1);
            }
            let take = (CHUNK_LEN - self.chunk.len()).min(input.len());
            self.chunk.update(&input[..take]);
            input = &input[take..];
        }
    }

    /// The hash of everything given so far. The hasher is unchanged, so
    /// more can still be given.
    pub fn finalize(&self) -> [u8; OUT_LEN] {
        // Up the tree's right edge to the root.
        let mut out = self.chunk.output();
        let mut n = self.stack_len as usize;
        while n > 0 {
            n -= 1;
            out = parent(self.stack[n], out.chaining_value());
        }
        out.root()
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

/// BLAKE3 of `input`.
pub fn hash(input: &[u8]) -> [u8; OUT_LEN] {
    let mut h = Hasher::new();
    h.update(input);
    h.finalize()
}

/// BLAKE3's official test vectors (test_vectors/test_vectors.json in the
/// BLAKE3 repository): the input is `len` bytes 0, 1, ..., 250, 0, 1, ...
/// and these are the first 32 bytes of its hash. They cover inputs inside
/// one block, across blocks, across chunks, and trees of up to 100 chunks.
pub const TEST_VECTORS: [(usize, &str); 34] = [
    (0, "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
    (1, "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"),
    (2, "7b7015bb92cf0b318037702a6cdd81dee41224f734684c2c122cd6359cb1ee63"),
    (3, "e1be4d7a8ab5560aa4199eea339849ba8e293d55ca0a81006726d184519e647f"),
    (4, "f30f5ab28fe047904037f77b6da4fea1e27241c5d132638d8bedce9d40494f32"),
    (5, "b40b44dfd97e7a84a996a91af8b85188c66c126940ba7aad2e7ae6b385402aa2"),
    (6, "06c4e8ffb6872fad96f9aaca5eee1553eb62aed0ad7198cef42e87f6a616c844"),
    (7, "3f8770f387faad08faa9d8414e9f449ac68e6ff0417f673f602a646a891419fe"),
    (8, "2351207d04fc16ade43ccab08600939c7c1fa70a5c0aaca76063d04c3228eaeb"),
    (63, "e9bc37a594daad83be9470df7f7b3798297c3d834ce80ba85d6e207627b7db7b"),
    (64, "4eed7141ea4a5cd4b788606bd23f46e212af9cacebacdc7d1f4c6dc7f2511b98"),
    (65, "de1e5fa0be70df6d2be8fffd0e99ceaa8eb6e8c93a63f2d8d1c30ecb6b263dee"),
    (127, "d81293fda863f008c09e92fc382a81f5a0b4a1251cba1634016a0f86a6bd640d"),
    (129, "683aaae9f3c5ba37eaaf072aed0f9e30bac0865137bae68b1fde4ca2aebdcb12"),
    (1023, "10108970eeda3eb932baac1428c7a2163b0e924c9a9e25b35bba72b28f70bd11"),
    (1024, "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7"),
    (1025, "d00278ae47eb27b34faecf67b4fe263f82d5412916c1ffd97c8cb7fb814b8444"),
    (2048, "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a"),
    (2049, "5f4d72f40d7a5f82b15ca2b2e44b1de3c2ef86c426c95c1af0b6879522563030"),
    (3072, "b98cb0ff3623be03326b373de6b9095218513e64f1ee2edd2525c7ad1e5cffd2"),
    (3073, "7124b49501012f81cc7f11ca069ec9226cecb8a2c850cfe644e327d22d3e1cd3"),
    (4096, "015094013f57a5277b59d8475c0501042c0b642e531b0a1c8f58d2163229e969"),
    (4097, "9b4052b38f1c5fc8b1f9ff7ac7b27cd242487b3d890d15c96a1c25b8aa0fb995"),
    (5120, "9cadc15fed8b5d854562b26a9536d9707cadeda9b143978f319ab34230535833"),
    (5121, "628bd2cb2004694adaab7bbd778a25df25c47b9d4155a55f8fbd79f2fe154cff"),
    (6144, "3e2e5b74e048f3add6d21faab3f83aa44d3b2278afb83b80b3c35164ebeca205"),
    (6145, "f1323a8631446cc50536a9f705ee5cb619424d46887f3c376c695b70e0f0507f"),
    (7168, "61da957ec2499a95d6b8023e2b0e604ec7f6b50e80a9678b89d2628e99ada77a"),
    (7169, "a003fc7a51754a9b3c7fae0367ab3d782dccf28855a03d435f8cfe74605e7817"),
    (8192, "aae792484c8efe4f19e2ca7d371d8c467ffb10748d8a5a1ae579948f718a2a63"),
    (8193, "bab6c09cb8ce8cf459261398d2e7aef35700bf488116ceb94a36d0f5f1b7bc3b"),
    (16384, "f875d6646de28985646f34ee13be9a576fd515f76b5b0a26bb324735041ddde4"),
    (31744, "62b6960e1a44bcc1eb1a611a8d6235b6b4b78f32e7abc4fb4c6cdcce94895c47"),
    (102400, "bc3e3d41a1146b069abffad3c0d44860cf664390afce4d9661f7902e7943e085"),
];

/// A test vector's input byte at `i`.
pub fn test_input(i: usize) -> u8 {
    (i % 251) as u8
}

/// Lowercase hex of a hash, for comparing with TEST_VECTORS and printing.
pub fn hex(h: &[u8; OUT_LEN]) -> [u8; 2 * OUT_LEN] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 2 * OUT_LEN];
    for (i, b) in h.iter().enumerate() {
        out[2 * i] = DIGITS[(b >> 4) as usize];
        out[2 * i + 1] = DIGITS[(b & 15) as usize];
    }
    out
}

/// How many of the test vectors this implementation gets right, hashing
/// each input at once and again fed in uneven pieces (1, 2, 3, ... bytes),
/// so that buffering across blocks and chunks is checked too. Needs no
/// heap: the input is made 1 KiB at a time.
pub fn test_vectors_passed() -> usize {
    let mut passed = 0;
    for &(len, expected) in TEST_VECTORS.iter() {
        let mut whole = Hasher::new();
        let mut pieces = Hasher::new();
        let mut buf = [0u8; 1024];
        let mut done = 0;
        let mut piece = 1;
        while done < len {
            let n = (len - done).min(buf.len());
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = test_input(done + i);
            }
            whole.update(&buf[..n]);
            let mut at = 0;
            while at < n {
                let take = piece.min(n - at);
                pieces.update(&buf[at..at + take]);
                at += take;
                piece = piece % 97 + 1;
            }
            done += n;
        }
        let a = hex(&whole.finalize());
        let b = hex(&pieces.finalize());
        if &a[..] == expected.as_bytes() && &b[..] == expected.as_bytes() {
            passed += 1;
        }
    }
    passed
}
