//! The USB name map: which Multibus module gets which device names.
//!
//! With one module on a controller the udev rule names everything by itself,
//! because there is only one thing to name. With two it cannot: both modules
//! have the same vendor and product id and the same interface layout, so
//! nothing in a uevent says which slot a device belongs to. The USB serial
//! number does tell modules apart, but it says nothing about slots either.
//!
//! The module bus closes that gap. It reaches one particular slot, and from
//! 0.3.0 the application answers `IDENTITY` on it with the very serial number
//! its USB descriptor carries. The service asks every module during bring-up
//! and writes down what it learned, one line per module:
//!
//! ```text
//! 206435A63245 mb7_
//! 2064359B3245 mb8_
//! ```
//!
//! `gocontroll-multibus-canname` looks the serial of every device udev names
//! up in here and puts the prefix in front of the name: `mb7_can1`,
//! `/dev/mb7_serial1`, `/dev/mb7_protocol`.
//!
//! There are two copies. [`RUN_PATH`] is the live one, rewritten during every
//! bring-up and gone after a reboot. [`STORE_PATH`] survives the reboot, so a
//! module that enumerates before the service has run - one that stayed powered
//! through a warm reboot - is still named right. The helper reads the live
//! copy first.
//!
//! A controller with one module has no map at all, and the helper then gives
//! every Multibus device the `mb_` names it has always had.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use crate::error::{Error, Result};

/// The live map, read first by the udev helper.
pub const RUN_PATH: &str = "/run/gocontroll/multibus-usb-names";
/// The copy that survives a reboot.
pub const STORE_PATH: &str = "/var/lib/go-multibus/usb-names";

/// Every module's supervisor runs in its own thread and records its own line,
/// and each of those is a read-modify-write of the same two files. Serialised
/// here so one module's update can never drop another's.
static LOCK: Mutex<()> = Mutex::new(());

/// USB serial number -> name prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameMap {
    entries: BTreeMap<String, String>,
}

impl NameMap {
    /// Read the text format: one `<serial> <prefix>` pair per line. Blank lines,
    /// `#` comments and anything malformed are skipped rather than refused, the
    /// same as the shell helper does.
    pub fn parse(text: &str) -> NameMap {
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            if let (Some(serial), Some(prefix)) = (words.next(), words.next()) {
                entries.insert(serial.to_string(), prefix.to_string());
            }
        }
        NameMap { entries }
    }

    /// One line per module, in prefix order so the file reads in slot order.
    pub fn render(&self) -> String {
        let mut lines: Vec<(&String, &String)> = self.entries.iter().collect();
        lines.sort_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)));
        lines
            .into_iter()
            .map(|(serial, prefix)| format!("{serial} {prefix}\n"))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn prefix_for(&self, serial: &str) -> Option<&str> {
        self.entries.get(serial).map(String::as_str)
    }

    pub fn serial_for(&self, prefix: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, value)| value.as_str() == prefix)
            .map(|(serial, _)| serial.as_str())
    }

    /// Give `serial` the names under `prefix`. Returns whether anything changed.
    ///
    /// Any other serial holding the same prefix loses it: that is a module
    /// that has since been swapped out of the slot, and leaving its line in
    /// would hand two modules the same names if it ever came back elsewhere.
    pub fn set(&mut self, serial: &str, prefix: &str) -> bool {
        let before = self.entries.clone();
        self.entries
            .retain(|other, value| other == serial || value != prefix);
        self.entries.insert(serial.to_string(), prefix.to_string());
        self.entries != before
    }

    /// Keep only the lines for these prefixes. Returns whether anything went.
    pub fn retain_prefixes(&mut self, prefixes: &[String]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|_, prefix| prefixes.contains(prefix));
        self.entries.len() != before
    }

    /// The map at `path`, or `None` when there is no such file.
    pub fn load(path: &str) -> Option<NameMap> {
        fs::read_to_string(path).ok().map(|text| NameMap::parse(&text))
    }

    /// Replace the file at `path` in one step, so udev never reads half a map.
    pub fn write(&self, path: &str) -> Result<()> {
        if let Some(directory) = Path::new(path).parent() {
            fs::create_dir_all(directory)?;
        }
        let temporary = format!("{path}.tmp");
        {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(self.render().as_bytes())?;
            // The stored copy is what the next boot names devices by.
            file.sync_all()?;
        }
        fs::rename(&temporary, path)?;
        Ok(())
    }
}

fn locked() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn failed(path: &str, failure: Error) -> Error {
    Error::system(format!("cannot write the name map {path}: {failure}"))
}

/// Record that the module with `serial` is named under `prefix`, in both
/// copies. Returns whether either changed; an unchanged copy is not
/// rewritten, so a steady machine does not write to flash on every bring-up.
pub fn record(serial: &str, prefix: &str) -> Result<bool> {
    let _guard = locked();
    let mut changed = false;

    for path in [RUN_PATH, STORE_PATH] {
        let existing = NameMap::load(path);
        let missing = existing.is_none();
        let mut map = existing.unwrap_or_default();
        if map.set(serial, prefix) || missing {
            map.write(path).map_err(|failure| failed(path, failure))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Get the map ready before any module is reset, on a controller with more
/// than one module configured.
///
/// Lines for prefixes no longer configured are dropped from both copies, so a
/// module taken out of the configuration goes back to the plain names. And the
/// live copy is made to exist, seeded from the stored one: its presence is
/// what tells the udev helper to wait for a serial number it has not seen yet
/// rather than fall back to the single-module names straight away.
pub fn prepare(prefixes: &[String]) -> Result<()> {
    let _guard = locked();

    let stored = NameMap::load(STORE_PATH);
    if let Some(mut map) = stored.clone() {
        if map.retain_prefixes(prefixes) {
            map.write(STORE_PATH)
                .map_err(|failure| failed(STORE_PATH, failure))?;
        }
    }

    let mut live = NameMap::load(RUN_PATH).or(stored).unwrap_or_default();
    live.retain_prefixes(prefixes);
    live.write(RUN_PATH)
        .map_err(|failure| failed(RUN_PATH, failure))
}

/// Remove both copies, on a controller with one module configured. Returns
/// whether there was anything to remove.
///
/// Without a map the helper names every Multibus device the way it always
/// has, which is exactly what one module should get - also when the map is
/// left over from a configuration that used to have two.
pub fn clear() -> Result<bool> {
    let _guard = locked();
    let mut removed = false;
    for path in [RUN_PATH, STORE_PATH] {
        match fs::remove_file(path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::system(format!("cannot remove {path}: {error}")));
            }
        }
    }
    Ok(removed)
}

/// The prefix a module's devices are named under, live copy first.
pub fn prefix_for_serial(serial: &str) -> Option<String> {
    [RUN_PATH, STORE_PATH]
        .iter()
        .filter_map(|path| NameMap::load(path))
        .find_map(|map| map.prefix_for(serial).map(str::to_string))
}

/// The slot the map puts a module in, if it lists the module at all.
pub fn slot_for_serial(serial: &str) -> Option<u8> {
    prefix_for_serial(serial).and_then(|prefix| crate::discover::slot_of_prefix(&prefix))
}

/// The serial number of the module named under `prefix`, live copy first.
pub fn serial_for_prefix(prefix: &str) -> Option<String> {
    [RUN_PATH, STORE_PATH]
        .iter()
        .filter_map(|path| NameMap::load(path))
        .find_map(|map| map.serial_for(prefix).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_format_round_trips() {
        let text = "206435A63245 mb7_\n2064359B3245 mb8_\n";
        let map = NameMap::parse(text);
        assert_eq!(map.prefix_for("206435A63245"), Some("mb7_"));
        assert_eq!(map.serial_for("mb8_"), Some("2064359B3245"));
        assert_eq!(map.render(), text);
    }

    #[test]
    fn rubbish_in_the_file_is_skipped_rather_than_refused() {
        let map = NameMap::parse("# comment\n\n   \nlonely\n206435A63245   mb7_  extra\n");
        assert_eq!(map.prefix_for("206435A63245"), Some("mb7_"));
        assert_eq!(map.prefix_for("lonely"), None);
        assert_eq!(map.render(), "206435A63245 mb7_\n");
    }

    #[test]
    fn a_new_module_in_a_slot_takes_the_names_from_the_old_one() {
        let mut map = NameMap::parse("OLD000000001 mb7_\n206435A63245 mb8_\n");
        assert!(map.set("NEW000000002", "mb7_"));
        assert_eq!(map.prefix_for("OLD000000001"), None);
        assert_eq!(map.serial_for("mb7_"), Some("NEW000000002"));
        assert_eq!(map.serial_for("mb8_"), Some("206435A63245"));

        // Recording what is already there changes nothing, so nothing is
        // written.
        assert!(!map.set("NEW000000002", "mb7_"));
    }

    #[test]
    fn a_module_that_moves_slot_keeps_one_line() {
        let mut map = NameMap::parse("206435A63245 mb7_\n");
        assert!(map.set("206435A63245", "mb5_"));
        assert_eq!(map.render(), "206435A63245 mb5_\n");
    }

    #[test]
    fn unconfigured_prefixes_are_dropped() {
        let mut map = NameMap::parse("A mb7_\nB mb8_\nC mb3_\n");
        assert!(map.retain_prefixes(&["mb7_".to_string(), "mb8_".to_string()]));
        assert_eq!(map.render(), "A mb7_\nB mb8_\n");
        assert!(!map.retain_prefixes(&["mb7_".to_string(), "mb8_".to_string()]));
    }
}
