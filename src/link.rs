//! One open connection to the protocol port of a Multibus module.
//!
//! The port is a CDC-ACM tty, opened directly and put into raw mode with
//! termios. The baud rate is meaningless: behind this port sits the protocol
//! handler in the firmware, not a UART. What matters is that the kernel passes
//! every byte through untouched — no echo, no CR/LF translation, no flow
//! control.

use std::ffi::CString;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::proto::{
    self, cmd, CanChannelStats, CellSnapshot, Identity, Info, InterfaceConfig, LinkStatus,
    LinkTest, Mode, Role, SerialPortStats, Supply,
};

/// How long a request waits for its answer before it is retried.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);
/// How many times a request is repeated after a timeout.
pub const DEFAULT_RETRIES: u32 = 2;

pub struct MultibusLink {
    device: String,
    fd: libc::c_int,
    timeout: Duration,
    retries: u32,
    seq: u8,
    buffer: Vec<u8>,
}

impl MultibusLink {
    /// Open the given device, or the only Multibus protocol port present.
    pub fn open(device: Option<&str>) -> Result<MultibusLink> {
        let device = match device {
            Some(path) => path.to_string(),
            None => crate::discover::find_protocol_port()?,
        };
        MultibusLink::open_path(&device, DEFAULT_TIMEOUT, DEFAULT_RETRIES)
    }

    pub fn open_path(device: &str, timeout: Duration, retries: u32) -> Result<MultibusLink> {
        let path = CString::new(device)
            .map_err(|_| Error::user(format!("{device} is not a usable path")))?;

        // SAFETY: `path` is a valid NUL terminated string that outlives the call.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(Error::System(format!(
                "cannot open {device}: {}",
                std::io::Error::last_os_error()
            )));
        }

        let mut link = MultibusLink {
            device: device.to_string(),
            fd,
            timeout,
            retries,
            seq: 0,
            buffer: Vec::new(),
        };

        if let Err(failure) = link.configure_raw() {
            link.close();
            return Err(failure);
        }
        Ok(link)
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// Release the port. Safe to call more than once.
    ///
    /// Queued output is discarded first. Closing a tty makes the kernel wait
    /// for its write buffer to drain, and on a CDC port whose far end is not
    /// reading, that wait never ends. It is easy to hit here: a request that
    /// timed out on write did so precisely because the buffer was full, and
    /// closing straight afterwards would hang on those same bytes.
    pub fn close(&mut self) {
        let fd = std::mem::replace(&mut self.fd, -1);
        if fd < 0 {
            return;
        }
        // SAFETY: fd was open and is taken out of self before either call, so
        // it cannot be closed twice.
        unsafe {
            libc::tcflush(fd, libc::TCIOFLUSH);
            libc::close(fd);
        }
    }

    fn configure_raw(&mut self) -> Result<()> {
        // SAFETY: termios is a plain C struct; tcgetattr fills every field that
        // is read below, and self.fd is open for the duration.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.fd, &mut settings) != 0 {
                return Err(Error::System(format!(
                    "cannot read the terminal settings of {}: {}",
                    self.device,
                    std::io::Error::last_os_error()
                )));
            }

            settings.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON
                | libc::IXOFF
                | libc::IXANY);
            settings.c_oflag &= !libc::OPOST;
            settings.c_lflag &= !(libc::ECHO
                | libc::ECHOE
                | libc::ECHONL
                | libc::ICANON
                | libc::ISIG
                | libc::IEXTEN);
            settings.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::CSTOPB);
            settings.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;

            // Non-blocking reads: poll() decides how long to wait, not the tty.
            settings.c_cc[libc::VMIN] = 0;
            settings.c_cc[libc::VTIME] = 0;

            if libc::tcsetattr(self.fd, libc::TCSANOW, &settings) != 0 {
                return Err(Error::System(format!(
                    "cannot set raw mode on {}: {}",
                    self.device,
                    std::io::Error::last_os_error()
                )));
            }
            libc::tcflush(self.fd, libc::TCIOFLUSH);
        }
        Ok(())
    }

    /// Wait until the descriptor is ready, or the deadline passes.
    fn wait(&self, write: bool, deadline: Instant) -> Result<bool> {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let remaining = (deadline - now).as_millis().min(i32::MAX as u128) as libc::c_int;

        let mut fds = libc::pollfd {
            fd: self.fd,
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };

        // SAFETY: one initialised pollfd is passed, with a matching count.
        let ready = unsafe { libc::poll(&mut fds, 1, remaining) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(Error::System(format!("poll on {} failed: {error}", self.device)));
        }
        Ok(ready > 0)
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        let mut sent = 0usize;

        while sent < data.len() {
            if !self.wait(true, deadline)? {
                return Err(Error::module(format!("timed out writing to {}", self.device)));
            }
            // SAFETY: the slice is in bounds and the descriptor is open.
            let written = unsafe {
                libc::write(
                    self.fd,
                    data[sent..].as_ptr() as *const libc::c_void,
                    data.len() - sent,
                )
            };
            if written < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(Error::System(format!("write to {} failed: {error}", self.device)));
            }
            sent += written as usize;
        }
        Ok(())
    }

    /// Next valid frame, or None when the deadline passes first.
    fn read_frame(&mut self, deadline: Instant) -> Result<Option<proto::Frame>> {
        loop {
            let (frame, used) = proto::parse_frame(&self.buffer);
            if used > 0 {
                self.buffer.drain(..used);
            }
            if frame.is_some() {
                return Ok(frame);
            }

            if !self.wait(false, deadline)? {
                return Ok(None);
            }

            let mut chunk = [0u8; 512];
            // SAFETY: the buffer is owned and its length is passed unchanged.
            let read = unsafe {
                libc::read(
                    self.fd,
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                )
            };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(Error::System(format!("read from {} failed: {error}", self.device)));
            }
            if read == 0 {
                // A CDC-ACM node returns nothing at all once its USB device is
                // gone, which is how a module that was unplugged or reset shows
                // up here.
                return Err(Error::module(format!("{} went away", self.device)));
            }
            self.buffer.extend_from_slice(&chunk[..read as usize]);
        }
    }

    /// Send one request and return the answer payload without its status byte.
    ///
    /// Retries on timeout: a lost request costs one round trip, and the
    /// sequence number keeps a late answer to an earlier request from being
    /// mistaken for this one.
    pub fn request(&mut self, command: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let mut last: Option<Error> = None;

        for _ in 0..=self.retries {
            self.seq = self.seq.wrapping_add(1);
            let seq = self.seq;
            self.buffer.clear();

            self.write_all(&proto::build_frame(seq, command, payload))?;

            let deadline = Instant::now() + self.timeout;
            loop {
                let frame = match self.read_frame(deadline)? {
                    Some(frame) => frame,
                    None => {
                        last = Some(Error::module(format!(
                            "no answer to command {command:#04X} within {:.1}s",
                            self.timeout.as_secs_f64()
                        )));
                        break;
                    }
                };

                if frame.seq != seq || frame.command != (command | proto::RESPONSE_BIT) {
                    continue; // a stale answer to an earlier request
                }

                let Some((&status, data)) = frame.payload.split_first() else {
                    last = Some(Error::module(format!(
                        "empty answer to command {command:#04X}"
                    )));
                    break;
                };

                if status != 0 {
                    return Err(Error::status(command, status));
                }
                return Ok(data.to_vec());
            }
        }

        Err(last.unwrap_or_else(|| Error::module("request failed")))
    }

    // -- commands ------------------------------------------------------------

    pub fn info(&mut self) -> Result<Info> {
        Info::from_bytes(&self.request(cmd::INFO, &[])?)
    }

    pub fn link_status(&mut self) -> Result<LinkStatus> {
        LinkStatus::from_bytes(&self.request(cmd::LINK_STATUS, &[])?)
    }

    pub fn read_cells(&mut self) -> Result<CellSnapshot> {
        CellSnapshot::from_bytes(&self.request(cmd::CELLS_READ, &[])?)
    }

    pub fn get_interfaces(&mut self) -> Result<InterfaceConfig> {
        InterfaceConfig::from_bytes(&self.request(cmd::IF_GET, &[])?)
    }

    /// Apply an interface configuration.
    ///
    /// The module answers with the configuration that is active afterwards, so
    /// a rejected combination arrives as "nothing changed" rather than as a
    /// refusal the caller has to interpret.
    pub fn set_interfaces(&mut self, modes: &[Mode]) -> Result<InterfaceConfig> {
        let payload: Vec<u8> = modes.iter().map(|mode| mode.as_byte()).collect();
        InterfaceConfig::from_bytes(&self.request(cmd::IF_SET, &payload)?)
    }

    pub fn configure_cells(&mut self, cell_count: u8, poll_ms: u16) -> Result<()> {
        let mut payload = vec![cell_count];
        payload.extend_from_slice(&poll_ms.to_le_bytes());
        self.request(cmd::CELLS_CONFIG, &payload)?;
        Ok(())
    }

    pub fn get_supply(&mut self) -> Result<Supply> {
        Supply::from_bytes(&self.request(cmd::SUPPLY_GET, &[])?)
    }

    /// Switch the external supply on or off and set its voltage.
    ///
    /// The module stores this in flash and re-applies it on its next start, so
    /// one call is enough: there is no separate "save" step.
    pub fn set_supply(&mut self, enabled: bool, millivolts: u16) -> Result<Supply> {
        let mut payload = vec![u8::from(enabled)];
        payload.extend_from_slice(&millivolts.to_le_bytes());
        Supply::from_bytes(&self.request(cmd::SUPPLY_SET, &payload)?)
    }

    pub fn isospi_transfer(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.request(cmd::ISOSPI_XFER, data)
    }

    pub fn simulate_cell(&mut self, index: u8, hundred_uv: u16) -> Result<()> {
        let mut payload = vec![index];
        payload.extend_from_slice(&hundred_uv.to_le_bytes());
        self.request(cmd::SIM_SET, &payload)?;
        Ok(())
    }

    /// The isoSPI role in force. Firmware older than 0.3.0 answers `unknown
    /// command`; the role byte of [`MultibusLink::info`] is the fallback there.
    pub fn get_role(&mut self) -> Result<Role> {
        Role::from_bytes(&self.request(cmd::ROLE_GET, &[])?)
    }

    /// Switch the isoSPI role. Takes effect immediately and is not stored: the
    /// module starts in its build default after a reset, which is why the
    /// service pushes the configured role on every bring-up.
    ///
    /// A role the module cannot take comes back as status `bad parameter`.
    pub fn set_role(&mut self, role: Role) -> Result<Role> {
        Role::from_bytes(&self.request(cmd::ROLE_SET, &[role.as_byte()])?)
    }

    /// USB serial number and chip UID, 0.3.0 and later.
    pub fn identity(&mut self) -> Result<Identity> {
        Identity::from_bytes(&self.request(cmd::IDENTITY, &[])?)
    }

    /// One isoSPI round trip off the slave, timed by the master. Master only:
    /// a slave answers `not supported`, and a powered down transceiver (interface
    /// 5 off) answers `link down`.
    pub fn link_test(&mut self, payload: &[u8]) -> Result<LinkTest> {
        LinkTest::from_bytes(&self.request(cmd::LINK_TEST, payload)?)
    }

    /// The payload, sent back. Checks the protocol port itself, nothing behind it.
    pub fn echo(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        self.request(cmd::ECHO, payload)
    }

    pub fn can_stats(&mut self) -> Result<Vec<CanChannelStats>> {
        CanChannelStats::list_from_bytes(&self.request(cmd::CAN_STATS, &[])?)
    }

    pub fn serial_stats(&mut self) -> Result<Vec<SerialPortStats>> {
        SerialPortStats::list_from_bytes(&self.request(cmd::SERIAL_STATS, &[])?)
    }
}

impl Drop for MultibusLink {
    fn drop(&mut self) {
        self.close();
    }
}
