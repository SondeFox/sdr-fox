//! FM discrimination: polar discriminator with two `atan2` backends.
//!
//! The polar discriminator computes the phase difference between consecutive
//! samples: `arg(x[n] * conj(x[n-1]))`. That phase difference *is* the
//! instantaneous frequency (the demodulated audio sample).
//!
//! Two atan backends:
//! - [`atan2_reference`]: `f32::atan2`. The reference.
//! - [`fast_atan2`]: a branch-reduced rational approximation (<0.01 rad error).
//!
//! Both are unit-tested for accuracy against `f32::atan2`. Note that on a
//! modern FPU `fast_atan2` is not necessarily faster than `f32::atan2`
//! (measured slower on an Apple M1), so pick a backend by benchmarking the
//! enclosing loop rather than by name.

use std::f32::consts::PI;

/// Reference atan2 — the slow-but-exact backend.
#[inline]
#[must_use]
pub fn atan2_reference(y: f32, x: f32) -> f32 {
    y.atan2(x)
}

/// Fast atan2 via a polynomial approximation (range-reduce then approximate).
/// Accuracy: <0.01 rad across the full circle.
///
/// Uses the standard rational approximation `r / (1 + 0.28*r²)` with proper
/// quadrant handling. The polynomial is applied to `|y/x|` (always in `[0, 1]`
/// after reduction), then the correct quadrant is selected based on signs.
#[inline]
#[must_use]
pub fn fast_atan2(y: f32, x: f32) -> f32 {
    // Preserve IEEE axis, signed-zero, infinity, and NaN behavior exactly.
    // The approximation below is only defined for finite, nonzero coordinates.
    if x == 0.0 || y == 0.0 || !x.is_finite() || !y.is_finite() {
        return y.atan2(x);
    }
    // Compute atan(|y/x|) using the rational approximation.
    let r = (y.abs() / x.abs()).min(1e9); // clamp to avoid inf
    let atan_r = if r > 1.0 {
        // atan(r) = π/2 - atan(1/r) for r > 1
        let inv = 1.0 / r;
        PI / 2.0 - inv / (1.0 + 0.28 * inv * inv)
    } else {
        r / (1.0 + 0.28 * r * r)
    };
    // Select quadrant.
    if x >= 0.0 {
        if y >= 0.0 {
            atan_r // QI
        } else {
            -atan_r // QIV
        }
    } else if y >= 0.0 {
        PI - atan_r // QII: π - atan(|y/x|)
    } else {
        -PI + atan_r // QIII: -π + atan(|y/x|)
    }
}

/// Compute the polar-discriminator output for one step: the phase difference
/// between the current and previous complex samples. `prev` = (pr, pj),
/// `cur` = (cr, cj). Returns the phase delta in radians, in (-π, π].
///
/// The math: `arg(cur * conj(prev)) = atan2(cr*pj - cj*pr, cr*pr + cj*pj)`.
#[inline]
#[must_use]
pub fn polar_discriminant(pr: f32, pj: f32, cr: f32, cj: f32) -> f32 {
    // arg(cur * conj(prev)): real = cr*pr + cj*pj, imag = cj*pr - cr*pj.
    let real = cr * pr + cj * pj;
    let imag = cj * pr - cr * pj;
    imag.atan2(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_atan2_matches_stdlib() {
        // Trivially correct by construction; this is the regression anchor.
        for &(y, x) in &[
            (1.0f32, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (-1.0, 1.0),
            (1.0, -1.0),
        ] {
            assert!((atan2_reference(y, x) - y.atan2(x)).abs() < 1e-6);
        }
    }

    #[test]
    fn fast_atan2_within_tolerance() {
        // Sweep the full unit circle; enforce the documented <0.01 rad error.
        let mut max_err = 0.0f32;
        for i in 0..360 {
            let a = (i as f32) * PI / 180.0;
            let (y, x) = (a.sin(), a.cos());
            let got = fast_atan2(y, x);
            let truth = y.atan2(x);
            let mut diff = (got - truth).abs();
            if diff > PI {
                diff = 2.0 * PI - diff;
            }
            max_err = max_err.max(diff);
        }
        assert!(max_err < 0.01, "fast_atan2 max error {max_err} > 0.01");
    }

    #[test]
    fn fast_atan2_specific_quadrants() {
        // Test specific points that were wrong before the fix.
        let cases = [
            (1.0f32, -1.0f32),  // QII: 3π/4
            (-1.0f32, -1.0f32), // QIII: -3π/4
            (1.0f32, 1.0f32),   // QI: π/4
            (-1.0f32, 1.0f32),  // QIV: -π/4
            (0.0f32, -1.0f32),  // π
            (1.0f32, 0.0f32),   // π/2
        ];
        for &(y, x) in &cases {
            let got = fast_atan2(y, x);
            let truth = y.atan2(x);
            let mut diff = (got - truth).abs();
            if diff > PI {
                diff = 2.0 * PI - diff;
            }
            assert!(
                diff < 0.01,
                "fast_atan2({y},{x}) = {got}, truth = {truth}, diff = {diff}"
            );
        }
    }

    #[test]
    fn atan_backends_cover_signed_zero_infinity_and_cartesian_grid() {
        let special = [
            (0.0, -1.0),
            (-0.0, -1.0),
            (0.0, 1.0),
            (-0.0, 1.0),
            (1.0, 0.0),
            (-1.0, 0.0),
            (f32::INFINITY, f32::INFINITY),
            (f32::INFINITY, f32::NEG_INFINITY),
            (f32::NEG_INFINITY, f32::INFINITY),
            (f32::NEG_INFINITY, f32::NEG_INFINITY),
        ];
        for (y, x) in special {
            let truth = y.atan2(x);
            let fast = fast_atan2(y, x);
            assert_eq!(fast.to_bits(), truth.to_bits(), "special ({y:?}, {x:?})");
        }

        // Deterministic property grid exercises widely varying ratios in all
        // quadrants, not only unit-circle inputs.
        for yi in -32..=32 {
            for xi in -32..=32 {
                if xi == 0 && yi == 0 {
                    continue;
                }
                let y = yi as f32 / 3.0;
                let x = xi as f32 / 5.0;
                let truth = y.atan2(x);
                assert!(angular_error(fast_atan2(y, x), truth) < 0.01);
            }
        }
    }

    fn angular_error(a: f32, b: f32) -> f32 {
        let diff = (a - b).abs();
        if diff > PI {
            2.0 * PI - diff
        } else {
            diff
        }
    }

    #[test]
    fn polar_discriminant_of_constant_phase_is_zero() {
        // Two identical samples => zero phase difference.
        let d = polar_discriminant(1.0, 0.0, 1.0, 0.0);
        assert!(d.abs() < 1e-6);
    }

    #[test]
    fn polar_discriminant_of_quarter_turn_is_pi_over_2() {
        // (1,0) -> (0,1) is a +90° rotation.
        let d = polar_discriminant(1.0, 0.0, 0.0, 1.0);
        assert!((d - PI / 2.0).abs() < 1e-5, "got {d}");
    }

    #[test]
    fn polar_discriminant_of_known_fm_tone() {
        // A complex exponential at frequency f has constant phase advance
        // 2πf/fs per sample. Generate one and check the discriminator output.
        let f = 1000.0_f32; // 1 kHz audio
        let fs = 48_000.0_f32;
        let phase_step = 2.0 * PI * f / fs;
        let n = 256;
        let mut max_err = 0.0f32;
        let mut pr = 1.0f32;
        let mut pj = 0.0f32;
        let mut ph = 0.0f32;
        for _ in 1..n {
            ph += phase_step;
            let cr = ph.cos();
            let cj = ph.sin();
            let disc = polar_discriminant(pr, pj, cr, cj);
            let diff = (disc - phase_step).abs();
            max_err = max_err.max(diff);
            pr = cr;
            pj = cj;
        }
        assert!(max_err < 1e-4, "FM tone discriminator error {max_err}");
    }
}
