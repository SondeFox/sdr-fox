//! Demodulators: WBFM, NBFM, AM, USB, LSB.
//!
//! Each is a streaming demod: feed complex f32 IQ, get real f32 audio out.
//! FM uses the polar discriminator; AM uses the envelope; SSB uses phasing
//! with a Hilbert FIR.

use crate::filters::{ComplexLowPass, DcBlocker, Deemphasis, LowPass};

/// A streaming FM demodulator. Feed cf32 IQ at `input_rate`, get f32 audio at
/// `output_rate`. Pipeline: polar discriminate (input rate) → decimating
/// low-pass → deemphasis (optional) → DC block. The discriminator is normalized
/// back to its input-rate phase-step convention after channel decimation, so
/// adding the channel filter does not change public output gain. All state is
/// persistent so the demod is truly streaming.
pub struct FmDemod {
    prev_i: f32,
    prev_q: f32,
    /// Optional complex channel filter/decimator. Filtering before the FM
    /// non-linearity prevents adjacent channels from intermodulating into the
    /// wanted audio and reduces the discriminator rate.
    channel_filter: ChannelFilter,
    /// Convert phase steps measured at `discriminator_rate` back to the phase
    /// step that the pre-channelizer implementation emitted at `input_rate`.
    discriminator_scale: f32,
    /// Decimating low-pass: discriminates at the input rate, decimates to
    /// `output_rate`. Holds the persistent FIR history + decimation phase.
    audio_lp: LowPass,
    /// Optional fractional-rate correction after integer decimation. This is
    /// needed for SDR rates such as Airspy's 2.5/10 MS/s, which are not exact
    /// multiples of common audio rates.
    rate_adjust: Option<CubicFarrow>,
    /// Optional deemphasis (None for NBFM-without-deemphasis).
    deemph: Option<Deemphasis>,
    dc_blocker: DcBlocker,
}

impl FmDemod {
    /// Build a WBFM demod. `input_rate` is the SDR sample rate; `output_rate`
    /// is the desired audio rate (e.g. 48000). `bandwidth` is the FM channel
    /// bandwidth to pass (e.g. 180_000 for broadcast WBFM). Deemphasis is the
    /// US 75µs broadcast curve.
    ///
    /// # Panics
    ///
    /// Panics unless both rates are finite, positive, and
    /// `input_rate >= output_rate`.
    #[must_use]
    pub fn new(input_rate: f32, output_rate: f32, bandwidth: f32) -> Self {
        let (channel_filter, discriminator_rate) =
            make_channel_filter(input_rate, output_rate, bandwidth);
        let (audio_decimation, final_integer_rate) =
            decimation_plan(discriminator_rate, output_rate);
        // Audio anti-alias cutoff: keep below the new output Nyquist, capped at
        // 15 kHz (broadcast FM audio ceiling). The old 90 kHz cutoff aliasing
        // into a 48 kHz output was a bug.
        let cutoff = bandwidth
            .min(15_000.0)
            .min(output_rate / 2.0 - 1_000.0)
            .max(500.0);
        Self {
            prev_i: 0.0,
            prev_q: 0.0,
            channel_filter,
            discriminator_scale: discriminator_rate / input_rate,
            audio_lp: LowPass::new(
                cutoff,
                discriminator_rate,
                audio_decimation,
                anti_alias_taps(audio_decimation),
            ),
            rate_adjust: CubicFarrow::if_needed(final_integer_rate, output_rate),
            deemph: Some(Deemphasis::us_fm(output_rate)),
            dc_blocker: DcBlocker::new(0.01),
        }
    }

    /// Build an FM demod with explicit deemphasis control.
    /// `deemphasis_us = None` disables deemphasis (e.g. NBFM voice). A value
    /// sets the time constant (75e-6 US, 50e-6 EU, etc.).
    ///
    /// # Panics
    ///
    /// Panics unless both rates are finite, positive, and
    /// `input_rate >= output_rate`.
    #[must_use]
    pub fn with_deemphasis(
        input_rate: f32,
        output_rate: f32,
        bandwidth: f32,
        deemphasis_us: Option<f32>,
    ) -> Self {
        let (channel_filter, discriminator_rate) =
            make_channel_filter(input_rate, output_rate, bandwidth);
        let (audio_decimation, final_integer_rate) =
            decimation_plan(discriminator_rate, output_rate);
        let cutoff = bandwidth.min(output_rate / 2.0 - 1_000.0).max(500.0);
        let deemph = deemphasis_us.map(|tau_us| Deemphasis::new(tau_us * 1e-6, output_rate));
        Self {
            prev_i: 0.0,
            prev_q: 0.0,
            channel_filter,
            discriminator_scale: discriminator_rate / input_rate,
            audio_lp: LowPass::new(
                cutoff,
                discriminator_rate,
                audio_decimation,
                anti_alias_taps(audio_decimation),
            ),
            rate_adjust: CubicFarrow::if_needed(final_integer_rate, output_rate),
            deemph,
            dc_blocker: DcBlocker::new(0.01),
        }
    }

    /// Demodulate a block of interleaved cf32 IQ. Returns audio samples.
    pub fn process(&mut self, iq: &[f32]) -> Vec<f32> {
        debug_assert!(iq.len() % 2 == 0, "IQ must be interleaved pairs");
        let Self {
            prev_i,
            prev_q,
            channel_filter,
            discriminator_scale,
            audio_lp,
            rate_adjust,
            deemph,
            dc_blocker,
        } = self;
        let channelized = channel_filter.process(iq);
        let discriminator_input = channelized.as_deref().unwrap_or(iq);
        // Feed discriminator values straight into the decimating FIR. This
        // retains every input sample for anti-aliasing while eliminating the
        // former full-rate `disc` allocation.
        let discriminator = discriminator_input.chunks_exact(2).map(|pair| {
            let cr = pair[0];
            let cj = pair[1];
            let real = cr * *prev_i + cj * *prev_q;
            let imag = cj * *prev_i - cr * *prev_q;
            *prev_i = cr;
            *prev_q = cj;
            imag.atan2(real)
        });
        let decimated = audio_lp.process_iter(discriminator);
        let mut audio = match rate_adjust {
            Some(resampler) => resampler.process(&decimated),
            None => decimated,
        };
        // The FIR and residual resampler are linear, so normalize at the
        // audio rate instead of adding a multiply to every discriminator
        // sample. This preserves the old input-rate phase-step gain while
        // keeping the hot iterator identical to the measured-fast version.
        let phase_scale = *discriminator_scale;
        for sample in &mut audio {
            *sample *= phase_scale;
        }
        dc_blocker.process_after_deemphasis(&mut audio, deemph.as_mut());
        audio
    }
}

fn decimation_plan(input_rate: f32, output_rate: f32) -> (usize, f32) {
    assert!(
        input_rate.is_finite() && output_rate.is_finite(),
        "FM sample rates must be finite"
    );
    assert!(
        input_rate > 0.0 && output_rate > 0.0 && input_rate >= output_rate,
        "FM rates must satisfy input_rate >= output_rate > 0"
    );
    let ratio = input_rate / output_rate;
    let rounded = ratio.round();
    let tolerance = f32::EPSILON * ratio.max(1.0) * 4.0;
    let decimation = if (ratio - rounded).abs() <= tolerance {
        rounded as usize
    } else {
        ratio.floor() as usize
    }
    .max(1);
    (decimation, input_rate / decimation as f32)
}

enum ChannelFilter {
    None,
    One(ComplexLowPass),
    Two {
        first: ComplexLowPass,
        second: ComplexLowPass,
    },
}

impl ChannelFilter {
    fn process(&mut self, iq: &[f32]) -> Option<Vec<f32>> {
        match self {
            Self::None => None,
            Self::One(filter) => Some(filter.process(iq)),
            Self::Two { first, second } => {
                let intermediate = first.process(iq);
                Some(second.process(&intermediate))
            }
        }
    }
}

fn make_channel_filter(input_rate: f32, output_rate: f32, bandwidth: f32) -> (ChannelFilter, f32) {
    // Keep the discriminator around 240-250 kS/s: enough headroom for a
    // 180-kHz WBFM channel while substantially reducing atan2 work. The extra
    // 1.5 audio-rate margin creates a real transition band, unlike simply
    // flooring input/output at Airspy rates.
    let target_rate = (output_rate * 5.0).max(bandwidth + output_rate * 1.5);
    let mut total_decimation = (input_rate / target_rate).round().max(1.0) as usize;
    while total_decimation > 1 && input_rate / (total_decimation as f32) < bandwidth + output_rate {
        total_decimation -= 1;
    }
    if total_decimation <= 1 {
        return (ChannelFilter::None, input_rate);
    }

    let cutoff = bandwidth * 0.5;
    if total_decimation > 5 && total_decimation % 5 == 0 {
        let second_decimation = total_decimation / 5;
        let first_rate = input_rate / 5.0;
        let final_rate = first_rate / second_decimation as f32;
        let first_taps = decimator_taps(input_rate, first_rate, cutoff);
        let second_taps = decimator_taps(first_rate, final_rate, cutoff);
        (
            ChannelFilter::Two {
                first: ComplexLowPass::new(cutoff, input_rate, 5, first_taps),
                second: ComplexLowPass::new(cutoff, first_rate, second_decimation, second_taps),
            },
            final_rate,
        )
    } else {
        let final_rate = input_rate / total_decimation as f32;
        let taps = decimator_taps(input_rate, final_rate, cutoff);
        (
            ChannelFilter::One(ComplexLowPass::new(
                cutoff,
                input_rate,
                total_decimation,
                taps,
            )),
            final_rate,
        )
    }
}

fn decimator_taps(input_rate: f32, output_rate: f32, cutoff: f32) -> usize {
    // A Hamming-windowed sinc is approximately 53 dB down after a normalized
    // transition of 3.3/N. Frequencies above output_rate-cutoff alias into the
    // wanted passband, so size each stage from that actual transition width.
    let transition = (output_rate - 2.0 * cutoff).max(output_rate * 0.05);
    ((3.3 * input_rate / transition).ceil() as usize).max(7) | 1
}

/// Streaming cubic Farrow interpolator. It corrects the small residual rate
/// error after integer decimation while preserving state across arbitrary USB
/// block boundaries. One sample of look-ahead keeps the cubic segment smooth.
struct CubicFarrow {
    /// Input-sample positions advanced per output sample.
    step: f64,
    /// Absolute input-sample position of the next output.
    next_output: f64,
    /// Four most recent samples, oldest to newest.
    history: [f32; 4],
    samples_seen: u64,
}

impl CubicFarrow {
    fn if_needed(input_rate: f32, output_rate: f32) -> Option<Self> {
        let difference = (input_rate - output_rate).abs();
        let tolerance = f32::EPSILON * input_rate.max(output_rate) * 4.0;
        (difference > tolerance).then(|| Self {
            step: f64::from(input_rate) / f64::from(output_rate),
            next_output: 0.0,
            history: [0.0; 4],
            samples_seen: 0,
        })
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let capacity = ((input.len() as f64 / self.step).ceil() as usize).saturating_add(1);
        let mut output = Vec::with_capacity(capacity);
        for &sample in input {
            if self.samples_seen == 0 {
                self.history = [sample; 4];
                output.push(sample);
                self.next_output += self.step;
                self.samples_seen = 1;
                continue;
            }

            self.history.rotate_left(1);
            self.history[3] = sample;
            let newest_index = self.samples_seen as f64;
            let segment_start = newest_index - 2.0;
            let segment_end = newest_index - 1.0;
            while self.next_output <= segment_end {
                if self.next_output >= segment_start {
                    let t = (self.next_output - segment_start) as f32;
                    output.push(cubic_interpolate(self.history, t));
                }
                self.next_output += self.step;
            }
            self.samples_seen += 1;
        }
        output
    }
}

#[inline]
fn cubic_interpolate([x0, x1, x2, x3]: [f32; 4], t: f32) -> f32 {
    // Catmull-Rom written in Horner form: a cubic Farrow structure with no
    // coefficient table or phase quantization.
    let a = 0.5 * (-x0 + 3.0 * x1 - 3.0 * x2 + x3);
    let b = 0.5 * (2.0 * x0 - 5.0 * x1 + 4.0 * x2 - x3);
    let c = 0.5 * (-x0 + x2);
    ((a * t + b) * t + c) * t + x1
}

fn anti_alias_taps(decimation: usize) -> usize {
    decimation.saturating_mul(8).saturating_add(1).max(63)
}

/// One-shot WBFM demodulation convenience: demodulate the whole IQ buffer
/// through a fresh pipeline at the given rates. Returns f32 audio.
#[must_use]
pub fn fm_demod(iq: &[f32], input_rate: f32, output_rate: f32) -> Vec<f32> {
    let mut d = FmDemod::new(input_rate, output_rate, 180_000.0);
    d.process(iq)
}

/// One-shot NBFM demodulation (narrower bandwidth, no deemphasis by default).
#[must_use]
pub fn nbfm_demod(iq: &[f32], input_rate: f32, output_rate: f32) -> Vec<f32> {
    let mut d = FmDemod::with_deemphasis(input_rate, output_rate, 12_500.0, None);
    d.process(iq)
}

/// AM demodulation: envelope detector `sqrt(I² + Q²)`. Returns f32 audio at
/// the input rate (caller decimates if desired).
#[must_use]
pub fn am_demod(iq: &[f32]) -> Vec<f32> {
    debug_assert!(iq.len() % 2 == 0, "IQ must be interleaved pairs");
    let n = iq.len() / 2;
    let mut out = vec![0.0; n];
    for (sample, pair) in out.iter_mut().zip(iq.chunks_exact(2)) {
        *sample = (pair[0] * pair[0] + pair[1] * pair[1]).sqrt();
    }
    out
}

/// Sliding dot product for the parity-decimated Hilbert FIR: `out[c] =
/// dot(taps_rev, src[c .. c + taps_rev.len()])`.
///
/// Eight independent accumulator lanes with a constant-trip inner loop are what
/// LLVM needs to emit NEON/AVX FMA chains (a single accumulator serializes on
/// FMA latency). The final reduction MUST stay the by-value `for a in acc`
/// form: replacing it with an explicitly indexed tree reduction — even with
/// constant indices — makes LLVM keep `acc` in memory and drops this loop from
/// 13.5 to 5.8 GMAC/s on an M1 Max. The by-value iteration consumes a copy of
/// the array, so the accumulators stay in vector registers for the whole
/// window.
///
/// The summation order is fixed and independent of block boundaries, so
/// streaming output remains bitwise chunk-invariant.
fn sliding_dot(taps_rev: &[f32], src: &[f32], out: &mut [f32]) {
    const LANES: usize = 8;
    let t_len = taps_rev.len();
    for (c, o) in out.iter_mut().enumerate() {
        let window = &src[c..c + t_len];
        let mut acc = [0.0f32; LANES];
        let mut taps = taps_rev.chunks_exact(LANES);
        let mut win = window.chunks_exact(LANES);
        for (t, w) in taps.by_ref().zip(win.by_ref()) {
            for lane in 0..LANES {
                acc[lane] += t[lane] * w[lane];
            }
        }
        let mut sum = 0.0;
        for a in acc {
            sum += a;
        }
        for (t, w) in taps.remainder().iter().zip(win.remainder()) {
            sum += t * w;
        }
        *o = sum;
    }
}

/// A streaming single-sideband (SSB) demodulator using the phasing method with
/// a Type-IV Hilbert FIR. Selects the upper or lower sideband from complex
/// baseband IQ and emits real audio.
///
/// The Hilbert transformer shifts the Q branch by ±90°; combining I with the
/// shifted Q cancels the opposite sideband. The default 255-tap FIR gives at
/// least 40 dB opposite-sideband rejection from roughly 375 Hz to 15 kHz at a
/// 48 kHz input rate. Decimate channelized IQ before this stage at high SDR
/// rates; the CLI does so.
///
/// # Implementation
///
/// An odd-length Hilbert transformer has zero coefficients at every even
/// offset from its centre, so the FIR only ever reads input samples whose
/// delay from the current sample shares one parity. The Q history is therefore
/// kept *pre-decimated by two* — one buffer per input-sample parity — which
/// turns the sparse anti-symmetric kernel into a dense forward dot product
/// over the nonzero taps only. Each output reads a flat contiguous window of
/// exactly one parity buffer; there is no index table, no reversed read, and
/// no gather, which is what lets the dot product vectorize (measured 8.8x on
/// the crate bench versus the folded gather form this replaced).
pub struct SsbDemag {
    /// The nonzero Hilbert taps `h[first_tap], h[first_tap + 2], …` stored in
    /// reverse order so taps and window walk forward together.
    taps_rev: Vec<f32>,
    /// Number of nonzero taps (`taps_rev.len()`), i.e. the length of every
    /// window into a parity buffer.
    t_len: usize,
    /// FIR group delay `M/2`; the I branch is delayed by exactly this.
    group_delay: usize,
    /// Delay of the first nonzero tap: 0 when `M/2` is odd, 1 when even.
    /// Determines which parity buffer each output's window comes from.
    first_tap: usize,
    /// The `group_delay` most recent I samples (the I branch's delay line).
    i_carry: Vec<f32>,
    /// Reusable `[i_carry ++ block I]` buffer; `i_scratch[k]` is the
    /// group-delay-aligned I value for block output `k`.
    i_scratch: Vec<f32>,
    /// Per-parity Q history: the `t_len` most recent Q samples of each parity.
    q_carry: [Vec<f32>; 2],
    /// Reusable `[q_carry[p] ++ block Q of parity p]` buffers.
    q_scratch: [Vec<f32>; 2],
    /// Reusable Hilbert-output buffer for one parity class.
    hilbert_buf: Vec<f32>,
    /// Parity (0/1) of the next input sample's absolute stream index.
    parity: usize,
}

impl SsbDemag {
    /// Construct the default 255-tap Hilbert SSB demodulator.
    #[must_use]
    pub fn new() -> Self {
        Self::with_taps(255)
    }

    /// Construct with an N-tap (odd) Hilbert FIR. Coefficients follow
    /// `h[n] = 2/(π·(n − M/2))` for odd `(n − M/2)`, 0 otherwise, windowed with
    /// Hamming (a Type-IV-style Hilbert transformer).
    #[must_use]
    pub fn with_taps(taps: usize) -> Self {
        let taps = taps.max(7) | 1; // odd, >=7
        let m = taps - 1; // order
        let mid = (m / 2) as f32;
        let mut h = Vec::with_capacity(taps);
        for n in 0..taps {
            let k = n as f32 - mid;
            // Nonzero only where (n - M/2) is odd.
            let is_odd = (n as i64 - (m / 2) as i64).rem_euclid(2) == 1;
            let c = if is_odd {
                2.0 / (std::f32::consts::PI * k)
            } else {
                0.0
            };
            let win = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / m as f32).cos();
            h.push(c * win);
        }
        // Odd (n - M/2) means the nonzero delays are the evens when M/2 is
        // odd, the odds when M/2 is even. Keep only those taps, reversed so
        // the dot product walks taps and window in the same direction.
        let first_tap = ((m / 2) + 1) & 1;
        let mut taps_rev: Vec<f32> = h[first_tap..].iter().copied().step_by(2).collect();
        let t_len = taps_rev.len();
        taps_rev.reverse();
        Self {
            taps_rev,
            t_len,
            group_delay: m / 2,
            first_tap,
            i_carry: vec![0.0; m / 2],
            i_scratch: Vec::new(),
            q_carry: [vec![0.0; t_len], vec![0.0; t_len]],
            q_scratch: [Vec::new(), Vec::new()],
            hilbert_buf: Vec::new(),
            parity: 0,
        }
    }

    /// Demodulate a block of interleaved cf32 IQ into real audio. `upper =
    /// true` selects USB (keeps the upper sideband / positive frequencies);
    /// `false` selects LSB.
    ///
    /// Per block: deinterleave I and the two Q parity classes into retained
    /// `[carry ++ block]` scratch buffers, run the dense Hilbert FIR over each
    /// parity class as a flat sliding dot product, then combine with the
    /// group-delay-aligned I branch. State (I delay line, per-parity Q
    /// history, stream parity) persists across calls, so output is bitwise
    /// identical no matter how the stream is chunked.
    pub fn process(&mut self, iq: &[f32], upper: bool) -> Vec<f32> {
        debug_assert!(iq.len() % 2 == 0, "IQ must be interleaved pairs");
        let n = iq.len() / 2;
        let t_len = self.t_len;
        let start_parity = self.parity;

        // [previous history ++ this block] views, as in `LowPass`: every
        // window is a flat forward slice and steady state allocates nothing.
        self.i_scratch.clear();
        self.i_scratch.reserve(self.group_delay + n);
        self.i_scratch.extend_from_slice(&self.i_carry);
        for (scratch, carry) in self.q_scratch.iter_mut().zip(&self.q_carry) {
            scratch.clear();
            scratch.reserve(t_len + n / 2 + 1);
            scratch.extend_from_slice(carry);
        }
        let mut sample_parity = start_parity;
        for pair in iq.chunks_exact(2) {
            self.i_scratch.push(pair[0]);
            self.q_scratch[sample_parity].push(pair[1]);
            sample_parity ^= 1;
        }

        // z = I + jQ; H_z = H{I} + j·H{Q}.
        //   USB (keep +f): Re(0.5*(z + j·H_z)) = 0.5*(I - H{Q})
        //   LSB (keep -f): Re(0.5*(z - j·H_z)) = 0.5*(I + H{Q})
        // 0.5·x - 0.5·y is bit-identical to 0.5·(x - y): scaling by 0.5 is
        // exact, so folding the sign into a multiplier changes nothing.
        let hilbert_sign = if upper { -0.5f32 } else { 0.5f32 };
        let mut out = vec![0.0f32; n];
        for parity in 0..2usize {
            // Block index of the first output whose input sample has this
            // stream parity; outputs of one parity land at every second index.
            let first_k = parity ^ start_parity;
            if first_k >= n {
                continue;
            }
            let outputs = (n - first_k).div_ceil(2);
            // Output c of this class reads the window of `t_len` consecutive
            // parity-buffer entries ending at its newest reachable sample:
            // the just-arrived sample itself when `first_tap` is 0, otherwise
            // the previous sample (which lives in the *other* parity buffer).
            // `window_shift` turns that into the window's start offset.
            let window_shift = usize::from(self.first_tap == 0 || first_k == 1);
            let src = &self.q_scratch[parity ^ self.first_tap][window_shift..];
            self.hilbert_buf.clear();
            self.hilbert_buf.resize(outputs, 0.0);
            sliding_dot(&self.taps_rev, src, &mut self.hilbert_buf);
            for (c, &hilbert_q) in self.hilbert_buf.iter().enumerate() {
                let k = first_k + 2 * c;
                out[k] = 0.5 * self.i_scratch[k] + hilbert_sign * hilbert_q;
            }
        }

        // Retain exactly the history the next block needs.
        let i_len = self.i_scratch.len();
        self.i_carry
            .copy_from_slice(&self.i_scratch[i_len - self.group_delay..]);
        for (carry, scratch) in self.q_carry.iter_mut().zip(&self.q_scratch) {
            let q_len = scratch.len();
            carry.copy_from_slice(&scratch[q_len - t_len..]);
        }
        self.parity = sample_parity;
        out
    }
}

impl Default for SsbDemag {
    fn default() -> Self {
        Self::new()
    }
}

/// USB demodulation via the phasing method with a Hilbert FIR. Keeps the
/// upper sideband (positive frequencies), rejecting the lower by ≥40 dB.
#[must_use]
pub fn usb_demod(iq: &[f32]) -> Vec<f32> {
    let mut d = SsbDemag::new();
    d.process(iq, true)
}

/// LSB demodulation via the phasing method with a Hilbert FIR. Keeps the
/// lower sideband (negative frequencies), rejecting the upper by ≥40 dB.
#[must_use]
pub fn lsb_demod(iq: &[f32]) -> Vec<f32> {
    let mut d = SsbDemag::new();
    d.process(iq, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Generate a WBFM-modulated cf32 signal: a carrier frequency-modulated
    /// by an audio tone.
    fn synth_fm(audio_hz: f32, fs: f32, n: usize, deviation: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(n * 2);
        let mut phase = 0.0_f32;
        for k in 0..n {
            let t = k as f32 / fs;
            // Integrate the audio to get the phase.
            let inst_freq = deviation * (2.0 * PI * audio_hz * t).sin();
            phase += 2.0 * PI * inst_freq / fs;
            out.push(phase.cos());
            out.push(phase.sin());
        }
        out
    }

    #[test]
    fn fm_demod_recovers_tone_energy() {
        // A 1 kHz tone modulating a 75 kHz deviation carrier at 240 ks/s.
        let fs = 240_000.0_f32;
        let iq = synth_fm(1000.0, fs, 24_000, 75_000.0);
        let audio = fm_demod(&iq, fs, 48_000.0);
        assert!(!audio.is_empty(), "demod produced no output");
        // The demodulated audio should have non-trivial energy (RMS > silence).
        let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len() as f32).sqrt();
        assert!(rms > 0.01, "demod RMS {rms} too low; expected tone energy");
    }

    #[test]
    fn fm_demod_tone_frequency_is_recovered() {
        // A slow-varying FM: the discriminator output frequency should match.
        let fs = 240_000.0_f32;
        let iq = synth_fm(2000.0, fs, 48_000, 50_000.0);
        let audio = fm_demod(&iq, fs, 48_000.0);
        // Count zero-crossings in the latter half (post-transient) to estimate
        // the dominant frequency. 2 kHz => ~2 crossings per ms => over 0.5s of
        // 48 kHz audio that's ~2000 crossings.
        let half = audio.len() / 2;
        let mut crossings = 0;
        for w in audio[half..].windows(2) {
            if (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
                crossings += 1;
            }
        }
        assert!(
            crossings > 300,
            "expected >500 zero crossings for 2 kHz, got {crossings}"
        );
    }

    #[test]
    fn am_demod_returns_envelope() {
        // An AM-modulated carrier: envelope should track the modulation.
        let fs = 48_000.0_f32;
        let n = 1024;
        let mut iq = Vec::with_capacity(n * 2);
        for k in 0..n {
            let t = k as f32 / fs;
            let envelope = 0.5 + 0.5 * (2.0 * PI * 100.0 * t).sin();
            iq.push(envelope * (2.0 * PI * 10_000.0 * t).cos()); // carrier at 10 kHz
            iq.push(envelope * (2.0 * PI * 10_000.0 * t).sin());
        }
        let env = am_demod(&iq);
        // The recovered envelope should oscillate between ~0 and ~1.
        let max = env.iter().copied().fold(0.0f32, f32::max);
        let min = env.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(max > 0.8, "AM envelope max {max} too low");
        assert!(min < 0.2, "AM envelope min {min} too high (no modulation)");
    }

    #[test]
    fn usb_lsb_are_opposite_signatures() {
        // USB and LSB of the same complex IQ should produce different outputs
        // (one keeps the upper sideband, the other the lower).
        // Generate a complex signal with both positive and negative frequency content.
        let fs = 48_000.0_f32;
        let n = 1024;
        let mut iq = Vec::with_capacity(n * 2);
        let mut phase = 0.0;
        for _ in 0..n {
            // Two tones: one at +2kHz, one at -1kHz (complex).
            let t = phase / (2.0 * PI * 2000.0 / fs);
            let i_val = (2.0 * PI * 2000.0 / fs * t as f32).sin()
                + 0.5 * (-2.0 * PI * 1000.0 / fs * t as f32).sin();
            let q_val = (2.0 * PI * 2000.0 / fs * t as f32).cos()
                + 0.5 * (-2.0 * PI * 1000.0 / fs * t as f32).cos();
            iq.push(i_val);
            iq.push(q_val);
            phase += 2.0 * PI * 2000.0 / fs;
        }
        let usb = usb_demod(&iq);
        let lsb = lsb_demod(&iq);
        assert_eq!(usb.len(), lsb.len());
        // USB and LSB should differ on most samples (different sideband content).
        let diffs = usb
            .iter()
            .zip(lsb.iter())
            .filter(|(a, b)| (**a - **b).abs() > 1e-4)
            .count();
        assert!(
            diffs > usb.len() / 4,
            "USB and LSB should differ significantly ({} diffs out of {})",
            diffs,
            usb.len()
        );
    }

    #[test]
    fn ssb_rejects_opposite_sideband() {
        // Generate two complex tones: one at +f (upper sideband) and one at -f
        // (lower sideband), each as a unit-amplitude complex exponential. Feed
        // them separately and measure the demod output at f. USB must pass the
        // +f tone and reject the -f tone by ≥40 dB; LSB the opposite.
        let fs = 48_000.0_f32;
        let n = 8192usize;
        // Naive DFT probe at frequency `freq` over the real audio output.
        let probe = |audio: &[f32], freq: f32| -> f32 {
            let m = audio.len() as f32;
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for (k, &s) in audio.iter().enumerate() {
                let phase = -2.0 * PI * freq * k as f32 / fs;
                re += s * phase.cos();
                im += s * phase.sin();
            }
            (re * re + im * im).sqrt() / m
        };

        for f in [375.0_f32, 1_000.0, 3_000.0, 6_000.0, 10_000.0, 15_000.0] {
            let w = 2.0 * PI * f / fs;
            let pos: Vec<f32> = (0..n)
                .flat_map(|k| [(w * k as f32).cos(), (w * k as f32).sin()])
                .collect();
            let neg: Vec<f32> = (0..n)
                .flat_map(|k| [(w * k as f32).cos(), -(w * k as f32).sin()])
                .collect();

            let usb_keep = probe(&usb_demod(&pos), f);
            let usb_reject = probe(&usb_demod(&neg), f);
            let lsb_keep = probe(&lsb_demod(&neg), f);
            let lsb_reject = probe(&lsb_demod(&pos), f);

            let usb_rej_db = 20.0 * (usb_keep / usb_reject).max(1e-12).log10();
            let lsb_rej_db = 20.0 * (lsb_keep / lsb_reject).max(1e-12).log10();
            assert!(
                usb_rej_db >= 40.0,
                "{f} Hz USB rejection {usb_rej_db:.1} dB < 40 dB (keep={usb_keep:.4}, reject={usb_reject:.5})"
            );
            assert!(
                lsb_rej_db >= 40.0,
                "{f} Hz LSB rejection {lsb_rej_db:.1} dB < 40 dB (keep={lsb_keep:.4}, reject={lsb_reject:.5})"
            );
        }
    }

    #[test]
    fn fm_demod_silence_produces_near_zero() {
        // A constant-phase (unmodulated) carrier demodulates to ~0 audio.
        let n = 1024;
        let iq: Vec<f32> = (0..n).flat_map(|_| [1.0_f32, 0.0]).collect();
        let audio = fm_demod(&iq, 48_000.0, 8_000.0);
        let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len().max(1) as f32).sqrt();
        assert!(
            rms < 0.1,
            "unmodulated carrier should produce ~0 audio, RMS={rms}"
        );
    }

    #[test]
    fn nbfm_demod_has_no_deemphasis_by_default() {
        // NBFM convenience path disables deemphasis (Option None). A step
        // input's steady-state gain is unaffected by a deemphasis pole.
        let mut d = FmDemod::with_deemphasis(240_000.0, 48_000.0, 12_500.0, None);
        assert!(d.deemph.is_none(), "NBFM default must have no deemphasis");
        // Feeding a steady carrier → near-zero audio (no modulation).
        let iq: Vec<f32> = (0..2048).flat_map(|_| [1.0_f32, 0.0]).collect();
        let audio = d.process(&iq);
        let rms = (audio.iter().map(|x| x * x).sum::<f32>() / audio.len().max(1) as f32).sqrt();
        assert!(
            rms < 0.1,
            "steady carrier should yield ~0 audio, got RMS={rms}"
        );
    }

    #[test]
    fn fm_demod_streaming_matches_one_block() {
        // Persistent discriminator, decimator, deemphasis, and DC state: every
        // possible IQ-pair split must match a single block exactly.
        let fs = 240_000.0_f32;
        let complex_samples = 257usize;
        let iq = synth_fm(1000.0, fs, complex_samples, 75_000.0);
        let mut whole = FmDemod::new(fs, 48_000.0, 180_000.0);
        let out_whole = whole.process(&iq);
        for split_sample in 0..=complex_samples {
            let split_at = split_sample * 2;
            let mut split = FmDemod::new(fs, 48_000.0, 180_000.0);
            let mut out_split = split.process(&iq[..split_at]);
            out_split.extend(split.process(&iq[split_at..]));
            assert_eq!(
                out_whole, out_split,
                "streaming mismatch at complex sample {split_sample}"
            );
        }
    }

    #[test]
    fn fm_demod_supports_both_airspy_advertised_rates() {
        for input_rate in [2_500_000.0_f32, 10_000_000.0] {
            let count = (input_rate / 100.0) as usize; // 10 ms
            let iq: Vec<f32> = (0..count).flat_map(|_| [1.0, 0.0]).collect();
            let mut demod = FmDemod::new(input_rate, 48_000.0, 180_000.0);
            let audio = demod.process(&iq);
            assert!(
                (478..=482).contains(&audio.len()),
                "{input_rate} Hz produced {} samples for 10 ms",
                audio.len()
            );
            assert!(audio.iter().all(|sample| sample.is_finite()));
        }
    }

    #[test]
    fn rational_rate_fm_is_chunk_invariant() {
        let input_rate = 2_500_000.0_f32;
        let iq = synth_fm(1_000.0, input_rate, 5_213, 75_000.0);
        let mut whole = FmDemod::new(input_rate, 48_000.0, 180_000.0);
        let expected = whole.process(&iq);

        for split_sample in [0, 1, 51, 52, 53, 624, 625, 2_607, 5_213] {
            let split_at = split_sample * 2;
            let mut split = FmDemod::new(input_rate, 48_000.0, 180_000.0);
            let mut actual = split.process(&iq[..split_at]);
            actual.extend(split.process(&iq[split_at..]));
            assert_eq!(expected, actual, "split at {split_sample}");
        }
    }

    #[test]
    fn channelized_fm_preserves_input_rate_phase_step_gain() {
        let output_rate = 48_000.0_f32;
        let deviation = 75_000.0_f32;
        let mut normalized_rms = Vec::new();

        for input_rate in [2_400_000.0_f32, 2_500_000.0, 10_000_000.0] {
            let count = (input_rate * 0.04) as usize;
            let iq = synth_fm(1_000.0, input_rate, count, deviation);
            let mut demod = FmDemod::with_deemphasis(input_rate, output_rate, 180_000.0, None);
            let audio = demod.process(&iq);
            let tail = &audio[audio.len() / 2..];
            let rms =
                (tail.iter().map(|sample| sample * sample).sum::<f32>() / tail.len() as f32).sqrt();
            let expected_rms = (2.0 * PI * deviation / input_rate) / 2.0_f32.sqrt();
            let relative_error = (rms - expected_rms).abs() / expected_rms;
            assert!(
                relative_error < 0.12,
                "{input_rate} Hz gain drift: rms={rms}, expected={expected_rms}"
            );
            assert!(
                tail.iter().all(|sample| sample.abs() < 0.3),
                "{input_rate} Hz full-deviation WBFM would clip downstream"
            );
            normalized_rms.push(rms * input_rate);
        }

        let baseline = normalized_rms[0];
        for (index, &level) in normalized_rms.iter().enumerate().skip(1) {
            assert!(
                ((level - baseline) / baseline).abs() < 0.08,
                "rate index {index} changes normalized FM gain: {level} vs {baseline}"
            );
        }
    }

    #[test]
    fn airspy_channel_filter_rejects_adjacent_alias_band() {
        let input_rate = 2_500_000.0_f32;
        let output_rate = 48_000.0_f32;
        let bandwidth = 180_000.0_f32;
        let count = 100_000usize;
        let run = |tone_hz: f32| {
            let iq: Vec<f32> = (0..count)
                .flat_map(|index| {
                    let angle = 2.0 * PI * tone_hz * index as f32 / input_rate;
                    [angle.cos(), angle.sin()]
                })
                .collect();
            let (mut filter, final_rate) = make_channel_filter(input_rate, output_rate, bandwidth);
            assert!((final_rate - 250_000.0).abs() < 1.0);
            filter
                .process(&iq)
                .expect("Airspy path must channel-filter")
        };
        let pass = run(80_000.0);
        // At a 250 kS/s intermediate rate, 170 kHz aliases onto -80 kHz.
        // The pre-decimation filter must suppress it before that collision.
        let stop = run(170_000.0);
        let rms = |iq: &[f32]| {
            let tail = &iq[iq.len() / 2..];
            (tail.iter().map(|sample| sample * sample).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let pass_rms = rms(&pass);
        let stop_rms = rms(&stop);
        assert!(
            stop_rms < pass_rms * 0.01,
            "adjacent alias insufficiently rejected: pass={pass_rms}, stop={stop_rms}"
        );
    }

    #[test]
    fn fm_scaled_filter_rejects_above_output_nyquist_at_sdr_rate() {
        let input_rate = 2_400_000.0_f32;
        let output_rate = 48_000.0_f32;
        let count = 240_000usize;
        let deviation = 10_000.0;

        let mut pass = FmDemod::with_deemphasis(input_rate, output_rate, 12_500.0, None);
        let mut stop = FmDemod::with_deemphasis(input_rate, output_rate, 12_500.0, None);
        let pass_audio = pass.process(&synth_fm(1_000.0, input_rate, count, deviation));
        let stop_audio = stop.process(&synth_fm(30_000.0, input_rate, count, deviation));
        let rms = |samples: &[f32]| {
            let tail = &samples[samples.len() / 2..];
            (tail.iter().map(|sample| sample * sample).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let pass_rms = rms(&pass_audio);
        let stop_rms = rms(&stop_audio);
        assert!(
            stop_rms < pass_rms * 0.03,
            "30 kHz alias insufficiently rejected: pass={pass_rms}, stop={stop_rms}"
        );
    }

    #[test]
    fn ssb_matches_direct_hilbert_convolution() {
        // Absolute correctness against the mathematically defined phasing
        // demodulator: y[k] = 0.5*(I[k - M/2] ∓ Σ_j h[j]·Q[k - j]) with the
        // designed kernel h and the stream zero-padded before its start,
        // accumulated in f64. This pins the group-delay alignment of the I
        // branch, the tap direction, and the parity bookkeeping of the
        // decimated Q history; a rewrite that is merely chunk-consistent but
        // filters the wrong samples fails here. Covers both tap-parity cases
        // (M/2 odd: nonzero taps at even delays; M/2 even: odd delays).
        let fs = 48_000.0_f32;
        let signal_len = 600usize;
        let iq: Vec<f32> = (0..signal_len)
            .flat_map(|k| {
                let pos = 2.0 * PI * 2_900.0 * k as f32 / fs;
                let neg = -2.0 * PI * 1_300.0 * k as f32 / fs;
                [
                    pos.cos() + 0.4 * neg.cos() + 0.1 * (k as f32 * 0.37).sin(),
                    pos.sin() + 0.4 * neg.sin() + 0.1 * (k as f32 * 0.53).cos(),
                ]
            })
            .collect();
        let peak = iq.iter().fold(0.0f32, |m, v| m.max(v.abs()));

        for &taps in &[255usize, 257, 63, 65] {
            // The designed kernel, restated from the definition.
            let m = taps - 1;
            let mid = (m / 2) as f32;
            let h: Vec<f32> = (0..taps)
                .map(|n| {
                    let k = n as f32 - mid;
                    let is_odd = (n as i64 - (m / 2) as i64).rem_euclid(2) == 1;
                    let c = if is_odd { 2.0 / (PI * k) } else { 0.0 };
                    let win = 0.54 - 0.46 * (2.0 * PI * n as f32 / m as f32).cos();
                    c * win
                })
                .collect();
            for upper in [true, false] {
                let got = SsbDemag::with_taps(taps).process(&iq, upper);
                assert_eq!(got.len(), signal_len);
                for (k, &y) in got.iter().enumerate() {
                    let mut hilbert_q = 0.0f64;
                    for (j, &tap) in h.iter().enumerate() {
                        if let Some(idx) = k.checked_sub(j) {
                            hilbert_q += f64::from(tap) * f64::from(iq[idx * 2 + 1]);
                        }
                    }
                    let i_val = k.checked_sub(m / 2).map_or(0.0, |idx| iq[idx * 2]);
                    let hilbert_q = hilbert_q as f32;
                    let want = if upper {
                        0.5 * (i_val - hilbert_q)
                    } else {
                        0.5 * (i_val + hilbert_q)
                    };
                    assert!(
                        (y - want).abs() <= 1e-6 * peak,
                        "taps={taps} upper={upper} output {k}: got {y}, want {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn ssb_streaming_survives_irregular_chunking() {
        // The parity-decimated Q history must track the absolute stream parity
        // across arbitrary block boundaries. Odd complex-sample chunks flip
        // the parity carried between blocks, so prime/odd chunk sizes exercise
        // every residue that uniform even splits hide. Output must be bitwise
        // identical to the whole-block run.
        let fs = 48_000.0_f32;
        let n = 1021usize;
        let iq: Vec<f32> = (0..n)
            .flat_map(|k| {
                let p = 2.0 * PI * 2_100.0 * k as f32 / fs;
                [p.cos() + 0.2 * (k as f32 * 0.29).sin(), p.sin()]
            })
            .collect();
        for &taps in &[255usize, 257] {
            for upper in [true, false] {
                let expected = SsbDemag::with_taps(taps).process(&iq, upper);
                for &chunk_samples in &[1_usize, 2, 3, 5, 7, 13, 64, 127, 257] {
                    let mut streamed = SsbDemag::with_taps(taps);
                    let mut got = Vec::new();
                    for block in iq.chunks(chunk_samples * 2) {
                        got.extend(streamed.process(block, upper));
                    }
                    assert_eq!(
                        expected, got,
                        "taps={taps} upper={upper} chunk={chunk_samples}: streaming diverged"
                    );
                }
            }
        }
    }

    #[test]
    fn ssb_demod_streaming_matches_one_block() {
        let fs = 48_000.0_f32;
        let n = 257usize;
        let w_pos = 2.0 * PI * 3_000.0 / fs;
        let w_neg = -2.0 * PI * 1_700.0 / fs;
        let iq: Vec<f32> = (0..n)
            .flat_map(|k| {
                let p = w_pos * k as f32;
                let m = w_neg * k as f32;
                [p.cos() + 0.4 * m.cos(), p.sin() + 0.4 * m.sin()]
            })
            .collect();

        for upper in [true, false] {
            let mut whole = SsbDemag::new();
            let expected = whole.process(&iq, upper);
            for split_sample in 0..=n {
                let split_at = split_sample * 2;
                let mut chunked = SsbDemag::new();
                let mut got = chunked.process(&iq[..split_at], upper);
                got.extend(chunked.process(&iq[split_at..], upper));
                assert_eq!(
                    expected, got,
                    "{upper:?} sideband mismatch at complex sample {split_sample}"
                );
            }
        }
    }
}
