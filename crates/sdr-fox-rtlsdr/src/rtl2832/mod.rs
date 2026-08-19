//! RTL2832U demodulator control plane.
//!
//! Speaks the RTL2832U register protocol with the firmware-mandated encoding.
//! There is no USB "command ID byte"; every control transfer uses
//! `bRequest = 0` (vendor, device recipient) and encodes the command in
//! `wValue` (addr) and `wIndex` (block | flags):
//!
//! - `read_array(block, addr, buf)`: `index = block << 8`
//! - `write_array(block, addr, buf)`: `index = (block << 8) | 0x10` — the
//!   `0x10` bit is the write-enable flag
//! - `demod_read_reg(page, addr)`: `addr = (addr << 8) | 0x20` — the `0x20`
//!   bit selects demod page addressing; `index = page`
//! - `demod_write_reg(page, addr, val)`: `index = 0x10 | page`, same
//!   `addr | 0x20` transform. After every demod write, a dummy read of page
//!   `0x0a` addr `0x01` acts as a flush/delay.
//!
//! I2C tunneling: `block = IICB (6)`, `addr = i2c_addr`. Tuner register access
//! is wrapped by an I2C repeater: `demod_write_reg(1, 0x01, on ? 0x18 : 0x10)`.

use sdr_fox_core::{
    ControlRequest, ControlType, DeviceRecipient, SdrError, TransferDirection, Transport, Tuner,
    TunerBus, TunerError, TunerKind,
};

pub mod baseband;
pub mod device;
pub mod open;

pub use device::{make_info, RtlSdr, RtlSdrBackend, BULK_ENDPOINT};

/// Construct a tuner instance for the detected kind. The R82xx family and the
/// E4000 have real drivers; the remaining kinds (FC0012/FC0013/FC2580) are
/// rejected until their modules exist.
fn tuner_factory(kind: TunerKind) -> Result<Box<dyn Tuner>, SdrError> {
    match kind {
        TunerKind::R820T | TunerKind::R820T2 | TunerKind::R828D => {
            Ok(Box::new(crate::tuners::r82xx::R82xx::new(kind)))
        }
        TunerKind::E4000 => Ok(Box::new(crate::tuners::e4000::E4000::new())),
        other => Err(SdrError::Unsupported(format!(
            "tuner {other:?} not yet implemented"
        ))),
    }
}

/// RTL2832U register blocks (`wIndex >> 8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Block {
    /// OFDM/DVB demodulator registers, and the DDC and resampler that the
    /// SDR path reprograms.
    Demod = 0,
    /// USB controller: endpoint sizing, FIFO control, and bulk transfer setup.
    Usb = 1,
    /// Chip-level system registers, including the GPIO bank used for bias-T
    /// and tuner reset.
    Sys = 2,
    /// Direct tuner address space (unused on this path — R82xx is reached
    /// through the I2C repeater at [`Block::Iic`]).
    Tuner = 3,
    /// Factory EEPROM image, holding vendor/product IDs and the serial string.
    Rom = 4,
    /// Infrared receiver peripheral, present on the die but unused here.
    Ir = 5,
    /// I2C bridge; every tuner register access is tunnelled through this.
    Iic = 6,
}

impl Block {
    /// The block value packed into the high byte of `wIndex`.
    const fn as_index_high(self) -> u16 {
        (self as u8 as u16) << 8
    }
}

/// The `0x10` bit set in `wIndex` for writes.
const WRITE_FLAG: u16 = 0x10;
/// The `0x20` bit set in the demod address to select page addressing.
const DEMOD_PAGE_FLAG: u16 = 0x20;
/// USB vendor control request `bRequest` (always 0 for RTL2832U).
const VENDOR_REQUEST: u8 = 0;

/// Build the control request the RTL2832U firmware expects for a register
/// access. `index_high` is the block (<< 8); `addr` is `wValue`; `write` adds
/// the `0x10` flag.
fn reg_request(addr: u16, index_high: u16, write: bool, data: Vec<u8>) -> ControlRequest {
    let mut index = index_high;
    if write {
        index |= WRITE_FLAG;
    }
    if write {
        ControlRequest {
            direction: TransferDirection::Out,
            control_type: ControlType::Vendor,
            recipient: DeviceRecipient::Device,
            request: VENDOR_REQUEST,
            value: addr,
            index,
            data,
        }
    } else {
        ControlRequest::vendor_in(VENDOR_REQUEST, addr, index, data.len())
    }
}

/// Read a register array from `block` at `addr`. Returns the bytes read.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] if the control transfer fails.
pub fn read_array(
    transport: &mut dyn Transport,
    block: Block,
    addr: u16,
    len: usize,
) -> Result<Vec<u8>, SdrError> {
    let req = reg_request(addr, block.as_index_high(), false, vec![0u8; len]);
    transport.control_in(&req)
}

/// Write a byte array to `block` at `addr`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] if the control transfer fails.
pub fn write_array(
    transport: &mut dyn Transport,
    block: Block,
    addr: u16,
    data: &[u8],
) -> Result<(), SdrError> {
    let req = reg_request(addr, block.as_index_high(), true, data.to_vec());
    transport.control_out(&req)?;
    Ok(())
}

/// Read a 1- or 2-byte register from `block` at `addr`. `len` is 1 or 2.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure, or
/// [`SdrError::InvalidParameter`] if `len` is not 1 or 2.
pub fn read_reg(
    transport: &mut dyn Transport,
    block: Block,
    addr: u16,
    len: usize,
) -> Result<u16, SdrError> {
    if !matches!(len, 1 | 2) {
        return Err(SdrError::InvalidParameter(format!(
            "register read len must be 1 or 2, got {len}"
        )));
    }
    let buf = read_array(transport, block, addr, len)?;
    let mut val: u16 = u16::from(buf[0]);
    if len == 2 {
        val |= u16::from(buf[1]) << 8;
    }
    Ok(val)
}

/// Write a 1- or 2-byte register to `block` at `addr`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure, or
/// [`SdrError::InvalidParameter`] if `len` is not 1 or 2.
pub fn write_reg(
    transport: &mut dyn Transport,
    block: Block,
    addr: u16,
    val: u16,
    len: usize,
) -> Result<(), SdrError> {
    if !matches!(len, 1 | 2) {
        return Err(SdrError::InvalidParameter(format!(
            "register write len must be 1 or 2, got {len}"
        )));
    }
    // RTL2832U firmware reads 2-byte control transfers as big-endian (high byte
    // first) — a wire-format fact, and any working driver must emit the same
    // bytes (cf. the Osmocom reference's `rtlsdr_write_reg`). For len==1 only
    // the low byte is transferred. Getting this backwards corrupts
    // e.g. USB_EPA_MAXPKT=0x0002 (sent as 02 00, read as 0x0200 = 512 bytes).
    let mut data = Vec::with_capacity(len);
    if len == 2 {
        data.push((val >> 8) as u8);
        data.push(val as u8);
    } else {
        data.push(val as u8);
    }
    write_array(transport, block, addr, &data)
}

/// Read a demod-page register: `addr = (addr << 8) | 0x20`, `index = page`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure.
pub fn demod_read_reg(
    transport: &mut dyn Transport,
    page: u16,
    addr: u8,
    len: usize,
) -> Result<u16, SdrError> {
    let shifted_addr = (u16::from(addr) << 8) | DEMOD_PAGE_FLAG;
    let req = ControlRequest::vendor_in(VENDOR_REQUEST, shifted_addr, page, len);
    let buf = transport.control_in(&req)?;
    let mut val: u16 = u16::from(buf[0]);
    if len == 2 {
        val |= u16::from(buf[1]) << 8;
    }
    Ok(val)
}

/// Write a demod-page register: `addr = (addr << 8) | 0x20`, `index = 0x10 |
/// page`. After the write, performs a dummy read of page `0x0a` addr `0x01`
/// as a flush/delay (firmware requirement).
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure.
pub fn demod_write_reg(
    transport: &mut dyn Transport,
    page: u16,
    addr: u8,
    val: u16,
    len: usize,
) -> Result<(), SdrError> {
    let shifted_addr = (u16::from(addr) << 8) | DEMOD_PAGE_FLAG;
    // Same big-endian 2-byte payload as `write_reg` — see the note there. The
    // firmware demands the same byte order on the demod-page path (cf. the
    // Osmocom reference's `rtlsdr_demod_write_reg`).
    let mut data = Vec::with_capacity(len);
    if len == 2 {
        data.push((val >> 8) as u8);
        data.push(val as u8);
    } else {
        data.push(val as u8);
    }
    let req = ControlRequest {
        direction: TransferDirection::Out,
        control_type: ControlType::Vendor,
        recipient: DeviceRecipient::Device,
        request: VENDOR_REQUEST,
        value: shifted_addr,
        index: WRITE_FLAG | page,
        data,
    };
    transport.control_out(&req)?;
    // Firmware flush/delay: dummy read of page 0x0a, addr 0x01.
    let _ = demod_read_reg(transport, 0x0a, 0x01, 1);
    Ok(())
}

/// Enable/disable the I2C repeater so the tuner can be reached. The demod
/// register `(1, 0x01)` controls it: `0x18` = on, `0x10` = off.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure.
pub fn set_i2c_repeater(transport: &mut dyn Transport, on: bool) -> Result<(), SdrError> {
    demod_write_reg(transport, 1, 0x01, if on { 0x18 } else { 0x10 }, 1)
}

/// I2C bus address of the R820T/R820T2 (a hardware strapping fact).
pub const R820T_I2C_ADDR: u8 = 0x34;
/// I2C bus address of the R828D, which is strapped differently from the rest
/// of the R82xx family despite sharing its register map.
pub const R828D_I2C_ADDR: u8 = 0x74;

/// The I2C address a tuner of the given kind answers on.
///
/// The match is exhaustive on purpose: adding a [`TunerKind`] variant is a
/// compile error here, so a new tuner's address must be decided in the same
/// change as its driver — it can never silently inherit 0x34 and program a
/// chip that is not there.
pub(crate) fn tuner_i2c_addr(kind: TunerKind) -> u8 {
    match kind {
        TunerKind::R820T | TunerKind::R820T2 => R820T_I2C_ADDR,
        TunerKind::R828D => R828D_I2C_ADDR,
        TunerKind::E4000 => crate::tuners::e4000::E4000_I2C_ADDR,
        // No driver exists for these kinds: [`tuner_factory`] rejects them
        // before any bus is constructed, so this arm is unreachable by
        // construction. It panics rather than defaulting to an address so a
        // future driver is forced to record the real one here.
        TunerKind::Fc0012 | TunerKind::Fc0013 | TunerKind::Fc2580 => {
            unreachable!(
                "tuner {kind:?} has no driver; tuner_factory rejects it before a bus is built"
            )
        }
    }
}

/// Whether a tuner of this kind returns register bytes bit-reversed
/// (MSB<->LSB) when read back over the RTL2832's I2C tunnel.
///
/// The reversal is a behaviour of the **R82xx silicon**, not of the tunnel:
/// R82xx chips shift register read-back bits in the opposite order from
/// their datasheet numbering, so every byte read from one arrives
/// bit-reversed. The osmocom reference corrects for this inside its R820T
/// driver (`r82xx_read`), not in its generic I2C read. Other tuners — the
/// E4000 included — return plain datasheet bytes that must not be touched;
/// reversing them would corrupt every read, starting with the chip-id probe.
pub(crate) const fn tuner_reverses_read_bits(kind: TunerKind) -> bool {
    matches!(
        kind,
        TunerKind::R820T | TunerKind::R820T2 | TunerKind::R828D
    )
}

/// I2C write to a tuner register: tunnel via `block = IICB`, `addr = i2c_addr`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure.
pub fn i2c_write(transport: &mut dyn Transport, i2c_addr: u8, data: &[u8]) -> Result<(), SdrError> {
    write_array(transport, Block::Iic, u16::from(i2c_addr), data)
}

/// I2C read from a tuner: tunnel via `block = IICB`, `addr = i2c_addr`.
///
/// # Errors
///
/// Returns [`SdrError::Transport`] on transfer failure.
pub fn i2c_read(
    transport: &mut dyn Transport,
    i2c_addr: u8,
    len: usize,
) -> Result<Vec<u8>, SdrError> {
    read_array(transport, Block::Iic, u16::from(i2c_addr), len)
}

/// The I2C bus a tuner uses, backed by an RTL2832 transport. Implements
/// [`TunerBus`] so tuners are unit-testable against a `MockTransport`.
pub struct RtlI2cBus<'a> {
    transport: &'a mut dyn Transport,
    /// The detected tuner's I2C address. R828D straps to 0x74 while the rest
    /// of the R82xx family answers at 0x34, so a hardcoded address would make
    /// one variant program a chip that is not there.
    i2c_addr: u8,
    /// Whether read-back bytes must be bit-reversed for this tuner — an
    /// R82xx silicon behaviour; see [`tuner_reverses_read_bits`].
    reverse_reads: bool,
}

impl<'a> RtlI2cBus<'a> {
    /// Wrap a transport as the I2C bus for a tuner of the given kind. The I2C
    /// repeater must already be enabled (callers wrap tuner access in
    /// [`set_i2c_repeater`]).
    ///
    /// Takes the [`TunerKind`] rather than a raw address so no call site can
    /// pair a tuner with the wrong address — the mapping lives once, in
    /// [`tuner_i2c_addr`].
    ///
    /// Crate-internal on purpose. [`tuner_i2c_addr`] panics for kinds that
    /// have no driver, on the grounds that [`tuner_factory`] rejects them
    /// before any bus is built — an invariant this crate can enforce but an
    /// external caller could otherwise break just by naming such a kind.
    /// Keeping the constructor internal makes that `unreachable!` true rather
    /// than merely intended.
    #[must_use]
    pub(crate) fn for_tuner(transport: &'a mut dyn Transport, kind: TunerKind) -> Self {
        Self {
            transport,
            i2c_addr: tuner_i2c_addr(kind),
            reverse_reads: tuner_reverses_read_bits(kind),
        }
    }
}

impl TunerBus for RtlI2cBus<'_> {
    fn i2c_write(&mut self, reg: u8, data: &[u8]) -> Result<(), TunerError> {
        // The RTL2832's I2C tunneling has a max message length of 8 bytes
        // (osmocom: max_i2c_msg_len = 8). Chunk the write: each chunk is
        // [reg + offset, up to 7 data bytes].
        const MAX_I2C_MSG_LEN: usize = 8;
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk_size = (data.len() - offset).min(MAX_I2C_MSG_LEN - 1);
            let mut buf = Vec::with_capacity(chunk_size + 1);
            buf.push(reg.wrapping_add(offset as u8));
            buf.extend_from_slice(&data[offset..offset + chunk_size]);
            i2c_write(self.transport, self.i2c_addr, &buf)
                .map_err(|_| TunerError::I2cTransferFailed { addr: reg })?;
            offset += chunk_size;
        }
        Ok(())
    }

    fn i2c_read(&mut self, reg: u8, len: usize) -> Result<Vec<u8>, TunerError> {
        // Write the register address, then read.
        i2c_write(self.transport, self.i2c_addr, &[reg])
            .map_err(|_| TunerError::I2cTransferFailed { addr: reg })?;
        let mut buf = i2c_read(self.transport, self.i2c_addr, len)
            .map_err(|_| TunerError::I2cTransferFailed { addr: reg })?;
        // R82xx-family chips return register bytes with their bits reversed
        // (MSB<->LSB) relative to the datasheet numbering. That is behaviour
        // of the R82xx silicon, NOT of the RTL2832's I2C tunnel — the osmocom
        // reference applies the correction inside its R820T driver
        // (`r82xx_read`), not in its generic I2C read. The bus corrects it
        // here, per tuner kind, so every R82xx driver read is the logical
        // datasheet byte (e.g. the VCO-lock bit R2[6] tests the right
        // physical bit), while tuners that already speak datasheet bytes —
        // the E4000 — pass through untouched. An unconditional reversal
        // would corrupt every E4000 read, starting with its chip-id probe.
        if self.reverse_reads {
            for byte in &mut buf {
                *byte = byte.reverse_bits();
            }
        }
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_transport::MockTransport;

    /// Assert that a `write_array` produces the firmware-mandated encoding:
    /// index = (block << 8) | 0x10, value = addr, bRequest = 0, vendor/device.
    #[test]
    fn write_array_encodes_block_and_write_flag() {
        let mut mock = MockTransport::new();
        write_array(&mut mock, Block::Sys, 0x3000, &[0xe8]).unwrap();
        let r = &mock.recorded()[0];
        assert_eq!(r.request, 0);
        assert_eq!(r.value, 0x3000);
        assert_eq!(r.index, (Block::Sys as u16) << 8 | 0x10);
        assert_eq!(r.direction, TransferDirection::Out);
        assert_eq!(r.data, vec![0xe8]);
    }

    /// 2-byte register writes must be **big-endian** (high byte first). This is
    /// the load-bearing fix: RTL2832U firmware reads `USB_EPA_MAXPKT` etc. as
    /// big-endian, so 0x0002 must go on the wire as `[0x00, 0x02]` (not the
    /// little-endian `[0x02, 0x00]`, which the firmware would read as 0x0200 =
    /// 512-byte packets). The Osmocom reference's `rtlsdr_write_reg` emits the
    /// same order — a firmware requirement, common to every working driver.
    #[test]
    fn write_reg_two_byte_payload_is_big_endian() {
        let mut mock = MockTransport::new();
        // USB_EPA_MAXPKT = 0x0002.
        write_reg(&mut mock, Block::Usb, 0x2158, 0x0002, 2).unwrap();
        let r = &mock.recorded()[0];
        assert_eq!(r.value, 0x2158);
        assert_eq!(r.index, (Block::Usb as u16) << 8 | 0x10);
        assert_eq!(r.data, vec![0x00, 0x02], "0x0002 must be big-endian: 00 02");
    }

    /// `USB_EPA_CTL` = 0x1002 must serialize as `[0x10, 0x02]`.
    #[test]
    fn write_reg_two_byte_payload_0x1002_is_big_endian() {
        let mut mock = MockTransport::new();
        write_reg(&mut mock, Block::Usb, 0x2148, 0x1002, 2).unwrap();
        let r = &mock.recorded()[0];
        assert_eq!(r.data, vec![0x10, 0x02], "0x1002 must be big-endian: 10 02");
    }

    /// 1-byte register writes carry only the low byte (byte order is moot for
    /// len==1, but lock in the low-byte-only payload — the firmware convention,
    /// cf. the Osmocom reference).
    #[test]
    fn write_reg_one_byte_payload_is_low_byte_only() {
        let mut mock = MockTransport::new();
        write_reg(&mut mock, Block::Usb, 0x2000, 0x09, 1).unwrap();
        let r = &mock.recorded()[0];
        assert_eq!(r.data, vec![0x09]);
    }

    /// `demod_write_reg` with a 2-byte value must also be big-endian. This is
    /// the path used by the sample-rate ratio writes (1, 0x9f)/(1, 0xa1).
    #[test]
    fn demod_write_reg_two_byte_payload_is_big_endian() {
        let mut mock = MockTransport::new();
        // rsamp_ratio high half = 0x1234 -> bytes 12 34.
        demod_write_reg(&mut mock, 1, 0x9f, 0x1234, 2).unwrap();
        // First recorded request is the write; the second is the flush read.
        let w = &mock.recorded()[0];
        assert_eq!(w.direction, TransferDirection::Out);
        assert_eq!(w.value, (0x9fu16 << 8) | 0x20);
        assert_eq!(w.data, vec![0x12, 0x34], "0x1234 must be big-endian: 12 34");
    }

    #[test]
    fn read_array_omits_write_flag() {
        let mut mock = MockTransport::new();
        mock.push_reply(sdr_fox_transport::ScriptedReply::any_in(vec![0x69]));
        let buf = read_array(&mut mock, Block::Iic, 0x34, 1).unwrap();
        assert_eq!(buf, vec![0x69]);
        let r = &mock.recorded()[0];
        assert_eq!(r.index, (Block::Iic as u16) << 8);
        assert_eq!(r.direction, TransferDirection::In);
    }

    #[test]
    fn demod_write_reg_sets_page_and_page_flag_and_flushes() {
        let mut mock = MockTransport::new();
        demod_write_reg(&mut mock, 1, 0x15, 0x00, 1).unwrap();
        // Two recorded requests: the write + the dummy flush read.
        assert_eq!(mock.recorded().len(), 2);
        let write = &mock.recorded()[0];
        assert_eq!(write.value, (0x15u16 << 8) | 0x20);
        assert_eq!(write.index, 0x10 | 1);
        let flush = &mock.recorded()[1];
        assert_eq!(flush.value, (0x01u16 << 8) | 0x20);
        assert_eq!(flush.index, 0x0a);
    }

    #[test]
    fn set_i2c_repeater_writes_correct_value() {
        let mut mock = MockTransport::new();
        set_i2c_repeater(&mut mock, true).unwrap();
        // The write, then the dummy flush read.
        assert_eq!(mock.recorded().len(), 2);
        // demod_write_reg(1, 0x01, 0x18, 1): the data byte is 0x18.
        // The flush read is an IN. Find the OUT write.
        let writes: Vec<_> = mock
            .recorded()
            .iter()
            .filter(|r| r.direction == TransferDirection::Out)
            .collect();
        assert_eq!(writes.len(), 1);
        // We can't directly inspect the written byte via RecordedRequest data
        // without encoding it, but the control_out recorded data is the
        // little-endian register value (0x18 as a single byte).
        assert_eq!(writes[0].data, vec![0x18]);
    }

    #[test]
    fn read_reg_assembles_le_value() {
        let mut mock = MockTransport::new();
        mock.push_reply(sdr_fox_transport::ScriptedReply::any_in(vec![0x34, 0x12]));
        let val = read_reg(&mut mock, Block::Usb, 0x2014, 2).unwrap();
        assert_eq!(val, 0x1234); // little-endian: lo=0x34, hi=0x12
    }

    #[test]
    fn read_reg_rejects_invalid_len() {
        let mut mock = MockTransport::new();
        assert!(read_reg(&mut mock, Block::Usb, 0, 3).is_err());
        assert!(read_reg(&mut mock, Block::Usb, 0, 0).is_err());
    }

    #[test]
    fn rtl_i2c_bus_prepends_register_to_write() {
        let mut mock = MockTransport::new();
        let mut bus = RtlI2cBus::for_tuner(&mut mock, TunerKind::R820T2);
        bus.i2c_write(0x05, &[0xaa, 0xbb]).unwrap();
        // The IICB write should carry [reg, data...] = [0x05, 0xaa, 0xbb].
        let r = &mock.recorded()[0];
        assert_eq!(r.value, u16::from(R820T_I2C_ADDR));
        assert_eq!(r.data, vec![0x05, 0xaa, 0xbb]);
        assert_eq!(r.index, (Block::Iic as u16) << 8 | 0x10);
    }

    /// An R828D bus must tunnel to 0x74 on both the write and the
    /// address-then-read path — a bus that probes at 0x74 but then talks to
    /// 0x34 would program a chip that is not there, which is exactly the
    /// failure threading the address through the bus exists to prevent.
    #[test]
    fn rtl_i2c_bus_for_r828d_addresses_0x74() {
        let mut mock = MockTransport::new();
        {
            let mut bus = RtlI2cBus::for_tuner(&mut mock, TunerKind::R828D);
            bus.i2c_write(0x05, &[0xaa]).unwrap();
            bus.i2c_read(0x00, 1).unwrap();
        }
        // Three tunnel requests: the register write, then the read's
        // address write + IN transfer. All must carry wValue = 0x74.
        assert_eq!(mock.recorded().len(), 3);
        for r in mock.recorded() {
            assert_eq!(
                r.value,
                u16::from(R828D_I2C_ADDR),
                "every R828D tunnel access must address 0x74; got {r:?}"
            );
        }
    }

    /// R82xx silicon returns register bytes bit-reversed over the tunnel.
    /// `RtlI2cBus::i2c_read` must undo that for R82xx kinds, so their drivers
    /// see logical datasheet bytes and register masks (e.g. the R82xx
    /// VCO-lock bit R2[6]) test the correct physical bit. Without this, the
    /// chip-ID read returns 0x69 (bitrev of 0x96) and the R82xx `0x40` lock
    /// mask hits the wrong bit.
    #[test]
    fn rtl_i2c_bus_read_bit_reverses_each_byte_for_r82xx() {
        let mut mock = MockTransport::new();
        // Wire bytes: bitrev8(0x96)=0x69, bitrev8(0x40)=0x02, bitrev8(0x20)=0x04.
        mock.push_reply(sdr_fox_transport::ScriptedReply::any_in(vec![
            0x69, 0x02, 0x04,
        ]));
        let mut bus = RtlI2cBus::for_tuner(&mut mock, TunerKind::R820T2);
        let buf = bus.i2c_read(0x00, 3).unwrap();
        // The driver sees the logical register bytes, not the wire order.
        assert_eq!(buf, vec![0x96, 0x40, 0x20]);
    }

    /// The E4000 speaks plain datasheet bytes — the bit reversal is R82xx
    /// silicon behaviour, not a tunnel property — so an E4000 bus must return
    /// exactly the wire bytes. If the reversal were applied unconditionally,
    /// the chip id 0x40 would arrive as 0x02 and every register read
    /// (gain, PLL, DC-offset calibration) would be corrupted.
    #[test]
    fn rtl_i2c_bus_read_is_unreversed_for_e4000() {
        let mut mock = MockTransport::new();
        mock.push_reply(sdr_fox_transport::ScriptedReply::any_in(vec![
            0x40, 0x96, 0x01,
        ]));
        let mut bus = RtlI2cBus::for_tuner(&mut mock, TunerKind::E4000);
        let buf = bus.i2c_read(0x02, 3).unwrap();
        assert_eq!(
            buf,
            vec![0x40, 0x96, 0x01],
            "E4000 reads are datasheet bytes and must not be bit-reversed"
        );
    }

    /// An E4000 bus must tunnel every access to the chip's strapped I2C
    /// address, 0xc8 — same invariant the R828D test pins for 0x74.
    #[test]
    fn rtl_i2c_bus_for_e4000_addresses_0xc8() {
        let mut mock = MockTransport::new();
        {
            let mut bus = RtlI2cBus::for_tuner(&mut mock, TunerKind::E4000);
            bus.i2c_write(0x05, &[0xaa]).unwrap();
            bus.i2c_read(0x00, 1).unwrap();
        }
        assert_eq!(mock.recorded().len(), 3);
        for r in mock.recorded() {
            assert_eq!(
                r.value,
                u16::from(crate::tuners::e4000::E4000_I2C_ADDR),
                "every E4000 tunnel access must address 0xc8; got {r:?}"
            );
        }
    }
}
