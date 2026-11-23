//! JPEG route: lossless recompression of baseline and progressive JPEG through
//! the `lepton_jpeg` crate (Microsoft, Apache-2.0, pure Rust). It re-encodes the
//! quantised DCT coefficients with a stronger model and restores the original
//! file bit for bit.

use lepton_jpeg::{decode_lepton, encode_lepton_verify, EnabledFeatures, SingleThreadPool};
use std::io::Cursor;

pub fn looks_like_jpeg(data: &[u8]) -> bool {
    data.len() > 1024 && data.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// `None` when Lepton cannot represent the file exactly (the caller then uses
/// the generic route). `encode_lepton_verify` decodes its own output and
/// compares it with the input before returning.
pub fn compress(data: &[u8]) -> Option<Vec<u8>> {
    let features = EnabledFeatures::compat_lepton_vector_write();
    match encode_lepton_verify(data, &features, &SingleThreadPool::default()) {
        Ok((out, _)) if out.len() < data.len() => Some(out),
        _ => None,
    }
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let features = EnabledFeatures::compat_lepton_vector_read();
    let mut out = Vec::new();
    decode_lepton(&mut Cursor::new(data), &mut out, &features, &SingleThreadPool::default())
        .map_err(|e| format!("JPEG stream could not be decoded: {e}"))?;
    Ok(out)
}
