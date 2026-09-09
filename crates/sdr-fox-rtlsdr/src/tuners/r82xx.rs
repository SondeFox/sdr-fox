//! Rafael Micro R820T / R820T2 / R828D unified tuner driver.
//!
//! Implements the [`Tuner`] trait against a [`TunerBus`] (the RTL2832 I2C
//! repeater in production, a mock in tests). The register programming sequence
//! is a chip interface fact, re-derived from the chip datasheet and from the
//! register-level behaviour established publicly by the osmocom `tuner_r82xx`
//! reference driver.
//!
//! ## PLL math
//!
//! The R82xx PLL synthesizes the LO from a reference clock (28.8 MHz on most
//! RTL-SDR dongles; 16 MHz on R828D) via:
//!
//! ```text
//! vco_freq = (freq + if_freq) * 2           // the VCO runs at 2× the target
//! integer + fractional divider → registers 0x00..0x05
//! ```

use sdr_fox_core::{GainMode, GainRequest, GainStep, Tuner, TunerBus, TunerError, TunerKind};

/// R82xx IF frequency (3.57 MHz) — the standard value used by osmocom.
pub const R82XX_IF_HZ: u32 = 3_570_000;
/// Default reference clock for R820T/T2 dongles.
pub const R82XX_XTAL_R820T_HZ: u32 = 28_800_000;
/// Default reference clock for R828D dongles.
pub const R828D_XTAL_HZ: u32 = 16_000_000;

/// The static gain table for R820T2 (tenths of dB) — the discrete overall
/// gain steps this tuner exposes. The values are chip characterisation data:
/// they match `r82xx_gains[]` in the Osmocom reference's `librtlsdr.c`, the
/// published measurement of the chip's gain steps, because both describe the
/// same hardware.
pub const R820T2_GAIN_TABLE_TENTHS_DB: &[GainStep] = &[
    GainStep::new("OVERALL", 0),
    GainStep::new("OVERALL", 9),
    GainStep::new("OVERALL", 14),
    GainStep::new("OVERALL", 27),
    GainStep::new("OVERALL", 37),
    GainStep::new("OVERALL", 77),
    GainStep::new("OVERALL", 87),
    GainStep::new("OVERALL", 125),
    GainStep::new("OVERALL", 144),
    GainStep::new("OVERALL", 157),
    GainStep::new("OVERALL", 166),
    GainStep::new("OVERALL", 197),
    GainStep::new("OVERALL", 207),
    GainStep::new("OVERALL", 229),
    GainStep::new("OVERALL", 254),
    GainStep::new("OVERALL", 280),
    GainStep::new("OVERALL", 297),
    GainStep::new("OVERALL", 328),
    GainStep::new("OVERALL", 338),
    GainStep::new("OVERALL", 364),
    GainStep::new("OVERALL", 372),
    GainStep::new("OVERALL", 386),
    GainStep::new("OVERALL", 402),
    GainStep::new("OVERALL", 421),
    GainStep::new("OVERALL", 434),
    GainStep::new("OVERALL", 439),
    GainStep::new("OVERALL", 445),
    GainStep::new("OVERALL", 480),
    GainStep::new("OVERALL", 496),
];

/// First register covered by the init array and shadow writes (0x05):
/// R82xx registers 0x00-0x04 are read-only status registers. This is the
/// same boundary the Osmocom reference driver calls `REG_SHADOW_START`.
const REG_SHADOW_START: u8 = 0x05;

/// Number of shadow-tracked registers (R82xx registers 0x00-0x1f).
const NUM_REGS: usize = 32;

/// Bandwidth contribution of each selectable low-pass filter tap (Hz),
/// indexed by the tap chosen in [`R82xx::set_bandwidth`]. The widths are
/// chip filter facts; they match the Osmocom reference's
/// `r82xx_if_low_pass_bw_table[]` because both describe the same filter
/// hardware, not because code was copied.
const IF_LOW_PASS_BW_TABLE: [u32; 10] = [
    1_700_000, 1_600_000, 1_550_000, 1_450_000, 1_200_000, 900_000, 700_000, 550_000, 450_000,
    350_000,
];

/// High-pass filter bandwidth contributions (Hz) — chip filter facts, the
/// same values the Osmocom reference names `FILT_HP_BW1` / `FILT_HP_BW2`.
const FILT_HP_BW1: u32 = 350_000;
const FILT_HP_BW2: u32 = 380_000;

/// R82xx VCO range. The upper edge is exclusive, matching the reference
/// driver's range test.
const VCO_MIN_HZ: u64 = 1_770_000_000;
const VCO_MAX_HZ: u64 = VCO_MIN_HZ * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PllPlan {
    mix_div: u8,
    div_num: u8,
    vco_freq: u64,
    nint: u32,
    sdm: u16,
}

/// Compute every PLL-derived value once so the public diagnostic helper and
/// the hardware programming path cannot drift apart.
fn pll_plan(lo_hz: u64, xtal_hz: u32) -> Result<PllPlan, TunerError> {
    let fail = || TunerError::PllNotLocked { freq_hz: lo_hz };
    if xtal_hz == 0 {
        return Err(fail());
    }

    // The reference picks the output divider using the LO rounded to the
    // nearest kHz, but computes the fractional divider from the exact LO.
    let rounded_khz = lo_hz.checked_add(500).ok_or_else(fail)? / 1_000;
    let mut mix_div = 2u8;
    while mix_div <= 64 {
        let vco_khz = rounded_khz
            .checked_mul(u64::from(mix_div))
            .ok_or_else(fail)?;
        if (VCO_MIN_HZ / 1_000..VCO_MAX_HZ / 1_000).contains(&vco_khz) {
            let vco_freq = lo_hz.checked_mul(u64::from(mix_div)).ok_or_else(fail)?;
            let pll_ref = u64::from(xtal_hz);
            let scaled = 65_536u64.checked_mul(vco_freq).ok_or_else(fail)?;
            let vco_div = pll_ref.checked_add(scaled).ok_or_else(fail)? / (2 * pll_ref);
            let nint = u32::try_from(vco_div / 65_536).map_err(|_| fail())?;
            let sdm = u16::try_from(vco_div % 65_536).map_err(|_| fail())?;
            return Ok(PllPlan {
                mix_div,
                div_num: u8::try_from(mix_div.trailing_zeros() - 1).map_err(|_| fail())?,
                vco_freq,
                nint,
                sdm,
            });
        }
        mix_div = mix_div.checked_mul(2).ok_or_else(fail)?;
    }
    Err(fail())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BandwidthConfig {
    pub(crate) if_hz: u32,
    reg_a: u8,
    reg_b: u8,
}

/// Select the filter registers and the IF at which that filter is centered.
/// Keeping this pure lets the device layer program the RTL2832 DDC to exactly
/// the same IF without exposing tuner internals through the core trait.
#[must_use]
pub(crate) fn bandwidth_config(hz: u32) -> BandwidthConfig {
    if hz > 7_000_000 {
        return BandwidthConfig {
            if_hz: 4_570_000,
            reg_a: 0x10,
            reg_b: 0x0b,
        };
    }
    if hz > 6_000_000 {
        return BandwidthConfig {
            if_hz: 4_570_000,
            reg_a: 0x10,
            reg_b: 0x2a,
        };
    }
    if hz > IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 + FILT_HP_BW2 {
        return BandwidthConfig {
            if_hz: R82XX_IF_HZ,
            reg_a: 0x10,
            reg_b: 0x6b,
        };
    }

    let mut bw_left = hz;
    let mut real_bw = 0u32;
    let mut if_hz = 2_300_000u32;
    let mut reg_b = 0x80u8;
    if bw_left > IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 {
        bw_left = bw_left.saturating_sub(FILT_HP_BW2);
        if_hz += FILT_HP_BW2;
        real_bw += FILT_HP_BW2;
    } else {
        reg_b |= 0x20;
    }
    if bw_left > IF_LOW_PASS_BW_TABLE[0] {
        bw_left = bw_left.saturating_sub(FILT_HP_BW1);
        if_hz += FILT_HP_BW1;
        real_bw += FILT_HP_BW1;
    } else {
        reg_b |= 0x40;
    }

    // Choose the smallest available low-pass width that is still at least
    // the requested remainder. This is the intended form of the reference
    // driver's `--i` search, without its unsigned-underflow footgun below the
    // widest table entry.
    let tap = IF_LOW_PASS_BW_TABLE
        .partition_point(|&candidate| candidate >= bw_left)
        .saturating_sub(1)
        .min(IF_LOW_PASS_BW_TABLE.len() - 1);
    reg_b |= 15u8 - tap as u8;
    real_bw += IF_LOW_PASS_BW_TABLE[tap];
    if_hz = if_hz.saturating_sub(real_bw / 2);

    BandwidthConfig {
        if_hz,
        reg_a: 0x00,
        reg_b,
    }
}

/// Per-step gain deltas (tenths of dB) for the VGA, LNA, and mixer stages.
/// These are measured chip characteristics, not program logic: the values
/// are Osmocom's published hardware measurements of the R820T stages (its
/// `r82xx_vga_gain_steps[]`, `r82xx_lna_gain_steps[]`, and
/// `r82xx_mixer_gain_steps[]` tables, measured with a GSM test set at
/// 928 MHz). The manual gain path walks these tables to map an overall
/// target gain onto discrete LNA/mixer indices.
#[allow(dead_code)] // VGA register held fixed for now; the VGA deltas are kept for future per-stage gain support.
const VGA_GAIN_STEPS: [i32; 16] = [
    0, 26, 26, 30, 42, 35, 24, 13, 14, 32, 36, 34, 35, 37, 35, 36,
];
const LNA_GAIN_STEPS: [i32; 16] = [0, 9, 13, 40, 38, 13, 31, 22, 26, 31, 26, 14, 19, 5, 35, 13];
const MIXER_GAIN_STEPS: [i32; 16] = [0, 5, 10, 10, 19, 9, 10, 25, 17, 10, 8, 16, 13, 6, 3, -8];

/// The 27-byte R820T2 register init array, written to registers 0x05-0x1f.
///
/// These bring-up values configure the LNA, mixer, PLL, filter, and bias
/// sections. The bytes are chip facts: every working driver programs the
/// same image (the Osmocom reference carries it as `r82xx_init_array`), so
/// the values match by necessity, not because code was copied.
#[rustfmt::skip]
const R82XX_INIT_ARRAY: [u8; 27] = [
    0x83, 0x32, 0x75,            // 0x05..=0x07
    0xc0, 0x40, 0xd6, 0x6c,      // 0x08..=0x0b
    0xf5, 0x63, 0x75, 0x68,      // 0x0c..=0x0f
    0x6c, 0x83, 0x80, 0x00,      // 0x10..=0x13
    0x0f, 0x00, 0xc0, 0x30,      // 0x14..=0x17
    0x48, 0xcc, 0x60, 0x00,      // 0x18..=0x1b
    0x54, 0xae, 0x4a, 0xc0,      // 0x1c..=0x1f
];

/// Shadow register file used to merge masked writes during init.
///
/// The R82xx reference driver programs many registers with read-modify-write
/// masks (`r82xx_write_reg_mask`). To reproduce the exact bytes the hardware
/// sees without an I2C read-back, we mirror the chip's register file locally:
/// every masked write merges `val` into the cached byte before flushing it.
struct ShadowRegs {
    regs: [u8; NUM_REGS],
}

impl ShadowRegs {
    /// Build the shadow file by loading the static init array into the
    /// register-address-indexed cache. The init array programs registers
    /// `0x05..=0x1f`, so byte `i` lands at shadow index `0x05 + i`. Indexing
    /// the cache by register address (not by array offset) keeps masked writes
    /// honest: `regs[addr]` for `addr` in `0x00..=0x1f`.
    fn from_init_array() -> Self {
        let mut regs = [0u8; NUM_REGS];
        let start = usize::from(REG_SHADOW_START);
        let end = start + R82XX_INIT_ARRAY.len();
        regs[start..end].copy_from_slice(&R82XX_INIT_ARRAY);
        Self { regs }
    }

    /// Apply `val & mask` to shadow register `reg`, preserving the unmasked
    /// bits, then flush the resulting byte. A no-op when nothing changes.
    ///
    /// The cache is updated *after* the bus write succeeds, so a failed I/O
    /// never leaves the shadow lying about what the hardware actually holds.
    fn write_reg_mask(
        &mut self,
        bus: &mut dyn TunerBus,
        reg: u8,
        val: u8,
        mask: u8,
    ) -> Result<(), TunerError> {
        let idx = usize::from(reg);
        let merged = (self.regs[idx] & !mask) | (val & mask);
        if merged == self.regs[idx] {
            return Ok(());
        }
        bus.i2c_write(reg, &[merged])?;
        self.regs[idx] = merged;
        Ok(())
    }

    /// Write `val` to shadow register `reg` unconditionally (full-byte write,
    /// the osmocom `r82xx_write_reg` shape). The bus call is skipped when the
    /// cached value already matches. Cache updated after the bus write.
    fn write_reg_full(
        &mut self,
        bus: &mut dyn TunerBus,
        reg: u8,
        val: u8,
    ) -> Result<(), TunerError> {
        let idx = usize::from(reg);
        if val == self.regs[idx] {
            return Ok(());
        }
        bus.i2c_write(reg, &[val])?;
        self.regs[idx] = val;
        Ok(())
    }

    /// Copy `bytes` into the shadow file starting at `reg`, flushing the whole
    /// block in a single I2C transaction when anything differs (osmocom's
    /// `r82xx_write` 7-byte PLL block). Cache updated after the bus write.
    fn write_block(
        &mut self,
        bus: &mut dyn TunerBus,
        reg: u8,
        bytes: &[u8],
    ) -> Result<(), TunerError> {
        let start = usize::from(reg);
        let cur = &self.regs[start..start + bytes.len()];
        if cur == bytes {
            return Ok(());
        }
        bus.i2c_write(reg, bytes)?;
        self.regs[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Read the cached byte for `reg` (no bus access — for callers building a
    /// register block image to edit then flush with [`write_block`]).
    fn get(&self, reg: u8) -> u8 {
        self.regs[usize::from(reg)]
    }
}

/// Xtal load-capacitor selector. The chip offers four low-cap settings
/// (30/20/10/0 pF, drive bit set) plus `High0p` (0 pF with the drive bit
/// clear); the Osmocom reference models the same five options as
/// `enum r82xx_xtal_cap_value`. Determines which `xtal_cap_*p` field of the
/// active [`R82xxFreqRange`] gets written to register 0x10 during mux setup.
#[allow(dead_code)] // all five chip settings are modeled; only High0p is selected today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtalCapSel {
    Low30p,
    Low20p,
    Low10p,
    Low0p,
    /// High-capacitance 0pF — the default osmocom `r82xx_init` selects.
    High0p,
}

/// Per-band RF mux / tracking-filter coefficients for one frequency range.
///
/// `freq` is the lower edge of the band, in MHz; the rest are register field
/// values programmed in [`set_mux`]. The per-band data is a chip fact, the
/// same data the Osmocom reference tabulates in its `struct r82xx_freq_range`.
#[derive(Debug, Clone, Copy)]
struct R82xxFreqRange {
    /// Lower band edge, MHz.
    freq: u32,
    /// R23[3] open-drain select (0x00 = high, 0x08 = low).
    open_drain: u8,
    /// R26[7:6] RF mux + R26[1:0] poly mux.
    rf_mux_poly: u8,
    /// R27[7:0] tracking-filter band caps.
    track_filter_cap: u8,
    /// R16[1:0] 20pF xtal cap selection.
    xtal_cap_20p: u8,
    /// R16[1:0] 10pF xtal cap selection.
    xtal_cap_10p: u8,
    /// R16[1:0] 0pF xtal cap selection.
    xtal_cap_0p: u8,
}

/// The R82xx band table, indexed by tuned MHz in [`set_mux`]; the final
/// entry is the catch-all for the highest band. The per-band values are chip
/// facts and therefore match the Osmocom reference's `freq_ranges[]` — the
/// bytes are what the hardware requires, not copied code.
#[rustfmt::skip]
const FREQ_RANGES: &[R82xxFreqRange] = &[
    R82xxFreqRange { freq:   0, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0xdf, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  50, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0xbe, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  55, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0x8b, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  60, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0x7b, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  65, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0x69, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  70, open_drain: 0x08, rf_mux_poly: 0x02, track_filter_cap: 0x58, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  75, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x44, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  80, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x44, xtal_cap_20p: 0x02, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq:  90, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x34, xtal_cap_20p: 0x01, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 100, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x34, xtal_cap_20p: 0x01, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 110, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x24, xtal_cap_20p: 0x01, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 120, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x24, xtal_cap_20p: 0x01, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 140, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x14, xtal_cap_20p: 0x01, xtal_cap_10p: 0x01, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 180, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x13, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 220, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x13, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 250, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x11, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 280, open_drain: 0x00, rf_mux_poly: 0x02, track_filter_cap: 0x00, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 310, open_drain: 0x00, rf_mux_poly: 0x41, track_filter_cap: 0x00, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 450, open_drain: 0x00, rf_mux_poly: 0x41, track_filter_cap: 0x00, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 588, open_drain: 0x00, rf_mux_poly: 0x40, track_filter_cap: 0x00, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
    R82xxFreqRange { freq: 650, open_drain: 0x00, rf_mux_poly: 0x40, track_filter_cap: 0x00, xtal_cap_20p: 0x00, xtal_cap_10p: 0x00, xtal_cap_0p: 0x00 },
];

/// Select the RF input mux and tracking filter for `lo_hz` (the LO, in Hz).
///
/// Picks the active band from [`FREQ_RANGES`] (a binary search here, where
/// the Osmocom reference's `r82xx_set_mux` scans the table linearly) and
/// programs the open-drain, RF mux/poly, tracking-filter band, and xtal
/// cap/drive registers — the chip's band-switch sequence. `lo_hz` is the LO
/// frequency (RF + IF): the band is chosen on the LO, not the RF, exactly as
/// in the reference driver.
fn set_mux(
    bus: &mut dyn TunerBus,
    shadow: &mut ShadowRegs,
    lo_hz: u64,
    xtal_cap_sel: XtalCapSel,
) -> Result<(), TunerError> {
    // `lo_hz` and `lo_mhz` are the same quantity in different units, so the
    // similar-names lint is silenced here.
    #![allow(clippy::similar_names)]
    let lo_mhz = u32::try_from(lo_hz / 1_000_000).unwrap_or(u32::MAX);
    // Select the last lower edge at or below the requested LO. Saturation
    // preserves the first range for values below the table.
    let idx = FREQ_RANGES
        .partition_point(|range| range.freq <= lo_mhz)
        .saturating_sub(1);
    let range = &FREQ_RANGES[idx];

    // Open Drain (R23[3]).
    shadow.write_reg_mask(bus, 0x17, range.open_drain, 0x08)?;
    // RF_MUX, Polymux (R26[7:6] + R26[1:0]).
    shadow.write_reg_mask(bus, 0x1a, range.rf_mux_poly, 0xc3)?;
    // TF BAND (R27[7:0]) — full byte write, like osmocom r82xx_write_reg.
    shadow.write_reg_full(bus, 0x1b, range.track_filter_cap)?;

    // XTAL CAP & Drive (R16[1:0] cap + R16[3] drive). The drive bit (0x08)
    // is set for every cap selector except High0p — the same per-selector
    // adjustment the reference driver applies — so High0p is the only path
    // that leaves the drive bit clear.
    let val = match xtal_cap_sel {
        XtalCapSel::Low30p | XtalCapSel::Low20p => range.xtal_cap_20p | 0x08,
        XtalCapSel::Low10p => range.xtal_cap_10p | 0x08,
        XtalCapSel::High0p => range.xtal_cap_0p,
        XtalCapSel::Low0p => range.xtal_cap_0p | 0x08,
    };
    shadow.write_reg_mask(bus, 0x10, val, 0x0b)?;

    shadow.write_reg_mask(bus, 0x08, 0x00, 0x3f)?;
    shadow.write_reg_mask(bus, 0x09, 0x00, 0x3f)
}

/// Program the R82xx PLL sigma-delta synthesizer for `lo_hz` (the LO, in Hz).
///
/// Computes the integer mixer divider, the 16-bit sigma-delta fractional
/// word, and the `ni`/`si` PLL divider codes, writes registers 0x10..=0x16
/// (the chip's PLL programming sequence, as established publicly by the
/// Osmocom reference's `r82xx_set_pll`), and then **verifies the VCO has
/// locked**. The verification is load-bearing: without it, a marginal dongle
/// tunes "successfully" and then delivers silence, a failure that is
/// frequency-dependent and so reads as a decoder bug rather than a tuner
/// bug.
///
/// Two read-backs drive the sequence (both read the status page at 0x00;
/// `RtlI2cBus::i2c_read` already returns logical, bit-corrected bytes):
///
/// 1. **Before** the fractional registers are flushed, a 5-byte read supplies
///    `vco_fine_tune` (R4[5:4]); the mixer divider `div_num` is nudged ±1
///    toward the chip's `vco_power_ref`.
/// 2. **After** the flush, a 2-iteration loop reads 3 bytes and tests the
///    VCO lock indicator R2[6]. On the first miss the VCO current
///    (R12[7:5]) is lowered to `011`. If R2[6] never asserts, the tune fails
///    loudly with [`TunerError::PllNotLocked`] rather than silently.
fn set_pll(
    bus: &mut dyn TunerBus,
    shadow: &mut ShadowRegs,
    lo_hz: u64,
    xtal_hz: u32,
    kind: TunerKind,
) -> Result<(), TunerError> {
    let pll_fail = || TunerError::PllNotLocked { freq_hz: lo_hz };
    let plan = pll_plan(lo_hz, xtal_hz)?;

    // vco_power_ref: 1 for R828D, 2 otherwise — bounds the integer divider and
    // the vco_fine_tune comparison below.
    let vco_power_ref: u8 = if matches!(kind, TunerKind::R828D) {
        1
    } else {
        2
    };
    if !(13..=u32::from(128u8 / vco_power_ref - 1)).contains(&plan.nint) {
        return Err(pll_fail());
    }

    // PLL autotune = 128 kHz while retuning (clear R26[3:2]).
    shadow.write_reg_mask(bus, 0x1a, 0x00, 0x0c)?;

    // Build the regs[0x10..=0x16] image from the shadow file, edit the PLL
    // fields in place, and flush it as one block — the same read-modify-flush
    // of these seven registers the reference driver performs.
    let mut regs: [u8; 7] = [
        shadow.get(0x10),
        shadow.get(0x11),
        shadow.get(0x12),
        shadow.get(0x13),
        shadow.get(0x14),
        shadow.get(0x15),
        shadow.get(0x16),
    ];

    // refdiv2 = 0 (no reference divide-by-2) — clear R16[4].
    regs[0] = mask_reg8(regs[0], 0x00, 0x10);
    // VCO current = 100 (R12[7:5] = 100).
    regs[2] = mask_reg8(regs[2], 0x80, 0xe0);

    // Pre-PLL read: derive the VCO fine-tune word and nudge the mixer divider.
    // A 5-byte read of the status page (R0..R4) supplies vco_fine_tune at
    // R4[5:4]; the reference driver nudges div_num by ±1 toward vco_power_ref
    // so the VCO lands in a region it can lock. Skipping this leaves the
    // divider at the computed edge and is a common cause of marginal lock.
    let mut div_num = plan.div_num;
    if let Ok(status) = bus.i2c_read(0x00, 5) {
        if let Some(r4) = status.get(4) {
            let vco_fine_tune = (r4 & 0x30) >> 4;
            if vco_fine_tune > vco_power_ref {
                div_num = div_num.saturating_sub(1);
            } else if vco_fine_tune < vco_power_ref {
                div_num = div_num.saturating_add(1).min(7);
            }
        }
    }
    // Write the (possibly nudged) mixer divider into R16[7:5].
    regs[0] = mask_reg8(regs[0], div_num << 5, 0xe0);

    // Encode the integer divider as ni + si*64 (R14[5:0]=ni, R14[7:6]=si).
    let ni = ((plan.nint - 13) / 4) as u8;
    let si = (plan.nint - 4 * u32::from(ni) - 13) as u8;
    regs[4] = ni + (si << 6);

    // pw_sdm: power down the SDM when the fractional part is zero.
    let pw_sdm = if plan.sdm == 0 { 0x08 } else { 0x00 };
    regs[2] = mask_reg8(regs[2], pw_sdm, 0x08);

    // 16-bit sigma-delta word, little-endian across R15/R16.
    let [sdm_lo, sdm_hi] = plan.sdm.to_le_bytes();
    regs[5] = sdm_lo;
    regs[6] = sdm_hi;

    // Flush the 7-byte register block to 0x10..=0x16. The TunerBus chunks
    // writes to ≤7 data bytes itself, matching osmocom max_i2c_msg_len=8.
    shadow.write_block(bus, 0x10, &regs)?;

    // Post-PLL lock verification. Read R0..R2 and test R2[6] (VCO_INDICATOR).
    // Up to two attempts: on the first miss, lower the VCO current to 011
    // (R12[7:5] = 011) and retry; if the second read still shows no lock, the
    // tune failed. A small delay between attempts matches the kernel driver's
    // intent (osmocom userspace relies on the I2C transaction time alone).
    let mut locked = false;
    for attempt in 0..2u8 {
        if let Ok(status) = bus.i2c_read(0x00, 3) {
            if status.get(2).is_some_and(|r2| r2 & 0x40 != 0) {
                locked = true;
                break;
            }
        }
        if attempt == 0 {
            // First miss: lower VCO current R12[7:5] from 100 to 011. Masked
            // write against the live shadow so the next flush preserves the
            // other R12 bits (pw_sdm etc.).
            shadow.write_reg_mask(bus, 0x12, 0x60, 0xe0)?;
        }
    }
    if !locked {
        return Err(pll_fail());
    }

    // PLL autotune = 8 kHz once locked (set R26[3]).
    shadow.write_reg_mask(bus, 0x1a, 0x08, 0x08)
}

/// Merge `val & mask` into `byte`, preserving unmasked bits — the standard
/// masked-merge idiom (the Osmocom reference implements the same one-liner
/// as its `mask_reg8` helper).
#[inline]
fn mask_reg8(byte: u8, val: u8, mask: u8) -> u8 {
    (byte & !mask) | (val & mask)
}

/// R82xx unified driver covering R820T, R820T2, and R828D.
pub struct R82xx {
    kind: TunerKind,
    xtal_hz: u32,
    /// Xtal capacitor selector used during mux setup. osmocom `r82xx_init`
    /// hard-codes [`XtalCapSel::High0p`]; a future xtal-check routine may
    /// override this.
    xtal_cap_sel: XtalCapSel,
    /// Mirror of the chip's register file, kept in lock-step with every
    /// write so masked writes (`write_reg_mask`) merge against the bytes the
    /// hardware actually sees. Persists across calls so `set_freq` sees the
    /// post-init PLL register image.
    shadow: ShadowRegs,
    /// Cache of the last programmed frequency, for tests and diagnostics.
    last_freq_hz: u64,
    /// Current tuner IF. Bandwidth selection moves this so the requested
    /// channel remains centered in the analog filter.
    if_hz: u32,
}

impl R82xx {
    /// Construct for a specific chip variant. Picks the reference clock
    /// appropriately (28.8 MHz for R820T/T2, 16 MHz for R828D).
    #[must_use]
    pub fn new(kind: TunerKind) -> Self {
        let xtal_hz = match kind {
            TunerKind::R828D => R828D_XTAL_HZ,
            _ => R82XX_XTAL_R820T_HZ,
        };
        Self {
            kind,
            xtal_hz,
            xtal_cap_sel: XtalCapSel::High0p,
            shadow: ShadowRegs::from_init_array(),
            last_freq_hz: 0,
            if_hz: R82XX_IF_HZ,
        }
    }

    /// RTL-SDR Blog V4 uses a shared 28.8 MHz oscillator for its R828D
    /// tuner and RTL2832U (manufacturer's published 2023 design description).
    /// Ordinary R828D receivers retain the 16 MHz default in `new`.
    #[must_use]
    pub fn for_blog_v4() -> Self {
        let mut tuner = Self::new(TunerKind::R828D);
        tuner.xtal_hz = R82XX_XTAL_R820T_HZ;
        tuner
    }

    /// The reference clock this instance was constructed with, in Hz.
    #[must_use]
    pub fn xtal_hz(&self) -> u32 {
        self.xtal_hz
    }

    /// The last frequency programmed via [`Tuner::set_freq`].
    #[must_use]
    pub fn last_freq_hz(&self) -> u64 {
        self.last_freq_hz
    }

    /// The analog IF selected by the current bandwidth configuration.
    #[must_use]
    pub fn if_hz(&self) -> u32 {
        self.if_hz
    }

    /// Compute the PLL divider values for a target frequency. Returns
    /// `(div_num, vco_freq)` where `div_num` is the integer divider (a power of
    /// two, 2..=64 — the R82xx VCO divider is power-of-two based) and `vco_freq`
    /// is the VCO target. Public for unit testing the PLL math without a bus.
    ///
    /// This matches osmocom `r82xx_set_pll`'s `mix_div` loop, which only ever
    /// evaluates `mix_div = 2, 4, 8, 16, 32, 64` (each iteration does
    /// `mix_div <<= 1`). Checking non-power-of-two dividers wastes cycles and,
    /// worse, can pick a divider the hardware cannot realize.
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::PllNotLocked`] if the frequency is out of range.
    pub fn compute_pll(target_hz: u64, xtal_hz: u32) -> Result<(u8, u64), TunerError> {
        let lo_hz = target_hz
            .checked_add(u64::from(R82XX_IF_HZ))
            .ok_or(TunerError::PllNotLocked { freq_hz: target_hz })?;
        let plan = pll_plan(lo_hz, xtal_hz)
            .map_err(|_| TunerError::PllNotLocked { freq_hz: target_hz })?;
        Ok((plan.mix_div, plan.vco_freq))
    }
}

impl Tuner for R82xx {
    fn init(&mut self, bus: &mut dyn TunerBus) -> Result<(), TunerError> {
        self.shadow = ShadowRegs::from_init_array();
        bus.i2c_write(REG_SHADOW_START, &R82XX_INIT_ARRAY)?;
        // Write the exact post-init registers from the osmocom trace
        // (verified byte-exact).
        // These configure the filter calibration, LNA, and system frequency.
        let post_init: &[(u8, &[u8])] = &[
            (0x0c, &[0xf0]),
            (0x13, &[0x31]),
            (0x1d, &[0x86]),
            (0x0f, &[0x6c]),
            (0x10, &[0x8c, 0x83, 0x80, 0x31, 0x84, 0x72, 0x1c]),
            (0x1a, &[0x68]),
            (0x0b, &[0x7c]),
            (0x0b, &[0x6c]),
            (0x0f, &[0x68]),
            (0x0b, &[0x6b]),
            (0x06, &[0x12]),
            (0x1e, &[0x6a]),
            (0x05, &[0x03]),
            (0x1f, &[0x40]),
            (0x19, &[0xec]),
            (0x1d, &[0xc5]),
            (0x1c, &[0x24]),
            (0x0d, &[0x53]),
            (0x11, &[0xbb]),
            (0x1c, &[0x20]),
            (0x1a, &[0x78]),
            (0x1d, &[0xdd]),
            (0x1c, &[0x24]),
            (0x1e, &[0x6e]),
            (0x1a, &[0x68]),
        ];
        for (reg, data) in post_init {
            bus.i2c_write(*reg, data)?;
            // Update the shadow to match what we sent to the bus.
            for (i, &byte) in data.iter().enumerate() {
                let r = reg.wrapping_add(i as u8) as usize;
                if r < self.shadow.regs.len() {
                    self.shadow.regs[r] = byte;
                }
            }
        }
        Ok(())
    }

    fn set_freq(&mut self, bus: &mut dyn TunerBus, hz: u64) -> Result<(), TunerError> {
        // PLL synthesizes LO = RF + IF.
        let lo_hz = hz
            .checked_add(u64::from(self.if_hz))
            .ok_or(TunerError::PllNotLocked { freq_hz: hz })?;
        set_mux(bus, &mut self.shadow, lo_hz, self.xtal_cap_sel)?;
        set_pll(bus, &mut self.shadow, lo_hz, self.xtal_hz, self.kind)?;
        self.last_freq_hz = hz;
        Ok(())
    }

    fn set_bandwidth(&mut self, bus: &mut dyn TunerBus, hz: u32) -> Result<(), TunerError> {
        let config = bandwidth_config(hz);
        self.shadow.write_reg_mask(bus, 0x0a, config.reg_a, 0x10)?;
        self.shadow.write_reg_mask(bus, 0x0b, config.reg_b, 0xef)?;
        // Publish the new IF only after both hardware writes succeeded.
        self.if_hz = config.if_hz;
        Ok(())
    }

    fn set_gain(&mut self, bus: &mut dyn TunerBus, req: GainRequest) -> Result<(), TunerError> {
        let tenths = match req {
            GainRequest::Overall(t) => t,
            GainRequest::PerStage { .. } => {
                return Err(TunerError::InvalidGain);
            }
        };
        // Follows the same manual-gain procedure the Osmocom reference uses
        // (`r82xx_set_gain`, manual branch), so a requested overall gain maps
        // onto the same LNA/mixer settings: VGA is held at a fixed gain
        // (16.3 dB → 0x08 in R12[4:0]); the LNA and mixer indices are walked
        // up the per-stage tables until the accumulated gain reaches the
        // requested target.
        //
        // LNA auto off (R5[4]).
        self.shadow.write_reg_mask(bus, 0x05, 0x10, 0x10)?;
        // Mixer auto off (R7[4]).
        self.shadow.write_reg_mask(bus, 0x07, 0x00, 0x10)?;
        // Fixed VGA gain for now (16.3 dB) — R12[4:0] = 0x08, mask 0x9f.
        self.shadow.write_reg_mask(bus, 0x0c, 0x08, 0x9f)?;

        // Match the reference driver's accounting exactly: the fixed VGA
        // register is programmed above, but the VGA's measured -4.7 dB offset
        // (osmocom's `VGA_BASE_GAIN`) is not part of the target accumulator
        // used to select the LNA and mixer indices.
        let mut total_gain = 0i32;
        let mut lna_index: u8 = 0;
        let mut mix_index: u8 = 0;
        for _ in 0..15 {
            if total_gain >= tenths {
                break;
            }
            lna_index = lna_index.saturating_add(1);
            if (lna_index as usize) < LNA_GAIN_STEPS.len() {
                total_gain += LNA_GAIN_STEPS[lna_index as usize];
            }
            if total_gain >= tenths {
                break;
            }
            mix_index = mix_index.saturating_add(1);
            if (mix_index as usize) < MIXER_GAIN_STEPS.len() {
                total_gain += MIXER_GAIN_STEPS[mix_index as usize];
            }
        }

        // Set LNA gain (R5[3:0]).
        self.shadow
            .write_reg_mask(bus, 0x05, lna_index & 0x0f, 0x0f)?;
        // Set mixer gain (R7[3:0]).
        self.shadow
            .write_reg_mask(bus, 0x07, mix_index & 0x0f, 0x0f)
    }

    fn gains(&self) -> &[GainStep] {
        R820T2_GAIN_TABLE_TENTHS_DB
    }

    fn set_gain_mode(&mut self, bus: &mut dyn TunerBus, mode: GainMode) -> Result<(), TunerError> {
        // Osmocom controls the LNA and mixer auto/manual selectors directly;
        // register 0x1d holds LNA-top/predetect thresholds, not AGC enables.
        let (lna_manual, mixer_auto, vga) = match mode {
            GainMode::Auto => (0x00, 0x10, 0x0b),
            GainMode::Manual => (0x10, 0x00, 0x08),
        };
        self.shadow.write_reg_mask(bus, 0x05, lna_manual, 0x10)?;
        self.shadow.write_reg_mask(bus, 0x07, mixer_auto, 0x10)?;
        self.shadow.write_reg_mask(bus, 0x0c, vga, 0x9f)
    }

    fn kind(&self) -> TunerKind {
        self.kind
    }
}

impl R82xx {
    /// Find the index of the gain table entry closest to `tenths_db`.
    #[must_use]
    pub fn closest_gain_index(tenths_db: i32) -> usize {
        R820T2_GAIN_TABLE_TENTHS_DB
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (i64::from(s.tenths_db) - i64::from(tenths_db)).abs())
            .map_or(0, |(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording mock tuner bus for unit-testing register programming
    /// without an RTL2832 transport.
    struct MockBus {
        writes: Vec<(u8, Vec<u8>)>,
    }
    impl MockBus {
        fn new() -> Self {
            Self { writes: vec![] }
        }
    }
    impl TunerBus for MockBus {
        fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError> {
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
        fn i2c_read(&mut self, _reg: u8, len: usize) -> Result<Vec<u8>, TunerError> {
            // Model a locked, centered VCO: R2[6] (VCO_INDICATOR) set, and
            // R4[5:4] (vco_fine_tune) at the R820T2 vco_power_ref (2) so
            // set_pll's div_num nudge is a no-op. This keeps the happy-path
            // register-programming tests byte-exact while exercising the new
            // lock-verification read path.
            let mut buf = vec![0u8; len];
            if len > 2 {
                buf[2] = 0x40; // R2[6] = locked
            }
            if len > 4 {
                buf[4] = 0x20; // R4[5:4] = 2 (== vco_power_ref, no nudge)
            }
            Ok(buf)
        }
    }

    struct FailOnceBus {
        attempts: Vec<(u8, Vec<u8>)>,
        fail_next: bool,
    }

    impl TunerBus for FailOnceBus {
        fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError> {
            self.attempts.push((reg, data.to_vec()));
            if std::mem::take(&mut self.fail_next) {
                return Err(TunerError::I2cTransferFailed { addr: reg });
            }
            Ok(())
        }

        fn i2c_read(&mut self, _reg: u8, _len: usize) -> Result<Vec<u8>, TunerError> {
            Ok(vec![0])
        }
    }

    #[test]
    fn failed_masked_write_does_not_advance_shadow_or_suppress_retry() {
        let mut shadow = ShadowRegs::from_init_array();
        let original = shadow.get(0x1d);
        let expected = (original & !0x03) | 0x03;
        let mut bus = FailOnceBus {
            attempts: Vec::new(),
            fail_next: true,
        };

        assert!(shadow.write_reg_mask(&mut bus, 0x1d, 0x03, 0x03).is_err());
        assert_eq!(shadow.get(0x1d), original, "failed write changed shadow");

        shadow.write_reg_mask(&mut bus, 0x1d, 0x03, 0x03).unwrap();
        assert_eq!(bus.attempts, vec![(0x1d, vec![expected]); 2]);
        assert_eq!(shadow.get(0x1d), expected);
    }

    #[test]
    fn blog_v4_uses_shared_28m8_reference_without_changing_generic_r828d() {
        assert_eq!(R82xx::for_blog_v4().xtal_hz(), 28_800_000);
        assert_eq!(R82xx::new(TunerKind::R828D).xtal_hz(), 16_000_000);
    }

    #[test]
    fn pll_divider_for_fm_broadcast_is_in_range() {
        // 100 MHz FM broadcast: vco = (100e6 + 3.57e6) * mix_div; needs
        // a divider to reach the VCO range [1.77, 3.54] GHz.
        let (div, vco) = R82xx::compute_pll(100_000_000, R82XX_XTAL_R820T_HZ).unwrap();
        assert!((1_770_000_000..=3_540_000_000).contains(&vco), "vco={vco}");
        assert!(div <= 64, "div={div}");
    }

    #[test]
    fn pll_divider_for_2m_ham_band() {
        // 144 MHz.
        let (_div, vco) = R82xx::compute_pll(144_000_000, R82XX_XTAL_R820T_HZ).unwrap();
        assert!((1_770_000_000..=3_540_000_000).contains(&vco));
    }

    #[test]
    fn pll_divider_for_70cm_band() {
        // 440 MHz.
        let (_div, vco) = R82xx::compute_pll(440_000_000, R82XX_XTAL_R820T_HZ).unwrap();
        assert!((1_770_000_000..=3_540_000_000).contains(&vco));
    }

    #[test]
    fn pll_rejects_impossible_frequency() {
        // 0 Hz.
        assert!(R82xx::compute_pll(0, R82XX_XTAL_R820T_HZ).is_err());
    }

    #[test]
    fn pll_vco_range_has_inclusive_lower_and_exclusive_upper_edges() {
        let lower = pll_plan(885_000_000, R82XX_XTAL_R820T_HZ).unwrap();
        assert_eq!(lower.mix_div, 2);
        assert_eq!(lower.vco_freq, VCO_MIN_HZ);
        assert!(pll_plan(1_770_000_000, R82XX_XTAL_R820T_HZ).is_err());
    }

    #[test]
    fn pll_rejects_zero_reference_clock_without_writing() {
        let mut bus = MockBus::new();
        let mut shadow = ShadowRegs::from_init_array();
        assert!(set_pll(&mut bus, &mut shadow, 100_000_000, 0, TunerKind::R820T2).is_err());
        assert!(bus.writes.is_empty());
    }

    #[test]
    fn r828d_uses_16mhz_xtal() {
        let t = R82xx::new(TunerKind::R828D);
        assert_eq!(t.xtal_hz(), R828D_XTAL_HZ);
    }

    #[test]
    fn r820t2_uses_28_8mhz_xtal() {
        let t = R82xx::new(TunerKind::R820T2);
        assert_eq!(t.xtal_hz(), R82XX_XTAL_R820T_HZ);
    }

    #[test]
    fn init_writes_power_on_sequence() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.init(&mut bus).unwrap();
        // The very first write is the full 27-byte init array (registers
        // 0x05..=0x1f), followed by the filter-calibration and sysfreq
        // register writes. So there is more than the old 3 writes, and the
        // first write carries the init array verbatim.
        assert!(
            bus.writes.len() > 3,
            "init must write the full init array plus calibration regs, got {}",
            bus.writes.len()
        );
        assert_eq!(bus.writes[0].0, 0x05);
        assert_eq!(bus.writes[0].1, R82XX_INIT_ARRAY);
        // The init array must be exactly 27 bytes (0x05..=0x1f).
        assert_eq!(R82XX_INIT_ARRAY.len(), 27);
    }

    #[test]
    fn set_freq_records_last_frequency() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_freq(&mut bus, 162_400_000).unwrap();
        assert_eq!(tuner.last_freq_hz(), 162_400_000);
        // set_freq must program the full PLL block at 0x10..=0x16 (7 bytes),
        // not just the integer divider at 0x0d as v1 did.
        let pll_write = bus
            .writes
            .iter()
            .find(|(reg, data)| *reg == 0x10 && data.len() == 7);
        assert!(
            pll_write.is_some(),
            "set_freq must write the 7-byte PLL block at 0x10"
        );
        // And the mux path must touch the tracking-filter band register 0x1b.
        assert!(
            bus.writes.iter().any(|(reg, _)| *reg == 0x1b),
            "set_freq must program the RF mux (0x1b)"
        );
        // The old v1 behaviour — a lone 2-bit write to 0x0d — must be gone.
        assert!(
            !bus.writes.iter().any(|(reg, _)| *reg == 0x0d),
            "set_freq must not write the legacy integer-only divider at 0x0d"
        );
    }

    /// PLL register block matches the osmocom reference math byte-for-byte.
    ///
    /// For LO = 162.4 MHz + 3.57 MHz IF = 165.97 MHz, R820T2 (xtal = 28.8 MHz):
    /// the VCO divider is 16 (`div_num` = 3), giving VCO ≈ 2.6555 `GHz`, and the
    /// sigma-delta word encodes the fractional remainder. This test pins the
    /// exact bytes so a regression in any bit position is caught.
    #[test]
    fn set_freq_pll_block_matches_osmocom_math() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.init(&mut bus).unwrap();
        bus.writes.clear();
        tuner.set_freq(&mut bus, 162_400_000).unwrap();

        let (_, pll) = bus
            .writes
            .iter()
            .find(|(reg, data)| *reg == 0x10 && data.len() == 7)
            .expect("PLL block write must be present")
            .clone();

        // Recompute the expected register image from the osmocom formulas.
        let lo_hz = 162_400_000u64 + u64::from(R82XX_IF_HZ);
        let mix_div: u64 = 16; // smallest power-of-two ≥2 putting VCO in range
        let vco_freq = lo_hz * mix_div;
        let pll_ref = u64::from(R82XX_XTAL_R820T_HZ);
        let vco_div = (pll_ref + 65536 * vco_freq) / (2 * pll_ref);
        let nint = u32::try_from(vco_div / 65536).unwrap();
        let sdm = u32::try_from(vco_div % 65536).unwrap();
        let div_num: u8 = 3; // log2(16) - 1
        let ni = ((nint - 13) / 4) as u8;
        let si = (nint - 4 * u32::from(ni) - 13) as u8;

        // R16: div_num in [7:5], refdiv2=0 in [4], xtal cap from mux in [3:0].
        // The mux wrote val = xtal_cap_0p (0x00 for High0p) | 0x00 into [3:0]
        // via mask 0x0b, so the low nibble here is 0.
        assert_eq!(pll[0] & 0xe0, div_num << 5, "R16 div_num bits");
        assert_eq!(pll[0] & 0x10, 0x00, "R16 refdiv2 clear");
        // R12 (pll[2]): VCO current = 100 → 0x80 in [7:5]; pw_sdm in [3].
        assert_eq!(pll[2] & 0xe0, 0x80, "R12 VCO current");
        let expected_pw_sdm = if sdm == 0 { 0x08 } else { 0x00 };
        assert_eq!(pll[2] & 0x08, expected_pw_sdm, "R12 pw_sdm");
        // R14 (pll[4]): ni in [5:0], si in [7:6]. Fine-tune read-back may
        // adjust only `div_num`; the reference never subtracts from `nint`.
        assert_eq!(pll[4], ni | (si << 6), "R14 ni/si encoding");
        assert_eq!(pll[5], (sdm & 0xff) as u8, "R15 sdm low byte");
        assert_eq!(pll[6], (sdm >> 8) as u8, "R16 sdm high byte");

        // nint must be in range for the (128 / vco_power_ref - 1) bound.
        assert!(nint <= 63, "nint out of range: {nint}");
    }

    /// `set_mux` picks the highest band for a UHF tune.
    #[test]
    fn set_freq_mux_picks_correct_band() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.init(&mut bus).unwrap();
        bus.writes.clear();
        // 850 MHz → LO ≈ 853.57 MHz → band starting at 650 MHz (rf_mux_poly 0x40).
        tuner.set_freq(&mut bus, 850_000_000).unwrap();
        // R26 (0x1a) low bits should carry rf_mux_poly = 0x40 (mask 0xc3).
        let mux_write = bus
            .writes
            .iter()
            .find(|(reg, _)| *reg == 0x1a)
            .expect("0x1a mux write must be present");
        assert_eq!(mux_write.1[0] & 0xc3, 0x40, "UHF rf_mux_poly");
    }

    #[test]
    fn set_gain_mode_auto_restores_lna_and_mixer_auto() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_gain(&mut bus, GainRequest::overall(200)).unwrap();
        bus.writes.clear();
        tuner.set_gain_mode(&mut bus, GainMode::Auto).unwrap();
        assert_eq!(tuner.shadow.get(0x05) & 0x10, 0x00, "LNA auto on");
        assert_eq!(tuner.shadow.get(0x07) & 0x10, 0x10, "mixer auto on");
        assert_eq!(tuner.shadow.get(0x0c) & 0x9f, 0x0b, "automatic VGA");
        assert_eq!(
            bus.writes
                .iter()
                .map(|(register, _)| *register)
                .collect::<Vec<_>>(),
            [0x05, 0x07, 0x0c]
        );
    }

    #[test]
    fn set_gain_mode_manual_forces_manual_selectors() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.shadow.regs[0x05] &= !0x10;
        tuner.shadow.regs[0x07] |= 0x10;
        tuner.set_gain_mode(&mut bus, GainMode::Manual).unwrap();
        assert_eq!(tuner.shadow.get(0x05) & 0x10, 0x10, "LNA manual");
        assert_eq!(tuner.shadow.get(0x07) & 0x10, 0x00, "mixer manual");
        assert_eq!(tuner.shadow.get(0x0c) & 0x9f, 0x08, "manual VGA");
    }

    #[test]
    fn set_bandwidth_8mhz_uses_fixed_registers() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_bandwidth(&mut bus, 8_000_000).unwrap();
        // 0x0b encodes the BW tap (mask 0xef). For 8 MHz osmocom writes 0x0b.
        // 0x0a bit 4 may already match the init value, so it can be a no-op;
        // the 0x0b write is the load-bearing indicator.
        let w0b = bus
            .writes
            .iter()
            .find(|(r, _)| *r == 0x0b)
            .expect("0x0b write");
        assert_eq!(w0b.1[0] & 0xef, 0x0b);
        // The 0x0a bit-4 value must be reflected in the shadow regardless of
        // whether a bus write was needed.
        assert_eq!(tuner.shadow.get(0x0a) & 0x10, 0x10);
        assert_eq!(tuner.if_hz(), 4_570_000);
    }

    #[test]
    fn set_bandwidth_6mhz_uses_0x6b() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_bandwidth(&mut bus, 6_000_000).unwrap();
        let w0b = bus
            .writes
            .iter()
            .find(|(r, _)| *r == 0x0b)
            .expect("0x0b write");
        assert_eq!(w0b.1[0] & 0xef, 0x6b);
        assert_eq!(tuner.if_hz(), R82XX_IF_HZ);
    }

    #[test]
    fn set_bandwidth_narrow_walks_low_pass_table() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        // 2 MHz: below the 6 MHz shortcut; lands in the low-pass table walk.
        tuner.set_bandwidth(&mut bus, 2_000_000).unwrap();
        let w0b = bus
            .writes
            .iter()
            .find(|(r, _)| *r == 0x0b)
            .expect("0x0b write");
        // Narrow path sets bit 7 (0x80); the low nibble encodes the tap.
        assert_eq!(w0b.1[0] & 0x80, 0x80);
        assert_eq!(tuner.if_hz(), 1_625_000);
    }

    /// The narrowest rate the RTL2832 supports (225–300 kS/s) must select the
    /// bottom of the low-pass table and drop the IF to 2.125 MHz.
    ///
    /// This is the radiosonde configuration — 250 kS/s at 400 MHz — and it is
    /// the case that regressed when a host forgot to call `set_bandwidth` at
    /// all: the tuner then stays on the DVB-T filter (~6 MHz at IF 3.57 MHz)
    /// and its AGC detector integrates ~24x more spectrum than the host is
    /// decimating, backing the front end off by roughly 23 dB. Both high-pass
    /// sections are bypassed (0x20|0x40) and the tap is the table's last entry
    /// (350 kHz), giving `15 - 9 = 6` in the low nibble.
    #[test]
    fn set_bandwidth_250khz_selects_the_narrowest_tap_and_lowest_if() {
        let cfg = bandwidth_config(250_000);
        assert_eq!(cfg.if_hz, 2_125_000, "IF must drop from 3.57 MHz");
        assert_eq!(cfg.reg_a, 0x00);
        assert_eq!(
            cfg.reg_b, 0xe6,
            "0x80 narrow | 0x20 bypass HP2 | 0x40 bypass HP1 | tap 6",
        );

        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_bandwidth(&mut bus, 250_000).unwrap();
        assert_eq!(tuner.if_hz(), 2_125_000);
    }

    #[test]
    fn narrowband_pll_failure_reports_local_oscillator_not_requested_rf() {
        let mut bus = ScriptedReadBus::new(vec![]);
        let mut tuner = R82xx::for_blog_v4();
        tuner.set_bandwidth(&mut bus, 250_000).unwrap();
        // Empty scripted reads never assert the PLL lock bit. This independently
        // reproduces the reported RF/LO relation without requiring RF input.
        let error = tuner.set_freq(&mut bus, 401_500_000).unwrap_err();
        assert!(matches!(
            error,
            TunerError::PllNotLocked {
                freq_hz: 403_625_000
            }
        ));
    }

    #[test]
    fn bandwidth_boundaries_are_total_and_do_not_underflow() {
        let narrowest = bandwidth_config(0);
        assert_eq!(narrowest.reg_b & 0x0f, 6);
        assert_eq!(narrowest.if_hz, 2_125_000);

        assert_eq!(bandwidth_config(2_430_000).if_hz, 1_815_000);
        assert_eq!(bandwidth_config(2_430_001).if_hz, R82XX_IF_HZ);
        assert_eq!(bandwidth_config(6_000_001).if_hz, 4_570_000);
        assert_eq!(bandwidth_config(7_000_001).reg_b, 0x0b);
    }

    #[test]
    fn failed_bandwidth_write_does_not_publish_new_if() {
        let mut tuner = R82xx::new(TunerKind::R820T2);
        let mut bus = FailOnceBus {
            attempts: Vec::new(),
            fail_next: true,
        };
        // R0a bit 4 already matches the 8 MHz config, so the first attempted
        // bus transaction is R0b and fails.
        assert!(tuner.set_bandwidth(&mut bus, 8_000_000).is_err());
        assert_eq!(tuner.if_hz(), R82XX_IF_HZ);
    }

    #[test]
    fn set_gain_overall_programs_lna_and_mixer() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_gain(&mut bus, GainRequest::overall(200)).unwrap();
        // LNA auto off (0x05 mask 0x10), mixer auto off (0x07 mask 0x10),
        // VGA fixed (0x0c), then LNA index (0x05 mask 0x0f) and mixer index
        // (0x07 mask 0x0f). All five register addresses must be touched.
        for addr in [0x05u8, 0x07, 0x0c] {
            assert!(
                bus.writes.iter().any(|(r, _)| *r == addr),
                "set_gain must write register 0x{addr:02x}"
            );
        }
        // The first 0x05 write is the LNA-auto-off (mask 0x10); a later 0x05
        // write carries the LNA index in the low nibble (mask 0x0f). The high
        // nibble holds preserved bits, so only the low nibble must be a valid
        // index.
        let lna_idx_write = bus
            .writes
            .iter()
            .filter(|(r, _)| *r == 0x05)
            .nth(1)
            .expect("two 0x05 writes (auto-off + index)");
        assert_eq!(
            lna_idx_write.1[0] & 0xf0,
            0x90,
            "masked index write must preserve the configured high nibble"
        );
        assert_eq!(tuner.shadow.get(0x05) & 0x0f, 6, "200: LNA index");
        assert_eq!(tuner.shadow.get(0x07) & 0x0f, 6, "200: mixer index");
        assert_eq!(tuner.shadow.get(0x0c) & 0x9f, 0x08, "fixed VGA");
        assert_eq!(
            lna_idx_write.1[0] & 0x0f,
            tuner.shadow.get(0x05) & 0x0f,
            "bus byte and committed shadow must agree"
        );
    }

    #[test]
    fn gains_table_is_monotonic_non_decreasing() {
        let table = R820T2_GAIN_TABLE_TENTHS_DB;
        for w in table.windows(2) {
            assert!(
                w[0].tenths_db <= w[1].tenths_db,
                "gain table not monotonic at {} -> {}",
                w[0].tenths_db,
                w[1].tenths_db
            );
        }
    }

    #[test]
    fn closest_gain_index_snaps_correctly() {
        // Table starts 0, 9, 14, 27, 37, 77, ...
        assert_eq!(R82xx::closest_gain_index(0), 0);
        assert_eq!(R82xx::closest_gain_index(10), 1); // closest to 9
        assert_eq!(R82xx::closest_gain_index(30), 3); // closest to 27
        assert_eq!(
            R82xx::closest_gain_index(1000),
            R820T2_GAIN_TABLE_TENTHS_DB.len() - 1
        );
    }

    #[test]
    fn set_gain_overall_matches_osmocom_step_walk() {
        for (gain, expected_lna, expected_mixer) in [(0, 0, 0), (200, 6, 6), (496, 15, 14)] {
            let mut bus = MockBus::new();
            let mut tuner = R82xx::new(TunerKind::R820T2);
            tuner
                .set_gain(&mut bus, GainRequest::overall(gain))
                .unwrap();
            assert_eq!(
                tuner.shadow.get(0x05) & 0x0f,
                expected_lna,
                "gain {gain}: LNA index"
            );
            assert_eq!(
                tuner.shadow.get(0x07) & 0x0f,
                expected_mixer,
                "gain {gain}: mixer index"
            );
        }
    }

    #[test]
    fn set_gain_per_stage_rejected() {
        let mut bus = MockBus::new();
        let mut tuner = R82xx::new(TunerKind::R820T2);
        assert!(tuner
            .set_gain(
                &mut bus,
                GainRequest::PerStage {
                    name: "LNA",
                    tenths_db: 100
                }
            )
            .is_err());
    }

    /// A bus whose reads are scripted per call, so the PLL lock loop can be
    /// exercised against a tuner that never locks, locks on the second read,
    /// or reports a specific VCO fine-tune.
    struct ScriptedReadBus {
        reads: std::collections::VecDeque<Vec<u8>>,
        writes: Vec<(u8, Vec<u8>)>,
    }
    impl ScriptedReadBus {
        fn new(reads: Vec<Vec<u8>>) -> Self {
            Self {
                reads: reads.into(),
                writes: Vec::new(),
            }
        }
    }
    impl TunerBus for ScriptedReadBus {
        fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError> {
            self.writes.push((reg, data.to_vec()));
            Ok(())
        }
        fn i2c_read(&mut self, _reg: u8, len: usize) -> Result<Vec<u8>, TunerError> {
            // The lock path always reads 3 or 5 bytes from page 0x00. Hand
            // back the next scripted reply, sized to the request.
            let mut reply = self.reads.pop_front().unwrap_or_else(|| vec![0u8; len]);
            reply.resize(len, 0);
            Ok(reply)
        }
    }

    /// R2[6] is the `VCO_INDICATOR` lock bit (0x40 after bit-reversal at the bus
    /// boundary). A tuner that never asserts it must fail the tune loudly with
    /// [`TunerError::PllNotLocked`], not silently deliver silence.
    #[test]
    fn set_freq_fails_loudly_when_pll_never_locks() {
        // Two 3-byte reads both return R2 with no lock bit (0x00). The 5-byte
        // pre-read returns zeros (no nudge).
        let mut bus = ScriptedReadBus::new(vec![vec![0; 5], vec![0; 3], vec![0; 3]]);
        let mut tuner = R82xx::new(TunerKind::R820T2);
        let err = tuner
            .set_freq(&mut bus, 162_400_000)
            .expect_err("no lock must surface as an error");
        // The PLL error is reported at the LO frequency (RF + IF), which is
        // what the synthesizer was asked to lock.
        let lo = 162_400_000 + u64::from(R82XX_IF_HZ);
        assert!(
            matches!(err, TunerError::PllNotLocked { freq_hz } if freq_hz == lo),
            "expected PllNotLocked at {lo}, got {err:?}"
        );
        // The frequency must NOT be recorded as tuned.
        assert_eq!(tuner.last_freq_hz(), 0);
    }

    /// On the first lock miss the driver lowers VCO current (R12[7:5] = 011)
    /// before retrying — the osmocom recovery nudge.
    #[test]
    fn first_lock_miss_lowers_vco_current_before_retry() {
        // 5-byte pre-read (neutral fine-tune), first 3-byte read misses lock,
        // second 3-byte read locks (R2[6] set).
        let mut bus = ScriptedReadBus::new(vec![
            vec![0, 0, 0, 0, 0x20],
            vec![0, 0, 0x00],
            vec![0, 0, 0x40],
        ]);
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner
            .set_freq(&mut bus, 162_400_000)
            .expect("locks on retry");
        // Exactly one masked write to R12 (0x12) with value 0x60, mask 0xe0 —
        // the VCO-current bump — must have occurred between the two reads.
        let vco_bumps = bus
            .writes
            .iter()
            .filter(|(reg, data)| *reg == 0x12 && data.len() == 1 && data[0] & 0xe0 == 0x60)
            .count();
        assert_eq!(
            vco_bumps, 1,
            "VCO current bumped exactly once on first miss"
        );
    }

    /// A tuner that locks on the first read does not get the VCO-current bump.
    #[test]
    fn immediate_lock_skips_vco_current_bump() {
        let mut bus = ScriptedReadBus::new(vec![vec![0, 0, 0, 0, 0x20], vec![0, 0, 0x40]]);
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner
            .set_freq(&mut bus, 162_400_000)
            .expect("locks first try");
        let has_bump = bus
            .writes
            .iter()
            .any(|(reg, data)| *reg == 0x12 && data[0] & 0xe0 == 0x60);
        assert!(!has_bump, "no VCO-current bump when lock is immediate");
    }

    /// The pre-PLL read nudges `div_num` toward `vco_power_ref`: for R820T2
    /// (ref=2), a `vco_fine_tune` of 3 (> ref) decrements the divider.
    #[test]
    fn pre_pll_read_nudges_div_num_down_when_fine_tune_is_high() {
        // vco_fine_tune = (R4 & 0x30) >> 4 = 3 → R4 = 0x30. R2 reports lock so
        // the tune succeeds; the effect is observable in the PLL block's
        // R16[7:5] being one less than the computed div_num.
        let mut bus = ScriptedReadBus::new(vec![vec![0, 0, 0x40, 0, 0x30], vec![0, 0, 0x40]]);
        let mut tuner = R82xx::new(TunerKind::R820T2);
        tuner.set_freq(&mut bus, 162_400_000).unwrap();
        let (_, pll) = bus
            .writes
            .iter()
            .find(|(reg, data)| *reg == 0x10 && data.len() == 7)
            .expect("PLL block present");
        // Computed div_num for 162.4 MHz is 3 (mix_div 16); nudge −1 → 2.
        assert_eq!(pll[0] & 0xe0, 2 << 5, "div_num nudged down from 3 to 2");
    }
}
