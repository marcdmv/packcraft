//! PPMd variant H (PPMd7) decoder with the carryless range coder RAR 2.9 to
//! 4.x uses for its PPMd blocks.
//!
//! The model (`memory.rs`, `model.rs`, `model/decoder.rs`) is taken from the
//! `ppmd-rust` crate 1.5.2, a Rust port of Igor Pavlov's PPMd code from 7-Zip,
//! which is based on PPMd var.H (2001) by Dmitry Shkarin; the carryless range
//! coder is by Dmitry Subbotin (1999). Those authors put their code in the
//! public domain and `ppmd-rust` is licensed CC0-1.0 OR MIT-0. It is vendored
//! (not used as a dependency) because RAR restarts the range coder between
//! blocks while keeping the model, which the crate's public API cannot do.
//! Changes: encoder parts removed, the range decoder takes its bytes from
//! the RAR bit stream and is re-initialised per block.

#![allow(dead_code, clippy::all)]

mod memory;
mod model;

use memory::{CtxRef, Memory, StateRef, Successor, UnitRef, STATE_SIZE, UNIT_SIZE};

const PPMD_INT_BITS: u32 = 7;
const PPMD_PERIOD_BITS: u32 = 7;
const PPMD_BIN_SCALE: u32 = 1 << (PPMD_INT_BITS + PPMD_PERIOD_BITS);

const fn ppmd_get_mean_spec(summ: u32, shift: u32, round: u32) -> u32 {
    (summ + (1 << (shift - round))) >> shift
}

const fn ppmd_get_mean(summ: u32) -> u32 {
    ppmd_get_mean_spec(summ, PPMD_PERIOD_BITS, 2)
}

const fn ppmd_update_prob_1(prob: u32) -> u32 {
    prob - ppmd_get_mean(prob)
}

const PPMD_N1: u32 = 4;
const PPMD_N2: u32 = 4;
const PPMD_N3: u32 = 4;
const PPMD_N4: u32 = (128 + 3 - PPMD_N1 - 2 * PPMD_N2 - 3 * PPMD_N3) / 4;
const PPMD_NUM_INDEXES: u32 = PPMD_N1 + PPMD_N2 + PPMD_N3 + PPMD_N4;

enum SeeSource {
    Dummy,
    Table(usize, usize),
}

#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct See {
    summ: u16,
    shift: u8,
    count: u8,
}

impl See {
    fn update(&mut self) {
        if (self.shift as i32) < 7 && {
            self.count = self.count.wrapping_sub(1);
            self.count as i32 == 0
        } {
            self.summ = ((self.summ as i32) << 1) as u16;
            let fresh = self.shift;
            self.shift = self.shift.wrapping_add(1);
            self.count = (3 << fresh as i32) as u8;
        }
    }
}

pub(crate) use model::PPMd7;

pub(crate) const SYM_END: i32 = -1;
pub(crate) const SYM_ERROR: i32 = -2;

pub(crate) const MIN_ORDER: u32 = 2;
pub(crate) const MAX_ORDER: u32 = 64;

pub(crate) enum Error {
    MemoryAllocation,
}

/// Where the range decoder gets its bytes from.
pub(crate) trait ByteIn {
    fn byte(&mut self) -> u8;
}

const K_TOP: u32 = 1 << 24;
const K_BOT: u32 = 1 << 15;

/// Subbotin's carryless range decoder as RAR uses it (Ppmd7a in 7-Zip).
#[derive(Default)]
pub(crate) struct RarRc {
    range: u32,
    code: u32,
    low: u32,
}

impl RarRc {
    /// Starts a new block: reads four bytes. False when the stream is invalid.
    pub(crate) fn init<I: ByteIn>(&mut self, inp: &mut I) -> bool {
        self.range = 0xFFFF_FFFF;
        self.low = 0;
        self.code = 0;
        for _ in 0..4 {
            self.code = self.code << 8 | inp.byte() as u32;
        }
        self.code != 0xFFFF_FFFF
    }

    #[inline(always)]
    fn normalize<I: ByteIn>(&mut self, inp: &mut I) {
        while self.low ^ self.low.wrapping_add(self.range) < K_TOP
            || self.range < K_BOT && {
                self.range = 0u32.wrapping_sub(self.low) & (K_BOT - 1);
                true
            }
        {
            self.code = self.code << 8 | inp.byte() as u32;
            self.range <<= 8;
            self.low <<= 8;
        }
    }

    #[inline(always)]
    fn range(&self) -> u32 {
        self.range
    }

    #[inline(always)]
    fn code(&self) -> u32 {
        self.code
    }

    #[inline(always)]
    fn get_threshold(&mut self, total: u32) -> u32 {
        self.range /= total;
        self.code / self.range
    }

    #[inline(always)]
    fn decode_bit_0<I: ByteIn>(&mut self, inp: &mut I, size: u32) -> Result<(), std::io::Error> {
        self.range = size;
        self.normalize(inp);
        Ok(())
    }

    #[inline(always)]
    fn decode_bit_1(&mut self, size: u32) {
        self.low = self.low.wrapping_add(size);
        self.code = self.code.wrapping_sub(size);
        self.range = (self.range & !(PPMD_BIN_SCALE - 1)).wrapping_sub(size);
    }

    #[inline(always)]
    fn decode(&mut self, start: u32, size: u32) {
        let start = start.wrapping_mul(self.range);
        self.low = self.low.wrapping_add(start);
        self.code = self.code.wrapping_sub(start);
        self.range = self.range.wrapping_mul(size);
    }

    #[inline(always)]
    fn decode_final<I: ByteIn>(&mut self, inp: &mut I, start: u32, freq: u32) -> Result<(), std::io::Error> {
        self.decode(start, freq);
        self.normalize(inp);
        Ok(())
    }

    #[inline(always)]
    fn normalize_remote<I: ByteIn>(&mut self, inp: &mut I) -> Result<(), std::io::Error> {
        self.normalize(inp);
        Ok(())
    }
}

impl PPMd7<RarRc> {
    /// A fresh model of `order` (2..=64) using `mem_size` bytes.
    pub(crate) fn new_rar(order: u32, mem_size: u32) -> Result<Self, String> {
        if !(MIN_ORDER..=MAX_ORDER).contains(&order) || mem_size < 2048 || mem_size > u32::MAX - 36 {
            return Err(format!("invalid PPMd parameters (order {order}, memory {mem_size})"));
        }
        Self::construct(RarRc::default(), order, mem_size).map_err(|_| "out of memory for the PPMd model".to_string())
    }

    pub(crate) fn rc_mut(&mut self) -> &mut RarRc {
        &mut self.rc
    }
}
