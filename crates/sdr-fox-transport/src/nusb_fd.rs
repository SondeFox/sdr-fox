//! nusb file-descriptor transport for Android.
//!
//! Android gives an unprivileged process no way to walk the USB bus. Instead,
//! after the user grants permission, the framework opens the device and hands
//! the app an integer file descriptor via
//! `UsbDeviceConnection.getFileDescriptor()`. This module wraps that descriptor
//! behind the [`Transport`] trait using `nusb` — the same pure-Rust usbfs
//! backend the desktop uses, just entered through its `from_fd` entry point.
//!
//! The Android target excludes `rusb`/`libusb1-sys`, so its `.so` contains no
//! libusb code. nusb is Apache-2.0 OR MIT. See
//! `docs/LIBUSB-REMOVAL-PLAN.md`.
//!
//! ## File-descriptor ownership
//!
//! `getFileDescriptor()` returns an `int` the JVM still owns, so the descriptor
//! is BORROWED here: [`NusbFdTransport::new`] duplicates it and owns only the
//! duplicate, leaving the JVM's `UsbDeviceConnection` and any
//! `ParcelFileDescriptor` owning theirs.
//!
//! This is a correctness requirement, not a style choice. Android tags
//! descriptors with fdsan; if native closes one the JVM still owns, bionic
//! aborts the process outright:
//!
//! ```text
//! fdsan: attempted to close file descriptor 255, expected to be unowned,
//!        actually owned by ParcelFileDescriptor 0x8059cfa
//! ```
//!
//! Duplicating on this side makes the JNI contract impossible to misuse: a
//! caller may pass a borrowed `ParcelFileDescriptor.fd` or a detached one, and
//! either way each side closes only what it owns.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, In, TransferError};
use nusb::{Device, Interface, MaybeFuture};
use sdr_fox_core::session::{reopen_required, ReopenLatch};
use sdr_fox_core::{ControlRequest, SdrError, StreamHandle, TransferDirection, Transport};

use crate::nusb_backend::{
    completion_into_bytes, map_nusb_control_error, NusbBufferSource, NusbTransport, CONTROL_TIMEOUT,
};
use crate::stream::{
    bulk_timeout, start_stream, validate_bulk_len, validate_control_len, validate_stream_config,
    StreamLease,
};

/// nusb file-descriptor transport for Android.
///
/// Holds a cheap clone of the [`Device`] (which keeps the fd alive) and an
/// `Arc<Interface>` for control/bulk I/O. Cheaply clonable so the streaming
/// worker can take its own copy.
///
/// ## Reset semantics
///
/// [`Transport::reset_device`] on this transport is **terminal by design**:
/// it issues the port reset (the only software path that clears a wedged
/// RTL2832U on Android) but never returns `Ok`, because the nusb handle does
/// not survive its own reset and this process cannot re-enumerate the bus to
/// rebuild it — only the app can, via `UsbManager`. After a reset, every
/// clone of this transport refuses further I/O with the stable
/// [`sdr_fox_core::session::REOPEN_REQUIRED_MARKER`] condition. See
/// [`Transport::reset_device`] on the impl below for the full contract.
#[derive(Clone)]
pub struct NusbFdTransport {
    device: Device,
    iface: Arc<Interface>,
    stream_busy: Arc<AtomicBool>,
    /// Shared across clones (streams hold their own transport clone): once a
    /// device reset has been issued, the whole handle family is defunct, and
    /// pretending otherwise reproduces the unrecoverable-recovery bug this
    /// latch exists to prevent.
    health: ReopenLatch,
}

impl NusbFdTransport {
    /// Construct from a raw OS file descriptor obtained from
    /// `UsbDeviceConnection.getFileDescriptor()` on Android.
    ///
    /// `fd` must be a valid, independently owned descriptor — the Kotlin helper
    /// duplicates the framework fd before calling native, so closing it here
    /// never invalidates the Java `UsbDeviceConnection`. `from_fd` consumes the
    /// descriptor; nusb owns and closes it for the lifetime of the resulting
    /// `Device` and any interfaces claimed from it.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Transport`] if the descriptor cannot be wrapped or
    ///   interface 0 cannot be claimed.
    ///
    /// # Safety
    ///
    /// The caller transfers ownership of an open OS file descriptor. The
    /// descriptor must be a valid USB device-node fd obtained from the Android
    /// framework. A negative or closed `fd` is rejected.
    ///
    /// **`fd` is BORROWED, not adopted.** This function duplicates it and owns
    /// only the duplicate; the caller keeps ownership of what it passed in and
    /// must close that itself.
    ///
    /// That direction matters, and getting it backwards aborts the process
    /// rather than leaking. Android tags descriptors with fdsan, so when the
    /// JVM hands one over via `ParcelFileDescriptor.fd` — which borrows — and
    /// native then closes it, bionic aborts with
    /// `fdsan: attempted to close file descriptor N, expected to be unowned,
    /// actually owned by ParcelFileDescriptor`. Duplicating here makes the
    /// contract impossible to misuse from Kotlin: whether the caller passes a
    /// borrowed `.fd` or a detached one, each side closes only its own.
    pub unsafe fn new(fd: i32) -> Result<Self, SdrError> {
        if fd < 0 {
            return Err(SdrError::Transport(format!(
                "invalid Android USB file descriptor: {fd}"
            )));
        }
        // SAFETY: `dup` only reads the descriptor table; an invalid `fd` fails
        // with EBADF rather than misbehaving, which is checked below.
        let duplicated = unsafe { libc::dup(fd) };
        if duplicated < 0 {
            let error = std::io::Error::last_os_error();
            return Err(SdrError::Transport(format!(
                "dup of Android USB file descriptor {fd} failed: {error}"
            )));
        }
        // SAFETY: `duplicated` is a fresh descriptor this function exclusively
        // owns, so transferring it to `OwnedFd` is sound.
        let owned_fd = unsafe { OwnedFd::from_raw_fd(duplicated) };

        let device = Device::from_fd(owned_fd)
            .wait()
            .map_err(|error| SdrError::Transport(format!("nusb from_fd: {error}")))?;
        let iface = device
            .claim_interface(0)
            .wait()
            .map_err(|error| SdrError::Transport(format!("nusb claim interface 0: {error}")))?;
        Ok(Self {
            device,
            iface: Arc::new(iface),
            stream_busy: Arc::new(AtomicBool::new(false)),
            health: ReopenLatch::new(),
        })
    }
}

impl Transport for NusbFdTransport {
    fn control_in(&mut self, req: &ControlRequest) -> Result<Vec<u8>, SdrError> {
        self.health.ensure_usable("control_in")?;
        debug_assert_eq!(req.direction, TransferDirection::In);
        let request = ControlIn {
            control_type: NusbTransport::map_control_type(req.control_type),
            recipient: NusbTransport::map_recipient(req.recipient),
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
        self.health.ensure_usable("control_out")?;
        debug_assert_eq!(req.direction, TransferDirection::Out);
        validate_control_len("control_out", req.data.len())?;
        let request = ControlOut {
            control_type: NusbTransport::map_control_type(req.control_type),
            recipient: NusbTransport::map_recipient(req.recipient),
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
        self.health.ensure_usable("bulk_read")?;
        validate_bulk_len("bulk_read", len)?;
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
        self.health.ensure_usable("start_bulk_stream")?;
        validate_stream_config(buffer_count, buffer_size, queue_depth)?;
        let lease = StreamLease::acquire(&self.stream_busy)?;
        let source =
            NusbBufferSource::new(self.iface.clone(), endpoint, buffer_size, buffer_count)?
                .with_stream_lease(lease);
        Ok(start_stream(source, queue_depth, None))
    }

    /// Issue a USB port reset — and report this handle dead, by design.
    ///
    /// This method **never returns `Ok`**. nusb documents `Device::reset()`
    /// as terminal for the handle ("This `Device` will no longer be usable,
    /// and you should drop it and call `list_devices` to find and re-open
    /// it"), and on Android `list_devices` does not exist: the descriptor
    /// came from the JVM, and only the app can mint a new one via
    /// `UsbManager`. The previous implementation returned `Ok` here, which
    /// sent `RtlSdr::start_stream`'s wedge recovery off to re-initialise and
    /// re-stream a dead handle — guaranteeing the on-device "one USB device
    /// reset plus full reinitialisation did not recover it" failure.
    ///
    /// The reset is still worth issuing rather than refusing outright:
    /// USBDEVFS_RESET is the only software path that clears the RTL2832U
    /// wedge signature (control endpoint answers, bulk endpoint silent, and
    /// re-running init does not help). Without it, the user's only remedy is
    /// physically re-plugging the dongle. So this method resets the port to
    /// clear the hardware, latches every clone of this transport defunct,
    /// and reports the one honest outcome: the app must re-open the device.
    ///
    /// # Errors
    ///
    /// The caller can distinguish three postures:
    ///
    /// - **Re-open the device** — [`SdrError::Transport`] carrying
    ///   [`sdr_fox_core::session::REOPEN_REQUIRED_MARKER`] (test with
    ///   [`sdr_fox_core::session::is_reopen_required`]): the reset was issued
    ///   (or left the handle indeterminate); discard this device and re-open
    ///   it with a fresh `UsbManager` file descriptor.
    /// - **Retry later** — [`SdrError::DeviceBusy`]: the kernel refused the
    ///   reset outright, so nothing was reset and this handle is still
    ///   usable.
    /// - **Device gone** — [`SdrError::DeviceLost`]: the device disappeared;
    ///   re-opening cannot help until it is physically re-attached.
    fn reset_device(&mut self) -> Result<(), SdrError> {
        // A second reset on an already-defunct handle reports the re-open
        // condition instead of ioctl-ing a dead nusb device.
        self.health.ensure_usable("reset_device")?;
        match self.device.reset().wait() {
            Ok(()) => {
                // The reset reached the port, clearing the hardware wedge —
                // but this handle died with it. Returning `Ok` would tell the
                // recovery path "carry on with this handle", the exact lie
                // this method used to tell.
                self.health.mark_defunct();
                Err(reopen_required(
                    "the Android USB device was reset to clear a wedged endpoint, and this \
                     handle did not survive the reset (nusb Device::reset is terminal and \
                     Android cannot re-enumerate); close this device and re-open it with a \
                     fresh UsbManager file descriptor",
                ))
            }
            Err(error) => match error.kind() {
                // The kernel refused the reset outright: nothing was reset
                // and the handle is intact, so this is the one failure that
                // is retryable on the same handle. Do NOT latch.
                nusb::ErrorKind::Busy => Err(SdrError::DeviceBusy),
                // The device is gone entirely; a fresh fd cannot help until
                // it is physically re-attached.
                nusb::ErrorKind::Disconnected | nusb::ErrorKind::NotFound => {
                    self.health.mark_defunct();
                    Err(SdrError::DeviceLost)
                }
                // The reset was attempted and failed for an unknown reason:
                // the handle state is indeterminate, which for the caller
                // means the same thing as a successful reset — re-open.
                _ => {
                    self.health.mark_defunct();
                    Err(reopen_required(&format!(
                        "USB port reset failed ({error}) and left this Android fd handle in \
                         an indeterminate state; close this device and re-open it with a \
                         fresh UsbManager file descriptor"
                    )))
                }
            },
        }
    }

    fn boxed_clone(&self) -> Box<dyn Transport> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{ControlType as CoreControlType, DeviceRecipient};

    // The Android transport's functional paths need a real framework fd and a
    // USB device; they are covered by the on-device validation pass. The
    // control-encoding helpers are shared with the desktop nusb backend and are
    // unit-tested there, so here we only assert the type is wired for the
    // platform it is built for.

    #[test]
    fn nusb_fd_transport_type_exists() {
        // Compile-time check that the struct and its Transport impl are present
        // on Android builds, and that the encoding helpers resolve from this
        // module.
        let _ = std::any::type_name::<NusbFdTransport>();
        // The shared control-type/recipient mappings are the pure-function
        // surface reused from the desktop backend; exercise them so a regression
        // in their visibility is caught at compile time on this target.
        assert_eq!(
            NusbTransport::map_control_type(CoreControlType::Vendor),
            nusb::transfer::ControlType::Vendor
        );
        assert_eq!(
            NusbTransport::map_recipient(DeviceRecipient::Device),
            nusb::transfer::Recipient::Device
        );
    }

    #[test]
    fn new_rejects_negative_fd() {
        // A negative fd is never a valid OS descriptor; `new` must reject it
        // before any ownership transfer, without panicking.
        // SAFETY: no ownership is transferred because the fd is rejected; the
        // function returns Err before calling from_raw_fd.
        assert!(unsafe { NusbFdTransport::new(-1) }.is_err());
    }
}
