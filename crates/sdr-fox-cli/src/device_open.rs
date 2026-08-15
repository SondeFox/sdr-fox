//! Device enumeration and open helper for the CLI.

use sdr_fox_airspy::AirspyBackend;
use sdr_fox_core::{DeviceDescriptor, DeviceKind, SdrBackend, SdrDevice, SdrError};
use sdr_fox_rtlsdr::RtlSdrBackend;
use sdr_fox_transport::UsbDeviceLocation;

/// The known device families the CLI probes, in priority order.
const PROBE_TABLE: &[(u16, u16, DeviceKind, &str)] = &[
    (0x0bda, 0x2832, DeviceKind::RtlSdr, "RTL-SDR"),
    (0x0bda, 0x2838, DeviceKind::RtlSdr, "RTL-SDR (EEPROM)"),
    (0x1d50, 0x6089, DeviceKind::RtlSdr, "RTL-SDR (Nooelec)"),
    (0x1d50, 0xcc60, DeviceKind::RtlSdr, "RTL-SDR (Blog V4)"),
    (0x1d50, 0x60a1, DeviceKind::Airspy, "Airspy"),
];

/// A boxed open device (the CLI owns it for the duration of a subcommand).
pub type BoxedDevice = Box<dyn SdrDevice>;

/// One known SDR found by the single USB bus enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedDevice {
    /// Backend opener coordinates.
    pub location: UsbDeviceLocation,
    /// Device family.
    pub kind: DeviceKind,
    /// Human-readable family name.
    pub name: &'static str,
}

/// Enumerate the USB bus once and retain known SDR devices.
pub fn enumerate_devices() -> Result<Vec<DetectedDevice>, SdrError> {
    let locations = sdr_fox_transport::enumerate_usb_devices()?;
    Ok(classify_devices(&locations))
}

fn classify_devices(locations: &[UsbDeviceLocation]) -> Vec<DetectedDevice> {
    let mut detected = Vec::new();
    // Preserve the CLI's documented RTL-first family priority while keeping
    // each backend's per-VID/PID match index from the one bus walk.
    for &(vendor_id, product_id, kind, name) in PROBE_TABLE {
        detected.extend(
            locations
                .iter()
                .filter(|location| {
                    location.vendor_id == vendor_id && location.product_id == product_id
                })
                .copied()
                .map(|location| DetectedDevice {
                    location,
                    kind,
                    name,
                }),
        );
    }
    detected
}

/// Open the `index`-th detected device across both backends.
///
/// # Errors
///
/// Returns [`SdrError::DeviceNotFound`] if no device exists at `index`.
pub fn open_device(index: usize) -> Result<BoxedDevice, SdrError> {
    let found = enumerate_devices()?;
    let selected = found
        .get(index)
        .copied()
        .ok_or_else(|| SdrError::DeviceNotFound(format!("no device at index {index}")))?;
    let location = selected.location;
    let transport = sdr_fox_transport::open_default(
        location.vendor_id,
        location.product_id,
        location.match_index,
    )?;
    let descriptor = DeviceDescriptor {
        vendor_id: location.vendor_id,
        product_id: location.product_id,
        vendor_name: None,
        product_name: None,
        serial: None,
        index: location.match_index,
        kind: selected.kind,
    };
    let backend: Box<dyn SdrBackend> = match selected.kind {
        DeviceKind::Airspy => Box::new(AirspyBackend),
        _ => Box::new(RtlSdrBackend),
    };
    backend.open(&descriptor, transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(vendor_id: u16, product_id: u16, match_index: usize) -> UsbDeviceLocation {
        UsbDeviceLocation {
            vendor_id,
            product_id,
            match_index,
        }
    }

    #[test]
    fn classification_is_one_pass_priority_ordered_and_complete() {
        let locations = [
            location(0xffff, 0x0001, 0),
            location(0x1d50, 0x60a1, 0),
            location(0x1d50, 0xcc60, 0),
            location(0x0bda, 0x2832, 0),
            location(0x0bda, 0x2832, 1),
        ];
        let found = classify_devices(&locations);
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].location, location(0x0bda, 0x2832, 0));
        assert_eq!(found[1].location, location(0x0bda, 0x2832, 1));
        assert_eq!(found[2].location, location(0x1d50, 0xcc60, 0));
        assert_eq!(found[3].location, location(0x1d50, 0x60a1, 0));
        assert_eq!(found[2].kind, DeviceKind::RtlSdr);
        assert_eq!(found[3].kind, DeviceKind::Airspy);
    }

    #[test]
    fn all_supported_rtl_ids_are_classified() {
        for &(vendor_id, product_id) in &[
            (0x0bda, 0x2832),
            (0x0bda, 0x2838),
            (0x1d50, 0x6089),
            (0x1d50, 0xcc60),
        ] {
            let found = classify_devices(&[location(vendor_id, product_id, 7)]);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].kind, DeviceKind::RtlSdr);
            assert_eq!(found[0].location.match_index, 7);
        }
    }
}
