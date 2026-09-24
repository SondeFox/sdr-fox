//! Board routing derived from the independently measured USB contract recorded
//! in `docs/BLOG_V4_RF_ROUTING.md`. These are masked fields, not register images.

pub(crate) const HF_LIMIT_HZ: u64 = 28_800_000;

/// A plan in the physical SMA frequency domain, after any external converter.
/// The built-in HF conversion is applied before the tuner's separate IF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RfPlan {
    pub(crate) tuner_rf_hz: u64,
    pub(crate) input_r05: u8,
    pub(crate) input_r06: u8,
    pub(crate) notch_r17: u8,
    pub(crate) gpio5_high: bool,
}

impl RfPlan {
    pub(crate) fn for_sma_frequency(hz: u64) -> Self {
        // Deliberate coherent boundary: the observed reference selects HF at
        // exactly 28.8 MHz but omits its translation there. We select VHF at
        // that point, matching the measured configuration one Hz above it.
        let hf = hz < HF_LIMIT_HZ;
        let (input_r05, input_r06) = if hf {
            (0x20, 0x08)
        } else if hz < 250_000_000 {
            (0x60, 0x00)
        } else {
            (0x00, 0x00)
        };
        let notch_clear = hz <= 2_200_000
            || (85_000_000..=112_000_000).contains(&hz)
            || (172_000_000..=242_000_000).contains(&hz);
        Self {
            // The bounded HF branch cannot overflow.
            tuner_rf_hz: if hf { hz + HF_LIMIT_HZ } else { hz },
            input_r05,
            input_r06,
            notch_r17: if notch_clear { 0x00 } else { 0x08 },
            gpio5_high: !hf,
        }
    }
}
