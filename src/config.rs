//! The stored configuration, `/etc/gocontroll/multibus.json`.
//!
//! This file is what the controller wants the module to be; what the module
//! actually reports is read back separately, so a refused combination shows up
//! as a difference rather than as a silent success.
//!
//! Every field has a default, and unknown keys are ignored, so a file written
//! by an older or a newer version still loads. The key names are the ones the
//! Python implementation used, so an existing file keeps working unchanged.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::proto::{Mode, INTERFACES};

pub const CONFIG_PATH: &str = "/etc/gocontroll/multibus.json";
pub const OUTPUT_PATH: &str = "/dev/shm/gocontroll/multibus-cells.json";
pub const STATE_PATH: &str = "/dev/shm/gocontroll/multibus-state.json";

fn default_slot() -> u8 {
    8
}
fn default_interfaces() -> Vec<String> {
    ["can", "rs485", "can", "rs485", "isospi"]
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}
fn default_cell_count() -> u8 {
    12
}
fn default_module_poll_ms() -> u16 {
    100
}
fn default_publish_interval_s() -> f64 {
    0.5
}
fn default_bring_up_can() -> bool {
    true
}
fn default_can_bitrate() -> u32 {
    500_000
}
fn default_output() -> String {
    OUTPUT_PATH.to_string()
}
fn default_supply_mv() -> u16 {
    5000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_slot")]
    pub slot: u8,

    #[serde(default = "default_interfaces")]
    pub interfaces: Vec<String>,

    #[serde(default = "default_cell_count")]
    pub cell_count: u8,

    #[serde(default = "default_module_poll_ms")]
    pub module_poll_ms: u16,

    #[serde(default = "default_publish_interval_s")]
    pub publish_interval_s: f64,

    #[serde(default = "default_bring_up_can")]
    pub bring_up_can: bool,

    /// Fallback only. Bus parameters belong to go-can; this is used solely when
    /// go-can is not installed or has no config for the interface yet.
    #[serde(default = "default_can_bitrate")]
    pub can_bitrate: u32,

    #[serde(default = "default_output")]
    pub output: String,

    /// External supply on the connector. The module keeps its own copy in
    /// flash so it can restore the supply without the controller, but this is
    /// the controller's copy and it is pushed on every bring-up — so what is
    /// here wins whenever the two disagree.
    #[serde(default)]
    pub supply_enabled: bool,

    #[serde(default = "default_supply_mv")]
    pub supply_mv: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            slot: default_slot(),
            interfaces: default_interfaces(),
            cell_count: default_cell_count(),
            module_poll_ms: default_module_poll_ms(),
            publish_interval_s: default_publish_interval_s(),
            bring_up_can: default_bring_up_can(),
            can_bitrate: default_can_bitrate(),
            output: default_output(),
            supply_enabled: false,
            supply_mv: default_supply_mv(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| Error::System(format!("cannot read {path}: {error}"))),
            // The defaults are a working configuration, so a missing file is
            // not a problem worth reporting.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(Error::System(format!("cannot read {path}: {error}"))),
        }
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(directory) = Path::new(path).parent() {
            fs::create_dir_all(directory)?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');

        // Replace in one step, so a reader never sees half a document.
        let temporary = format!("{path}.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    /// The configuration as the modes the module expects.
    pub fn interface_modes(&self) -> Result<Vec<Mode>> {
        if self.interfaces.len() != INTERFACES.len() {
            return Err(Error::user(format!(
                "the interfaces list has {} entries, expected {}",
                self.interfaces.len(),
                INTERFACES.len()
            )));
        }
        self.interfaces.iter().map(|name| Mode::parse(name)).collect()
    }

    pub fn set_interface(&mut self, number: u8, mode: Mode) -> Result<()> {
        let spec = INTERFACES
            .iter()
            .find(|spec| spec.number == number)
            .ok_or_else(|| Error::user(format!("interface must be 1..{}", INTERFACES.len())))?;

        // A hand-edited file can hold a list of any length, and indexing it
        // below would panic rather than say what is wrong with the file.
        if self.interfaces.len() != INTERFACES.len() {
            return Err(Error::user(format!(
                "the interfaces list has {} entries, expected {}; fix it, or delete the file",
                self.interfaces.len(),
                INTERFACES.len()
            )));
        }

        if !spec.modes.contains(&mode) {
            let allowed: Vec<&str> = spec.modes.iter().map(|mode| mode.as_str()).collect();
            return Err(Error::user(format!(
                "interface {number} cannot be {}; it accepts {}",
                mode.as_str(),
                allowed.join(", ")
            )));
        }

        self.interfaces[number as usize - 1] = mode.as_str().to_string();
        Ok(())
    }

    /// Which gs_usb channels have a live transceiver behind them.
    ///
    /// The module always presents three channels. Channel 0 is connector
    /// interface 1 and is always CAN; channels 1 and 2 follow interfaces 3 and
    /// 4. Bringing up a channel whose interface is RS485 would give a netdev
    /// that can never carry a frame.
    ///
    /// These are channel *indices*, not interface names — see
    /// [`crate::discover::enumerate_can_interfaces`] for why the two are not
    /// interchangeable.
    pub fn can_channels(&self) -> Result<Vec<usize>> {
        let modes = self.interface_modes()?;
        let mut channels = vec![0usize];
        for (channel, interface) in [(1usize, 2usize), (2, 3)] {
            if modes[interface] == Mode::Can {
                channels.push(channel);
            }
        }
        Ok(channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_json() {
        let text = serde_json::to_string(&Config::default()).unwrap();
        let parsed: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.slot, 8);
        assert_eq!(parsed.interfaces, default_interfaces());
        assert!(!parsed.supply_enabled);
    }

    #[test]
    fn a_file_from_the_python_version_still_loads() {
        // No supply keys: that release did not have them.
        let text = r#"{
            "slot": 6,
            "interfaces": ["can", "rs232", "rs485", "can", "isospi"],
            "cell_count": 24,
            "module_poll_ms": 200,
            "publish_interval_s": 1.0,
            "bring_up_can": false,
            "can_bitrate": 250000,
            "output": "/dev/shm/x.json"
        }"#;
        let parsed: Config = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.slot, 6);
        assert_eq!(parsed.cell_count, 24);
        assert!(!parsed.supply_enabled);
        assert_eq!(parsed.supply_mv, 5000);
    }

    #[test]
    fn can_channels_follow_the_configured_modes() {
        let mut config = Config::default();
        // can, rs485, can, rs485, isospi -> channels 0 and 1
        assert_eq!(config.can_channels().unwrap(), vec![0, 1]);

        config.set_interface(4, Mode::Can).unwrap();
        assert_eq!(config.can_channels().unwrap(), vec![0, 1, 2]);

        config.set_interface(3, Mode::Rs485).unwrap();
        assert_eq!(config.can_channels().unwrap(), vec![0, 2]);
    }

    #[test]
    fn a_truncated_interfaces_list_is_reported_rather_than_panicking() {
        let mut config = Config::default();
        config.interfaces.truncate(3);
        assert!(config.set_interface(2, Mode::Rs232).is_err());
        assert!(config.interface_modes().is_err());
        assert!(config.can_channels().is_err());
    }

    #[test]
    fn an_interface_refuses_a_mode_its_hardware_cannot_serve() {
        let mut config = Config::default();
        assert!(config.set_interface(3, Mode::Rs232).is_err());
        assert!(config.set_interface(1, Mode::Rs485).is_err());
        assert!(config.set_interface(2, Mode::Rs232).is_ok());
    }
}
