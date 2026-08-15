//! # sdr-fox-airspy
//!
//! Airspy R2 / Mini SDR driver. Implements the same `SdrDevice` /
//! `SdrBackend` traits as `sdr-fox-rtlsdr`, so consumers can swap hardware
//! behind one interface.
//!
//! Handles the Airspy Mini's 2× real-sample streaming mode required by the
//! SondeFox Android integration.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_errors_doc)]
// DSP/register math intentionally casts integer widths/signs.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_lossless)] // `as` form is conventional in DSP; `From` is noisier here

pub mod airspy;
pub mod iq_synth;

pub use airspy::{Airspy, AirspyBackend};
pub use iq_synth::IqSynthesizer;
