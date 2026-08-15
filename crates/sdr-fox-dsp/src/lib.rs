//! # sdr-fox-dsp
//!
//! Demodulators (WBFM/NBFM/AM/SSB), ADS-B decoding, and WAV/PNG artifact
//! writers for sdr-fox. The algorithms are standard DSP techniques —
//! polar-discriminator FM, envelope AM, phasing-method SSB, non-coherent
//! Mode S detection — implemented independently for this crate. They cover
//! the same jobs as Osmocom's `rtl_fm`/`rtl_adsb` utilities but are not
//! ports of that code.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::bool_to_int_with_if)]
#![allow(clippy::unreadable_literal)]
#![allow(clippy::unusual_byte_groupings)]
#![allow(clippy::trivially_copy_pass_by_ref)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::identity_op)]
#![allow(clippy::unnecessary_cast)]

pub mod adsb;
pub mod channelizer;
pub mod demods;
pub mod discrim;
pub mod filters;
pub mod png;
pub mod spurs;
pub mod wav;

pub use channelizer::{ChannelizerError, PolyphaseChannelizer};
pub use demods::{am_demod, fm_demod, lsb_demod, nbfm_demod, usb_demod, FmDemod};
pub use spurs::{reference_harmonics, SpurCanceller};
