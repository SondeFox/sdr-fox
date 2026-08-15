//! Live hardware validation tests for sdr-fox.
//!
//! All tests are `#[ignore]` — run via:
//! ```sh
//! cargo test -p sdr-fox-hardware -- --ignored
//! ```
//!
//! Each test is assertion-driven (physics-based, not just "didn't crash") and
//! saves an artifact to `tests/artifacts/`.

#![cfg(test)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::needless_pass_by_value)]

pub mod airspy;
pub mod cross_cutting;
pub mod helpers;
pub mod rtl_sdr;
