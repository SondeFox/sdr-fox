//! JNI bridge implementation (separated from lib.rs to avoid formatter races).
//!
//! Streaming is a synchronous pull API into a caller-owned direct
//! `ByteBuffer`. Native worker threads never enter the JVM, and reads allocate
//! no Java objects or native spill buffers per block.
//!
//! ## Handle registry
//!
//! Devices and streams are stored in typed `OnceLock<RwLock<...>>` registries
//! rather than handed out as raw pointers. Lookups clone an `Arc` under a read
//! lock, then release the registry before device work, receives, or stop
//! callbacks. Closing removes a monotonic generation under a write lock; the
//! object drops only after its last in-flight clone is released.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use jni::objects::{JByteBuffer, JIntArray, JLongArray, JObject, JString};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jintArray, jlong};
use jni::{jni_str, EnvUnowned as JNIEnv};

use sdr_fox_airspy::AirspyBackend;
use sdr_fox_core::sample::StreamStopHandle;
use sdr_fox_core::{
    DeviceDescriptor, DeviceKind, GainMode, GainRequest, GainStageId, IqBlock, IqFormat, IqSamples,
    SdrBackend, SdrDevice, StreamConfig, StreamHandle, Transport,
};
use sdr_fox_rtlsdr::RtlSdrBackend;

/// The native-side device, stored once under an `Arc` in the global registry
/// and cloned out per call so each setter/getter works on its own reference.
/// Because the `Arc` is reference-counted and the inner device is behind a
/// `Mutex`, no `&'static mut` aliasing exists: this is sound under concurrent
/// access from multiple JNI threads.
pub(crate) struct NativeDevice {
    /// The device, behind a Mutex so the struct is `Send + Sync` for the
    /// streaming worker thread and for concurrent JNI calls.
    pub(crate) device: Mutex<Box<dyn SdrDevice>>,
}

impl NativeDevice {
    fn new(device: Box<dyn SdrDevice>) -> Self {
        Self {
            device: Mutex::new(device),
        }
    }
}

/// A pull stream retained independently from its device registry entry.
///
/// `_parent` deliberately owns the device `Arc`: closing the Java `SdrFox`
/// object removes its device handle but cannot invalidate a live stream. The
/// stop capability is independent of the receive mutex, so close/stop can wake
/// a thread blocked in `recv` without deadlocking behind it.
struct NativeStream {
    _parent: Arc<NativeDevice>,
    state: Mutex<NativeStreamState>,
    stop: StreamStopHandle,
    stopped: AtomicBool,
    stats: Mutex<NativeStreamStats>,
}

struct NativeStreamState {
    handle: StreamHandle,
    pending: Option<IqBlock>,
    pending_offset: usize,
}

/// One coherent stats snapshot, recorded per delivered block.
///
/// Two kinds of counter live here and must not be conflated:
///
/// - **Monotonic** (accumulate since stream start): `last_dropped` and
///   `last_sequence` mirror the block's own monotonic fields; `blocks_read`,
///   `bytes_read`, `total_clips`, and `total_raw_samples` are summed by this
///   layer. Consumers diff successive snapshots for windowed figures.
/// - **Per-block** (reset every block): `last_block_clips` and
///   `last_block_raw_samples` are the most recent block's raw ADC-domain clip
///   telemetry only, exactly as delivered in [`IqBlock::clips`] /
///   [`IqBlock::raw_samples`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NativeStreamStats {
    last_dropped: u64,
    last_sequence: u64,
    blocks_read: u64,
    bytes_read: u64,
    last_block_clips: u64,
    last_block_raw_samples: u64,
    total_clips: u64,
    total_raw_samples: u64,
}

impl NativeStream {
    fn new(parent: Arc<NativeDevice>, handle: StreamHandle) -> Self {
        let stop = handle.stop_handle();
        Self {
            _parent: parent,
            state: Mutex::new(NativeStreamState {
                handle,
                pending: None,
                pending_offset: 0,
            }),
            stop,
            stopped: AtomicBool::new(false),
            stats: Mutex::new(NativeStreamStats::default()),
        }
    }

    fn request_stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.stop.stop();
        }
    }

    fn record_block(&self, block: &IqBlock) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.last_dropped = block.dropped;
        stats.last_sequence = block.sequence;
        stats.blocks_read = stats.blocks_read.saturating_add(1);
        // `clips`/`raw_samples` are per-block on the IqBlock (unlike the
        // monotonic `dropped`/`sequence`), so snapshot the latest block's
        // values AND accumulate running totals for windowed consumers.
        stats.last_block_clips = block.clips;
        stats.last_block_raw_samples = block.raw_samples;
        stats.total_clips = stats.total_clips.saturating_add(block.clips);
        stats.total_raw_samples = stats.total_raw_samples.saturating_add(block.raw_samples);
    }

    fn record_bytes(&self, bytes: usize) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.bytes_read = stats.bytes_read.saturating_add(bytes as u64);
    }

    fn stats(&self) -> NativeStreamStats {
        *self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        self.request_stop();
    }
}

/// Global handle registry: a monotonic positive `jlong` generation maps to an
/// `Arc<NativeDevice>`. Generations are never truncated or recycled.
///
/// Replacing the previous raw-`*mut NativeDevice` scheme removes two unsafety
/// hazards:
/// 1. A stale/dangling handle could dereference freed memory.
/// 2. Two threads holding the same `&'static mut NativeDevice` from
///    `from_handle` was undefined behavior (aliased `&mut`).
///
/// With this design `from_handle` returns a fresh `Arc` clone, so each call
/// owns its own strong reference. `nativeClose` simply removes the entry; a
/// double-close finds nothing and is a no-op. If another thread still holds a
/// clone, the `NativeDevice` is dropped only when the last clone goes away.
struct HandleRegistry<T> {
    next_id: Option<jlong>,
    map: HashMap<jlong, Arc<T>>,
}

impl<T> Default for HandleRegistry<T> {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            map: HashMap::new(),
        }
    }
}

impl<T> HandleRegistry<T> {
    fn insert(&mut self, value: T) -> jlong {
        let Some(id) = self.next_id else {
            return 0;
        };
        self.next_id = id.checked_add(1);
        self.map.insert(id, Arc::new(value));
        id
    }
}

static DEVICE_REGISTRY: OnceLock<RwLock<HandleRegistry<NativeDevice>>> = OnceLock::new();
static STREAM_REGISTRY: OnceLock<RwLock<HandleRegistry<NativeStream>>> = OnceLock::new();

fn device_registry() -> &'static RwLock<HandleRegistry<NativeDevice>> {
    DEVICE_REGISTRY.get_or_init(|| RwLock::new(HandleRegistry::default()))
}

fn stream_registry() -> &'static RwLock<HandleRegistry<NativeStream>> {
    STREAM_REGISTRY.get_or_init(|| RwLock::new(HandleRegistry::default()))
}

/// Insert a `NativeDevice` into the registry and return its ID (as a `jlong`).
/// The ID is never zero; a zero handle always denotes an invalid/missing
/// device.
fn box_handle(dev: Box<dyn SdrDevice>) -> jlong {
    device_registry()
        .write()
        .expect("JNI device registry lock poisoned")
        .insert(NativeDevice::new(dev))
}

/// Clone the `Arc<NativeDevice>` for `handle` out of the registry.
///
/// Returns `None` for a zero handle or an unknown id (e.g. already closed or
/// double-close). The returned `Arc` keeps the device alive for the duration
/// of the call regardless of what other threads do.
fn from_handle(handle: jlong) -> Option<std::sync::Arc<NativeDevice>> {
    if handle <= 0 {
        return None;
    }
    device_registry()
        .read()
        .expect("JNI device registry lock poisoned")
        .map
        .get(&handle)
        .cloned()
}

/// Remove `handle` from the registry. Idempotent: a second close (or an
/// unknown id) is a no-op. The `NativeDevice` drops when the last outstanding
/// `Arc` clone (held by other threads) is released.
fn close_handle(handle: jlong) {
    if handle <= 0 {
        return;
    }
    device_registry()
        .write()
        .expect("JNI device registry lock poisoned")
        .map
        .remove(&handle);
}

fn insert_stream(stream: NativeStream) -> jlong {
    stream_registry()
        .write()
        .expect("JNI stream registry lock poisoned")
        .insert(stream)
}

fn stream_from_handle(handle: jlong) -> Option<Arc<NativeStream>> {
    if handle <= 0 {
        return None;
    }
    stream_registry()
        .read()
        .expect("JNI stream registry lock poisoned")
        .map
        .get(&handle)
        .cloned()
}

fn close_stream_handle(handle: jlong) {
    if handle <= 0 {
        return;
    }
    let stream = stream_registry()
        .write()
        .expect("JNI stream registry lock poisoned")
        .map
        .remove(&handle);
    if let Some(stream) = stream {
        // Never run stop callbacks while a registry lock is held.
        stream.request_stop();
    }
}

/// Open by file descriptor (the Android path). `fd` comes from
/// `UsbDeviceConnection.getFileDescriptor()`; `product_name` is the USB product
/// string the Java side read from `UsbDevice.getProductName()` (needed so the
/// Airspy driver can detect the Mini variant and the descriptor carries the
/// real product label). Returns 0 on failure.
///
/// # Safety
///

/// Route Rust `tracing`/`log` output into logcat, once per process.
///
/// Without this the whole Rust stack is silent on Android: a streaming failure
/// surfaces as one `SdrException` string with none of the transport-level
/// detail that makes it diagnosable. Enabled at `Debug` so the USB ring's
/// submit/complete accounting is visible under `adb logcat -s sdr-fox`.
///
/// Called from every entry point that can start a device session; `Once` makes
/// repeated calls free.
#[cfg(target_os = "android")]
fn init_android_logging() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("sdr-fox"),
        );
        tracing::info!("sdr-fox JNI logging initialised");
    });
}

/// No-op off Android; desktop callers install their own subscriber.
#[cfg(not(target_os = "android"))]
fn init_android_logging() {}

/// `env` must be a valid `JNIEnv` and `product_name` a valid `JString` (or null).
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeOpenByFd(
    mut env: JNIEnv,
    _this: JObject<'_>,
    fd: jint,
    backend_kind: jint,
    product_name: JString<'_>,
) -> jlong {
    init_android_logging();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<jlong, String> {
        if fd < 0 {
            return Err(format!("invalid USB file descriptor: {fd}"));
        }
        let (kind, explicit_product_name) = decode_backend_kind(backend_kind)?;
        // Read the product string from Java (null/empty → None). The Airspy
        // driver keys Mini detection off this string, so passing None (the old
        // behaviour) silently mis-detected Mini devices as R2. jni-0.22 hands
        // native methods an `EnvUnowned`; borrow a real env via `with_env` for
        // the string read, then resolve the `EnvOutcome` into an `Outcome`.
        let product_name_str = read_optional_java_string(&mut env, &product_name)?
            .filter(|name| !name.trim().is_empty())
            .or(explicit_product_name);
        let desc = DeviceDescriptor {
            vendor_id: if matches!(kind, DeviceKind::Airspy) {
                0x1d50
            } else {
                0x0bda
            },
            product_id: if matches!(kind, DeviceKind::Airspy) {
                0x60a1
            } else {
                0x2832
            },
            vendor_name: None,
            product_name: product_name_str,
            serial: None,
            index: 0,
            kind,
        };
        open_fd_device(fd, &desc)
    }));
    finish_open(&mut env, result)
}

/// Open with metadata from the same permission-authorized Android USB device.
/// The old entry point remains available for clients that have only an fd.
///
/// # Safety
/// `env` must be valid and both string arguments must be valid JNI strings or null.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeOpenByFdWithIdentity(
    mut env: JNIEnv,
    _this: JObject<'_>,
    fd: jint,
    backend_kind: jint,
    vendor_id: jint,
    product_id: jint,
    manufacturer_name: JString<'_>,
    product_name: JString<'_>,
) -> jlong {
    init_android_logging();
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<jlong, String> {
        if fd < 0 {
            return Err(format!("invalid USB file descriptor: {fd}"));
        }
        let manufacturer = read_optional_java_string(&mut env, &manufacturer_name)?;
        let product = read_optional_java_string(&mut env, &product_name)?;
        let desc = descriptor_from_usb_identity(
            backend_kind,
            vendor_id,
            product_id,
            manufacturer,
            product,
        )?;
        open_fd_device(fd, &desc)
    }));
    finish_open(&mut env, result)
}

fn descriptor_from_usb_identity(
    backend_kind: jint,
    vendor_id: jint,
    product_id: jint,
    manufacturer: Option<String>,
    product: Option<String>,
) -> Result<DeviceDescriptor, String> {
    let (kind, explicit_product) = decode_backend_kind(backend_kind)?;
    Ok(DeviceDescriptor {
        vendor_id: u16::try_from(vendor_id).map_err(|_| "invalid USB vendor ID".to_owned())?,
        product_id: u16::try_from(product_id).map_err(|_| "invalid USB product ID".to_owned())?,
        // Preserve exact spelling: the board-specific V4 predicate deliberately
        // requires both published strings and actual VID/PID. No serial needed.
        vendor_name: manufacturer.filter(|name| !name.trim().is_empty()),
        product_name: product
            .filter(|name| !name.trim().is_empty())
            .or(explicit_product),
        serial: None,
        index: 0,
        kind,
    })
}

fn open_fd_device(fd: jint, desc: &DeviceDescriptor) -> Result<jlong, String> {
    let transport = open_fd_transport(fd, desc.kind).map_err(|e| e.to_string())?;
    let backend: Box<dyn SdrBackend> = match desc.kind {
        DeviceKind::Airspy => Box::new(AirspyBackend),
        _ => Box::new(RtlSdrBackend),
    };
    let dev = backend.open(desc, transport).map_err(|e| e.to_string())?;
    let handle = box_handle(dev);
    if handle == 0 {
        Err("native device handle registry exhausted".to_owned())
    } else {
        Ok(handle)
    }
}

fn finish_open(env: &mut JNIEnv<'_>, result: std::thread::Result<Result<jlong, String>>) -> jlong {
    match result {
        Ok(Ok(handle)) => handle,
        Ok(Err(message)) => {
            throw_runtime_exception(env, &message);
            0
        }
        Err(_) => {
            throw_runtime_exception(env, "panic while opening SDR device");
            0
        }
    }
}

fn decode_backend_kind(kind: jint) -> Result<(DeviceKind, Option<String>), String> {
    match kind {
        0 => Ok((DeviceKind::RtlSdr, None)),
        1 => Ok((DeviceKind::Airspy, None)),
        // An explicit Mini discriminator keeps model selection reliable on
        // devices/ROMs that do not expose USB product strings.
        2 => Ok((DeviceKind::Airspy, Some("Airspy Mini".to_owned()))),
        other => Err(format!("invalid SDR backend kind: {other}")),
    }
}

fn read_optional_java_string(
    env: &mut JNIEnv<'_>,
    value: &JString<'_>,
) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    match env
        .with_env(|e| -> jni::errors::Result<String> {
            let chars = value.mutf8_chars(e)?;
            Ok(chars.to_string())
        })
        .into_outcome()
    {
        jni::Outcome::Ok(value) => Ok(Some(value)),
        jni::Outcome::Err(error) => Err(format!("failed to read USB product name: {error}")),
        jni::Outcome::Panic(_) => Err("panic while reading USB product name".to_owned()),
    }
}

fn throw_runtime_exception(env: &mut JNIEnv<'_>, message: &str) {
    let message = JNIString::from(message);
    let _ = env
        .with_env(|e| e.throw_new(jni_str!("java/lang/RuntimeException"), message.borrowed()))
        .into_outcome();
}

fn throw_closed_exception(env: &mut JNIEnv<'_>) {
    let _ = env
        .with_env(|e| {
            e.throw_new(
                jni_str!("java/lang/IllegalStateException"),
                jni_str!("SDR device is closed"),
            )
        })
        .into_outcome();
}

/// Construct the nusb fd transport for Android. Requires the `android`
/// feature (the `nusb_fd` transport); without it, errors cleanly.
fn open_fd_transport(
    fd: jint,
    _kind: DeviceKind,
) -> Result<Box<dyn Transport>, sdr_fox_core::SdrError> {
    #[cfg(feature = "android")]
    {
        // SAFETY: the Kotlin helper (`ParcelFileDescriptor.fromFd`) duplicated
        // the framework fd before calling native, so this owned descriptor's
        // lifetime is independent of the JVM's `UsbDeviceConnection`.
        // `NusbFdTransport::new` rejects a negative fd before adopting it, and
        // nusb closes the descriptor only when the device is dropped.
        let transport = unsafe { sdr_fox_transport::NusbFdTransport::new(fd) }?;
        Ok(Box::new(transport))
    }
    #[cfg(not(feature = "android"))]
    {
        Err(sdr_fox_core::SdrError::Unsupported(format!(
            "fd-based open requires the 'android' feature (got fd={fd})"
        )))
    }
}

enum DeviceCallError {
    Closed,
    Failed(String),
    Panicked,
}

/// Resolve a generation handle, retain its `Arc` for the full operation, and
/// serialize mutable device access. Every failure remains inside Rust and is
/// translated to a Java exception by [`report_device_call`].
fn with_device<R>(
    handle: jlong,
    body: impl FnOnce(&mut dyn SdrDevice) -> Result<R, sdr_fox_core::SdrError>,
) -> Result<R, DeviceCallError> {
    match catch_unwind(AssertUnwindSafe(|| {
        let device = from_handle(handle).ok_or(DeviceCallError::Closed)?;
        let mut guard = device
            .device
            .lock()
            .map_err(|_| DeviceCallError::Failed("native device mutex poisoned".to_owned()))?;
        body(guard.as_mut()).map_err(|error| DeviceCallError::Failed(error.to_string()))
    })) {
        Ok(result) => result,
        Err(_) => Err(DeviceCallError::Panicked),
    }
}

fn report_device_call(env: &mut JNIEnv<'_>, result: Result<(), DeviceCallError>) {
    match result {
        Ok(()) => {}
        Err(DeviceCallError::Closed) => throw_closed_exception(env),
        Err(DeviceCallError::Failed(message)) => throw_runtime_exception(env, &message),
        Err(DeviceCallError::Panicked) => {
            throw_runtime_exception(env, "panic in native SDR operation");
        }
    }
}

fn throw_illegal_argument(env: &mut JNIEnv<'_>, message: &str) {
    let message = JNIString::from(message);
    let _ = env
        .with_env(|e| {
            e.throw_new(
                jni_str!("java/lang/IllegalArgumentException"),
                message.borrowed(),
            )
        })
        .into_outcome();
}

/// Set the center frequency.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetFrequency(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    hz: jlong,
) {
    let Ok(hz) = u64::try_from(hz) else {
        throw_illegal_argument(&mut env, "frequency must be non-negative");
        return;
    };
    report_device_call(&mut env, with_device(handle, |dev| dev.set_frequency(hz)));
}

/// Set the sample rate and return the rate the hardware actually settled on,
/// in Hz.
///
/// The settled rate may differ from the request (integer-ratio synthesis on
/// RTL-SDR, a discrete firmware table on Airspy) and is what every downstream
/// DSP consumer must size against. Returns 0 after throwing on failure.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetSampleRate(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    hz: jint,
) -> jint {
    let Ok(hz) = u32::try_from(hz) else {
        throw_illegal_argument(&mut env, "sample rate must be non-negative");
        return 0;
    };
    match with_device(handle, |dev| dev.set_sample_rate(hz)) {
        Ok(settled) => jint::try_from(settled).unwrap_or(jint::MAX),
        Err(error) => {
            report_device_call(&mut env, Err(error));
            0
        }
    }
}

/// Convert device-reported sample rates (`u32` Hz) into `jint` array elements.
///
/// Values above `jint::MAX` saturate rather than wrap; no supported hardware
/// reports a rate anywhere near 2.1 gigasamples, so saturation is purely
/// defensive.
fn sample_rates_to_jint(rates: &[u32]) -> Vec<jint> {
    rates
        .iter()
        .map(|&hz| jint::try_from(hz).unwrap_or(jint::MAX))
        .collect()
}

/// Enumerate the discrete sample rates the device supports, in Hz, as a new
/// `int[]`.
///
/// An **empty array means the rates are not enumerable** (e.g. RTL-SDR's
/// continuous synthesizer ranges): any in-range rate may be attempted via
/// `nativeSetSampleRate`, which reports the settled rate. Airspy devices
/// return their firmware-queried table in firmware index order. Returns null
/// after throwing on failure.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeGetSampleRates(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
) -> jintArray {
    let rates = match with_device(handle, |dev| Ok(dev.supported_sample_rates())) {
        Ok(rates) => rates,
        Err(error) => {
            report_device_call(&mut env, Err(error));
            return std::ptr::null_mut();
        }
    };
    let values = sample_rates_to_jint(&rates);
    let result = env
        .with_env(|e| -> jni::errors::Result<jintArray> {
            let array = JIntArray::new(e, values.len())?;
            array.set_region(e, 0, &values)?;
            Ok(array.into_raw())
        })
        .into_outcome();
    match result {
        jni::Outcome::Ok(array) => array,
        jni::Outcome::Err(error) => {
            throw_runtime_exception(
                &mut env,
                &format!("failed to build sample-rate array: {error}"),
            );
            std::ptr::null_mut()
        }
        jni::Outcome::Panic(_) => {
            throw_runtime_exception(&mut env, "panic while building sample-rate array");
            std::ptr::null_mut()
        }
    }
}

/// Set the analog channel bandwidth in Hz.
///
/// On RTL-SDR this programs the R82xx IF filter AND the matching RTL2832 IF
/// frequency, then retunes so the centre stays put — the three are one atomic
/// operation and must not be issued separately.
///
/// **Callers must invoke this after every sample-rate change.** The tuner
/// comes up from `init_tuner` with the DVB-T filter (~6 MHz at IF 3.57 MHz)
/// and nothing narrows it implicitly. A host that decimates 250 kHz out of a
/// 6 MHz-wide IF still sees every signal in that 6 MHz at the tuner's AGC
/// detector, which then backs the front end off — measured at roughly 23 dB
/// of lost sensitivity against a 400 MHz radiosonde. Passing the *settled*
/// sample rate reproduces librtlsdr's `rtlsdr_set_tuner_bandwidth(dev, 0)`
/// automatic behaviour.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetBandwidth(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    hz: jint,
) {
    let Ok(hz) = u32::try_from(hz) else {
        throw_illegal_argument(&mut env, "bandwidth must be non-negative");
        return;
    };
    report_device_call(&mut env, with_device(handle, |dev| dev.set_bandwidth(hz)));
}

/// Enable/disable the bias tee.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetBiasTee(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    on: jint,
) {
    report_device_call(
        &mut env,
        with_device(handle, |dev| dev.set_bias_tee(on != 0)),
    );
}

/// Enable/disable the **device-wide** AGC.
///
/// On RTL-SDR this drives the RTL2832 **digital** AGC loop in the demodulator
/// — a different knob from the R82xx tuner's automatic gain mode, which is
/// `nativeSetGainMode`. On Airspy this switches the coupled LNA+mixer AGC
/// flags together; use `nativeSetStageAgc` to control one stage on its own.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetAgc(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    on: jint,
) {
    report_device_call(&mut env, with_device(handle, |dev| dev.set_agc(on != 0)));
}

/// Set the overall gain (tenths dB).
///
/// Devices that only accept per-stage gain (Airspy) reject overall requests;
/// use `nativeSetGainStage` for those.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetGain(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    tenths_db: jint,
) {
    report_device_call(
        &mut env,
        with_device(handle, |dev| dev.set_gain(GainRequest::overall(tenths_db))),
    );
}

/// Decode a JNI gain-stage code through the core's stable cross-language
/// mapping ([`GainStageId::from_code`]): 0 = LNA, 1 = MIXER, 2 = VGA.
fn decode_gain_stage(code: jint) -> Result<GainStageId, String> {
    GainStageId::from_code(code)
        .ok_or_else(|| format!("unknown gain stage code {code} (expected 0=LNA, 1=MIXER, 2=VGA)"))
}

/// Set the gain of a single analog stage (tenths dB).
///
/// `stage` uses the stable core contract: 0 = LNA, 1 = MIXER, 2 = VGA. This is
/// the only way to set gain on devices that reject overall requests (Airspy).
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetGainStage(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    stage: jint,
    tenths_db: jint,
) {
    let stage = match decode_gain_stage(stage) {
        Ok(stage) => stage,
        Err(message) => {
            throw_illegal_argument(&mut env, &message);
            return;
        }
    };
    report_device_call(
        &mut env,
        with_device(handle, |dev| {
            dev.set_gain(GainRequest::per_stage(stage, tenths_db))
        }),
    );
}

/// Switch the tuner between automatic (`auto == true`) and manual gain.
///
/// This is the tuner's gain-mode knob (e.g. the R82xx auto gain used by
/// software AGC loops) — **not** the RTL2832 digital AGC, which stays on
/// `nativeSetAgc`.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetGainMode(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    auto: jboolean,
) {
    let mode = if auto {
        GainMode::Auto
    } else {
        GainMode::Manual
    };
    report_device_call(&mut env, with_device(handle, |dev| dev.set_gain_mode(mode)));
}

/// Enable/disable the AGC loop of a **single** gain stage, independently of
/// the others (e.g. Airspy's separate LNA and mixer AGC).
///
/// `stage` uses the stable core contract: 0 = LNA, 1 = MIXER, 2 = VGA. Devices
/// without per-stage AGC hardware (RTL-SDR), and stages without an AGC loop
/// (Airspy VGA), throw. `nativeSetAgc` remains the coupled device-wide switch.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeSetStageAgc(
    mut env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
    stage: jint,
    on: jboolean,
) {
    let stage = match decode_gain_stage(stage) {
        Ok(stage) => stage,
        Err(message) => {
            throw_illegal_argument(&mut env, &message);
            return;
        }
    };
    report_device_call(
        &mut env,
        with_device(handle, |dev| dev.set_stage_agc(stage, on)),
    );
}

fn decode_stream_format(format: jint) -> Result<IqFormat, String> {
    match format {
        0 => Ok(IqFormat::Cu8),
        1 => Ok(IqFormat::Cs8),
        2 => Ok(IqFormat::Cs16),
        3 => Ok(IqFormat::Cf32),
        other => Err(format!("invalid IQ format: {other}")),
    }
}

fn positive_usize(value: jint, name: &str) -> Result<usize, String> {
    let value = usize::try_from(value).map_err(|_| format!("{name} must be positive"))?;
    if value == 0 {
        Err(format!("{name} must be positive"))
    } else {
        Ok(value)
    }
}

/// Start a native pull stream. The returned generation handle owns an Arc to
/// the parent device and remains valid even after `nativeClose(device)`.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeStartStream(
    mut env: JNIEnv,
    _this: JObject<'_>,
    device_handle: jlong,
    format: jint,
    buffer_count: jint,
    buffer_size: jint,
    queue_depth: jint,
) -> jlong {
    let config = (|| {
        Ok::<_, String>(StreamConfig {
            format: decode_stream_format(format)?,
            buffer_count: positive_usize(buffer_count, "bufferCount")?,
            buffer_size: positive_usize(buffer_size, "bufferSize")?,
            queue_depth: positive_usize(queue_depth, "queueDepth")?,
        })
    })();
    let config = match config {
        Ok(config) => config,
        Err(message) => {
            throw_illegal_argument(&mut env, &message);
            return 0;
        }
    };

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<jlong, DeviceCallError> {
        let parent = from_handle(device_handle).ok_or(DeviceCallError::Closed)?;
        let handle = {
            let mut device = parent
                .device
                .lock()
                .map_err(|_| DeviceCallError::Failed("native device mutex poisoned".to_owned()))?;
            device
                .start_stream(config)
                .map_err(|error| DeviceCallError::Failed(error.to_string()))?
        };
        let stream_handle = insert_stream(NativeStream::new(parent, handle));
        if stream_handle == 0 {
            Err(DeviceCallError::Failed(
                "native stream handle registry exhausted".to_owned(),
            ))
        } else {
            Ok(stream_handle)
        }
    }));

    match result {
        Ok(Ok(handle)) => handle,
        Ok(Err(error)) => {
            report_device_call(&mut env, Err(error));
            0
        }
        Err(_) => {
            throw_runtime_exception(&mut env, "panic while starting native SDR stream");
            0
        }
    }
}

#[derive(Debug)]
enum StreamReadError {
    Closed,
    Busy,
    Failed(String),
}

fn samples_as_bytes(samples: &IqSamples) -> &[u8] {
    match samples {
        IqSamples::Cu8(bytes) => bytes,
        IqSamples::Cs8(samples) => unsafe {
            // SAFETY: i8/u8 have identical layouts and the borrow is bounded
            // by the input vector.
            std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), samples.len())
        },
        IqSamples::Cs16(samples) => unsafe {
            // SAFETY: vectors are contiguous; this native-endian view has the
            // exact initialized storage size.
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples.as_slice()),
            )
        },
        IqSamples::Cf32(samples) => unsafe {
            // SAFETY: vectors are contiguous; this native-endian view has the
            // exact initialized storage size.
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples.as_slice()),
            )
        },
    }
}

fn copy_pending(state: &mut NativeStreamState, output: &mut [u8]) -> usize {
    let Some(block) = state.pending.as_ref() else {
        return 0;
    };
    let bytes = samples_as_bytes(&block.samples);
    let available = bytes.len().saturating_sub(state.pending_offset);
    let copied = available.min(output.len());
    output[..copied].copy_from_slice(&bytes[state.pending_offset..state.pending_offset + copied]);
    state.pending_offset += copied;
    if state.pending_offset == bytes.len() {
        state.pending = None;
        state.pending_offset = 0;
    }
    copied
}

fn read_stream_once(
    stream: &NativeStream,
    output: &mut [u8],
    timeout: Option<Duration>,
) -> Result<usize, StreamReadError> {
    let mut state = match stream.state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::WouldBlock) => return Err(StreamReadError::Busy),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Err(StreamReadError::Failed(
                "native stream mutex poisoned".to_owned(),
            ));
        }
    };
    if stream.stopped.load(Ordering::Acquire) {
        return Ok(0);
    }

    let copied = copy_pending(&mut state, output);
    if copied != 0 || output.is_empty() {
        stream.record_bytes(copied);
        return Ok(copied);
    }

    let received = match timeout {
        Some(duration) => {
            let deadline = Instant::now()
                .checked_add(duration)
                .ok_or_else(|| StreamReadError::Failed("read timeout is too large".to_owned()))?;
            state.handle.recv_deadline(deadline)
        }
        None => state.handle.recv(),
    };
    let Some(received) = received else {
        stream.stopped.store(true, Ordering::Release);
        return Ok(0);
    };
    let block = match received {
        Ok(block) => block,
        Err(sdr_fox_core::SdrError::Timeout) => return Ok(0),
        Err(error) => {
            stream.stopped.store(true, Ordering::Release);
            return Err(StreamReadError::Failed(error.to_string()));
        }
    };
    if samples_as_bytes(&block.samples).is_empty() {
        stream.request_stop();
        return Err(StreamReadError::Failed(
            "stream returned an empty IQ block".to_owned(),
        ));
    }

    stream.record_block(&block);
    state.pending = Some(block);
    state.pending_offset = 0;
    let copied = copy_pending(&mut state, output);
    stream.record_bytes(copied);
    Ok(copied)
}

fn throw_stream_read_error(env: &mut JNIEnv<'_>, error: StreamReadError) {
    match error {
        StreamReadError::Closed => {
            let _ = env
                .with_env(|e| {
                    e.throw_new(
                        jni_str!("java/lang/IllegalStateException"),
                        jni_str!("SDR stream is closed"),
                    )
                })
                .into_outcome();
        }
        StreamReadError::Busy => {
            let _ = env
                .with_env(|e| {
                    e.throw_new(
                        jni_str!("java/lang/IllegalStateException"),
                        jni_str!("another thread is already reading this SDR stream"),
                    )
                })
                .into_outcome();
        }
        StreamReadError::Failed(message) => throw_runtime_exception(env, &message),
    }
}

fn direct_region_address(
    address: *mut u8,
    offset: usize,
    length: usize,
) -> Result<Option<*mut u8>, StreamReadError> {
    if length == 0 {
        return Ok(None);
    }
    let address = std::ptr::NonNull::new(address)
        .ok_or_else(|| StreamReadError::Failed("direct buffer has a null address".to_owned()))?;
    // SAFETY: the JNI entry point validates offset + length against the direct
    // buffer capacity before calling this helper.
    Ok(Some(unsafe { address.as_ptr().add(offset) }))
}

/// Pull at most one native IQ block into a direct byte buffer region.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeReadStream(
    mut env: JNIEnv,
    _this: JObject<'_>,
    stream_handle: jlong,
    buffer: JByteBuffer<'_>,
    offset: jint,
    length: jint,
    timeout_ms: jint,
) -> jint {
    let (offset, length, timeout) = match (
        usize::try_from(offset),
        usize::try_from(length),
        u64::try_from(timeout_ms),
    ) {
        (Ok(offset), Ok(length), Ok(0)) => (offset, length, None),
        (Ok(offset), Ok(length), Ok(timeout)) => {
            (offset, length, Some(Duration::from_millis(timeout)))
        }
        _ => {
            throw_illegal_argument(
                &mut env,
                "offset, length, and timeoutMs must be non-negative",
            );
            return -1;
        }
    };

    let direct = env
        .with_env(|e| -> jni::errors::Result<(*mut u8, usize)> {
            Ok((
                e.get_direct_buffer_address(&buffer)?,
                e.get_direct_buffer_capacity(&buffer)?,
            ))
        })
        .into_outcome();
    let (address, capacity) = match direct {
        jni::Outcome::Ok(parts) => parts,
        jni::Outcome::Err(error) => {
            throw_illegal_argument(&mut env, &format!("buffer must be direct: {error}"));
            return -1;
        }
        jni::Outcome::Panic(_) => {
            throw_runtime_exception(&mut env, "panic while resolving direct byte buffer");
            return -1;
        }
    };
    let Some(end) = offset.checked_add(length) else {
        throw_illegal_argument(&mut env, "buffer region overflows address space");
        return -1;
    };
    if end > capacity {
        throw_illegal_argument(&mut env, "buffer region exceeds direct buffer capacity");
        return -1;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let stream = stream_from_handle(stream_handle).ok_or(StreamReadError::Closed)?;
        let Some(region) = direct_region_address(address, offset, length)? else {
            // A zero-capacity direct buffer may report a null address. Rust
            // slices require a non-null aligned pointer even at length zero.
            return Ok(0);
        };
        // SAFETY: JNI guarantees the direct buffer address remains valid for
        // this native invocation. Bounds were checked against its capacity,
        // and the slice is neither stored nor shared after this call returns.
        let output = unsafe { std::slice::from_raw_parts_mut(region, length) };
        read_stream_once(&stream, output, timeout)
    }));
    match result {
        Ok(Ok(copied)) => jint::try_from(copied).unwrap_or(jint::MAX),
        Ok(Err(error)) => {
            throw_stream_read_error(&mut env, error);
            -1
        }
        Err(_) => {
            throw_runtime_exception(&mut env, "panic while reading native SDR stream");
            -1
        }
    }
}

/// Request cancellation without releasing the stream handle. Idempotent.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeStopStream(
    _env: JNIEnv,
    _this: JObject<'_>,
    stream_handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(stream) = stream_from_handle(stream_handle) {
            stream.request_stop();
        }
    }));
}

/// Stop and remove a native stream generation. Idempotent.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeCloseStream(
    _env: JNIEnv,
    _this: JObject<'_>,
    stream_handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| close_stream_handle(stream_handle)));
}

/// Fill `[lastDropped, lastSequence, blocksRead, bytesRead, lastBlockClips,
/// lastBlockRawSamples, totalClips, totalRawSamples]` from one coherent native
/// snapshot. The caller owns and may reuse the primitive array (`long[8]` or
/// larger).
///
/// Counter semantics differ by index and must not be conflated:
/// - Indices 0–3, 6, 7 are **monotonic** (accumulate since stream start);
///   consumers diff successive snapshots for windowed figures such as a clip
///   fraction over a poll interval.
/// - Indices 4 and 5 are **per-block**: the raw ADC-domain clip telemetry of
///   the most recently received block only, resetting with every block.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeStreamStats(
    mut env: JNIEnv,
    _this: JObject<'_>,
    stream_handle: jlong,
    output: JLongArray<'_>,
) {
    let snapshot = match catch_unwind(AssertUnwindSafe(|| {
        stream_from_handle(stream_handle).map(|stream| stream.stats())
    })) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            throw_stream_read_error(&mut env, StreamReadError::Closed);
            return;
        }
        Err(_) => {
            throw_runtime_exception(&mut env, "panic while reading stream statistics");
            return;
        }
    };
    let values = [
        jlong::try_from(snapshot.last_dropped).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.last_sequence).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.blocks_read).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.bytes_read).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.last_block_clips).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.last_block_raw_samples).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.total_clips).unwrap_or(jlong::MAX),
        jlong::try_from(snapshot.total_raw_samples).unwrap_or(jlong::MAX),
    ];
    let result = env
        .with_env(|e| -> jni::errors::Result<bool> {
            if output.len(e)? < values.len() {
                return Ok(false);
            }
            output.set_region(e, 0, &values)?;
            Ok(true)
        })
        .into_outcome();
    if !matches!(result, jni::Outcome::Ok(true)) {
        throw_illegal_argument(&mut env, "stats output must be a long[8] or larger");
    }
}

/// Close: remove the device from the registry. Idempotent — a second close (or
/// an unknown id) finds nothing and is a no-op. The `NativeDevice` is dropped
/// once the last outstanding `Arc` clone is released, so a concurrent setter
/// holding a clone remains safe.
#[no_mangle]
pub unsafe extern "system" fn Java_com_sdrfox_SdrFox_nativeClose(
    _env: JNIEnv,
    _this: JObject<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        close_handle(handle);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{
        DeviceInfo, DeviceKind, GainMode, GainRequest, GainStep, SdrError, StreamHandle,
        StreamSink, Upconverter,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc as StdArc, Condvar, Mutex as StdMutex};
    use std::thread;

    /// Calls recorded by [`StubDevice`], shared with the test through an `Arc`
    /// so assertions can inspect what reached the device after `with_device`.
    #[derive(Default)]
    struct StubCalls {
        gains: StdMutex<Vec<GainRequest>>,
        modes: StdMutex<Vec<GainMode>>,
        stage_agc: StdMutex<Vec<(GainStageId, bool)>>,
    }

    /// Minimal `SdrDevice` stub so unit tests can exercise the registry without
    /// a real USB device. Gain/rate calls are recorded or configurable; the
    /// remaining methods return `Err`/empty since no test calls them.
    struct StubDevice {
        freq: StdMutex<u64>,
        info: DeviceInfo,
        settled_rate: Option<u32>,
        rates: Vec<u32>,
        calls: StdArc<StubCalls>,
    }

    impl SdrDevice for StubDevice {
        fn info(&self) -> &DeviceInfo {
            &self.info
        }
        fn set_sample_rate(&mut self, _hz: u32) -> Result<u32, SdrError> {
            self.settled_rate
                .ok_or_else(|| SdrError::Unsupported("stub".into()))
        }
        fn supported_sample_rates(&self) -> Vec<u32> {
            self.rates.clone()
        }
        fn set_frequency(&mut self, hz: u64) -> Result<(), SdrError> {
            *self.freq.lock().unwrap() = hz;
            Ok(())
        }
        fn set_bandwidth(&mut self, _hz: u32) -> Result<(), SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
        fn set_gain(&mut self, req: GainRequest) -> Result<(), SdrError> {
            self.calls.gains.lock().unwrap().push(req);
            Ok(())
        }
        fn set_gain_mode(&mut self, mode: GainMode) -> Result<(), SdrError> {
            self.calls.modes.lock().unwrap().push(mode);
            Ok(())
        }
        fn gains(&self) -> &[GainStep] {
            &[]
        }
        fn set_bias_tee(&mut self, _on: bool) -> Result<(), SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
        fn set_agc(&mut self, _on: bool) -> Result<(), SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
        fn set_stage_agc(&mut self, stage: GainStageId, on: bool) -> Result<(), SdrError> {
            self.calls.stage_agc.lock().unwrap().push((stage, on));
            Ok(())
        }
        fn set_frequency_correction_ppm(&mut self, _ppm: f64) -> Result<(), SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
        fn set_upconverter(&mut self, _up: Option<Upconverter>) -> Result<(), SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
        fn start_stream(
            &mut self,
            _cfg: sdr_fox_core::StreamConfig,
        ) -> Result<StreamHandle, SdrError> {
            Err(SdrError::Unsupported("stub".into()))
        }
    }

    fn make_configured_stub(
        settled_rate: Option<u32>,
        rates: Vec<u32>,
    ) -> (Box<dyn SdrDevice>, StdArc<StubCalls>) {
        let calls = StdArc::new(StubCalls::default());
        let device = Box::new(StubDevice {
            freq: StdMutex::new(0),
            info: DeviceInfo {
                kind: DeviceKind::RtlSdr,
                ..DeviceInfo::default()
            },
            settled_rate,
            rates,
            calls: StdArc::clone(&calls),
        });
        (device, calls)
    }

    fn make_stub() -> Box<dyn SdrDevice> {
        make_configured_stub(None, Vec::new()).0
    }

    struct QueueSink {
        blocks: VecDeque<Result<IqBlock, SdrError>>,
        stops: StdArc<AtomicUsize>,
    }

    impl StreamSink for QueueSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            self.blocks.pop_front()
        }

        fn recv_deadline(&mut self, _deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            self.recv()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            let stops = StdArc::clone(&self.stops);
            StreamStopHandle::new(move || {
                stops.fetch_add(1, AtomicOrdering::Relaxed);
            })
        }
    }

    fn test_stream_blocks(blocks: Vec<IqBlock>) -> (NativeStream, StdArc<AtomicUsize>) {
        let stops = StdArc::new(AtomicUsize::new(0));
        let sink = QueueSink {
            blocks: blocks.into_iter().map(Ok).collect(),
            stops: StdArc::clone(&stops),
        };
        let parent = StdArc::new(NativeDevice::new(make_stub()));
        (NativeStream::new(parent, Box::new(sink)), stops)
    }

    fn test_stream(
        bytes: Vec<u8>,
        dropped: u64,
        sequence: u64,
    ) -> (NativeStream, StdArc<AtomicUsize>) {
        test_stream_blocks(vec![IqBlock {
            samples: IqSamples::Cu8(bytes),
            dropped,
            sequence,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        }])
    }

    fn telemetry_block(bytes: Vec<u8>, sequence: u64, clips: u64, raw_samples: u64) -> IqBlock {
        IqBlock {
            samples: IqSamples::Cu8(bytes),
            dropped: 0,
            sequence,
            timestamp: None,
            clips,
            raw_samples,
        }
    }

    #[derive(Default)]
    struct BlockingState {
        recv_started: bool,
        stopped: bool,
    }

    #[derive(Default)]
    struct BlockingSignals {
        state: StdMutex<BlockingState>,
        changed: Condvar,
    }

    struct BlockingSink {
        signals: StdArc<BlockingSignals>,
    }

    impl StreamSink for BlockingSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            let mut state = self.signals.state.lock().unwrap();
            state.recv_started = true;
            self.signals.changed.notify_all();
            while !state.stopped {
                state = self.signals.changed.wait(state).unwrap();
            }
            None
        }

        fn recv_deadline(&mut self, deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            let mut state = self.signals.state.lock().unwrap();
            state.recv_started = true;
            self.signals.changed.notify_all();
            while !state.stopped {
                let now = Instant::now();
                if now >= deadline {
                    return Some(Err(SdrError::Timeout));
                }
                let (next, timed) = self
                    .signals
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap();
                state = next;
                if timed.timed_out() && !state.stopped {
                    return Some(Err(SdrError::Timeout));
                }
            }
            None
        }

        fn stop_handle(&self) -> StreamStopHandle {
            let signals = StdArc::clone(&self.signals);
            StreamStopHandle::new(move || {
                let mut state = signals.state.lock().unwrap();
                state.stopped = true;
                signals.changed.notify_all();
            })
        }
    }

    fn blocking_stream() -> (StdArc<NativeStream>, StdArc<BlockingSignals>) {
        let signals = StdArc::new(BlockingSignals::default());
        let sink = BlockingSink {
            signals: StdArc::clone(&signals),
        };
        let parent = StdArc::new(NativeDevice::new(make_stub()));
        (
            StdArc::new(NativeStream::new(parent, Box::new(sink))),
            signals,
        )
    }

    #[test]
    fn handle_zero_returns_none() {
        assert!(from_handle(0).is_none());
    }

    #[cfg(not(feature = "android"))]
    #[test]
    fn open_fd_transport_errors_without_android_feature() {
        assert!(open_fd_transport(3, DeviceKind::RtlSdr).is_err());
    }

    #[cfg(feature = "android")]
    #[test]
    fn open_fd_transport_rejects_invalid_fd_with_android_feature() {
        assert!(open_fd_transport(-1, DeviceKind::RtlSdr).is_err());
    }

    #[test]
    fn explicit_airspy_mini_kind_supplies_reliable_model_metadata() {
        let (kind, product) = decode_backend_kind(2).unwrap();
        assert!(matches!(kind, DeviceKind::Airspy));
        assert_eq!(product.as_deref(), Some("Airspy Mini"));
        assert!(decode_backend_kind(99).is_err());
    }

    #[test]
    fn android_usb_identity_preserves_blog_v4_board_metadata_without_a_serial() {
        let desc = descriptor_from_usb_identity(
            0,
            0x0bda,
            0x2838,
            Some("RTLSDRBlog".into()),
            Some("Blog V4".into()),
        )
        .unwrap();
        assert_eq!((desc.vendor_id, desc.product_id), (0x0bda, 0x2838));
        assert_eq!(desc.vendor_name.as_deref(), Some("RTLSDRBlog"));
        assert_eq!(desc.product_name.as_deref(), Some("Blog V4"));
        assert_eq!(desc.kind, DeviceKind::RtlSdr);
        assert_eq!(desc.serial, None);
    }

    #[test]
    fn android_usb_identity_never_invents_or_normalizes_v4_identity() {
        let absent = descriptor_from_usb_identity(0, 0x0bda, 0x2832, None, None).unwrap();
        assert_eq!(absent.product_id, 0x2832);
        assert_eq!(absent.vendor_name, None);
        assert_eq!(absent.product_name, None);
        let edited = descriptor_from_usb_identity(
            0,
            0x0bda,
            0x2838,
            Some(" RTLSDRBlog".into()),
            Some("Blog V4 ".into()),
        )
        .unwrap();
        assert_eq!(edited.vendor_name.as_deref(), Some(" RTLSDRBlog"));
        assert_eq!(edited.product_name.as_deref(), Some("Blog V4 "));
        let mini = descriptor_from_usb_identity(2, 0x1d50, 0x60a1, None, None).unwrap();
        assert_eq!(mini.kind, DeviceKind::Airspy);
        assert_eq!(mini.product_name.as_deref(), Some("Airspy Mini"));
    }

    #[test]
    fn android_usb_identity_rejects_out_of_range_ids_and_unknown_kinds() {
        for invalid in [-1, 65_536, i32::MAX] {
            assert!(descriptor_from_usb_identity(0, invalid, 0x2838, None, None).is_err());
            assert!(descriptor_from_usb_identity(0, 0x0bda, invalid, None, None).is_err());
        }
        assert!(descriptor_from_usb_identity(99, 0x0bda, 0x2838, None, None).is_err());
    }

    #[test]
    fn double_close_is_safe() {
        let h = box_handle(make_stub());
        assert_ne!(h, 0);
        // First close removes the entry; second close is a no-op.
        close_handle(h);
        close_handle(h);
        // from_handle now returns None.
        assert!(from_handle(h).is_none());
    }

    #[test]
    fn unknown_handle_returns_none() {
        assert!(from_handle(-1).is_none());
        assert!(from_handle(jlong::MAX).is_none());
    }

    #[test]
    fn closed_generation_is_never_reused() {
        let first = box_handle(make_stub());
        close_handle(first);
        let second = box_handle(make_stub());
        assert!(second > first);
        assert!(from_handle(first).is_none());
        assert!(from_handle(second).is_some());
        close_handle(second);
    }

    #[test]
    fn concurrent_access_does_not_ub() {
        let h = box_handle(make_stub());
        // A concurrent close must not race with active setters into UB: each
        // setter clones the Arc, so the device outlives the registry entry.
        let arc1 = from_handle(h).expect("live handle");
        let arc2 = StdArc::clone(&arc1);
        let t = thread::spawn(move || {
            // Simulate a setter on a cloned Arc.
            let dev = arc2;
            let _ = dev.device.lock().unwrap().set_frequency(42);
        });
        // Close concurrently while the other thread holds a clone.
        close_handle(h);
        t.join().unwrap();
        // The clone is still valid (dropped here) even though the registry
        // entry was removed.
        drop(arc1);
    }

    #[test]
    fn pull_read_retains_block_remainder_and_updates_coherent_stats() {
        let (stream, _stops) = test_stream(vec![1, 2, 3, 4, 5], 7, 11);
        let mut first = [0_u8; 3];
        assert_eq!(read_stream_once(&stream, &mut first, None).unwrap(), 3);
        assert_eq!(first, [1, 2, 3]);
        assert_eq!(
            stream.stats(),
            NativeStreamStats {
                last_dropped: 7,
                last_sequence: 11,
                blocks_read: 1,
                bytes_read: 3,
                last_block_clips: 0,
                last_block_raw_samples: 0,
                total_clips: 0,
                total_raw_samples: 0,
            }
        );

        let mut second = [0_u8; 8];
        assert_eq!(read_stream_once(&stream, &mut second, None).unwrap(), 2);
        assert_eq!(&second[..2], &[4, 5]);
        assert_eq!(stream.stats().blocks_read, 1);
        assert_eq!(stream.stats().bytes_read, 5);
    }

    #[test]
    fn stats_expose_per_block_clips_and_monotonic_totals() {
        let (stream, _stops) = test_stream_blocks(vec![
            telemetry_block(vec![1, 2], 0, 5, 1000),
            telemetry_block(vec![3, 4], 1, 2, 800),
        ]);
        let mut buf = [0_u8; 2];
        assert_eq!(read_stream_once(&stream, &mut buf, None).unwrap(), 2);
        let first = stream.stats();
        assert_eq!(first.last_block_clips, 5);
        assert_eq!(first.last_block_raw_samples, 1000);
        assert_eq!(first.total_clips, 5);
        assert_eq!(first.total_raw_samples, 1000);

        assert_eq!(read_stream_once(&stream, &mut buf, None).unwrap(), 2);
        let second = stream.stats();
        // Per-block: replaced by the newest block's values, never summed.
        assert_eq!(second.last_block_clips, 2);
        assert_eq!(second.last_block_raw_samples, 800);
        // Monotonic: totals accumulate across blocks for windowed diffing.
        assert_eq!(second.total_clips, 7);
        assert_eq!(second.total_raw_samples, 1800);
    }

    #[test]
    fn set_sample_rate_plumbing_returns_the_settled_rate() {
        let (device, _calls) = make_configured_stub(Some(2_400_000), Vec::new());
        let handle = box_handle(device);
        let settled = with_device(handle, |dev| dev.set_sample_rate(2_500_000));
        assert!(matches!(settled, Ok(2_400_000)));
        close_handle(handle);
    }

    #[test]
    fn sample_rate_enumeration_reaches_the_device_and_converts_to_jint() {
        let (device, _calls) = make_configured_stub(None, vec![6_000_000, 3_000_000]);
        let handle = box_handle(device);
        let rates = with_device(handle, |dev| Ok(dev.supported_sample_rates()));
        assert!(matches!(rates, Ok(ref r) if r == &[6_000_000, 3_000_000]));
        close_handle(handle);

        assert_eq!(
            sample_rates_to_jint(&[6_000_000, u32::MAX]),
            vec![6_000_000, jint::MAX]
        );
        assert!(sample_rates_to_jint(&[]).is_empty());
    }

    #[test]
    fn gain_stage_codes_follow_the_core_contract() {
        // The JNI decode must be the core mapping, not a parallel one: every
        // typed stage round-trips through its own `code()`.
        for stage in [GainStageId::Lna, GainStageId::Mixer, GainStageId::Vga] {
            assert_eq!(decode_gain_stage(stage.code()).unwrap(), stage);
        }
        assert!(decode_gain_stage(3).is_err());
        assert!(decode_gain_stage(-1).is_err());
    }

    #[test]
    fn per_stage_gain_gain_mode_and_stage_agc_reach_the_device() {
        let (device, calls) = make_configured_stub(None, Vec::new());
        let handle = box_handle(device);

        assert!(with_device(handle, |dev| {
            dev.set_gain(GainRequest::per_stage(GainStageId::Vga, 105))
        })
        .is_ok());
        assert!(with_device(handle, |dev| dev.set_gain_mode(GainMode::Auto)).is_ok());
        assert!(with_device(handle, |dev| dev.set_stage_agc(GainStageId::Mixer, true)).is_ok());
        close_handle(handle);

        assert_eq!(
            *calls.gains.lock().unwrap(),
            vec![GainRequest::per_stage(GainStageId::Vga, 105)]
        );
        assert_eq!(*calls.modes.lock().unwrap(), vec![GainMode::Auto]);
        assert_eq!(
            *calls.stage_agc.lock().unwrap(),
            vec![(GainStageId::Mixer, true)]
        );
    }

    #[test]
    fn pull_read_rejects_an_empty_success_block() {
        let (stream, stops) = test_stream(Vec::new(), 0, 0);
        let error = read_stream_once(&stream, &mut [0_u8; 1], None)
            .expect_err("empty successful block must not look like a timeout");
        assert!(matches!(error, StreamReadError::Failed(_)));
        assert!(stream.stopped.load(Ordering::Acquire));
        assert_eq!(stops.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn concurrent_pull_fails_busy_instead_of_waiting() {
        let (stream, _stops) = test_stream(vec![1], 0, 0);
        let _guard = stream.state.lock().unwrap();
        let error = read_stream_once(&stream, &mut [0_u8; 1], None)
            .expect_err("second reader must not wait on the receive mutex");
        assert!(matches!(error, StreamReadError::Busy));
    }

    #[test]
    fn stream_registry_is_monotonic_and_close_is_idempotent() {
        let (first, first_stops) = test_stream(vec![1], 0, 0);
        let first_handle = insert_stream(first);
        let retained = stream_from_handle(first_handle).expect("registered stream");
        close_stream_handle(first_handle);
        close_stream_handle(first_handle);
        assert!(stream_from_handle(first_handle).is_none());
        assert_eq!(first_stops.load(AtomicOrdering::Relaxed), 1);
        drop(retained);

        let (second, _second_stops) = test_stream(vec![2], 0, 1);
        let second_handle = insert_stream(second);
        assert!(second_handle > first_handle);
        close_stream_handle(second_handle);
    }

    #[test]
    fn finite_timeout_is_nonterminal() {
        let (stream, _signals) = blocking_stream();
        assert_eq!(
            read_stream_once(&stream, &mut [0_u8; 1], Some(Duration::from_millis(1))).unwrap(),
            0
        );
        assert!(!stream.stopped.load(Ordering::Acquire));
        assert!(stream.state.try_lock().is_ok());
        assert_eq!(
            read_stream_once(&stream, &mut [0_u8; 1], Some(Duration::from_millis(1))).unwrap(),
            0
        );
        stream.request_stop();
    }

    #[test]
    fn stop_wakes_a_blocked_pull() {
        let (stream, signals) = blocking_stream();
        let reader = StdArc::clone(&stream);
        let thread =
            thread::spawn(move || read_stream_once(&reader, &mut [0_u8; 1], None).unwrap());
        let state = signals.state.lock().unwrap();
        let (state, timed) = signals
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.recv_started)
            .unwrap();
        assert!(!timed.timed_out());
        drop(state);
        stream.request_stop();
        assert_eq!(thread.join().unwrap(), 0);
    }

    #[test]
    fn close_wakes_a_blocked_pull_and_removes_generation() {
        let signals = StdArc::new(BlockingSignals::default());
        let sink = BlockingSink {
            signals: StdArc::clone(&signals),
        };
        let parent = StdArc::new(NativeDevice::new(make_stub()));
        let handle = insert_stream(NativeStream::new(parent, Box::new(sink)));
        let reader = stream_from_handle(handle).expect("registered stream");
        let thread =
            thread::spawn(move || read_stream_once(&reader, &mut [0_u8; 1], None).unwrap());
        let state = signals.state.lock().unwrap();
        let (state, timed) = signals
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.recv_started)
            .unwrap();
        assert!(!timed.timed_out());
        drop(state);
        close_stream_handle(handle);
        assert!(stream_from_handle(handle).is_none());
        assert_eq!(thread.join().unwrap(), 0);
    }

    #[test]
    fn zero_length_direct_region_never_offsets_a_null_pointer() {
        assert!(direct_region_address(std::ptr::null_mut(), usize::MAX, 0)
            .unwrap()
            .is_none());
        assert!(direct_region_address(std::ptr::null_mut(), 0, 1).is_err());
    }
}
