//! The Multibus module protocol: frame codec and decoded answers.
//!
//! The wire format and the command table are shared with the firmware; the
//! authoritative description is `Documentation/multibus-usb-protocol.md` in the
//! Module-Multibus repository, and the firmware side is `MbProto.c`. Extending
//! the protocol means adding a constant to [`cmd`] and a decoder here, and a
//! handler in the firmware. Nothing else changes.
//!
//! Frame, little endian throughout:
//!
//! ```text
//!  offset  0        1   2      3     4      5 ...            last 2
//!         +--------+-------+------+------+-----------------+--------+
//!         | SOF    | LEN   | SEQ  | CMD  | PAYLOAD         | CRC16  |
//!         | 0x7E   | 2 B   | 1 B  | 1 B  | LEN-2 bytes     | 2 B    |
//!         +--------+-------+------+------+-----------------+--------+
//!                            \_________________________/
//!                             LEN counts these, and the CRC covers exactly them
//! ```
//!
//! The first payload byte of every answer is a status code; answer data starts
//! at payload byte 1. [`crate::link::MultibusLink::request`] strips it.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const SOF: u8 = 0x7E;
pub const HEADER_SIZE: usize = 5;
pub const CRC_SIZE: usize = 2;
pub const MAX_PAYLOAD: usize = 200;
pub const MAX_FRAME: usize = HEADER_SIZE + MAX_PAYLOAD + CRC_SIZE;
pub const RESPONSE_BIT: u8 = 0x80;

pub const PROTOCOL_VERSION: u8 = 1;

pub const MAX_CELLS: usize = 24;
pub const MAX_TEMPS: usize = 4;

/// Command codes. Keep in step with `MbProto.h`.
pub mod cmd {
    pub const INFO: u8 = 0x01;
    pub const IF_GET: u8 = 0x02;
    pub const IF_SET: u8 = 0x03;
    /// 0.3.0 and later: the isoSPI role in force, and switching it at runtime.
    pub const ROLE_GET: u8 = 0x04;
    pub const ROLE_SET: u8 = 0x05;
    /// 0.3.0 and later: USB serial number and chip UID.
    pub const IDENTITY: u8 = 0x06;
    pub const LINK_STATUS: u8 = 0x10;
    pub const CELLS_READ: u8 = 0x20;
    pub const CELLS_CONFIG: u8 = 0x21;
    pub const ISOSPI_XFER: u8 = 0x30;
    /// 0.3.0 and later: bounce bytes off the slave and time the round trip.
    pub const LINK_TEST: u8 = 0x31;
    pub const SIM_SET: u8 = 0x40;
    pub const SUPPLY_GET: u8 = 0x50;
    pub const SUPPLY_SET: u8 = 0x51;
    /// 0.3.0 and later: answers with its own payload.
    pub const ECHO: u8 = 0x60;
    /// 0.3.0 and later: per channel and per port counters.
    pub const CAN_STATS: u8 = 0x70;
    pub const SERIAL_STATS: u8 = 0x71;
}

/// Answer status codes, the first payload byte of every answer. Keep in step
/// with `MbProto.h`; [`crate::error::status_text`] has the wording.
pub mod status {
    pub const OK: u8 = 0x00;
    pub const UNKNOWN_COMMAND: u8 = 0x01;
    pub const BAD_LENGTH: u8 = 0x02;
    pub const BAD_PARAMETER: u8 = 0x03;
    pub const NOT_SUPPORTED: u8 = 0x04;
    pub const LINK_DOWN: u8 = 0x05;
    pub const BUSY: u8 = 0x06;
}

/// Cell snapshot flags, matching `CellMonitor.h`.
pub mod flag {
    pub const LINK_OK: u8 = 0x01;
    pub const STALE: u8 = 0x02;
    pub const SIMULATED: u8 = 0x04;
    pub const UNDERVOLT: u8 = 0x08;
    pub const OVERVOLT: u8 = 0x10;
}

// ── Interface modes ──────────────────────────────────────────────────────────

/// Mode of one connector interface, matching `MbIfMode_t`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Can,
    Rs485,
    Rs232,
    IsoSpi,
}

impl Mode {
    pub fn from_byte(value: u8) -> Option<Mode> {
        match value {
            0 => Some(Mode::Off),
            1 => Some(Mode::Can),
            2 => Some(Mode::Rs485),
            3 => Some(Mode::Rs232),
            4 => Some(Mode::IsoSpi),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Mode::Off => 0,
            Mode::Can => 1,
            Mode::Rs485 => 2,
            Mode::Rs232 => 3,
            Mode::IsoSpi => 4,
        }
    }

    /// The name used in the configuration file, on the command line and in the
    /// published documents. One spelling everywhere, so a mode can be copied
    /// from a log straight into a command.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Can => "can",
            Mode::Rs485 => "rs485",
            Mode::Rs232 => "rs232",
            Mode::IsoSpi => "isospi",
        }
    }

    /// How the UI spells it, where the reader is looking at a bus rather than
    /// at a configuration key.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Can => "CAN FD",
            Mode::Rs485 => "RS485",
            Mode::Rs232 => "RS232",
            Mode::IsoSpi => "isoSPI",
        }
    }

    pub fn parse(name: &str) -> Result<Mode> {
        match name.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Mode::Off),
            "can" => Ok(Mode::Can),
            "rs485" => Ok(Mode::Rs485),
            "rs232" => Ok(Mode::Rs232),
            "isospi" => Ok(Mode::IsoSpi),
            other => Err(Error::user(format!(
                "unknown interface mode {other}; pick from can, rs485, rs232, isospi, off"
            ))),
        }
    }
}

/// The connector interfaces, numbered 1..5 as the pins are on the connector.
///
/// Mirrors the table in `MbInterfaces.c`. Note that neither the board's RS485
/// labels nor its CAN labels follow this numbering - see
/// `Documentation/multibus-interfaces.md`, which is the one place that trap is
/// written down in full.
pub struct InterfaceSpec {
    pub number: u8,
    pub pins: &'static str,
    pub modes: &'static [Mode],
}

pub const INTERFACES: [InterfaceSpec; 5] = [
    InterfaceSpec { number: 1, pins: "6/5", modes: &[Mode::Can] },
    InterfaceSpec { number: 2, pins: "12/13", modes: &[Mode::Rs485, Mode::Rs232, Mode::Off] },
    InterfaceSpec { number: 3, pins: "18/19", modes: &[Mode::Rs485, Mode::Can, Mode::Off] },
    InterfaceSpec { number: 4, pins: "24/25", modes: &[Mode::Rs485, Mode::Can, Mode::Off] },
    InterfaceSpec { number: 5, pins: "isoSPI", modes: &[Mode::IsoSpi, Mode::Off] },
];

// ── isoSPI role ──────────────────────────────────────────────────────────────

/// Which end of the isoSPI link a module drives, matching `MB_ROLE_*`.
///
/// The master owns the clock and asks the battery side for cells; the slave
/// answers. Two modules on one link must therefore be one of each - and since
/// go-modules gives every module of a hardware revision the same image, the
/// role cannot come from the build. From 0.3.0 the controller pushes it on
/// every bring-up, the same way it pushes the interface configuration; the
/// module does not store it, so after a reset it is back in the role its
/// firmware starts in until told otherwise.
///
/// Spelled `master` / `slave` in the configuration file, on the command line
/// and in the published documents.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Master,
    Slave,
}

impl Role {
    /// Both roles, in the order a picker lists them.
    pub const ALL: [Role; 2] = [Role::Master, Role::Slave];

    pub fn from_byte(value: u8) -> Option<Role> {
        match value {
            0 => Some(Role::Master),
            1 => Some(Role::Slave),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Role::Master => 0,
            Role::Slave => 1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Slave => "slave",
        }
    }

    pub fn parse(name: &str) -> Result<Role> {
        match name.trim().to_ascii_lowercase().as_str() {
            "master" => Ok(Role::Master),
            "slave" => Ok(Role::Slave),
            other => Err(Error::user(format!(
                "unknown isoSPI role {other}; pick master or slave"
            ))),
        }
    }

    /// Answer to `ROLE_GET` / `ROLE_SET`: one byte, the role in force.
    pub fn from_bytes(data: &[u8]) -> Result<Role> {
        need(data, 1, "ROLE")?;
        Role::from_byte(data[0])
            .ok_or_else(|| Error::module(format!("module reported unknown role {}", data[0])))
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

// ── Frame codec ──────────────────────────────────────────────────────────────

/// CRC-16/XMODEM: polynomial 0x1021, initial value 0x0000, no reflection, no
/// final xor. Check value over `"123456789"` is `0x31C3`.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn build_frame(seq: u8, command: u8, payload: &[u8]) -> Vec<u8> {
    let body = (payload.len() + 2) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len() + CRC_SIZE);

    frame.push(SOF);
    frame.extend_from_slice(&body.to_le_bytes());
    frame.push(seq);
    frame.push(command);
    frame.extend_from_slice(payload);

    let crc = crc16(&frame[3..]);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

/// One decoded frame.
pub struct Frame {
    pub seq: u8,
    pub command: u8,
    pub payload: Vec<u8>,
}

/// Find the first valid frame in `buffer`.
///
/// Returns the frame, if one is complete, and how many bytes of `buffer` were
/// consumed. Anything that fails validation costs one byte of resynchronisation
/// rather than the whole buffer, so a single corrupted byte cannot swallow the
/// frames behind it.
pub fn parse_frame(buffer: &[u8]) -> (Option<Frame>, usize) {
    let mut offset = 0usize;

    loop {
        // Skip anything in front of a start byte.
        while offset < buffer.len() && buffer[offset] != SOF {
            offset += 1;
        }

        let rest = &buffer[offset..];
        if rest.len() < HEADER_SIZE {
            return (None, offset);
        }

        let body = u16::from_le_bytes([rest[1], rest[2]]) as usize;
        if body < 2 || body > MAX_PAYLOAD + 2 {
            offset += 1; // not a header after all
            continue;
        }

        let total = body + HEADER_SIZE - 2 + CRC_SIZE;
        if rest.len() < total {
            return (None, offset); // wait for the rest of the frame
        }

        let crc_wire = u16::from_le_bytes([rest[3 + body], rest[3 + body + 1]]);
        if crc16(&rest[3..3 + body]) != crc_wire {
            offset += 1;
            continue;
        }

        let frame = Frame {
            seq: rest[3],
            command: rest[4],
            payload: rest[5..3 + body].to_vec(),
        };
        return (Some(frame), offset + total);
    }
}

// ── Decoded answers ──────────────────────────────────────────────────────────

fn need(data: &[u8], size: usize, what: &str) -> Result<()> {
    if data.len() < size {
        return Err(Error::module(format!(
            "{what} answer is {} bytes, expected {size}",
            data.len()
        )));
    }
    Ok(())
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Answer to `INFO`: who the module is and what it can do.
#[derive(Clone, Debug)]
pub struct Info {
    pub protocol_version: u8,
    pub role: u8,
    pub hardware: [u8; 4],
    pub software: [u8; 3],
    pub interface_count: u8,
    pub protocol_port: u8,
    pub max_cells: u8,
    pub max_temperatures: u8,
    pub max_transfer: u8,
}

impl Info {
    pub fn from_bytes(data: &[u8]) -> Result<Info> {
        need(data, 14, "INFO")?;
        Ok(Info {
            protocol_version: data[0],
            role: data[1],
            hardware: [data[2], data[3], data[4], data[5]],
            software: [data[6], data[7], data[8]],
            interface_count: data[9],
            protocol_port: data[10],
            max_cells: data[11],
            max_temperatures: data[12],
            max_transfer: data[13],
        })
    }

    pub fn role_name(&self) -> String {
        match self.role {
            0 => "master".into(),
            1 => "slave".into(),
            other => format!("role {other}"),
        }
    }

    /// The role byte as a [`Role`]. From 0.3.0 it is the role in force rather
    /// than the one the image was built for, which is what lets it stand in
    /// for `ROLE_GET` on a firmware that does not have that command.
    pub fn role(&self) -> Option<Role> {
        Role::from_byte(self.role)
    }

    pub fn hardware_name(&self) -> String {
        self.hardware
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>()
            .join("-")
    }

    pub fn software_name(&self) -> String {
        self.software
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// Answer to `LINK_STATUS`: how the isoSPI link is behaving.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkStatus {
    pub role: u8,
    pub link_ok: bool,
    pub poll_ms: u16,
    pub tx_frames: u32,
    pub rx_frames: u32,
    pub crc_errors: u32,
    pub spi_errors: u32,
    pub resyncs: u32,
    pub last_rx_age_ms: u32,
}

impl LinkStatus {
    pub const SIZE: usize = 28;

    pub fn from_bytes(data: &[u8]) -> Result<LinkStatus> {
        need(data, Self::SIZE, "LINK_STATUS")?;
        Ok(LinkStatus {
            role: data[0],
            link_ok: data[1] != 0,
            poll_ms: u16_at(data, 2),
            tx_frames: u32_at(data, 4),
            rx_frames: u32_at(data, 8),
            crc_errors: u32_at(data, 12),
            spi_errors: u32_at(data, 16),
            resyncs: u32_at(data, 20),
            last_rx_age_ms: u32_at(data, 24),
        })
    }
}

/// Answer to `CELLS_READ`.
///
/// Voltages travel in 100 µV steps, the native unit of the LTC681x family, so
/// the numbers are exactly what a real battery monitor hands over. The
/// conversions to volts are for display only.
#[derive(Clone, Debug, Default)]
pub struct CellSnapshot {
    pub version: u8,
    pub flags: u8,
    pub sequence: u32,
    pub age_ms: u32,
    pub pack_mv: u32,
    pub cells_100uv: Vec<u16>,
    pub temperatures_dc: Vec<i16>,
}

impl CellSnapshot {
    pub const SIZE: usize = 16 + MAX_CELLS * 2 + MAX_TEMPS * 2;

    pub fn from_bytes(data: &[u8]) -> Result<CellSnapshot> {
        need(data, Self::SIZE, "CELLS_READ")?;

        let n_cells = (data[1] as usize).min(MAX_CELLS);
        let n_temps = (data[2] as usize).min(MAX_TEMPS);

        let cells = (0..n_cells).map(|i| u16_at(data, 16 + i * 2)).collect();
        let temperatures = (0..n_temps)
            .map(|i| u16_at(data, 16 + MAX_CELLS * 2 + i * 2) as i16)
            .collect();

        Ok(CellSnapshot {
            version: data[0],
            flags: data[3],
            sequence: u32_at(data, 4),
            age_ms: u32_at(data, 8),
            pack_mv: u32_at(data, 12),
            cells_100uv: cells,
            temperatures_dc: temperatures,
        })
    }

    pub fn link_ok(&self) -> bool {
        self.flags & flag::LINK_OK != 0
    }
    pub fn stale(&self) -> bool {
        self.flags & flag::STALE != 0
    }
    pub fn simulated(&self) -> bool {
        self.flags & flag::SIMULATED != 0
    }
    pub fn undervoltage(&self) -> bool {
        self.flags & flag::UNDERVOLT != 0
    }
    pub fn overvoltage(&self) -> bool {
        self.flags & flag::OVERVOLT != 0
    }

    pub fn cells_v(&self) -> Vec<f64> {
        self.cells_100uv
            .iter()
            .map(|&value| value as f64 / 10_000.0)
            .collect()
    }

    pub fn temperatures_c(&self) -> Vec<f64> {
        self.temperatures_dc
            .iter()
            .map(|&value| value as f64 / 10.0)
            .collect()
    }

    pub fn pack_v(&self) -> f64 {
        self.pack_mv as f64 / 1000.0
    }

    /// Difference between the highest and the lowest cell.
    pub fn spread_v(&self) -> f64 {
        match (
            self.cells_100uv.iter().max(),
            self.cells_100uv.iter().min(),
        ) {
            (Some(&high), Some(&low)) => (high - low) as f64 / 10_000.0,
            _ => 0.0,
        }
    }
}

/// Answer to `IF_GET` / `IF_SET`: the mode of every connector interface.
#[derive(Clone, Debug)]
pub struct InterfaceConfig {
    pub modes: Vec<Mode>,
}

impl InterfaceConfig {
    pub fn from_bytes(data: &[u8]) -> Result<InterfaceConfig> {
        need(data, INTERFACES.len(), "interface")?;
        let modes = data[..INTERFACES.len()]
            .iter()
            .map(|&byte| {
                Mode::from_byte(byte)
                    .ok_or_else(|| Error::module(format!("module reported unknown mode {byte}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(InterfaceConfig { modes })
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.modes.iter().map(|mode| mode.as_str()).collect()
    }

    pub fn joined(&self) -> String {
        self.names().join(",")
    }
}

/// Answer to `SUPPLY_GET` / `SUPPLY_SET`: the external supply on the connector.
///
/// The limits come from the module rather than from a table here on purpose.
/// A controller can then drive a module whose firmware widened the range
/// without needing a new build of this tool, and the picker in the UI can never
/// offer a voltage the module would refuse.
#[derive(Clone, Copy, Debug)]
pub struct Supply {
    pub enabled: bool,
    pub millivolts: u16,
    pub min_mv: u16,
    pub max_mv: u16,
    pub step_mv: u16,
    /// Whether the module has this in flash, so it comes back after a reset.
    pub stored: bool,
}

impl Supply {
    pub const SIZE: usize = 10;

    pub fn from_bytes(data: &[u8]) -> Result<Supply> {
        need(data, Self::SIZE, "SUPPLY")?;
        Ok(Supply {
            enabled: data[0] != 0,
            millivolts: u16_at(data, 1),
            min_mv: u16_at(data, 3),
            max_mv: u16_at(data, 5),
            step_mv: u16_at(data, 7),
            stored: data[9] != 0,
        })
    }

    /// Every setpoint the module accepts, low to high.
    pub fn steps(&self) -> Vec<u16> {
        if self.step_mv == 0 || self.max_mv < self.min_mv {
            return Vec::new();
        }
        let mut values = Vec::new();
        let mut value = self.min_mv;
        while value <= self.max_mv {
            values.push(value);
            match value.checked_add(self.step_mv) {
                Some(next) => value = next,
                None => break,
            }
        }
        values
    }

    pub fn volts(&self) -> f64 {
        self.millivolts as f64 / 1000.0
    }

    /// How the state reads in one phrase, for a list row or a log line.
    pub fn summary(&self) -> String {
        if self.enabled {
            format!("{} on", format_volts(self.millivolts))
        } else {
            "off".to_string()
        }
    }
}

/// A setpoint as the UI and the CLI print it: whole volts without a decimal
/// point while the step is a whole volt, which is what the firmware allows.
pub fn format_volts(millivolts: u16) -> String {
    if millivolts % 1000 == 0 {
        format!("{} V", millivolts / 1000)
    } else {
        format!("{:.1} V", millivolts as f64 / 1000.0)
    }
}

// ── Identity ─────────────────────────────────────────────────────────────────

/// Length of the USB serial number in an `IDENTITY` answer, in characters.
pub const SERIAL_LEN: usize = 12;
/// Length of the STM32 unique id in an `IDENTITY` answer, in bytes.
pub const UID_LEN: usize = 12;

/// Answer to `IDENTITY`, over USB and over the module bus alike.
///
/// The serial number is exactly what the module's USB descriptor carries, so
/// it is also what `/sys/bus/usb/devices/<device>/serial` shows. That is what
/// ties the two links together: the module bus reaches a *slot*, USB reaches a
/// *device*, and with two modules on one controller nothing else says which
/// device sits in which slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub serial: String,
    pub uid: [u8; UID_LEN],
}

impl Identity {
    pub const SIZE: usize = SERIAL_LEN + UID_LEN;

    pub fn from_bytes(data: &[u8]) -> Result<Identity> {
        need(data, Self::SIZE, "IDENTITY")?;

        // Printable and free of whitespace, because the serial travels on as a
        // word in the name map and as an argument on a udev rule line.
        let raw = &data[..SERIAL_LEN];
        let end = raw.iter().position(|&byte| byte == 0).unwrap_or(SERIAL_LEN);
        if end == 0 || !raw[..end].iter().all(|byte| byte.is_ascii_graphic()) {
            return Err(Error::module(
                "the module reported a serial number that is not printable ASCII",
            ));
        }

        let mut uid = [0u8; UID_LEN];
        uid.copy_from_slice(&data[SERIAL_LEN..Self::SIZE]);

        Ok(Identity {
            serial: String::from_utf8_lossy(&raw[..end]).into_owned(),
            uid,
        })
    }

    pub fn uid_hex(&self) -> String {
        self.uid.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

// ── Link test ────────────────────────────────────────────────────────────────

/// Result codes of `LINK_TEST`, matching `CellMon_LinkTest`.
pub mod link_test {
    pub const OK: u8 = 0;
    pub const NO_ANSWER: u8 = 1;
    pub const CRC: u8 = 2;
    pub const MISMATCH: u8 = 3;

    /// The longest payload one test may carry.
    pub const MAX_PAYLOAD: usize = 80;

    pub fn result_name(code: u8) -> &'static str {
        match code {
            OK => "ok",
            NO_ANSWER => "no answer",
            CRC => "crc error",
            MISMATCH => "mismatch",
            _ => "unknown result",
        }
    }
}

/// Answer to `LINK_TEST`: how one round trip to the slave went.
///
/// Timed by the master, so the figure is the isoSPI round trip alone, without
/// the USB hop in front of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTest {
    pub result: u8,
    pub micros: u32,
    pub echoed: Vec<u8>,
}

impl LinkTest {
    pub fn from_bytes(data: &[u8]) -> Result<LinkTest> {
        need(data, 5, "LINK_TEST")?;
        Ok(LinkTest {
            result: data[0],
            micros: u32_at(data, 1),
            echoed: data[5..].to_vec(),
        })
    }

    pub fn ok(&self) -> bool {
        self.result == link_test::OK
    }
}

// ── Counters ─────────────────────────────────────────────────────────────────

/// One gs_usb channel's entry in the `CAN_STATS` answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanChannelStats {
    pub rx_frames: u32,
    pub tx_frames: u32,
    /// Frames the FDCAN receive FIFO overwrote before the firmware read them.
    pub rx_fifo_lost: u32,
    /// Frames the host never collected.
    pub host_dropped: u32,
    pub tx_dropped: u32,
    pub bus_off: u32,
    pub restarts: u32,
    pub state: u8,
    pub tec: u8,
    pub rec: u8,
    pub running: bool,
}

impl CanChannelStats {
    pub const SIZE: usize = 32;

    /// Every channel in the answer. An empty answer is valid: it is what a
    /// build without the counters sends.
    pub fn list_from_bytes(data: &[u8]) -> Result<Vec<CanChannelStats>> {
        if data.len() % Self::SIZE != 0 {
            return Err(Error::module(format!(
                "CAN_STATS answer is {} bytes, not a whole number of {} byte channels",
                data.len(),
                Self::SIZE
            )));
        }
        Ok(data
            .chunks_exact(Self::SIZE)
            .map(|block| CanChannelStats {
                rx_frames: u32_at(block, 0),
                tx_frames: u32_at(block, 4),
                rx_fifo_lost: u32_at(block, 8),
                host_dropped: u32_at(block, 12),
                tx_dropped: u32_at(block, 16),
                bus_off: u32_at(block, 20),
                restarts: u32_at(block, 24),
                state: block[28],
                tec: block[29],
                rec: block[30],
                running: block[31] != 0,
            })
            .collect())
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            0 => "error active",
            1 => "error warning",
            2 => "error passive",
            3 => "bus-off",
            4 => "stopped",
            _ => "unknown",
        }
    }
}

/// One serial bridge's entry in the `SERIAL_STATS` answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SerialPortStats {
    pub uart_rx_bytes: u32,
    pub uart_tx_bytes: u32,
    /// Bytes lost because the ring towards USB was full.
    pub ring_dropped: u32,
    pub uart_errors: u32,
    pub overruns: u32,
}

impl SerialPortStats {
    pub const SIZE: usize = 20;

    pub fn list_from_bytes(data: &[u8]) -> Result<Vec<SerialPortStats>> {
        if data.len() % Self::SIZE != 0 {
            return Err(Error::module(format!(
                "SERIAL_STATS answer is {} bytes, not a whole number of {} byte ports",
                data.len(),
                Self::SIZE
            )));
        }
        Ok(data
            .chunks_exact(Self::SIZE)
            .map(|block| SerialPortStats {
                uart_rx_bytes: u32_at(block, 0),
                uart_tx_bytes: u32_at(block, 4),
                ring_dropped: u32_at(block, 8),
                uart_errors: u32_at(block, 12),
                overruns: u32_at(block, 16),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_documented_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn a_built_frame_parses_back() {
        let frame = build_frame(0x42, cmd::CELLS_READ, &[]);
        let (parsed, used) = parse_frame(&frame);
        let parsed = parsed.expect("frame should parse");
        assert_eq!(used, frame.len());
        assert_eq!(parsed.seq, 0x42);
        assert_eq!(parsed.command, cmd::CELLS_READ);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn leading_rubbish_is_skipped_and_a_partial_frame_waits() {
        let mut stream = vec![0x00, 0xFF, 0x7E];
        stream.extend_from_slice(&build_frame(7, cmd::INFO, &[1, 2, 3]));

        let (parsed, used) = parse_frame(&stream);
        let parsed = parsed.expect("frame should parse");
        assert_eq!(parsed.payload, vec![1, 2, 3]);
        assert_eq!(used, stream.len());

        // The same frame one byte short must not decode.
        let (none, _) = parse_frame(&stream[..stream.len() - 1]);
        assert!(none.is_none());
    }

    #[test]
    fn a_corrupted_crc_costs_one_byte_not_the_buffer() {
        let mut frame = build_frame(1, cmd::INFO, &[9]);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        let (parsed, used) = parse_frame(&frame);
        assert!(parsed.is_none());
        assert_eq!(used, frame.len(), "the whole buffer is rubbish once scanned");
    }

    #[test]
    fn supply_steps_cover_the_reported_range() {
        let supply = Supply {
            enabled: false,
            millivolts: 5000,
            min_mv: 5000,
            max_mv: 15000,
            step_mv: 1000,
            stored: true,
        };
        let steps = supply.steps();
        assert_eq!(steps.first(), Some(&5000));
        assert_eq!(steps.last(), Some(&15000));
        assert_eq!(steps.len(), 11);
    }

    #[test]
    fn a_role_set_request_carries_one_role_byte() {
        let frame = build_frame(3, cmd::ROLE_SET, &[Role::Slave.as_byte()]);
        let (parsed, _) = parse_frame(&frame);
        let parsed = parsed.expect("frame should parse");
        assert_eq!(parsed.command, 0x05);
        assert_eq!(parsed.payload, vec![1]);
    }

    #[test]
    fn roles_round_trip_through_byte_name_and_json() {
        for role in Role::ALL {
            assert_eq!(Role::from_byte(role.as_byte()), Some(role));
            assert_eq!(Role::parse(role.as_str()).unwrap(), role);
            let text = serde_json::to_string(&role).unwrap();
            assert_eq!(text, format!("\"{}\"", role.as_str()));
        }
        assert_eq!(Role::from_bytes(&[1]).unwrap(), Role::Slave);
        assert!(Role::from_bytes(&[7]).is_err());
        assert!(Role::from_bytes(&[]).is_err());
        assert!(Role::parse("Master ").is_ok());
        assert!(Role::parse("monitor").is_err());
    }

    #[test]
    fn identity_decodes_the_usb_serial_and_the_uid() {
        let mut data = b"206435A63245".to_vec();
        data.extend(1..=12u8);
        let identity = Identity::from_bytes(&data).unwrap();
        assert_eq!(identity.serial, "206435A63245");
        assert_eq!(identity.uid[0], 1);
        assert_eq!(identity.uid[11], 12);
        assert_eq!(identity.uid_hex(), "0102030405060708090a0b0c");
    }

    #[test]
    fn an_identity_that_cannot_travel_as_a_word_is_refused() {
        let mut data = b"2064 5A63245".to_vec();
        data.extend([0u8; UID_LEN]);
        assert!(Identity::from_bytes(&data).is_err());

        let mut short = b"206435A6\0\0\0\0".to_vec();
        short.extend([0u8; UID_LEN]);
        assert_eq!(Identity::from_bytes(&short).unwrap().serial, "206435A6");

        assert!(Identity::from_bytes(&[0u8; Identity::SIZE]).is_err());
        assert!(Identity::from_bytes(b"206435A63245").is_err(), "the UID is missing");
    }

    #[test]
    fn a_link_test_answer_splits_into_result_latency_and_echo() {
        let mut data = vec![link_test::OK];
        data.extend_from_slice(&1234u32.to_le_bytes());
        data.extend_from_slice(&[0xA0, 0xA1, 0xA2]);
        let test = LinkTest::from_bytes(&data).unwrap();
        assert!(test.ok());
        assert_eq!(test.micros, 1234);
        assert_eq!(test.echoed, vec![0xA0, 0xA1, 0xA2]);

        let failed = LinkTest::from_bytes(&[link_test::CRC, 0, 0, 0, 0]).unwrap();
        assert!(!failed.ok());
        assert!(failed.echoed.is_empty());
        assert_eq!(link_test::result_name(failed.result), "crc error");

        assert!(LinkTest::from_bytes(&[0, 1, 2]).is_err());
    }

    #[test]
    fn can_stats_decode_per_channel() {
        let mut data = Vec::new();
        for channel in 0..3u32 {
            for counter in 0..7u32 {
                data.extend_from_slice(&(channel * 100 + counter).to_le_bytes());
            }
            data.extend_from_slice(&[channel as u8, 10 + channel as u8, 20, 1]);
        }
        let channels = CanChannelStats::list_from_bytes(&data).unwrap();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0].rx_frames, 0);
        assert_eq!(channels[1].tx_frames, 101);
        assert_eq!(channels[2].restarts, 206);
        assert_eq!(channels[2].state, 2);
        assert_eq!(channels[2].state_name(), "error passive");
        assert_eq!(channels[1].tec, 11);
        assert_eq!(channels[0].rec, 20);
        assert!(channels[0].running);

        assert!(CanChannelStats::list_from_bytes(&[]).unwrap().is_empty());
        assert!(CanChannelStats::list_from_bytes(&data[..40]).is_err());
    }

    #[test]
    fn serial_stats_decode_per_port() {
        let mut data = Vec::new();
        for value in 0..15u32 {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let ports = SerialPortStats::list_from_bytes(&data).unwrap();
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].uart_rx_bytes, 0);
        assert_eq!(ports[1].uart_tx_bytes, 6);
        assert_eq!(ports[2].overruns, 14);
        assert!(SerialPortStats::list_from_bytes(&data[..19]).is_err());
    }
}
