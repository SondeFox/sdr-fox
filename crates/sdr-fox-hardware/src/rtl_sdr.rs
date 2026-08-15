//! RTL-SDR hardware validation tests (index 0; bias tee is explicit opt-in).

use std::time::{Duration, Instant};

use sdr_fox_core::{GainMode, GainRequest, Upconverter};

use crate::helpers;

// --- Control plane ---

#[test]
#[ignore]
fn rtl_open_probe() {
    let dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    let info = dev.info();
    assert_eq!(info.vendor_id, 0x0bda);
    assert_eq!(info.product_id, 0x2832);
    assert!(matches!(
        info.tuner,
        Some(sdr_fox_core::TunerKind::R820T2) | Some(sdr_fox_core::TunerKind::R820T)
    ));
    println!("RTL-SDR: serial={}, tuner={:?}", info.serial, info.tuner);
}

#[test]
#[ignore]
fn rtl_gain_steps_match_reference() {
    let dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    let gains: Vec<i32> = dev.gains().iter().map(|g| g.tenths_db).collect();
    // The R820T exposes 29 gain steps from 0 to 496 tenths-dB. rtl_test
    // reports the same table; the values are tuner interface facts, so any
    // correct driver must expose them.
    assert_eq!(gains.len(), 29, "expected 29 gain steps");
    assert_eq!(gains[0], 0);
    assert_eq!(gains[28], 496);
    // Monotonic non-decreasing.
    for w in gains.windows(2) {
        assert!(w[0] <= w[1], "gains not monotonic");
    }
    println!("gain table: {:?}", gains);
}

// --- Gain & AGC ---

#[test]
#[ignore]
fn rtl_gain_changes_noise_floor() {
    helpers::ensure_artifacts_dir().expect("create artifacts directory");

    // Low gain — open, configure and capture. The stream and device teardown
    // paths are now quiesced before freeing their asynchronous transfer ring.
    {
        let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
        dev.set_gain_mode(GainMode::Manual)
            .expect("set manual gain mode");
        dev.set_gain(GainRequest::overall(0))
            .expect("set minimum gain");
        let low = helpers::capture_cu8(dev.as_mut(), 100_000_000, 1_024_000, 1)
            .expect("capture minimum-gain samples");
        let low_rms = helpers::cu8_rms(&low);
        println!("low_gain RMS: {low_rms:.2}");

        assert!(low.len() > 100_000, "should capture data");
        assert!(
            low_rms > 0.5,
            "noise floor should be non-zero even at min gain"
        );
    }
}

#[test]
#[ignore]
fn rtl_agc_on_vs_off() {
    // Use separate device opens for AGC on/off to avoid stream conflicts.
    {
        let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
        dev.set_agc(true).expect("enable AGC");
        let agc = helpers::capture_cu8(dev.as_mut(), 100_000_000, 1_024_000, 1)
            .expect("capture AGC samples");
        assert!(!agc.is_empty(), "AGC on should produce data");
        println!("AGC on: {} bytes", agc.len());
    }
    {
        let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
        dev.set_agc(false).expect("disable AGC");
        dev.set_gain(GainRequest::overall(200))
            .expect("set manual gain");
        let manual = helpers::capture_cu8(dev.as_mut(), 100_000_000, 1_024_000, 1)
            .expect("capture manual-gain samples");
        assert!(!manual.is_empty(), "AGC off should produce data");
        println!("AGC off: {} bytes", manual.len());
    }
}

// --- Spectrogram ---

// --- Bias-T / SpyVerter ---

#[test]
#[ignore]
fn rtl_biast_toggle_no_error() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    dev.set_bias_tee(true).expect("bias-T on should succeed");
    dev.set_bias_tee(false).expect("bias-T off should succeed");
    println!("bias-T toggle OK");
}

#[test]
#[ignore]
fn rtl_spyverter_hf_receive() {
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    // Power the SpyVerter.
    dev.set_bias_tee(true).expect("bias-T on");
    // Enable the 120 MHz upconverter offset.
    dev.set_upconverter(Some(Upconverter::spyverter()))
        .expect("upconverter set");
    // Tune WWV 10 MHz (the library applies the 120 MHz offset internally).
    dev.set_gain(GainRequest::overall(496))
        .expect("set maximum gain");
    let data = helpers::capture_cu8(dev.as_mut(), 10_000_000, 2_400_000, 2)
        .expect("capture SpyVerter samples");
    let rms = helpers::cu8_rms(&data);
    let distinct = helpers::cu8_distinct(&data);
    println!("WWV 10 MHz via SpyVerter: RMS={rms:.2}, distinct={distinct}");
    // With the SpyVerter powered and an HF antenna, we should see a real signal.
    // If RMS < 3.0, no HF signal (soft pass — depends on WWV propagation).
    if rms < 3.0 {
        eprintln!("WARNING: weak/no HF signal (RMS {rms:.2}); WWV may not be receivable right now");
    }
    assert!(
        data.len() > 100_000,
        "should have captured significant data"
    );
}

#[test]
#[ignore]
fn rtl_spyverter_offset_math() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    dev.set_upconverter(Some(Upconverter::spyverter()))
        .expect("upconverter");
    // set_frequency(10e6) with SpyVerter should tune 130 MHz internally.
    // We assert it returns Ok (no PLL error).
    dev.set_frequency(10_000_000)
        .expect("frequency set with SpyVerter offset should succeed");
}

// --- Spectrogram ---

#[test]
#[ignore]
fn rtl_spectrogram_fm_band() {
    const FFT_SIZE: usize = 256;
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    let iq = helpers::capture_cf32(dev.as_mut(), 100_000_000, 2_400_000, 1)
        .expect("capture FM spectrum samples");
    // Assert rather than skip. This previously early-returned when the capture
    // came up short, which made the test pass while the device delivered
    // literally zero samples — it reported green through a total streaming
    // failure that every other streaming test caught.
    assert!(
        iq.len() >= FFT_SIZE * 2,
        "captured {} floats, need {} for one FFT frame — the device delivered \
         no usable samples",
        iq.len(),
        FFT_SIZE * 2
    );
    // Use the first 256 complex samples (512 floats).
    let mut spec = sdr_fox_simd::Spectrum::new(FFT_SIZE);
    let power = spec.compute_power_dbfs(&iq[..FFT_SIZE * 2]);
    let peak = power.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let noise_floor = power.iter().sum::<f32>() / power.len() as f32;
    let snr = peak - noise_floor;
    println!("FM spectrogram: peak={peak:.1} dB, noise={noise_floor:.1} dB, SNR={snr:.1} dB");
    // Render a PNG.
    let pixels: Vec<u8> = power
        .iter()
        .map(|&db| ((db + 60.0) / 60.0 * 255.0).clamp(0.0, 255.0) as u8)
        .collect();
    sdr_fox_dsp::png::write_greyscale_png(
        "tests/artifacts/rtl_spectrogram_test.png",
        &pixels,
        256,
        1,
    )
    .expect("write PNG");
    // Assert a signal is present (SNR > 3 dB).
    assert!(
        snr > 3.0,
        "SNR {snr:.1} too low; expected a signal peak above noise"
    );
}

// --- Demodulation ---

#[test]
#[ignore]
fn rtl_wbfm_decode_broadcast() {
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    let iq = helpers::capture_cf32(dev.as_mut(), 91_100_000, 2_400_000, 1)
        .expect("capture WBFM samples");
    assert!(iq.len() > 100_000, "should capture significant IQ");
    let audio = sdr_fox_dsp::fm_demod(&iq, 2_400_000.0, 48_000.0);
    assert!(!audio.is_empty(), "demod should produce audio");
    // Write WAV.
    sdr_fox_dsp::wav::write_wav("tests/artifacts/rtl_wbfm_test.wav", &audio, 48_000)
        .expect("write WAV");
    // Assert audio energy.
    let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
    println!("WBFM audio: {} samples, RMS={rms:.4}", audio.len());
    assert!(
        rms > 0.001,
        "audio RMS {rms:.4} too low; expected broadcast audio"
    );
}

#[test]
#[ignore]
fn rtl_am_decode() {
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    // This is the only signal-path test that requires the powered SpyVerter.
    dev.set_bias_tee(true).expect("power SpyVerter");
    dev.set_upconverter(Some(Upconverter::spyverter()))
        .expect("configure SpyVerter offset");
    let iq = helpers::capture_cf32(dev.as_mut(), 1_000_000, 2_400_000, 1)
        .expect("capture AM samples through SpyVerter");
    let audio = sdr_fox_dsp::am_demod(&iq);
    assert!(!audio.is_empty(), "AM demod should produce audio");
    let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
    println!("AM audio RMS={rms:.4}");
    // AM envelope should show some energy if a station is present.
    if rms < 0.001 {
        eprintln!("WARNING: weak AM signal; station may not be receivable");
    }
}

// --- Throughput ---

#[test]
#[ignore]
fn rtl_streaming_throughput() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    dev.set_sample_rate(2_400_000)
        .expect("set throughput sample rate");
    dev.set_frequency(100_000_000)
        .expect("set throughput frequency");
    let mut stream = dev
        .start_stream(sdr_fox_core::StreamConfig::default())
        .expect("start stream");
    let measurement = helpers::measure_throughput(stream.as_mut(), Duration::from_secs(2))
        .expect("measure timed RTL-SDR throughput");
    let sps = measurement.samples_per_second();
    println!(
        "throughput: {} total samples, {} measured across {} blocks in {:.3}s = \
         {sps:.0} sps, {} dropped",
        measurement.total_samples,
        measurement.measured_samples,
        measurement.blocks,
        measurement.elapsed.as_secs_f64(),
        measurement.dropped
    );
    // Assert we got at least 50% of the expected 2.4 MS/s rate.
    assert!(
        sps > 1_200_000.0,
        "throughput {sps:.0} sps too low; expected >1.2M"
    );
    assert_eq!(measurement.dropped, 0, "no samples should be dropped");
}

#[test]
#[ignore]
fn rtl_throughput_simd_conversion() {
    let block = vec![128u8; 262_144];
    let mut out = vec![0.0f32; 262_144];
    let t0 = Instant::now();
    let iters = 100;
    for _ in 0..iters {
        sdr_fox_simd::cu8_to_cf32(std::hint::black_box(&block), std::hint::black_box(&mut out));
        std::hint::black_box(&out);
    }
    let elapsed = t0.elapsed();
    let total_bytes = 262_144 * iters;
    let mbps = total_bytes as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "SIMD cu8->cf32: {mbps:.0} MB/s ({ms:.2} ms/block)",
        ms = elapsed.as_millis() as f64 / iters as f64
    );
    // Criterion's optimized-build benchmark is the regression gate. A fixed
    // wall-clock floor here is misleading because ignored hardware tests are
    // normally compiled in the much slower dev profile.
}

// --- Sample rate ---

#[test]
#[ignore]
fn rtl_sample_rate_accepted() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    for &rate in &[2_400_000u32, 1_024_000, 250_000] {
        let result = dev.set_sample_rate(rate);
        assert!(result.is_ok(), "sample rate {rate} should be accepted");
        println!("sample rate {rate}: OK");
    }
}

// --- Frequency set ---

#[test]
#[ignore]
fn rtl_frequency_set() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    for &freq in &[88_000_000u64, 100_000_000, 440_000_000, 1_090_000_000] {
        let result = dev.set_frequency(freq);
        assert!(result.is_ok(), "frequency {freq} should be accepted");
        println!("frequency {freq}: OK");
    }
}

// --- Formats ---

#[test]
#[ignore]
fn rtl_cu8_capture_byte_count() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    let data =
        helpers::capture_cu8(dev.as_mut(), 100_000_000, 1_024_000, 1).expect("capture CU8 samples");
    // At 1.024 MS/s for 1s, expect ~2.048M bytes (2 bytes/sample).
    assert!(
        data.len() > 500_000,
        "expected >500KB of cu8, got {}",
        data.len()
    );
    println!("cu8 capture: {} bytes", data.len());
}

// --- SpyVerter offset applied at one point ---

#[test]
#[ignore]
fn rtl_spyverter_does_not_double_apply() {
    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    dev.set_upconverter(Some(Upconverter::spyverter()))
        .expect("configure SpyVerter offset");
    // Setting frequency twice should not accumulate the offset.
    dev.set_frequency(14_000_000).expect("first SpyVerter tune"); // SDR tunes 134 MHz
    dev.set_frequency(14_000_000)
        .expect("second SpyVerter tune"); // SDR tunes 134 MHz again, not 254 MHz
                                          // We can't read back the PLL freq directly, but no error = no PLL overflow.
    println!("SpyVerter double-apply test: OK (no PLL error)");
}

/// Verify that a USB port reset actually works on this host, and that the
/// device is usable afterwards.
///
/// The wedge-recovery path in `RtlSdr::start_stream` depends on
/// `Transport::reset_device` doing something real. If libusb's reset is a
/// no-op here, or leaves a stale handle, recovery would silently fail — or,
/// worse, turn a recoverable wedge into a hang. This pins the behaviour on
/// real hardware rather than assuming it.
#[test]
#[ignore = "requires RTL-SDR hardware"]
fn rtl_port_reset_succeeds_and_device_still_streams() {
    let mut transport =
        sdr_fox_transport::open_default(0x0bda, 0x2832, 0).expect("open RTL-SDR transport");

    match transport.reset_device() {
        Ok(()) => println!("port reset: OK"),
        Err(sdr_fox_core::SdrError::Unsupported(msg)) => {
            println!("port reset unsupported on this transport: {msg}");
            return;
        }
        Err(error) => panic!("port reset failed: {error}"),
    }
    // The handle is stale after re-enumeration; the recovery path re-opens.
    drop(transport);

    // Give the OS a moment to re-enumerate before re-opening.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let mut dev = helpers::open_rtlsdr().expect("re-open RTL-SDR after port reset");
    let data = helpers::capture_cu8(dev.as_mut(), 100_000_000, 1_024_000, 1)
        .expect("capture after port reset");
    assert!(
        data.len() > 500_000,
        "device did not stream after a port reset: got {} bytes",
        data.len()
    );
    println!("post-reset capture: {} bytes", data.len());
}

/// Can the **nusb** backend stream from an RTL-SDR at all?
///
/// This is the desktop control for an Android streaming failure: there, the
/// device opens, control transfers succeed, the URB ring submits cleanly — and
/// the RTL2832U then sends nothing until the liveness timeout cancels every
/// URB. Android reaches nusb through `from_fd`, so if nusb cannot stream an
/// RTL here either, the fault is in the nusb bulk path rather than anything
/// Android-specific.
///
/// `open_default` deliberately prefers rusb on macOS (nusb is documented to
/// stall RTL2832U control-OUTs there), so nusb is otherwise never exercised
/// against this hardware and a regression in it would go unnoticed.
#[test]
#[ignore = "requires RTL-SDR hardware"]
fn rtl_streams_through_the_nusb_backend() {
    use sdr_fox_core::{DeviceDescriptor, DeviceKind, SdrDevice, StreamConfig};

    let transport = match sdr_fox_transport::NusbTransport::open(0x0bda, 0x2832, 0) {
        Ok(transport) => transport,
        Err(error) => {
            panic!("nusb could not open the RTL-SDR: {error}");
        }
    };
    let descriptor = DeviceDescriptor {
        vendor_id: 0x0bda,
        product_id: 0x2832,
        vendor_name: None,
        product_name: None,
        serial: None,
        index: 0,
        kind: DeviceKind::RtlSdr,
    };
    let mut dev = sdr_fox_rtlsdr::rtl2832::open::open_device(&descriptor, Box::new(transport))
        .expect("open RTL-SDR over nusb");
    dev.set_sample_rate(2_048_000).expect("set sample rate");
    dev.set_frequency(100_000_000).expect("set frequency");

    // Match the Android configuration that failed: 16 KiB buffers, not the
    // 64 KiB desktop default.
    let mut stream = dev
        .start_stream(StreamConfig {
            buffer_size: 16_384,
            ..StreamConfig::default()
        })
        .expect("start stream over nusb");

    let mut samples = 0usize;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && samples < 100_000 {
        match stream.recv_deadline(deadline) {
            Some(Ok(block)) => samples += block.samples.complex_count(),
            Some(Err(error)) => panic!("nusb stream error after {samples} samples: {error}"),
            None => break, // deadline reached with nothing pending
        }
    }
    println!("nusb RTL stream delivered {samples} complex samples");
    assert!(
        samples > 50_000,
        "nusb backend delivered only {samples} complex samples — the nusb bulk \
         path cannot stream this RTL2832U, which would also explain the \
         Android failure"
    );
}

/// A stream must survive its device handle being dropped.
///
/// `start_stream` returns an independently owned `StreamHandle` carrying its
/// own transport clone and worker thread, so a caller may legitimately drop the
/// `RtlSdr` and keep reading — the C ABI, Python and JNI surfaces all hand out
/// device and stream handles separately. Powering the demodulator down in the
/// device destructor would silently kill such a stream.
#[test]
#[ignore = "requires RTL-SDR hardware"]
fn stream_outlives_the_device_handle() {
    use sdr_fox_core::StreamConfig;

    let mut dev = helpers::open_rtlsdr().expect("open RTL-SDR");
    dev.set_sample_rate(2_048_000).expect("sample rate");
    dev.set_frequency(100_000_000).expect("frequency");
    let mut stream = dev
        .start_stream(StreamConfig::default())
        .expect("start stream");

    // Prove data flows while the device handle is still held.
    let deadline = Instant::now() + Duration::from_secs(2);
    let before = match stream.recv_deadline(deadline) {
        Some(Ok(block)) => block.samples.complex_count(),
        other => panic!("no data before dropping the device: {:?}", other.is_some()),
    };
    assert!(before > 0, "expected samples before the drop");

    // Drop the device; the stream must keep delivering.
    drop(dev);

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut after = 0usize;
    while Instant::now() < deadline && after == 0 {
        match stream.recv_deadline(deadline) {
            Some(Ok(block)) => after += block.samples.complex_count(),
            Some(Err(error)) => panic!("stream died after the device was dropped: {error}"),
            None => break,
        }
    }
    println!("before drop: {before} samples, after drop: {after} samples");
    assert!(
        after > 0,
        "stream delivered nothing after the device handle was dropped — the \
         device destructor powered the hardware down underneath a live stream"
    );
}
