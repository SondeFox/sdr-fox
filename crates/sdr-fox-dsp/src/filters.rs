//! DSP filters: low-pass decimator, deemphasis, DC blocker.
//!
//! All operate on real-valued f32 sample streams (the demodulated audio path).

use std::f32::consts::PI;

/// A windowed-sinc low-pass filter with decimation. Every input sample enters
/// the FIR history; an output sample is emitted every `decimation` samples.
///
/// The FIR is a linear-phase (symmetric) Hamming-windowed sinc. It is a
/// **streaming** filter: state (history + decimation phase) is preserved across
/// `process` calls, so feeding the same signal as one block or as several
/// chunks yields identical output.
pub struct LowPass {
    /// Kernel stored in reverse order so the dot product walks the taps and the
    /// signal window in the same direction. Reading one of them backwards (as
    /// the previous doubled-ring form did) defeats vectorization.
    taps_rev: Vec<f32>,
    decimation: usize,
    /// The `ntaps - 1` most recent input samples, i.e. exactly the history the
    /// next block needs to produce its first output. Zero-filled at
    /// construction so the filter starts from rest.
    carry: Vec<f32>,
    /// Reusable `[carry ++ input]` work buffer. Retained across calls so a
    /// steady-state stream performs no allocation here.
    scratch: Vec<f32>,
    ntaps: usize,
    /// Offset of the next output's window within `scratch`, carried across
    /// calls. This is the decimation phase expressed in the scratch frame:
    /// output `k` reads `scratch[k .. k + ntaps]`.
    phase: usize,
}

/// Dot product with eight independent accumulators.
///
/// A single `f32` accumulator serializes the whole kernel on FMA latency
/// (~4 cycles/tap regardless of how many FMA units are idle). Eight independent
/// chains let the scheduler saturate the pipelines, and the `chunks_exact`
/// shape is what LLVM needs to emit NEON/AVX lanes. Measured on an M1 Max at
/// the rates this library streams: 0.95 -> 9.42 GMAC/s at 401 taps, 1.41 ->
/// 9.20 at 81 taps. Eight is the robust width — 16 wins only for very long
/// kernels, and 32 loses everywhere.
///
/// Summation order differs from a serial accumulator, so results differ from a
/// naive left fold in the last ulp (~1e-6 relative). The order is fixed and
/// independent of block size, so streaming output stays bitwise chunk-invariant.
#[inline]
fn dot_product(taps_rev: &[f32], window: &[f32]) -> f32 {
    const LANES: usize = 8;
    let mut acc = [0.0f32; LANES];
    let mut taps = taps_rev.chunks_exact(LANES);
    let mut win = window.chunks_exact(LANES);
    for (t, w) in taps.by_ref().zip(win.by_ref()) {
        for lane in 0..LANES {
            acc[lane] += t[lane] * w[lane];
        }
    }
    // MUST stay the by-value `for a in acc` form. Replacing it with an
    // explicitly indexed tree reduction — even with constant indices — makes
    // LLVM keep `acc` in memory instead of NEON registers for the whole window.
    // Measured on an M1 Max over the geometries this crate builds (indexed tree
    // vs by-value, GMAC/s): dec=50/401 taps 6.37 -> 11.67, dec=10/81 6.07 ->
    // 13.44, dec=8/63 5.81 -> 10.06. Consuming the array by value keeps the
    // accumulators in vector registers.
    let mut sum = 0.0f32;
    for a in acc {
        sum += a;
    }
    for (t, w) in taps.remainder().iter().zip(win.remainder()) {
        sum += t * w;
    }
    sum
}

impl LowPass {
    /// Build a low-pass with cutoff `freq_hz` at sample rate `sample_rate`,
    /// decimating by `decimation`. `taps` controls the kernel length
    /// (longer = sharper rolloff, more compute).
    #[must_use]
    pub fn new(freq_hz: f32, sample_rate: f32, decimation: usize, taps: usize) -> Self {
        let taps = taps.max(4) | 1; // odd, >=4
        let fc = freq_hz / sample_rate;
        let mut kernel = Vec::with_capacity(taps);
        let mid = (taps / 2) as f32;
        for i in 0..taps {
            let n = i as f32 - mid;
            // Windowed sinc: 2*fc*sinc(2*fc*n). The sinc argument is the
            // (doubled) normalized cutoff; using fc*n here would place the -6 dB
            // point at ~2x the intended frequency.
            let arg = PI * 2.0 * fc * n;
            let sinc = if arg.abs() < 1e-9 {
                1.0
            } else {
                arg.sin() / arg
            };
            let window = 0.54 - 0.46 * (2.0 * PI * (i as f32) / (taps - 1) as f32).cos();
            kernel.push(2.0 * fc * sinc * window);
        }
        // Normalize for unity DC gain.
        let sum: f32 = kernel.iter().sum();
        for t in &mut kernel {
            *t /= sum;
        }
        kernel.reverse();
        Self {
            taps_rev: kernel,
            decimation: decimation.max(1),
            carry: vec![0.0; taps - 1],
            scratch: Vec::new(),
            ntaps: taps,
            phase: 0,
        }
    }

    /// Process a block of real samples; returns the decimated output.
    ///
    /// Every input sample is pushed into the FIR history; the dot product is
    /// computed and emitted when the decimation phase is zero.
    /// This produces correct alias rejection — striding the *input* by the
    /// decimation factor would subsample the history and destroy the response.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        self.process_iter(input.iter().copied())
    }

    /// Iterator form used to fuse a producer such as the FM discriminator with
    /// this decimator, avoiding a full-rate intermediate allocation.
    pub(crate) fn process_iter(&mut self, input: impl IntoIterator<Item = f32>) -> Vec<f32> {
        let ntaps = self.ntaps;
        let dec = self.decimation;
        let input = input.into_iter();

        // Build one contiguous view of [previous history ++ this block] so each
        // dot product reads a flat slice. Every output's window holds the same
        // values no matter how the caller chunked the stream, which is what
        // keeps the output bitwise chunk-invariant.
        self.scratch.clear();
        self.scratch.reserve(self.carry.len() + input.size_hint().0);
        self.scratch.extend_from_slice(&self.carry);
        self.scratch.extend(input);
        let buf = &self.scratch;

        // `buf` always holds at least the `ntaps - 1` carry samples, so this
        // cannot underflow; it is the count of newly consumable positions.
        let consumed = buf.len() - (ntaps - 1);
        let mut out = Vec::with_capacity(consumed / dec + 1);
        let mut k = self.phase;
        while k + ntaps <= buf.len() {
            out.push(dot_product(&self.taps_rev, &buf[k..k + ntaps]));
            k += dec;
        }

        // The loop stops with `k >= consumed`, so the next block resumes at
        // `k - consumed` once the retained tail becomes its prefix.
        debug_assert!(k >= consumed, "decimation phase would go backwards");
        self.phase = k - consumed;
        let tail = buf.len() - (ntaps - 1);
        self.carry.copy_from_slice(&buf[tail..]);
        out
    }
}

/// Fused complex dot product over an **interleaved** IQ window against a
/// kernel whose every tap is stored twice (`h[j], h[j]`, reversed). Even
/// accumulator lanes sum I products, odd lanes sum Q products, so one
/// contiguous pass over the window produces both output components.
///
/// Sixteen f32 lanes are eight independent accumulator chains per component —
/// the complex analogue of `dot_product`'s eight — and measured fastest on the
/// M1 Max across the kernel range the demodulators build (27..283 taps):
/// vs 8 lanes it is +7% at 81 taps and +7% at 283, -8% at 63, level at 27;
/// 32 lanes loses everywhere.
///
/// Both slice lengths are always even (`2 * ntaps` and `2 * ntaps`), so the
/// `chunks_exact(2)` remainder loop drops nothing.
#[inline]
fn dot_product_iq(taps_rev2: &[f32], window: &[f32]) -> (f32, f32) {
    const LANES: usize = 16;
    let mut acc = [0.0f32; LANES];
    let mut taps = taps_rev2.chunks_exact(LANES);
    let mut win = window.chunks_exact(LANES);
    for (t, w) in taps.by_ref().zip(win.by_ref()) {
        for lane in 0..LANES {
            acc[lane] += t[lane] * w[lane];
        }
    }
    let mut i = 0.0f32;
    let mut q = 0.0f32;
    for pair in acc.chunks_exact(2) {
        i += pair[0];
        q += pair[1];
    }
    for (t, w) in taps
        .remainder()
        .chunks_exact(2)
        .zip(win.remainder().chunks_exact(2))
    {
        i += t[0] * w[0];
        q += t[1] * w[1];
    }
    (i, q)
}

/// Stateful low-pass decimator for interleaved complex IQ. I and Q use
/// identical FIRs and decimation phase, preserving quadrature while reducing
/// the rate before expensive demodulation stages such as a Hilbert transform.
///
/// Unlike the earlier two-`LowPass` form (which deinterleaved the block twice,
/// allocated two intermediate vectors, and re-interleaved), this runs one
/// fused pass directly over the interleaved samples via `dot_product_iq` and
/// writes straight into the interleaved output. Steady-state windows read the
/// caller's block in place; only the few windows that straddle the block
/// boundary go through the small retained `boundary` buffer, so no per-block
/// allocation or bulk copy happens at all.
pub struct ComplexLowPass {
    /// Reversed kernel with each tap duplicated (`h[j], h[j]`), aligned with
    /// the interleaved window so taps and signal walk forward together.
    taps_rev2: Vec<f32>,
    decimation: usize,
    /// The `ntaps - 1` most recent complex samples, interleaved: exactly the
    /// history the next block's first windows need. Zero-filled at
    /// construction so the filter starts from rest.
    carry: Vec<f32>,
    /// Retained `2 * ntaps` assembly buffer for windows that straddle the
    /// carry/input boundary.
    boundary: Vec<f32>,
    ntaps: usize,
    /// Complex-sample offset of the next output's window within the virtual
    /// `[carry ++ input]` stream, i.e. the decimation phase carried across
    /// calls.
    phase: usize,
}

impl ComplexLowPass {
    /// Build matched I/Q low-pass decimators.
    #[must_use]
    pub fn new(freq_hz: f32, sample_rate: f32, decimation: usize, taps: usize) -> Self {
        // Design the kernel through `LowPass` so the response stays defined in
        // exactly one place; only the storage layout differs here.
        let proto = LowPass::new(freq_hz, sample_rate, decimation, taps);
        let mut taps_rev2 = Vec::with_capacity(proto.ntaps * 2);
        for &tap in &proto.taps_rev {
            taps_rev2.push(tap);
            taps_rev2.push(tap);
        }
        Self {
            taps_rev2,
            decimation: proto.decimation,
            carry: vec![0.0; (proto.ntaps - 1) * 2],
            boundary: vec![0.0; proto.ntaps * 2],
            ntaps: proto.ntaps,
            phase: 0,
        }
    }

    /// Filter and decimate interleaved cf32 IQ. State and decimation phase are
    /// retained across calls, so any chunking of the stream yields bitwise
    /// identical output.
    pub fn process(&mut self, iq: &[f32]) -> Vec<f32> {
        debug_assert!(iq.len() % 2 == 0, "IQ must be interleaved pairs");
        let ntaps = self.ntaps;
        let dec = self.decimation;
        let carry_pairs = ntaps - 1;
        // A trailing odd sample cannot form a pair; ignore it exactly as the
        // previous `chunks_exact(2)` deinterleave did.
        let iq = &iq[..iq.len() & !1];
        let pairs = iq.len() / 2;
        let total_pairs = carry_pairs + pairs;

        let mut out = Vec::with_capacity((pairs / dec + 1) * 2);
        let mut k = self.phase;
        // Windows that begin inside the carried history: assemble
        // [carry tail ++ input head] in the retained boundary buffer. The
        // window contents (and therefore the output bits) are identical to the
        // steady-state path, only their storage differs.
        while k < carry_pairs && k + ntaps <= total_pairs {
            let from_carry = (carry_pairs - k) * 2;
            let from_input = ntaps * 2 - from_carry;
            self.boundary[..from_carry].copy_from_slice(&self.carry[k * 2..]);
            self.boundary[from_carry..].copy_from_slice(&iq[..from_input]);
            let (i, q) = dot_product_iq(&self.taps_rev2, &self.boundary);
            out.push(i);
            out.push(q);
            k += dec;
        }
        // Steady state: the window lies entirely inside the caller's block and
        // is read in place — no copy.
        while k + ntaps <= total_pairs {
            let start = (k - carry_pairs) * 2;
            let (i, q) = dot_product_iq(&self.taps_rev2, &iq[start..start + ntaps * 2]);
            out.push(i);
            out.push(q);
            k += dec;
        }

        // The loop stops with `k >= pairs` (`pairs` is the count of newly
        // consumable window positions), so the next block resumes at
        // `k - pairs` in its own [carry ++ input] frame.
        debug_assert!(k >= pairs, "decimation phase would go backwards");
        self.phase = k - pairs;
        // carry := last `carry_pairs` complex samples of [carry ++ input].
        if pairs >= carry_pairs {
            self.carry
                .copy_from_slice(&iq[iq.len() - carry_pairs * 2..]);
        } else {
            self.carry.copy_within(pairs * 2.., 0);
            self.carry[(carry_pairs - pairs) * 2..].copy_from_slice(iq);
        }
        out
    }
}

/// Deemphasis filter (FM broadcast audio). Time constant: 75µs (US) or 50µs
/// (EU). A simple one-pole IIR low-pass.
pub struct Deemphasis {
    alpha: f32,
    state: f32,
}

impl Deemphasis {
    /// Build with the given time constant at the given sample rate.
    #[must_use]
    pub fn new(tau_seconds: f32, sample_rate: f32) -> Self {
        let dt = 1.0 / sample_rate;
        let alpha = dt / (tau_seconds + dt);
        Self { alpha, state: 0.0 }
    }

    /// US FM broadcast (75µs).
    #[must_use]
    pub fn us_fm(sample_rate: f32) -> Self {
        Self::new(75e-6, sample_rate)
    }

    /// EU FM broadcast (50µs).
    #[must_use]
    pub fn eu_fm(sample_rate: f32) -> Self {
        Self::new(50e-6, sample_rate)
    }

    /// Process a block; modifies in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            self.state += self.alpha * (*s - self.state);
            *s = self.state;
        }
    }
}

/// DC blocker: a leaky integrator that subtracts the slow DC component.
pub struct DcBlocker {
    alpha: f32,
    state: f32,
}

impl DcBlocker {
    /// Build with a time constant (smaller alpha = faster tracking).
    #[must_use]
    pub fn new(alpha: f32) -> Self {
        Self { alpha, state: 0.0 }
    }

    /// Process a block; modifies in place (removes DC).
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            self.state += self.alpha * (*s - self.state);
            *s -= self.state;
        }
    }

    /// Apply optional deemphasis followed by DC blocking in one cache pass.
    /// This is algebraically identical to calling the two filters separately;
    /// their state recurrences are independent and remain sample-ordered.
    pub(crate) fn process_after_deemphasis(
        &mut self,
        samples: &mut [f32],
        deemphasis: Option<&mut Deemphasis>,
    ) {
        let Some(deemphasis) = deemphasis else {
            self.process(samples);
            return;
        };
        for sample in samples {
            deemphasis.state += deemphasis.alpha * (*sample - deemphasis.state);
            *sample = deemphasis.state;
            self.state += self.alpha * (*sample - self.state);
            *sample -= self.state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_pass_attenuates_high_frequency() {
        let fs = 48_000.0_f32;
        let mut lp = LowPass::new(3000.0, fs, 1, 31);
        // 5 kHz tone (above 3 kHz cutoff) should be attenuated vs DC.
        let high: Vec<f32> = (0..1024)
            .map(|i| (2.0 * PI * 5000.0 * i as f32 / fs).sin())
            .collect();
        let dc_in = vec![1.0_f32; 1024];
        let high_out = lp.process(&high);
        let dc_out = lp.process(&dc_in);
        let high_rms = rms(&high_out[100..]);
        let dc_rms = rms(&dc_out[100..]);
        assert!(
            high_rms < dc_rms * 0.3,
            "high {high_rms} should be < 30% of dc {dc_rms}"
        );
    }

    #[test]
    fn low_pass_passes_low_frequency() {
        let fs = 48_000.0_f32;
        let mut lp = LowPass::new(3000.0, fs, 1, 31);
        let low: Vec<f32> = (0..1024)
            .map(|i| (2.0 * PI * 500.0 * i as f32 / fs).sin())
            .collect();
        let out = lp.process(&low);
        // Output amplitude should be close to input amplitude after the
        // transient settles.
        let out_rms = rms(&out[200..]);
        let in_rms = rms(&low[200..]);
        assert!(
            out_rms > in_rms * 0.7,
            "low {out_rms} should be > 70% of input {in_rms}"
        );
    }

    #[test]
    fn low_pass_streaming_matches_one_block() {
        // The decimation phase and FIR history must be persistent across every
        // possible split, especially boundaries not divisible by decimation.
        let fs = 48_000.0_f32;
        let signal: Vec<f32> = (0..257)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / fs).sin())
            .collect();
        let mut whole = LowPass::new(3000.0, fs, 4, 31);
        let out_whole = whole.process(&signal);
        for split_at in 0..=signal.len() {
            let mut split = LowPass::new(3000.0, fs, 4, 31);
            let mut out_split = split.process(&signal[..split_at]);
            out_split.extend(split.process(&signal[split_at..]));
            assert_eq!(
                out_whole, out_split,
                "streaming mismatch at input sample {split_at}"
            );
        }
    }

    #[test]
    fn low_pass_matches_direct_convolution() {
        // Absolute correctness, not just self-consistency: every emitted sample
        // must equal the textbook decimating convolution
        // `y[m] = sum_j h[j] * x[m*D - j]` with the stream zero-padded before
        // its start. A rewrite that gets the window offset, the tap direction,
        // or the decimation phase wrong still passes the chunk-invariance
        // tests, but fails here.
        let fs = 48_000.0_f32;
        let signal: Vec<f32> = (0..600)
            .map(|i| (2.0 * PI * 1500.0 * i as f32 / fs).sin() + 0.25 * (i as f32 * 0.31).cos())
            .collect();

        // Accuracy is judged against the signal scale, not against each output:
        // near a zero crossing the relative error of a single sample is
        // unbounded and says nothing about the filter.
        let peak = signal.iter().fold(0.0f32, |m, v| m.max(v.abs()));

        for &(dec, taps) in &[(1_usize, 31_usize), (4, 31), (5, 63), (7, 81)] {
            let mut lp = LowPass::new(3000.0, fs, dec, taps);
            // Taps are stored reversed; recover design order for the reference.
            let mut h = lp.taps_rev.clone();
            h.reverse();
            let ntaps = h.len();

            let got = lp.process(&signal);
            for (m, &y) in got.iter().enumerate() {
                let center = m * dec;
                let mut want = 0.0f32;
                for (j, &tap) in h.iter().enumerate() {
                    // Samples before the stream start read as zero.
                    if let Some(idx) = center.checked_sub(j) {
                        want += tap * signal[idx];
                    }
                }
                assert!(
                    (y - want).abs() <= 1e-6 * peak,
                    "dec={dec} taps={ntaps} output {m}: got {y}, want {want}"
                );
            }
            // The emitted count must cover exactly the outputs whose centers
            // land inside the block.
            assert_eq!(got.len(), signal.len().div_ceil(dec), "dec={dec} count");
        }
    }

    #[test]
    fn low_pass_phase_survives_irregular_chunking() {
        // The scratch/carry rewrite carries the decimation phase across block
        // boundaries as an offset. Prime-sized chunks land the phase on every
        // residue and would expose an off-by-one that uniform splits hide.
        let fs = 48_000.0_f32;
        let signal: Vec<f32> = (0..1021)
            .map(|i| (2.0 * PI * 900.0 * i as f32 / fs).sin())
            .collect();
        for &dec in &[3_usize, 4, 7, 50] {
            let mut whole = LowPass::new(3000.0, fs, dec, 63);
            let expected = whole.process(&signal);
            for &chunk in &[1_usize, 2, 3, 5, 11, 13, 64, 257] {
                let mut streamed = LowPass::new(3000.0, fs, dec, 63);
                let mut got = Vec::new();
                for block in signal.chunks(chunk) {
                    got.extend(streamed.process(block));
                }
                assert_eq!(
                    expected, got,
                    "dec={dec} chunk={chunk}: streaming diverged from whole-block"
                );
            }
        }
    }

    #[test]
    fn low_pass_rejects_alias_of_above_output_nyquist() {
        // Decimate by 4 from 48 kHz → output rate 12 kHz, output Nyquist 6 kHz.
        // A tone at 9 kHz is above the output Nyquist. A correct decimating
        // filter (every sample into the FIR) attenuates it strongly instead of
        // aliasing it into the passband (where it would appear near 3 kHz).
        let fs = 48_000.0_f32;
        let dec = 4;
        let out_fs = fs / dec as f32;
        let mut lp = LowPass::new(out_fs / 2.0 - 1500.0, fs, dec, 63);
        let alias_freq = 9_000.0; // above out_nyquist=6k, aliases to 3k if unfiltered
        let tone: Vec<f32> = (0..8192)
            .map(|i| (2.0 * PI * alias_freq * i as f32 / fs).sin())
            .collect();
        let out = lp.process(&tone);
        // Measure the residual energy at the alias frequency (3 kHz) and the
        // passband overall. The aliased component must be well below the input.
        let out_rms = rms(&out[out.len() / 2..]);
        let in_rms = rms(&tone);
        // Strong attenuation (>20 dB). A buggy striding filter would pass the
        // alias through at near-full amplitude.
        assert!(
            out_rms < in_rms * 0.1,
            "alias leaked through: out_rms={out_rms}, in_rms={in_rms}"
        );
    }

    #[test]
    fn complex_low_pass_is_chunk_invariant() {
        let fs = 192_000.0_f32;
        let n = 257usize;
        let iq: Vec<f32> = (0..n)
            .flat_map(|sample| {
                let phase = 2.0 * PI * 3_000.0 * sample as f32 / fs;
                [phase.cos(), phase.sin()]
            })
            .collect();
        let mut whole = ComplexLowPass::new(8_000.0, fs, 4, 63);
        let expected = whole.process(&iq);
        for split_sample in 0..=n {
            let split_at = split_sample * 2;
            let mut chunked = ComplexLowPass::new(8_000.0, fs, 4, 63);
            let mut got = chunked.process(&iq[..split_at]);
            got.extend(chunked.process(&iq[split_at..]));
            assert_eq!(expected, got, "split at complex sample {split_sample}");
        }
    }

    #[test]
    fn complex_low_pass_matches_direct_convolution() {
        // Absolute correctness against the textbook decimating convolution
        // `z[m] = sum_j h[j] * x[m*D - j]` applied per component on the
        // zero-padded stream. The fused interleaved dot product could get the
        // window offset, tap duplication, decimation phase, or I/Q lane
        // assignment wrong and still pass every self-consistency test; not
        // this one.
        let fs = 2_400_000.0_f32;
        let n = 700_usize;
        let iq: Vec<f32> = (0..n)
            .flat_map(|s| {
                let x = s as f32;
                [
                    (x * 0.0173).sin() + 0.25 * (x * 0.031).cos(),
                    (x * 0.0129).cos() - 0.5 * (x * 0.047).sin(),
                ]
            })
            .collect();
        let peak = iq.iter().fold(0.0f32, |m, v| m.max(v.abs()));

        for &(dec, taps) in &[(1_usize, 31_usize), (4, 31), (5, 63), (10, 81), (50, 31)] {
            let mut clp = ComplexLowPass::new(90_000.0, fs, dec, taps);
            // Taps are stored reversed with each value duplicated; recover the
            // design-order kernel for the reference (and check the pairing).
            let mut h: Vec<f32> = clp
                .taps_rev2
                .chunks_exact(2)
                .map(|pair| {
                    // Bitwise equality intended: both are copies of one tap.
                    assert_eq!(
                        pair[0].to_bits(),
                        pair[1].to_bits(),
                        "taps must be duplicated per lane pair"
                    );
                    pair[0]
                })
                .collect();
            h.reverse();

            let got = clp.process(&iq);
            assert_eq!(got.len() / 2, n.div_ceil(dec), "dec={dec} output count");
            for (m, z) in got.chunks_exact(2).enumerate() {
                let center = m * dec;
                let mut want_i = 0.0f32;
                let mut want_q = 0.0f32;
                for (j, &tap) in h.iter().enumerate() {
                    // Samples before the stream start read as zero.
                    if let Some(idx) = center.checked_sub(j) {
                        want_i += tap * iq[idx * 2];
                        want_q += tap * iq[idx * 2 + 1];
                    }
                }
                assert!(
                    (z[0] - want_i).abs() <= 1e-6 * peak,
                    "dec={dec} taps={taps} output {m} I: got {}, want {want_i}",
                    z[0]
                );
                assert!(
                    (z[1] - want_q).abs() <= 1e-6 * peak,
                    "dec={dec} taps={taps} output {m} Q: got {}, want {want_q}",
                    z[1]
                );
            }
        }
    }

    #[test]
    fn complex_low_pass_streaming_survives_prime_chunking() {
        // The boundary-window/steady-state split moves with the chunking, and
        // prime chunk sizes land the decimation phase on every residue. Output
        // must stay bitwise identical to the whole-block run regardless.
        let fs = 2_400_000.0_f32;
        let n = 1021_usize; // prime count of complex samples
        let iq: Vec<f32> = (0..n)
            .flat_map(|s| {
                let x = s as f32;
                [(x * 0.017).sin(), (x * 0.013).cos()]
            })
            .collect();
        for &(dec, taps) in &[(10_usize, 81_usize), (4, 63), (7, 27), (50, 31)] {
            let mut whole = ComplexLowPass::new(90_000.0, fs, dec, taps);
            let expected = whole.process(&iq);
            for &chunk_pairs in &[1_usize, 2, 3, 5, 11, 13, 61, 127, 257] {
                let mut streamed = ComplexLowPass::new(90_000.0, fs, dec, taps);
                let mut got = Vec::new();
                for block in iq.chunks(chunk_pairs * 2) {
                    got.extend(streamed.process(block));
                }
                assert_eq!(
                    expected, got,
                    "dec={dec} taps={taps} chunk={chunk_pairs}: streaming diverged"
                );
            }
        }
    }

    #[test]
    fn complex_low_pass_preserves_quadrature() {
        // A passband complex exponential must come out as a complex
        // exponential at the same frequency: near-unity constant magnitude and
        // a phase increment of exactly 2*pi*f*dec/fs per output sample. An I/Q
        // swap flips the increment's sign, and a one-sample misalignment
        // between the components ripples the magnitude; both fail here.
        let fs = 2_400_000.0_f32;
        let f = 50_000.0_f32;
        let dec = 10_usize;
        let n = 4096_usize;
        let iq: Vec<f32> = (0..n)
            .flat_map(|s| {
                let phase = 2.0 * PI * f * s as f32 / fs;
                [phase.cos(), phase.sin()]
            })
            .collect();
        let mut clp = ComplexLowPass::new(90_000.0, fs, dec, 81);
        let out = clp.process(&iq);
        // Skip the fill-in transient before the FIR window holds only signal.
        let settled = &out[20 * 2..];
        let expected_step = 2.0 * PI * f * dec as f32 / fs;
        for (prev, next) in settled.chunks_exact(2).zip(settled.chunks_exact(2).skip(1)) {
            let mag = (next[0] * next[0] + next[1] * next[1]).sqrt();
            assert!((mag - 1.0).abs() < 0.05, "magnitude drifted: {mag}");
            // angle(next * conj(prev)) is the per-output phase advance.
            let re = next[0] * prev[0] + next[1] * prev[1];
            let im = next[1] * prev[0] - next[0] * prev[1];
            let step = im.atan2(re);
            assert!(
                (step - expected_step).abs() < 1e-3,
                "phase step {step} != {expected_step}"
            );
        }
    }

    #[test]
    fn deemphasis_smooths_high_frequencies() {
        let mut dp = Deemphasis::us_fm(48_000.0);
        // Step input: deemphasis should produce a rising-then-decaying response.
        let mut samples = vec![1.0_f32; 1000];
        dp.process(&mut samples);
        // After 1000 samples the output should be near the input (steady state).
        assert!(
            (samples[999] - 1.0).abs() < 0.01,
            "steady state {}",
            samples[999]
        );
    }

    #[test]
    fn dc_blocker_removes_dc_offset() {
        let mut dc = DcBlocker::new(0.01);
        let mut samples: Vec<f32> = (0..2000).map(|_| 1.0).collect();
        dc.process(&mut samples);
        // After enough samples, the DC offset should be removed.
        assert!(
            samples[1999].abs() < 0.1,
            "DC not removed: {}",
            samples[1999]
        );
    }

    #[test]
    fn fused_deemphasis_dc_matches_two_pass_pipeline() {
        let input: Vec<f32> = (0..1_003)
            .map(|index| (index as f32 * 0.173).sin() + 0.25)
            .collect();
        let mut expected = input.clone();
        let mut separate_deemphasis = Deemphasis::us_fm(48_000.0);
        let mut separate_dc = DcBlocker::new(0.01);
        separate_deemphasis.process(&mut expected);
        separate_dc.process(&mut expected);

        let mut actual = input;
        let mut fused_deemphasis = Deemphasis::us_fm(48_000.0);
        let mut fused_dc = DcBlocker::new(0.01);
        fused_dc.process_after_deemphasis(&mut actual, Some(&mut fused_deemphasis));
        assert_eq!(actual, expected);
    }

    fn rms(s: &[f32]) -> f32 {
        let sum: f32 = s.iter().map(|x| x * x).sum();
        (sum / s.len() as f32).sqrt()
    }
}
