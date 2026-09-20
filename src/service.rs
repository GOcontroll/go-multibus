//! Bring-up and supervision of a Multibus module.
//!
//! The daemon and the UI both drive a module through this state machine, so it
//! lives here rather than in either of them. What the UI shows is what the
//! service does, because both run this code.
//!
//! The sequence a module goes through after a controller boot or a reset:
//!
//! ```text
//!     RESET        pulse the slot reset line
//!       |          the STM32 comes up in the GOcontroll bootloader
//!     START        tell the bootloader its firmware is current, so it jumps to
//!       |          the application. Without this the module sits in the
//!       |          bootloader forever and never enumerates on USB.
//!     CONFIGURE    push the interface configuration and the external supply
//!       |          over the module bus (SPI). This happens over SPI and not
//!       |          over USB on purpose: the controller has to know which
//!       |          interfaces are real before it initialises them, and USB is
//!       |          not up yet at this point.
//!     ENUMERATE    wait for the USB device and its four CDC ports. The
//!       |          descriptor is fixed, so this always yields the same set of
//!       |          ports regardless of the configuration.
//!     RUNNING      poll the isoSPI cell data over USB and publish it
//! ```
//!
//! Every step is idempotent and the whole sequence is re-entered from the top
//! when the module disappears, which is what makes recovery after a reset or a
//! re-plug automatic rather than something an operator has to do.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, STATE_PATH};
use crate::discover;
use crate::error::{Error, Result};
use crate::link::MultibusLink;
use crate::modulebus::{self, ModuleBus};
use crate::proto::{CellSnapshot, LinkStatus, Mode, Supply};
use crate::shm;

/// How long to wait for USB to enumerate after the application starts. The
/// STM32 has to run its init, attach, and be enumerated by the host; on a
/// Moduline that settles in well under two seconds, and five leaves room for a
/// slow boot.
pub const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to let udev finish naming the module's devices after they appear.
/// Renaming three netdevs and adding four symlinks takes a few tens of
/// milliseconds on a Moduline; two seconds is a ceiling, not a wait.
const UDEV_SETTLE: Duration = Duration::from_secs(2);
const ENUMERATION_POLL: Duration = Duration::from_millis(200);

/// A module that stops answering is given this long before the whole sequence
/// is restarted from RESET. Short enough to recover quickly, long enough that a
/// single dropped poll does not trigger a reset.
const FAILURE_GRACE: Duration = Duration::from_secs(3);

/// How long to wait before starting the bring-up sequence over, indexed by how
/// many times in a row it has now failed. Without a back-off, a controller
/// whose OTG port is still in peripheral mode resets the module every six
/// seconds indefinitely — a hundred resets in seven minutes, measured on a
/// Moduline L4 — which buries the one line that says what is actually wrong
/// under copies of itself. The last entry is the ceiling.
const RETRY_BACKOFF: [Duration; 5] = [
    Duration::from_secs(0),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum State {
    Reset,
    Start,
    Configure,
    Enumerate,
    Running,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Reset => "reset",
            State::Start => "start",
            State::Configure => "configure",
            State::Enumerate => "enumerate",
            State::Running => "running",
            State::Failed => "failed",
        }
    }

    /// What the UI and `status` print, in words rather than as a state name.
    pub fn label(self) -> &'static str {
        match self {
            State::Reset => "resetting the module",
            State::Start => "starting the application",
            State::Configure => "configuring interfaces over SPI",
            State::Enumerate => "waiting for USB",
            State::Running => "running",
            State::Failed => "not reachable - see the log below",
        }
    }
}

/// Run a command with a ceiling on how long it may take.
///
/// The daemon must not be able to park forever inside a helper it shelled out
/// to, which `Command::output()` on its own would allow.
fn run_bounded(program: &str, args: &[&str], limit: Duration) -> Option<std::process::ExitStatus> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub struct Supervisor {
    pub config: Config,

    /// Where the sequence begins. The daemon starts at RESET because a
    /// controller boot should leave the module in a known state. The UI starts
    /// at ENUMERATE, so that merely opening it to look at a running machine
    /// does not reset the module underneath it.
    start_state: State,
    /// Whether a failure may restart the sequence from RESET on its own. The
    /// UI must not reset hardware without being asked.
    auto_reset: bool,

    link_timeout: Duration,
    link_retries: u32,
    enumeration_timeout: Duration,

    pub state: State,
    pub last_error: String,
    pub failures: u32,
    pub module_running: bool,
    pub applied_interfaces: Vec<Mode>,
    pub supply: Option<Supply>,
    pub snapshot: Option<CellSnapshot>,
    pub link_status: Option<LinkStatus>,

    link: Option<MultibusLink>,
    /// Set when the module bus would not take something, so the enumerate step
    /// applies it over USB instead.
    configure_over_usb: bool,
    supply_over_usb: bool,
    /// Set once the module has answered "unknown command" to a supply request,
    /// so an older firmware is reported once rather than on every cycle.
    supply_unsupported: bool,

    failing_since: Option<Instant>,
    last_publish: Option<Instant>,
    attempts: usize,
    retry_at: Option<Instant>,

    log_sink: Box<dyn FnMut(&str)>,
}

impl Supervisor {
    pub fn new(config: Config, log_sink: Box<dyn FnMut(&str)>) -> Supervisor {
        Supervisor {
            config,
            start_state: State::Reset,
            auto_reset: true,
            link_timeout: crate::link::DEFAULT_TIMEOUT,
            link_retries: crate::link::DEFAULT_RETRIES,
            enumeration_timeout: ENUMERATION_TIMEOUT,
            state: State::Reset,
            last_error: String::new(),
            failures: 0,
            module_running: false,
            applied_interfaces: Vec::new(),
            supply: None,
            snapshot: None,
            link_status: None,
            link: None,
            configure_over_usb: false,
            supply_over_usb: false,
            supply_unsupported: false,
            failing_since: None,
            last_publish: None,
            attempts: 0,
            retry_at: None,
            log_sink,
        }
    }

    /// A supervisor for the UI: it attaches to a module that is already running
    /// instead of resetting one, and never resets hardware on its own.
    ///
    /// The shorter link timeout is the point of the distinction: the daemon can
    /// afford to wait, the UI cannot, because every second spent blocking is a
    /// second it does not repaint or read the keyboard.
    pub fn attached(config: Config, log_sink: Box<dyn FnMut(&str)>) -> Supervisor {
        let mut supervisor = Supervisor::new(config, log_sink);
        supervisor.start_state = State::Enumerate;
        supervisor.state = State::Enumerate;
        supervisor.auto_reset = false;
        supervisor.link_timeout = Duration::from_millis(300);
        supervisor.link_retries = 0;
        supervisor.enumeration_timeout = Duration::from_millis(500);
        supervisor
    }

    fn log(&mut self, message: impl AsRef<str>) {
        (self.log_sink)(message.as_ref());
    }

    pub fn close(&mut self) {
        self.link = None;
    }

    /// Go back to the start of the sequence.
    ///
    /// `full` forces a hardware reset even for a supervisor that normally
    /// attaches without one; that is what the UI's restart key asks for. It
    /// also clears any back-off: someone asking for a restart by hand has
    /// usually just changed something, and should not be made to wait out a
    /// delay earned before they did.
    pub fn restart(&mut self, full: bool) {
        self.close();
        if full {
            self.attempts = 0;
            self.retry_at = None;
        }
        self.state = if full { State::Reset } else { self.start_state };
        self.module_running = false;
        self.snapshot = None;
        self.failing_since = None;
    }

    // -- individual steps ----------------------------------------------------

    fn do_reset(&mut self) -> Result<()> {
        let slot = self.config.slot;
        self.log(format!("resetting slot {slot}"));
        modulebus::pulse_reset(slot)?;
        self.state = State::Start;
        Ok(())
    }

    fn do_start(&mut self) -> Result<()> {
        let slot = self.config.slot;
        let bus = ModuleBus::open(slot)?;

        if bus.identify()?.is_none() {
            return Err(Error::module(format!(
                "slot {slot} does not answer on the module bus. Either nothing is \
                 in the slot, or the reset line does not reach the STM32."
            )));
        }

        bus.start_application()?;
        self.log("told the bootloader to start the application");
        self.state = State::Configure;
        Ok(())
    }

    /// Push the configuration over SPI, before USB comes up.
    fn do_configure(&mut self) -> Result<()> {
        let modes = self.config.interface_modes()?;
        let bus = ModuleBus::open(self.config.slot)?;

        let Some((accepted, active)) = bus.set_interfaces(&modes)? else {
            // Not fatal. Firmware built before ModuleBus.c serves the module bus
            // from the bootloader only, so an application that ignores us here
            // may still speak the protocol over USB. Carry on and configure
            // there instead; the controller learns the interface layout a
            // little later than it would have, which costs nothing because the
            // USB descriptor is fixed either way.
            self.log("no module bus answer; will configure over USB instead");
            self.configure_over_usb = true;
            self.supply_over_usb = true;
            self.state = State::Enumerate;
            return Ok(());
        };

        if !accepted {
            let wanted: Vec<&str> = modes.iter().map(|mode| mode.as_str()).collect();
            return Err(Error::module(format!(
                "the module refused the interface combination {}",
                wanted.join(",")
            )));
        }

        self.applied_interfaces = active;
        self.configure_over_usb = false;
        let applied: Vec<&str> = self
            .applied_interfaces
            .iter()
            .map(|mode| mode.as_str())
            .collect();
        self.log(format!("interfaces configured over SPI: {}", applied.join(",")));

        self.configure_supply_over_spi(&bus)?;

        self.module_running = true;
        self.state = State::Enumerate;
        Ok(())
    }

    /// Apply the stored supply setting over the module bus.
    ///
    /// The module keeps its own copy in flash, so it has already restored
    /// whatever it had before the reset by the time this runs. Pushing anyway
    /// is what keeps the controller authoritative: the two only differ when
    /// someone changed the file, and then the file should win.
    fn configure_supply_over_spi(&mut self, bus: &ModuleBus) -> Result<()> {
        let enabled = self.config.supply_enabled;
        let millivolts = self.config.supply_mv;

        match bus.set_supply(enabled, millivolts)? {
            Some((true, active)) => {
                self.supply = Some(active);
                self.supply_over_usb = false;
                self.log(format!("external supply {}", active.summary()));
            }
            Some((false, active)) => {
                // A refusal here is about the value, not about the transport,
                // so there is nothing USB would do differently.
                self.supply = Some(active);
                self.supply_over_usb = false;
                self.log(format!(
                    "the module refused a supply setpoint of {}; it kept {}",
                    crate::proto::format_volts(millivolts),
                    active.summary()
                ));
            }
            None => {
                // Older firmware: the message id did not exist yet. Try over
                // USB, which reports an unknown command properly.
                self.supply_over_usb = true;
            }
        }
        Ok(())
    }

    /// Why nothing enumerated, naming the OTG port when that is the reason.
    ///
    /// Worth the extra file read at the one moment it is needed: "check that
    /// the port is in host mode" sends people looking at cables and modules,
    /// while the device tree can state outright that the port booted as a
    /// gadget — which no cable, module or reset is going to fix.
    fn enumeration_failure(&self) -> Error {
        let seconds = self.enumeration_timeout.as_secs_f64();
        match discover::otg_dr_mode() {
            None => Error::module(format!(
                "the module did not enumerate on USB within {seconds:.1}s. Check that \
                 the controller USB port is in host mode."
            )),
            Some(mode) if mode == "host" => Error::module(format!(
                "the module did not enumerate on USB within {seconds:.1}s. Check that \
                 the controller USB port is in host mode."
            )),
            Some(mode) => Error::module(format!(
                "the module did not enumerate on USB within {seconds:.1}s because the \
                 controller's OTG port booted in {mode} mode, not host. No module can \
                 enumerate until that is fixed: run `gocontroll-usb-hostmode --apply` \
                 and reboot."
            )),
        }
    }

    /// Wait for USB, then open the protocol port and finish the configuration.
    fn do_enumerate(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.enumeration_timeout;

        loop {
            if discover::enumerate_ports()
                .iter()
                .any(|port| port.is_protocol_port)
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(self.enumeration_failure());
            }
            std::thread::sleep(ENUMERATION_POLL);
        }

        // Same race as the CAN names: the ttyACM nodes exist before udev has
        // added /dev/mb_protocol. Waiting means the log names the stable device
        // rather than whichever ttyACM number this enumeration happened to get.
        let settle = Instant::now() + UDEV_SETTLE;
        while !std::path::Path::new(discover::PROTOCOL_SYMLINK).exists()
            && Instant::now() < settle
        {
            std::thread::sleep(Duration::from_millis(100));
        }

        // Opening is part of the wait, not a step after it. Right after a reset
        // the device nodes from before it can still be around for a moment: the
        // name exists, but opening it gives ENODEV because the USB device
        // behind it is gone. Treating that as a hard failure threw the whole
        // sequence away and started over - visible at boot as an "enumerate
        // failed: No such device" followed by a second, successful pass.
        // Retrying until the deadline rides it out.
        let mut link = loop {
            match MultibusLink::open_path(
                &discover::find_protocol_port()?,
                self.link_timeout,
                self.link_retries,
            ) {
                Ok(link) => break link,
                Err(failure) => {
                    if Instant::now() >= deadline {
                        return Err(Error::module(format!(
                            "the protocol port did not become usable within {:.1}s ({failure})",
                            self.enumeration_timeout.as_secs_f64()
                        )));
                    }
                    std::thread::sleep(ENUMERATION_POLL);
                }
            }
        };

        let device = link.device().to_string();
        self.log(format!("connected to {device}"));

        // The port existing proves the module enumerated, so a failure from
        // here on is about what the firmware serves rather than about USB. Say
        // that, because "not reachable" sends people looking at cables when the
        // real answer is that the module is running a build from before the
        // protocol existed - which is exactly what a stock 0.0.1 image does.
        let info = link.info().map_err(|failure| {
            Error::module(format!(
                "{device} enumerated but does not answer the module protocol ({failure}). \
                 The firmware predates MbProto - on such a build this port is a plain \
                 RS485 bridge. Flash a current image."
            ))
        })?;

        self.log(format!(
            "module is a {}, hardware {}, software {}",
            info.role_name(),
            info.hardware_name(),
            info.software_name()
        ));

        // Fall back to configuring over USB when the module bus would not take
        // it. Failing here is fatal: at this point the module clearly does
        // speak the protocol, so a refusal is a real answer about the
        // configuration.
        if self.configure_over_usb {
            let wanted = self.config.interface_modes()?;
            let applied = link.set_interfaces(&wanted)?;
            if applied.modes != wanted {
                let asked: Vec<&str> = wanted.iter().map(|mode| mode.as_str()).collect();
                return Err(Error::module(format!(
                    "the module refused the interface combination {}; it kept {}",
                    asked.join(","),
                    applied.joined()
                )));
            }
            self.log(format!("interfaces configured over USB: {}", applied.joined()));
            self.configure_over_usb = false;
        }

        link.configure_cells(self.config.cell_count, self.config.module_poll_ms)?;

        // Read back what the module actually has, which also covers the case
        // where the SPI configuration did not land.
        self.applied_interfaces = link.get_interfaces()?.modes;

        self.link = Some(link);
        self.settle_supply_over_usb()?;

        if self.config.bring_up_can {
            self.bring_up_can()?;
        }

        self.state = State::Running;
        self.failures = 0;
        self.failing_since = None;
        Ok(())
    }

    /// Apply the supply over USB when SPI could not, and read back either way.
    ///
    /// A module whose firmware predates the supply commands answers "unknown
    /// command". That is reported once and then left alone: everything else
    /// about such a module works, and failing the bring-up over it would take a
    /// working machine down for a feature it does not have.
    fn settle_supply_over_usb(&mut self) -> Result<()> {
        if self.supply_unsupported {
            return Ok(());
        }

        let enabled = self.config.supply_enabled;
        let millivolts = self.config.supply_mv;
        let push = self.supply_over_usb;

        let Some(link) = self.link.as_mut() else {
            return Ok(());
        };

        let outcome = if push {
            link.set_supply(enabled, millivolts)
        } else {
            link.get_supply()
        };

        match outcome {
            Ok(supply) => {
                self.supply = Some(supply);
                self.supply_over_usb = false;
                if push {
                    self.log(format!("external supply {}", supply.summary()));
                }
            }
            Err(failure) if failure.is_unknown_command() => {
                self.supply_unsupported = true;
                self.supply = None;
                self.supply_over_usb = false;
                self.log(
                    "this firmware has no external supply control; \
                     flash 0.2.0 or newer to configure it",
                );
            }
            Err(failure) => return Err(failure),
        }
        Ok(())
    }

    /// Bring up only the CAN interfaces that have a transceiver behind them.
    ///
    /// The bus parameters are go-can's to decide, not ours — it is the
    /// canonical CAN configuration tool on these controllers and keeps them in
    /// `/etc/gocontroll/can.d/`. Setting a bitrate here as well would make the
    /// two tools disagree: go-can would keep reporting its stored value while
    /// the interface actually ran at ours.
    fn bring_up_can(&mut self) -> Result<()> {
        // Wait for udev's queue to drain before looking. Polling for the names
        // races with the renames; this waits for the events themselves.
        run_bounded("udevadm", &["settle", "--timeout=5"], Duration::from_secs(10));

        let interfaces = discover::enumerate_can_interfaces(UDEV_SETTLE);
        if interfaces.len() < 3 {
            let found = if interfaces.is_empty() {
                "none".to_string()
            } else {
                interfaces.join(", ")
            };
            self.log(format!(
                "expected 3 gs_usb interfaces, found {} ({found}); not touching CAN",
                interfaces.len()
            ));
            return Ok(());
        }

        for channel in self.config.can_channels()? {
            let name = interfaces[channel].clone();
            if self.apply_can_with_go_can(&name) {
                continue;
            }
            self.apply_can_with_ip(&name);
        }
        Ok(())
    }

    /// Let go-can configure and raise the interface. False if it could not.
    ///
    /// Fails when go-can is absent, or when it has no config for this interface
    /// yet — a fresh controller that has never had a module in it. The caller
    /// falls back in both cases.
    fn apply_can_with_go_can(&mut self, name: &str) -> bool {
        let Some(status) = run_bounded("go-can", &["apply", name], Duration::from_secs(15)) else {
            return false;
        };
        if !status.success() {
            return false;
        }
        self.log(format!("{name} up, configured by go-can"));
        true
    }

    /// Raise the interface directly, for a controller without go-can.
    fn apply_can_with_ip(&mut self, name: &str) {
        let bitrate = self.config.can_bitrate.to_string();
        run_bounded("ip", &["link", "set", name, "down"], Duration::from_secs(10));

        let raised = run_bounded(
            "ip",
            &["link", "set", name, "up", "type", "can", "bitrate", &bitrate],
            Duration::from_secs(10),
        );

        match raised {
            Some(status) if status.success() => {
                self.log(format!("{name} up at {bitrate} bit/s (go-can not available)"))
            }
            // A CAN interface that will not come up is worth reporting but must
            // not stop the cell data from being published.
            _ => self.log(format!("could not bring up {name}")),
        }
    }

    fn do_running(&mut self) -> Result<()> {
        let interval = Duration::from_secs_f64(self.config.publish_interval_s.max(0.0));
        if let Some(last) = self.last_publish {
            if last.elapsed() < interval {
                return Ok(());
            }
        }
        self.last_publish = Some(Instant::now());

        let Some(link) = self.link.as_mut() else {
            return Err(Error::module("the protocol port is not open"));
        };

        let snapshot = link.read_cells()?;
        let status = link.link_status()?;
        let device = link.device().to_string();

        self.snapshot = Some(snapshot.clone());
        self.link_status = Some(status);

        let mut document = shm::snapshot_document(
            &snapshot,
            &status,
            &device,
            &self.applied_interfaces,
            self.supply.as_ref(),
        );
        document["errors"] = serde_json::json!({
            "read_failures": self.failures,
            "last_error": self.last_error,
        });
        shm::write_atomic(&self.config.output, &document)?;

        self.failures = 0;
        self.last_error.clear();
        self.failing_since = None;
        Ok(())
    }

    // -- the step the caller drives ------------------------------------------

    /// Advance the state machine by one bounded piece of work.
    pub fn step(&mut self) {
        // Sitting out a back-off after a failed attempt. Returning rather than
        // sleeping keeps a step non-blocking, which is what lets the UI call
        // this from its own loop without going deaf to the keyboard.
        if let Some(retry_at) = self.retry_at {
            if Instant::now() < retry_at {
                return;
            }
        }

        let outcome = match self.state {
            State::Reset => self.do_reset(),
            State::Start => self.do_start(),
            State::Configure => self.do_configure(),
            State::Enumerate => self.do_enumerate(),
            State::Running => self.do_running(),
            State::Failed => {
                // Stay put until asked to retry, unless this supervisor is
                // allowed to recover on its own.
                if self.auto_reset {
                    self.restart(false);
                }
                self.publish_state();
                return;
            }
        };

        if let Err(failure) = outcome {
            self.handle_failure(failure);
        } else if self.state == State::Running {
            // Getting as far as RUNNING clears the back-off, so a module that
            // drops out later is picked up straight away instead of inheriting
            // the delay earned by some earlier problem.
            self.attempts = 0;
            self.retry_at = None;
        }

        self.publish_state();
    }

    fn handle_failure(&mut self, failure: Error) {
        self.failures += 1;

        let previous = std::mem::take(&mut self.last_error);
        self.last_error = failure.to_string();

        // Logged once per run of identical failures. A cause that is not going
        // to clear on its own - an OTG port in the wrong role, an empty slot -
        // fails the same way every cycle, and repeating it buries the first and
        // only useful copy. The state file still carries the current error for
        // anything that polls.
        if self.last_error != previous {
            let line = format!("{} failed: {}", self.state.as_str(), self.last_error);
            self.log(line);
        }

        let now = Instant::now();
        if self.failing_since.is_none() {
            self.failing_since = Some(now);
        }

        self.close();

        // Publish the failure: a reader that only ever sees a stale file cannot
        // tell a dead service from a dead battery link.
        let document =
            shm::failure_document(self.state.as_str(), self.failures, &self.last_error);
        let _ = shm::write_atomic(&self.config.output, &document);

        // Failing inside RUNNING gets a grace period, because a single dropped
        // poll is not a reason to reset a working module. Failing during
        // bring-up restarts the sequence straight away.
        if self.state == State::Running {
            if let Some(since) = self.failing_since {
                if now.duration_since(since) < FAILURE_GRACE {
                    self.state = State::Enumerate;
                    return;
                }
            }
        }

        if !self.auto_reset {
            // The UI must not reset hardware on its own; park in a state the
            // operator can see and let them press the restart key.
            self.state = State::Failed;
            return;
        }

        self.back_off();
        self.restart(false);
    }

    /// Hold off the next bring-up attempt, and say so while it is growing.
    fn back_off(&mut self) {
        let delay = RETRY_BACKOFF[self.attempts.min(RETRY_BACKOFF.len() - 1)];
        // Only while the delay is still climbing: once it is at the ceiling the
        // message would repeat forever, which is the thing being avoided.
        if !delay.is_zero() && self.attempts < RETRY_BACKOFF.len() {
            let seconds = delay.as_secs();
            self.log(format!("retrying in {seconds}s"));
        }
        self.attempts += 1;
        self.retry_at = Some(Instant::now() + delay);
    }

    fn publish_state(&mut self) {
        let document = shm::state_document(
            self.state.as_str(),
            self.config.slot,
            self.module_running,
            &self.applied_interfaces,
            self.supply.as_ref(),
            self.failures,
            &self.last_error,
        );
        let _ = shm::write_atomic(STATE_PATH, &document);
    }

    // -- operations the UI drives --------------------------------------------

    /// Apply a supply setting to a module that is already running.
    ///
    /// Unlike an interface change this needs no reset: the module takes the new
    /// setpoint immediately and stores it, so it survives the next one as well.
    pub fn apply_supply(&mut self, enabled: bool, millivolts: u16) -> Result<Supply> {
        let Some(link) = self.link.as_mut() else {
            return Err(Error::module(
                "the module is not reachable; the setting is stored and will be \
                 applied on the next bring-up",
            ));
        };

        let supply = link.set_supply(enabled, millivolts)?;
        self.supply = Some(supply);
        Ok(supply)
    }
}
