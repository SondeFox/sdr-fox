//! Durable incremental 16-bit PCM mono WAV writer for demodulated audio.
//!
//! Audio is streamed in bounded blocks. Header sizes are checkpointed during
//! long captures and patched best-effort on drop, so ordinary early returns
//! and unwinding leave a playable prefix instead of a zero-length header.

use std::fs::File;
use std::io::{BufWriter, Error, ErrorKind, Result, Write};
#[cfg(not(unix))]
use std::io::{Seek, SeekFrom};
use std::path::Path;

const BYTES_PER_SAMPLE: u64 = 2;
const RIFF_SIZE_WITHOUT_DATA: u64 = 36;
const HEADER_CHECKPOINT_BYTES: u64 = 1024 * 1024;
const WRITER_BUFFER_BYTES: usize = 256 * 1024;

/// Write f32 audio samples (assumed in [-1, 1]) as 16-bit PCM mono WAV.
pub fn write_wav<P: AsRef<Path>>(path: P, samples: &[f32], sample_rate: u32) -> Result<()> {
    let mut writer = WavWriter::create(path, sample_rate)?;
    writer.write_samples(samples)?;
    writer.finish()
}

/// Incremental 16-bit PCM mono WAV writer.
///
/// [`checkpoint`](Self::checkpoint) publishes a valid header while capture is
/// still active. Large captures checkpoint automatically, and
/// [`finish`](Self::finish) reports final flush/header errors to the caller.
pub struct WavWriter {
    writer: BufWriter<File>,
    samples_written: u64,
    checkpointed_samples: u64,
    pcm_scratch: Vec<u8>,
    finished: bool,
}

impl WavWriter {
    /// Create a WAV file and write its placeholder header.
    pub fn create<P: AsRef<Path>>(path: P, sample_rate: u32) -> Result<Self> {
        validate_sample_rate(sample_rate)?;
        let mut writer = BufWriter::with_capacity(WRITER_BUFFER_BYTES, File::create(path)?);
        write_header(&mut writer, sample_rate)?;
        Ok(Self {
            writer,
            samples_written: 0,
            checkpointed_samples: 0,
            pcm_scratch: Vec::new(),
            finished: false,
        })
    }

    /// Return the number of complete PCM samples accepted by the writer.
    #[must_use]
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// Append one block of normalized floating-point audio.
    pub fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        let block_samples = u64::try_from(samples.len()).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "WAV block sample count does not fit u64",
            )
        })?;
        let new_sample_count = self
            .samples_written
            .checked_add(block_samples)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "WAV sample-count overflow"))?;

        // Validate the final RIFF and data lengths before allocating or
        // writing any part of this block.
        wav_sizes(new_sample_count)?;
        let scratch_bytes = samples
            .len()
            .checked_mul(BYTES_PER_SAMPLE as usize)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "WAV PCM block-size overflow"))?;
        self.pcm_scratch.resize(scratch_bytes, 0);

        // Quantize into one byte buffer so the output path performs a bounded
        // buffered write per block instead of a two-byte write per sample.
        for (&sample, bytes) in samples.iter().zip(self.pcm_scratch.chunks_exact_mut(2)) {
            let scaled = sample.clamp(-1.0, 1.0) * 32_767.0;
            let value = (scaled + 0.5f32.copysign(scaled)) as i16;
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        self.writer.write_all(&self.pcm_scratch)?;

        // Advance only after the complete block was accepted. This prevents a
        // failed write from advertising samples that were never returned as a
        // successful append.
        self.samples_written = new_sample_count;
        let uncheckpointed_bytes = (self.samples_written - self.checkpointed_samples)
            .checked_mul(BYTES_PER_SAMPLE)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "WAV checkpoint overflow"))?;
        if uncheckpointed_bytes >= HEADER_CHECKPOINT_BYTES {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Flush all PCM data and publish the current RIFF/data lengths.
    ///
    /// Header writes are positional on Unix and preserve the append cursor on
    /// other platforms, so capture can continue after a checkpoint.
    pub fn checkpoint(&mut self) -> Result<()> {
        let sizes = wav_sizes(self.samples_written)?;
        self.writer.flush()?;
        patch_header(self.writer.get_mut(), sizes)?;
        self.checkpointed_samples = self.samples_written;
        Ok(())
    }

    /// Flush, patch the final header, and close the writer.
    pub fn finish(mut self) -> Result<()> {
        self.checkpoint()?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        if !self.finished {
            // Drop cannot report I/O failures. Keep the operation idempotent
            // and best-effort; callers needing error reporting use finish().
            let _ = self.checkpoint();
            self.finished = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WavSizes {
    riff_size: u32,
    data_bytes: u32,
}

fn wav_sizes(samples: u64) -> Result<WavSizes> {
    let data_bytes = samples
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "WAV data-size overflow"))?;
    let riff_size = RIFF_SIZE_WITHOUT_DATA
        .checked_add(data_bytes)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "WAV RIFF-size overflow"))?;
    Ok(WavSizes {
        data_bytes: u32::try_from(data_bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "WAV exceeds RIFF data-size limit"))?,
        riff_size: u32::try_from(riff_size)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "WAV exceeds RIFF size limit"))?,
    })
}

fn validate_sample_rate(sample_rate: u32) -> Result<u32> {
    if sample_rate == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "WAV sample rate must be non-zero",
        ));
    }
    sample_rate
        .checked_mul(BYTES_PER_SAMPLE as u32)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "WAV byte rate exceeds the 32-bit header field",
            )
        })
}

fn write_header(writer: &mut impl Write, sample_rate: u32) -> Result<()> {
    let byte_rate = validate_sample_rate(sample_rate)?;
    let sizes = wav_sizes(0)?;

    writer.write_all(b"RIFF")?;
    writer.write_all(&sizes.riff_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;

    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    writer.write_all(&1u16.to_le_bytes())?; // PCM
    writer.write_all(&1u16.to_le_bytes())?; // mono
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&2u16.to_le_bytes())?; // block align
    writer.write_all(&16u16.to_le_bytes())?; // bits per sample

    // Data chunk header; sizes are checkpointed as audio is appended.
    writer.write_all(b"data")?;
    writer.write_all(&sizes.data_bytes.to_le_bytes())
}

#[cfg(unix)]
fn patch_header(file: &mut File, sizes: WavSizes) -> Result<()> {
    use std::os::unix::fs::FileExt;

    file.write_all_at(&sizes.riff_size.to_le_bytes(), 4)?;
    file.write_all_at(&sizes.data_bytes.to_le_bytes(), 40)
}

#[cfg(not(unix))]
fn patch_header(file: &mut File, sizes: WavSizes) -> Result<()> {
    let append_position = file.stream_position()?;
    let patch_result = (|| {
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&sizes.riff_size.to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&sizes.data_bytes.to_le_bytes())
    })();
    let restore_result = file.seek(SeekFrom::Start(append_position)).map(|_| ());
    match patch_result {
        Ok(()) => restore_result,
        Err(error) => {
            let _ = restore_result;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> std::path::PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sdrfox_wav_{}_{}.wav", std::process::id(), id))
    }

    fn header_sizes(path: &Path) -> (u32, u32) {
        let bytes = std::fs::read(path).unwrap();
        (
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
        )
    }

    #[test]
    fn one_shot_wav_is_well_formed_and_rounds_samples() {
        let path = temp_path();
        write_wav(&path, &[0.0, 0.5, -0.5, 1.0, -1.0], 48_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(header_sizes(&path), (46, 10));
        assert_eq!(i16::from_le_bytes(bytes[44..46].try_into().unwrap()), 0);
        assert_eq!(
            i16::from_le_bytes(bytes[46..48].try_into().unwrap()),
            16_384
        );
        assert_eq!(
            i16::from_le_bytes(bytes[48..50].try_into().unwrap()),
            -16_384
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn checkpoint_publishes_a_live_header() {
        let path = temp_path();
        let mut writer = WavWriter::create(&path, 48_000).unwrap();
        writer.write_samples(&[0.0, 0.25, -0.25]).unwrap();
        assert_eq!(writer.samples_written(), 3);
        writer.checkpoint().unwrap();
        assert_eq!(header_sizes(&path), (42, 6));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 50);
        writer.finish().unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn automatic_progress_checkpoint_updates_long_capture() {
        let path = temp_path();
        let sample_count = (HEADER_CHECKPOINT_BYTES / BYTES_PER_SAMPLE) as usize;
        let mut writer = WavWriter::create(&path, 48_000).unwrap();
        writer.write_samples(&vec![0.0; sample_count]).unwrap();
        assert_eq!(
            header_sizes(&path),
            (
                36 + HEADER_CHECKPOINT_BYTES as u32,
                HEADER_CHECKPOINT_BYTES as u32
            )
        );
        writer.finish().unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn checkpoint_preserves_append_position() {
        let path = temp_path();
        let mut writer = WavWriter::create(&path, 48_000).unwrap();
        writer.write_samples(&[0.0, 0.25]).unwrap();
        writer.checkpoint().unwrap();
        writer.checkpoint().unwrap(); // repeated patching is idempotent
        writer.write_samples(&[-0.25, 1.0, -1.0]).unwrap();
        writer.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 54);
        assert_eq!(header_sizes(&path), (46, 10));
        assert_eq!(
            i16::from_le_bytes(bytes[48..50].try_into().unwrap()),
            -8_192
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn drop_patches_header_on_early_return() {
        fn capture_prefix(path: &Path) -> Result<()> {
            let mut writer = WavWriter::create(path, 8_000)?;
            writer.write_samples(&[0.0, 0.5, -0.5])?;
            Ok(())
        }

        let path = temp_path();
        capture_prefix(&path).unwrap();
        assert_eq!(header_sizes(&path), (42, 6));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn drop_patches_header_during_unwind() {
        let path = temp_path();
        let unwind_path = path.clone();
        let result = std::panic::catch_unwind(move || {
            let mut writer = WavWriter::create(&unwind_path, 8_000).unwrap();
            writer.write_samples(&[0.0, 0.5]).unwrap();
            panic!("exercise WavWriter drop during unwinding");
        });
        assert!(result.is_err());
        assert_eq!(header_sizes(&path), (40, 4));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn explicit_finish_patches_multiblock_lengths() {
        let path = temp_path();
        let mut writer = WavWriter::create(&path, 48_000).unwrap();
        writer.write_samples(&[0.0, 0.25]).unwrap();
        writer.write_samples(&[-0.25, 1.0, -1.0]).unwrap();
        writer.finish().unwrap();
        assert_eq!(header_sizes(&path), (46, 10));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn riff_boundary_math_rejects_before_overflow() {
        let max_samples = (u64::from(u32::MAX) - RIFF_SIZE_WITHOUT_DATA) / BYTES_PER_SAMPLE;
        assert_eq!(
            wav_sizes(max_samples).unwrap(),
            WavSizes {
                riff_size: u32::MAX - 1,
                data_bytes: u32::MAX - 37,
            }
        );
        assert_eq!(
            wav_sizes(max_samples + 1).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_sample_rate(u32::MAX).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}
