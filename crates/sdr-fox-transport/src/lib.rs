//! # sdr-fox-transport
//!
//! USB transport abstraction for sdr-fox with four implementations:
//!
//! - `RusbTransport` — Linux/Windows-only fallback; never compiled on macOS
//!   or Android.
//! - [`NusbTransport`] — pure-Rust USB on macOS/Linux/Windows. macOS uses
//!   the published, unmodified nusb 0.2.7 IOKit path.
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
//! [`open_default`] picks the right backend for the platform: `nusb` on macOS, with a rusb fallback on Linux/Windows. Driver crates should call `open_default` rather than
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
#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub mod rusb_async;
#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub mod rusb_backend;
pub mod stream;

pub use mock::{MockTransport, RecordedRequest, ScriptedReply};
pub use nusb_backend::NusbTransport;
#[cfg(target_os = "android")]
pub use nusb_fd::NusbFdTransport;
#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub use rusb_async::RusbAsyncSource;
#[cfg(not(any(target_os = "android", target_os = "macos")))]
pub use rusb_backend::{RusbBufferSource, RusbTransport};
pub use stream::{
    convert_cu8_block, start_stream, BufferSource, Stream, StreamControl, StreamStats,
    SyntheticSource,
};

/// One USB device discovered in bus order, with the index expected by an
/// `(vendor_id, product_id)` backend opener.
///
/// The string descriptors are **best-effort**: a device may expose none of
/// them, and on some platforms or permission levels the OS withholds them
/// even when the device has them, so `None` means "unknown", never "the
/// device has no such string". They are named after the fields of
/// `sdr_fox_core::DeviceDescriptor` (not after nusb's accessors) so the copy
/// at each descriptor-construction site is one-to-one and greppable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceLocation {
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Manufacturer string descriptor, if the OS handed one over.
    pub vendor_name: Option<String>,
    /// Product string descriptor, if the OS handed one over.
    pub product_name: Option<String>,
    /// Serial string descriptor, if the OS handed one over.
    pub serial: Option<String>,
    /// Zero-based occurrence among devices with this exact VID/PID.
    pub match_index: usize,
}

/// Fold one enumerated device into a [`UsbDeviceLocation`], assigning the
/// next per-VID/PID `match_index` from `counts`.
///
/// Split out of [`enumerate_usb_devices`] so the mapping — string descriptors
/// included — is unit-testable without walking a real USB bus (nusb's device
/// info type cannot be constructed in tests).
#[cfg(not(target_os = "android"))]
fn location_for_device(
    vendor_id: u16,
    product_id: u16,
    vendor_name: Option<&str>,
    product_name: Option<&str>,
    serial: Option<&str>,
    counts: &mut std::collections::HashMap<(u16, u16), usize>,
) -> UsbDeviceLocation {
    let match_index = counts.entry((vendor_id, product_id)).or_default();
    let location = UsbDeviceLocation {
        vendor_id,
        product_id,
        vendor_name: vendor_name.map(str::to_owned),
        product_name: product_name.map(str::to_owned),
        serial: serial.map(str::to_owned),
        match_index: *match_index,
    };
    *match_index += 1;
    location
}

/// Enumerate USB devices once and compute stable per-VID/PID opener indices.
///
/// Each location also carries the device's manufacturer/product/serial string
/// descriptors when the OS exposes them, so callers can surface real device
/// identity (e.g. `sdrfox info`) instead of blanks. Absent strings are `None`,
/// never an error — see [`UsbDeviceLocation`].
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
            location_for_device(
                device.vendor_id(),
                device.product_id(),
                device.manufacturer_string(),
                device.product_string(),
                device.serial_number(),
                &mut counts,
            )
        })
        .collect())
}

/// Open the platform-default transport for the `index`-th device matching
/// `(vendor_id, product_id)`.
///
/// macOS uses only [`NusbTransport`]; Linux/Windows retain the rusb fallback.
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
    // macOS: published nusb only; no libusb fallback enters this target.
    #[cfg(target_os = "macos")]
    {
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

/// Stable identity for exact selection. macOS uses the USB topology location
/// plus the serial when present; unplug/replug at the same port preserves it.
/// Moving an unnumbered receiver requires deliberate reselection.
#[cfg(not(target_os = "android"))]
fn stable_usb_id(info: &nusb::DeviceInfo) -> String {
    use std::fmt::Write;
    #[cfg(target_os = "macos")]
    let location = format!("{:08x}", info.location_id());
    #[cfg(not(target_os = "macos"))]
    let location = format!("{}-{}", info.bus_id(), info.device_address());
    let mut serial = String::new();
    for b in info.serial_number().unwrap_or("").bytes() {
        let _ = write!(serial, "{b:02x}");
    }
    format!(
        "usb:{:04x}:{:04x}:{location}:{serial}",
        info.vendor_id(),
        info.product_id()
    )
}

/// Enumerate stable identifiers paired with lightweight USB metadata.
#[cfg(not(target_os = "android"))]
pub fn enumerate_stable_usb_devices(
) -> Result<Vec<(String, UsbDeviceLocation)>, sdr_fox_core::SdrError> {
    use nusb::MaybeFuture;
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| sdr_fox_core::SdrError::Transport(format!("nusb list: {e}")))?;
    let mut counts = std::collections::HashMap::new();
    Ok(devices
        .map(|d| {
            (
                stable_usb_id(&d),
                location_for_device(
                    d.vendor_id(),
                    d.product_id(),
                    d.manufacturer_string(),
                    d.product_string(),
                    d.serial_number(),
                    &mut counts,
                ),
            )
        })
        .collect())
}

/// Open the exact current native USB object matching an opaque stable ID.
/// Duplicate identities and disappeared devices fail closed. No index fallback.
#[cfg(not(target_os = "android"))]
pub fn open_stable_usb(
    id: &str,
) -> Result<(UsbDeviceLocation, Box<dyn sdr_fox_core::Transport>), sdr_fox_core::SdrError> {
    use nusb::MaybeFuture;
    let mut matches = nusb::list_devices()
        .wait()
        .map_err(|e| sdr_fox_core::SdrError::Transport(format!("nusb list: {e}")))?
        .filter(|d| stable_usb_id(d) == id);
    let info = matches.next().ok_or_else(|| {
        sdr_fox_core::SdrError::DeviceNotFound("selected receiver disconnected".into())
    })?;
    if matches.next().is_some() {
        return Err(sdr_fox_core::SdrError::DeviceNotFound(
            "receiver identity is ambiguous".into(),
        ));
    }
    let location = location_for_device(
        info.vendor_id(),
        info.product_id(),
        info.manufacturer_string(),
        info.product_string(),
        info.serial_number(),
        &mut std::collections::HashMap::new(),
    );
    let transport = NusbTransport::open_info(&info)?;
    Ok((location, Box::new(transport)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn bus_strings_map_onto_the_location_and_absent_ones_stay_none() {
        let mut counts = HashMap::new();
        let with_strings = location_for_device(
            0x0bda,
            0x2838,
            Some("Nooelec"),
            Some("SMArt XTR v5"),
            Some("38956405"),
            &mut counts,
        );
        assert_eq!(with_strings.vendor_id, 0x0bda);
        assert_eq!(with_strings.product_id, 0x2838);
        assert_eq!(with_strings.vendor_name.as_deref(), Some("Nooelec"));
        assert_eq!(with_strings.product_name.as_deref(), Some("SMArt XTR v5"));
        assert_eq!(with_strings.serial.as_deref(), Some("38956405"));

        let without_strings = location_for_device(0x1d50, 0x60a1, None, None, None, &mut counts);
        assert_eq!(without_strings.vendor_name, None);
        assert_eq!(without_strings.product_name, None);
        assert_eq!(without_strings.serial, None);
    }

    #[test]
    fn match_index_counts_per_vid_pid_and_ignores_strings() {
        let mut counts = HashMap::new();
        // Two dongles with the same VID/PID but different serials must still
        // get distinct opener indices — the index is the opener's coordinate,
        // the strings are identity metadata only.
        let first = location_for_device(0x0bda, 0x2838, None, None, Some("A"), &mut counts);
        let second = location_for_device(0x0bda, 0x2838, None, None, Some("B"), &mut counts);
        let other_family = location_for_device(0x1d50, 0x60a1, None, None, None, &mut counts);
        assert_eq!(first.match_index, 0);
        assert_eq!(second.match_index, 1);
        assert_eq!(other_family.match_index, 0);
    }
}
