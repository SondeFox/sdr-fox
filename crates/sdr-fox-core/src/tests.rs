//! Unit tests for `sdr-fox-core`. No hardware required.

#![cfg(test)]

use std::str::FromStr;

use crate::device::{DeviceInfo, DeviceKind, SdrDevice, Upconverter};
use crate::error::{SdrError, TunerError};
use crate::gain::{GainMode, GainRequest, GainStageId, GainStep};
use crate::sample::{IqBlock, IqFormat, IqSamples, StreamConfig, StreamHandle};

#[test]
fn error_is_disconnected_classifies_expected_variants() {
    assert!(SdrError::DeviceLost.is_disconnected());
    assert!(SdrError::DeviceNotFound("idx 0".into()).is_disconnected());
    // Transport errors are NOT necessarily disconnects (could be a transient
    // I/O failure on a still-present device), so they no longer match.
    assert!(!SdrError::Transport("usb".into()).is_disconnected());
    // New transient/streaming variants are not disconnects:
    assert!(!SdrError::Timeout.is_disconnected());
    assert!(!SdrError::Stall.is_disconnected());
    assert!(!SdrError::Overflow {
        dropped_samples: 10
    }
    .is_disconnected());
    assert!(!SdrError::Cancelled.is_disconnected());
    // Non-disconnect errors:
    assert!(!SdrError::DeviceBusy.is_disconnected());
    assert!(!SdrError::InvalidSampleRate { rate_hz: 7 }.is_disconnected());
}

#[test]
fn error_is_timeout_matches_timeout_variant_only() {
    assert!(SdrError::Timeout.is_timeout());
    assert!(!SdrError::Stall.is_timeout());
    assert!(!SdrError::DeviceLost.is_timeout());
    assert!(!SdrError::Transport("slow".into()).is_timeout());
}

#[test]
fn error_from_tuner_error_coerces_into_sdr_error() {
    let inner = TunerError::PllNotLocked { freq_hz: 100_000 };
    let outer: SdrError = inner.into();
    assert!(matches!(
        outer,
        SdrError::Tuner(TunerError::PllNotLocked { .. })
    ));
}

#[test]
fn iq_format_round_trips_through_display_fromstr() {
    for fmt in [IqFormat::Cu8, IqFormat::Cs8, IqFormat::Cs16, IqFormat::Cf32] {
        let s = fmt.to_string();
        let back = IqFormat::from_str(&s).unwrap();
        assert_eq!(fmt, back, "round-trip failed for {fmt}");
    }
}

#[test]
fn iq_format_rejects_unknown_names() {
    assert!(matches!(
        IqFormat::from_str("not-a-format"),
        Err(SdrError::InvalidParameter(_))
    ));
}

#[test]
fn iq_format_bytes_per_sample_matches_wire_sizes() {
    assert_eq!(IqFormat::Cu8.bytes_per_sample(), 2);
    assert_eq!(IqFormat::Cs8.bytes_per_sample(), 2);
    assert_eq!(IqFormat::Cs16.bytes_per_sample(), 4);
    assert_eq!(IqFormat::Cf32.bytes_per_sample(), 8);
}

#[test]
fn iq_format_accepts_aliases() {
    assert_eq!(IqFormat::from_str("Cfloat").unwrap(), IqFormat::Cf32);
    assert_eq!(IqFormat::from_str("RTLSDR").unwrap(), IqFormat::Cu8);
}

#[test]
fn iq_samples_complex_count_is_pairs_not_bytes() {
    let cu8 = IqSamples::Cu8(vec![0; 256]); // 128 complex samples
    assert_eq!(cu8.complex_count(), 128);
    let cf32 = IqSamples::Cf32(vec![0.0; 64]); // 32 complex samples
    assert_eq!(cf32.complex_count(), 32);
    assert_eq!(IqSamples::Cs8(vec![0; 10]).complex_count(), 5);
    assert_eq!(IqSamples::Cs16(vec![0; 8]).complex_count(), 4);
}

#[test]
fn iq_samples_format_matches_backing() {
    assert_eq!(IqSamples::Cu8(vec![]).format(), IqFormat::Cu8);
    assert_eq!(IqSamples::Cf32(vec![]).format(), IqFormat::Cf32);
}

#[test]
fn spyverter_translate_adds_120mhz_non_inverting() {
    let spv = Upconverter::spyverter();
    assert_eq!(spv.lo_hz, 120_000_000);
    assert!(!spv.invert);
    // 14.2 MHz HF → 134.2 MHz on the SDR.
    assert_eq!(spv.translate(14_200_000), 134_200_000);
    // 0 Hz → exactly the LO.
    assert_eq!(spv.translate(0), 120_000_000);
}

#[test]
fn upconverter_inverting_subtracts_lo() {
    let inv = Upconverter {
        lo_hz: 100_000_000,
        invert: true,
    };
    assert_eq!(inv.translate(150_000_000), 50_000_000);
}

#[test]
fn upconverter_translate_saturates_below_zero() {
    let inv = Upconverter {
        lo_hz: 200_000_000,
        invert: true,
    };
    // Would underflow; must saturate to 0, not wrap.
    assert_eq!(inv.translate(100_000_000), 0);
}

#[test]
fn upconverter_default_is_spyverter() {
    assert_eq!(Upconverter::default(), Upconverter::spyverter());
}

#[test]
fn stream_config_default_is_sensible() {
    let cfg = StreamConfig::default();
    assert_eq!(cfg.format, IqFormat::Cu8);
    assert!(cfg.buffer_count >= 8, "ring depth should be >= 8");
    assert_eq!(
        cfg.buffer_size % 512,
        0,
        "buffer_size must be a 512 multiple"
    );
    assert!(cfg.queue_depth > 0);
}

#[test]
fn closest_gain_snaps_to_nearest_overall_step() {
    // A toy device whose only "OVERALL" steps are 0, 297, 338, 372, 407, ..tenths dB.
    let dev = ToyDevice {
        gains: vec![
            GainStep::new("OVERALL", 0),
            GainStep::new("OVERALL", 297),
            GainStep::new("OVERALL", 338),
            GainStep::new("OVERALL", 372),
            GainStep::new("OVERALL", 407),
        ],
    };
    assert_eq!(dev.closest_gain(300), Some(297));
    assert_eq!(dev.closest_gain(340), Some(338));
    assert_eq!(dev.closest_gain(10_000), Some(407)); // clamps to max
    assert_eq!(dev.closest_gain(-5), Some(0)); // clamps to min
}

#[test]
fn closest_gain_returns_none_when_no_overall_steps() {
    let dev = ToyDevice {
        gains: vec![GainStep::new("LNA", 100), GainStep::new("VGA", 50)],
    };
    assert_eq!(dev.closest_gain(80), None);
}

/// A minimal `SdrDevice` impl used only to exercise `closest_gain` default logic.
struct ToyDevice {
    gains: Vec<GainStep>,
}

impl SdrDevice for ToyDevice {
    fn info(&self) -> &DeviceInfo {
        unreachable!()
    }
    fn set_sample_rate(&mut self, _hz: u32) -> Result<u32, SdrError> {
        unreachable!()
    }
    fn set_frequency(&mut self, _hz: u64) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_bandwidth(&mut self, _hz: u32) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_gain(&mut self, _req: GainRequest) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_gain_mode(&mut self, _mode: GainMode) -> Result<(), SdrError> {
        unreachable!()
    }
    fn gains(&self) -> &[GainStep] {
        &self.gains
    }
    fn set_bias_tee(&mut self, _on: bool) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_agc(&mut self, _on: bool) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_frequency_correction_ppm(&mut self, _ppm: f64) -> Result<(), SdrError> {
        unreachable!()
    }
    fn set_upconverter(&mut self, _up: Option<crate::device::Upconverter>) -> Result<(), SdrError> {
        unreachable!()
    }
    fn start_stream(&mut self, _cfg: StreamConfig) -> Result<StreamHandle, SdrError> {
        unreachable!()
    }
}

#[test]
fn gain_stage_id_codes_are_the_jni_contract() {
    // 0=LNA, 1=MIXER, 2=VGA is a stable cross-language contract; changing it
    // silently breaks every foreign binding.
    assert_eq!(GainStageId::Lna.code(), 0);
    assert_eq!(GainStageId::Mixer.code(), 1);
    assert_eq!(GainStageId::Vga.code(), 2);
    assert_eq!(i32::from(GainStageId::Lna), 0);
    assert_eq!(i32::from(GainStageId::Mixer), 1);
    assert_eq!(i32::from(GainStageId::Vga), 2);
}

#[test]
fn gain_stage_id_from_code_round_trips_and_rejects_unknown() {
    for stage in [GainStageId::Lna, GainStageId::Mixer, GainStageId::Vga] {
        assert_eq!(GainStageId::from_code(stage.code()), Some(stage));
        assert_eq!(GainStageId::try_from(stage.code()).unwrap(), stage);
    }
    for bad in [-1, 3, 42, i32::MIN, i32::MAX] {
        assert_eq!(GainStageId::from_code(bad), None);
        assert!(matches!(
            GainStageId::try_from(bad),
            Err(SdrError::InvalidParameter(_))
        ));
    }
}

#[test]
fn gain_stage_id_names_match_per_stage_convention() {
    // These strings are what drivers (e.g. Airspy) match on in
    // GainRequest::PerStage; they must stay exact.
    assert_eq!(GainStageId::Lna.name(), "LNA");
    assert_eq!(GainStageId::Mixer.name(), "MIXER");
    assert_eq!(GainStageId::Vga.name(), "VGA");
    assert_eq!(GainStageId::Mixer.to_string(), "MIXER");
}

#[test]
fn gain_request_per_stage_builds_matching_variant() {
    let req = GainRequest::per_stage(GainStageId::Vga, 120);
    assert_eq!(
        req,
        GainRequest::PerStage {
            name: "VGA",
            tenths_db: 120
        }
    );
}

#[test]
fn supported_sample_rates_defaults_to_not_enumerable() {
    let dev = ToyDevice { gains: vec![] };
    assert!(
        dev.supported_sample_rates().is_empty(),
        "default must mean 'not enumerable', i.e. empty"
    );
}

#[test]
fn set_stage_agc_defaults_to_unsupported() {
    let mut dev = ToyDevice { gains: vec![] };
    for stage in [GainStageId::Lna, GainStageId::Mixer, GainStageId::Vga] {
        for on in [true, false] {
            assert!(matches!(
                dev.set_stage_agc(stage, on),
                Err(SdrError::Unsupported(_))
            ));
        }
    }
}

#[test]
fn iq_block_carries_per_block_clip_telemetry() {
    let block = IqBlock {
        samples: IqSamples::Cf32(vec![0.0; 16]), // 8 complex samples
        dropped: 42,
        sequence: 7,
        timestamp: None,
        clips: 3,
        raw_samples: 4096,
    };
    // clips/raw_samples describe the RAW ADC domain of THIS block only
    // (per-block, unlike the monotonic dropped/sequence), so raw_samples may
    // legitimately exceed the delivered complex count.
    assert_eq!(block.clips, 3);
    assert_eq!(block.raw_samples, 4096);
    assert!(block.raw_samples > block.samples.complex_count() as u64);
    let cloned = block.clone();
    assert_eq!(cloned.clips, block.clips);
    assert_eq!(cloned.raw_samples, block.raw_samples);
    assert_eq!(cloned.dropped, block.dropped);
    assert_eq!(cloned.sequence, block.sequence);
}

#[test]
fn no_supported_tuner_is_distinct_from_pll_programming_failure() {
    let probe_miss: SdrError = TunerError::NoSupportedTuner.into();
    assert!(matches!(
        probe_miss,
        SdrError::Tuner(TunerError::NoSupportedTuner)
    ));
    assert!(!matches!(
        probe_miss,
        SdrError::Tuner(TunerError::PllProgrammingFailed)
    ));
    assert!(!probe_miss.is_disconnected());
    assert_eq!(
        TunerError::NoSupportedTuner.to_string(),
        "no supported tuner detected"
    );
}

#[test]
fn device_kind_is_copy_and_eq() {
    let k = DeviceKind::RtlSdr;
    let k2 = k;
    assert_eq!(k, k2);
    assert_ne!(k, DeviceKind::Airspy);
}

#[allow(clippy::field_reassign_with_default)]
fn _dummy_info() -> DeviceInfo {
    let mut i = DeviceInfo::default();
    i.kind = DeviceKind::Airspy;
    i
}

#[allow(dead_code)]
mod compile_time_guarantees {
    use super::SdrError;
    // SdrError is non_exhaustive externally; ensure we can still match inside.
    #[test]
    fn match_on_internal_variant() {
        let e = SdrError::DeviceBusy;
        assert!(!e.is_disconnected());
    }
}
