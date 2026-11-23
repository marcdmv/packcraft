//! Context-mixing compressor: the generic route for text, project files,
//! tables and binaries.
//!
//! Each bit is predicted by several context models, their predictions are
//! combined by online logistic mixing, refined by an adaptive probability map
//! and coded with the binary arithmetic coder.
//!
//! Sources (behaviour and ideas only, no code consulted):
//! - M. Mahoney, "Data Compression Explained" (2013): 4.3 logistic mixing,
//!   4.3.3 adaptive probability maps (SSE), 4.2 indirect and match modeling.
//! - M. Mahoney, "Adaptive Weighing of Context Models for Lossless Data
//!   Compression", Florida Tech. Tech. Report CS-2005-16 (2005).
//! - J. G. Cleary, I. H. Witten, "Data Compression Using Adaptive Coding and
//!   Partial String Matching", IEEE Trans. Comm. 32(4), 1984 (order-n contexts).
//!
//! Every operation is integer arithmetic, so output is identical on every platform.

use crate::coder::{p12_to_p16, squash, Decoder, Encoder, Stretch};

/// Number of hashed context models (orders 1, 2, 3, 4, 6, two word models,
/// a column model and a sparse model).
const NH: usize = 9;
/// Mixer inputs: the hashed models, order 0, the match model and a bias.
const NIN: usize = NH + 3;
/// Bytes hashed to find a match candidate.
const MATCH_MIN: usize = 6;
/// Mixer learning rate (error multiplier).
const LR: i32 = 6;
/// Probability counter: high 22 bits probability, low 10 bits count.
const COUNTER_INIT: u32 = (1 << 21) << 10;

#[inline]
fn hash2(a: u32, b: u32) -> u32 {
    let h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77).rotate_left(13);
    let h = h ^ (h >> 15);
    h.wrapping_mul(0x2C1B_3C6D) ^ (h >> 13)
}

/// Reciprocal table: adaptation rate 1/(n + 1.5) in 16-bit fixed point.
fn reciprocals() -> [u32; 1024] {
    let mut r = [0u32; 1024];
    for (n, v) in r.iter_mut().enumerate() {
        *v = (65536 * 2 / (2 * n as u32 + 3)).max(1);
    }
    r
}

#[inline]
fn counter_update(e: &mut u32, bit: u32, limit: u32, rec: &[u32; 1024]) {
    let p = (*e >> 10) as i64;
    let n = *e & 1023;
    let target: i64 = if bit != 0 { (1 << 22) - 1 } else { 0 };
    let p = (p + (((target - p) * rec[n as usize] as i64) >> 16)).clamp(0, (1 << 22) - 1);
    let n = if n < limit { n + 1 } else { n };
    *e = ((p as u32) << 10) | n;
}

#[inline]
fn counter_p12(e: u32) -> i32 {
    ((e >> 20) as i32).clamp(1, 4095)
}

pub struct Predictor {
    st: Stretch,
    rec: [u32; 1024],
    bucket_bits: u32,
    tables: Vec<Vec<u32>>,
    hashes: [u32; NH],
    bucket: [usize; NH],
    slot: [usize; NH],
    o0: Vec<u32>,
    // Match model.
    mm_table: Vec<u32>,
    mm_bits: u32,
    mm_ptr: usize,
    mm_len: usize,
    mm_sm: Vec<u32>,
    mm_ctx: Option<usize>,
    // History and byte-level state.
    hist: Vec<u8>,
    c0: u32,
    bpos: u32,
    c8: u64,
    word: u32,
    prev_word: u32,
    line_start: usize,
    prev_line_start: usize,
    // Mixers.
    w_a: Vec<i32>,
    w_b: Vec<i32>,
    sel_a: usize,
    sel_b: usize,
    x: [i32; NIN],
    pr_a: i32,
    pr_b: i32,
    // Adaptive probability map.
    apm: Vec<u16>,
    apm_idx: usize,
    apm_w: i32,
    pr: i32,
}

impl Predictor {
    /// `table_bits` is log2 of the slots per context table (each slot 4 bytes).
    pub fn new(table_bits: u32) -> Self {
        let table_bits = table_bits.clamp(12, 26);
        let mm_bits = table_bits.min(22);
        let mut apm = vec![0u16; 65536 * 33];
        for ctx in 0..65536 {
            for j in 0..33 {
                apm[ctx * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16;
            }
        }
        let mut p = Predictor {
            st: Stretch::new(),
            rec: reciprocals(),
            bucket_bits: table_bits - 4,
            tables: (0..NH).map(|_| vec![0u32; 1 << table_bits]).collect(),
            hashes: [0; NH],
            bucket: [0; NH],
            slot: [0; NH],
            o0: vec![COUNTER_INIT; 256],
            mm_table: vec![0u32; 1 << mm_bits],
            mm_bits,
            mm_ptr: 0,
            mm_len: 0,
            mm_sm: vec![COUNTER_INIT; 64],
            mm_ctx: None,
            hist: Vec::new(),
            c0: 1,
            bpos: 0,
            c8: 0,
            word: 0,
            prev_word: 0,
            line_start: 0,
            prev_line_start: 0,
            w_a: vec![1 << 14; 256 * NIN],
            w_b: vec![1 << 14; 32 * NIN],
            sel_a: 0,
            sel_b: 0,
            x: [0; NIN],
            pr_a: 2048,
            pr_b: 2048,
            apm,
            apm_idx: 0,
            apm_w: 0,
            pr: 2048,
        };
        p.compute_hashes();
        p.select_buckets();
        p.predict();
        p
    }

    /// Probability that the next bit is 1, 12-bit.
    #[inline]
    pub fn p(&self) -> i32 {
        self.pr
    }

    pub fn update(&mut self, bit: u32) {
        // 1. Counters.
        for i in 0..NH {
            let limit = if i < 2 { 1023 } else { 255 };
            counter_update(&mut self.tables[i][self.slot[i]], bit, limit, &self.rec);
        }
        counter_update(&mut self.o0[self.c0 as usize], bit, 1023, &self.rec);
        if let Some(ctx) = self.mm_ctx {
            counter_update(&mut self.mm_sm[ctx], bit, 1023, &self.rec);
        }

        // 2. Mixers: gradient step on coding cost.
        let target = (bit as i32) << 12;
        let err_a = (target - self.pr_a) * LR;
        let err_b = (target - self.pr_b) * LR;
        let wa = &mut self.w_a[self.sel_a * NIN..self.sel_a * NIN + NIN];
        let wb = &mut self.w_b[self.sel_b * NIN..self.sel_b * NIN + NIN];
        for i in 0..NIN {
            wa[i] += (self.x[i] * err_a + 512) >> 10;
            wb[i] += (self.x[i] * err_b + 512) >> 10;
        }

        // 3. Adaptive probability map: move both interpolation ends toward the bit.
        let t16 = if bit != 0 { 65535i32 } else { 0 };
        let lo = &mut self.apm[self.apm_idx];
        *lo = (*lo as i32 + (((t16 - *lo as i32) * (128 - self.apm_w)) >> 13)) as u16;
        let hi = &mut self.apm[self.apm_idx + 1];
        *hi = (*hi as i32 + (((t16 - *hi as i32) * self.apm_w) >> 13)) as u16;

        // 4. Advance the bit and byte state.
        self.c0 = (self.c0 << 1) | bit;
        self.bpos += 1;
        if self.bpos == 8 {
            let byte = (self.c0 & 255) as u8;
            self.byte_done(byte);
            self.c0 = 1;
            self.bpos = 0;
        }
        if self.bpos == 0 || self.bpos == 4 {
            self.select_buckets();
        }
        self.predict();
    }

    fn byte_done(&mut self, byte: u8) {
        self.hist.push(byte);
        self.c8 = (self.c8 << 8) | byte as u64;
        let pos = self.hist.len();

        // Words: letters, digits are not words; bytes >= 128 count as letters (UTF-8).
        let lower = byte.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || byte >= 128 {
            self.word = hash2(self.word.wrapping_add(1), lower as u32);
        } else if self.word != 0 {
            self.prev_word = self.word;
            self.word = 0;
        }
        if byte == b'\n' {
            self.prev_line_start = self.line_start;
            self.line_start = pos;
        }

        // Match model: extend the current match or look up a new candidate.
        if self.mm_len > 0 && self.mm_ptr < pos - 1 && self.hist[self.mm_ptr] == byte {
            self.mm_len = (self.mm_len + 1).min(65535);
            self.mm_ptr += 1;
        } else {
            self.mm_len = 0;
        }
        if pos >= MATCH_MIN {
            let key = (self.c8 & 0xffff_ffff_ffff).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let h = (key >> (64 - self.mm_bits)) as usize;
            if self.mm_len == 0 {
                let cand = self.mm_table[h] as usize;
                if cand > 0 && cand < pos {
                    let mut l = 0;
                    while l < 48 && l < cand && self.hist[cand - 1 - l] == self.hist[pos - 1 - l] {
                        l += 1;
                    }
                    if l >= MATCH_MIN {
                        self.mm_len = l;
                        self.mm_ptr = cand;
                    }
                }
            }
            self.mm_table[h] = pos as u32;
        }
        self.compute_hashes();
    }

    fn compute_hashes(&mut self) {
        let c8 = self.c8;
        let c1 = (c8 & 0xff) as u32;
        let pos = self.hist.len();
        let col = pos - self.line_start;
        let above_idx = self.prev_line_start + col;
        let above = if above_idx < self.line_start { self.hist[above_idx] as u32 } else { 0 };
        self.hashes = [
            hash2(1, c1),
            hash2(2, (c8 & 0xffff) as u32),
            hash2(3, (c8 & 0xff_ffff) as u32),
            hash2(4, c8 as u32),
            hash2(hash2(6, c8 as u32), ((c8 >> 32) & 0xffff) as u32),
            hash2(hash2(7, self.word), c1),
            hash2(hash2(8, self.word), self.prev_word),
            hash2(hash2(9, above), c1 | ((col.min(255) as u32) << 8)),
            hash2(10, ((c8 >> 8) & 0xffff) as u32),
        ];
    }

    fn select_buckets(&mut self) {
        for i in 0..NH {
            let h = if self.bpos == 0 { self.hashes[i] } else { hash2(self.hashes[i], self.c0) };
            let b = ((h >> (32 - self.bucket_bits)) as usize) << 4;
            let check = h.wrapping_mul(0x2545_F491) | 1;
            let t = &mut self.tables[i];
            if t[b] != check {
                t[b] = check;
                t[b + 1..b + 16].fill(COUNTER_INIT);
            }
            self.bucket[i] = b;
        }
    }

    fn predict(&mut self) {
        let nib = if self.bpos < 4 {
            self.c0 as usize
        } else {
            let k = self.bpos - 4;
            ((self.c0 & ((1 << k) - 1)) | (1 << k)) as usize
        };
        for i in 0..NH {
            self.slot[i] = self.bucket[i] + nib;
            self.x[i] = self.st.get(counter_p12(self.tables[i][self.slot[i]]));
        }
        self.x[NH] = self.st.get(counter_p12(self.o0[self.c0 as usize]));

        // Match model input: only while the predicted byte agrees with the bits so far.
        self.mm_ctx = None;
        self.x[NH + 1] = 0;
        if self.mm_len > 0 {
            let pb = self.hist[self.mm_ptr] as u32;
            if (pb | 256) >> (8 - self.bpos) == self.c0 {
                let expected = (pb >> (7 - self.bpos)) & 1;
                let ctx = self.mm_len.min(31) * 2 + expected as usize;
                self.mm_ctx = Some(ctx);
                self.x[NH + 1] = self.st.get(counter_p12(self.mm_sm[ctx]));
            } else {
                self.mm_len = 0;
            }
        }
        self.x[NH + 2] = 256;

        let len_bucket = match self.mm_len {
            0 => 0,
            1..=15 => 1,
            16..=31 => 2,
            _ => 3,
        };
        self.sel_a = self.c0 as usize;
        self.sel_b = len_bucket * 8 + self.bpos as usize;
        let wa = &self.w_a[self.sel_a * NIN..self.sel_a * NIN + NIN];
        let wb = &self.w_b[self.sel_b * NIN..self.sel_b * NIN + NIN];
        let mut dot_a: i64 = 0;
        let mut dot_b: i64 = 0;
        for i in 0..NIN {
            dot_a += self.x[i] as i64 * wa[i] as i64;
            dot_b += self.x[i] as i64 * wb[i] as i64;
        }
        let da = ((dot_a >> 16) as i32).clamp(-2047, 2047);
        let db = ((dot_b >> 16) as i32).clamp(-2047, 2047);
        self.pr_a = squash(da);
        self.pr_b = squash(db);
        let pr_mix = squash((da + db) >> 1);

        let c1 = (self.c8 & 0xff) as usize;
        let s = self.st.get(pr_mix) + 2048;
        let ctx = self.c0 as usize | (c1 << 8);
        self.apm_idx = ctx * 33 + (s >> 7) as usize;
        self.apm_w = s & 127;
        let p_apm = (self.apm[self.apm_idx] as i32 * (128 - self.apm_w)
            + self.apm[self.apm_idx + 1] as i32 * self.apm_w)
            >> 11;
        self.pr = ((pr_mix + 3 * p_apm + 2) >> 2).clamp(1, 4095);
    }
}

/// Stream layout: `[u8 table_bits][u64 LE length][coded bytes]`.
pub fn compress(data: &[u8], table_bits: u32) -> Vec<u8> {
    let mut p = Predictor::new(table_bits);
    let mut e = Encoder::new();
    for &byte in data {
        for i in (0..8).rev() {
            let bit = ((byte >> i) & 1) as u32;
            e.encode(bit, p12_to_p16(p.p()));
            p.update(bit);
        }
    }
    let coded = e.finish();
    let mut out = Vec::with_capacity(coded.len() + 9);
    out.push(table_bits.clamp(12, 26) as u8);
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(&coded);
    out
}

pub fn decompress(stream: &[u8]) -> Result<Vec<u8>, String> {
    if stream.len() < 9 {
        return Err("context-mixing stream shorter than its header".into());
    }
    let table_bits = stream[0] as u32;
    if !(12..=26).contains(&table_bits) {
        return Err(format!("context-mixing stream has invalid table size {table_bits}"));
    }
    let len = u64::from_le_bytes(stream[1..9].try_into().unwrap()) as usize;
    let mut p = Predictor::new(table_bits);
    let mut d = Decoder::new(&stream[9..]);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let mut c = 0u32;
        for _ in 0..8 {
            let bit = d.decode(p12_to_p16(p.p()));
            p.update(bit);
            c = (c << 1) | bit;
        }
        out.push(c as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8]) -> usize {
        let c = compress(data, 16);
        assert_eq!(decompress(&c).unwrap(), data);
        c.len()
    }

    #[test]
    fn empty_and_tiny_inputs_round_trip() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(&[0u8, 255, 0, 255]);
    }

    #[test]
    fn text_compresses_well_below_raw() {
        let text = "the quick brown fox jumps over the lazy dog. ".repeat(400);
        let n = round_trip(text.as_bytes());
        assert!(n * 20 < text.len(), "repetitive text: {n} of {}", text.len());
    }

    #[test]
    fn csv_columns_and_binary_round_trip() {
        let mut csv = String::from("id,name,amount\n");
        for i in 0..3000 {
            csv.push_str(&format!("{},item{},{}.{:02}\n", 1000 + i, i % 37, i * 7 % 500, i % 100));
        }
        round_trip(csv.as_bytes());
        let bin: Vec<u8> = (0..50_000u32).flat_map(|i| (i * 3).to_le_bytes()).collect();
        let n = round_trip(&bin);
        assert!(n * 4 < bin.len(), "structured binary: {n} of {}", bin.len());
    }

    #[test]
    fn random_bytes_do_not_blow_up() {
        let mut x = 0x1234_5678u32;
        let data: Vec<u8> = (0..20_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let n = round_trip(&data);
        assert!(n < data.len() + data.len() / 50 + 64, "random overhead too high: {n}");
    }

    #[test]
    fn truncated_stream_is_an_error_or_wrong_bytes_never_a_panic() {
        let c = compress(b"hello hello hello", 16);
        assert!(decompress(&c[..5]).is_err());
        let _ = decompress(&c[..c.len() - 3]);
    }
}
