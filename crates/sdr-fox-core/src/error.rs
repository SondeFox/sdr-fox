//! Typed error model shared across sdr-fox crates and bindings.

use thiserror::Error;

/// Top-level error. `#[non_exhaustive]` so new variants don't break consumers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SdrError {
    /// USB or transport-layer failure (carries a descriptive string in v0).
    #[error("transport error: {0}")]
    Transport(String),

    /// No device matched the requested selector.
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    /// Device is already streaming or otherwise busy.
    #[error("device busy")]
    DeviceBusy,

    /// Device disappeared mid-operation (unplug, re-plug, power loss).
    #[error("device lost")]
    DeviceLost,

    /// Tuner-layer failure.
    #[error("tuner error: {0}")]
    Tuner(#[from] TunerError),

    /// Sample rate outside the device's supported range.
    #[error("invalid sample rate: {rate_hz} Hz")]
    InvalidSampleRate {
        /// The rejected rate, in Hz.
        rate_hz: u32,
    },

    /// A parameter was syntactically valid but semantically rejected.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    /// The operation is unsupported on this hardware/configuration.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A transfer did not complete within the timeout. Transient — the device
    /// is still present and the operation may be retried.
    #[error("timeout")]
    Timeout,

    /// A transfer completed but returned fewer bytes than the protocol
    /// requires. Retrying blindly may be unsafe because device state may have
    /// advanced already.
    #[error("short {operation}: expected {expected} bytes, got {actual}")]
    ShortTransfer {
        /// Stable operation name suitable for logs and foreign bindings.
        operation: &'static str,
        /// Exact byte count required by the protocol.
        expected: usize,
        /// Byte count reported by the backend.
        actual: usize,
    },

    /// An endpoint stalled (USB STALL handshake). Often cleared by resetting
    /// the endpoint; not a disconnect.
    #[error("endpoint stall")]
    Stall,

    /// A recovery procedure (for example clearing a stalled bulk endpoint)
    /// failed to restore the data path within its cumulative time budget.
    ///
    /// Terminal — unlike [`SdrError::Timeout`] this is NOT transient: the
    /// budget covered the entire recovery episode across every attempt, so
    /// retrying only repeats the same failed recovery. Streams end when they
    /// hit this error; the alternative is a live-looking stream that never
    /// delivers a sample, which a radio consumer cannot distinguish from
    /// "no signal".
    #[error("{operation} recovery exhausted after {attempts} attempt(s) in {elapsed:?}")]
    RecoveryExhausted {
        /// Stable name of the procedure that was being recovered, suitable
        /// for logs and foreign bindings (e.g. `"bulk endpoint stall clear"`).
        operation: &'static str,
        /// Total wall-clock time spent in the failed recovery episode.
        elapsed: std::time::Duration,
        /// Number of recovery attempts made before giving up.
        attempts: u32,
    },

    /// The device produced data faster than the consumer drained it, and
    /// samples were dropped. `dropped_samples` is the count lost in this event.
    #[error("overflow: {dropped_samples} samples dropped")]
    Overflow {
        /// Number of samples dropped due to the overflow.
        dropped_samples: u64,
    },

    /// The operation was cancelled (e.g. the stream was stopped mid-transfer).
    #[error("cancelled")]
    Cancelled,
}

impl SdrError {
    /// True when the error indicates the device disconnected (hot-unplug).
    /// Consumers use this to decide whether to retry or give up.
    ///
    /// Only [`SdrError::DeviceLost`] and [`SdrError::DeviceNotFound`] count — a
    /// generic [`SdrError::Transport`] failure may be a transient I/O error on a
    /// still-connected device, so it is NOT treated as a disconnect. Callers
    /// that previously relied on `Transport(_)` matching here should instead
    /// react to the transport-level error directly.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        matches!(self, SdrError::DeviceLost | SdrError::DeviceNotFound(_))
    }

    /// True when the error is a transient timeout worth retrying.
    ///
    /// [`SdrError::RecoveryExhausted`] deliberately does NOT count: it is the
    /// cumulative bound placed on a timeout-retry loop, so classifying it as
    /// a timeout would feed it back into the very retry path it terminates.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, SdrError::Timeout)
    }
}

#[cfg(test)]
mod recovery_exhausted_tests {
    use std::time::Duration;

    use super::SdrError;

    fn exhausted() -> SdrError {
        SdrError::RecoveryExhausted {
            operation: "bulk endpoint stall clear",
            elapsed: Duration::from_secs(2),
            attempts: 7,
        }
    }

    /// Retry loops classify errors via these helpers; if either returned
    /// true the terminal recovery error would be swallowed as transient.
    #[test]
    fn recovery_exhausted_is_neither_timeout_nor_disconnect() {
        assert!(!exhausted().is_timeout());
        assert!(!exhausted().is_disconnected());
    }

    #[test]
    fn recovery_exhausted_display_carries_actionable_context() {
        let message = exhausted().to_string();
        assert!(message.contains("bulk endpoint stall clear"), "{message}");
        assert!(message.contains("7 attempt(s)"), "{message}");
        assert!(message.contains("2s"), "{message}");
    }
}

/// Tuner-specific failures, carried by [`SdrError::Tuner`].
///
/// Keeping tuner errors in their own enum (rather than flattening them into
/// [`SdrError`]) lets tuner drivers report precise failure modes without
/// depending on transport-level variants. Splitting errors this way is a
/// common pattern in Rust drivers, and librtlsdr-rs-pure applies it to this
/// same hardware; the variants and fields below were chosen for the failure
/// modes sdr-fox's own tuner code reports.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TunerError {
    /// PLL did not lock at the requested frequency.
    #[error("PLL not locked at {freq_hz} Hz")]
    PllNotLocked {
        /// The frequency, in Hz, that failed to lock.
        freq_hz: u64,
    },

    /// I2C transfer to the tuner failed.
    #[error("I2C transfer failed at addr 0x{addr:02x}")]
    I2cTransferFailed {
        /// The I2C address that did not respond.
        addr: u8,
    },

    /// Gain value out of range for this tuner.
    #[error("invalid gain")]
    InvalidGain,

    /// PLL register programming sequence failed.
    #[error("PLL programming failed")]
    PllProgrammingFailed,

    /// No supported tuner chip responded during the open-time probe.
    ///
    /// Distinct from [`TunerError::PllProgrammingFailed`]: this means the
    /// device carries a tuner sdr-fox does not drive (e.g. FC0012/FC0013/
    /// FC2580), or no tuner ACKed at all — not that a supported tuner failed
    /// to program.
    #[error("no supported tuner detected")]
    NoSupportedTuner,
}
