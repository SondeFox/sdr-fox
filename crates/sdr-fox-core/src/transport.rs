//! Transport trait: the USB I/O abstraction.
//!
//! Implementations live in `sdr-fox-transport`:
//! - [`crate::transport::Transport`] is implemented by `NusbTransport`
//!   (desktop, pure-Rust), `RusbTransport` (desktop, libusb), and
//!   `NusbFdTransport` (Android, pure-Rust over an injected fd).
//! - `MockTransport` is used for unit tests in every driver crate.

use crate::error::SdrError;

/// Direction of a control transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferDirection {
    /// Host → device.
    Out,
    /// Device → host.
    In,
}

/// Control transfer type (USB spec §9.3, bmRequestType bits 5..6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlType {
    /// Standard request.
    Standard,
    /// Class request.
    Class,
    /// Vendor request (the type RTL2832U and Airspy use).
    Vendor,
}

/// Recipient of a control transfer (bmRequestType bits 0..4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceRecipient {
    /// Device recipient.
    Device,
    /// Interface recipient (used by RTL2832U/Airspy control transfers).
    Interface,
    /// Endpoint recipient.
    Endpoint,
    /// Other.
    Other,
}

/// A control transfer request, builder-style.
///
/// For RTL2832U the conventional encoding is `ControlType::Vendor` /
/// `Recipient::Device`, with `bRequest = 0` (the command is encoded in
/// `value`/`index`). Airspy puts its command in `bRequest` and uses
/// `value`/`index` for command parameters.
#[derive(Debug, Clone)]
pub struct ControlRequest {
    /// Direction.
    pub direction: TransferDirection,
    /// Request type.
    pub control_type: ControlType,
    /// Recipient.
    pub recipient: DeviceRecipient,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// For `Out`: the bytes to send. For `In`: ignored (length implied by the
    /// expected reply length passed to [`Transport::control_in`]).
    pub data: Vec<u8>,
}

impl ControlRequest {
    /// Build a vendor OUT control transfer (host → device).
    #[must_use]
    pub fn vendor_out(request: u8, value: u16, index: u16, data: Vec<u8>) -> Self {
        Self {
            direction: TransferDirection::Out,
            control_type: ControlType::Vendor,
            recipient: DeviceRecipient::Device,
            request,
            value,
            index,
            data,
        }
    }

    /// Build a vendor IN control transfer (device → host).
    #[must_use]
    pub fn vendor_in(request: u8, value: u16, index: u16, expected_len: usize) -> Self {
        Self {
            direction: TransferDirection::In,
            control_type: ControlType::Vendor,
            recipient: DeviceRecipient::Device,
            request,
            value,
            index,
            data: vec![0; expected_len],
        }
    }
}

/// The USB transport abstraction.
///
/// Control transfers are synchronous (cheap, low frequency). Bulk streaming
/// is asynchronous (high frequency, latency-sensitive); see
/// [`Transport::start_bulk_stream`].
pub trait Transport: Send {
    /// Perform a synchronous control transfer (IN).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Timeout`] when the operation times out.
    /// - [`SdrError::ShortTransfer`] when the reply is not exactly the
    ///   requested length.
    /// - [`SdrError::Transport`] on another transfer failure.
    /// - [`SdrError::DeviceLost`] if the device disappeared.
    fn control_in(&mut self, req: &ControlRequest) -> Result<Vec<u8>, SdrError>;

    /// Perform a synchronous control transfer (OUT). Returns bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`SdrError::Timeout`] on timeout and [`SdrError::Transport`] on
    /// another transfer failure. A successful but incomplete write returns
    /// [`SdrError::ShortTransfer`].
    fn control_out(&mut self, req: &ControlRequest) -> Result<usize, SdrError>;

    /// Perform one synchronous bulk read. Useful for low-rate capture and tests;
    /// streaming uses [`Transport::start_bulk_stream`] instead.
    /// `timeout_ms == 0` selects the finite one-second backend default; it
    /// never means an unbounded wait or an immediate poll.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Timeout`] on timeout.
    /// - [`SdrError::Stall`] when the endpoint is halted.
    /// - [`SdrError::Transport`] on another transfer failure.
    /// - [`SdrError::DeviceLost`] on hot-unplug.
    fn bulk_read(&mut self, endpoint: u8, len: usize, timeout_ms: u32)
        -> Result<Vec<u8>, SdrError>;

    /// Begin a multi-transfer bulk stream on `endpoint`. Returns a receiver
    /// of completed buffers plus a control handle. The implementation owns
    /// the worker thread and the ring of N in-flight transfers.
    ///
    /// # Errors
    ///
    /// - [`SdrError::DeviceBusy`] if a stream is already running on this transport.
    /// - [`SdrError::Transport`] if the transfer ring cannot be submitted.
    fn start_bulk_stream(
        &mut self,
        endpoint: u8,
        buffer_count: usize,
        buffer_size: usize,
        queue_depth: usize,
    ) -> Result<crate::sample::StreamHandle, SdrError>;

    /// Boxed clone so a streaming worker can take its own copy. The
    /// trait-object equivalent of the Arc-backed `Device: Clone` pattern in
    /// desperado `rs-rtl` (MIT; see the workspace `NOTICE` file).
    fn boxed_clone(&self) -> Box<dyn Transport>;

    /// Issue a USB port reset, re-establishing the device from a clean state.
    ///
    /// This is the escape hatch for a wedged device: one whose control
    /// endpoint still answers but which no longer delivers bulk data, a state
    /// an RTL2832U can reach after a host process is killed mid-stream. Driver
    /// open paths use it as a last resort, since it is disruptive — the device
    /// re-enumerates, and any configuration written before the reset is lost,
    /// so the caller must re-run its full initialisation afterwards.
    ///
    /// The default implementation reports [`SdrError::Unsupported`], which is
    /// correct for backends with no way to reach the port (the mock, and any
    /// transport handed a descriptor it does not own).
    ///
    /// # Errors
    ///
    /// - [`SdrError::Unsupported`] if this backend cannot reset the port.
    /// - [`SdrError::Transport`] if the reset itself fails.
    /// - [`SdrError::DeviceLost`] if the device did not come back.
    fn reset_device(&mut self) -> Result<(), SdrError> {
        Err(SdrError::Unsupported(
            "USB port reset is not supported by this transport".into(),
        ))
    }
}

impl Clone for Box<dyn Transport> {
    fn clone(&self) -> Self {
        self.boxed_clone()
    }
}
