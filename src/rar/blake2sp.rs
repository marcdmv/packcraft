//! BLAKE2s (RFC 7693) and BLAKE2sp, the 8-way tree mode from the BLAKE2
//! paper (Aumasson, Neves, Wilcox-O'Hearn, Winnerlein, 2013), which RAR 5 can
//! store instead of CRC32. Written from those documents.

const IV: [u32; 8] = [0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// Streaming BLAKE2s with a 32-byte digest, no key, and the tree parameters
/// BLAKE2sp needs.
#[derive(Clone)]
pub struct Blake2s {
    h: [u32; 8],
    t: u64,
    buf: [u8; 64],
    len: usize,
    last_node: bool,
}

impl Blake2s {
    /// `fanout`, `depth`, `node_offset`, `node_depth` and `inner_len` as in
    /// the BLAKE2 parameter block; (1, 1, 0, 0, 0) is plain BLAKE2s-256.
    pub fn with_params(fanout: u8, depth: u8, node_offset: u64, node_depth: u8, inner_len: u8, last_node: bool) -> Self {
        let p = [
            32 | (fanout as u32) << 16 | (depth as u32) << 24,
            0,
            node_offset as u32,
            ((node_offset >> 32) as u32 & 0xFFFF) | (node_depth as u32) << 16 | (inner_len as u32) << 24,
            0,
            0,
            0,
            0,
        ];
        let mut h = IV;
        for i in 0..8 {
            h[i] ^= p[i];
        }
        Blake2s { h, t: 0, buf: [0; 64], len: 0, last_node }
    }

    pub fn new() -> Self {
        Self::with_params(1, 1, 0, 0, 0, false)
    }

    fn compress(&mut self, block: &[u8; 64], last: bool) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= self.t as u32;
        v[13] ^= (self.t >> 32) as u32;
        if last {
            v[14] = !v[14];
            if self.last_node {
                v[15] = !v[15];
            }
        }
        #[inline(always)]
        fn g(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
            v[d] = (v[d] ^ v[a]).rotate_right(16);
            v[c] = v[c].wrapping_add(v[d]);
            v[b] = (v[b] ^ v[c]).rotate_right(12);
            v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
            v[d] = (v[d] ^ v[a]).rotate_right(8);
            v[c] = v[c].wrapping_add(v[d]);
            v[b] = (v[b] ^ v[c]).rotate_right(7);
        }
        for s in &SIGMA {
            g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            self.h[i] ^= v[i] ^ v[i + 8];
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // The last block is compressed in `finish`, so a full buffer is
            // only compressed once more input arrives.
            if self.len == 64 {
                self.t += 64;
                let b = self.buf;
                self.compress(&b, false);
                self.len = 0;
            }
            let n = (64 - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        self.t += self.len as u64;
        self.buf[self.len..].fill(0);
        let b = self.buf;
        self.compress(&b, true);
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

/// BLAKE2sp: input 64-byte blocks go round-robin to 8 leaves; the root hashes
/// the 8 leaf digests.
pub struct Blake2sp {
    leaves: Vec<Blake2s>,
    pos: u64,
}

impl Default for Blake2sp {
    fn default() -> Self {
        Self::new()
    }
}

impl Blake2sp {
    pub fn new() -> Self {
        Blake2sp { leaves: (0..8).map(|i| Blake2s::with_params(8, 2, i, 0, 32, i == 7)).collect(), pos: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let leaf = ((self.pos / 64) % 8) as usize;
            let n = (64 - (self.pos % 64) as usize).min(data.len());
            self.leaves[leaf].update(&data[..n]);
            self.pos += n as u64;
            data = &data[n..];
        }
    }

    pub fn finish(self) -> [u8; 32] {
        let mut root = Blake2s::with_params(8, 2, 0, 1, 32, true);
        for l in self.leaves {
            root.update(&l.finish());
        }
        root.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn blake2s_rfc7693_abc() {
        let mut h = Blake2s::new();
        h.update(b"abc");
        assert_eq!(hex(&h.finish()), "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982");
    }

    #[test]
    fn blake2s_empty_and_split_updates_agree() {
        assert_eq!(hex(&Blake2s::new().finish()), "69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9");
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut a = Blake2s::new();
        a.update(&data);
        let mut b = Blake2s::new();
        for c in data.chunks(7) {
            b.update(c);
        }
        assert_eq!(a.finish(), b.finish());
    }
}
