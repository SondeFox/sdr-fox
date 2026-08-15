//! Streaming greyscale PNG writer for spectrogram artifacts, plus a CSV
//! power-sweep writer.
//!
//! PNG rows are filtered adaptively and compressed into one zlib stream. The
//! compressed stream is split into bounded IDAT chunks, so writer memory is
//! proportional to one image row rather than the full waterfall.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs::File;
use std::io::{BufWriter, Error, ErrorKind, Result, Write};
use std::path::Path;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_MAX_DIMENSION: u32 = i32::MAX as u32;
const IDAT_CHUNK_BYTES: usize = 64 * 1024;

/// Incremental 8-bit greyscale PNG writer.
///
/// The final dimensions are known at creation time, but rows may be supplied
/// in any number of batches. Call [`finish`](Self::finish) after writing
/// exactly `height` rows.
pub struct PngWriter {
    encoder: ZlibEncoder<IdatWriter<BufWriter<File>>>,
    width: usize,
    height: u32,
    rows_written: u32,
    previous_row: Vec<u8>,
    filtered_row: Vec<u8>,
}

impl PngWriter {
    /// Create a streaming PNG with fixed, non-zero dimensions.
    pub fn create<P: AsRef<Path>>(path: P, width: u32, height: u32) -> Result<Self> {
        let width_usize = validate_dimensions(width, height)?;
        let height_usize = usize::try_from(height).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "PNG height does not fit this platform",
            )
        })?;
        checked_pixel_count(width_usize, height_usize)?;
        let mut output = BufWriter::new(File::create(path)?);
        output.write_all(&PNG_MAGIC)?;

        let mut ihdr = [0u8; 13];
        ihdr[..4].copy_from_slice(&width.to_be_bytes());
        ihdr[4..8].copy_from_slice(&height.to_be_bytes());
        ihdr[8] = 8; // bit depth
        ihdr[9] = 0; // colour type: greyscale
        ihdr[10] = 0; // compression: deflate
        ihdr[11] = 0; // adaptive row filtering
        ihdr[12] = 0; // no interlace
        write_chunk(&mut output, b"IHDR", &ihdr)?;

        Ok(Self {
            // Level 1 keeps artifact generation off the real-time path while
            // still collapsing repetitive waterfall rows very effectively.
            encoder: ZlibEncoder::new(IdatWriter::new(output), Compression::fast()),
            width: width_usize,
            height,
            rows_written: 0,
            // Allocate row-sized buffers lazily. This lets invalid/oversized
            // inputs fail before attempting a very large allocation.
            previous_row: Vec::new(),
            filtered_row: Vec::new(),
        })
    }

    /// Compress one or more complete, tightly packed greyscale rows.
    pub fn write_rows(&mut self, pixels: &[u8]) -> Result<()> {
        if pixels.len() % self.width != 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "PNG row batch is not a multiple of the image width",
            ));
        }

        let batch_rows = pixels.len() / self.width;
        let batch_rows = u32::try_from(batch_rows).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "PNG row batch exceeds the format row-count limit",
            )
        })?;
        let end_row = self
            .rows_written
            .checked_add(batch_rows)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "PNG row count overflow"))?;
        if end_row > self.height {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "PNG row batch exceeds the declared image height",
            ));
        }

        if batch_rows == 0 {
            return Ok(());
        }
        if self.previous_row.is_empty() {
            let filtered_len = self.width.checked_add(1).ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, "PNG filtered-row size overflow")
            })?;
            try_reserve_bytes(&mut self.previous_row, self.width, "previous PNG row")?;
            try_reserve_bytes(&mut self.filtered_row, filtered_len, "filtered PNG row")?;
            self.previous_row.resize(self.width, 0);
            self.filtered_row.resize(filtered_len, 0);
        }

        for row in pixels.chunks_exact(self.width) {
            let filter = choose_filter(row, &self.previous_row);
            self.filtered_row[0] = filter as u8;
            apply_filter(filter, row, &self.previous_row, &mut self.filtered_row[1..]);
            self.encoder.write_all(&self.filtered_row)?;
            self.previous_row.copy_from_slice(row);
            self.rows_written += 1;
        }
        debug_assert_eq!(self.rows_written, end_row);
        Ok(())
    }

    /// Finish the zlib stream, write IEND, and flush the file.
    pub fn finish(self) -> Result<()> {
        if self.rows_written != self.height {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "PNG has {} rows, but {} were declared",
                    self.rows_written, self.height
                ),
            ));
        }

        let idat = self.encoder.finish()?;
        let mut output = idat.finish()?;
        write_chunk(&mut output, b"IEND", &[])?;
        output.flush()
    }
}

fn try_reserve_bytes(buffer: &mut Vec<u8>, additional: usize, label: &str) -> Result<()> {
    buffer.try_reserve_exact(additional).map_err(|error| {
        Error::new(
            ErrorKind::OutOfMemory,
            format!("cannot allocate {label}: {error}"),
        )
    })
}

/// Write a complete greyscale PNG from a row-major pixel slice.
///
/// This compatibility helper delegates to [`PngWriter`], so it does not make
/// another full-image copy.
pub fn write_greyscale_png<P: AsRef<Path>>(
    path: P,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<()> {
    let width_usize = validate_dimensions(width, height)?;
    let height_usize = usize::try_from(height).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "PNG height does not fit this platform",
        )
    })?;
    let expected = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "PNG pixel-count overflow"))?;
    if pixels.len() != expected {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "pixel buffer must contain exactly width * height bytes",
        ));
    }

    let mut writer = PngWriter::create(path, width, height)?;
    writer.write_rows(pixels)?;
    writer.finish()
}

fn validate_dimensions(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "PNG dimensions must be non-zero",
        ));
    }
    // PNG encoders/decoders conventionally enforce the specification's
    // signed-31-bit dimension ceiling. Rejecting it here also avoids lossy
    // arithmetic on 32-bit hosts.
    if width > PNG_MAX_DIMENSION || height > PNG_MAX_DIMENSION {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "PNG dimensions must not exceed 2^31 - 1",
        ));
    }
    let width = usize::try_from(width).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "PNG width does not fit this platform",
        )
    })?;
    width
        .checked_add(1)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "PNG row-size overflow"))?;
    Ok(width)
}

fn checked_pixel_count(width: usize, height: usize) -> Result<usize> {
    width
        .checked_mul(height)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "PNG pixel-count overflow"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum RowFilter {
    None = 0,
    Sub = 1,
    Up = 2,
    Average = 3,
    Paeth = 4,
}

fn choose_filter(row: &[u8], previous: &[u8]) -> RowFilter {
    debug_assert_eq!(row.len(), previous.len());
    let filters = [
        RowFilter::None,
        RowFilter::Sub,
        RowFilter::Up,
        RowFilter::Average,
        RowFilter::Paeth,
    ];
    let mut scores = [0u64; 5];

    for i in 0..row.len() {
        let current = row[i];
        let left = if i == 0 { 0 } else { row[i - 1] };
        let up = previous[i];
        let up_left = if i == 0 { 0 } else { previous[i - 1] };
        let predictors = [
            0,
            left,
            up,
            ((u16::from(left) + u16::from(up)) / 2) as u8,
            paeth_predictor(left, up, up_left),
        ];
        for (score, predictor) in scores.iter_mut().zip(predictors) {
            let residual = current.wrapping_sub(predictor);
            *score += u64::from(i16::from(residual as i8).unsigned_abs());
        }
    }

    let best = scores
        .iter()
        .enumerate()
        .min_by_key(|(_, score)| *score)
        .map_or(0, |(index, _)| index);
    filters[best]
}

fn apply_filter(filter: RowFilter, row: &[u8], previous: &[u8], output: &mut [u8]) {
    debug_assert_eq!(row.len(), previous.len());
    debug_assert_eq!(row.len(), output.len());
    for i in 0..row.len() {
        let left = if i == 0 { 0 } else { row[i - 1] };
        let up = previous[i];
        let up_left = if i == 0 { 0 } else { previous[i - 1] };
        let predictor = match filter {
            RowFilter::None => 0,
            RowFilter::Sub => left,
            RowFilter::Up => up,
            RowFilter::Average => ((u16::from(left) + u16::from(up)) / 2) as u8,
            RowFilter::Paeth => paeth_predictor(left, up, up_left),
        };
        output[i] = row[i].wrapping_sub(predictor);
    }
}

fn paeth_predictor(left: u8, up: u8, up_left: u8) -> u8 {
    let left = i32::from(left);
    let up = i32::from(up);
    let up_left = i32::from(up_left);
    let estimate = left + up - up_left;
    let left_distance = (estimate - left).abs();
    let up_distance = (estimate - up).abs();
    let diagonal_distance = (estimate - up_left).abs();
    if left_distance <= up_distance && left_distance <= diagonal_distance {
        left as u8
    } else if up_distance <= diagonal_distance {
        up as u8
    } else {
        up_left as u8
    }
}

/// Turns a single zlib byte stream into bounded consecutive PNG IDAT chunks.
struct IdatWriter<W> {
    output: W,
    pending: Vec<u8>,
}

impl<W: Write> IdatWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            pending: Vec::with_capacity(IDAT_CHUNK_BYTES),
        }
    }

    fn flush_chunk(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            write_chunk(&mut self.output, b"IDAT", &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<W> {
        self.flush_chunk()?;
        Ok(self.output)
    }
}

impl<W: Write> Write for IdatWriter<W> {
    fn write(&mut self, input: &[u8]) -> Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        if self.pending.len() == IDAT_CHUNK_BYTES {
            self.flush_chunk()?;
        }
        let count = input.len().min(IDAT_CHUNK_BYTES - self.pending.len());
        self.pending.extend_from_slice(&input[..count]);
        if self.pending.len() == IDAT_CHUNK_BYTES {
            self.flush_chunk()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_chunk()?;
        self.output.flush()
    }
}

fn chunk_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "PNG chunk exceeds the 32-bit chunk length",
        )
    })
}

fn write_chunk<W: Write>(output: &mut W, chunk_type: &[u8; 4], data: &[u8]) -> Result<()> {
    output.write_all(&chunk_len(data.len())?.to_be_bytes())?;
    output.write_all(chunk_type)?;
    output.write_all(data)?;
    let mut crc = Crc32::new();
    crc.update(chunk_type);
    crc.update(data);
    output.write_all(&crc.finalize().to_be_bytes())
}

static CRC_TABLE: [u32; 256] = {
    const fn build() -> [u32; 256] {
        let mut table = [0u32; 256];
        let mut i = 0u32;
        while i < 256 {
            let mut crc = i;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    0xEDB8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[i as usize] = crc;
            i += 1;
        }
        table
    }
    build()
};

struct Crc32 {
    state: u32,
}

impl Crc32 {
    fn new() -> Self {
        Self { state: u32::MAX }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let index = ((self.state ^ u32::from(byte)) & 0xff) as usize;
            self.state = (self.state >> 8) ^ CRC_TABLE[index];
        }
    }

    fn finalize(self) -> u32 {
        self.state ^ u32::MAX
    }
}

/// Write a CSV power sweep (frequency, power_db) in the `rtl_power` shape.
pub fn write_power_csv<P: AsRef<Path>>(path: P, rows: &[(f64, f64)]) -> Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(output, "frequency_hz,power_db")?;
    for (frequency, power) in rows {
        writeln!(output, "{frequency},{power:.2}")?;
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_path(extension: &str) -> std::path::PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "sdrfox_png_{}_{}.{}",
            std::process::id(),
            id,
            extension
        ))
    }

    fn decode(path: &Path) -> (::png::OutputInfo, Vec<u8>) {
        let decoder = ::png::Decoder::new(BufReader::new(File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut bytes = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut bytes).unwrap();
        bytes.truncate(info.buffer_size());
        (info, bytes)
    }

    fn chunks(bytes: &[u8]) -> Vec<([u8; 4], usize)> {
        let mut result = Vec::new();
        let mut offset = PNG_MAGIC.len();
        while offset < bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let chunk_type = bytes[offset + 4..offset + 8].try_into().unwrap();
            result.push((chunk_type, length));
            offset += 12 + length;
        }
        assert_eq!(offset, bytes.len());
        result
    }

    #[test]
    fn decoder_round_trip_matches_streamed_pixels_and_dimensions() {
        let path = temp_path("png");
        let width = 19u32;
        let height = 7u32;
        let pixels: Vec<u8> = (0..width * height)
            .map(|index| index.wrapping_mul(37) as u8)
            .collect();
        let split = width as usize * 3;
        let mut writer = PngWriter::create(&path, width, height).unwrap();
        writer.write_rows(&pixels[..split]).unwrap();
        writer.write_rows(&pixels[split..]).unwrap();
        writer.finish().unwrap();

        let (info, decoded) = decode(&path);
        assert_eq!((info.width, info.height), (width, height));
        assert_eq!(info.bit_depth, ::png::BitDepth::Eight);
        assert_eq!(info.color_type, ::png::ColorType::Grayscale);
        assert_eq!(decoded, pixels);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn incompressible_image_is_split_across_bounded_idat_chunks() {
        let path = temp_path("png");
        let (width, height) = (512u32, 512u32);
        let mut state = 0x1234_5678u32;
        let pixels: Vec<u8> = (0..width * height)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        write_greyscale_png(&path, &pixels, width, height).unwrap();

        let file = std::fs::read(&path).unwrap();
        let idat_lengths: Vec<usize> = chunks(&file)
            .into_iter()
            .filter_map(|(kind, length)| (kind == *b"IDAT").then_some(length))
            .collect();
        assert!(idat_lengths.len() > 1);
        assert!(idat_lengths
            .iter()
            .all(|&length| length <= IDAT_CHUNK_BYTES));
        let (_, decoded) = decode(&path);
        assert_eq!(decoded, pixels);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn repetitive_waterfall_is_compressed() {
        let path = temp_path("png");
        let (width, height) = (1024u32, 128u32);
        let pixels = vec![93u8; width as usize * height as usize];
        write_greyscale_png(&path, &pixels, width, height).unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(file_size < pixels.len() / 16);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn repeated_row_selects_up_filter() {
        let row: Vec<u8> = (0..127).map(|value| (value * 29) as u8).collect();
        assert_eq!(choose_filter(&row, &row), RowFilter::Up);
    }

    #[test]
    fn rejects_zero_mismatched_and_excess_rows() {
        let path = temp_path("png");
        assert_eq!(
            PngWriter::create(&path, 0, 1).err().unwrap().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            write_greyscale_png(&path, &[0; 7], 4, 2)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );

        let mut writer = PngWriter::create(&path, 4, 2).unwrap();
        assert_eq!(
            writer.write_rows(&[0; 5]).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            writer.write_rows(&[0; 12]).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(writer.finish().unwrap_err().kind(), ErrorKind::InvalidInput);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_dimensions_outside_png_limit() {
        let path = temp_path("png");
        assert_eq!(
            PngWriter::create(&path, u32::MAX, 1).err().unwrap().kind(),
            ErrorKind::InvalidInput
        );
        assert!(!path.exists());
    }

    #[test]
    fn rejects_streaming_pixel_count_overflow() {
        assert_eq!(
            checked_pixel_count(usize::MAX, 2).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn row_buffer_allocation_failure_is_reported() {
        let mut buffer = Vec::new();
        assert_eq!(
            try_reserve_bytes(&mut buffer, usize::MAX, "test row")
                .unwrap_err()
                .kind(),
            ErrorKind::OutOfMemory
        );
        assert!(buffer.is_empty());
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn rejects_chunk_length_overflow() {
        assert_eq!(
            chunk_len(u32::MAX as usize + 1).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn crc32_matches_known_value() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn csv_has_header_and_rows() {
        let path = temp_path("csv");
        write_power_csv(&path, &[(100e6, -42.0), (100.1e6, -38.0)]).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("frequency_hz,power_db"));
        assert!(contents.contains("100000000,-42.00"));
        std::fs::remove_file(path).ok();
    }
}
