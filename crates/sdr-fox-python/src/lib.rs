//! Python bindings for sdr-fox (PyO3). Built with `maturin develop`.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::used_underscore_binding)]
// PyO3 dunder methods have protocol-mandated signatures.
#![allow(clippy::unused_self)]
#![allow(clippy::wildcard_enum_match_arm)]
#![allow(clippy::match_wildcard_for_single_variants)]
#![allow(clippy::unnecessary_wraps)]

mod binding_impl;

pub use binding_impl::{convert_cu8_to_cf32, sdr_fox, IqBlockPy, SdrFox, StreamStatsPy};
