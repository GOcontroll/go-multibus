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
//!     CONFIGURE    push the interface configuration, the isoSPI role and the
//!       |          external supply over the module bus (SPI), and ask the
//!       |          module who it is. This happens over SPI and not over USB on
//!       |          purpose: the controller has to know which interfaces are
//!       |          real before it initialises them, and USB is not up yet at
//!       |          this point.
//!     ENUMERATE    wait for the USB device and its four CDC ports. The
//!       |          descriptor is fixed, so this always yields the same set of
//!       |          ports regardless of the configuration.
//!     RUNNING      poll the isoSPI cell data over USB and publish it
//! ```
//!
//! Every step is idempotent and the whole sequence is re-entered from the top
//! when the module disappears, which is what makes recovery after a reset or a
//! re-plug automatic rather than something an operator has to do.
//!
//! **One supervisor drives one module.** A controller with several modules
//! runs one per slot, each in its own thread, and they share nothing but the
//! name map. With one module configured a supervisor behaves exactly as it did
//! before there could be more: it takes whichever Multibus device turns up and
//! uses the `mb_` names. With more than one, it learns its module's USB serial
//! number over the module bus during CONFIGURE, records it in the name map so
//! udev names the module's devices after the slot, and in ENUMERATE waits for
//! that one USB device and no other.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{ModuleConfig, ModuleSetup};
use crate::discover::{self, Names, UsbModule};
use crate::error::{Error, Result};
use crate::link::MultibusLink;
use crate::modulebus::{self, Answer, ModuleBus};
use crate::namemap;
use crate::proto::{status, CellSnapshot, Info, LinkStatus, Mode, Role, Supply};
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

/// Why a module whose firmware has no `IDENTITY` cannot run next to another.
const TOO_OLD_TO_TELL_APART: &str =
    "firmware older than 0.3.0 cannot be told apart from another module; update it";

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

    pub const ALL: [State; 6] = [
        State::Reset,
        State::Start,
        State::Configure,
        State::Enumerate,
        State::Running,
        State::Failed,
    ];
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
    pub config: ModuleConfig,
    publish_interval_s: f64,

    /// More than one module configured: the names carry the slot, the
    /// documents are per slot, and the USB device is the one with this
    /// module's serial number rather than whichever turns up.
    multi: bool,
    names: Names,
    output: String,
    state_path: String,

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

    /// The module's USB serial number, as it reported it over the module bus.
    /// Only learned with more than one module configured, where it is what
    /// finds the module's USB device.
    pub usb_serial: Option<String>,
    /// The isoSPI role the module last reported running in.
    pub reported_role: Option<Role>,
    /// Set when the firmware cannot switch role at runtime (older than
    /// 0.3.0), so its role is whatever it was built with.
    pub role_fixed: bool,

    link: Option<MultibusLink>,
    usb_module: Option<UsbModule>,
    /// Set when the module bus would not take something, so the enumerate step
    /// applies it over USB instead.
    configure_over_usb: bool,
    supply_over_usb: bool,
    role_over_usb: bool,
    /// Set once the module has answered "unknown command" to a supply request,
    /// so an older firmware is reported once rather than on every cycle.
    supply_unsupported: bool,
    /// The last thing said about the role, so a lasting disagreement is
    /// logged once rather than on every bring-up.
    role_note: String,

    failing_since: Option<Instant>,
    last_publish: Option<Instant>,
    attempts: usize,
    retry_at: Option<Instant>,

    log_sink: Box<dyn FnMut(&str)>,
}

impl Supervisor {
    pub fn new(setup: ModuleSetup, log_sink: Box<dyn FnMut(&str)>) -> Supervisor {
        let output = setup.output();
        let state_path = setup.state_path();
        let names = setup.names();
        Supervisor {
            config: setup.module,
            publish_interval_s: setup.publish_interval_s,
            multi: setup.multi,
            names,
            output,
            state_path,
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
            usb_serial: None,
            reported_role: None,
            role_fixed: false,
            link: None,
            usb_module: None,
            configure_over_usb: false,
            supply_over_usb: false,
            role_over_usb: false,
            supply_unsupported: false,
            role_note: String::new(),
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
    pub fn attached(setup: ModuleSetup, log_sink: Box<dyn FnMut(&str)>) -> Supervisor {
        let mut supervisor = Supervisor::new(setup, log_sink);
        supervisor.start_state = State::Enumerate;
        supervisor.state = State::Enumerate;
        supervisor.auto_reset = false;
        supervisor.link_timeout = Duration::from_millis(300);
        supervisor.link_retries = 0;
        supervisor.enumeration_timeout = Duration::from_millis(500);
        supervisor
    }

    pub fn slot(&self) -> u8 {
        self.config.slot
    }

    pub fn is_multi(&self) -> bool {
        self.multi
    }

    pub fn names(&self) -> &Names {
        &self.names
    }

    /// Whether the module runs in a role other than the configured one.
    pub fn role_mismatch(&self) -> bool {
        matches!(
            (self.config.isospi_role, self.reported_role),
            (Some(wanted), Some(reported)) if wanted != reported
        )
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
        self.usb_module = None;
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
    ///
    /// The order is interfaces, role, supply, identity. The interfaces decide
    /// what Linux is about to see, so they go first; the identity is only
    /// needed once USB is about to appear, and asking for it last keeps it from
    /// holding up anything that changes the hardware.
    fn do_configure(&mut self) -> Result<()> {
        let modes = self.config.interface_modes()?;
        let bus = ModuleBus::open(self.config.slot)?;

        let Some((accepted, active)) = bus.set_interfaces(&modes)? else {
            if self.multi {
                // With several modules the module bus is the only thing that
                // ties this slot to one of their USB devices, so there is no
                // USB fallback to be had.
                return Err(Error::module(format!(
                    "the application does not answer on the module bus, which is what \
                     tells this slot's USB device apart from the others; {TOO_OLD_TO_TELL_APART}"
                )));
            }

            // Not fatal. Firmware built before ModuleBus.c serves the module bus
            // from the bootloader only, so an application that ignores us here
            // may still speak the protocol over USB. Carry on and configure
            // there instead; the controller learns the interface layout a
            // little later than it would have, which costs nothing because the
            // USB descriptor is fixed either way.
            self.log("no module bus answer; will configure over USB instead");
            self.configure_over_usb = true;
            self.supply_over_usb = true;
            self.role_over_usb = true;
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

        self.configure_role_over_spi(&bus)?;
        self.configure_supply_over_spi(&bus)?;
        if self.multi {
            self.learn_identity_over_spi(&bus)?;
        }

        self.module_running = true;
        self.state = State::Enumerate;
        Ok(())
    }

    /// Push the configured isoSPI role over the module bus.
    ///
    /// The module does not store its role: after every reset it is back in
    /// whatever its firmware starts in, which is master on a hardware v2 build
    /// and slave on a v1 build. So the role is pushed on every bring-up, like
    /// the interfaces. A one-module file that never set a role pushes nothing
    /// and leaves the module as it was before 0.3.0 existed.
    fn configure_role_over_spi(&mut self, bus: &ModuleBus) -> Result<()> {
        let Some(wanted) = self.config.isospi_role else {
            return Ok(());
        };

        match bus.set_role(wanted)? {
            Answer::Reply((true, active)) => {
                self.reported_role = Some(active);
                self.role_fixed = false;
                self.role_over_usb = false;
                self.log(format!("isoSPI role {active}"));
            }
            Answer::Reply((false, active)) => {
                // A refusal is about the hardware, not about the transport, so
                // USB would not do any better.
                self.reported_role = Some(active);
                self.role_over_usb = false;
                self.log(format!(
                    "the module refused isoSPI role {wanted}; it runs as {active}"
                ));
            }
            // Older firmware, or a module bus that went quiet halfway through.
            // USB answers an unknown command properly, so settle it there.
            Answer::Unsupported | Answer::Silent => self.role_over_usb = true,
        }
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

    /// Ask the module for its USB serial number, and put it in the name map.
    ///
    /// Only needed with more than one module, and there it is not optional: the
    /// serial number is the only way to tell which of the USB devices that are
    /// about to appear is this slot's.
    fn learn_identity_over_spi(&mut self, bus: &ModuleBus) -> Result<()> {
        match bus.identity()? {
            Answer::Reply(identity) => {
                self.adopt_serial(identity.serial);
                Ok(())
            }
            Answer::Unsupported => Err(Error::module(TOO_OLD_TO_TELL_APART)),
            Answer::Silent => Err(Error::module(format!(
                "slot {} did not answer IDENTITY on the module bus",
                self.config.slot
            ))),
        }
    }

    /// Take `serial` as this module's, and record it so udev names the
    /// module's devices after its slot.
    ///
    /// Written as early as possible - straight after the module said who it
    /// is - because USB enumerates at around the same moment, and every device
    /// udev reaches before the line is there has to be named a second time.
    fn adopt_serial(&mut self, serial: String) {
        if self.usb_serial.as_deref() != Some(serial.as_str()) {
            self.log(format!(
                "USB serial {serial}, named {}*",
                self.names.prefix()
            ));
        }
        if let Err(failure) = namemap::record(&serial, self.names.prefix()) {
            // Not fatal: the module works without its names, it just gets the
            // wrong ones. The name check after enumeration says so as well.
            self.log(failure.to_string());
        }
        self.usb_serial = Some(serial);
    }

    /// This module's USB serial number, learning it if need be.
    ///
    /// The daemon learns it in CONFIGURE. The UI attaches at ENUMERATE and
    /// never ran that step, so it asks now: the module bus answers while the
    /// application runs, and `IDENTITY` changes nothing. Where the module bus
    /// is not reachable the name map, which the service wrote when it brought
    /// the module up, is the fallback.
    fn known_serial(&mut self) -> Result<String> {
        if let Some(serial) = &self.usb_serial {
            return Ok(serial.clone());
        }

        if let Ok(bus) = ModuleBus::open(self.config.slot) {
            match bus.identity() {
                Ok(Answer::Reply(identity)) => {
                    let serial = identity.serial.clone();
                    self.adopt_serial(identity.serial);
                    return Ok(serial);
                }
                Ok(Answer::Unsupported) => return Err(Error::module(TOO_OLD_TO_TELL_APART)),
                _ => {}
            }
        }

        if let Some(serial) = namemap::serial_for_prefix(self.names.prefix()) {
            self.usb_serial = Some(serial.clone());
            return Ok(serial);
        }

        Err(Error::module(format!(
            "cannot tell which USB device is slot {}'s: the module bus does not answer \
             and the name map has no entry for {}. Resetting the module asks it again.",
            self.config.slot,
            self.names.prefix()
        )))
    }

    /// Why nothing enumerated, naming the OTG port when that is the reason.
    ///
    /// Worth the extra file read at the one moment it is needed: "check that
    /// the port is in host mode" sends people looking at cables and modules,
    /// while the device tree can state outright that the port booted as a
    /// gadget — which no cable, module or reset is going to fix.
    fn enumeration_failure(&self) -> Error {
        let seconds = self.enumeration_timeout.as_secs_f64();
        let subject = match (&self.usb_serial, self.multi) {
            (Some(serial), true) => format!("the module with USB serial {serial}"),
            _ => "the module".to_string(),
        };
        match discover::otg_dr_mode() {
            None => Error::module(format!(
                "{subject} did not enumerate on USB within {seconds:.1}s. Check that \
                 the controller USB port is in host mode."
            )),
            Some(mode) if mode == "host" => Error::module(format!(
                "{subject} did not enumerate on USB within {seconds:.1}s. Check that \
                 the controller USB port is in host mode."
            )),
            Some(mode) => Error::module(format!(
                "{subject} did not enumerate on USB within {seconds:.1}s because the \
                 controller's OTG port booted in {mode} mode, not host. No module can \
                 enumerate until that is fixed: run `gocontroll-usb-hostmode --apply` \
                 and reboot."
            )),
        }
    }

    /// One module: wait for the protocol port of whichever Multibus device
    /// turns up, and open it.
    fn open_only_port(&mut self, deadline: Instant) -> Result<MultibusLink> {
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
        loop {
            match MultibusLink::open_path(
                &discover::find_protocol_port()?,
                self.link_timeout,
                self.link_retries,
            ) {
                Ok(link) => return Ok(link),
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
        }
    }

    /// Several modules: wait for this module's own USB device - the one with
    /// the serial number it reported over the module bus - and open its
    /// protocol port. Any other module's devices are none of this
    /// supervisor's business, however they are named.
    fn open_own_port(&mut self, deadline: Instant) -> Result<MultibusLink> {
        let serial = self.known_serial()?;

        let module = loop {
            if let Some(module) = discover::find_usb_module(&serial) {
                if discover::module_protocol_port(&module).is_some() {
                    break module;
                }
            }
            if Instant::now() >= deadline {
                return Err(self.enumeration_failure());
            }
            std::thread::sleep(ENUMERATION_POLL);
        };

        self.ensure_names(&module);

        // Opening is part of the wait, for the same reason as with one module.
        let link = loop {
            let opened = discover::module_protocol_path(&module, &self.names)
                .ok_or_else(|| {
                    Error::module(format!(
                        "the module with USB serial {serial} lost its protocol port"
                    ))
                })
                .and_then(|path| {
                    MultibusLink::open_path(&path, self.link_timeout, self.link_retries)
                });
            match opened {
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

        self.usb_module = Some(module);
        Ok(link)
    }

    /// Make sure udev named this module's devices after its slot.
    ///
    /// udev can only name a module by the name map, and the module's line only
    /// goes in once the module has told the service its serial number - which
    /// happens at about the moment USB enumerates. Whatever udev reached first
    /// got the single-module names, or kept its kernel name where another
    /// module already had those. That is put right here, by having udev
    /// process exactly those devices again now that the map has the line. The
    /// CAN interfaces are still down at this point, which is what lets a
    /// rename succeed; nothing of another module is touched.
    fn ensure_names(&mut self, module: &UsbModule) {
        if !discover::udev_names_expected() {
            return;
        }
        run_bounded("udevadm", &["settle", "--timeout=5"], Duration::from_secs(10));

        let wrong = discover::misnamed(module, &self.names);
        if wrong.is_empty() {
            return;
        }

        let listed: Vec<String> = wrong
            .iter()
            .map(|device| format!("{} -> {}", device.current, device.expected))
            .collect();
        self.log(format!(
            "renaming what udev named before the name map had this module: {}",
            listed.join(", ")
        ));

        let mut args: Vec<&str> = vec!["trigger", "--action=add"];
        args.extend(wrong.iter().map(|device| device.sysfs.as_str()));
        run_bounded("udevadm", &args, Duration::from_secs(10));
        run_bounded("udevadm", &["settle", "--timeout=5"], Duration::from_secs(10));

        let still: Vec<String> = discover::misnamed(module, &self.names)
            .iter()
            .map(|device| format!("{} (should be {})", device.current, device.expected))
            .collect();
        if !still.is_empty() {
            self.log(format!(
                "udev did not rename {}; check {} and 81-gocontroll-multibus.rules",
                still.join(", "),
                namemap::RUN_PATH
            ));
        }
    }

    /// Confirm over USB that the port belongs to the module in this slot.
    ///
    /// The device was found by the serial number the module bus reported, so
    /// a disagreement here means the map or sysfs pointed somewhere else -
    /// and driving another slot's module as this one would configure the wrong
    /// hardware. Refused outright.
    fn check_identity_over_usb(&mut self, link: &mut MultibusLink) -> Result<()> {
        let expected = self.usb_serial.clone().unwrap_or_default();
        let identity = link.identity().map_err(|failure| {
            if failure.is_unknown_command() {
                Error::module(TOO_OLD_TO_TELL_APART)
            } else {
                failure
            }
        })?;
        if identity.serial != expected {
            return Err(Error::module(format!(
                "{} answers as USB serial {}, but slot {} has {expected}; not using it",
                link.device(),
                identity.serial,
                self.config.slot
            )));
        }
        Ok(())
    }

    /// Apply the role over USB when SPI could not, then read back what the
    /// module runs as.
    ///
    /// A firmware older than 0.3.0 answers "unknown command" to both role
    /// commands. Its role is then the one it was built with, which the INFO
    /// answer reports; a disagreement with the configuration is logged once and
    /// shown, not treated as a failure, because everything but the cell data
    /// works in either role.
    fn settle_role(&mut self, link: &mut MultibusLink, info: &Info) -> Result<()> {
        let Some(wanted) = self.config.isospi_role else {
            // No role configured: report what the firmware started in, change
            // nothing - exactly what happened before 0.3.0.
            self.reported_role = info.role();
            return Ok(());
        };

        if self.role_over_usb {
            match link.set_role(wanted) {
                Ok(active) => {
                    self.role_fixed = false;
                    self.log(format!("isoSPI role {active} (over USB)"));
                }
                Err(failure) if failure.is_unknown_command() => self.role_fixed = true,
                Err(failure) if failure.is_status(status::BAD_PARAMETER) => {
                    self.log(format!("the module refused isoSPI role {wanted}"))
                }
                Err(failure) => return Err(failure),
            }
            self.role_over_usb = false;
        }

        self.reported_role = match link.get_role() {
            Ok(role) => {
                self.role_fixed = false;
                Some(role)
            }
            Err(failure) if failure.is_unknown_command() => {
                self.role_fixed = true;
                info.role()
            }
            Err(failure) => return Err(failure),
        };

        let note = match self.reported_role {
            Some(role) if role != wanted && self.role_fixed => format!(
                "the module runs as isoSPI {role} and its firmware cannot switch to {wanted}; \
                 flash 0.3.0 or newer"
            ),
            Some(role) if role != wanted => {
                format!("the module runs as isoSPI {role}, configured is {wanted}")
            }
            _ => String::new(),
        };
        if note != self.role_note {
            if !note.is_empty() {
                self.log(&note);
            }
            self.role_note = note;
        }
        Ok(())
    }

    /// Wait for USB, then open the protocol port and finish the configuration.
    fn do_enumerate(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.enumeration_timeout;

        let mut link = if self.multi {
            self.open_own_port(deadline)?
        } else {
            self.open_only_port(deadline)?
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

        if self.multi {
            self.check_identity_over_usb(&mut link)?;
        }

        self.settle_role(&mut link, &info)?;

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

    /// This module's CAN interfaces in channel order, once udev has given them
    /// the names they should have - or as they are when the deadline passes.
    fn own_can_interfaces(&self, module: &UsbModule) -> Vec<String> {
        let deadline = Instant::now() + UDEV_SETTLE;
        loop {
            let present = discover::module_can_interfaces(module);
            let named = present
                .iter()
                .enumerate()
                .all(|(channel, name)| *name == self.names.can(channel));
            if (present.len() >= 3 && named)
                || !discover::udev_names_expected()
                || Instant::now() >= deadline
            {
                return present;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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

        // With several modules only this module's own netdevs are candidates,
        // found under its USB device; the system-wide lookup would find every
        // module's.
        let own = match (&self.usb_module, self.multi) {
            (Some(module), true) => Some(module.clone()),
            _ => None,
        };
        let interfaces = match &own {
            Some(module) => self.own_can_interfaces(module),
            None => discover::enumerate_can_interfaces(UDEV_SETTLE),
        };

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

            // A netdev still on the wrong name is about to be renamed, or
            // failed to be. Raising it would block the rename for good, since
            // an interface cannot be renamed while it is up.
            if own.is_some() && discover::udev_names_expected() {
                let expected = self.names.can(channel);
                if name != expected {
                    self.log(format!("{name} is not named {expected}; not touching it"));
                    continue;
                }
            }

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
        let interval = Duration::from_secs_f64(self.publish_interval_s.max(0.0));
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
        shm::write_atomic(&self.output, &document)?;

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
        let _ = shm::write_atomic(&self.output, &document);

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
        let mut document = shm::state_document(
            self.state.as_str(),
            self.config.slot,
            self.module_running,
            &self.applied_interfaces,
            self.supply.as_ref(),
            self.failures,
            &self.last_error,
        );
        // With one module the document stays exactly as it was before 0.3.0,
        // so nothing that reads it has to change.
        if self.multi {
            document["isospi_role"] = serde_json::json!({
                "configured": self.config.isospi_role.map(Role::as_str),
                "reported": self.reported_role.map(Role::as_str),
            });
            document["usb_serial"] = serde_json::json!(self.usb_serial);
        }
        let _ = shm::write_atomic(&self.state_path, &document);
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

    /// Switch the isoSPI role of a module that is already running.
    ///
    /// No reset either: the module switches on the spot. It does not store
    /// the role, but the configuration does, and every bring-up pushes it.
    pub fn apply_role(&mut self, role: Role) -> Result<Role> {
        let Some(link) = self.link.as_mut() else {
            return Err(Error::module(
                "the module is not reachable; the role is stored and will be \
                 applied on the next bring-up",
            ));
        };

        match link.set_role(role) {
            Ok(active) => {
                self.reported_role = Some(active);
                self.role_fixed = false;
                self.role_note.clear();
                Ok(active)
            }
            Err(failure) if failure.is_unknown_command() => {
                self.role_fixed = true;
                Err(Error::module(
                    "this firmware cannot switch its isoSPI role; flash 0.3.0 or newer. \
                     The role is stored and pushed once it can.",
                ))
            }
            Err(failure) if failure.is_status(status::BAD_PARAMETER) => {
                let active = link.get_role().ok();
                self.reported_role = active.or(self.reported_role);
                Err(Error::module(match active {
                    Some(active) => format!("the module refused {role}; it runs as {active}"),
                    None => format!("the module refused {role}"),
                }))
            }
            Err(failure) => Err(failure),
        }
    }
}
