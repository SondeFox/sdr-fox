//! Airspy R2 / Mini device driver.
//!
//! Speaks the Airspy vendor USB control protocol (VID 0x1d50, PID 0x60a1).
//! The command IDs and their argument encodings below are what the Airspy
//! firmware accepts; the open and health-probe sequence follows the order the
//! device requires, as also seen in the SondeFox `Airspy.kt` implementation.
//!
//! ## Vendor command IDs
//!
//! `RECEIVER_MODE=1`, `BOARD_ID_READ=9`, `VERSION_STRING_READ=10`,
//! `SET_SAMPLERATE=12`, `SET_FREQ=13`, `SET_LNA_GAIN=14`, `SET_MIXER_GAIN=15`,
//! `SET_VGA_GAIN=16`, `SET_LNA_AGC=17`, `SET_MIXER_AGC=18`, `GPIO_WRITE=21`
//! (the bias tee is GPIO port 1 pin 13 — see [`rf_bias_request`]),
//! `GET_SAMPLERATES=25`, `SET_PACKING=26`.
//!
//! Gain/rate commands are DIR_IN with `wValue=0`, parameter in `wIndex`, plus a
//! 1-byte ack read-back. Frequency is DIR_OUT with the Hz in the data payload.

use sdr_fox_core::{
    ControlRequest, DeviceDescriptor, DeviceInfo, DeviceKind, GainMode, GainRequest, GainStageId,
    GainStep, SdrDevice, SdrError, StreamConfig, Transport, Upconverter,
};

const AIRSPY_GAIN_STEP_COUNT: usize = 15 + 16 + 16;

const fn airspy_gain_steps() -> [GainStep; AIRSPY_GAIN_STEP_COUNT] {
    let mut gains = [GainStep::new("LNA", 0); AIRSPY_GAIN_STEP_COUNT];
    let mut output = 0usize;
    let mut value = 0i32;
    while value <= 14 {
        gains[output] = GainStep::new("LNA", value * 10);
        output += 1;
        value += 1;
    }
    value = 0;
    while value <= 15 {
        gains[output] = GainStep::new("MIXER", value * 10);
        output += 1;
        value += 1;
    }
    value = 0;
    while value <= 15 {
        gains[output] = GainStep::new("VGA", value * 10);
        output += 1;
        value += 1;
    }
    gains
}

const AIRSPY_GAINS: [GainStep; AIRSPY_GAIN_STEP_COUNT] = airspy_gain_steps();
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::iq_synth::IqSynthesizer;

/// Airspy USB vendor ID.
pub const AIRSPY_VID: u16 = 0x1d50;
/// Airspy USB product ID.
pub const AIRSPY_PID: u16 = 0x60a1;

/// Airspy vendor command IDs (protocol facts).
pub mod cmd {
    /// Receiver mode (bitfield: off, RX, etc.).
    pub const RECEIVER_MODE: u8 = 1;
    /// Read the board ID (health probe).
    pub const BOARD_ID_READ: u8 = 9;
    /// Read the firmware version string (`AirSpy MINI …` / `AirSpy NOS …` on
    /// current firmware). Every Airspy One reports board id 0, so this string
    /// is the only wire-level way to tell an R2 from a Mini.
    pub const VERSION_STRING_READ: u8 = 10;
    /// Set the sample rate (param = index into the rates table).
    pub const SET_SAMPLERATE: u8 = 12;
    /// Set the LO frequency (data payload = Hz as u32 LE).
    pub const SET_FREQ: u8 = 13;
    /// Set LNA gain (0..=14).
    pub const SET_LNA_GAIN: u8 = 14;
    /// Set mixer gain (0..=15).
    pub const SET_MIXER_GAIN: u8 = 15;
    /// Set VGA gain (0..=15).
    pub const SET_VGA_GAIN: u8 = 16;
    /// Enable/disable LNA AGC.
    pub const SET_LNA_AGC: u8 = 17;
    /// Enable/disable mixer AGC.
    pub const SET_MIXER_AGC: u8 = 18;
    /// `AIRSPY_SET_RF_BIAS_CMD` in the reference command enum. **Not usable for
    /// bias control**: the reference host library (libairspy) never sends this
    /// request — its `airspy_set_rf_bias` is a GPIO write (see
    /// [`rf_bias_request`]) — and the firmware ignores it. Kept only because it
    /// occupies slot 20 of the command enum.
    pub const SET_RF_BIAS: u8 = 20;
    /// Write an Airspy GPIO: `wValue` = 0/1 (pin level), `wIndex` =
    /// `(port << 5) | pin`. The RF bias tee is GPIO port 1, pin 13.
    pub const GPIO_WRITE: u8 = 21;
    /// Query supported sample rates (returns a u32 LE array).
    pub const GET_SAMPLERATES: u8 = 25;
    /// Enable/disable packed (12-bit) sample mode.
    pub const SET_PACKING: u8 = 26;
}

/// Receiver-mode bitfield values.
pub mod rx_mode {
    /// RX off.
    pub const OFF: u8 = 0;
    /// RX on.
    pub const RX: u8 = 1;
}

const MAX_SAMPLE_RATE_COUNT: u32 = 256;
/// Legacy-firmware fallback rate table for R2-class boards (libairspy's
/// documented two-rate default), in firmware index order.
const R2_FALLBACK_SAMPLE_RATES: &[u32] = &[10_000_000, 2_500_000];
/// Legacy-firmware fallback rate table for the Airspy Mini (6/3 MSPS), in
/// firmware index order. An old Mini cannot do the R2 table's 10 MSPS index 0.
const MINI_FALLBACK_SAMPLE_RATES: &[u32] = &[6_000_000, 3_000_000];
/// Bytes requested from `VERSION_STRING_READ` (the reply may be shorter).
const VERSION_STRING_LEN: usize = 128;

/// The bulk-IN endpoint Airspy streams on.
pub const BULK_ENDPOINT: u8 = 0x81;

/// An opened Airspy device.
pub struct Airspy {
    info: DeviceInfo,
    transport: Box<dyn Transport>,
    supported_sample_rates: Vec<u32>,
    sample_rate: u32,
    center_freq: u64,
    upconverter: Option<Upconverter>,
    iq_synth: IqSynthesizer,
}

impl Airspy {
    /// Construct from an opened transport. Performs the BOARD_ID_READ health
    /// probe and forces unpacked mode.
    ///
    /// # Errors
    ///
    /// - [`SdrError::Transport`] if the device does not respond on the control
    ///   endpoint (the canonical "re-plug" failure mode SondeFox documented).
    pub fn open(desc: &DeviceDescriptor, transport: Box<dyn Transport>) -> Result<Self, SdrError> {
        let mut sdr = Self {
            info: DeviceInfo {
                vendor_id: desc.vendor_id,
                product_id: desc.product_id,
                vendor_name: desc.vendor_name.clone().unwrap_or_else(|| "Airspy".into()),
                product_name: desc.product_name.clone().unwrap_or_else(|| "Airspy".into()),
                serial: desc.serial.clone().unwrap_or_default(),
                kind: DeviceKind::Airspy,
                tuner: None,
            },
            transport,
            supported_sample_rates: R2_FALLBACK_SAMPLE_RATES.to_vec(),
            sample_rate: 10_000_000,
            center_freq: 100_000_000,
            upconverter: None,
            iq_synth: IqSynthesizer::new(),
        };

        // Health probe: read BOARD_ID. rc < 0 ⇒ re-plug.
        let req = ControlRequest::vendor_in(cmd::BOARD_ID_READ, 0, 0, 1);
        let board_id = sdr.transport.control_in(&req)?;
        if board_id.is_empty() {
            return Err(SdrError::Transport(
                "Airspy not responding on the control endpoint — re-plug".into(),
            ));
        }
        // Firmware reports a model-specific, index-ordered rate table. Older
        // firmware may not implement the query, in which case fall back to a
        // model-appropriate legacy table (index order preserved).
        match sdr.query_sample_rates() {
            Ok(rates) => sdr.supported_sample_rates = rates,
            Err(_) => sdr.supported_sample_rates = sdr.fallback_sample_rates_for_model(),
        }

        // Force unpacked mode (raw 12-bit samples in 16-bit containers).
        // Older firmware does not acknowledge SET_PACKING but also predates
        // packed mode, so it already streams unpacked: failing open here
        // would brick devices that work fine — warn and continue instead
        // (the field-proven SondeFox behavior).
        if let Err(error) = sdr.vendor_set(cmd::SET_PACKING, 0) {
            tracing::warn!(%error, "Airspy SET_PACKING(0) not acknowledged (older firmware?); continuing unpacked");
        }
        Ok(sdr)
    }

    /// Pick the legacy fallback rate table for this device's model when the
    /// firmware cannot enumerate its rates.
    ///
    /// `BOARD_ID_READ` cannot tell the models apart — every Airspy One
    /// reports board id 0 ("AIRSPY") — so the model is identified from the
    /// `VERSION_STRING_READ` reply (`AirSpy MINI …` vs `AirSpy NOS …`). If
    /// that read also fails, assume the R2-class table, matching libairspy's
    /// documented fallback.
    fn fallback_sample_rates_for_model(&mut self) -> Vec<u32> {
        let req = ControlRequest::vendor_in(cmd::VERSION_STRING_READ, 0, 0, VERSION_STRING_LEN);
        match self.transport.control_in(&req) {
            Ok(reply) => {
                let version = String::from_utf8_lossy(&reply);
                if version.to_ascii_lowercase().contains("mini") {
                    MINI_FALLBACK_SAMPLE_RATES.to_vec()
                } else {
                    R2_FALLBACK_SAMPLE_RATES.to_vec()
                }
            }
            Err(_) => R2_FALLBACK_SAMPLE_RATES.to_vec(),
        }
    }

    fn query_sample_rates(&mut self) -> Result<Vec<u32>, SdrError> {
        let count_reply =
            self.transport
                .control_in(&ControlRequest::vendor_in(cmd::GET_SAMPLERATES, 0, 0, 4))?;
        let count_bytes: [u8; 4] = count_reply.try_into().map_err(|reply: Vec<u8>| {
            SdrError::Transport(format!(
                "Airspy sample-rate count reply is {} bytes, expected 4",
                reply.len()
            ))
        })?;
        let count = u32::from_le_bytes(count_bytes);
        if count == 0 || count > MAX_SAMPLE_RATE_COUNT {
            return Err(SdrError::Transport(format!(
                "Airspy reported invalid sample-rate count {count}"
            )));
        }
        let count_u16 = u16::try_from(count)
            .map_err(|_| SdrError::Transport("Airspy sample-rate count overflow".into()))?;
        let byte_len = usize::try_from(count)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| SdrError::Transport("Airspy sample-rate reply overflow".into()))?;
        let reply = self.transport.control_in(&ControlRequest::vendor_in(
            cmd::GET_SAMPLERATES,
            0,
            count_u16,
            byte_len,
        ))?;
        if reply.len() != byte_len {
            return Err(SdrError::Transport(format!(
                "Airspy sample-rate table is {} bytes, expected {byte_len}",
                reply.len()
            )));
        }
        let rates: Vec<u32> = reply
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        if rates.contains(&0) {
            return Err(SdrError::Transport(
                "Airspy reported a zero sample rate".into(),
            ));
        }
        Ok(rates)
    }

    /// Issue a gain/rate vendor-IN command: DIR_IN, wValue=0, param in wIndex,
    /// 1-byte ack read-back.
    fn vendor_set(&mut self, command: u8, param: u32) -> Result<u8, SdrError> {
        let req = vendor_set_request(command, param)?;
        let ack = self.transport.control_in(&req)?;
        ack.first()
            .copied()
            .ok_or_else(|| SdrError::Transport(format!("airspy cmd {command:#04x}: empty ack")))
    }

    /// Apply the configured upconverter offset (if any). Single point.
    fn sdr_freq(&self, true_rf_hz: u64) -> u64 {
        match self.upconverter {
            Some(up) => up.translate(true_rf_hz),
            None => true_rf_hz,
        }
    }
}

fn vendor_set_request(command: u8, param: u32) -> Result<ControlRequest, SdrError> {
    let param = u16::try_from(param).map_err(|_| {
        SdrError::InvalidParameter(format!("Airspy command parameter {param} exceeds u16"))
    })?;
    Ok(ControlRequest::vendor_in(command, 0, param, 1))
}

fn receiver_mode_request(mode: u8) -> ControlRequest {
    ControlRequest::vendor_out(cmd::RECEIVER_MODE, u16::from(mode), 0, Vec::new())
}

fn frequency_request(hz: u64) -> Result<ControlRequest, SdrError> {
    let hz = u32::try_from(hz)
        .map_err(|_| SdrError::InvalidParameter(format!("Airspy frequency {hz} Hz exceeds u32")))?;
    Ok(ControlRequest::vendor_out(
        cmd::SET_FREQ,
        0,
        0,
        hz.to_le_bytes().to_vec(),
    ))
}

/// GPIO coordinates of the 4.5 V RF bias tee, from libairspy's
/// `airspy_set_rf_bias`, which is `airspy_gpio_write(GPIO_PORT1, GPIO_PIN13, v)`.
const BIAS_GPIO_PORT: u16 = 1;
const BIAS_GPIO_PIN: u16 = 13;

fn rf_bias_request(on: bool) -> ControlRequest {
    // libairspy's airspy_set_rf_bias() drives the bias tee as a plain GPIO
    // write: vendor DIR_OUT, bRequest = GPIO_WRITE(21), wValue = 0/1 (pin
    // level), wIndex = (port << 5) | pin = (1 << 5) | 13 = 0x2D, no data.
    // Sending SET_RF_BIAS_CMD(20) instead is silently ignored by the firmware
    // (verified on hardware: the transfer succeeds, the bias light stays off).
    let port_pin = (BIAS_GPIO_PORT << 5) | BIAS_GPIO_PIN;
    ControlRequest::vendor_out(cmd::GPIO_WRITE, u16::from(on), port_pin, Vec::new())
}

impl SdrDevice for Airspy {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn set_sample_rate(&mut self, hz: u32) -> Result<u32, SdrError> {
        // The command takes an index into the table queried from this device.
        // Pick the closest advertised rate.
        let (idx, actual) = self
            .supported_sample_rates
            .iter()
            .enumerate()
            .min_by_key(|(_, &r)| (i64::from(r) - i64::from(hz)).abs())
            .map(|(idx, &rate)| (idx, rate))
            .ok_or_else(|| SdrError::Unsupported("no Airspy sample rates".into()))?;
        self.vendor_set(cmd::SET_SAMPLERATE, idx as u32)?;
        self.sample_rate = actual;
        Ok(actual)
    }

    fn supported_sample_rates(&self) -> Vec<u32> {
        // The firmware-queried, index-ordered table captured at open time
        // (or the model-appropriate legacy fallback on old firmware).
        self.supported_sample_rates.clone()
    }

    fn set_frequency(&mut self, hz: u64) -> Result<(), SdrError> {
        let sdr_hz = self.sdr_freq(hz);
        let req = frequency_request(sdr_hz)?;
        self.transport.control_out(&req)?;
        self.center_freq = sdr_hz;
        Ok(())
    }

    fn set_bandwidth(&mut self, _hz: u32) -> Result<(), SdrError> {
        // Airspy has no programmable IF bandwidth (the R2/Mini filters are fixed).
        Err(SdrError::Unsupported(
            "Airspy has no programmable IF bandwidth".into(),
        ))
    }

    fn set_gain(&mut self, req: GainRequest) -> Result<(), SdrError> {
        match req {
            GainRequest::Overall(_) => Err(SdrError::Unsupported(
                "Airspy gain is per-stage; use GainRequest::PerStage for LNA/MIXER/VGA".into(),
            )),
            GainRequest::PerStage { name, tenths_db } => {
                let (cmd_id, maximum) = match name {
                    "LNA" => (cmd::SET_LNA_GAIN, 14),
                    "MIXER" => (cmd::SET_MIXER_GAIN, 15),
                    "VGA" => (cmd::SET_VGA_GAIN, 15),
                    other => {
                        return Err(SdrError::InvalidParameter(format!(
                            "unknown Airspy gain stage '{other}'"
                        )));
                    }
                };
                // Airspy gain params are 0..=14/15 (unit steps), not tenths-dB.
                if tenths_db < 0 || tenths_db % 10 != 0 || tenths_db > maximum * 10 {
                    return Err(SdrError::InvalidParameter(format!(
                        "Airspy {name} gain must be an integer dB value in 0..={maximum}"
                    )));
                }
                let value = u32::try_from(tenths_db / 10).map_err(|_| {
                    SdrError::InvalidParameter(format!("invalid Airspy {name} gain"))
                })?;
                self.vendor_set(cmd_id, value)?;
                Ok(())
            }
        }
    }

    fn set_gain_mode(&mut self, mode: GainMode) -> Result<(), SdrError> {
        let on = matches!(mode, GainMode::Auto) as u32;
        // Device-wide switch: enable/disable both hardware AGC loops together.
        // Use `set_stage_agc` to drive LNA and mixer AGC independently.
        self.vendor_set(cmd::SET_LNA_AGC, on)?;
        self.vendor_set(cmd::SET_MIXER_AGC, on)?;
        Ok(())
    }

    fn set_stage_agc(&mut self, stage: GainStageId, on: bool) -> Result<(), SdrError> {
        // The Airspy front end has independent AGC loops for the LNA and the
        // mixer (separate vendor commands); the VGA has none.
        let cmd_id = match stage {
            GainStageId::Lna => cmd::SET_LNA_AGC,
            GainStageId::Mixer => cmd::SET_MIXER_AGC,
            GainStageId::Vga => {
                return Err(SdrError::Unsupported(
                    "Airspy VGA has no AGC loop; only LNA and MIXER AGC exist".into(),
                ));
            }
        };
        self.vendor_set(cmd_id, u32::from(on))?;
        Ok(())
    }

    fn gains(&self) -> &[GainStep] {
        // Airspy accepts integer stage indices 0..=14/15. The public gain
        // model represents those accepted values in tenths, matching
        // `set_gain`'s validation and conversion to the firmware index.
        &AIRSPY_GAINS
    }

    fn set_bias_tee(&mut self, on: bool) -> Result<(), SdrError> {
        let req = rf_bias_request(on);
        self.transport.control_out(&req)?;
        Ok(())
    }

    fn set_agc(&mut self, on: bool) -> Result<(), SdrError> {
        self.set_gain_mode(if on { GainMode::Auto } else { GainMode::Manual })
    }

    fn set_frequency_correction_ppm(&mut self, _ppm: f64) -> Result<(), SdrError> {
        // Airspy R2/Mini has no software PPM register (the crystal is TCXO);
        // correction is applied by the consumer on the sample stream.
        Err(SdrError::Unsupported(
            "Airspy R2/Mini has no PPM register (TCXO); correct on the sample stream".into(),
        ))
    }

    fn set_upconverter(&mut self, up: Option<Upconverter>) -> Result<(), SdrError> {
        self.upconverter = up;
        Ok(())
    }

    fn start_stream(&mut self, cfg: StreamConfig) -> Result<sdr_fox_core::StreamHandle, SdrError> {
        // Reset a possibly stale firmware stream before entering RX mode.
        self.transport
            .control_out(&receiver_mode_request(rx_mode::OFF))?;
        self.transport
            .control_out(&receiver_mode_request(rx_mode::RX))?;

        // Start the bulk stream. The Airspy delivers 12-bit samples in 16-bit LE
        // containers; for a Mini they're REAL at 2× the IQ rate, so the
        // IqSynthesizer decimates by 2 and produces complex samples.
        let endpoint = BULK_ENDPOINT;
        let receiver_control = self.transport.boxed_clone();
        let handle = match self.transport.start_bulk_stream(
            endpoint,
            cfg.buffer_count,
            cfg.buffer_size,
            cfg.queue_depth,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = self
                    .transport
                    .control_out(&receiver_mode_request(rx_mode::OFF));
                return Err(error);
            }
        };
        // Every Airspy One model sends a 2× real 12-bit wire stream. Convert it
        // to complex IQ independently of optional USB product strings.
        let fmt = cfg.format;
        Ok(Box::new(AirspyStream {
            inner: handle,
            synth: std::mem::take(&mut self.iq_synth),
            output_format: fmt,
            receiver: Arc::new(AirspyReceiverControl {
                transport: Mutex::new(receiver_control),
                stopped: AtomicBool::new(false),
            }),
        }))
    }
}

/// Legacy firmware fallback sample-rate table (Hz) for R2-class boards, in
/// firmware index order. Modern devices are queried at open time and may
/// advertise a different list; an old Mini falls back to 6/3 MSPS instead
/// (selected by the firmware version string, since every Airspy One reports
/// the same board id). Prefer [`SdrDevice::supported_sample_rates`] on an
/// opened [`Airspy`], which reports what the actual device advertises.
#[must_use]
pub fn supported_sample_rates() -> Vec<u32> {
    R2_FALLBACK_SAMPLE_RATES.to_vec()
}

/// Wrapper sink that runs Airspy raw bytes through the [`IqSynthesizer`]
/// (converting Airspy's 2× real wire stream) before delivery.
pub struct AirspyStream {
    inner: sdr_fox_core::StreamHandle,
    synth: IqSynthesizer,
    output_format: sdr_fox_core::IqFormat,
    receiver: Arc<AirspyReceiverControl>,
}

impl AirspyStream {
    fn synthesize_block(&mut self, block: sdr_fox_core::IqBlock) -> sdr_fox_core::IqBlock {
        let dropped = block.dropped / 2;
        let sequence = block.sequence;
        let timestamp = block.timestamp;
        let (samples, clips, raw_samples) = match block.samples {
            sdr_fox_core::IqSamples::Cu8(bytes) => {
                let samples = match self.output_format {
                    sdr_fox_core::IqFormat::Cu8 => {
                        sdr_fox_core::IqSamples::Cu8(self.synth.synthesize_mini(&bytes))
                    }
                    sdr_fox_core::IqFormat::Cs8 => {
                        sdr_fox_core::IqSamples::Cs8(self.synth.synthesize_mini_cs8(&bytes))
                    }
                    sdr_fox_core::IqFormat::Cs16 => {
                        sdr_fox_core::IqSamples::Cs16(self.synth.synthesize_mini_cs16(&bytes))
                    }
                    sdr_fox_core::IqFormat::Cf32 => {
                        sdr_fox_core::IqSamples::Cf32(self.synth.synthesize_mini_cf32(&bytes))
                    }
                };
                // Raw ADC-domain telemetry for this block, counted by the
                // synthesizer during the container walk (the raw codes are
                // consumed here, before delivery, so this is the only place
                // a faithful clip counter can live).
                (
                    samples,
                    self.synth.last_clips(),
                    self.synth.last_raw_samples(),
                )
            }
            other => (other, block.clips, block.raw_samples),
        };
        sdr_fox_core::IqBlock {
            samples,
            dropped,
            sequence,
            timestamp,
            clips,
            raw_samples,
        }
    }
}

struct AirspyReceiverControl {
    transport: Mutex<Box<dyn Transport>>,
    stopped: AtomicBool,
}

impl AirspyReceiverControl {
    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            if let Ok(mut control) = self.transport.lock() {
                let _ = control.control_out(&receiver_mode_request(rx_mode::OFF));
            }
        }
    }
}

impl sdr_fox_core::StreamSink for AirspyStream {
    fn recv(&mut self) -> Option<Result<sdr_fox_core::IqBlock, SdrError>> {
        let result = self.inner.recv()?;
        Some(result.map(|block| self.synthesize_block(block)))
    }

    fn recv_deadline(
        &mut self,
        deadline: std::time::Instant,
    ) -> Option<Result<sdr_fox_core::IqBlock, SdrError>> {
        let result = self.inner.recv_deadline(deadline)?;
        Some(result.map(|block| self.synthesize_block(block)))
    }

    fn stop(&self) {
        self.inner.stop();
        self.receiver.stop();
    }

    fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
        let inner = self.inner.stop_handle();
        let receiver = Arc::clone(&self.receiver);
        sdr_fox_core::sample::StreamStopHandle::new(move || {
            inner.stop();
            receiver.stop();
        })
    }
}

impl Drop for AirspyStream {
    fn drop(&mut self) {
        self.inner.stop();
        self.receiver.stop();
    }
}

/// Backend matching the Airspy VID:PID.
pub struct AirspyBackend;

impl sdr_fox_core::SdrBackend for AirspyBackend {
    fn matches(&self, d: &DeviceDescriptor) -> bool {
        d.vendor_id == AIRSPY_VID && d.product_id == AIRSPY_PID
    }

    fn name(&self) -> &'static str {
        "airspy"
    }

    fn open(
        &self,
        d: &DeviceDescriptor,
        transport: Box<dyn Transport>,
    ) -> Result<Box<dyn SdrDevice>, SdrError> {
        let device = Airspy::open(d, transport)?;
        Ok(Box::new(device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{IqBlock, IqFormat, IqSamples, SdrBackend, StreamSink, TransferDirection};
    use sdr_fox_transport::{MockTransport, ScriptedReply};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    struct CountingControl {
        receiver_off_requests: Arc<AtomicUsize>,
    }

    impl Transport for CountingControl {
        fn control_in(&mut self, request: &ControlRequest) -> Result<Vec<u8>, SdrError> {
            Ok(vec![0; request.data.len()])
        }

        fn control_out(&mut self, request: &ControlRequest) -> Result<usize, SdrError> {
            if request.request == cmd::RECEIVER_MODE && request.value == u16::from(rx_mode::OFF) {
                self.receiver_off_requests.fetch_add(1, Ordering::AcqRel);
            }
            Ok(request.data.len())
        }

        fn bulk_read(
            &mut self,
            _endpoint: u8,
            _len: usize,
            _timeout_ms: u32,
        ) -> Result<Vec<u8>, SdrError> {
            Err(SdrError::Unsupported("test control transport".into()))
        }

        fn start_bulk_stream(
            &mut self,
            _endpoint: u8,
            _buffer_count: usize,
            _buffer_size: usize,
            _queue_depth: usize,
        ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
            Err(SdrError::Unsupported("test control transport".into()))
        }

        fn boxed_clone(&self) -> Box<dyn Transport> {
            Box::new(self.clone())
        }
    }

    fn in_reply(request: u8, value: u16, index: u16, payload: Vec<u8>) -> ScriptedReply {
        ScriptedReply {
            direction: TransferDirection::In,
            request: Some(request),
            value: Some(value),
            index: Some(index),
            payload,
        }
    }

    fn airspy_desc() -> DeviceDescriptor {
        DeviceDescriptor {
            vendor_id: AIRSPY_VID,
            product_id: AIRSPY_PID,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::Airspy,
        }
    }

    /// A `VERSION_STRING_READ` reply, zero-padded to the requested length as
    /// the firmware pads its version buffer.
    fn version_reply(version: &str) -> ScriptedReply {
        let mut payload = version.as_bytes().to_vec();
        payload.resize(VERSION_STRING_LEN, 0);
        in_reply(cmd::VERSION_STRING_READ, 0, 0, payload)
    }

    /// One recorded control transfer: `(direction, request, value, index)`.
    type RecordedTransfer = (TransferDirection, u8, u16, u16);

    /// Control-only transport that acks every request with zeros and records
    /// `(direction, request, value, index)` so tests can assert exactly which
    /// vendor commands a driver call produced.
    #[derive(Clone, Default)]
    struct RecordingControl {
        requests: Arc<Mutex<Vec<RecordedTransfer>>>,
    }

    impl Transport for RecordingControl {
        fn control_in(&mut self, request: &ControlRequest) -> Result<Vec<u8>, SdrError> {
            self.requests.lock().unwrap().push((
                TransferDirection::In,
                request.request,
                request.value,
                request.index,
            ));
            Ok(vec![0; request.data.len()])
        }

        fn control_out(&mut self, request: &ControlRequest) -> Result<usize, SdrError> {
            self.requests.lock().unwrap().push((
                TransferDirection::Out,
                request.request,
                request.value,
                request.index,
            ));
            Ok(request.data.len())
        }

        fn bulk_read(
            &mut self,
            _endpoint: u8,
            _len: usize,
            _timeout_ms: u32,
        ) -> Result<Vec<u8>, SdrError> {
            Err(SdrError::Unsupported("test control transport".into()))
        }

        fn start_bulk_stream(
            &mut self,
            _endpoint: u8,
            _buffer_count: usize,
            _buffer_size: usize,
            _queue_depth: usize,
        ) -> Result<sdr_fox_core::StreamHandle, SdrError> {
            Err(SdrError::Unsupported("test control transport".into()))
        }

        fn boxed_clone(&self) -> Box<dyn Transport> {
            Box::new(self.clone())
        }
    }

    struct OneBlock(Option<IqBlock>);

    impl StreamSink for OneBlock {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            self.0.take().map(Ok)
        }

        fn recv_deadline(
            &mut self,
            _deadline: std::time::Instant,
        ) -> Option<Result<IqBlock, SdrError>> {
            self.recv()
        }

        fn stop(&self) {}

        fn stop_handle(&self) -> sdr_fox_core::sample::StreamStopHandle {
            sdr_fox_core::sample::StreamStopHandle::new(|| {})
        }
    }

    #[test]
    fn backend_matches_airspy_vid_pid() {
        let b = AirspyBackend;
        assert!(b.matches(&DeviceDescriptor {
            vendor_id: AIRSPY_VID,
            product_id: AIRSPY_PID,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::Airspy,
        }));
        assert!(!b.matches(&DeviceDescriptor {
            vendor_id: 0x0bda,
            product_id: 0x2838,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::RtlSdr,
        }));
    }

    #[test]
    fn open_health_probes_and_forces_unpacked() {
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x01]))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                0,
                2u32.to_le_bytes().to_vec(),
            ))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                2,
                [10_000_000u32.to_le_bytes(), 2_500_000u32.to_le_bytes()].concat(),
            ))
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![0x00]));
        let desc = DeviceDescriptor {
            vendor_id: AIRSPY_VID,
            product_id: AIRSPY_PID,
            vendor_name: None,
            product_name: Some("Airspy R2".into()),
            serial: None,
            index: 0,
            kind: DeviceKind::Airspy,
        };
        let transport: Box<dyn Transport> = Box::new(mock);
        let sdr = Airspy::open(&desc, transport).unwrap();
        assert_eq!(sdr.supported_sample_rates, vec![10_000_000, 2_500_000]);
    }

    #[test]
    fn open_fails_when_control_endpoint_silent() {
        let mut mock = MockTransport::new();
        // BOARD_ID_READ returns empty ⇒ re-plug error.
        mock.push_reply(ScriptedReply::any_in(vec![]));
        let desc = DeviceDescriptor {
            vendor_id: AIRSPY_VID,
            product_id: AIRSPY_PID,
            vendor_name: None,
            product_name: None,
            serial: None,
            index: 0,
            kind: DeviceKind::Airspy,
        };
        let transport: Box<dyn Transport> = Box::new(mock);
        assert!(Airspy::open(&desc, transport).is_err());
    }

    #[test]
    fn protocol_requests_match_official_control_transfer_fields() {
        let rate = vendor_set_request(cmd::SET_SAMPLERATE, 2).unwrap();
        assert_eq!(rate.direction, TransferDirection::In);
        assert_eq!(
            (rate.request, rate.value, rate.index),
            (cmd::SET_SAMPLERATE, 0, 2)
        );
        assert_eq!(rate.data.len(), 1);

        let packing = vendor_set_request(cmd::SET_PACKING, 0).unwrap();
        assert_eq!(
            (packing.request, packing.value, packing.index),
            (cmd::SET_PACKING, 0, 0)
        );

        let frequency = frequency_request(162_400_000).unwrap();
        assert_eq!(frequency.direction, TransferDirection::Out);
        assert_eq!(
            (frequency.request, frequency.value, frequency.index),
            (cmd::SET_FREQ, 0, 0)
        );
        assert_eq!(frequency.data, 162_400_000u32.to_le_bytes());

        let receiver = receiver_mode_request(rx_mode::RX);
        assert_eq!(
            (receiver.request, receiver.value, receiver.index),
            (cmd::RECEIVER_MODE, 1, 0)
        );
        assert!(receiver.data.is_empty());

        // Bias tee must be a GPIO write (libairspy's airspy_set_rf_bias ==
        // airspy_gpio_write(GPIO_PORT1, GPIO_PIN13, v)): bRequest 21, on/off
        // in wValue, wIndex = (1 << 5) | 13 = 0x2D. Sending SET_RF_BIAS(20)
        // is silently ignored by the firmware — the bias light stays off.
        let bias = rf_bias_request(true);
        assert_eq!(bias.direction, TransferDirection::Out);
        assert_eq!(
            (bias.request, bias.value, bias.index),
            (cmd::GPIO_WRITE, 1, 0x2d)
        );
        assert!(bias.data.is_empty());
        let bias_off = rf_bias_request(false);
        assert_eq!(
            (bias_off.request, bias_off.value, bias_off.index),
            (cmd::GPIO_WRITE, 0, 0x2d)
        );
        assert!(frequency_request(u64::from(u32::MAX) + 1).is_err());
    }

    #[test]
    fn fallback_sample_rates_preserve_firmware_index_order() {
        let rates = supported_sample_rates();
        assert_eq!(rates, vec![10_000_000, 2_500_000]);
    }

    #[test]
    fn advertised_gain_steps_cover_every_accepted_airspy_stage_value() {
        for (stage, maximum) in [("LNA", 14), ("MIXER", 15), ("VGA", 15)] {
            let values = AIRSPY_GAINS
                .iter()
                .filter(|step| step.name == stage)
                .map(|step| step.tenths_db)
                .collect::<Vec<_>>();
            assert_eq!(
                values,
                (0..=maximum).map(|value| value * 10).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn stream_delivers_every_requested_r2_format_without_intermediate_coercion() {
        let raw = vec![0x00, 0x00, 0xff, 0x0f, 0x00, 0x08, 0x01, 0x08];
        for format in [IqFormat::Cu8, IqFormat::Cs8, IqFormat::Cs16, IqFormat::Cf32] {
            let inner: sdr_fox_core::StreamHandle = Box::new(OneBlock(Some(IqBlock {
                samples: IqSamples::Cu8(raw.clone()),
                dropped: 0,
                sequence: 7,
                timestamp: None,
                clips: 0,
                raw_samples: 0,
            })));
            let mut stream = AirspyStream {
                inner,
                synth: IqSynthesizer::new(),
                output_format: format,
                receiver: Arc::new(AirspyReceiverControl {
                    transport: Mutex::new(Box::new(MockTransport::new())),
                    stopped: AtomicBool::new(false),
                }),
            };
            let block = stream.recv().expect("one block").expect("successful block");
            assert_eq!(block.samples.format(), format);
            assert_eq!(block.samples.complex_count(), 2);
            assert_eq!(block.sequence, 7);
        }
    }

    #[test]
    fn dropped_metric_is_converted_from_real_containers_to_complex_samples() {
        let inner: sdr_fox_core::StreamHandle = Box::new(OneBlock(Some(IqBlock {
            samples: IqSamples::Cu8(vec![0; 8]),
            dropped: 10,
            sequence: 1,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        })));
        let mut stream = AirspyStream {
            inner,
            synth: IqSynthesizer::new(),
            output_format: IqFormat::Cu8,
            receiver: Arc::new(AirspyReceiverControl {
                transport: Mutex::new(Box::new(MockTransport::new())),
                stopped: AtomicBool::new(false),
            }),
        };
        assert_eq!(stream.recv().unwrap().unwrap().dropped, 5);
    }

    #[test]
    fn device_reports_the_firmware_queried_rate_table() {
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x00]))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                0,
                3u32.to_le_bytes().to_vec(),
            ))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                3,
                [
                    6_000_000u32.to_le_bytes(),
                    3_000_000u32.to_le_bytes(),
                    10_000_000u32.to_le_bytes(),
                ]
                .concat(),
            ))
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![0x00]));
        let sdr = Airspy::open(&airspy_desc(), Box::new(mock)).unwrap();
        // The trait accessor exposes the firmware table (not the static
        // fallback), in firmware index order.
        assert_eq!(
            SdrDevice::supported_sample_rates(&sdr),
            vec![6_000_000, 3_000_000, 10_000_000]
        );
    }

    #[test]
    fn per_stage_agc_drives_independent_vendor_commands() {
        let control = RecordingControl::default();
        let log = Arc::clone(&control.requests);
        let mut sdr = Airspy::open(&airspy_desc(), Box::new(control)).unwrap();
        log.lock().unwrap().clear();

        sdr.set_stage_agc(GainStageId::Lna, true).unwrap();
        sdr.set_stage_agc(GainStageId::Mixer, false).unwrap();
        let vga = sdr.set_stage_agc(GainStageId::Vga, true).unwrap_err();
        assert!(matches!(vga, SdrError::Unsupported(_)));

        // Exactly one command per accepted call, each touching only its own
        // stage (vendor-IN, on/off in wIndex), and none for the VGA refusal.
        assert_eq!(
            log.lock().unwrap().clone(),
            vec![
                (TransferDirection::In, cmd::SET_LNA_AGC, 0, 1),
                (TransferDirection::In, cmd::SET_MIXER_AGC, 0, 0),
            ]
        );
    }

    #[test]
    fn open_survives_unacknowledged_set_packing() {
        // Older firmware never acks SET_PACKING; open must warn and continue
        // (M2), not abort — those devices already stream unpacked.
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x00]))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                0,
                2u32.to_le_bytes().to_vec(),
            ))
            .push_reply(in_reply(
                cmd::GET_SAMPLERATES,
                0,
                2,
                [10_000_000u32.to_le_bytes(), 2_500_000u32.to_le_bytes()].concat(),
            ))
            // Empty payload ⇒ the SET_PACKING control transfer fails.
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![]));
        let sdr = Airspy::open(&airspy_desc(), Box::new(mock))
            .expect("SET_PACKING failure must not abort open");
        assert_eq!(
            SdrDevice::supported_sample_rates(&sdr),
            vec![10_000_000, 2_500_000]
        );
    }

    #[test]
    fn old_firmware_mini_falls_back_to_the_mini_rate_table() {
        // GET_SAMPLERATES unimplemented (short reply ⇒ error): the fallback
        // must be model-appropriate. Board id cannot distinguish the models
        // (every Airspy One reports 0), so the version string decides (M13).
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x00]))
            .push_reply(in_reply(cmd::GET_SAMPLERATES, 0, 0, vec![]))
            .push_reply(version_reply("AirSpy MINI v1.0.0-rc4 2015-01-01"))
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![0x00]));
        let sdr = Airspy::open(&airspy_desc(), Box::new(mock)).unwrap();
        assert_eq!(
            SdrDevice::supported_sample_rates(&sdr),
            vec![6_000_000, 3_000_000]
        );
    }

    #[test]
    fn old_firmware_r2_keeps_the_r2_rate_table() {
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x00]))
            .push_reply(in_reply(cmd::GET_SAMPLERATES, 0, 0, vec![]))
            .push_reply(version_reply("AirSpy NOS v1.0.0-rc4 2015-01-01"))
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![0x00]));
        let sdr = Airspy::open(&airspy_desc(), Box::new(mock)).unwrap();
        assert_eq!(
            SdrDevice::supported_sample_rates(&sdr),
            vec![10_000_000, 2_500_000]
        );
    }

    #[test]
    fn fallback_assumes_r2_when_the_version_read_also_fails() {
        let mut mock = MockTransport::new();
        mock.strict(true)
            .push_reply(in_reply(cmd::BOARD_ID_READ, 0, 0, vec![0x00]))
            .push_reply(in_reply(cmd::GET_SAMPLERATES, 0, 0, vec![]))
            .push_reply(in_reply(cmd::VERSION_STRING_READ, 0, 0, vec![]))
            .push_reply(in_reply(cmd::SET_PACKING, 0, 0, vec![0x00]));
        let sdr = Airspy::open(&airspy_desc(), Box::new(mock)).unwrap();
        assert_eq!(
            SdrDevice::supported_sample_rates(&sdr),
            vec![10_000_000, 2_500_000]
        );
    }

    #[test]
    fn stream_blocks_carry_raw_domain_clip_telemetry() {
        // Containers: low rail, high rail, mid-scale, one count inside the
        // rail — 2 clips of 4 raw samples, reported in the RAW ADC domain
        // (pre-decimation, so raw_samples exceeds the complex count).
        let containers: [u16; 4] = [0, 4095, 2048, 5];
        let raw: Vec<u8> = containers.iter().flat_map(|v| v.to_le_bytes()).collect();
        let inner: sdr_fox_core::StreamHandle = Box::new(OneBlock(Some(IqBlock {
            samples: IqSamples::Cu8(raw),
            dropped: 0,
            sequence: 3,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        })));
        let mut stream = AirspyStream {
            inner,
            synth: IqSynthesizer::new(),
            output_format: IqFormat::Cf32,
            receiver: Arc::new(AirspyReceiverControl {
                transport: Mutex::new(Box::new(MockTransport::new())),
                stopped: AtomicBool::new(false),
            }),
        };
        let block = stream.recv().expect("one block").expect("success");
        assert_eq!(block.raw_samples, 4);
        assert_eq!(block.clips, 2);
        assert!(block.raw_samples > block.samples.complex_count() as u64);
    }

    #[test]
    fn stop_handle_sends_receiver_off_exactly_once() {
        let requests = Arc::new(AtomicUsize::new(0));
        let stream = AirspyStream {
            inner: Box::new(OneBlock(None)),
            synth: IqSynthesizer::new(),
            output_format: IqFormat::Cu8,
            receiver: Arc::new(AirspyReceiverControl {
                transport: Mutex::new(Box::new(CountingControl {
                    receiver_off_requests: Arc::clone(&requests),
                })),
                stopped: AtomicBool::new(false),
            }),
        };
        let stop = stream.stop_handle();
        stop.stop();
        stop.stop();
        assert_eq!(requests.load(Ordering::Acquire), 1);
    }
}
