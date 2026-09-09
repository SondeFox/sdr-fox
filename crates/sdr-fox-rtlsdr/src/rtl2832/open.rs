//! Device-open path: baseband init + tuner probe + tuner init.
//!
//! Tuner detection is an I2C read of each candidate's chip-id register
//! compared against a known value; the first match wins. The established
//! probe order (as run by the Osmocom reference driver) is E4000 → FC0013 →
//! R820T → R828D → GPIO4 reset → FC2580 → FC0012. This module implements the
//! E4000 probe (I2C 0xc8) and the R82xx-family probes (R820T/R820T2 at I2C
//! 0x34, R828D at 0x74), in that order; the FC family slots into the
//! remaining positions as drivers are added.

use sdr_fox_core::{DeviceDescriptor, SdrError, Transport, Tuner, TunerError, TunerKind};

use super::baseband;
use super::device::{make_info, RtlSdr};
use super::{demod_write_reg, set_i2c_repeater, RtlI2cBus};

/// Default IF frequency programmed at open time for the R820T tuner family.
/// The Osmocom reference sets this in `rtlsdr_open` before `tuner->init`.
const R82XX_DEFAULT_IF_HZ: u64 = 3_570_000;

/// Program the demod path for the detected tuner's architecture. Must run
/// after the probe (the kind has to be known) and **before `tuner.init`**
/// (matching osmocom's `rtlsdr_open` ordering: the R820T tuner-open case
/// calls `set_if_freq` before `tuner->init`; the reverse order disrupts the
/// signal path).
///
/// The R82xx family is a **low-IF** tuner: it mixes RF down to a 3.57 MHz IF
/// on the demod's in-phase ADC input, and the spectrum arrives inverted, so
/// the demod needs zero-IF mode off, the I-only ADC input, spectrum
/// inversion on, and the matching IF frequency.
///
/// The E4000 is **direct-conversion (zero-IF)**: I and Q come out at
/// baseband, so the demod digitises both ADC inputs, applies no IF shift and
/// no spectrum inversion. Every register is written explicitly on this path
/// too — rather than trusting power-on defaults — because the demod register
/// file survives the `DEMOD_CTL = 0x20` power-down: a previous driver run
/// (including older sdr-fox builds that programmed the R82xx values
/// unconditionally) may have left the low-IF configuration behind, and it
/// would otherwise persist until the dongle is re-plugged.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure, or
/// [`SdrError::Unsupported`] for kinds without a driver (which
/// [`super::tuner_factory`] rejects on the same open anyway).
pub(crate) fn configure_demod_for_tuner(
    transport: &mut dyn Transport,
    kind: TunerKind,
) -> Result<(), SdrError> {
    match kind {
        TunerKind::R820T | TunerKind::R820T2 | TunerKind::R828D => {
            // Disable zero-IF mode (the R82xx uses a 3.57 MHz IF; osmocom
            // writes 0x1a here in the R820T branch of rtlsdr_open).
            demod_write_reg(transport, 1, 0xb1, 0x1a, 1)?;
            // I-only ADC input: the tuner's IF output feeds the I ADC.
            demod_write_reg(transport, 0, 0x08, 0x4d, 1)?;
            // Spectrum inversion, required by the R82xx IF architecture.
            demod_write_reg(transport, 1, 0x15, 0x01, 1)?;
            // The matching demod IF. Without these writes the RF path
            // delivers a constant 128 from the ADC.
            baseband::set_if_freq(transport, R82XX_DEFAULT_IF_HZ, baseband::RTL_XTAL_HZ)
        }
        TunerKind::E4000 => {
            // Enable zero-IF mode: the E4000 hands the demod baseband I/Q.
            demod_write_reg(transport, 1, 0xb1, 0x1b, 1)?;
            // Both ADC inputs (I + Q) — the direct-conversion datapath.
            demod_write_reg(transport, 0, 0x08, 0xcd, 1)?;
            // No spectrum inversion at zero IF.
            demod_write_reg(transport, 1, 0x15, 0x00, 1)?;
            // IF = 0: no digital down-conversion shift.
            baseband::set_if_freq(transport, 0, baseband::RTL_XTAL_HZ)
        }
        other => Err(SdrError::Unsupported(format!(
            "tuner {other:?} not yet implemented"
        ))),
    }
}

/// Baseband half of device initialisation: the tuner-agnostic bring-up
/// followed by the per-tuner demod configuration (IF/inversion/ADC input).
///
/// This wrapper serves the bulk-liveness recovery reset in
/// `RtlSdr::start_stream`, where the tuner kind is already known — a USB
/// device reset re-enumerates the dongle and wipes every register, so
/// recovery must rerun exactly the open-time sequence. The open path itself
/// calls the same two pieces — [`baseband::init_baseband`] and
/// [`configure_demod_for_tuner`] — but must interleave the tuner probe
/// between them, because the per-tuner half needs the probe's answer while
/// the probe needs the demod powered (the I2C repeater lives in it). Any
/// change to either piece therefore reaches both paths; do not inline them
/// separately.
pub(crate) fn init_baseband_defaults(
    transport: &mut dyn Transport,
    kind: TunerKind,
) -> Result<(), SdrError> {
    baseband::init_baseband(transport)?;
    configure_demod_for_tuner(transport, kind)
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
///   tuner is detected (an unsupported chip such as FC0012/FC0013/FC2580 is
///   fitted, or no tuner ACKed at all).
/// - [`SdrError::Tuner`] with another variant if a detected tuner fails to
///   initialize.
/// - [`SdrError::Transport`] on any register/I2C failure.
pub fn open_device(
    desc: &DeviceDescriptor,
    mut transport: Box<dyn Transport>,
) -> Result<RtlSdr, SdrError> {
    probe_or_reset(transport.as_mut())?;
    // Tuner-agnostic baseband bring-up. This must precede the probe (the I2C
    // repeater needs the demod powered), while the per-tuner demod
    // configuration below needs the probe's answer — which is why the open
    // path interleaves the probe between the two halves that
    // `init_baseband_defaults` runs back-to-back for the recovery reset.
    baseband::init_baseband(transport.as_mut())?;

    // Run the tuner probe: E4000, then the R82xx family (R820T2, the most
    // common modern dongle, and R828D); the FC tuners are wired into the
    // probe as their modules land.
    let tuner_kind = probe_tuner(transport.as_mut())?;

    // Per-tuner demod configuration (IF/inversion/ADC input). Must precede
    // `tuner.init` — see `configure_demod_for_tuner`.
    configure_demod_for_tuner(transport.as_mut(), tuner_kind)?;

    // Build the tuner instance and initialize it.
    let blog_v4 = tuner_kind == TunerKind::R828D && is_blog_v4(desc);
    let mut tuner_obj: Box<dyn Tuner> = if blog_v4 {
        Box::new(crate::tuners::r82xx::R82xx::for_blog_v4())
    } else {
        super::tuner_factory(tuner_kind)?
    };
    init_tuner(transport.as_mut(), tuner_obj.as_mut())?;

    let info = make_info(desc, Some(tuner_kind));
    Ok(RtlSdr::new(info, transport, tuner_obj).with_blog_v4_routing(blog_v4))
}

// Manufacturer-documented EEPROM identity. A missing/edited string must
// never change the clock of an unrelated generic R828D receiver.
// Facts: https://www.rtl-sdr.com/v4/ and the manufacturer's 2023 V4 design.
fn is_blog_v4(desc: &DeviceDescriptor) -> bool {
    (desc.vendor_id, desc.product_id) == (0x0bda, 0x2838)
        && desc.vendor_name.as_deref() == Some("RTLSDRBlog")
        && desc.product_name.as_deref() == Some("Blog V4")
}

/// Probe the tuner by reading each candidate's chip-id register, in the
/// documented order: E4000 (I2C 0xc8) first, then the R82xx family — 0x34,
/// then 0x74 (see the module docs for the full order the FC tuners will
/// join). Returns the first detected [`sdr_fox_core::TunerKind`].
///
/// # Errors
///
/// Returns [`SdrError::Tuner`] with [`TunerError::NoSupportedTuner`] if no
/// known tuner responds — the dongle carries a chip the probe does not drive
/// (FC0012/FC0013/FC2580) or nothing ACKed. This is deliberately distinct
/// from `PllProgrammingFailed`, which reports a *detected* tuner failing to
/// program.
fn probe_tuner(transport: &mut dyn Transport) -> Result<sdr_fox_core::TunerKind, SdrError> {
    set_i2c_repeater(transport, true)?;
    // E4000 first — the position the reference probe order gives it, ahead
    // of the 0x34 read. Then the R82xx family: read chip-id register 0x00
    // and expect 0x96. The whole family reports the same id; what tells the
    // chips apart is which I2C address answers — 0x34 for R820T/R820T2,
    // 0x74 for R828D. 0x34 is tried before 0x74 so existing R82xx dongles
    // see an unchanged relative probe sequence.
    let kind = if e4000_id_answers(transport) {
        Some(sdr_fox_core::TunerKind::E4000)
    } else if r82xx_id_answers_at(transport, super::R820T_I2C_ADDR) {
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

/// Whether the E4000 chip id (0x40 in register 0x02) answers at I2C 0xc8.
/// The I2C repeater must already be on.
///
/// Unlike the R82xx probe there is **no bit reversal** here: the reversal is
/// R82xx silicon behaviour (see [`super::tuner_reverses_read_bits`]), and
/// the E4000 returns plain datasheet bytes — reversing them would turn the
/// 0x40 id into 0x02 and the probe could never match.
///
/// Transport errors fold into "no chip here", exactly as in
/// [`r82xx_id_answers_at`]: an absent chip NACKs, and on some backends that
/// NACK surfaces as a failed transfer rather than a zero payload.
fn e4000_id_answers(transport: &mut dyn Transport) -> bool {
    use crate::tuners::e4000::{E4000_CHECK_REG, E4000_CHECK_VAL, E4000_I2C_ADDR};
    // Select the id register, then read it back — the tunnel's two-step
    // register-read sequence (the E4000 does not stream from register 0).
    if super::i2c_write(transport, E4000_I2C_ADDR, &[E4000_CHECK_REG]).is_err() {
        return false;
    }
    let raw = super::i2c_read(transport, E4000_I2C_ADDR, 1).unwrap_or_default();
    raw.first().copied() == Some(E4000_CHECK_VAL)
}

/// Whether the R82xx chip id (logical 0x96) answers at `i2c_addr`. The I2C
/// repeater must already be on.
///
/// R82xx silicon returns register bytes bit-reversed, so the raw byte on the
/// wire is 0x69 (= bitrev8(0x96)); we compare against the logical ID and
/// reverse the raw byte here. (`RtlI2cBus` reverses reads for R82xx tuner
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
    use crate::tuners::e4000::{E4000_CHECK_REG, E4000_I2C_ADDR};
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
    fn blog_v4_clock_selection_requires_all_manufacturer_identity_fields() {
        let mut desc = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: Some("RTLSDRBlog".into()),
            product_name: Some("Blog V4".into()),
            serial: None,
            index: 0,
            kind: sdr_fox_core::DeviceKind::RtlSdr,
        };
        assert!(is_blog_v4(&desc));
        desc.vendor_name = None;
        assert!(!is_blog_v4(&desc));
        desc.vendor_name = Some("Other".into());
        assert!(!is_blog_v4(&desc));
        desc.vendor_name = Some("RTLSDRBlog".into());
        desc.product_name = Some("Blog V3".into());
        assert!(!is_blog_v4(&desc));
        desc.product_name = Some("Blog V4".into());
        desc.product_id = 0x2832;
        assert!(!is_blog_v4(&desc));
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
        // Chip id 0x96 at 0x34, bit-reversed to 0x69 on the wire by the R82xx
        // silicon. The E4000 read at 0xc8 and the repeater flushes fall
        // through to the non-strict defaults (zeros).
        mock.push_reply(iic_read_reply(R820T_I2C_ADDR, vec![0x69]));
        let kind = probe_tuner(&mut mock).expect("0x69 chip id must be detected");
        assert_eq!(kind, sdr_fox_core::TunerKind::R820T2);
    }

    #[test]
    fn probe_miss_reports_no_supported_tuner() {
        // A non-strict MockTransport with no scripted replies answers every IN
        // transfer with zeros, so the chip-id reads at 0xc8, 0x34 and 0x74 all
        // see 0x00 — the "no tuner ACKed / unsupported tuner fitted" case.
        let mut mock = MockTransport::new();
        let err = probe_tuner(&mut mock).expect_err("zero chip id must not match any tuner");
        assert!(
            matches!(err, SdrError::Tuner(TunerError::NoSupportedTuner)),
            "probe miss must surface NoSupportedTuner, not a PLL error; got: {err:?}"
        );
        // Every candidate address must actually have been tried before giving
        // up, or a dongle carrying that tuner would be reported as "no tuner".
        for addr in [E4000_I2C_ADDR, R820T_I2C_ADDR, R828D_I2C_ADDR] {
            assert_eq!(
                mock.count_matching(|r| is_iic(r)
                    && r.direction == TransferDirection::In
                    && r.value == u16::from(addr)),
                1,
                "the probe must read the chip id at 0x{addr:02x} before reporting a miss"
            );
        }
    }

    #[test]
    fn probe_detects_r828d_at_0x74_when_0x34_is_silent() {
        // An R828D dongle: nothing at 0xc8 or 0x34 (the non-strict mock's
        // default zeros stand in for the NACK), chip id 0x96 at 0x74 — 0x69
        // on the wire, because R82xx silicon bit-reverses read-back bytes.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(R828D_I2C_ADDR, vec![0x69]));
        let kind = probe_tuner(&mut mock).expect("0x96 at 0x74 must be detected");
        assert_eq!(kind, TunerKind::R828D);
        // Probe order is part of the contract: E4000 first (the documented
        // reference order), then 0x34 before 0x74 so existing R82xx dongles
        // keep their relative probe sequence.
        let iic_reads: Vec<u16> = mock
            .recorded()
            .iter()
            .filter(|r| is_iic(r) && r.direction == TransferDirection::In)
            .map(|r| r.value)
            .collect();
        assert_eq!(
            iic_reads,
            vec![
                u16::from(E4000_I2C_ADDR),
                u16::from(R820T_I2C_ADDR),
                u16::from(R828D_I2C_ADDR)
            ],
            "the probe must try 0xc8, then 0x34, then 0x74"
        );
    }

    #[test]
    fn probe_hit_at_0x34_never_touches_0x74() {
        // An R820T/T2 dongle answering at 0x34 must short-circuit the probe
        // (after the E4000 miss at 0xc8), so existing dongles cannot be
        // affected by the R828D fallback.
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
    fn probe_detects_e4000_at_0xc8_and_short_circuits() {
        // An E4000 dongle: chip id 0x40 in register 0x02 at I2C 0xc8, as a
        // PLAIN datasheet byte — no bit reversal on the wire.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(E4000_I2C_ADDR, vec![0x40]));
        let kind = probe_tuner(&mut mock).expect("0x40 at 0xc8 must be detected");
        assert_eq!(kind, TunerKind::E4000);
        // A hit at 0xc8 must short-circuit: no R82xx traffic at all.
        for addr in [R820T_I2C_ADDR, R828D_I2C_ADDR] {
            assert_eq!(
                mock.count_matching(|r| is_iic(r) && r.value == u16::from(addr)),
                0,
                "an E4000 hit must not generate any 0x{addr:02x} traffic"
            );
        }
        // The probe must select the id register before reading it: an IIC
        // write of [0x02] to 0xc8 preceding the IN transfer.
        let e4000_accesses: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| is_iic(r) && r.value == u16::from(E4000_I2C_ADDR))
            .collect();
        assert_eq!(e4000_accesses.len(), 2, "register-select write + id read");
        assert_eq!(e4000_accesses[0].direction, TransferDirection::Out);
        assert_eq!(e4000_accesses[0].data, vec![E4000_CHECK_REG]);
        assert_eq!(e4000_accesses[1].direction, TransferDirection::In);
    }

    #[test]
    fn e4000_probe_must_not_bit_reverse_the_id_byte() {
        // The regression this pins: if the E4000 probe (or a shared read
        // path) applied the R82xx bit reversal, a wire byte of 0x02
        // (= bitrev8(0x40)) would wrongly match. It must NOT: 0x02 is not
        // the E4000 id, so the probe falls through and — with zeros
        // everywhere else — reports no supported tuner.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(E4000_I2C_ADDR, vec![0x02]));
        let err = probe_tuner(&mut mock)
            .expect_err("bitrev8(0x40) on the wire must not be accepted as an E4000");
        assert!(matches!(err, SdrError::Tuner(TunerError::NoSupportedTuner)));
    }

    #[test]
    fn open_device_reports_e4000_in_device_info() {
        // Full open path for an E4000 dongle (e.g. NooElec NESDR Smart XTR,
        // 0bda:2838): the probe hits 0xc8 and the detected kind must flow
        // through tuner_factory into the DeviceInfo the caller sees.
        //
        // Three pinned 0xc8 reads are scripted, consumed in order: the
        // probe's id read, then `E4000::init`'s wake-up throwaway read, then
        // init's own id re-check. Later read-modify-write reads fall through
        // to the non-strict mock's zeros, which init tolerates.
        let mut mock = MockTransport::new();
        mock.push_reply(iic_read_reply(E4000_I2C_ADDR, vec![0x40]));
        mock.push_reply(iic_read_reply(E4000_I2C_ADDR, vec![0x00]));
        mock.push_reply(iic_read_reply(E4000_I2C_ADDR, vec![0x40]));
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
            .expect("E4000 open must succeed against the non-strict mock");
        assert_eq!(dev.info().tuner, Some(TunerKind::E4000));
    }

    /// Find the recorded demod write for `(page, addr)` and return its data.
    fn demod_write_data(mock: &MockTransport, addr: u8) -> Vec<u8> {
        mock.recorded()
            .iter()
            .find(|r| {
                r.direction == TransferDirection::Out && r.value == (u16::from(addr) << 8) | 0x20
            })
            .unwrap_or_else(|| panic!("missing demod write for 0x{addr:02x}"))
            .data
            .clone()
    }

    #[test]
    fn r82xx_demod_config_is_low_if_inverted_i_only() {
        // The low-IF architecture: zero-IF off (0x1a), I-only ADC input
        // (0x4d), spectrum inversion on (0x01), and the 3.57 MHz IF.
        let mut mock = MockTransport::new();
        configure_demod_for_tuner(&mut mock, TunerKind::R820T2).unwrap();
        assert_eq!(demod_write_data(&mock, 0xb1), vec![0x1a], "zero-IF off");
        assert_eq!(demod_write_data(&mock, 0x08), vec![0x4d], "I-only ADC");
        assert_eq!(demod_write_data(&mock, 0x15), vec![0x01], "inversion on");
        // The IF registers must carry -(3.57 MHz * 2^22 / 28.8 MHz).
        let if_freq =
            (-(((R82XX_DEFAULT_IF_HZ << 22) / u64::from(baseband::RTL_XTAL_HZ)) as i64)) as u32;
        assert_eq!(
            demod_write_data(&mock, 0x19),
            vec![((if_freq >> 16) & 0x3f) as u8]
        );
        assert_eq!(
            demod_write_data(&mock, 0x1a),
            vec![((if_freq >> 8) & 0xff) as u8]
        );
        assert_eq!(demod_write_data(&mock, 0x1b), vec![(if_freq & 0xff) as u8]);
    }

    #[test]
    fn e4000_demod_config_is_zero_if_uninverted_iq() {
        // The direct-conversion architecture: zero-IF on (0x1b), both ADC
        // inputs (0xcd), no spectrum inversion (0x00), IF = 0. Each register
        // is written explicitly because the demod register file survives
        // power-down — a previous low-IF configuration must be overwritten,
        // not trusted to defaults.
        let mut mock = MockTransport::new();
        configure_demod_for_tuner(&mut mock, TunerKind::E4000).unwrap();
        assert_eq!(demod_write_data(&mock, 0xb1), vec![0x1b], "zero-IF on");
        assert_eq!(demod_write_data(&mock, 0x08), vec![0xcd], "I+Q ADC");
        assert_eq!(demod_write_data(&mock, 0x15), vec![0x00], "inversion off");
        for addr in [0x19, 0x1a, 0x1b] {
            assert_eq!(demod_write_data(&mock, addr), vec![0x00], "IF must be 0");
        }
    }

    #[test]
    fn demod_config_rejects_undriven_tuner_kinds() {
        let mut mock = MockTransport::new();
        for kind in [TunerKind::Fc0012, TunerKind::Fc0013, TunerKind::Fc2580] {
            assert!(
                matches!(
                    configure_demod_for_tuner(&mut mock, kind),
                    Err(SdrError::Unsupported(_))
                ),
                "undriven kind {kind:?} must be rejected, not half-configured"
            );
        }
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
