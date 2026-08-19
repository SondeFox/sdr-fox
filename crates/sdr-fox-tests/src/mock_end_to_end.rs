//! End-to-end mock-stack integration tests.

use sdr_fox_core::{TransferDirection, Transport};
use sdr_fox_simd::{cu8_to_cf32, Spectrum, Window};
use sdr_fox_transport::{MockTransport, ScriptedReply};

/// Firmware-mandated register-write encoding is byte-exact.
#[test]
fn rtl2832_register_write_encoding_is_byte_exact() {
    let mut mock = MockTransport::new();
    // demod_write_reg(page=1, addr=0x15, val=0x00, len=1) + flush read
    sdr_fox_rtlsdr::rtl2832::demod_write_reg(&mut mock, 1, 0x15, 0x00, 1).unwrap();

    let recorded = mock.recorded();
    assert_eq!(recorded.len(), 2, "write + flush read");

    let w = &recorded[0];
    assert_eq!(w.direction, TransferDirection::Out);
    assert_eq!(w.request, 0);
    assert_eq!(w.value, (0x15u16 << 8) | 0x20);
    assert_eq!(w.index, 0x10 | 1);
    assert_eq!(w.data, vec![0x00]);

    let f = &recorded[1];
    assert_eq!(f.direction, TransferDirection::In);
    assert_eq!(f.value, (0x01u16 << 8) | 0x20);
    assert_eq!(f.index, 0x0a);
}

/// I2C repeater enable/disable writes the right demod value.
#[test]
fn i2c_repeater_toggle() {
    for (on, val) in [(true, 0x18u8), (false, 0x10u8)] {
        let mut mock = MockTransport::new();
        sdr_fox_rtlsdr::rtl2832::set_i2c_repeater(&mut mock, on).unwrap();
        let writes: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.direction == TransferDirection::Out && r.data == vec![val])
            .collect();
        assert_eq!(writes.len(), 1, "repeater {on} should write 0x{val:02x}");
    }
}

/// R820T2 PLL covers the common amateur bands.
#[test]
fn r820t2_pll_covers_common_bands() {
    use sdr_fox_rtlsdr::tuners::r82xx::R82xx;
    // R820T2 native range is ~24-1766 MHz. Frequencies below ~28 MHz are at
    // the VCO's lower edge; the R820T2 can reach them with mix_div=64 but the
    // VCO drops just below the nominal 1.77 GHz minimum. We test frequencies
    // that are solidly in range.
    for band_mhz in [50u64, 144, 222, 440, 915, 1280] {
        let hz = band_mhz * 1_000_000;
        let (_, vco) = R82xx::compute_pll(hz, 28_800_000).expect("PLL must lock");
        assert!(
            (1_770_000_000..=3_540_000_000).contains(&vco),
            "VCO out of range at {band_mhz} MHz: {vco}"
        );
    }
}

/// SIMD cu8→cf32 matches the scalar reference on a realistic block.
#[test]
fn simd_matches_scalar_on_realistic_block() {
    let input: Vec<u8> = (0..65_536).map(|i| (i % 256) as u8).collect();
    let mut scalar = vec![0.0f32; input.len()];
    for (i, &b) in input.iter().enumerate() {
        scalar[i] = (f32::from(b) - 127.5) * (1.0 / 127.5);
    }
    let mut simd = vec![0.0f32; input.len()];
    cu8_to_cf32(&input, &mut simd);
    for (i, (a, b)) in scalar.iter().zip(simd.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-6,
            "divergence at {i}: scalar={a} simd={b}"
        );
    }
}

/// Spectrum analyzer places a known tone at the expected fftshifted bin.
#[test]
fn spectrum_detects_known_tone() {
    let bins = 128;
    let mut spec = Spectrum::new(bins);
    spec.set_window(Window::Rectangular);
    let k = 10;
    let two_pi = 2.0 * std::f32::consts::PI;
    let iq: Vec<f32> = (0..bins)
        .flat_map(|n| {
            let phase = two_pi * k as f32 * n as f32 / bins as f32;
            [phase.cos(), phase.sin()]
        })
        .collect();
    let power = spec.compute_power_dbfs(&iq).to_vec();
    let mut peak = 0;
    for (i, &p) in power.iter().enumerate() {
        if p > power[peak] {
            peak = i;
        }
    }
    assert_eq!(peak, bins / 2 + k);
}

/// SpyVerter 120 MHz offset applied correctly.
#[test]
fn spyverter_offset_applied_correctly() {
    let spv = sdr_fox_core::Upconverter::spyverter();
    assert_eq!(spv.translate(14_000_000), 134_000_000);
    assert_eq!(spv.translate(0), 120_000_000);
}

/// Mock transport round-trips a scripted control exchange.
#[test]
fn mock_transport_scripted_exchange() {
    let mut mock = MockTransport::new();
    mock.push_reply(ScriptedReply::any_in(vec![0x69]));
    let reply = mock
        .control_in(&sdr_fox_core::ControlRequest::vendor_in(0, 0, 0x34, 1))
        .unwrap();
    assert_eq!(reply, vec![0x69]);
}

/// Backend matchers accept the right VID:PID pairs.
#[test]
fn backends_match_their_vid_pid() {
    use sdr_fox_airspy::AirspyBackend;
    use sdr_fox_core::{DeviceDescriptor, DeviceKind, SdrBackend};
    use sdr_fox_rtlsdr::RtlSdrBackend;

    let rtl = DeviceDescriptor {
        vendor_id: 0x0bda,
        product_id: 0x2832,
        vendor_name: None,
        product_name: None,
        serial: None,
        index: 0,
        kind: DeviceKind::RtlSdr,
    };
    let airspy = DeviceDescriptor {
        vendor_id: 0x1d50,
        product_id: 0x60a1,
        ..rtl.clone()
    };
    assert!(RtlSdrBackend.matches(&rtl));
    assert!(!RtlSdrBackend.matches(&airspy));
    assert!(AirspyBackend.matches(&airspy));
    assert!(!AirspyBackend.matches(&rtl));
}

/// Hardware-gated: opens the attached RTL-SDR via nusb. Run with --ignored.
#[test]
#[ignore = "requires an attached RTL-SDR; run with --ignored"]
fn hardware_open_rtlsdr() {
    use sdr_fox_transport::NusbTransport;
    // An RTL dongle enumerates as 0bda:2832 (bare chipset) or 0bda:2838
    // (EEPROM-configured, e.g. NooElec NESDR Smart XTR); accept either.
    let opened = [(0x0bda, 0x2832), (0x0bda, 0x2838)]
        .iter()
        .any(|&(vid, pid)| NusbTransport::open(vid, pid, 0).is_ok());
    assert!(opened, "should open the attached RTL-SDR at a known USB id");
}
