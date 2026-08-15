//! IQ sample formats, blocks, and stream configuration.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use crate::error::SdrError;

/// The on-the-wire sample format produced by an SDR stream.
///
/// RTL-SDR delivers [`IqFormat::Cu8`] natively (interleaved unsigned 8-bit
/// pairs centered at ~127). Other formats are produced by conversion
/// ([`sdr_fox_simd`](https://docs.rs/sdr-fox-simd)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IqFormat {
    /// Interleaved unsigned 8-bit IQ, 2 bytes/sample. Native RTL-SDR.
    Cu8,
    /// Interleaved signed 8-bit IQ, 2 bytes/sample.
    Cs8,
    /// Interleaved signed little-endian 16-bit IQ, 4 bytes/sample.
    Cs16,
    /// Interleaved single-precision float IQ in roughly [-1, 1], 8 bytes/sample.
    Cf32,
}

impl IqFormat {
    /// Bytes per complex sample for this format.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            IqFormat::Cu8 | IqFormat::Cs8 => 2,
            IqFormat::Cs16 => 4,
            IqFormat::Cf32 => 8,
        }
    }
}

impl fmt::Display for IqFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IqFormat::Cu8 => "cu8",
            IqFormat::Cs8 => "cs8",
            IqFormat::Cs16 => "cs16",
            IqFormat::Cf32 => "cf32",
        })
    }
}

impl FromStr for IqFormat {
    type Err = SdrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("cu8") || s.eq_ignore_ascii_case("rtlsdr") {
            Ok(IqFormat::Cu8)
        } else if s.eq_ignore_ascii_case("cs8") {
            Ok(IqFormat::Cs8)
        } else if s.eq_ignore_ascii_case("cs16") {
            Ok(IqFormat::Cs16)
        } else if s.eq_ignore_ascii_case("cf32")
            || s.eq_ignore_ascii_case("cfloat")
            || s.eq_ignore_ascii_case("float")
        {
            Ok(IqFormat::Cf32)
        } else {
            Err(SdrError::InvalidParameter(format!(
                "unknown IQ format '{s}' (expected cu8, cs8, cs16, or cf32)"
            )))
        }
    }
}

/// Owned IQ samples in one of the supported formats.
#[derive(Debug, Clone)]
pub enum IqSamples {
    /// Cu8 backing store.
    Cu8(Vec<u8>),
    /// Cs8 backing store (interleaved bytes).
    Cs8(Vec<i8>),
    /// Cs16 backing store (interleaved, little-endian on the wire; here native).
    Cs16(Vec<i16>),
    /// Cf32 backing store (interleaved re, im).
    Cf32(Vec<f32>),
}

impl IqSamples {
    /// Number of complex samples (not bytes).
    #[must_use]
    pub fn complex_count(&self) -> usize {
        match self {
            IqSamples::Cu8(v) => v.len() / 2,
            IqSamples::Cs8(v) => v.len() / 2,
            IqSamples::Cs16(v) => v.len() / 2,
            IqSamples::Cf32(v) => v.len() / 2,
        }
    }

    /// The format of these samples.
    #[must_use]
    pub fn format(&self) -> IqFormat {
        match self {
            IqSamples::Cu8(_) => IqFormat::Cu8,
            IqSamples::Cs8(_) => IqFormat::Cs8,
            IqSamples::Cs16(_) => IqFormat::Cs16,
            IqSamples::Cf32(_) => IqFormat::Cf32,
        }
    }
}

/// A single delivered block from a stream.
#[derive(Debug, Clone)]
pub struct IqBlock {
    /// The samples in this block.
    pub samples: IqSamples,
    /// Monotonic count of samples dropped due to consumer stalls, since start.
    pub dropped: u64,
    /// Monotonic block sequence number, starting at 0.
    pub sequence: u64,
    /// Wall-clock receive time of the block, if available.
    pub timestamp: Option<Instant>,
    /// Number of raw ADC-domain samples at or beyond the ADC rails among the
    /// raw samples this block was produced from, counted **before any
    /// filtering or conversion**.
    ///
    /// **Per-block, not monotonic**: unlike the sibling `dropped` and
    /// `sequence` fields (which accumulate since stream start), this counts
    /// only the current block and starts from zero for every delivered block.
    /// Treating it as cumulative would silently break overload-AGC consumers.
    /// Producers that cannot observe the raw ADC domain leave it 0.
    pub clips: u64,
    /// Number of raw ADC-domain samples this block was produced from, counted
    /// **before any filtering or decimation** (so it can exceed the delivered
    /// complex-sample count).
    ///
    /// **Per-block, not monotonic** — see `clips`, whose denominator this is:
    /// `clips as f64 / raw_samples as f64` is the block's clip fraction.
    /// Producers that do not report raw-domain telemetry leave it 0.
    pub raw_samples: u64,
}

/// How a stream should be configured when started.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Output sample format. Device wrappers may convert on the consumer side
    /// after the transport worker delivers a native wire-format block.
    pub format: IqFormat,
    /// Number of in-flight USB transfers (the ring depth). Default 16.
    pub buffer_count: usize,
    /// Size in bytes of each transfer buffer. Must be a multiple of 512.
    /// Default `65_536`.
    pub buffer_size: usize,
    /// Depth of the bounded delivery channel (backpressure). Default 32.
    pub queue_depth: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            format: IqFormat::Cu8,
            buffer_count: 16,
            buffer_size: 65_536,
            queue_depth: 32,
        }
    }
}

/// Cloneable, thread-safe cancellation capability for a running stream.
///
/// The callback must capture only state that is independently safe to access
/// while [`StreamSink::recv`] holds its exclusive borrow. This deliberately
/// separates cancellation from the non-`Sync` stream object: foreign-language
/// adapters can retain this handle before moving the stream to a blocking
/// receiver thread, then wake that thread without aliasing the stream itself.
#[derive(Clone)]
pub struct StreamStopHandle {
    stop: Arc<dyn Fn() + Send + Sync + 'static>,
}

impl StreamStopHandle {
    /// Create a cancellation handle from a thread-safe, idempotent callback.
    #[must_use]
    pub fn new(stop: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            stop: Arc::new(stop),
        }
    }

    /// Request stream cancellation. Calling this repeatedly is harmless.
    pub fn stop(&self) {
        (self.stop)();
    }
}

impl fmt::Debug for StreamStopHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamStopHandle").finish_non_exhaustive()
    }
}

/// The object-safe sink a running stream delivers blocks through.
///
/// The concrete type lives in `sdr-fox-transport` (it owns the channel and
/// worker thread); core defines only this trait so that `Transport` and
/// `SdrDevice` can return a `Box<dyn StreamSink>` without pulling
/// channel/threading dependencies into core.
pub trait StreamSink: Send {
    /// Block waiting for the next IQ block.
    ///
    /// Returns `None` when the stream has ended (cancel or device lost).
    /// Returns `Some(Err(...))` if the worker hit a fatal error.
    fn recv(&mut self) -> Option<Result<IqBlock, crate::error::SdrError>>;

    /// Wait until `deadline` for the next IQ block.
    ///
    /// `Some(Err(SdrError::Timeout))` means no block arrived before the
    /// deadline and leaves the stream usable. `None` means the stream ended.
    /// Implementations must not consume a pending terminal error on timeout.
    fn recv_deadline(
        &mut self,
        deadline: Instant,
    ) -> Option<Result<IqBlock, crate::error::SdrError>>;

    /// Obtain an independently synchronized cancellation capability.
    ///
    /// Invoking the returned handle must promptly wake a concurrently blocked
    /// [`StreamSink::recv`] call. Implementations must not capture a reference
    /// or raw pointer to `self`; capture only atomics, channels, or other state
    /// whose synchronization is independent of the stream's mutable receiver.
    fn stop_handle(&self) -> StreamStopHandle;

    /// Request the stream to stop. Idempotent and safe from any thread.
    fn stop(&self) {
        self.stop_handle().stop();
    }
}

/// Type alias for the boxed sink returned by streaming APIs.
pub type StreamHandle = Box<dyn StreamSink>;
