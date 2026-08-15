//! libusb-backed transport (`rusb`).
//!
//! This is the **macOS desktop default** because nusb's IOKit backend stalls
//! vendor control-OUT transfers on the RTL2832U (the device is fully
//! functional under libusb — `rtl_test` proves it). On Linux/Windows nusb is
//! preferred (pure-Rust, no libusb dylib), but this backend works everywhere
//! libusb does and is the reliable cross-platform fallback.
//!
//! Async streaming uses a ring of pre-submitted libusb transfers with
//! re-submit-on-completion (the osmocom librtlsdr pattern), delivered through
//! the same bounded-channel `Stream` as the nusb backend.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use rusb::{Context, DeviceHandle, GlobalContext, UsbContext};
use sdr_fox_core::{
    ControlRequest, ControlType as CoreControlType, DeviceRecipient, SdrError, TransferDirection,
    Transport,
};

use crate::stream::{
    bulk_timeout, start_stream, validate_bulk_len, validate_control_len, validate_stream_config,
    BufferSource, StreamLease,
};

const CONTROL_TIMEOUT: Duration = Duration::from_millis(300);
const BULK_TIMEOUT: Duration = Duration::from_secs(1);

/// libusb-backed transport. Holds an `Arc<DeviceHandle>` so it is cheaply
/// clonable for the streaming worker.
#[derive(Clone)]
pub struct RusbTransport {
    handle: RusbHandle,
    stream_busy: Arc<AtomicBool>,
}

#[derive(Clone)]
enum RusbHandle {
    Global(Arc<DeviceHandle<GlobalContext>>),
    Private(Arc<DeviceHandle<Context>>),
}

impl RusbTransport {
    /// Wrap an already-opened, claimed libusb handle.
    #[must_use]
    pub fn from_handle(handle: DeviceHandle<GlobalContext>) -> Self {
        Self {
            handle: RusbHandle::Global(Arc::new(handle)),
            stream_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Expose the raw libusb device handle pointer (for the async FFI backend).
    #[must_use]
    pub fn raw_handle(&self) -> *mut std::ffi::c_void {
        match &self.handle {
            RusbHandle::Global(handle) => handle.as_raw().cast(),
            RusbHandle::Private(handle) => handle.as_raw().cast(),
        }
    }

    /// Expose the raw libusb context pointer (for the async FFI backend).
    #[must_use]
    pub fn raw_context(&self) -> *mut std::ffi::c_void {
        match &self.handle {
            RusbHandle::Global(handle) => handle.context().as_raw().cast(),
            RusbHandle::Private(handle) => handle.context().as_raw().cast(),
        }
    }

    /// Open the `index`-th device matching `(vendor_id, product_id)`, claim
    /// interface 0, and (on Linux) detach the kernel driver.
    ///
    /// # Errors
    ///
    /// - [`SdrError::DeviceNotFound`] if no matching device exists.
    /// - [`SdrError::Transport`] on open/claim failure.
    pub fn open(vendor_id: u16, product_id: u16, index: usize) -> Result<Self, SdrError> {
        let context = Context::new()
            .map_err(|error| SdrError::Transport(format!("rusb context: {error}")))?;
        let devices = context
            .devices()
            .map_err(|error| SdrError::Transport(format!("rusb devices: {error}")))?;
        let dev = devices
            .iter()
            .filter(|d| {
                d.device_descriptor()
                    .is_ok_and(|dd| dd.vendor_id() == vendor_id && dd.product_id() == product_id)
            })
            .nth(index)
            .ok_or_else(|| {
                SdrError::DeviceNotFound(format!(
                    "no USB device vid={vendor_id:#06x} pid={product_id:#06x} at index {index}"
                ))
            })?;
        let handle = dev
            .open()
            .map_err(|e| SdrError::Transport(format!("rusb open: {e}")))?;
        #[cfg(target_os = "linux")]
        {
            let _ = handle.set_auto_detach_kernel_driver(true);
        }
        // macOS/Windows: claim directly. If a kernel driver is attached on
        // macOS it's typically none for RTL-SDR (no in-tree DVB driver).
        handle
            .claim_interface(0)
            .map_err(|e| SdrError::Transport(format!("rusb claim interface 0: {e}")))?;
        Ok(Self {
            handle: RusbHandle::Private(Arc::new(handle)),
            stream_busy: Arc::new(AtomicBool::new(false)),
        })
    }

    fn request_type(direction: TransferDirection, ct: CoreControlType, r: DeviceRecipient) -> u8 {
        // libusb encodes bmRequestType as a single byte.
        let dir = match direction {
            TransferDirection::Out => 0x00, // host→device
            TransferDirection::In => 0x80,  // device→host
        };
        let typ = match ct {
            CoreControlType::Standard => 0x00,
            CoreControlType::Class => 0x20,
            CoreControlType::Vendor => 0x40,
        };
        let rec = match r {
            DeviceRecipient::Device => 0x00,
            DeviceRecipient::Interface => 0x01,
            DeviceRecipient::Endpoint => 0x02,
            DeviceRecipient::Other => 0x03,
        };
        dir | typ | rec
    }
}

impl Transport for RusbTransport {
    fn control_in(&mut self, req: &ControlRequest) -> Result<Vec<u8>, SdrError> {
        debug_assert_eq!(req.direction, TransferDirection::In);
        validate_control_len("control_in", req.data.len())?;
        let bmrequest = Self::request_type(TransferDirection::In, req.control_type, req.recipient);
        let mut buf = vec![0u8; req.data.len()];
        let result = match &self.handle {
            RusbHandle::Global(handle) => handle.read_control(
                bmrequest,
                req.request,
                req.value,
                req.index,
                &mut buf,
                CONTROL_TIMEOUT,
            ),
            RusbHandle::Private(handle) => handle.read_control(
                bmrequest,
                req.request,
                req.value,
                req.index,
                &mut buf,
                CONTROL_TIMEOUT,
            ),
        };
        let n = result.map_err(|e| map_rusb_error("control_in", e))?;
        if n != req.data.len() {
            return Err(SdrError::ShortTransfer {
                operation: "control_in",
                expected: req.data.len(),
                actual: n,
            });
        }
        Ok(buf)
    }

    fn control_out(&mut self, req: &ControlRequest) -> Result<usize, SdrError> {
        debug_assert_eq!(req.direction, TransferDirection::Out);
        validate_control_len("control_out", req.data.len())?;
        let bmrequest = Self::request_type(TransferDirection::Out, req.control_type, req.recipient);
        let request = req.request;
        let result = match &self.handle {
            RusbHandle::Global(handle) => handle.write_control(
                bmrequest,
                request,
                req.value,
                req.index,
                &req.data,
                CONTROL_TIMEOUT,
            ),
            RusbHandle::Private(handle) => handle.write_control(
                bmrequest,
                request,
                req.value,
                req.index,
                &req.data,
                CONTROL_TIMEOUT,
            ),
        };
        let n = result.map_err(|e| map_rusb_error("control_out", e))?;
        if n != req.data.len() {
            return Err(SdrError::ShortTransfer {
                operation: "control_out",
                expected: req.data.len(),
                actual: n,
            });
        }
        Ok(n)
    }

    fn bulk_read(
        &mut self,
        endpoint: u8,
        len: usize,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, SdrError> {
        validate_bulk_len("bulk_read", len)?;
        let timeout = bulk_timeout(timeout_ms);
        let mut buf = vec![0u8; len];
        let result = match &self.handle {
            RusbHandle::Global(handle) => handle.read_bulk(endpoint, &mut buf, timeout),
            RusbHandle::Private(handle) => handle.read_bulk(endpoint, &mut buf, timeout),
        };
        let n = result.map_err(|e| map_rusb_error("bulk_read", e))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn start_bulk_stream(
        &mut self,
        endpoint: u8,
        buffer_count: usize,
        buffer_size: usize,
        queue_depth: usize,
    ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
        validate_stream_config(buffer_count, buffer_size, queue_depth)?;
        // Async multi-URB ring via direct libusb-1.0 FFI — the canonical
        // RTL-SDR streaming pattern. Pre-submits N bulk transfers, reaps on
        // completion via libusb_handle_events_completed. The synchronous
        // read_bulk path gets only ~10 kS/s because the EPA FIFO overflows
        // between single transfers; the async ring keeps the pipeline full.
        //
        // The source retains this clone of the Arc<DeviceHandle> so the device
        // stays alive for the full stream lifetime even if the transport is
        // dropped while the stream worker is still running (P0#1 fix).
        let lease = StreamLease::acquire(&self.stream_busy)?;
        match &self.handle {
            RusbHandle::Global(handle) => {
                let source = crate::rusb_async::RusbAsyncSource::new(
                    handle.clone(),
                    endpoint,
                    buffer_size,
                    buffer_count,
                    queue_depth.max(buffer_count),
                    lease,
                )?;
                Ok(start_stream(source, queue_depth, None))
            }
            RusbHandle::Private(handle) => {
                let source = crate::rusb_async::LibusbAsyncSource::new(
                    handle.clone(),
                    endpoint,
                    buffer_size,
                    buffer_count,
                    queue_depth.max(buffer_count),
                    lease,
                )?;
                Ok(start_stream(source, queue_depth, None))
            }
        }
    }

    fn reset_device(&mut self) -> Result<(), SdrError> {
        // Recovery for a wedged dongle: the control endpoint still answers but
        // the bulk endpoint delivers nothing. Observed on an RTL2832U after a
        // host process was killed mid-stream — control transfers (tune, gain,
        // register reads) all succeed while streaming returns zero bytes
        // indefinitely, and nothing short of a port reset clears it.
        //
        // libusb re-enumerates the device, so every register written before
        // this point is gone; the caller must re-run its full initialisation.
        let result = match &self.handle {
            RusbHandle::Global(handle) => handle.reset(),
            RusbHandle::Private(handle) => handle.reset(),
        };
        result.map_err(|error| match error {
            rusb::Error::NoDevice | rusb::Error::NotFound => SdrError::DeviceLost,
            other => SdrError::Transport(format!("USB port reset failed: {other}")),
        })
    }

    fn boxed_clone(&self) -> Box<dyn Transport> {
        Box::new(self.clone())
    }
}

/// Legacy `BufferSource` backed by synchronous libusb bulk reads.
///
/// Production streams use the callback-driven async ring. This type remains
/// only for downstream source compatibility; new code should obtain a stream
/// through [`RusbTransport::start_bulk_stream`].
#[doc(hidden)]
pub struct RusbBufferSource {
    handle: Arc<DeviceHandle<GlobalContext>>,
    endpoint: u8,
    buffer_size: usize,
    #[allow(dead_code)]
    _buffer_count: usize,
}

impl RusbBufferSource {
    /// Construct. Validates the buffer size is a 512 multiple.
    ///
    /// # Errors
    ///
    /// Returns [`SdrError::InvalidParameter`] for non-512 buffer sizes.
    pub fn new(
        handle: Arc<DeviceHandle<GlobalContext>>,
        endpoint: u8,
        buffer_size: usize,
        buffer_count: usize,
    ) -> Result<Self, SdrError> {
        validate_stream_config(buffer_count.max(1), buffer_size, 1)?;
        Ok(Self {
            handle,
            endpoint,
            buffer_size,
            _buffer_count: buffer_count,
        })
    }
}

impl BufferSource for RusbBufferSource {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        let mut buf = vec![0u8; self.buffer_size];
        let n = self
            .handle
            .read_bulk(self.endpoint, &mut buf, BULK_TIMEOUT)
            .map_err(|e| map_rusb_error("bulk stream", e))?;
        buf.truncate(n);
        Ok(buf)
    }
}

fn map_rusb_error(operation: &str, error: rusb::Error) -> SdrError {
    match error {
        rusb::Error::NoDevice => SdrError::DeviceLost,
        rusb::Error::Timeout => SdrError::Timeout,
        rusb::Error::Overflow => SdrError::Overflow { dropped_samples: 0 },
        rusb::Error::Pipe => SdrError::Stall,
        rusb::Error::Interrupted => SdrError::Cancelled,
        rusb::Error::Busy => SdrError::DeviceBusy,
        other => SdrError::Transport(format!("{operation}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_type_encoding_matches_libusb() {
        // Vendor IN to device = 0x80 | 0x40 | 0x00 = 0xC0.
        assert_eq!(
            RusbTransport::request_type(
                TransferDirection::In,
                CoreControlType::Vendor,
                DeviceRecipient::Device
            ),
            0xC0
        );
        // Vendor OUT to device = 0x00 | 0x40 | 0x00 = 0x40.
        assert_eq!(
            RusbTransport::request_type(
                TransferDirection::Out,
                CoreControlType::Vendor,
                DeviceRecipient::Device
            ),
            0x40
        );
        // Vendor IN to interface = 0x80 | 0x40 | 0x01 = 0xC1.
        assert_eq!(
            RusbTransport::request_type(
                TransferDirection::In,
                CoreControlType::Vendor,
                DeviceRecipient::Interface
            ),
            0xC1
        );
    }

    #[test]
    fn rusb_errors_map_to_typed_stream_errors() {
        assert!(matches!(
            map_rusb_error("read", rusb::Error::NoDevice),
            SdrError::DeviceLost
        ));
        assert!(matches!(
            map_rusb_error("read", rusb::Error::Timeout),
            SdrError::Timeout
        ));
        assert!(matches!(
            map_rusb_error("read", rusb::Error::Pipe),
            SdrError::Stall
        ));
        assert!(matches!(
            map_rusb_error("read", rusb::Error::Busy),
            SdrError::DeviceBusy
        ));
    }
}
