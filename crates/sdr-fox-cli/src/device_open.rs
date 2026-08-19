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
///
/// Not `Copy`: the location carries the bus's best-effort string descriptors
/// (owned `String`s), which the open path forwards into the device
/// descriptor so `sdrfox info` can report real identity.
#[derive(Debug, Clone, PartialEq, Eq)]
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
                .cloned()
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
        .cloned()
        .ok_or_else(|| SdrError::DeviceNotFound(format!("no device at index {index}")))?;
    let location = &selected.location;
    let transport = sdr_fox_transport::open_default(
        location.vendor_id,
        location.product_id,
        location.match_index,
    )?;
    let descriptor = descriptor_for(&selected);
    let backend: Box<dyn SdrBackend> = match selected.kind {
        DeviceKind::Airspy => Box::new(AirspyBackend),
        _ => Box::new(RtlSdrBackend),
    };
    backend.open(&descriptor, transport)
}

/// Build the open descriptor for a detected device, forwarding the bus's
/// best-effort string descriptors so an opened device reports its real
/// manufacturer/product/serial instead of blanks.
fn descriptor_for(selected: &DetectedDevice) -> DeviceDescriptor {
    DeviceDescriptor {
        vendor_id: selected.location.vendor_id,
        product_id: selected.location.product_id,
        vendor_name: selected.location.vendor_name.clone(),
        product_name: selected.location.product_name.clone(),
        serial: selected.location.serial.clone(),
        index: selected.location.match_index,
        kind: selected.kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(vendor_id: u16, product_id: u16, match_index: usize) -> UsbDeviceLocation {
        UsbDeviceLocation {
            vendor_id,
            product_id,
            vendor_name: None,
            product_name: None,
            serial: None,
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

    #[test]
    fn open_descriptor_carries_the_bus_string_descriptors() {
        let mut with_strings = location(0x0bda, 0x2838, 3);
        with_strings.vendor_name = Some("Nooelec".to_string());
        with_strings.product_name = Some("SMArt XTR v5".to_string());
        with_strings.serial = Some("38956405".to_string());
        let detected = DetectedDevice {
            location: with_strings,
            kind: DeviceKind::RtlSdr,
            name: "RTL-SDR (EEPROM)",
        };
        let descriptor = descriptor_for(&detected);
        assert_eq!(descriptor.vendor_id, 0x0bda);
        assert_eq!(descriptor.product_id, 0x2838);
        assert_eq!(descriptor.vendor_name.as_deref(), Some("Nooelec"));
        assert_eq!(descriptor.product_name.as_deref(), Some("SMArt XTR v5"));
        assert_eq!(descriptor.serial.as_deref(), Some("38956405"));
        assert_eq!(descriptor.index, 3);
        assert_eq!(descriptor.kind, DeviceKind::RtlSdr);
    }

    #[test]
    fn classification_preserves_string_descriptors() {
        let mut with_serial = location(0x0bda, 0x2838, 0);
        with_serial.serial = Some("00000001".to_string());
        let found = classify_devices(std::slice::from_ref(&with_serial));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].location, with_serial);
    }
}
