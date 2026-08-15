//! Tuner trait and the I2C bus abstraction it sits on.
//!
//! Tuners are unit-testable without hardware: they talk to a [`TunerBus`]
//! which, in production, forwards to the RTL2832 control plane (I2C
//! repeater-gated register access) but, in tests, records requests.

use crate::error::TunerError;
use crate::gain::{GainMode, GainRequest, GainStep};

/// Known tuner chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TunerKind {
    /// Rafael Micro R820T.
    R820T,
    /// Rafael Micro R820T2 (most common modern RTL-SDR dongle).
    R820T2,
    /// Rafael Micro R828D (used in RTL-SDR Blog V4 and Pro dongles).
    R828D,
    /// Elonics E4000.
    E4000,
    /// Fitipower FC0012.
    Fc0012,
    /// Fitipower FC0013.
    Fc0013,
    /// FCI FC2580.
    Fc2580,
}

/// The bus a tuner uses for register access. In production this is the
/// RTL2832's I2C repeater; in tests it's a recording mock.
pub trait TunerBus {
    /// Write `data` to a tuner register, beginning at `reg`, over the bus.
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::I2cTransferFailed`] if the device does not ACK
    /// the address or the underlying transport reports a failure.
    fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError>;

    /// Read `len` bytes starting at `reg` from the tuner, over the bus.
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::I2cTransferFailed`] on a NACK or transport error.
    fn i2c_read(&mut self, reg: u8, len: usize) -> Result<Vec<u8>, TunerError>;
}

/// The tuner interface. One module per chip in `sdr-fox-rtlsdr/src/tuners/`.
///
/// The operation set — initialize, tune, set bandwidth, set gain, switch gain
/// mode — is the functional decomposition any tuner driver arrives at; the
/// osmocom `rtlsdr_tuner_iface` carves up the same duties. The trait design
/// here is sdr-fox's own: register access goes through an injected
/// [`TunerBus`] so tuners are testable without hardware, and failures are
/// structured [`TunerError`]s rather than C-style integer returns.
pub trait Tuner: Send {
    /// Initialize the tuner (power, reset, calibration).
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::I2cTransferFailed`] if any initialization register
    /// write is not ACKed.
    fn init(&mut self, bus: &mut dyn TunerBus) -> Result<(), TunerError>;

    /// Tune to `hz`. Implementations program the PLL and tracking filter.
    ///
    /// # Errors
    ///
    /// - [`TunerError::PllNotLocked`] if the PLL cannot lock at `hz`.
    /// - [`TunerError::PllProgrammingFailed`] if the register sequence is rejected.
    /// - [`TunerError::I2cTransferFailed`] on a bus error.
    fn set_freq(&mut self, bus: &mut dyn TunerBus, hz: u64) -> Result<(), TunerError>;

    /// Set the IF/channel bandwidth in Hz (where the tuner supports it).
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::I2cTransferFailed`] if the bandwidth register
    /// write fails.
    fn set_bandwidth(&mut self, bus: &mut dyn TunerBus, hz: u32) -> Result<(), TunerError>;

    /// Apply a gain request.
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::InvalidGain`] if the requested gain is out of range.
    fn set_gain(&mut self, bus: &mut dyn TunerBus, req: GainRequest) -> Result<(), TunerError>;

    /// Enumerate the gain steps this tuner supports.
    fn gains(&self) -> &[GainStep];

    /// Switch between auto and manual gain.
    ///
    /// # Errors
    ///
    /// Returns [`TunerError::I2cTransferFailed`] if the AGC mode register write fails.
    fn set_gain_mode(&mut self, bus: &mut dyn TunerBus, mode: GainMode) -> Result<(), TunerError>;

    /// Which chip this is.
    fn kind(&self) -> TunerKind;
}
