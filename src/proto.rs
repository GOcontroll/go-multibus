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
    pub const LINK_STATUS: u8 = 0x10;
    pub const CELLS_READ: u8 = 0x20;
    pub const CELLS_CONFIG: u8 = 0x21;
    pub const ISOSPI_XFER: u8 = 0x30;
    pub const SIM_SET: u8 = 0x40;
    pub const SUPPLY_GET: u8 = 0x50;
    pub const SUPPLY_SET: u8 = 0x51;
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
}
