//! `sdrfox` — the sdr-fox command-line tool (thin entry point; logic in `commands`).

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
// CLI ergonomics: pedantic lints that don't apply to arg-handling code.
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_lossless)]

mod commands;
mod demod_commands;
mod device_open;
mod numeric;
mod streaming;

fn main() -> std::process::ExitCode {
    commands::main()
}
