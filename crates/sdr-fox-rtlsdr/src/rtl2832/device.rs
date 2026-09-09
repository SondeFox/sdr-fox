//! The top-level [`RtlSdr`] device — the [`SdrDevice`] impl that wires the
//! RTL2832 control plane to a tuner and exposes the unified device API.

use std::time::{Duration, Instant};

use sdr_fox_core::session::HardwareSession;
use sdr_fox_core::{
    DeviceDescriptor, DeviceInfo, DeviceKind, GainMode, GainRequest, GainStep, IqBlock, SdrDevice,
    SdrError, StreamConfig, Transport, Tuner, TunerKind, Upconverter,
};

use super::baseband::{self, RTL_XTAL_HZ};
use super::{set_i2c_repeater, RtlI2cBus};

/// Bulk IN endpoint used by the RTL2832U in SDR mode.
pub const BULK_ENDPOINT: u8 = 0x81;

/// Fixed, rate-independent allowance in the first-block liveness deadline for
/// one-off stream-start latency: worker-thread spawn, transfer-ring
/// submission, endpoint priming, and OS/USB-stack scheduling under load. None
/// of these scale with the sample rate, so they get a constant term.
const FIRST_BLOCK_FIXED_ALLOWANCE_MS: u64 = 500;

/// Multiple of the ideal buffer fill time allowed on top of the fixed
/// allowance before the bulk endpoint is declared silent.
const FIRST_BLOCK_FILL_HEADROOM: u64 = 4;

/// Settle time between powering the demodulator off and back on during
/// tier-1 wedge recovery.
///
/// `DEMOD_CTL = 0x20` gates the demod/ADC supplies; the part needs those rails
/// to actually fall before the `0xe8` bring-up latches a fresh state, which is
/// the entire point of the power-cycle. Re-asserting power in the same
/// microsecond would leave the internal state the cycle is meant to clear.
/// This runs only on the rare recovery path, so a generous value costs nothing
/// on the healthy path.
const DEMOD_POWER_CYCLE_SETTLE: Duration = Duration::from_millis(100);

/// Bounded wait for the first block of a freshly started bulk stream.
///
/// Derivation: the transport delivers only whole `buffer_size`-byte
/// transfers and the RTL2832U produces 2 bytes per complex sample, so the
/// earliest a healthy device can complete its first transfer is the buffer
/// fill time `buffer_size / 2 / sample_rate` — 16 ms at 2.048 MS/s with the
/// default 64 KiB buffers, but ~146 ms at the 225 kS/s bottom of the range.
/// The bound therefore scales with the configured rate and buffer size
/// instead of hardcoding a number that is wrong at one end of the range:
/// `FIRST_BLOCK_FILL_HEADROOM` times the fill time (jitter proportional to
/// transfer duration), plus `FIRST_BLOCK_FIXED_ALLOWANCE_MS` for one-off
/// startup costs that do not scale with rate. A wedged endpoint delivers
/// nothing *ever*, so any finite bound detects it; over-waiting merely
/// delays recovery once, while under-waiting would reset a healthy device —
/// hence the deliberately generous headroom (564 ms at 2.048 MS/s, ~1.1 s at
/// 225 kS/s, both with 64 KiB buffers).
fn first_block_timeout(sample_rate_hz: u32, buffer_size: usize) -> Duration {
    let samples_per_buffer = buffer_size as u64 / 2;
    let rate = u64::from(sample_rate_hz.max(1));
    let fill_ms = samples_per_buffer.saturating_mul(1000).div_ceil(rate);
    Duration::from_millis(
        FIRST_BLOCK_FIXED_ALLOWANCE_MS
            .saturating_add(fill_ms.saturating_mul(FIRST_BLOCK_FILL_HEADROOM)),
    )
}

/// The error returned when a bulk stream ends (without a terminal error)
/// before delivering its first block.
fn stream_ended_early() -> SdrError {
    SdrError::Transport("bulk stream ended before delivering its first block".into())
}

/// An opened RTL-SDR device.
pub struct RtlSdr {
    info: DeviceInfo,
    transport: Box<dyn Transport>,
    tuner: Box<dyn Tuner>,
    /// Selected alongside the tuner by the strict open-time board predicate.
    /// Retained through recovery; generic constructors never assume this board.
    blog_v4: bool,
    /// Shared hardware-session lifetime. Every stream started from this
    /// device co-owns it; the RTL2832U power-down (`deinit_baseband`) runs
    /// when the LAST co-owner — this handle or any stream that outlived it —
    /// is dropped. See [`RtlSdr::new`] for why the power-down must not live
    /// in a `Drop for RtlSdr`.
    session: HardwareSession,
    sample_rate: u32,
    /// Last user-requested RF frequency. Keep this untranslated so a retune
    /// after a bandwidth change applies any upconverter offset exactly once.
    center_freq: Option<u64>,
    upconverter: Option<Upconverter>,
    bias_tee: bool,
    // --- Cached register state, recorded when each setter succeeds. ---
    // A bulk-liveness recovery reset (see `start_stream`) re-enumerates the
    // device and wipes EVERY register written since open, so the driver must
    // be able to replay the user's configuration from scratch. `None`/empty
    // means "never programmed" and is not replayed.
    freq_correction_ppm: Option<f64>,
    bandwidth: Option<u32>,
    gain_mode: Option<GainMode>,
    /// Successful gain requests in application order. An `Overall` request
    /// reprograms every stage, so it clears the list; a `PerStage` request
    /// upserts its stage in place. Replaying the list therefore reproduces
    /// the final hardware gain state.
    gain_requests: Vec<GainRequest>,
    agc: Option<bool>,
    /// Test-only override of the first-block liveness deadline so wedged-path
    /// unit tests do not wait out the real (deliberately generous) timeout.
    #[cfg(test)]
    probe_timeout_override: Option<Duration>,
}

impl RtlSdr {
    /// Construct from already-opened transport + probed tuner. The caller
    /// (the backend) is responsible for running the tuner probe sequence.
    #[must_use]
    pub fn new(info: DeviceInfo, transport: Box<dyn Transport>, tuner: Box<dyn Tuner>) -> Self {
        // The power-down belongs to the shared hardware session, NOT to a
        // `Drop for RtlSdr`. Streams hold their own transport clones and
        // legitimately outlive this handle — the C ABI, Python, and JNI
        // surfaces all register devices and streams independently — so
        // powering the demod down when the device handle closes would leave a
        // live stream reading from a dead radio. Yet the power-down must
        // still happen: a chip left running wedges the next open (control
        // transfers answer while the bulk endpoint stays silent, and
        // re-running init does not clear it — see `baseband::deinit_baseband`).
        // The session teardown threads that needle: it runs exactly once,
        // after the last co-owner (device handle or stream) is gone and every
        // stream worker has joined.
        //
        // Teardown errors are deliberately swallowed: by then the device may
        // already be unplugged, and nothing useful can be done about a failed
        // power-down.
        let session_transport = transport.boxed_clone();
        let session = HardwareSession::new(move || {
            let mut transport = session_transport;
            if let Err(error) = baseband::deinit_baseband(transport.as_mut()) {
                tracing::debug!(
                    %error,
                    "RTL2832U power-down on session close failed (device gone?)"
                );
            }
        });
        Self {
            info,
            transport,
            tuner,
            blog_v4: false,
            session,
            sample_rate: 2_048_000,
            center_freq: None,
            upconverter: None,
            bias_tee: false,
            freq_correction_ppm: None,
            bandwidth: None,
            gain_mode: None,
            gain_requests: Vec::new(),
            agc: None,
            #[cfg(test)]
            probe_timeout_override: None,
        }
    }

    /// Borrow the tuner (for tests / introspection).
    #[must_use]
    pub fn tuner(&self) -> &dyn Tuner {
        self.tuner.as_ref()
    }

    pub(super) fn with_blog_v4_routing(mut self, enabled: bool) -> Self {
        self.blog_v4 = enabled;
        self
    }

    /// Apply the configured upconverter offset (if any) to the true RF frequency,
    /// producing the SDR tuning frequency. This is the SINGLE point where the
    /// offset is applied (SondeFox invariant).
    fn sdr_freq(&self, true_rf_hz: u64) -> u64 {
        match self.upconverter {
            Some(up) => up.translate(true_rf_hz),
            None => true_rf_hz,
        }
    }

    fn with_i2c_repeater<R>(
        &mut self,
        f: impl FnOnce(&mut dyn Transport, &mut Box<dyn Tuner>) -> Result<R, SdrError>,
    ) -> Result<R, SdrError> {
        set_i2c_repeater(self.transport.as_mut(), true)?;
        let result = f(self.transport.as_mut(), &mut self.tuner);
        let _ = set_i2c_repeater(self.transport.as_mut(), false);
        result
    }

    /// The liveness deadline used by `start_stream`, honoring the test-only
    /// override.
    fn probe_timeout(&self, buffer_size: usize) -> Duration {
        #[cfg(test)]
        if let Some(overridden) = self.probe_timeout_override {
            return overridden;
        }
        first_block_timeout(self.sample_rate, buffer_size)
    }

    /// Flush the EPA FIFO and start the raw cu8 bulk stream — the two steps
    /// every (re)start of the stream must perform, in this order.
    fn start_raw_stream(
        &mut self,
        cfg: &StreamConfig,
    ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
        // Reset the USB EPA FIFO before streaming so the bulk endpoint
        // delivers fresh data (without this the FIFO stays flushed and no
        // samples flow).
        baseband::reset_buffer(self.transport.as_mut())?;
        self.transport.start_bulk_stream(
            BULK_ENDPOINT,
            cfg.buffer_count,
            cfg.buffer_size,
            cfg.queue_depth,
        )
    }

    /// Record a successful gain request so a recovery reset can replay it.
    fn remember_gain(&mut self, req: GainRequest) {
        match req {
            GainRequest::Overall(_) => {
                // An overall request reprograms every stage, superseding
                // anything recorded before it.
                self.gain_requests.clear();
                self.gain_requests.push(req);
            }
            GainRequest::PerStage { name, .. } => {
                let same_stage = self.gain_requests.iter_mut().find(|existing| {
                    matches!(
                        existing,
                        GainRequest::PerStage { name: existing_name, .. }
                            if *existing_name == name
                    )
                });
                if let Some(existing) = same_stage {
                    *existing = req;
                } else {
                    self.gain_requests.push(req);
                }
            }
        }
    }

    /// Reprogram a freshly re-enumerated device back to the driver's state.
    ///
    /// A USB device reset wipes EVERY register written since open — the chip
    /// comes back at power-on defaults — so this reruns the whole open-time
    /// initialisation (baseband bring-up, the per-tuner demod configuration
    /// for whichever tuner is fitted, tuner init; shared with the open path
    /// in `open::init_baseband_defaults` / `open::init_tuner`) and then
    /// replays everything programmed since: sample rate, frequency
    /// correction, bandwidth (which reprograms the matching IF and retunes),
    /// centre frequency, gain mode, gain requests, digital AGC, and bias-T.
    ///
    /// The sample rate is replayed unconditionally: the driver's belief
    /// (default 2.048 MS/s) is what the liveness deadline and the caller's
    /// downstream DSP are based on, so the hardware must be made to match it.
    fn reinitialize_after_reset(&mut self) -> Result<(), SdrError> {
        // No re-probe is needed here: the tuner is soldered, so the kind
        // detected at open remains the truth for the re-enumerated device.
        super::open::init_baseband_defaults(self.transport.as_mut(), self.tuner.kind())?;
        super::open::init_tuner(self.transport.as_mut(), self.tuner.as_mut())?;
        self.sample_rate =
            baseband::set_sample_rate(self.transport.as_mut(), self.sample_rate, RTL_XTAL_HZ)?;
        if let Some(ppm) = self.freq_correction_ppm {
            baseband::set_freq_correction(self.transport.as_mut(), ppm, RTL_XTAL_HZ)?;
        }
        if let Some(bandwidth) = self.bandwidth {
            // Replays the tuner bandwidth, the matching IF frequency, and the
            // centre frequency in one step (set_bandwidth retunes internally).
            self.set_bandwidth(bandwidth)?;
        } else if let Some(center_freq) = self.center_freq {
            self.set_frequency(center_freq)?;
        }
        // Mode before gain requests: replaying a manual gain after its mode
        // matches the order userspace programs them in.
        if let Some(mode) = self.gain_mode {
            self.set_gain_mode(mode)?;
        }
        for req in self.gain_requests.clone() {
            self.set_gain(req)?;
        }
        if let Some(agc) = self.agc {
            self.set_agc(agc)?;
        }
        if self.bias_tee {
            baseband::set_bias_tee(self.transport.as_mut(), true)?;
        }
        Ok(())
    }
}

impl SdrDevice for RtlSdr {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SdrError> {
        let actual = baseband::set_sample_rate(self.transport.as_mut(), hz, RTL_XTAL_HZ)?;
        self.sample_rate = actual;
        Ok(actual)
    }

    /// The RTL2832U's sample rates are **not enumerable**, so this returns an
    /// empty vector — deliberately, not as an accident of the trait default.
    ///
    /// The chip's resampler is programmed from a continuous ratio, giving two
    /// continuous usable ranges (225,001–300,000 Hz and 900,001–3,200,000 Hz)
    /// rather than a discrete rate table. Returning a fabricated discrete list
    /// would misrepresent the hardware; the trait contract defines an empty
    /// vector as "any in-range rate may be attempted". Callers should request
    /// a rate via [`SdrDevice::set_sample_rate`], which returns the rate the
    /// hardware actually settled on.
    fn supported_sample_rates(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The RTL2832U's 28.8 MHz reference. This is the part that generates the
    /// harmonic comb, so it is the right value even on R828D dongles whose
    /// *tuner* runs from a separate 16 MHz reference — the demodulator's
    /// crystal is what radiates into the front end.
    fn reference_clock_hz(&self) -> Option<u32> {
        Some(RTL_XTAL_HZ)
    }

    fn set_frequency(&mut self, hz: u64) -> Result<(), SdrError> {
        let sdr_hz = self.sdr_freq(hz);
        if self.blog_v4 {
            let plan = crate::tuners::blog_v4::RfPlan::for_sma_frequency(sdr_hz);
            // GPIO 5 is part of the observed board route. Read/modify/write
            // preserves the independent bias-T on GPIO 0 and every other pin.
            baseband::set_gpio_output(self.transport.as_mut(), 5)?;
            baseband::set_gpio_bit(self.transport.as_mut(), 5, plan.gpio5_high)?;
        }
        self.with_i2c_repeater(|t, tuner| {
            let mut bus = RtlI2cBus::for_tuner(t, tuner.kind());
            tuner.set_freq(&mut bus, sdr_hz).map_err(SdrError::Tuner)
        })?;
        self.center_freq = Some(hz);
        Ok(())
    }

    fn set_bandwidth(&mut self, hz: u32) -> Result<(), SdrError> {
        // A low-IF R82xx pairs every bandwidth setting with a matching demod
        // IF, so the demod must be reprogrammed and the centre frequency
        // retuned. The E4000 is direct-conversion (zero-IF): its IF is 0 at
        // every bandwidth, so there is deliberately no demod IF reprogram
        // and no retune — the filter change happens entirely inside the
        // tuner. The remaining kinds have no driver, so no tuner instance of
        // theirs can exist here; the match is exhaustive so a future driver
        // must decide its IF policy in this same arm.
        let if_hz = match self.tuner.kind() {
            TunerKind::R820T | TunerKind::R820T2 | TunerKind::R828D => {
                Some(crate::tuners::r82xx::bandwidth_config(hz).if_hz)
            }
            TunerKind::E4000 | TunerKind::Fc0012 | TunerKind::Fc0013 | TunerKind::Fc2580 => None,
        };
        self.with_i2c_repeater(|t, tuner| {
            let mut bus = RtlI2cBus::for_tuner(t, tuner.kind());
            tuner.set_bandwidth(&mut bus, hz).map_err(SdrError::Tuner)
        })?;

        if let Some(if_hz) = if_hz {
            baseband::set_if_freq(self.transport.as_mut(), u64::from(if_hz), RTL_XTAL_HZ)?;
            if let Some(center_freq) = self.center_freq {
                self.set_frequency(center_freq)?;
            }
        }
        self.bandwidth = Some(hz);
        Ok(())
    }

    fn set_gain(&mut self, req: GainRequest) -> Result<(), SdrError> {
        self.with_i2c_repeater(|t, tuner| {
            let mut bus = RtlI2cBus::for_tuner(t, tuner.kind());
            tuner.set_gain(&mut bus, req).map_err(SdrError::Tuner)
        })?;
        self.remember_gain(req);
        Ok(())
    }

    fn set_gain_mode(&mut self, mode: GainMode) -> Result<(), SdrError> {
        self.with_i2c_repeater(|t, tuner| {
            let mut bus = RtlI2cBus::for_tuner(t, tuner.kind());
            tuner.set_gain_mode(&mut bus, mode).map_err(SdrError::Tuner)
        })?;
        self.gain_mode = Some(mode);
        Ok(())
    }

    fn gains(&self) -> &[GainStep] {
        self.tuner.gains()
    }

    fn set_bias_tee(&mut self, on: bool) -> Result<(), SdrError> {
        baseband::set_bias_tee(self.transport.as_mut(), on)?;
        self.bias_tee = on;
        Ok(())
    }

    fn set_agc(&mut self, on: bool) -> Result<(), SdrError> {
        baseband::set_agc_mode(self.transport.as_mut(), on)?;
        self.agc = Some(on);
        Ok(())
    }

    fn set_frequency_correction_ppm(&mut self, ppm: f64) -> Result<(), SdrError> {
        baseband::set_freq_correction(self.transport.as_mut(), ppm, RTL_XTAL_HZ)?;
        self.freq_correction_ppm = Some(ppm);
        Ok(())
    }

    fn set_upconverter(&mut self, up: Option<Upconverter>) -> Result<(), SdrError> {
        self.upconverter = up;
        Ok(())
    }

    fn start_stream(&mut self, cfg: StreamConfig) -> Result<sdr_fox_core::StreamHandle, SdrError> {
        let handle = self.start_stream_inner(&cfg)?;
        // Bind every handed-out stream to the hardware session, in one place
        // so no success path of the recovery machinery below can miss it.
        // Streams carry their own transport clone and outlive this device
        // handle on every FFI surface (the C ABI, Python, and JNI registries
        // hand out device and stream handles independently), so the session —
        // not the device destructor — decides when the chip powers down.
        Ok(self.session.bind_stream(handle))
    }
}

impl RtlSdr {
    /// The full stream-start sequence — liveness probe, wedge recovery, one
    /// retry — WITHOUT the session binding, which [`SdrDevice::start_stream`]
    /// applies to every success path in one place.
    fn start_stream_inner(
        &mut self,
        cfg: &StreamConfig,
    ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
        // The transport returns a raw cu8 stream; the requested format is
        // applied by a converting sink on delivery.
        //
        // P1#9 fix: the old code used a no-op `wrap_with_conversion` shim that
        // silently returned cu8 even when Cf32/Cs8/Cs16 was requested. Now we
        // wrap the raw stream with a real format-converting sink.
        let timeout = self.probe_timeout(cfg.buffer_size);
        let mut handle = self.start_raw_stream(cfg)?;

        // Bulk-liveness probe. An RTL2832U that idled through a USB suspend
        // can reach a state where every control transfer succeeds but the
        // bulk endpoint never delivers a byte; the open-time `probe_or_reset`
        // cannot see this because its control write succeeds. The only
        // reliable detector is the data itself: wait a bounded time for the
        // first block. A healthy device pays nothing beyond this wait for
        // data the caller was about to block on anyway, and the consumed
        // block is re-delivered as block 0 — the probe never loses samples.
        match handle.recv_deadline(Instant::now() + timeout) {
            Some(Ok(first)) => return Ok(deliver_probed(first, handle, cfg.format)),
            // Non-terminal per the recv_deadline contract: nothing arrived
            // before the deadline. Fall through to recovery.
            Some(Err(SdrError::Timeout)) => {}
            // A terminal error (device lost, transport failure) is not the
            // wedge signature; surface it unchanged, no reset.
            Some(Err(fatal)) => return Err(fatal),
            None => return Err(stream_ended_early()),
        }

        // Stop the silent stream cleanly before touching the port. Dropping
        // the handle joins the transport worker, so no transfer is in flight
        // when the reset is issued.
        handle.stop();
        drop(handle);

        // Tier 1: power-cycle the demodulator. This is the recovery that
        // actually matches the failure — see `baseband::deinit_baseband`: a
        // wedged RTL2832U keeps answering control transfers while the bulk
        // endpoint stays dead, and re-running `init_baseband` over the
        // still-running demod does NOT clear it; cutting demod power does.
        //
        // It is tried before the port reset because it is strictly cheaper
        // (control transfers only, no re-enumeration) and, decisively, it is
        // the ONLY tier that can work on Android: there `reset_device` is
        // terminal for the fd handle, so escalating first would convert every
        // wedge into a "re-open required" that only the app can service. That
        // also made the port reset self-defeating — the reset invalidated the
        // handle before the power-down could run, so the fix for the wedge
        // could never be applied and the device stayed wedged for the next
        // attempt.
        match self.try_demod_power_cycle(cfg, timeout) {
            Ok(Some(recovered)) => return Ok(recovered),
            // Still silent after a clean power-cycle: escalate to the port.
            Ok(None) => {}
            Err(fatal) => return Err(fatal),
        }

        match self.transport.reset_device() {
            Ok(()) => {}
            Err(SdrError::Unsupported(_)) => {
                // Transports with no path to the port at all (the mock, and
                // any transport handed a descriptor it does not own) cannot
                // recover, and on those transports the timing heuristic is
                // not evidence of a wedged device. Do not fail the stream:
                // restart it and surface the original condition — a stream
                // that has produced no data yet — for the caller's own
                // timeout policy.
                tracing::warn!(
                    probe_timeout_ms = timeout.as_millis() as u64,
                    "RTL bulk endpoint produced no data before the liveness deadline, but this \
                     transport cannot reset the USB port; returning the stream unrecovered"
                );
                let handle = self.start_raw_stream(cfg)?;
                return Ok(wrap_with_conversion(handle, cfg.format));
            }
            // Includes the Android fd transport's "re-open required"
            // condition (`sdr_fox_core::session::is_reopen_required`): the
            // reset WAS issued to clear the wedge, but the handle cannot
            // survive it and only the app can mint a new fd via UsbManager.
            // Surface it unchanged — re-initialising or retrying against a
            // dead handle here is exactly the recovery-that-cannot-work this
            // arm used to perform when that transport pretended `Ok`.
            Err(reset_error) => return Err(reset_error),
        }

        tracing::warn!(
            probe_timeout_ms = timeout.as_millis() as u64,
            "RTL bulk endpoint produced no data before the liveness deadline; the USB device \
             was reset and is being reprogrammed from scratch"
        );
        // The reset re-enumerated the device: every register written since
        // open is gone. Rerun the full open-time initialisation and replay
        // the cached configuration, then retry the stream exactly once.
        self.reinitialize_after_reset()?;

        let mut retry = self.start_raw_stream(cfg)?;
        match retry.recv_deadline(Instant::now() + timeout) {
            Some(Ok(first)) => Ok(deliver_probed(first, retry, cfg.format)),
            Some(Err(SdrError::Timeout)) => {
                retry.stop();
                drop(retry);
                Err(SdrError::Transport(format!(
                    "RTL2832U bulk endpoint delivered no data within {} ms of stream start, \
                     and one USB device reset plus full reinitialisation did not recover it; \
                     power-cycle or re-plug the dongle",
                    timeout.as_millis()
                )))
            }
            Some(Err(fatal)) => Err(fatal),
            None => Err(stream_ended_early()),
        }
    }

    /// Tier-1 wedge recovery: cut power to the demodulator, bring it back up,
    /// reprogram the cached configuration, and retry the stream once — all
    /// over the existing handle, without resetting the USB port.
    ///
    /// Returns `Ok(Some(stream))` when the device delivered a first block
    /// afterwards, `Ok(None)` when it stayed silent (the caller escalates to a
    /// port reset), and `Err` only for genuinely terminal failures.
    ///
    /// A failure to power-cycle or reprogram is reported as `Ok(None)` rather
    /// than an error: it means this tier could not be applied, which is
    /// exactly the case the port reset exists to handle. The one exception is
    /// a handle that already needs re-opening — resetting a dead handle cannot
    /// help, so that propagates immediately and reaches the app as the
    /// actionable re-open request.
    fn try_demod_power_cycle(
        &mut self,
        cfg: &StreamConfig,
        timeout: Duration,
    ) -> Result<Option<sdr_fox_core::StreamHandle>, SdrError> {
        if let Err(error) = baseband::deinit_baseband(self.transport.as_mut()) {
            if sdr_fox_core::session::is_reopen_required(&error) {
                return Err(error);
            }
            tracing::debug!(%error, "demod power-down during wedge recovery failed; escalating");
            return Ok(None);
        }
        std::thread::sleep(DEMOD_POWER_CYCLE_SETTLE);

        // Same full reprogram the post-reset path uses: the power-cycle wiped
        // every register, so the cached configuration has to be replayed.
        if let Err(error) = self.reinitialize_after_reset() {
            if sdr_fox_core::session::is_reopen_required(&error) {
                return Err(error);
            }
            tracing::debug!(%error, "reprogramming after demod power-cycle failed; escalating");
            return Ok(None);
        }

        tracing::warn!(
            probe_timeout_ms = timeout.as_millis() as u64,
            "RTL bulk endpoint produced no data before the liveness deadline; the demodulator \
             was power-cycled and reprogrammed, and the stream is being retried"
        );

        let mut retry = self.start_raw_stream(cfg)?;
        match retry.recv_deadline(Instant::now() + timeout) {
            Some(Ok(first)) => Ok(Some(deliver_probed(first, retry, cfg.format))),
            Some(Err(SdrError::Timeout)) => {
                retry.stop();
                drop(retry);
                Ok(None)
            }
            Some(Err(fatal)) => Err(fatal),
            None => Err(stream_ended_early()),
        }
    }
}

/// Wrap the block consumed by the liveness probe back onto the front of the
/// stream and apply format conversion to the whole thing. The probe must not
/// lose samples: the consumed block is real data and is re-delivered first.
fn deliver_probed(
    first: IqBlock,
    inner: sdr_fox_core::StreamHandle,
    fmt: sdr_fox_core::IqFormat,
) -> sdr_fox_core::StreamHandle {
    wrap_with_conversion(
        Box::new(FirstBlockPrepended {
            first: Some(first),
            inner,
        }),
        fmt,
    )
}

/// A `StreamSink` that re-delivers the block consumed by the start-time
/// liveness probe before handing over to the underlying stream. After the
/// first `recv`, the cost is one `Option` check per block.
struct FirstBlockPrepended {
    first: Option<IqBlock>,
    inner: sdr_fox_core::StreamHandle,
}

impl sdr_fox_core::StreamSink for FirstBlockPrepended {
    fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
        if let Some(first) = self.first.take() {
            return Some(Ok(first));
        }
        self.inner.recv()
    }

    fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
        if let Some(first) = self.first.take() {
            return Some(Ok(first));
        }
        self.inner.recv_deadline(deadline)
    }

    fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
        self.inner.stop_handle()
    }

    fn stop(&self) {
        self.inner.stop();
    }
}

/// Wrap a boxed cu8 stream to convert samples to the requested format on
/// delivery. The underlying transport always returns raw cu8 (RTL2832U native
/// format). This wrapper performs SIMD conversion to Cf32, byte-level
/// conversion to Cs8, and scaled conversion to Cs16.
fn wrap_with_conversion(
    inner: sdr_fox_core::StreamHandle,
    fmt: sdr_fox_core::IqFormat,
) -> sdr_fox_core::StreamHandle {
    if matches!(fmt, sdr_fox_core::IqFormat::Cu8) {
        return inner;
    }
    Box::new(FormatConvertingSink { inner, format: fmt })
}

/// A `StreamSink` wrapper that converts cu8 blocks to the requested format.
struct FormatConvertingSink {
    inner: sdr_fox_core::StreamHandle,
    format: sdr_fox_core::IqFormat,
}

impl FormatConvertingSink {
    fn convert_block(&self, block: sdr_fox_core::IqBlock) -> sdr_fox_core::IqBlock {
        sdr_fox_transport::stream::convert_cu8_block(block, self.format)
    }
}

impl sdr_fox_core::StreamSink for FormatConvertingSink {
    fn recv(&mut self) -> Option<Result<sdr_fox_core::IqBlock, SdrError>> {
        Some(self.inner.recv()?.map(|block| self.convert_block(block)))
    }

    fn recv_deadline(
        &mut self,
        deadline: std::time::Instant,
    ) -> Option<Result<sdr_fox_core::IqBlock, SdrError>> {
        Some(
            self.inner
                .recv_deadline(deadline)?
                .map(|block| self.convert_block(block)),
        )
    }

    fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
        self.inner.stop_handle()
    }

    fn stop(&self) {
        self.inner.stop();
    }
}

// NOTE: there is deliberately NO `impl Drop for RtlSdr`. The RTL2832U
// power-down (`baseband::deinit_baseband`) lives in the shared
// `HardwareSession` teardown built in `RtlSdr::new`: a destructor here would
// power the chip down while a stream that outlived this handle is still
// reading (the C ABI, Python, and JNI surfaces all hand out device and stream
// handles independently). The session runs the same power-down exactly once,
// after the last co-owner is gone and every stream worker has joined —
// preserving the wedge fix (a chip left running comes back with a silent bulk
// endpoint on the next open) without racing live streams.

/// Backend matching known RTL-SDR VID:PID pairs.
pub struct RtlSdrBackend;

impl sdr_fox_core::SdrBackend for RtlSdrBackend {
    fn matches(&self, d: &DeviceDescriptor) -> bool {
        const KNOWN: [(u16, u16); 4] = [
            (0x0bda, 0x2832),
            (0x0bda, 0x2838),
            (0x1d50, 0x6089),
            (0x1d50, 0xcc60),
        ];
        KNOWN
            .iter()
            .any(|(v, p)| *v == d.vendor_id && *p == d.product_id)
    }

    fn name(&self) -> &'static str {
        "rtl-sdr"
    }

    fn open(
        &self,
        d: &DeviceDescriptor,
        transport: Box<dyn Transport>,
    ) -> Result<Box<dyn SdrDevice>, SdrError> {
        // The full open path (baseband init + tuner probe + tuner init) needs
        // the tuner modules wired in; this dispatches to the probe helper.
        let device = super::open::open_device(d, transport)?;
        Ok(Box::new(device))
    }
}

/// Construct a [`DeviceInfo`] for an RTL-SDR without running a probe.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn make_info(d: &DeviceDescriptor, tuner: Option<TunerKind>) -> DeviceInfo {
    DeviceInfo {
        vendor_id: d.vendor_id,
        product_id: d.product_id,
        vendor_name: d.vendor_name.clone().unwrap_or_default(),
        product_name: d.product_name.clone().unwrap_or_default(),
        serial: d.serial.clone().unwrap_or_default(),
        kind: DeviceKind::RtlSdr,
        tuner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{IqBlock, IqFormat, IqSamples, StreamSink, TunerBus, TunerError};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RfControlState {
        gpio: [u8; 5],
        tuner: [u8; 32],
        writes: Vec<(u16, u16, Vec<u8>)>,
        fail_gpio_once: bool,
    }

    struct RfControlTransport(Arc<Mutex<RfControlState>>);

    impl Transport for RfControlTransport {
        fn control_in(
            &mut self,
            request: &sdr_fox_core::ControlRequest,
        ) -> Result<Vec<u8>, SdrError> {
            let state = self.0.lock().unwrap();
            let mut bytes = vec![0; request.data.len()];
            if request.index == 0x200 && (0x3000..=0x3004).contains(&request.value) {
                bytes[0] = state.gpio[usize::from(request.value - 0x3000)];
            } else if request.index == 0x600 && request.value == 0x74 {
                // Wire-order R828D status: locked, centered VCO fine tune 1.
                if bytes.len() > 2 {
                    bytes[2] = 0x02;
                }
                if bytes.len() > 4 {
                    bytes[4] = 0x08;
                }
            }
            Ok(bytes)
        }

        fn control_out(
            &mut self,
            request: &sdr_fox_core::ControlRequest,
        ) -> Result<usize, SdrError> {
            let mut state = self.0.lock().unwrap();
            if request.index == 0x210 && request.value == 0x3001 && state.fail_gpio_once {
                state.fail_gpio_once = false;
                return Err(SdrError::Transport("authored GPIO failure".into()));
            }
            state
                .writes
                .push((request.value, request.index, request.data.clone()));
            if request.index == 0x210 && (0x3000..=0x3004).contains(&request.value) {
                state.gpio[usize::from(request.value - 0x3000)] = request.data[0];
            } else if request.index == 0x610 && request.value == 0x74 && request.data.len() > 1 {
                let start = usize::from(request.data[0]);
                for (index, byte) in request.data[1..].iter().enumerate() {
                    state.tuner[start + index] = *byte;
                }
            }
            Ok(request.data.len())
        }

        fn bulk_read(&mut self, _: u8, _: usize, _: u32) -> Result<Vec<u8>, SdrError> {
            Err(SdrError::Unsupported("control-only test".into()))
        }

        fn start_bulk_stream(
            &mut self,
            _: u8,
            _: usize,
            _: usize,
            _: usize,
        ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
            Err(SdrError::Unsupported("control-only test".into()))
        }

        fn boxed_clone(&self) -> Box<dyn Transport> {
            Box::new(Self(Arc::clone(&self.0)))
        }
    }

    fn rf_test_device(blog_v4: bool) -> (RtlSdr, Arc<Mutex<RfControlState>>) {
        let state = Arc::new(Mutex::new(RfControlState::default()));
        // Include bias GPIO 0 and unrelated pins in every GPIO image.
        state.lock().unwrap().gpio = [0, 0x99, 0, 0x49, 0xe7];
        let tuner = if blog_v4 {
            crate::tuners::r82xx::R82xx::for_blog_v4()
        } else {
            crate::tuners::r82xx::R82xx::new(TunerKind::R828D)
        };
        let device = RtlSdr::new(
            DeviceInfo {
                tuner: Some(TunerKind::R828D),
                ..DeviceInfo::default()
            },
            Box::new(RfControlTransport(Arc::clone(&state))),
            Box::new(tuner),
        )
        .with_blog_v4_routing(blog_v4);
        (device, state)
    }

    #[test]
    fn blog_v4_gpio_preserves_bias_and_uses_sma_rf_after_external_conversion() {
        let (mut device, state) = rf_test_device(true);
        device.set_frequency(500_000).unwrap();
        assert_eq!(state.lock().unwrap().gpio, [0, 0x99, 0, 0x69, 0xc7]);
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x20);
        device
            .set_upconverter(Some(Upconverter::spyverter()))
            .unwrap();
        device.set_frequency(500_000).unwrap();
        assert_eq!(state.lock().unwrap().gpio[1], 0xb9);
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x60);
        // Physical SMA RF is 120.5 MHz, outside the 85..112 MHz notch window.
        assert_eq!(state.lock().unwrap().tuner[0x17] & 0x08, 0x08);
        assert_eq!(device.center_freq, Some(500_000));
        device.set_bandwidth(250_000).unwrap();
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x60);
        assert_eq!(device.center_freq, Some(500_000));
        device.set_upconverter(None).unwrap();
        // Existing deferred converter behavior: the next tune changes route.
        device.set_frequency(28_800_000).unwrap();
        assert_eq!(state.lock().unwrap().gpio[1] & 0x20, 0x20);
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x60);
        device.set_frequency(28_799_999).unwrap();
        assert_eq!(state.lock().unwrap().gpio[1] & 0x20, 0x00);
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x20);
    }

    #[test]
    fn blog_v4_gpio_failure_does_not_publish_center_and_retry_reprograms_route() {
        let (mut device, state) = rf_test_device(true);
        device.set_frequency(401_500_000).unwrap();
        state.lock().unwrap().fail_gpio_once = true;
        assert!(device.set_frequency(500_000).is_err());
        assert_eq!(device.center_freq, Some(401_500_000));
        device.set_frequency(500_000).unwrap();
        assert_eq!(device.center_freq, Some(500_000));
        assert_eq!(state.lock().unwrap().gpio[1] & 0x20, 0);
        assert_eq!(state.lock().unwrap().tuner[5] & 0x60, 0x20);
    }

    #[test]
    fn blog_v4_reset_replays_route_bandwidth_gain_ppm_and_bias() {
        for (hz, r05, gpio5) in [
            (500_000, 0x20, 0),
            (100_000_000, 0x60, 0x20),
            (401_500_000, 0, 0x20),
        ] {
            let (mut device, state) = rf_test_device(true);
            device.set_sample_rate(2_400_000).unwrap();
            device.set_bandwidth(250_000).unwrap();
            device.set_frequency(hz).unwrap();
            device.set_gain_mode(GainMode::Manual).unwrap();
            device.set_gain(GainRequest::Overall(280)).unwrap();
            device.set_bias_tee(true).unwrap();
            let gpio_before_ppm = state.lock().unwrap().gpio;
            let tuner_before_ppm = state.lock().unwrap().tuner;
            device.set_frequency_correction_ppm(12.0).unwrap();
            assert_eq!(state.lock().unwrap().gpio, gpio_before_ppm);
            assert_eq!(state.lock().unwrap().tuner, tuner_before_ppm);
            // Model the register loss that precedes the production replay path.
            state.lock().unwrap().gpio = [0; 5];
            state.lock().unwrap().tuner = [0; 32];
            device.reinitialize_after_reset().unwrap();
            let state = state.lock().unwrap();
            assert_eq!(state.gpio[1] & 0x21, gpio5 | 1);
            assert_eq!(state.tuner[5] & 0x60, r05);
            assert_eq!(state.tuner[5] & 0x1f, tuner_before_ppm[5] & 0x1f);
            assert_eq!(state.tuner[0x0b], tuner_before_ppm[0x0b]);
            assert_eq!(device.center_freq, Some(hz));
        }
    }

    #[test]
    fn generic_r828d_tuning_never_configures_board_gpio() {
        let (mut device, state) = rf_test_device(false);
        let before = state.lock().unwrap().gpio;
        device.set_frequency(100_000_000).unwrap();
        device.set_bandwidth(250_000).unwrap();
        assert_eq!(state.lock().unwrap().gpio, before);
        assert!(!state
            .lock()
            .unwrap()
            .writes
            .iter()
            .any(|(_, index, _)| *index == 0x210));
    }

    /// A synthetic sink that yields one cu8 block then ends.
    struct OneShotCu8 {
        block: Option<IqBlock>,
    }

    impl StreamSink for OneShotCu8 {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            self.block.take().map(Ok)
        }

        fn recv_deadline(
            &mut self,
            _deadline: std::time::Instant,
        ) -> Option<Result<IqBlock, SdrError>> {
            self.recv()
        }

        fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
            sdr_fox_core::sample::StreamStopHandle::new(|| {})
        }

        fn stop(&self) {}
    }

    fn cu8_block(bytes: Vec<u8>) -> Box<dyn StreamSink> {
        Box::new(OneShotCu8 {
            block: Some(IqBlock {
                samples: IqSamples::Cu8(bytes),
                dropped: 0,
                sequence: 0,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            }),
        })
    }

    struct RecordingR82xx {
        frequencies: Arc<Mutex<Vec<u64>>>,
    }

    impl Tuner for RecordingR82xx {
        fn init(&mut self, _bus: &mut dyn TunerBus) -> Result<(), TunerError> {
            Ok(())
        }

        fn set_freq(&mut self, _bus: &mut dyn TunerBus, hz: u64) -> Result<(), TunerError> {
            self.frequencies.lock().unwrap().push(hz);
            Ok(())
        }

        fn set_bandwidth(&mut self, _bus: &mut dyn TunerBus, _hz: u32) -> Result<(), TunerError> {
            Ok(())
        }

        fn set_gain(
            &mut self,
            _bus: &mut dyn TunerBus,
            _req: GainRequest,
        ) -> Result<(), TunerError> {
            Ok(())
        }

        fn gains(&self) -> &[GainStep] {
            &[]
        }

        fn set_gain_mode(
            &mut self,
            _bus: &mut dyn TunerBus,
            _mode: GainMode,
        ) -> Result<(), TunerError> {
            Ok(())
        }

        fn kind(&self) -> TunerKind {
            TunerKind::R820T2
        }
    }

    #[test]
    fn bandwidth_retune_applies_upconverter_exactly_once() {
        let frequencies = Arc::new(Mutex::new(Vec::new()));
        let descriptor = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::RtlSdr,
        };
        let tuner = RecordingR82xx {
            frequencies: Arc::clone(&frequencies),
        };
        let mut device = RtlSdr::new(
            make_info(&descriptor, Some(TunerKind::R820T2)),
            Box::new(sdr_fox_transport::MockTransport::new()),
            Box::new(tuner),
        );
        device
            .set_upconverter(Some(Upconverter::spyverter()))
            .unwrap();
        device.set_frequency(10_000_000).unwrap();
        device.set_bandwidth(2_000_000).unwrap();

        assert_eq!(
            *frequencies.lock().unwrap(),
            [130_000_000, 130_000_000],
            "bandwidth retune must reuse the raw RF request, not retranslate the cached SDR frequency"
        );
    }

    #[test]
    fn supported_sample_rates_is_empty_meaning_not_enumerable() {
        // The RTL2832U resampler covers continuous ranges (225,001-300,000 and
        // 900,001-3,200,000 Hz), so the honest enumeration is "not enumerable"
        // — the trait-contract empty vector. This pins the explicit override
        // (G3, RTL half) so a future discrete table is a conscious decision.
        let descriptor = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::RtlSdr,
        };
        let device = RtlSdr::new(
            make_info(&descriptor, Some(TunerKind::R820T2)),
            Box::new(sdr_fox_transport::MockTransport::new()),
            Box::new(RecordingR82xx {
                frequencies: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        assert!(
            device.supported_sample_rates().is_empty(),
            "RTL sample rates are continuous ranges; enumeration must be empty"
        );
    }

    #[test]
    fn format_conversion_cf32_uses_simd_scaling() {
        // cu8 128 → cf32 0.0 (center); cu8 0 → -1.0; cu8 255 → ~+1.0.
        let inner = cu8_block(vec![128, 128, 0, 255]);
        let mut sink = FormatConvertingSink {
            inner,
            format: IqFormat::Cf32,
        };
        let block = sink.recv().unwrap().unwrap();
        let IqSamples::Cf32(f) = block.samples else {
            panic!("expected cf32");
        };
        assert!(
            (f[0] - (128.0 - 127.5) / 127.5).abs() < 1e-6,
            "128, got {}",
            f[0]
        );
        assert!(
            (f[1] - (128.0 - 127.5) / 127.5).abs() < 1e-6,
            "128, got {}",
            f[1]
        );
        assert!((f[2] + 1.0).abs() < 1e-6, "0 → -1.0, got {}", f[2]);
        assert!((f[3] - 1.0).abs() < 1e-6, "255 → +1.0, got {}", f[3]);
    }

    #[test]
    fn format_conversion_cs8_signs_bytes() {
        let inner = cu8_block(vec![128, 0, 255]);
        let mut sink = FormatConvertingSink {
            inner,
            format: IqFormat::Cs8,
        };
        let block = sink.recv().unwrap().unwrap();
        let IqSamples::Cs8(s) = block.samples else {
            panic!("expected cs8");
        };
        assert_eq!(s, vec![0i8, -128i8, 127i8]);
    }

    #[test]
    fn format_conversion_cs16_scales_to_16_bit() {
        // cu8 128 → 0; cu8 0 → -128<<8 = -32768; cu8 255 → 127<<8 = 32512.
        let inner = cu8_block(vec![128, 0, 255]);
        let mut sink = FormatConvertingSink {
            inner,
            format: IqFormat::Cs16,
        };
        let block = sink.recv().unwrap().unwrap();
        let IqSamples::Cs16(s) = block.samples else {
            panic!("expected cs16");
        };
        assert_eq!(s[0], 0, "128 → 0");
        assert_eq!(s[1], -128 << 8, "0 → -32768");
        assert_eq!(s[2], 127 << 8, "255 → 32512");
    }

    #[test]
    fn format_conversion_cu8_passes_through() {
        let inner = cu8_block(vec![10, 20, 30]);
        let mut sink = FormatConvertingSink {
            inner,
            format: IqFormat::Cu8,
        };
        let block = sink.recv().unwrap().unwrap();
        let IqSamples::Cu8(b) = block.samples else {
            panic!("expected cu8 passthrough");
        };
        assert_eq!(b, vec![10, 20, 30]);
    }

    // === Bulk-liveness probe and recovery (wedged-endpoint bug) ===

    use sdr_fox_transport::stream::{start_stream as engine_stream, BufferSource, SyntheticSource};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// What the next `start_bulk_stream` call on a [`ProbeTransport`] returns.
    enum StreamScript {
        /// Delivers `payload` blocks immediately, forever.
        Healthy(Vec<u8>),
        /// Never delivers a byte; parks until stopped — the wedged-bulk-
        /// endpoint signature established on hardware.
        Wedged,
        /// Dies immediately with a terminal `DeviceLost`.
        Dead,
    }

    #[derive(Clone, Copy)]
    enum ResetScript {
        Works,
        Unsupported,
        /// The Android fd transport's honest outcome: the reset was issued
        /// but the handle died with it; only the app can re-open the device.
        ReopenRequired,
    }

    struct ProbeShared {
        scripts: VecDeque<StreamScript>,
        reset: ResetScript,
        resets: usize,
        streams_started: usize,
        /// `(wValue, wIndex, payload)` of every control OUT transfer.
        control_out: Vec<(u16, u16, Vec<u8>)>,
    }

    /// Transport whose control plane always answers (the bug's signature) and
    /// whose bulk streams follow a per-call script.
    struct ProbeTransport {
        shared: Arc<Mutex<ProbeShared>>,
    }

    /// A source that never produces data: parks until the engine sets the
    /// stop flag, then reports `Cancelled`.
    struct WedgedSource {
        stop: Option<Arc<AtomicBool>>,
    }

    impl BufferSource for WedgedSource {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            while !self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::Acquire))
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(SdrError::Cancelled)
        }

        fn set_stop(&mut self, stop: Arc<AtomicBool>) {
            self.stop = Some(stop);
        }
    }

    impl Transport for ProbeTransport {
        fn control_in(&mut self, req: &sdr_fox_core::ControlRequest) -> Result<Vec<u8>, SdrError> {
            Ok(vec![0; req.data.len()])
        }

        fn control_out(&mut self, req: &sdr_fox_core::ControlRequest) -> Result<usize, SdrError> {
            self.shared
                .lock()
                .unwrap()
                .control_out
                .push((req.value, req.index, req.data.clone()));
            Ok(req.data.len())
        }

        fn bulk_read(
            &mut self,
            _endpoint: u8,
            _len: usize,
            _timeout_ms: u32,
        ) -> Result<Vec<u8>, SdrError> {
            Ok(Vec::new())
        }

        fn start_bulk_stream(
            &mut self,
            _endpoint: u8,
            _buffer_count: usize,
            _buffer_size: usize,
            queue_depth: usize,
        ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
            let script = {
                let mut shared = self.shared.lock().unwrap();
                shared.streams_started += 1;
                shared
                    .scripts
                    .pop_front()
                    .expect("test scripted enough streams")
            };
            Ok(match script {
                StreamScript::Healthy(payload) => {
                    engine_stream(SyntheticSource::new(vec![payload]), queue_depth, None)
                }
                StreamScript::Wedged => {
                    engine_stream(WedgedSource { stop: None }, queue_depth, None)
                }
                StreamScript::Dead => {
                    engine_stream(SyntheticSource::new(Vec::new()), queue_depth, None)
                }
            })
        }

        fn boxed_clone(&self) -> Box<dyn Transport> {
            Box::new(Self {
                shared: Arc::clone(&self.shared),
            })
        }

        fn reset_device(&mut self) -> Result<(), SdrError> {
            let mut shared = self.shared.lock().unwrap();
            match shared.reset {
                ResetScript::Works => {
                    shared.resets += 1;
                    Ok(())
                }
                ResetScript::Unsupported => Err(SdrError::Unsupported(
                    "no port access in this test transport".into(),
                )),
                ResetScript::ReopenRequired => Err(sdr_fox_core::session::reopen_required(
                    "test transport: the fd handle died with the reset",
                )),
            }
        }
    }

    /// A tuner that acks everything and logs what was programmed.
    struct StateTrackingTuner {
        inits: Arc<Mutex<usize>>,
        frequencies: Arc<Mutex<Vec<u64>>>,
        gains: Arc<Mutex<Vec<GainRequest>>>,
        modes: Arc<Mutex<Vec<GainMode>>>,
        kind: TunerKind,
    }

    impl Default for StateTrackingTuner {
        fn default() -> Self {
            Self {
                inits: Arc::default(),
                frequencies: Arc::default(),
                gains: Arc::default(),
                modes: Arc::default(),
                kind: TunerKind::R820T2,
            }
        }
    }

    impl Tuner for StateTrackingTuner {
        fn init(&mut self, _bus: &mut dyn TunerBus) -> Result<(), TunerError> {
            *self.inits.lock().unwrap() += 1;
            Ok(())
        }

        fn set_freq(&mut self, _bus: &mut dyn TunerBus, hz: u64) -> Result<(), TunerError> {
            self.frequencies.lock().unwrap().push(hz);
            Ok(())
        }

        fn set_bandwidth(&mut self, _bus: &mut dyn TunerBus, _hz: u32) -> Result<(), TunerError> {
            Ok(())
        }

        fn set_gain(
            &mut self,
            _bus: &mut dyn TunerBus,
            req: GainRequest,
        ) -> Result<(), TunerError> {
            self.gains.lock().unwrap().push(req);
            Ok(())
        }

        fn gains(&self) -> &[GainStep] {
            &[]
        }

        fn set_gain_mode(
            &mut self,
            _bus: &mut dyn TunerBus,
            mode: GainMode,
        ) -> Result<(), TunerError> {
            self.modes.lock().unwrap().push(mode);
            Ok(())
        }

        fn kind(&self) -> TunerKind {
            self.kind
        }
    }

    struct WedgeHarness {
        device: RtlSdr,
        shared: Arc<Mutex<ProbeShared>>,
        inits: Arc<Mutex<usize>>,
        frequencies: Arc<Mutex<Vec<u64>>>,
        gains: Arc<Mutex<Vec<GainRequest>>>,
        modes: Arc<Mutex<Vec<GainMode>>>,
    }

    fn wedge_harness(scripts: Vec<StreamScript>, reset: ResetScript) -> WedgeHarness {
        let shared = Arc::new(Mutex::new(ProbeShared {
            scripts: scripts.into_iter().collect(),
            reset,
            resets: 0,
            streams_started: 0,
            control_out: Vec::new(),
        }));
        let tuner = StateTrackingTuner::default();
        let inits = Arc::clone(&tuner.inits);
        let frequencies = Arc::clone(&tuner.frequencies);
        let gains = Arc::clone(&tuner.gains);
        let modes = Arc::clone(&tuner.modes);
        let descriptor = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::RtlSdr,
        };
        let mut device = RtlSdr::new(
            make_info(&descriptor, Some(TunerKind::R820T2)),
            Box::new(ProbeTransport {
                shared: Arc::clone(&shared),
            }),
            Box::new(tuner),
        );
        // The real deadline is deliberately generous (hundreds of ms); keep
        // the wedged-path tests fast without weakening what they prove.
        device.probe_timeout_override = Some(Duration::from_millis(50));
        WedgeHarness {
            device,
            shared,
            inits,
            frequencies,
            gains,
            modes,
        }
    }

    /// `USB_SYSCTL` write address — issued only by `init_baseband`, so its
    /// presence in the control log proves a full reinitialisation ran.
    const USB_SYSCTL_ADDR: u16 = 0x2000;
    /// Demod `wValue` for the sample-rate ratio high-half register (1, 0x9f).
    const RSAMP_RATIO_HI: u16 = (0x9f << 8) | 0x20;
    /// SYS block GPO register, written by the bias-T path.
    const GPO_ADDR: u16 = 0x3001;

    fn count_control_writes(shared: &Arc<Mutex<ProbeShared>>, addr: u16) -> usize {
        shared
            .lock()
            .unwrap()
            .control_out
            .iter()
            .filter(|(value, _, _)| *value == addr)
            .count()
    }

    /// SYS `DEMOD_CTL` register: `init_baseband` writes 0xe8 (power on),
    /// `deinit_baseband` writes 0x20 (power off). Counting the 0x20 payload
    /// isolates the power-down.
    const DEMOD_CTL_ADDR: u16 = 0x3000;

    fn count_power_down_writes(shared: &Arc<Mutex<ProbeShared>>) -> usize {
        shared
            .lock()
            .unwrap()
            .control_out
            .iter()
            .filter(|(value, _, data)| *value == DEMOD_CTL_ADDR && data.as_slice() == [0x20])
            .count()
    }

    #[test]
    fn healthy_stream_is_not_reset_and_probe_block_reaches_the_caller() {
        let mut harness = wedge_harness(
            vec![StreamScript::Healthy(vec![9, 8, 7, 6])],
            ResetScript::Works,
        );
        let mut stream = harness
            .device
            .start_stream(StreamConfig::default())
            .expect("healthy stream must start");

        let first = stream.recv().unwrap().unwrap();
        assert_eq!(
            first.sequence, 0,
            "the block consumed by the probe must be re-delivered, not dropped"
        );
        let IqSamples::Cu8(bytes) = first.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes, vec![9, 8, 7, 6]);
        let second = stream.recv().unwrap().unwrap();
        assert_eq!(second.sequence, 1, "the stream continues after the probe");

        assert_eq!(
            harness.shared.lock().unwrap().resets,
            0,
            "a healthy device must never be reset"
        );
        assert_eq!(harness.shared.lock().unwrap().streams_started, 1);
        assert_eq!(
            count_control_writes(&harness.shared, USB_SYSCTL_ADDR),
            0,
            "no reinitialisation on the healthy path"
        );
    }

    #[test]
    fn probed_first_block_is_still_format_converted() {
        let mut harness = wedge_harness(
            vec![StreamScript::Healthy(vec![128, 128, 0, 255])],
            ResetScript::Works,
        );
        let cfg = StreamConfig {
            format: IqFormat::Cf32,
            ..StreamConfig::default()
        };
        let mut stream = harness.device.start_stream(cfg).unwrap();
        let block = stream.recv().unwrap().unwrap();
        let IqSamples::Cf32(f) = block.samples else {
            panic!("the probed block must pass through format conversion");
        };
        assert!((f[2] + 1.0).abs() < 1e-6, "0 -> -1.0, got {}", f[2]);
        assert!((f[3] - 1.0).abs() < 1e-6, "255 -> +1.0, got {}", f[3]);
    }

    #[test]
    fn wedged_bulk_endpoint_is_recovered_reinitialised_and_retried_once() {
        // Config replay is tier-agnostic — both recovery tiers wipe every
        // register and share one reprogramming routine — so this exercises it
        // through the tier that now runs first and therefore runs most often.
        // Escalation to the port reset is covered by
        // `port_reset_is_escalated_to_only_after_the_power_cycle_fails`.
        let mut harness = wedge_harness(
            vec![
                StreamScript::Wedged,
                StreamScript::Healthy(vec![1, 2, 3, 4]),
            ],
            ResetScript::Works,
        );
        // Program user-visible state that the recovery must replay in full.
        harness.device.set_sample_rate(1_024_000).unwrap();
        harness.device.set_frequency(100_000_000).unwrap();
        harness.device.set_gain_mode(GainMode::Manual).unwrap();
        harness.device.set_gain(GainRequest::overall(280)).unwrap();
        harness.device.set_bias_tee(true).unwrap();

        let mut stream = harness
            .device
            .start_stream(StreamConfig::default())
            .expect("the retry after reset must succeed");
        let first = stream.recv().unwrap().unwrap();
        let IqSamples::Cu8(bytes) = first.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes, vec![1, 2, 3, 4], "retry data reaches the caller");

        assert_eq!(
            harness.shared.lock().unwrap().resets,
            0,
            "the power-cycle recovered the device, so the port must be left alone"
        );
        assert_eq!(
            harness.shared.lock().unwrap().streams_started,
            2,
            "exactly one retry"
        );
        // Full reinitialisation: baseband bring-up reran...
        assert_eq!(
            count_control_writes(&harness.shared, USB_SYSCTL_ADDR),
            1,
            "init_baseband must rerun after the recovery"
        );
        // ...the sample rate was reprogrammed (once explicitly, once by the
        // recovery)...
        assert_eq!(
            count_control_writes(&harness.shared, RSAMP_RATIO_HI),
            2,
            "sample rate must be reprogrammed after the reset"
        );
        // ...bias-T was reprogrammed...
        assert_eq!(
            count_control_writes(&harness.shared, GPO_ADDR),
            2,
            "bias-T must be reprogrammed after the reset"
        );
        // ...and the tuner was fully reinitialised and reconfigured.
        assert_eq!(
            *harness.inits.lock().unwrap(),
            1,
            "tuner init must rerun after the reset"
        );
        assert_eq!(
            harness.frequencies.lock().unwrap().as_slice(),
            &[100_000_000, 100_000_000],
            "centre frequency must be retuned after the reset"
        );
        assert_eq!(
            harness.gains.lock().unwrap().as_slice(),
            &[GainRequest::overall(280), GainRequest::overall(280)],
            "gain must be reprogrammed after the reset"
        );
        assert_eq!(
            harness.modes.lock().unwrap().as_slice(),
            &[GainMode::Manual, GainMode::Manual],
            "gain mode must be reprogrammed after the reset"
        );
    }

    #[test]
    fn unsupported_reset_surfaces_the_stream_not_an_error() {
        // Three streams: the initial wedge, the demod power-cycle's retry
        // (also wedged, so the ladder escalates), and the unrecovered stream
        // handed back when the transport cannot reach the port.
        let mut harness = wedge_harness(
            vec![
                StreamScript::Wedged,
                StreamScript::Wedged,
                StreamScript::Healthy(vec![5, 5]),
            ],
            ResetScript::Unsupported,
        );
        let mut stream = harness
            .device
            .start_stream(StreamConfig::default())
            .expect("an unresettable transport must not fail the stream");
        let block = stream.recv().unwrap().unwrap();
        let IqSamples::Cu8(bytes) = block.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes, vec![5, 5]);

        assert_eq!(
            harness.shared.lock().unwrap().resets,
            0,
            "unsupported reset must not be retried"
        );
        assert_eq!(harness.shared.lock().unwrap().streams_started, 3);
        assert_eq!(
            count_control_writes(&harness.shared, USB_SYSCTL_ADDR),
            1,
            "exactly one reinitialisation — the demod power-cycle's, which is \
             mandatory because cutting demod power wipes every register. The \
             unreachable port reset adds none of its own"
        );
        assert_eq!(
            *harness.inits.lock().unwrap(),
            1,
            "the tuner is reinitialised once by the demod power-cycle; the \
             unreachable port reset must not touch it again"
        );
    }

    #[test]
    fn wedged_retry_fails_with_a_clear_error_and_no_reset_loop() {
        // Wedged through every tier: the initial stream, the demod
        // power-cycle's retry, and the post-reset retry.
        let mut harness = wedge_harness(
            vec![
                StreamScript::Wedged,
                StreamScript::Wedged,
                StreamScript::Wedged,
            ],
            ResetScript::Works,
        );
        let Err(error) = harness.device.start_stream(StreamConfig::default()) else {
            panic!("silence after the reset must be fatal");
        };
        assert!(
            matches!(&error, SdrError::Transport(msg) if msg.contains("bulk endpoint")),
            "the failure must name the bulk endpoint; got: {error:?}"
        );
        assert_eq!(
            harness.shared.lock().unwrap().resets,
            1,
            "reset exactly once, never a loop"
        );
        assert_eq!(
            harness.shared.lock().unwrap().streams_started,
            3,
            "exactly one retry per tier, never a loop"
        );
    }

    #[test]
    fn terminal_stream_error_is_surfaced_without_reset() {
        let mut harness = wedge_harness(vec![StreamScript::Dead], ResetScript::Works);
        let Err(error) = harness.device.start_stream(StreamConfig::default()) else {
            panic!("a dead stream must fail start_stream");
        };
        assert!(
            matches!(error, SdrError::DeviceLost),
            "terminal errors pass through unchanged; got: {error:?}"
        );
        assert_eq!(
            harness.shared.lock().unwrap().resets,
            0,
            "a fatal transfer error is not the wedge signature"
        );
    }

    // === Hardware-session lifetime (power-down vs. live streams) ===

    #[test]
    fn stream_outliving_its_device_keeps_the_hardware_powered() {
        let harness = wedge_harness(
            vec![StreamScript::Healthy(vec![7, 7, 7, 7])],
            ResetScript::Works,
        );
        let WedgeHarness {
            mut device, shared, ..
        } = harness;
        let mut stream = device
            .start_stream(StreamConfig::default())
            .expect("healthy stream must start");
        let first = stream.recv().unwrap().unwrap();
        assert_eq!(first.sequence, 0);

        // Closing the device handle while the stream lives must NOT power the
        // chip down — the C ABI (and Python/JNI) registries hand out device
        // and stream handles independently, so this drop order is routine.
        drop(device);
        assert_eq!(
            count_power_down_writes(&shared),
            0,
            "device close powered the demod down underneath a live stream"
        );

        // The stream must still deliver data from the powered-up radio.
        let block = stream.recv().unwrap().unwrap();
        let IqSamples::Cu8(bytes) = block.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes, vec![7, 7, 7, 7]);

        // Releasing the last owner powers the chip down exactly once.
        drop(stream);
        assert_eq!(
            count_power_down_writes(&shared),
            1,
            "the power-down must run exactly once, after the last owner"
        );
    }

    #[test]
    fn power_down_waits_for_the_device_when_the_stream_drops_first() {
        let harness = wedge_harness(
            vec![StreamScript::Healthy(vec![1, 2, 3, 4])],
            ResetScript::Works,
        );
        let WedgeHarness {
            mut device, shared, ..
        } = harness;
        let stream = device
            .start_stream(StreamConfig::default())
            .expect("healthy stream must start");

        // Dropping the stream first must not power down either: the device
        // handle is still live and may start another stream.
        drop(stream);
        assert_eq!(
            count_power_down_writes(&shared),
            0,
            "stream close powered down a device handle that is still in use"
        );

        drop(device);
        assert_eq!(
            count_power_down_writes(&shared),
            1,
            "the power-down must run exactly once, when the device drops last"
        );
    }

    #[test]
    fn closing_an_unstreamed_device_still_powers_the_chip_down() {
        // Regression guard for session teardown: with no streams, dropping
        // the device is dropping the last session owner, and the chip must not
        // be left running (it can return with a silent bulk endpoint).
        let harness = wedge_harness(Vec::new(), ResetScript::Works);
        let WedgeHarness { device, shared, .. } = harness;
        drop(device);
        assert_eq!(
            count_power_down_writes(&shared),
            1,
            "a device closed without streaming must still power the demod down"
        );
    }

    // === Android reset protocol (handle dies with the reset) ===

    #[test]
    fn reopen_required_reset_aborts_recovery_and_surfaces_the_condition() {
        // The Android fd transport issues the reset (clearing the hardware
        // wedge) but cannot survive it; it reports the re-open condition
        // instead of Ok. The driver must NOT re-initialise or retry against
        // that dead handle — it surfaces the condition for the app to act on.
        //
        // Two wedged streams are scripted because the port reset is now the
        // SECOND recovery tier: the handle-preserving demod power-cycle is
        // tried first and gets its own retry.
        let mut harness = wedge_harness(
            vec![StreamScript::Wedged, StreamScript::Wedged],
            ResetScript::ReopenRequired,
        );
        let Err(error) = harness.device.start_stream(StreamConfig::default()) else {
            panic!("a reset that kills the handle must fail the stream start");
        };
        assert!(
            sdr_fox_core::session::is_reopen_required(&error),
            "the re-open condition must reach the caller unchanged; got: {error:?}"
        );
        assert_eq!(
            harness.shared.lock().unwrap().streams_started,
            2,
            "the power-cycle gets one retry; nothing may be retried after the reset \
             killed the handle"
        );
        assert_eq!(
            count_control_writes(&harness.shared, USB_SYSCTL_ADDR),
            1,
            "exactly one reinitialisation — the power-cycle's. Nothing may be \
             reprogrammed against a handle that did not survive the reset"
        );
        assert_eq!(
            *harness.inits.lock().unwrap(),
            1,
            "the tuner is initialised once by the power-cycle and never again \
             once the handle died with the reset"
        );
    }

    #[test]
    fn wedge_clearing_after_a_demod_power_cycle_never_touches_the_usb_port() {
        // The failure this tier exists for: the bulk endpoint is dead while
        // control transfers still work, and cutting demod power clears it.
        // Resetting the port would also work on desktop, but it is terminal
        // for the Android fd handle — so it must not be reached when the
        // cheap, handle-preserving tier suffices.
        let mut harness = wedge_harness(
            vec![
                StreamScript::Wedged,
                StreamScript::Healthy(vec![4, 3, 2, 1]),
            ],
            ResetScript::Works,
        );
        let mut stream = harness
            .device
            .start_stream(StreamConfig::default())
            .expect("the power-cycle must recover the stream");

        let first = stream.recv().unwrap().unwrap();
        assert_eq!(
            first.sequence, 0,
            "the block consumed by the post-recovery probe must be re-delivered"
        );
        let IqSamples::Cu8(bytes) = first.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes, vec![4, 3, 2, 1]);

        assert_eq!(
            harness.shared.lock().unwrap().resets,
            0,
            "the USB port must NOT be reset when the demod power-cycle recovers \
             the device — on Android that reset is terminal for the handle"
        );
        assert!(
            count_power_down_writes(&harness.shared) >= 1,
            "the recovery must actually cut demod power; re-running init over a \
             still-running demod does not clear this wedge"
        );
        assert_eq!(harness.shared.lock().unwrap().streams_started, 2);
    }

    #[test]
    fn port_reset_is_escalated_to_only_after_the_power_cycle_fails() {
        // A wedge the power-cycle cannot clear must still reach the port
        // reset — the cheap tier is added ahead of it, not in place of it.
        let mut harness = wedge_harness(
            vec![
                StreamScript::Wedged,
                StreamScript::Wedged,
                StreamScript::Healthy(vec![7, 7]),
            ],
            ResetScript::Works,
        );
        let mut stream = harness
            .device
            .start_stream(StreamConfig::default())
            .expect("the port reset must still recover a power-cycle-proof wedge");
        assert!(stream.recv().unwrap().is_ok());

        assert!(
            count_power_down_writes(&harness.shared) >= 1,
            "the power-cycle must be attempted BEFORE the port reset"
        );
        assert_eq!(
            harness.shared.lock().unwrap().resets,
            1,
            "exactly one port reset, and only after the cheap tier failed"
        );
        assert_eq!(harness.shared.lock().unwrap().streams_started, 3);
        assert_eq!(
            *harness.inits.lock().unwrap(),
            2,
            "one reinitialisation per recovery tier"
        );
    }

    // === Zero-IF bandwidth policy (E4000) ===

    #[test]
    fn e4000_bandwidth_change_skips_demod_if_reprogram_and_retune() {
        // The E4000 is direct-conversion: a bandwidth change is entirely a
        // tuner-filter change. Unlike the R82xx path, it must NOT reprogram
        // the demod IF registers (1, 0x19..0x1b) and must NOT retune the
        // centre frequency.
        let shared = Arc::new(Mutex::new(ProbeShared {
            scripts: VecDeque::new(),
            reset: ResetScript::Works,
            resets: 0,
            streams_started: 0,
            control_out: Vec::new(),
        }));
        let tuner = StateTrackingTuner {
            kind: TunerKind::E4000,
            ..StateTrackingTuner::default()
        };
        let frequencies = Arc::clone(&tuner.frequencies);
        let descriptor = DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::RtlSdr,
        };
        let mut device = RtlSdr::new(
            make_info(&descriptor, Some(TunerKind::E4000)),
            Box::new(ProbeTransport {
                shared: Arc::clone(&shared),
            }),
            Box::new(tuner),
        );
        device.set_frequency(100_000_000).unwrap();
        device.set_bandwidth(2_000_000).unwrap();

        assert_eq!(
            frequencies.lock().unwrap().as_slice(),
            &[100_000_000],
            "a zero-IF bandwidth change must not retune the centre frequency"
        );
        for addr in [0x19u16, 0x1a, 0x1b] {
            let value = (addr << 8) | 0x20;
            assert_eq!(
                shared
                    .lock()
                    .unwrap()
                    .control_out
                    .iter()
                    .filter(|(v, _, _)| *v == value)
                    .count(),
                0,
                "a zero-IF bandwidth change must not write demod IF register 0x{addr:02x}"
            );
        }
    }

    #[test]
    fn first_block_timeout_scales_with_rate_and_buffer_size() {
        // 2.048 MS/s, 64 KiB: 32,768 samples -> 16 ms fill; 4x + 500 ms fixed.
        assert_eq!(
            first_block_timeout(2_048_000, 65_536),
            Duration::from_millis(564)
        );
        // 225,001 S/s (bottom of the valid range), 64 KiB: 146 ms fill
        // (rounded up) -> 4 * 146 + 500 = 1084 ms. A single hardcoded bound
        // tuned for 2.048 MS/s would be wrong here.
        assert_eq!(
            first_block_timeout(225_001, 65_536),
            Duration::from_millis(1084)
        );
        // The bound must dominate the worst-case fill time with real headroom.
        let fill_ms = 32_768_u64 * 1000 / 225_001;
        assert!(first_block_timeout(225_001, 65_536) > Duration::from_millis(4 * fill_ms));
        // Bigger buffers stretch the bound rather than starving it.
        assert!(first_block_timeout(2_048_000, 262_144) > first_block_timeout(2_048_000, 65_536));
    }
}
