//! Elonics E4000 tuner driver.
//!
//! Implements the [`Tuner`] trait against a [`TunerBus`] (the RTL2832 I2C
//! repeater in production, a mock in tests). The register programming
//! sequence is a chip interface fact, re-derived from the E4000 register map
//! and from the register-level behaviour established publicly by the osmocom
//! reference driver and the Linux kernel driver for this part.
//!
//! ## Architecture
//!
//! The E4000 is a direct-conversion (zero-IF) tuner: a quadrature mixer
//! translates the wanted RF channel straight to baseband, so — unlike the
//! R82xx — there is no IF offset to add and the synthesizer is programmed to
//! the requested RF directly. The signal chain is: switched RF tracking
//! filter → LNA → quadrature mixer (4 or 12 dB) → mixer low-pass → RC
//! low-pass → six-stage IF amplifier → channel low-pass filter → baseband
//! I/Q out to the RTL2832 ADCs.
//!
//! ## Bus semantics — plain, non-reversed reads
//!
//! The E4000 returns register bytes in normal bit order and its whole
//! register file is readable. This driver therefore assumes
//! [`TunerBus::i2c_read`] delivers plain datasheet bytes with **no bit
//! reversal** (bit-reversed reads are an R82xx chip property, not an RTL2832
//! tunnel property). Masked register updates use live read-modify-write
//! rather than the R82xx driver's shadow-register file: the chip's power-on
//! defaults for untouched bits are read back instead of assumed, which
//! matters because the E4000's full reset-default register image is not
//! publicly documented.
//!
//! ## Synthesizer
//!
//! The fractional-N PLL synthesizes `f_vco = f_ref × (Z + X / 65536)` from
//! the dongle's 28.8 MHz reference, and the LO is `f_vco / M` where the
//! total multiple `M` comes from a fixed band table ([`SYNTH_BANDS`]). Below
//! 350 MHz the chip switches the mixer to 3-phase LO generation (bit 3 of
//! the R-divider register), which doubles the available division range.
//!
//! ## The L-band coverage hole
//!
//! Between [`E4000_GAP_LOW_HZ`] and [`E4000_GAP_HIGH_HZ`] the synthesizer
//! generally cannot reach the LO: the last table row (`M = 4`) tops out where
//! the VCO runs out of range above ~4.4 `GHz` (LO 1100 MHz), and the `M = 2`
//! fallback only reaches down to a VCO of ~2.48 `GHz` (LO ~1242 MHz).
//!
//! Both edges are therefore an **analog VCO limit, not a table boundary**, and
//! measurement on real hardware (NESDR Smart XTR) bears that out: the
//! reference driver locks at 1241 MHz, and 1100 MHz and 1242 MHz were each
//! seen both locking and failing across repeated runs — they sit exactly on
//! the VCO's lower edge. So [`Tuner::set_freq`] does **not** reject the hole
//! from a table; it programs the synthesizer and reads the lock bit, letting
//! the individual chip decide. Refusing up front would be a false negative on
//! frequencies the silicon can actually reach. The two constants remain as the
//! documented nominal hole for callers that want to plan a sweep around it.

use sdr_fox_core::{GainMode, GainRequest, GainStep, Tuner, TunerBus, TunerError, TunerKind};

/// 8-bit I2C write address of the E4000.
pub const E4000_I2C_ADDR: u8 = 0xc8;
/// Register holding the chip identification byte.
pub const E4000_CHECK_REG: u8 = 0x02;
/// Expected chip identification value (`0x40`, ASCII `@`).
pub const E4000_CHECK_VAL: u8 = 0x40;

/// Reference clock on RTL2832 dongles (28.8 MHz). The E4000 accepts 16–30
/// MHz; the synthesizer arithmetic below is exact for any reference, but the
/// RTL2832 always feeds the tuner its own 28.8 MHz crystal.
pub const E4000_XTAL_HZ: u32 = 28_800_000;

/// Nominal lowest tunable frequency, for callers planning a sweep.
///
/// Advisory, not enforced — see [`TUNE_ENVELOPE_MIN_HZ`]. Individual chips
/// differ: a NESDR Smart XTR locked at 52 MHz and failed at 51, while a NESDR
/// `SMArTee` XTR failed at 52 and reported a 53 MHz floor.
pub const E4000_MIN_HZ: u64 = 52_000_000;
/// Nominal highest tunable frequency, on the same advisory terms as
/// [`E4000_MIN_HZ`]. Measured spread across two boards: 2200 MHz on one,
/// 2221 MHz on the other.
pub const E4000_MAX_HZ: u64 = 2_200_000_000;

/// Hard lower bound for [`Tuner::set_freq`]: below this the request is refused
/// without touching the chip.
///
/// This is deliberately *wider* than [`E4000_MIN_HZ`]. The nominal figure is
/// what a typical chip manages, not a limit the silicon enforces, and the two
/// boards measured here disagree about it by a megahertz. Refusing at the
/// nominal value would deny frequencies a given chip can genuinely reach — the
/// same false-negative the L-band hole used to produce. The envelope exists
/// only to keep obvious nonsense away from the synthesizer; inside it the PLL
/// lock bit is the authority.
const TUNE_ENVELOPE_MIN_HZ: u64 = 50_000_000;
/// Hard upper bound for [`Tuner::set_freq`], on the same terms as
/// [`TUNE_ENVELOPE_MIN_HZ`]. Set clear of the highest edge observed on real
/// hardware (2221 MHz) so a better-binned chip is not artificially capped.
const TUNE_ENVELOPE_MAX_HZ: u64 = 2_300_000_000;

/// Nominal last usable frequency below the L-band coverage hole. Advisory,
/// not enforced: the edge is an analog VCO limit and is marginal in practice
/// (1100 MHz was observed both locking and failing on the same board), so
/// [`Tuner::set_freq`] asks the hardware rather than this constant.
pub const E4000_GAP_LOW_HZ: u64 = 1_100_000_000;
/// Nominal first usable frequency above the L-band coverage hole. Advisory
/// on the same terms as [`E4000_GAP_LOW_HZ`] — the reference driver locks at
/// 1241 MHz on the attached board, below this value.
pub const E4000_GAP_HIGH_HZ: u64 = 1_242_000_000;

/// The E4000 total-gain table (tenths of dB) — the 14 discrete overall gain
/// steps this tuner exposes, from −1.0 dB to 42.0 dB. The values are chip
/// characterisation data verified against the attached hardware with the
/// reference `rtl_test` tool; each step decomposes exactly into an LNA table
/// entry plus the 4 dB (or, for the top step, 12 dB) mixer setting — see
/// [`decompose_overall_gain`].
pub const E4000_GAIN_TABLE_TENTHS_DB: &[GainStep] = &[
    GainStep::new("OVERALL", -10),
    GainStep::new("OVERALL", 15),
    GainStep::new("OVERALL", 40),
    GainStep::new("OVERALL", 65),
    GainStep::new("OVERALL", 90),
    GainStep::new("OVERALL", 115),
    GainStep::new("OVERALL", 140),
    GainStep::new("OVERALL", 165),
    GainStep::new("OVERALL", 190),
    GainStep::new("OVERALL", 215),
    GainStep::new("OVERALL", 240),
    GainStep::new("OVERALL", 290),
    GainStep::new("OVERALL", 340),
    GainStep::new("OVERALL", 420),
];

// --- Register map -------------------------------------------------------
//
// Addresses are silicon facts from the E4000 register map. Names are this
// driver's own, chosen for the function each register performs here.

/// Power/standby control: bit 0 = soft reset, bit 1 = normal (not standby),
/// bit 2 = clear the power-on-reset latch.
const REG_MASTER: u8 = 0x00;
/// Reference-clock input configuration; `0x00` selects the default input
/// path used with the dongle's 28.8 MHz reference.
const REG_CLOCK_INPUT: u8 = 0x05;
/// Reference-clock output enable; `0x00` keeps the clock output off.
const REG_CLOCK_OUTPUT: u8 = 0x06;
/// Synthesizer band select (bits [2:1]) and, read-only in bit 0, the PLL
/// lock indicator.
const REG_SYNTH_BAND: u8 = 0x07;
/// Integer part `Z` of the fractional-N feedback divider.
const REG_SYNTH_INT: u8 = 0x09;
/// Fractional part `X`, low byte.
const REG_SYNTH_FRAC_LO: u8 = 0x0a;
/// Fractional part `X`, high byte.
const REG_SYNTH_FRAC_HI: u8 = 0x0b;
/// LO divider: bit 3 enables 3-phase mixing, bits [2:0] select the divider.
const REG_SYNTH_R_DIV: u8 = 0x0d;
/// RF tracking-filter select, bits [3:0].
const REG_RF_FILTER: u8 = 0x10;
/// Mixer low-pass (bits [7:4]) and RC low-pass (bits [3:0]) filter select.
const REG_IF_FILTER: u8 = 0x11;
/// Channel filter select (bits [4:0]) and disable (bit 5).
const REG_CHANNEL_FILTER: u8 = 0x12;
/// LNA gain code, bits [3:0].
const REG_LNA_GAIN: u8 = 0x14;
/// Mixer gain select, bit 0: 0 = 4 dB, 1 = 12 dB.
const REG_MIXER_GAIN: u8 = 0x15;
/// IF stages 1–4 gain codes (bit 0, bits [2:1], [4:3], [6:5]).
const REG_IF_GAIN_A: u8 = 0x16;
/// IF stages 5–6 gain codes (bits [2:0], [5:3]).
const REG_IF_GAIN_B: u8 = 0x17;
/// Gain-control mode: bits [3:0] select which stages the on-chip AGC runs.
const REG_AGC_MODE: u8 = 0x1a;
/// LNA AGC high detector threshold.
const REG_LNA_AGC_HIGH: u8 = 0x1d;
/// LNA AGC low detector threshold.
const REG_LNA_AGC_LOW: u8 = 0x1e;
/// LNA AGC calibration request and loop update rate.
const REG_LNA_AGC_RATE: u8 = 0x1f;
/// Mixer AGC control, bit 0 = automatic mixer gain.
const REG_MIXER_AGC: u8 = 0x20;
/// LNA gain-enhancement control, bits [2:0]; kept off.
const REG_LNA_BOOST: u8 = 0x24;
/// DC-offset calibration trigger, bit 0 (self-clearing one-shot).
const REG_DC_CAL: u8 = 0x29;
/// DC-offset machinery control: bits [1:0] I/Q lookup-table enables,
/// bit 2 range-detector enable, bit 4 time-variant correction.
const REG_DC_CTRL: u8 = 0x2d;
/// Time-variant DC correction time constant (I path), bits [1:0].
const REG_DC_TIME_A: u8 = 0x70;
/// Time-variant DC correction time constant (Q path), bits [1:0].
const REG_DC_TIME_B: u8 = 0x71;
/// Front-end bias current: 3 below the L band, 0 in the L band.
const REG_BIAS: u8 = 0x78;
/// Clock output power-down; `0x96` disables the output buffer.
const REG_CLOCK_OUT_PWR: u8 = 0x7a;
/// Channel-filter (RC) calibration trigger, bit 0.
const REG_CHANNEL_FILTER_CAL: u8 = 0x7b;

/// Reset + leave standby + clear the power-on-reset latch, in one write.
const MASTER_WAKE_RESET: u8 = 0x07;
/// PLL lock indicator in [`REG_SYNTH_BAND`].
const SYNTH_LOCKED_BIT: u8 = 0x01;
/// Band field in [`REG_SYNTH_BAND`] (bits [2:1]).
const SYNTH_BAND_MASK: u8 = 0x06;
/// Gain-control mode field in [`REG_AGC_MODE`].
const AGC_MODE_MASK: u8 = 0x0f;
/// All gain stages programmed over I2C (full manual).
const AGC_MODE_MANUAL: u8 = 0x00;
/// IF gain over I2C, LNA gain from the autonomous on-chip detector — the
/// chip's hardware AGC configuration for the LNA.
const AGC_MODE_LNA_AUTO: u8 = 0x09;
/// Automatic-mixer-gain enable bit in [`REG_MIXER_AGC`].
const MIXER_AGC_AUTO_BIT: u8 = 0x01;
/// Channel-filter disable bit in [`REG_CHANNEL_FILTER`].
const CHANNEL_FILTER_DISABLE_BIT: u8 = 0x20;
/// Range-detector enable bit in [`REG_DC_CTRL`], required for the one-shot
/// DC calibration to measure which correction range it needs.
const DC_RANGE_DETECTOR_BIT: u8 = 0x04;

/// Undocumented bring-up writes required for correct operation, applied once
/// after reset. These registers sit outside the documented map; every
/// working driver programs the same bytes, so they are reproduced here as
/// chip facts.
const UNDOCUMENTED_INIT: &[(u8, u8)] = &[
    (0x7e, 0x01),
    (0x7f, 0xfe),
    (0x82, 0x00),
    (0x86, 0x50),
    (0x87, 0x20),
    (0x88, 0x01),
    (0x9f, 0x7f),
    (0xa0, 0x07),
];

// --- Synthesizer --------------------------------------------------------

/// Denominator of the fractional feedback divider: `f_vco = f_ref × (Z + X /
/// FRAC_SCALE)` with a 16-bit `X`.
const FRAC_SCALE: u64 = 65_536;

/// One row of the synthesizer band table: for an LO below `upper_hz`, the
/// divider register takes `r_byte` and the VCO runs at `lo × multiple`.
struct SynthBand {
    /// Exclusive upper LO edge for this row, Hz.
    upper_hz: u64,
    /// Byte for [`REG_SYNTH_R_DIV`]: bit 3 = 3-phase mixing, bits [2:0] =
    /// divider code.
    r_byte: u8,
    /// Total LO-to-VCO multiple this row realizes.
    multiple: u64,
}

/// The synthesizer band table — chip facts tying each LO range to the
/// divider code that keeps the VCO in its working range. Rows below 350 MHz
/// use 3-phase mixing (bit 3 set); requests at or above the last edge fall
/// through to the divide-by-2 configuration (`r_byte 0x00`), which is what
/// creates the L-band coverage hole documented at the top of this module.
#[rustfmt::skip]
const SYNTH_BANDS: &[SynthBand] = &[
    SynthBand { upper_hz:    72_400_000, r_byte: 0x0f, multiple: 48 },
    SynthBand { upper_hz:    81_200_000, r_byte: 0x0e, multiple: 40 },
    SynthBand { upper_hz:   108_300_000, r_byte: 0x0d, multiple: 32 },
    SynthBand { upper_hz:   162_500_000, r_byte: 0x0c, multiple: 24 },
    SynthBand { upper_hz:   216_600_000, r_byte: 0x0b, multiple: 16 },
    SynthBand { upper_hz:   325_000_000, r_byte: 0x0a, multiple: 12 },
    SynthBand { upper_hz:   350_000_000, r_byte: 0x09, multiple:  8 },
    SynthBand { upper_hz:   432_000_000, r_byte: 0x03, multiple:  8 },
    SynthBand { upper_hz:   667_000_000, r_byte: 0x02, multiple:  6 },
    SynthBand { upper_hz: 1_200_000_000, r_byte: 0x01, multiple:  4 },
];

/// Divider configuration used above the last [`SYNTH_BANDS`] edge.
const SYNTH_FALLBACK: (u8, u64) = (0x00, 2);

/// Everything `set_freq` derives from the target frequency, computed once so
/// tests can pin the exact register bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SynthPlan {
    /// Byte for [`REG_SYNTH_R_DIV`].
    r_byte: u8,
    /// Integer feedback divider `Z`.
    integer: u8,
    /// 16-bit fractional feedback divider `X`.
    fraction: u16,
    /// LO the synthesizer will actually produce after quantization, Hz.
    actual_hz: u64,
}

/// Compute the synthesizer programming for `hz`, or fail if the frequency is
/// outside the tunable range (including the L-band hole).
fn synth_plan(hz: u64) -> Result<SynthPlan, TunerError> {
    let fail = || TunerError::PllNotLocked { freq_hz: hz };
    if !(TUNE_ENVELOPE_MIN_HZ..=TUNE_ENVELOPE_MAX_HZ).contains(&hz) {
        return Err(fail());
    }
    // NOTE: the L-band hole is deliberately NOT rejected here. Its edges are
    // an analog VCO limit, not a table boundary, so they are marginal and
    // vary with the individual chip and its temperature: on the attached
    // NESDR Smart XTR the reference driver locks at 1241 MHz, and 1100 MHz
    // and 1242 MHz were each observed both locking and failing across runs.
    // A static reject turns that into a false negative — it refuses
    // frequencies the silicon can actually reach. The synthesizer is
    // programmed and the lock bit read instead, so the hardware is the
    // authority; [`E4000_GAP_LOW_HZ`]/[`E4000_GAP_HIGH_HZ`] remain as the
    // documented nominal hole for callers that want to plan around it.

    let (r_byte, multiple) = SYNTH_BANDS
        .iter()
        .find(|band| hz < band.upper_hz)
        .map_or(SYNTH_FALLBACK, |band| (band.r_byte, band.multiple));

    let reference = u64::from(E4000_XTAL_HZ);
    let vco_hz = hz.checked_mul(multiple).ok_or_else(fail)?;
    let integer_wide = vco_hz / reference;
    // The integer divider register is 8 bits. Within the tunable range the
    // divider never exceeds 153 (4.4 GHz VCO / 28.8 MHz), so a failure here
    // means the range constants are wrong, not that the PLL missed lock.
    let integer = u8::try_from(integer_wide).map_err(|_| TunerError::PllProgrammingFailed)?;
    let remainder = vco_hz - integer_wide * reference;
    // remainder < reference, so the scaled value fits u64 and the quotient
    // fits 16 bits.
    let fraction = (remainder * FRAC_SCALE / reference) as u16;

    let actual_vco = reference * integer_wide + reference * u64::from(fraction) / FRAC_SCALE;
    Ok(SynthPlan {
        r_byte,
        integer,
        fraction,
        actual_hz: actual_vco / multiple,
    })
}

// --- Band and RF tracking filter ----------------------------------------

/// The four RF input bands. The band selects front-end bias and which RF
/// tracking-filter table applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RfBand {
    /// Below 140 MHz.
    Vhf2,
    /// 140–350 MHz.
    Vhf3,
    /// 350–1135 MHz.
    Uhf,
    /// Above 1135 MHz.
    LBand,
}

impl RfBand {
    /// Band for a given LO frequency — the thresholds are chip facts.
    fn for_lo(hz: u64) -> Self {
        if hz < 140_000_000 {
            RfBand::Vhf2
        } else if hz < 350_000_000 {
            RfBand::Vhf3
        } else if hz < 1_135_000_000 {
            RfBand::Uhf
        } else {
            RfBand::LBand
        }
    }

    /// Two-bit band code written to [`REG_SYNTH_BAND`] bits [2:1].
    fn code(self) -> u8 {
        match self {
            RfBand::Vhf2 => 0,
            RfBand::Vhf3 => 1,
            RfBand::Uhf => 2,
            RfBand::LBand => 3,
        }
    }

    /// Front-end bias register value: 3 everywhere except the L band, which
    /// runs with bias 0.
    fn bias(self) -> u8 {
        match self {
            RfBand::LBand => 0,
            _ => 3,
        }
    }
}

/// Center frequencies (Hz) of the sixteen switched RF tracking filters in
/// the UHF band, indexed by the code written to [`REG_RF_FILTER`].
#[rustfmt::skip]
const RF_FILTER_CENTERS_UHF_HZ: [u64; 16] = [
    360_000_000, 380_000_000, 405_000_000, 425_000_000,
    450_000_000, 475_000_000, 505_000_000, 540_000_000,
    575_000_000, 615_000_000, 670_000_000, 720_000_000,
    760_000_000, 840_000_000, 890_000_000, 970_000_000,
];

/// Center frequencies (Hz) of the sixteen L-band RF tracking filters.
#[rustfmt::skip]
const RF_FILTER_CENTERS_L_HZ: [u64; 16] = [
    1_300_000_000, 1_320_000_000, 1_360_000_000, 1_410_000_000,
    1_445_000_000, 1_460_000_000, 1_490_000_000, 1_530_000_000,
    1_560_000_000, 1_590_000_000, 1_640_000_000, 1_660_000_000,
    1_680_000_000, 1_700_000_000, 1_720_000_000, 1_750_000_000,
];

/// Index of the table entry closest to `hz` (first entry wins ties).
fn closest_center(centers: &[u64], hz: u64) -> u8 {
    centers
        .iter()
        .enumerate()
        .min_by_key(|(_, &center)| center.abs_diff(hz))
        .map_or(0, |(i, _)| i as u8)
}

/// RF tracking-filter code for a band and LO. Both VHF bands use a single
/// fixed filter (code 0); UHF and L select the nearest switched filter.
fn rf_filter_code(band: RfBand, hz: u64) -> u8 {
    match band {
        RfBand::Vhf2 | RfBand::Vhf3 => 0,
        RfBand::Uhf => closest_center(&RF_FILTER_CENTERS_UHF_HZ, hz),
        RfBand::LBand => closest_center(&RF_FILTER_CENTERS_L_HZ, hz),
    }
}

// --- Baseband filters ---------------------------------------------------

/// Mixer low-pass bandwidths (Hz), indexed by the code in bits [7:4] of
/// [`REG_IF_FILTER`]. Codes 0–7 all select the wide-open 27 MHz setting;
/// 8–15 step down to 1.9 MHz. Chip filter facts.
#[rustfmt::skip]
const MIXER_FILTER_BW_HZ: [u32; 16] = [
    27_000_000, 27_000_000, 27_000_000, 27_000_000,
    27_000_000, 27_000_000, 27_000_000, 27_000_000,
    4_600_000, 4_200_000, 3_800_000, 3_400_000,
    3_300_000, 2_700_000, 2_300_000, 1_900_000,
];

/// RC low-pass bandwidths (Hz), indexed by the code in bits [3:0] of
/// [`REG_IF_FILTER`]. Chip filter facts.
#[rustfmt::skip]
const RC_FILTER_BW_HZ: [u32; 16] = [
    21_400_000, 21_000_000, 17_600_000, 14_700_000,
    12_400_000, 10_600_000, 9_000_000, 7_700_000,
    6_400_000, 5_300_000, 4_400_000, 3_400_000,
    2_600_000, 1_800_000, 1_200_000, 1_000_000,
];

/// Channel-filter bandwidths (Hz), indexed by the code in bits [4:0] of
/// [`REG_CHANNEL_FILTER`]. Chip filter facts.
#[rustfmt::skip]
const CHANNEL_FILTER_BW_HZ: [u32; 32] = [
    5_500_000, 5_300_000, 5_000_000, 4_800_000,
    4_600_000, 4_400_000, 4_300_000, 4_100_000,
    3_900_000, 3_800_000, 3_700_000, 3_600_000,
    3_400_000, 3_300_000, 3_200_000, 3_100_000,
    3_000_000, 2_950_000, 2_900_000, 2_800_000,
    2_750_000, 2_700_000, 2_600_000, 2_550_000,
    2_500_000, 2_450_000, 2_400_000, 2_300_000,
    2_280_000, 2_240_000, 2_200_000, 2_150_000,
];

/// Pick the narrowest filter that still passes `hz` from a widest-first
/// table: the requested bandwidth is a floor, so the closest entry *at or
/// above* it wins. Requests wider than the table clamp to the widest entry.
fn narrowest_at_or_above(table: &[u32], hz: u32) -> u8 {
    let idx = table.partition_point(|&bw| bw >= hz);
    idx.saturating_sub(1) as u8
}

// --- Gain tables --------------------------------------------------------

/// LNA gain steps: (tenths of dB, register code) for [`REG_LNA_GAIN`] bits
/// [3:0]. The codes are not contiguous — 2 and 3 are unused by the silicon.
/// Chip characterisation facts.
#[rustfmt::skip]
const LNA_GAIN_STEPS: [(i32, u8); 13] = [
    (-50, 0), (-25, 1), (0, 4), (25, 5), (50, 6), (75, 7), (100, 8),
    (125, 9), (150, 10), (175, 11), (200, 12), (250, 13), (300, 14),
];

/// The two mixer gain settings, tenths of dB: bit 0 of [`REG_MIXER_GAIN`]
/// clear = 4 dB, set = 12 dB.
const MIXER_GAIN_LOW_TENTHS_DB: i32 = 40;
/// See [`MIXER_GAIN_LOW_TENTHS_DB`].
const MIXER_GAIN_HIGH_TENTHS_DB: i32 = 120;

/// Per-stage gain steps of the six IF amplifier stages, tenths of dB,
/// indexed by the stage's register code. Non-decreasing within each stage;
/// trailing repeats are register codes the silicon aliases to the same gain.
/// Chip characterisation facts.
const IF_STAGE_STEPS_TENTHS_DB: [&[i32]; 6] = [
    &[-30, 60],
    &[0, 30, 60, 90],
    &[0, 30, 60, 90],
    &[0, 10, 20, 20],
    &[30, 60, 90, 120, 150, 150, 150, 150],
    &[30, 60, 90, 120, 150, 150, 150, 150],
];

/// Where each IF stage's code lives: (register, shift, field mask before
/// shifting).
const IF_STAGE_FIELDS: [(u8, u8, u8); 6] = [
    (REG_IF_GAIN_A, 0, 0x01),
    (REG_IF_GAIN_A, 1, 0x03),
    (REG_IF_GAIN_A, 3, 0x03),
    (REG_IF_GAIN_A, 5, 0x03),
    (REG_IF_GAIN_B, 0, 0x07),
    (REG_IF_GAIN_B, 3, 0x07),
];

/// IF stage codes programmed at init: 6, 0, 0, 0, 9, 9 dB — a moderate
/// 24 dB total that leaves headroom above and below for the AGC.
const IF_STAGE_DEFAULT_CODES: [u8; 6] = [1, 0, 0, 0, 2, 2];

/// Decompose an overall-gain request into the LNA code and the mixer
/// high/low bit. The request snaps to the nearest entry of
/// [`E4000_GAIN_TABLE_TENTHS_DB`]; every table entry then decomposes
/// exactly: the top step (42 dB) uses the 12 dB mixer setting, all others
/// the 4 dB setting, and the remainder is an exact LNA table value (clamped
/// to the 30 dB LNA maximum). The IF chain stays at its 24 dB default — the
/// table values are relative step sizes, not absolute chain gain.
fn decompose_overall_gain(tenths_db: i32) -> (u8, bool) {
    let snapped = E4000_GAIN_TABLE_TENTHS_DB
        .iter()
        .min_by_key(|step| (i64::from(step.tenths_db) - i64::from(tenths_db)).abs())
        .map_or(0, |step| step.tenths_db);
    let mixer_high = snapped > 340;
    let mixer_tenths = if mixer_high {
        MIXER_GAIN_HIGH_TENTHS_DB
    } else {
        MIXER_GAIN_LOW_TENTHS_DB
    };
    let lna_target = (snapped - mixer_tenths).min(300);
    (nearest_lna_code(lna_target), mixer_high)
}

/// LNA register code nearest to a requested gain in tenths of dB.
fn nearest_lna_code(tenths_db: i32) -> u8 {
    LNA_GAIN_STEPS
        .iter()
        .min_by_key(|(gain, _)| (i64::from(*gain) - i64::from(tenths_db)).abs())
        .map_or(0, |(_, code)| *code)
}

/// Plan the six IF stage codes for a combined IF ("VGA") gain request.
///
/// Starts every stage at its minimum (3 dB total) and walks the stages in
/// order, raising each by one step per pass, until the accumulated gain
/// reaches the target — the same table-walk shape the R82xx driver uses for
/// its LNA/mixer split. Steps that would not increase gain (the aliased
/// trailing codes) are skipped, so the walk terminates at the chain's true
/// 56 dB maximum.
fn if_chain_plan(target_tenths_db: i32) -> [u8; 6] {
    let mut codes = [0usize; 6];
    let mut total: i32 = IF_STAGE_STEPS_TENTHS_DB.iter().map(|s| s[0]).sum();
    if total < target_tenths_db {
        loop {
            let mut moved = false;
            for (stage, steps) in IF_STAGE_STEPS_TENTHS_DB.iter().enumerate() {
                let next = codes[stage] + 1;
                if next < steps.len() && steps[next] > steps[codes[stage]] {
                    total += steps[next] - steps[codes[stage]];
                    codes[stage] = next;
                    moved = true;
                    if total >= target_tenths_db {
                        break;
                    }
                }
            }
            if !moved || total >= target_tenths_db {
                break;
            }
        }
    }
    let mut out = [0u8; 6];
    for (dst, src) in out.iter_mut().zip(codes) {
        *dst = src as u8;
    }
    out
}

// --- Register access helpers --------------------------------------------

/// Unconditional full-byte register write.
fn write_reg(bus: &mut dyn TunerBus, reg: u8, val: u8) -> Result<(), TunerError> {
    bus.i2c_write(reg, &[val])
}

/// Read one register byte, surfacing a short read as a bus failure.
fn read_reg(bus: &mut dyn TunerBus, reg: u8) -> Result<u8, TunerError> {
    bus.i2c_read(reg, 1)?
        .first()
        .copied()
        .ok_or(TunerError::I2cTransferFailed {
            addr: E4000_I2C_ADDR,
        })
}

/// Masked read-modify-write: merge `val & mask` into the live register,
/// preserving the other bits, skipping the write when the field already
/// holds the value. Live readback (not a shadow) because the E4000's
/// reset-default register image is not publicly documented — see the module
/// docs.
fn write_masked(bus: &mut dyn TunerBus, reg: u8, mask: u8, val: u8) -> Result<(), TunerError> {
    let current = read_reg(bus, reg)?;
    if current & mask == val & mask {
        return Ok(());
    }
    write_reg(bus, reg, (current & !mask) | (val & mask))
}

/// Program one IF stage's gain code into its register field.
fn write_if_stage(bus: &mut dyn TunerBus, stage: usize, code: u8) -> Result<(), TunerError> {
    let (reg, shift, field) = IF_STAGE_FIELDS[stage];
    write_masked(bus, reg, field << shift, (code & field) << shift)
}

// --- Driver -------------------------------------------------------------

/// Elonics E4000 driver.
pub struct E4000 {
    /// Last frequency successfully programmed via [`Tuner::set_freq`], for
    /// tests and diagnostics.
    last_freq_hz: u64,
}

impl E4000 {
    /// Construct the driver. No bus access happens until [`Tuner::init`].
    #[must_use]
    pub fn new() -> Self {
        Self { last_freq_hz: 0 }
    }

    /// The last frequency programmed via [`Tuner::set_freq`].
    #[must_use]
    pub fn last_freq_hz(&self) -> u64 {
        self.last_freq_hz
    }
}

impl Default for E4000 {
    fn default() -> Self {
        Self::new()
    }
}

impl Tuner for E4000 {
    /// Full bring-up: wake, verify the chip id, reset, configure the
    /// reference clock path, apply the undocumented bring-up writes, set the
    /// AGC thresholds, select auto gain, program moderate IF gains and the
    /// narrowest baseband filters, then run the channel-filter calibration
    /// and a one-shot DC-offset calibration at the default operating point.
    ///
    /// The final register state matches the field-proven osmocom bring-up
    /// (DC lookup-table and time-variant correction machinery left
    /// disabled — the SDR-proven configuration); the two calibration
    /// triggers at the end are the chip's documented one-shot commands.
    fn init(&mut self, bus: &mut dyn TunerBus) -> Result<(), TunerError> {
        // The first transaction after power-up can be NACKed while the chip
        // wakes; issue a throwaway read and ignore the outcome.
        let _ = bus.i2c_read(REG_MASTER, 1);

        // Identify the chip before writing anything.
        if read_reg(bus, E4000_CHECK_REG)? != E4000_CHECK_VAL {
            return Err(TunerError::NoSupportedTuner);
        }

        // Reset, leave standby, clear the power-on-reset latch.
        write_reg(bus, REG_MASTER, MASTER_WAKE_RESET)?;

        // Reference clock: default input path for the dongle's 28.8 MHz
        // reference; clock output off and its buffer powered down.
        write_reg(bus, REG_CLOCK_INPUT, 0x00)?;
        write_reg(bus, REG_CLOCK_OUTPUT, 0x00)?;
        write_reg(bus, REG_CLOCK_OUT_PWR, 0x96)?;

        for &(reg, val) in UNDOCUMENTED_INIT {
            write_reg(bus, reg, val)?;
        }

        // LNA AGC detector thresholds and loop rate.
        write_reg(bus, REG_LNA_AGC_HIGH, 0x10)?;
        write_reg(bus, REG_LNA_AGC_LOW, 0x04)?;
        write_reg(bus, REG_LNA_AGC_RATE, 0x1a)?;

        // Start from full manual gain, then hand over to the hardware AGC —
        // the chip's documented mode-transition order.
        write_masked(bus, REG_AGC_MODE, AGC_MODE_MASK, AGC_MODE_MANUAL)?;
        write_masked(bus, REG_MIXER_AGC, MIXER_AGC_AUTO_BIT, 0x00)?;
        write_masked(bus, REG_AGC_MODE, AGC_MODE_MASK, AGC_MODE_LNA_AUTO)?;
        write_masked(bus, REG_MIXER_AGC, MIXER_AGC_AUTO_BIT, MIXER_AGC_AUTO_BIT)?;
        write_masked(bus, REG_LNA_BOOST, 0x07, 0x00)?;

        // Deterministic gain defaults: 4 dB mixer, 24 dB IF chain.
        write_masked(bus, REG_MIXER_GAIN, 0x01, 0x00)?;
        for (stage, &code) in IF_STAGE_DEFAULT_CODES.iter().enumerate() {
            write_if_stage(bus, stage, code)?;
        }

        // Narrowest baseband filters until the host asks for a bandwidth:
        // mixer 1.9 MHz, RC 1.0 MHz, channel 2.15 MHz, channel filter on.
        write_reg(bus, REG_IF_FILTER, 0xff)?;
        write_masked(
            bus,
            REG_CHANNEL_FILTER,
            0x1f | CHANNEL_FILTER_DISABLE_BIT,
            0x1f,
        )?;

        // DC lookup-table and time-variant machinery off (field-proven SDR
        // configuration; a time-variant corrector would fight the signal at
        // zero IF).
        write_masked(bus, REG_DC_CTRL, 0x03, 0x00)?;
        write_masked(bus, REG_DC_TIME_A, 0x03, 0x00)?;
        write_masked(bus, REG_DC_TIME_B, 0x03, 0x00)?;

        // One-shot channel-filter (RC time-constant) calibration.
        write_reg(bus, REG_CHANNEL_FILTER_CAL, 0x01)?;

        // One-shot DC-offset calibration at the default operating point: the
        // measured correction lands in the offset DACs and applies
        // statically. The range detector must be on for the calibration to
        // pick its correction range.
        write_masked(
            bus,
            REG_DC_CTRL,
            DC_RANGE_DETECTOR_BIT,
            DC_RANGE_DETECTOR_BIT,
        )?;
        write_reg(bus, REG_DC_CAL, 0x01)
    }

    /// Program the synthesizer, select the band and RF tracking filter, and
    /// verify PLL lock.
    ///
    /// Only frequencies outside the [`TUNE_ENVELOPE_MIN_HZ`]..=
    /// [`TUNE_ENVELOPE_MAX_HZ`] envelope are refused before any register
    /// write, with [`TunerError::PllNotLocked`]. Everything inside it —
    /// including the nominal L-band hole and the margins beyond
    /// [`E4000_MIN_HZ`]/[`E4000_MAX_HZ`] — is programmed and settled by
    /// reading the chip's own lock bit, because both the hole's edges and the
    /// range's are analog limits that differ from chip to chip.
    fn set_freq(&mut self, bus: &mut dyn TunerBus, hz: u64) -> Result<(), TunerError> {
        let plan = synth_plan(hz)?;

        // Divider, integer, and fractional words. The VCO tracks the new
        // feedback divider through its auto-calibration; no manual trigger.
        write_reg(bus, REG_SYNTH_R_DIV, plan.r_byte)?;
        write_reg(bus, REG_SYNTH_INT, plan.integer)?;
        let [frac_lo, frac_hi] = plan.fraction.to_le_bytes();
        write_reg(bus, REG_SYNTH_FRAC_LO, frac_lo)?;
        write_reg(bus, REG_SYNTH_FRAC_HI, frac_hi)?;

        // Band select from the LO actually synthesized. The band field is
        // cleared before being set: skipping the clear loses lock between
        // 325 and 350 MHz (silicon quirk).
        let band = RfBand::for_lo(plan.actual_hz);
        write_reg(bus, REG_BIAS, band.bias())?;
        write_masked(bus, REG_SYNTH_BAND, SYNTH_BAND_MASK, 0x00)?;
        write_masked(bus, REG_SYNTH_BAND, SYNTH_BAND_MASK, band.code() << 1)?;

        // Re-point the switched RF tracking filter at the new channel.
        write_masked(
            bus,
            REG_RF_FILTER,
            0x0f,
            rf_filter_code(band, plan.actual_hz),
        )?;

        // Lock verification is load-bearing: without it a marginal tune
        // delivers silence that reads as a decoder bug. The indicator is
        // valid by the time the readback transaction completes.
        if read_reg(bus, REG_SYNTH_BAND)? & SYNTH_LOCKED_BIT == 0 {
            return Err(TunerError::PllNotLocked { freq_hz: hz });
        }
        self.last_freq_hz = hz;
        Ok(())
    }

    /// Program all three baseband low-pass filters (mixer, RC, channel) to
    /// the narrowest setting that still passes `hz`. Requests wider than a
    /// table clamp to that table's widest filter; the channel filter stays
    /// enabled throughout because its widest setting (5.5 MHz) already
    /// exceeds the RTL2832's usable baseband.
    fn set_bandwidth(&mut self, bus: &mut dyn TunerBus, hz: u32) -> Result<(), TunerError> {
        let mixer = narrowest_at_or_above(&MIXER_FILTER_BW_HZ, hz);
        let rc = narrowest_at_or_above(&RC_FILTER_BW_HZ, hz);
        let channel = narrowest_at_or_above(&CHANNEL_FILTER_BW_HZ, hz);
        write_reg(bus, REG_IF_FILTER, (mixer << 4) | rc)?;
        write_masked(
            bus,
            REG_CHANNEL_FILTER,
            0x1f | CHANNEL_FILTER_DISABLE_BIT,
            channel,
        )
    }

    /// Apply a gain request.
    ///
    /// `Overall` snaps to the nearest step of [`E4000_GAIN_TABLE_TENTHS_DB`]
    /// and decomposes into LNA + mixer (forcing manual mode first, like the
    /// R82xx driver). `PerStage` accepts `"LNA"`, `"MIXER"`, and `"VGA"`
    /// (the six-stage IF chain), snapping each to its nearest hardware step.
    fn set_gain(&mut self, bus: &mut dyn TunerBus, req: GainRequest) -> Result<(), TunerError> {
        match req {
            GainRequest::Overall(tenths_db) => {
                let (lna_code, mixer_high) = decompose_overall_gain(tenths_db);
                self.set_gain_mode(bus, GainMode::Manual)?;
                write_masked(bus, REG_MIXER_GAIN, 0x01, u8::from(mixer_high))?;
                write_masked(bus, REG_LNA_GAIN, 0x0f, lna_code)
            }
            GainRequest::PerStage {
                name: "LNA",
                tenths_db,
            } => {
                self.set_gain_mode(bus, GainMode::Manual)?;
                write_masked(bus, REG_LNA_GAIN, 0x0f, nearest_lna_code(tenths_db))
            }
            GainRequest::PerStage {
                name: "MIXER",
                tenths_db,
            } => {
                self.set_gain_mode(bus, GainMode::Manual)?;
                let midpoint = (MIXER_GAIN_LOW_TENTHS_DB + MIXER_GAIN_HIGH_TENTHS_DB) / 2;
                write_masked(bus, REG_MIXER_GAIN, 0x01, u8::from(tenths_db > midpoint))
            }
            GainRequest::PerStage {
                name: "VGA",
                tenths_db,
            } => {
                // The IF chain is always I2C-controlled in both gain modes,
                // so no mode switch is needed.
                let codes = if_chain_plan(tenths_db);
                for (stage, &code) in codes.iter().enumerate() {
                    write_if_stage(bus, stage, code)?;
                }
                Ok(())
            }
            GainRequest::PerStage { .. } => Err(TunerError::InvalidGain),
        }
    }

    fn gains(&self) -> &[GainStep] {
        E4000_GAIN_TABLE_TENTHS_DB
    }

    /// Switch the on-chip gain control. `Auto` hands the LNA to the chip's
    /// autonomous detector and enables automatic mixer gain (the IF chain is
    /// always I2C-programmed on this part); `Manual` routes every stage
    /// through I2C.
    fn set_gain_mode(&mut self, bus: &mut dyn TunerBus, mode: GainMode) -> Result<(), TunerError> {
        match mode {
            GainMode::Manual => {
                write_masked(bus, REG_AGC_MODE, AGC_MODE_MASK, AGC_MODE_MANUAL)?;
                write_masked(bus, REG_MIXER_AGC, MIXER_AGC_AUTO_BIT, 0x00)
            }
            GainMode::Auto => {
                write_masked(bus, REG_AGC_MODE, AGC_MODE_MASK, AGC_MODE_LNA_AUTO)?;
                write_masked(bus, REG_MIXER_AGC, MIXER_AGC_AUTO_BIT, MIXER_AGC_AUTO_BIT)?;
                // Keep the LNA gain-enhancement off in auto mode; it trades
                // linearity for sensitivity and the AGC does not manage it.
                write_masked(bus, REG_LNA_BOOST, 0x07, 0x00)
            }
        }
    }

    fn kind(&self) -> TunerKind {
        TunerKind::E4000
    }
}

#[cfg(test)]
mod tests {
    use sdr_fox_core::GainStageId;

    use super::*;

    /// Register-file mock bus: writes land in a 256-byte register image and
    /// reads serve from it, so the driver's read-modify-write cycles behave
    /// exactly as they would against silicon. Every write is also recorded
    /// as (register, byte) for byte-exact assertions.
    struct MockBus {
        regs: [u8; 256],
        writes: Vec<(u8, u8)>,
    }

    impl MockBus {
        /// A responsive chip: correct id, PLL lock indicator asserted.
        fn responsive() -> Self {
            let mut regs = [0u8; 256];
            regs[usize::from(E4000_CHECK_REG)] = E4000_CHECK_VAL;
            regs[usize::from(REG_SYNTH_BAND)] = SYNTH_LOCKED_BIT;
            Self {
                regs,
                writes: Vec::new(),
            }
        }

        /// A chip whose PLL never indicates lock.
        fn never_locking() -> Self {
            let mut bus = Self::responsive();
            bus.regs[usize::from(REG_SYNTH_BAND)] = 0;
            bus
        }
    }

    impl TunerBus for MockBus {
        fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError> {
            for (i, &byte) in data.iter().enumerate() {
                let addr = usize::from(reg) + i;
                self.regs[addr] = byte;
                self.writes.push((addr as u8, byte));
            }
            Ok(())
        }

        fn i2c_read(&mut self, reg: u8, len: usize) -> Result<Vec<u8>, TunerError> {
            let start = usize::from(reg);
            Ok(self.regs[start..start + len].to_vec())
        }
    }

    /// A bus that fails every write, for error-propagation tests.
    struct DeadBus;
    impl TunerBus for DeadBus {
        fn i2c_write(&mut self, _reg: u8, _data: &[u8]) -> Result<(), TunerError> {
            Err(TunerError::I2cTransferFailed {
                addr: E4000_I2C_ADDR,
            })
        }
        fn i2c_read(&mut self, reg: u8, len: usize) -> Result<Vec<u8>, TunerError> {
            // Reads succeed so init reaches its first write: chip id and
            // lock bit look plausible.
            let mut buf = vec![0u8; len];
            if reg == E4000_CHECK_REG && len > 0 {
                buf[0] = E4000_CHECK_VAL;
            }
            Ok(buf)
        }
    }

    #[test]
    fn init_verifies_chip_id_then_programs_bring_up_sequence() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.init(&mut bus).unwrap();
        // Byte-exact pin of the full bring-up as seen on the wire. Masked
        // writes whose field already holds the target value emit nothing
        // (all mock registers start at 0), matching read-modify-write
        // against a freshly reset chip image of zeros.
        let expected: Vec<(u8, u8)> = vec![
            (REG_MASTER, MASTER_WAKE_RESET),
            (REG_CLOCK_INPUT, 0x00),
            (REG_CLOCK_OUTPUT, 0x00),
            (REG_CLOCK_OUT_PWR, 0x96),
            (0x7e, 0x01),
            (0x7f, 0xfe),
            (0x82, 0x00),
            (0x86, 0x50),
            (0x87, 0x20),
            (0x88, 0x01),
            (0x9f, 0x7f),
            (0xa0, 0x07),
            (REG_LNA_AGC_HIGH, 0x10),
            (REG_LNA_AGC_LOW, 0x04),
            (REG_LNA_AGC_RATE, 0x1a),
            (REG_AGC_MODE, AGC_MODE_LNA_AUTO),
            (REG_MIXER_AGC, MIXER_AGC_AUTO_BIT),
            (REG_IF_GAIN_A, 0x01),      // IF stage 1 -> 6 dB
            (REG_IF_GAIN_B, 0x02),      // IF stage 5 -> 9 dB
            (REG_IF_GAIN_B, 0x12),      // IF stage 6 -> 9 dB
            (REG_IF_FILTER, 0xff),      // narrowest mixer + RC filters
            (REG_CHANNEL_FILTER, 0x1f), // narrowest channel filter, enabled
            (REG_CHANNEL_FILTER_CAL, 0x01),
            (REG_DC_CTRL, DC_RANGE_DETECTOR_BIT),
            (REG_DC_CAL, 0x01),
        ];
        assert_eq!(bus.writes, expected);
    }

    #[test]
    fn init_rejects_wrong_chip_id_without_writing() {
        let mut bus = MockBus::responsive();
        bus.regs[usize::from(E4000_CHECK_REG)] = 0x00;
        let mut tuner = E4000::new();
        let err = tuner.init(&mut bus).unwrap_err();
        assert!(matches!(err, TunerError::NoSupportedTuner), "{err:?}");
        assert!(bus.writes.is_empty(), "must not program an unknown chip");
    }

    #[test]
    fn init_propagates_bus_write_failure() {
        let mut tuner = E4000::new();
        assert!(matches!(
            tuner.init(&mut DeadBus).unwrap_err(),
            TunerError::I2cTransferFailed {
                addr: E4000_I2C_ADDR
            }
        ));
    }

    /// 100 MHz sits in the 3-phase 81.2–108.3 MHz row (M = 32): VCO
    /// 3.2 `GHz`, Z = 111, X = 7281, and the sub-140 MHz band code 0 with
    /// bias 3. Band and RF filter fields are already 0, so only the bias
    /// write follows the four synthesizer bytes.
    #[test]
    fn set_freq_100mhz_programs_three_phase_synth() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_freq(&mut bus, 100_000_000).unwrap();
        assert_eq!(
            bus.writes,
            vec![
                (REG_SYNTH_R_DIV, 0x0d),
                (REG_SYNTH_INT, 111),
                (REG_SYNTH_FRAC_LO, 0x71),
                (REG_SYNTH_FRAC_HI, 0x1c),
                (REG_BIAS, 0x03),
            ]
        );
        assert_eq!(tuner.last_freq_hz(), 100_000_000);
    }

    /// 500 MHz: 2-phase row below 667 MHz (M = 6): VCO 3.0 `GHz`, Z = 104,
    /// X = 10922; UHF band (code 2), tracking filter centered at 505 MHz
    /// (code 6). The band write carries the lock bit read back from the
    /// status register, as on real silicon.
    #[test]
    fn set_freq_500mhz_programs_uhf_band_and_tracking_filter() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_freq(&mut bus, 500_000_000).unwrap();
        assert_eq!(
            bus.writes,
            vec![
                (REG_SYNTH_R_DIV, 0x02),
                (REG_SYNTH_INT, 104),
                (REG_SYNTH_FRAC_LO, 0xaa),
                (REG_SYNTH_FRAC_HI, 0x2a),
                (REG_BIAS, 0x03),
                (REG_SYNTH_BAND, 0x05),
                (REG_RF_FILTER, 0x06),
            ]
        );
    }

    /// 1.7 `GHz`: the above-1.2 `GHz` fallback (M = 2, divider byte 0x00): VCO
    /// 3.4 `GHz`, Z = 118, X = 3640; L band (code 3, bias 0), tracking filter
    /// centered at 1700 MHz (code 13).
    #[test]
    fn set_freq_1700mhz_programs_l_band() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_freq(&mut bus, 1_700_000_000).unwrap();
        assert_eq!(
            bus.writes,
            vec![
                (REG_SYNTH_R_DIV, 0x00),
                (REG_SYNTH_INT, 118),
                (REG_SYNTH_FRAC_LO, 0x38),
                (REG_SYNTH_FRAC_HI, 0x0e),
                (REG_BIAS, 0x00),
                (REG_SYNTH_BAND, 0x07),
                (REG_RF_FILTER, 0x0d),
            ]
        );
    }

    /// The nominal edges must tune, and so must frequencies beyond them that a
    /// better-binned chip can reach. A NESDR `SMArTee` XTR reports a 2221 MHz
    /// ceiling where a NESDR Smart XTR stops at 2200; enforcing the nominal
    /// figure would deny the former ~21 MHz of genuine range.
    #[test]
    fn set_freq_covers_both_range_edges_and_beyond_the_nominal_ones() {
        for hz in [
            E4000_MIN_HZ,
            E4000_MAX_HZ,
            E4000_MIN_HZ - 1,
            E4000_MAX_HZ + 21_000_000,
            TUNE_ENVELOPE_MIN_HZ,
            TUNE_ENVELOPE_MAX_HZ,
        ] {
            let mut bus = MockBus::responsive();
            let mut tuner = E4000::new();
            tuner
                .set_freq(&mut bus, hz)
                .unwrap_or_else(|e| panic!("{hz}: a locking chip must tune: {e:?}"));
            assert_eq!(tuner.last_freq_hz(), hz);
        }
    }

    #[test]
    fn set_freq_rejects_out_of_range_without_writing() {
        for hz in [
            0,
            TUNE_ENVELOPE_MIN_HZ - 1,
            TUNE_ENVELOPE_MAX_HZ + 1,
            u64::MAX,
        ] {
            let mut bus = MockBus::responsive();
            let mut tuner = E4000::new();
            let err = tuner.set_freq(&mut bus, hz).unwrap_err();
            assert!(
                matches!(err, TunerError::PllNotLocked { freq_hz } if freq_hz == hz),
                "{hz}: {err:?}"
            );
            assert!(bus.writes.is_empty(), "{hz}: must not program the synth");
            assert_eq!(tuner.last_freq_hz(), 0);
        }
    }

    /// Frequencies inside the nominal L-band hole must still be attempted:
    /// the hole's edges are an analog VCO limit, so whether a given chip
    /// locks near them is a hardware fact, not a table fact. The driver
    /// programs the synthesizer and lets the lock bit decide — a static
    /// reject would refuse 1241 MHz, which the reference driver locks on the
    /// attached hardware.
    #[test]
    fn set_freq_attempts_the_l_band_hole_and_lets_the_lock_bit_decide() {
        for hz in [
            1_100_000_000u64,
            1_101_000_000,
            1_150_000_000,
            1_241_000_000,
            1_242_000_000,
        ] {
            let mut bus = MockBus::responsive();
            let mut tuner = E4000::new();
            tuner
                .set_freq(&mut bus, hz)
                .unwrap_or_else(|e| panic!("{hz}: a locking chip must tune: {e:?}"));
            assert!(
                !bus.writes.is_empty(),
                "{hz}: the synthesizer must actually be programmed"
            );
        }
    }

    /// The converse: when the chip reports no lock inside the hole, that is
    /// what the caller sees — carrying the requested frequency, not a
    /// table-derived guess.
    #[test]
    fn set_freq_in_the_hole_reports_the_chips_lock_failure() {
        let mut bus = MockBus::never_locking();
        let mut tuner = E4000::new();
        let err = tuner
            .set_freq(&mut bus, 1_150_000_000)
            .expect_err("a non-locking chip must fail");
        assert!(
            matches!(err, TunerError::PllNotLocked { freq_hz } if freq_hz == 1_150_000_000),
            "got {err:?}"
        );
    }

    /// The gap boundary rows themselves stay in VCO range: 1100 MHz uses
    /// M = 4 (VCO 4.4 `GHz`) and 1242 MHz falls through to M = 2 (VCO
    /// 2.484 `GHz`).
    #[test]
    fn synth_plan_gap_edges_use_expected_dividers() {
        let low = synth_plan(1_100_000_000).unwrap();
        assert_eq!((low.r_byte, low.integer), (0x01, 152));
        let high = synth_plan(1_242_000_000).unwrap();
        assert_eq!((high.r_byte, high.integer), (0x00, 86));
        assert_eq!(high.fraction, 0x4000, "7.2 MHz remainder = 16384/65536");
    }

    #[test]
    fn set_freq_fails_loudly_when_pll_never_locks() {
        let mut bus = MockBus::never_locking();
        let mut tuner = E4000::new();
        let err = tuner.set_freq(&mut bus, 500_000_000).unwrap_err();
        assert!(
            matches!(
                err,
                TunerError::PllNotLocked {
                    freq_hz: 500_000_000
                }
            ),
            "{err:?}"
        );
        assert_eq!(tuner.last_freq_hz(), 0, "failed tune must not be recorded");
    }

    /// Synthesized LO error stays within one fractional step (`f_ref/65536`
    /// ≈ 440 Hz) across representative frequencies in every divider band.
    #[test]
    fn synth_plan_quantization_error_is_below_one_fractional_step() {
        for hz in [
            52_000_000u64,
            60_000_000,
            75_000_000,
            100_000_000,
            144_000_000,
            200_000_000,
            340_000_000,
            400_000_000,
            500_000_000,
            868_000_000,
            1_090_000_000,
            1_575_420_000,
            2_200_000_000,
        ] {
            let plan = synth_plan(hz).unwrap();
            let step = u64::from(E4000_XTAL_HZ) / FRAC_SCALE + 1;
            assert!(
                plan.actual_hz.abs_diff(hz) <= step,
                "{hz}: actual {} off by more than {step}",
                plan.actual_hz
            );
        }
    }

    #[test]
    fn set_bandwidth_2mhz_picks_narrow_taps() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_bandwidth(&mut bus, 2_000_000).unwrap();
        // Mixer 2.3 MHz (code 14), RC 2.6 MHz (code 12), channel 2.15 MHz
        // (code 31) — each the narrowest filter still >= 2 MHz.
        assert_eq!(
            bus.writes,
            vec![(REG_IF_FILTER, 0xec), (REG_CHANNEL_FILTER, 0x1f)]
        );
    }

    #[test]
    fn set_bandwidth_boundary_is_at_or_above() {
        // Exactly 2.3 MHz may use the 2.3 MHz mixer tap...
        assert_eq!(narrowest_at_or_above(&MIXER_FILTER_BW_HZ, 2_300_000), 14);
        // ...but one hertz more must widen to the 2.7 MHz tap.
        assert_eq!(narrowest_at_or_above(&MIXER_FILTER_BW_HZ, 2_300_001), 13);
        assert_eq!(narrowest_at_or_above(&CHANNEL_FILTER_BW_HZ, 2_300_000), 27);
        assert_eq!(narrowest_at_or_above(&CHANNEL_FILTER_BW_HZ, 2_300_001), 26);
    }

    #[test]
    fn set_bandwidth_clamps_to_table_edges() {
        // Narrower than everything: the narrowest taps.
        assert_eq!(narrowest_at_or_above(&MIXER_FILTER_BW_HZ, 100_000), 15);
        assert_eq!(narrowest_at_or_above(&RC_FILTER_BW_HZ, 100_000), 15);
        assert_eq!(narrowest_at_or_above(&CHANNEL_FILTER_BW_HZ, 100_000), 31);
        // Wider than the channel filter offers: clamp to its widest tap.
        assert_eq!(narrowest_at_or_above(&CHANNEL_FILTER_BW_HZ, 8_000_000), 0);
        // 6 MHz: mixer stays wide open (27 MHz), RC picks 6.4 MHz.
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_bandwidth(&mut bus, 6_000_000).unwrap();
        assert_eq!(bus.writes, vec![(REG_IF_FILTER, 0x78)]);
        // Channel filter masked write was a no-op (already at code 0), but
        // the register must remain enabled (bit 5 clear).
        assert_eq!(bus.regs[usize::from(REG_CHANNEL_FILTER)] & 0x20, 0);
    }

    #[test]
    fn gain_table_has_14_monotonic_steps_matching_hardware() {
        assert_eq!(E4000_GAIN_TABLE_TENTHS_DB.len(), 14);
        assert_eq!(E4000_GAIN_TABLE_TENTHS_DB[0].tenths_db, -10);
        assert_eq!(E4000_GAIN_TABLE_TENTHS_DB[13].tenths_db, 420);
        for pair in E4000_GAIN_TABLE_TENTHS_DB.windows(2) {
            assert!(pair[0].tenths_db < pair[1].tenths_db, "{pair:?}");
        }
    }

    /// Every advertised total-gain step must decompose exactly: LNA tenths
    /// plus mixer tenths reproduce the table value (the task's ground-truth
    /// totals), except the top step where the LNA saturates at 30 dB.
    #[test]
    fn every_gain_step_decomposes_into_lna_plus_mixer() {
        for step in E4000_GAIN_TABLE_TENTHS_DB {
            let (lna_code, mixer_high) = decompose_overall_gain(step.tenths_db);
            let lna_tenths = LNA_GAIN_STEPS
                .iter()
                .find(|(_, code)| *code == lna_code)
                .map(|(tenths, _)| *tenths)
                .expect("decomposition must land on a real LNA step");
            let mixer_tenths = if mixer_high { 120 } else { 40 };
            assert_eq!(
                lna_tenths + mixer_tenths,
                step.tenths_db,
                "step {} must equal LNA {} + mixer {}",
                step.tenths_db,
                lna_tenths,
                mixer_tenths
            );
        }
    }

    #[test]
    fn set_gain_overall_programs_lna_and_mixer_registers() {
        // (request, expected LNA code, expected mixer bit)
        for (request, lna, mixer) in [
            (420, 0x0e, 1),    // top step: 30 dB LNA + 12 dB mixer
            (90, 0x06, 0),     // 9 dB total: 5 dB LNA + 4 dB mixer
            (-10, 0x00, 0),    // bottom step: -5 dB LNA + 4 dB mixer
            (100, 0x06, 0),    // snaps to the nearer 90 step
            (10_000, 0x0e, 1), // clamps to the top step
        ] {
            let mut bus = MockBus::responsive();
            let mut tuner = E4000::new();
            tuner
                .set_gain(&mut bus, GainRequest::overall(request))
                .unwrap();
            assert_eq!(
                bus.regs[usize::from(REG_LNA_GAIN)] & 0x0f,
                lna,
                "request {request}: LNA code"
            );
            assert_eq!(
                bus.regs[usize::from(REG_MIXER_GAIN)] & 0x01,
                mixer,
                "request {request}: mixer bit"
            );
            // Overall gain forces manual mode.
            assert_eq!(bus.regs[usize::from(REG_AGC_MODE)] & AGC_MODE_MASK, 0);
            assert_eq!(bus.regs[usize::from(REG_MIXER_AGC)] & 0x01, 0);
        }
    }

    #[test]
    fn set_gain_per_stage_lna_and_mixer() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner
            .set_gain(&mut bus, GainRequest::per_stage(GainStageId::Lna, 175))
            .unwrap();
        assert_eq!(bus.regs[usize::from(REG_LNA_GAIN)] & 0x0f, 0x0b);
        tuner
            .set_gain(&mut bus, GainRequest::per_stage(GainStageId::Mixer, 120))
            .unwrap();
        assert_eq!(bus.regs[usize::from(REG_MIXER_GAIN)] & 0x01, 0x01);
        tuner
            .set_gain(&mut bus, GainRequest::per_stage(GainStageId::Mixer, 40))
            .unwrap();
        assert_eq!(bus.regs[usize::from(REG_MIXER_GAIN)] & 0x01, 0x00);
    }

    /// A 24 dB IF request walks one step into every stage: 6+3+3+1+6+6 =
    /// 25 dB, the first walk state at or above the target.
    #[test]
    fn set_gain_per_stage_vga_walks_the_if_chain() {
        assert_eq!(if_chain_plan(240), [1, 1, 1, 1, 1, 1]);
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner
            .set_gain(&mut bus, GainRequest::per_stage(GainStageId::Vga, 240))
            .unwrap();
        assert_eq!(bus.regs[usize::from(REG_IF_GAIN_A)], 0x2b);
        assert_eq!(bus.regs[usize::from(REG_IF_GAIN_B)], 0x09);
        // Extremes: minimum chain and maximum chain terminate.
        assert_eq!(if_chain_plan(-100), [0, 0, 0, 0, 0, 0]);
        assert_eq!(if_chain_plan(10_000), [1, 3, 3, 2, 4, 4]);
    }

    #[test]
    fn set_gain_rejects_unknown_stage() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        let err = tuner
            .set_gain(
                &mut bus,
                GainRequest::PerStage {
                    name: "PREAMP",
                    tenths_db: 100,
                },
            )
            .unwrap_err();
        assert!(matches!(err, TunerError::InvalidGain), "{err:?}");
    }

    #[test]
    fn gain_mode_auto_and_manual_program_agc_registers() {
        let mut bus = MockBus::responsive();
        let mut tuner = E4000::new();
        tuner.set_gain_mode(&mut bus, GainMode::Auto).unwrap();
        assert_eq!(
            bus.regs[usize::from(REG_AGC_MODE)] & AGC_MODE_MASK,
            AGC_MODE_LNA_AUTO
        );
        assert_eq!(bus.regs[usize::from(REG_MIXER_AGC)] & 0x01, 0x01);
        tuner.set_gain_mode(&mut bus, GainMode::Manual).unwrap();
        assert_eq!(bus.regs[usize::from(REG_AGC_MODE)] & AGC_MODE_MASK, 0x00);
        assert_eq!(bus.regs[usize::from(REG_MIXER_AGC)] & 0x01, 0x00);
    }

    /// Masked writes must preserve unrelated bits in the same register —
    /// the reason this driver reads back instead of assuming defaults.
    #[test]
    fn masked_writes_preserve_neighbouring_bits() {
        let mut bus = MockBus::responsive();
        bus.regs[usize::from(REG_LNA_GAIN)] = 0xf0; // undocumented high bits set
        write_masked(&mut bus, REG_LNA_GAIN, 0x0f, 0x06).unwrap();
        assert_eq!(bus.regs[usize::from(REG_LNA_GAIN)], 0xf6);
        // And an already-satisfied field emits no write at all.
        let before = bus.writes.len();
        write_masked(&mut bus, REG_LNA_GAIN, 0x0f, 0x06).unwrap();
        assert_eq!(bus.writes.len(), before);
    }

    #[test]
    fn kind_and_contract_constants() {
        let tuner = E4000::new();
        assert_eq!(tuner.kind(), TunerKind::E4000);
        assert_eq!(E4000_I2C_ADDR, 0xc8);
        assert_eq!(E4000_CHECK_REG, 0x02);
        assert_eq!(E4000_CHECK_VAL, 0x40);
        assert_eq!(E4000_MIN_HZ, 52_000_000);
        assert_eq!(E4000_MAX_HZ, 2_200_000_000);
    }
}
