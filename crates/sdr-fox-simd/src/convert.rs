//! IQ sample conversion with platform SIMD acceleration.
//!
//! The primary operation is cu8 (RTL-SDR's native interleaved unsigned 8-bit
//! IQ, centered at ~127) → cf32 (interleaved float in roughly [-1, 1]). The
//! conversion per sample is:
//!
//! ```text
//! I_f32 = (I_u8 as f32 - 127.5) / 127.5
//! Q_f32 = (Q_u8 as f32 - 127.5) / 127.5
//! ```
//!
//! This is memory-bound work (one byte in → four bytes out), so the ceiling is
//! store bandwidth rather than arithmetic. That shapes the dispatch below: SIMD
//! helps where the baseline instruction set is narrow, and does not where the
//! compiler already emits a bandwidth-saturating loop.
//!
//! ## Dispatch
//!
//! - **x86-64**: runtime feature detection picks AVX2 (32 bytes/iter), then
//!   SSE2 (16 bytes/iter), then a scalar fallback. The "SSE2" path uses only
//!   SSE2 intrinsics (`_mm_unpacklo/hi_epi8/16`, `_mm_cvtepi32_ps`, ...); it is
//!   gated on SSE2, not SSE4.2, so every x86-64 CPU runs it. The dispatch
//!   decision is resolved once and cached in a `OnceLock<fn>` to avoid
//!   re-running `is_x86_feature_detected!` per call. This tier is load-bearing:
//!   with no `target-cpu` set, x86-64 builds are baseline SSE2.
//! - **aarch64 (incl. Apple Silicon & Android arm64)**: the *scalar* path, which
//!   LLVM autovectorizes. Measured 16% faster than the hand-written NEON kernel
//!   at the transport's block size; see `select_backend` for the numbers and for
//!   why that kernel is nevertheless retained.
//! - **Other targets**: scalar fallback (still autovectorizable).
//!
//! Every SIMD path is unit-tested against the scalar reference for the exact
//! same byte stream, so a bug in one path cannot silently diverge.

use sdr_fox_core::IqFormat;

/// Conversion scale: divides the centered value to land in roughly [-1, 1].
pub const CU8_CENTER: f32 = 127.5;
/// Reciprocal of [`CU8_CENTER`] (multiply is faster than divide on every ISA).
pub const CU8_SCALE: f32 = 1.0 / CU8_CENTER;

/// Convert an interleaved byte buffer of IQ samples into interleaved cf32.
///
/// Each scalar component becomes one float. Cu8/Cs8 therefore require
/// `output.len() == input.len()`; little-endian Cs16 requires
/// `output.len() == input.len()/2`. `to` must be [`IqFormat::Cf32`].
///
/// # Panics
///
/// Panics if `from`/`to` are unsupported, or if `output` does not have the
/// format-specific element count documented above.
#[allow(clippy::missing_panics_doc)]
pub fn convert_iq(input: &[u8], from: IqFormat, to: IqFormat, output: &mut [f32]) {
    assert!(
        matches!(to, IqFormat::Cf32),
        "convert_iq currently only targets Cf32"
    );
    match from {
        IqFormat::Cu8 => {
            // cu8: one byte in → one f32 out (an IQ pair is 2 bytes → 2 floats).
            assert_eq!(
                output.len(),
                input.len(),
                "cu8→cf32: output must hold input.len() floats ({} bytes -> {} floats, got {})",
                input.len(),
                input.len(),
                output.len()
            );
            cu8_to_cf32(input, output);
        }
        IqFormat::Cs8 => {
            assert_eq!(output.len(), input.len());
            cs8_to_cf32(input, output);
        }
        IqFormat::Cs16 => cs16le_to_cf32(input, output),
        IqFormat::Cf32 => panic!("convert_iq: Cf32 input is already in the target format"),
    }
}

/// In-place cu8 → cf32 conversion (zero-alloc hot path).
///
/// Writes exactly `input.len()` f32 values to `output` (one byte → one float).
/// Dispatches to the fastest available SIMD path for the current CPU.
///
/// # Panics
///
/// Panics if `output.len() != input.len()`.
pub fn cu8_to_cf32(input: &[u8], output: &mut [f32]) {
    assert_eq!(
        output.len(),
        input.len(),
        "cu8_to_cf32: output must hold input.len() floats ({} bytes -> {} floats, got {})",
        input.len(),
        input.len(),
        output.len()
    );

    let n = input.len();
    if n == 0 {
        return;
    }

    // SAFETY: `select_backend` only returns a backend whose required CPU
    // feature has been verified available (or is a baseline guarantee, e.g.
    // NEON on aarch64 / SSE2 on any x86-64), so calling it is sound here.
    unsafe { select_backend()(input, output) };
}

/// Signature shared by every backend. The SIMD backends are `#[target_feature]`
/// `unsafe fn`s; the scalar trampoline is also typed as `unsafe fn` for a
/// uniform dispatch table.
type Backend = unsafe fn(&[u8], &mut [f32]);

/// Resolve the fastest available SIMD backend once, then cache the decision in
/// a `OnceLock` so `is_x86_feature_detected!` is not re-evaluated on every call.
/// Each backend handles an aligned "wide" head/tail via the scalar fallback; the
/// scalar fallback alone is also correct for the whole buffer.
#[cfg(target_arch = "x86_64")]
fn select_backend() -> Backend {
    use std::sync::OnceLock;
    static BACKEND: OnceLock<Backend> = OnceLock::new();
    // SAFETY: the feature gate checked below matches each function's
    // `#[target_feature(enable = ...)]`, so the selected backend is safe to
    // call on this CPU.
    unsafe fn pick() -> Backend {
        if is_x86_feature_detected!("avx2") {
            cu8_to_cf32_avx2
        } else if is_x86_feature_detected!("sse2") {
            cu8_to_cf32_sse2
        } else {
            scalar_as_backend
        }
    }
    // SAFETY: see `pick`.
    *BACKEND.get_or_init(|| unsafe { pick() })
}

#[cfg(target_arch = "aarch64")]
fn select_backend() -> Backend {
    // Deliberately the scalar path, not the hand-written NEON kernel below.
    //
    // This conversion expands one byte into four, so it is bound by store
    // bandwidth rather than arithmetic, and there is no headroom for
    // instruction selection to recover. LLVM autovectorizes the scalar loop
    // into the same shape and schedules it better. Measured by this crate's own
    // `cu8_to_cf32` bench at the 262144-byte block the transport delivers
    // (M1 Max): scalar 21.89 us / 11.15 GiB/s, NEON 26.07 us / 9.37 GiB/s —
    // the intrinsics cost 16%.
    //
    // `cu8_to_cf32_neon` is retained, compiled, and equivalence-tested rather
    // than deleted: it has not been measured on an in-order ARM core such as
    // the Cortex-A55 found in Android little clusters, where the autovectorizer
    // may do worse. Re-selecting it is a one-line change, but it must be
    // justified against the bench above on the hardware in question.
    scalar_as_backend
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn select_backend() -> Backend {
    scalar_as_backend
}

/// Trampoline wrapping the safe scalar fallback so it shares the `Backend`
/// (`unsafe fn`) signature used by the `#[target_feature]` SIMD paths.
///
/// Selected on `x86_64` as the CPU-feature fallback, on `aarch64` as the
/// measured-fastest path, and on targets with no SIMD backend at all.
unsafe fn scalar_as_backend(input: &[u8], output: &mut [f32]) {
    cu8_to_cf32_scalar(input, output);
}

/// Scalar reference implementation. Autovectorizes cleanly under `-O3`, and on
/// aarch64 it is also the dispatched backend — see `select_backend`.
///
/// # Panics
///
/// Panics if `input` and `output` have different lengths.
pub fn cu8_to_cf32_scalar(input: &[u8], output: &mut [f32]) {
    assert_eq!(input.len(), output.len());
    for (i, &b) in input.iter().enumerate() {
        output[i] = (f32::from(b) - CU8_CENTER) * CU8_SCALE;
    }
}

/// Convert cs8 (signed 8-bit, centered at zero) to cf32.
///
/// Signed integer formats use the conventional power-of-two full-scale
/// denominator: `-128` maps to exactly `-1.0`, while `127` maps to
/// `127/128`. This intentionally differs from cu8's half-step midpoint
/// convention (`127.5`), whose two endpoints both map to unit magnitude.
///
/// # Panics
///
/// Panics if `input` and `output` have different lengths.
pub fn cs8_to_cf32(input: &[u8], output: &mut [f32]) {
    assert_eq!(input.len(), output.len());
    for (i, &b) in input.iter().enumerate() {
        output[i] = f32::from(b as i8) * (1.0 / 128.0);
    }
}

/// Convert unsigned 8-bit components centered at 128 to signed 8-bit.
///
/// # Panics
///
/// Panics if `input` and `output` have different lengths.
pub fn cu8_to_cs8(input: &[u8], output: &mut [i8]) {
    assert_eq!(input.len(), output.len());
    for (output, &input) in output.iter_mut().zip(input) {
        *output = i8::from_ne_bytes([input ^ 0x80]);
    }
}

/// Convert unsigned 8-bit components to signed 16-bit with the original
/// 8-bit value occupying the high byte.
///
/// # Panics
///
/// Panics if `input` and `output` have different lengths.
pub fn cu8_to_cs16(input: &[u8], output: &mut [i16]) {
    assert_eq!(input.len(), output.len());
    for (output, &input) in output.iter_mut().zip(input) {
        *output = (i16::from(input) - 128) << 8;
    }
}

/// Convert signed 16-bit components to cf32 in `[-1.0, 1.0)`.
///
/// As with cs8, this uses the conventional power-of-two denominator so the
/// negative endpoint is exactly `-1.0` and the positive endpoint is one LSB
/// below `1.0`.
///
/// # Panics
///
/// Panics if `input` and `output` have different lengths.
pub fn cs16_to_cf32(input: &[i16], output: &mut [f32]) {
    assert_eq!(input.len(), output.len());
    for (output, &input) in output.iter_mut().zip(input) {
        *output = f32::from(input) * (1.0 / 32_768.0);
    }
}

/// Convert little-endian signed 16-bit component bytes to cf32.
///
/// # Panics
///
/// Panics if `input` has an odd byte count or if `output.len()` is not half
/// `input.len()`.
pub fn cs16le_to_cf32(input: &[u8], output: &mut [f32]) {
    assert_eq!(input.len() % 2, 0, "Cs16 input must contain whole words");
    assert_eq!(input.len() / 2, output.len());
    for (output, bytes) in output.iter_mut().zip(input.chunks_exact(2)) {
        *output = f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) * (1.0 / 32_768.0);
    }
}

// ---------------------------------------------------------------------------
// x86-64 SIMD paths
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn cu8_to_cf32_avx2(input: &[u8], output: &mut [f32]) {
    #[allow(clippy::wildcard_imports)] // idiomatic for std::arch SIMD intrinsics
    use std::arch::x86_64::*;
    // Process 32 input bytes -> 32 output floats per iteration.
    const LANES: usize = 32;
    let n = input.len();
    let chunks = n / LANES;

    let center = _mm256_set1_ps(CU8_CENTER);
    let scale = _mm256_set1_ps(CU8_SCALE);

    for c in 0..chunks {
        let base = c * LANES;
        // SAFETY: c < chunks means base..base+32 is in-bounds.
        let raw = input.as_ptr().add(base).cast::<__m128i>();

        // Widen 32 u8 -> 32 i32 in four 8-byte chunks using `_mm256_cvtepu8_epi32`,
        // which zero-extends the low 8 bytes of an __m128i into 8 i32 lanes in
        // source order. This avoids the lane-scrambling that `_mm256_unpacklo/
        // unpackhi_epi8/16` would introduce (those interleave 128-bit lanes and
        // produce output in the order 0..3, 16..19, 4..7, 20..23, ...).
        // SAFETY: base + q*8..+8 in-bounds (q < 4 since base..base+32 is in-bounds).
        let parts = [
            _mm256_cvtepu8_epi32(_mm_loadl_epi64(raw)),
            _mm256_cvtepu8_epi32(_mm_loadl_epi64(raw.byte_add(8))),
            _mm256_cvtepu8_epi32(_mm_loadl_epi64(raw.byte_add(16))),
            _mm256_cvtepu8_epi32(_mm_loadl_epi64(raw.byte_add(24))),
        ];
        for (q, p) in parts.iter().enumerate() {
            let f = _mm256_cvtepi32_ps(*p);
            let centered = _mm256_sub_ps(f, center);
            let scaled = _mm256_mul_ps(centered, scale);
            // SAFETY: base + q*8..+8 in-bounds (q < 4).
            _mm256_storeu_ps(output.as_mut_ptr().add(base + q * 8), scaled);
        }
    }

    // Scalar tail.
    let tail_start = chunks * LANES;
    for i in tail_start..n {
        *output.get_unchecked_mut(i) =
            (f32::from(*input.get_unchecked(i)) - CU8_CENTER) * CU8_SCALE;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn cu8_to_cf32_sse2(input: &[u8], output: &mut [f32]) {
    #[allow(clippy::wildcard_imports)] // idiomatic for std::arch SIMD intrinsics
    use std::arch::x86_64::*;
    // Process 16 input bytes -> 16 output floats per iteration.
    // Every intrinsic used here (`_mm_unpacklo/hi_epi8/16`, `_mm_cvtepi32_ps`,
    // `_mm_sub_ps`, `_mm_mul_ps`, `_mm_setzero_si128`, `_mm_loadu_si128`,
    // `_mm_storeu_ps`) is SSE2, so this path only requires SSE2 — not SSE4.2.
    const LANES: usize = 16;
    let n = input.len();
    let chunks = n / LANES;

    let zero = _mm_setzero_si128();
    let center = _mm_set1_ps(CU8_CENTER);
    let scale = _mm_set1_ps(CU8_SCALE);

    for c in 0..chunks {
        let base = c * LANES;
        // SAFETY: base..base+16 in-bounds.
        let raw = _mm_loadu_si128(input.as_ptr().add(base).cast::<__m128i>());
        let lo = _mm_unpacklo_epi8(raw, zero);
        let hi = _mm_unpackhi_epi8(raw, zero);

        let lo_lo = _mm_unpacklo_epi16(lo, zero);
        let lo_hi = _mm_unpackhi_epi16(lo, zero);
        let hi_lo = _mm_unpacklo_epi16(hi, zero);
        let hi_hi = _mm_unpackhi_epi16(hi, zero);

        for (q, p) in [lo_lo, lo_hi, hi_lo, hi_hi].into_iter().enumerate() {
            let f = _mm_cvtepi32_ps(p);
            let centered = _mm_sub_ps(f, center);
            let scaled = _mm_mul_ps(centered, scale);
            // SAFETY: base + q*4..+4 in-bounds (q < 4).
            _mm_storeu_ps(output.as_mut_ptr().add(base + q * 4), scaled);
        }
    }

    let tail_start = chunks * LANES;
    for i in tail_start..n {
        *output.get_unchecked_mut(i) =
            (f32::from(*input.get_unchecked(i)) - CU8_CENTER) * CU8_SCALE;
    }
}

// ---------------------------------------------------------------------------
// aarch64 NEON path (Apple Silicon, Android arm64)
// ---------------------------------------------------------------------------

/// Hand-written NEON kernel, retained but **not dispatched** — see
/// [`select_backend`] for the measurement (16% slower than the autovectorized
/// scalar loop on an M1 Max, because the stage is store-bandwidth bound).
///
/// It is kept compiled and pinned by `neon_backend_matches_scalar_exactly` so
/// it cannot rot, because it has never been measured on an in-order ARM core.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(clippy::wildcard_imports)] // idiomatic for std::arch SIMD intrinsics
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn cu8_to_cf32_neon(input: &[u8], output: &mut [f32]) {
    use std::arch::aarch64::*;
    // Process 16 input bytes -> 16 output floats per iteration.
    const LANES: usize = 16;
    let n = input.len();
    let chunks = n / LANES;

    let center = vdupq_n_f32(CU8_CENTER);
    let scale = vdupq_n_f32(CU8_SCALE);

    for c in 0..chunks {
        let base = c * LANES;
        // SAFETY: base..base+16 in-bounds.
        let raw = vld1q_u8(input.as_ptr().add(base));
        // 16 u8 -> two 8-wide u16 halves.
        let lo_u16 = vmovl_u8(vget_low_u8(raw));
        let hi_u16 = vmovl_u8(vget_high_u8(raw));

        // Each u16 half -> two u32 quarters -> subtract center, scale, store.
        let mut written = 0usize;
        for half in [lo_u16, hi_u16] {
            for vec in [
                vmovl_u16(vget_low_u16(half)),
                vmovl_u16(vget_high_u16(half)),
            ] {
                let f = vcvtq_f32_u32(vec);
                let centered = vsubq_f32(f, center);
                let scaled = vmulq_f32(centered, scale);
                let dst = output.as_mut_ptr().add(base + written * 4);
                // SAFETY: written < 4, so base+written*4..+4 is inside the 16-wide block.
                vst1q_f32(dst, scaled);
                written += 1;
            }
        }
    }

    let tail_start = chunks * LANES;
    for i in tail_start..n {
        *output.get_unchecked_mut(i) =
            (f32::from(*input.get_unchecked(i)) - CU8_CENTER) * CU8_SCALE;
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn _force_scalar_only_compile() {}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    #[test]
    fn scalar_converts_known_values() {
        // 128 → ~0.0039 (just above center), 127 → -0.0039 (just below), 0 → -1, 255 → ~1
        let input = [128u8, 127, 0, 255];
        let mut out = [0.0f32; 4];
        cu8_to_cf32_scalar(&input, &mut out);
        assert!((out[0] - (128.0 - 127.5) / 127.5).abs() < 1e-6);
        assert!((out[1] - (127.0 - 127.5) / 127.5).abs() < 1e-6);
        assert!((out[2] + 1.0).abs() < 1e-6);
        assert!((out[3] - (255.0 - 127.5) / 127.5).abs() < 1e-6);
    }

    #[test]
    fn simd_matches_scalar_on_various_lengths() {
        // Cover aligned, unaligned, and zero-length inputs.
        for &len in &[0usize, 1, 16, 17, 31, 32, 33, 64, 100, 256] {
            let input: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let mut scalar = vec![0.0f32; len];
            cu8_to_cf32_scalar(&input, &mut scalar);
            let mut simd = vec![0.0f32; len];
            cu8_to_cf32(&input, &mut simd);
            for (i, (a, b)) in scalar.iter().zip(simd.iter()).enumerate() {
                assert!(
                    (a - b).abs() < 1e-6,
                    "len={len} idx={i}: scalar={a} simd={b}"
                );
            }
        }
    }

    /// The NEON kernel is deliberately not dispatched (it measured 16% slower
    /// than the autovectorized scalar loop on an M1 Max), but it is retained
    /// for in-order ARM cores that have not been measured. Pin it against the
    /// scalar oracle so an unselected path cannot silently rot, and so
    /// re-enabling it stays a one-line change.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_backend_matches_scalar_exactly() {
        for &len in &[0usize, 1, 15, 16, 17, 63, 64, 65, 256, 1021] {
            let input: Vec<u8> = (0..len).map(|i| i.wrapping_mul(37) as u8).collect();
            let mut scalar = vec![0.0f32; len];
            cu8_to_cf32_scalar(&input, &mut scalar);
            let mut neon = vec![0.0f32; len];
            // SAFETY: NEON is a baseline guarantee on aarch64.
            unsafe { cu8_to_cf32_neon(&input, &mut neon) };
            // Both compute (b - 127.5) * (1/127.5) per byte with no reassociation,
            // so agreement here is exact, not approximate.
            assert_eq!(scalar, neon, "len={len}");
        }
    }

    #[test]
    fn convert_iq_entrypoint_routes_cu8() {
        let input = vec![100u8, 200, 50, 250];
        let mut out = vec![0.0f32; 4];
        convert_iq(&input, IqFormat::Cu8, IqFormat::Cf32, &mut out);
        let mut expect = vec![0.0f32; 4];
        cu8_to_cf32_scalar(&input, &mut expect);
        assert_eq!(out, expect);
    }

    #[test]
    fn convert_iq_entrypoint_routes_cs8() {
        // cs8: bytes reinterpreted as i8, divided by 128.
        let input = vec![0u8, 128u8 /* -128 as u8 */, 64, 192 /* -64 */];
        let mut out = vec![0.0f32; 4];
        convert_iq(&input, IqFormat::Cs8, IqFormat::Cf32, &mut out);
        assert!(out[0].abs() < 1e-6);
        assert!((out[1] - (-128.0 / 128.0)).abs() < 1e-6);
        assert!((out[2] - (64.0 / 128.0)).abs() < 1e-6);
        assert!((out[3] - (-64.0 / 128.0)).abs() < 1e-6);
    }

    #[test]
    fn signed_and_unsigned_full_scale_conventions_are_explicit() {
        let mut cu8 = [0.0; 2];
        cu8_to_cf32(&[0, u8::MAX], &mut cu8);
        assert_eq!(
            cu8.map(f32::to_bits),
            [(-1.0f32).to_bits(), 1.0f32.to_bits()]
        );

        let mut cs8 = [0.0; 2];
        cs8_to_cf32(
            &[i8::MIN.to_ne_bytes()[0], i8::MAX.to_ne_bytes()[0]],
            &mut cs8,
        );
        assert_eq!(
            cs8.map(f32::to_bits),
            [(-1.0f32).to_bits(), (127.0f32 / 128.0).to_bits()]
        );

        // The 0.068 dB positive-peak difference is an encoding convention,
        // not an accidental calibration drift between conversion backends.
        let peak_difference_db = 20.0 * (cu8[1] / cs8[1]).log10();
        assert!((peak_difference_db - 0.068_124_97).abs() < 1e-6);
    }

    #[test]
    fn convert_iq_entrypoint_routes_little_endian_cs16() {
        let values = [i16::MIN, -1, 0, i16::MAX];
        let input: Vec<u8> = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let mut output = vec![0.0; values.len()];
        convert_iq(&input, IqFormat::Cs16, IqFormat::Cf32, &mut output);
        let expected: Vec<f32> = values
            .iter()
            .map(|&value| f32::from(value) / 32_768.0)
            .collect();
        assert_eq!(output, expected);
    }

    #[test]
    fn integer_format_helpers_cover_every_input_byte() {
        let input: Vec<u8> = (0..=u8::MAX).collect();
        let mut cs8 = vec![0i8; input.len()];
        let mut cs16 = vec![0i16; input.len()];
        cu8_to_cs8(&input, &mut cs8);
        cu8_to_cs16(&input, &mut cs16);
        for (index, &byte) in input.iter().enumerate() {
            assert_eq!(cs8[index], i8::from_ne_bytes([byte ^ 0x80]));
            assert_eq!(cs16[index], (i16::from(byte) - 128) << 8);
        }
    }

    #[test]
    fn typed_cs16_conversion_covers_arbitrary_tail() {
        let input = [i16::MIN, -16_384, -1, 0, 1, 16_384, i16::MAX];
        let mut output = [0.0; 7];
        cs16_to_cf32(&input, &mut output);
        for (actual, input) in output.iter().zip(input) {
            assert_eq!(actual.to_bits(), (f32::from(input) / 32_768.0).to_bits());
        }
    }

    #[test]
    #[should_panic(expected = "floats (3 bytes")]
    fn convert_iq_rejects_wrong_output_length() {
        let input = [1u8, 2, 3];
        let mut out = [0.0f32; 4]; // should be 3
        convert_iq(&input, IqFormat::Cu8, IqFormat::Cf32, &mut out);
    }

    #[test]
    fn full_scale_cu8_maps_to_approx_unit_magnitude() {
        // A full-scale sample (0, 255) → magnitude ≈ sqrt(1 + 1) ≈ 1.414.
        let input = [0u8, 255];
        let mut out = [0.0f32; 2];
        cu8_to_cf32(&input, &mut out);
        let mag = (out[0] * out[0] + out[1] * out[1]).sqrt();
        assert!((mag - 2.0_f32.sqrt()).abs() < 1e-5, "mag={mag}");
    }

    #[test]
    fn empty_input_is_no_op() {
        let mut out: [f32; 0] = [];
        cu8_to_cf32(&[], &mut out);
    }

    // -------------------------------------------------------------------------
    // AVX2-specific lane-order regression tests.
    //
    // These only run (and only force the AVX2 path) on x86_64 hosts that report
    // the `avx2` feature. The original AVX2 path used `_mm256_unpacklo/hi_epi8`
    // + `_mm256_unpacklo/hi_epi16`, which interleaves 128-bit lanes and emits
    // samples in permuted order (0..3, 16..19, 4..7, 20..23, ...). The fixed
    // path uses four sequential `_mm256_cvtepu8_epi32` loads that preserve
    // source order. These tests catch a regression back to the scrambled order
    // by feeding unique monotonic bytes and checking the output matches the
    // scalar reference exactly at every index.
    // -------------------------------------------------------------------------

    /// Build a 64-byte repeating ramp: [0,1,2,...,31, 0,1,2,...,31].
    #[cfg(target_arch = "x86_64")]
    fn ramp_block(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 32) as u8).collect()
    }

    /// Scalar reference computed inline (independent of `cu8_to_cf32_scalar`,
    /// so a bug in the shared scalar wouldn't mask the AVX2 bug).
    #[cfg(target_arch = "x86_64")]
    fn scalar_ref(input: &[u8]) -> Vec<f32> {
        input
            .iter()
            .map(|&b| (f32::from(b) - CU8_CENTER) * CU8_SCALE)
            .collect()
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn avx2_convert(input: &[u8], output: &mut [f32]) {
        cu8_to_cf32_avx2(input, output);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_matches_scalar_at_vector_boundaries() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("skipping avx2 test: CPU lacks avx2");
            return;
        }
        // Cover lengths around every vector boundary (8/16/24/32 wide chunks
        // plus the scalar tail).
        for &len in &[0usize, 8, 16, 24, 31, 32, 33, 40, 64, 65, 96, 100, 256] {
            let input = ramp_block(len);
            let expected = scalar_ref(&input);
            let mut got = vec![0.0f32; len];
            // SAFETY: guarded by an `avx2` runtime check above.
            unsafe { avx2_convert(&input, &mut got) };
            for (i, (a, b)) in expected.iter().zip(got.iter()).enumerate() {
                assert!(
                    (a - b).abs() < 1e-6,
                    "avx2 len={len} idx={i}: scalar={a} avx2={b} (input byte={})",
                    input[i]
                );
            }
        }
    }

    /// Specifically target the lane-scrambling signature: with the old buggy
    /// path, the first 8 output floats came from input bytes 0,1,2,3,16,17,18,
    /// 19 instead of 0..7. So output[4] (which should be byte 4) would equal
    /// the scalar value of byte 16. This test asserts each output index maps
    /// to the *correct* source byte, which fails loudly under any lane
    /// permutation.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_preserves_source_order_unique_bytes() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("skipping avx2 test: CPU lacks avx2");
            return;
        }
        // One full 32-byte chunk of distinct bytes, so a lane permutation is
        // detectable at any index.
        let input: Vec<u8> = (0..32u8).collect();
        let expected = scalar_ref(&input);
        let mut got = vec![0.0f32; 32];
        // SAFETY: guarded by an `avx2` runtime check above.
        unsafe { avx2_convert(&input, &mut got) };
        for (i, (a, b)) in expected.iter().zip(got.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-6,
                "avx2 lane order idx={i}: expected (byte {i})={a} got={b}",
            );
        }
    }
}
