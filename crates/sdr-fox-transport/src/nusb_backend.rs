//! Pure-Rust USB transport backed by `nusb`.
//!
//! This is the desktop default (Linux/macOS/Windows). It provides the
//! control-plane [`Transport`] methods plus a [`NusbBufferSource`] that the
//! streaming engine pulls completed bulk transfers from.
//!
//! ## Linux kernel driver
//!
//! On Linux the in-kernel `dvb_usb_rtl28xxu` driver claims RTL-SDR dongles by
//! default and must be detached before we can claim interface 0. This is done
//! via `device.detach_kernel_driver(0)` under `cfg(target_os = "linux")`.

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use nusb::transfer::{
    Buffer, Bulk, Completion, ControlIn, ControlOut, ControlType, In, Recipient, TransferError,
};
use nusb::{Endpoint, Interface, MaybeFuture};
use sdr_fox_core::{
    ControlRequest, ControlType as CoreControlType, DeviceRecipient, SdrError, StreamHandle,
    Transport,
};

use crate::stream::{
    bulk_timeout, start_stream, validate_bulk_len, validate_control_len, validate_stream_config,
    BufferSource, OperationPoll, OwnedOperation, RetainOnDrop, StreamLease,
};

pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_millis(300);
/// Bulk reap poll interval for the streaming worker.
const BULK_REAP_TIMEOUT: Duration = Duration::from_millis(50);
/// CUMULATIVE wall-clock budget for one stall-recovery episode: it runs from
/// the first recovery attempt after the endpoint stalls until a buffer is
/// actually delivered, shared across every attempt in between. It is NOT a
/// per-attempt allowance — see [`RecoveryBudget`] for why.
const STALL_RECOVERY_BUDGET: Duration = Duration::from_secs(2);
/// Stable operation name carried by [`SdrError::RecoveryExhausted`] when the
/// stall-recovery budget expires.
const STALL_RECOVERY_OPERATION: &str = "bulk endpoint stall clear";
const MAX_STALL_REAP_TIMEOUTS: usize = 4;
const CLEAR_HALT_POLL: Duration = Duration::from_millis(50);

/// Pure-Rust USB transport backed by `nusb`. Holds an `Arc<Interface>` so it
/// is cheaply clonable (the streaming worker takes its own copy).
#[derive(Clone)]
pub struct NusbTransport {
    iface: Arc<Interface>,
    stream_busy: Arc<AtomicBool>,
}

impl NusbTransport {
    /// Start the ordinary stream with an opt-in diagnostic observer before
    /// bounded delivery. The callback sees each successful USB payload's
    /// actual length, including payloads subsequently dropped by the queue.
    /// It must be brief and nonblocking; it runs on the USB worker. Ordinary
    /// application streams do not install or execute this observer.
    ///
    /// # Errors
    /// Returns the same configuration, ownership, and USB errors as streaming.
    pub fn start_bulk_stream_observed(
        &mut self,
        endpoint: u8,
        buffer_count: usize,
        buffer_size: usize,
        queue_depth: usize,
        observe: impl FnMut(usize) + Send + 'static,
    ) -> Result<crate::stream::Stream, SdrError> {
        validate_stream_config(buffer_count, buffer_size, queue_depth)?;
        let lease = StreamLease::acquire(&self.stream_busy)?;
        let source =
            NusbBufferSource::new(self.iface.clone(), endpoint, buffer_size, buffer_count)?
                .with_stream_lease(lease);
        Ok(crate::stream::start_stream_concrete(
            ObservedSource { source, observe },
            queue_depth,
            None,
        ))
    }

    /// Wrap an already-opened, claimed `nusb` interface.
    #[must_use]
    pub fn from_interface(iface: Interface) -> Self {
        Self {
            iface: Arc::new(iface),
            stream_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open the `index`-th device matching `(vendor_id, product_id)`, claim
    /// interface 0, and (on Linux) detach the kernel driver.
    ///
    /// Not available on Android: an unprivileged Android process cannot
    /// enumerate the USB bus, so this method returns [`SdrError::Unsupported`]
    /// there. The Android transport enters nusb through `Device::from_fd`
    /// instead (see `NusbFdTransport`).
    ///
    /// # Errors
    ///
    /// - [`SdrError::DeviceNotFound`] if no matching device exists.
    /// - [`SdrError::Transport`] on open/claim failure.
    #[cfg(not(target_os = "android"))]
    pub fn open(vendor_id: u16, product_id: u16, index: usize) -> Result<Self, SdrError> {
        let mut info_iter = nusb::list_devices()
            .wait()
            .map_err(|e| SdrError::Transport(format!("nusb list: {e}")))?
            .filter(|d| d.vendor_id() == vendor_id && d.product_id() == product_id);
        let info = info_iter.nth(index).ok_or_else(|| {
            SdrError::DeviceNotFound(format!(
                "no USB device vid={vendor_id:#06x} pid={product_id:#06x} at index {index}"
            ))
        })?;
        drop(info_iter);
        Self::open_info(&info)
    }

    /// Open the exact enumerated native object, never a replacement index.
    #[cfg(not(target_os = "android"))]
    pub fn open_info(info: &nusb::DeviceInfo) -> Result<Self, SdrError> {
        let device = info
            .open()
            .wait()
            .map_err(|e| SdrError::Transport(format!("nusb open: {e}")))?;
        #[cfg(target_os = "linux")]
        {
            let _ = device.detach_kernel_driver(0);
        }
        let iface = device
            .claim_interface(0)
            .wait()
            .map_err(|e| SdrError::Transport(format!("nusb claim interface 0: {e}")))?;
        Ok(Self::from_interface(iface))
    }

    pub(crate) fn map_control_type(t: CoreControlType) -> ControlType {
        match t {
            CoreControlType::Standard => ControlType::Standard,
            CoreControlType::Class => ControlType::Class,
            CoreControlType::Vendor => ControlType::Vendor,
        }
    }

    pub(crate) fn map_recipient(r: DeviceRecipient) -> Recipient {
        match r {
            DeviceRecipient::Device => Recipient::Device,
            DeviceRecipient::Interface => Recipient::Interface,
            DeviceRecipient::Endpoint => Recipient::Endpoint,
            DeviceRecipient::Other => Recipient::Other,
        }
    }
}

struct ObservedSource<F> {
    source: NusbBufferSource,
    observe: F,
}

impl<F: FnMut(usize) + Send + 'static> BufferSource for ObservedSource<F> {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        let result = self.source.next_buffer();
        if let Ok(bytes) = &result {
            (self.observe)(bytes.len());
        }
        result
    }

    fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.source.set_stop(stop);
    }

    fn suppress_error_accounting(&mut self) -> bool {
        self.source.suppress_error_accounting()
    }
}

impl Transport for NusbTransport {
    fn control_in(&mut self, req: &ControlRequest) -> Result<Vec<u8>, SdrError> {
        debug_assert_eq!(req.direction, sdr_fox_core::TransferDirection::In);
        // nusb's control APIs return a `MaybeFuture`; `.wait()` blocks. Control
        // transfers are low-frequency so blocking is appropriate. Direction is
        // implicit: `ControlIn` is device→host.
        let request = ControlIn {
            control_type: Self::map_control_type(req.control_type),
            recipient: Self::map_recipient(req.recipient),
            request: req.request,
            value: req.value,
            index: req.index,
            length: validate_control_len("control_in", req.data.len())?,
        };
        let buf = self
            .iface
            .control_in(request, CONTROL_TIMEOUT)
            .wait()
            .map_err(map_nusb_control_error)?;
        if buf.len() != req.data.len() {
            return Err(SdrError::ShortTransfer {
                operation: "control_in",
                expected: req.data.len(),
                actual: buf.len(),
            });
        }
        Ok(buf)
    }

    fn control_out(&mut self, req: &ControlRequest) -> Result<usize, SdrError> {
        debug_assert_eq!(req.direction, sdr_fox_core::TransferDirection::Out);
        validate_control_len("control_out", req.data.len())?;
        let request = ControlOut {
            control_type: Self::map_control_type(req.control_type),
            recipient: Self::map_recipient(req.recipient),
            request: req.request,
            value: req.value,
            index: req.index,
            data: &req.data,
        };
        self.iface
            .control_out(request, CONTROL_TIMEOUT)
            .wait()
            .map_err(map_nusb_control_error)?;
        Ok(req.data.len())
    }

    fn bulk_read(
        &mut self,
        endpoint: u8,
        len: usize,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, SdrError> {
        validate_bulk_len("bulk_read", len)?;
        // Single-shot synchronous bulk read via the endpoint queue: submit one
        // transfer, wait for it, return its data.
        let mut ep = self
            .iface
            .endpoint::<Bulk, In>(endpoint)
            .map_err(|e| SdrError::Transport(format!("bulk endpoint open: {e}")))?;
        let completion = ep.transfer_blocking(Buffer::new(len), bulk_timeout(timeout_ms));
        if matches!(completion.status, Err(TransferError::Cancelled)) {
            if completion.actual_len == 0 {
                return Err(SdrError::Timeout);
            }
            let mut partial = completion.buffer.into_vec();
            partial.truncate(completion.actual_len.min(partial.len()));
            return Ok(partial);
        }
        completion_into_bytes(completion)
    }

    fn start_bulk_stream(
        &mut self,
        endpoint: u8,
        buffer_count: usize,
        buffer_size: usize,
        queue_depth: usize,
    ) -> Result<StreamHandle, SdrError> {
        validate_stream_config(buffer_count, buffer_size, queue_depth)?;
        let lease = StreamLease::acquire(&self.stream_busy)?;
        let source =
            NusbBufferSource::new(self.iface.clone(), endpoint, buffer_size, buffer_count)?
                .with_stream_lease(lease);
        Ok(start_stream(source, queue_depth, None))
    }

    fn boxed_clone(&self) -> Box<dyn Transport> {
        Box::new(self.clone())
    }
}

/// Extract the data bytes from a completed transfer, mapping errors. Returns
/// only the bytes the device actually wrote (`actual_len`), not the full
/// backing buffer.
/// Takes `Completion` by value because the buffer is moved out of it.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn completion_into_bytes(c: Completion) -> Result<Vec<u8>, SdrError> {
    c.status
        .map(|()| {
            // Convert the nusb Buffer into a Vec without an extra copy.
            // nusb's Buffer::into_vec() gives us the full backing allocation;
            // we truncate to actual_len.
            let mut buf = c.buffer.into_vec();
            let len = c.actual_len.min(buf.len());
            buf.truncate(len);
            buf
        })
        .map_err(map_nusb_transfer_error)
}

/// Cumulative time budget for one endpoint-recovery episode.
///
/// WHY cumulative, and why the invariants below must not be "simplified"
/// away: the generic stream worker retries [`SdrError::Timeout`] without
/// bound. A recovery deadline recomputed per attempt therefore produces
/// recover → fresh budget → `Timeout` → worker retries → recover → fresh
/// budget → forever: a stream that looks live but never delivers a sample
/// and never ends — for a radio, indistinguishable from "no signal".
///
/// Invariants:
/// - The deadline is fixed when the episode starts ([`Self::begin_attempt`]
///   on an idle budget) and is never extended by later attempts.
/// - Only a **delivered buffer** ([`Self::note_forward_progress`]) closes an
///   episode. A "successful" `clear_halt` does not: a stall→clear→stall
///   ping-pong that never yields data would otherwise re-arm its own budget
///   indefinitely.
/// - When the deadline has passed, an attempt-level `Timeout` escalates
///   ([`Self::escalate`]) to the terminal [`SdrError::RecoveryExhausted`].
struct RecoveryBudget {
    operation: &'static str,
    budget: Duration,
    episode: Option<RecoveryEpisode>,
}

struct RecoveryEpisode {
    started: Instant,
    attempts: u32,
}

impl RecoveryBudget {
    fn new(operation: &'static str, budget: Duration) -> Self {
        Self {
            operation,
            budget,
            episode: None,
        }
    }

    /// Record one recovery attempt and return the episode's fixed deadline,
    /// starting a new episode at `now` if none is active.
    fn begin_attempt(&mut self, now: Instant) -> Instant {
        let episode = self.episode.get_or_insert(RecoveryEpisode {
            started: now,
            attempts: 0,
        });
        episode.attempts = episode.attempts.saturating_add(1);
        episode.started + self.budget
    }

    /// Escalate an attempt-level `Timeout` into the terminal
    /// [`SdrError::RecoveryExhausted`] once the episode deadline has passed.
    ///
    /// A `Timeout` before the deadline passes through untouched — it is the
    /// yield mechanism that returns control to the worker so stop flags stay
    /// responsive. Non-timeout errors (including `Cancelled`) also pass
    /// through: they carry more information than exhaustion would, and the
    /// worker already bounds them by `MAX_CONSECUTIVE_ERRORS`.
    fn escalate(&self, result: Result<(), SdrError>, now: Instant) -> Result<(), SdrError> {
        if !matches!(result, Err(SdrError::Timeout)) {
            return result;
        }
        let Some(episode) = &self.episode else {
            return result;
        };
        if now < episode.started + self.budget {
            return result;
        }
        Err(SdrError::RecoveryExhausted {
            operation: self.operation,
            elapsed: now.saturating_duration_since(episode.started),
            attempts: episode.attempts,
        })
    }

    /// Close the active episode. Call ONLY on genuine forward progress — a
    /// buffer actually delivered to the consumer — never merely because a
    /// recovery step succeeded (see the type-level invariants).
    fn note_forward_progress(&mut self) {
        self.episode = None;
    }
}

struct ClearHaltCompletion {
    result: Result<(), SdrError>,
    stream_lease: Option<StreamLease>,
}

fn launch_clear_halt(
    operation: impl MaybeFuture<Output = Result<(), nusb::Error>> + 'static,
    stream_lease: Option<StreamLease>,
) -> mpsc::Receiver<ClearHaltCompletion> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut retained_lease = RetainOnDrop::new(stream_lease);
    let result = std::thread::Builder::new()
        .name("sdr-fox-nusb-clear-halt".into())
        .spawn(move || {
            let result = operation
                .wait()
                .map_err(|error| SdrError::Transport(format!("clear bulk endpoint halt: {error}")));
            let completion = ClearHaltCompletion {
                result,
                stream_lease: retained_lease.take(),
            };
            // If the source was dropped after a bounded timeout, dropping the
            // SendError payload releases the lease only now, after clear_halt
            // has actually returned.
            let _ = sender.send(completion);
        });
    if let Err(error) = result {
        // The failed spawn drops its closure. RetainOnDrop intentionally
        // leaves the source busy rather than permitting a racing stream.
        tracing::error!(%error, "failed to start nusb clear-halt worker; stream retained");
    }
    receiver
}

/// `BufferSource` impl backed by nusb bulk transfers. v1 uses a small ring
/// (default `buffer_count` in flight) via `submit`/`wait_next_complete`.
pub struct NusbBufferSource {
    iface: Arc<Interface>,
    endpoint: u8,
    buffer_size: usize,
    ep: Option<Endpoint<Bulk, In>>,
    target_in_flight: usize,
    recovering_stall: bool,
    stall_reported: bool,
    recovery: RecoveryBudget,
    stop: Option<Arc<AtomicBool>>,
    clear_halt: OwnedOperation<ClearHaltCompletion>,
    suppress_error_accounting: bool,
    #[allow(dead_code)] // RAII field: its Drop is the behavior.
    stream_lease: Option<StreamLease>,
}

impl NusbBufferSource {
    /// Construct a buffer source with a ring of `buffer_count` in-flight
    /// transfers of `buffer_size` bytes each.
    ///
    /// # Errors
    ///
    /// Returns [`SdrError::InvalidParameter`] if `buffer_size` is not a
    /// positive multiple of 512 (USB requirement).
    pub fn new(
        iface: Arc<Interface>,
        endpoint: u8,
        buffer_size: usize,
        buffer_count: usize,
    ) -> Result<Self, SdrError> {
        validate_source_config(buffer_count, buffer_size)?;
        Ok(Self {
            iface,
            endpoint,
            buffer_size,
            ep: None,
            target_in_flight: buffer_count,
            recovering_stall: false,
            stall_reported: false,
            recovery: RecoveryBudget::new(STALL_RECOVERY_OPERATION, STALL_RECOVERY_BUDGET),
            stop: None,
            clear_halt: OwnedOperation::new(),
            suppress_error_accounting: false,
            stream_lease: None,
        })
    }

    pub(crate) fn with_stream_lease(mut self, lease: StreamLease) -> Self {
        self.stream_lease = Some(lease);
        self
    }

    fn ensure_started(&mut self) -> Result<(), SdrError> {
        if self.ep.is_none() {
            let ep = self
                .iface
                .endpoint::<Bulk, In>(self.endpoint)
                .map_err(|e| SdrError::Transport(format!("bulk endpoint open: {e}")))?;
            self.ep = Some(ep);
        }
        Ok(())
    }

    fn fill_ring(&mut self) {
        let buffer_size = self.buffer_size;
        let target = self.target_in_flight;
        let ep = self
            .ep
            .as_mut()
            .expect("endpoint initialized before filling the ring");
        while ep.pending() < target {
            ep.submit(Buffer::new(buffer_size));
        }
    }

    fn recover_stall(&mut self) -> Result<(), SdrError> {
        // The deadline is CUMULATIVE across the whole recovery episode —
        // fixed when the first attempt begins, never recomputed per call.
        // The stream worker retries `Timeout` without bound, so a fresh
        // per-call budget would let a persistently stalled endpoint cycle
        // recover→Timeout→retry forever, streaming silence indefinitely.
        // Once the episode budget is spent, the attempt-level `Timeout`
        // escalates to the terminal `RecoveryExhausted`.
        let deadline = self.recovery.begin_attempt(Instant::now());
        let result = self.recover_stall_until(deadline);
        self.recovery.escalate(result, Instant::now())
    }

    fn recover_stall_until(&mut self, deadline: Instant) -> Result<(), SdrError> {
        let stop = self.stop.clone();
        if !self.clear_halt.is_running() {
            {
                let ep = self
                    .ep
                    .as_mut()
                    .expect("endpoint initialized before STALL recovery");
                let mut recovery = NusbEndpointRecovery { ep };
                reap_stalled_endpoint(&mut recovery, deadline, || {
                    stop.as_ref()
                        .is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Acquire))
                })?;
            }

            let stream_lease = self.stream_lease.take();
            let operation = self
                .ep
                .as_mut()
                .expect("endpoint remains owned while clear-halt runs")
                .clear_halt();
            self.clear_halt
                .install(launch_clear_halt(operation, stream_lease));
        }

        match self.clear_halt.poll_until(deadline, CLEAR_HALT_POLL, || {
            stop.as_ref()
                .is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Acquire))
        }) {
            OperationPoll::Complete(completion) => {
                self.stream_lease = completion.stream_lease;
                completion.result?;
                self.fill_ring();
                Ok(())
            }
            OperationPoll::TimedOut => Err(SdrError::Timeout),
            OperationPoll::Stopped => Err(SdrError::Cancelled),
            OperationPoll::Disconnected => Err(SdrError::Transport(
                "nusb clear-halt worker stopped without returning ownership".into(),
            )),
        }
    }
}

impl BufferSource for NusbBufferSource {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        self.ensure_started()?;
        loop {
            if self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(SdrError::Cancelled);
            }
            if self.recovering_stall {
                match self.recover_stall() {
                    Ok(()) => {
                        self.recovering_stall = false;
                        if !self.stall_reported {
                            // Surface the failed transfer after the endpoint
                            // is usable again so stream stats and producer
                            // sequence expose the lost block.
                            return Err(SdrError::Stall);
                        }
                        self.stall_reported = false;
                    }
                    Err(error) => {
                        if !self.stall_reported {
                            self.stall_reported = true;
                            return Err(SdrError::Stall);
                        }
                        self.suppress_error_accounting = true;
                        return Err(error);
                    }
                }
            }

            self.fill_ring();
            let Some(completion) = self
                .ep
                .as_mut()
                .expect("endpoint initialized")
                .wait_next_complete(BULK_REAP_TIMEOUT)
            else {
                continue;
            };

            if matches!(completion.status, Err(TransferError::Stall)) {
                // A STALL is endpoint-wide. The other ring entries must be
                // cancelled and returned before clear_halt, after which the
                // complete configured ring is submitted again.
                self.recovering_stall = true;
                continue;
            }
            // Re-arm the completed slot before handing its allocation to the
            // consumer. This keeps the configured ring depth continuously in
            // flight on healthy and non-STALL completion paths.
            self.fill_ring();
            let bytes = completion_into_bytes(completion)?;
            // A delivered buffer is the ONLY event that closes a recovery
            // episode. Recovery merely succeeding must not: a stall→clear→
            // stall cycle that never yields data would re-arm its own budget
            // forever, recreating the silent non-terminal stream the budget
            // exists to prevent.
            self.recovery.note_forward_progress();
            return Ok(bytes);
        }
    }

    fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }

    fn suppress_error_accounting(&mut self) -> bool {
        std::mem::take(&mut self.suppress_error_accounting)
    }
}

/// Small recovery interface shared by the real nusb endpoint and a
/// deterministic fake. Keeping the ordering in one function makes it
/// impossible for the production path to clear a halt while URBs are pending.
trait StalledEndpoint {
    fn pending(&self) -> usize;
    fn cancel_all(&mut self);
    fn reap_next(&mut self, timeout: Duration) -> bool;
}

struct NusbEndpointRecovery<'a> {
    ep: &'a mut Endpoint<Bulk, In>,
}

impl StalledEndpoint for NusbEndpointRecovery<'_> {
    fn pending(&self) -> usize {
        self.ep.pending()
    }

    fn cancel_all(&mut self) {
        self.ep.cancel_all();
    }

    fn reap_next(&mut self, timeout: Duration) -> bool {
        self.ep.wait_next_complete(timeout).is_some()
    }
}

fn reap_stalled_endpoint(
    endpoint: &mut impl StalledEndpoint,
    deadline: Instant,
    mut stopped: impl FnMut() -> bool,
) -> Result<(), SdrError> {
    endpoint.cancel_all();
    let mut total_timeouts = 0;
    while endpoint.pending() > 0 {
        if stopped() {
            return Err(SdrError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SdrError::Timeout);
        }
        // Cancellation is asynchronous. A timeout means ownership has not
        // returned yet, so keep reaping; dropping or clearing here would race
        // the platform completion path.
        if !endpoint.reap_next(BULK_REAP_TIMEOUT.min(remaining)) {
            total_timeouts += 1;
            if total_timeouts == MAX_STALL_REAP_TIMEOUTS {
                // Keep the endpoint and its pending transfers owned. The next
                // source call resumes recovery; Drop remains bounded and nusb
                // itself safely abandons still-pending platform transfers.
                return Err(SdrError::Timeout);
            }
        }
    }
    Ok(())
}

pub(crate) fn map_nusb_transfer_error(error: TransferError) -> SdrError {
    match error {
        TransferError::Cancelled => SdrError::Cancelled,
        TransferError::Stall => SdrError::Stall,
        TransferError::Disconnected => SdrError::DeviceLost,
        TransferError::InvalidArgument => SdrError::InvalidParameter(error.to_string()),
        TransferError::Fault | TransferError::Unknown(_) => {
            SdrError::Transport(format!("bulk transfer: {error}"))
        }
    }
}

pub(crate) fn map_nusb_control_error(error: TransferError) -> SdrError {
    match error {
        TransferError::Cancelled => SdrError::Timeout,
        other => map_nusb_transfer_error(other),
    }
}

impl Drop for NusbBufferSource {
    fn drop(&mut self) {
        if let Some(ep) = self.ep.as_mut() {
            ep.cancel_all();
            // Drain pending completions so the kernel frees the URBs.
            // Only drain while we have pending transfers — waiting with zero
            // pending can panic in nusb (P1#11 liveness fix).
            while ep.pending() > 0 {
                if ep.wait_next_complete(Duration::from_millis(50)).is_none() {
                    break; // Timeout with no completion — stop waiting.
                }
            }
        }
    }
}

/// Validate that a USB bulk transfer buffer size is a positive 512 multiple.
#[cfg(test)]
fn validate_buffer_size(buffer_size: usize) -> Result<(), SdrError> {
    validate_stream_config(1, buffer_size, 1)
}

fn validate_source_config(buffer_count: usize, buffer_size: usize) -> Result<(), SdrError> {
    validate_stream_config(buffer_count, buffer_size, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    enum RecoveryEvent {
        Cancel,
        Reap,
    }

    struct FakeStalledEndpoint {
        pending: usize,
        cancelled: bool,
        complete_reaps: bool,
        events: Vec<RecoveryEvent>,
    }

    impl StalledEndpoint for FakeStalledEndpoint {
        fn pending(&self) -> usize {
            self.pending
        }

        fn cancel_all(&mut self) {
            self.cancelled = true;
            self.events.push(RecoveryEvent::Cancel);
        }

        fn reap_next(&mut self, _timeout: Duration) -> bool {
            assert!(self.cancelled);
            self.events.push(RecoveryEvent::Reap);
            if self.complete_reaps {
                self.pending -= 1;
            }
            self.complete_reaps
        }
    }

    #[test]
    fn stall_recovery_cancels_and_reaps_before_clear_can_start() {
        let mut endpoint = FakeStalledEndpoint {
            pending: 3,
            cancelled: false,
            complete_reaps: true,
            events: Vec::new(),
        };

        reap_stalled_endpoint(
            &mut endpoint,
            Instant::now() + Duration::from_secs(1),
            || false,
        )
        .unwrap();

        assert_eq!(endpoint.pending, 0);
        assert_eq!(
            endpoint.events,
            [
                RecoveryEvent::Cancel,
                RecoveryEvent::Reap,
                RecoveryEvent::Reap,
                RecoveryEvent::Reap,
            ]
        );
    }

    #[test]
    fn stall_recovery_never_clears_or_refills_after_permanent_reap_timeouts() {
        let mut endpoint = FakeStalledEndpoint {
            pending: 2,
            cancelled: false,
            complete_reaps: false,
            events: Vec::new(),
        };

        assert!(matches!(
            reap_stalled_endpoint(
                &mut endpoint,
                Instant::now() + Duration::from_secs(1),
                || false
            ),
            Err(SdrError::Timeout)
        ));
        assert_eq!(endpoint.pending, 2);
        assert_eq!(
            endpoint.events,
            [
                RecoveryEvent::Cancel,
                RecoveryEvent::Reap,
                RecoveryEvent::Reap,
                RecoveryEvent::Reap,
                RecoveryEvent::Reap,
            ]
        );
    }

    #[test]
    fn stall_recovery_observes_stop_without_clearing_or_refilling() {
        let mut endpoint = FakeStalledEndpoint {
            pending: 2,
            cancelled: false,
            complete_reaps: true,
            events: Vec::new(),
        };

        assert!(matches!(
            reap_stalled_endpoint(
                &mut endpoint,
                Instant::now() + Duration::from_secs(1),
                || true
            ),
            Err(SdrError::Cancelled)
        ));
        assert_eq!(endpoint.events, [RecoveryEvent::Cancel]);
    }

    #[test]
    fn diagnostic_rings_reap_every_pending_transfer_or_preserve_pending_ownership() {
        for pending in [0, 4, 16, 32] {
            for complete_reaps in [false, true] {
                let mut endpoint = FakeStalledEndpoint {
                    pending,
                    cancelled: false,
                    complete_reaps,
                    events: Vec::new(),
                };
                let result = reap_stalled_endpoint(
                    &mut endpoint,
                    Instant::now() + Duration::from_secs(2),
                    || false,
                );
                assert_eq!(endpoint.events[0], RecoveryEvent::Cancel);
                if complete_reaps || pending == 0 {
                    assert!(result.is_ok());
                    assert_eq!(endpoint.pending, 0);
                    assert_eq!(endpoint.events.len(), pending + 1);
                } else {
                    assert!(matches!(result, Err(SdrError::Timeout)));
                    assert_eq!(endpoint.pending, pending);
                    assert_eq!(endpoint.events.len(), MAX_STALL_REAP_TIMEOUTS + 1);
                }
            }
            let mut endpoint = FakeStalledEndpoint {
                pending,
                cancelled: false,
                complete_reaps: true,
                events: Vec::new(),
            };
            let mut stop_checks = 0;
            let result = reap_stalled_endpoint(
                &mut endpoint,
                Instant::now() + Duration::from_secs(2),
                || {
                    stop_checks += 1;
                    stop_checks > 2
                },
            );
            if pending == 0 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(SdrError::Cancelled)));
                assert_eq!(endpoint.pending, pending - 2);
                assert_eq!(
                    endpoint.events,
                    [
                        RecoveryEvent::Cancel,
                        RecoveryEvent::Reap,
                        RecoveryEvent::Reap
                    ]
                );
            }
        }
    }

    #[test]
    fn stopped_clear_keeps_the_original_stream_lease_until_late_completion() {
        let busy = Arc::new(AtomicBool::new(false));
        let lease = StreamLease::acquire(&busy).unwrap();
        let (send, receive) = mpsc::channel();
        let mut operation = OwnedOperation::new();
        operation.install(receive);
        assert!(matches!(
            operation.poll_until(Instant::now(), CLEAR_HALT_POLL, || false),
            OperationPoll::TimedOut
        ));
        assert!(matches!(
            operation.poll_until(Instant::now(), CLEAR_HALT_POLL, || true),
            OperationPoll::Stopped
        ));
        assert!(operation.is_running());
        assert!(matches!(
            StreamLease::acquire(&busy),
            Err(SdrError::DeviceBusy)
        ));
        // A channel models the retained worker returning its exact ownership
        // anchor after the native call, including completion after a stop.
        send.send(lease).unwrap();
        let OperationPoll::Complete(returned) = operation.poll_until(
            Instant::now() + Duration::from_secs(2),
            CLEAR_HALT_POLL,
            || false,
        ) else {
            panic!("original clear completion must return ownership")
        };
        assert!(!operation.is_running());
        assert!(matches!(
            StreamLease::acquire(&busy),
            Err(SdrError::DeviceBusy)
        ));
        drop(returned);
        assert!(StreamLease::acquire(&busy).is_ok());
    }

    #[test]
    fn never_completing_clear_times_out_without_starting_a_duplicate() {
        let (sender, receiver) = mpsc::channel();
        let mut operation = OwnedOperation::new();
        let mut prepare_and_start_calls = 0;
        if !operation.is_running() {
            prepare_and_start_calls += 1;
            operation.install(receiver);
        }
        assert!(matches!(
            operation.poll_until(
                Instant::now() + Duration::from_millis(5),
                CLEAR_HALT_POLL,
                || false
            ),
            OperationPoll::TimedOut
        ));
        assert!(operation.is_running());

        // This is the production retry branch: an installed clear is polled
        // directly, so endpoint cancel/reap and clear launch are not repeated.
        if !operation.is_running() {
            prepare_and_start_calls += 1;
        }
        assert_eq!(prepare_and_start_calls, 1);

        // The exact original task may complete late; its value is consumed on
        // the next poll and only then does the state become idle.
        sender.send(73).unwrap();
        match operation.poll_until(
            Instant::now() + Duration::from_secs(1),
            CLEAR_HALT_POLL,
            || false,
        ) {
            OperationPoll::Complete(value) => assert_eq!(value, 73),
            _ => panic!("late clear completion was not returned"),
        }
        assert!(!operation.is_running());
    }

    #[test]
    fn clear_wait_observes_stop_and_keeps_the_owned_task_detached() {
        let (_sender, receiver) = mpsc::channel::<()>();
        let mut operation = OwnedOperation::new();
        operation.install(receiver);

        assert!(matches!(
            operation.poll_until(
                Instant::now() + Duration::from_secs(1),
                CLEAR_HALT_POLL,
                || true
            ),
            OperationPoll::Stopped
        ));
        assert!(operation.is_running());
    }

    #[test]
    fn validate_buffer_size_rejects_zero_and_non_512_multiples() {
        assert!(validate_buffer_size(0).is_err());
        assert!(validate_buffer_size(100).is_err());
        assert!(validate_buffer_size(511).is_err());
        assert!(validate_buffer_size(513).is_err());
    }

    #[test]
    fn validate_buffer_size_accepts_512_multiples() {
        assert!(validate_buffer_size(512).is_ok());
        assert!(validate_buffer_size(65_536).is_ok());
        assert!(validate_buffer_size(262_144).is_ok());
    }

    #[test]
    fn source_config_rejects_zero_ring_depth_before_endpoint_wait() {
        assert!(matches!(
            validate_source_config(0, 65_536),
            Err(SdrError::InvalidParameter(_))
        ));
    }

    #[test]
    fn one_shot_bulk_timeout_honors_caller_milliseconds() {
        assert_eq!(bulk_timeout(1), Duration::from_millis(1));
        assert_eq!(bulk_timeout(12_345), Duration::from_millis(12_345));
        assert_eq!(bulk_timeout(0), Duration::from_secs(1));
    }

    fn test_budget() -> RecoveryBudget {
        RecoveryBudget::new(STALL_RECOVERY_OPERATION, Duration::from_secs(2))
    }

    #[test]
    fn recovery_deadline_is_fixed_at_episode_start_not_per_attempt() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let first = budget.begin_attempt(t0);
        assert_eq!(first, t0 + Duration::from_secs(2));
        // Re-entering recovery must NOT grant a fresh budget: the deadline of
        // a later attempt inside the same episode is unchanged.
        let second = budget.begin_attempt(t0 + Duration::from_secs(1));
        assert_eq!(second, first);
        let third = budget.begin_attempt(t0 + Duration::from_millis(1999));
        assert_eq!(third, first);
    }

    #[test]
    fn attempt_timeout_before_the_episode_deadline_stays_transient() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let _ = budget.begin_attempt(t0);
        // The MAX_STALL_REAP_TIMEOUTS yield path returns Timeout well before
        // the deadline; it must stay retryable so recovery can resume.
        assert!(matches!(
            budget.escalate(Err(SdrError::Timeout), t0 + Duration::from_millis(200)),
            Err(SdrError::Timeout)
        ));
    }

    #[test]
    fn timeout_at_the_episode_deadline_escalates_to_recovery_exhausted() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let _ = budget.begin_attempt(t0);
        let _ = budget.begin_attempt(t0 + Duration::from_millis(300));
        let _ = budget.begin_attempt(t0 + Duration::from_millis(600));
        match budget.escalate(Err(SdrError::Timeout), t0 + Duration::from_secs(2)) {
            Err(SdrError::RecoveryExhausted {
                operation,
                elapsed,
                attempts,
            }) => {
                assert_eq!(operation, STALL_RECOVERY_OPERATION);
                assert_eq!(attempts, 3);
                assert!(elapsed >= Duration::from_secs(2));
            }
            other => panic!("expected RecoveryExhausted, got {other:?}"),
        }
    }

    #[test]
    fn non_timeout_results_pass_through_even_after_the_deadline() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let _ = budget.begin_attempt(t0);
        let late = t0 + Duration::from_secs(5);
        // Stop must win over exhaustion, and richer errors keep their detail.
        assert!(matches!(
            budget.escalate(Err(SdrError::Cancelled), late),
            Err(SdrError::Cancelled)
        ));
        assert!(matches!(
            budget.escalate(Err(SdrError::DeviceLost), late),
            Err(SdrError::DeviceLost)
        ));
        assert!(budget.escalate(Ok(()), late).is_ok());
    }

    #[test]
    fn successful_recovery_without_delivery_does_not_rearm_the_budget() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let _ = budget.begin_attempt(t0);
        // clear_halt succeeded, but no buffer was delivered before the
        // endpoint stalled again: same episode, same deadline.
        assert!(budget
            .escalate(Ok(()), t0 + Duration::from_millis(100))
            .is_ok());
        let deadline = budget.begin_attempt(t0 + Duration::from_millis(1900));
        assert_eq!(deadline, t0 + Duration::from_secs(2));
        assert!(matches!(
            budget.escalate(Err(SdrError::Timeout), t0 + Duration::from_secs(2)),
            Err(SdrError::RecoveryExhausted { attempts: 2, .. })
        ));
    }

    #[test]
    fn delivered_buffer_closes_the_episode_and_a_new_stall_gets_a_fresh_budget() {
        let mut budget = test_budget();
        let t0 = Instant::now();
        let _ = budget.begin_attempt(t0);
        budget.note_forward_progress();

        // A stall long after the first episode starts over: fresh deadline,
        // fresh attempt count, and a timeout before the NEW deadline stays
        // transient even though the old deadline is long past.
        let t1 = t0 + Duration::from_secs(60);
        let deadline = budget.begin_attempt(t1);
        assert_eq!(deadline, t1 + Duration::from_secs(2));
        assert!(matches!(
            budget.escalate(Err(SdrError::Timeout), t1 + Duration::from_millis(1)),
            Err(SdrError::Timeout)
        ));
        assert!(matches!(
            budget.escalate(Err(SdrError::Timeout), t1 + Duration::from_secs(2)),
            Err(SdrError::RecoveryExhausted { attempts: 1, .. })
        ));
    }

    #[test]
    fn transfer_errors_map_to_typed_stream_errors() {
        assert!(matches!(
            map_nusb_transfer_error(TransferError::Cancelled),
            SdrError::Cancelled
        ));
        assert!(matches!(
            map_nusb_transfer_error(TransferError::Stall),
            SdrError::Stall
        ));
        assert!(matches!(
            map_nusb_transfer_error(TransferError::Disconnected),
            SdrError::DeviceLost
        ));
    }
}
