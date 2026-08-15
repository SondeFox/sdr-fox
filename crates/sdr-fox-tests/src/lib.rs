//! Cross-crate integration tests for sdr-fox.
//!
//! The mock-stack tests run in CI without hardware; the hardware-gated tests
//! are `#[ignore]` and run via `cargo test --ignored` against an attached
//! RTL-SDR.

#![cfg(test)]

pub mod mock_end_to_end;
