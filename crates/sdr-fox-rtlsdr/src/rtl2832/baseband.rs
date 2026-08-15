//! RTL2832U baseband initialization and high-level register helpers.
//!
//! These are the demod register writes that constitute the "power-on" and
//! "configure" sequences. The register addresses and values are hardware
//! facts — any working driver must emit the same bytes — re-derived from the
//! RTL2832U datasheet and the osmocom reference.

use sdr_fox_core::{SdrError, Transport};

use super::{demod_write_reg, read_reg, write_reg, Block};

/// RTL-SDR default crystal frequencies (Hz).
pub const RTL_XTAL_HZ: u32 = 28_800_000;

/// Power-on and reset the demodulator — the chip bring-up sequence that
/// `rtlsdr_init_baseband` performs in the Osmocom reference driver.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on any register-access failure.
pub fn init_baseband(transport: &mut dyn Transport) -> Result<(), SdrError> {
    // --- Initialize USB (USBB block) — must come first, before any demod write. ---
    // The chip's USB bring-up writes: USB_SYSCTL, USB_EPA_MAXPKT, USB_EPA_CTL.
    // Register names and values per the Osmocom reference (`rtlsdr_init_baseband`).
    write_reg(transport, Block::Usb, 0x2000, 0x09, 1)?; // USB_SYSCTL
    write_reg(transport, Block::Usb, 0x2158, 0x0002, 2)?; // USB_EPA_MAXPKT
    write_reg(transport, Block::Usb, 0x2148, 0x1002, 2)?; // USB_EPA_CTL

    // --- Power on the demod (SYSB block). ---
    write_reg(transport, Block::Sys, 0x300b, 0x22, 1)?; // DEMOD_CTL_1
    write_reg(transport, Block::Sys, 0x3000, 0xe8, 1)?; // DEMOD_CTL

    // Reset the demod (soft) — demod writes are now safe (chip is powered).
    demod_write_reg(transport, 1, 0x01, 0x14, 1)?;
    demod_write_reg(transport, 1, 0x01, 0x10, 1)?;
    // Disable spectrum inversion and ACR (automatic sample-rate correction).
    // NOTE: osmocom writes 0x00 here in init_baseband, then the R820T2
    // tuner-open path (rtlsdr_open) overrides with 0x01 (enable inversion).
    // We do the R820T2-specific override at the end of init_baseband since
    // this is the only tuner we currently support.
    demod_write_reg(transport, 1, 0x15, 0x00, 1)?;
    demod_write_reg(transport, 1, 0x16, 0x0000, 2)?;
    // Clear DDC shift AND IF frequency registers (osmocom: 0x16 through 0x1b).
    for addr in 0x16..=0x1b {
        demod_write_reg(transport, 1, addr, 0x00, 1)?;
    }
    // Enable SDR mode, disable DAGC.
    demod_write_reg(transport, 0, 0x19, 0x05, 1)?;
    // Set the FIR coefficients (canonical default — disables the DVB-T FIR).
    set_fir(transport)?;

    // Init FSM state-holding registers.
    demod_write_reg(transport, 1, 0x93, 0xf0, 1)?;
    demod_write_reg(transport, 1, 0x94, 0x0f, 1)?;

    // Disable AGC (en_dagc, bit 0).
    demod_write_reg(transport, 1, 0x11, 0x00, 1)?;
    // Disable RF and IF AGC loop.
    demod_write_reg(transport, 1, 0x04, 0x00, 1)?;

    // Disable PID filter (enable_PID = 0).
    demod_write_reg(transport, 0, 0x61, 0x60, 1)?;

    // `opt_adc_iq = 0`, default ADC_I/ADC_Q datapath.
    //
    // Keep this distinct from `(0, 0x08) = 0x4d`: the two registers configure
    // different parts of initialization and both writes are required.
    //
    // The cost of losing it is severe and its signature is deceptive. Bit 7
    // gates the ADC datapath feeding the USB EP-A FIFO, and it is set
    // unconditionally in every mode upstream programs — normal and both
    // direct-sampling inputs — and never cleared. With it clear on a cold
    // chip, every control transfer still succeeds (open, tune, gain, rate all
    // report OK) while the bulk endpoint delivers nothing at all: on a
    // Motorola Edge Plus, 0 of 64 URBs on ep 0x81 ever completed. That is
    // indistinguishable from a wedged dongle, so it draws the wedge recovery
    // in `RtlSdr::start_stream`, which cannot help — both tiers re-run this
    // same function.
    //
    // It is also self-concealing: the demod register file survives the
    // `DEMOD_CTL = 0x20` power-down, so once ANY driver that writes this
    // register has run once, sdr-fox works until the device is
    // re-enumerated. Testing after any such run hides the bug completely.
    demod_write_reg(transport, 0, 0x06, 0x80, 1)?;

    // === R820T2-specific demod configuration (from osmocom rtlsdr_open) ===
    // These writes happen AFTER the generic init, specific to R820T/R828D tuners.
    // They are CRITICAL for signal reception — without them, the ADC delivers
    // constant 128 (no RF path).

    // Disable Zero-IF mode (R820T2 uses a 3.57 MHz IF, NOT zero-IF).
    // Osmocom writes 0x1a here, NOT 0x1b.
    demod_write_reg(transport, 1, 0xb1, 0x1a, 1)?;

    // Enable I-only ADC input (direct sampling mode for the tuner's IF output).
    // Osmocom writes this in the R820T branch of `rtlsdr_open`
    // (librtlsdr.c:1709). It is the companion of `(0, 0x06) = 0x80` above, not
    // a replacement for it — see the note there.
    demod_write_reg(transport, 0, 0x08, 0x4d, 1)?;

    // Enable spectrum inversion (required for the R820T2's IF architecture).
    demod_write_reg(transport, 1, 0x15, 0x01, 1)?;

    // Disable 4.096 MHz clock output on pin TP_CK0. (Not actually tuner-
    // specific: osmocom issues this write at the end of init_baseband.)
    demod_write_reg(transport, 0, 0x0d, 0x83, 1)?;
    Ok(())
}

/// Reset the USB endpoint-A FIFO buffer. **Must be called before reading
/// samples** — without it the bulk endpoint stays in the "flush" state and
/// delivers no data. The `USB_EPA_CTL` flush/run toggle is the firmware's
/// buffer-reset convention (the same two writes the Osmocom reference issues
/// in `rtlsdr_reset_buffer`).
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn reset_buffer(transport: &mut dyn Transport) -> Result<(), SdrError> {
    write_reg(transport, Block::Usb, 0x2148, 0x1002, 2)?; // USB_EPA_CTL = 0x1002 (flush)
    write_reg(transport, Block::Usb, 0x2148, 0x0000, 2)?; // USB_EPA_CTL = 0x0000 (run)
    Ok(())
}

/// Power down the demodulator and ADCs, the counterpart to
/// [`init_baseband`].
///
/// This is not merely a battery courtesy. An RTL2832U left with its demod
/// running can come back wedged on the next open: control transfers keep
/// working — register reads, tuning, gain all succeed — while the bulk
/// endpoint delivers nothing at all, and re-running `init_baseband` over the
/// still-running chip does not clear it. Powering the demod off returns the
/// part to the state a fresh plug-in would leave it in, which is why a device
/// wedged by an abrupt exit recovers after any tool that closes cleanly.
///
/// Writes `DEMOD_CTL = 0x20`, the power-off counterpart of the `0xe8` written
/// during bring-up.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] if the control transfer fails. Callers
/// tearing a device down should generally ignore the result: a device that has
/// already been unplugged cannot be powered down, and that is not an error
/// worth propagating out of a destructor.
pub fn deinit_baseband(transport: &mut dyn Transport) -> Result<(), SdrError> {
    write_reg(transport, Block::Sys, 0x3000, 0x20, 1)
}

/// Load the default FIR coefficients (osmocom `fir_default`, NOT zeros).
/// These are the DVB-T decimation filter coefficients. Without them, the
/// demod produces no output (all-128 ADC). The values are byte-exact
/// verified against the osmocom I2C trace.
fn set_fir(transport: &mut dyn Transport) -> Result<(), SdrError> {
    // osmocom fir_default: [-54,-36,-41,-40,-32,-14,14,53, 101,156,215,273,327,372,404,421]
    // Encoded: 8×int8_t + 8×int12_t packed into 20 bytes.
    const FIR_BYTES: [u8; 20] = [
        0xca, 0xdc, 0xd7, 0xd8, 0xe0, 0xf2, 0x0e, 0x35, 0x06, 0x50, 0x9c, 0x0d, 0x71, 0x11, 0x14,
        0x71, 0x74, 0x19, 0x41, 0xa5,
    ];
    for (i, &byte) in FIR_BYTES.iter().enumerate() {
        demod_write_reg(transport, 1, 0x1c + i as u8, u16::from(byte), 1)?;
    }
    Ok(())
}

/// Set the ADC sample rate via the `rsamp_ratio` register pair.
///
/// `rsamp_ratio = (rtl_xtal << 22) / sample_rate`, masked to `0x0ffffffc`
/// with sign extension. Valid rates: 225 001–300 000 or 900 001–3 200 000 Hz.
///
/// # Errors
///
/// Returns [`SdrError::InvalidSampleRate`] if the rate is outside the valid
/// ranges, or [`SdrError::Transport`] on a register-access failure.
pub fn set_sample_rate(
    transport: &mut dyn Transport,
    sample_rate_hz: u32,
    rtl_xtal_hz: u32,
) -> Result<u32, SdrError> {
    let rate = sample_rate_hz;
    let valid = (225_001..=300_000).contains(&rate) || (900_001..=3_200_000).contains(&rate);
    if !valid {
        return Err(SdrError::InvalidSampleRate {
            rate_hz: sample_rate_hz,
        });
    }
    if rtl_xtal_hz == 0 {
        return Err(SdrError::InvalidParameter(
            "RTL crystal frequency must be non-zero".into(),
        ));
    }
    // Write the raw 28-bit ratio. Bit 28 is a software-only sign extension
    // used to recover the achieved low-band rate; it is not part of the
    // hardware register field.
    let ratio_num = (u64::from(rtl_xtal_hz) << 22) / u64::from(rate);
    let rsamp_ratio = ratio_num & 0x0fff_fffc;
    let real_rsamp_ratio = rsamp_ratio | ((rsamp_ratio & 0x0800_0000) << 1);
    if real_rsamp_ratio == 0 {
        return Err(SdrError::InvalidParameter(
            "sample-rate ratio underflow".into(),
        ));
    }
    let actual_rate = ((u64::from(rtl_xtal_hz) << 22) / real_rsamp_ratio) as u32;
    // High 16 bits to (1, 0x9f), low 16 bits to (1, 0xa1).
    let hi = ((rsamp_ratio >> 16) & 0xffff) as u16;
    let lo = (rsamp_ratio & 0xffff) as u16;
    demod_write_reg(transport, 1, 0x9f, hi, 2)?;
    demod_write_reg(transport, 1, 0xa1, lo, 2)?;
    // Soft-reset the demod so the new ratio takes effect. The required write
    // is the demod soft-reset register (1, 0x01) with 0x14 then 0x10 — NOT
    // (1, 0x14) — as documented by osmocom `rtlsdr_set_sample_rate`
    // (librtlsdr.c ~1130).
    demod_write_reg(transport, 1, 0x01, 0x14, 1)?;
    demod_write_reg(transport, 1, 0x01, 0x10, 1)?;
    Ok(actual_rate)
}

/// Set the IF frequency (24-bit signed, three registers).
///
/// `if_freq = -((center_freq * 2^22) / rtl_xtal)`, written to demod
/// registers `(1,0x19)/(1,0x1a)/(1,0x1b)` as bits 22-16, 15-8, 7-0.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_if_freq(
    transport: &mut dyn Transport,
    center_freq_hz: u64,
    rtl_xtal_hz: u32,
) -> Result<(), SdrError> {
    let if_freq = -(((center_freq_hz
        .checked_mul(1 << 22)
        .ok_or_else(|| SdrError::InvalidParameter("center frequency overflow".into()))?)
        / u64::from(rtl_xtal_hz)) as i64);
    let if_freq = if_freq as u32; // 24-bit signed stored in 32 bits
                                  // Osmocom uses & 0x3f for the high byte (only 6 bits of register 0x19).
    demod_write_reg(transport, 1, 0x19, ((if_freq >> 16) & 0x3f) as u16, 1)?;
    demod_write_reg(transport, 1, 0x1a, ((if_freq >> 8) & 0xff) as u16, 1)?;
    demod_write_reg(transport, 1, 0x1b, (if_freq & 0xff) as u16, 1)?;
    Ok(())
}

/// Set the RTL digital AGC (distinct from the tuner analog AGC).
/// `(0, 0x19) = on ? 0x25 : 0x05`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_agc_mode(transport: &mut dyn Transport, on: bool) -> Result<(), SdrError> {
    demod_write_reg(transport, 0, 0x19, if on { 0x25 } else { 0x05 }, 1)
}

/// Set the test-mode counter (8-bit incrementing pattern) on/off.
/// `(0, 0x19) = on ? 0x03 : 0x05`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_testmode(transport: &mut dyn Transport, on: bool) -> Result<(), SdrError> {
    demod_write_reg(transport, 0, 0x19, if on { 0x03 } else { 0x05 }, 1)
}

/// Set the frequency-correction PPM.
///
/// The RTL2832U stores the crystal correction in registers `(1, 0x3e)`/`(
/// 1, 0x3f)` as a signed value. The register field is 14 bits wide (6-bit high
/// byte at `0x3e` + 8-bit low byte at `0x3f`). The correction is applied to
/// the crystal frequency used in all subsequent PLL/sample-rate calculations.
///
/// The value written is `(ppm * (1 << 24)) / 1_000_000` — this converts parts
/// per million to the register's native fixed-point units (fractions of the
/// crystal frequency in units of `2^-24`). The sign is negative because a
/// positive ppm error means the crystal runs fast, so we subtract correction.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_freq_correction(
    transport: &mut dyn Transport,
    ppm: f64,
    _rtl_xtal_hz: u32,
) -> Result<(), SdrError> {
    if !ppm.is_finite() {
        return Err(SdrError::InvalidParameter(
            "frequency correction must be finite".into(),
        ));
    }
    // The correction value in register units: ppm * 2^24 / 1e6. This is
    // independent of crystal frequency; the register encodes a ratio.
    // Clamped to the 14-bit signed range [-8192, 8191].
    let offset = (-(ppm * f64::from(1 << 24) / 1_000_000.0)).round() as i32;
    let offset = offset.clamp(-(1 << 13), (1 << 13) - 1);
    // Write as two bytes: low 8 bits to 0x3f, high 6 bits to 0x3e.
    let lo = (offset & 0xff) as u16;
    let hi = ((offset >> 8) & 0x3f) as u16;
    demod_write_reg(transport, 1, 0x3f, lo, 1)?;
    demod_write_reg(transport, 1, 0x3e, hi, 1)?;
    Ok(())
}

/// Configure GPIO `pin` as an output (clears `GPD` bit, sets `GPOE` bit).
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_gpio_output(transport: &mut dyn Transport, pin: u8) -> Result<(), SdrError> {
    let gpd = read_reg(transport, Block::Sys, 0x3004, 1)?;
    let gpoe = read_reg(transport, Block::Sys, 0x3003, 1)?;
    let mask = 1u16 << pin;
    write_reg(transport, Block::Sys, 0x3004, gpd & !mask, 1)?;
    write_reg(transport, Block::Sys, 0x3003, gpoe | mask, 1)?;
    Ok(())
}

/// Set/clear GPIO `pin` output bit (read-modify-write `GPO` at `0x3001`).
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_gpio_bit(transport: &mut dyn Transport, pin: u8, on: bool) -> Result<(), SdrError> {
    let gpo = read_reg(transport, Block::Sys, 0x3001, 1)?;
    let mask = 1u16 << pin;
    let new = if on { gpo | mask } else { gpo & !mask };
    write_reg(transport, Block::Sys, 0x3001, new, 1)
}

/// Enable/disable the bias tee (GPIO 0). Convenience wrapper.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on a register-access failure.
pub fn set_bias_tee(transport: &mut dyn Transport, on: bool) -> Result<(), SdrError> {
    set_gpio_output(transport, 0)?;
    set_gpio_bit(transport, 0, on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::TransferDirection;
    use sdr_fox_transport::{MockTransport, ScriptedReply};

    #[test]
    fn set_sample_rate_rejects_invalid_rates() {
        let mut mock = MockTransport::new();
        // Too low.
        assert!(set_sample_rate(&mut mock, 100_000, RTL_XTAL_HZ).is_err());
        // In the gap.
        assert!(set_sample_rate(&mut mock, 500_000, RTL_XTAL_HZ).is_err());
        // Too high.
        assert!(set_sample_rate(&mut mock, 4_000_000, RTL_XTAL_HZ).is_err());
    }

    #[test]
    fn set_sample_rate_accepts_valid_rates() {
        let mut mock = MockTransport::new();
        // Valid low band.
        mock.push_reply(ScriptedReply::any_out()); // (1, 0x9f) ratio hi
        mock.push_reply(ScriptedReply::any_out()); // (1, 0xa1) ratio lo
        mock.push_reply(ScriptedReply::any_out()); // (1, 0x01) soft-reset 0x14
        mock.push_reply(ScriptedReply::any_out()); // (1, 0x01) soft-reset 0x10
        assert!(set_sample_rate(&mut mock, 250_000, RTL_XTAL_HZ).is_ok());

        let mut mock = MockTransport::new();
        for _ in 0..4 {
            mock.push_reply(ScriptedReply::any_out());
        }
        // Valid high band.
        assert!(set_sample_rate(&mut mock, 2_400_000, RTL_XTAL_HZ).is_ok());
    }

    #[test]
    fn set_agc_mode_writes_correct_value() {
        let mut mock = MockTransport::new();
        mock.push_reply(ScriptedReply::any_out()); // write
                                                   // The demod_write_reg produces a write + a dummy flush read.
        mock.push_reply(ScriptedReply::any_in(vec![0])); // flush
        set_agc_mode(&mut mock, true).unwrap();
        let writes: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.direction == sdr_fox_core::TransferDirection::Out)
            .collect();
        assert_eq!(writes[0].data, vec![0x25]);
    }

    #[test]
    fn set_bias_tee_enables_gpio0_output_then_sets_bit() {
        let mut mock = MockTransport::new();
        // set_gpio_output reads GPD and GPOE (2 reads), writes each (2 writes),
        // each write has a flush read; then set_gpio_bit reads GPO (1 read) and
        // writes it (1 write + flush read).
        mock.push_reply(ScriptedReply::any_in(vec![0x00])); // GPD read
        mock.push_reply(ScriptedReply::any_in(vec![0x00])); // GPOE read
        mock.push_reply(ScriptedReply::any_out()); // GPD write
        mock.push_reply(ScriptedReply::any_out()); // GPOE write
        mock.push_reply(ScriptedReply::any_in(vec![0x00])); // GPO read
        mock.push_reply(ScriptedReply::any_out()); // GPO write
        set_bias_tee(&mut mock, true).unwrap();
        // Find the final GPO write at sys addr 0x3001 and confirm bit 0 set.
        let gpo_writes: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.value == 0x3001 && r.direction == sdr_fox_core::TransferDirection::Out)
            .collect();
        assert_eq!(gpo_writes.last().unwrap().data, vec![0x01]);
    }

    /// `init_baseband`'s USB EPA writes must serialize **big-endian**. This is
    /// the regression guard for the max-packet-size bug: `USB_EPA_MAXPKT=0x0002`
    /// on the wire as `[0x00, 0x02]`, `USB_EPA_CTL=0x1002` as `[0x10, 0x02]`.
    #[test]
    fn init_baseband_epa_writes_are_big_endian() {
        let mut mock = MockTransport::new();
        init_baseband(&mut mock).unwrap();
        // Find the EPA writes by their addresses.
        let maxpkt = mock
            .recorded()
            .iter()
            .find(|r| r.value == 0x2158 && r.direction == TransferDirection::Out)
            .expect("USB_EPA_MAXPKT write");
        assert_eq!(maxpkt.data, vec![0x00, 0x02], "MAXPKT 0x0002 -> 00 02");
        let epa_ctl = mock
            .recorded()
            .iter()
            .find(|r| r.value == 0x2148 && r.direction == TransferDirection::Out)
            .expect("USB_EPA_CTL write");
        assert_eq!(epa_ctl.data, vec![0x10, 0x02], "EPA_CTL 0x1002 -> 10 02");
    }

    /// `init_baseband` must program the default ADC datapath, demod page-0
    /// register 0x06 = 0x80.
    ///
    /// This is a regression guard against confusing the required `(0, 0x06)`
    /// and `(0, 0x08)` writes. Losing the former costs the whole bulk data path
    /// on a cold device while every control transfer keeps succeeding — a
    /// failure that looks exactly like a wedged dongle and that can hide after
    /// another driver has initialized the chip. Both writes are required.
    #[test]
    fn init_baseband_programs_the_default_adc_datapath() {
        let mut mock = MockTransport::new();
        init_baseband(&mut mock).unwrap();
        let recorded = mock.recorded();
        // demod_write_reg(page 0, addr 0x06) encodes as value (0x06 << 8) | 0x20.
        let adc_datapath = recorded
            .iter()
            .find(|r| r.value == (0x06 << 8) | 0x20 && r.direction == TransferDirection::Out)
            .expect(
                "init_baseband must write demod (0, 0x06) — without it the bulk \
                 endpoint stays silent on a cold device",
            );
        assert_eq!(
            adc_datapath.data,
            vec![0x80],
            "default ADC_I/ADC_Q datapath is 0x80"
        );
        // Its companion, which a previous change wrongly treated as a substitute.
        let in_phase = recorded
            .iter()
            .find(|r| r.value == (0x08 << 8) | 0x20 && r.direction == TransferDirection::Out)
            .expect("init_baseband must also write demod (0, 0x08)");
        assert_eq!(in_phase.data, vec![0x4d], "in-phase ADC input is 0x4d");
    }

    /// `reset_buffer` flushes `EPA_CTL` with 0x1002 then 0x0000 — both must be
    /// big-endian. A wrong byte order here re-introduces the 2-byte-packet bug.
    #[test]
    fn reset_buffer_writes_big_endian_epa_ctl() {
        let mut mock = MockTransport::new();
        reset_buffer(&mut mock).unwrap();
        let writes: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.value == 0x2148 && r.direction == TransferDirection::Out)
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].data, vec![0x10, 0x02], "flush 0x1002 -> 10 02");
        assert_eq!(writes[1].data, vec![0x00, 0x00], "run   0x0000 -> 00 00");
    }

    /// The sample-rate ratio registers (1, 0x9f)/(1, 0xa1) must carry the
    /// 16-bit halves big-endian. We compute the expected halves from the
    /// documented ratio formula and assert against the recorded bytes.
    #[test]
    fn set_sample_rate_ratio_writes_are_big_endian() {
        let mut mock = MockTransport::new();
        // 4 demod writes: 0x9f, 0xa1, 0x01 (0x14), 0x01 (0x10). Each is an OUT
        // and the mock auto-acks unmatched OUTs and zeroes unmatched INs.
        for _ in 0..4 {
            mock.push_reply(ScriptedReply::any_out());
        }
        let rate = 2_400_000u32;
        set_sample_rate(&mut mock, rate, RTL_XTAL_HZ).unwrap();

        // Recompute the expected ratio exactly as set_sample_rate does.
        let ratio_num = (u64::from(RTL_XTAL_HZ) << 22) / u64::from(rate);
        let rsamp_ratio = (ratio_num & 0x0fff_ffff) & 0x0fff_fffc;
        let hi = ((rsamp_ratio >> 16) & 0xffff) as u16;
        let lo = (rsamp_ratio & 0xffff) as u16;
        let expected_hi = vec![(hi >> 8) as u8, hi as u8];
        let expected_lo = vec![(lo >> 8) as u8, lo as u8];

        let find_ratio_write = |addr_low: u8| {
            mock.recorded()
                .iter()
                .find(|r| {
                    r.direction == TransferDirection::Out
                        && r.value == (u16::from(addr_low) << 8) | 0x20
                })
                .unwrap_or_else(|| panic!("missing ratio write for 0x{addr_low:02x}"))
        };
        assert_eq!(
            find_ratio_write(0x9f).data,
            expected_hi,
            "ratio hi 0x{hi:04x} must be big-endian"
        );
        assert_eq!(
            find_ratio_write(0xa1).data,
            expected_lo,
            "ratio lo 0x{lo:04x} must be big-endian"
        );
    }

    #[test]
    fn low_band_ratio_writes_raw_field_without_software_sign_bit() {
        let mut mock = MockTransport::new();
        for _ in 0..4 {
            mock.push_reply(ScriptedReply::any_out());
        }
        let actual = set_sample_rate(&mut mock, 300_000, RTL_XTAL_HZ).unwrap();
        assert_eq!(actual, 300_000);

        let high_half = mock
            .recorded()
            .iter()
            .find(|r| {
                r.direction == TransferDirection::Out && r.value == (u16::from(0x9f_u8) << 8) | 0x20
            })
            .expect("sample ratio high-half write");
        assert_eq!(high_half.data, vec![0x08, 0x00]);
    }

    #[test]
    fn set_sample_rate_returns_quantized_hardware_rate() {
        let mut mock = MockTransport::new();
        let requested = 3_199_624;
        let actual = set_sample_rate(&mut mock, requested, RTL_XTAL_HZ).unwrap();
        let raw = ((u64::from(RTL_XTAL_HZ) << 22) / u64::from(requested)) & 0x0fff_fffc;
        let real = raw | ((raw & 0x0800_0000) << 1);
        let expected = ((u64::from(RTL_XTAL_HZ) << 22) / real) as u32;
        assert_eq!(actual, expected);
    }

    /// The sample-rate soft-reset must write `(1, 0x01)` with values 0x14 then
    /// 0x10 — matching osmocom `rtlsdr_set_sample_rate`. The previous code
    /// wrongly wrote `(1, 0x14)` with 0x01/0x00 (addr and value swapped).
    #[test]
    fn set_sample_rate_soft_reset_matches_osmocom() {
        let mut mock = MockTransport::new();
        for _ in 0..4 {
            mock.push_reply(ScriptedReply::any_out());
        }
        set_sample_rate(&mut mock, 2_400_000, RTL_XTAL_HZ).unwrap();
        // Two writes to demod addr 0x01 (value << 8 | 0x20 = 0x0120).
        let soft_resets: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.value == ((0x01u16) << 8) | 0x20 && r.direction == TransferDirection::Out)
            .collect();
        assert_eq!(soft_resets.len(), 2, "expect exactly two (1, 0x01) writes");
        assert_eq!(soft_resets[0].data, vec![0x14]);
        assert_eq!(soft_resets[1].data, vec![0x10]);
    }

    /// Frequency-correction helper: compute the expected register bytes for a
    /// given ppm, mirroring `set_freq_correction` exactly.
    fn expected_corr_bytes(ppm: f64) -> (u8, u8) {
        let off = (-(ppm * f64::from(1 << 24) / 1_000_000.0)).round() as i32;
        let off = off.clamp(-(1 << 13), (1 << 13) - 1);
        let lo = (off & 0xff) as u8;
        let hi = ((off >> 8) & 0x3f) as u8;
        (lo, hi)
    }

    /// Find the demod write recorded for `addr` (a 1-byte payload).
    fn find_corr_write(mock: &MockTransport, addr: u8) -> &sdr_fox_transport::RecordedRequest {
        mock.recorded()
            .iter()
            .find(|r| {
                r.direction == TransferDirection::Out && r.value == (u16::from(addr) << 8) | 0x20
            })
            .unwrap_or_else(|| panic!("missing demod write for 0x{addr:02x}"))
    }

    /// Run `set_freq_correction(ppm)` and assert the recorded register bytes.
    fn assert_corr(ppm: f64) {
        let mut mock = MockTransport::new();
        // 2 demod writes (0x3f, 0x3e), each followed by a dummy flush read.
        mock.push_reply(ScriptedReply::any_out()); // (1, 0x3f) write
        mock.push_reply(ScriptedReply::any_in(vec![0])); // flush
        mock.push_reply(ScriptedReply::any_out()); // (1, 0x3e) write
        mock.push_reply(ScriptedReply::any_in(vec![0])); // flush
        set_freq_correction(&mut mock, ppm, RTL_XTAL_HZ).unwrap();
        let (lo, hi) = expected_corr_bytes(ppm);
        assert_eq!(
            find_corr_write(&mock, 0x3f).data,
            vec![lo],
            "ppm={ppm}: low byte mismatch"
        );
        assert_eq!(
            find_corr_write(&mock, 0x3e).data,
            vec![hi],
            "ppm={ppm}: high byte mismatch"
        );
    }

    #[test]
    fn set_freq_correction_positive_ppm() {
        // +100 ppm: crystal runs fast ⇒ negative correction.
        assert_corr(100.0);
        let mut mock = MockTransport::new();
        set_freq_correction(&mut mock, 1.0, RTL_XTAL_HZ).unwrap();
        assert_eq!(find_corr_write(&mock, 0x3f).data, vec![0xef]);
        assert_eq!(find_corr_write(&mock, 0x3e).data, vec![0x3f]);
    }

    #[test]
    fn set_freq_correction_negative_ppm() {
        // -100 ppm: crystal runs slow ⇒ positive correction.
        assert_corr(-100.0);
        let mut mock = MockTransport::new();
        set_freq_correction(&mut mock, -1.0, RTL_XTAL_HZ).unwrap();
        assert_eq!(find_corr_write(&mock, 0x3f).data, vec![0x11]);
        assert_eq!(find_corr_write(&mock, 0x3e).data, vec![0x00]);
    }

    #[test]
    fn set_freq_correction_fractional_ppm_rounds_byte_exactly() {
        // +0.5 ppm -> round(-8.388608) = -8 -> 14-bit 0x3ff8.
        let mut mock = MockTransport::new();
        set_freq_correction(&mut mock, 0.5, RTL_XTAL_HZ).unwrap();
        assert_eq!(find_corr_write(&mock, 0x3f).data, vec![0xf8]);
        assert_eq!(find_corr_write(&mock, 0x3e).data, vec![0x3f]);
    }

    #[test]
    fn set_freq_correction_rejects_non_finite_values() {
        for ppm in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut mock = MockTransport::new();
            assert!(set_freq_correction(&mut mock, ppm, RTL_XTAL_HZ).is_err());
            assert!(mock.recorded().is_empty());
        }
    }

    #[test]
    fn set_freq_correction_saturates_at_14_bit_field() {
        // A huge ppm would overflow the 14-bit register field; it must clamp to
        // ±(1<<13) rather than silently truncating.
        assert_corr(100_000.0); // saturates negative
        assert_corr(-100_000.0); // saturates positive
                                 // Verify the saturation value directly: +1e5 ppm → offset = -8192.
                                 // The register field is 14-bit signed; sign-extend from bit 13.
        let sext14 = |hi: u8, lo: u8| -> i32 {
            let raw = (u16::from(hi) << 8) | u16::from(lo);
            let raw = i32::from(raw);
            if raw & (1 << 13) != 0 {
                raw | !0x3fff // set the upper bits
            } else {
                raw
            }
        };
        let (lo, hi) = expected_corr_bytes(100_000.0);
        assert_eq!(sext14(hi, lo), -8192);
        let (lo, hi) = expected_corr_bytes(-100_000.0);
        assert_eq!(sext14(hi, lo), 8191);
    }
}
