//! PyO3 binding implementation (separated from lib.rs to avoid formatter races).

use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes};

use sdr_fox_airspy::AirspyBackend;
use sdr_fox_core::sample::StreamStopHandle;
use sdr_fox_core::{
    DeviceDescriptor, DeviceKind, GainRequest, IqBlock, IqFormat, SdrBackend, SdrDevice,
    StreamConfig, StreamHandle, Upconverter,
};
use sdr_fox_rtlsdr::RtlSdrBackend;
use sdr_fox_transport::UsbDeviceLocation;

/// Open an SDR device by index.
///
/// The device lives behind a `Mutex`, and an optional persistent stream lives
/// behind a second `Mutex`. `read()` lazily starts (and reuses) the stream so
/// repeated reads do not pay the start/stop cost; `close()` drops it.
#[pyclass]
pub struct SdrFox {
    device: Mutex<Box<dyn SdrDevice>>,
    stream: PythonStreamSlot,
}

#[derive(Default)]
struct PythonStreamState {
    handle: Option<StreamHandle>,
    stop: Option<StreamStopHandle>,
    format: Option<IqFormat>,
    pending: Option<IqBlock>,
    pending_offset: usize,
    generation: u64,
    stats: StreamStatsPy,
}

impl PythonStreamState {
    fn advance_generation(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("Python stream generation exhausted");
        self.generation
    }

    fn drain_pending_into(&mut self, output: &mut Vec<u8>, count: usize) {
        let remaining = count.saturating_sub(output.len());
        let Some(block) = self.pending.as_ref() else {
            return;
        };
        let bytes = samples_as_bytes(&block.samples);
        let available = bytes.len().saturating_sub(self.pending_offset);
        let take = available.min(remaining);
        output.extend_from_slice(&bytes[self.pending_offset..self.pending_offset + take]);
        self.pending_offset += take;
        if self.pending_offset == bytes.len() {
            self.pending = None;
            self.pending_offset = 0;
        }
    }
}

/// Interior synchronization for the persistent Python stream.
///
/// `read_gate` deliberately serializes readers without ever delaying `close`.
/// Readers use `try_lock`: waiting for another GIL-detached reader while
/// holding the GIL would prevent that reader from returning to Python.
#[derive(Default)]
struct PythonStreamSlot {
    state: Mutex<PythonStreamState>,
    read_gate: Mutex<()>,
}

struct RetiredStream {
    handle: Option<StreamHandle>,
    stop: Option<StreamStopHandle>,
}

enum PrepareStream {
    Reuse,
    Start {
        generation: u64,
        retired: RetiredStream,
    },
}

struct CheckedOutStream {
    handle: StreamHandle,
    stop: StreamStopHandle,
    generation: u64,
    format: IqFormat,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReadSummary {
    last_dropped: u64,
    last_sequence: u64,
    blocks_read: u64,
    bytes_read: u64,
}

impl ReadSummary {
    fn record_block(&mut self, block: &sdr_fox_core::IqBlock) {
        self.last_dropped = block.dropped;
        self.last_sequence = block.sequence;
        self.blocks_read = self.blocks_read.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadGateError {
    Busy,
    Poisoned,
}

impl ReadGateError {
    const fn message(self) -> &'static str {
        match self {
            Self::Busy => "another read is already in progress",
            Self::Poisoned => "stream read mutex poisoned",
        }
    }
}

impl PythonStreamSlot {
    fn lock_state(&self) -> PyResult<MutexGuard<'_, PythonStreamState>> {
        self.state
            .lock()
            .map_err(|_| PyRuntimeError::new_err("stream mutex poisoned"))
    }

    fn try_lock_reader(&self) -> Result<MutexGuard<'_, ()>, ReadGateError> {
        match self.read_gate.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::WouldBlock) => Err(ReadGateError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(ReadGateError::Poisoned),
        }
    }

    fn invalidate_locked(state: &mut PythonStreamState) -> (u64, RetiredStream) {
        let generation = state.advance_generation();
        state.format = None;
        state.pending = None;
        state.pending_offset = 0;
        let retired = RetiredStream {
            handle: state.handle.take(),
            stop: state.stop.take(),
        };
        (generation, retired)
    }

    fn prepare(&self, format: IqFormat) -> PyResult<PrepareStream> {
        let mut state = self.lock_state()?;
        if state.format == Some(format) && state.handle.is_some() && state.stop.is_some() {
            return Ok(PrepareStream::Reuse);
        }
        let (generation, retired) = Self::invalidate_locked(&mut state);
        Ok(PrepareStream::Start {
            generation,
            retired,
        })
    }

    /// Install a newly started stream only if the reservation is still current.
    ///
    /// The independent stop capability is obtained before the stream can ever
    /// leave this slot for a blocking receive.
    fn install(
        &self,
        generation: u64,
        format: IqFormat,
        handle: StreamHandle,
    ) -> PyResult<Result<(), RetiredStream>> {
        let stop = handle.stop_handle();
        let mut state = self.lock_state()?;
        if state.generation != generation
            || state.format.is_some()
            || state.handle.is_some()
            || state.stop.is_some()
        {
            return Ok(Err(RetiredStream {
                handle: Some(handle),
                stop: Some(stop),
            }));
        }
        state.handle = Some(handle);
        state.stop = Some(stop);
        state.format = Some(format);
        state.stats = StreamStatsPy::default();
        Ok(Ok(()))
    }

    fn checkout(&self, format: IqFormat) -> PyResult<CheckedOutStream> {
        let mut state = self.lock_state()?;
        if state.format != Some(format) {
            return Err(PyRuntimeError::new_err("stream was closed or replaced"));
        }
        let stop = state
            .stop
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("stream has no stop capability"))?;
        let handle = state
            .handle
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("stream is not available"))?;
        Ok(CheckedOutStream {
            handle,
            stop,
            generation: state.generation,
            format,
        })
    }

    /// Return a handle after a successful receive, unless an overlapping close
    /// or replacement invalidated its generation.
    fn restore(&self, checked: CheckedOutStream) -> PyResult<Result<u64, RetiredStream>> {
        self.restore_after_read(checked, None, ReadSummary::default())
    }

    fn restore_after_read(
        &self,
        checked: CheckedOutStream,
        pending: Option<(IqBlock, usize)>,
        summary: ReadSummary,
    ) -> PyResult<Result<u64, RetiredStream>> {
        let mut state = self.lock_state()?;
        if state.generation != checked.generation
            || state.format != Some(checked.format)
            || state.stop.is_none()
            || state.handle.is_some()
        {
            return Ok(Err(RetiredStream {
                handle: Some(checked.handle),
                stop: Some(checked.stop),
            }));
        }
        state.handle = Some(checked.handle);
        if let Some((block, offset)) = pending {
            state.pending = Some(block);
            state.pending_offset = offset;
        }
        apply_read_summary(&mut state.stats, summary);
        Ok(Ok(checked.generation))
    }

    /// Retire a handle whose receive ended or failed. The boolean reports
    /// whether this operation, rather than an overlapping close/restart, still
    /// owned the current generation.
    fn retire_checked(&self, checked: CheckedOutStream) -> PyResult<(RetiredStream, bool)> {
        self.retire_after_read(checked, ReadSummary::default())
    }

    fn retire_after_read(
        &self,
        checked: CheckedOutStream,
        summary: ReadSummary,
    ) -> PyResult<(RetiredStream, bool)> {
        let mut state = self.lock_state()?;
        let current = state.generation == checked.generation
            && state.format == Some(checked.format)
            && state.handle.is_none();
        let stored_stop = if current {
            apply_read_summary(&mut state.stats, summary);
            let (_, retired) = Self::invalidate_locked(&mut state);
            retired.stop
        } else {
            None
        };
        Ok((
            RetiredStream {
                handle: Some(checked.handle),
                stop: stored_stop.or(Some(checked.stop)),
            },
            current,
        ))
    }

    fn close(&self) -> PyResult<RetiredStream> {
        let mut state = self.lock_state()?;
        let (_, retired) = Self::invalidate_locked(&mut state);
        Ok(retired)
    }

    fn drain_pending_into(&self, output: &mut Vec<u8>, count: usize) -> PyResult<()> {
        self.lock_state()?.drain_pending_into(output, count);
        Ok(())
    }

    fn has_pending(&self) -> PyResult<bool> {
        Ok(self.lock_state()?.pending.is_some())
    }

    fn record_returned_bytes(&self, bytes: usize) -> PyResult<()> {
        let mut state = self.lock_state()?;
        state.stats.bytes_read = state.stats.bytes_read.saturating_add(bytes as u64);
        Ok(())
    }

    fn stats(&self) -> PyResult<StreamStatsPy> {
        Ok(self.lock_state()?.stats)
    }
}

fn apply_read_summary(stats: &mut StreamStatsPy, summary: ReadSummary) {
    if summary.blocks_read != 0 {
        stats.last_dropped = summary.last_dropped;
        stats.last_sequence = summary.last_sequence;
        stats.blocks_read = stats.blocks_read.saturating_add(summary.blocks_read);
    }
    stats.bytes_read = stats.bytes_read.saturating_add(summary.bytes_read);
}

fn shutdown_retired(retired: RetiredStream, py: Python<'_>) {
    // A stop callback may itself issue a bounded USB control transfer (Airspy),
    // so neither signalling nor destruction may hold the GIL.
    let RetiredStream { handle, stop } = retired;
    py.detach(move || {
        if let Some(stop) = &stop {
            stop.stop();
        }
        drop((handle, stop));
    });
}

impl SdrFox {
    /// Lock the inner device (returns a `PyErr` on poison).
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Box<dyn SdrDevice>>> {
        self.device
            .lock()
            .map_err(|_| PyRuntimeError::new_err("device mutex poisoned (a prior call panicked)"))
    }

    fn detached_device_call<R: Send>(
        &self,
        py: Python<'_>,
        body: impl FnOnce(&mut dyn SdrDevice) -> Result<R, sdr_fox_core::SdrError> + Send,
    ) -> PyResult<R> {
        py.detach(|| -> Result<R, String> {
            let mut device = self
                .device
                .lock()
                .map_err(|_| "device mutex poisoned".to_owned())?;
            body(device.as_mut()).map_err(|error| error.to_string())
        })
        .map_err(PyRuntimeError::new_err)
    }

    /// Ensure a persistent stream of `fmt` is running. Potentially blocking
    /// stream destruction and startup both run with the GIL detached.
    fn ensure_stream(&self, fmt: IqFormat, py: Python<'_>) -> PyResult<()> {
        let (generation, retired) = match self.stream.prepare(fmt)? {
            PrepareStream::Reuse => return Ok(()),
            PrepareStream::Start {
                generation,
                retired,
            } => (generation, retired),
        };
        shutdown_retired(retired, py);

        let cfg = StreamConfig {
            format: fmt,
            ..StreamConfig::default()
        };
        let started = py.detach(|| -> Result<StreamHandle, String> {
            let mut device = self
                .device
                .lock()
                .map_err(|_| "device mutex poisoned".to_owned())?;
            device.start_stream(cfg).map_err(|error| error.to_string())
        });
        let handle = started.map_err(PyRuntimeError::new_err)?;
        match self.stream.install(generation, fmt, handle)? {
            Ok(()) => Ok(()),
            Err(stale) => {
                shutdown_retired(stale, py);
                Err(PyRuntimeError::new_err(
                    "stream was closed while it was starting",
                ))
            }
        }
    }
}

/// Coherent snapshot of the active Python stream adapter.
#[pyclass(skip_from_py_object)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamStatsPy {
    last_dropped: u64,
    last_sequence: u64,
    blocks_read: u64,
    bytes_read: u64,
}

#[pymethods]
impl StreamStatsPy {
    /// Producer-reported cumulative dropped-sample count on the last block.
    #[getter]
    fn last_dropped(&self) -> u64 {
        self.last_dropped
    }

    /// Producer sequence number of the last block returned to Python.
    #[getter]
    fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Number of complete blocks accepted from the core stream.
    #[getter]
    fn blocks_read(&self) -> u64 {
        self.blocks_read
    }

    /// Number of payload bytes returned to Python callers.
    #[getter]
    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

/// One block of IQ samples from a stream.
#[pyclass]
pub struct IqBlockPy {
    samples: Py<PyAny>,
    dropped: u64,
    sequence: u64,
}

#[pymethods]
impl IqBlockPy {
    #[getter]
    fn samples(&self) -> &Py<PyAny> {
        &self.samples
    }
    #[getter]
    fn dropped(&self) -> u64 {
        self.dropped
    }
    #[getter]
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

const fn is_rtl_id(vendor_id: u16, product_id: u16) -> bool {
    matches!(
        (vendor_id, product_id),
        (0x0bda, 0x2832 | 0x2838) | (0x1d50, 0x6089 | 0xcc60)
    )
}

fn select_family_location(
    locations: &[UsbDeviceLocation],
    index: usize,
    kind: DeviceKind,
) -> Option<UsbDeviceLocation> {
    locations
        .iter()
        .filter(|location| match kind {
            DeviceKind::RtlSdr => is_rtl_id(location.vendor_id, location.product_id),
            DeviceKind::Airspy => (location.vendor_id, location.product_id) == (0x1d50, 0x60a1),
            DeviceKind::Unknown => false,
        })
        .nth(index)
        .copied()
}

fn parse_device_kind(kind: Option<&str>) -> PyResult<DeviceKind> {
    match kind {
        None | Some("rtl-sdr") => Ok(DeviceKind::RtlSdr),
        Some("airspy") => Ok(DeviceKind::Airspy),
        Some(other) => Err(PyRuntimeError::new_err(format!(
            "unknown device kind {other:?}; expected 'rtl-sdr' or 'airspy'"
        ))),
    }
}

#[pymethods]
impl SdrFox {
    /// Open the `index`-th detected device. `kind` is "rtl-sdr" or "airspy".
    #[staticmethod]
    #[pyo3(signature = (index, kind=None))]
    pub fn open(index: usize, kind: Option<&str>, py: Python<'_>) -> PyResult<Self> {
        let backend_kind = parse_device_kind(kind)?;
        let device = py
            .detach(move || -> Result<Box<dyn SdrDevice>, String> {
                let locations = sdr_fox_transport::enumerate_usb_devices()
                    .map_err(|error| error.to_string())?;
                let selected =
                    select_family_location(&locations, index, backend_kind).ok_or_else(|| {
                        format!("no matching {backend_kind:?} device at index {index}")
                    })?;
                let desc = DeviceDescriptor {
                    vendor_id: selected.vendor_id,
                    product_id: selected.product_id,
                    vendor_name: None,
                    product_name: None,
                    serial: None,
                    index: selected.match_index,
                    kind: backend_kind,
                };
                let transport = sdr_fox_transport::open_default(
                    selected.vendor_id,
                    selected.product_id,
                    selected.match_index,
                )
                .map_err(|error| error.to_string())?;
                let backend: Box<dyn SdrBackend> = match backend_kind {
                    DeviceKind::Airspy => Box::new(AirspyBackend),
                    _ => Box::new(RtlSdrBackend),
                };
                backend
                    .open(&desc, transport)
                    .map_err(|error| error.to_string())
            })
            .map_err(PyRuntimeError::new_err)?;
        Ok(Self {
            device: Mutex::new(device),
            stream: PythonStreamSlot::default(),
        })
    }

    /// Set the center frequency in Hz.
    #[setter]
    pub fn set_frequency(&self, py: Python<'_>, hz: u64) -> PyResult<()> {
        self.detached_device_call(py, |device| device.set_frequency(hz))
    }

    /// Set the sample rate in Hz.
    #[setter]
    pub fn set_sample_rate(&self, py: Python<'_>, hz: u32) -> PyResult<()> {
        self.detached_device_call(py, |device| device.set_sample_rate(hz).map(|_| ()))
    }

    /// Enable/disable the bias tee.
    #[setter]
    pub fn set_bias_tee(&self, py: Python<'_>, on: bool) -> PyResult<()> {
        self.detached_device_call(py, |device| device.set_bias_tee(on))
    }

    /// Enable/disable AGC.
    #[setter]
    pub fn set_agc(&self, py: Python<'_>, on: bool) -> PyResult<()> {
        self.detached_device_call(py, |device| device.set_agc(on))
    }

    /// Set the overall gain in tenths of dB.
    #[setter]
    pub fn set_gain(&self, py: Python<'_>, tenths_db: i32) -> PyResult<()> {
        self.detached_device_call(py, |device| {
            device.set_gain(GainRequest::overall(tenths_db))
        })
    }

    /// Enable the SpyVerter (120 MHz HF upconverter).
    pub fn enable_spyverter(&self, py: Python<'_>) -> PyResult<()> {
        self.detached_device_call(py, |device| {
            device.set_upconverter(Some(Upconverter::spyverter()))
        })
    }

    /// Disable any upconverter.
    pub fn disable_upconverter(&self, py: Python<'_>) -> PyResult<()> {
        self.detached_device_call(py, |device| device.set_upconverter(None))
    }

    /// Read exactly `count` bytes of IQ samples unless the stream ends first.
    ///
    /// The blocking receive runs with the GIL released (`Python::detach`) so
    /// other Python threads can run during I/O. A persistent stream is kept on
    /// the object across calls; pass a different `format` to restart it.
    /// Surplus bytes from the final device block are retained for the next call,
    /// so the requested count is not rounded up to a transport block. Returns a
    /// contiguous buffer-protocol `bytes` object: Cu8/Cs8 as bytes, Cs16 as
    /// native-endian i16, and Cf32 as native-endian f32.
    #[pyo3(signature = (count=65536, format="cu8"))]
    pub fn read(&self, count: usize, format: &str, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let fmt = parse_format(format)?;
        if count == 0 {
            return Ok(PyBytes::new(py, &[]).into());
        }
        // A shared PyO3 borrow allows `close()` from another Python thread.
        // The non-blocking interior gate prevents two readers from racing over
        // the one persistent receiver without ever waiting while holding the
        // GIL (which would deadlock the detached reader returning from recv).
        let _reader = self
            .stream
            .try_lock_reader()
            .map_err(|error| PyRuntimeError::new_err(error.message()))?;
        self.ensure_stream(fmt, py)?;
        let mut acc = Vec::with_capacity(count);
        self.stream.drain_pending_into(&mut acc, count)?;
        if acc.len() == count {
            self.stream.record_returned_bytes(acc.len())?;
            return Ok(PyBytes::new(py, &acc).into());
        }

        // Check out once and keep the GIL detached for the whole accumulation.
        // The retained stop handle plus generation check still let close()
        // cancel a blocked receive without resurrecting a stale stream.
        let checked = self.stream.checkout(fmt)?;
        let result = py.detach(move || receive_bytes(checked, acc, count));
        match result.end {
            ByteReadEnd::TargetReached => {
                match self.stream.restore_after_read(
                    result.checked,
                    result.spill,
                    result.summary,
                )? {
                    Ok(_) => {}
                    Err(stale) => {
                        shutdown_retired(stale, py);
                        return Err(PyRuntimeError::new_err(
                            "stream was closed or replaced during read",
                        ));
                    }
                }
            }
            ByteReadEnd::StreamEnded => {
                let (retired, was_current) = self
                    .stream
                    .retire_after_read(result.checked, result.summary)?;
                shutdown_retired(retired, py);
                if !was_current {
                    return Err(PyRuntimeError::new_err(
                        "stream was closed or replaced during read",
                    ));
                }
                if result.output.is_empty() {
                    return Err(PyRuntimeError::new_err("stream ended with no data"));
                }
            }
            ByteReadEnd::Error(error) => {
                let (retired, was_current) = self
                    .stream
                    .retire_after_read(result.checked, result.summary)?;
                shutdown_retired(retired, py);
                if !was_current {
                    return Err(PyRuntimeError::new_err(
                        "stream was closed or replaced during read",
                    ));
                }
                return Err(py_err(error));
            }
        }

        let acc = result.output;
        Ok(PyBytes::new(py, &acc).into())
    }

    /// Read one complete IQ block with loss metadata.
    ///
    /// A finite timeout returns `None` without stopping or replacing the
    /// stream. Mixing this method with a byte read that still has an unread
    /// suffix is rejected so block ordering and metadata remain unambiguous.
    #[pyo3(signature = (format="cu8", timeout_ms=None))]
    pub fn read_block(
        &self,
        format: &str,
        timeout_ms: Option<u64>,
        py: Python<'_>,
    ) -> PyResult<Option<Py<IqBlockPy>>> {
        let fmt = parse_format(format)?;
        let _reader = self
            .stream
            .try_lock_reader()
            .map_err(|error| PyRuntimeError::new_err(error.message()))?;
        self.ensure_stream(fmt, py)?;
        if self.stream.has_pending()? {
            return Err(PyRuntimeError::new_err(
                "cannot read a structured block while a byte-read remainder is pending",
            ));
        }

        let checked = self.stream.checkout(fmt)?;
        let deadline = timeout_ms
            .map(Duration::from_millis)
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .ok_or_else(|| PyRuntimeError::new_err("timeout is too large"))
            })
            .transpose()?;
        let (checked, received) = py.detach(move || {
            let mut checked = checked;
            let received = match deadline {
                Some(end) => checked.handle.recv_deadline(end),
                None => checked.handle.recv(),
            };
            (checked, received)
        });

        match received {
            Some(Ok(block)) => {
                let mut summary = ReadSummary::default();
                summary.record_block(&block);
                summary.bytes_read = samples_as_bytes(&block.samples).len() as u64;
                match self.stream.restore_after_read(checked, None, summary)? {
                    Ok(_) => {}
                    Err(stale) => {
                        shutdown_retired(stale, py);
                        return Err(PyRuntimeError::new_err(
                            "stream was closed or replaced during read",
                        ));
                    }
                }
                let samples: Py<PyAny> = PyBytes::new(py, samples_as_bytes(&block.samples)).into();
                Ok(Some(Py::new(
                    py,
                    IqBlockPy {
                        samples,
                        dropped: block.dropped,
                        sequence: block.sequence,
                    },
                )?))
            }
            Some(Err(sdr_fox_core::SdrError::Timeout)) => match self.stream.restore(checked)? {
                Ok(_) => Ok(None),
                Err(stale) => {
                    shutdown_retired(stale, py);
                    Err(PyRuntimeError::new_err(
                        "stream was closed or replaced during read",
                    ))
                }
            },
            Some(Err(error)) => {
                let (retired, was_current) = self.stream.retire_checked(checked)?;
                shutdown_retired(retired, py);
                if was_current {
                    Err(py_err(error))
                } else {
                    Err(PyRuntimeError::new_err(
                        "stream was closed or replaced during read",
                    ))
                }
            }
            None => {
                let (retired, was_current) = self.stream.retire_checked(checked)?;
                shutdown_retired(retired, py);
                if was_current {
                    Ok(None)
                } else {
                    Err(PyRuntimeError::new_err(
                        "stream was closed or replaced during read",
                    ))
                }
            }
        }
    }

    /// Return a coherent snapshot of stream loss and copy progress.
    pub fn stream_stats(&self) -> PyResult<StreamStatsPy> {
        self.stream.stats()
    }

    /// Stop and drop the persistent stream, if any. Safe to call when no
    /// stream is running (no-op). Subsequent `read()` calls start a fresh one.
    pub fn close(&self, py: Python<'_>) -> PyResult<()> {
        // Invalidate first, then signal outside the mutex. If `read()` owns the
        // StreamHandle inside recv, the retained stop capability wakes it and
        // its stale generation prevents reinsertion.
        let retired = self.stream.close()?;
        shutdown_retired(retired, py);
        Ok(())
    }

    /// Context-manager entry.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Context-manager exit: drop the persistent stream.
    fn __exit__(
        &self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _tb: &Bound<'_, PyAny>,
        py: Python<'_>,
    ) -> PyResult<()> {
        self.close(py)
    }

    /// Supported gains as (name, `tenths_db`) tuples.
    pub fn gains(&self) -> PyResult<Vec<(String, i32)>> {
        let guard = self.lock()?;
        Ok(guard
            .gains()
            .iter()
            .map(|g| (g.name.to_string(), g.tenths_db))
            .collect())
    }
}

/// Convert cu8 bytes to cf32 floats using the SIMD path.
#[pyfunction]
pub fn convert_cu8_to_cf32(py: Python<'_>, input: &[u8]) -> PyResult<Py<PyAny>> {
    let byte_len = input
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| PyRuntimeError::new_err("converted buffer is too large"))?;
    let bytes = PyBytes::new_with(py, byte_len, |destination| {
        // Python bytes storage is normally sufficiently aligned, but the API
        // promises only bytes. Use the allocation-free direct path when it is
        // aligned and retain a safe fallback for alternative interpreters.
        let (head, floats, tail) = unsafe { destination.align_to_mut::<f32>() };
        if head.is_empty() && tail.is_empty() && floats.len() == input.len() {
            py.detach(|| sdr_fox_simd::cu8_to_cf32(input, floats));
        } else {
            let converted = py.detach(|| {
                let mut converted = vec![0.0_f32; input.len()];
                sdr_fox_simd::cu8_to_cf32(input, &mut converted);
                converted
            });
            let converted_bytes = unsafe {
                // SAFETY: the vector is contiguous and remains alive for the
                // duration of this native-endian byte copy.
                std::slice::from_raw_parts(
                    converted.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(converted.as_slice()),
                )
            };
            destination.copy_from_slice(converted_bytes);
        }
        Ok(())
    })?;
    Ok(bytes.into())
}

fn parse_format(s: &str) -> PyResult<IqFormat> {
    s.parse::<IqFormat>().map_err(py_err)
}

#[derive(Debug)]
enum ByteReadEnd {
    TargetReached,
    StreamEnded,
    Error(sdr_fox_core::SdrError),
}

struct DetachedByteRead {
    checked: CheckedOutStream,
    output: Vec<u8>,
    spill: Option<(IqBlock, usize)>,
    summary: ReadSummary,
    end: ByteReadEnd,
}

/// Accumulate a byte request while the caller has released the GIL.
fn receive_bytes(
    mut checked: CheckedOutStream,
    mut output: Vec<u8>,
    count: usize,
) -> DetachedByteRead {
    let mut summary = ReadSummary::default();
    let mut spill = None;

    let end = loop {
        if output.len() >= count {
            summary.bytes_read = output.len() as u64;
            break ByteReadEnd::TargetReached;
        }

        match checked.handle.recv() {
            None => {
                // A terminal short read is still returned to the caller.
                summary.bytes_read = output.len() as u64;
                break ByteReadEnd::StreamEnded;
            }
            Some(Err(error)) => break ByteReadEnd::Error(error),
            Some(Ok(block)) => {
                let bytes = samples_as_bytes(&block.samples);
                if bytes.is_empty() {
                    break ByteReadEnd::Error(sdr_fox_core::SdrError::Transport(
                        "stream returned an empty IQ block".into(),
                    ));
                }
                summary.record_block(&block);
                let needed = count - output.len();
                let take = needed.min(bytes.len());
                output.extend_from_slice(&bytes[..take]);

                if take < bytes.len() {
                    spill = Some((block, take));
                }
            }
        }
    };

    DetachedByteRead {
        checked,
        output,
        spill,
        summary,
        end,
    }
}

/// Borrow an IQ block as its contiguous native-endian byte representation.
fn samples_as_bytes(samples: &sdr_fox_core::IqSamples) -> &[u8] {
    match samples {
        sdr_fox_core::IqSamples::Cu8(bytes) => bytes,
        sdr_fox_core::IqSamples::Cs8(samples) => unsafe {
            // SAFETY: `i8` and `u8` have identical size/alignment, and the
            // returned slice cannot outlive the input vector.
            std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), samples.len())
        },
        sdr_fox_core::IqSamples::Cs16(samples) => unsafe {
            // SAFETY: the vector is contiguous and the borrowed byte view has
            // exactly the vector's initialized storage length.
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples.as_slice()),
            )
        },
        sdr_fox_core::IqSamples::Cf32(samples) => unsafe {
            // SAFETY: the vector is contiguous and the borrowed byte view has
            // exactly the vector's initialized storage length.
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples.as_slice()),
            )
        },
    }
}

fn py_err(e: sdr_fox_core::SdrError) -> PyErr {
    PyRuntimeError::new_err(format!("{e}"))
}

/// The sdr-fox Python module entry point (called by the Python interpreter).
#[pymodule]
pub fn sdr_fox(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<SdrFox>()?;
    m.add_class::<IqBlockPy>()?;
    m.add_class::<StreamStatsPy>()?;
    m.add_function(wrap_pyfunction!(convert_cu8_to_cf32, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar};
    use std::thread;
    use std::time::Duration;

    use sdr_fox_core::{DeviceInfo, GainMode, GainStep, IqSamples, SdrError, StreamSink};

    #[derive(Default)]
    struct BlockingSinkState {
        recv_started: bool,
        stopped: bool,
        stop_calls: usize,
        drops: usize,
    }

    #[derive(Default)]
    struct BlockingSinkSignals {
        state: Mutex<BlockingSinkState>,
        changed: Condvar,
    }

    impl BlockingSinkSignals {
        fn wait_for_recv(&self) {
            let state = self.state.lock().expect("fake sink mutex poisoned");
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.recv_started)
                .expect("fake sink condvar wait poisoned");
            assert!(!timeout.timed_out(), "fake recv did not start");
            assert!(state.recv_started);
        }

        fn counts(&self) -> (usize, usize) {
            let state = self.state.lock().expect("fake sink mutex poisoned");
            (state.stop_calls, state.drops)
        }

        fn force_stop(&self) {
            let mut state = self.state.lock().expect("fake sink mutex poisoned");
            state.stopped = true;
            self.changed.notify_all();
        }
    }

    struct BlockingSink {
        signals: Arc<BlockingSinkSignals>,
    }

    impl BlockingSink {
        fn new(signals: Arc<BlockingSinkSignals>) -> Self {
            Self { signals }
        }
    }

    impl StreamSink for BlockingSink {
        fn recv(&mut self) -> Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>> {
            let mut state = self.signals.state.lock().expect("fake sink mutex poisoned");
            state.recv_started = true;
            self.signals.changed.notify_all();
            while !state.stopped {
                state = self
                    .signals
                    .changed
                    .wait(state)
                    .expect("fake sink condvar wait poisoned");
            }
            None
        }

        fn recv_deadline(
            &mut self,
            deadline: std::time::Instant,
        ) -> Option<Result<sdr_fox_core::IqBlock, sdr_fox_core::SdrError>> {
            let mut state = self.signals.state.lock().expect("fake sink mutex poisoned");
            state.recv_started = true;
            self.signals.changed.notify_all();
            while !state.stopped {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Some(Err(sdr_fox_core::SdrError::Timeout));
                }
                let (next, timed) = self
                    .signals
                    .changed
                    .wait_timeout(state, deadline - now)
                    .expect("fake sink condvar wait poisoned");
                state = next;
                if timed.timed_out() && !state.stopped {
                    return Some(Err(sdr_fox_core::SdrError::Timeout));
                }
            }
            None
        }

        fn stop_handle(&self) -> StreamStopHandle {
            let signals = Arc::clone(&self.signals);
            StreamStopHandle::new(move || {
                let mut state = signals.state.lock().expect("fake sink mutex poisoned");
                state.stop_calls += 1;
                state.stopped = true;
                signals.changed.notify_all();
            })
        }
    }

    impl Drop for BlockingSink {
        fn drop(&mut self) {
            let mut state = self.signals.state.lock().expect("fake sink mutex poisoned");
            state.drops += 1;
            self.signals.changed.notify_all();
        }
    }

    struct EmptyBlockSink;

    impl StreamSink for EmptyBlockSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            Some(Ok(IqBlock {
                samples: IqSamples::Cu8(Vec::new()),
                dropped: 0,
                sequence: 0,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            }))
        }

        fn recv_deadline(
            &mut self,
            _deadline: std::time::Instant,
        ) -> Option<Result<IqBlock, SdrError>> {
            self.recv()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            StreamStopHandle::new(|| {})
        }
    }

    struct UnusedDevice {
        info: DeviceInfo,
    }

    impl SdrDevice for UnusedDevice {
        fn info(&self) -> &DeviceInfo {
            &self.info
        }

        fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SdrError> {
            Ok(hz)
        }

        fn set_frequency(&mut self, _hz: u64) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_bandwidth(&mut self, _hz: u32) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_gain(&mut self, _req: GainRequest) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_gain_mode(&mut self, _mode: GainMode) -> Result<(), SdrError> {
            Ok(())
        }

        fn gains(&self) -> &[GainStep] {
            &[]
        }

        fn set_bias_tee(&mut self, _on: bool) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_agc(&mut self, _on: bool) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_frequency_correction_ppm(&mut self, _ppm: f64) -> Result<(), SdrError> {
            Ok(())
        }

        fn set_upconverter(&mut self, _up: Option<Upconverter>) -> Result<(), SdrError> {
            Ok(())
        }

        fn start_stream(&mut self, _cfg: StreamConfig) -> Result<StreamHandle, SdrError> {
            Err(SdrError::DeviceBusy)
        }
    }

    fn start_fake_stream(
        slot: &PythonStreamSlot,
        format: IqFormat,
        signals: Arc<BlockingSinkSignals>,
    ) -> u64 {
        let PrepareStream::Start {
            generation,
            retired,
        } = slot.prepare(format).expect("prepare stream")
        else {
            panic!("fresh slot unexpectedly reused a stream");
        };
        stop_and_drop(retired);
        slot.install(generation, format, Box::new(BlockingSink::new(signals)))
            .expect("install stream")
            .unwrap_or_else(|_| panic!("fresh generation rejected stream"));
        generation
    }

    fn stop_and_drop(retired: RetiredStream) {
        let RetiredStream { handle, stop } = retired;
        if let Some(stop) = &stop {
            stop.stop();
        }
        drop((handle, stop));
    }

    #[test]
    fn device_kind_parser_rejects_unknown_values() {
        assert_eq!(parse_device_kind(None).unwrap(), DeviceKind::RtlSdr);
        assert_eq!(
            parse_device_kind(Some("rtl-sdr")).unwrap(),
            DeviceKind::RtlSdr
        );
        assert_eq!(
            parse_device_kind(Some("airspy")).unwrap(),
            DeviceKind::Airspy
        );
        assert!(parse_device_kind(Some("rtl")).is_err());
    }

    #[test]
    fn byte_read_rejects_an_empty_success_block_instead_of_spinning() {
        let checked = CheckedOutStream {
            handle: Box::new(EmptyBlockSink),
            stop: StreamStopHandle::new(|| {}),
            generation: 1,
            format: IqFormat::Cu8,
        };
        let result = receive_bytes(checked, Vec::new(), 1);
        assert!(matches!(
            result.end,
            ByteReadEnd::Error(SdrError::Transport(_))
        ));
    }

    #[test]
    fn pending_block_drains_exact_counts_without_shifting() {
        let mut state = PythonStreamState {
            pending: Some(IqBlock {
                samples: IqSamples::Cu8(vec![1, 2, 3, 4, 5]),
                dropped: 0,
                sequence: 0,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            }),
            ..PythonStreamState::default()
        };
        let mut first = Vec::new();
        state.drain_pending_into(&mut first, 2);
        assert_eq!(first, vec![1, 2]);
        assert_eq!(state.pending_offset, 2);

        let mut second = Vec::new();
        state.drain_pending_into(&mut second, 3);
        assert_eq!(second, vec![3, 4, 5]);
        assert!(state.pending.is_none());
        assert_eq!(state.pending_offset, 0);
    }

    #[test]
    fn close_cancels_checked_out_blocking_recv_without_resurrection() {
        let slot = Arc::new(PythonStreamSlot::default());
        let signals = Arc::new(BlockingSinkSignals::default());
        start_fake_stream(&slot, IqFormat::Cu8, Arc::clone(&signals));

        let checked = slot.checkout(IqFormat::Cu8).expect("checkout stream");
        let recv_slot = Arc::clone(&slot);
        let recv_thread = thread::spawn(move || {
            let mut checked = checked;
            assert!(checked.handle.recv().is_none());
            let (retired, was_current) = recv_slot
                .retire_checked(checked)
                .expect("retire stale receiver");
            stop_and_drop(retired);
            was_current
        });

        signals.wait_for_recv();
        stop_and_drop(slot.close().expect("close active stream"));
        assert!(!recv_thread.join().expect("recv thread panicked"));

        let state = slot.lock_state().expect("inspect closed stream");
        assert!(state.handle.is_none());
        assert!(state.stop.is_none());
        assert!(state.format.is_none());
        assert!(state.pending.is_none());
        drop(state);
        let (stop_calls, drops) = signals.counts();
        assert!(stop_calls >= 1);
        assert_eq!(drops, 1);
    }

    #[test]
    fn close_racing_start_rejects_stale_install() {
        let slot = PythonStreamSlot::default();
        let signals = Arc::new(BlockingSinkSignals::default());
        let PrepareStream::Start {
            generation,
            retired,
        } = slot.prepare(IqFormat::Cf32).expect("reserve generation")
        else {
            panic!("fresh slot unexpectedly reused a stream");
        };
        stop_and_drop(retired);

        stop_and_drop(slot.close().expect("close during startup"));
        let rejected = slot
            .install(
                generation,
                IqFormat::Cf32,
                Box::new(BlockingSink::new(Arc::clone(&signals))),
            )
            .expect("try stale install")
            .expect_err("closed generation must reject installation");
        stop_and_drop(rejected);

        let state = slot.lock_state().expect("inspect closed stream");
        assert!(state.handle.is_none());
        assert!(state.stop.is_none());
        assert!(state.format.is_none());
        drop(state);
        assert_eq!(signals.counts(), (1, 1));
    }

    #[test]
    fn repeated_close_is_idempotent() {
        let slot = PythonStreamSlot::default();
        let signals = Arc::new(BlockingSinkSignals::default());
        start_fake_stream(&slot, IqFormat::Cs16, Arc::clone(&signals));

        stop_and_drop(slot.close().expect("first close"));
        stop_and_drop(slot.close().expect("second close"));

        assert_eq!(signals.counts(), (1, 1));
        let state = slot.lock_state().expect("inspect closed stream");
        assert!(state.handle.is_none());
        assert!(state.stop.is_none());
        assert!(state.format.is_none());
    }

    #[test]
    fn second_reader_gets_busy_instead_of_waiting() {
        let slot = PythonStreamSlot::default();
        let first = slot.try_lock_reader().expect("first reader permit");
        let error = slot
            .try_lock_reader()
            .expect_err("second reader must fail immediately");
        assert_eq!(error, ReadGateError::Busy);
        drop(first);
        assert!(slot.try_lock_reader().is_ok());
    }

    #[test]
    fn stale_receiver_cannot_restore_handle_or_pending_block() {
        let slot = PythonStreamSlot::default();
        let signals = Arc::new(BlockingSinkSignals::default());
        start_fake_stream(&slot, IqFormat::Cs8, Arc::clone(&signals));
        let checked = slot.checkout(IqFormat::Cs8).expect("checkout stream");

        stop_and_drop(slot.close().expect("invalidate checked-out stream"));
        let rejected = slot
            .restore(checked)
            .expect("try stale restore")
            .expect_err("closed generation must reject receiver return");
        stop_and_drop(rejected);
        let state = slot.lock_state().expect("inspect closed stream");
        assert!(state.handle.is_none());
        assert!(state.pending.is_none());
        drop(state);
        assert_eq!(signals.counts(), (2, 1));
    }

    #[test]
    fn format_replacement_fences_old_receiver_return() {
        let slot = PythonStreamSlot::default();
        let old_signals = Arc::new(BlockingSinkSignals::default());
        let new_signals = Arc::new(BlockingSinkSignals::default());
        start_fake_stream(&slot, IqFormat::Cu8, Arc::clone(&old_signals));
        let old_receiver = slot.checkout(IqFormat::Cu8).expect("checkout old stream");

        let PrepareStream::Start {
            generation,
            retired,
        } = slot.prepare(IqFormat::Cf32).expect("reserve replacement")
        else {
            panic!("format change unexpectedly reused old stream");
        };
        stop_and_drop(retired);
        slot.install(
            generation,
            IqFormat::Cf32,
            Box::new(BlockingSink::new(Arc::clone(&new_signals))),
        )
        .expect("install replacement")
        .unwrap_or_else(|_| panic!("replacement generation rejected stream"));

        let rejected = slot
            .restore(old_receiver)
            .expect("try old receiver return")
            .expect_err("old receiver must not replace new format stream");
        stop_and_drop(rejected);
        {
            let state = slot.lock_state().expect("inspect replacement stream");
            assert_eq!(state.format, Some(IqFormat::Cf32));
            assert!(state.handle.is_some());
            assert!(state.stop.is_some());
        }

        stop_and_drop(slot.close().expect("close replacement"));
        assert_eq!(old_signals.counts(), (2, 1));
        assert_eq!(new_signals.counts(), (1, 1));
    }

    #[test]
    fn pyo3_allows_close_while_shared_read_is_detached() {
        Python::initialize();
        let slot = PythonStreamSlot::default();
        let signals = Arc::new(BlockingSinkSignals::default());
        start_fake_stream(&slot, IqFormat::Cu8, Arc::clone(&signals));
        let radio = Python::attach(|py| {
            Py::new(
                py,
                SdrFox {
                    device: Mutex::new(Box::new(UnusedDevice {
                        info: DeviceInfo::default(),
                    })),
                    stream: slot,
                },
            )
            .expect("construct Python SdrFox")
        });
        let (reader, closer) = Python::attach(|py| (radio.clone_ref(py), radio.clone_ref(py)));

        let read_thread = thread::spawn(move || {
            Python::attach(|py| reader.call_method1(py, "read", (2usize, "cu8")))
        });
        signals.wait_for_recv();
        let close_result = Python::attach(|py| closer.call_method0(py, "close"));
        if close_result.is_err() {
            // Keep a failed assertion from stranding the blocking fake thread.
            signals.force_stop();
        }
        let read_result = read_thread.join().expect("Python read thread panicked");

        assert!(
            close_result.is_ok(),
            "shared PyO3 close borrow was rejected"
        );
        assert!(
            read_result.is_err(),
            "cancelled read unexpectedly returned data"
        );
        let (stop_calls, drops) = signals.counts();
        assert!(stop_calls >= 1);
        assert_eq!(drops, 1);
    }
}
