//! Device-level types: discovery, info, the [`SdrDevice`] trait, upconverters.

use crate::error::SdrError;
use crate::gain::{GainMode, GainRequest, GainStageId, GainStep};
use crate::sample::StreamConfig;
use crate::tuner::TunerKind;

/// Broad hardware family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DeviceKind {
    /// RTL2832U-based dongle (R820T2 + RTL2832 and variants).
    RtlSdr,
    /// Airspy R2 or Mini.
    Airspy,
    /// Unknown / unsupported.
    #[default]
    Unknown,
}

/// Lightweight descriptor used during enumeration, before opening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Manufacturer string (if the device exposes one).
    pub vendor_name: Option<String>,
    /// Product string (if the device exposes one). Together with VID/PID
    /// and manufacturer, this selects manufacturer-documented board variants
    /// such as Blog V4's shared tuner clock. It also reaches [`DeviceInfo`].
    pub product_name: Option<String>,
    /// Device serial string (if present).
    pub serial: Option<String>,
    /// Bus / device index for stable selection.
    pub index: usize,
    /// Best-effort hardware family guess.
    pub kind: DeviceKind,
}

/// Information reported by an opened device.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Manufacturer string.
    pub vendor_name: String,
    /// Product string.
    pub product_name: String,
    /// Serial number string.
    pub serial: String,
    /// Hardware family.
    pub kind: DeviceKind,
    /// Detected tuner, if applicable.
    pub tuner: Option<TunerKind>,
}

/// An external frequency upconverter applied *before* the SDR.
///
/// **SondeFox invariant, preserved here**: callers always speak the TRUE RF
/// frequency. The LO offset is applied at exactly one point —
/// [`SdrDevice::set_frequency`] — when an `Upconverter` is configured.
///
/// The canonical example is the **SpyVerter R2**: a 120 MHz LO, "positive
/// image" (non-inverting: output = input + 120 MHz), RF input 1 kHz–60 MHz,
/// powered from the SDR's bias tee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Upconverter {
    /// Local-oscillator frequency in Hz added to the requested RF frequency.
    pub lo_hz: u64,
    /// If true, the image is inverted (subtract instead of add).
    pub invert: bool,
}

impl Upconverter {
    /// SpyVerter R2 preset: 120 MHz LO, positive (non-inverting) image.
    #[must_use]
    pub const fn spyverter() -> Self {
        Self {
            lo_hz: 120_000_000,
            invert: false,
        }
    }

    /// Translate a true RF frequency into the SDR's tuning frequency.
    #[must_use]
    pub fn translate(&self, true_rf_hz: u64) -> u64 {
        if self.invert {
            true_rf_hz.saturating_sub(self.lo_hz)
        } else {
            true_rf_hz.saturating_add(self.lo_hz)
        }
    }
}

impl Default for Upconverter {
    fn default() -> Self {
        Self::spyverter()
    }
}

/// The polymorphic device interface. Implemented by RTL-SDR, Airspy, and any
/// future backend, so consumers can swap hardware behind one type.
///
/// Modeled on SondeFox's `SdrSource` Kotlin interface, made Rust-native.
pub trait SdrDevice: Send {
    /// Static info about this opened device.
    fn info(&self) -> &DeviceInfo;

    /// Set the sample rate in Hz; returns the actual rate the hardware settled on.
    ///
    /// For RTL-SDR the valid ranges are 225 001–300 000 Hz and 900 001–3 200 000 Hz
    /// (firmware constraint). The returned value may differ slightly from the
    /// requested value because the rate is synthesized by an integer ratio.
    ///
    /// # Errors
    ///
    /// - [`SdrError::InvalidSampleRate`] if `hz` is outside the supported range.
    /// - [`SdrError::DeviceLost`] if the device disappeared mid-call.
    /// - [`SdrError::Transport`] on a USB/control-transfer failure.
    fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SdrError>;

    /// Enumerate the discrete sample rates this device supports, in Hz.
    ///
    /// An **empty vector means the rates are not enumerable**: any rate within
    /// the device's documented range may be attempted via
    /// [`SdrDevice::set_sample_rate`], which reports the rate the hardware
    /// actually settled on. Devices with a queryable rate table (e.g. Airspy,
    /// whose firmware reports it at open time) override this; devices with
    /// continuous synthesizer ranges (e.g. RTL-SDR) may return an empty vector
    /// or a representative set.
    ///
    /// The default implementation returns an empty vector, so existing
    /// implementors are unaffected until they opt in.
    fn supported_sample_rates(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The device's reference oscillator frequency in Hz, if it is known.
    ///
    /// This is the clock whose harmonics appear as fixed-frequency "birdies"
    /// in the receiver's own passband — on a 28.8 MHz RTL-SDR they land at
    /// 144.000, 403.200, 432.000, 460.800 MHz and every other multiple. A
    /// measured example sat 30 dB above the noise floor with an antenna
    /// attached, which is fatal for anything narrowband sharing the frequency.
    ///
    /// Consumers pass this to `sdr_fox_dsp::SpurCanceller` so the spur
    /// frequencies are derived from the hardware rather than hardcoded. It is
    /// exposed here, rather than assumed by the DSP layer, because it is a
    /// property of the specific device: it differs between backends and, on
    /// some designs, between tuner variants.
    ///
    /// Reports the **nominal** frequency, not the crystal's true one. That is
    /// the useful value: since the same oscillator sets the receiver's own
    /// frequency scale, its error cancels and a harmonic appears at its
    /// nominal multiple regardless of how far off the physical part is.
    ///
    /// The default implementation returns `None`, so existing implementors are
    /// unaffected until they opt in. `None` means "unknown", not "no reference
    /// oscillator" — callers should skip spur cancellation rather than guess.
    fn reference_clock_hz(&self) -> Option<u32> {
        None
    }

    /// Set the center frequency in Hz. If an [`Upconverter`] is configured,
    /// the LO offset is applied here (the single source of truth).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Tuner`] if the tuner PLL fails to lock at the requested frequency.
    /// - [`SdrError::DeviceLost`] on hot-unplug.
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_frequency(&mut self, hz: u64) -> Result<(), SdrError>;

    /// Set the IF/channel bandwidth in Hz (where the tuner supports it).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Unsupported`] if this tuner has no programmable bandwidth.
    /// - [`SdrError::Tuner`] if the requested bandwidth has no valid filter setting.
    fn set_bandwidth(&mut self, hz: u32) -> Result<(), SdrError>;

    /// Apply a gain request (overall or per-stage).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Tuner`] (`TunerError::InvalidGain`) if the requested gain
    ///   is out of range for the active tuner.
    fn set_gain(&mut self, req: GainRequest) -> Result<(), SdrError>;

    /// Switch between auto and manual gain.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Tuner`] if the tuner rejects the mode change.
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_gain_mode(&mut self, mode: GainMode) -> Result<(), SdrError>;

    /// Enumerate the discrete gain steps this device supports.
    fn gains(&self) -> &[GainStep];

    /// Snap an arbitrary desired gain (tenths dB) to the nearest hardware step.
    ///
    /// Returns `None` if the device exposes no named `"OVERALL"` gain steps.
    ///
    /// The difference is widened to `i64` before taking its absolute value:
    /// `i32::MIN.abs()` panics in debug and wraps in release, which a caller
    /// could otherwise trigger with an extreme `desired_tenths_db`.
    fn closest_gain(&self, desired_tenths_db: i32) -> Option<i32> {
        let desired = i64::from(desired_tenths_db);
        self.gains()
            .iter()
            .filter(|s| s.name == "OVERALL")
            .min_by_key(|s| i64::from(s.tenths_db).abs_diff(desired))
            .map(|s| s.tenths_db)
    }

    /// Enable/disable the bias tee (GPIO0 on RTL-SDR, RF bias on Airspy).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Unsupported`] if the device has no bias tee.
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_bias_tee(&mut self, on: bool) -> Result<(), SdrError>;

    /// Enable/disable automatic gain control (RTL digital AGC; Airspy AGC flags).
    ///
    /// This is the **device-wide** AGC switch. Devices whose hardware exposes
    /// independent AGC loops per gain stage additionally implement
    /// [`SdrDevice::set_stage_agc`].
    ///
    /// # Errors
    ///
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_agc(&mut self, on: bool) -> Result<(), SdrError>;

    /// Enable/disable the AGC loop of a **single** gain stage, independently
    /// of the others (e.g. Airspy's separate LNA and mixer AGC).
    ///
    /// Unlike [`SdrDevice::set_agc`], which switches the whole device, this
    /// affects only `stage`. The default implementation returns
    /// [`SdrError::Unsupported`], so devices without per-stage AGC hardware
    /// (e.g. RTL-SDR) need not override it.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Unsupported`] if the device cannot control AGC per stage
    ///   (the default), or if `stage` has no AGC loop on this hardware.
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_stage_agc(&mut self, stage: GainStageId, on: bool) -> Result<(), SdrError> {
        let _ = on;
        Err(SdrError::Unsupported(format!(
            "per-stage AGC ({stage}) is not supported by this device"
        )))
    }

    /// Set the frequency correction in parts-per-million.
    ///
    /// # Errors
    ///
    /// - [`SdrError::InvalidParameter`] if `ppm` is outside the supported range.
    /// - [`SdrError::Transport`] on a control-transfer failure.
    fn set_frequency_correction_ppm(&mut self, ppm: f64) -> Result<(), SdrError>;

    /// Configure an external upconverter (e.g. SpyVerter). `None` disables it.
    ///
    /// The offset is applied transparently inside [`SdrDevice::set_frequency`];
    /// callers always speak the true RF frequency.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Unsupported`] if the device cannot accommodate the upconverter.
    fn set_upconverter(&mut self, up: Option<Upconverter>) -> Result<(), SdrError>;

    /// Start streaming. Returns a handle that yields [`crate::sample::IqBlock`]s.
    ///
    /// # Errors
    ///
    /// - [`SdrError::DeviceBusy`] if a stream is already running.
    /// - [`SdrError::Transport`] if the USB transfers cannot be submitted.
    fn start_stream(&mut self, cfg: StreamConfig) -> Result<crate::sample::StreamHandle, SdrError>;
}

/// Factory for a backend. Each hardware family registers one of these.
/// Modeled on SondeFox's `SdrBackend` Kotlin interface.
pub trait SdrBackend: Send + Sync {
    /// Does this backend claim the enumerated descriptor?
    fn matches(&self, d: &DeviceDescriptor) -> bool;

    /// Human-readable name for diagnostics.
    fn name(&self) -> &'static str;

    /// Open the device. The transport is supplied by the caller (nusb desktop,
    /// nusb Android fd, rusb desktop, or mock).
    ///
    /// # Errors
    ///
    /// - [`SdrError::DeviceNotFound`] if the descriptor no longer maps to a device.
    /// - [`SdrError::Tuner`] if no supported tuner is detected during probing.
    /// - [`SdrError::Transport`] on a USB open/claim failure.
    fn open(
        &self,
        d: &DeviceDescriptor,
        transport: Box<dyn crate::transport::Transport>,
    ) -> Result<Box<dyn SdrDevice>, SdrError>;
}
