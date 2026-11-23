//! PackCraft: a content-aware archiver. Each file is routed to the coder that
//! models its type best (context mixing for text and binaries, Lepton for JPEG,
//! an adaptive predictor for PCM audio) inside one `.pcraft` container.

pub mod archive;
pub mod audio;
pub mod cm;
pub mod coder;
pub mod jpeg;
