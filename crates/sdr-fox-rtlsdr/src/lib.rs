//! # sdr-fox-rtlsdr
//!
//! RTL2832U demodulator control plane plus a pluggable multi-tuner driver.
//!
//! This is the heart of sdr-fox. The control plane (`rtl2832`) speaks the
//! RTL2832U register protocol with the firmware-mandated encoding
//! (`block<<8 | 0x10` write-bit, `addr | 0x20` demod-page-bit). Tuners are
//! behind the `Tuner` trait; one module per chip.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// DSP/register math intentionally casts integer widths and signs; for any
// realistic frequency/gain value these are loss-free. Intentional.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]
// Trait/struct method docs land with each module; allow missing_errors_doc
// where functions return Result but the contract is obvious from context.
#![allow(clippy::missing_errors_doc)]

pub mod rtl2832;
pub mod tuners;

pub use rtl2832::RtlSdr;
pub use rtl2832::RtlSdrBackend;
