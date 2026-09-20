//! The module bus: the controller's SPI link to a module slot.
//!
//! This is the link that exists *before* USB. The bootloader serves it, and
//! once the application is running `ModuleBus.c` takes over serving it, which
//! is what lets the controller decide what each connector interface carries
//! before Linux initialises the CAN channels and serial ports behind them.
//!
//! Messages are the bootloader's fixed 46 byte format — identifier in byte 0
//! and byte 2, a running 8 bit sum in the last byte — and the controller clocks
//! one extra byte, so a transfer is 47. Keeping the format identical is why
//! there is one code path here rather than two.
//!
//! **A reply arrives one transfer late.** SPI is full duplex, so the module
//! only learns what was asked once the last byte has been clocked in; its
//! answer therefore goes out with the *next* transfer. [`request`] sends the
//! same message repeatedly until the matching reply comes back, which is safe
//! because every application message is idempotent.

use std::ffi::CString;
use std::fs;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::proto::{Mode, Supply};

pub const MESSAGE_LENGTH: usize = 46;
pub const BUFFER_LENGTH: usize = MESSAGE_LENGTH + 1;

/// Message identifiers. 9 and 19 are the bootloader's; the rest are the
/// application's and start at 100 so they can never collide.
pub mod msg {
    pub const IDENTIFY: u8 = 9;
    pub const UP_TO_DATE: u8 = 19;
    pub const IF_GET: u8 = 100;
    pub const IF_SET: u8 = 101;
    pub const STATUS: u8 = 102;
    pub const SUPPLY_GET: u8 = 103;
    pub const SUPPLY_SET: u8 = 104;
}

const SPI_IOC_WR_MODE: u64 = 0x4001_6B01;
const SPI_IOC_WR_BITS_PER_WORD: u64 = 0x4001_6B03;
const SPI_IOC_WR_MAX_SPEED_HZ: u64 = 0x4004_6B04;
/// `_IOW('k', 0, struct spi_ioc_transfer[1])`
const SPI_IOC_MESSAGE_1: u64 = 0x4020_6B00;

const SPI_SPEED_HZ: u32 = 2_000_000;

/// The libc crate types the ioctl request differently per target: `c_ulong` on
/// gnu, `c_int` on musl. One alias keeps the call sites identical.
#[cfg(target_env = "musl")]
type IoctlRequest = libc::c_int;
#[cfg(not(target_env = "musl"))]
type IoctlRequest = libc::c_ulong;

/// Slot -> spidev, mirroring the Moduline L4 mapping in go-modules. Other
/// controller types use a different layout; add them here when needed.
pub const SLOT_SPIDEV: [(u8, &str); 8] = [
    (1, "/dev/spidev1.0"),
    (2, "/dev/spidev1.1"),
    (3, "/dev/spidev2.0"),
    (4, "/dev/spidev2.1"),
    (5, "/dev/spidev2.2"),
    (6, "/dev/spidev2.3"),
    (7, "/dev/spidev0.0"),
    (8, "/dev/spidev0.1"),
];

pub fn spidev_for_slot(slot: u8) -> Result<&'static str> {
    SLOT_SPIDEV
        .iter()
        .find(|(number, _)| *number == slot)
        .map(|(_, path)| *path)
        .ok_or_else(|| Error::user(format!("slot must be 1..{}", SLOT_SPIDEV.len())))
}

/// Running 8 bit sum over the first `length` bytes, as the bootloader does.
pub fn checksum(buffer: &[u8], length: usize) -> u8 {
    buffer[..length]
        .iter()
        .fold(0u32, |sum, &byte| sum + byte as u32) as u8
}

/// A complete module bus message. Payload bytes start at offset 3, which is
/// where the application's messages carry their arguments.
pub fn build_message(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut message = vec![0u8; BUFFER_LENGTH];
    message[0] = id;
    message[1] = (MESSAGE_LENGTH - 1) as u8;
    message[2] = id;
    message[3..3 + payload.len()].copy_from_slice(payload);
    message[MESSAGE_LENGTH - 1] = checksum(&message, MESSAGE_LENGTH - 1);
    message
}

/// Whether a reply is well formed, and optionally of the expected type.
pub fn message_valid(reply: &[u8], id: Option<u8>) -> bool {
    if reply.len() < MESSAGE_LENGTH {
        return false;
    }
    if checksum(reply, MESSAGE_LENGTH - 1) != reply[MESSAGE_LENGTH - 1] {
        return false;
    }
    if reply[0] != reply[2] {
        return false;
    }
    id.is_none_or(|expected| reply[0] == expected)
}

/// One open spidev, for the duration of a conversation with a slot.
pub struct ModuleBus {
    slot: u8,
    path: String,
    fd: libc::c_int,
}

#[repr(C)]
struct SpiIocTransfer {
    tx_buf: u64,
    rx_buf: u64,
    len: u32,
    speed_hz: u32,
    delay_usecs: u16,
    bits_per_word: u8,
    cs_change: u8,
    tx_nbits: u8,
    rx_nbits: u8,
    word_delay_usecs: u8,
    pad: u8,
}

impl ModuleBus {
    pub fn open(slot: u8) -> Result<ModuleBus> {
        let path = spidev_for_slot(slot)?;
        let c_path =
            CString::new(path).map_err(|_| Error::user(format!("{path} is not a usable path")))?;

        // SAFETY: a valid NUL terminated path that outlives the call.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(Error::System(format!(
                "cannot open {path}: {}. The module bus only exists on a controller.",
                std::io::Error::last_os_error()
            )));
        }

        let bus = ModuleBus {
            slot,
            path: path.to_string(),
            fd,
        };

        bus.set(SPI_IOC_WR_MODE, &0u8)?; // SPI mode 0
        bus.set(SPI_IOC_WR_BITS_PER_WORD, &8u8)?;
        bus.set(SPI_IOC_WR_MAX_SPEED_HZ, &SPI_SPEED_HZ)?;
        Ok(bus)
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }

    fn set<T>(&self, request: u64, value: &T) -> Result<()> {
        // SAFETY: `value` outlives the call and matches the size the ioctl
        // expects for its request number.
        let result = unsafe {
            libc::ioctl(
                self.fd,
                request as IoctlRequest,
                value as *const T as *mut libc::c_void,
            )
        };
        if result < 0 {
            return Err(Error::System(format!(
                "cannot configure {}: {}",
                self.path,
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Full duplex transfer of `tx.len()` bytes; returns what the module sent
    /// back during the same clock.
    pub fn transfer(&self, tx: &[u8]) -> Result<Vec<u8>> {
        let mut rx = vec![0u8; tx.len()];

        let message = SpiIocTransfer {
            tx_buf: tx.as_ptr() as u64,
            rx_buf: rx.as_mut_ptr() as u64,
            len: tx.len() as u32,
            speed_hz: SPI_SPEED_HZ,
            delay_usecs: 0,
            bits_per_word: 8,
            cs_change: 0,
            tx_nbits: 0,
            rx_nbits: 0,
            word_delay_usecs: 0,
            pad: 0,
        };

        // SAFETY: both buffers outlive the call and `len` matches them.
        let result = unsafe {
            libc::ioctl(
                self.fd,
                SPI_IOC_MESSAGE_1 as IoctlRequest,
                &message as *const SpiIocTransfer as *mut libc::c_void,
            )
        };
        if result < 0 {
            return Err(Error::System(format!(
                "SPI transfer on {} failed: {}",
                self.path,
                std::io::Error::last_os_error()
            )));
        }
        Ok(rx)
    }

    /// Send the identify message; `None` when the reply is not usable.
    pub fn identify(&self) -> Result<Option<Vec<u8>>> {
        let reply = self.transfer(&build_message(msg::IDENTIFY, &[]))?;
        let usable = checksum(&reply, MESSAGE_LENGTH - 1) == reply[MESSAGE_LENGTH - 1]
            && (reply[0] == msg::IDENTIFY || reply[2] == msg::IDENTIFY);
        Ok(if usable { Some(reply) } else { None })
    }

    /// Send an application message and return its reply.
    ///
    /// Repeats because the answer to a request only goes out with the next
    /// transfer — see the note at the top of this module.
    pub fn request(&self, id: u8, payload: &[u8]) -> Result<Option<Vec<u8>>> {
        let message = build_message(id, payload);
        for _ in 0..5 {
            let reply = self.transfer(&message)?;
            if message_valid(&reply, Some(id)) {
                return Ok(Some(reply));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(None)
    }

    /// Tell the bootloader its firmware is current, so it hands over to the
    /// application. It jumps the moment it accepts this, so there is no reply.
    pub fn start_application(&self) -> Result<()> {
        self.transfer(&build_message(msg::UP_TO_DATE, &[]))?;
        Ok(())
    }

    pub fn get_interfaces(&self) -> Result<Option<Vec<Mode>>> {
        let Some(reply) = self.request(msg::IF_GET, &[])? else {
            return Ok(None);
        };
        Ok(decode_modes(&reply[3..8]))
    }

    /// Push an interface configuration.
    ///
    /// `Ok(None)` means the module never answered, which an application built
    /// before `ModuleBus.c` does. `Ok(Some(Err(..)))` would be noise, so a
    /// refusal is reported as the modes that are actually active instead: the
    /// caller compares them with what it asked for.
    pub fn set_interfaces(&self, modes: &[Mode]) -> Result<Option<(bool, Vec<Mode>)>> {
        let payload: Vec<u8> = modes.iter().map(|mode| mode.as_byte()).collect();
        let Some(reply) = self.request(msg::IF_SET, &payload)? else {
            return Ok(None);
        };
        let accepted = reply[3] == 0;
        let active = decode_modes(&reply[4..9])
            .ok_or_else(|| Error::module("the module reported an unknown interface mode"))?;
        Ok(Some((accepted, active)))
    }

    pub fn get_supply(&self) -> Result<Option<Supply>> {
        let Some(reply) = self.request(msg::SUPPLY_GET, &[])? else {
            return Ok(None);
        };
        Ok(Some(Supply::from_bytes(&reply[3..3 + Supply::SIZE])?))
    }

    pub fn set_supply(&self, enabled: bool, millivolts: u16) -> Result<Option<(bool, Supply)>> {
        let mut payload = vec![u8::from(enabled)];
        payload.extend_from_slice(&millivolts.to_le_bytes());

        let Some(reply) = self.request(msg::SUPPLY_SET, &payload)? else {
            return Ok(None);
        };
        let accepted = reply[3] == 0;
        let active = Supply::from_bytes(&reply[4..4 + Supply::SIZE])?;
        Ok(Some((accepted, active)))
    }
}

impl Drop for ModuleBus {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: the descriptor was opened here and is closed once.
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

fn decode_modes(bytes: &[u8]) -> Option<Vec<Mode>> {
    bytes.iter().map(|&byte| Mode::from_byte(byte)).collect()
}

/// Drive a slot's reset line. Active high, through the leds interface.
pub fn set_reset(slot: u8, asserted: bool) -> Result<()> {
    let path = format!("/sys/class/leds/ResetM-{slot}/brightness");
    fs::write(&path, if asserted { "255" } else { "0" }).map_err(|error| {
        Error::System(format!(
            "cannot drive the reset line of slot {slot} ({path}): {error}"
        ))
    })
}

/// Reset the module and give the bootloader time to come up and arm SPI.
pub fn pulse_reset(slot: u8) -> Result<()> {
    set_reset(slot, true)?;
    std::thread::sleep(Duration::from_millis(200));
    set_reset(slot, false)?;
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_message_validates() {
        let message = build_message(msg::IF_SET, &[1, 2, 1, 2, 4]);
        assert_eq!(message.len(), BUFFER_LENGTH);
        assert!(message_valid(&message, Some(msg::IF_SET)));
        assert!(!message_valid(&message, Some(msg::IF_GET)));
        assert_eq!(&message[3..8], &[1, 2, 1, 2, 4]);
    }

    #[test]
    fn a_corrupted_message_does_not_validate() {
        let mut message = build_message(msg::STATUS, &[]);
        message[7] ^= 0xFF;
        assert!(!message_valid(&message, Some(msg::STATUS)));
    }

    #[test]
    fn the_transfer_struct_is_the_size_the_ioctl_encodes() {
        // SPI_IOC_MESSAGE_1 carries the struct size in bits 16..29; a mismatch
        // makes the kernel reject every transfer with EINVAL.
        assert_eq!(
            std::mem::size_of::<SpiIocTransfer>(),
            ((SPI_IOC_MESSAGE_1 >> 16) & 0x3FFF) as usize
        );
    }
}
