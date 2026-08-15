//! # sdr-fox-simd
//!
//! SIMD-accelerated IQ sample conversion and FFT spectrum for sdr-fox.
//!
//! Runtime dispatch keeps one public conversion contract across accelerated
//! and scalar implementations. See `docs/ARCHITECTURE.md`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
// SIMD and DSP code inherently casts integer indices/lengths to float for the
// arithmetic; for any realistic FFT size (≤ a few million samples) this loses
// no precision. These casts are intentional and safe here.
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
// SIMD unaligned-load intrinsics (`_mm_loadu_si128`, `_mm256_loadu_si256`,
// ...) intentionally cast a `*const u8` to a wider `__m128i`/`__m256i` pointer.
// The load itself is unaligned, so the stricter alignment of the target type is
// irrelevant; clippy's `cast_ptr_alignment` is a false positive here.
#![allow(clippy::cast_ptr_alignment)]
// `missing_errors_doc` is allowed: SIMD hot-path functions panic on contract
// violations rather than returning Result, and the panic docs suffice.
#![allow(clippy::missing_errors_doc)]

pub mod convert;
pub mod spectrum;

pub use convert::{
    convert_iq, cs16_to_cf32, cs16le_to_cf32, cs8_to_cf32, cu8_to_cf32, cu8_to_cf32_scalar,
    cu8_to_cs16, cu8_to_cs8,
};
pub use spectrum::{Normalization, Spectrum, Window};
