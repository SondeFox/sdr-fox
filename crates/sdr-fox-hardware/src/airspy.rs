//! Airspy hardware validation tests (index 1, standalone).

use std::time::Duration;

use crate::helpers;
use sdr_fox_core::{GainMode, SdrError};

#[test]
#[ignore]
fn airspy_open_probe() {
    let dev = helpers::open_airspy().expect("open Airspy");
    let info = dev.info();
    assert_eq!(info.vendor_id, 0x1d50);
    assert_eq!(info.product_id, 0x60a1);
    assert!(matches!(info.kind, sdr_fox_core::DeviceKind::Airspy));
    println!("Airspy: name={}, serial={}", info.product_name, info.serial);
}

#[test]
#[ignore]
fn airspy_sample_rates() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    // The Airspy may not support all rates. Accept whatever works.
    let rates = [2_500_000u32, 3_000_000, 6_000_000, 9_000_000, 10_000_000];
    let mut accepted = 0;
    for &rate in &rates {
        match dev.set_sample_rate(rate) {
            Ok(_) => {
                println!("Airspy rate {rate}: OK");
                accepted += 1;
            }
            Err(SdrError::InvalidSampleRate { .. } | SdrError::Unsupported(_)) => {
                println!("Airspy rate {rate}: not supported");
            }
            Err(error) => panic!("Airspy rate probe failed unexpectedly: {error}"),
        }
    }
    assert!(accepted >= 2, "at least 2 sample rates should be accepted");
}

#[test]
#[ignore]
fn airspy_per_stage_gain() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    dev.set_gain_mode(GainMode::Manual)
        .expect("set manual gain mode");

    // Min gain.
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "LNA",
        tenths_db: 0,
    })
    .expect("set minimum LNA gain");
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "MIXER",
        tenths_db: 0,
    })
    .expect("set minimum mixer gain");
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "VGA",
        tenths_db: 0,
    })
    .expect("set minimum VGA gain");
    let low = helpers::capture_cu8(dev.as_mut(), 100_000_000, 2_500_000, 1)
        .expect("capture minimum-gain samples");
    let low_rms = helpers::cu8_rms(&low);

    // Max gain.
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "LNA",
        tenths_db: 140,
    })
    .expect("set maximum LNA gain");
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "MIXER",
        tenths_db: 150,
    })
    .expect("set maximum mixer gain");
    dev.set_gain(sdr_fox_core::GainRequest::PerStage {
        name: "VGA",
        tenths_db: 150,
    })
    .expect("set maximum VGA gain");
    let high = helpers::capture_cu8(dev.as_mut(), 100_000_000, 2_500_000, 1)
        .expect("capture maximum-gain samples");
    let high_rms = helpers::cu8_rms(&high);

    println!("Airspy gain: low_rms={low_rms:.2}, high_rms={high_rms:.2}");
    assert!(high_rms > low_rms, "high-gain RMS should exceed low-gain");
}

#[test]
#[ignore]
fn airspy_lna_agc_toggle() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    // AGC on.
    dev.set_gain_mode(GainMode::Auto)
        .expect("enable Airspy AGC");
    let agc =
        helpers::capture_cu8(dev.as_mut(), 100_000_000, 2_500_000, 1).expect("capture AGC samples");
    assert!(!agc.is_empty(), "AGC on should produce data");
    // AGC off.
    dev.set_gain_mode(GainMode::Manual)
        .expect("disable Airspy AGC");
    let manual = helpers::capture_cu8(dev.as_mut(), 100_000_000, 2_500_000, 1)
        .expect("capture manual-gain samples");
    assert!(!manual.is_empty(), "AGC off should produce data");
    println!(
        "Airspy AGC: on={} bytes, off={} bytes",
        agc.len(),
        manual.len()
    );
}

#[test]
#[ignore]
fn airspy_bias_tee() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    dev.set_bias_tee(true).expect("Airspy bias-T on");
    dev.set_bias_tee(false).expect("Airspy bias-T off");
    println!("Airspy bias-T toggle OK");
}

#[test]
#[ignore]
fn airspy_spectrogram_fm() {
    const FFT_SIZE: usize = 256;
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_airspy().expect("open Airspy");
    let iq = helpers::capture_cf32_samples(
        dev.as_mut(),
        100_000_000,
        2_500_000,
        FFT_SIZE,
        Duration::from_secs(2),
    )
    .expect("capture one Airspy FFT frame");
    assert_eq!(
        iq.len(),
        FFT_SIZE * 2,
        "capture helper must return one exact complex FFT frame"
    );
    let mut spec = sdr_fox_simd::Spectrum::new(FFT_SIZE);
    let power = spec.compute_power_dbfs(&iq);
    let peak = power.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let noise_floor = power.iter().sum::<f32>() / power.len() as f32;
    let snr = peak - noise_floor;
    println!("Airspy FM spectrogram: SNR={snr:.1} dB");
    let pixels: Vec<u8> = power
        .iter()
        .map(|&db| ((db + 60.0) / 60.0 * 255.0).clamp(0.0, 255.0) as u8)
        .collect();
    sdr_fox_dsp::png::write_greyscale_png(
        "tests/artifacts/airspy_spectrogram_test.png",
        &pixels,
        FFT_SIZE as u32,
        1,
    )
    .expect("write PNG");
    assert!(snr.is_finite(), "spectrogram SNR must be finite");
}

#[test]
#[ignore]
fn airspy_wbfm_decode() {
    helpers::ensure_artifacts_dir().expect("create artifacts directory");
    let mut dev = helpers::open_airspy().expect("open Airspy");
    let iq = helpers::capture_cf32(dev.as_mut(), 91_100_000, 2_500_000, 1)
        .expect("capture Airspy WBFM samples");
    let audio = sdr_fox_dsp::fm_demod(&iq, 2_500_000.0, 48_000.0);
    sdr_fox_dsp::wav::write_wav("tests/artifacts/airspy_wbfm_test.wav", &audio, 48_000)
        .expect("write WAV");
    let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len().max(1) as f32).sqrt();
    println!("Airspy WBFM: {} samples, RMS={rms:.4}", audio.len());
    assert!(!audio.is_empty());
}

#[test]
#[ignore]
fn airspy_frequency_set() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    for &freq in &[88_000_000u64, 100_000_000, 440_000_000, 1_090_000_000] {
        let result = dev.set_frequency(freq);
        assert!(result.is_ok(), "Airspy freq {freq} should be accepted");
        println!("Airspy freq {freq}: OK");
    }
}

#[test]
#[ignore]
fn airspy_ppm_unsupported() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    let result = dev.set_frequency_correction_ppm(1.0);
    assert!(result.is_err(), "Airspy PPM should be unsupported (TCXO)");
    println!("Airspy PPM correctly unsupported");
}

#[test]
#[ignore]
fn airspy_bandwidth_unsupported() {
    let mut dev = helpers::open_airspy().expect("open Airspy");
    let result = dev.set_bandwidth(1_000_000);
    assert!(
        result.is_err(),
        "Airspy bandwidth should be unsupported (fixed filters)"
    );
    println!("Airspy bandwidth correctly unsupported");
}
