//! Demodulation, spectrogram, and benchmark CLI commands.
//!
//! Each command captures live IQ from an SDR, runs it through the `sdr-fox-dsp`
//! pipeline, and writes an artifact (WAV audio, PNG spectrogram, CSV sweep,
//! or decoded ADS-B frames to stdout).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fmt::Write as _, io::Write as _};

use clap::{Parser, Subcommand};

use crate::device_open::open_device;
use crate::streaming::negotiate_sample_rate;

/// Demodulation and proof commands.
#[derive(Subcommand)]
pub enum DemodCommand {
    /// Decode WBFM broadcast to a WAV file.
    Wbfm(DemodArgs),
    /// Decode narrowband FM to a WAV file.
    Nbfm(DemodArgs),
    /// Decode AM to a WAV file.
    Am(DemodArgs),
    /// Decode USB to a WAV file.
    Usb(DemodArgs),
    /// Decode LSB to a WAV file.
    Lsb(DemodArgs),
    /// Decode ADS-B (1090 MHz) aircraft transponders, printing frames.
    Adsb(AdsbArgs),
    /// Sweep a frequency range and write a CSV power spectrogram (counterpart
    /// to `rtl_power`; the `frequency_hz,power_db` CSV layout is sdr-fox's own).
    Power(PowerArgs),
    /// Render a PNG spectrogram waterfall from a single capture.
    Sweep(SweepArgs),
    /// Throughput benchmark: SIMD conversion rate + sustained capture with drop count.
    Bench(BenchArgs),
}

#[derive(Parser, Clone)]
pub struct DemodArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Center frequency in Hz.
    #[arg(short, long, default_value_t = 100_000_000)]
    pub frequency: u64,
    /// Sample rate in Hz (capture rate).
    #[arg(short, long, default_value_t = 2_400_000)]
    pub sample_rate: u32,
    /// Output audio rate in Hz.
    #[arg(long, default_value_t = 48_000)]
    pub audio_rate: u32,
    /// Output WAV file.
    #[arg(short, long, default_value = "sdrfox_demod.wav")]
    pub output: String,
    /// Capture duration in seconds.
    #[arg(long, default_value_t = 3)]
    pub seconds: u64,
    /// Tuner gain in tenths of dB.
    #[arg(short = 'g', long)]
    pub gain: Option<i32>,
    /// Enable AGC.
    #[arg(long)]
    pub agc: bool,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
}

#[derive(Parser, Clone)]
pub struct AdsbArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Sample rate in Hz. The current two-samples-per-symbol decoder requires
    /// exactly 2,000,000.
    #[arg(short, long, default_value_t = 2_000_000)]
    pub sample_rate: u32,
    /// Capture duration in seconds.
    #[arg(long, default_value_t = 10)]
    pub seconds: u64,
    /// Tuner gain in tenths of dB.
    #[arg(short = 'g', long)]
    pub gain: Option<i32>,
    /// Explicitly enable bias-tee power for an active antenna or LNA.
    #[arg(long)]
    pub bias_tee: bool,
}

#[derive(Parser, Clone)]
pub struct PowerArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Frequency range `start:stop:bin_hz` (e.g. `88M:108M:100k`).
    #[arg(short = 'F', long)]
    pub freq: String,
    /// Tuner gain in tenths of dB.
    #[arg(short = 'g', long)]
    pub gain: Option<i32>,
    /// Requested capture rate; hardware may negotiate a nearby rate.
    #[arg(short = 'r', long, default_value_t = 2_400_000)]
    pub sample_rate: u32,
    /// Output CSV file.
    #[arg(short = 'O', long, default_value = "sdrfox_power.csv")]
    pub output: String,
    /// Dwell time per hop in seconds.
    #[arg(short = 't', long, default_value_t = 1)]
    pub dwell: u64,
    /// Settling interval after each live retune.
    #[arg(long, default_value_t = 50)]
    pub settle_ms: u64,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
}

#[derive(Parser, Clone)]
pub struct SweepArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Center frequency in Hz.
    #[arg(short, long, default_value_t = 100_000_000)]
    pub frequency: u64,
    /// Sample rate in Hz.
    #[arg(short, long, default_value_t = 2_400_000)]
    pub sample_rate: u32,
    /// Output PNG file.
    #[arg(short = 'O', long, default_value = "sdrfox_sweep.png")]
    pub output: String,
    /// Number of waterfall rows (time steps).
    #[arg(short, long, default_value_t = 64)]
    pub rows: u32,
    /// FFT size per row.
    #[arg(long, default_value_t = 256)]
    pub fft: u32,
    /// Tuner gain in tenths of dB.
    #[arg(short = 'g', long)]
    pub gain: Option<i32>,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
}

#[derive(Parser, Clone)]
pub struct BenchArgs {
    /// Device index.
    #[arg(short, long, default_value_t = 0)]
    pub device: usize,
    /// Sample rate in Hz.
    #[arg(short = 'r', long, default_value_t = 2_400_000)]
    pub sample_rate: u32,
    /// Duration in seconds.
    #[arg(short, long, default_value_t = 3)]
    pub seconds: u64,
    /// Explicitly enable bias-tee power.
    #[arg(long)]
    pub bias_tee: bool,
}

/// Run a demod command.
pub fn run(cmd: DemodCommand) -> Result<(), String> {
    match cmd {
        DemodCommand::Wbfm(a) => demod_fm(a, true),
        DemodCommand::Nbfm(a) => demod_fm(a, false),
        DemodCommand::Am(a) => demod_am(a),
        DemodCommand::Usb(a) => demod_ssb(a, true),
        DemodCommand::Lsb(a) => demod_ssb(a, false),
        DemodCommand::Adsb(a) => adsb(a),
        DemodCommand::Power(a) => power(a),
        DemodCommand::Sweep(a) => sweep(a),
        DemodCommand::Bench(a) => bench(a),
    }
}

#[derive(Debug, Default)]
struct CaptureStats {
    complex_samples: u64,
}

struct Cf32Capture {
    _device: crate::device_open::BoxedDevice,
    stream: sdr_fox_core::StreamHandle,
    deadline: Instant,
    actual_sample_rate: u32,
    converted: Vec<f32>,
    /// Cancels the reference-clock harmonics that land in this band. Empty
    /// (and therefore a no-op) when the device does not report a reference
    /// clock or no harmonic falls in the tuned band.
    spurs: sdr_fox_dsp::SpurCanceller,
    /// Whether [`sdr_fox_dsp::SpurCanceller::acquire`] has run. The nominal
    /// harmonic is tens of Hz off because of tuner PLL quantization, so the
    /// first block is spent refining the estimate before it can cancel well.
    spurs_acquired: bool,
}

/// Open a cf32 capture using the hardware-selected sample rate.
fn open_cf32_capture(
    device: usize,
    frequency: u64,
    sample_rate: u32,
    seconds: u64,
    gain: Option<i32>,
    agc: bool,
    bias_tee: bool,
) -> Result<Cf32Capture, String> {
    let mut dev = open_device(device).map_err(|e| e.to_string())?;
    let actual_sample_rate =
        negotiate_sample_rate(sample_rate, |requested| dev.set_sample_rate(requested))?;
    if bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    dev.set_frequency(frequency).map_err(|e| e.to_string())?;
    if agc {
        dev.set_agc(true).map_err(|e| e.to_string())?;
    }
    if let Some(g) = gain {
        dev.set_gain(sdr_fox_core::GainRequest::overall(g))
            .map_err(|e| e.to_string())?;
    }
    // Ask the driver to convert directly so Airspy retains its 12-bit ADC
    // precision instead of quantizing to Cu8 and expanding it again here.
    let cfg = sdr_fox_core::StreamConfig {
        format: sdr_fox_core::IqFormat::Cf32,
        ..sdr_fox_core::StreamConfig::default()
    };
    // Derive the spur list from the hardware's own reference clock, so no
    // caller has to know that an RTL-SDR birdies every 28.8 MHz. `frequency`
    // is the true RF the user asked for; any upconverter offset lives inside
    // the driver and does not move where the harmonics land in the samples.
    let spurs = match dev.reference_clock_hz() {
        Some(reference) => sdr_fox_dsp::SpurCanceller::for_reference(
            f64::from(reference),
            frequency as f64,
            f64::from(actual_sample_rate),
        ),
        None => sdr_fox_dsp::SpurCanceller::new(f64::from(actual_sample_rate)),
    };
    if !spurs.is_empty() {
        tracing::info!(
            spurs = spurs.len(),
            offsets_hz = ?spurs.frequencies(),
            "cancelling reference-clock harmonics in band"
        );
    }
    let stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let now = Instant::now();
    Ok(Cf32Capture {
        _device: dev,
        stream,
        deadline: crate::streaming::checked_deadline(now, Duration::from_secs(seconds))?,
        actual_sample_rate,
        converted: Vec::new(),
        spurs,
        spurs_acquired: false,
    })
}

/// Refine the spur frequencies on the first block, then cancel in place.
///
/// Acquisition is deferred to the first real block rather than done at open
/// time because it needs samples; it is skipped for blocks too short to give a
/// usable estimate, so a runt first block just delays it by one block.
fn cancel_spurs(capture: &mut Cf32Capture, iq: &mut [f32]) {
    if capture.spurs.is_empty() {
        return;
    }
    if !capture.spurs_acquired && capture.spurs.acquire(iq) {
        capture.spurs_acquired = true;
        tracing::debug!(offsets_hz = ?capture.spurs.frequencies(), "spur frequencies acquired");
    }
    capture.spurs.process(iq);
}

fn process_cf32_blocks(
    capture: &mut Cf32Capture,
    mut consume: impl FnMut(&[f32]) -> bool,
) -> Result<CaptureStats, String> {
    let mut stats = CaptureStats::default();
    while let Some(block) = crate::streaming::recv_until(
        capture.stream.as_mut(),
        Some(capture.deadline),
        crate::streaming::shutdown_flag(),
    )? {
        let keep_streaming = match block.samples {
            sdr_fox_core::IqSamples::Cu8(bytes) => {
                capture.converted.resize(bytes.len(), 0.0);
                sdr_fox_simd::cu8_to_cf32(&bytes, &mut capture.converted);
                stats.complex_samples = stats
                    .complex_samples
                    .saturating_add((capture.converted.len() / 2) as u64);
                // Take the buffer out so the canceller can borrow it mutably
                // while `capture` stays borrowed for the spur state, then put
                // it back to keep the allocation across blocks.
                let mut converted = std::mem::take(&mut capture.converted);
                cancel_spurs(capture, &mut converted);
                let keep = consume(&converted);
                capture.converted = converted;
                keep
            }
            sdr_fox_core::IqSamples::Cf32(mut iq) => {
                stats.complex_samples = stats.complex_samples.saturating_add((iq.len() / 2) as u64);
                cancel_spurs(capture, &mut iq);
                consume(&iq)
            }
            other => {
                return Err(format!(
                    "cf32 processing received unsupported {} block",
                    other.format()
                ));
            }
        };
        if !keep_streaming {
            break;
        }
    }
    Ok(stats)
}

fn demod_fm(args: DemodArgs, wide: bool) -> Result<(), String> {
    let mut capture = open_cf32_capture(
        args.device,
        args.frequency,
        args.sample_rate,
        args.seconds,
        args.gain,
        args.agc,
        args.bias_tee,
    )?;
    validate_fm_rates(capture.actual_sample_rate, args.audio_rate)?;
    let mut demod = if wide {
        sdr_fox_dsp::FmDemod::new(
            capture.actual_sample_rate as f32,
            args.audio_rate as f32,
            180_000.0,
        )
    } else {
        sdr_fox_dsp::FmDemod::with_deemphasis(
            capture.actual_sample_rate as f32,
            args.audio_rate as f32,
            12_500.0,
            None,
        )
    };
    let mut writer = sdr_fox_dsp::wav::WavWriter::create(&args.output, args.audio_rate)
        .map_err(|e| format!("write {}: {e}", args.output))?;
    let mut write_error = None;
    let mut audio_samples = 0u64;
    let stats = process_cf32_blocks(&mut capture, |iq| {
        let audio = demod.process(iq);
        audio_samples = audio_samples.saturating_add(audio.len() as u64);
        if let Err(error) = writer.write_samples(&audio) {
            write_error = Some(error);
            return false;
        }
        true
    })?;
    if let Some(error) = write_error {
        return Err(format!("write {}: {error}", args.output));
    }
    writer
        .finish()
        .map_err(|e| format!("write {}: {e}", args.output))?;
    eprintln!("captured {} complex samples", stats.complex_samples);
    eprintln!("demodulated {audio_samples} audio samples");
    eprintln!("wrote {}", args.output);
    Ok(())
}

fn demod_am(args: DemodArgs) -> Result<(), String> {
    let mut capture = open_cf32_capture(
        args.device,
        args.frequency,
        args.sample_rate,
        args.seconds,
        args.gain,
        args.agc,
        args.bias_tee,
    )?;
    let decimation = audio_decimation(capture.actual_sample_rate, args.audio_rate)?;
    let cutoff = 5_000.0_f32.min(args.audio_rate as f32 * 0.45);
    let mut dc_blocker = sdr_fox_dsp::filters::DcBlocker::new(0.001);
    let filter_taps = decimation.saturating_mul(8).saturating_add(1).max(63);
    let mut audio_filter = sdr_fox_dsp::filters::LowPass::new(
        cutoff,
        capture.actual_sample_rate as f32,
        decimation,
        filter_taps,
    );
    let mut writer = sdr_fox_dsp::wav::WavWriter::create(&args.output, args.audio_rate)
        .map_err(|e| format!("write {}: {e}", args.output))?;
    let mut write_error = None;
    let stats = process_cf32_blocks(&mut capture, |iq| {
        let envelope = sdr_fox_dsp::am_demod(iq);
        let mut audio = audio_filter.process(&envelope);
        // The serial DC blocker belongs at the decimated audio rate, not at
        // the multi-megasample envelope rate.
        dc_blocker.process(&mut audio);
        if let Err(error) = writer.write_samples(&audio) {
            write_error = Some(error);
            return false;
        }
        true
    })?;
    if let Some(error) = write_error {
        return Err(format!("write {}: {error}", args.output));
    }
    writer
        .finish()
        .map_err(|e| format!("write {}: {e}", args.output))?;
    eprintln!("captured {} complex samples", stats.complex_samples);
    eprintln!("wrote {}", args.output);
    Ok(())
}

fn demod_ssb(args: DemodArgs, upper: bool) -> Result<(), String> {
    let mut capture = open_cf32_capture(
        args.device,
        args.frequency,
        args.sample_rate,
        args.seconds,
        args.gain,
        args.agc,
        args.bias_tee,
    )?;
    let decimation = audio_decimation(capture.actual_sample_rate, args.audio_rate)?;
    let mut demod = sdr_fox_dsp::demods::SsbDemag::new();
    let cutoff = 5_000.0_f32.min(args.audio_rate as f32 * 0.45);
    // Channel-filter and decimate complex IQ before the 255-tap Hilbert stage.
    // Scale the anti-alias FIR with the rate change so its transition band does
    // not collapse at raw multi-megasample SDR rates.
    let channel_taps = decimation.saturating_mul(8).saturating_add(1).max(63);
    let mut channel_filter = sdr_fox_dsp::filters::ComplexLowPass::new(
        cutoff,
        capture.actual_sample_rate as f32,
        decimation,
        channel_taps,
    );
    let mut writer = sdr_fox_dsp::wav::WavWriter::create(&args.output, args.audio_rate)
        .map_err(|e| format!("write {}: {e}", args.output))?;
    let mut write_error = None;
    let stats = process_cf32_blocks(&mut capture, |iq| {
        let channelized = channel_filter.process(iq);
        let audio = demod.process(&channelized, upper);
        if let Err(error) = writer.write_samples(&audio) {
            write_error = Some(error);
            return false;
        }
        true
    })?;
    if let Some(error) = write_error {
        return Err(format!("write {}: {error}", args.output));
    }
    writer
        .finish()
        .map_err(|e| format!("write {}: {e}", args.output))?;
    eprintln!("captured {} complex samples", stats.complex_samples);
    eprintln!("wrote {}", args.output);
    Ok(())
}

fn adsb(args: AdsbArgs) -> Result<(), String> {
    // ADS-B at 1090 MHz. Capture cu8 (the native format; ADS-B uses magnitude).
    validate_adsb_rate(args.sample_rate)?;
    let mut dev = open_device(args.device).map_err(|e| e.to_string())?;
    let actual_sample_rate =
        negotiate_sample_rate(args.sample_rate, |requested| dev.set_sample_rate(requested))?;
    validate_adsb_rate(actual_sample_rate)?;
    if args.bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    dev.set_frequency(1_090_000_000)
        .map_err(|e| e.to_string())?;
    if let Some(g) = args.gain {
        dev.set_gain(sdr_fox_core::GainRequest::overall(g))
            .map_err(|e| e.to_string())?;
    }
    let cfg = sdr_fox_core::StreamConfig::default(); // cu8
    let mut stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let deadline = crate::streaming::checked_deadline(started, Duration::from_secs(args.seconds))?;
    let mut total_bytes = 0u64;
    let mut frame_count = 0u64;
    let mut dropped = 0u64;
    // Preserve magnitude/frame state across USB blocks so a 56- or 112-bit
    // message that straddles a completion boundary is still decoded exactly
    // once. Calling the one-shot `decode_cu8` here would discard that tail.
    let mut decoder = sdr_fox_dsp::adsb::AdsbDecoder::new();
    let stdout = std::io::stdout();
    let mut output = std::io::BufWriter::new(stdout.lock());
    let mut hex = String::with_capacity(28);
    let mut unflushed_frames = 0usize;
    let mut last_flush = Instant::now();
    while let Some(block) = crate::streaming::recv_until(
        stream.as_mut(),
        Some(deadline),
        crate::streaming::shutdown_flag(),
    )? {
        dropped = dropped.max(block.dropped);
        let sdr_fox_core::IqSamples::Cu8(bytes) = &block.samples else {
            return Err(format!(
                "ADS-B received unexpected {} block",
                block.samples.format()
            ));
        };
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        let frames = decoder.decode_block(bytes);
        for frame in &frames {
            frame_count = frame_count.saturating_add(1);
            unflushed_frames += 1;
            hex.clear();
            for byte in &frame.message {
                write!(hex, "{byte:02x}").map_err(|error| error.to_string())?;
            }
            writeln!(
                output,
                "DF={} CRC={} {}",
                frame.df,
                if frame.crc_ok { "OK" } else { "FAIL" },
                hex
            )
            .map_err(|error| format!("write ADS-B output: {error}"))?;
        }
        if unflushed_frames >= 64 || last_flush.elapsed() >= Duration::from_millis(100) {
            output
                .flush()
                .map_err(|error| format!("flush ADS-B output: {error}"))?;
            unflushed_frames = 0;
            last_flush = Instant::now();
        }
    }
    output
        .flush()
        .map_err(|error| format!("flush ADS-B output: {error}"))?;
    stream.stop();
    eprintln!(
        "ADS-B: processed {total_bytes} bytes, decoded {frame_count} CRC-valid frames, {dropped} dropped"
    );
    Ok(())
}

fn power(args: PowerArgs) -> Result<(), String> {
    if args.dwell == 0 {
        return Err("--dwell must be greater than zero".into());
    }
    let (start, stop, bin) = parse_power_range(&args.freq)?;
    let mut dev = open_device(args.device).map_err(|e| e.to_string())?;
    let actual_rate =
        negotiate_sample_rate(args.sample_rate, |requested| dev.set_sample_rate(requested))?;
    if args.bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    if let Some(g) = args.gain {
        dev.set_gain(sdr_fox_core::GainRequest::overall(g))
            .map_err(|error| error.to_string())?;
    }
    let fft_size = sweep_fft_size(actual_rate, bin)?;
    let centers = sweep_centers(start, stop, actual_rate)?;
    let mut grid = PowerGrid::new(start, stop, bin)?;
    let mut spectrum = sdr_fox_simd::Spectrum::new(fft_size);
    spectrum.set_normalization(sdr_fox_simd::Normalization::Psd);
    let mut welch = WelchAccumulator::new(fft_size)?;

    dev.set_frequency(centers[0])
        .map_err(|error| error.to_string())?;
    let cfg = sdr_fox_core::StreamConfig {
        format: sdr_fox_core::IqFormat::Cf32,
        ..sdr_fox_core::StreamConfig::default()
    };
    let drain_limit = sweep_drain_limit(&cfg)?;
    let settle_duration = effective_settle_duration(args.settle_ms, cfg.buffer_size, actual_rate)?;
    let mut stream = dev.start_stream(cfg).map_err(|error| error.to_string())?;
    let mut dropped = 0u64;
    let mut visited = 0usize;
    let mut interrupted = false;
    'centers: for (index, &center) in centers.iter().enumerate() {
        if crate::streaming::shutdown_flag().load(std::sync::atomic::Ordering::Acquire) {
            interrupted = true;
            break;
        }
        if index != 0 {
            dev.set_frequency(center)
                .map_err(|error| error.to_string())?;
        }
        drain_queued(&mut *stream, drain_limit)?;
        let settle_started = Instant::now();
        let settle_deadline = crate::streaming::checked_deadline(settle_started, settle_duration)?;
        while Instant::now() < settle_deadline {
            if crate::streaming::recv_until(
                stream.as_mut(),
                Some(settle_deadline),
                crate::streaming::shutdown_flag(),
            )?
            .is_none()
            {
                break;
            }
        }
        if crate::streaming::shutdown_flag().load(std::sync::atomic::Ordering::Acquire) {
            interrupted = true;
            break 'centers;
        }

        // A low-rate transfer can have started before the retune yet complete
        // after a time-only settle window. Discard the first full completion
        // after settling so no mixed/old-center URB enters the Welch average.
        let discard_deadline =
            crate::streaming::checked_deadline(Instant::now(), Duration::from_secs(args.dwell))?;
        let Some(discarded) = discard_post_settle_block(
            stream.as_mut(),
            discard_deadline,
            crate::streaming::shutdown_flag(),
        )?
        else {
            interrupted = true;
            break 'centers;
        };
        dropped = dropped.max(discarded.dropped);
        if !matches!(&discarded.samples, sdr_fox_core::IqSamples::Cf32(_)) {
            return Err(format!(
                "power sweep received unexpected {} block while discarding post-retune data",
                discarded.samples.format()
            ));
        }

        welch.reset();
        let dwell_started = Instant::now();
        let dwell_deadline =
            crate::streaming::checked_deadline(dwell_started, Duration::from_secs(args.dwell))?;
        while let Some(block) = crate::streaming::recv_until(
            stream.as_mut(),
            Some(dwell_deadline),
            crate::streaming::shutdown_flag(),
        )? {
            dropped = dropped.max(block.dropped);
            let sdr_fox_core::IqSamples::Cf32(iq) = &block.samples else {
                return Err(format!(
                    "power sweep received unexpected {} block",
                    block.samples.format()
                ));
            };
            welch.push(&mut spectrum, iq);
        }
        if crate::streaming::shutdown_flag().load(std::sync::atomic::Ordering::Acquire) {
            interrupted = true;
            break 'centers;
        }
        let average = welch.average()?;
        grid.observe(center, actual_rate, average)?;
        visited += 1;
    }
    stream.stop();
    if interrupted {
        eprintln!(
            "power sweep interrupted after {visited} completed retunes; no partial CSV was written"
        );
        return Ok(());
    }
    let rows = grid.rows()?;
    let mut pending_artifact = PendingArtifact::new(&args.output);
    sdr_fox_dsp::png::write_power_csv(pending_artifact.path(), &rows)
        .map_err(|e| format!("write {}: {e}", args.output))?;
    pending_artifact.commit(&args.output)?;
    eprintln!(
        "wrote {} ({} bins, {} retunes, negotiated {} S/s, FFT {}, {} dropped)",
        args.output,
        rows.len(),
        visited,
        actual_rate,
        fft_size,
        dropped
    );
    Ok(())
}

const MIN_SWEEP_FFT: usize = 256;
const MAX_SWEEP_FFT: usize = 1 << 20;
const MAX_POWER_ROWS: usize = 1_000_000;
const MAX_RETUNES: usize = 100_000;

fn parse_power_range(value: &str) -> Result<(u64, u64, u64), String> {
    let mut parts = value.split(':');
    let start = parts.next().ok_or("missing sweep start")?;
    let stop = parts.next().ok_or("missing sweep stop")?;
    let bin = parts.next().ok_or("missing sweep bin width")?;
    if parts.next().is_some() {
        return Err("--freq must be start:stop:bin (e.g. 88M:108M:100k)".into());
    }
    let (start, stop, bin) = (parse_freq(start)?, parse_freq(stop)?, parse_freq(bin)?);
    if start > stop {
        return Err("sweep start must not exceed stop".into());
    }
    if bin == 0 {
        return Err("sweep bin width must be greater than zero".into());
    }
    Ok((start, stop, bin))
}

fn sweep_fft_size(actual_rate: u32, bin_hz: u64) -> Result<usize, String> {
    if actual_rate == 0 || bin_hz == 0 {
        return Err("sample rate and bin width must be non-zero".into());
    }
    let needed = u64::from(actual_rate)
        .div_ceil(bin_hz)
        .max(MIN_SWEEP_FFT as u64);
    let needed = usize::try_from(needed).map_err(|_| "FFT size exceeds this platform")?;
    let size = needed
        .checked_next_power_of_two()
        .ok_or("FFT size overflow")?;
    if size > MAX_SWEEP_FFT {
        return Err(format!(
            "requested bin width requires FFT {size}, maximum is {MAX_SWEEP_FFT}"
        ));
    }
    Ok(size)
}

fn sweep_centers(start: u64, stop: u64, actual_rate: u32) -> Result<Vec<u64>, String> {
    let span = u64::from(actual_rate)
        .checked_mul(4)
        .map(|value| value / 5)
        .filter(|&value| value != 0)
        .ok_or("negotiated sample rate has no usable sweep span")?;
    let low_width = span / 2;
    let high_width = span - low_width;
    let width = stop - start;
    if width <= span {
        return Ok(vec![start + width / 2]);
    }
    let estimated = width
        .div_ceil(span)
        .checked_add(1)
        .ok_or("sweep retune-count overflow")?;
    let estimated = usize::try_from(estimated).map_err(|_| "sweep has too many retunes")?;
    if estimated > MAX_RETUNES {
        return Err(format!(
            "sweep requires approximately {estimated} retunes, maximum is {MAX_RETUNES}"
        ));
    }
    let mut centers = Vec::with_capacity(estimated);
    let mut center = start
        .checked_add(low_width)
        .ok_or("sweep center overflow")?;
    loop {
        centers.push(center);
        if center
            .checked_add(high_width)
            .is_some_and(|high| high >= stop)
        {
            break;
        }
        let next = center.checked_add(span).ok_or("sweep center overflow")?;
        if next
            .checked_add(high_width)
            .is_some_and(|high| high >= stop)
        {
            let final_center = stop
                .checked_sub(high_width)
                .ok_or("sweep center underflow")?;
            if final_center > center {
                centers.push(final_center);
            }
            break;
        }
        center = next;
    }
    Ok(centers)
}

fn drain_queued(stream: &mut dyn sdr_fox_core::StreamSink, limit: usize) -> Result<(), String> {
    for _ in 0..limit {
        match stream.recv_deadline(Instant::now()) {
            Some(Ok(_)) => {}
            Some(Err(sdr_fox_core::SdrError::Timeout)) => return Ok(()),
            Some(Err(error)) => return Err(error.to_string()),
            None => return Err("power sweep stream ended while draining stale data".into()),
        }
    }
    Ok(())
}

fn sweep_drain_limit(config: &sdr_fox_core::StreamConfig) -> Result<usize, String> {
    config
        .queue_depth
        .checked_add(config.queue_depth.max(config.buffer_count))
        .filter(|&limit| limit != 0)
        .ok_or_else(|| "power-sweep drain limit overflow or zero".into())
}

fn buffer_fill_duration(buffer_size: usize, actual_rate: u32) -> Result<Duration, String> {
    let complex_samples = buffer_size / 2;
    if complex_samples == 0 || actual_rate == 0 {
        return Err("buffer size and actual sample rate must be non-zero".into());
    }
    let complex_samples =
        u128::try_from(complex_samples).map_err(|_| "stream buffer sample count exceeds u128")?;
    let nanos = complex_samples
        .checked_mul(1_000_000_000)
        .ok_or("stream buffer duration overflow")?
        .div_ceil(u128::from(actual_rate));
    let nanos = u64::try_from(nanos).map_err(|_| "stream buffer duration exceeds u64 nanos")?;
    Ok(Duration::from_nanos(nanos))
}

fn effective_settle_duration(
    requested_ms: u64,
    buffer_size: usize,
    actual_rate: u32,
) -> Result<Duration, String> {
    Ok(Duration::from_millis(requested_ms).max(buffer_fill_duration(buffer_size, actual_rate)?))
}

fn discard_post_settle_block(
    stream: &mut dyn sdr_fox_core::StreamSink,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<Option<sdr_fox_core::IqBlock>, String> {
    match crate::streaming::recv_until(stream, Some(deadline), shutdown)? {
        Some(block) => Ok(Some(block)),
        None if shutdown.load(Ordering::Acquire) => Ok(None),
        None => Err("timed out waiting for a complete post-retune block".into()),
    }
}

struct WelchAccumulator {
    assembler: crate::streaming::FrameAssembler,
    sums: Vec<f64>,
    frames: u64,
    average: Vec<f32>,
}

impl WelchAccumulator {
    fn new(fft_size: usize) -> Result<Self, String> {
        let frame_floats = fft_size.checked_mul(2).ok_or("FFT frame size overflow")?;
        Ok(Self {
            assembler: crate::streaming::FrameAssembler::new(frame_floats, fft_size)?,
            sums: vec![0.0; fft_size],
            frames: 0,
            average: vec![0.0; fft_size],
        })
    }

    fn reset(&mut self) {
        self.assembler.clear();
        self.sums.fill(0.0);
        self.frames = 0;
    }

    fn push(&mut self, spectrum: &mut sdr_fox_simd::Spectrum, iq: &[f32]) {
        let sums = &mut self.sums;
        let frames = &mut self.frames;
        self.assembler.push(iq, |frame| {
            for (sum, &power) in sums.iter_mut().zip(spectrum.compute_power_linear(frame)) {
                *sum += f64::from(power);
            }
            *frames = frames.saturating_add(1);
        });
    }

    fn average(&mut self) -> Result<&[f32], String> {
        if self.frames == 0 {
            return Err("dwell produced no complete Welch frames".into());
        }
        let divisor = self.frames as f64;
        for (average, &sum) in self.average.iter_mut().zip(&self.sums) {
            *average = (sum / divisor) as f32;
        }
        Ok(&self.average)
    }
}

struct PowerGrid {
    start: u64,
    stop: u64,
    bin: u64,
    sums: Vec<f64>,
    counts: Vec<u32>,
}

impl PowerGrid {
    fn new(start: u64, stop: u64, bin: u64) -> Result<Self, String> {
        let count_u64 = (stop - start)
            .checked_div(bin)
            .and_then(|value| value.checked_add(1))
            .ok_or("power-grid size overflow")?;
        let count = usize::try_from(count_u64).map_err(|_| "power grid exceeds this platform")?;
        if count > MAX_POWER_ROWS {
            return Err(format!(
                "power grid has {count} rows, maximum is {MAX_POWER_ROWS}"
            ));
        }
        Ok(Self {
            start,
            stop,
            bin,
            sums: vec![0.0; count],
            counts: vec![0; count],
        })
    }

    fn observe(&mut self, center: u64, actual_rate: u32, power: &[f32]) -> Result<(), String> {
        let span = u64::from(actual_rate) * 4 / 5;
        let low_width = span / 2;
        let high_width = span - low_width;
        let low = center.saturating_sub(low_width).max(self.start);
        let high = center.saturating_add(high_width).min(self.stop);
        let first = low.saturating_sub(self.start).div_ceil(self.bin);
        let last = high.saturating_sub(self.start) / self.bin;
        let fft_len = i128::try_from(power.len()).map_err(|_| "FFT length overflow")?;
        let rate = i128::from(actual_rate);
        for grid_index in first..=last {
            let index = usize::try_from(grid_index).map_err(|_| "grid index overflow")?;
            let frequency = self
                .start
                .checked_add(
                    grid_index
                        .checked_mul(self.bin)
                        .ok_or("frequency overflow")?,
                )
                .ok_or("frequency overflow")?;
            let scaled = (i128::from(frequency) - i128::from(center)) * fft_len;
            let rounded = if scaled >= 0 {
                (scaled + rate / 2) / rate
            } else {
                (scaled - rate / 2) / rate
            };
            let fft_index = fft_len / 2 + rounded;
            if let Ok(fft_index) = usize::try_from(fft_index) {
                if let (Some(&value), Some(sum), Some(count)) = (
                    power.get(fft_index),
                    self.sums.get_mut(index),
                    self.counts.get_mut(index),
                ) {
                    *sum += f64::from(value);
                    *count = count.saturating_add(1);
                }
            }
        }
        Ok(())
    }

    fn rows(&self) -> Result<Vec<(f64, f64)>, String> {
        let mut rows = Vec::with_capacity(self.sums.len());
        for (index, (&sum, &count)) in self.sums.iter().zip(&self.counts).enumerate() {
            if count == 0 {
                return Err(format!(
                    "no sweep observation covered power-grid row {index}"
                ));
            }
            let index = u64::try_from(index).map_err(|_| "grid index overflow")?;
            let frequency = self
                .start
                .checked_add(index.checked_mul(self.bin).ok_or("frequency overflow")?)
                .ok_or("frequency overflow")?;
            let linear = (sum / f64::from(count)).max(1e-30);
            rows.push((frequency as f64, 10.0 * linear.log10()));
        }
        Ok(rows)
    }
}

fn sweep(args: SweepArgs) -> Result<(), String> {
    let (fft, rows, complex_needed) = validate_sweep_shape(args.fft, args.rows, args.sample_rate)?;
    let mut spec = sdr_fox_simd::Spectrum::new(fft);
    let samples_per_row = fft.checked_mul(2).ok_or("sweep frame-size overflow")?;
    let mut assembler = crate::streaming::FrameAssembler::new(samples_per_row, samples_per_row)?;
    let mut pixels = vec![0u8; fft];
    let mut row = 0usize;

    let mut capture = open_cf32_capture(
        args.device,
        args.frequency,
        args.sample_rate,
        1,
        args.gain,
        false,
        args.bias_tee,
    )?;
    let capture_seconds = sweep_capture_seconds(complex_needed, capture.actual_sample_rate)?;
    capture.deadline =
        crate::streaming::checked_deadline(Instant::now(), Duration::from_secs(capture_seconds))?;
    let mut pending_artifact = PendingArtifact::new(&args.output);
    let mut png = sdr_fox_dsp::png::PngWriter::create(pending_artifact.path(), args.fft, args.rows)
        .map_err(|error| format!("create {}: {error}", args.output))?;
    let mut write_error = None;
    let stats = process_cf32_blocks(&mut capture, |iq| {
        assembler.push(iq, |frame| {
            if row >= rows || write_error.is_some() {
                return;
            }
            let power = spec.compute_power_dbfs(frame);
            for (col, &db) in power.iter().enumerate() {
                pixels[col] = ((db + 60.0) / 60.0 * 255.0).clamp(0.0, 255.0) as u8;
            }
            if let Err(error) = png.write_rows(&pixels) {
                write_error = Some(error);
                return;
            }
            row += 1;
        });
        row < rows && write_error.is_none()
    })?;
    if let Some(error) = write_error {
        return Err(format!("write {}: {error}", args.output));
    }
    if row != rows {
        if crate::streaming::shutdown_flag().load(std::sync::atomic::Ordering::Acquire) {
            eprintln!(
                "spectrogram interrupted after {row} of {rows} rows; no partial PNG was kept"
            );
            return Ok(());
        }
        return Err(format!(
            "spectrogram ended after {row} of {rows} requested rows"
        ));
    }
    eprintln!("captured {} complex samples", stats.complex_samples);
    png.finish()
        .map_err(|e| format!("write {}: {e}", args.output))?;
    pending_artifact.commit(&args.output)?;
    eprintln!("wrote {} ({}x{} greyscale)", args.output, fft, args.rows);
    Ok(())
}

fn validate_sweep_shape(
    fft_arg: u32,
    rows_arg: u32,
    sample_rate: u32,
) -> Result<(usize, usize, u64), String> {
    let fft = fft_arg as usize;
    if !fft.is_power_of_two() || fft <= 1 {
        return Err("--fft must be a power of two greater than one".into());
    }
    if fft > MAX_SWEEP_FFT {
        return Err(format!(
            "--fft {fft} exceeds the safe maximum {MAX_SWEEP_FFT}"
        ));
    }
    let rows = rows_arg as usize;
    if rows == 0 {
        return Err("--rows must be greater than zero".into());
    }
    if rows_arg > i32::MAX as u32 {
        return Err(format!(
            "--rows {rows_arg} exceeds the PNG dimension limit {}",
            i32::MAX
        ));
    }
    if sample_rate == 0 {
        return Err("--sample-rate must be greater than zero".into());
    }
    let rows_u64 = u64::try_from(rows).map_err(|_| "sweep row count exceeds u64")?;
    let fft_u64 = u64::try_from(fft).map_err(|_| "sweep FFT size exceeds u64")?;
    let complex_needed = checked_sweep_sample_count(rows_u64, fft_u64)?;
    Ok((fft, rows, complex_needed))
}

fn checked_sweep_sample_count(rows: u64, fft: u64) -> Result<u64, String> {
    rows.checked_mul(fft)
        .ok_or_else(|| "sweep sample-count overflow".into())
}

fn sweep_capture_seconds(complex_needed: u64, actual_sample_rate: u32) -> Result<u64, String> {
    if actual_sample_rate == 0 {
        return Err("device negotiated a zero sample rate".into());
    }
    complex_needed
        .div_ceil(u64::from(actual_sample_rate))
        .checked_add(2)
        .ok_or_else(|| "sweep duration overflow".into())
}

static NEXT_ARTIFACT_ID: AtomicU64 = AtomicU64::new(0);

struct PendingArtifact {
    path: PathBuf,
    committed: bool,
}

impl PendingArtifact {
    fn new(final_path: impl AsRef<Path>) -> Self {
        let final_path = final_path.as_ref();
        let mut path = final_path.to_path_buf();
        let name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sdrfox.png");
        let artifact_id = NEXT_ARTIFACT_ID.fetch_add(1, Ordering::Relaxed);
        path.set_file_name(format!(
            ".{name}.sdrfox-{}-{artifact_id}.part",
            std::process::id()
        ));
        Self {
            path,
            committed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(&mut self, final_path: impl AsRef<Path>) -> Result<(), String> {
        std::fs::rename(&self.path, final_path.as_ref())
            .map_err(|error| format!("commit {}: {error}", final_path.as_ref().display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PendingArtifact {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn bench(args: BenchArgs) -> Result<(), String> {
    if args.seconds == 0 {
        return Err("--seconds must be greater than zero".into());
    }
    // SIMD throughput benchmark.
    let block = vec![128u8; 262_144];
    let mut out = vec![0.0f32; 262_144];
    let t0 = Instant::now();
    let iters = 50;
    for _ in 0..iters {
        sdr_fox_simd::cu8_to_cf32(std::hint::black_box(&block), std::hint::black_box(&mut out));
        std::hint::black_box(&out);
    }
    let elapsed = t0.elapsed();
    let bytes = 262_144 * iters;
    let ms_per_block = elapsed.as_secs_f64() * 1000.0 / iters as f64;
    let ms_s = (bytes as f64) / elapsed.as_secs_f64() / 1e6;
    eprintln!("SIMD cu8->cf32: {ms_per_block:.2} ms/block, {ms_s:.0} MB/s");

    // Sustained capture with drop count.
    let mut dev = open_device(args.device).map_err(|e| e.to_string())?;
    let actual_rate =
        negotiate_sample_rate(args.sample_rate, |requested| dev.set_sample_rate(requested))?;
    if args.bias_tee {
        dev.set_bias_tee(true).map_err(|error| error.to_string())?;
    }
    let cfg = sdr_fox_core::StreamConfig::default();
    let mut stream = dev.start_stream(cfg).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let deadline = crate::streaming::checked_deadline(started, Duration::from_secs(args.seconds))?;
    let mut samples: u64 = 0;
    let mut capture_bytes: u64 = 0;
    let mut blocks = 0u64;
    let mut dropped: u64 = 0;
    let mut scratch = Vec::new();
    while let Some(block) = crate::streaming::recv_until(
        stream.as_mut(),
        Some(deadline),
        crate::streaming::shutdown_flag(),
    )? {
        samples = samples.saturating_add(block.samples.complex_count() as u64);
        capture_bytes = capture_bytes.saturating_add(
            crate::streaming::sample_bytes(&block.samples, &mut scratch).len() as u64,
        );
        dropped = dropped.max(block.dropped);
        blocks = blocks.saturating_add(1);
    }
    stream.stop();
    let capture_elapsed = started.elapsed();
    let elapsed_seconds = capture_elapsed.as_secs_f64();
    let samples_per_second = if elapsed_seconds == 0.0 {
        0.0
    } else {
        samples as f64 / elapsed_seconds
    };
    let capture_mbps = if elapsed_seconds == 0.0 {
        0.0
    } else {
        capture_bytes as f64 / elapsed_seconds / 1e6
    };
    eprintln!(
        "capture: {blocks} blocks, {samples} samples, {capture_bytes} bytes in {elapsed_seconds:.3}s ({samples_per_second:.0} S/s, {capture_mbps:.3} MB/s), {dropped} dropped, negotiated {actual_rate} S/s"
    );
    Ok(())
}

/// Return the exact integer audio decimation or reject a configuration that
/// would otherwise write a WAV header whose sample rate does not match the
/// samples produced by the current integer decimator.
fn validate_fm_rates(sample_rate: u32, audio_rate: u32) -> Result<(), String> {
    if audio_rate == 0 || sample_rate < audio_rate {
        return Err(format!(
            "FM rates must satisfy sample rate >= audio rate > 0, got {sample_rate} and {audio_rate}"
        ));
    }
    Ok(())
}

fn audio_decimation(sample_rate: u32, audio_rate: u32) -> Result<usize, String> {
    if audio_rate == 0 || sample_rate < audio_rate || sample_rate % audio_rate != 0 {
        return Err(format!(
            "audio rate {audio_rate} must be a non-zero integer divisor of sample rate {sample_rate}"
        ));
    }
    Ok((sample_rate / audio_rate) as usize)
}

fn validate_adsb_rate(sample_rate: u32) -> Result<(), String> {
    if sample_rate != 2_000_000 {
        return Err(format!(
            "ADS-B decoder currently requires exactly 2000000 samples/s, got {sample_rate}"
        ));
    }
    Ok(())
}

/// Parse a frequency string like "88M", "108M", "100k", "1090e6".
fn parse_freq(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_str, mult) = if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1_000_000u64)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1_000u64)
    } else if let Some(n) = s.strip_suffix("e6") {
        (n, 1_000_000u64)
    } else {
        (s, 1u64)
    };
    let value = num_str
        .parse::<u64>()
        .map_err(|e| format!("bad frequency '{s}': {e}"))?;
    value
        .checked_mul(mult)
        .ok_or_else(|| format!("frequency '{s}' overflows u64"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use sdr_fox_core::sample::StreamStopHandle;

    use super::*;

    #[test]
    fn audio_decimation_rejects_mislabeled_output_rates() {
        assert_eq!(audio_decimation(2_400_000, 48_000), Ok(50));
        assert!(audio_decimation(2_400_000, 44_100).is_err());
        assert!(audio_decimation(48_000, 0).is_err());
        assert!(audio_decimation(48_000, 96_000).is_err());
    }

    #[test]
    fn adsb_rejects_unmodeled_sample_rates() {
        assert_eq!(validate_adsb_rate(2_000_000), Ok(()));
        assert!(validate_adsb_rate(2_400_000).is_err());
    }

    #[test]
    fn hardware_selected_rate_drives_downstream_validation() {
        let actual = negotiate_sample_rate(2_400_000, |_| Ok(2_500_000)).unwrap();
        assert_eq!(actual, 2_500_000);
        assert!(audio_decimation(actual, 48_000).is_err());
        assert!(validate_fm_rates(actual, 48_000).is_ok());
        assert!(validate_fm_rates(10_000_000, 48_000).is_ok());

        let adsb_actual = negotiate_sample_rate(2_000_000, |_| Ok(2_500_000)).unwrap();
        assert!(validate_adsb_rate(adsb_actual).is_err());
    }

    #[test]
    fn sample_rate_negotiation_rejects_zero_from_either_side() {
        assert!(negotiate_sample_rate(0, |_| Ok(2_400_000)).is_err());
        assert!(negotiate_sample_rate(2_400_000, |_| Ok(0)).is_err());
    }

    #[test]
    fn power_range_validation_rejects_zero_reverse_and_extra_parts() {
        assert_eq!(
            parse_power_range("88M:108M:100k"),
            Ok((88_000_000, 108_000_000, 100_000))
        );
        assert!(parse_power_range("108M:88M:100k").is_err());
        assert!(parse_power_range("88M:108M:0").is_err());
        assert!(parse_power_range("88M:108M:100k:extra").is_err());
    }

    #[test]
    fn negotiated_rate_drives_fft_size() {
        assert_eq!(sweep_fft_size(2_500_000, 10_000), Ok(256));
        assert_eq!(sweep_fft_size(10_000_000, 10_000), Ok(1024));
        assert!(sweep_fft_size(10_000_000, 1).is_err());
    }

    #[test]
    fn center_generation_is_bounded_before_allocation() {
        assert!(sweep_centers(0, u64::MAX, 5).is_err());
    }

    #[test]
    fn odd_usable_span_has_no_frequency_grid_gap() {
        // 12 * 4 / 5 = 9 Hz: asymmetric 4/5-Hz sides must meet exactly.
        let start = 100;
        let stop = 140;
        let centers = sweep_centers(start, stop, 12).unwrap();
        let mut grid = PowerGrid::new(start, stop, 1).unwrap();
        let power = vec![1.0f32; 256];
        for center in centers {
            grid.observe(center, 12, &power).unwrap();
        }
        assert_eq!(grid.rows().unwrap().len(), 41);
    }

    #[test]
    fn grid_averages_linear_power_before_log() {
        let mut grid = PowerGrid::new(100, 100, 1).unwrap();
        let mut low = vec![0.0f32; 256];
        low[128] = 1.0;
        let mut high = vec![0.0f32; 256];
        high[128] = 100.0;
        grid.observe(100, 1_000, &low).unwrap();
        grid.observe(100, 1_000, &high).unwrap();
        let rows = grid.rows().unwrap();
        let expected = 10.0 * 50.5f64.log10();
        assert!((rows[0].1 - expected).abs() < 1e-9);
    }

    fn welch_for_chunks(chunks: &[&[f32]]) -> Vec<f32> {
        let mut spectrum = sdr_fox_simd::Spectrum::new(16);
        spectrum.set_normalization(sdr_fox_simd::Normalization::Psd);
        let mut welch = WelchAccumulator::new(16).unwrap();
        for chunk in chunks {
            welch.push(&mut spectrum, chunk);
        }
        welch.average().unwrap().to_vec()
    }

    #[test]
    fn welch_half_overlap_is_chunk_invariant() {
        let iq = (0..160)
            .map(|index| ((index as f32) * 0.17).sin())
            .collect::<Vec<_>>();
        let expected = welch_for_chunks(&[&iq]);
        for split in 0..=iq.len() {
            let actual = welch_for_chunks(&[&iq[..split], &iq[split..]]);
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[test]
    fn power_grid_rejects_excessive_allocation() {
        assert!(PowerGrid::new(0, MAX_POWER_ROWS as u64, 1).is_err());
    }

    #[test]
    fn sweep_shape_rejects_unbounded_or_empty_artifacts() {
        assert!(validate_sweep_shape(1, 1, 2_400_000).is_err());
        assert!(validate_sweep_shape((MAX_SWEEP_FFT + 1) as u32, 1, 2_400_000).is_err());
        assert!(validate_sweep_shape(256, 0, 2_400_000).is_err());
        assert!(validate_sweep_shape(256, u32::MAX, 2_400_000).is_err());
        assert!(validate_sweep_shape(256, 1, 0).is_err());
        assert_eq!(
            validate_sweep_shape(256, 64, 2_400_000),
            Ok((256, 64, 16_384))
        );
    }

    #[test]
    fn sweep_sample_count_overflow_is_reported() {
        assert!(checked_sweep_sample_count(u64::MAX, 2).is_err());
    }

    #[test]
    fn negotiated_rate_drives_sweep_deadline() {
        let samples = 5_000_000;
        assert_eq!(sweep_capture_seconds(samples, 2_500_000), Ok(4));
        assert_eq!(sweep_capture_seconds(samples, 2_400_000), Ok(5));
        assert!(sweep_capture_seconds(samples, 0).is_err());
        assert!(sweep_capture_seconds(u64::MAX, 1).is_err());
    }

    #[test]
    fn sweep_drain_limit_covers_queued_and_in_flight_buffers() {
        let default = sdr_fox_core::StreamConfig::default();
        assert_eq!(sweep_drain_limit(&default).unwrap(), 64);

        let configured = sdr_fox_core::StreamConfig {
            queue_depth: 8,
            buffer_count: 16,
            ..default
        };
        assert_eq!(sweep_drain_limit(&configured).unwrap(), 24);

        let overflowing = sdr_fox_core::StreamConfig {
            queue_depth: usize::MAX,
            buffer_count: 1,
            ..configured
        };
        assert!(sweep_drain_limit(&overflowing).is_err());
    }

    #[test]
    fn buffer_fill_and_effective_settle_durations_are_ceil_rounded() {
        assert_eq!(
            buffer_fill_duration(65_536, 2_400_000).unwrap(),
            Duration::from_nanos(13_653_334)
        );
        assert_eq!(
            buffer_fill_duration(65_536, 250_000).unwrap(),
            Duration::from_micros(131_072)
        );
        assert_eq!(
            effective_settle_duration(50, 65_536, 2_400_000).unwrap(),
            Duration::from_millis(50)
        );
        assert_eq!(
            effective_settle_duration(0, 65_536, 250_000).unwrap(),
            Duration::from_micros(131_072)
        );
    }

    #[test]
    fn buffer_fill_duration_rejects_empty_or_zero_rate_inputs() {
        assert!(buffer_fill_duration(0, 1).is_err());
        assert!(buffer_fill_duration(1, 1).is_err());
        assert!(buffer_fill_duration(65_536, 0).is_err());
    }

    struct ScriptedPowerSink {
        events: VecDeque<Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>>>,
    }

    impl sdr_fox_core::StreamSink for ScriptedPowerSink {
        fn recv(&mut self) -> Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>> {
            self.events.pop_front().flatten()
        }

        fn recv_deadline(
            &mut self,
            _deadline: Instant,
        ) -> Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>> {
            self.events.pop_front().flatten()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            StreamStopHandle::new(|| {})
        }
    }

    fn marked_cf32_block(marker: f32) -> sdr_fox_core::IqBlock {
        sdr_fox_core::IqBlock {
            samples: sdr_fox_core::IqSamples::Cf32(vec![marker, 0.0]),
            dropped: 0,
            sequence: 0,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        }
    }

    fn cf32_marker(block: sdr_fox_core::IqBlock) -> f32 {
        let sdr_fox_core::IqSamples::Cf32(values) = block.samples else {
            panic!("expected CF32 test block");
        };
        values[0]
    }

    #[test]
    fn post_settle_discard_excludes_stale_first_completion() {
        let mut sink = ScriptedPowerSink {
            events: [
                Some(Ok(marked_cf32_block(1.0))),
                Some(Ok(marked_cf32_block(2.0))),
            ]
            .into(),
        };
        let shutdown = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        let discarded = discard_post_settle_block(&mut sink, deadline, &shutdown)
            .unwrap()
            .unwrap();
        assert!((cf32_marker(discarded) - 1.0).abs() < f32::EPSILON);
        let kept = crate::streaming::recv_until(&mut sink, Some(deadline), &shutdown)
            .unwrap()
            .unwrap();
        assert!((cf32_marker(kept) - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn post_settle_discard_propagates_fatal_and_honors_shutdown() {
        let mut fatal = ScriptedPowerSink {
            events: [Some(Err(sdr_fox_core::SdrError::DeviceLost))].into(),
        };
        let running = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(discard_post_settle_block(&mut fatal, deadline, &running).is_err());

        let mut idle = ScriptedPowerSink {
            events: VecDeque::new(),
        };
        let shutdown = AtomicBool::new(true);
        assert!(discard_post_settle_block(&mut idle, deadline, &shutdown)
            .unwrap()
            .is_none());
    }

    #[test]
    fn pending_artifact_names_are_unique_and_cleanup_is_best_effort() {
        let first = PendingArtifact::new("power.csv");
        let second = PendingArtifact::new("power.csv");
        assert_ne!(first.path(), second.path());

        let final_path =
            std::env::temp_dir().join(format!("sdrfox_pending_cleanup_{}.csv", std::process::id()));
        let pending = PendingArtifact::new(&final_path);
        std::fs::write(pending.path(), b"partial").unwrap();
        let pending_path = pending.path().to_owned();
        drop(pending);
        assert!(!pending_path.exists());
    }
}
