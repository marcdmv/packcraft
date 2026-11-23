//! Lossless route for uncompressed PCM WAV (16- and 24-bit, mono or stereo).
//!
//! Stereo is decorrelated into mid and side; each channel then passes a
//! fixed first-order predictor and an adaptive sign-sign LMS filter, and the
//! residual is coded bit by bit with adaptive binary models (Elias-gamma style
//! binarisation, context = recent residual magnitude).
//!
//! Sources (ideas only): RIFF/WAVE format, Microsoft Multimedia Programming
//! Interface and Data Specifications 1.0 (1991); B. Widrow, S. Stearns,
//! "Adaptive Signal Processing" (1985), sign-sign LMS; T. Robinson, "SHORTEN:
//! Simple lossless and near-lossless waveform compression", CUED/F-INFENG/TR.156
//! (1994), fixed polynomial prediction and residual coding.
//!
//! Integer arithmetic only, so decoding is bit-exact on every platform.

use crate::coder::{Bit, Decoder, Encoder};

const LMS_ORDER: usize = 32;
const MAX_NB: usize = 32;

/// What the archive needs to rebuild the file byte for byte.
struct Layout {
    channels: usize,
    bytes_per_sample: usize,
    data_start: usize,
    data_len: usize,
}

fn u16le(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32le(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn parse(wav: &[u8]) -> Option<Layout> {
    if wav.get(0..4)? != b"RIFF" || wav.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut i = 12;
    let mut fmt: Option<(u16, usize, usize)> = None;
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let size = u32le(wav, i + 4)? as usize;
        let body = i + 8;
        if id == b"fmt " {
            let tag = u16le(wav, body)?;
            let channels = u16le(wav, body + 2)? as usize;
            let bits = u16le(wav, body + 14)? as usize;
            // 0xFFFE (extensible) carries its real format in the sub-format GUID.
            let pcm = tag == 1 || (tag == 0xFFFE && size >= 40 && u16le(wav, body + 24)? == 1);
            if !pcm {
                return None;
            }
            fmt = Some((tag, channels, bits));
        } else if id == b"data" {
            let (_, channels, bits) = fmt?;
            if !(1..=2).contains(&channels) || !(bits == 16 || bits == 24) {
                return None;
            }
            let bps = bits / 8;
            let data_len = size.min(wav.len() - body);
            let frame = bps * channels;
            let data_len = data_len - data_len % frame;
            if data_len < frame * 64 {
                return None;
            }
            return Some(Layout { channels, bytes_per_sample: bps, data_start: body, data_len });
        }
        i = body + size + (size & 1);
    }
    None
}

/// Per-channel predictor state, shared by encoder and decoder.
struct Channel {
    prev: i32,
    hist: [i32; LMS_ORDER],
    w: [i32; LMS_ORDER],
    avg: u32,
    nb_bits: Vec<Bit>,
    mant_bits: Vec<Bit>,
}

impl Channel {
    fn new() -> Self {
        Channel {
            prev: 0,
            hist: [0; LMS_ORDER],
            w: [0; LMS_ORDER],
            avg: 16 << 4,
            nb_bits: vec![Bit::new(); 32 * (MAX_NB + 1)],
            mant_bits: vec![Bit::new(); (MAX_NB + 1) * 4 * 32],
        }
    }

    /// Prediction of the first-order difference from its recent history.
    fn predict(&self) -> i32 {
        let mut s: i64 = 0;
        for i in 0..LMS_ORDER {
            s += self.hist[i] as i64 * self.w[i] as i64;
        }
        (s >> 12).clamp(-(1 << 26), 1 << 26) as i32
    }

    fn adapt(&mut self, d: i32, residual: i32) {
        let sr = residual.signum();
        if sr != 0 {
            for i in 0..LMS_ORDER {
                self.w[i] = (self.w[i] + sr * self.hist[i].signum() * 4).clamp(-(1 << 16), 1 << 16);
            }
        }
        self.hist.copy_within(0..LMS_ORDER - 1, 1);
        self.hist[0] = d;
    }

    fn ctx(&self) -> usize {
        let a = self.avg >> 4;
        (32 - a.leading_zeros()) as usize
    }

    fn track(&mut self, u: u32) {
        let v = u.min(1 << 24);
        self.avg = self.avg - (self.avg >> 4) + v;
    }
}

fn zigzag(r: i32) -> u32 {
    ((r << 1) ^ (r >> 31)) as u32
}

fn unzigzag(u: u32) -> i32 {
    ((u >> 1) as i32) ^ -((u & 1) as i32)
}

fn encode_residual(e: &mut Encoder, ch: &mut Channel, r: i32) {
    let u = zigzag(r);
    let nb = (32 - u.leading_zeros()) as usize; // 0 for u == 0
    let ctx = ch.ctx();
    // Unary code for the bit length, one adaptive model per (context, step).
    for i in 0..=MAX_NB {
        let stop = (i == nb) as u32;
        let m = &mut ch.nb_bits[ctx * (MAX_NB + 1) + i];
        e.encode(stop, m.p16());
        m.update(stop, 5);
        if stop == 1 {
            break;
        }
    }
    // Bits below the leading one; the top ones are modelled, the rest nearly uniform.
    for k in (0..nb.saturating_sub(1)).rev() {
        let bit = (u >> k) & 1;
        let depth = (nb - 2 - k).min(3);
        let m = &mut ch.mant_bits[(nb * 4 + depth) * 32 + ctx];
        e.encode(bit, m.p16());
        m.update(bit, 6);
    }
    ch.track(u);
}

fn decode_residual(d: &mut Decoder, ch: &mut Channel) -> Result<i32, String> {
    let ctx = ch.ctx();
    let mut nb = None;
    for i in 0..=MAX_NB {
        let m = &mut ch.nb_bits[ctx * (MAX_NB + 1) + i];
        let stop = d.decode(m.p16());
        m.update(stop, 5);
        if stop == 1 {
            nb = Some(i);
            break;
        }
    }
    let nb = nb.ok_or("audio residual longer than any valid sample")?;
    let mut u: u32 = if nb == 0 { 0 } else { 1 };
    for k in (0..nb.saturating_sub(1)).rev() {
        let depth = (nb - 2 - k).min(3);
        let m = &mut ch.mant_bits[(nb * 4 + depth) * 32 + ctx];
        let bit = d.decode(m.p16());
        m.update(bit, 6);
        u = (u << 1) | bit;
    }
    ch.track(u);
    Ok(unzigzag(u))
}

fn read_sample(b: &[u8], bps: usize) -> i32 {
    match bps {
        2 => i16::from_le_bytes([b[0], b[1]]) as i32,
        _ => (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i8 as i32) << 16),
    }
}

fn write_sample(out: &mut Vec<u8>, v: i32, bps: usize) {
    out.extend_from_slice(&v.to_le_bytes()[..bps]);
}

/// Returns `None` when the file is not a supported PCM WAV.
/// Layout: `[u8 channels][u8 bytes/sample][u64 data_start][u64 data_len]
/// [u64 header+trailer CM length][header+trailer CM][residual stream]`.
pub fn compress(wav: &[u8], table_bits: u32) -> Option<Vec<u8>> {
    let l = parse(wav)?;
    let frames = l.data_len / (l.bytes_per_sample * l.channels);
    let mut chans: Vec<Channel> = (0..l.channels).map(|_| Channel::new()).collect();
    let mut e = Encoder::new();
    let data = &wav[l.data_start..l.data_start + l.data_len];
    let frame = l.bytes_per_sample * l.channels;
    for f in 0..frames {
        let s0 = read_sample(&data[f * frame..], l.bytes_per_sample);
        let vals = if l.channels == 2 {
            let s1 = read_sample(&data[f * frame + l.bytes_per_sample..], l.bytes_per_sample);
            let side = s0 - s1;
            let mid = s1 + (side >> 1);
            [mid, side]
        } else {
            [s0, 0]
        };
        for (c, ch) in chans.iter_mut().enumerate() {
            let x = vals[c];
            let d = x - ch.prev;
            let pred = ch.predict();
            let r = d - pred;
            encode_residual(&mut e, ch, r);
            ch.adapt(d, r);
            ch.prev = x;
        }
    }
    let residuals = e.finish();
    let mut side = Vec::with_capacity(wav.len() - l.data_len);
    side.extend_from_slice(&wav[..l.data_start]);
    side.extend_from_slice(&wav[l.data_start + l.data_len..]);
    let side = crate::cm::compress(&side, table_bits.min(18));

    let mut out = Vec::with_capacity(residuals.len() + side.len() + 32);
    out.push(l.channels as u8);
    out.push(l.bytes_per_sample as u8);
    out.extend_from_slice(&(l.data_start as u64).to_le_bytes());
    out.extend_from_slice(&(l.data_len as u64).to_le_bytes());
    out.extend_from_slice(&(side.len() as u64).to_le_bytes());
    out.extend_from_slice(&side);
    out.extend_from_slice(&residuals);
    Some(out)
}

pub fn decompress(s: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "audio stream header is damaged".to_string();
    if s.len() < 26 {
        return Err(bad());
    }
    let channels = s[0] as usize;
    let bps = s[1] as usize;
    if !(1..=2).contains(&channels) || !(bps == 2 || bps == 3) {
        return Err(bad());
    }
    let data_start = u64::from_le_bytes(s[2..10].try_into().unwrap()) as usize;
    let data_len = u64::from_le_bytes(s[10..18].try_into().unwrap()) as usize;
    let side_len = u64::from_le_bytes(s[18..26].try_into().unwrap()) as usize;
    let side_end = 26usize.checked_add(side_len).filter(|&e| e <= s.len()).ok_or_else(bad)?;
    let side = crate::cm::decompress(&s[26..side_end])?;
    if data_start > side.len() || data_len % (bps * channels) != 0 {
        return Err(bad());
    }
    let mut d = Decoder::new(&s[side_end..]);
    let mut chans: Vec<Channel> = (0..channels).map(|_| Channel::new()).collect();
    let mut out = Vec::with_capacity(side.len() + data_len);
    out.extend_from_slice(&side[..data_start]);
    let frames = data_len / (bps * channels);
    for _ in 0..frames {
        let mut vals = [0i32; 2];
        for (c, ch) in chans.iter_mut().enumerate() {
            let pred = ch.predict();
            let r = decode_residual(&mut d, ch)?;
            let dlt = r + pred;
            ch.adapt(dlt, r);
            let x = ch.prev + dlt;
            ch.prev = x;
            vals[c] = x;
        }
        if channels == 2 {
            let (mid, sd) = (vals[0], vals[1]);
            let s1 = mid - (sd >> 1);
            let s0 = sd + s1;
            write_sample(&mut out, s0, bps);
            write_sample(&mut out, s1, bps);
        } else {
            write_sample(&mut out, vals[0], bps);
        }
    }
    out.extend_from_slice(&side[data_start..]);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic WAV: two detuned tones plus deterministic noise.
    pub(crate) fn synth_wav(channels: u16, bits: u16, frames: usize) -> Vec<u8> {
        let bps = (bits / 8) as usize;
        let mut data = Vec::new();
        let mut x = 0x9E37_79B9u32;
        let amp = if bits == 16 { 12000.0 } else { 3_000_000.0 };
        for f in 0..frames {
            for c in 0..channels {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let t = f as f64 / 44100.0;
                let v = amp * (t * 440.0 * (1.0 + c as f64 * 0.01) * std::f64::consts::TAU).sin()
                    + amp * 0.3 * (t * 1234.0 * std::f64::consts::TAU).sin()
                    + ((x % 200) as f64 - 100.0);
                write_sample(&mut data, v as i32, bps);
            }
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&44100u32.to_le_bytes());
        w.extend_from_slice(&(44100 * channels as u32 * bps as u32).to_le_bytes());
        w.extend_from_slice(&(channels * bps as u16).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w.extend_from_slice(b"LIST\x04\x00\x00\x00abcd"); // trailing chunk must survive
        w
    }

    #[test]
    fn stereo_16_bit_round_trips_and_shrinks() {
        let w = synth_wav(2, 16, 30000);
        let c = compress(&w, 18).expect("PCM WAV accepted");
        assert_eq!(decompress(&c).unwrap(), w);
        // The synthetic noise carries about 8 bits per sample, so ~50% is the floor.
        assert!(c.len() * 100 < w.len() * 60, "{} of {}", c.len(), w.len());
        // The reason this route exists: it must beat the generic coder on audio.
        let generic = crate::cm::compress(&w, 18).len();
        assert!(c.len() * 10 < generic * 9, "audio {} vs generic {generic}", c.len());
    }

    #[test]
    fn mono_24_bit_round_trips() {
        let w = synth_wav(1, 24, 20000);
        let c = compress(&w, 18).unwrap();
        assert_eq!(decompress(&c).unwrap(), w);
    }

    #[test]
    fn extreme_samples_round_trip() {
        let mut w = synth_wav(2, 16, 1000);
        let start = 44;
        for (i, b) in w[start..start + 4000].iter_mut().enumerate() {
            *b = if i % 4 < 2 { 0xff } else { 0x80 };
        }
        let c = compress(&w, 18).unwrap();
        assert_eq!(decompress(&c).unwrap(), w);
    }

    #[test]
    fn non_wav_and_float_wav_are_rejected() {
        assert!(compress(b"not a wav at all", 18).is_none());
        let mut w = synth_wav(1, 16, 1000);
        w[20] = 3; // IEEE float format tag
        assert!(compress(&w, 18).is_none());
    }
}
