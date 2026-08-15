//! Gain model: modes, requests, and hardware gain steps.

use std::fmt;

use crate::error::SdrError;

/// Auto vs manual gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GainMode {
    /// Hardware/software AGC controls gain.
    Auto,
    /// User controls gain explicitly via [`GainRequest`].
    Manual,
}

/// Identifies one analog gain stage in a tuner's signal chain.
///
/// The integer codes are a stable cross-language contract used by foreign
/// bindings (JNI, C ABI): **0 = LNA, 1 = MIXER, 2 = VGA**. Use
/// [`GainStageId::code`] / [`GainStageId::from_code`] for those conversions
/// rather than re-deriving the mapping at each binding layer.
///
/// [`GainStageId::name`] yields the hardware-defined stage string used by
/// [`GainRequest::PerStage`], so a typed stage can be turned into a gain
/// request with [`GainRequest::per_stage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GainStageId {
    /// Low-noise amplifier (RF front-end stage). Code 0, name `"LNA"`.
    Lna,
    /// Mixer stage. Code 1, name `"MIXER"`.
    Mixer,
    /// Variable-gain amplifier (IF stage). Code 2, name `"VGA"`.
    Vga,
}

impl GainStageId {
    /// Stable integer code for foreign bindings: 0 = LNA, 1 = MIXER, 2 = VGA.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            GainStageId::Lna => 0,
            GainStageId::Mixer => 1,
            GainStageId::Vga => 2,
        }
    }

    /// Inverse of [`GainStageId::code`]. Returns `None` for unknown codes.
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(GainStageId::Lna),
            1 => Some(GainStageId::Mixer),
            2 => Some(GainStageId::Vga),
            _ => None,
        }
    }

    /// Hardware-defined stage name as used by [`GainRequest::PerStage`]:
    /// `"LNA"`, `"MIXER"`, or `"VGA"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            GainStageId::Lna => "LNA",
            GainStageId::Mixer => "MIXER",
            GainStageId::Vga => "VGA",
        }
    }
}

impl fmt::Display for GainStageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<GainStageId> for i32 {
    fn from(stage: GainStageId) -> Self {
        stage.code()
    }
}

impl TryFrom<i32> for GainStageId {
    type Error = SdrError;

    fn try_from(code: i32) -> Result<Self, Self::Error> {
        Self::from_code(code).ok_or_else(|| {
            SdrError::InvalidParameter(format!(
                "unknown gain stage code {code} (expected 0=LNA, 1=MIXER, 2=VGA)"
            ))
        })
    }
}

/// A gain request issued to a device or tuner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GainRequest {
    /// Overall gain in tenths of a dB (e.g. `496` = 49.6 dB).
    Overall(i32),
    /// Per-stage gain; `name` is hardware-defined ("LNA", "MIXER", "VGA", ...).
    PerStage {
        /// Stage name, e.g. "LNA".
        name: &'static str,
        /// Tenths of a dB.
        tenths_db: i32,
    },
}

impl GainRequest {
    /// Convenience: an overall-gain request.
    #[must_use]
    pub const fn overall(tenths_db: i32) -> Self {
        Self::Overall(tenths_db)
    }

    /// Convenience: a per-stage request for a typed [`GainStageId`].
    ///
    /// Equivalent to [`GainRequest::PerStage`] with `name` set to
    /// [`GainStageId::name`], so drivers that match on the stage string
    /// ("LNA"/"MIXER"/"VGA") accept it unchanged.
    #[must_use]
    pub const fn per_stage(stage: GainStageId, tenths_db: i32) -> Self {
        Self::PerStage {
            name: stage.name(),
            tenths_db,
        }
    }
}

/// One discrete hardware gain step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GainStep {
    /// Stage name, or "OVERALL" for the aggregate.
    pub name: &'static str,
    /// Gain in tenths of a dB.
    pub tenths_db: i32,
}

impl GainStep {
    /// Construct a gain step.
    #[must_use]
    pub const fn new(name: &'static str, tenths_db: i32) -> Self {
        Self { name, tenths_db }
    }
}
