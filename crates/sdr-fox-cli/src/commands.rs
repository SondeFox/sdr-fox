//! CLI command implementations and clap definition.
//!
//! Kept in a module separate from `main.rs` so the binary entry point stays
//! a thin shim. `main.rs` calls [`Cli::parse`] then [`run`].

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tracing_subscriber::util::SubscriberInitExt;

use crate::device_open::open_device;
use crate::numeric::FormatArg;
use crate::streaming::negotiate_sample_rate;

const WATERFALL_FFT_SIZE: usize = 256;
const WATERFALL_FRAME_FLOATS: usize = WATERFALL_FFT_SIZE * 2;
const WATERFALL_COLUMNS: usize = 64;
const WATERFALL_PERIOD: Duration = Duration::from_millis(50);
/// Bound spectrum work while retaining a stable sampling phase across USB
/// block boundaries. At 2.4 MS/s this is about 29 FFTs per display interval.
const WATERFALL_FRAME_STRIDE: usize = 16;

/// sdr-fox command-line tool.
#[derive(Parser)]
#[command(name = "sdrfox", version, about, propagate_version = true)]
pub struct Cli {
    /// Verbose logging (repeat for more detail).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List detected SDR devices.
    Devices,
    /// Print detailed info about a device.
    Info {
        /// Device index.
        #[arg(short, long, default_value_t = 0)]
        device: usize,
    },
    /// Capture IQ samples to a raw file (counterpart to the classic `rtl_sdr` tool).
    Capture(CaptureArgs),
    /// Stream samples to stdout or a terminal waterfall.
    Stream(StreamArgs),
    /// Run a throughput / dropped-sample benchmark (counterpart to the classic `rtl_test` tool).
    Test {
        /// Device index.
        #[arg(short, long, default_value_t = 0)]
        device: usize,
        /// Duration in seconds.
        #[arg(short, long, default_value_t = 10)]
        seconds: u64,
    },
    /// Toggle the bias tee (counterpart to the classic `rtl_biast` tool).
    Biast {
        /// Device index.
        #[arg(short, long, default_value_t = 0)]
        device: usize,
        /// Turn the bias tee on.
        #[arg(long)]
        on: bool,
    },
    /// Demodulation and proof commands (wbfm/nbfm/am/usb/lsb/adsb/power/sweep/bench).
    #[command(subcommand)]
    Demod(crate::demod_commands::DemodCommand),
}

#[derive(Parser, Clone)]
pub struct CaptureArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Center frequency in Hz.
    #[arg(short, long, default_value_t = 100_000_000)]
    pub frequency: u64,
    /// Sample rate in Hz.
    #[arg(short, long, default_value_t = 2_048_000)]
    pub sample_rate: u32,
    /// Number of bytes to capture (0 = unlimited until Ctrl-C).
    #[arg(short, long, default_value_t = 0)]
    pub number: u64,
    /// Output file (default: `sdrfox_capture.<format>`).
    #[arg(short, long)]
    pub output: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = FormatArg::Cu8)]
    pub format: FormatArg,
    /// Enable the SpyVerter (120 MHz HF upconverter).
    #[arg(long)]
    pub spyverter: bool,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
    /// Enable AGC.
    #[arg(long)]
    pub agc: bool,
    /// Tuner gain in tenths of dB (e.g. 400 = 40.0 dB).
    #[arg(short = 'g', long)]
    pub gain: Option<i32>,
}

#[derive(Parser, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct StreamArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Center frequency in Hz.
    #[arg(short, long, default_value_t = 100_000_000)]
    pub frequency: u64,
    /// Sample rate in Hz.
    #[arg(short, long, default_value_t = 2_048_000)]
    pub sample_rate: u32,
    /// Output format on stdout.
    #[arg(long, value_enum, default_value_t = FormatArg::Cu8)]
    pub format: FormatArg,
    /// Render a terminal waterfall (dBFS to stderr) instead of raw to stdout.
    #[arg(long)]
    pub waterfall: bool,
    /// Enable the SpyVerter (120 MHz HF upconverter).
    #[arg(long)]
    pub spyverter: bool,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
    /// Enable AGC.
    #[arg(long)]
    pub agc: bool,
}

/// Entry point: parse args, run, return an exit code.
pub fn main() -> ExitCode {
    if let Err(error) = crate::streaming::install_signal_handler() {
        eprintln!("sdrfox: error: {error}");
        return ExitCode::FAILURE;
    }
    let cli = Cli::parse();
    init_logging(cli.verbose);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sdrfox: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = logging_subscriber(filter).try_init();
}

fn logging_subscriber(filter: &str) -> impl tracing::Subscriber + Send + Sync {
    // USB recovery failures can be emitted by the stream worker while the CLI
    // is blocked in receive. Keep tracing off stdout so a raw stream may hold
    // its stdout lock without blocking the worker that must report and exit.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .finish()
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Devices => list_devices(),
        Command::Info { device } => info(device),
        Command::Capture(a) => capture(a),
        Command::Stream(a) => stream(a),
        Command::Test { device, seconds } => test_device(device, seconds),
        Command::Biast { device, on } => biast(device, on),
        Command::Demod(sub) => crate::demod_commands::run(sub),
    }
}

fn list_devices() -> Result<(), String> {
    let devices = crate::device_open::enumerate_devices().map_err(|error| error.to_string())?;
    for (index, device) in devices.iter().enumerate() {
        let location = device.location;
        println!(
            "  {index}: {} (vid={:#06x} pid={:#06x})",
            device.name, location.vendor_id, location.product_id
        );
    }
    if devices.is_empty() {
        println!("No SDR devices detected.");
    }
    Ok(())
}

fn info(device: usize) -> Result<(), String> {
    let dev = open_device(device).map_err(|e| e.to_string())?;
    let info = dev.info();
    println!("Device {device}:");
    println!(
        "  vendor/product: {:#06x}:{:#06x}",
        info.vendor_id, info.product_id
    );
    println!("  name:           {}", info.product_name);
    println!("  serial:         {}", info.serial);
    println!("  kind:           {:?}", info.kind);
    if let Some(t) = info.tuner {
        println!("  tuner:          {t:?}");
    }
    let gains = dev.gains();
    let overall: Vec<_> = gains.iter().filter(|g| g.name == "OVERALL").collect();
    if !overall.is_empty() {
        let steps: Vec<String> = overall.iter().map(|g| format!("{}", g.tenths_db)).collect();
        println!("  gain steps (tenths dB): OVERALL = [{}]", steps.join(", "));
    }
    Ok(())
}

fn capture(args: CaptureArgs) -> Result<(), String> {
    let mut dev = open_device(args.device).map_err(|e| e.to_string())?;
    negotiate_sample_rate(args.sample_rate, |requested| dev.set_sample_rate(requested))?;
    if args.spyverter {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
        dev.set_upconverter(Some(sdr_fox_core::Upconverter::spyverter()))
            .map_err(|error| error.to_string())?;
    } else if args.bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    // The upconverter must be configured before translating the requested RF
    // frequency into the device's tuner frequency.
    dev.set_frequency(args.frequency)
        .map_err(|error| error.to_string())?;
    if args.agc {
        dev.set_agc(true).map_err(|error| error.to_string())?;
    }
    if let Some(g) = args.gain {
        dev.set_gain(sdr_fox_core::GainRequest::overall(g))
            .map_err(|error| error.to_string())?;
    }
    let cfg = sdr_fox_core::StreamConfig {
        format: args.format.into(),
        ..sdr_fox_core::StreamConfig::default()
    };
    let mut stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let output = capture_output_path(&args);
    let mut out =
        std::fs::File::create(&output).map_err(|error| format!("create {output}: {error}"))?;
    let mut captured: u64 = 0;
    let mut blocks = 0u64;
    let mut scratch = Vec::new();
    while args.number == 0 || captured < args.number {
        let Some(block) =
            crate::streaming::recv_until(stream.as_mut(), None, crate::streaming::shutdown_flag())?
        else {
            break;
        };
        let bytes = crate::streaming::sample_bytes(&block.samples, &mut scratch);
        let write_len = limited_write_len(args.number, captured, bytes.len());
        out.write_all(&bytes[..write_len])
            .map_err(|e| format!("write: {e}"))?;
        captured = captured.saturating_add(write_len as u64);
        blocks = blocks.saturating_add(1);
    }
    stream.stop();
    out.flush()
        .map_err(|error| format!("flush {output}: {error}"))?;
    eprintln!("captured {captured} bytes in {blocks} blocks to {output}");
    Ok(())
}

fn limited_write_len(limit: u64, written: u64, available: usize) -> usize {
    if limit == 0 {
        return available;
    }
    available.min(usize::try_from(limit.saturating_sub(written)).unwrap_or(usize::MAX))
}

fn capture_output_path(args: &CaptureArgs) -> String {
    args.output
        .clone()
        .unwrap_or_else(|| format!("sdrfox_capture.{}", args.format.extension()))
}

fn stream(args: StreamArgs) -> Result<(), String> {
    let mut dev = open_device(args.device).map_err(|e| e.to_string())?;
    negotiate_sample_rate(args.sample_rate, |requested| dev.set_sample_rate(requested))?;
    if args.spyverter {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
        dev.set_upconverter(Some(sdr_fox_core::Upconverter::spyverter()))
            .map_err(|error| error.to_string())?;
    } else if args.bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    dev.set_frequency(args.frequency)
        .map_err(|error| error.to_string())?;
    if args.agc {
        dev.set_agc(true).map_err(|error| error.to_string())?;
    }
    let format = if args.waterfall {
        sdr_fox_core::IqFormat::Cf32
    } else {
        args.format.into()
    };
    let cfg = sdr_fox_core::StreamConfig {
        format,
        ..sdr_fox_core::StreamConfig::default()
    };
    let mut stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let stdout = std::io::stdout();
    // Raw output is the only mode that needs a long-lived stdout lock. Tracing
    // is routed to stderr, and waterfall stderr locks are scoped to one write,
    // so no output lock used by a worker spans `recv_until`.
    let mut stdout = (!args.waterfall).then(|| stdout.lock());
    let mut spec = args
        .waterfall
        .then(|| sdr_fox_simd::Spectrum::new(WATERFALL_FFT_SIZE));
    let mut assembler = args
        .waterfall
        .then(|| {
            crate::streaming::FrameAssembler::new(WATERFALL_FRAME_FLOATS, WATERFALL_FRAME_FLOATS)
        })
        .transpose()?;
    let mut column_max = [f32::NEG_INFINITY; WATERFALL_COLUMNS];
    let mut accumulated_frames = 0usize;
    let mut frames_until_analysis = 0usize;
    let mut next_render = Instant::now() + WATERFALL_PERIOD;
    let mut line = String::with_capacity(WATERFALL_COLUMNS);
    let mut scratch = Vec::new();
    let mut blocks = 0u64;
    while let Some(block) =
        crate::streaming::recv_until(stream.as_mut(), None, crate::streaming::shutdown_flag())?
    {
        if args.waterfall {
            if let sdr_fox_core::IqSamples::Cf32(f) = &block.samples {
                let spectrum = spec.as_mut().expect("waterfall spectrum");
                assembler
                    .as_mut()
                    .expect("waterfall frame assembler")
                    .push(f, |frame| {
                        if take_waterfall_frame(&mut frames_until_analysis) {
                            accumulate_waterfall(
                                spectrum.compute_power_dbfs(frame),
                                &mut column_max,
                            );
                            accumulated_frames += 1;
                        }
                    });
                if accumulated_frames != 0 && Instant::now() >= next_render {
                    render_waterfall_line_to_stderr(&column_max, &mut line)?;
                    column_max.fill(f32::NEG_INFINITY);
                    accumulated_frames = 0;
                    next_render = Instant::now() + WATERFALL_PERIOD;
                }
            } else {
                return Err(format!(
                    "waterfall received unexpected {} block",
                    block.samples.format()
                ));
            }
        } else {
            let bytes = crate::streaming::sample_bytes(&block.samples, &mut scratch);
            stdout
                .as_mut()
                .expect("raw stream stdout")
                .write_all(bytes)
                .map_err(|e| format!("write: {e}"))?;
        }
        blocks = blocks.saturating_add(1);
    }
    if args.waterfall && accumulated_frames != 0 {
        render_waterfall_line_to_stderr(&column_max, &mut line)?;
    }
    stream.stop();
    if !args.waterfall {
        stdout
            .as_mut()
            .expect("raw stream stdout")
            .flush()
            .map_err(|error| format!("flush stdout: {error}"))?;
    }
    writeln!(std::io::stderr().lock(), "streamed {blocks} blocks")
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn take_waterfall_frame(frames_until_analysis: &mut usize) -> bool {
    if *frames_until_analysis == 0 {
        *frames_until_analysis = WATERFALL_FRAME_STRIDE - 1;
        true
    } else {
        *frames_until_analysis -= 1;
        false
    }
}

fn accumulate_waterfall<const COLUMNS: usize>(power: &[f32], columns: &mut [f32; COLUMNS]) {
    let bins = power.len();
    for (column, maximum) in columns.iter_mut().enumerate() {
        let lo = column * bins / COLUMNS;
        let hi = ((column + 1) * bins / COLUMNS).max(lo + 1);
        for b in lo..hi.min(bins) {
            *maximum = (*maximum).max(power[b]);
        }
    }
}

fn render_waterfall_line(
    columns: &[f32],
    line: &mut String,
    output: &mut impl Write,
) -> Result<(), String> {
    line.clear();
    line.extend(columns.iter().copied().map(db_to_char));
    writeln!(output, "{line}").map_err(|error| format!("write waterfall: {error}"))
}

fn render_waterfall_line_to_stderr(columns: &[f32], line: &mut String) -> Result<(), String> {
    // Do not move this lock outside the call: USB recovery logs on the worker
    // thread and must be able to complete before the receiver disconnects.
    render_waterfall_line(columns, line, &mut std::io::stderr().lock())
}

fn db_to_char(db: f32) -> char {
    match db {
        d if d <= -60.0 => ' ',
        d if d < -45.0 => '.',
        d if d < -30.0 => ':',
        d if d < -15.0 => '+',
        d if d < -5.0 => '*',
        _ => '#',
    }
}

fn test_device(device: usize, seconds: u64) -> Result<(), String> {
    if seconds == 0 {
        return Err("--seconds must be greater than zero".into());
    }
    let mut dev = open_device(device).map_err(|e| e.to_string())?;
    let actual_rate = negotiate_sample_rate(2_048_000, |requested| dev.set_sample_rate(requested))?;
    let cfg = sdr_fox_core::StreamConfig::default();
    let mut stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let deadline = crate::streaming::checked_deadline(started, Duration::from_secs(seconds))?;
    let mut blocks = 0u64;
    let mut bytes = 0u64;
    let mut complex_samples = 0u64;
    let mut dropped = 0u64;
    let mut scratch = Vec::new();
    while let Some(block) = crate::streaming::recv_until(
        stream.as_mut(),
        Some(deadline),
        crate::streaming::shutdown_flag(),
    )? {
        bytes = bytes.saturating_add(
            crate::streaming::sample_bytes(&block.samples, &mut scratch).len() as u64,
        );
        complex_samples = complex_samples.saturating_add(block.samples.complex_count() as u64);
        dropped = dropped.max(block.dropped);
        blocks = blocks.saturating_add(1);
    }
    stream.stop();
    let elapsed = started.elapsed();
    let throughput = if elapsed.is_zero() {
        0.0
    } else {
        bytes as f64 / elapsed.as_secs_f64() / 1e6
    };
    let sample_rate = if elapsed.is_zero() {
        0.0
    } else {
        complex_samples as f64 / elapsed.as_secs_f64()
    };
    eprintln!(
        "test: {blocks} blocks, {bytes} bytes, {complex_samples} complex samples in {:.3}s ({throughput:.3} MB/s, {sample_rate:.0} S/s), {dropped} dropped, negotiated {actual_rate} S/s",
        elapsed.as_secs_f64()
    );
    Ok(())
}

fn biast(device: usize, on: bool) -> Result<(), String> {
    let mut dev = open_device(device).map_err(|e| e.to_string())?;
    dev.set_bias_tee(on).map_err(|e| e.to_string())?;
    eprintln!("bias tee: {}", if on { "ON" } else { "OFF" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use clap::CommandFactory;

    use super::*;

    fn capture_args(format: FormatArg) -> CaptureArgs {
        CaptureArgs {
            device: 0,
            frequency: 100_000_000,
            sample_rate: 2_048_000,
            number: 0,
            output: None,
            format,
            spyverter: false,
            bias_tee: false,
            agc: false,
            gain: None,
        }
    }

    #[test]
    fn default_capture_name_tracks_format() {
        for (format, extension) in [
            (FormatArg::Cu8, "cu8"),
            (FormatArg::Cs8, "cs8"),
            (FormatArg::Cs16, "cs16"),
            (FormatArg::Cf32, "cf32"),
        ] {
            assert_eq!(
                capture_output_path(&capture_args(format)),
                format!("sdrfox_capture.{extension}")
            );
        }
        let mut args = capture_args(FormatArg::Cf32);
        args.output = Some("chosen.raw".into());
        assert_eq!(capture_output_path(&args), "chosen.raw");
    }

    #[test]
    fn clap_command_tree_has_no_option_collisions() {
        Cli::command().debug_assert();
    }

    #[test]
    fn tracing_does_not_wait_for_raw_stream_stdout() {
        let dispatch = tracing::Dispatch::new(logging_subscriber("trace"));
        let stdout = std::io::stdout();
        let stdout_guard = stdout.lock();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                tracing::error!("synthetic USB recovery failure");
            });
            finished_tx.send(()).expect("completion receiver");
        });

        let completed_without_stdout = finished_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        drop(stdout_guard);
        worker.join().expect("logging worker");
        assert!(
            completed_without_stdout,
            "worker logging tried to acquire stdout held by the stream consumer"
        );
    }

    #[test]
    fn exact_byte_limit_never_overshoots() {
        assert_eq!(limited_write_len(0, 0, 64), 64);
        assert_eq!(limited_write_len(5, 0, 64), 5);
        assert_eq!(limited_write_len(5, 4, 64), 1);
        assert_eq!(limited_write_len(5, 5, 64), 0);
    }

    #[test]
    fn waterfall_aggregation_keeps_cross_frame_maxima() {
        let mut columns = [f32::NEG_INFINITY; 2];
        accumulate_waterfall(&[-60.0, -30.0, -20.0, -10.0], &mut columns);
        accumulate_waterfall(&[-40.0, -50.0, -5.0, -15.0], &mut columns);
        assert!((columns[0] + 30.0).abs() < f32::EPSILON);
        assert!((columns[1] + 5.0).abs() < f32::EPSILON);
    }

    fn sampled_waterfall_starts(chunks: &[&[f32]]) -> Vec<f32> {
        const TEST_FRAME: usize = 8;
        let mut assembler = crate::streaming::FrameAssembler::new(TEST_FRAME, TEST_FRAME).unwrap();
        let mut frames_until_analysis = 0;
        let mut starts = Vec::new();
        for chunk in chunks {
            assembler.push(chunk, |frame| {
                if take_waterfall_frame(&mut frames_until_analysis) {
                    starts.push(frame[0]);
                }
            });
        }
        starts
    }

    #[test]
    fn waterfall_fft_sampling_is_bounded_and_chunk_invariant() {
        const TEST_FRAME: usize = 8;
        const FRAME_COUNT: usize = 40;
        let input = (0..TEST_FRAME * FRAME_COUNT)
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        let expected = sampled_waterfall_starts(&[&input]);
        assert_eq!(expected, [0.0, 128.0, 256.0]);
        assert_eq!(expected.len(), FRAME_COUNT.div_ceil(WATERFALL_FRAME_STRIDE));

        for split in 0..=input.len() {
            assert_eq!(
                sampled_waterfall_starts(&[&input[..split], &input[split..]]),
                expected,
                "split {split}"
            );
        }
        let tiny_chunks = input.chunks(3).collect::<Vec<_>>();
        assert_eq!(sampled_waterfall_starts(&tiny_chunks), expected);
    }
}
