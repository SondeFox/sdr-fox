//! FFT-based spectrum for waterfall / panadapter display.
//!
//! Single-shot Welch-style power spectrum: window → complex FFT → |·|² →
//! 10·log10 dBFS, fftshifted so DC sits at the center bin. Multi-frame
//! averaging, noise-floor tracking, and peak-hold are deliberately left to
//! the consumer (the display layer); this module is the per-block primitive.
//!
//! The IQ input is interleaved complex float (cf32). We compute the power by
//! taking the magnitude-squared of the complex FFT directly (not a real
//! FFT of the magnitude), which is what a spectrum analyzer needs.
//!
//! ## Normalization modes
//!
//! Two normalization conventions are supported via [`Normalization`]:
//!
//! - [`Normalization::DbfsCoherent`] (default): divide the magnitude-squared by
//!   `(sum window[i])^2`, the window's *coherent gain*. A full-scale complex
//!   exponential at an integer bin then reads 0 dBFS at that bin, independent
//!   of the window shape. This matches what "dBFS relative to a full-scale
//!   tone" means on a spectrum analyzer.
//! - [`Normalization::Psd`]: divide by `N * sum(window[i]^2)`. This is a
//!   power-spectral-density-style normalization (total window power spread
//!   across bins). Useful for noise-power integration but it conflates window
//!   gain with bin count, so a full-scale tone reads below 0 dBFS.
//!
//! ## Performance note
//!
//! The FFT is a planned [`rustfft`] forward transform (O(N log N)), reused
//! across blocks. Per-block allocations are confined to the reusable scratch
//! buffers; the window power and coherent gain are cached when the window is
//! set, not recomputed per block.

use num_complex::Complex32 as Complexf32;
use rustfft::{Fft, FftDirection, FftPlanner};

/// Window function applied before the FFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Window {
    /// Rectangular (no window). Highest frequency resolution, worst leakage.
    Rectangular,
    /// Hann window. Good general-purpose choice.
    #[default]
    Hann,
    /// Hamming window.
    Hamming,
    /// Blackman window. Best side-lobe rejection (dynamic range).
    Blackman,
}

/// How the magnitude-squared spectrum is normalized before taking 10·log10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Normalization {
    /// Coherent-gain normalization: divide `|X[k]|²` by `(sum window[i])²`.
    ///
    /// A full-scale complex exponential at an integer bin then reads 0 dBFS,
    /// independent of window shape. This is the natural "dBFS relative to a
    /// full-scale tone" convention.
    #[default]
    DbfsCoherent,
    /// Power-spectral-density normalization: divide `|X[k]|²` by
    /// `N · sum(window[i]²)`. Spreads total window power across bins; useful
    /// for integrating noise power but reads a full-scale tone below 0 dBFS.
    Psd,
}

/// Single-shot spectrum analyzer: window → complex FFT → power in dBFS.
///
/// `size` is the number of *complex* samples per block (equivalently the FFT
/// size). It must be a power of two. Output has `size` bins, fftshifted so DC
/// is at index `size / 2`.
pub struct Spectrum {
    size: usize,
    window: Vec<f32>,
    /// Cached `sum(window[i] * window[i])`. Recomputed in [`set_window`].
    win_power: f32,
    /// Cached `sum(window[i])`. Recomputed in [`set_window`].
    win_sum: f32,
    /// Active normalization convention.
    normalization: Normalization,
    /// Cached linear multiplier for the active window/normalization pair.
    norm_linear: f32,
    /// Planned forward FFT, length `size`.
    fft_plan: std::sync::Arc<dyn Fft<f32>>,
    /// Scratch: windowed complex input / FFT work buffer, length `size`.
    windowed: Vec<Complexf32>,
    /// Scratch: scratch space required by the FFT plan, length
    /// `fft_plan.get_inplace_scratch_len()`.
    fft_scratch: Vec<Complexf32>,
    /// Scratch: normalized power in the representation requested by the most
    /// recent compute call, length `size`.
    power: Vec<f32>,
}

impl Spectrum {
    /// Construct a spectrum analyzer with `size` complex bins.
    ///
    /// # Panics
    ///
    /// Panics if `size` is not a power of two or is `<= 1`.
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn new(size: usize) -> Self {
        assert!(
            size.is_power_of_two(),
            "Spectrum size must be a power of two"
        );
        assert!(size > 1, "Spectrum size must be > 1");

        let mut planner: FftPlanner<f32> = FftPlanner::new();
        let fft_plan = planner.plan_fft(size, FftDirection::Forward);
        let scratch_len = fft_plan.get_inplace_scratch_len();
        let window = Self::build_window(Window::default(), size);
        let (win_power, win_sum) = Self::window_stats(&window);
        let normalization = Normalization::default();
        let norm_linear = Self::normalization_factor_for(normalization, size, win_power, win_sum);

        Self {
            size,
            window,
            win_power,
            win_sum,
            normalization,
            norm_linear,
            fft_plan,
            windowed: vec![Complexf32::new(0.0, 0.0); size],
            fft_scratch: vec![Complexf32::new(0.0, 0.0); scratch_len],
            power: vec![0.0; size],
        }
    }

    /// Set the window function. Rebuilds the window vector and recomputes the
    /// cached normalization statistics.
    pub fn set_window(&mut self, window: Window) {
        self.window = Self::build_window(window, self.size);
        let (win_power, win_sum) = Self::window_stats(&self.window);
        self.win_power = win_power;
        self.win_sum = win_sum;
        self.refresh_normalization();
    }

    /// Set the normalization convention used by [`compute_power_dbfs`] and
    /// [`compute_power_linear`].
    ///
    /// [`compute_power_dbfs`]: Self::compute_power_dbfs
    /// [`compute_power_linear`]: Self::compute_power_linear
    pub fn set_normalization(&mut self, normalization: Normalization) {
        self.normalization = normalization;
        self.refresh_normalization();
    }

    /// Compute the power spectrum (dBFS) for one block of interleaved cf32
    /// samples. `iq` must contain exactly `self.size * 2` floats (i.e.
    /// `self.size` complex samples). Returns a slice of `self.size` bins,
    /// fftshifted (DC at center).
    ///
    /// # Panics
    ///
    /// Panics if `iq.len() != self.size * 2`.
    #[allow(clippy::missing_panics_doc)]
    pub fn compute_power_dbfs(&mut self, iq: &[f32]) -> &[f32] {
        self.transform(iq);

        // Power = |X[k]|² / norm, then 10·log10. The normalization depends on
        // the selected convention (see [`Normalization`]); the statistics it
        // uses were cached in `set_window` / `new`.
        for (k, bin) in self.windowed.iter().enumerate() {
            // Keep silence finite and deterministic for display consumers.
            let mag_sq = (bin.norm_sqr() * self.norm_linear).max(1e-30);
            self.power[k] = 10.0 * mag_sq.log10();
        }

        // fftshift: move DC (bin 0) to the center.
        Self::fftshift(&mut self.power);
        &self.power
    }

    /// Compute normalized linear power for one block of interleaved cf32
    /// samples. The input and output layout match [`Self::compute_power_dbfs`], but
    /// no logarithm is applied. This is the correct primitive for Welch
    /// averaging: callers average linear power and convert to dB only once.
    ///
    /// # Panics
    ///
    /// Panics if `iq.len() != self.size * 2`.
    #[allow(clippy::missing_panics_doc)]
    pub fn compute_power_linear(&mut self, iq: &[f32]) -> &[f32] {
        self.transform(iq);
        for (k, bin) in self.windowed.iter().enumerate() {
            self.power[k] = bin.norm_sqr() * self.norm_linear;
        }
        Self::fftshift(&mut self.power);
        &self.power
    }

    fn transform(&mut self, iq: &[f32]) {
        assert_eq!(
            iq.len(),
            self.size * 2,
            "iq must hold exactly size*2 floats ({} complex samples)",
            self.size
        );

        // Build windowed complex samples directly into the FFT work buffer —
        // no per-call allocation (the previous code cloned `self.windowed`
        // every block to work around the aliasing borrow).
        for ((destination, pair), &window) in self
            .windowed
            .iter_mut()
            .zip(iq.chunks_exact(2))
            .zip(&self.window)
        {
            *destination = Complexf32::new(pair[0] * window, pair[1] * window);
        }

        // Forward complex FFT in place (O(N log N)), using the reusable
        // scratch buffer. Replaces the previous O(N²) naive DFT.
        self.fft_plan
            .process_with_scratch(&mut self.windowed, &mut self.fft_scratch);
    }

    /// Per-bin linear normalization factor (multiply |X[k]|² by this before
    /// taking 10·log10). The `+1e-30` floor keeps `log10` finite for an
    /// all-zero input without measurably changing any real bin.
    fn normalization_factor_for(
        normalization: Normalization,
        size: usize,
        win_power: f32,
        win_sum: f32,
    ) -> f32 {
        match normalization {
            Normalization::DbfsCoherent => 1.0 / (win_sum * win_sum + 1e-30),
            Normalization::Psd => 1.0 / (size as f32 * win_power + 1e-30),
        }
    }

    fn refresh_normalization(&mut self) {
        self.norm_linear = Self::normalization_factor_for(
            self.normalization,
            self.size,
            self.win_power,
            self.win_sum,
        );
    }

    /// `(sum(window²), sum(window))` for a window vector.
    fn window_stats(window: &[f32]) -> (f32, f32) {
        let mut win_power = 0.0_f64;
        let mut win_sum = 0.0_f64;
        for &w in window {
            let w = f64::from(w);
            win_power += w * w;
            win_sum += w;
        }
        (win_power as f32, win_sum as f32)
    }

    /// In-place fftshift: swap the two halves so DC moves to the center.
    fn fftshift(buf: &mut [f32]) {
        let half = buf.len() / 2;
        let (left, right) = buf.split_at_mut(half);
        left.swap_with_slice(right);
    }

    fn build_window(window: Window, n: usize) -> Vec<f32> {
        let two_pi = 2.0 * std::f32::consts::PI;
        (0..n)
            .map(|i| {
                // Periodic (`fftbins=true`) windows are the appropriate form
                // for an N-point spectrum; symmetric N-1 windows are for FIRs.
                let t = two_pi * i as f32 / n as f32;
                match window {
                    Window::Rectangular => 1.0,
                    Window::Hann => 0.5 - 0.5 * t.cos(),
                    Window::Hamming => 0.54 - 0.46 * t.cos(),
                    Window::Blackman => 0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_power_of_two() {
        let result = std::panic::catch_unwind(|| Spectrum::new(100));
        assert!(result.is_err(), "size 100 is not a power of two");
    }

    #[test]
    fn rejects_size_zero_and_one() {
        assert!(std::panic::catch_unwind(|| Spectrum::new(0)).is_err());
        assert!(std::panic::catch_unwind(|| Spectrum::new(1)).is_err());
    }

    #[test]
    fn pure_tone_peaks_at_expected_bin() {
        // A complex exponential e^{j*2π*k*n/N} has all its energy at bin k.
        // After fftshift, +k lands at index N/2 + k.
        let size = 64;
        let mut spec = Spectrum::new(size);
        spec.set_window(Window::Rectangular); // no spreading → sharp peak
        let k = 7;
        let two_pi = 2.0 * std::f32::consts::PI;
        let mut iq = vec![0.0f32; size * 2];
        for n in 0..size {
            let phase = two_pi * k as f32 * n as f32 / size as f32;
            iq[n * 2] = phase.cos();
            iq[n * 2 + 1] = phase.sin();
        }
        let power = spec.compute_power_dbfs(&iq).to_vec();

        // Peak must be exactly at the expected fftshifted bin.
        let expected = size / 2 + k;
        let mut max_idx = 0;
        for (i, &p) in power.iter().enumerate() {
            if p > power[max_idx] {
                max_idx = i;
            }
        }
        assert_eq!(
            max_idx,
            expected,
            "peak landed at {max_idx}, expected {expected} (DC center {} + k {k})",
            size / 2
        );

        // And it should dominate the nearest neighbor by a healthy margin
        // (rectangular window on an integer-bin tone → near-delta).
        let peak = power[expected];
        let others_max = power
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != expected)
            .map(|(_, &p)| p)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            peak > others_max + 20.0,
            "peak {peak} should be >20 dB above next {others_max}"
        );
    }

    #[test]
    fn dc_input_peaks_at_center_after_shift() {
        let size = 32;
        let mut spec = Spectrum::new(size);
        // All-ones IQ (DC).
        let iq = vec![1.0f32; size * 2];
        let power = spec.compute_power_dbfs(&iq).to_vec();
        // After fftshift, DC bin lives at index size/2.
        let mut max_idx = 0;
        for (i, &p) in power.iter().enumerate() {
            if p > power[max_idx] {
                max_idx = i;
            }
        }
        assert_eq!(
            max_idx,
            size / 2,
            "DC peak must land at center after fftshift"
        );
    }

    #[test]
    fn window_hann_is_bell_shaped() {
        let w = Spectrum::build_window(Window::Hann, 64);
        // Midpoint should be the maximum (~1.0 for Hann).
        let mid = w[32];
        let edge = w[0];
        assert!(mid > edge, "Hann midpoint {mid} should exceed edge {edge}");
        assert!(mid <= 1.0 + 1e-5);
    }

    #[test]
    fn analyzer_windows_are_periodic_not_symmetric() {
        let window = Spectrum::build_window(Window::Hann, 64);
        assert!(window[0].abs() < f32::EPSILON);
        assert!(
            window[63] > 0.0,
            "periodic Hann does not repeat its zero endpoint"
        );
        assert!((window[32] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalization_cache_tracks_mode_and_window() {
        let mut spectrum = Spectrum::new(64);
        let initial = spectrum.norm_linear;
        spectrum.set_normalization(Normalization::Psd);
        assert_ne!(spectrum.norm_linear.to_bits(), initial.to_bits());
        let psd_hann = spectrum.norm_linear;
        spectrum.set_window(Window::Blackman);
        assert_ne!(spectrum.norm_linear.to_bits(), psd_hann.to_bits());
    }

    #[test]
    fn window_blackman_side_lobe_below_hann_at_edges() {
        // Blackman should suppress edges harder than Hann.
        let hann = Spectrum::build_window(Window::Hann, 64);
        let blackman = Spectrum::build_window(Window::Blackman, 64);
        assert!(blackman[0] < hann[0]);
    }

    #[test]
    fn fftshift_swaps_halves() {
        let mut buf = (0..8).map(|x| x as f32).collect::<Vec<_>>();
        Spectrum::fftshift(&mut buf);
        // [0,1,2,3,4,5,6,7] -> [4,5,6,7,0,1,2,3]
        assert_eq!(buf, vec![4.0, 5.0, 6.0, 7.0, 0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn compute_power_dbfs_rejects_wrong_length() {
        use std::panic::AssertUnwindSafe;
        let mut spec = Spectrum::new(16);
        let bad = vec![0.0f32; 10];
        let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
            let _ = spec.compute_power_dbfs(&bad);
        }));
        assert!(result.is_err());
    }

    #[test]
    fn silence_has_finite_floor() {
        let mut spec = Spectrum::new(32);
        let iq = vec![0.0f32; 64];
        let power = spec.compute_power_dbfs(&iq);
        assert!(power.iter().all(|bin| bin.is_finite()));
        assert!(power.iter().all(|&bin| (bin + 300.0).abs() < 1e-4));
    }

    #[test]
    fn linear_power_is_the_pre_log_dbfs_value() {
        let size = 64;
        let mut iq = vec![0.0f32; size * 2];
        for (index, pair) in iq.chunks_exact_mut(2).enumerate() {
            let phase = 2.0 * std::f32::consts::PI * 7.0 * index as f32 / size as f32;
            pair[0] = 0.25 * phase.cos();
            pair[1] = 0.25 * phase.sin();
        }
        let mut spec = Spectrum::new(size);
        let linear = spec.compute_power_linear(&iq).to_vec();
        let db = spec.compute_power_dbfs(&iq).to_vec();
        for (&linear, &db) in linear.iter().zip(&db) {
            let expected = 10.0 * linear.max(1e-30).log10();
            assert!((db - expected).abs() < 1e-5, "{db} != {expected}");
        }
    }

    /// A full-scale complex exponential at an integer bin, normalized by
    /// coherent gain, must read 0 dBFS at that bin. This validates the
    /// `Normalization::DbfsCoherent` mode (the default) and the FFT path:
    /// bin placement, windowing, normalization, and dB conversion.
    #[test]
    fn dbfs_coherent_full_scale_tone_reads_zero_db() {
        let size = 256;
        for k in [1usize, 13, 64] {
            let mut spec = Spectrum::new(size);
            spec.set_window(Window::Rectangular);
            spec.set_normalization(Normalization::DbfsCoherent);

            let two_pi = 2.0 * std::f32::consts::PI;
            let mut iq = vec![0.0f32; size * 2];
            for n in 0..size {
                let phase = two_pi * k as f32 * n as f32 / size as f32;
                iq[n * 2] = phase.cos();
                iq[n * 2 + 1] = phase.sin();
            }
            let power = spec.compute_power_dbfs(&iq).to_vec();

            let peak_idx = size / 2 + k;
            let peak_db = power[peak_idx];
            // Rectangular window, coherent gain = sum(window) = N, so a
            // unit-amplitude complex tone yields |X[k]|²/N² = 1 → 0 dBFS.
            assert!(
                (peak_db - 0.0).abs() < 0.05,
                "k={k}: peak {peak_db} dBFS should be ~0 dBFS (<0.05 dB error)"
            );

            // Noise floor: every other bin should be well below the peak.
            let floor_max = power
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != peak_idx)
                .map(|(_, &p)| p)
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                peak_db > floor_max + 20.0,
                "k={k}: peak {peak_db} should be >20 dB above floor {floor_max}"
            );
        }
    }

    /// A full-scale Hann-windowed complex exponential should still read 0
    /// dBFS under coherent-gain normalization — the coherent gain
    /// `sum(window)` cancels the window's amplitude scaling exactly.
    #[test]
    fn dbfs_coherent_is_window_invariant() {
        let size = 128;
        let k = 5;
        let two_pi = 2.0 * std::f32::consts::PI;
        let mut iq = vec![0.0f32; size * 2];
        for n in 0..size {
            let phase = two_pi * k as f32 * n as f32 / size as f32;
            iq[n * 2] = phase.cos();
            iq[n * 2 + 1] = phase.sin();
        }
        for window in [
            Window::Rectangular,
            Window::Hann,
            Window::Hamming,
            Window::Blackman,
        ] {
            let mut spec = Spectrum::new(size);
            spec.set_window(window);
            spec.set_normalization(Normalization::DbfsCoherent);
            let power = spec.compute_power_dbfs(&iq).to_vec();
            let peak_db = power[size / 2 + k];
            // The whole point of coherent-gain normalization: the peak level
            // of an integer-bin tone is independent of the window shape.
            assert!(
                peak_db.abs() < 0.05,
                "{window:?}: peak {peak_db} dBFS should be ~0 dBFS (<0.05 dB)"
            );
        }
    }

    /// PSD mode divides |X[k]|² by `N · sum(window²)`. For a unit-amplitude
    /// complex exponential at an integer bin, |X[k]|² = N² (coherent sum),
    /// so the peak reads `10·log10(N² / (N · win_power))`. With a rectangular
    /// window `win_power = N`, that collapses to 0 dBFS. The meaningful
    /// difference from coherent-gain mode shows up for *noise*, where PSD
    /// keeps the total noise power independent of N.
    #[test]
    fn psd_mode_tone_matches_theory() {
        let size = 128;
        let k = 9;
        let two_pi = 2.0 * std::f32::consts::PI;
        let mut iq = vec![0.0f32; size * 2];
        for n in 0..size {
            let phase = two_pi * k as f32 * n as f32 / size as f32;
            iq[n * 2] = phase.cos();
            iq[n * 2 + 1] = phase.sin();
        }
        let mut spec = Spectrum::new(size);
        spec.set_window(Window::Rectangular);
        spec.set_normalization(Normalization::Psd);
        let power = spec.compute_power_dbfs(&iq).to_vec();
        let peak_idx = size / 2 + k;
        // |X[k]|² = N², win_power = N → N²/(N·N) = 1 → 0 dBFS.
        let win_power = size as f32; // rectangular
        let expected_db = 10.0 * ((size as f32).powi(2) / (size as f32 * win_power)).log10();
        let got_db = power[peak_idx];
        assert!(
            (got_db - expected_db).abs() < 0.05,
            "PSD peak {got_db} dB vs theoretical {expected_db} dB (>0.05 dB error)"
        );
    }

    /// PSD vs coherent-gain differ in how they normalize. For a tone the two
    /// modes agree (a tone is coherent), but for white noise PSD mode preserves
    /// total power across FFT sizes. Rather than wrestle with stochastic noise
    /// integration, directly verify the normalization factor `1/(N·win_power)`
    /// is applied: feed a unit tone and check the peak scales exactly as
    /// `(sum w)² / (N · sum(w²))` predicts.
    #[test]
    fn psd_normalization_factor_matches_formula() {
        for &size in &[64usize, 256] {
            let k = 3;
            let two_pi = 2.0 * std::f32::consts::PI;
            let mut iq = vec![0.0f32; size * 2];
            for n in 0..size {
                let phase = two_pi * k as f32 * n as f32 / size as f32;
                iq[n * 2] = phase.cos();
                iq[n * 2 + 1] = phase.sin();
            }
            let mut spec = Spectrum::new(size);
            spec.set_window(Window::Hann);
            spec.set_normalization(Normalization::Psd);
            let power = spec.compute_power_dbfs(&iq).to_vec();
            let peak_idx = size / 2 + k;
            // |X[k]|² for a unit tone under window w is (sum w)²; PSD divides
            // by N·sum(w²). So peak_lin = (sum w)² / (N · sum(w²)).
            let (win_power, win_sum) = Spectrum::window_stats(&spec.window);
            let expected_lin = win_sum * win_sum / (size as f32 * win_power);
            let expected_db = 10.0 * expected_lin.log10();
            let got_db = power[peak_idx];
            assert!(
                (got_db - expected_db).abs() < 0.05,
                "size={size}: PSD peak {got_db} dB vs formula {expected_db} dB"
            );
        }
    }
}
