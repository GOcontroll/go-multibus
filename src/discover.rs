//! Finding a Multibus module's devices on a controller.
//!
//! Nothing here assumes a device name. Kernel names depend on the controller —
//! a Moduline L4 has four onboard mcp251x controllers so a module's CAN
//! channels land on `can4..can6`, while an M1 has two and the same module lands
//! on `can2..can4`, and `ttyACM` numbers follow enumeration order. Everything
//! is therefore looked up: ttys by their USB interface number, CAN interfaces
//! by the names the udev rule installs with a sysfs fallback that recovers the
//! channel order from `ifindex`.

use std::fs;
use std::path::Path;
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
        });
    }

    found
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
            let nodes: Vec<&str> = candidates.iter().map(|port| port.device.as_str()).collect();
            Err(Error::user(format!(
                "more than one Multibus module present: {}. Name the one you want explicitly.",
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
