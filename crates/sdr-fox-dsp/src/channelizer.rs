//! Stateful CPU reference for a polyphase filter-bank channelizer.
//!
//! The implementation deliberately stays in safe scalar Rust, shaped so LLVM's
//! autovectorizer does the wide work: the filter bank walks contiguous
//! per-frame slices with the branch index innermost, and the accumulators live
//! in fixed-size register tiles. It is both a useful low-cost channelizer at
//! RTL-SDR rates and the correctness oracle for any future NEON, SVE, or GPU
//! backend.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Construction or input validation failure for [`PolyphaseChannelizer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelizerError {
    /// The branch count must be a non-zero power of two.
    InvalidBranchCount,
    /// The prototype must contain one or more complete taps per branch.
    InvalidPrototypeLength,
    /// Interleaved complex input must contain complete I/Q pairs.
    OddIqLength,
    /// A requested allocation size overflowed `usize`.
    SizeOverflow,
}

impl Display for ChannelizerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBranchCount => {
                formatter.write_str("channelizer branch count must be a non-zero power of two")
            }
            Self::InvalidPrototypeLength => formatter.write_str(
                "channelizer prototype must contain complete, finite taps for every branch",
            ),
            Self::OddIqLength => formatter.write_str("channelizer IQ input has an odd length"),
            Self::SizeOverflow => formatter.write_str("channelizer allocation size overflow"),
        }
    }
}

impl Error for ChannelizerError {}

/// Number of `f32` lanes accumulated per register tile in the filter bank.
///
/// The tile is a fixed-size array with constant trip counts so it stays in
/// registers across all tap-frames; 32 lanes is eight 128-bit NEON registers.
/// Measured on an M1 Max against the shipped branch-at-a-time kernel at
/// 256x8 (32k samples): one accumulator pair 253 us, contiguous frame slices
/// 131 us, 16-lane tiles 107 us, 32-lane tiles 95 us, 64-lane tiles 120 us
/// (register spills). Banks narrower than `TILE / 2` branches take the scalar
/// remainder path, where throughput is irrelevant.
const TILE: usize = 32;

/// An M-branch critically sampled polyphase filter bank followed by an
/// M-point FFT.
///
/// Each output frame contains one complex sample per channel in ordinary FFT
/// bin order. FIR history and an incomplete input frame persist across calls,
/// so arbitrary USB block boundaries do not change the output.
///
/// # Data layout
///
/// All filter state is stored as interleaved `[re, im]` `f32` planes rather
/// than `Complex32`, with each prototype coefficient duplicated per lane.
/// This turns the per-frame filter work into pure element-wise multiply-adds
/// over contiguous slices — the shape LLVM vectorizes — instead of the
/// branch-at-a-time form whose strided ring gathers and single accumulator
/// pair serialize on FMA latency.
pub struct PolyphaseChannelizer {
    branches: usize,
    taps_per_branch: usize,
    /// Prototype coefficients duplicated per interleaved lane: the natural
    /// order tap `tap * branches + branch` occupies lanes
    /// `tap * width + 2 * branch` and `tap * width + 2 * branch + 1`
    /// (`width = 2 * branches`), so one coefficient row multiplies one frame
    /// of interleaved IQ element-wise.
    taps_dup: Vec<f32>,
    /// Previous complete input frames in a circular frame ring, interleaved
    /// IQ; frame `f` occupies `history[f * width .. (f + 1) * width]`.
    history: Vec<f32>,
    /// Ring position of the oldest stored frame (the next slot overwritten).
    history_pos: usize,
    /// The incomplete newest frame, interleaved IQ.
    pending: Vec<f32>,
    /// Complex samples currently buffered in `pending`.
    pending_len: usize,
    /// Ring index of the frame that is `age + 1` frames old, recomputed per
    /// emitted frame; kept as state only to avoid a per-frame allocation.
    frame_order: Vec<usize>,
    fft_plan: Arc<dyn Fft<f32>>,
    fft_scratch: Vec<Complex32>,
    fft_buffer: Vec<Complex32>,
}

impl PolyphaseChannelizer {
    /// Build a Hamming-windowed sinc prototype with `taps_per_branch` taps in
    /// each of `branches` phases.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelizerError`] when `branches` is not a power of two,
    /// `taps_per_branch` is zero, or their product overflows.
    pub fn new(branches: usize, taps_per_branch: usize) -> Result<Self, ChannelizerError> {
        validate_branches(branches)?;
        if taps_per_branch == 0 {
            return Err(ChannelizerError::InvalidPrototypeLength);
        }
        let count = branches
            .checked_mul(taps_per_branch)
            .ok_or(ChannelizerError::SizeOverflow)?;
        let cutoff = 0.5 / branches as f32;
        let midpoint = (count - 1) as f32 * 0.5;
        let mut taps = Vec::with_capacity(count);
        for index in 0..count {
            let offset = index as f32 - midpoint;
            let angle = 2.0 * std::f32::consts::PI * cutoff * offset;
            let sinc = if angle.abs() < f32::EPSILON {
                1.0
            } else {
                angle.sin() / angle
            };
            let window = if count == 1 {
                1.0
            } else {
                0.54 - 0.46 * (2.0 * std::f32::consts::PI * index as f32 / (count - 1) as f32).cos()
            };
            taps.push(2.0 * cutoff * sinc * window);
        }
        let sum: f32 = taps.iter().sum();
        for tap in &mut taps {
            *tap /= sum;
        }
        Self::with_taps(branches, taps)
    }

    /// Build from a caller-supplied prototype in natural FIR order.
    /// `taps.len()` must be a non-zero multiple of `branches` and every tap
    /// must be finite.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelizerError`] for an invalid branch count or prototype.
    // The coefficients are now stored duplicated per interleaved lane rather
    // than verbatim, but the established public signature keeps ownership.
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_taps(branches: usize, taps: Vec<f32>) -> Result<Self, ChannelizerError> {
        validate_branches(branches)?;
        if taps.is_empty() || taps.len() % branches != 0 || taps.iter().any(|tap| !tap.is_finite())
        {
            return Err(ChannelizerError::InvalidPrototypeLength);
        }
        let taps_per_branch = taps.len() / branches;
        let width = branches
            .checked_mul(2)
            .ok_or(ChannelizerError::SizeOverflow)?;
        let dup_len = taps
            .len()
            .checked_mul(2)
            .ok_or(ChannelizerError::SizeOverflow)?;
        let history_len = width
            .checked_mul(taps_per_branch - 1)
            .ok_or(ChannelizerError::SizeOverflow)?;
        let mut taps_dup = Vec::with_capacity(dup_len);
        for &tap in &taps {
            taps_dup.push(tap);
            taps_dup.push(tap);
        }
        let mut planner = FftPlanner::new();
        let fft_plan = planner.plan_fft_forward(branches);
        let fft_scratch = vec![Complex32::new(0.0, 0.0); fft_plan.get_inplace_scratch_len()];
        Ok(Self {
            branches,
            taps_per_branch,
            taps_dup,
            history: vec![0.0; history_len],
            history_pos: 0,
            pending: vec![0.0; width],
            pending_len: 0,
            frame_order: vec![0; taps_per_branch - 1],
            fft_plan,
            fft_scratch,
            fft_buffer: vec![Complex32::new(0.0, 0.0); branches],
        })
    }

    /// Number of frequency channels produced per output frame.
    #[must_use]
    pub fn branches(&self) -> usize {
        self.branches
    }

    /// Number of prototype taps evaluated by each polyphase branch.
    #[must_use]
    pub fn taps_per_branch(&self) -> usize {
        self.taps_per_branch
    }

    /// Consume interleaved cf32 IQ and append complete channelizer frames to
    /// `output`, also interleaved cf32. Returns the number of frames appended.
    /// An incomplete M-sample input frame remains buffered for the next call.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelizerError::OddIqLength`] for incomplete I/Q pairs or
    /// [`ChannelizerError::SizeOverflow`] if output reservation overflows.
    pub fn process(
        &mut self,
        iq: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<usize, ChannelizerError> {
        if iq.len() % 2 != 0 {
            return Err(ChannelizerError::OddIqLength);
        }
        let complete_frames = (self.pending_len + iq.len() / 2) / self.branches;
        let output_values = complete_frames
            .checked_mul(self.branches)
            .and_then(|complex| complex.checked_mul(2))
            .ok_or(ChannelizerError::SizeOverflow)?;
        output.reserve(output_values);

        let mut frames = 0;
        let mut remaining = iq;
        while !remaining.is_empty() {
            // `remaining` stays even, so a non-empty remainder always yields
            // at least one complex sample and the loop makes progress.
            let need = self.branches - self.pending_len;
            let take = need.min(remaining.len() / 2);
            let (head, rest) = remaining.split_at(take * 2);
            let start = self.pending_len * 2;
            self.pending[start..start + head.len()].copy_from_slice(head);
            self.pending_len += take;
            remaining = rest;
            if self.pending_len == self.branches {
                self.emit_frame(output);
                self.pending_len = 0;
                frames += 1;
            }
        }
        Ok(frames)
    }

    /// Filter the completed frame in `pending` against the frame ring, FFT it
    /// in place, and append the interleaved result to `output`.
    ///
    /// Every branch accumulates its taps newest-to-oldest, matching the
    /// natural per-branch convolution order, so results are bitwise
    /// independent of how the caller chunks the stream.
    fn emit_frame(&mut self, output: &mut Vec<f32>) {
        let width = self.branches * 2;
        let history_frames = self.taps_per_branch - 1;
        // Resolve the ring wrap once per frame: frame_order[age - 1] is the
        // ring index holding the frame that is `age` frames old.
        for (slot, age) in self.frame_order.iter_mut().zip(1..) {
            *slot = (self.history_pos + history_frames - age) % history_frames;
        }

        // Main region: TILE interleaved lanes (TILE / 2 channels) at a time.
        // The accumulator tile is a fixed-size array with constant trip
        // counts, so it stays in registers across all tap-frames, and every
        // coefficient and sample load is a contiguous forward slice.
        let mut base = 0;
        let mut bin_blocks = self.fft_buffer.chunks_exact_mut(TILE / 2);
        for bins in bin_blocks.by_ref() {
            let mut tile = [0.0f32; TILE];
            let coeffs = &self.taps_dup[base..base + TILE];
            let values = &self.pending[base..base + TILE];
            for lane in 0..TILE {
                tile[lane] = coeffs[lane] * values[lane];
            }
            for (age_minus_1, &frame) in self.frame_order.iter().enumerate() {
                let coeff_base = (age_minus_1 + 1) * width + base;
                let coeffs = &self.taps_dup[coeff_base..coeff_base + TILE];
                let frame_base = frame * width + base;
                let values = &self.history[frame_base..frame_base + TILE];
                for lane in 0..TILE {
                    tile[lane] += coeffs[lane] * values[lane];
                }
            }
            for (bin, pair) in bins.iter_mut().zip(tile.chunks_exact(2)) {
                *bin = Complex32::new(pair[0], pair[1]);
            }
            base += TILE;
        }
        // Remainder: banks narrower than TILE / 2 channels. Identical math
        // and summation order, scalar accumulators.
        for (offset, bin) in bin_blocks.into_remainder().iter_mut().enumerate() {
            let lane = base + offset * 2;
            let mut re = self.taps_dup[lane] * self.pending[lane];
            let mut im = self.taps_dup[lane + 1] * self.pending[lane + 1];
            for (age_minus_1, &frame) in self.frame_order.iter().enumerate() {
                let coeff = (age_minus_1 + 1) * width + lane;
                let sample = frame * width + lane;
                re += self.taps_dup[coeff] * self.history[sample];
                im += self.taps_dup[coeff + 1] * self.history[sample + 1];
            }
            *bin = Complex32::new(re, im);
        }

        self.fft_plan
            .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);
        let start = output.len();
        output.resize(start + width, 0.0);
        for (pair, bin) in output[start..].chunks_exact_mut(2).zip(&self.fft_buffer) {
            pair[0] = bin.re;
            pair[1] = bin.im;
        }

        if history_frames != 0 {
            let start = self.history_pos * width;
            self.history[start..start + width].copy_from_slice(&self.pending);
            self.history_pos += 1;
            if self.history_pos == history_frames {
                self.history_pos = 0;
            }
        }
    }
}

fn validate_branches(branches: usize) -> Result<(), ChannelizerError> {
    if branches == 0 || !branches.is_power_of_two() {
        Err(ChannelizerError::InvalidBranchCount)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_dft(values: &[Complex32]) -> Vec<Complex32> {
        let count = values.len();
        (0..count)
            .map(|bin| {
                values
                    .iter()
                    .enumerate()
                    .fold(Complex32::new(0.0, 0.0), |sum, (index, value)| {
                        let angle =
                            -2.0 * std::f32::consts::PI * (bin * index) as f32 / count as f32;
                        sum + *value * Complex32::new(angle.cos(), angle.sin())
                    })
            })
            .collect()
    }

    #[test]
    fn one_tap_branches_match_direct_dft_oracle() {
        let branches = 4;
        let taps = vec![0.25; branches];
        let mut channelizer = PolyphaseChannelizer::with_taps(branches, taps).unwrap();
        let input = [
            Complex32::new(1.0, 0.5),
            Complex32::new(-2.0, 1.0),
            Complex32::new(0.25, -0.75),
            Complex32::new(3.0, 2.0),
        ];
        let iq: Vec<f32> = input
            .iter()
            .flat_map(|sample| [sample.re, sample.im])
            .collect();
        let mut output = Vec::new();
        assert_eq!(channelizer.process(&iq, &mut output).unwrap(), 1);

        let expected = direct_dft(
            &input
                .iter()
                .map(|sample| *sample * 0.25)
                .collect::<Vec<_>>(),
        );
        for (actual, expected) in output.chunks_exact(2).zip(expected) {
            assert!((actual[0] - expected.re).abs() < 2e-6);
            assert!((actual[1] - expected.im).abs() < 2e-6);
        }
    }

    #[test]
    fn multi_tap_history_matches_direct_convolution_and_dft() {
        let branches = 4;
        let taps_per_branch = 3;
        let taps = vec![
            0.03, -0.07, 0.11, 0.05, // newest phase taps
            -0.13, 0.17, 0.19, -0.23, // one frame old
            0.29, 0.31, -0.37, 0.41, // two frames old
        ];
        let input: Vec<Complex32> = (0..branches * 5)
            .map(|index| {
                let x = index as f32;
                Complex32::new((x * 0.37).sin(), (x * 0.23).cos())
            })
            .collect();
        let iq: Vec<f32> = input
            .iter()
            .flat_map(|sample| [sample.re, sample.im])
            .collect();
        let mut channelizer = PolyphaseChannelizer::with_taps(branches, taps.clone()).unwrap();
        let mut actual = Vec::new();
        assert_eq!(channelizer.process(&iq, &mut actual).unwrap(), 5);

        let zero = Complex32::new(0.0, 0.0);
        let mut expected = Vec::new();
        for frame in 0usize..5 {
            let mut phases = vec![zero; branches];
            for branch in 0..branches {
                for tap in 0..taps_per_branch {
                    let source_frame = frame.checked_sub(tap);
                    let sample =
                        source_frame.map_or(zero, |source| input[source * branches + branch]);
                    phases[branch] += sample * taps[tap * branches + branch];
                }
            }
            expected.extend(direct_dft(&phases));
        }

        for (index, (actual, expected)) in actual
            .chunks_exact(2)
            .map(|pair| Complex32::new(pair[0], pair[1]))
            .zip(expected)
            .enumerate()
        {
            assert!(
                (actual - expected).norm() < 3e-6,
                "frame/bin {index}: actual={actual:?}, expected={expected:?}"
            );
        }
    }

    #[test]
    fn wide_bank_matches_direct_reference_oracle() {
        // Absolute correctness for the register-tiled path (branches >= 16),
        // which the 4-branch oracle above cannot reach: every output must
        // equal the textbook per-branch convolution followed by a direct DFT.
        // Asymmetric pseudo-random taps and enough frames to wrap the history
        // ring twice catch wrong tile/remainder splits, a wrong duplicated
        // coefficient layout, or a wrong ring rotation.
        let branches = 32;
        let taps_per_branch = 5;
        let taps: Vec<f32> = (0..branches * taps_per_branch)
            .map(|index| ((index * 37 + 11) % 23) as f32 / 23.0 - 0.5)
            .collect();
        let frames = 11;
        let input: Vec<Complex32> = (0..branches * frames)
            .map(|index| {
                let x = index as f32;
                Complex32::new((x * 0.29).sin(), (x * 0.41).cos())
            })
            .collect();
        let iq: Vec<f32> = input
            .iter()
            .flat_map(|sample| [sample.re, sample.im])
            .collect();
        let mut channelizer = PolyphaseChannelizer::with_taps(branches, taps.clone()).unwrap();
        let mut actual = Vec::new();
        assert_eq!(channelizer.process(&iq, &mut actual).unwrap(), frames);

        let zero = Complex32::new(0.0, 0.0);
        let mut expected = Vec::new();
        for frame in 0..frames {
            let mut phases = vec![zero; branches];
            for branch in 0..branches {
                for tap in 0..taps_per_branch {
                    let source_frame = frame.checked_sub(tap);
                    let sample =
                        source_frame.map_or(zero, |source| input[source * branches + branch]);
                    phases[branch] += sample * taps[tap * branches + branch];
                }
            }
            expected.extend(direct_dft(&phases));
        }

        let peak = expected.iter().fold(0.0f32, |max, bin| max.max(bin.norm()));
        for (index, (actual, expected)) in actual
            .chunks_exact(2)
            .map(|pair| Complex32::new(pair[0], pair[1]))
            .zip(expected)
            .enumerate()
        {
            assert!(
                (actual - expected).norm() < 2e-5 * peak,
                "frame/bin {index}: actual={actual:?}, expected={expected:?}"
            );
        }
    }

    #[test]
    fn channelizer_is_chunk_invariant() {
        let branches = 8;
        let iq: Vec<f32> = (0..(branches * 7 + 3))
            .flat_map(|index| {
                let x = index as f32;
                [(x * 0.17).sin(), (x * 0.11).cos()]
            })
            .collect();
        let mut whole = PolyphaseChannelizer::new(branches, 6).unwrap();
        let mut expected = Vec::new();
        whole.process(&iq, &mut expected).unwrap();

        for split_complex in 0..=iq.len() / 2 {
            let mut split = PolyphaseChannelizer::new(branches, 6).unwrap();
            let mut actual = Vec::new();
            split
                .process(&iq[..split_complex * 2], &mut actual)
                .unwrap();
            split
                .process(&iq[split_complex * 2..], &mut actual)
                .unwrap();
            assert_eq!(expected, actual, "split at complex sample {split_complex}");
        }
    }

    #[test]
    fn wide_bank_streaming_survives_prime_chunks() {
        // Prime chunk sizes land the pending fill on every possible partial
        // frame residue of the tiled path; output must stay bitwise equal to
        // the whole-block run.
        let branches = 32;
        let iq: Vec<f32> = (0..(branches * 9 + 17))
            .flat_map(|index| {
                let x = index as f32;
                [(x * 0.19).sin(), (x * 0.31).cos()]
            })
            .collect();
        let mut whole = PolyphaseChannelizer::new(branches, 4).unwrap();
        let mut expected = Vec::new();
        whole.process(&iq, &mut expected).unwrap();

        for &chunk_complex in &[1usize, 3, 7, 31, 101, 257] {
            let mut streamed = PolyphaseChannelizer::new(branches, 4).unwrap();
            let mut actual = Vec::new();
            for block in iq.chunks(chunk_complex * 2) {
                streamed.process(block, &mut actual).unwrap();
            }
            assert_eq!(expected, actual, "chunk of {chunk_complex} complex samples");
        }
    }

    #[test]
    fn bin_centered_tone_lands_in_expected_channel() {
        let branches = 16;
        let expected_bin = 5;
        let frames = 16;
        let iq: Vec<f32> = (0..branches * frames)
            .flat_map(|index| {
                let angle = 2.0 * std::f32::consts::PI * expected_bin as f32 * index as f32
                    / branches as f32;
                [angle.cos(), angle.sin()]
            })
            .collect();
        let mut channelizer = PolyphaseChannelizer::new(branches, 8).unwrap();
        let mut output = Vec::new();
        channelizer.process(&iq, &mut output).unwrap();
        let final_frame = &output[output.len() - branches * 2..];
        let peak = final_frame
            .chunks_exact(2)
            .enumerate()
            .max_by(|(_, left), (_, right)| {
                let left_power = left[0] * left[0] + left[1] * left[1];
                let right_power = right[0] * right[0] + right[1] * right[1];
                left_power.total_cmp(&right_power)
            })
            .map(|(index, _)| index)
            .unwrap();
        assert_eq!(peak, expected_bin);
    }

    #[test]
    fn tone_gain_and_neighbour_rejection_are_absolute() {
        // Mathematically exact steady-state response, not just an argmax: a
        // complex exponential centred on bin k makes every branch's input
        // constant across taps, so bin k must equal the prototype's DC gain
        // (exactly 1 after normalization) and every other bin must equal the
        // prototype response at a multiple of the bin spacing — deep in the
        // stopband. A wrong tap order, tile split, or ring rotation shifts
        // energy into the neighbours and fails the rejection bound.
        let branches = 64;
        let expected_bin = 19usize;
        let frames = 24;
        let iq: Vec<f32> = (0..branches * frames)
            .flat_map(|index| {
                let angle = 2.0 * std::f32::consts::PI * expected_bin as f32 * index as f32
                    / branches as f32;
                [angle.cos(), angle.sin()]
            })
            .collect();
        let mut channelizer = PolyphaseChannelizer::new(branches, 8).unwrap();
        let mut output = Vec::new();
        channelizer.process(&iq, &mut output).unwrap();

        let final_frame = &output[output.len() - branches * 2..];
        for (bin, pair) in final_frame.chunks_exact(2).enumerate() {
            let magnitude = (pair[0] * pair[0] + pair[1] * pair[1]).sqrt();
            if bin == expected_bin {
                assert!(
                    (pair[0] - 1.0).abs() < 1e-3 && pair[1].abs() < 1e-3,
                    "centre bin should be 1+0i, got {pair:?}"
                );
            } else {
                assert!(
                    magnitude < 0.02,
                    "bin {bin} leaked {magnitude} (>{} dB)",
                    20.0 * magnitude.log10()
                );
            }
        }
    }

    #[test]
    fn validates_configuration_and_input_shape() {
        assert!(matches!(
            PolyphaseChannelizer::new(3, 8),
            Err(ChannelizerError::InvalidBranchCount)
        ));
        assert!(matches!(
            PolyphaseChannelizer::new(8, 0),
            Err(ChannelizerError::InvalidPrototypeLength)
        ));
        let mut channelizer = PolyphaseChannelizer::new(8, 4).unwrap();
        assert_eq!(
            channelizer.process(&[1.0], &mut Vec::new()),
            Err(ChannelizerError::OddIqLength)
        );
    }
}
