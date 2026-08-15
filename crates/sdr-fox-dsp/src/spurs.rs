//! Coherent cancellation of narrowband spurs ("birdies").
//!
//! A receiver's own reference oscillator radiates harmonics that its front
//! end then receives. On a 28.8 MHz RTL-SDR these land every 28.8 MHz —
//! 144.000, 403.200, 432.000, 460.800 MHz and so on — and a measured example
//! sat 30 dB above the noise floor with an antenna attached, which is fatal
//! for anything narrowband sharing the frequency.
//!
//! The property that makes them removable rather than merely notchable is
//! **coherence**: the harmonic and the ADC sample clock derive from the same
//! crystal, so after downconversion the spur sits at a normalized frequency
//! that is an exact rational number. It does not drift with temperature the
//! way a real signal does — a measured spur held to within 0.014 Hz over
//! 40 minutes of warm-up. Its linewidth is likewise unresolvable: under 1 Hz
//! at a 2-second observation.
//!
//! So each spur can be modelled as a single complex exponential and removed
//! by projection: for every block, estimate the spur's complex amplitude by
//! correlating against a unit phasor at its frequency, then subtract that
//! component. This costs one basis vector out of `N`, so at a 8192-sample
//! block it removes 0.01% of the signal's energy — against a notch filter,
//! which must be wide enough to cover its own transition band and imposes
//! group delay across it.
//!
//! Cancelling a spur that is not actually present is harmless: the projection
//! then measures noise, and subtracting it removes a single degree of freedom.
//!
//! ```no_run
//! use sdr_fox_dsp::spurs::SpurCanceller;
//!
//! // 2.048 MS/s centred on 403.0 MHz, cancelling the 28.8 MHz reference comb.
//! let mut sc = SpurCanceller::new(2_048_000.0);
//! sc.add_reference_harmonics(28_800_000.0, 403_000_000.0);
//! # let mut iq: Vec<f32> = Vec::new();
//! sc.acquire(&iq);      // one-time fine frequency estimate
//! sc.process(&mut iq);  // in-place, call per block
//! ```

use std::f64::consts::TAU;

/// Default correlation block, in complex samples, used by [`SpurCanceller::acquire`]
/// when the caller does not supply one. Large enough that the projection is a
/// tight estimate and small enough to stay cheap.
const DEFAULT_ACQUIRE_BLOCK: usize = 1 << 17;

/// Fewest complex samples [`SpurCanceller::acquire`] will estimate from.
/// Below this the coarse scan's step is wider than the error it is correcting,
/// so the estimate would be worse than the nominal it started from.
pub const MIN_ACQUIRE_PAIRS: usize = 1024;

/// One tracked spur.
#[derive(Debug, Clone)]
struct Spur {
    /// Baseband offset from the tuned centre, Hz. Refined by [`SpurCanceller::acquire`].
    freq_hz: f64,
    /// Running phase in cycles, wrapped to `[0, 1)`. Kept in cycles rather
    /// than radians, and wrapped every block, so a long-running stream never
    /// loses phase precision to a growing sample counter.
    phase_cycles: f64,
}

/// Enumerate the harmonics of a reference clock that fall inside a tuned band.
///
/// Returns each harmonic's **baseband offset** from `center_hz`, in Hz, for
/// every integer multiple of `reference_hz` inside
/// `center_hz ± sample_rate / 2`. Harmonics land at multiples of the nominal
/// reference even when the physical crystal is off-frequency: the same crystal
/// sets the frequency scale, so the error cancels and the harmonic appears at
/// its nominal multiple regardless. Residual error comes only from tuner PLL
/// quantization and is on the order of tens of Hz, which [`SpurCanceller::acquire`]
/// then removes.
///
/// Returns an empty vector for non-finite or non-positive inputs.
#[must_use]
pub fn reference_harmonics(reference_hz: f64, center_hz: f64, sample_rate: f64) -> Vec<f64> {
    if !(reference_hz.is_finite() && center_hz.is_finite() && sample_rate.is_finite())
        || reference_hz <= 0.0
        || sample_rate <= 0.0
        || center_hz <= 0.0
    {
        return Vec::new();
    }
    let half = sample_rate / 2.0;
    let lo = center_hz - half;
    let hi = center_hz + half;
    let first = (lo / reference_hz).ceil().max(1.0);
    let last = (hi / reference_hz).floor();
    if last < first {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut n = first;
    while n <= last {
        out.push(n * reference_hz - center_hz);
        n += 1.0;
    }
    out
}

/// Removes one or more coherent narrowband spurs from an interleaved cf32 IQ
/// stream by least-squares projection.
///
/// Unlike the filters in [`crate::filters`], the output **does** depend on how
/// the stream is chunked: the spur's amplitude is estimated once per call to
/// [`process`](Self::process), so a longer block yields a tighter estimate.
/// Buffering internally to make chunking irrelevant would cost a full block of
/// latency and buy nothing, since a caller feeding consistent blocks already
/// gets consistent output. Feed at least a few thousand samples per call.
#[derive(Debug, Clone)]
pub struct SpurCanceller {
    sample_rate: f64,
    spurs: Vec<Spur>,
}

impl SpurCanceller {
    /// Build an empty canceller for a stream at `sample_rate` Hz.
    #[must_use]
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            spurs: Vec::new(),
        }
    }

    /// Build a canceller for every harmonic of a device's reference clock that
    /// is visible in the tuned band — the one-liner for the common case.
    ///
    /// Pair with `SdrDevice::reference_clock_hz`, which reports the reference
    /// frequency so it need not be hardcoded:
    ///
    /// ```no_run
    /// # use sdr_fox_dsp::spurs::SpurCanceller;
    /// # let (reference, center, rate) = (28_800_000.0, 403_000_000.0, 2_048_000.0);
    /// let mut sc = SpurCanceller::for_reference(reference, center, rate);
    /// ```
    ///
    /// Yields an empty canceller — a no-op — when no harmonic falls in band,
    /// so callers need not test for that case.
    #[must_use]
    pub fn for_reference(reference_hz: f64, center_hz: f64, sample_rate: f64) -> Self {
        let mut this = Self::new(sample_rate);
        this.add_reference_harmonics(reference_hz, center_hz);
        this
    }

    /// Track a spur at `offset_hz` from the tuned centre (may be negative).
    ///
    /// Offsets outside `±sample_rate / 2` are ignored: they are not present in
    /// the sampled band, and admitting them would alias onto a real frequency
    /// and delete signal there.
    pub fn add_spur(&mut self, offset_hz: f64) {
        if !offset_hz.is_finite() || offset_hz.abs() > self.sample_rate / 2.0 {
            return;
        }
        self.spurs.push(Spur {
            freq_hz: offset_hz,
            phase_cycles: 0.0,
        });
    }

    /// Track every harmonic of `reference_hz` visible in the band centred on
    /// `center_hz`, per [`reference_harmonics`].
    pub fn add_reference_harmonics(&mut self, reference_hz: f64, center_hz: f64) {
        for offset in reference_harmonics(reference_hz, center_hz, self.sample_rate) {
            self.add_spur(offset);
        }
    }

    /// Number of spurs being tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spurs.len()
    }

    /// Whether no spurs are being tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spurs.is_empty()
    }

    /// The current frequency estimates, as baseband offsets in Hz.
    #[must_use]
    pub fn frequencies(&self) -> Vec<f64> {
        self.spurs.iter().map(|s| s.freq_hz).collect()
    }

    /// Refine every spur's frequency against a captured block.
    ///
    /// The nominal harmonic frequency is only accurate to the tuner's PLL
    /// quantization — tens of Hz in practice — which is enough to leave a
    /// visible residual after subtraction. This does a coarse scan over
    /// ±`sample_rate / block` around the nominal, then a phase-slope refinement
    /// between the two halves of the block, which resolves the frequency far
    /// below the scan's own step.
    ///
    /// Cheap enough to run once at stream start; the spur does not drift.
    ///
    /// Returns `false` and leaves the estimates untouched when `iq` holds
    /// fewer than [`MIN_ACQUIRE_PAIRS`] complex samples, which is too few to
    /// estimate from. Callers driving this from a live stream should retry on
    /// the next block rather than treat acquisition as done.
    pub fn acquire(&mut self, iq: &[f32]) -> bool {
        let pairs = iq.len() / 2;
        if pairs < MIN_ACQUIRE_PAIRS {
            return false;
        }
        let n = pairs.min(DEFAULT_ACQUIRE_BLOCK);
        let iq = &iq[..n * 2];
        let sample_rate = self.sample_rate;

        for spur in &mut self.spurs {
            // Coarse: maximise |projection| over a window one FFT-bin wide
            // either side of the nominal, which comfortably brackets the PLL
            // quantization error.
            let span = sample_rate / n as f64;
            let steps = 32i32;
            let mut best = (f64::NEG_INFINITY, spur.freq_hz);
            for k in -steps..=steps {
                let f = spur.freq_hz + span * f64::from(k) / f64::from(steps);
                let (re, im) = project(iq, f / sample_rate, 0.0);
                let mag = re * re + im * im;
                if mag > best.0 {
                    best = (mag, f);
                }
            }
            let coarse = best.1;

            // Fine: the projection over each half-block rotates at exactly the
            // residual frequency error, so the phase step between halves gives
            // it directly. Unambiguous while |error| < sample_rate / n, which
            // the coarse scan guarantees.
            let half = n / 2;
            let f_norm = coarse / sample_rate;
            let (r1, i1) = project(&iq[..half * 2], f_norm, 0.0);
            let start_phase = (f_norm * half as f64).fract();
            let (r2, i2) = project(&iq[half * 2..], f_norm, start_phase);
            // arg(a2 * conj(a1))
            let cross_re = r2 * r1 + i2 * i1;
            let cross_im = i2 * r1 - r2 * i1;
            if cross_re != 0.0 || cross_im != 0.0 {
                let dphi = cross_im.atan2(cross_re);
                let dt = half as f64 / sample_rate;
                spur.freq_hz = coarse + dphi / (TAU * dt);
            } else {
                spur.freq_hz = coarse;
            }
            spur.phase_cycles = 0.0;
        }
        true
    }

    /// Cancel every tracked spur from `iq` in place.
    ///
    /// `iq` is interleaved cf32. Phase is carried across calls, so a spur is
    /// removed continuously across block boundaries rather than restarting each
    /// block. A trailing odd sample cannot form an IQ pair and is left alone.
    pub fn process(&mut self, iq: &mut [f32]) {
        debug_assert!(iq.len() % 2 == 0, "IQ must be interleaved pairs");
        let pairs = iq.len() / 2;
        if pairs == 0 || self.spurs.is_empty() {
            return;
        }
        let iq = &mut iq[..pairs * 2];

        for spur in &mut self.spurs {
            let f_norm = spur.freq_hz / self.sample_rate;
            let (re, im) = project(iq, f_norm, spur.phase_cycles);

            // Subtract amp * exp(j2*pi*f*n), continuing the phase ramp.
            let step = TAU * f_norm;
            let (sin_step, cos_step) = step.sin_cos();
            let start = TAU * spur.phase_cycles;
            let (mut sin_p, mut cos_p) = start.sin_cos();
            for k in 0..pairs {
                iq[2 * k] -= (re * cos_p - im * sin_p) as f32;
                iq[2 * k + 1] -= (re * sin_p + im * cos_p) as f32;
                // Rotate the phasor one step. Restarted from an exact sin_cos
                // every block, so incremental drift never accumulates beyond
                // a single block's worth.
                let next_cos = cos_p * cos_step - sin_p * sin_step;
                sin_p = sin_p * cos_step + cos_p * sin_step;
                cos_p = next_cos;
            }

            // Advance the phase by exactly this block, wrapped to [0, 1) so a
            // long stream keeps full precision.
            spur.phase_cycles = (spur.phase_cycles + f_norm * pairs as f64).rem_euclid(1.0);
        }
    }
}

/// Correlate interleaved cf32 `iq` against `exp(-j2*pi*(f_norm*n + phase0))`,
/// returning the mean complex amplitude as `(re, im)`.
///
/// Accumulates in `f64`: the sum runs over the whole block and an `f32`
/// accumulator would lose the small residual that matters once the spur is
/// mostly cancelled.
fn project(iq: &[f32], f_norm: f64, phase0: f64) -> (f64, f64) {
    let pairs = iq.len() / 2;
    if pairs == 0 {
        return (0.0, 0.0);
    }
    let step = TAU * f_norm;
    let (sin_step, cos_step) = step.sin_cos();
    let (mut sin_p, mut cos_p) = (TAU * phase0).sin_cos();
    let mut acc_re = 0.0f64;
    let mut acc_im = 0.0f64;
    for k in 0..pairs {
        let i = f64::from(iq[2 * k]);
        let q = f64::from(iq[2 * k + 1]);
        // (i + jq) * conj(cos_p + j sin_p)
        acc_re += i * cos_p + q * sin_p;
        acc_im += q * cos_p - i * sin_p;
        let next_cos = cos_p * cos_step - sin_p * sin_step;
        sin_p = sin_p * cos_step + cos_p * sin_step;
        cos_p = next_cos;
    }
    let n = pairs as f64;
    (acc_re / n, acc_im / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, f_norm: f64, amp: f64, phase: f64) -> Vec<f32> {
        (0..n)
            .flat_map(|k| {
                let p = TAU * (f_norm * k as f64 + phase);
                [(amp * p.cos()) as f32, (amp * p.sin()) as f32]
            })
            .collect()
    }

    fn power(iq: &[f32]) -> f64 {
        iq.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / (iq.len() / 2) as f64
    }

    #[test]
    fn harmonics_of_28m8_in_a_403mhz_band() {
        // 2.048 MS/s at 403.0 MHz spans 401.976..404.024, containing only
        // 14 x 28.8 = 403.2 MHz.
        let h = reference_harmonics(28_800_000.0, 403_000_000.0, 2_048_000.0);
        assert_eq!(h.len(), 1);
        assert!((h[0] - 200_000.0).abs() < 1e-6);
    }

    #[test]
    fn harmonics_empty_when_none_in_band() {
        // 410 MHz +- 1.024 MHz contains no multiple of 28.8 MHz.
        assert!(reference_harmonics(28_800_000.0, 410_000_000.0, 2_048_000.0).is_empty());
    }

    #[test]
    fn harmonics_reject_bad_input() {
        assert!(reference_harmonics(0.0, 403e6, 2.048e6).is_empty());
        assert!(reference_harmonics(f64::NAN, 403e6, 2.048e6).is_empty());
        assert!(reference_harmonics(28.8e6, 403e6, -1.0).is_empty());
    }

    #[test]
    fn cancels_a_pure_tone_to_the_noise_floor() {
        let n = 8192;
        let f_norm = 0.09765625; // 200 kHz at 2.048 MS/s, exactly on a bin
        let mut iq = tone(n, f_norm, 1.0, 0.31);
        let before = power(&iq);

        let mut sc = SpurCanceller::new(2_048_000.0);
        sc.add_spur(200_000.0);
        sc.process(&mut iq);

        // The residual floor is set by f32 storage of the input, not by the
        // algorithm; 1e-12 is 120 dB of cancellation and leaves ample room
        // above that floor for platform rounding differences.
        let after = power(&iq);
        assert!(
            after / before < 1e-12,
            "expected total cancellation, got {:.3e}",
            after / before
        );
    }

    #[test]
    fn phase_is_continuous_across_blocks() {
        // Cancelling in two halves must match cancelling in one pass; if the
        // phase did not carry, the second block would restart at zero phase
        // and leave a large residual.
        let n = 8192;
        let f_norm = 0.037_109_375;
        let full = tone(n, f_norm, 1.0, 0.77);

        let mut one = full.clone();
        let mut sc = SpurCanceller::new(2_048_000.0);
        sc.add_spur(f_norm * 2_048_000.0);
        sc.process(&mut one);

        let mut two = full;
        let mut sc2 = SpurCanceller::new(2_048_000.0);
        sc2.add_spur(f_norm * 2_048_000.0);
        let (a, b) = two.split_at_mut(n);
        sc2.process(a);
        sc2.process(b);

        // Without phase carry the second block would restart at zero phase and
        // leave a residual comparable to the original tone, so this threshold
        // is the whole point of the test.
        assert!(power(&two) < 1e-12, "chunked residual {:.3e}", power(&two));
        assert!((power(&one) - power(&two)).abs() < 1e-12);
    }

    #[test]
    fn acquire_finds_an_offset_frequency() {
        // Nominal 200 kHz, actual 200 kHz + 37 Hz: the PLL-quantization case.
        let n = 1 << 16;
        let rate = 2_048_000.0;
        let actual = 200_037.0;
        let mut iq = tone(n, actual / rate, 1.0, 0.11);

        let mut sc = SpurCanceller::new(rate);
        sc.add_spur(200_000.0);
        sc.acquire(&iq);
        assert!(
            (sc.frequencies()[0] - actual).abs() < 0.5,
            "acquired {:?}",
            sc.frequencies()
        );

        sc.process(&mut iq);
        assert!(
            power(&iq) < 1e-6,
            "residual after acquire {:.3e}",
            power(&iq)
        );
    }

    #[test]
    fn leaves_a_clean_signal_essentially_intact() {
        // A tone well away from the spur must survive: the projection removes
        // one basis vector out of n, so the loss is ~1/n of the energy.
        let n = 8192;
        let mut iq = tone(n, 0.25, 1.0, 0.0);
        let before = power(&iq);
        let mut sc = SpurCanceller::new(2_048_000.0);
        sc.add_spur(200_000.0);
        sc.process(&mut iq);
        let loss_db = 10.0 * (power(&iq) / before).log10();
        assert!(loss_db > -0.01, "unexpected loss {loss_db:.4} dB");
    }

    #[test]
    fn rejects_out_of_band_spurs() {
        let mut sc = SpurCanceller::new(2_048_000.0);
        sc.add_spur(2_000_000.0); // beyond +-1.024 MHz
        sc.add_spur(f64::NAN);
        assert!(sc.is_empty());
    }

    #[test]
    fn empty_canceller_is_a_no_op() {
        let mut iq = tone(64, 0.1, 1.0, 0.0);
        let copy = iq.clone();
        SpurCanceller::new(2_048_000.0).process(&mut iq);
        assert_eq!(iq, copy);
    }
}
