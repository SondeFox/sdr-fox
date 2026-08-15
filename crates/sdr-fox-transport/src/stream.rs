//! Streaming engine: a fixed ring of N in-flight bulk transfers delivered
//! through a bounded, non-blocking channel.
//!
//! This is the design that avoids the RTL2832U FIFO-overflow footgun that
//! single-blocking-read libraries hit at 2.4 MS/s. The pattern follows
//! desperado `rs-rtl` (MIT), itself modeled on osmocom librtlsdr; the
//! workspace `NOTICE` file records the attribution. The steps:
//!
//! 1. Pre-submit `buffer_count` transfers to fill the ring.
//! 2. On each completion, attempt delivery through a bounded
//!    `crossbeam_channel` of depth `queue_depth`. A full delivery queue drops
//!    the newest block and records an explicit discontinuity; it never stops
//!    the USB worker from reaping and re-submitting the hardware ring.
//! 3. Re-submit a fresh transfer to refill the ring slot.
//! 4. Disconnect after `MAX_CONSECUTIVE_ERRORS` failed completions.
//!
//! A clonable [`StreamControl`] handle lets any thread cancel, retune, or
//! query stats mid-stream.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use sdr_fox_core::{IqBlock, IqFormat, IqSamples, SdrError};

/// Maximum consecutive transfer errors before the stream declares the device
/// lost. Matches desperado `rs-rtl`'s default.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Default deadline for one public synchronous bulk read when callers pass
/// zero. A zero timeout is never interpreted as an unbounded wait.
pub(crate) const DEFAULT_BULK_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn bulk_timeout(timeout_ms: u32) -> Duration {
    if timeout_ms == 0 {
        DEFAULT_BULK_TIMEOUT
    } else {
        Duration::from_millis(u64::from(timeout_ms))
    }
}

pub(crate) fn validate_control_len(operation: &'static str, len: usize) -> Result<u16, SdrError> {
    u16::try_from(len).map_err(|_| {
        SdrError::InvalidParameter(format!(
            "{operation} length exceeds the USB control-transfer limit of 65535 bytes"
        ))
    })
}

pub(crate) fn validate_bulk_len(operation: &'static str, len: usize) -> Result<i32, SdrError> {
    i32::try_from(len).map_err(|_| {
        SdrError::InvalidParameter(format!(
            "{operation} length exceeds the common USB backend limit of {} bytes",
            i32::MAX
        ))
    })
}

pub(crate) fn validate_stream_config(
    buffer_count: usize,
    buffer_size: usize,
    queue_depth: usize,
) -> Result<(), SdrError> {
    if buffer_count == 0 {
        return Err(SdrError::InvalidParameter(
            "buffer_count must be greater than zero".into(),
        ));
    }
    if queue_depth == 0 {
        return Err(SdrError::InvalidParameter(
            "queue_depth must be greater than zero".into(),
        ));
    }
    if buffer_size == 0 || buffer_size % 512 != 0 {
        return Err(SdrError::InvalidParameter(format!(
            "buffer_size must be a positive multiple of 512, got {buffer_size}"
        )));
    }
    if buffer_size > i32::MAX as usize {
        return Err(SdrError::InvalidParameter(format!(
            "buffer_size exceeds the USB backend limit of {} bytes",
            i32::MAX
        )));
    }
    Ok(())
}

/// RAII enforcement for the one-stream-per-transport contract. Backend clones
/// share the same atomic and move the lease into their `BufferSource`, so both
/// failed construction and normal teardown release it automatically.
pub(crate) struct StreamLease {
    busy: Arc<AtomicBool>,
}

impl StreamLease {
    pub(crate) fn acquire(busy: &Arc<AtomicBool>) -> Result<Self, SdrError> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SdrError::DeviceBusy)?;
        Ok(Self { busy: busy.clone() })
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

pub(crate) enum OperationPoll<T> {
    Complete(T),
    TimedOut,
    Stopped,
    Disconnected,
}

/// Persist one owned blocking operation across caller-side timeouts. The
/// receiver remains installed after timeout, stop, or worker disconnect so a
/// retry cannot launch a duplicate platform operation.
pub(crate) struct OwnedOperation<T> {
    receiver: Option<mpsc::Receiver<T>>,
}

impl<T> OwnedOperation<T> {
    pub(crate) fn new() -> Self {
        Self { receiver: None }
    }

    pub(crate) fn install(&mut self, receiver: mpsc::Receiver<T>) {
        assert!(
            self.receiver.is_none(),
            "cannot replace an in-progress owned operation"
        );
        self.receiver = Some(receiver);
    }

    pub(crate) fn poll_until(
        &mut self,
        deadline: Instant,
        poll_interval: Duration,
        mut stopped: impl FnMut() -> bool,
    ) -> OperationPoll<T> {
        let receiver = self
            .receiver
            .as_ref()
            .expect("owned operation polled only after installation");
        loop {
            if stopped() {
                return OperationPoll::Stopped;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return OperationPoll::TimedOut;
            }
            match receiver.recv_timeout(poll_interval.min(remaining)) {
                Ok(value) => {
                    self.receiver = None;
                    return OperationPoll::Complete(value);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return OperationPoll::Disconnected;
                }
            }
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.receiver.is_some()
    }
}

/// Leak-protect an ownership anchor while a blocking native operation is in
/// flight. The worker explicitly takes it only after the native call returns;
/// spawn failure or panic intentionally retains it.
pub(crate) struct RetainOnDrop<T> {
    value: Option<T>,
}

impl<T> RetainOnDrop<T> {
    pub(crate) fn new(value: T) -> Self {
        Self { value: Some(value) }
    }

    pub(crate) fn take(&mut self) -> T {
        self.value
            .take()
            .expect("retained ownership anchor returned exactly once")
    }

    /// Borrow the retained anchor while a blocking native operation is in
    /// flight. Only the desktop libusb clear-halt path uses this; it is gated
    /// to non-Android so the Android build (which has no libusb modules) does
    /// not flag it dead.
    #[cfg(not(target_os = "android"))]
    pub(crate) fn value(&self) -> &T {
        self.value
            .as_ref()
            .expect("retained ownership anchor is present")
    }
}

impl<T> Drop for RetainOnDrop<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            std::mem::forget(value);
        }
    }
}

/// A raw block stamped by the producer before delivery-queue contention.
struct RawBlock {
    bytes: Vec<u8>,
    sequence: u64,
    timestamp: Instant,
}

/// Clonable runtime control for a running stream. Lets any thread stop the
/// stream or query live statistics. Modeled on desperado `rs-rtl`'s
/// `AsyncReadControlHandle`.
#[derive(Clone)]
pub struct StreamControl {
    stop: Arc<AtomicBool>,
    stop_waker: Option<sdr_fox_core::sample::StreamStopHandle>,
    bytes_delivered: Arc<AtomicU64>,
    samples_dropped: Arc<AtomicU64>,
    dropped_blocks: Arc<AtomicU64>,
    failed_transfers: Arc<AtomicU64>,
    hardware_overruns_unknown: Arc<AtomicU64>,
    /// High-water mark of in-flight (queued-but-unconsumed) buffers.
    high_water_mark: Arc<AtomicU64>,
    errors: Arc<std::sync::atomic::AtomicU32>,
}

impl StreamControl {
    /// Request the stream to stop. The receiver will see the channel close.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(waker) = &self.stop_waker {
            waker.stop();
        }
    }

    /// Live streaming statistics.
    #[must_use]
    pub fn stats(&self) -> StreamStats {
        StreamStats {
            bytes_delivered: self.bytes_delivered.load(Ordering::Relaxed),
            sample_pairs_dropped_estimate: self.samples_dropped.load(Ordering::Relaxed),
            dropped_blocks: self.dropped_blocks.load(Ordering::Relaxed),
            failed_transfers: self.failed_transfers.load(Ordering::Relaxed),
            hardware_overruns_unknown: self.hardware_overruns_unknown.load(Ordering::Relaxed),
            high_water_mark: self.high_water_mark.load(Ordering::Relaxed),
            consecutive_errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// Streaming statistics reported by [`Stream`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStats {
    /// Total raw payload bytes delivered to this transport consumer. Device
    /// layers derive complex-sample counts after applying their wire format
    /// and any synthesis or decimation.
    pub bytes_delivered: u64,
    /// Compatibility estimate in raw two-byte sample pairs. This is an exact
    /// complex-sample count for CU8 IQ queue loss, but not for wire formats
    /// such as Airspy's 2× real stream. Device wrappers convert block-level
    /// loss metadata into their output sample domain; use this field only for
    /// transport diagnostics.
    pub sample_pairs_dropped_estimate: u64,
    /// Number of full blocks that were dropped.
    pub dropped_blocks: u64,
    /// Transfer completions that ended in an error status.
    pub failed_transfers: u64,
    /// Hardware overflow events whose exact lost-sample count was unavailable.
    pub hardware_overruns_unknown: u64,
    /// Maximum number of buffers that were queued (delivered-but-unconsumed)
    /// at any one time. A value approaching `queue_depth` indicates the
    /// consumer cannot keep up.
    pub high_water_mark: u64,
    /// Consecutive retryable transfer errors. A successful transfer resets it.
    pub consecutive_errors: u32,
}

/// Convert a raw CU8 IQ block to an advertised host format while preserving
/// its timestamp, sequence, and loss metadata. Blocks that are already in a
/// non-CU8 representation pass through unchanged.
#[must_use]
pub fn convert_cu8_block(block: IqBlock, format: IqFormat) -> IqBlock {
    let IqBlock {
        samples,
        dropped,
        sequence,
        timestamp,
        clips,
        raw_samples,
    } = block;
    let samples = match samples {
        IqSamples::Cu8(bytes) => match format {
            IqFormat::Cu8 => IqSamples::Cu8(bytes),
            IqFormat::Cf32 => {
                let mut out = vec![0.0f32; bytes.len()];
                sdr_fox_simd::cu8_to_cf32(&bytes, &mut out);
                IqSamples::Cf32(out)
            }
            IqFormat::Cs8 => IqSamples::Cs8(
                bytes
                    .iter()
                    .map(|&byte| i8::from_ne_bytes([byte ^ 0x80]))
                    .collect(),
            ),
            IqFormat::Cs16 => IqSamples::Cs16(
                bytes
                    .iter()
                    .map(|&byte| (i16::from(byte) - 128) << 8)
                    .collect(),
            ),
        },
        samples => samples,
    };
    IqBlock {
        samples,
        dropped,
        sequence,
        timestamp,
        // Format conversion does not change how many raw ADC samples the block
        // came from, nor how many of them were railed.
        clips,
        raw_samples,
    }
}

/// Concrete handle to a running stream. Owns the delivery channel receiver and
/// the worker join handle. Drop stops the stream and joins the worker.
///
/// Boxed behind `sdr_fox_core::StreamHandle` (`Box<dyn StreamSink>`) when
/// returned from device APIs; the concrete type is public so callers that want
/// typed access (e.g. the CLI) can downcast.
pub struct Stream {
    rx: Option<Receiver<RawBlock>>,
    control: StreamControl,
    join: Option<JoinHandle<()>>,
    /// Fatal worker error, delivered once after the data channel closes. This
    /// side channel means terminal delivery can never block behind a full data
    /// queue.
    terminal_error: Arc<Mutex<Option<SdrError>>>,
    /// The output format the consumer asked for; cu8 buffers are converted on
    /// delivery. `None` means deliver raw cu8.
    output_format: Option<IqFormat>,
}

impl Stream {
    /// Block waiting for the next IQ block.
    ///
    /// Returns `None` when the stream has ended (cancel or device lost).
    /// Returns `Some(Err(...))` if the worker hit a fatal error.
    pub fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
        let Ok(raw) = self.rx.as_ref()?.recv() else {
            return self.take_terminal_error();
        };
        Some(Ok(self.convert_raw(raw)))
    }

    /// Wait until `deadline` for the next IQ block without changing stream
    /// state when the deadline expires.
    pub fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
        let raw = match self.rx.as_ref()?.recv_deadline(deadline) {
            Ok(raw) => raw,
            Err(RecvTimeoutError::Timeout) => return Some(Err(SdrError::Timeout)),
            Err(RecvTimeoutError::Disconnected) => return self.take_terminal_error(),
        };
        Some(Ok(self.convert_raw(raw)))
    }

    fn take_terminal_error(&self) -> Option<Result<IqBlock, SdrError>> {
        let mut terminal = self
            .terminal_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        terminal.take().map(Err)
    }

    fn convert_raw(&self, raw: RawBlock) -> IqBlock {
        let RawBlock {
            bytes,
            sequence,
            timestamp,
        } = raw;
        let byte_count = bytes.len();
        self.control
            .bytes_delivered
            .fetch_add(byte_count as u64, Ordering::Relaxed);
        convert_cu8_block(
            IqBlock {
                samples: IqSamples::Cu8(bytes),
                dropped: self.control.samples_dropped.load(Ordering::Relaxed),
                sequence,
                timestamp: Some(timestamp),
                // The generic USB path carries 8-bit codes that are already
                // past the ADC, so it cannot observe rail hits in the raw
                // domain. Devices that CAN (Airspy, which sees 12-bit
                // containers before decimation) fill these in themselves; for
                // cu8 devices a consumer counts rails on the delivered bytes.
                clips: 0,
                raw_samples: 0,
            },
            self.output_format.unwrap_or(IqFormat::Cu8),
        )
    }

    /// Get a clonable control handle (retune/stop/stats from any thread).
    #[must_use]
    pub fn control_handle(&self) -> StreamControl {
        self.control.clone()
    }

    /// Current streaming statistics.
    #[must_use]
    pub fn stats(&self) -> StreamStats {
        self.control.stats()
    }

    /// Builder: request a converted output format (default is raw cu8).
    #[must_use]
    pub fn with_output_format(mut self, fmt: IqFormat) -> Self {
        self.output_format = Some(fmt);
        self
    }
}

impl sdr_fox_core::StreamSink for Stream {
    fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
        Stream::recv(self)
    }

    fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
        Stream::recv_deadline(self, deadline)
    }

    fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
        let stop = Arc::clone(&self.control.stop);
        let waker = self.control.stop_waker.clone();
        sdr_fox_core::sample::StreamStopHandle::new(move || {
            stop.store(true, Ordering::Release);
            if let Some(waker) = &waker {
                waker.stop();
            }
        })
    }

    fn stop(&self) {
        self.control.stop();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.control.stop();
        // Disconnect data delivery before joining. The worker uses only
        // `try_send`, but this also makes any in-progress delivery observe the
        // closed consumer immediately.
        drop(self.rx.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A pull-based source the worker calls to obtain each completed buffer.
/// In the nusb backend this submits a transfer and waits for completion.
/// The trait lets the streaming engine be tested with a synthetic source.
pub trait BufferSource: Send + 'static {
    /// Block until the next buffer is ready, returning its bytes.
    /// Return `Err` on a transfer failure; the engine counts errors.
    ///
    /// CONTRACT: the worker retries [`SdrError::Timeout`] indefinitely (it is
    /// how a quiet poll yields control), so a source performing internal
    /// recovery that surfaces `Timeout` MUST bound the whole recovery episode
    /// and return a terminal error — [`SdrError::RecoveryExhausted`] — once
    /// that cumulative bound expires. A source that can yield `Timeout`
    /// forever turns the stream into one that looks live but never delivers
    /// a sample and never ends.
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError>;

    /// Return source-internal counters accumulated since the previous call as
    /// `(dropped_blocks, dropped_samples, failed_transfers, unknown_overruns)`.
    /// Async backends with a queue beneath the shared delivery channel override
    /// this so public statistics cover the complete pipeline.
    fn take_stats_delta(&mut self) -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }

    /// Return true once when the most recent error reports control-path
    /// recovery progress rather than another failed sample transfer. The
    /// worker still applies retry/terminal policy, but does not fabricate an
    /// additional dropped block, failed transfer, or producer sequence gap.
    fn suppress_error_accounting(&mut self) -> bool {
        false
    }

    /// Timestamp of the buffer returned by the most recent successful
    /// [`BufferSource::next_buffer`] call. Callback-driven sources override
    /// this to report completion time rather than consumer receive time.
    fn capture_timestamp(&self) -> Option<Instant> {
        None
    }

    /// Producer-assigned sequence for the most recently returned buffer.
    /// Callback-driven rings override this so drops that happen after an older
    /// queued completion cannot be reported before that older block.
    fn capture_sequence(&self) -> Option<u64> {
        None
    }

    /// Receive the shared stop flag used by [`Stream`] cancellation. Sources
    /// that block inside [`BufferSource::next_buffer`] (e.g. the libusb async
    /// ring pumping `handle_events_timeout`) MUST poll this flag in their event
    /// loop and return `Err(SdrError::Cancelled)` when it is set — otherwise
    /// `Stream::drop`'s `join()` deadlocks waiting for a worker that is parked
    /// inside the blocking wait. The default no-op is correct for sources whose
    /// `next_buffer` already returns promptly (synthetic, synchronous bulk).
    fn set_stop(&mut self, _stop: Arc<AtomicBool>) {}

    /// Optional independent wake primitive for a source blocked in native
    /// event handling. The engine captures it before moving the source into
    /// the worker, so [`StreamControl::stop`] can wake that call immediately.
    fn stop_waker(&self) -> Option<sdr_fox_core::sample::StreamStopHandle> {
        None
    }
}

struct WorkerShared {
    stop: Arc<AtomicBool>,
    samples_dropped: Arc<AtomicU64>,
    dropped_blocks: Arc<AtomicU64>,
    failed_transfers: Arc<AtomicU64>,
    hardware_overruns_unknown: Arc<AtomicU64>,
    last_block_bytes: Arc<AtomicU64>,
    high_water_mark: Arc<AtomicU64>,
    errors: Arc<std::sync::atomic::AtomicU32>,
    terminal_error: Arc<Mutex<Option<SdrError>>>,
}

fn record_transfer_error(shared: &WorkerShared, error: &SdrError) {
    if matches!(error, SdrError::Timeout) {
        return;
    }
    shared.dropped_blocks.fetch_add(1, Ordering::Relaxed);
    let estimated = shared.last_block_bytes.load(Ordering::Relaxed) / 2;
    let dropped_samples = match error {
        SdrError::Overflow { dropped_samples } if *dropped_samples > 0 => *dropped_samples,
        _ => estimated,
    };
    shared
        .samples_dropped
        .fetch_add(dropped_samples, Ordering::Relaxed);
}

fn apply_source_stats(
    source: &mut impl BufferSource,
    shared: &WorkerShared,
) -> (u64, u64, u64, bool) {
    let (blocks, samples, failed, unknown) = source.take_stats_delta();
    if blocks != 0 {
        shared.dropped_blocks.fetch_add(blocks, Ordering::Relaxed);
    }
    if samples != 0 {
        shared.samples_dropped.fetch_add(samples, Ordering::Relaxed);
    }
    if failed != 0 {
        shared.failed_transfers.fetch_add(failed, Ordering::Relaxed);
    }
    if unknown != 0 {
        shared
            .hardware_overruns_unknown
            .fetch_add(unknown, Ordering::Relaxed);
    }
    (blocks, failed, unknown, source.suppress_error_accounting())
}

fn run_stream_worker(mut source: impl BufferSource, tx: &Sender<RawBlock>, shared: &WorkerShared) {
    let mut consecutive = 0_u32;
    let mut producer_sequence = 0_u64;
    while !shared.stop.load(Ordering::Acquire) {
        let result = source.next_buffer();
        let (
            source_dropped_blocks,
            source_failed_transfers,
            source_unknown_overruns,
            suppress_error_accounting,
        ) = apply_source_stats(&mut source, shared);
        let source_sequence = source.capture_sequence();
        if source_sequence.is_none() {
            producer_sequence = producer_sequence.wrapping_add(source_dropped_blocks);
        }

        match result {
            Ok(buf) => {
                let capture_timestamp = source.capture_timestamp().unwrap_or_else(Instant::now);
                if consecutive != 0 {
                    consecutive = 0;
                    shared.errors.store(0, Ordering::Relaxed);
                }
                shared
                    .last_block_bytes
                    .store(buf.len() as u64, Ordering::Relaxed);
                let sequence = source_sequence.unwrap_or(producer_sequence);
                let block = RawBlock {
                    bytes: buf,
                    sequence,
                    timestamp: capture_timestamp,
                };
                producer_sequence = sequence.wrapping_add(1);
                match tx.try_send(block) {
                    Ok(()) => {
                        shared
                            .high_water_mark
                            .fetch_max(tx.len() as u64, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(dropped)) => {
                        // Explicit drop-newest policy keeps the USB event loop
                        // live; producer sequence exposes the discontinuity.
                        shared.dropped_blocks.fetch_add(1, Ordering::Relaxed);
                        shared
                            .samples_dropped
                            .fetch_add((dropped.bytes.len() / 2) as u64, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(error) => {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                // Some async sources count the callback failure before
                // returning a terminal error; synchronous sources rely on the
                // shared worker to count it. Add only dimensions the source did
                // not already report, otherwise one USB failure is doubled.
                if source_dropped_blocks == 0
                    && !suppress_error_accounting
                    && !matches!(error, SdrError::Timeout)
                {
                    producer_sequence = producer_sequence.wrapping_add(1);
                    record_transfer_error(shared, &error);
                }
                // Retryability is a positive allowlist so unknown/new errors
                // default to terminal. `SdrError::RecoveryExhausted` must
                // stay OFF this list: it is itself the cumulative bound on a
                // source's recover→Timeout→retry cycle, and retrying it would
                // recreate the unbounded silent stream it exists to end.
                let retryable = matches!(
                    error,
                    SdrError::Timeout
                        | SdrError::Stall
                        | SdrError::Overflow { .. }
                        | SdrError::Transport(_)
                );
                if source_failed_transfers == 0
                    && !suppress_error_accounting
                    && !matches!(error, SdrError::Timeout)
                {
                    shared.failed_transfers.fetch_add(1, Ordering::Relaxed);
                }
                if !suppress_error_accounting
                    && source_unknown_overruns == 0
                    && matches!(error, SdrError::Overflow { dropped_samples: 0 })
                {
                    shared
                        .hardware_overruns_unknown
                        .fetch_add(1, Ordering::Relaxed);
                }
                if retryable {
                    if matches!(error, SdrError::Timeout) {
                        // A quiet poll, not a failure: retry without counting.
                        // This retry is deliberately unbounded, so the
                        // BufferSource contract requires sources to bound any
                        // internal recovery that yields Timeout — the worker
                        // cannot tell a healthy quiet poll from a wedged
                        // endpoint (see `BufferSource::next_buffer`).
                        continue;
                    }
                    consecutive += 1;
                    shared.errors.store(consecutive, Ordering::Relaxed);
                }
                if retryable && consecutive < MAX_CONSECUTIVE_ERRORS {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                *shared
                    .terminal_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                break;
            }
        }
    }
}

/// Start a streaming worker that pulls from `source` and delivers through a
/// bounded channel of depth `queue_depth`. Output format `cu8` is delivered
/// raw; pass `Some(fmt)` to convert.
///
/// This is the transport-agnostic streaming engine. The nusb backend supplies
/// a `BufferSource` impl that submits/reaps bulk transfers; the mock/test path
/// supplies a synthetic source.
#[must_use]
pub fn start_stream(
    source: impl BufferSource,
    queue_depth: usize,
    output_format: Option<IqFormat>,
) -> sdr_fox_core::StreamHandle {
    Box::new(start_stream_concrete(source, queue_depth, output_format))
}

/// Concrete (unboxed) stream constructor. Used by callers that want the typed
/// [`Stream`] (e.g. the CLI for live stats, or tests). Device APIs return the
/// boxed [`sdr_fox_core::StreamSink`] via [`start_stream`].
pub fn start_stream_concrete(
    mut source: impl BufferSource,
    queue_depth: usize,
    output_format: Option<IqFormat>,
) -> Stream {
    let (tx, rx) = bounded::<RawBlock>(queue_depth.max(1));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_waker = source.stop_waker();
    // Hand the stop flag to the source BEFORE spawning the worker. Sources with
    // a blocking event loop (libusb async ring) poll this inside next_buffer;
    // without it, Stream::drop's join() deadlocks against a parked worker.
    source.set_stop(stop.clone());
    let bytes_delivered = Arc::new(AtomicU64::new(0));
    let samples_dropped = Arc::new(AtomicU64::new(0));
    let dropped_blocks = Arc::new(AtomicU64::new(0));
    let failed_transfers = Arc::new(AtomicU64::new(0));
    let hardware_overruns_unknown = Arc::new(AtomicU64::new(0));
    let last_block_bytes = Arc::new(AtomicU64::new(0));
    let high_water_mark = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let terminal_error = Arc::new(Mutex::new(None));

    let control = StreamControl {
        stop: stop.clone(),
        stop_waker,
        bytes_delivered: bytes_delivered.clone(),
        samples_dropped: samples_dropped.clone(),
        dropped_blocks: dropped_blocks.clone(),
        failed_transfers: failed_transfers.clone(),
        hardware_overruns_unknown: hardware_overruns_unknown.clone(),
        high_water_mark: high_water_mark.clone(),
        errors: errors.clone(),
    };

    let worker = WorkerShared {
        stop: stop.clone(),
        samples_dropped,
        dropped_blocks,
        failed_transfers,
        hardware_overruns_unknown,
        last_block_bytes,
        high_water_mark,
        errors,
        terminal_error: terminal_error.clone(),
    };
    let join = thread::Builder::new()
        .name("sdr-fox-stream".into())
        .spawn(move || run_stream_worker(source, &tx, &worker))
        .expect("stream worker thread spawn");

    Stream {
        rx: Some(rx),
        control,
        join: Some(join),
        terminal_error,
        output_format,
    }
}

/// A synthetic source for tests: yields the configured buffers round-robin
/// until stopped or until an optional injected error threshold is reached.
pub struct SyntheticSource {
    buffers: Vec<Vec<u8>>,
    idx: usize,
    error_after: Option<usize>,
    count: usize,
}

impl SyntheticSource {
    /// Construct a source that yields `buffers` round-robin.
    #[must_use]
    pub fn new(buffers: Vec<Vec<u8>>) -> Self {
        Self {
            buffers,
            idx: 0,
            error_after: None,
            count: 0,
        }
    }

    /// After `n` successful pulls, every pull returns `DeviceLost`.
    #[must_use]
    pub fn error_after(mut self, n: usize) -> Self {
        self.error_after = Some(n);
        self
    }
}

impl BufferSource for SyntheticSource {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        if let Some(limit) = self.error_after {
            if self.count >= limit {
                return Err(SdrError::DeviceLost);
            }
        }
        self.count += 1;
        if self.buffers.is_empty() {
            return Err(SdrError::DeviceLost);
        }
        let buf = self.buffers[self.idx].clone();
        self.idx = (self.idx + 1) % self.buffers.len();
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    struct CountingSource {
        pulls: Arc<AtomicUsize>,
    }

    impl BufferSource for CountingSource {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            let value = self.pulls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![value as u8; 4])
        }
    }

    struct OneThenOverflow {
        first: bool,
    }

    impl BufferSource for OneThenOverflow {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            if self.first {
                self.first = false;
                Ok(vec![0; 8])
            } else {
                Err(SdrError::Overflow { dropped_samples: 7 })
            }
        }
    }

    struct OneErrorThenData {
        failed: bool,
        delivered: bool,
        stop: Option<Arc<AtomicBool>>,
    }

    struct CountedTerminalError {
        reported: bool,
    }

    struct CountedStallThenRecoveryProgress {
        step: usize,
        reported: bool,
        suppress: bool,
        stop: Option<Arc<AtomicBool>>,
    }

    struct GateThenFatal {
        release: mpsc::Receiver<()>,
    }

    /// Script steps replaying the externally observable behaviour of a
    /// stall-recovering source (e.g. `NusbBufferSource`) without hardware.
    #[derive(Clone)]
    enum ScriptStep {
        /// The initial surfaced stall report (unsuppressed accounting).
        Stall,
        /// A bounded recovery yield: `Timeout` with suppressed accounting.
        RecoveryYield,
        /// The cumulative budget expired: terminal `RecoveryExhausted` with
        /// suppressed accounting, exactly as the recovery path emits it.
        Exhausted,
        /// A successfully delivered buffer.
        Deliver(Vec<u8>),
        /// Block until the stream is stopped, then report cancellation.
        ParkUntilStop,
    }

    struct ScriptedRecoverySource {
        script: Vec<ScriptStep>,
        step: usize,
        suppress: bool,
        stop: Option<Arc<AtomicBool>>,
    }

    impl ScriptedRecoverySource {
        fn new(script: Vec<ScriptStep>) -> Self {
            Self {
                script,
                step: 0,
                suppress: false,
                stop: None,
            }
        }
    }

    impl BufferSource for ScriptedRecoverySource {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            let Some(step) = self.script.get(self.step).cloned() else {
                // Pulling past the script is itself a failure signal: a
                // terminal step must never be followed by another pull, so
                // surface a distinct error the assertions will catch.
                return Err(SdrError::DeviceLost);
            };
            match step {
                ScriptStep::ParkUntilStop => {
                    while !self
                        .stop
                        .as_ref()
                        .is_some_and(|stop| stop.load(Ordering::Acquire))
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(SdrError::Cancelled)
                }
                other => {
                    self.step += 1;
                    match other {
                        ScriptStep::Stall => Err(SdrError::Stall),
                        ScriptStep::RecoveryYield => {
                            self.suppress = true;
                            Err(SdrError::Timeout)
                        }
                        ScriptStep::Exhausted => {
                            self.suppress = true;
                            Err(SdrError::RecoveryExhausted {
                                operation: "bulk endpoint stall clear",
                                elapsed: Duration::from_secs(2),
                                attempts: 3,
                            })
                        }
                        ScriptStep::Deliver(bytes) => Ok(bytes),
                        ScriptStep::ParkUntilStop => unreachable!("handled above"),
                    }
                }
            }
        }

        fn suppress_error_accounting(&mut self) -> bool {
            std::mem::take(&mut self.suppress)
        }

        fn set_stop(&mut self, stop: Arc<AtomicBool>) {
            self.stop = Some(stop);
        }
    }

    impl BufferSource for GateThenFatal {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            self.release
                .recv()
                .expect("test must release the gated source");
            Err(SdrError::DeviceLost)
        }
    }

    impl BufferSource for CountedTerminalError {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            Err(SdrError::DeviceLost)
        }

        fn take_stats_delta(&mut self) -> (u64, u64, u64, u64) {
            if self.reported {
                (0, 0, 0, 0)
            } else {
                self.reported = true;
                (1, 4, 1, 0)
            }
        }
    }

    impl BufferSource for CountedStallThenRecoveryProgress {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            match self.step {
                0 => {
                    self.step += 1;
                    Err(SdrError::Stall)
                }
                1..=3 => {
                    self.step += 1;
                    self.suppress = true;
                    Err(SdrError::Timeout)
                }
                4 => {
                    self.step += 1;
                    Ok(vec![0; 8])
                }
                _ => {
                    while !self
                        .stop
                        .as_ref()
                        .is_some_and(|stop| stop.load(Ordering::Acquire))
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(SdrError::Cancelled)
                }
            }
        }

        fn take_stats_delta(&mut self) -> (u64, u64, u64, u64) {
            if self.reported || self.step == 0 {
                (0, 0, 0, 0)
            } else {
                self.reported = true;
                (1, 4, 1, 0)
            }
        }

        fn suppress_error_accounting(&mut self) -> bool {
            std::mem::take(&mut self.suppress)
        }

        fn set_stop(&mut self, stop: Arc<AtomicBool>) {
            self.stop = Some(stop);
        }
    }

    impl BufferSource for OneErrorThenData {
        fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
            if !self.failed {
                self.failed = true;
                return Err(SdrError::Timeout);
            }
            if !self.delivered {
                self.delivered = true;
                return Ok(vec![0; 8]);
            }
            while !self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::Acquire))
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(SdrError::Cancelled)
        }

        fn set_stop(&mut self, stop: Arc<AtomicBool>) {
            self.stop = Some(stop);
        }
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !predicate() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(predicate(), "condition was not reached before timeout");
    }

    #[test]
    fn stream_delivers_buffers_in_order() {
        let source = SyntheticSource::new(vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        let mut handle = start_stream_concrete(source, 4, None);
        let b1 = handle.recv().unwrap().unwrap();
        let b2 = handle.recv().unwrap().unwrap();
        assert_eq!(b1.sequence, 0);
        assert_eq!(b2.sequence, 1);
        let IqSamples::Cu8(bytes1) = b1.samples else {
            panic!("expected cu8");
        };
        assert_eq!(bytes1, vec![1, 2, 3, 4]);
    }

    #[test]
    fn timed_receive_is_nonterminal_and_preserves_a_later_fatal_error() {
        let (release, gated) = mpsc::channel();
        let source = GateThenFatal { release: gated };
        let mut handle = start_stream_concrete(source, 1, None);

        assert!(matches!(
            handle.recv_deadline(Instant::now()),
            Some(Err(SdrError::Timeout))
        ));

        release.send(()).unwrap();
        assert!(matches!(
            handle.recv_deadline(Instant::now() + Duration::from_secs(1)),
            Some(Err(SdrError::DeviceLost))
        ));
        assert!(handle.recv_deadline(Instant::now()).is_none());
    }

    #[test]
    fn stream_converts_to_cf32_on_request() {
        // cu8 bytes 128/127 → ~0.0039 / -0.0039.
        let source = SyntheticSource::new(vec![vec![128u8, 127]]);
        let mut handle = start_stream_concrete(source, 2, Some(IqFormat::Cf32));
        let block = handle.recv().unwrap().unwrap();
        let IqSamples::Cf32(f) = block.samples else {
            panic!("expected cf32");
        };
        assert!((f[0] - (128.0 - 127.5) / 127.5).abs() < 1e-6);
        assert!((f[1] - (127.0 - 127.5) / 127.5).abs() < 1e-6);
    }

    #[test]
    fn stream_converts_cu8_endpoints_to_cs8() {
        let source = SyntheticSource::new(vec![vec![0, 127, 128, 255]]);
        let mut handle = start_stream_concrete(source, 2, Some(IqFormat::Cs8));
        let block = handle.recv().unwrap().unwrap();
        let IqSamples::Cs8(samples) = block.samples else {
            panic!("expected cs8");
        };

        assert_eq!(samples, vec![-128, -1, 0, 127]);
    }

    #[test]
    fn stream_converts_cu8_endpoints_to_cs16() {
        let source = SyntheticSource::new(vec![vec![0, 127, 128, 255]]);
        let mut handle = start_stream_concrete(source, 2, Some(IqFormat::Cs16));
        let block = handle.recv().unwrap().unwrap();
        let IqSamples::Cs16(samples) = block.samples else {
            panic!("expected cs16");
        };

        assert_eq!(samples, vec![-32768, -256, 0, 32512]);
    }

    #[test]
    fn stream_reports_device_lost_after_consecutive_errors() {
        // Succeed twice, then fail forever.
        let source = SyntheticSource::new(vec![vec![0u8; 4]]).error_after(2);
        let mut handle = start_stream_concrete(source, 4, None);
        // Two good buffers.
        let _ = handle.recv();
        let _ = handle.recv();
        // Then errors until the limit trips; expect DeviceLost.
        let mut saw_device_lost = false;
        for _ in 0..100 {
            match handle.recv() {
                None => break,
                Some(Err(SdrError::DeviceLost)) => {
                    saw_device_lost = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_device_lost, "should have reported DeviceLost");
    }

    #[test]
    fn stop_control_ends_the_stream() {
        let source = SyntheticSource::new(vec![vec![0u8; 4]]);
        let mut handle = start_stream_concrete(source, 2, None);
        let ctrl = handle.control_handle();
        ctrl.stop();
        // Eventually recv returns None.
        let mut ended = false;
        for _ in 0..1000 {
            if handle.recv().is_none() {
                ended = true;
                break;
            }
        }
        assert!(ended, "stream should end after stop");
    }

    #[test]
    fn drop_joins_the_worker() {
        let source = SyntheticSource::new(vec![vec![0u8; 4]]);
        let handle = start_stream_concrete(source, 2, None);
        let join_ptr = handle.join.as_ref().map(|j| j.thread().id());
        drop(handle);
        // If the worker didn't join, Drop would have blocked; reaching here is success.
        let _ = join_ptr;
    }

    #[test]
    fn depth_one_saturated_queue_drops_without_shutdown_deadlock() {
        let pulls = Arc::new(AtomicUsize::new(0));
        let handle = start_stream_concrete(
            CountingSource {
                pulls: pulls.clone(),
            },
            1,
            None,
        );
        wait_until(|| pulls.load(Ordering::Relaxed) >= 2);

        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(handle);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
            "dropping a saturated depth-one stream must not wait on delivery"
        );
    }

    #[test]
    fn delivery_queue_is_bounded_and_exposes_sequence_gaps() {
        let pulls = Arc::new(AtomicUsize::new(0));
        let mut handle = start_stream_concrete(
            CountingSource {
                pulls: pulls.clone(),
            },
            1,
            None,
        );
        wait_until(|| handle.stats().dropped_blocks > 0);
        let first = handle.recv().unwrap().unwrap();
        let second = handle.recv().unwrap().unwrap();

        assert_eq!(first.sequence, 0);
        assert!(second.sequence > 1, "producer sequence must expose drops");
        assert!(handle.stats().sample_pairs_dropped_estimate >= 2);
        assert!(handle.stats().dropped_blocks >= 1);
        assert_eq!(handle.stats().high_water_mark, 1);
    }

    #[test]
    fn typed_overflow_counts_exact_samples_and_is_reported() {
        let source = OneThenOverflow { first: true };
        let mut handle = start_stream_concrete(source, 2, None);
        assert!(handle.recv().unwrap().is_ok());
        assert!(matches!(
            handle.recv(),
            Some(Err(SdrError::Overflow { dropped_samples: 7 }))
        ));
        let stats = handle.stats();
        assert_eq!(stats.dropped_blocks, u64::from(MAX_CONSECUTIVE_ERRORS));
        assert_eq!(
            stats.sample_pairs_dropped_estimate,
            7 * u64::from(MAX_CONSECUTIVE_ERRORS)
        );
        assert_eq!(stats.failed_transfers, u64::from(MAX_CONSECUTIVE_ERRORS));
        assert_eq!(stats.hardware_overruns_unknown, 0);
    }

    #[test]
    fn quiet_timeout_does_not_fabricate_a_sequence_gap() {
        let source = OneErrorThenData {
            failed: false,
            delivered: false,
            stop: None,
        };
        let mut handle = start_stream_concrete(source, 2, None);
        let first = handle.recv().unwrap().unwrap();

        assert_eq!(first.sequence, 0, "a quiet poll did not lose a block");
        assert_eq!(handle.stats().dropped_blocks, 0);
        assert_eq!(handle.stats().failed_transfers, 0);
    }

    #[test]
    fn source_counted_terminal_error_is_not_double_counted() {
        let mut handle = start_stream_concrete(CountedTerminalError { reported: false }, 2, None);
        assert!(matches!(handle.recv(), Some(Err(SdrError::DeviceLost))));
        let stats = handle.stats();
        assert_eq!(stats.dropped_blocks, 1);
        assert_eq!(stats.sample_pairs_dropped_estimate, 4);
        assert_eq!(stats.failed_transfers, 1);
    }

    #[test]
    fn recovery_poll_slices_do_not_fabricate_transfer_loss_or_sequence_gaps() {
        let source = CountedStallThenRecoveryProgress {
            step: 0,
            reported: false,
            suppress: false,
            stop: None,
        };
        let mut handle = start_stream_concrete(source, 2, None);
        let block = handle.recv().unwrap().unwrap();
        assert_eq!(block.sequence, 1, "only the original STALL creates a gap");
        let stats = handle.stats();
        assert_eq!(stats.dropped_blocks, 1);
        assert_eq!(stats.failed_transfers, 1);
        assert_eq!(stats.sample_pairs_dropped_estimate, 4);
    }

    #[test]
    fn recovery_exhaustion_is_terminal_and_ends_the_stream_promptly() {
        // A persistently stalled endpoint: one surfaced stall, bounded
        // recovery yields, then the cumulative budget expires. The stream
        // must END with the terminal error inside a bounded wait — not spin
        // silently retrying Timeout forever.
        let source = ScriptedRecoverySource::new(vec![
            ScriptStep::Stall,
            ScriptStep::RecoveryYield,
            ScriptStep::RecoveryYield,
            ScriptStep::Exhausted,
        ]);
        let mut handle = start_stream_concrete(source, 2, None);

        match handle.recv_deadline(Instant::now() + Duration::from_secs(2)) {
            Some(Err(SdrError::RecoveryExhausted {
                operation,
                attempts,
                ..
            })) => {
                assert_eq!(operation, "bulk endpoint stall clear");
                assert_eq!(attempts, 3);
            }
            other => panic!("stream must end with RecoveryExhausted, got {other:?}"),
        }
        assert!(
            handle.recv_deadline(Instant::now()).is_none(),
            "the terminal error ends the stream"
        );

        // Only the original stall is accounted; the suppressed recovery
        // yields and the exhaustion report add no fabricated loss.
        let stats = handle.stats();
        assert_eq!(stats.dropped_blocks, 1);
        assert_eq!(stats.failed_transfers, 1);
    }

    #[test]
    fn recovered_stall_keeps_streaming_and_stays_nonterminal() {
        // Two separate stall episodes that DO recover: the stream keeps
        // delivering, exposes each lost block as a sequence gap, and never
        // reports a terminal error.
        let source = ScriptedRecoverySource::new(vec![
            ScriptStep::Stall,
            ScriptStep::RecoveryYield,
            ScriptStep::Deliver(vec![1u8; 8]),
            ScriptStep::Stall,
            ScriptStep::RecoveryYield,
            ScriptStep::Deliver(vec![2u8; 8]),
            ScriptStep::ParkUntilStop,
        ]);
        let mut handle = start_stream_concrete(source, 4, None);

        let deadline = Instant::now() + Duration::from_secs(2);
        let first = handle
            .recv_deadline(deadline)
            .expect("stream is live")
            .expect("first recovered block");
        let second = handle
            .recv_deadline(deadline)
            .expect("stream is live")
            .expect("second recovered block");
        assert_eq!(first.sequence, 1, "first stall surfaces as a gap");
        assert_eq!(second.sequence, 3, "second stall surfaces as a gap");

        let stats = handle.stats();
        assert_eq!(stats.dropped_blocks, 2);
        assert_eq!(stats.failed_transfers, 2);
        assert!(
            matches!(
                handle.recv_deadline(Instant::now()),
                Some(Err(SdrError::Timeout))
            ),
            "a recovered stream stays live, with no terminal error"
        );
    }

    #[test]
    fn blocking_operation_anchor_is_retained_on_drop_and_released_once_on_take() {
        struct DropProbe<'a>(&'a AtomicUsize);
        impl Drop for DropProbe<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let retained_drops = AtomicUsize::new(0);
        {
            let _retained = RetainOnDrop::new(DropProbe(&retained_drops));
        }
        assert_eq!(retained_drops.load(Ordering::Relaxed), 0);

        let returned_drops = AtomicUsize::new(0);
        {
            let mut retained = RetainOnDrop::new(DropProbe(&returned_drops));
            drop(retained.take());
        }
        assert_eq!(returned_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stream_lease_allows_one_owner_and_releases_on_drop() {
        let busy = Arc::new(AtomicBool::new(false));
        let lease = StreamLease::acquire(&busy).unwrap();
        assert!(matches!(
            StreamLease::acquire(&busy),
            Err(SdrError::DeviceBusy)
        ));
        drop(lease);
        assert!(StreamLease::acquire(&busy).is_ok());
    }

    #[test]
    fn control_transfer_lengths_are_checked_before_backend_casts() {
        assert_eq!(validate_control_len("control_in", 65_535).unwrap(), 65_535);
        assert!(matches!(
            validate_control_len("control_out", 65_536),
            Err(SdrError::InvalidParameter(_))
        ));
        assert_eq!(
            validate_bulk_len("bulk_read", i32::MAX as usize).unwrap(),
            i32::MAX
        );
        assert!(validate_bulk_len("bulk_read", (i32::MAX as usize) + 1).is_err());
    }

    #[test]
    fn stats_track_delivered_bytes() {
        let source = SyntheticSource::new(vec![vec![0u8; 8]]); // 4 complex samples
        let mut handle = start_stream_concrete(source, 4, None);
        let _ = handle.recv();
        let _ = handle.recv();
        assert!(handle.stats().bytes_delivered >= 8, "delivered byte count");
    }
}
