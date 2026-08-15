//! # sdr-fox-transport
//!
//! USB transport abstraction for sdr-fox with four implementations:
//!
//! - [`RusbTransport`] — libusb-backed (`rusb`). **macOS desktop default**
//!   (nusb's IOKit backend stalls RTL2832U control-OUT transfers; libusb
//!   works, as `rtl_test` confirms). Reliable cross-platform fallback.
//!   Desktop only: not compiled for Android (see `NusbFdTransport`).
//! - [`NusbTransport`] — pure-Rust (`nusb`). Default on Linux/Windows
//!   (no libusb dylib). Works for control-IN but stalls control-OUT on macOS.
//! - `NusbFdTransport` — pure-Rust (`nusb`) over an Android-injected fd.
//!   The only transport compiled for Android. Uses `nusb::Device::from_fd`,
//!   so the Android `.so` contains no libusb code.
//! - [`MockTransport`] — records requests, replays scripted replies.
//!
//! The streaming surface is a bounded-channel `Stream` with non-blocking,
//! explicitly counted drop-newest overload handling; the USB source differs
//! per backend.
//!
//! ## Which backend opens what
//!
//! [`open_default`] picks the right backend for the platform: `rusb` on macOS,
//! `nusb` elsewhere. Driver crates should call `open_default` rather than
//! hard-coding a backend. Android does not enumerate the bus and reaches the
//! device through `NusbFdTransport` instead.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
// The libusb async FFI in rusb_async requires unsafe (it's direct C FFI to
// libusb-1.0's transfer API). The rest of the crate remains safe. rusb_async
// and rusb_backend are not compiled for Android at all, so an Android build
// pulls in no libusb FFI and links no libusb.
#![allow(unsafe_code)]

pub mod mock;
// Pure-Rust USB backend. Compiles on every target; the enumerating
// `NusbTransport::open` is gated to non-Android (an unprivileged Android
// process cannot walk the USB bus — nusb::list_devices is unavailable there).
// The streaming source and control/bulk methods this module defines are
// reused by the Android fd transport, which enters the same nusb usbfs
// backend through `nusb::Device::from_fd`.
pub mod nusb_backend;
// Android fd-injection transport, built only on Android. Enters the same
// pure-Rust usbfs backend through `nusb::Device::from_fd`, so Android links no
// libusb. See docs/LIBUSB-REMOVAL-PLAN.md.
#[cfg(target_os = "android")]
pub mod nusb_fd;
#[cfg(not(target_os = "android"))]
pub mod rusb_async;
#[cfg(not(target_os = "android"))]
pub mod rusb_backend;
pub mod stream;

pub use mock::{MockTransport, RecordedRequest, ScriptedReply};
pub use nusb_backend::NusbTransport;
#[cfg(target_os = "android")]
pub use nusb_fd::NusbFdTransport;
#[cfg(not(target_os = "android"))]
pub use rusb_async::RusbAsyncSource;
#[cfg(not(target_os = "android"))]
pub use rusb_backend::{RusbBufferSource, RusbTransport};
pub use stream::{
    convert_cu8_block, start_stream, BufferSource, Stream, StreamControl, StreamStats,
    SyntheticSource,
};

/// One USB device discovered in bus order, with the index expected by an
/// `(vendor_id, product_id)` backend opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbDeviceLocation {
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Zero-based occurrence among devices with this exact VID/PID.
    pub match_index: usize,
}

/// Enumerate USB devices once and compute stable per-VID/PID opener indices.
///
/// Not available on Android: the platform gives an unprivileged process no way
/// to walk the USB bus, so a device arrives as a file descriptor handed down by
/// the JVM after the user grants permission. Use `NusbFdTransport::new` with
/// that descriptor instead, and do the enumeration on the Java side via
/// `UsbManager`.
///
/// # Errors
///
/// Returns [`sdr_fox_core::SdrError::Transport`] if bus enumeration fails.
#[cfg(not(target_os = "android"))]
pub fn enumerate_usb_devices() -> Result<Vec<UsbDeviceLocation>, sdr_fox_core::SdrError> {
    use std::collections::HashMap;

    use nusb::MaybeFuture;

    let devices = nusb::list_devices()
        .wait()
        .map_err(|error| sdr_fox_core::SdrError::Transport(format!("nusb list: {error}")))?;
    let mut counts = HashMap::<(u16, u16), usize>::new();
    Ok(devices
        .map(|device| {
            let key = (device.vendor_id(), device.product_id());
            let match_index = counts.entry(key).or_default();
            let location = UsbDeviceLocation {
                vendor_id: key.0,
                product_id: key.1,
                match_index: *match_index,
            };
            *match_index += 1;
            location
        })
        .collect())
}

/// Open the platform-default transport for the `index`-th device matching
/// `(vendor_id, product_id)`.
///
/// On macOS this uses [`RusbTransport`] (libusb) because nusb's IOKit backend
/// stalls RTL2832U control-OUT transfers; elsewhere it uses [`NusbTransport`]
/// (pure-Rust, no libusb dylib). Falls back to the other backend on failure.
///
/// Not meaningful on Android: an unprivileged Android process cannot enumerate
/// the USB bus, so a device is never opened by VID/PID/index. The Android path
/// opens through `NusbFdTransport::new` with a descriptor the JVM hands down
/// after the user grants USB permission.
///
/// # Errors
///
/// - [`sdr_fox_core::SdrError::DeviceNotFound`] if no matching device.
/// - [`sdr_fox_core::SdrError::Transport`] on open/claim failure.
#[cfg(not(target_os = "android"))]
pub fn open_default(
    vendor_id: u16,
    product_id: u16,
    index: usize,
) -> Result<Box<dyn sdr_fox_core::Transport>, sdr_fox_core::SdrError> {
    // macOS: prefer rusb (libusb) — nusb stalls RTL2832U control-OUTs here.
    #[cfg(target_os = "macos")]
    {
        match RusbTransport::open(vendor_id, product_id, index) {
            Ok(t) => return Ok(Box::new(t)),
            Err(e) => {
                tracing::debug!("rusb open failed, falling back to nusb: {e}");
            }
        }
        NusbTransport::open(vendor_id, product_id, index)
            .map(|t| Box::new(t) as Box<dyn sdr_fox_core::Transport>)
    }
    // Linux/Windows: prefer nusb, fall back to rusb.
    #[cfg(all(not(target_os = "macos"), not(target_os = "android")))]
    {
        match NusbTransport::open(vendor_id, product_id, index) {
            Ok(t) => return Ok(Box::new(t)),
            Err(e) => {
                tracing::debug!("nusb open failed, falling back to rusb: {e}");
            }
        }
        RusbTransport::open(vendor_id, product_id, index)
            .map(|t| Box::new(t) as Box<dyn sdr_fox_core::Transport>)
    }
}
