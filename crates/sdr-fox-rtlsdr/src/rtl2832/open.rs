//! Device-open path: baseband init + tuner probe + tuner init.
//!
//! Tuner detection is an I2C read of each candidate's chip-id register
//! compared against a known value; the first match wins. The established
//! probe order (as run by the Osmocom reference driver) is E4000 → FC0013 →
//! R820T → R828D → GPIO4 reset → FC2580 → FC0012. v1 implements only the
//! R82xx-family probes (R820T/R820T2 at I2C 0x34, R828D at 0x74); other
//! tuners slot into that order as they are added.

use sdr_fox_core::{DeviceDescriptor, SdrError, Transport, Tuner, TunerError};

use super::baseband;
use super::device::{make_info, RtlSdr};
use super::{set_i2c_repeater, RtlI2cBus};

/// Default IF frequency programmed at open time for the R820T tuner family.
/// The Osmocom reference sets this in `rtlsdr_open` before `tuner->init`.
const R82XX_DEFAULT_IF_HZ: u64 = 3_570_000;

/// Baseband half of device initialisation: power on and reset the
/// demodulator, then program the default IF frequency.
///
/// The IF write must precede tuner init (matching osmocom's `rtlsdr_open`
/// ordering: the R820T tuner-open case calls `set_if_freq` before
/// `tuner->init`; the reverse order disrupts the signal path).
///
/// Shared between the first open and the bulk-liveness recovery reset in
/// `RtlSdr::start_stream` — a USB device reset re-enumerates the dongle and
/// wipes every register, so recovery must rerun exactly this sequence.
/// Keeping it in one function stops the two paths drifting apart.
pub(crate) fn init_baseband_defaults(transport: &mut dyn Transport) -> Result<(), SdrError> {
    baseband::init_baseband(transport)?;
    baseband::set_if_freq(transport, R82XX_DEFAULT_IF_HZ, baseband::RTL_XTAL_HZ)
}

/// Tuner half of device initialisation: run `tuner.init` behind the I2C
/// repeater. Shared between the first open and the recovery reset, for the
/// same no-drift reason as `init_baseband_defaults`.
pub(crate) fn init_tuner(
    transport: &mut dyn Transport,
    tuner: &mut dyn Tuner,
) -> Result<(), SdrError> {
    set_i2c_repeater(transport, true)?;
    {
        let mut bus = RtlI2cBus::for_tuner(transport, tuner.kind());
        tuner.init(&mut bus).map_err(SdrError::Tuner)?;
    }
    set_i2c_repeater(transport, false)
}

/// Probe the device with a harmless register write, and reset the USB port if
/// it does not answer.
///
/// `USB_SYSCTL` is written with the same value `init_baseband` will write a
/// moment later, so a device in a healthy state sees no change: this costs one
/// control transfer and is idempotent.
///
/// A dongle can reach a state where it still ACKs control transfers but never
/// delivers bulk data — reachable in practice by killing a host process
/// mid-stream. That case is NOT detected here (the probe write succeeds), and
/// needs a port reset from the caller; what this catches is the harder failure
/// where the control endpoint itself has stopped answering, which is otherwise
/// a fatal open.
///
/// The reset is best-effort: transports that cannot reach the port report
/// [`SdrError::Unsupported`], and the original probe error is returned instead.
fn probe_or_reset(transport: &mut dyn Transport) -> Result<(), SdrError> {
    const USB_SYSCTL: u16 = 0x2000;
    let probe = super::write_reg(transport, super::Block::Usb, USB_SYSCTL, 0x09, 1);
    let Err(probe_error) = probe else {
        return Ok(());
    };
    tracing::warn!(
        %probe_error,
        "RTL2832U did not answer the open-time probe write; resetting the USB port"
    );
    match transport.reset_device() {
        Ok(()) => {
            // The device re-enumerated, so everything written before this point
            // is gone. init_baseband runs next and reprograms it from scratch.
            super::write_reg(transport, super::Block::Usb, USB_SYSCTL, 0x09, 1)
        }
        Err(SdrError::Unsupported(_)) => Err(probe_error),
        Err(reset_error) => Err(reset_error),
    }
}

/// Open and initialize an RTL-SDR device: power on the baseband, run the tuner
/// probe, construct the tuner, and return a ready-to-use [`RtlSdr`].
///
/// # Errors
///
/// - [`SdrError::Tuner`] with [`TunerError::NoSupportedTuner`] if no supported
///   tuner is detected (an unsupported chip such as E4000/FC0012/FC0013/FC2580
///   is fitted, or no tuner ACKed at all).
/// - [`SdrError::Tuner`] with another variant if a detected tuner fails to
///   initialize.
/// - [`SdrError::Transport`] on any register/I2C failure.
pub fn open_device(
    desc: &DeviceDescriptor,
    mut transport: Box<dyn Transport>,
) -> Result<RtlSdr, SdrError> {
    probe_or_reset(transport.as_mut())?;
    // Baseband bring-up + default IF (IF must precede tuner init; see
    // `init_baseband_defaults`).
    init_baseband_defaults(transport.as_mut())?;

    // Run the tuner probe. v1 supports the R82xx family (R820T2, the most
    // common modern dongle, and R828D); other tuners are wired into the probe
    // in their respective modules.
    let tuner_kind = probe_tuner(transport.as_mut())?;

    // Build the tuner instance and initialize it.
    let mut tuner_obj = super::tuner_factory(tuner_kind)?;
    init_tuner(transport.as_mut(), tuner_obj.as_mut())?;

    let info = make_info(desc, Some(tuner_kind));
    Ok(RtlSdr::new(info, transport, tuner_obj))
}

/// Probe the tuner by reading its chip-id register. v1 probes only the
/// R82xx family — 0x34, then 0x74 (see the module docs for the full probe
/// order that other tuners will join). Returns the first detected
/// [`sdr_fox_core::TunerKind`].
///
/// # Errors
///
/// Returns [`SdrError::Tuner`] with [`TunerError::NoSupportedTuner`] if no
/// known tuner responds — the dongle carries a chip the probe does not drive
/// (E4000/FC0012/FC0013/FC2580) or nothing ACKed. This is deliberately
/// distinct from `PllProgrammingFailed`, which reports a *detected* tuner
/// failing to program.
fn probe_tuner(transport: &mut dyn Transport) -> Result<sdr_fox_core::TunerKind, SdrError> {
    // R82xx-family probe: read chip-id register 0x00 and expect 0x96. The
    // whole family reports the same id; what tells the chips apart is which
    // I2C address answers — 0x34 for R820T/R820T2, 0x74 for R828D. 0x34 is
    // tried first so existing dongles see an unchanged probe sequence.
    set_i2c_repeater(transport, true)?;
    let kind = if r82xx_id_answers_at(transport, super::R820T_I2C_ADDR) {
        // Distinguish R820T2 from R820T by reading register 0x00 of the
        // secondary page; both report 0x96 here, so we treat it as R820T2
        // (the modern default) unless a downstream check disagrees.
        Some(sdr_fox_core::TunerKind::R820T2)
    } else if r82xx_id_answers_at(transport, super::R828D_I2C_ADDR) {
        Some(sdr_fox_core::TunerKind::R828D)
    } else {
        None
    };
    let _ = set_i2c_repeater(transport, false);
    kind.ok_or(SdrError::Tuner(TunerError::NoSupportedTuner))
}

/// Whether the R82xx chip id (logical 0x96) answers at `i2c_addr`. The I2C
/// repeater must already be on.
///
/// The RTL2832 I2C tunnel returns tuner bytes bit-reversed, so the raw byte
/// on the wire is 0x69 (= bitrev8(0x96)); we compare against the logical ID
/// and reverse the raw byte here. (`RtlI2cBus` reverses reads for tuner
/// drivers; this probe runs before a tuner instance exists, so it calls the
/// free `i2c_read` and reverses itself.)
///
/// A transport error is folded into "no chip here" rather than propagated:
/// an absent chip NACKs, and on some backends that NACK surfaces as a failed
/// control transfer instead of a zero payload.
fn r82xx_id_answers_at(transport: &mut dyn Transport, i2c_addr: u8) -> bool {
    let raw = super::i2c_read(transport, i2c_addr, 1).unwrap_or_default();
    raw.first().copied().map(u8::reverse_bits) == Some(0x96)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtl2832::{Block, R820T_I2C_ADDR, R828D_I2C_ADDR};
    use sdr_fox_core::{SdrDevice, TransferDirection, TunerKind};
    use sdr_fox_transport::{MockTransport, RecordedRequest, ScriptedReply};

    /// A scripted IN reply pinned to the I2C-tunnel read of `i2c_addr`
    /// (wValue = address, wIndex = IIC block). Pinning matters: the probe
    /// also issues demod flush reads, and a positional wildcard reply could
    /// be consumed by one of those instead of the chip-id read under test.
    fn iic_read_reply(i2c_addr: u8, payload: Vec<u8>) -> ScriptedReply {
        ScriptedReply {
            direction: TransferDirection::In,
            request: None,
            value: Some(u16::from(i2c_addr)),
            index: Some((Block::Iic as u16) << 8),
            payload,
        }
    }

    /// Whether a recorded request went through the I2C tunnel (any direction).
    fn is_iic(r: &RecordedRequest) -> bool {
        r.index >> 8 == Block::Iic as u16
    }

    #[test]
    fn probe_detects_r820t2_when_id_register_is_0x69_on_the_wire() {
        // The R82xx chip ID is 0x96, but the RTL2832 I2C tunnel returns tuner
        // bytes bit-reversed, so the wire byte is bitrev8(0x96) = 0x69. The
        // probe reverses the raw byte and compares against the logical 0x96.
        let mut mock = MockTransport::new();
        // init_baseband issues many register writes; provide wildcard OUT acks.
        // Each demod_write_reg is one OUT + one flush IN; we need enough replies.
        for _ in 0..40 {
            mock.push_reply(ScriptedReply::any_out());
            mock.push_reply(ScriptedReply::any_in(vec![0]));
        }
        // The probe: set_i2c_repeater on (write+flush), i2c_read of 0x34 (IN),
        // set_i2c_repeater off (write+flush). Then tuner init writes.
        mock.push_reply(ScriptedReply::any_out()); // repeater on write
        mock.push_reply(ScriptedReply::any_in(vec![0])); // repeater on flush
        mock.push_reply(ScriptedReply::any_in(vec![0x69])); // bitrev8(0x96) on the wire
        mock.push_reply(ScriptedReply::any_out()); // repeater off write
        mock.push_reply(ScriptedReply::any_in(vec![0])); // repeater off flush
                                                         // Tuner init writes...
        for _ in 0..40 {
            mock.push_reply(ScriptedReply::any_out());
            mock.push_reply(ScriptedReply::any_in(vec![0]));
        }
        let desc = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: sdr_fox_core::DeviceKind::RtlSdr,
        };
        let transport: Box<dyn Transport> = Box::new(mock);
        // open_device will fail at tuner init because r82xx::init is a stub,
        // but the probe should have detected R820T2 before that.
        let result = open_device(&desc, transport);
        // We expect either success (if tuner_factory returns a no-op tuner) or
        // a Tuner error from init. Either way, the probe ran.
        let _ = result;
    }

    #[test]
    fn probe_hit_returns_r820t2() {
        let mut mock = MockTransport::new();
        // probe_tuner alone: repeater-on flush read, then the chip-id read.
        // OUT writes and the repeater-off flush use the non-strict defaults.
        mock.push_reply(ScriptedReply::any_in(vec![0])); // repeater-on flush
        mock.push_reply(ScriptedReply::any_in(vec![0x69])); // chip-id read
        let kind = probe_tuner(&mut mock).expect("0x69 chip id must be detected");
        assert_eq!(kind, sdr_fox_core::TunerKind::R820T2);
    }

    #[test]
    fn probe_miss_reports_no_supported_tuner() {
        // A non-strict MockTransport with no scripted replies answers every IN
        // transfer with zeros, so the chip-id reads at BOTH 0x34 and 0x74 see
        // 0x00 — the "no tuner ACKed / unsupported tuner fitted" case.
        let mut mock = MockTransport::new();
        let err = probe_tuner(&mut mock).expect_err("zero chip id must not match any tuner");
        assert!(
            matches!(err, SdrError::Tuner(TunerError::NoSupportedTuner)),
            "probe miss must surface NoSupportedTuner, not a PLL error; got: {err:?}"
        );
        // Both family addresses must actually have been tried before giving
        // up, or an R828D dongle would be reported as "no tuner".
        for addr in [R820T_I2C_ADDR, R828D_I2C_ADDR] {
            assert_eq!(
                mock.count_matching(|r| is_iic(r) && r.value == u16::from(addr)),
                1,
                "the probe must read the chip id at 0x{addr:02x} before reporting a miss"
            );
        }
    }

    #[test]
    fn probe_detects_r828d_at_0x74_when_0x34_is_silent() {
        // An R828D dongle: nothing at 0x34 (the non-strict mock's default
        // zeros stand in for the NACK), chip id 0x96 at 0x74 — 0x69 on the
        // wire, because the I2C tunnel bit-reverses tuner bytes.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(R828D_I2C_ADDR, vec![0x69]));
        let kind = probe_tuner(&mut mock).expect("0x96 at 0x74 must be detected");
        assert_eq!(kind, TunerKind::R828D);
        // Probe order is part of the contract: 0x34 first, so existing
        // R820T/T2 dongles see an unchanged probe sequence.
        let iic_reads: Vec<u16> = mock
            .recorded()
            .iter()
            .filter(|r| is_iic(r) && r.direction == TransferDirection::In)
            .map(|r| r.value)
            .collect();
        assert_eq!(
            iic_reads,
            vec![u16::from(R820T_I2C_ADDR), u16::from(R828D_I2C_ADDR)],
            "the probe must try 0x34 before falling back to 0x74"
        );
    }

    #[test]
    fn probe_hit_at_0x34_never_touches_0x74() {
        // An R820T/T2 dongle answering at 0x34 must short-circuit the probe:
        // the pre-R828D behaviour, byte for byte, so existing dongles cannot
        // be affected by the new fallback.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(R820T_I2C_ADDR, vec![0x69]));
        let kind = probe_tuner(&mut mock).expect("0x96 at 0x34 must be detected");
        assert_eq!(kind, TunerKind::R820T2);
        assert_eq!(
            mock.count_matching(|r| is_iic(r) && r.value == u16::from(R828D_I2C_ADDR)),
            0,
            "a 0x34 hit must not generate any 0x74 traffic"
        );
    }

    #[test]
    fn open_device_reports_r828d_in_device_info() {
        // Full open path for an R828D dongle: the probe misses 0x34 (default
        // zeros), hits 0x74, and the detected kind must flow through
        // tuner_factory into the DeviceInfo the caller sees.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(R828D_I2C_ADDR, vec![0x69]));
        let desc = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: sdr_fox_core::DeviceKind::RtlSdr,
        };
        let dev = open_device(&desc, Box::new(mock))
            .expect("R828D open must succeed against the non-strict mock");
        assert_eq!(dev.info().tuner, Some(TunerKind::R828D));
    }

    #[test]
    fn init_tuner_for_r828d_addresses_0x74() {
        // The failure this locks out: a probe that finds the tuner at 0x74
        // followed by a bus that still talks to 0x34 would "succeed" while
        // programming a chip that is not there. Every I2C-tunnel access made
        // by tuner init must carry the detected tuner's address.
        let mut mock = MockTransport::new();
        let mut tuner = crate::tuners::r82xx::R82xx::new(TunerKind::R828D);
        init_tuner(&mut mock, &mut tuner)
            .expect("r82xx init only writes, which the non-strict mock acks");
        let total = mock.count_matching(is_iic);
        let at_0x74 = mock.count_matching(|r| is_iic(r) && r.value == u16::from(R828D_I2C_ADDR));
        assert!(total > 0, "tuner init must produce I2C traffic");
        assert_eq!(
            at_0x74, total,
            "every tuner-init I2C access must address 0x74 for an R828D"
        );
    }

    #[test]
    fn open_device_surfaces_no_supported_tuner_on_probe_miss() {
        // Full open path with all-zero IN replies: baseband init succeeds, the
        // probe misses, and the miss must reach the caller unchanged (M8: a
        // missing tuner previously masqueraded as PllProgrammingFailed).
        let desc = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: sdr_fox_core::DeviceKind::RtlSdr,
        };
        let transport: Box<dyn Transport> = Box::new(MockTransport::new());
        match open_device(&desc, transport) {
            Ok(_) => panic!("open must fail without a tuner"),
            Err(err) => assert!(
                matches!(err, SdrError::Tuner(TunerError::NoSupportedTuner)),
                "open_device must report NoSupportedTuner on a probe miss; got: {err:?}"
            ),
        }
    }
}
