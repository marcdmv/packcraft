//! Binary arithmetic coder and the logistic helpers shared by every model.
//!
//! Sources: I. H. Witten, R. M. Neal, J. G. Cleary, "Arithmetic Coding for Data
//! Compression", CACM 30(6), 1987 (carry-less binary range coding); M. Mahoney,
//! "Data Compression Explained" (2013), sections 4.1 and 4.3 (logistic mixing,
//! stretch and squash in the probability domain).
//!
//! All arithmetic is integer, so an archive written on one platform decodes
//! bit-identically on every other one. The logistic curve is a hardcoded
//! 33-point table rather than a call to the platform's `exp()`.

/// Logistic curve 4096 / (1 + e^(-x/256)) sampled at x = -2048, -1920, ..., 2048.
const SQUASH_POINTS: [i32; 33] = [
    1, 2, 4, 6, 10, 17, 27, 45, 74, 120, 194, 311, 488, 747, 1102, 1546, 2048, 2550, 2994, 3349,
    3608, 3785, 3902, 3976, 4022, 4051, 4069, 4079, 4086, 4090, 4092, 4094, 4095,
];

/// Maps a stretched value (about -2047..2047) to a 12-bit probability (1..4095).
#[inline]
pub fn squash(d: i32) -> i32 {
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 1;
    }
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (SQUASH_POINTS[i] * (128 - w) + SQUASH_POINTS[i + 1] * w + 64) >> 7
}

/// Inverse of [`squash`]: 12-bit probability to the stretched (logit) domain.
pub struct Stretch {
    table: Vec<i16>,
}

impl Stretch {
    pub fn new() -> Self {
        let mut table = vec![0i16; 4096];
        let mut pi = 0usize;
        for x in -2047..=2047 {
            let v = squash(x) as usize;
            for slot in table.iter_mut().take(v + 1).skip(pi) {
                *slot = x as i16;
            }
            pi = v + 1;
        }
        for slot in table.iter_mut().skip(pi) {
            *slot = 2047;
        }
        Stretch { table }
    }

    #[inline]
    pub fn get(&self, p12: i32) -> i32 {
        self.table[p12 as usize] as i32
    }
}

impl Default for Stretch {
    fn default() -> Self {
        Self::new()
    }
}

/// Carry-less binary arithmetic encoder. `p1` is the probability that the bit
/// is 1, in 16-bit fixed point (1..65535).
pub struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder { x1: 0, x2: 0xffff_ffff, out: Vec::new() }
    }

    #[inline]
    pub fn encode(&mut self, bit: u32, p1: u32) {
        debug_assert!((1..65536).contains(&p1));
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p1 as u64) >> 16) as u32;
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        // Four bytes of x1 pin the final interval whatever the decoder reads after it.
        self.out.extend_from_slice(&self.x1.to_be_bytes());
        self.out
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = Decoder { x1: 0, x2: 0xffff_ffff, x: 0, data, pos: 0 };
        for _ in 0..4 {
            d.x = (d.x << 8) | d.next_byte();
        }
        d
    }

    #[inline]
    fn next_byte(&mut self) -> u32 {
        // Past the end the stream reads as zeros; a truncated archive then fails
        // its CRC check loudly instead of panicking here.
        let b = self.data.get(self.pos).copied().unwrap_or(0) as u32;
        self.pos += 1;
        b
    }

    #[inline]
    pub fn decode(&mut self, p1: u32) -> u32 {
        let range = (self.x2 - self.x1) as u64;
        let xmid = self.x1 + ((range * p1 as u64) >> 16) as u32;
        let bit = if self.x <= xmid {
            self.x2 = xmid;
            1
        } else {
            self.x1 = xmid + 1;
            0
        };
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next_byte();
        }
        bit
    }
}

/// Converts a 12-bit probability to the coder's 16-bit range, never 0 or 65536.
#[inline]
pub fn p12_to_p16(p: i32) -> u32 {
    ((p as u32) << 4).clamp(1, 65535)
}

/// Adaptive bit probability with a fixed adaptation shift, for small models
/// (audio residual bits) where count-based rates are not worth the memory.
#[derive(Clone, Copy)]
pub struct Bit {
    p: u16,
}

impl Bit {
    pub const fn new() -> Self {
        Bit { p: 32768 }
    }

    #[inline]
    pub fn p16(&self) -> u32 {
        (self.p as u32).clamp(64, 65472)
    }

    #[inline]
    pub fn update(&mut self, bit: u32, shift: u32) {
        if bit != 0 {
            self.p += (65535 - self.p) >> shift;
        } else {
            self.p -= self.p >> shift;
        }
    }
}

impl Default for Bit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squash_is_monotonic_and_bounded() {
        let mut prev = 0;
        for x in -2100..2100 {
            let v = squash(x);
            assert!((1..=4095).contains(&v));
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn stretch_inverts_squash() {
        let s = Stretch::new();
        for x in (-2000..2000).step_by(7) {
            let back = s.get(squash(x));
            // Squash is flat near its ends, so the inverse is exact only where it is steep.
            if (-1000..1000).contains(&x) {
                assert!((back - x).abs() <= 16, "x={x} back={back}");
            }
        }
    }

    #[test]
    fn coder_round_trips_skewed_bits() {
        let bits: Vec<u32> = (0..20000u32).map(|i| ((i * 2654435761) >> 29 == 0) as u32).collect();
        let mut e = Encoder::new();
        let mut m = Bit::new();
        for &b in &bits {
            e.encode(b, m.p16());
            m.update(b, 4);
        }
        let out = e.finish();
        assert!(out.len() < bits.len() / 8, "skewed bits should compress");
        let mut d = Decoder::new(&out);
        let mut m = Bit::new();
        for &b in &bits {
            let got = d.decode(m.p16());
            assert_eq!(got, b);
            m.update(got, 4);
        }
    }
}
