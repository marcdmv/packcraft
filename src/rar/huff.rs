//! Canonical Huffman decoding shared by the RAR 4 and RAR 5 decoders: a
//! quick lookup table for short codes plus per-length limits, as in
//! libarchive's RAR 5 reader (BSD-2-Clause, Grzegorz Antoniak; notice in
//! `v5.rs`). Code lengths are at most 15 bits; codes are assigned in order
//! of length, then symbol.

/// The largest alphabet either format uses (RAR 5 literal/length table).
pub(crate) const MAX_SYMS: usize = 306;

/// A bit stream the decoder can look 16 bits into.
pub(crate) trait Peek {
    fn peek16(&self) -> u32;
    fn skip(&mut self, n: u32);
}

#[derive(Clone)]
pub(crate) struct Huff {
    size: u32,
    quick_bits: u32,
    decode_len: [u32; 16],
    decode_pos: [u32; 16],
    quick_len: [u8; 1024],
    quick_num: [u16; 1024],
    decode_num: [u16; MAX_SYMS],
}

impl Huff {
    pub(crate) fn new(lengths: &[u8]) -> Box<Huff> {
        let size = lengths.len();
        let mut t = Box::new(Huff {
            size: size as u32,
            quick_bits: if size >= 256 { 10 } else { 7 },
            decode_len: [0; 16],
            decode_pos: [0; 16],
            quick_len: [0; 1024],
            quick_num: [0; 1024],
            decode_num: [0; MAX_SYMS],
        });
        let mut lc = [0u32; 16];
        for &l in lengths {
            lc[(l & 15) as usize] += 1;
        }
        lc[0] = 0;
        let mut upper: u32 = 0;
        for i in 1..16 {
            upper = upper.wrapping_add(lc[i]);
            t.decode_len[i] = upper.wrapping_shl(16 - i as u32);
            t.decode_pos[i] = t.decode_pos[i - 1].wrapping_add(lc[i - 1]);
            upper = upper.wrapping_shl(1);
        }
        let mut pos = t.decode_pos;
        for (i, &l) in lengths.iter().enumerate() {
            let l = (l & 15) as usize;
            if l > 0 {
                let p = pos[l] as usize;
                if p < MAX_SYMS {
                    t.decode_num[p] = i as u16;
                }
                pos[l] = pos[l].wrapping_add(1);
            }
        }
        let quick = 1usize << t.quick_bits;
        let mut cur = 1usize;
        for code in 0..quick {
            let field = (code as u32) << (16 - t.quick_bits);
            while cur < 16 && field >= t.decode_len[cur] {
                cur += 1;
            }
            t.quick_len[code] = cur as u8;
            let dist = (field as i64 - t.decode_len[cur - 1] as i64) >> (16 - cur.min(16));
            let p = t.decode_pos[cur & 15] as i64 + dist;
            t.quick_num[code] = if cur < 16 && p >= 0 && (p as u32) < t.size { t.decode_num[p as usize] } else { 0 };
        }
        t
    }

    #[inline(always)]
    pub(crate) fn decode<B: Peek>(&self, b: &mut B) -> u16 {
        let field = b.peek16() & 0xfffe;
        if field < self.decode_len[self.quick_bits as usize] {
            let code = (field >> (16 - self.quick_bits)) as usize;
            b.skip(self.quick_len[code] as u32);
            return self.quick_num[code];
        }
        let mut bits = 15usize;
        for i in self.quick_bits as usize + 1..15 {
            if field < self.decode_len[i] {
                bits = i;
                break;
            }
        }
        b.skip(bits as u32);
        let dist = field.wrapping_sub(self.decode_len[bits - 1]) >> (16 - bits);
        let mut pos = self.decode_pos[bits].wrapping_add(dist);
        if pos >= self.size {
            pos = 0;
        }
        self.decode_num[pos as usize]
    }
}

