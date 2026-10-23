//! Finding a Multibus module's devices on a controller.
//!
//! Nothing here assumes a device name. Kernel names depend on the controller —
//! a Moduline L4 has four onboard mcp251x controllers so a module's CAN
//! channels land on `can4..can6`, while an M1 has two and the same module lands
//! on `can2..can4`, and `ttyACM` numbers follow enumeration order. Everything
//! is therefore looked up: ttys by their USB interface number, CAN interfaces
//! by the names the udev rule installs with a sysfs fallback that recovers the
//! channel order from `ifindex`.
//!
//! Two sets of lookups live here. The whole-system ones - [`enumerate_ports`],
//! [`enumerate_can_interfaces`], [`find_protocol_port`] - assume one module and
//! are what a single-module controller has always used. The per-module ones
//! start from a USB serial number - [`find_usb_module`], [`module_ports`],
//! [`module_can_interfaces`] - and are what a controller with several modules
//! has to use, because there every module has the same vendor and product id
//! and the same interface layout, and only the serial number tells them apart.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// candleLight ids, so Linux binds gs_usb to the CAN interfaces by itself.
pub const USB_VENDOR_ID: &str = "1d50";
pub const USB_PRODUCT_ID: &str = "606f";

/// The protocol rides on CDC port 3, whose USB control interface is number 7.
pub const PROTOCOL_INTERFACE: u8 = 7;

/// Symlink the udev rule installs for the protocol port. Preferred over the
/// ttyACM node, whose number depends on enumeration order.
pub const PROTOCOL_SYMLINK: &str = "/dev/mb_protocol";

/// Interface name per gs_usb channel, installed by
/// `81-gocontroll-multibus.rules`. Numbered CAN 1..3 as the knowledge base
/// does, which is **not** the connector interface numbering: CAN 2 is connector
/// interface 3 and CAN 3 is connector interface 4.
pub const CAN_INTERFACE_NAMES: [&str; 3] = ["mb_can1", "mb_can2", "mb_can3"];

/// Connector interface each gs_usb channel sits on, for reporting.
pub const CAN_CHANNEL_INTERFACE: [u8; 3] = [1, 3, 4];

/// Where the naming rule can be installed. Its presence means the kernel names
/// are temporary — udev is about to replace them — so they must not be used.
const UDEV_RULE_PATHS: [&str; 3] = [
    "/etc/udev/rules.d/81-gocontroll-multibus.rules",
    "/usr/lib/udev/rules.d/81-gocontroll-multibus.rules",
    "/lib/udev/rules.d/81-gocontroll-multibus.rules",
];

/// The device tree node that says which role the controller's OTG port booted
/// in. A Multibus module hangs off that port, so a port that came up as a
/// gadget can never enumerate one however often the module is reset.
pub const OTG_DR_MODE_PATH: &str = "/proc/device-tree/soc@0/bus@32c00000/usb@32e40000/dr_mode";

/// Where the USB core lists every device and interface.
const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// The prefix every Multibus device name starts with on a controller with one
/// module: `mb_can1`, `/dev/mb_serial1`, `/dev/mb_protocol`.
pub const LEGACY_PREFIX: &str = "mb_";

/// The names udev gives one module's devices.
///
/// One module gets the names it has always had, `mb_can1..3`,
/// `/dev/mb_serial1..3` and `/dev/mb_protocol`. With more than one module
/// configured those would collide, so each module's names carry its slot
/// instead: `mb7_can1`, `/dev/mb7_serial1`, `/dev/mb7_protocol`. The longest,
/// `mb8_can3`, is well inside the 15 characters a network interface name may
/// have.
///
/// The prefix is the whole difference, which is why it is what the name map
/// stores and what the udev helper prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    prefix: String,
}

impl Names {
    pub fn legacy() -> Names {
        Names {
            prefix: LEGACY_PREFIX.to_string(),
        }
    }

    pub fn for_slot(slot: u8) -> Names {
        Names {
            prefix: format!("mb{slot}_"),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Network interface name of a gs_usb channel, 0 based.
    pub fn can(&self, channel: usize) -> String {
        format!("{}can{}", self.prefix, channel + 1)
    }

    /// Symlink for a serial bridge, numbered 1..3 as the knowledge base does.
    pub fn serial(&self, number: u8) -> String {
        format!("/dev/{}serial{number}", self.prefix)
    }

    pub fn protocol(&self) -> String {
        format!("/dev/{}protocol", self.prefix)
    }

    /// The symlink the udev rule gives the tty on a CDC control interface.
    pub fn tty_link(&self, usb_interface: u8) -> Option<String> {
        match usb_interface {
            1 => Some(self.serial(1)),
            3 => Some(self.serial(2)),
            5 => Some(self.serial(3)),
            PROTOCOL_INTERFACE => Some(self.protocol()),
            _ => None,
        }
    }
}

/// The slot a name prefix stands for: `mb7_` is slot 7, the legacy `mb_` none.
pub fn slot_of_prefix(prefix: &str) -> Option<u8> {
    prefix
        .strip_prefix("mb")?
        .strip_suffix('_')?
        .parse()
        .ok()
}

/// One CDC-ACM tty that belongs to a Multibus module.
#[derive(Clone, Debug)]
pub struct Port {
    pub device: String,
    pub usb_interface: u8,
    /// Connector interface this port bridges, for the three serial ports.
    /// `None` for the protocol port, which bridges nothing.
    pub connector_interface: Option<u8>,
    pub is_protocol_port: bool,
    pub usb_path: String,
    /// USB serial number of the module the port belongs to.
    pub serial: String,
}

/// One Multibus module as USB sees it.
#[derive(Clone, Debug)]
pub struct UsbModule {
    /// The serial number from the USB descriptor; what `IDENTITY` reports.
    pub serial: String,
    /// The device's directory, `/sys/bus/usb/devices/<usb_path>`.
    pub sysfs: PathBuf,
    /// The kernel's name for the device, such as `1-1`: where it hangs on the bus.
    pub usb_path: String,
}

fn read_sysfs(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// CDC control interface number -> connector interface number.
fn connector_interface_of(usb_interface: u8) -> Option<u8> {
    match usb_interface {
        1 => Some(2),
        3 => Some(3),
        5 => Some(4),
        _ => None,
    }
}

/// Every CDC-ACM tty that belongs to a Multibus module.
pub fn enumerate_ports() -> Vec<Port> {
    let mut found = Vec::new();

    let Ok(entries) = fs::read_dir("/sys/class/tty") else {
        return found;
    };

    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("ttyACM"))
        .collect();
    names.sort();

    for name in names {
        let device = Path::new("/sys/class/tty").join(&name).join("device");
        if !device.is_dir() {
            continue;
        }

        let interface_text = read_sysfs(device.join("bInterfaceNumber"));
        let Ok(usb_interface) = u8::from_str_radix(&interface_text, 16) else {
            continue;
        };

        // The vendor and product ids live on the USB *device*, one level up
        // from the interface the tty hangs off.
        let Ok(resolved) = fs::canonicalize(&device) else {
            continue;
        };
        let Some(usb_device) = resolved.parent() else {
            continue;
        };

        if read_sysfs(usb_device.join("idVendor")) != USB_VENDOR_ID
            || read_sysfs(usb_device.join("idProduct")) != USB_PRODUCT_ID
        {
            continue;
        }

        found.push(Port {
            device: format!("/dev/{name}"),
            usb_interface,
            connector_interface: connector_interface_of(usb_interface),
            is_protocol_port: usb_interface == PROTOCOL_INTERFACE,
            usb_path: usb_device
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            serial: read_sysfs(usb_device.join("serial")),
        });
    }

    found
}

// ── one module, by USB serial number ─────────────────────────────────────────

/// Every Multibus module on the bus, in USB path order.
pub fn usb_modules() -> Vec<UsbModule> {
    let Ok(entries) = fs::read_dir(USB_DEVICES) else {
        return Vec::new();
    };

    let mut found: Vec<UsbModule> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let usb_path = entry.file_name().to_string_lossy().into_owned();
            // Interfaces are listed alongside devices, as `1-1:1.0`; only a
            // device carries the ids.
            if usb_path.contains(':') {
                return None;
            }
            let sysfs = entry.path();
            if read_sysfs(sysfs.join("idVendor")) != USB_VENDOR_ID
                || read_sysfs(sysfs.join("idProduct")) != USB_PRODUCT_ID
            {
                return None;
            }
            Some(UsbModule {
                serial: read_sysfs(sysfs.join("serial")),
                sysfs,
                usb_path,
            })
        })
        .collect();

    found.sort_by(|a, b| a.usb_path.cmp(&b.usb_path));
    found
}

/// The module whose USB descriptor carries this serial number.
pub fn find_usb_module(serial: &str) -> Option<UsbModule> {
    usb_modules()
        .into_iter()
        .find(|module| !serial.is_empty() && module.serial == serial)
}

/// The interface directories of a USB device, `<device>/<device>:1.N`.
fn interface_dirs(module: &UsbModule) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(&module.sysfs) else {
        return Vec::new();
    };
    let marker = format!("{}:", module.usb_path);
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&marker))
        .map(|entry| entry.path())
        .collect()
}

/// The CDC-ACM ttys of one module, in USB interface order.
///
/// Looked up under the module's own USB device rather than through
/// `/sys/class/tty`, so a second module's ports can never be mistaken for
/// this one's.
pub fn module_ports(module: &UsbModule) -> Vec<Port> {
    let mut found = Vec::new();

    for interface in interface_dirs(module) {
        let Ok(usb_interface) =
            u8::from_str_radix(&read_sysfs(interface.join("bInterfaceNumber")), 16)
        else {
            continue;
        };
        let Ok(ttys) = fs::read_dir(interface.join("tty")) else {
            continue;
        };
        for tty in ttys.filter_map(|entry| entry.ok()) {
            let name = tty.file_name().to_string_lossy().into_owned();
            if !name.starts_with("ttyACM") {
                continue;
            }
            found.push(Port {
                device: format!("/dev/{name}"),
                usb_interface,
                connector_interface: connector_interface_of(usb_interface),
                is_protocol_port: usb_interface == PROTOCOL_INTERFACE,
                usb_path: module.usb_path.clone(),
                serial: module.serial.clone(),
            });
        }
    }

    found.sort_by_key(|port| port.usb_interface);
    found
}

/// The protocol port of one module, if it has enumerated.
pub fn module_protocol_port(module: &UsbModule) -> Option<Port> {
    module_ports(module)
        .into_iter()
        .find(|port| port.is_protocol_port)
}

/// The CAN interfaces of one module, in gs_usb channel order, under whatever
/// names they carry right now.
///
/// gs_usb registers one netdev per channel in channel order, all under the
/// one USB interface it binds to, so sorting that interface's netdevs by
/// ifindex - the kernel's creation counter - puts them back in channel order.
/// No name is involved, which is the point: this is what decides whether the
/// names are right.
pub fn module_can_interfaces(module: &UsbModule) -> Vec<String> {
    let mut found: Vec<(u32, String)> = Vec::new();

    for interface in interface_dirs(module) {
        let Ok(netdevs) = fs::read_dir(interface.join("net")) else {
            continue;
        };
        for netdev in netdevs.filter_map(|entry| entry.ok()) {
            let Ok(index) = read_sysfs(netdev.path().join("ifindex")).parse::<u32>() else {
                continue;
            };
            found.push((index, netdev.file_name().to_string_lossy().into_owned()));
        }
    }

    found.sort();
    found.into_iter().map(|(_index, name)| name).collect()
}

/// A device of one module that does not carry the name it should.
#[derive(Clone, Debug)]
pub struct Misnamed {
    /// The device's sysfs path, which is what `udevadm trigger` takes.
    pub sysfs: String,
    /// What it is called now: the netdev name, or the tty node.
    pub current: String,
    /// What it should be called.
    pub expected: String,
}

/// The devices of `module` whose names do not match `names`.
///
/// A netdev is wrong when its name differs; a tty is wrong when the symlink it
/// should have does not lead to it. Both happen when udev named the module
/// before the name map listed its serial number - it then falls back to the
/// single-module names, or keeps the kernel name when those are taken.
pub fn misnamed(module: &UsbModule, names: &Names) -> Vec<Misnamed> {
    // The device's own directory under /sys/devices rather than the class
    // symlink: the form `udevadm trigger` takes as a device path without
    // having to resolve anything itself.
    let device_path = |class_path: String| {
        fs::canonicalize(&class_path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(class_path)
    };

    let mut wrong = Vec::new();

    for (channel, current) in module_can_interfaces(module).into_iter().enumerate() {
        let expected = names.can(channel);
        if current != expected {
            wrong.push(Misnamed {
                sysfs: device_path(format!("/sys/class/net/{current}")),
                current,
                expected,
            });
        }
    }

    for port in module_ports(module) {
        let Some(link) = names.tty_link(port.usb_interface) else {
            continue;
        };
        let leads_here = fs::canonicalize(&link)
            .map(|target| target == Path::new(&port.device))
            .unwrap_or(false);
        if !leads_here {
            let node = port.device.trim_start_matches("/dev/").to_string();
            wrong.push(Misnamed {
                sysfs: device_path(format!("/sys/class/tty/{node}")),
                current: port.device.clone(),
                expected: link,
            });
        }
    }

    wrong
}

/// The path to open for a module's protocol port: its udev symlink when that
/// leads to this module's tty, otherwise the tty node itself.
///
/// The symlink reads better in a log and survives renumbering; the node is
/// what is left when the rule is not installed or has not caught up.
pub fn module_protocol_path(module: &UsbModule, names: &Names) -> Option<String> {
    let port = module_protocol_port(module)?;
    let link = names.protocol();
    let leads_here = fs::canonicalize(&link)
        .map(|target| target == Path::new(&port.device))
        .unwrap_or(false);
    Some(if leads_here { link } else { port.device })
}

/// The names a module's devices should carry, going by the name map: the slot
/// names when the map lists its serial number, the single-module ones when not.
pub fn names_for_serial(serial: &str) -> Names {
    crate::namemap::prefix_for_serial(serial)
        .map(|prefix| Names { prefix })
        .unwrap_or_else(Names::legacy)
}

/// The udev symlink that leads to this port, if there is one.
pub fn port_link(port: &Port) -> Option<String> {
    let link = names_for_serial(&port.serial).tty_link(port.usb_interface)?;
    let leads_here = fs::canonicalize(&link)
        .map(|target| target == Path::new(&port.device))
        .unwrap_or(false);
    leads_here.then_some(link)
}

/// The protocol port of the module with this USB serial number.
pub fn protocol_port_for_serial(serial: &str) -> Result<String> {
    let Some(module) = find_usb_module(serial) else {
        let present: Vec<String> = usb_modules().into_iter().map(|module| module.serial).collect();
        return Err(Error::module(format!(
            "no Multibus module with USB serial {serial} is present{}",
            if present.is_empty() {
                String::new()
            } else {
                format!(" (present: {})", present.join(", "))
            }
        )));
    };

    module_protocol_path(&module, &names_for_serial(serial)).ok_or_else(|| {
        Error::module(format!(
            "the module with USB serial {serial} has no protocol port. Check that cdc_acm is loaded."
        ))
    })
}

/// The protocol port of the module in `slot`, found through the name map the
/// service writes once more than one module is configured.
pub fn protocol_port_for_slot(slot: u8) -> Result<String> {
    let names = Names::for_slot(slot);
    if let Some(serial) = crate::namemap::serial_for_prefix(names.prefix()) {
        return protocol_port_for_serial(&serial);
    }

    // No map entry, but udev may still have named it - from a map that has
    // since been cleaned up, or by hand.
    let link = names.protocol();
    if Path::new(&link).exists() {
        return Ok(link);
    }

    Err(Error::user(format!(
        "no module is known in slot {slot}: the name map ({}) has no entry for it. \
         go-multibus writes the map when more than one module is configured; \
         otherwise name the module with --serial or --device.",
        crate::namemap::RUN_PATH
    )))
}

/// Whether the naming rule is installed, so `mb_canN` names should appear.
pub fn udev_names_expected() -> bool {
    UDEV_RULE_PATHS.iter().any(|path| Path::new(path).exists())
}

/// The Linux CAN interfaces of a Multibus module, in gs_usb channel order.
///
/// `settle` is how long to wait for the udev names to turn up. Straight after
/// the module enumerates the netdevs exist for a moment under their kernel
/// names before udev renames them; acting in that window picks up a name that
/// is about to become wrong, and an `ip link set can5 up` a second later then
/// fails because `can5` no longer exists.
pub fn enumerate_can_interfaces(settle: Duration) -> Vec<String> {
    let deadline = Instant::now() + settle;

    let named = loop {
        let named: Vec<String> = CAN_INTERFACE_NAMES
            .iter()
            .filter(|name| Path::new("/sys/class/net").join(name).is_dir())
            .map(|name| (*name).to_string())
            .collect();

        if named.len() == CAN_INTERFACE_NAMES.len() {
            return named;
        }
        if Instant::now() >= deadline {
            break named;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    // With the rule installed, hand back only what udev has actually named -
    // never the kernel names. They are about to be replaced, and acting on one
    // is worse than doing nothing: `ip link set can4 up` fails once udev has
    // renamed it, and if it does succeed the rename is blocked for good,
    // because an interface cannot be renamed while it is up.
    if udev_names_expected() {
        return named;
    }

    // Fallback: gs_usb registers one netdev per channel in channel order, so
    // sorting by ifindex - the kernel's creation counter - puts them back in
    // that order.
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return named;
    };

    let mut found: Vec<(u32, String)> = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(driver) = fs::canonicalize(path.join("device").join("driver")) else {
            continue;
        };
        if driver.file_name().map(|name| name != "gs_usb").unwrap_or(true) {
            continue;
        }
        let Ok(index) = read_sysfs(path.join("ifindex")).parse::<u32>() else {
            continue;
        };
        found.push((index, entry.file_name().to_string_lossy().into_owned()));
    }

    found.sort();
    found.into_iter().map(|(_index, name)| name).collect()
}

/// Device node of the protocol port, failing when there is not exactly one.
pub fn find_protocol_port() -> Result<String> {
    // The udev symlink is unambiguous and survives renumbering, so prefer it.
    if Path::new(PROTOCOL_SYMLINK).exists() {
        return Ok(PROTOCOL_SYMLINK.to_string());
    }

    let candidates: Vec<Port> = enumerate_ports()
        .into_iter()
        .filter(|port| port.is_protocol_port)
        .collect();

    match candidates.len() {
        0 => Err(Error::module(format!(
            "no Multibus protocol port found. Check that the module enumerated \
             (lsusb should list {USB_VENDOR_ID}:{USB_PRODUCT_ID}) and that cdc_acm is loaded."
        ))),
        1 => Ok(candidates[0].device.clone()),
        _ => {
            let nodes: Vec<String> = candidates
                .iter()
                .map(|port| format!("{} (serial {})", port.device, port.serial))
                .collect();
            Err(Error::user(format!(
                "more than one Multibus module present: {}. Name the one you want \
                 with --slot, --serial or --device.",
                nodes.join(", ")
            )))
        }
    }
}

/// The role the controller's OTG port booted in, or `None` if unknowable.
///
/// `None` is not an answer, it is the absence of one: the node is missing on a
/// controller with a different device tree, and in any build container. Only an
/// explicit role that is not "host" is evidence of anything, so a caller must
/// treat `None` as "no opinion" rather than as a fault.
pub fn otg_dr_mode() -> Option<String> {
    let raw = fs::read(OTG_DR_MODE_PATH).ok()?;
    // Device tree strings are NUL terminated, and the property is read back
    // with that terminator still on it.
    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_module_keeps_the_names_it_always_had() {
        let names = Names::legacy();
        for (channel, name) in CAN_INTERFACE_NAMES.iter().enumerate() {
            assert_eq!(names.can(channel), *name);
        }
        assert_eq!(names.protocol(), PROTOCOL_SYMLINK);
        assert_eq!(names.serial(1), "/dev/mb_serial1");
        assert_eq!(names.tty_link(5).as_deref(), Some("/dev/mb_serial3"));
        assert_eq!(names.tty_link(7).as_deref(), Some(PROTOCOL_SYMLINK));
        assert_eq!(names.tty_link(2), None, "a data interface has no tty of its own");
    }

    #[test]
    fn several_modules_carry_their_slot_in_every_name() {
        let names = Names::for_slot(7);
        assert_eq!(names.prefix(), "mb7_");
        assert_eq!(names.can(0), "mb7_can1");
        assert_eq!(names.can(2), "mb7_can3");
        assert_eq!(names.serial(2), "/dev/mb7_serial2");
        assert_eq!(names.protocol(), "/dev/mb7_protocol");
        assert_eq!(names.tty_link(1).as_deref(), Some("/dev/mb7_serial1"));
    }

    #[test]
    fn every_slot_name_fits_a_network_interface() {
        // IFNAMSIZ is 16 including the terminator.
        for slot in 1..=8 {
            assert!(Names::for_slot(slot).can(2).len() <= 15);
        }
    }

    #[test]
    fn a_prefix_gives_back_its_slot() {
        assert_eq!(slot_of_prefix("mb7_"), Some(7));
        assert_eq!(slot_of_prefix(Names::for_slot(3).prefix()), Some(3));
        assert_eq!(slot_of_prefix("mb_"), None);
        assert_eq!(slot_of_prefix("can_"), None);
    }
}
