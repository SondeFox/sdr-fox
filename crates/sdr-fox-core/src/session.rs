//! Shared hardware-session lifetime and dead-handle signalling.
//!
//! Both halves of this module exist because of the same fact: an opened SDR is
//! used through MORE than one object. The device handle and every started
//! stream each hold their own transport clone, and the FFI surfaces (C ABI,
//! Python, JNI) register devices and streams in independent registries, so a
//! stream routinely — and legitimately — outlives the device handle it was
//! started from.
//!
//! - [`HardwareSession`] answers *when may hardware teardown run?* It is a
//!   shared lifetime co-owned by the device handle and every stream; its
//!   teardown closure runs exactly once, when the LAST co-owner is dropped,
//!   and — via [`HardwareSession::bind_stream`] — only after each bound
//!   stream's worker thread has been stopped and joined. Drivers with an
//!   ordering-sensitive power-down hang it here: the RTL2832U demodulator
//!   must not be powered off underneath a live bulk stream, yet MUST be
//!   powered off eventually or the chip comes back wedged on the next open.
//! - [`ReopenLatch`], [`reopen_required`], and [`is_reopen_required`] answer
//!   *how does a transport whose handle has died say so?* Some transports
//!   cannot survive a USB device reset and cannot re-open themselves — on
//!   Android (`NusbFdTransport`), enumeration does not exist and only the
//!   application can mint a fresh file descriptor via `UsbManager`. The latch
//!   makes such a handle refuse every subsequent operation with one stable,
//!   machine-checkable condition instead of limping on and producing
//!   confusing downstream failures.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::error::SdrError;
use crate::sample::{IqBlock, StreamHandle, StreamSink, StreamStopHandle};

/// A shared hardware-session lifetime.
///
/// Clone one into every object that must keep the hardware alive — the device
/// handle and (via [`HardwareSession::bind_stream`]) each stream handle. The
/// teardown closure runs exactly once, when the last clone is dropped.
#[derive(Clone)]
pub struct HardwareSession {
    // Never read, and that is the point: this field IS the mechanism. The
    // shared `Arc` refcount is what defers teardown, and `SessionTeardown`'s
    // `Drop` is what fires it when the last clone goes. Removing the field
    // because "nothing reads it" would silently delete the entire lifetime
    // guarantee.
    #[allow(dead_code)]
    inner: Arc<SessionTeardown>,
}

/// The refcounted core of a [`HardwareSession`]; its `Drop` *is* the
/// "last clone released" event.
struct SessionTeardown {
    /// `Mutex<Option<..>>` rather than a bare `Option` because `Arc<T>`
    /// requires `T: Sync` to stay `Send`, and a boxed `FnOnce` is not `Sync`.
    /// The lock is uncontended: it is taken exactly once, from `drop`.
    teardown: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Drop for SessionTeardown {
    fn drop(&mut self) {
        let teardown = self
            .teardown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(teardown) = teardown {
            teardown();
        }
    }
}

impl HardwareSession {
    /// Create a session whose `teardown` runs exactly once, when the last
    /// clone of the returned session is dropped.
    ///
    /// The closure must be self-contained (own its transport clone or
    /// whatever else it needs) and must not panic: it runs inside a `Drop`.
    #[must_use]
    pub fn new(teardown: impl FnOnce() + Send + 'static) -> Self {
        Self {
            inner: Arc::new(SessionTeardown {
                teardown: Mutex::new(Some(Box::new(teardown))),
            }),
        }
    }

    /// Bind a stream to this session so the stream co-owns the hardware.
    ///
    /// The returned handle behaves exactly like `stream`, and additionally
    /// holds a session clone that is released only AFTER the inner stream has
    /// been dropped — i.e. after the transport worker thread has been stopped
    /// and joined. A stream that outlives its device therefore keeps the
    /// hardware alive, and the teardown can never run underneath a worker
    /// that is still touching the device.
    #[must_use]
    pub fn bind_stream(&self, stream: StreamHandle) -> StreamHandle {
        Box::new(SessionBoundSink {
            inner: stream,
            _session: self.clone(),
        })
    }
}

/// A [`StreamSink`] wrapper binding a stream's lifetime into a session.
struct SessionBoundSink {
    // FIELD ORDER IS LOAD-BEARING. Rust drops fields in declaration order:
    // `inner` must drop first (stopping the stream and joining its transport
    // worker), and only then may `_session` release its co-ownership —
    // possibly firing the teardown. Reversing these fields would let the
    // teardown (e.g. an RTL2832U power-down) race a worker mid-transfer.
    inner: StreamHandle,
    _session: HardwareSession,
}

impl StreamSink for SessionBoundSink {
    fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
        self.inner.recv()
    }

    fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
        self.inner.recv_deadline(deadline)
    }

    fn stop_handle(&self) -> StreamStopHandle {
        self.inner.stop_handle()
    }

    fn stop(&self) {
        self.inner.stop();
    }
}

/// Stable machine-checkable prefix marking [`SdrError::Transport`] messages
/// that mean "this device handle is dead by protocol, not by accident: close
/// this device and open a fresh one".
///
/// This is deliberately a message-level contract: foreign bindings that only
/// ever see stringified errors (JNI exception messages, C error strings) can
/// match this prefix verbatim, and it keeps working until a dedicated
/// [`SdrError`] variant exists. From Rust, prefer [`is_reopen_required`].
pub const REOPEN_REQUIRED_MARKER: &str = "device re-open required";

/// Build the "close this device and re-open it" condition.
///
/// Used by transports whose handle cannot survive a recovery action and
/// cannot be rebuilt library-side — e.g. the Android fd transport
/// (`NusbFdTransport`), where a USB device reset invalidates the underlying
/// nusb handle and only the application can mint a new file descriptor via
/// `UsbManager`.
#[must_use]
pub fn reopen_required(detail: &str) -> SdrError {
    SdrError::Transport(format!("{REOPEN_REQUIRED_MARKER}: {detail}"))
}

/// Does `error` carry the [`REOPEN_REQUIRED_MARKER`] condition?
///
/// Distinguishes the three recovery postures a caller must tell apart:
/// **re-open the device** (this condition), **retry later**
/// ([`SdrError::Timeout`] / [`SdrError::DeviceBusy`]), and **give up until
/// physical re-attach** ([`SdrError::DeviceLost`]).
#[must_use]
pub fn is_reopen_required(error: &SdrError) -> bool {
    matches!(error, SdrError::Transport(message) if message.starts_with(REOPEN_REQUIRED_MARKER))
}

/// A one-way "this handle family is dead" latch, shared across transport
/// clones.
///
/// Once [`ReopenLatch::mark_defunct`] fires, every subsequent
/// [`ReopenLatch::ensure_usable`] call — on any clone — fails with the
/// [`reopen_required`] condition, so a dead handle can never be mistaken for
/// a usable one. The failure mode this replaces was a reset-killed Android
/// transport being re-initialised and re-streamed as if it had recovered.
#[derive(Clone, Debug, Default)]
pub struct ReopenLatch {
    defunct: Arc<AtomicBool>,
}

impl ReopenLatch {
    /// Create a latch in the usable state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Permanently mark every clone of this latch defunct. Idempotent.
    pub fn mark_defunct(&self) {
        self.defunct.store(true, Ordering::Release);
    }

    /// Has this latch been marked defunct?
    #[must_use]
    pub fn is_defunct(&self) -> bool {
        self.defunct.load(Ordering::Acquire)
    }

    /// Refuse `operation` on a defunct handle.
    ///
    /// # Errors
    ///
    /// Returns the [`reopen_required`] condition once the latch has been
    /// marked defunct.
    pub fn ensure_usable(&self, operation: &str) -> Result<(), SdrError> {
        if self.is_defunct() {
            return Err(reopen_required(&format!(
                "{operation} refused: this transport handle did not survive a \
                 device reset; drop the device and open a fresh one"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::IqSamples;
    use std::sync::atomic::AtomicUsize;

    /// A sink that always delivers one two-byte block, records its own drop,
    /// and counts stop requests.
    struct ProbeSink {
        events: Arc<Mutex<Vec<&'static str>>>,
        stops: Arc<AtomicUsize>,
    }

    impl StreamSink for ProbeSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            Some(Ok(IqBlock {
                samples: IqSamples::Cu8(vec![1, 2]),
                dropped: 0,
                sequence: 0,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            }))
        }

        fn recv_deadline(&mut self, _deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            self.recv()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            let stops = Arc::clone(&self.stops);
            StreamStopHandle::new(move || {
                stops.fetch_add(1, Ordering::Relaxed);
            })
        }
    }

    impl Drop for ProbeSink {
        fn drop(&mut self) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("sink dropped");
        }
    }

    #[test]
    fn teardown_runs_exactly_once_when_the_last_clone_drops() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let session = HardwareSession::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let clone_a = session.clone();
        let clone_b = session.clone();
        drop(session);
        drop(clone_a);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "teardown must wait for the last owner"
        );
        drop(clone_b);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "teardown runs exactly once");
    }

    #[test]
    fn bound_stream_keeps_the_session_alive_and_drops_before_teardown() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let teardown_events = Arc::clone(&events);
        let session = HardwareSession::new(move || {
            teardown_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("teardown");
        });
        let stream = session.bind_stream(Box::new(ProbeSink {
            events: Arc::clone(&events),
            stops: Arc::default(),
        }));
        drop(session);
        assert!(
            events.lock().unwrap().is_empty(),
            "a live stream must keep the session alive after the device drops"
        );
        drop(stream);
        assert_eq!(
            *events.lock().unwrap(),
            ["sink dropped", "teardown"],
            "the stream (and its worker) must be torn down before the session \
             teardown fires"
        );
    }

    #[test]
    fn bound_sink_delegates_receive_and_stop() {
        let session = HardwareSession::new(|| {});
        let stops = Arc::new(AtomicUsize::new(0));
        let mut stream = session.bind_stream(Box::new(ProbeSink {
            events: Arc::default(),
            stops: Arc::clone(&stops),
        }));
        let block = stream.recv().unwrap().unwrap();
        assert_eq!(block.samples.complex_count(), 1);
        assert!(stream.recv_deadline(Instant::now()).unwrap().is_ok());
        stream.stop();
        stream.stop_handle().stop();
        assert_eq!(stops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reopen_condition_is_recognizable_and_specific() {
        let error = reopen_required("close and re-open");
        assert!(is_reopen_required(&error));
        assert!(error.to_string().contains(REOPEN_REQUIRED_MARKER));
        assert!(error.to_string().contains("close and re-open"));
        assert!(!is_reopen_required(&SdrError::DeviceLost));
        assert!(!is_reopen_required(&SdrError::Timeout));
        assert!(!is_reopen_required(&SdrError::Transport(
            "some other transport failure".into()
        )));
        // The condition must not read as a hot-unplug: consumers keying off
        // is_disconnected() would treat it as "give up until re-attach"
        // instead of "re-open now".
        assert!(!error.is_disconnected());
    }

    #[test]
    fn latch_is_shared_one_way_and_refuses_operations_once_defunct() {
        let latch = ReopenLatch::new();
        let clone = latch.clone();
        assert!(!latch.is_defunct());
        assert!(latch.ensure_usable("control_in").is_ok());
        clone.mark_defunct();
        clone.mark_defunct(); // idempotent
        assert!(latch.is_defunct(), "clones must share the latch");
        let error = latch.ensure_usable("control_in").unwrap_err();
        assert!(is_reopen_required(&error));
        assert!(error.to_string().contains("control_in"));
    }
}
