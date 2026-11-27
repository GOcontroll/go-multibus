//! The USB name map: which Multibus module gets which device names.
//!
//! With one module on a controller the udev rule names everything by itself,
//! because there is only one thing to name. With two it cannot: both modules
//! have the same vendor and product id and the same interface layout, so
//! nothing in a uevent says which module a device belongs to. The USB serial
//! number does tell modules apart, but it says nothing about which one is which
//! in the configuration.
//!
//! The module bus closes that gap. It reaches one particular slot, and from
//! 0.3.0 the application answers `IDENTITY` on it with the very serial number
//! its USB descriptor carries. The service asks every module during bring-up
//! and writes down what it learned, one line per module: the serial number,
//! the module number the configuration gives that slot, and the slot.
//!
//! ```text
//! 2064359B3245 1 8
//! 206435A63245 2 7
//! ```
//!
//! `gocontroll-multibus-canname` looks the serial of every device udev names
//! up in here and names it after the module number: module 1 gets `mb_can1..3`,
//! `/dev/mb_serial1..3` and `/dev/mb_protocol`, module 2 `mb_can4..6`,
//! `/dev/mb_serial4..6` and `/dev/mb_protocol2`. The slot is for the tools that
//! are asked for a module by slot (`gocontroll-cellmon --slot 7`, `ports`); the
//! helper does not read it.
//!
//! There are two copies. [`RUN_PATH`] is the live one, rewritten during every
//! bring-up and gone after a reboot. [`STORE_PATH`] survives the reboot, so a
//! module that enumerates before the service has run - one that stayed powered
//! through a warm reboot - is still named right. The helper reads the live
//! copy first.
//!
//! A controller with one module has no map at all, and the helper then gives
//! every Multibus device module 1's names - the `mb_` names it has always had.
//!
//! A line from before 0.3.3, which held a name prefix such as `mb7_` where the
//! number now is, is skipped: the next bring-up writes the line again.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use crate::discover::MAX_MODULE_NUMBER;
use crate::error::{Error, Result};

/// The live map, read first by the udev helper.
pub const RUN_PATH: &str = "/run/gocontroll/multibus-usb-names";
/// The copy that survives a reboot.
pub const STORE_PATH: &str = "/var/lib/go-multibus/usb-names";

/// Every module's supervisor runs in its own thread and records its own line,
/// and each of those is a read-modify-write of the same two files. Serialised
/// here so one module's update can never drop another's.
static LOCK: Mutex<()> = Mutex::new(());

/// What the map knows about one module.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The module number, which decides the names.
    pub number: u8,
    /// The slot it was in when it was recorded.
    pub slot: Option<u8>,
}

/// USB serial number -> module number and slot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameMap {
    entries: BTreeMap<String, Entry>,
}

impl NameMap {
    /// Read the text format: one `<serial> <number> [<slot>]` line per module.
    /// Blank lines, `#` comments and anything malformed - a number out of
    /// range, an old prefix line - are skipped rather than refused, the same
    /// as the shell helper does.
    pub fn parse(text: &str) -> NameMap {
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let (Some(serial), Some(number)) = (words.next(), words.next()) else {
                continue;
            };
            let Ok(number) = number.parse::<u8>() else {
                continue;
            };
            if !(1..=MAX_MODULE_NUMBER).contains(&number) {
                continue;
            }
            let slot = words.next().and_then(|slot| slot.parse::<u8>().ok());
            entries.insert(serial.to_string(), Entry { number, slot });
        }
        NameMap { entries }
    }

    /// One line per module, in module number order.
    pub fn render(&self) -> String {
        let mut lines: Vec<(&String, &Entry)> = self.entries.iter().collect();
        lines.sort_by(|a, b| (a.1.number, a.0).cmp(&(b.1.number, b.0)));
        lines
            .into_iter()
            .map(|(serial, entry)| match entry.slot {
                Some(slot) => format!("{serial} {} {slot}\n", entry.number),
                None => format!("{serial} {}\n", entry.number),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entry_for(&self, serial: &str) -> Option<Entry> {
        self.entries.get(serial).copied()
    }

    pub fn serial_for_number(&self, number: u8) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.number == number)
            .map(|(serial, _)| serial.as_str())
    }

    pub fn serial_for_slot(&self, slot: u8) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.slot == Some(slot))
            .map(|(serial, _)| serial.as_str())
    }

    /// Give `serial` the names of module `number`, in `slot`. Returns whether
    /// anything changed.
    ///
    /// Any other serial holding the same number or the same slot loses its
    /// line: that is a module that has since been swapped out, and leaving its
    /// line in would hand two modules the same names if it ever came back.
    pub fn set(&mut self, serial: &str, number: u8, slot: u8) -> bool {
        let before = self.entries.clone();
        self.entries.retain(|other, entry| {
            other == serial || (entry.number != number && entry.slot != Some(slot))
        });
        self.entries.insert(
            serial.to_string(),
            Entry {
                number,
                slot: Some(slot),
            },
        );
        self.entries != before
    }

    /// Keep only the lines for these module numbers. Returns whether anything
    /// went.
    pub fn retain_numbers(&mut self, numbers: &[u8]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|_, entry| numbers.contains(&entry.number));
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

/// Record that the module with `serial`, in `slot`, is module `number`, in both
/// copies. Returns whether either changed; an unchanged copy is not rewritten,
/// so a steady machine does not write to flash on every bring-up.
pub fn record(serial: &str, number: u8, slot: u8) -> Result<bool> {
    let _guard = locked();
    let mut changed = false;

    for path in [RUN_PATH, STORE_PATH] {
        let existing = NameMap::load(path);
        let missing = existing.is_none();
        let mut map = existing.unwrap_or_default();
        if map.set(serial, number, slot) || missing {
            map.write(path).map_err(|failure| failed(path, failure))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Get the map ready before any module is reset, on a controller with more
/// than one module configured.
///
/// Lines for module numbers no longer configured are dropped from both copies,
/// so a number given up does not linger. And the live copy is made to exist,
/// seeded from the stored one: its presence is what tells the udev helper to
/// wait for a serial number it has not seen yet rather than fall back to module
/// 1's names straight away.
pub fn prepare(numbers: &[u8]) -> Result<()> {
    let _guard = locked();

    let stored = NameMap::load(STORE_PATH);
    if let Some(mut map) = stored.clone() {
        if map.retain_numbers(numbers) {
            map.write(STORE_PATH)
                .map_err(|failure| failed(STORE_PATH, failure))?;
        }
    }

    let mut live = NameMap::load(RUN_PATH).or(stored).unwrap_or_default();
    live.retain_numbers(numbers);
    live.write(RUN_PATH)
        .map_err(|failure| failed(RUN_PATH, failure))
}

/// Remove both copies, on a controller with one module configured. Returns
/// whether there was anything to remove.
///
/// Without a map the helper gives every Multibus device module 1's names,
/// which is exactly what one module should get - also when the map is left
/// over from a configuration that used to have two.
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

/// What either copy knows about a module, live copy first.
fn entry_for_serial(serial: &str) -> Option<Entry> {
    [RUN_PATH, STORE_PATH]
        .iter()
        .filter_map(|path| NameMap::load(path))
        .find_map(|map| map.entry_for(serial))
}

/// The module number a module's devices are named after, live copy first.
pub fn number_for_serial(serial: &str) -> Option<u8> {
    entry_for_serial(serial).map(|entry| entry.number)
}

/// The slot the map puts a module in, if it lists the module at all.
pub fn slot_for_serial(serial: &str) -> Option<u8> {
    entry_for_serial(serial).and_then(|entry| entry.slot)
}

/// The serial number of module `number`, live copy first.
pub fn serial_for_number(number: u8) -> Option<String> {
    [RUN_PATH, STORE_PATH]
        .iter()
        .filter_map(|path| NameMap::load(path))
        .find_map(|map| map.serial_for_number(number).map(str::to_string))
}

/// The serial number of the module last seen in `slot`, live copy first.
pub fn serial_for_slot(slot: u8) -> Option<String> {
    [RUN_PATH, STORE_PATH]
        .iter()
        .filter_map(|path| NameMap::load(path))
        .find_map(|map| map.serial_for_slot(slot).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(number: u8, slot: u8) -> Option<Entry> {
        Some(Entry {
            number,
            slot: Some(slot),
        })
    }

    #[test]
    fn the_text_format_round_trips() {
        let text = "2064359B3245 1 8\n206435A63245 2 7\n";
        let map = NameMap::parse(text);
        assert_eq!(map.entry_for("206435A63245"), entry(2, 7));
        assert_eq!(map.serial_for_number(1), Some("2064359B3245"));
        assert_eq!(map.serial_for_slot(7), Some("206435A63245"));
        assert_eq!(map.render(), text);
    }

    #[test]
    fn rubbish_in_the_file_is_skipped_rather_than_refused() {
        let map = NameMap::parse(
            "# comment\n\n   \nlonely\n206435A63245   2  7 extra\nTOOBIG 9 3\nZERO 0 4\n",
        );
        assert_eq!(map.entry_for("206435A63245"), entry(2, 7));
        assert_eq!(map.entry_for("lonely"), None);
        assert_eq!(map.entry_for("TOOBIG"), None);
        assert_eq!(map.entry_for("ZERO"), None);
        assert_eq!(map.render(), "206435A63245 2 7\n");
    }

    #[test]
    fn a_line_from_before_the_numbers_is_skipped() {
        let map = NameMap::parse("206435A63245 mb7_\n2064359B3245 1 8\n");
        assert_eq!(map.entry_for("206435A63245"), None);
        assert_eq!(map.render(), "2064359B3245 1 8\n");
    }

    #[test]
    fn a_line_without_a_slot_still_names_the_module() {
        let map = NameMap::parse("206435A63245 2\n");
        assert_eq!(
            map.entry_for("206435A63245"),
            Some(Entry {
                number: 2,
                slot: None
            })
        );
        assert_eq!(map.render(), "206435A63245 2\n");
    }

    #[test]
    fn a_new_module_in_a_slot_takes_the_names_from_the_old_one() {
        let mut map = NameMap::parse("OLD000000001 2 7\n206435A63245 1 8\n");
        assert!(map.set("NEW000000002", 2, 7));
        assert_eq!(map.entry_for("OLD000000001"), None);
        assert_eq!(map.serial_for_number(2), Some("NEW000000002"));
        assert_eq!(map.serial_for_number(1), Some("206435A63245"));

        // Recording what is already there changes nothing, so nothing is
        // written.
        assert!(!map.set("NEW000000002", 2, 7));
    }

    #[test]
    fn a_module_that_moves_slot_keeps_one_line() {
        let mut map = NameMap::parse("206435A63245 2 7\n");
        assert!(map.set("206435A63245", 2, 5));
        assert_eq!(map.render(), "206435A63245 2 5\n");
    }

    #[test]
    fn unconfigured_numbers_are_dropped() {
        let mut map = NameMap::parse("A 1 8\nB 2 7\nC 3 3\n");
        assert!(map.retain_numbers(&[1, 2]));
        assert_eq!(map.render(), "A 1 8\nB 2 7\n");
        assert!(!map.retain_numbers(&[1, 2]));
    }
}
