//! # sdr-fox-core
//!
//! Shared vocabulary for the sdr-fox ecosystem: error model, sample and IQ
//! types, gain model, device/tuner/transport traits.
//!
//! This crate performs **no I/O** — it only defines the contracts that the
//! `sdr-fox-transport`, `sdr-fox-rtlsdr`, and `sdr-fox-airspy` crates implement.
//!
//! See `docs/ARCHITECTURE.md` for the rationale behind this split.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::needless_doctest_main)]

pub mod device;
pub mod error;
pub mod gain;
pub mod sample;
pub mod session;
pub mod transport;
pub mod tuner;

#[cfg(test)]
mod tests;

pub use device::{DeviceDescriptor, DeviceInfo, DeviceKind, SdrBackend, SdrDevice, Upconverter};
pub use error::{SdrError, TunerError};
pub use gain::{GainMode, GainRequest, GainStageId, GainStep};
pub use sample::{IqBlock, IqFormat, IqSamples, StreamConfig, StreamHandle, StreamSink};
pub use transport::{ControlRequest, ControlType, DeviceRecipient, TransferDirection, Transport};
pub use tuner::{Tuner, TunerBus, TunerKind};
