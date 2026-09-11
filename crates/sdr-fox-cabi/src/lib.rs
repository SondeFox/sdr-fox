//! Stable C ABI for sdr-fox.
//!
//! Every exported function is wrapped in `catch_unwind` so no Rust panic may
//! cross the FFI boundary. Opaque handles are non-recycling registry tokens:
//! each call resolves a short-lived [`Arc`] clone, and close removes the token.
//! This makes destruction idempotent and prevents close/use races from turning
//! into use-after-free.
//!
//! The generated header (`bindings/sdr_fox.h`, produced by cbindgen at build
//! time) is the source of truth for C consumers.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::missing_errors_doc)]
// FFI size casts (usize→isize) and enum-by-value are inherent to the C ABI.
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::needless_pass_by_value)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use sdr_fox_airspy::AirspyBackend;
use sdr_fox_core::{
    DeviceDescriptor, DeviceKind, GainRequest, IqFormat, SdrBackend, SdrDevice, Upconverter,
};
use sdr_fox_rtlsdr::RtlSdrBackend;

use crossbeam_channel::{Receiver, Sender};
#[cfg(test)]
use sdr_fox_core::StreamConfig;

mod transfer_policy;
#[cfg(feature = "transfer-probe")]
#[doc(hidden)]
pub mod transfer_probe;
#[cfg(test)]
mod transfer_tests;

#[cfg(test)]
const STOP_POLL: Duration = Duration::from_millis(10);
const BRIDGE_DEPTH: usize = 8;

/// Opaque device handle.
pub struct SdrFoxDevice {
    state: Mutex<DeviceState>,
    last_error: Mutex<Option<CString>>,
}

/// Controls, applied-rate knowledge and stream-policy selection share one lock.
/// A failed rate operation leaves the hardware rate unknown to this adapter.
struct DeviceState {
    receiver: Box<dyn SdrDevice>,
    applied_rate_hz: Option<u32>,
}

impl DeviceState {
    fn new(receiver: Box<dyn SdrDevice>) -> Self {
        Self {
            receiver,
            applied_rate_hz: None,
        }
    }
}

/// Snapshot of metadata and copy progress for one C stream.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SdrFoxStreamStats {
    /// Producer-reported cumulative dropped-sample count on the last block.
    pub last_dropped: u64,
    /// Producer sequence number of the last block accepted by this adapter.
    pub last_sequence: u64,
    /// Blocks accepted by the adapter, including a partially drained block.
    pub blocks_read: u64,
    /// Bytes copied into caller buffers across all successful reads.
    pub bytes_read: u64,
}

/// Opaque stream handle.
///
/// RTL streams are pulled directly through the core timed-receive contract.
/// Airspy retains a bounded worker so IQ synthesis overlaps caller copying.
/// In both modes a partially read [`sdr_fox_core::IqBlock`] stays owned here;
/// no second spill allocation or suffix copy is made.
pub struct SdrFoxStream {
    state: Mutex<StreamReadState>,
    read_gate: Arc<ReadGate>,
    stopped: Arc<AtomicBool>,
    stop_handle: sdr_fox_core::sample::StreamStopHandle,
    stop_sender: Mutex<Option<Sender<()>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    stats: Mutex<SdrFoxStreamStats>,
}

struct StreamReadState {
    source: StreamSource,
    pending: Option<sdr_fox_core::IqBlock>,
    pending_offset: usize,
    stop_receiver: Receiver<()>,
}

enum StreamSource {
    Direct(sdr_fox_core::StreamHandle),
    Bridged(Receiver<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>>),
}

#[derive(Default)]
struct ReadGate {
    busy: Mutex<bool>,
    changed: Condvar,
}

struct ReadPermit {
    gate: Arc<ReadGate>,
}

impl Drop for ReadPermit {
    fn drop(&mut self) {
        let mut busy = self
            .gate
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *busy = false;
        self.gate.changed.notify_one();
    }
}

impl ReadGate {
    fn acquire(self: &Arc<Self>, deadline: Option<Instant>) -> Option<ReadPermit> {
        let mut busy = self.busy.lock().ok()?;
        while *busy {
            busy = match deadline {
                None => self.changed.wait(busy).ok()?,
                Some(end) => {
                    let remaining = end.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    let (next, timeout) = self.changed.wait_timeout(busy, remaining).ok()?;
                    if timeout.timed_out() && *next {
                        return None;
                    }
                    next
                }
            };
        }
        *busy = true;
        drop(busy);
        Some(ReadPermit {
            gate: Arc::clone(self),
        })
    }
}

impl SdrFoxStream {
    fn with_bridge_depth(
        mut stream: sdr_fox_core::StreamHandle,
        bridge: bool,
        bridge_depth: usize,
    ) -> Self {
        assert!(bridge_depth > 0);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop_handle = stream.stop_handle();
        let (stop_sender, stop_receiver) = crossbeam_channel::bounded::<()>(0);

        let (source, worker) = if bridge {
            // Airspy synthesis currently runs in StreamSink::recv. Keep a
            // bounded worker so synthesis overlaps the C consumer copy, but
            // use event-driven select rather than a blind full-queue sleep.
            let (sender, receiver) = crossbeam_channel::bounded(bridge_depth);
            let worker_stop = stop_receiver.clone();
            let worker_stopped = Arc::clone(&stopped);
            let worker = thread::spawn(move || {
                'receive: loop {
                    if worker_stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(block) = stream.recv() else { break };
                    crossbeam_channel::select! {
                        send(sender, block) -> result => {
                            if result.is_err() { break 'receive; }
                        }
                        recv(worker_stop) -> _ => break 'receive,
                    }
                }
                stream.stop();
            });
            (StreamSource::Bridged(receiver), Some(worker))
        } else {
            (StreamSource::Direct(stream), None)
        };
        Self {
            state: Mutex::new(StreamReadState {
                source,
                pending: None,
                pending_offset: 0,
                stop_receiver,
            }),
            read_gate: Arc::new(ReadGate::default()),
            stopped,
            stop_handle,
            stop_sender: Mutex::new(Some(stop_sender)),
            worker: Mutex::new(worker),
            stats: Mutex::new(SdrFoxStreamStats::default()),
        }
    }

    fn request_stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut sender) = self.stop_sender.lock() {
            sender.take();
        }
        self.stop_handle.stop();
        self.read_gate.changed.notify_all();
    }
}

impl Drop for SdrFoxStream {
    fn drop(&mut self) {
        self.request_stop();
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

/// Integer hardware-family selector accepted by [`sdrfox_open_index`].
///
/// This deliberately is not a Rust/C enum: every `u32` bit pattern is valid at
/// the ABI boundary and is checked before conversion to the private Rust enum.
pub type SdrFoxKind = u32;
/// Auto-detect (RTL-SDR first, then Airspy).
pub const SDRFOX_KIND_AUTO: SdrFoxKind = 0;
/// RTL-SDR.
pub const SDRFOX_KIND_RTLSDR: SdrFoxKind = 1;
/// Airspy.
pub const SDRFOX_KIND_AIRSPY: SdrFoxKind = 2;
// Source-compatibility aliases for the names emitted by the earlier enum-based
// header. They remain integer constants and therefore cannot introduce an
// invalid Rust discriminant.
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_KIND_AUTO`].
pub const SdrFoxKind_Auto: SdrFoxKind = SDRFOX_KIND_AUTO;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_KIND_RTLSDR`].
pub const SdrFoxKind_Rtlsdr: SdrFoxKind = SDRFOX_KIND_RTLSDR;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_KIND_AIRSPY`].
pub const SdrFoxKind_Airspy: SdrFoxKind = SDRFOX_KIND_AIRSPY;

/// Integer sample-format selector accepted by [`sdrfox_start_stream`].
///
/// Like [`SdrFoxKind`], this is an integer typedef rather than an enum so an
/// invalid value can be rejected inside the FFI boundary without first
/// creating an invalid Rust discriminant.
pub type SdrFoxFormat = u32;
/// Interleaved unsigned 8-bit IQ.
pub const SDRFOX_FORMAT_CU8: SdrFoxFormat = 0;
/// Interleaved signed 8-bit IQ.
pub const SDRFOX_FORMAT_CS8: SdrFoxFormat = 1;
/// Interleaved signed native-endian 16-bit IQ.
pub const SDRFOX_FORMAT_CS16: SdrFoxFormat = 2;
/// Interleaved native-endian IEEE-754 single-precision IQ.
pub const SDRFOX_FORMAT_CF32: SdrFoxFormat = 3;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_FORMAT_CU8`].
pub const SdrFoxFormat_Cu8: SdrFoxFormat = SDRFOX_FORMAT_CU8;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_FORMAT_CS8`].
pub const SdrFoxFormat_Cs8: SdrFoxFormat = SDRFOX_FORMAT_CS8;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_FORMAT_CS16`].
pub const SdrFoxFormat_Cs16: SdrFoxFormat = SDRFOX_FORMAT_CS16;
#[allow(non_upper_case_globals)]
/// Legacy spelling of [`SDRFOX_FORMAT_CF32`].
pub const SdrFoxFormat_Cf32: SdrFoxFormat = SDRFOX_FORMAT_CF32;

#[derive(Clone, Copy)]
enum Kind {
    Auto,
    Rtlsdr,
    Airspy,
}

#[derive(Clone, Copy)]
enum Format {
    Cu8,
    Cs8,
    Cs16,
    Cf32,
}

impl TryFrom<SdrFoxKind> for Kind {
    type Error = &'static str;

    fn try_from(v: SdrFoxKind) -> Result<Self, Self::Error> {
        match v {
            SDRFOX_KIND_AUTO => Ok(Self::Auto),
            SDRFOX_KIND_RTLSDR => Ok(Self::Rtlsdr),
            SDRFOX_KIND_AIRSPY => Ok(Self::Airspy),
            _ => Err("invalid SdrFoxKind value"),
        }
    }
}

impl TryFrom<SdrFoxFormat> for Format {
    type Error = &'static str;

    fn try_from(v: SdrFoxFormat) -> Result<Self, Self::Error> {
        match v {
            SDRFOX_FORMAT_CU8 => Ok(Self::Cu8),
            SDRFOX_FORMAT_CS8 => Ok(Self::Cs8),
            SDRFOX_FORMAT_CS16 => Ok(Self::Cs16),
            SDRFOX_FORMAT_CF32 => Ok(Self::Cf32),
            _ => Err("invalid SdrFoxFormat value"),
        }
    }
}

impl From<Format> for IqFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Cu8 => IqFormat::Cu8,
            Format::Cs8 => IqFormat::Cs8,
            Format::Cs16 => IqFormat::Cs16,
            Format::Cf32 => IqFormat::Cf32,
        }
    }
}

struct HandleRegistry<T> {
    next_id: usize,
    entries: HashMap<usize, Arc<T>>,
}

impl<T> Default for HandleRegistry<T> {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: HashMap::new(),
        }
    }
}

impl<T> HandleRegistry<T> {
    /// Insert a handle under a process-unique, nonzero token. IDs are never
    /// recycled, so a stale token can never become valid for a later object.
    fn insert(&mut self, value: T) -> Option<*mut T> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1)?;
        self.entries.insert(id, Arc::new(value));
        Some(id as *mut T)
    }

    fn resolve(&self, handle: *mut T) -> Option<Arc<T>> {
        self.entries.get(&(handle as usize)).cloned()
    }

    fn remove(&mut self, handle: *mut T) -> Option<Arc<T>> {
        self.entries.remove(&(handle as usize))
    }
}

static DEVICES: OnceLock<RwLock<HandleRegistry<SdrFoxDevice>>> = OnceLock::new();
static STREAMS: OnceLock<RwLock<HandleRegistry<SdrFoxStream>>> = OnceLock::new();

fn devices() -> &'static RwLock<HandleRegistry<SdrFoxDevice>> {
    DEVICES.get_or_init(|| RwLock::new(HandleRegistry::default()))
}

fn streams() -> &'static RwLock<HandleRegistry<SdrFoxStream>> {
    STREAMS.get_or_init(|| RwLock::new(HandleRegistry::default()))
}

fn resolve_device(handle: *mut SdrFoxDevice) -> Option<Arc<SdrFoxDevice>> {
    if handle.is_null() {
        return None;
    }
    devices().read().ok()?.resolve(handle)
}

fn resolve_stream(handle: *mut SdrFoxStream) -> Option<Arc<SdrFoxStream>> {
    if handle.is_null() {
        return None;
    }
    streams().read().ok()?.resolve(handle)
}

fn ffi_safe<R>(body: impl FnOnce() -> R, panic_fallback: R) -> R {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(v) => v,
        Err(_) => panic_fallback,
    }
}

/// Open a device by index. On success, sets `*out` to a new handle and returns
/// null (no error). On failure, returns a pointer to a UTF-8 error string.
/// That error pointer remains valid until the next failed open on the calling
/// thread; copy it if it must be retained.
///
/// `kind` is a `u32` corresponding to a [`SdrFoxKind`] discriminant; an
/// out-of-range value is rejected with an error string rather than invoking
/// undefined behavior.
///
/// # Safety
///
/// `out` must be a valid, non-null pointer to a `sdrfox_device*` slot.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_open_index(
    index: usize,
    kind: SdrFoxKind,
    out: *mut *mut SdrFoxDevice,
) -> *const c_char {
    if out.is_null() {
        return static_err("out is null");
    }
    *out = std::ptr::null_mut();
    let kind = match Kind::try_from(kind) {
        Ok(k) => k,
        Err(e) => return static_err(e),
    };
    ffi_safe(
        move || match open_by_index(index, kind) {
            Ok(dev) => {
                let device = SdrFoxDevice {
                    state: Mutex::new(DeviceState::new(dev)),
                    last_error: Mutex::new(None),
                };
                match devices().write().ok().and_then(|mut r| r.insert(device)) {
                    Some(handle) => {
                        *out = handle;
                        std::ptr::null()
                    }
                    None => static_err("device handle registry exhausted"),
                }
            }
            Err(e) => static_err(&format!("{e}")),
        },
        static_err("panic in sdrfox_open_index"),
    )
}

fn open_by_index(index: usize, kind: Kind) -> Result<Box<dyn SdrDevice>, sdr_fox_core::SdrError> {
    let locations = sdr_fox_transport::enumerate_usb_devices()?;
    let selected = select_location(&locations, index, kind).ok_or_else(|| {
        sdr_fox_core::SdrError::DeviceNotFound(format!(
            "no matching {} device at family index {index}",
            match kind {
                Kind::Auto => "supported SDR",
                Kind::Rtlsdr => "RTL-SDR",
                Kind::Airspy => "Airspy",
            }
        ))
    })?;
    let device_kind = if is_rtl_id(selected.vendor_id, selected.product_id) {
        DeviceKind::RtlSdr
    } else {
        DeviceKind::Airspy
    };
    open_known_device(&selected, device_kind)
}

const fn is_rtl_id(vid: u16, pid: u16) -> bool {
    matches!(
        (vid, pid),
        (0x0bda, 0x2832 | 0x2838) | (0x1d50, 0x6089 | 0xcc60)
    )
}

fn select_location(
    locations: &[sdr_fox_transport::UsbDeviceLocation],
    index: usize,
    kind: Kind,
) -> Option<sdr_fox_transport::UsbDeviceLocation> {
    let is_rtl = |location: &&sdr_fox_transport::UsbDeviceLocation| {
        is_rtl_id(location.vendor_id, location.product_id)
    };
    let is_airspy = |location: &&sdr_fox_transport::UsbDeviceLocation| {
        (location.vendor_id, location.product_id) == (0x1d50, 0x60a1)
    };
    match kind {
        Kind::Rtlsdr => locations.iter().filter(is_rtl).nth(index),
        Kind::Airspy => locations.iter().filter(is_airspy).nth(index),
        Kind::Auto => locations
            .iter()
            .filter(is_rtl)
            .chain(locations.iter().filter(is_airspy))
            .nth(index),
    }
    .cloned()
}

/// Build the open descriptor for a located device, forwarding the bus's
/// best-effort string descriptors so the opened device's `DeviceInfo`
/// reports its real manufacturer/product/serial instead of blanks.
fn descriptor_for(
    location: &sdr_fox_transport::UsbDeviceLocation,
    kind: DeviceKind,
) -> DeviceDescriptor {
    DeviceDescriptor {
        vendor_id: location.vendor_id,
        product_id: location.product_id,
        vendor_name: location.vendor_name.clone(),
        product_name: location.product_name.clone(),
        serial: location.serial.clone(),
        index: location.match_index,
        kind,
    }
}

fn open_known_device(
    location: &sdr_fox_transport::UsbDeviceLocation,
    device_kind: DeviceKind,
) -> Result<Box<dyn SdrDevice>, sdr_fox_core::SdrError> {
    let desc = descriptor_for(location, device_kind);
    let transport = sdr_fox_transport::open_default(
        location.vendor_id,
        location.product_id,
        location.match_index,
    )?;
    let backend: Box<dyn SdrBackend> = match device_kind {
        DeviceKind::Airspy => Box::new(AirspyBackend),
        DeviceKind::RtlSdr => Box::new(RtlSdrBackend),
        DeviceKind::Unknown => {
            return Err(sdr_fox_core::SdrError::Unsupported(
                "unknown device family".into(),
            ));
        }
    };
    backend.open(&desc, transport)
}

/// Fixed-size receiver descriptor. Strings are UTF-8 and NUL-terminated;
/// oversized identities are excluded rather than truncated into collisions.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrFoxReceiver {
    /// Opaque exact-selection identity; treat as private user device metadata.
    pub id: [c_char; 512],
    /// Hardware-reported label or family fallback.
    pub label: [c_char; 256],
    /// `SDRFOX_KIND_RTLSDR` or `SDRFOX_KIND_AIRSPY`.
    pub kind: u32,
}

fn receiver_kind(location: &sdr_fox_transport::UsbDeviceLocation) -> Option<DeviceKind> {
    if is_rtl_id(location.vendor_id, location.product_id) {
        Some(DeviceKind::RtlSdr)
    } else if (location.vendor_id, location.product_id) == (0x1d50, 0x60a1) {
        Some(DeviceKind::Airspy)
    } else {
        None
    }
}

fn c_text<const N: usize>(text: &str) -> [c_char; N] {
    let mut out = [0; N];
    let mut end = text.len().min(N.saturating_sub(1));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    for (o, i) in out.iter_mut().zip(text.as_bytes()[..end].iter()) {
        *o = *i as c_char;
    }
    out
}

/// Enumerate supported receivers without opening hardware. Returns required
/// count (copies at most capacity entries), or -1. Re-run if count grew.
/// `out` must hold capacity writable entries; null is permitted for capacity 0.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_enumerate(out: *mut SdrFoxReceiver, capacity: usize) -> isize {
    if capacity > 4096 || (out.is_null() && capacity != 0) {
        return -1;
    }
    ffi_safe(
        move || {
            let Ok(found) = sdr_fox_transport::enumerate_stable_usb_devices() else {
                return -1;
            };
            let receivers: Vec<_> = found
                .into_iter()
                .filter_map(|(id, location)| {
                    let kind = receiver_kind(&location)?;
                    if id.len() >= 512 || id.contains('\0') {
                        return None;
                    }
                    let label = location.product_name.as_deref().unwrap_or(match kind {
                        DeviceKind::RtlSdr => "RTL-SDR",
                        _ => "Airspy",
                    });
                    Some(SdrFoxReceiver {
                        id: c_text(&id),
                        label: c_text(label),
                        kind: if kind == DeviceKind::RtlSdr {
                            SDRFOX_KIND_RTLSDR
                        } else {
                            SDRFOX_KIND_AIRSPY
                        },
                    })
                })
                .collect();
            for (i, receiver) in receivers.iter().take(capacity).enumerate() {
                out.add(i).write(*receiver);
            }
            isize::try_from(receivers.len()).unwrap_or(-1)
        },
        -1,
    )
}

/// Open only the selected receiver; no auto-detection/index fallback occurs.
/// `id` must point to a NUL-terminated UTF-8 string no longer than 511 bytes;
/// `out` is cleared before any IO. Returns null on success or thread-local error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_open_id(
    id: *const c_char,
    out: *mut *mut SdrFoxDevice,
) -> *const c_char {
    if out.is_null() {
        return static_err("out is null");
    }
    *out = std::ptr::null_mut();
    if id.is_null() {
        return static_err("id is null");
    }
    ffi_safe(
        move || {
            let Ok(id) = CStr::from_ptr(id).to_str() else {
                return static_err("id is not UTF-8");
            };
            if id.len() >= 512 {
                return static_err("id is too long");
            }
            let result = (|| {
                let (location, transport) = sdr_fox_transport::open_stable_usb(id)?;
                let kind = receiver_kind(&location).ok_or_else(|| {
                    sdr_fox_core::SdrError::Unsupported("unsupported receiver".into())
                })?;
                let desc = descriptor_for(&location, kind);
                match kind {
                    DeviceKind::RtlSdr => RtlSdrBackend.open(&desc, transport),
                    _ => AirspyBackend.open(&desc, transport),
                }
            })();
            match result {
                Ok(device) => match devices().write().ok().and_then(|mut r| {
                    r.insert(SdrFoxDevice {
                        state: Mutex::new(DeviceState::new(device)),
                        last_error: Mutex::new(None),
                    })
                }) {
                    Some(handle) => {
                        *out = handle;
                        std::ptr::null()
                    }
                    None => static_err("device handle registry exhausted"),
                },
                Err(error) => static_err(&error.to_string()),
            }
        },
        static_err("panic in sdrfox_open_id"),
    )
}

/// Set a rate and report the actual applied rate atomically. Never infer DSP
/// rate from the request. On failure *actual is zero.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_sample_rate_actual(
    dev: *mut SdrFoxDevice,
    hz: u32,
    actual: *mut u32,
) -> c_int {
    if actual.is_null() {
        return -1;
    }
    *actual = 0;
    state_setter(dev, |state| {
        // The setter may partially reconfigure hardware before failing. Do not
        // retain a stale low-rate policy until a new rate succeeds.
        state.applied_rate_hz = None;
        let applied = state.receiver.set_sample_rate(hz)?;
        state.applied_rate_hz = Some(applied);
        *actual = applied;
        Ok(())
    })
}

/// Query exact hardware sample rates. Empty means continuous/nonenumerable.
/// Returns required count, or -1. Copies at most capacity entries.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_sample_rates(
    dev: *mut SdrFoxDevice,
    out: *mut u32,
    capacity: usize,
) -> isize {
    if capacity > 4096 || (out.is_null() && capacity != 0) {
        return -1;
    }
    let Some(dev) = resolve_device(dev) else {
        return -1;
    };
    ffi_safe(
        move || {
            let Ok(state) = dev.state.lock() else {
                return -1;
            };
            let rates = state.receiver.supported_sample_rates();
            for (i, rate) in rates.iter().take(capacity).enumerate() {
                out.add(i).write(*rate);
            }
            isize::try_from(rates.len()).unwrap_or(-1)
        },
        -1,
    )
}

/// One hardware-reported discrete gain value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SdrFoxGainStep {
    /// -1 = OVERALL, 0 = LNA, 1 = MIXER, 2 = VGA.
    pub stage: i32,
    /// Gain in tenths of dB (Airspy register steps are value * 10).
    pub tenths_db: i32,
}

/// Query gain steps from the opened tuner. Returns required count, or -1.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_gain_steps(
    dev: *mut SdrFoxDevice,
    out: *mut SdrFoxGainStep,
    capacity: usize,
) -> isize {
    if capacity > 4096 || (out.is_null() && capacity != 0) {
        return -1;
    }
    let Some(dev) = resolve_device(dev) else {
        return -1;
    };
    ffi_safe(
        move || {
            let Ok(locked) = dev.state.lock() else {
                return -1;
            };
            let gains: Vec<_> = locked
                .receiver
                .gains()
                .iter()
                .filter_map(|gain| {
                    let stage = match gain.name {
                        "OVERALL" => -1,
                        "LNA" => 0,
                        "MIXER" => 1,
                        "VGA" => 2,
                        _ => return None,
                    };
                    Some(SdrFoxGainStep {
                        stage,
                        tenths_db: gain.tenths_db,
                    })
                })
                .collect();
            for (i, gain) in gains.iter().take(capacity).enumerate() {
                out.add(i).write(*gain);
            }
            isize::try_from(gains.len()).unwrap_or(-1)
        },
        -1,
    )
}

/// Apply a named hardware stage; invalid integer codes fail before IO.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_stage_gain(
    dev: *mut SdrFoxDevice,
    stage: i32,
    tenths_db: i32,
) -> c_int {
    setter(dev, |d| {
        d.set_gain(GainRequest::per_stage(
            sdr_fox_core::GainStageId::try_from(stage)?,
            tenths_db,
        ))
    })
}

/// Set independent LNA or mixer AGC where supported.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_stage_agc(
    dev: *mut SdrFoxDevice,
    stage: i32,
    on: c_int,
) -> c_int {
    setter(dev, |d| {
        d.set_stage_agc(sdr_fox_core::GainStageId::try_from(stage)?, on != 0)
    })
}

/// Select manual/automatic tuner gain mode independently of digital AGC.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_gain_mode(dev: *mut SdrFoxDevice, automatic: c_int) -> c_int {
    setter(dev, |d| {
        d.set_gain_mode(if automatic == 0 {
            sdr_fox_core::GainMode::Manual
        } else {
            sdr_fox_core::GainMode::Auto
        })
    })
}

/// Set IF bandwidth where hardware supports it.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_bandwidth(dev: *mut SdrFoxDevice, hz: u32) -> c_int {
    setter(dev, |d| d.set_bandwidth(hz))
}

/// Read the known reference oscillator rate; zero means unavailable.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_reference_clock(dev: *mut SdrFoxDevice) -> u32 {
    let Some(dev) = resolve_device(dev) else {
        return 0;
    };
    ffi_safe(
        move || {
            dev.state
                .lock()
                .ok()
                .and_then(|state| state.receiver.reference_clock_hz())
                .unwrap_or(0)
        },
        0,
    )
}

/// Close a device handle. Idempotent; null, stale, and repeated handles are
/// no-ops. A call already in progress retains an [`Arc`] until it returns.
///
/// # Safety
///
/// `dev` must be a valid pointer returned by `sdrfox_open_*`, or null (no-op).
#[no_mangle]
pub unsafe extern "C" fn sdrfox_close(dev: *mut SdrFoxDevice) {
    if dev.is_null() {
        return;
    }
    ffi_safe(
        move || {
            if let Ok(mut registry) = devices().write() {
                registry.remove(dev);
            }
        },
        (),
    );
}

/// Set the center frequency in Hz. Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `dev` must be a valid, non-null device handle.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_frequency(dev: *mut SdrFoxDevice, hz: u64) -> c_int {
    setter(dev, |d| d.set_frequency(hz))
}

/// Set the sample rate in Hz. Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `dev` must be a valid, non-null device handle.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_sample_rate(dev: *mut SdrFoxDevice, hz: u32) -> c_int {
    let mut actual = 0;
    sdrfox_set_sample_rate_actual(dev, hz, &raw mut actual)
}

/// Enable/disable the bias tee. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_bias_tee(dev: *mut SdrFoxDevice, on: c_int) -> c_int {
    setter(dev, |d| d.set_bias_tee(on != 0))
}

/// Enable/disable AGC. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_agc(dev: *mut SdrFoxDevice, on: c_int) -> c_int {
    setter(dev, |d| d.set_agc(on != 0))
}

/// Set the overall gain in tenths of dB. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_gain(dev: *mut SdrFoxDevice, tenths_db: c_int) -> c_int {
    setter(dev, |d| d.set_gain(GainRequest::overall(tenths_db)))
}

/// Set the frequency correction in PPM. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_set_freq_correction(dev: *mut SdrFoxDevice, ppm: f64) -> c_int {
    setter(dev, |d| d.set_frequency_correction_ppm(ppm))
}

/// Enable the SpyVerter (120 MHz HF upconverter). Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_enable_spyverter(dev: *mut SdrFoxDevice) -> c_int {
    setter(dev, |d| d.set_upconverter(Some(Upconverter::spyverter())))
}

/// Disable any upconverter. Returns 0 on success, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_disable_upconverter(dev: *mut SdrFoxDevice) -> c_int {
    setter(dev, |d| d.set_upconverter(None))
}

/// Generic setter wrapper: `null`-check + `catch_unwind` + error-store. Success → 0;
/// null/panic/error → -1 with the message stored for `sdrfox_last_error`.
unsafe fn setter(
    dev: *mut SdrFoxDevice,
    body: impl FnOnce(&mut Box<dyn SdrDevice>) -> Result<(), sdr_fox_core::SdrError>,
) -> c_int {
    state_setter(dev, |state| body(&mut state.receiver))
}

unsafe fn state_setter(
    dev: *mut SdrFoxDevice,
    body: impl FnOnce(&mut DeviceState) -> Result<(), sdr_fox_core::SdrError>,
) -> c_int {
    let Some(dev) = resolve_device(dev) else {
        return -1;
    };
    ffi_safe(
        move || {
            let Ok(mut state) = dev.state.lock() else {
                return -1;
            };
            match body(&mut state) {
                Ok(()) => 0,
                Err(e) => {
                    if let Ok(mut last_error) = dev.last_error.lock() {
                        *last_error = CString::new(format!("{e}")).ok();
                    }
                    -1
                }
            }
        },
        -1,
    )
}

/// Start streaming. On success, sets `*out` to a new stream handle and returns 0.
///
/// `format` is a `u32` corresponding to a [`SdrFoxFormat`] discriminant; an
/// out-of-range value is rejected with a non-zero return code before any device
/// state is touched.
///
/// # Safety
///
/// `dev` must be valid and non-null; `out` must be a valid pointer to a
/// `sdrfox_stream*` slot.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_start_stream(
    dev: *mut SdrFoxDevice,
    format: SdrFoxFormat,
    out: *mut *mut SdrFoxStream,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    *out = std::ptr::null_mut();
    let Some(dev) = resolve_device(dev) else {
        return -1;
    };
    let Ok(format) = Format::try_from(format) else {
        return -1;
    };
    ffi_safe(
        move || {
            let Ok(mut state) = dev.state.lock() else {
                return -1;
            };
            let kind = state.receiver.info().kind;
            let bridge = matches!(kind, DeviceKind::Airspy);
            let policy = transfer_policy::production_policy(kind, state.applied_rate_hz);
            let cfg = policy.config(format.into());
            let started = state.receiver.start_stream(cfg);
            drop(state);
            match started {
                Ok(stream) => {
                    // Thread creation and adapter setup happen outside the
                    // process-wide registry lock.
                    let entry =
                        SdrFoxStream::with_bridge_depth(stream, bridge, policy.bridge_blocks);
                    match streams().write().ok().and_then(|mut r| r.insert(entry)) {
                        Some(handle) => {
                            *out = handle;
                            0
                        }
                        None => -1,
                    }
                }
                Err(e) => {
                    if let Ok(mut last_error) = dev.last_error.lock() {
                        *last_error = CString::new(format!("{e}")).ok();
                    }
                    -1
                }
            }
        },
        -1,
    )
}

/// Read up to `len` bytes from the stream into `buf`. Returns bytes read, 0 on
/// timeout/end-of-stream, or -1 on error.
///
/// A positive `timeout_ms` bounds this call without cancelling the stream; a
/// later read can still receive the next block. Zero or a negative value blocks
/// until data, stop, end-of-stream, or error. Concurrent reads are serialized,
/// and the timeout includes any wait behind another reader.
///
/// Any bytes that do not fit in `buf` (because a delivered block is larger than
/// `len`) remain in the stream handle's owned IQ block and are returned on the
/// next read, so short caller buffers neither discard nor duplicate data.
///
/// All advertised formats are returned as contiguous native representations:
/// Cu8 and Cs8 as bytes, Cs16 as native-endian `i16` bytes, and Cf32 as
/// native-endian IEEE-754 `f32` bytes.
///
/// # Safety
///
/// `stream` must be valid and non-null; `buf` must point to at least `len`
/// writable bytes.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_read_stream(
    stream: *mut SdrFoxStream,
    buf: *mut u8,
    len: usize,
    timeout_ms: c_int,
) -> isize {
    if buf.is_null() && len != 0 {
        return -1;
    }
    if len > isize::MAX as usize {
        return -1;
    }
    let Some(stream) = resolve_stream(stream) else {
        return -1;
    };
    if len == 0 {
        return 0;
    }
    ffi_safe(
        move || {
            let deadline = if timeout_ms > 0 {
                let duration = Duration::from_millis(u64::from(timeout_ms.unsigned_abs()));
                let Some(end) = Instant::now().checked_add(duration) else {
                    return -1;
                };
                Some(end)
            } else {
                None
            };
            let Some(_permit) = stream.read_gate.acquire(deadline) else {
                return 0;
            };
            if stream.stopped.load(Ordering::Relaxed) {
                return 0;
            }
            let Ok(mut state) = stream.state.lock() else {
                return -1;
            };
            if state.pending.is_some() {
                return copy_pending(&mut state, &stream.stats, buf, len);
            }

            if stream.stopped.load(Ordering::Relaxed) {
                return 0;
            }
            match receive_block(&mut state, deadline) {
                Some(Ok(block)) => {
                    if samples_as_bytes(&block.samples).is_empty() {
                        return -1;
                    }
                    if let Ok(mut stats) = stream.stats.lock() {
                        stats.last_dropped = block.dropped;
                        stats.last_sequence = block.sequence;
                        stats.blocks_read = stats.blocks_read.saturating_add(1);
                    }
                    state.pending = Some(block);
                    state.pending_offset = 0;
                    copy_pending(&mut state, &stream.stats, buf, len)
                }
                Some(Err(sdr_fox_core::SdrError::Timeout)) | None => 0,
                Some(Err(_)) => -1,
            }
        },
        -1,
    )
}

/// Copy a coherent stream metadata snapshot into `out`. Returns 0 on success
/// and -1 for a null, stale, or invalid handle.
///
/// # Safety
///
/// `stream` must be a live stream token and `out` must point to writable
/// storage for one [`SdrFoxStreamStats`].
#[no_mangle]
pub unsafe extern "C" fn sdrfox_stream_stats(
    stream: *mut SdrFoxStream,
    out: *mut SdrFoxStreamStats,
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let Some(stream) = resolve_stream(stream) else {
        return -1;
    };
    ffi_safe(
        move || {
            let Ok(stats) = stream.stats.lock() else {
                return -1;
            };
            out.write(*stats);
            0
        },
        -1,
    )
}

fn receive_block(
    state: &mut StreamReadState,
    deadline: Option<Instant>,
) -> Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>> {
    match &mut state.source {
        StreamSource::Direct(stream) => match deadline {
            Some(end) => stream.recv_deadline(end),
            None => stream.recv(),
        },
        StreamSource::Bridged(receiver) => {
            if let Some(end) = deadline {
                match receiver.try_recv() {
                    Ok(block) => return Some(block),
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return None,
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }
                let remaining = end.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Some(Err(sdr_fox_core::SdrError::Timeout));
                }
                let timeout = crossbeam_channel::after(remaining);
                crossbeam_channel::select! {
                    recv(receiver) -> result => result.ok(),
                    recv(state.stop_receiver) -> _ => None,
                    recv(timeout) -> _ => Some(Err(sdr_fox_core::SdrError::Timeout)),
                }
            } else {
                crossbeam_channel::select! {
                    recv(receiver) -> result => result.ok(),
                    recv(state.stop_receiver) -> _ => None,
                }
            }
        }
    }
}

/// Copy up to `len` bytes from the retained block without making a second
/// copy of its unread suffix.
/// Returns the number of bytes copied.
unsafe fn copy_pending(
    state: &mut StreamReadState,
    read_stats: &Mutex<SdrFoxStreamStats>,
    buf: *mut u8,
    len: usize,
) -> isize {
    let Some(block) = state.pending.as_ref() else {
        return 0;
    };
    let bytes = samples_as_bytes(&block.samples);
    let available = bytes.len() - state.pending_offset;
    let take = available.min(len);
    std::ptr::copy_nonoverlapping(bytes.as_ptr().add(state.pending_offset), buf, take);
    state.pending_offset += take;
    if let Ok(mut stats) = read_stats.lock() {
        stats.bytes_read = stats.bytes_read.saturating_add(take as u64);
    }
    if state.pending_offset == bytes.len() {
        state.pending = None;
        state.pending_offset = 0;
    }
    take as isize
}

/// Materialize an `IqSamples` block as a contiguous `&[u8]`.
///
/// Cu8 and Cs8 are byte-oriented (Cs8 is reinterpreted as unsigned bytes).
/// Cs16 and Cf32 are reinterpreted as their native in-memory representations.
fn samples_as_bytes(samples: &sdr_fox_core::IqSamples) -> &[u8] {
    match samples {
        sdr_fox_core::IqSamples::Cu8(b) => b.as_slice(),
        sdr_fox_core::IqSamples::Cs8(b) => {
            // Reinterpret the i8 slice as raw bytes (same bit pattern).
            let ptr = b.as_ptr().cast::<u8>();
            // SAFETY: `[i8]` and `[u8]` have identical layout.
            unsafe { std::slice::from_raw_parts(ptr, b.len()) }
        }
        sdr_fox_core::IqSamples::Cs16(s) => {
            let ptr = s.as_ptr().cast::<u8>();
            // SAFETY: the returned slice borrows `s`, covers its initialized
            // allocation, and uses byte alignment (which is no stricter).
            unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of_val(s.as_slice())) }
        }
        sdr_fox_core::IqSamples::Cf32(f) => {
            let ptr = f.as_ptr().cast::<u8>();
            // SAFETY: same reasoning as the Cs16 arm above.
            unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of_val(f.as_slice())) }
        }
    }
}

/// Request stream stop without destroying the handle. Idempotent and safe to
/// call concurrently with [`sdrfox_read_stream`]. The handle remains valid
/// until [`sdrfox_close_stream`] is called.
///
/// # Safety
///
/// `stream` must be a valid pointer returned by `sdrfox_start_stream`, or null.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_stop_stream(stream: *mut SdrFoxStream) {
    if let Some(stream) = resolve_stream(stream) {
        ffi_safe(move || stream.request_stop(), ());
    }
}

/// Close a stream handle. Idempotent and safe to call concurrently with a
/// reader: removal prevents new calls while an in-flight call retains its own
/// [`Arc`]. Closing implicitly requests stop.
///
/// # Safety
///
/// `stream` may be a pointer returned by `sdrfox_start_stream`, a stale handle,
/// or null.
#[no_mangle]
pub unsafe extern "C" fn sdrfox_close_stream(stream: *mut SdrFoxStream) {
    if stream.is_null() {
        return;
    }
    ffi_safe(
        move || {
            // Drop (and therefore join) outside the global registry lock so a
            // slow USB shutdown cannot serialize unrelated stream handles.
            let removed = streams()
                .write()
                .ok()
                .and_then(|mut registry| registry.remove(stream));
            if let Some(stream) = removed {
                stream.request_stop();
                drop(stream);
            }
        },
        (),
    );
}

/// Retrieve the last error message for a device. Returns null if no error. The
/// message is copied into thread-local storage and remains valid until the next
/// FFI error-string operation on the calling thread (or thread exit).
///
/// # Safety
///
/// `dev` may be null (returns null).
#[no_mangle]
pub unsafe extern "C" fn sdrfox_last_error(dev: *mut SdrFoxDevice) -> *const c_char {
    let Some(dev) = resolve_device(dev) else {
        return std::ptr::null();
    };
    ffi_safe(
        move || copy_last_error_to_thread(&dev.last_error),
        std::ptr::null(),
    )
}

/// Get the sdr-fox library version string (static, never freed).
#[no_mangle]
pub extern "C" fn sdrfox_version() -> *const c_char {
    static VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
    unsafe { CStr::from_bytes_with_nul_unchecked(VERSION).as_ptr() }
}

// --- helpers ---

thread_local! {
    // Open errors and copied per-device errors share one simple per-thread
    // lifetime contract.
    static OPEN_ERROR: RefCell<CString> = RefCell::new(
        CString::new("error").expect("literal has no interior NUL")
    );
}

fn copy_last_error_to_thread(error: &Mutex<Option<CString>>) -> *const c_char {
    let Ok(error) = error.lock() else {
        return std::ptr::null();
    };
    error.as_ref().map_or(std::ptr::null(), |message| {
        static_err(&message.to_string_lossy())
    })
}

/// Store an FFI error in thread-local memory and return its pointer. It stays
/// valid until the next error-string operation on this thread (or thread exit),
/// avoiding both a dangling device-owned pointer and repeated allocation leaks.
fn static_err(msg: &str) -> *const c_char {
    OPEN_ERROR.with(|slot| {
        let mut slot = slot.borrow_mut();
        *slot = CString::new(msg)
            .unwrap_or_else(|_| CString::new("error").expect("literal has no interior NUL"));
        slot.as_ptr()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{IqBlock, IqSamples, SdrError, StreamSink};
    use std::sync::mpsc::{self, Sender};

    type TestStreamParts = (
        *mut SdrFoxStream,
        Sender<Option<Result<IqBlock, SdrError>>>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<std::sync::atomic::AtomicUsize>,
    );

    struct ChannelSink {
        receiver: mpsc::Receiver<Option<Result<IqBlock, SdrError>>>,
        stopped: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        delivered_blocks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl StreamSink for ChannelSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            loop {
                if self.stopped.load(Ordering::Acquire) {
                    return None;
                }
                match self.receiver.recv_timeout(STOP_POLL) {
                    Ok(value) => {
                        self.delivered_blocks.fetch_add(1, Ordering::Relaxed);
                        return value;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
        }

        fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            loop {
                if self.stopped.load(Ordering::Acquire) {
                    return None;
                }
                match self.receiver.try_recv() {
                    Ok(value) => {
                        self.delivered_blocks.fetch_add(1, Ordering::Relaxed);
                        return value;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return None,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                let now = Instant::now();
                if now >= deadline {
                    return Some(Err(SdrError::Timeout));
                }
                match self.receiver.recv_timeout((deadline - now).min(STOP_POLL)) {
                    Ok(value) => {
                        self.delivered_blocks.fetch_add(1, Ordering::Relaxed);
                        return value;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
        }

        fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
            let stopped = Arc::clone(&self.stopped);
            sdr_fox_core::sample::StreamStopHandle::new(move || {
                stopped.store(true, Ordering::Release);
            })
        }

        fn stop(&self) {
            self.stopped.store(true, Ordering::Release);
        }
    }

    impl Drop for ChannelSink {
        fn drop(&mut self) {
            self.finished.store(true, Ordering::Release);
        }
    }

    fn test_stream_mode(bridge: bool) -> TestStreamParts {
        test_stream_depth(bridge, BRIDGE_DEPTH)
    }

    pub(super) fn test_stream_depth(bridge: bool, depth: usize) -> TestStreamParts {
        let (sender, receiver) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let delivered_blocks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = ChannelSink {
            receiver,
            stopped: Arc::clone(&stopped),
            finished: Arc::clone(&finished),
            delivered_blocks: Arc::clone(&delivered_blocks),
        };
        let stream = SdrFoxStream::with_bridge_depth(Box::new(sink), bridge, depth);
        let handle = streams().write().unwrap().insert(stream).unwrap();
        (handle, sender, stopped, finished, delivered_blocks)
    }

    fn test_stream() -> TestStreamParts {
        test_stream_mode(true)
    }

    fn block(samples: IqSamples) -> IqBlock {
        IqBlock {
            samples,
            dropped: 0,
            sequence: 0,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        }
    }

    fn location(vid: u16, pid: u16, match_index: usize) -> sdr_fox_transport::UsbDeviceLocation {
        sdr_fox_transport::UsbDeviceLocation {
            vendor_id: vid,
            product_id: pid,
            vendor_name: None,
            product_name: None,
            serial: None,
            match_index,
        }
    }

    #[test]
    fn version_is_non_null_and_nul_terminated() {
        let v = sdrfox_version();
        assert!(!v.is_null());
        unsafe {
            let s = CStr::from_ptr(v).to_str().unwrap();
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn format_conversion_covers_all_variants() {
        assert!(matches!(IqFormat::from(Format::Cu8), IqFormat::Cu8));
        assert!(matches!(IqFormat::from(Format::Cf32), IqFormat::Cf32));
        assert!(matches!(IqFormat::from(Format::Cs8), IqFormat::Cs8));
        assert!(matches!(IqFormat::from(Format::Cs16), IqFormat::Cs16));
    }

    struct RateDevice {
        info: sdr_fox_core::DeviceInfo,
        stream_configs: Option<mpsc::Sender<StreamConfig>>,
        rate_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }
    impl SdrDevice for RateDevice {
        fn info(&self) -> &sdr_fox_core::DeviceInfo {
            &self.info
        }
        fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SdrError> {
            if let Some((entered, release)) = self.rate_gate.take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            if hz == 0 {
                Err(SdrError::InvalidParameter("zero rate".into()))
            } else {
                Ok(hz + 3)
            }
        }
        fn supported_sample_rates(&self) -> Vec<u32> {
            vec![2_500_000, 3_000_000]
        }
        fn reference_clock_hz(&self) -> Option<u32> {
            Some(28_800_000)
        }
        fn set_frequency(&mut self, _: u64) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_bandwidth(&mut self, _: u32) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_gain(&mut self, _: GainRequest) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_gain_mode(&mut self, _: sdr_fox_core::GainMode) -> Result<(), SdrError> {
            Ok(())
        }
        fn gains(&self) -> &[sdr_fox_core::GainStep] {
            &[]
        }
        fn set_bias_tee(&mut self, _: bool) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_agc(&mut self, _: bool) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_frequency_correction_ppm(&mut self, _: f64) -> Result<(), SdrError> {
            Ok(())
        }
        fn set_upconverter(&mut self, _: Option<Upconverter>) -> Result<(), SdrError> {
            Ok(())
        }
        fn start_stream(
            &mut self,
            config: StreamConfig,
        ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
            if let Some(sender) = &self.stream_configs {
                sender.send(config).unwrap();
            }
            Err(SdrError::DeviceBusy)
        }
    }
    #[test]
    fn actual_rate_and_capabilities_preserve_hardware_results_and_stale_safety() {
        let handle = devices()
            .write()
            .unwrap()
            .insert(SdrFoxDevice {
                state: Mutex::new(DeviceState::new(Box::new(RateDevice {
                    info: sdr_fox_core::DeviceInfo::default(),
                    stream_configs: None,
                    rate_gate: None,
                }))),
                last_error: Mutex::new(None),
            })
            .unwrap();
        unsafe {
            let mut actual = 99;
            assert_eq!(
                sdrfox_set_sample_rate_actual(handle, 1_024_000, &raw mut actual),
                0
            );
            assert_eq!(actual, 1_024_003);
            assert_eq!(
                sdrfox_set_sample_rate_actual(handle, 0, &raw mut actual),
                -1
            );
            assert_eq!(actual, 0);
            let mut rates = [0u32; 1];
            assert_eq!(sdrfox_sample_rates(handle, rates.as_mut_ptr(), 1), 2);
            assert_eq!(rates, [2_500_000]);
            assert_eq!(sdrfox_reference_clock(handle), 28_800_000);
            assert_eq!(sdrfox_set_stage_gain(handle, 99, 10), -1);
            sdrfox_close(handle);
            assert_eq!(sdrfox_sample_rates(handle, std::ptr::null_mut(), 0), -1);
            assert_eq!(
                sdrfox_set_sample_rate_actual(handle, 1_024_000, &raw mut actual),
                -1
            );
        }
    }

    fn rate_policy_device(
        rate_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    ) -> (*mut SdrFoxDevice, mpsc::Receiver<StreamConfig>) {
        let (sender, receiver) = mpsc::channel();
        let handle = devices()
            .write()
            .unwrap()
            .insert(SdrFoxDevice {
                state: Mutex::new(DeviceState::new(Box::new(RateDevice {
                    info: sdr_fox_core::DeviceInfo {
                        kind: DeviceKind::RtlSdr,
                        ..sdr_fox_core::DeviceInfo::default()
                    },
                    stream_configs: Some(sender),
                    rate_gate,
                }))),
                last_error: Mutex::new(None),
            })
            .unwrap();
        (handle, receiver)
    }

    fn observed_stream_config(
        handle: *mut SdrFoxDevice,
        configs: &mpsc::Receiver<StreamConfig>,
    ) -> StreamConfig {
        let mut stream = std::ptr::dangling_mut();
        // The mock records the configuration and then refuses startup. No
        // transport or stream worker is created by these control-plane tests.
        assert_eq!(
            unsafe { sdrfox_start_stream(handle, SDRFOX_FORMAT_CU8, &raw mut stream) },
            -1
        );
        assert!(stream.is_null());
        configs.recv_timeout(Duration::from_secs(2)).unwrap()
    }

    #[test]
    fn both_rate_setters_select_from_actual_rate_and_failed_rates_revoke_cache() {
        let (handle, configs) = rate_policy_device(None);
        let low_bytes = if cfg!(target_os = "macos") {
            16_384
        } else {
            65_536
        };
        assert_eq!(observed_stream_config(handle, &configs).buffer_size, 65_536);
        unsafe {
            // The mock settles three Hz above each request: crossing both
            // edges verifies policy uses the actual result, not the request.
            for (request, expected) in [
                (224_998, low_bytes),
                (250_000, low_bytes),
                (300_000, 65_536),
            ] {
                let mut actual = 0;
                assert_eq!(
                    sdrfox_set_sample_rate_actual(handle, request, &raw mut actual),
                    0
                );
                assert_eq!(actual, request + 3);
                assert_eq!(
                    observed_stream_config(handle, &configs).buffer_size,
                    expected
                );
            }
            assert_eq!(sdrfox_set_sample_rate(handle, 250_000), 0);
            assert_eq!(
                observed_stream_config(handle, &configs).buffer_size,
                low_bytes
            );
            // Invalid output pointers never attempt hardware and preserve the
            // last known rate; a hardware-setter failure invalidates it.
            assert_eq!(
                sdrfox_set_sample_rate_actual(handle, 1_024_000, std::ptr::null_mut()),
                -1
            );
            assert_eq!(
                observed_stream_config(handle, &configs).buffer_size,
                low_bytes
            );
            let mut actual = 17;
            assert_eq!(
                sdrfox_set_sample_rate_actual(handle, 0, &raw mut actual),
                -1
            );
            assert_eq!(actual, 0);
            assert_eq!(observed_stream_config(handle, &configs).buffer_size, 65_536);
            assert_eq!(sdrfox_set_sample_rate(handle, 250_000), 0);
            assert_eq!(sdrfox_set_sample_rate(handle, 0), -1);
            assert_eq!(observed_stream_config(handle, &configs).buffer_size, 65_536);
            assert_eq!(sdrfox_set_sample_rate(handle, 250_000), 0);
            assert_eq!(
                observed_stream_config(handle, &configs).buffer_size,
                low_bytes
            );
            assert_eq!(sdrfox_set_sample_rate(handle, 3_200_000), 0);
            assert_eq!(observed_stream_config(handle, &configs).buffer_size, 65_536);
            sdrfox_close(handle);
        }
    }

    #[test]
    fn concurrent_stream_start_waits_for_the_whole_actual_rate_transaction() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (handle, configs) = rate_policy_device(Some((entered_tx, release_rx)));
        let token = handle as usize;
        let setter_thread = thread::spawn(move || {
            let mut actual = 0;
            let result = unsafe {
                sdrfox_set_sample_rate_actual(token as *mut SdrFoxDevice, 250_000, &raw mut actual)
            };
            (result, actual)
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (starting_tx, starting_rx) = mpsc::channel();
        let start_thread = thread::spawn(move || {
            starting_tx.send(()).unwrap();
            let mut stream = std::ptr::null_mut();
            unsafe {
                sdrfox_start_stream(
                    token as *mut SdrFoxDevice,
                    SDRFOX_FORMAT_CU8,
                    &raw mut stream,
                )
            }
        });
        starting_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            configs.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(setter_thread.join().unwrap(), (0, 250_003));
        assert_eq!(start_thread.join().unwrap(), -1);
        let config = configs.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            config.buffer_size,
            if cfg!(target_os = "macos") {
                16_384
            } else {
                65_536
            }
        );
        unsafe { sdrfox_close(handle) };
    }
    #[test]
    fn receiver_strings_truncate_only_at_utf8_boundaries_and_terminate() {
        let text = c_text::<5>("SDéR");
        let bytes: Vec<_> = text.into_iter().map(|v| v.to_ne_bytes()[0]).collect();
        assert_eq!(&bytes, b"SD\xc3\xa9\0");
    }
    #[test]
    fn invalid_enumeration_and_open_arguments_do_not_touch_usb() {
        unsafe {
            assert_eq!(sdrfox_enumerate(std::ptr::null_mut(), 1), -1);
            assert_eq!(sdrfox_enumerate(std::ptr::null_mut(), 4097), -1);
            let mut out = std::ptr::dangling_mut();
            assert!(!sdrfox_open_id(std::ptr::null(), &raw mut out).is_null());
            assert!(out.is_null());
        }
    }

    #[test]
    fn kind_try_from_accepts_valid_values() {
        assert!(matches!(Kind::try_from(SDRFOX_KIND_AUTO), Ok(Kind::Auto)));
        assert!(matches!(
            Kind::try_from(SDRFOX_KIND_RTLSDR),
            Ok(Kind::Rtlsdr)
        ));
        assert!(matches!(
            Kind::try_from(SDRFOX_KIND_AIRSPY),
            Ok(Kind::Airspy)
        ));
    }

    #[test]
    fn kind_try_from_rejects_invalid_values() {
        assert!(Kind::try_from(3).is_err());
        assert!(Kind::try_from(u32::MAX).is_err());
    }

    #[test]
    fn format_try_from_accepts_valid_values() {
        assert!(matches!(
            Format::try_from(SDRFOX_FORMAT_CU8),
            Ok(Format::Cu8)
        ));
        assert!(matches!(
            Format::try_from(SDRFOX_FORMAT_CS8),
            Ok(Format::Cs8)
        ));
        assert!(matches!(
            Format::try_from(SDRFOX_FORMAT_CS16),
            Ok(Format::Cs16)
        ));
        assert!(matches!(
            Format::try_from(SDRFOX_FORMAT_CF32),
            Ok(Format::Cf32)
        ));
    }

    #[test]
    fn format_try_from_rejects_invalid_values() {
        assert!(Format::try_from(4).is_err());
        assert!(Format::try_from(u32::MAX).is_err());
    }

    #[test]
    fn invalid_kind_is_rejected_at_integer_ffi_boundary() {
        let mut out = std::ptr::dangling_mut::<SdrFoxDevice>();
        let error = unsafe { sdrfox_open_index(0, u32::MAX, &raw mut out) };
        assert!(!error.is_null());
        assert!(out.is_null());
    }

    #[test]
    fn registry_tokens_are_nonrecycling_and_removal_is_idempotent() {
        let mut registry = HandleRegistry::default();
        let first = registry.insert(10u8).unwrap();
        assert_eq!(*registry.resolve(first).unwrap(), 10);
        assert!(registry.remove(first).is_some());
        assert!(registry.remove(first).is_none());
        let second = registry.insert(20u8).unwrap();
        assert_ne!(first, second);
        assert!(registry.resolve(first).is_none());
        assert_eq!(*registry.resolve(second).unwrap(), 20);
    }

    #[test]
    fn family_indexing_spans_all_rtl_ids_and_auto_prioritizes_rtl() {
        let locations = [
            location(0x0bda, 0x2832, 0),
            location(0x1d50, 0x60a1, 0),
            location(0x0bda, 0x2838, 0),
            location(0x0bda, 0x2832, 1),
        ];
        assert_eq!(
            select_location(&locations, 1, Kind::Rtlsdr),
            Some(locations[2].clone())
        );
        assert_eq!(
            select_location(&locations, 2, Kind::Rtlsdr),
            Some(locations[3].clone())
        );
        assert_eq!(
            select_location(&locations, 3, Kind::Auto),
            Some(locations[1].clone())
        );
    }

    #[test]
    fn open_descriptor_carries_the_bus_string_descriptors() {
        let mut with_strings = location(0x0bda, 0x2838, 2);
        with_strings.vendor_name = Some("Nooelec".to_string());
        with_strings.product_name = Some("SMArt XTR v5".to_string());
        with_strings.serial = Some("38956405".to_string());
        let descriptor = descriptor_for(&with_strings, DeviceKind::RtlSdr);
        assert_eq!(descriptor.vendor_id, 0x0bda);
        assert_eq!(descriptor.product_id, 0x2838);
        assert_eq!(descriptor.vendor_name.as_deref(), Some("Nooelec"));
        assert_eq!(descriptor.product_name.as_deref(), Some("SMArt XTR v5"));
        assert_eq!(descriptor.serial.as_deref(), Some("38956405"));
        assert_eq!(descriptor.index, 2);
        assert_eq!(descriptor.kind, DeviceKind::RtlSdr);

        let bare = location(0x1d50, 0x60a1, 0);
        let bare_descriptor = descriptor_for(&bare, DeviceKind::Airspy);
        assert_eq!(bare_descriptor.vendor_name, None);
        assert_eq!(bare_descriptor.product_name, None);
        assert_eq!(bare_descriptor.serial, None);
    }

    #[test]
    fn close_on_null_is_noop() {
        unsafe { sdrfox_close(std::ptr::null_mut()) };
    }

    #[test]
    fn stop_stream_on_null_is_noop() {
        unsafe { sdrfox_stop_stream(std::ptr::null_mut()) };
    }

    #[test]
    fn close_stream_on_null_is_noop() {
        unsafe { sdrfox_close_stream(std::ptr::null_mut()) };
    }

    #[test]
    fn samples_as_bytes_handles_formats() {
        assert_eq!(samples_as_bytes(&IqSamples::Cu8(vec![1, 2, 3])), &[1, 2, 3]);
        let cs8 = IqSamples::Cs8(vec![-1i8, 0, 1]);
        assert_eq!(samples_as_bytes(&cs8), &[255, 0, 1]);
        let signed_16 = IqSamples::Cs16(vec![-2i16, 0x1234]);
        let expected_cs16: Vec<u8> = [-2i16, 0x1234]
            .into_iter()
            .flat_map(i16::to_ne_bytes)
            .collect();
        assert_eq!(samples_as_bytes(&signed_16), expected_cs16);
        let float_32 = IqSamples::Cf32(vec![1.0f32]);
        assert_eq!(samples_as_bytes(&float_32), &(1.0f32).to_ne_bytes());
    }

    #[test]
    fn short_reads_retain_the_exact_remainder() {
        let (handle, sender, ..) = test_stream();
        sender
            .send(Some(Ok(block(IqSamples::Cu8(vec![1, 2, 3, 4, 5])))))
            .unwrap();
        let mut first = [0u8; 2];
        let mut second = [0u8; 3];
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, first.as_mut_ptr(), 2, 500) },
            2
        );
        assert_eq!(first, [1, 2]);
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, second.as_mut_ptr(), 3, 500) },
            3
        );
        assert_eq!(second, [3, 4, 5]);
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn empty_success_block_is_rejected_instead_of_spinning() {
        let (handle, sender, ..) = test_stream();
        sender
            .send(Some(Ok(block(IqSamples::Cu8(Vec::new())))))
            .unwrap();
        let mut byte = 0_u8;
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, &raw mut byte, 1, 500) },
            -1
        );
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn stats_track_block_metadata_and_partial_copy_coherently() {
        let (handle, sender, ..) = test_stream();
        sender
            .send(Some(Ok(IqBlock {
                samples: IqSamples::Cu8(vec![1, 2, 3, 4]),
                dropped: 17,
                sequence: 9,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            })))
            .unwrap();
        let mut bytes = [0_u8; 2];
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), 2, 500) },
            2
        );
        let mut stats = SdrFoxStreamStats::default();
        assert_eq!(unsafe { sdrfox_stream_stats(handle, &raw mut stats) }, 0);
        assert_eq!(
            stats,
            SdrFoxStreamStats {
                last_dropped: 17,
                last_sequence: 9,
                blocks_read: 1,
                bytes_read: 2,
            }
        );
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn queued_blocks_win_over_an_expired_deadline_in_both_modes() {
        for bridge in [false, true] {
            let (handle, sender, _, _, delivered) = test_stream_mode(bridge);
            sender
                .send(Some(Ok(block(IqSamples::Cu8(vec![4, 3, 2, 1])))))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while delivered.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                thread::yield_now();
            }
            let mut out = [0_u8; 4];
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, out.as_mut_ptr(), 4, 1) },
                4
            );
            assert_eq!(out, [4, 3, 2, 1]);
            unsafe { sdrfox_close_stream(handle) };
        }
    }

    #[test]
    fn impossible_positive_return_length_is_rejected_before_copy() {
        let (handle, _sender, ..) = test_stream();
        let result = unsafe {
            sdrfox_read_stream(
                handle,
                std::ptr::NonNull::<u8>::dangling().as_ptr(),
                (isize::MAX as usize).saturating_add(1),
                1,
            )
        };
        assert_eq!(result, -1);
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn timeout_does_not_stop_or_poison_future_reads() {
        let (handle, sender, ..) = test_stream();
        let mut out = [0u8; 4];
        let started = Instant::now();
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, out.as_mut_ptr(), 4, 20) },
            0
        );
        assert!(started.elapsed() < Duration::from_millis(250));

        sender
            .send(Some(Ok(block(IqSamples::Cu8(vec![9, 8, 7, 6])))))
            .unwrap();
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, out.as_mut_ptr(), 4, 500) },
            4
        );
        assert_eq!(out, [9, 8, 7, 6]);
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn timeout_includes_waiting_behind_another_reader() {
        let (handle, sender, ..) = test_stream();
        let token = handle as usize;
        let first = thread::spawn(move || {
            let mut byte = 0u8;
            unsafe { sdrfox_read_stream(token as *mut SdrFoxStream, &raw mut byte, 1, 0) }
        });
        thread::sleep(Duration::from_millis(20));

        let mut byte = 0u8;
        let started = Instant::now();
        assert_eq!(
            unsafe { sdrfox_read_stream(handle, &raw mut byte, 1, 20) },
            0
        );
        assert!(started.elapsed() < Duration::from_millis(250));

        unsafe { sdrfox_stop_stream(handle) };
        assert_eq!(first.join().unwrap(), 0);
        unsafe { sdrfox_close_stream(handle) };
        drop(sender);
    }

    #[test]
    fn concurrent_close_wakes_reader_and_waiter_without_resurrection() {
        let (handle, sender, stopped, finished, _) = test_stream();
        let token = handle as usize;
        let first = thread::spawn(move || {
            let mut byte = 0_u8;
            unsafe { sdrfox_read_stream(token as *mut SdrFoxStream, &raw mut byte, 1, 0) }
        });
        thread::sleep(Duration::from_millis(20));
        let token = handle as usize;
        let second = thread::spawn(move || {
            let mut byte = 0_u8;
            unsafe { sdrfox_read_stream(token as *mut SdrFoxStream, &raw mut byte, 1, 0) }
        });
        thread::sleep(Duration::from_millis(20));

        unsafe { sdrfox_close_stream(handle) };
        assert_eq!(first.join().unwrap(), 0);
        assert_eq!(second.join().unwrap(), 0);
        assert!(resolve_stream(handle).is_none());
        assert!(stopped.load(Ordering::Acquire));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(finished.load(Ordering::Acquire));
        drop(sender);
    }

    #[test]
    fn stop_wakes_reader_and_close_is_idempotent() {
        let (handle, sender, stopped, finished, _) = test_stream();
        let token = handle as usize;
        let reader = thread::spawn(move || {
            let mut byte = 0u8;
            unsafe { sdrfox_read_stream(token as *mut SdrFoxStream, &raw mut byte, 1, 0) }
        });
        thread::sleep(Duration::from_millis(20));
        unsafe { sdrfox_stop_stream(handle) };
        assert_eq!(reader.join().unwrap(), 0);
        unsafe {
            sdrfox_close_stream(handle);
            sdrfox_close_stream(handle);
        }
        assert!(resolve_stream(handle).is_none());
        for _ in 0..50 {
            if finished.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(stopped.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire));
        drop(sender);
    }

    #[test]
    fn close_cancels_and_joins_a_silent_core_receiver() {
        let (handle, sender, stopped, finished, _) = test_stream();
        let token = handle as usize;
        let (done_tx, done_rx) = mpsc::channel();

        thread::spawn(move || {
            unsafe { sdrfox_close_stream(token as *mut SdrFoxStream) };
            let _ = done_tx.send(());
        });

        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
            "close must cancel and join a silent receiver"
        );
        assert!(stopped.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire));
        drop(sender);
    }

    #[test]
    fn stop_cancels_a_saturated_bridge_without_blocking_send() {
        let (handle, sender, stopped, finished, delivered_blocks) = test_stream();
        for value in 0..(BRIDGE_DEPTH + 2) {
            let byte = u8::try_from(value).expect("test bridge depth fits in a byte");
            sender
                .send(Some(Ok(block(IqSamples::Cu8(vec![byte; 4])))))
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while delivered_blocks.load(Ordering::Acquire) <= BRIDGE_DEPTH && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert!(delivered_blocks.load(Ordering::Acquire) > BRIDGE_DEPTH);

        unsafe { sdrfox_stop_stream(handle) };
        let deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }

        assert!(stopped.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire));
        unsafe { sdrfox_close_stream(handle) };
    }

    #[test]
    fn copied_last_error_outlives_its_device_storage() {
        let source = Mutex::new(Some(CString::new("device error").unwrap()));
        let pointer = copy_last_error_to_thread(&source);
        drop(source);

        assert!(!pointer.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(pointer) }.to_bytes(),
            b"device error"
        );
    }

    #[test]
    fn last_error_on_null_returns_null() {
        unsafe {
            assert!(sdrfox_last_error(std::ptr::null_mut()).is_null());
        }
    }

    #[test]
    fn set_frequency_on_null_returns_error_code() {
        unsafe {
            assert_eq!(sdrfox_set_frequency(std::ptr::null_mut(), 100_000_000), -1);
        }
    }
}
