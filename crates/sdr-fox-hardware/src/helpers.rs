//! Shared helpers for hardware tests: device open, bounded capture, and stats.

use std::time::{Duration, Instant};

use sdr_fox_airspy::AirspyBackend;
use sdr_fox_core::{
    DeviceDescriptor, DeviceKind, IqBlock, IqSamples, SdrBackend, SdrDevice, SdrError,
    StreamConfig, StreamSink, Transport,
};
use sdr_fox_rtlsdr::RtlSdrBackend;

const BYTES_PER_CU8_SAMPLE: u64 = 2;

/// USB IDs an RTL2832U dongle may enumerate with. `0x2832` is the bare
/// chipset id; `0x2838` is the id most SDR dongles carry in EEPROM (e.g. the
/// NooElec NESDR Smart XTR enumerates as 0bda:2838). Hardware helpers must
/// try both, or a test box with only one flavour plugged in cannot open it.
pub const RTL_USB_IDS: [(u16, u16); 2] = [(0x0bda, 0x2832), (0x0bda, 0x2838)];

/// Open the transport of the first RTL-SDR found, trying each known USB id.
/// Returns the transport together with the `(vendor_id, product_id)` that
/// matched, so callers can build an accurate descriptor.
pub fn open_rtl_transport() -> Result<(Box<dyn Transport>, u16, u16), SdrError> {
    let mut last_error =
        SdrError::DeviceNotFound("no RTL-SDR present at any known USB id".to_string());
    for (vendor_id, product_id) in RTL_USB_IDS {
        match sdr_fox_transport::open_default(vendor_id, product_id, 0) {
            Ok(transport) => return Ok((transport, vendor_id, product_id)),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Open an RTL-SDR at index 0 (first known USB id that answers).
pub fn open_rtlsdr() -> Result<Box<dyn SdrDevice>, SdrError> {
    let (transport, vendor_id, product_id) = open_rtl_transport()?;
    let descriptor = DeviceDescriptor {
        vendor_id,
        product_id,
        vendor_name: None,
        product_name: None,
        serial: None,
        index: 0,
        kind: DeviceKind::RtlSdr,
    };
    RtlSdrBackend.open(&descriptor, transport)
}

/// Open an Airspy at index 0.
pub fn open_airspy() -> Result<Box<dyn SdrDevice>, SdrError> {
    let transport = sdr_fox_transport::open_default(0x1d50, 0x60a1, 0)?;
    let descriptor = DeviceDescriptor {
        vendor_id: 0x1d50,
        product_id: 0x60a1,
        vendor_name: None,
        product_name: None,
        serial: None,
        index: 0,
        kind: DeviceKind::Airspy,
    };
    AirspyBackend.open(&descriptor, transport)
}

/// Capture `seconds` of CU8 IQ. Bias-tee state is never changed implicitly.
pub fn capture_cu8(
    dev: &mut dyn SdrDevice,
    frequency_hz: u64,
    sample_rate: u32,
    seconds: u64,
) -> Result<Vec<u8>, SdrError> {
    if seconds == 0 {
        return Err(SdrError::InvalidParameter(
            "capture duration must be non-zero".to_string(),
        ));
    }
    let config = StreamConfig::default();
    capture_cu8_inner(
        dev,
        frequency_hz,
        sample_rate,
        Duration::from_secs(seconds),
        None,
        config,
    )
}

/// Capture exactly `complex_samples` CU8 samples, bounded by `timeout`.
///
/// This is intended for smoke tests that need only one small FFT frame and
/// should not retain a full second of high-rate Airspy output.
pub fn capture_cu8_samples(
    dev: &mut dyn SdrDevice,
    frequency_hz: u64,
    sample_rate: u32,
    complex_samples: usize,
    timeout: Duration,
) -> Result<Vec<u8>, SdrError> {
    if complex_samples == 0 || timeout.is_zero() {
        return Err(SdrError::InvalidParameter(
            "sample-count captures require non-zero samples and timeout".to_string(),
        ));
    }
    let complex_samples = u64::try_from(complex_samples).map_err(|_| {
        SdrError::InvalidParameter("capture sample count does not fit u64".to_string())
    })?;
    let target_bytes = checked_cu8_bytes(complex_samples)?;
    let target_bytes = usize::try_from(target_bytes).map_err(|_| {
        SdrError::InvalidParameter("capture byte count does not fit this platform".to_string())
    })?;
    let config = StreamConfig::default();
    capture_cu8_inner(
        dev,
        frequency_hz,
        sample_rate,
        timeout,
        Some(target_bytes),
        config,
    )
}

fn capture_cu8_inner(
    dev: &mut dyn SdrDevice,
    frequency_hz: u64,
    sample_rate: u32,
    duration: Duration,
    target_bytes: Option<usize>,
    config: StreamConfig,
) -> Result<Vec<u8>, SdrError> {
    if sample_rate == 0 {
        return Err(SdrError::InvalidSampleRate { rate_hz: 0 });
    }
    let actual_rate = dev.set_sample_rate(sample_rate)?;
    if actual_rate == 0 {
        return Err(SdrError::InvalidParameter(
            "device negotiated a zero sample rate".to_string(),
        ));
    }
    dev.set_frequency(frequency_hz)?;
    let deadline = checked_deadline(duration)?;
    let capacity = match target_bytes {
        Some(target) => target,
        None => cu8_capacity(
            samples_for_duration(actual_rate, duration)?,
            config.buffer_size,
        )?,
    };

    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        SdrError::InvalidParameter(format!("capture allocation failed: {error}"))
    })?;
    let mut stream = dev.start_stream(config)?;
    let result = collect_cu8(&mut *stream, deadline, target_bytes, output);
    stream.stop();
    result
}

fn collect_cu8(
    stream: &mut dyn StreamSink,
    deadline: Instant,
    target_bytes: Option<usize>,
    mut output: Vec<u8>,
) -> Result<Vec<u8>, SdrError> {
    loop {
        if target_bytes.is_some_and(|target| output.len() == target) {
            return Ok(output);
        }
        let Some(block) = recv_before(stream, deadline)? else {
            if target_bytes.is_some() {
                return Err(SdrError::Timeout);
            }
            return Ok(output);
        };
        let IqSamples::Cu8(bytes) = block.samples else {
            return Err(SdrError::InvalidParameter(
                "hardware capture expected CU8 stream output".to_string(),
            ));
        };
        if bytes.len() % BYTES_PER_CU8_SAMPLE as usize != 0 {
            return Err(SdrError::ShortTransfer {
                operation: "CU8 IQ block",
                expected: bytes.len().saturating_add(1),
                actual: bytes.len(),
            });
        }

        let append_len = target_bytes
            .map(|target| (target - output.len()).min(bytes.len()))
            .unwrap_or(bytes.len());
        append_checked(&mut output, &bytes[..append_len])?;
    }
}

/// Capture CU8 and convert it to interleaved CF32 via the SIMD dispatcher.
pub fn capture_cf32(
    dev: &mut dyn SdrDevice,
    frequency_hz: u64,
    sample_rate: u32,
    seconds: u64,
) -> Result<Vec<f32>, SdrError> {
    let cu8 = capture_cu8(dev, frequency_hz, sample_rate, seconds)?;
    cu8_to_cf32(cu8)
}

/// Capture exactly `complex_samples` and convert the frame to interleaved CF32.
pub fn capture_cf32_samples(
    dev: &mut dyn SdrDevice,
    frequency_hz: u64,
    sample_rate: u32,
    complex_samples: usize,
    timeout: Duration,
) -> Result<Vec<f32>, SdrError> {
    let cu8 = capture_cu8_samples(dev, frequency_hz, sample_rate, complex_samples, timeout)?;
    cu8_to_cf32(cu8)
}

pub(crate) fn cu8_to_cf32(cu8: Vec<u8>) -> Result<Vec<f32>, SdrError> {
    let mut cf32 = Vec::new();
    cf32.try_reserve_exact(cu8.len())
        .map_err(|error| SdrError::InvalidParameter(format!("CF32 allocation failed: {error}")))?;
    cf32.resize(cu8.len(), 0.0);
    sdr_fox_simd::cu8_to_cf32(&cu8, &mut cf32);
    Ok(cf32)
}

fn checked_cu8_bytes(complex_samples: u64) -> Result<u64, SdrError> {
    complex_samples
        .checked_mul(BYTES_PER_CU8_SAMPLE)
        .ok_or_else(|| SdrError::InvalidParameter("capture byte-count overflow".to_string()))
}

fn samples_for_duration(sample_rate: u32, duration: Duration) -> Result<u64, SdrError> {
    let numerator = u128::from(sample_rate)
        .checked_mul(duration.as_nanos())
        .ok_or_else(|| SdrError::InvalidParameter("capture sample-count overflow".to_string()))?;
    let samples = numerator.div_ceil(1_000_000_000);
    u64::try_from(samples)
        .map_err(|_| SdrError::InvalidParameter("capture sample-count exceeds u64".to_string()))
}

fn cu8_capacity(complex_samples: u64, extra_bytes: usize) -> Result<usize, SdrError> {
    let extra_bytes = u64::try_from(extra_bytes).map_err(|_| {
        SdrError::InvalidParameter("stream buffer size does not fit u64".to_string())
    })?;
    let bytes = checked_cu8_bytes(complex_samples)?
        .checked_add(extra_bytes)
        .ok_or_else(|| SdrError::InvalidParameter("capture reserve overflow".to_string()))?;
    usize::try_from(bytes).map_err(|_| {
        SdrError::InvalidParameter("capture reserve does not fit this platform".to_string())
    })
}

fn append_checked(output: &mut Vec<u8>, input: &[u8]) -> Result<(), SdrError> {
    output
        .len()
        .checked_add(input.len())
        .ok_or_else(|| SdrError::InvalidParameter("capture length overflow".to_string()))?;
    output
        .try_reserve(input.len())
        .map_err(|error| SdrError::InvalidParameter(format!("capture growth failed: {error}")))?;
    output.extend_from_slice(input);
    Ok(())
}

fn checked_deadline(duration: Duration) -> Result<Instant, SdrError> {
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| SdrError::InvalidParameter("capture deadline overflow".to_string()))
}

fn recv_before(
    stream: &mut dyn StreamSink,
    deadline: Instant,
) -> Result<Option<IqBlock>, SdrError> {
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        match stream.recv_deadline(deadline) {
            Some(Ok(block)) => return Ok(Some(block)),
            Some(Err(SdrError::Timeout)) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
            }
            Some(Err(error)) => return Err(error),
            None => return classify_stream_end(deadline, Instant::now()),
        }
    }
}

fn classify_stream_end(
    deadline: Instant,
    observed_at: Instant,
) -> Result<Option<IqBlock>, SdrError> {
    if observed_at >= deadline {
        Ok(None)
    } else {
        Err(SdrError::Transport(
            "stream ended before the capture deadline".to_string(),
        ))
    }
}

/// Throughput measured between the first and last received blocks.
#[derive(Clone, Copy, Debug)]
pub struct ThroughputMeasurement {
    /// All complex samples received, including the first baseline block.
    pub total_samples: u64,
    /// Samples received after the first block, represented by `elapsed`.
    pub measured_samples: u64,
    /// Number of received blocks.
    pub blocks: u64,
    /// Final cumulative transport drop counter.
    pub dropped: u64,
    /// Wall time between the first and last received block.
    pub elapsed: Duration,
}

impl ThroughputMeasurement {
    /// Measured complex samples per second.
    #[must_use]
    pub fn samples_per_second(self) -> f64 {
        self.measured_samples as f64 / self.elapsed.as_secs_f64()
    }
}

#[derive(Default)]
struct ThroughputAccumulator {
    total_samples: u64,
    first_block_samples: u64,
    blocks: u64,
    dropped: u64,
    first_received: Option<Instant>,
    last_received: Option<Instant>,
}

impl ThroughputAccumulator {
    fn record(
        &mut self,
        complex_samples: usize,
        dropped: u64,
        received: Instant,
    ) -> Result<(), SdrError> {
        let complex_samples = u64::try_from(complex_samples).map_err(|_| {
            SdrError::InvalidParameter("block sample count does not fit u64".to_string())
        })?;
        self.total_samples = self
            .total_samples
            .checked_add(complex_samples)
            .ok_or_else(|| SdrError::InvalidParameter("throughput sample overflow".to_string()))?;
        self.blocks = self
            .blocks
            .checked_add(1)
            .ok_or_else(|| SdrError::InvalidParameter("throughput block overflow".to_string()))?;
        if self.first_received.is_none() {
            self.first_received = Some(received);
            self.first_block_samples = complex_samples;
        }
        self.last_received = Some(received);
        self.dropped = self.dropped.max(dropped);
        Ok(())
    }

    fn finish(self) -> Result<ThroughputMeasurement, SdrError> {
        if self.blocks < 2 {
            return Err(SdrError::Timeout);
        }
        let first = self
            .first_received
            .expect("two blocks have a first timestamp");
        let last = self
            .last_received
            .expect("two blocks have a last timestamp");
        let elapsed = last.duration_since(first);
        if elapsed.is_zero() {
            return Err(SdrError::InvalidParameter(
                "throughput interval has zero duration".to_string(),
            ));
        }
        Ok(ThroughputMeasurement {
            total_samples: self.total_samples,
            measured_samples: self.total_samples - self.first_block_samples,
            blocks: self.blocks,
            dropped: self.dropped,
            elapsed,
        })
    }
}

/// Receive for a bounded interval and return a measured throughput report.
pub fn measure_throughput(
    stream: &mut dyn StreamSink,
    duration: Duration,
) -> Result<ThroughputMeasurement, SdrError> {
    let result = (|| {
        if duration.is_zero() {
            return Err(SdrError::InvalidParameter(
                "throughput duration must be non-zero".to_string(),
            ));
        }
        let deadline = checked_deadline(duration)?;
        let mut accumulator = ThroughputAccumulator::default();
        while let Some(block) = recv_before(stream, deadline)? {
            accumulator.record(block.samples.complex_count(), block.dropped, Instant::now())?;
        }
        accumulator.finish()
    })();
    stream.stop();
    result
}

/// The smallest number of distinct ADC codes a live front end produces in the
/// FM broadcast band. A dongle whose tuner is powered but whose RF section is
/// not delivering emits only converter dither — measured on the NESDR Smart
/// XTR in that state: **5** distinct codes, hugging 127/128. A live capture at
/// 91.1 MHz on the same board yields **42**. 16 sits with wide margin either
/// side.
const LIVE_FRONT_END_MIN_ADC_CODES: usize = 16;

/// Assert that `data` came from a receiver that is actually receiving, not
/// from a dongle that streams convincingly while its RF section is dead.
///
/// This failure mode is real and is *not* caught by demodulate-and-check-audio
/// assertions: FM-demodulating pure dither produces large random phase steps,
/// so its audio RMS (~0.20 measured) is the same order as a real broadcast's
/// (~0.28). Every energy-based check downstream of the discriminator therefore
/// passes on a dead front end. Counting distinct ADC codes separates the two
/// cleanly, because a dead path cannot generate code diversity at all.
///
/// Only valid where the band guarantees strong signal — the FM broadcast band.
/// Do not reuse at the top of the tuning range: a *live* 2 GHz capture on this
/// board produces only 8 distinct codes, which is genuinely close to the dead
/// case, so the check would be meaningless there.
///
/// # Panics
///
/// Panics if the capture carries too few distinct ADC codes to have come from
/// a working front end.
pub fn assert_front_end_alive(data: &[u8], context: &str) {
    let codes = cu8_distinct(data);
    assert!(
        codes >= LIVE_FRONT_END_MIN_ADC_CODES,
        "{context}: only {codes} distinct ADC codes across the {} bytes sampled from a \
         {}-byte capture — the front end is delivering dither, not signal. The tuner is \
         powered and streaming (this is not a transport fault); its RF section is dead. \
         A full power cycle (unplug, wait, replug) clears it — a warm re-open does not.",
        data.len().min(CU8_DISTINCT_SAMPLE_BYTES),
        data.len()
    );
}

/// Compute RMS of CU8 bytes centered at 128.
pub fn cu8_rms(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let sum: u64 = data
        .iter()
        .map(|&byte| {
            let centered = i16::from(byte) - 128;
            u64::from((centered * centered) as u16)
        })
        .sum();
    (sum as f64 / data.len() as f64).sqrt()
}

/// How many leading bytes [`cu8_distinct`] inspects. Code diversity saturates
/// almost immediately on a live capture, so sampling the head is enough and
/// keeps the scan off the hot path of multi-megabyte captures.
pub const CU8_DISTINCT_SAMPLE_BYTES: usize = 10_000;

/// Count distinct byte values in a CU8 buffer, over the first
/// [`CU8_DISTINCT_SAMPLE_BYTES`] bytes.
pub fn cu8_distinct(data: &[u8]) -> usize {
    let mut seen = [false; 256];
    let mut count = 0;
    for &byte in data.iter().take(CU8_DISTINCT_SAMPLE_BYTES) {
        if !seen[byte as usize] {
            seen[byte as usize] = true;
            count += 1;
        }
    }
    count
}

/// Ensure the artifacts directory exists.
pub fn ensure_artifacts_dir() -> std::io::Result<()> {
    std::fs::create_dir_all("tests/artifacts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{sample::StreamStopHandle, IqSamples};
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct ScriptedSink {
        events: VecDeque<Option<Result<IqBlock, SdrError>>>,
        stopped: Arc<AtomicBool>,
    }

    impl StreamSink for ScriptedSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            self.events.pop_front().flatten()
        }

        fn recv_deadline(&mut self, _deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            self.events.pop_front().flatten()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            let stopped = Arc::clone(&self.stopped);
            StreamStopHandle::new(move || stopped.store(true, Ordering::Release))
        }
    }

    fn scripted(
        events: impl Into<VecDeque<Option<Result<IqBlock, SdrError>>>>,
    ) -> (ScriptedSink, Arc<AtomicBool>) {
        let stopped = Arc::new(AtomicBool::new(false));
        (
            ScriptedSink {
                events: events.into(),
                stopped: Arc::clone(&stopped),
            },
            stopped,
        )
    }

    fn block(complex_samples: usize, dropped: u64) -> IqBlock {
        IqBlock {
            samples: IqSamples::Cu8(vec![128; complex_samples * 2]),
            dropped,
            sequence: 0,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        }
    }

    #[test]
    fn reserve_math_is_checked() {
        assert_eq!(cu8_capacity(10, 65_536).unwrap(), 65_556);
        assert_eq!(
            samples_for_duration(2_500_000, Duration::from_millis(1500)).unwrap(),
            3_750_000
        );
        assert_eq!(
            samples_for_duration(3, Duration::from_millis(1)).unwrap(),
            1
        );
        assert!(checked_cu8_bytes(u64::MAX).is_err());
        assert!(cu8_capacity(u64::MAX / 2, usize::MAX).is_err());
        assert!(samples_for_duration(u32::MAX, Duration::MAX).is_err());
    }

    #[test]
    fn checked_append_preserves_data() {
        let mut output = vec![1, 2];
        append_checked(&mut output, &[3, 4]).unwrap();
        assert_eq!(output, [1, 2, 3, 4]);
    }

    #[test]
    fn sample_limited_collection_returns_one_exact_frame() {
        let (mut sink, _) = scripted([Some(Ok(block(300, 0)))]);
        let output = collect_cu8(
            &mut sink,
            Instant::now() + Duration::from_secs(1),
            Some(256 * 2),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(output.len(), 256 * 2);
    }

    #[test]
    fn timed_receive_skips_timeout_and_returns_next_block() {
        let (mut sink, _) = scripted([Some(Err(SdrError::Timeout)), Some(Ok(block(4, 0)))]);
        let received = recv_before(&mut sink, Instant::now() + Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(received.samples.complex_count(), 4);
    }

    #[test]
    fn timed_receive_propagates_fatal_error_and_early_end() {
        let (mut fatal, _) = scripted([Some(Err(SdrError::DeviceLost))]);
        assert!(matches!(
            recv_before(&mut fatal, Instant::now() + Duration::from_secs(1)),
            Err(SdrError::DeviceLost)
        ));

        let (mut ended, _) = scripted([None]);
        assert!(matches!(
            recv_before(&mut ended, Instant::now() + Duration::from_secs(1)),
            Err(SdrError::Transport(_))
        ));
    }

    #[test]
    fn stream_end_at_deadline_is_clean_but_early_end_is_fatal() {
        let now = Instant::now();
        assert!(classify_stream_end(now, now).unwrap().is_none());
        assert!(matches!(
            classify_stream_end(now + Duration::from_secs(1), now),
            Err(SdrError::Transport(_))
        ));
    }

    #[test]
    fn throughput_stops_stream_when_receive_fails() {
        let (mut sink, stopped) = scripted([Some(Err(SdrError::DeviceLost))]);
        assert!(matches!(
            measure_throughput(&mut sink, Duration::from_secs(1)),
            Err(SdrError::DeviceLost)
        ));
        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn throughput_excludes_unmeasured_first_block() {
        let start = Instant::now();
        let mut accumulator = ThroughputAccumulator::default();
        accumulator.record(100, 0, start).unwrap();
        accumulator
            .record(250, 7, start + Duration::from_millis(250))
            .unwrap();
        accumulator
            .record(250, 7, start + Duration::from_millis(500))
            .unwrap();
        let measurement = accumulator.finish().unwrap();
        assert_eq!(measurement.total_samples, 600);
        assert_eq!(measurement.measured_samples, 500);
        assert_eq!(measurement.blocks, 3);
        assert_eq!(measurement.dropped, 7);
        assert_eq!(measurement.elapsed, Duration::from_millis(500));
        assert_eq!(measurement.samples_per_second(), 1_000.0);
    }
}
