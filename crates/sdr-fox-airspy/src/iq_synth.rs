//! Airspy IQ synthesis.
//!
//! Airspy One devices (R2 and Mini) stream **REAL** 12-bit samples at **2× the
//! configured IQ rate**. To turn that common wire format into a proper
//! analytic (complex) signal we must:
//!
//! 1. Mix the real stream by an fs/4 complex mixer. This shifts the spectrum
//!    by fs/4 so one half of the real spectrum can be isolated.
//! 2. Apply a half-band low-pass FIR (null at fs/2 of the input rate) to remove
//!    the image.
//! 3. Decimate by 2 (the Mini's 2× oversampling factor).
//! 4. Emit the **conjugate** of the plain `exp(-j·π·n/2)` down-mix result:
//!    the bare fs/4 real→complex path is spectrally inverted relative to a
//!    native-IQ receiver, so without this a signal at `fc+Δ` would appear at
//!    `fc−Δ` (an exact spectral mirror). SondeFox's hardware-validated
//!    converter applies the same fix. Because conjugating after a real-valued
//!    linear filter equals conjugating before it, the conjugate is folded
//!    into the mixer sign table (`MIXER_SIGN`) at zero runtime cost — the
//!    stream is effectively mixed by `exp(+j·π·n/2)` = `[1, +j, -1, -j, …]`.
//!
//! All state — mixer phase, FIR history — is preserved across calls so the
//! synthesizer is truly streaming.
//!
//! # Kernel structure
//!
//! The (conjugated) mixer output is structurally sparse: `i[n] = x[n]·[1,0,-1,0]`
//! and `q[n] = x[n]·[0,+1,0,-1]`, so each input sample lands in exactly one rail
//! with a sign of `[+1,+1,-1,-1][n mod 4]`. Folding that sign into the stored
//! sample and splitting the stream by the parity of `n` yields two half-rate
//! real streams from which each decimated output is assembled **without any
//! sign or phase bookkeeping**:
//!
//! * the half-band's nonzero non-center taps (all at odd offsets from the
//!   center) form a plain dense convolution of one parity stream, and
//! * the center tap is a pure delayed read of the other parity stream.
//!
//! Concretely, with `mid = ntaps/2` odd (the default 47-tap kernel), output
//! `m` (at input index `n = 2m`) is
//! `I[m] = Σ_k h[2k]·E[m-k]` and `Q[m] = h[mid]·O[m-(mid+1)/2]`, where
//! `E[m] = x[2m]·(-1)^m` and `O[p] = x[2p+1]·(-1)^p` are the sign-folded
//! parity streams. When `mid` is even the roles of the two streams swap.
//!
//! The convolution therefore walks its taps and its window forward over flat
//! contiguous slices — the shape LLVM vectorizes — instead of the previous
//! stride-2 reversed gather over a doubled ring, and half of the former
//! multiply/store traffic (the structurally zero rail) disappears entirely.
//! On this crate's `iq_synth` criterion bench (M1 Max) the whole-path
//! restructure measures ~2.5× the previous per-container kernel; the
//! remaining floor is the DC blocker's serial per-sample recurrence (see
//! `RC-S01-H07`: blocking it would break chunk invariance, and reshaping it to
//! `dc = 0.99·dc + 0.01·s` measured no gain). The one shortening of that chain
//! that did pay is the fused multiply-add in `decode_dc`, worth ~20% of the
//! whole path on aarch64; the `FUSED_DC` constant carries the measurements and
//! the reason it is gated on the target rather than applied unconditionally.
//! (Both are private items, so they are named here rather than linked.)

/// Synthesizes complex cu8 IQ from raw Airspy sample containers.
pub struct IqSynthesizer {
    /// Half-band decimation FIR coefficients (symmetric, design order).
    /// The streaming kernel runs on the packed forms below; this is the
    /// specification the tests verify those forms against.
    #[cfg_attr(not(test), allow(dead_code))]
    hbf: Vec<f32>,
    /// The nonzero non-center taps (`h[mid±1], h[mid±3], …`), stored packed
    /// and reversed so the polyphase dot product walks taps and window
    /// forward together.
    poly_taps_rev: Vec<f32>,
    /// The center tap `h[mid]`, applied as a pure delay on the other rail.
    center_tap: f32,
    /// Retained tail of the convolution-side parity stream: exactly the
    /// history the next block's first output needs.
    poly_carry: Vec<f32>,
    /// Retained tail of the delay-side parity stream (the center tap's
    /// delay-line depth).
    delay_carry: Vec<f32>,
    /// Reusable `[carry ++ new samples]` buffers; retained across calls so a
    /// steady-state stream performs no allocation here.
    poly_scratch: Vec<f32>,
    delay_scratch: Vec<f32>,
    /// Staging for the polyphase dot products and the scaled delayed samples,
    /// handed to the per-format emit pass as flat slices so quantization
    /// vectorizes instead of round-tripping through per-sample pushes.
    stage_poly: Vec<f32>,
    stage_center: Vec<f32>,
    /// fs/4 complex-mixer phase, `0..=3`. Phase p multiplies by
    /// `[1, +j, -1, -j][p]` (conjugated mixer; see [`MIXER_SIGN`]).
    /// Its low bit is also the decimation phase: outputs are emitted on
    /// even-parity input samples.
    mixer_phase: usize,
    /// Streaming estimate used by the one-pole DC blocker.
    dc_average: f32,
    /// A trailing low byte from an odd-length USB completion. Airspy samples
    /// are 16-bit LE containers, so it must be joined to the next block.
    pending_low_byte: Option<u8>,
    /// Whether `ntaps/2` is odd: selects which parity stream feeds the
    /// polyphase convolution (and which rail of the output it produces).
    mid_odd: bool,
    /// Raw ADC-domain containers decoded by the most recent synthesize call.
    last_raw_samples: u64,
    /// Containers at/beyond the ADC rails in the most recent synthesize call.
    last_clips: u64,
}

/// Mixer sign of the nonzero rail at phase `p`, **with the output conjugate
/// folded in** — this table is the single place the G4 sideband fix lives.
///
/// The fs/4 down-mixer alone is `exp(-j·π·n/2)`: `i[n] = x·[1,0,-1,0]`,
/// `q[n] = x·[0,-1,0,1]`, surviving factor `[+1,-1,-1,+1][p]`. That
/// convention is spectrally inverted relative to a native-IQ receiver: a
/// signal at fc+Δ lands at fc−Δ (an exact mirror). FSK sonde decoders are
/// polarity-immune so frames still decode — the failure is silent mistuned
/// frequency bookkeeping, and chirp-sense modes (`LoRa`) break outright. The
/// fix is to emit the conjugate, i.e. mix by `exp(+j·π·n/2)` instead:
/// `q[n] = x·[0,+1,0,-1]`, so the surviving factor becomes
/// `[+1,+1,-1,-1][p]`. Folding the conjugate into the sign table (rather
/// than negating Q in every emit path) fixes all output formats at zero
/// runtime cost, and is bit-exact with post-filter negation because the FIR
/// is real-valued and IEEE rounding is sign-symmetric. Matches the
/// hardware-validated convention of SondeFox's converter.
const MIXER_SIGN: [f32; 4] = [1.0, 1.0, -1.0, -1.0];

/// Sixteen consecutive sliding-window dot products sharing one walk over the
/// taps, with four independent accumulator chains per output lane.
///
/// `win` must hold at least `taps.len() + 15` samples; lane `l` computes
/// `Σ_k taps[k]·win[l + k]`. One accumulator per lane would serialize each
/// output on FMA latency over the whole kernel (the same defect the scalar
/// filters had); four chains per lane keep the FMA pipes saturated. Measured
/// on an M1 Max at the default 24 nonzero taps: 6.9 GMAC/s with one chain,
/// 20.9 with four.
#[inline]
fn sliding_dot_x16(taps: &[f32], win: &[f32]) -> [f32; 16] {
    let mut acc = [[0.0f32; 16]; 4];
    let mut quads = taps.chunks_exact(4);
    let mut k = 0usize;
    for quad in quads.by_ref() {
        for (c, &g) in quad.iter().enumerate() {
            let w: &[f32; 16] = (&win[k + c..k + c + 16]).try_into().unwrap();
            for lane in 0..16 {
                acc[c][lane] += g * w[lane];
            }
        }
        k += 4;
    }
    for (c, &g) in quads.remainder().iter().enumerate() {
        let w: &[f32; 16] = (&win[k + c..k + c + 16]).try_into().unwrap();
        for lane in 0..16 {
            acc[c][lane] += g * w[lane];
        }
    }
    let mut out = [0.0f32; 16];
    for lane in 0..16 {
        out[lane] = (acc[0][lane] + acc[1][lane]) + (acc[2][lane] + acc[3][lane]);
    }
    out
}

/// Default 24-coefficient filter with a fixed window extent. The fixed bounds
/// let the compiler remove the four checked slices from every tap quad. Each
/// lane retains the dynamic kernel's four non-fused chains and reduction tree.
#[inline]
fn sliding_dot_24_x16(taps: &[f32; 24], win: &[f32; 39]) -> [f32; 16] {
    let mut acc = [[0.0f32; 16]; 4];
    for k in (0..24).step_by(4) {
        for c in 0..4 {
            let g = taps[k + c];
            for lane in 0..16 {
                acc[c][lane] += g * win[k + c + lane];
            }
        }
    }
    let mut out = [0.0f32; 16];
    for lane in 0..16 {
        out[lane] = (acc[0][lane] + acc[1][lane]) + (acc[2][lane] + acc[3][lane]);
    }
    out
}

/// Scalar mirror of one `sliding_dot_x16` lane: the same four chains keyed by
/// tap index mod 4 and the same reduction tree, hence bit-identical results.
///
/// This is what keeps the stream bitwise chunk-invariant: an output computed
/// here (because it fell in a block's ragged tail) must round exactly like
/// the same output computed in a 16-wide pass under a different chunking.
#[inline]
fn sliding_dot_tail(taps: &[f32], win: &[f32]) -> f32 {
    let mut chains = [0.0f32; 4];
    for (k, (&g, &w)) in taps.iter().zip(win).enumerate() {
        chains[k & 3] += g * w;
    }
    (chains[0] + chains[1]) + (chains[2] + chains[3])
}

/// Whether a decoded 12-bit container sits at/beyond the ADC rails:
/// `|v − 2048| >= 2044`, i.e. within 4 counts of 0 or 4095. This is the
/// raw-domain overload threshold the SondeFox Airspy AGC is tuned to.
#[inline]
fn is_clip(v: u16) -> bool {
    (i32::from(v) - 2048).abs() >= 2044
}

/// Whether to advance the DC blocker with a fused multiply-add.
///
/// **This flag changes the output bits, so it is deliberately a property of
/// the target and not of the CPU the binary happens to run on.** Do not
/// convert it to a runtime `is_x86_feature_detected!` dispatch: that would
/// make one binary produce different samples on different machines, which is
/// far worse than the build-time split documented here.
///
/// # Why gate it at all
///
/// `f32::mul_add` is *contractually* fused — one rounding, correctly rounded —
/// on every platform, so it is bit-identical whether it lowers to hardware or
/// to libm. Only its **cost** varies, and it varies enormously:
///
/// * `aarch64` has a mandatory `fmadd`, so the recurrence's loop-carried chain
///   drops from `fsub → fmul → fadd` to `fsub → fmadd`.
/// * The `x86_64` targets this project ships (`x86_64-unknown-linux-gnu` is
///   plain SSE2; `x86_64-linux-android` is SSE4.2) have **no** FMA
///   instruction, so `mul_add` lowers to `callq fmaf` — a non-inlinable libm
///   call per sample, inside the serial chain.
///
/// Measured on an M1 Max, isolated recurrence, 65536 samples, min-of-300:
/// aarch64 native 203.6 µs → 142.0 µs (**−30.2%**); `x86_64` (under Rosetta,
/// so indicative rather than native) 230.5 µs → 339.7 µs (**+47.4%**). The
/// `x86_64` direction is the one that matters here: it is a *regression*, which
/// is why this is gated rather than applied everywhere.
///
/// On the full path, this crate's `iq_synth` criterion bench measures
/// 328.6 µs → 262.4 µs, **−20.2%** (p = 0.00); an interleaved A/B harness
/// (min-of-800, so immune to machine drift) independently gives −19.4%.
/// Holding the refactor fixed and flipping only this constant reproduces the
/// same delta, so the win is attributable to the fused multiply-add and not to
/// the surrounding restructure.
///
/// # The cost of gating
///
/// Because the fused and unfused forms round differently, this makes Airspy
/// output depend on the build target at the ~1 ULP level. That divergence is
/// bounded and tiny — see [`decode_dc`] — and bitwise *chunk* invariance, the
/// property this module actually contracts, holds on either side of the gate.
/// `x86_64` with `-C target-feature=+fma` takes the fused path and so matches
/// aarch64 bit-for-bit.
const FUSED_DC: bool = cfg!(any(target_arch = "aarch64", target_feature = "fma"));

/// One DC-blocker step, rounding the `0.01·x` product before adding it.
#[inline]
fn dc_step_unfused(x: f32, dc: f32) -> f32 {
    dc + 0.01 * x
}

/// One DC-blocker step with a single rounding, via `fmadd` where one exists.
#[inline]
fn dc_step_fused(x: f32, dc: f32) -> f32 {
    0.01f32.mul_add(x, dc)
}

/// Scale a 12-bit container to [-1, 1) and apply the one-pole DC blocker.
///
/// This is a serial dependency chain and the throughput floor of the path, so
/// it is advanced with a fused multiply-add where the target has one; see
/// [`FUSED_DC`] for the measurements and the portability argument. The chain
/// is *not* otherwise reshaped: blocking it would break exact chunk
/// invariance, and `dc = 0.99·dc + 0.01·s` measured no gain (`RC-S01-H07`).
///
/// # Numerical difference between the two forms
///
/// The fused form skips the rounding of the `0.01·x` product, so the two
/// recurrences drift apart, and because `dc` feeds the next sample the
/// difference persists rather than staying a one-shot ULP. It is nonetheless
/// **bounded, not cumulative**, because the filter is contractive: writing `e`
/// for the difference between the two DC estimates, `e ← 0.99·e + δ` where `δ`
/// is one step's rounding difference. The 0.99 pole makes that a geometric
/// series summing to 100, so `e` reaches an equilibrium instead of
/// integrating. The loop *attenuates* the persistence of each difference; the
/// worst case is `100·δ`, not `n·δ`.
///
/// Measured per sample (not per block — sampling only at block boundaries
/// under-reports the peak) over 10^7 containers on each of six deliberately
/// adversarial streams: rail-to-rail square, held rail, 7/8-asymmetric duty, a
/// ramp that never lets `dc` settle, a tie-seeking sweep, and full-scale
/// alternation. Worst DC-state divergence **1.19e-7**, worst sample divergence
/// **2.38e-7** — 5e-4 of a single 12-bit ADC LSB (1/2048). Crucially the
/// per-decade maxima are flat: the divergence rises while `dc` converges
/// (~10^3 samples) and then stops growing through 10^4, 10^5 and 10^6. Over
/// 4·10^6 containers of realistic signal the packed cu8 output differed in 1
/// byte total.
///
/// `fused_and_unfused_dc_recurrences_stay_within_the_documented_bound` pins
/// both the magnitude and the non-accumulation.
#[inline]
fn decode_dc<const FUSED: bool>(v: u16, dc: &mut f32) -> f32 {
    let mut x = (f32::from(v) - 2048.0) / 2048.0;
    x -= *dc;
    // `FUSED` is a const parameter, so exactly one arm survives codegen; this
    // must never become a runtime branch in this loop.
    *dc = if FUSED {
        dc_step_fused(x, *dc)
    } else {
        dc_step_unfused(x, *dc)
    };
    x
}

impl Default for IqSynthesizer {
    fn default() -> Self {
        Self::new()
    }
}

impl IqSynthesizer {
    /// Construct with the default 47-tap half-band FIR. This keeps the useful
    /// passband flat through 0.20 of the real input rate while rejecting the
    /// mirror beyond 0.30; the earlier 15-tap kernel only passed an easy
    /// interior-tone test and degraded badly near the usable-band edge.
    #[must_use]
    pub fn new() -> Self {
        Self::with_taps(47)
    }

    /// Construct with an N-tap (odd) half-band FIR. The coefficients are the
    /// ideal half-band impulse response `h[k] = 0.5·sinc(0.5·(k − M/2))` (cutoff
    /// at fs/4), windowed with Hamming and normalized to unity DC gain.
    #[must_use]
    pub fn with_taps(taps: usize) -> Self {
        let taps = taps.max(5) | 1; // odd, >=5
        let mid = (taps / 2) as f32;
        let mut hbf = Vec::with_capacity(taps);
        for n in 0..taps {
            let k = n as f32 - mid;
            let integer_offset = n as isize - taps as isize / 2;
            // Ideal half-band: cutoff at fs/4 ⇒ 0.5·sinc(0.5·k).
            // Every non-centre even offset is mathematically zero. Store it as
            // exact zero rather than a small `sin(k*pi)` rounding residue so
            // the streaming kernel can execute the sparse polyphase form.
            let c = if integer_offset != 0 && integer_offset % 2 == 0 {
                0.0
            } else {
                let arg = std::f32::consts::PI * 0.5 * k;
                let sinc = if arg.abs() < 1e-9 {
                    1.0
                } else {
                    arg.sin() / arg
                };
                0.5 * sinc
            };
            let win =
                0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / (taps - 1) as f32).cos();
            hbf.push(c * win);
        }
        let sum: f32 = hbf.iter().sum();
        for t in &mut hbf {
            *t /= sum;
        }

        // Pack the structurally nonzero taps. With `mid` odd they sit at even
        // indices `h[2k], k = 0..=mid`; with `mid` even at odd indices
        // `h[2k+1], k = 0..mid`. Reversed so the dot product walks forward.
        let mid = taps / 2;
        let mid_odd = mid & 1 == 1;
        let mut poly: Vec<f32> = if mid_odd {
            (0..=mid).map(|k| hbf[2 * k]).collect()
        } else {
            (0..mid).map(|k| hbf[2 * k + 1]).collect()
        };
        poly.reverse();
        // The center tap reads the *other* parity stream `(mid+1)/2` (mid
        // odd) or `mid/2` (mid even) half-rate samples back; that is the
        // delay-line depth the carry must retain. The convolution side
        // retains `len-1` samples when its newest sample arrives with the
        // emitting (even-parity) input, and `len` when it arrives one input
        // earlier (mid even).
        let delay_depth = if mid_odd { mid.div_ceil(2) } else { mid / 2 };
        let poly_carry_len = if mid_odd { poly.len() - 1 } else { poly.len() };
        Self {
            center_tap: hbf[mid],
            poly_carry: vec![0.0; poly_carry_len],
            delay_carry: vec![0.0; delay_depth],
            poly_scratch: Vec::new(),
            delay_scratch: Vec::new(),
            stage_poly: Vec::new(),
            stage_center: Vec::new(),
            poly_taps_rev: poly,
            hbf,
            mixer_phase: 0,
            dc_average: 0.0,
            pending_low_byte: None,
            mid_odd,
            last_raw_samples: 0,
            last_clips: 0,
        }
    }

    /// Raw ADC-domain containers decoded by the most recent synthesize call
    /// (counted **before** filtering and decimation, so it is roughly 2× the
    /// complex samples that call produced). Per-call, not cumulative; a
    /// container split across calls counts in the call that completes it.
    ///
    /// This is the denominator for [`IqSynthesizer::last_clips`], mirroring
    /// the `raw_samples` field on `sdr_fox_core::IqBlock`.
    #[must_use]
    pub fn last_raw_samples(&self) -> u64 {
        self.last_raw_samples
    }

    /// Containers at/beyond the ADC rails in the most recent synthesize call:
    /// a 12-bit container `v` clips when `|v − 2048| >= 2044` (within 4
    /// counts of the rails — the overload threshold SondeFox's Airspy AGC is
    /// tuned to). Per-call, not cumulative.
    ///
    /// `last_clips() as f64 / last_raw_samples() as f64` is the call's clip
    /// fraction, the input to overload-driven gain step-down.
    #[must_use]
    pub fn last_clips(&self) -> u64 {
        self.last_clips
    }

    /// Compatibility name for synthesizing an Airspy R2 wire buffer. R2 and
    /// Mini use the same 2× real wire format and therefore the same converter.
    #[must_use]
    pub fn synthesize_r2(&mut self, raw: &[u8]) -> Vec<u8> {
        self.synthesize_mini(raw)
    }

    /// Synthesize complex cu8 IQ from an Airspy One raw buffer. The device
    /// delivers REAL 12-bit samples at 2× the IQ rate. We mix by fs/4 (creating
    /// an analytic signal), half-band-filter, decimate by 2, then pack to cu8.
    ///
    /// State (mixer phase, FIR history) persists across calls.
    #[must_use]
    pub fn synthesize_mini(&mut self, raw: &[u8]) -> Vec<u8> {
        self.synthesize_mini_with::<FUSED_DC, _>(raw, 2, |i_vals, q_vals, out| {
            out.extend(
                i_vals
                    .iter()
                    .zip(q_vals)
                    .flat_map(|(&i, &q)| [to_cu8(i), to_cu8(q)]),
            );
        })
    }

    /// Synthesize Mini input directly to interleaved cf32 `[i0, q0, i1, q1, …]`,
    /// retaining the full filter precision instead of round-tripping through
    /// cu8. Samples are clamped to `[-1.0, 1.0]`.
    ///
    /// State (mixer phase, FIR history) persists across calls, exactly as for
    /// [`IqSynthesizer::synthesize_mini`].
    #[must_use]
    pub fn synthesize_mini_cf32(&mut self, raw: &[u8]) -> Vec<f32> {
        self.synthesize_mini_with::<FUSED_DC, _>(raw, 2, |i_vals, q_vals, out| {
            out.extend(
                i_vals
                    .iter()
                    .zip(q_vals)
                    .flat_map(|(&i, &q)| [i.clamp(-1.0, 1.0), q.clamp(-1.0, 1.0)]),
            );
        })
    }

    /// Synthesize Mini input directly to interleaved cs8.
    pub(crate) fn synthesize_mini_cs8(&mut self, raw: &[u8]) -> Vec<i8> {
        self.synthesize_mini_with::<FUSED_DC, _>(raw, 2, |i_vals, q_vals, out| {
            out.extend(
                i_vals
                    .iter()
                    .zip(q_vals)
                    .flat_map(|(&i, &q)| [to_cs8(i), to_cs8(q)]),
            );
        })
    }

    /// Synthesize Mini input directly to interleaved cs16.
    pub(crate) fn synthesize_mini_cs16(&mut self, raw: &[u8]) -> Vec<i16> {
        self.synthesize_mini_with::<FUSED_DC, _>(raw, 2, |i_vals, q_vals, out| {
            out.extend(
                i_vals
                    .iter()
                    .zip(q_vals)
                    .flat_map(|(&i, &q)| [to_cs16(i), to_cs16(q)]),
            );
        })
    }

    /// Streaming Mini kernel: assemble 16-bit containers, DC-block, fold the
    /// fs/4 mixer signs into the two parity streams, run the sparse half-band
    /// as one flat convolution plus one delayed tap, and hand the results to
    /// `emit_block` as slices for format packing.
    ///
    /// Every arithmetic step is position-independent — the same input sample
    /// takes the identical sequence of rounded operations no matter how the
    /// byte stream is chunked — which keeps the output bitwise chunk-invariant.
    fn synthesize_mini_with<const FUSED: bool, T>(
        &mut self,
        raw: &[u8],
        elements_per_complex: usize,
        emit_block: impl Fn(&[f32], &[f32], &mut Vec<T>),
    ) -> Vec<T> {
        let complete = usize::midpoint(raw.len(), usize::from(self.pending_low_byte.is_some()));
        // Per-call raw-domain telemetry (G6): every full container decoded by
        // this call counts toward `last_raw_samples`.
        self.last_raw_samples = complete as u64;
        self.last_clips = 0;
        let mut out = Vec::with_capacity((complete / 2 + 1) * elements_per_complex);
        // Input-sample parity at block start; also the offset aligning this
        // block's first output into the delay/poly scratch frames below.
        let phi = self.mixer_phase & 1;

        let even_count = usize::midpoint(complete, usize::from(phi == 0));
        let odd_count = complete - even_count;
        let (poly_count, delay_count) = if self.mid_odd {
            (even_count, odd_count)
        } else {
            (odd_count, even_count)
        };
        // Keep initialized storage between calls. Routing overwrites exactly
        // these spans, with no per-sample Vec length/capacity bookkeeping.
        self.poly_scratch
            .resize(self.poly_carry.len() + poly_count, 0.0);
        self.poly_scratch[..self.poly_carry.len()].copy_from_slice(&self.poly_carry);
        self.delay_scratch
            .resize(self.delay_carry.len() + delay_count, 0.0);
        self.delay_scratch[..self.delay_carry.len()].copy_from_slice(&self.delay_carry);

        // ---- decode containers, DC-block, fold mixer signs, split by parity.
        let mut pending_head: Option<u16> = None;
        let mut offset = 0usize;
        if let Some(low) = self.pending_low_byte.take() {
            if let Some(&high) = raw.first() {
                pending_head = Some(u16::from_le_bytes([low, high]) & 0x0fff);
                offset = 1;
            } else {
                self.pending_low_byte = Some(low);
                return out;
            }
        }
        // Count raw ADC rails at the same masked containers the router uses.
        // A pending or partial container is counted only when completed.
        self.last_clips = self.route_containers::<FUSED>(pending_head, &raw[offset..]);
        self.run_polyphase(phi);

        // The polyphase rail is I when `mid` is odd, Q when it is even.
        if self.mid_odd {
            emit_block(&self.stage_poly, &self.stage_center, &mut out);
        } else {
            emit_block(&self.stage_center, &self.stage_poly, &mut out);
        }

        // ---- retain exactly the history the next block needs.
        let cs = self.poly_scratch.len();
        let ccl = self.poly_carry.len();
        self.poly_carry
            .copy_from_slice(&self.poly_scratch[cs - ccl..]);
        let ds = self.delay_scratch.len();
        let dcl = self.delay_carry.len();
        self.delay_carry
            .copy_from_slice(&self.delay_scratch[ds - dcl..]);
        out
    }

    /// Decode 16-bit containers from `body` (preceded by an optional
    /// already-assembled container), DC-block each sample, fold the
    /// conjugated fs/4 mixer sign in, and fill the prepared parity-split
    /// spans. A trailing odd byte is retained for the next call.
    fn route_containers<const FUSED: bool>(
        &mut self,
        pending_head: Option<u16>,
        body: &[u8],
    ) -> u64 {
        let mut clips = 0u64;
        let mut dc = self.dc_average;
        let mut phase = self.mixer_phase;
        let mid_odd = self.mid_odd;
        let mut poly = self.poly_scratch[self.poly_carry.len()..].iter_mut();
        let mut delay = self.delay_scratch[self.delay_carry.len()..].iter_mut();
        let mut route_single =
            |v: u16,
             phase: &mut usize,
             dc: &mut f32,
             poly: &mut std::slice::IterMut<'_, f32>,
             delay: &mut std::slice::IterMut<'_, f32>| {
                clips += u64::from(is_clip(v));
                let folded = MIXER_SIGN[*phase] * decode_dc::<FUSED>(v, dc);
                if (*phase & 1 == 0) == mid_odd {
                    *poly.next().unwrap() = folded;
                } else {
                    *delay.next().unwrap() = folded;
                }
                *phase = (*phase + 1) & 3;
            };
        if let Some(v) = pending_head {
            route_single(v, &mut phase, &mut dc, &mut poly, &mut delay);
        }
        let mut offset = 0;
        if phase & 1 == 1 && body.len() >= 2 {
            route_single(
                u16::from_le_bytes([body[0], body[1]]) & 0x0fff,
                &mut phase,
                &mut dc,
                &mut poly,
                &mut delay,
            );
            offset = 2;
        }
        let paired = &body[offset..];
        let pair_count = paired.len() / 4;
        let (pairs, remainder) = paired.split_at(pair_count * 4);
        let mut s = MIXER_SIGN[phase];
        // Selecting the destinations once makes both filter modes share the
        // same pair loop without per-sample allocation checks. DC remains
        // strictly v0 then v1.
        let (even, odd) = if mid_odd {
            (poly.into_slice(), delay.into_slice())
        } else {
            (delay.into_slice(), poly.into_slice())
        };
        let (even_pairs, even_tail) = even.split_at_mut(pair_count);
        let odd_pairs = &mut odd[..pair_count];
        for pair in 0..pair_count {
            let at = pair * 4;
            // Explicit little-endian assembly is safe for unaligned/odd-start
            // input slices. The upper four bits of each container are ignored.
            let bytes: &[u8; 4] = pairs[at..at + 4].try_into().unwrap();
            let packed = u32::from_le_bytes(*bytes);
            let v0 = (packed & 0x0fff) as u16;
            let v1 = ((packed >> 16) & 0x0fff) as u16;
            clips += u64::from(is_clip(v0)) + u64::from(is_clip(v1));
            // The DC recurrence stays strictly v0 then v1, with its existing
            // target-selected arithmetic and unchanged sign multiplications.
            let x0 = decode_dc::<FUSED>(v0, &mut dc);
            let x1 = decode_dc::<FUSED>(v1, &mut dc);
            even_pairs[pair] = s * x0;
            odd_pairs[pair] = s * x1;
            s = -s;
        }
        phase = (phase + 2 * (pair_count & 1)) & 3;
        if remainder.len() >= 2 {
            let v = u16::from_le_bytes([remainder[0], remainder[1]]) & 0x0fff;
            clips += u64::from(is_clip(v));
            let x = decode_dc::<FUSED>(v, &mut dc);
            even_tail[0] = MIXER_SIGN[phase] * x;
            phase = (phase + 1) & 3;
        }
        self.dc_average = dc;
        self.mixer_phase = phase;
        if remainder.len() & 1 == 1 {
            self.pending_low_byte = remainder.last().copied();
        }
        clips
    }

    /// Run the sparse half-band over the parity streams: the packed nonzero
    /// taps as one flat forward convolution into `stage_poly`, the center tap
    /// as a scaled delayed read into `stage_center`. One output per
    /// even-parity input sample in this block.
    ///
    /// With `mid` odd the emitting sample is also the polyphase window's
    /// newest sample, so output `t` reads `poly_scratch[t .. t+ncoef]`
    /// (carry = ncoef-1) and the delayed rail at `delay_scratch[t + phi]`
    /// (carry = delay depth). With `mid` even the window's newest sample
    /// arrived one input earlier: the window shifts by `phi` instead
    /// (carry = ncoef) and the delayed rail reads `delay_scratch[t]`.
    fn run_polyphase(&mut self, phi: usize) {
        let ncoef = self.poly_taps_rev.len();
        let taps = &self.poly_taps_rev[..];
        let center = self.center_tap;
        let (n_out, poly_off, delay_off) = if self.mid_odd {
            (self.poly_scratch.len() - self.poly_carry.len(), 0, phi)
        } else {
            (self.delay_scratch.len() - self.delay_carry.len(), phi, 0)
        };
        // Every output below is overwritten. Retain initialized elements on
        // steady-size calls so resize does not zero the entire stage again.
        self.stage_poly.resize(n_out, 0.0);
        {
            let poly_scratch = &self.poly_scratch[..];
            let stage = &mut self.stage_poly[..];
            let mut t = 0usize;
            if let Ok(fixed_taps) = <&[f32; 24]>::try_from(taps) {
                while t + 16 <= n_out {
                    let w = (&poly_scratch[t + poly_off..t + poly_off + 39])
                        .try_into()
                        .unwrap();
                    stage[t..t + 16].copy_from_slice(&sliding_dot_24_x16(fixed_taps, w));
                    t += 16;
                }
            } else {
                while t + 16 <= n_out {
                    let w = &poly_scratch[t + poly_off..t + poly_off + ncoef + 15];
                    stage[t..t + 16].copy_from_slice(&sliding_dot_x16(taps, w));
                    t += 16;
                }
            }
            while t < n_out {
                let w = &poly_scratch[t + poly_off..t + poly_off + ncoef];
                stage[t] = sliding_dot_tail(taps, w);
                t += 1;
            }
        }
        self.stage_center.clear();
        self.stage_center.extend(
            self.delay_scratch[delay_off..delay_off + n_out]
                .iter()
                .map(|&d| center * d),
        );
    }

    /// Run the full cf32 path with an explicitly chosen DC-blocker form, so
    /// tests can exercise **both** the fused and unfused recurrences on any
    /// host rather than only whichever one [`FUSED_DC`] selected for this
    /// build. Not part of the public API.
    #[cfg(test)]
    fn synthesize_cf32_with_fused<const FUSED: bool>(&mut self, raw: &[u8]) -> Vec<f32> {
        self.synthesize_mini_with::<FUSED, _>(raw, 2, |i_vals, q_vals, out| {
            out.extend(
                i_vals
                    .iter()
                    .zip(q_vals)
                    .flat_map(|(&i, &q)| [i.clamp(-1.0, 1.0), q.clamp(-1.0, 1.0)]),
            );
        })
    }

    /// Reset all streaming state (mixer phase + FIR history). Used by tests.
    pub fn reset(&mut self) {
        self.poly_carry.fill(0.0);
        self.delay_carry.fill(0.0);
        self.mixer_phase = 0;
        self.dc_average = 0.0;
        self.pending_low_byte = None;
        self.last_raw_samples = 0;
        self.last_clips = 0;
    }
}

/// Map a normalized [-1, 1] sample to cu8 [0, 255] centered at 127.5.
fn to_cu8(x: f32) -> u8 {
    ((x * 127.5) + 127.5).round().clamp(0.0, 255.0) as u8
}

fn to_cs8(x: f32) -> i8 {
    (x * 127.0).round().clamp(-128.0, 127.0) as i8
}

fn to_cs16(x: f32) -> i16 {
    (x * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2_synthesis_halves_byte_count() {
        let raw = vec![0xff, 0x0f, 0x00, 0x08, 0xaa, 0x05, 0x55, 0x02];
        let mut synth = IqSynthesizer::new();
        let cu8 = synth.synthesize_r2(&raw);
        assert_eq!(cu8.len(), 4); // four real containers become two IQ pairs
    }

    #[test]
    fn r2_and_mini_use_the_same_real_wire_converter() {
        let raw: Vec<u8> = (0..127u16)
            .flat_map(|n| ((n * 31) & 0x0fff).to_le_bytes())
            .collect();
        let mut r2 = IqSynthesizer::new();
        let mut mini = IqSynthesizer::new();
        assert_eq!(r2.synthesize_r2(&raw), mini.synthesize_mini(&raw));
    }

    #[test]
    fn r2_retains_odd_trailing_byte_across_blocks() {
        let raw = [0x34, 0x02, 0xab, 0x0c, 0x55, 0x05];
        let mut whole = IqSynthesizer::new();
        let expected = whole.synthesize_r2(&raw);
        for split in 0..=raw.len() {
            let mut chunked = IqSynthesizer::new();
            let mut got = chunked.synthesize_r2(&raw[..split]);
            got.extend(chunked.synthesize_r2(&raw[split..]));
            assert_eq!(got, expected, "split at raw byte {split}");
        }
    }

    #[test]
    fn r2_cf32_preserves_more_than_eight_bits() {
        // Cf32 retains filter precision rather than round-tripping through Cu8.
        let raw: Vec<u8> = (0..128)
            .flat_map(|n| {
                let phase = 2.0 * std::f32::consts::PI * 0.31 * n as f32;
                ((phase.sin() * 1700.0 + 2048.0) as u16).to_le_bytes()
            })
            .collect();
        let mut synth = IqSynthesizer::new();
        let cf32 = synth.synthesize_mini_cf32(&raw);
        assert_eq!(cf32.len(), 128);
        assert!(cf32
            .iter()
            .any(|sample| (sample * 127.5 - (sample * 127.5).round()).abs() > 1e-4));
    }

    #[test]
    fn mini_synthesis_decimates_by_two() {
        // Mini: real samples at 2× → half as many complex samples (cu8 pairs).
        let raw: Vec<u8> = (0..64).collect();
        let mut synth = IqSynthesizer::new();
        let cu8 = synth.synthesize_mini(&raw);
        assert!(cu8.len() % 2 == 0, "output must be pairs of cu8");
        assert!(cu8.len() <= 64, "mini must decimate; got {}", cu8.len());
        assert!(!cu8.is_empty());
    }

    #[test]
    fn mini_synthesis_preserves_dc_center() {
        // All-zero-center samples (12-bit value 2048 = mid) normalize to 0.0
        // and pack to cu8 ≈ 128.
        let raw = vec![[0x00u8, 0x08u8]; 64]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();
        let mut synth = IqSynthesizer::new();
        let cu8 = synth.synthesize_mini(&raw);
        let tail = &cu8[cu8.len().saturating_sub(16)..];
        for &v in tail {
            assert!((v as i32 - 128).abs() <= 2, "v={v} should be ~128");
        }
    }

    #[test]
    fn r2_synthesis_handles_empty_input() {
        let mut synth = IqSynthesizer::new();
        assert!(synth.synthesize_r2(&[]).is_empty());
    }

    #[test]
    fn mini_state_persists_across_blocks() {
        // Feeding one block vs splitting at every raw byte boundary must yield
        // identical output. Odd byte and odd sample boundaries exercise the
        // retained container byte, mixer phase, decimation phase, and FIR state.
        let fs_in = 2_000_000.0_f32;
        let fc = fs_in / 4.0 + 30_000.0;
        let n = 257usize;
        let synth_in: Vec<u8> = (0..n)
            .flat_map(|i| {
                let t = i as f32 / fs_in;
                let v = (2.0 * std::f32::consts::PI * fc * t).sin();
                let q = (v * 1800.0 + 2048.0).round().clamp(0.0, 4095.0) as u16;
                [q as u8, (q >> 8) as u8]
            })
            .collect();
        let mut whole = IqSynthesizer::new();
        let out_whole = whole.synthesize_mini(&synth_in);
        for split_at in 0..=synth_in.len() {
            let mut split = IqSynthesizer::new();
            let mut out_split = split.synthesize_mini(&synth_in[..split_at]);
            out_split.extend(split.synthesize_mini(&synth_in[split_at..]));
            assert_eq!(
                out_whole, out_split,
                "streaming mismatch at raw byte {split_at}"
            );
        }
    }

    #[test]
    fn cf32_stream_is_bitwise_chunk_invariant() {
        // Stronger than the cu8 split test: unquantized f32 outputs must be
        // BITWISE identical under any chunking. This is the test that catches
        // a kernel whose wide fast path and scalar tail sum in different
        // orders — quantization can mask that; exact f32 comparison cannot.
        // Covers both `mid` parities (47 → mid odd, 45 → mid even).
        let raw: Vec<u8> = (0..1021usize)
            .flat_map(|i| {
                let tone = (2.0 * std::f32::consts::PI * 0.083 * i as f32).sin();
                let v = ((i * 977 + i / 7) as f32 * 0.37 + tone * 1500.0 + 2048.0)
                    .rem_euclid(4096.0) as u16
                    & 0x0fff;
                v.to_le_bytes()
            })
            .collect();
        for &taps in &[45usize, 47] {
            let mut whole = IqSynthesizer::with_taps(taps);
            let expected = whole.synthesize_mini_cf32(&raw);
            // Prime strides land every phase/parity/tail alignment.
            for &stride in &[1usize, 3, 7, 13, 61, 257] {
                let mut streamed = IqSynthesizer::with_taps(taps);
                let mut got = Vec::new();
                for block in raw.chunks(stride) {
                    got.extend(streamed.synthesize_mini_cf32(block));
                }
                assert_eq!(expected.len(), got.len(), "taps={taps} stride={stride}");
                for (k, (a, b)) in expected.iter().zip(&got).enumerate() {
                    assert!(
                        a.to_bits() == b.to_bits(),
                        "taps={taps} stride={stride} value {k}: {a} != {b}"
                    );
                }
            }
            // Every two-block split of a smaller prefix, including mid-container.
            let small = &raw[..301];
            let mut whole = IqSynthesizer::with_taps(taps);
            let expected = whole.synthesize_mini_cf32(small);
            for split in 0..=small.len() {
                let mut s = IqSynthesizer::with_taps(taps);
                let mut got = s.synthesize_mini_cf32(&small[..split]);
                got.extend(s.synthesize_mini_cf32(&small[split..]));
                assert_eq!(expected.len(), got.len(), "taps={taps} split={split}");
                assert!(
                    expected
                        .iter()
                        .zip(&got)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "taps={taps} split={split}"
                );
            }
        }
    }

    #[test]
    fn mini_matches_dense_direct_convolution() {
        // Absolute correctness against the mathematically defined pipeline,
        // not self-consistency: decode 12-bit containers, run the DC
        // recurrence, mix by exp(-j·π·n/2), convolve with the FULL half-band
        // kernel (zeros included) in f64, and keep every second sample. A
        // rewrite with a wrong sign fold, parity split, window offset, or
        // delay depth passes chunk-invariance tests but fails here.
        //
        // The reference below writes the DC step in its UNFUSED form. On a
        // target where `FUSED_DC` is true the implementation rounds once
        // instead of twice, so it is no longer bit-for-bit with this
        // reference — but the divergence is bounded at ~1e-7 (see `decode_dc`)
        // while the tolerance here is 2e-6, and what this test exists to pin
        // is the pipeline's STRUCTURE, not the DC step's exact rounding.
        // `fused_and_unfused_full_paths_agree_far_below_one_adc_lsb` pins the
        // divergence itself, on both forms, on every host.
        let raw: Vec<u8> = (0..800usize)
            .flat_map(|i| {
                let tone = (2.0 * std::f32::consts::PI * 0.19 * i as f32).sin();
                let v = ((i * 733) as f32 * 0.61 + tone * 1400.0 + 2048.0).rem_euclid(4096.0)
                    as u16
                    & 0x0fff;
                v.to_le_bytes()
            })
            .collect();
        // 47 → mid odd (polyphase on I), 45 → mid even (polyphase on Q),
        // 5 → smallest legal kernel.
        for &taps in &[5usize, 45, 47] {
            let mut synth = IqSynthesizer::with_taps(taps);
            let hbf: Vec<f64> = synth.hbf.iter().map(|&t| f64::from(t)).collect();
            let got = synth.synthesize_mini_cf32(&raw);

            // Reference mixed rails in f64 (DC recurrence replicated in f32,
            // exactly as specified for the streaming kernel). The mixer table
            // is exp(+j·π·n/2) — the CONJUGATE of the fs/4 down-mixer —
            // because the synthesizer's output convention is the conjugated
            // analytic signal (Q negated after the filter; conjugation
            // commutes with the real-valued FIR, so conjugating the mixer
            // here is the equivalent reference).
            let mut dc = 0.0f32;
            let (mut xi, mut xq) = (Vec::new(), Vec::new());
            for (n, chunk) in raw.chunks_exact(2).enumerate() {
                let v = u16::from_le_bytes([chunk[0], chunk[1]]) & 0x0fff;
                let mut x = (f32::from(v) - 2048.0) / 2048.0;
                x -= dc;
                dc += 0.01 * x;
                let (i, q) = match n & 3 {
                    0 => (x, 0.0),
                    1 => (0.0, x),
                    2 => (-x, 0.0),
                    _ => (0.0, -x),
                };
                xi.push(f64::from(i));
                xq.push(f64::from(q));
            }
            let n_out = got.len() / 2;
            assert_eq!(n_out, xi.len().div_ceil(2), "taps={taps} output count");
            for m in 0..n_out {
                let center = 2 * m;
                let (mut want_i, mut want_q) = (0.0f64, 0.0f64);
                for (j, &h) in hbf.iter().enumerate() {
                    if let Some(idx) = center.checked_sub(j) {
                        want_i += h * xi[idx];
                        want_q += h * xq[idx];
                    }
                }
                let (got_i, got_q) = (got[2 * m], got[2 * m + 1]);
                assert!(
                    (f64::from(got_i) - want_i).abs() <= 2e-6
                        && (f64::from(got_q) - want_q).abs() <= 2e-6,
                    "taps={taps} output {m}: got ({got_i}, {got_q}), want ({want_i}, {want_q})"
                );
            }
        }
    }

    #[test]
    fn mini_image_rejection_at_least_40_db() {
        // Feed a real sinusoid at fs/4 + delta. Under the conjugated output
        // convention (Q negated — the hardware-validated SondeFox convention)
        // the desired tone lands at -delta, with the mirror image at +delta
        // at least 40 dB down (image rejection).
        let fs_in = 2_000_000.0_f32; // Mini real input rate
        let delta = 50_000.0_f32; // 50 kHz above fs/4
        let fc = fs_in / 4.0 + delta;
        let n = 8192usize;
        let synth_in: Vec<u8> = (0..n)
            .flat_map(|i| {
                let t = i as f32 / fs_in;
                let v = (2.0 * std::f32::consts::PI * fc * t).sin();
                let q = (v * 1800.0 + 2048.0).round().clamp(0.0, 4095.0) as u16;
                [q as u8, (q >> 8) as u8]
            })
            .collect();

        // The production default is 47 taps and must satisfy this bound.
        let mut synth = IqSynthesizer::new();
        let cu8 = synth.synthesize_mini(&synth_in);

        // Decode cu8 → complex f32 (I, Q in [-1, 1]).
        let complex: Vec<(f32, f32)> = cu8
            .chunks_exact(2)
            .map(|c| {
                let i = (i16::from(c[0]) - 128) as f32 / 127.5;
                let q = (i16::from(c[1]) - 128) as f32 / 127.5;
                (i, q)
            })
            .collect();
        // Skip the FIR transient.
        let start = complex.len() / 4;
        let m = complex.len() - start;
        let fs_out = fs_in / 2.0;

        // Naive DFT probe at a target frequency (Goertzel-style sum).
        let probe = |freq: f32| -> f32 {
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for (k, &(i, q)) in complex[start..].iter().enumerate() {
                let phase = -2.0 * std::f32::consts::PI * freq * k as f32 / fs_out;
                let (cp, sp) = (phase.cos(), phase.sin());
                re += i * cp - q * sp;
                im += i * sp + q * cp;
            }
            (re * re + im * im).sqrt() / m as f32
        };
        let signal = probe(-delta); // desired output tone (conjugated convention)
        let image = probe(delta); // mirror image from the real→complex step
        let rej_db = 20.0 * (signal / image).max(1e-12).log10();
        assert!(
            rej_db >= 40.0,
            "image rejection {rej_db:.1} dB < 40 dB (signal={signal:.4}, image={image:.4})"
        );
    }

    #[test]
    fn image_rejection_holds_near_usable_band_edge() {
        let fs_in = 2_000_000.0_f32;
        for delta in [50_000.0_f32, 200_000.0, 300_000.0, 400_000.0] {
            let fc = fs_in / 4.0 + delta;
            let n = 16_384usize;
            let raw: Vec<u8> = (0..n)
                .flat_map(|i| {
                    let phase = 2.0 * std::f32::consts::PI * fc * i as f32 / fs_in;
                    let value = (phase.sin() * 1700.0 + 2048.0).round() as u16;
                    value.to_le_bytes()
                })
                .collect();
            let mut synth = IqSynthesizer::new();
            let iq = synth.synthesize_mini_cf32(&raw);
            let complex: Vec<(f32, f32)> =
                iq.chunks_exact(2).map(|pair| (pair[0], pair[1])).collect();
            let start = complex.len() / 4;
            let samples = &complex[start..];
            let fs_out = fs_in / 2.0;
            let probe = |freq: f32| {
                let (re, im) =
                    samples
                        .iter()
                        .enumerate()
                        .fold((0.0f32, 0.0f32), |(re, im), (k, &(i, q))| {
                            let phase = -2.0 * std::f32::consts::PI * freq * k as f32 / fs_out;
                            let (sin, cos) = phase.sin_cos();
                            (re + i * cos - q * sin, im + i * sin + q * cos)
                        });
                (re * re + im * im).sqrt() / samples.len() as f32
            };
            // Conjugated output convention: input fs/4+delta emerges at
            // -delta; the residual image sits at +delta.
            let desired = probe(-delta);
            let image = probe(delta);
            let rejection_db = 20.0 * (desired / image.max(1e-12)).log10();
            assert!(
                rejection_db >= 40.0,
                "delta={delta}: image rejection {rejection_db:.1} dB"
            );
        }
    }

    /// Adversarial container streams for the DC-blocker divergence bound:
    /// each is chosen to stop the rounding difference decaying out, which is
    /// what a naive "one ULP on a short buffer" check would miss.
    fn adversarial_streams(n: usize) -> Vec<(&'static str, Vec<u16>)> {
        vec![
            (
                "rail-to-rail square",
                (0..n).map(|i| if i % 2 == 0 { 4095 } else { 0 }).collect(),
            ),
            // `dc` never settles, so `x` never shrinks toward zero.
            (
                "slow ramp",
                (0..n).map(|i| ((i / 64) % 4096) as u16).collect(),
            ),
            // Every step's rounding error carries the same sign: the worst
            // case for COHERENT accumulation through the feedback path.
            ("held at rail", vec![4095; n]),
            (
                "asymmetric duty",
                (0..n).map(|i| if i % 8 == 0 { 12 } else { 4083 }).collect(),
            ),
        ]
    }

    #[test]
    fn fused_and_unfused_dc_recurrences_stay_within_the_documented_bound() {
        // The DC blocker is a FEEDBACK loop, so a per-sample rounding
        // difference persists rather than vanishing. It does not, however,
        // integrate without bound: the loop is contractive (pole 0.99), so the
        // difference `e` obeys `e ← 0.99·e + δ` and is bounded by
        // `100·½ulp(0.01·x) ≈ 9.3e-8` even under perfectly correlated δ.
        // Bounds are set with ~4x headroom over the worst measured value
        // across these streams at 10^7 samples (DC 1.19e-7, sample 2.38e-7).
        // Both stay far under one 12-bit ADC LSB (1/2048 ≈ 4.9e-4).
        const DC_BOUND: f32 = 5e-7;
        // The emitted sample `x = s − dc` carries the DC divergence PLUS the
        // rounding of its own subtraction, a per-sample quantization effect
        // rather than loop accumulation, so it is bounded separately and
        // higher. Folding the two into one loose bound would hide exactly the
        // failure this test exists to catch.
        const SAMPLE_BOUND: f32 = 1e-6;
        let n = 1_000_000usize;
        for (name, samples) in adversarial_streams(n) {
            let (mut dc_u, mut dc_f) = (0.0f32, 0.0f32);
            let (mut worst_dc, mut worst_sample) = (0.0f32, 0.0f32);
            // Split the run in half: if the difference were accumulating
            // rather than reaching an equilibrium, the second half would be
            // strictly worse than the first. This is the part of the test with
            // real teeth — an absolute bound alone can be satisfied by a slow
            // drift that simply has not had time to blow up yet.
            let (mut first_half, mut second_half) = (0.0f32, 0.0f32);
            for (i, &v) in samples.iter().enumerate() {
                let xu = decode_dc::<false>(v, &mut dc_u);
                let xf = decode_dc::<true>(v, &mut dc_f);
                let d = (dc_u - dc_f).abs();
                worst_dc = worst_dc.max(d);
                worst_sample = worst_sample.max((xu - xf).abs());
                if i < n / 2 {
                    first_half = first_half.max(d);
                } else {
                    second_half = second_half.max(d);
                }
            }
            assert!(
                worst_dc <= DC_BOUND,
                "{name}: DC-state divergence {worst_dc:e} exceeds {DC_BOUND:e}"
            );
            assert!(
                worst_sample <= SAMPLE_BOUND,
                "{name}: sample divergence {worst_sample:e} exceeds {SAMPLE_BOUND:e}"
            );
            assert!(
                second_half <= first_half.max(f32::MIN_POSITIVE),
                "{name}: divergence is ACCUMULATING, not settling — worst in \
                 the second half ({second_half:e}) exceeds the first half \
                 ({first_half:e}); the DC loop has stopped attenuating its \
                 own rounding difference"
            );
        }
    }

    #[test]
    fn fused_and_unfused_full_paths_agree_far_below_one_adc_lsb() {
        // End-to-end version of the bound: the DC difference is filtered by
        // the half-band, so it can grow by ‖h‖₁, but must stay orders of
        // magnitude under one 12-bit ADC LSB (1/2048 ≈ 4.9e-4) — otherwise
        // the gate in `FUSED_DC` would be changing observable signal.
        const BOUND: f32 = 1e-6;
        let raw: Vec<u8> = (0..200_000usize)
            .flat_map(|i| {
                let tone = (2.0 * std::f32::consts::PI * 0.2313 * i as f32).sin() * 1500.0;
                let v = (2048.0 + 800.0 + tone).round().clamp(0.0, 4095.0) as u16;
                (v & 0x0fff).to_le_bytes()
            })
            .collect();
        let mut unfused = IqSynthesizer::new();
        let mut fused = IqSynthesizer::new();
        let a = unfused.synthesize_cf32_with_fused::<false>(&raw);
        let b = fused.synthesize_cf32_with_fused::<true>(&raw);
        assert_eq!(a.len(), b.len());
        let worst = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst <= BOUND,
            "fused/unfused full-path divergence {worst:e} exceeds {BOUND:e} \
             ({:.4} of one 12-bit ADC LSB)",
            worst * 2048.0
        );
    }

    #[test]
    fn both_dc_forms_are_bitwise_chunk_invariant() {
        // `cf32_stream_is_bitwise_chunk_invariant` pins this contract for
        // whichever form THIS build selected. The fused form must not break it
        // on the targets that do select it, so check both forms here rather
        // than assuming — a host that builds unfused would otherwise never
        // exercise the fused path at all.
        fn check<const FUSED: bool>(raw: &[u8]) {
            for &taps in &[45usize, 47] {
                let mut whole = IqSynthesizer::with_taps(taps);
                let expected = whole.synthesize_cf32_with_fused::<FUSED>(raw);
                for &stride in &[1usize, 3, 7, 13, 61, 257] {
                    let mut streamed = IqSynthesizer::with_taps(taps);
                    let mut got = Vec::new();
                    for block in raw.chunks(stride) {
                        got.extend(streamed.synthesize_cf32_with_fused::<FUSED>(block));
                    }
                    assert_eq!(expected.len(), got.len(), "fused={FUSED} taps={taps}");
                    for (k, (a, b)) in expected.iter().zip(&got).enumerate() {
                        assert!(
                            a.to_bits() == b.to_bits(),
                            "fused={FUSED} taps={taps} stride={stride} value {k}: {a} != {b}"
                        );
                    }
                }
            }
        }
        let raw: Vec<u8> = (0..2003usize)
            .flat_map(|i| {
                let tone = (2.0 * std::f32::consts::PI * 0.083 * i as f32).sin();
                let v = ((i * 977 + i / 7) as f32 * 0.37 + tone * 1500.0 + 2048.0)
                    .rem_euclid(4096.0) as u16
                    & 0x0fff;
                v.to_le_bytes()
            })
            .collect();
        check::<false>(&raw);
        check::<true>(&raw);
    }

    #[test]
    fn fused_dc_step_really_fuses() {
        // Guards the premise of FUSED_DC: if `mul_add` ever silently degraded
        // to a rounded multiply followed by an add, the gate would be paying a
        // portability cost for nothing. Pick a case where the single rounding
        // is observably different from the double rounding.
        let mut differed = false;
        let mut dc = 0.0f32;
        for i in 0..100_000u32 {
            let v = ((i.wrapping_mul(2_654_435_761)) % 4096) as u16;
            let mut x = (f32::from(v) - 2048.0) / 2048.0;
            x -= dc;
            let fused = dc_step_fused(x, dc);
            let unfused = dc_step_unfused(x, dc);
            if fused.to_bits() != unfused.to_bits() {
                differed = true;
            }
            dc = unfused;
        }
        assert!(
            differed,
            "mul_add produced bit-identical results to a separate mul+add on \
             every sample; it is not fusing, so FUSED_DC buys nothing"
        );
    }

    #[test]
    fn fixed_default_tile_preserves_four_chain_rounding() {
        let synth = IqSynthesizer::new();
        let taps: &[f32; 24] = synth.poly_taps_rev.as_slice().try_into().unwrap();
        let mut window = [0.0f32; 39];
        // Include signed zeros and subnormals as well as ordinary values;
        // equality is raw bits, including the scalar ragged-tail path.
        let values = [
            0.0,
            -0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            0.125,
            -0.75,
            1.0,
            -1.0,
        ];
        for shift in 0..values.len() {
            for (i, sample) in window.iter_mut().enumerate() {
                *sample = values[(i + shift) % values.len()];
            }
            let got = sliding_dot_24_x16(taps, &window);
            let dynamic = sliding_dot_x16(taps, &window);
            for (lane, value) in got.iter().enumerate() {
                assert_eq!(value.to_bits(), dynamic[lane].to_bits());
                assert_eq!(
                    value.to_bits(),
                    sliding_dot_tail(taps, &window[lane..]).to_bits()
                );
            }
        }
    }

    #[test]
    fn default_halfband_is_exactly_sparse() {
        let synth = IqSynthesizer::new();
        let mid = synth.hbf.len() / 2;
        for (index, &coefficient) in synth.hbf.iter().enumerate() {
            let offset = index as isize - mid as isize;
            if offset != 0 && offset % 2 == 0 {
                assert_eq!(
                    coefficient.to_bits(),
                    0.0f32.to_bits(),
                    "tap {index} must be structurally zero"
                );
            }
        }
    }

    #[test]
    fn conjugate_pins_low_side_input_tone_to_positive_baseband() {
        // SIGN pin for the output conjugate (G4), not an image-rejection or
        // magnitude check: a real input tone Δ BELOW fs/4 must emerge at +Δ
        // (the hardware-validated SondeFox convention). On the un-conjugated
        // (spectrally mirrored) kernel the same energy lands at -Δ and the
        // ratio below inverts, so this test fails if the conjugate is ever
        // dropped again.
        let fs_in = 2_000_000.0_f32; // Mini real input rate
        let fs_out = fs_in / 2.0;
        let delta = fs_out / 8.0; // +fs_out/8 = 125 kHz
        let fc = fs_in / 4.0 - delta;
        let n = 8192usize;
        let raw: Vec<u8> = (0..n)
            .flat_map(|i| {
                let phase = 2.0 * std::f32::consts::PI * fc * i as f32 / fs_in;
                let value = (phase.sin() * 1700.0 + 2048.0).round().clamp(0.0, 4095.0) as u16;
                value.to_le_bytes()
            })
            .collect();
        let mut synth = IqSynthesizer::new();
        let iq = synth.synthesize_mini_cf32(&raw);
        let complex: Vec<(f32, f32)> = iq.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        let start = complex.len() / 4; // skip the FIR transient
        let samples = &complex[start..];
        let probe = |freq: f32| {
            let (re, im) =
                samples
                    .iter()
                    .enumerate()
                    .fold((0.0f32, 0.0f32), |(re, im), (k, &(i, q))| {
                        let phase = -2.0 * std::f32::consts::PI * freq * k as f32 / fs_out;
                        let (sin, cos) = phase.sin_cos();
                        (re + i * cos - q * sin, im + i * sin + q * cos)
                    });
            (re * re + im * im).sqrt() / samples.len() as f32
        };
        let positive = probe(delta);
        let negative = probe(-delta);
        let ratio_db = 20.0 * (positive / negative.max(1e-12)).log10();
        assert!(
            ratio_db >= 40.0,
            "tone must land at +{delta} Hz, not -{delta} Hz: \
             positive={positive:.6}, negative={negative:.6}, ratio {ratio_db:.1} dB"
        );
    }

    #[test]
    fn clip_and_raw_counters_track_the_adc_rails() {
        // Threshold is |container - 2048| >= 2044: 0, 4095, 4 and 4092 clip;
        // 5 and 4091 sit one count inside and do not.
        let vals: [u16; 8] = [0, 4095, 4, 4092, 5, 4091, 2048, 1000];
        let raw: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut synth = IqSynthesizer::new();
        let _ = synth.synthesize_mini_cf32(&raw);
        assert_eq!(synth.last_raw_samples(), 8);
        assert_eq!(synth.last_clips(), 4);

        // Counters are per-call: a railing container split across calls
        // counts in the call that completes it, and a call that decodes
        // nothing reports zero.
        let rail = 4095u16.to_le_bytes();
        let _ = synth.synthesize_mini_cf32(&rail[..1]);
        assert_eq!(synth.last_raw_samples(), 0);
        assert_eq!(synth.last_clips(), 0);
        let _ = synth.synthesize_mini_cf32(&rail[1..]);
        assert_eq!(synth.last_raw_samples(), 1);
        assert_eq!(synth.last_clips(), 1);

        // reset() clears the telemetry along with the stream state.
        synth.reset();
        assert_eq!(synth.last_raw_samples(), 0);
        assert_eq!(synth.last_clips(), 0);
    }

    #[test]
    fn packed_counter_covers_all_u16_encodings_and_both_positions() {
        let mut raw = vec![0xa5];
        raw.extend(
            (0..=u16::MAX)
                .flat_map(|v| [v, 2048, 2048, v])
                .flat_map(u16::to_le_bytes),
        );
        let mut synth = IqSynthesizer::new();
        let _ = synth.synthesize_mini_cf32(&raw[1..]);
        assert_eq!(synth.last_raw_samples(), 4 * 65_536);
        // Nine masked rail values per4096, each in16 high-nibble encodings
        // and in both packed positions. Midpoint2048 never clips.
        assert_eq!(synth.last_clips(), 9 * 16 * 2);
    }

    #[test]
    fn pending_empty_calls_reset_stats_without_advancing_the_sample() {
        let mut synth = IqSynthesizer::new();
        let _ = synth.synthesize_mini_cf32(&[0xff]);
        let before_dc = synth.dc_average.to_bits();
        let before_phase = synth.mixer_phase;
        for _ in 0..3 {
            assert!(synth.synthesize_mini_cf32(&[]).is_empty());
            assert_eq!(synth.pending_low_byte, Some(0xff));
            assert_eq!(synth.dc_average.to_bits(), before_dc);
            assert_eq!(synth.mixer_phase, before_phase);
            assert_eq!(synth.last_raw_samples(), 0);
            assert_eq!(synth.last_clips(), 0);
        }
        let _ = synth.synthesize_mini_cf32(&[0x0f]);
        assert_eq!(synth.last_raw_samples(), 1);
        assert_eq!(synth.last_clips(), 1);
        assert_eq!(synth.pending_low_byte, None);
        synth.reset();
        assert_eq!(synth.last_raw_samples(), 0);
        assert_eq!(synth.last_clips(), 0);
        assert_eq!(synth.pending_low_byte, None);
    }

    #[test]
    fn railing_stream_reports_the_expected_clip_fraction() {
        // Synthetic overload: every second container rails (alternating low
        // and high rail), the rest sit mid-scale. Clip fraction must be
        // exactly one half, independent of chunking.
        let containers = 512u16;
        let raw: Vec<u8> = (0..containers)
            .flat_map(|n| {
                let v: u16 = if n % 2 == 0 {
                    if n % 4 == 0 {
                        0
                    } else {
                        4095
                    }
                } else {
                    2048
                };
                v.to_le_bytes()
            })
            .collect();
        let mut synth = IqSynthesizer::new();
        let _ = synth.synthesize_mini(&raw);
        assert_eq!(synth.last_raw_samples(), u64::from(containers));
        assert_eq!(synth.last_clips(), u64::from(containers) / 2);

        // Chunked feeding attributes every container to exactly one call.
        let mut chunked = IqSynthesizer::new();
        let (mut raw_total, mut clip_total) = (0u64, 0u64);
        for block in raw.chunks(61) {
            let _ = chunked.synthesize_mini(block);
            raw_total += chunked.last_raw_samples();
            clip_total += chunked.last_clips();
        }
        assert_eq!(raw_total, u64::from(containers));
        assert_eq!(clip_total, u64::from(containers) / 2);
    }

    #[test]
    fn reset_restores_a_fresh_stream() {
        let raw: Vec<u8> = (0..301u16)
            .flat_map(|n| ((n * 173) & 0x0fff).to_le_bytes())
            .collect();
        let mut fresh = IqSynthesizer::new();
        let expected = fresh.synthesize_mini(&raw);
        let mut reused = IqSynthesizer::new();
        // Pollute every piece of streaming state, including the pending byte.
        let _ = reused.synthesize_mini(&raw[..151]);
        reused.reset();
        assert_eq!(reused.synthesize_mini(&raw), expected);
    }
}
