//! Async multi-URB bulk streaming using libusb's native ABI.
//!
//! Every source owns an independent ring. Each transfer's `user_data` points
//! at a `CallbackSlot` containing the ring state and a fenced slot phase, so
//! callbacks never consult process-global state. Transfer memory is freed only
//! after the slot has reached a terminal callback state.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use libusb1_sys as usb;
use libusb1_sys::constants::{
    LIBUSB_ERROR_INTERRUPTED, LIBUSB_ERROR_NOT_FOUND, LIBUSB_SUCCESS, LIBUSB_TRANSFER_CANCELLED,
    LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR, LIBUSB_TRANSFER_NO_DEVICE,
    LIBUSB_TRANSFER_OVERFLOW, LIBUSB_TRANSFER_STALL, LIBUSB_TRANSFER_TIMED_OUT,
    LIBUSB_TRANSFER_TYPE_BULK,
};
use rusb::{DeviceHandle, GlobalContext, UsbContext};
use sdr_fox_core::SdrError;

use crate::stream::{BufferSource, OperationPoll, OwnedOperation, RetainOnDrop, StreamLease};

const EVENT_POLL: Duration = Duration::from_millis(50);
const INLINE_REAP_BUDGET: Duration = Duration::from_millis(250);
/// CUMULATIVE wall-clock budget for one stall-recovery episode: it runs from
/// the first recovery attempt after the endpoint stalls until a buffer is
/// actually delivered, shared across every attempt in between. It is NOT a
/// per-attempt allowance — see [`RecoveryBudget`] for why.
const STALL_RECOVERY_BUDGET: Duration = Duration::from_secs(2);
/// Stable operation name carried by [`SdrError::RecoveryExhausted`] when the
/// stall-recovery budget expires. Deliberately identical to the nusb backend's
/// name: consumers must not have to tell the two backends apart to recognise a
/// wedged bulk endpoint.
const STALL_RECOVERY_OPERATION: &str = "bulk endpoint stall clear";
const CLEAR_HALT_POLL: Duration = Duration::from_millis(50);
const MAX_SUBMIT_ERRORS: usize = 5;

const SLOT_PENDING: u8 = 0;
const SLOT_SUBMITTED: u8 = 1;
const SLOT_TERMINAL: u8 = 2;

const FAILURE_NONE: u8 = 0;
const FAILURE_DEVICE_LOST: u8 = 1;
const FAILURE_STALL: u8 = 2;
const FAILURE_TRANSPORT: u8 = 3;

/// Owner contract for a raw libusb handle/context pair used by the shared
/// async ring.
///
/// # Safety
///
/// Implementors must keep both pointers live and stable until the owner is
/// dropped, permit libusb calls from the stream/reaper threads, and ensure the
/// context belongs to the handle. The ring retains an `Arc<Self>` through all
/// callbacks and quarantined teardown.
pub(crate) unsafe trait RawLibusbHandleOwner: Send + Sync + 'static {
    fn raw_handle(&self) -> *mut usb::libusb_device_handle;
    fn raw_context(&self) -> *mut usb::libusb_context;

    fn clear_halt(&self, endpoint: u8) -> Result<(), SdrError> {
        let rc = unsafe { usb::libusb_clear_halt(self.raw_handle(), endpoint) };
        if rc == LIBUSB_SUCCESS {
            Ok(())
        } else {
            Err(map_libusb_operation_error(rc, "clear bulk endpoint halt"))
        }
    }

    fn interrupt_events(&self) {
        unsafe { usb::libusb_interrupt_event_handler(self.raw_context()) };
    }
}

// SAFETY: rusb's DeviceHandle owns a live libusb handle and its GlobalContext;
// both are internally synchronized by libusb and stay live through the Arc.
unsafe impl RawLibusbHandleOwner for DeviceHandle<GlobalContext> {
    fn raw_handle(&self) -> *mut usb::libusb_device_handle {
        self.as_raw()
    }

    fn raw_context(&self) -> *mut usb::libusb_context {
        self.context().as_raw()
    }
}

// SAFETY: identical to the GlobalContext implementation, but the explicitly
// created Context is device-local and retained by DeviceHandle.
unsafe impl RawLibusbHandleOwner for DeviceHandle<rusb::Context> {
    fn raw_handle(&self) -> *mut usb::libusb_device_handle {
        self.as_raw()
    }

    fn raw_context(&self) -> *mut usb::libusb_context {
        self.context().as_raw()
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn event_stop_handle<H: RawLibusbHandleOwner>(
    handle: Arc<H>,
) -> sdr_fox_core::sample::StreamStopHandle {
    sdr_fox_core::sample::StreamStopHandle::new(move || {
        // The captured Arc keeps the context live even if callers retain this
        // stop handle after the stream itself has been dropped.
        handle.interrupt_events();
    })
}

fn event_timeout_for(timeout: Duration) -> libc::timeval {
    libc::timeval {
        tv_sec: timeout.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        // `subsec_micros` is at most 999_999, which fits `suseconds_t` on
        // every supported host (including macOS, where it is an `i32`).
        tv_usec: libc::suseconds_t::try_from(u64::from(timeout.subsec_micros()))
            .expect("subsecond microseconds fit in suseconds_t"),
    }
}

struct NativeBuffer {
    ptr: *mut u8,
    capacity: usize,
}

fn allocate_buffer(min_capacity: usize) -> NativeBuffer {
    let mut buffer = Vec::<u8>::with_capacity(min_capacity);
    let native = NativeBuffer {
        ptr: buffer.as_mut_ptr(),
        capacity: buffer.capacity(),
    };
    std::mem::forget(buffer);
    native
}

/// # Safety
///
/// `ptr` and `capacity` must come from one [`allocate_buffer`] allocation,
/// `initialized_len` bytes must have been initialized by libusb, and ownership
/// must not have been reclaimed before this call.
unsafe fn reclaim_buffer(ptr: *mut u8, initialized_len: usize, capacity: usize) -> Vec<u8> {
    unsafe { Vec::from_raw_parts(ptr, initialized_len, capacity) }
}

/// # Safety
///
/// Same allocation contract as [`reclaim_buffer`]. No byte is assumed
/// initialized, which is the correct terminal-ring deallocation path.
unsafe fn free_buffer(ptr: *mut u8, capacity: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, capacity) });
}

#[derive(Debug)]
struct CompletedBuf {
    buffer: Vec<u8>,
    timestamp: Instant,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryKind {
    Resubmit,
    Stall,
}

#[derive(Clone, Copy, Debug)]
struct PendingAction {
    transfer_addr: usize,
    kind: RecoveryKind,
}

struct RingState {
    completed: Mutex<VecDeque<CompletedBuf>>,
    pending: Mutex<VecDeque<PendingAction>>,
    pending_count: AtomicUsize,
    spare_buffers: Mutex<Vec<Vec<u8>>>,
    spare_target: usize,
    buffer_size: usize,
    failure: AtomicU8,
    stopping: AtomicBool,
    /// Callbacks that have entered Rust but not yet returned to libusb. Slot
    /// phase alone is insufficient because a callback marks terminal just
    /// before returning; teardown must also wait for this count to reach zero.
    callbacks_active: AtomicUsize,
    terminal_count: AtomicUsize,
    next_sequence: AtomicU64,
    samples_dropped: AtomicU64,
    dropped_blocks: AtomicU64,
    failed_transfers: AtomicU64,
    hardware_overruns_unknown: AtomicU64,
    high_water_mark: AtomicUsize,
    queue_cap: usize,
}

impl RingState {
    fn new(queue_cap: usize, num_transfers: usize, buffer_size: usize) -> Self {
        let mut spare_buffers = Vec::with_capacity(num_transfers);
        for _ in 0..num_transfers {
            spare_buffers.push(Vec::with_capacity(buffer_size));
        }
        Self {
            completed: Mutex::new(VecDeque::with_capacity(queue_cap.max(1))),
            pending: Mutex::new(VecDeque::with_capacity(num_transfers)),
            pending_count: AtomicUsize::new(0),
            spare_buffers: Mutex::new(spare_buffers),
            spare_target: num_transfers,
            buffer_size,
            failure: AtomicU8::new(FAILURE_NONE),
            stopping: AtomicBool::new(false),
            callbacks_active: AtomicUsize::new(0),
            terminal_count: AtomicUsize::new(0),
            next_sequence: AtomicU64::new(0),
            samples_dropped: AtomicU64::new(0),
            dropped_blocks: AtomicU64::new(0),
            failed_transfers: AtomicU64::new(0),
            hardware_overruns_unknown: AtomicU64::new(0),
            high_water_mark: AtomicUsize::new(0),
            queue_cap: queue_cap.max(1),
        }
    }

    fn fail(&self, failure: u8) {
        let _ = self.failure.compare_exchange(
            FAILURE_NONE,
            failure,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.stopping.store(true, Ordering::Release);
    }

    fn error(&self) -> Option<SdrError> {
        match self.failure.load(Ordering::Acquire) {
            FAILURE_NONE => None,
            FAILURE_DEVICE_LOST => Some(SdrError::DeviceLost),
            FAILURE_STALL => Some(SdrError::Stall),
            FAILURE_TRANSPORT => Some(SdrError::Transport(
                "libusb async transfer ring stopped".into(),
            )),
            _ => Some(SdrError::Transport(
                "libusb async transfer ring entered an unknown state".into(),
            )),
        }
    }

    #[cfg(test)]
    fn enqueue_completed(&self, buffer: Vec<u8>, timestamp: Instant) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let mut queue = lock_unpoisoned(&self.completed);
        if queue.len() == self.queue_cap {
            self.samples_dropped
                .fetch_add((buffer.len() / 2) as u64, Ordering::Relaxed);
            self.dropped_blocks.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.push_back(CompletedBuf {
            buffer,
            timestamp,
            sequence,
        });
        self.high_water_mark
            .fetch_max(queue.len(), Ordering::Relaxed);
    }

    fn enqueue_recovery(&self, transfer: *mut usb::libusb_transfer, kind: RecoveryKind) {
        let mut pending = lock_unpoisoned(&self.pending);
        pending.push_back(PendingAction {
            transfer_addr: transfer as usize,
            kind,
        });
        // Publish the count before releasing the queue lock. Otherwise the
        // owner could drain this action between push and fetch_add, then
        // underflow the counter while subtracting the larger drained batch.
        self.pending_count.fetch_add(1, Ordering::Release);
    }

    fn pop_completed(&self) -> Option<CompletedBuf> {
        let completed = lock_unpoisoned(&self.completed).pop_front();
        if completed.is_some() {
            // Allocation is deliberately owner-thread work, never callback
            // work. Replenish the one spare consumed by this completion.
            self.replenish_spare();
        }
        completed
    }

    fn replenish_spare(&self) {
        {
            let spares = lock_unpoisoned(&self.spare_buffers);
            if spares.len() >= self.spare_target {
                return;
            }
        }
        // The public BufferSource contract transfers ownership of each Vec to
        // the consumer and has no return path. Replenishment therefore still
        // allocates once per delivered block, but crucially on the owner
        // thread rather than inside the native completion callback.
        let replacement = Vec::with_capacity(self.buffer_size);
        let mut spares = lock_unpoisoned(&self.spare_buffers);
        if spares.len() < self.spare_target {
            spares.push(replacement);
        }
    }
}

/// Return an already-queued completion immediately, invoking `pump` only when
/// the callback queue is empty. Draining before polling is important under a
/// callback burst: if every consumer call pumps first and removes only one
/// buffer afterward, an existing backlog can never shrink during continuous
/// traffic and will eventually force avoidable bounded-queue overflow.
fn pop_completed_or_pump(
    state: &RingState,
    pump: impl FnOnce() -> i32,
) -> Result<Option<CompletedBuf>, SdrError> {
    if let Some(completed) = state.pop_completed() {
        return Ok(Some(completed));
    }

    let rc = pump();
    if rc == LIBUSB_ERROR_INTERRUPTED {
        return Ok(None);
    }
    if rc != LIBUSB_SUCCESS {
        return Err(map_libusb_event_error(rc));
    }
    Ok(state.pop_completed())
}

struct CallbackGuard<'a>(&'a RingState);

impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.callbacks_active.fetch_sub(1, Ordering::Release);
    }
}

/// State owned by one libusb transfer and exposed to its callback through
/// `user_data` for the full allocated lifetime of that transfer.
struct CallbackSlot {
    ring: Arc<RingState>,
    phase: AtomicU8,
    submit_failures: AtomicUsize,
    buffer_capacity: AtomicUsize,
}

impl CallbackSlot {
    fn new(ring: Arc<RingState>, buffer_capacity: usize) -> Self {
        Self {
            ring,
            phase: AtomicU8::new(SLOT_PENDING),
            submit_failures: AtomicUsize::new(0),
            buffer_capacity: AtomicUsize::new(buffer_capacity),
        }
    }

    fn mark_terminal(&self) {
        if self.phase.swap(SLOT_TERMINAL, Ordering::AcqRel) != SLOT_TERMINAL {
            self.ring.terminal_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Submit a currently-pending slot. Publishing `SLOT_SUBMITTED` before the
/// FFI call fences against teardown: Drop will either see the pending slot and
/// terminalize it, or see the submitted slot and cancel it.
///
/// # Safety
///
/// `transfer` and its `user_data` must remain allocated. The slot must not be
/// actively submitted when this function is called.
unsafe fn submit_pending(transfer: *mut usb::libusb_transfer) -> bool {
    let tr = unsafe { &mut *transfer };
    let slot = unsafe { &*(tr.user_data.cast::<CallbackSlot>()) };
    if slot.ring.stopping.load(Ordering::Acquire)
        || slot
            .phase
            .compare_exchange(
                SLOT_PENDING,
                SLOT_SUBMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        return false;
    }

    let rc = unsafe { usb::libusb_submit_transfer(transfer) };
    if rc == LIBUSB_SUCCESS {
        slot.submit_failures.store(0, Ordering::Relaxed);
        post_submit_cancel_if_stopping(slot, || unsafe { usb::libusb_cancel_transfer(transfer) });
        true
    } else {
        let _ = slot.phase.compare_exchange(
            SLOT_SUBMITTED,
            SLOT_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let failures = slot.submit_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= MAX_SUBMIT_ERRORS {
            slot.ring.fail(FAILURE_TRANSPORT);
            slot.mark_terminal();
        } else {
            slot.ring.enqueue_recovery(transfer, RecoveryKind::Resubmit);
        }
        false
    }
}

fn post_submit_cancel_if_stopping(slot: &CallbackSlot, cancel: impl FnOnce() -> i32) -> bool {
    if slot.ring.stopping.load(Ordering::Acquire) {
        // begin_quiesce may have observed SLOT_SUBMITTED and received
        // NOT_FOUND just before native submission. A post-submit stop check
        // closes that publish/submit window. Ownership still transitions only
        // through the eventual callback, regardless of this return code.
        let _ = cancel();
        true
    } else {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionDisposition {
    Enqueued,
    DroppedFull,
    DroppedNoSpare,
    InvalidSpare,
}

fn record_callback_drop(state: &RingState, actual: usize) {
    state
        .samples_dropped
        .fetch_add((actual / 2) as u64, Ordering::Relaxed);
    state.dropped_blocks.fetch_add(1, Ordering::Relaxed);
}

/// Rotate one callback-owned native buffer into the completed queue without
/// allocating, reserving, or constructing a reference to uninitialized bytes.
///
/// # Safety
///
/// `tr.buffer` must be the allocation described by `slot.buffer_capacity`, and
/// libusb must have initialized exactly `0..actual_length` before this call.
unsafe fn rotate_completed_buffer(
    tr: &mut usb::libusb_transfer,
    slot: &CallbackSlot,
    timestamp: Instant,
    sequence: u64,
) -> CompletionDisposition {
    let requested = tr.length.max(0) as usize;
    let actual = (tr.actual_length.max(0) as usize).min(requested);
    let current_capacity = slot.buffer_capacity.load(Ordering::Acquire);
    if tr.buffer.is_null() || requested == 0 || current_capacity < requested {
        slot.ring.fail(FAILURE_TRANSPORT);
        return CompletionDisposition::InvalidSpare;
    }

    let mut queue = lock_unpoisoned(&slot.ring.completed);
    if queue.len() == slot.ring.queue_cap {
        record_callback_drop(&slot.ring, actual);
        return CompletionDisposition::DroppedFull;
    }

    let mut spares = lock_unpoisoned(&slot.ring.spare_buffers);
    let Some(mut replacement) = spares.pop() else {
        record_callback_drop(&slot.ring, actual);
        return CompletionDisposition::DroppedNoSpare;
    };
    replacement.clear();
    if replacement.capacity() < requested {
        // This violates a construction invariant. Keep ownership in the ring
        // (so callback Drop does not touch the allocator), fail closed, and
        // leave the current native allocation terminally reclaimable.
        spares.push(replacement);
        record_callback_drop(&slot.ring, actual);
        slot.ring.failed_transfers.fetch_add(1, Ordering::Relaxed);
        slot.ring.fail(FAILURE_TRANSPORT);
        return CompletionDisposition::InvalidSpare;
    }
    let replacement_ptr = replacement.as_mut_ptr();
    let replacement_capacity = replacement.capacity();
    std::mem::forget(replacement);
    drop(spares);

    let completed = unsafe { reclaim_buffer(tr.buffer, actual, current_capacity) };
    tr.buffer = replacement_ptr;
    slot.buffer_capacity
        .store(replacement_capacity, Ordering::Release);
    queue.push_back(CompletedBuf {
        buffer: completed,
        timestamp,
        sequence,
    });
    slot.ring
        .high_water_mark
        .fetch_max(queue.len(), Ordering::Relaxed);
    CompletionDisposition::Enqueued
}

/// libusb completion callback. It performs only ownership swaps, bounded queue
/// insertion, and re-submission. Synchronous endpoint recovery is deferred to
/// the source's event thread.
extern "system" fn transfer_callback(transfer: *mut usb::libusb_transfer) {
    if transfer.is_null() {
        return;
    }
    let tr = unsafe { &mut *transfer };
    if tr.user_data.is_null() {
        return;
    }
    let slot = unsafe { &*(tr.user_data.cast::<CallbackSlot>()) };
    slot.ring.callbacks_active.fetch_add(1, Ordering::AcqRel);
    let _callback_guard = CallbackGuard(&slot.ring);

    // A callback means libusb no longer owns this slot as submitted.
    if slot
        .phase
        .compare_exchange(
            SLOT_SUBMITTED,
            SLOT_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }

    if slot.ring.stopping.load(Ordering::Acquire) {
        slot.mark_terminal();
        return;
    }

    match tr.status {
        LIBUSB_TRANSFER_COMPLETED => {
            let sequence = slot.ring.next_sequence.fetch_add(1, Ordering::Relaxed);
            let disposition =
                unsafe { rotate_completed_buffer(tr, slot, Instant::now(), sequence) };
            match disposition {
                CompletionDisposition::Enqueued
                | CompletionDisposition::DroppedFull
                | CompletionDisposition::DroppedNoSpare => {
                    // No queue lock is held across native submission.
                    let _ = unsafe { submit_pending(transfer) };
                }
                CompletionDisposition::InvalidSpare => slot.mark_terminal(),
            }
        }
        LIBUSB_TRANSFER_STALL => {
            slot.ring.next_sequence.fetch_add(1, Ordering::Relaxed);
            slot.ring.failed_transfers.fetch_add(1, Ordering::Relaxed);
            slot.ring
                .samples_dropped
                .fetch_add((tr.length.max(0) as u64) / 2, Ordering::Relaxed);
            slot.ring.dropped_blocks.fetch_add(1, Ordering::Relaxed);
            slot.ring.enqueue_recovery(transfer, RecoveryKind::Stall);
        }
        LIBUSB_TRANSFER_NO_DEVICE => {
            slot.ring.next_sequence.fetch_add(1, Ordering::Relaxed);
            slot.ring.failed_transfers.fetch_add(1, Ordering::Relaxed);
            slot.ring.fail(FAILURE_DEVICE_LOST);
            slot.mark_terminal();
        }
        LIBUSB_TRANSFER_OVERFLOW
        | LIBUSB_TRANSFER_TIMED_OUT
        | LIBUSB_TRANSFER_ERROR
        | LIBUSB_TRANSFER_CANCELLED => {
            slot.ring.next_sequence.fetch_add(1, Ordering::Relaxed);
            // An unsolicited cancellation is recoverable; teardown sets
            // `stopping` and was handled before the status match. All four
            // statuses lose the current block and keep its allocation for the
            // owner-thread re-submit.
            slot.ring.failed_transfers.fetch_add(1, Ordering::Relaxed);
            if tr.status == LIBUSB_TRANSFER_OVERFLOW {
                slot.ring
                    .hardware_overruns_unknown
                    .fetch_add(1, Ordering::Relaxed);
            }
            slot.ring
                .samples_dropped
                .fetch_add((tr.length.max(0) as u64) / 2, Ordering::Relaxed);
            slot.ring.dropped_blocks.fetch_add(1, Ordering::Relaxed);
            slot.ring.enqueue_recovery(transfer, RecoveryKind::Resubmit);
        }
        _ => {
            slot.ring.fail(FAILURE_TRANSPORT);
            slot.mark_terminal();
        }
    }
}

struct TransferSlot {
    transfer: *mut usb::libusb_transfer,
    callback: *mut CallbackSlot,
}

impl TransferSlot {
    fn phase(&self) -> u8 {
        unsafe { &*self.callback }.phase.load(Ordering::Acquire)
    }

    fn mark_terminal_if_pending(&self) {
        let callback = unsafe { &*self.callback };
        if callback
            .phase
            .compare_exchange(
                SLOT_PENDING,
                SLOT_TERMINAL,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            callback.ring.terminal_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn ring_is_quiescent(transfers: &[TransferSlot], state: &RingState) -> bool {
    transfers.iter().all(|slot| slot.phase() == SLOT_TERMINAL)
        && state.callbacks_active.load(Ordering::Acquire) == 0
}

fn mark_pending_slots_terminal(transfers: &[TransferSlot]) {
    for slot in transfers {
        slot.mark_terminal_if_pending();
    }
}

/// Reclaim a callback-confirmed terminal ring. Draining first makes a second
/// call a no-op, providing a single ownership point for both inline teardown
/// and the detached reaper.
fn reclaim_terminal_slots(transfers: &mut Vec<TransferSlot>) -> usize {
    let mut reclaimed = 0;
    for slot in transfers.drain(..) {
        debug_assert_eq!(slot.phase(), SLOT_TERMINAL);
        unsafe {
            let transfer = &mut *slot.transfer;
            if !transfer.buffer.is_null() {
                let capacity = (&*slot.callback).buffer_capacity.load(Ordering::Acquire);
                free_buffer(transfer.buffer, capacity);
                transfer.buffer = std::ptr::null_mut();
            }
            transfer.user_data = std::ptr::null_mut();
            drop(Box::from_raw(slot.callback));
            usb::libusb_free_transfer(slot.transfer);
        }
        reclaimed += 1;
    }
    reclaimed
}

/// An anchor that intentionally leaks its value unless explicitly released.
/// A quarantined native ring uses this for the device/context owner and stream
/// lease: if thread creation fails or the reaper panics before quiescence,
/// dropping the job can never close the handle or admit a replacement stream
/// while native code might still reference the ring.
struct LeakUnlessReleased<T> {
    value: Option<T>,
}

impl<T> LeakUnlessReleased<T> {
    fn new(value: T) -> Self {
        Self { value: Some(value) }
    }

    fn release(&mut self) {
        drop(self.value.take());
    }
}

impl<T> Drop for LeakUnlessReleased<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            std::mem::forget(value);
        }
    }
}

/// Sole owner of a ring that could not be callback-confirmed within the
/// caller's bounded Drop budget.
struct QuarantinedRing<H: RawLibusbHandleOwner> {
    handle: LeakUnlessReleased<Arc<H>>,
    context_raw: *mut usb::libusb_context,
    transfers: Vec<TransferSlot>,
    state: Arc<RingState>,
    stream_lease: LeakUnlessReleased<StreamLease>,
}

// SAFETY: this is the same transfer ownership as `RusbAsyncSource`, moved as a
// whole to one reaper thread. Callbacks access only atomics/mutexes in
// `RingState`; terminal phase plus callbacks_active==0 fences reclamation.
unsafe impl<H: RawLibusbHandleOwner> Send for QuarantinedRing<H> {}

impl<H: RawLibusbHandleOwner> QuarantinedRing<H> {
    fn pump_events_once(&self) -> i32 {
        let timeout = event_timeout_for(EVENT_POLL);
        unsafe { usb::libusb_handle_events_timeout(self.context_raw, &raw const timeout) }
    }

    fn reap(mut self) {
        loop {
            mark_pending_slots_terminal(&self.transfers);
            if ring_is_quiescent(&self.transfers, &self.state) {
                let _ = reclaim_terminal_slots(&mut self.transfers);
                lock_unpoisoned(&self.state.completed).clear();
                lock_unpoisoned(&self.state.pending).clear();
                // Native callback-visible allocations are gone. Only now may
                // a new stream acquire the lease and the handle/context close.
                self.stream_lease.release();
                self.handle.release();
                return;
            }

            let rc = self.pump_events_once();
            if rc != LIBUSB_SUCCESS && rc != LIBUSB_ERROR_INTERRUPTED {
                // Event failure proves neither cancellation nor callback
                // completion. Retain the ring indefinitely and keep polling;
                // permanent failure becomes a bounded caller-side leak, not a
                // timed native free.
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn launch_reaper<H: RawLibusbHandleOwner>(quarantine: QuarantinedRing<H>) {
    let result = thread::Builder::new()
        .name("sdr-fox-usb-reaper".into())
        .spawn(move || quarantine.reap());
    match result {
        Ok(handle) => drop(handle), // Detached; the payload owns its lifetime.
        Err(error) => {
            // The failed spawn drops its closure. LeakUnlessReleased retains
            // the handle/context and stream lease, while the raw slots remain
            // intentionally allocated, so this degrades to a safe leak.
            tracing::error!(%error, "failed to start USB transfer reaper; ring retained");
        }
    }
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
///
/// This mirrors `NusbBufferSource`'s budget field for field. The two copies
/// exist because the backends share no recovery module yet; they MUST keep
/// identical semantics, so change both or neither.
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
    /// responsive. This guard is not dead weight just because the current
    /// libusb recovery path only surfaces `Timeout` at the deadline itself:
    /// the semantics must match the nusb backend, whose reap loop yields
    /// early, and any future early yield added here must stay retryable.
    /// Non-timeout errors (including `Cancelled`) also pass through: they
    /// carry more information than exhaustion would, and the worker already
    /// bounds them by its consecutive-error limit.
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

struct LibusbClearCompletion<H: RawLibusbHandleOwner> {
    result: Result<(), SdrError>,
    _handle: Arc<H>,
    stream_lease: Option<StreamLease>,
}

fn launch_libusb_clear<H: RawLibusbHandleOwner>(
    handle: Arc<H>,
    endpoint: u8,
    stream_lease: Option<StreamLease>,
) -> mpsc::Receiver<LibusbClearCompletion<H>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut anchors = RetainOnDrop::new((handle, stream_lease));
    let result = thread::Builder::new()
        .name("sdr-fox-rusb-clear-halt".into())
        .spawn(move || {
            let result = anchors.value().0.clear_halt(endpoint);
            let (handle, stream_lease) = anchors.take();
            let _ = sender.send(LibusbClearCompletion {
                result,
                _handle: handle,
                stream_lease,
            });
        });
    if let Err(error) = result {
        tracing::error!(%error, "failed to start rusb clear-halt worker; stream retained");
    }
    receiver
}

/// Async multi-URB bulk source with callback-fenced transfer ownership.
pub(crate) struct LibusbAsyncSource<H: RawLibusbHandleOwner> {
    handle: Arc<H>,
    context_raw: *mut usb::libusb_context,
    transfers: Vec<TransferSlot>,
    state: Arc<RingState>,
    stop: Option<Arc<AtomicBool>>,
    reported_dropped_blocks: u64,
    reported_dropped_samples: u64,
    reported_failed_transfers: u64,
    reported_unknown_overruns: u64,
    last_capture_timestamp: Option<Instant>,
    last_capture_sequence: Option<u64>,
    stream_lease: Option<StreamLease>,
    stall_endpoint: Option<u8>,
    clear_halt: OwnedOperation<LibusbClearCompletion<H>>,
    recovery: RecoveryBudget,
    suppress_error_accounting: bool,
}

// SAFETY: libusb transfer ownership is fenced by each CallbackSlot phase. The
// source is moved to exactly one stream worker and all callbacks use atomics or
// mutex-protected RingState fields.
unsafe impl<H: RawLibusbHandleOwner> Send for LibusbAsyncSource<H> {}

impl<H: RawLibusbHandleOwner> LibusbAsyncSource<H> {
    /// Allocate and submit an async transfer ring.
    pub(crate) fn new(
        handle: Arc<H>,
        endpoint: u8,
        buffer_size: usize,
        num_transfers: usize,
        queue_cap: usize,
        stream_lease: StreamLease,
    ) -> Result<Self, SdrError> {
        validate_config(buffer_size, num_transfers)?;
        let context_raw = handle.raw_context();
        let state = Arc::new(RingState::new(
            callback_queue_capacity(num_transfers, queue_cap),
            num_transfers,
            buffer_size,
        ));
        let mut source = Self {
            handle,
            context_raw,
            transfers: Vec::with_capacity(num_transfers),
            state,
            stop: None,
            reported_dropped_blocks: 0,
            reported_dropped_samples: 0,
            reported_failed_transfers: 0,
            reported_unknown_overruns: 0,
            last_capture_timestamp: None,
            last_capture_sequence: None,
            stream_lease: Some(stream_lease),
            stall_endpoint: None,
            clear_halt: OwnedOperation::new(),
            recovery: RecoveryBudget::new(STALL_RECOVERY_OPERATION, STALL_RECOVERY_BUDGET),
            suppress_error_accounting: false,
        };

        for _ in 0..num_transfers {
            let transfer = unsafe { usb::libusb_alloc_transfer(0) };
            if transfer.is_null() {
                return Err(SdrError::Transport(
                    "libusb_alloc_transfer returned null".into(),
                ));
            }

            let buffer = allocate_buffer(buffer_size);
            let callback = Box::into_raw(Box::new(CallbackSlot::new(
                source.state.clone(),
                buffer.capacity,
            )));
            unsafe {
                (*transfer).dev_handle = source.handle.raw_handle();
                (*transfer).flags = 0;
                (*transfer).endpoint = endpoint;
                (*transfer).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
                // Streaming transfers are intentionally untimed. A fixed
                // per-URB deadline fails at valid low sample rates; stream
                // cancellation interrupts the event pump independently.
                (*transfer).timeout = 0;
                (*transfer).length = buffer_size as i32;
                (*transfer).actual_length = 0;
                (*transfer).callback = transfer_callback;
                (*transfer).user_data = callback.cast::<c_void>();
                (*transfer).buffer = buffer.ptr;
            }

            source.transfers.push(TransferSlot { transfer, callback });
            if !unsafe { submit_pending(transfer) } {
                // `source` Drop cancels and terminally reaps all previously
                // submitted slots; the failed slot is already pending or
                // terminal and is therefore safe to reclaim there as well.
                return Err(SdrError::Transport(
                    "libusb_submit_transfer failed while constructing ring".into(),
                ));
            }
        }

        Ok(source)
    }

    fn owns_transfer(&self, transfer_addr: usize) -> bool {
        self.transfers
            .iter()
            .any(|slot| slot.transfer as usize == transfer_addr)
    }

    fn pump_events_for(&self, timeout: Duration) -> i32 {
        let timeout = event_timeout_for(timeout);
        unsafe { usb::libusb_handle_events_timeout(self.context_raw, &raw const timeout) }
    }

    fn pump_events_once(&self) -> i32 {
        self.pump_events_for(EVENT_POLL)
    }

    /// Request cancellation without making any inference from a cancellation
    /// return code about callback ownership.
    fn begin_quiesce(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        for slot in &self.transfers {
            if slot.phase() == SLOT_SUBMITTED {
                let rc = unsafe { usb::libusb_cancel_transfer(slot.transfer) };
                // SUCCESS completes asynchronously through the callback.
                // NOT_FOUND means completion is already being delivered; it
                // is still unsafe to free until that callback changes phase.
                if rc != LIBUSB_SUCCESS && rc != LIBUSB_ERROR_NOT_FOUND {
                    self.state.fail(FAILURE_TRANSPORT);
                }
            } else {
                slot.mark_terminal_if_pending();
            }
        }
    }

    /// Reap until all terminal callbacks have fully returned or the caller's
    /// deadline expires. No memory is freed here.
    fn quiesce_until(&mut self, deadline: Instant) -> bool {
        self.begin_quiesce();
        loop {
            mark_pending_slots_terminal(&self.transfers);
            if ring_is_quiescent(&self.transfers, &self.state) {
                return true;
            }

            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let rc = self.pump_events_for(EVENT_POLL.min(deadline.saturating_duration_since(now)));
            if rc != LIBUSB_SUCCESS && rc != LIBUSB_ERROR_INTERRUPTED {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn restart_ring_after_clear(&mut self) {
        self.state.failure.store(FAILURE_NONE, Ordering::Release);
        self.state.terminal_count.store(0, Ordering::Relaxed);
        self.state.stopping.store(false, Ordering::Release);
        for slot in &self.transfers {
            let callback = unsafe { &*slot.callback };
            callback.phase.store(SLOT_PENDING, Ordering::Release);
            callback.submit_failures.store(0, Ordering::Relaxed);
            if !unsafe { submit_pending(slot.transfer) } {
                self.state.fail(FAILURE_TRANSPORT);
                break;
            }
        }
    }

    /// Quiesce the full endpoint ring, then run the unbounded native
    /// `clear_halt` call in one owned background task. The exact task persists
    /// across timeout and stop; no slot is resubmitted before its success.
    fn recover_stall(&mut self, endpoint: u8) -> Result<(), SdrError> {
        // The deadline is CUMULATIVE across the whole recovery episode —
        // fixed when the first attempt begins, never recomputed per call.
        // The stream worker retries `Timeout` without bound, so a fresh
        // per-call budget would let a persistently stalled endpoint cycle
        // recover→Timeout→retry forever, streaming silence indefinitely.
        // Once the episode budget is spent, the attempt-level `Timeout`
        // escalates to the terminal `RecoveryExhausted`.
        let deadline = self.recovery.begin_attempt(Instant::now());
        let result = self.recover_stall_until(endpoint, deadline);
        self.recovery.escalate(result, Instant::now())
    }

    fn recover_stall_until(&mut self, endpoint: u8, deadline: Instant) -> Result<(), SdrError> {
        if !self.clear_halt.is_running() {
            if !self.quiesce_until(deadline) {
                return Err(SdrError::Timeout);
            }
            let receiver =
                launch_libusb_clear(self.handle.clone(), endpoint, self.stream_lease.take());
            self.clear_halt.install(receiver);
        }

        let stop = self.stop.clone();
        match self.clear_halt.poll_until(deadline, CLEAR_HALT_POLL, || {
            stop.as_ref()
                .is_some_and(|stop| stop.load(Ordering::Acquire))
        }) {
            OperationPoll::Complete(completion) => {
                self.stream_lease = completion.stream_lease;
                completion.result?;
                self.restart_ring_after_clear();
                Ok(())
            }
            OperationPoll::TimedOut => Err(SdrError::Timeout),
            OperationPoll::Stopped => Err(SdrError::Cancelled),
            OperationPoll::Disconnected => Err(SdrError::Transport(
                "rusb clear-halt worker stopped without returning ownership".into(),
            )),
        }
    }

    fn process_pending(&mut self) -> Result<(), SdrError> {
        if let Some(endpoint) = self.stall_endpoint {
            self.recover_stall(endpoint)?;
            self.stall_endpoint = None;
            return Ok(());
        }

        if self.state.pending_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        let actions: Vec<_> = lock_unpoisoned(&self.state.pending).drain(..).collect();
        if self
            .state
            .pending_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(actions.len())
            })
            .is_err()
        {
            self.state.fail(FAILURE_TRANSPORT);
            return Err(SdrError::Transport(
                "libusb recovery action counter lost queue coherence".into(),
            ));
        }
        if let Some(stall) = actions
            .iter()
            .find(|action| action.kind == RecoveryKind::Stall)
        {
            if self.owns_transfer(stall.transfer_addr) {
                let endpoint =
                    unsafe { (*(stall.transfer_addr as *mut usb::libusb_transfer)).endpoint };
                self.stall_endpoint = Some(endpoint);
                self.recover_stall(endpoint)?;
                self.stall_endpoint = None;
            } else {
                self.state.fail(FAILURE_TRANSPORT);
            }
            return Ok(());
        }

        for action in actions {
            if action.kind != RecoveryKind::Resubmit || !self.owns_transfer(action.transfer_addr) {
                self.state.fail(FAILURE_TRANSPORT);
                continue;
            }
            let transfer = action.transfer_addr as *mut usb::libusb_transfer;
            let _ = unsafe { submit_pending(transfer) };
        }
        Ok(())
    }
}

/// Desktop rusb wrapper around the shared libusb async ring. Keeping this
/// concrete public type preserves the existing API while Android reuses the
/// crate-private generic engine with an fd-backed owner.
pub struct RusbAsyncSource {
    inner: LibusbAsyncSource<DeviceHandle<GlobalContext>>,
}

impl RusbAsyncSource {
    pub(crate) fn new(
        handle: Arc<DeviceHandle<GlobalContext>>,
        endpoint: u8,
        buffer_size: usize,
        num_transfers: usize,
        queue_cap: usize,
        stream_lease: StreamLease,
    ) -> Result<Self, SdrError> {
        Ok(Self {
            inner: LibusbAsyncSource::new(
                handle,
                endpoint,
                buffer_size,
                num_transfers,
                queue_cap,
                stream_lease,
            )?,
        })
    }
}

impl<H: RawLibusbHandleOwner> BufferSource for LibusbAsyncSource<H> {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        loop {
            if self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::Acquire))
            {
                return Err(SdrError::Cancelled);
            }
            if let Some(error) = self.state.error() {
                self.suppress_error_accounting = true;
                return Err(error);
            }

            if let Err(error) = self.process_pending() {
                self.suppress_error_accounting = true;
                return Err(error);
            }
            if let Some(error) = self.state.error() {
                self.suppress_error_accounting = true;
                return Err(error);
            }

            if let Some(completed) = pop_completed_or_pump(&self.state, || self.pump_events_once())?
            {
                self.last_capture_timestamp = Some(completed.timestamp);
                self.last_capture_sequence = Some(completed.sequence);
                // A delivered buffer is the ONLY event that closes a recovery
                // episode. Recovery merely succeeding must not: a stall→clear→
                // stall cycle that never yields data would re-arm its own
                // budget forever, recreating the silent non-terminal stream
                // the budget exists to prevent.
                self.recovery.note_forward_progress();
                return Ok(completed.buffer);
            }
        }
    }

    fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.stop = Some(stop);
    }

    fn stop_waker(&self) -> Option<sdr_fox_core::sample::StreamStopHandle> {
        Some(event_stop_handle(Arc::clone(&self.handle)))
    }

    fn take_stats_delta(&mut self) -> (u64, u64, u64, u64) {
        let blocks = self.state.dropped_blocks.load(Ordering::Relaxed);
        let samples = self.state.samples_dropped.load(Ordering::Relaxed);
        let failed = self.state.failed_transfers.load(Ordering::Relaxed);
        let unknown = self.state.hardware_overruns_unknown.load(Ordering::Relaxed);
        let delta = (
            blocks.saturating_sub(self.reported_dropped_blocks),
            samples.saturating_sub(self.reported_dropped_samples),
            failed.saturating_sub(self.reported_failed_transfers),
            unknown.saturating_sub(self.reported_unknown_overruns),
        );
        self.reported_dropped_blocks = blocks;
        self.reported_dropped_samples = samples;
        self.reported_failed_transfers = failed;
        self.reported_unknown_overruns = unknown;
        delta
    }

    fn capture_timestamp(&self) -> Option<Instant> {
        self.last_capture_timestamp
    }

    fn capture_sequence(&self) -> Option<u64> {
        self.last_capture_sequence
    }

    fn suppress_error_accounting(&mut self) -> bool {
        std::mem::take(&mut self.suppress_error_accounting)
    }
}

impl BufferSource for RusbAsyncSource {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        self.inner.next_buffer()
    }

    fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.inner.set_stop(stop);
    }

    fn stop_waker(&self) -> Option<sdr_fox_core::sample::StreamStopHandle> {
        self.inner.stop_waker()
    }

    fn take_stats_delta(&mut self) -> (u64, u64, u64, u64) {
        self.inner.take_stats_delta()
    }

    fn capture_timestamp(&self) -> Option<Instant> {
        self.inner.capture_timestamp()
    }

    fn capture_sequence(&self) -> Option<u64> {
        self.inner.capture_sequence()
    }

    fn suppress_error_accounting(&mut self) -> bool {
        self.inner.suppress_error_accounting()
    }
}

impl<H: RawLibusbHandleOwner> Drop for LibusbAsyncSource<H> {
    fn drop(&mut self) {
        if self.quiesce_until(Instant::now() + INLINE_REAP_BUDGET) {
            let _ = reclaim_terminal_slots(&mut self.transfers);
            lock_unpoisoned(&self.state.completed).clear();
            lock_unpoisoned(&self.state.pending).clear();
            return;
        }

        let stream_lease = self
            .stream_lease
            .take()
            .expect("stream lease moved to the reaper at most once");
        let quarantine = QuarantinedRing {
            handle: LeakUnlessReleased::new(self.handle.clone()),
            context_raw: self.context_raw,
            transfers: std::mem::take(&mut self.transfers),
            state: self.state.clone(),
            stream_lease: LeakUnlessReleased::new(stream_lease),
        };
        launch_reaper(quarantine);
    }
}

fn validate_config(buffer_size: usize, num_transfers: usize) -> Result<(), SdrError> {
    if buffer_size == 0 || buffer_size % 512 != 0 || buffer_size > i32::MAX as usize {
        return Err(SdrError::InvalidParameter(format!(
            "buffer_size must be a positive 512-byte multiple no larger than i32::MAX, got {buffer_size}"
        )));
    }
    if num_transfers == 0 {
        return Err(SdrError::InvalidParameter(
            "num_transfers must be greater than zero".into(),
        ));
    }
    Ok(())
}

const fn callback_queue_capacity(num_transfers: usize, requested: usize) -> usize {
    if requested < num_transfers {
        num_transfers
    } else {
        requested
    }
}

fn map_libusb_event_error(rc: i32) -> SdrError {
    match rc {
        libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE => SdrError::DeviceLost,
        libusb1_sys::constants::LIBUSB_ERROR_TIMEOUT => SdrError::Timeout,
        libusb1_sys::constants::LIBUSB_ERROR_PIPE => SdrError::Stall,
        libusb1_sys::constants::LIBUSB_ERROR_OVERFLOW => SdrError::Overflow { dropped_samples: 0 },
        _ => SdrError::Transport(format!("libusb_handle_events_timeout failed: {rc}")),
    }
}

fn map_libusb_operation_error(rc: i32, operation: &str) -> SdrError {
    match rc {
        libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE => SdrError::DeviceLost,
        libusb1_sys::constants::LIBUSB_ERROR_TIMEOUT => SdrError::Timeout,
        libusb1_sys::constants::LIBUSB_ERROR_PIPE => SdrError::Stall,
        libusb1_sys::constants::LIBUSB_ERROR_OVERFLOW => SdrError::Overflow { dropped_samples: 0 },
        libusb1_sys::constants::LIBUSB_ERROR_BUSY => SdrError::DeviceBusy,
        libusb1_sys::constants::LIBUSB_ERROR_INVALID_PARAM => {
            SdrError::InvalidParameter(format!("{operation}: libusb error {rc}"))
        }
        _ => SdrError::Transport(format!("{operation}: libusb error {rc}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn allocated_test_slot(ring: &Arc<RingState>, phase: u8) -> TransferSlot {
        let buffer = allocate_buffer(512);
        let callback = Box::into_raw(Box::new(CallbackSlot::new(ring.clone(), buffer.capacity)));
        unsafe { &*callback }.phase.store(phase, Ordering::Release);
        let transfer = unsafe { usb::libusb_alloc_transfer(0) };
        assert!(!transfer.is_null());
        unsafe {
            (*transfer).length = 512;
            (*transfer).buffer = buffer.ptr;
            (*transfer).user_data = callback.cast::<c_void>();
        }
        TransferSlot { transfer, callback }
    }

    #[test]
    fn configuration_rejects_zero_and_unrepresentable_values() {
        assert!(validate_config(0, 1).is_err());
        assert!(validate_config(511, 1).is_err());
        assert!(validate_config(512, 0).is_err());
        assert!(validate_config((i32::MAX as usize) + 1, 1).is_err());
        assert!(validate_config(65_536, 16).is_ok());
        assert_eq!(callback_queue_capacity(16, 2), 16);
        assert_eq!(callback_queue_capacity(16, 32), 32);
    }

    fn bare_completion(
        ring: Arc<RingState>,
        requested: usize,
        actual: usize,
    ) -> (Box<CallbackSlot>, *mut usb::libusb_transfer, usize) {
        let native = allocate_buffer(requested);
        for index in 0..actual {
            unsafe { native.ptr.add(index).write(index as u8) };
        }
        let callback = Box::new(CallbackSlot::new(ring, native.capacity));
        let transfer = unsafe { usb::libusb_alloc_transfer(0) };
        assert!(!transfer.is_null());
        unsafe {
            (*transfer).length = requested as i32;
            (*transfer).actual_length = actual as i32;
            (*transfer).buffer = native.ptr;
        }
        (callback, transfer, native.capacity)
    }

    unsafe fn free_bare_completion(callback: &CallbackSlot, transfer: *mut usb::libusb_transfer) {
        let capacity = callback.buffer_capacity.load(Ordering::Acquire);
        unsafe { free_buffer((*transfer).buffer, capacity) };
        unsafe { usb::libusb_free_transfer(transfer) };
    }

    #[test]
    fn completion_rotation_is_allocation_free_and_fail_closed() {
        // Healthy completion swaps in one preallocated spare and reconstructs
        // only the initialized prefix of the native buffer.
        let ring = Arc::new(RingState::new(1, 1, 512));
        let (callback, transfer, original_capacity) = bare_completion(ring.clone(), 512, 8);
        let original_ptr = unsafe { (*transfer).buffer };
        assert_eq!(
            unsafe { rotate_completed_buffer(&mut *transfer, &callback, Instant::now(), 0) },
            CompletionDisposition::Enqueued
        );
        assert_ne!(unsafe { (*transfer).buffer }, original_ptr);
        assert_eq!(lock_unpoisoned(&ring.spare_buffers).len(), 0);
        let completed = lock_unpoisoned(&ring.completed).pop_front().unwrap();
        assert_eq!(completed.buffer, (0_u8..8).collect::<Vec<_>>());
        assert_eq!(completed.buffer.capacity(), original_capacity);
        unsafe { free_bare_completion(&callback, transfer) };

        // Full queue and empty spare both retain the current pointer for
        // in-place resubmission and account exactly one dropped block.
        let full = Arc::new(RingState::new(1, 1, 512));
        full.enqueue_completed(vec![7; 4], Instant::now());
        let (callback, transfer, _) = bare_completion(full.clone(), 512, 10);
        let current = unsafe { (*transfer).buffer };
        assert_eq!(
            unsafe { rotate_completed_buffer(&mut *transfer, &callback, Instant::now(), 1) },
            CompletionDisposition::DroppedFull
        );
        assert_eq!(unsafe { (*transfer).buffer }, current);
        assert_eq!(full.dropped_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(full.samples_dropped.load(Ordering::Relaxed), 5);
        unsafe { free_bare_completion(&callback, transfer) };

        let empty = Arc::new(RingState::new(1, 1, 512));
        lock_unpoisoned(&empty.spare_buffers).clear();
        let (callback, transfer, _) = bare_completion(empty.clone(), 512, 12);
        let current = unsafe { (*transfer).buffer };
        assert_eq!(
            unsafe { rotate_completed_buffer(&mut *transfer, &callback, Instant::now(), 2) },
            CompletionDisposition::DroppedNoSpare
        );
        assert_eq!(unsafe { (*transfer).buffer }, current);
        assert_eq!(empty.dropped_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(empty.samples_dropped.load(Ordering::Relaxed), 6);
        unsafe { free_bare_completion(&callback, transfer) };

        // An undersized spare is retained, never reserved in the callback,
        // and terminally fails the ring without changing current ownership.
        let undersized = Arc::new(RingState::new(1, 1, 1));
        let (callback, transfer, _) = bare_completion(undersized.clone(), 512, 14);
        let current = unsafe { (*transfer).buffer };
        assert_eq!(
            unsafe { rotate_completed_buffer(&mut *transfer, &callback, Instant::now(), 3) },
            CompletionDisposition::InvalidSpare
        );
        assert_eq!(unsafe { (*transfer).buffer }, current);
        assert_eq!(lock_unpoisoned(&undersized.spare_buffers).len(), 1);
        assert!(matches!(undersized.error(), Some(SdrError::Transport(_))));
        assert_eq!(undersized.dropped_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(undersized.samples_dropped.load(Ordering::Relaxed), 7);
        unsafe { free_bare_completion(&callback, transfer) };
    }

    #[test]
    fn callback_queue_is_bounded_and_counts_exact_cu8_samples() {
        let state = RingState::new(2, 1, 512);
        state.enqueue_completed(vec![1; 8], Instant::now());
        state.enqueue_completed(vec![2; 10], Instant::now());
        state.enqueue_completed(vec![3; 12], Instant::now());

        let queue = lock_unpoisoned(&state.completed);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.front().unwrap().buffer[0], 1);
        assert_eq!(queue.back().unwrap().buffer[0], 2);
        assert_eq!(state.samples_dropped.load(Ordering::Relaxed), 6);
        assert_eq!(state.dropped_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(state.high_water_mark.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn queued_callback_backlog_drains_before_event_pump() {
        let state = RingState::new(4, 2, 512);
        state.enqueue_completed(vec![1; 8], Instant::now());
        state.enqueue_completed(vec![2; 8], Instant::now());
        let pump_calls = AtomicUsize::new(0);

        let first = pop_completed_or_pump(&state, || {
            pump_calls.fetch_add(1, Ordering::Relaxed);
            LIBUSB_SUCCESS
        })
        .unwrap()
        .unwrap();
        let second = pop_completed_or_pump(&state, || {
            pump_calls.fetch_add(1, Ordering::Relaxed);
            LIBUSB_SUCCESS
        })
        .unwrap()
        .unwrap();

        assert_eq!(first.buffer[0], 1);
        assert_eq!(second.buffer[0], 2);
        assert_eq!(pump_calls.load(Ordering::Relaxed), 0);

        assert!(pop_completed_or_pump(&state, || {
            pump_calls.fetch_add(1, Ordering::Relaxed);
            LIBUSB_SUCCESS
        })
        .unwrap()
        .is_none());
        assert_eq!(pump_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn terminal_transition_is_idempotent() {
        let ring = Arc::new(RingState::new(1, 1, 512));
        let slot = CallbackSlot::new(ring.clone(), 512);
        slot.mark_terminal();
        slot.mark_terminal();
        assert_eq!(slot.phase.load(Ordering::Relaxed), SLOT_TERMINAL);
        assert_eq!(ring.terminal_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn post_submit_stop_closes_pre_submit_cancel_window() {
        let ring = Arc::new(RingState::new(1, 1, 512));
        let slot = CallbackSlot::new(ring.clone(), 512);
        slot.phase.store(SLOT_SUBMITTED, Ordering::Release);
        ring.stopping.store(true, Ordering::Release);
        let cancels = AtomicUsize::new(0);
        assert!(post_submit_cancel_if_stopping(&slot, || {
            cancels.fetch_add(1, Ordering::Relaxed);
            LIBUSB_ERROR_NOT_FOUND
        }));
        assert_eq!(cancels.load(Ordering::Relaxed), 1);
        assert_eq!(slot.phase.load(Ordering::Acquire), SLOT_SUBMITTED);

        ring.stopping.store(false, Ordering::Release);
        assert!(!post_submit_cancel_if_stopping(&slot, || {
            cancels.fetch_add(1, Ordering::Relaxed);
            LIBUSB_SUCCESS
        }));
        assert_eq!(cancels.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn retained_stop_waker_invokes_interrupt_and_anchors_owner() {
        struct FakeOwner {
            interrupts: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
        }

        impl Drop for FakeOwner {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        // SAFETY: the test overrides interrupt_events and never submits either
        // null pointer to libusb.
        unsafe impl RawLibusbHandleOwner for FakeOwner {
            fn raw_handle(&self) -> *mut usb::libusb_device_handle {
                std::ptr::null_mut()
            }

            fn raw_context(&self) -> *mut usb::libusb_context {
                std::ptr::null_mut()
            }

            fn interrupt_events(&self) {
                self.interrupts.fetch_add(1, Ordering::Relaxed);
            }
        }

        let interrupts = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(FakeOwner {
            interrupts: Arc::clone(&interrupts),
            drops: Arc::clone(&drops),
        });
        let stop = event_stop_handle(Arc::clone(&owner));
        drop(owner);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        stop.stop();
        assert_eq!(interrupts.load(Ordering::Relaxed), 1);
        drop(stop);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn permanent_event_failure_or_lost_callback_never_authorizes_reclamation() {
        let ring = Arc::new(RingState::new(1, 2, 512));
        let mut transfers = vec![
            allocated_test_slot(&ring, SLOT_SUBMITTED),
            allocated_test_slot(&ring, SLOT_SUBMITTED),
        ];

        // Repeated event-pump errors provide no ownership evidence. A
        // submitted slot must remain non-terminal and therefore quarantined.
        for _ in 0..16 {
            mark_pending_slots_terminal(&transfers);
            assert!(!ring_is_quiescent(&transfers, &ring));
        }
        assert_eq!(Arc::strong_count(&ring), 3);

        // Test cleanup simulates eventual terminal callbacks; production does
        // not perform this transition on an event error.
        for slot in &transfers {
            unsafe { &*slot.callback }
                .phase
                .store(SLOT_TERMINAL, Ordering::Release);
        }
        assert!(ring_is_quiescent(&transfers, &ring));
        assert_eq!(reclaim_terminal_slots(&mut transfers), 2);
        assert_eq!(reclaim_terminal_slots(&mut transfers), 0);
        assert_eq!(Arc::strong_count(&ring), 1);
    }

    #[test]
    fn partial_startup_ring_reaps_eventual_callbacks_exactly_once() {
        let ring = Arc::new(RingState::new(1, 2, 512));
        let mut transfers = vec![
            allocated_test_slot(&ring, SLOT_PENDING),
            allocated_test_slot(&ring, SLOT_SUBMITTED),
        ];

        // A submit failure leaves one slot pending while an earlier slot is
        // still callback-visible. Only the pending slot can terminalize now.
        mark_pending_slots_terminal(&transfers);
        assert_eq!(transfers[0].phase(), SLOT_TERMINAL);
        assert_eq!(transfers[1].phase(), SLOT_SUBMITTED);
        assert!(!ring_is_quiescent(&transfers, &ring));

        // The submitted slot eventually delivers its terminal callback. Its
        // active guard must return before either allocation can be reclaimed.
        unsafe { &*transfers[1].callback }
            .phase
            .store(SLOT_TERMINAL, Ordering::Release);
        ring.callbacks_active.store(1, Ordering::Release);
        assert!(!ring_is_quiescent(&transfers, &ring));
        ring.callbacks_active.store(0, Ordering::Release);
        assert!(ring_is_quiescent(&transfers, &ring));

        assert_eq!(reclaim_terminal_slots(&mut transfers), 2);
        assert!(transfers.is_empty());
        assert_eq!(reclaim_terminal_slots(&mut transfers), 0);
        assert_eq!(Arc::strong_count(&ring), 1);
    }

    #[test]
    fn failed_reaper_start_retains_anchors_while_success_releases_once() {
        struct DropProbe<'a>(&'a AtomicUsize);
        impl Drop for DropProbe<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let leaked_drops = AtomicUsize::new(0);
        {
            // Models the closure payload being dropped when Builder::spawn
            // fails: the anchor itself must not run Drop.
            let _anchor = LeakUnlessReleased::new(DropProbe(&leaked_drops));
        }
        assert_eq!(leaked_drops.load(Ordering::Relaxed), 0);

        let released_drops = AtomicUsize::new(0);
        {
            let mut anchor = LeakUnlessReleased::new(DropProbe(&released_drops));
            anchor.release();
            anchor.release();
        }
        assert_eq!(released_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocked_rusb_clear_is_polled_without_duplicate_quiesce_or_launch() {
        let (sender, receiver) = mpsc::channel();
        let mut clear = OwnedOperation::new();
        let mut prepare_count = 0;
        if !clear.is_running() {
            prepare_count += 1;
            clear.install(receiver);
        }
        assert!(matches!(
            clear.poll_until(
                Instant::now() + Duration::from_millis(5),
                CLEAR_HALT_POLL,
                || false
            ),
            OperationPoll::TimedOut
        ));
        if !clear.is_running() {
            prepare_count += 1;
        }
        assert_eq!(prepare_count, 1);

        sender.send(()).unwrap();
        assert!(matches!(
            clear.poll_until(
                Instant::now() + Duration::from_secs(1),
                CLEAR_HALT_POLL,
                || false
            ),
            OperationPoll::Complete(())
        ));
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
        // Any recovery yield that happens before the cumulative deadline is a
        // worker-yield, not a failure; it must stay retryable so recovery can
        // resume and the stop flag keeps getting polled.
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
        assert!(matches!(
            budget.escalate(
                Err(SdrError::Transport("clear-halt worker lost".into())),
                late
            ),
            Err(SdrError::Transport(_))
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

    /// Handle owner that scripts `clear_halt` entirely in Rust.
    ///
    /// Its raw pointers are null, so a source built on it MUST keep an empty
    /// transfer ring and a non-empty completion queue: nothing here may reach
    /// `libusb_submit_transfer`, `libusb_cancel_transfer`, or the event pump.
    struct ScriptedClearOwner {
        clears: AtomicUsize,
        clear_delay: Duration,
    }

    impl ScriptedClearOwner {
        fn new(clear_delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                clears: AtomicUsize::new(0),
                clear_delay,
            })
        }
    }

    // SAFETY: every method is overridden or unused; the null handle and
    // context are never handed to libusb by the tests that use this owner.
    unsafe impl RawLibusbHandleOwner for ScriptedClearOwner {
        fn raw_handle(&self) -> *mut usb::libusb_device_handle {
            std::ptr::null_mut()
        }

        fn raw_context(&self) -> *mut usb::libusb_context {
            std::ptr::null_mut()
        }

        fn clear_halt(&self, _endpoint: u8) -> Result<(), SdrError> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            thread::sleep(self.clear_delay);
            // The endpoint halt clears every time. That is the interesting
            // case: recovery "works" and the endpoint stalls again anyway.
            Ok(())
        }

        fn interrupt_events(&self) {}
    }

    fn scripted_source(
        handle: Arc<ScriptedClearOwner>,
        budget: Duration,
    ) -> LibusbAsyncSource<ScriptedClearOwner> {
        let busy = Arc::new(AtomicBool::new(false));
        let stream_lease = StreamLease::acquire(&busy).expect("fresh transport is not streaming");
        LibusbAsyncSource {
            handle,
            context_raw: std::ptr::null_mut(),
            // Empty by construction: see ScriptedClearOwner's contract.
            transfers: Vec::new(),
            state: Arc::new(RingState::new(1, 1, 512)),
            stop: None,
            reported_dropped_blocks: 0,
            reported_dropped_samples: 0,
            reported_failed_transfers: 0,
            reported_unknown_overruns: 0,
            last_capture_timestamp: None,
            last_capture_sequence: None,
            stream_lease: Some(stream_lease),
            stall_endpoint: None,
            clear_halt: OwnedOperation::new(),
            recovery: RecoveryBudget::new(STALL_RECOVERY_OPERATION, budget),
            suppress_error_accounting: false,
        }
    }

    #[test]
    fn endless_stall_clear_stall_terminates_with_recovery_exhausted() {
        let budget = Duration::from_millis(200);
        let handle = ScriptedClearOwner::new(Duration::from_millis(2));
        let mut source = scripted_source(Arc::clone(&handle), budget);

        let started = Instant::now();
        let mut outcome = None;
        // Each iteration is one full "endpoint stalled again" report from the
        // callback: recovery succeeds, no buffer is ever delivered. Before the
        // cumulative budget this cycled forever and the stream stayed alive
        // and silent. The iteration cap turns a regression into a failing
        // test rather than a hung one.
        for _ in 0..2_000 {
            source.stall_endpoint = Some(0x81);
            if let Err(error) = source.process_pending() {
                outcome = Some(error);
                break;
            }
        }
        let wall_clock = started.elapsed();

        match outcome {
            Some(SdrError::RecoveryExhausted {
                operation,
                elapsed,
                attempts,
            }) => {
                assert_eq!(operation, STALL_RECOVERY_OPERATION);
                // More than one attempt shared the single episode budget:
                // that is precisely what a renewed per-attempt deadline
                // would have made unbounded.
                assert!(
                    attempts >= 2,
                    "expected a multi-attempt episode, got {attempts}"
                );
                assert!(elapsed >= budget, "{elapsed:?} < {budget:?}");
            }
            other => panic!("stalled endpoint must end with RecoveryExhausted, got {other:?}"),
        }
        assert!(
            wall_clock < Duration::from_secs(5),
            "recovery must be bounded in wall-clock time, took {wall_clock:?}"
        );
        assert!(handle.clears.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn recovered_stall_that_delivers_keeps_streaming_on_a_full_budget() {
        let budget = Duration::from_millis(500);
        let handle = ScriptedClearOwner::new(Duration::ZERO);
        let mut source = scripted_source(Arc::clone(&handle), budget);
        // Pre-queued so next_buffer never reaches the native event pump; the
        // scripted owner has no libusb context to pump.
        source.state.enqueue_completed(vec![9; 8], Instant::now());
        source.stall_endpoint = Some(0x81);

        let buffer = source
            .next_buffer()
            .expect("a cleared stall followed by a completion delivers");
        assert_eq!(buffer, vec![9; 8]);
        assert_eq!(handle.clears.load(Ordering::Relaxed), 1);
        assert!(
            source.stream_lease.is_some(),
            "clear-halt returns the lease"
        );
        // Delivery — not the successful clear — is what closed the episode.
        assert!(source.recovery.episode.is_none());

        // So a stall arbitrarily far into the stream starts from the whole
        // budget again. Synthetic time keeps that exact without sleeping it
        // away, and without pretending the earlier episode never happened.
        let much_later = Instant::now() + Duration::from_secs(3_600);
        assert_eq!(
            source.recovery.begin_attempt(much_later),
            much_later + budget
        );
        assert!(matches!(
            source.recovery.escalate(
                Err(SdrError::Timeout),
                much_later + Duration::from_millis(1)
            ),
            Err(SdrError::Timeout)
        ));
    }

    #[test]
    fn recovery_action_carries_the_exact_transfer_identity() {
        let state = RingState::new(1, 1, 512);
        let transfer = 0x1234usize as *mut usb::libusb_transfer;
        state.enqueue_recovery(transfer, RecoveryKind::Stall);
        let action = lock_unpoisoned(&state.pending).pop_front().unwrap();
        assert_eq!(action.transfer_addr, transfer as usize);
        assert_eq!(action.kind, RecoveryKind::Stall);
    }

    #[test]
    fn typed_event_errors_preserve_retry_semantics() {
        assert!(matches!(
            map_libusb_event_error(libusb1_sys::constants::LIBUSB_ERROR_TIMEOUT),
            SdrError::Timeout
        ));
        assert!(matches!(
            map_libusb_event_error(libusb1_sys::constants::LIBUSB_ERROR_PIPE),
            SdrError::Stall
        ));
        assert!(matches!(
            map_libusb_event_error(libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE),
            SdrError::DeviceLost
        ));
    }

    #[test]
    fn no_device_callback_is_terminal_and_fully_returns() {
        let ring = Arc::new(RingState::new(1, 1, 512));
        let buffer = allocate_buffer(512);
        let callback = Box::into_raw(Box::new(CallbackSlot::new(ring.clone(), buffer.capacity)));
        unsafe { &*callback }
            .phase
            .store(SLOT_SUBMITTED, Ordering::Release);
        let transfer = unsafe { usb::libusb_alloc_transfer(0) };
        assert!(!transfer.is_null());
        unsafe {
            (*transfer).length = 512;
            (*transfer).status = LIBUSB_TRANSFER_NO_DEVICE;
            (*transfer).buffer = buffer.ptr;
            (*transfer).user_data = callback.cast::<c_void>();
        }

        transfer_callback(transfer);
        assert_eq!(
            unsafe { &*callback }.phase.load(Ordering::Acquire),
            SLOT_TERMINAL
        );
        assert_eq!(ring.callbacks_active.load(Ordering::Acquire), 0);
        assert!(matches!(ring.error(), Some(SdrError::DeviceLost)));

        unsafe {
            free_buffer((*transfer).buffer, buffer.capacity);
            drop(Box::from_raw(callback));
            usb::libusb_free_transfer(transfer);
        }
    }

    #[test]
    fn transient_callback_queues_resubmit_for_its_own_slot() {
        let ring = Arc::new(RingState::new(1, 1, 512));
        let buffer = allocate_buffer(512);
        let callback = Box::into_raw(Box::new(CallbackSlot::new(ring.clone(), buffer.capacity)));
        unsafe { &*callback }
            .phase
            .store(SLOT_SUBMITTED, Ordering::Release);
        let transfer = unsafe { usb::libusb_alloc_transfer(0) };
        assert!(!transfer.is_null());
        unsafe {
            (*transfer).length = 512;
            (*transfer).status = LIBUSB_TRANSFER_OVERFLOW;
            (*transfer).buffer = buffer.ptr;
            (*transfer).user_data = callback.cast::<c_void>();
        }

        transfer_callback(transfer);
        let action = lock_unpoisoned(&ring.pending).pop_front().unwrap();
        assert_eq!(action.transfer_addr, transfer as usize);
        assert_eq!(action.kind, RecoveryKind::Resubmit);
        assert_eq!(ring.samples_dropped.load(Ordering::Relaxed), 256);
        assert_eq!(ring.dropped_blocks.load(Ordering::Relaxed), 1);
        assert_eq!(ring.failed_transfers.load(Ordering::Relaxed), 1);
        assert_eq!(ring.hardware_overruns_unknown.load(Ordering::Relaxed), 1);
        assert_eq!(ring.callbacks_active.load(Ordering::Acquire), 0);

        unsafe {
            free_buffer((*transfer).buffer, buffer.capacity);
            drop(Box::from_raw(callback));
            usb::libusb_free_transfer(transfer);
        }
    }
}
