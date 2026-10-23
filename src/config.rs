//! The stored configuration, `/etc/gocontroll/multibus.json`.
//!
//! This file is what the controller wants the modules to be; what a module
//! actually reports is read back separately, so a refused combination shows up
//! as a difference rather than as a silent success.
//!
//! Every field has a default, and unknown keys are ignored, so a file written
//! by an older or a newer version still loads. The key names are the ones the
//! Python implementation used, so an existing file keeps working unchanged.
//!
//! The file comes in two forms.
//!
//! **One module**, the form every release before 0.3.0 wrote: the module's
//! settings at the top level.
//!
//! ```json
//! { "slot": 8, "interfaces": ["can", "rs485", "can", "rs485", "isospi"], ... }
//! ```
//!
//! **A module list**, for a controller with more than one module. Every entry
//! takes the same keys as the one-module form, and only `slot` is required;
//! `publish_interval_s` stays at the top because there is one publish loop
//! per service, not per module.
//!
//! ```json
//! { "publish_interval_s": 0.5,
//!   "modules": [ { "slot": 7, "isospi_role": "slave" }, { "slot": 8 } ] }
//! ```
//!
//! What decides the behaviour is how many modules there are, not which form
//! the file is in. With one module - either form - everything is named and
//! published exactly as before 0.3.0: `mb_can1`, `/dev/mb_protocol`,
//! `multibus-cells.json`. With two or more, every name and every published
//! document carries the slot, because otherwise they would collide.
//!
//! A file is written back in the form it was read in, so a one-module file
//! that only had an interface changed comes back byte for byte as 0.2.0 would
//! have written it.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discover::Names;
use crate::error::{Error, Result};
use crate::modulebus::SLOT_SPIDEV;
use crate::proto::{Mode, Role, INTERFACES};

pub const CONFIG_PATH: &str = "/etc/gocontroll/multibus.json";
pub const OUTPUT_PATH: &str = "/dev/shm/gocontroll/multibus-cells.json";
pub const STATE_PATH: &str = "/dev/shm/gocontroll/multibus-state.json";

/// Cell document of one module, once more than one is configured.
pub fn slot_output_path(slot: u8) -> String {
    format!("/dev/shm/gocontroll/multibus-slot{slot}-cells.json")
}

/// State document of one module, once more than one is configured.
pub fn slot_state_path(slot: u8) -> String {
    format!("/dev/shm/gocontroll/multibus-slot{slot}-state.json")
}

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

/// One module's settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleConfig {
    /// Controller slot, 1..8. The one key a module list entry cannot do
    /// without: it is what the entry is about.
    pub slot: u8,

    /// The isoSPI role pushed on every bring-up. `None` only happens in a
    /// one-module file that never set it, and means what it meant before
    /// 0.3.0: leave the module in whatever role its firmware starts in. A
    /// module list entry without the key gets `master`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isospi_role: Option<Role>,

    #[serde(default = "default_interfaces")]
    pub interfaces: Vec<String>,

    #[serde(default = "default_cell_count")]
    pub cell_count: u8,

    #[serde(default = "default_module_poll_ms")]
    pub module_poll_ms: u16,

    #[serde(default = "default_bring_up_can")]
    pub bring_up_can: bool,

    /// Fallback only. Bus parameters belong to go-can; this is used solely when
    /// go-can is not installed or has no config for the interface yet.
    #[serde(default = "default_can_bitrate")]
    pub can_bitrate: u32,

    /// External supply on the connector. The module keeps its own copy in
    /// flash so it can restore the supply without the controller, but this is
    /// the controller's copy and it is pushed on every bring-up — so what is
    /// here wins whenever the two disagree.
    #[serde(default)]
    pub supply_enabled: bool,

    #[serde(default = "default_supply_mv")]
    pub supply_mv: u16,

    /// Where the cell data is published. `None` takes the default for the
    /// mode the controller is in; see [`ModuleSetup::output`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl ModuleConfig {
    /// A module list entry with every default: what `module add` creates.
    pub fn new(slot: u8) -> ModuleConfig {
        ModuleConfig {
            slot,
            isospi_role: Some(Role::Master),
            interfaces: default_interfaces(),
            cell_count: default_cell_count(),
            module_poll_ms: default_module_poll_ms(),
            bring_up_can: default_bring_up_can(),
            can_bitrate: default_can_bitrate(),
            supply_enabled: false,
            supply_mv: default_supply_mv(),
            output: None,
        }
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

/// The one-module form, laid out field for field as 0.2.0 wrote it, so a file
/// read in this form is written back unchanged. `isospi_role` is new and only
/// appears once it has been set.
#[derive(Serialize, Deserialize)]
struct LegacyFile {
    #[serde(default = "default_slot")]
    slot: u8,
    #[serde(default = "default_interfaces")]
    interfaces: Vec<String>,
    #[serde(default = "default_cell_count")]
    cell_count: u8,
    #[serde(default = "default_module_poll_ms")]
    module_poll_ms: u16,
    #[serde(default = "default_publish_interval_s")]
    publish_interval_s: f64,
    #[serde(default = "default_bring_up_can")]
    bring_up_can: bool,
    #[serde(default = "default_can_bitrate")]
    can_bitrate: u32,
    #[serde(default = "default_output")]
    output: String,
    #[serde(default)]
    supply_enabled: bool,
    #[serde(default = "default_supply_mv")]
    supply_mv: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    isospi_role: Option<Role>,
}

/// The module list form.
#[derive(Serialize, Deserialize)]
struct ModulesFile {
    #[serde(default = "default_publish_interval_s")]
    publish_interval_s: f64,
    modules: Vec<ModuleConfig>,
}

/// Which of the two forms a file is in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Form {
    /// Top-level module settings, as before 0.3.0.
    Single,
    /// A `modules` list.
    List,
}

/// The whole file.
#[derive(Clone, Debug)]
pub struct Config {
    /// How often each module's cell data is read and published.
    pub publish_interval_s: f64,
    /// At least one, in slot order once anything has been added or removed.
    pub modules: Vec<ModuleConfig>,
    pub form: Form,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_legacy(LegacyFile {
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
            isospi_role: None,
        })
    }
}

impl Config {
    fn from_legacy(file: LegacyFile) -> Config {
        Config {
            publish_interval_s: file.publish_interval_s,
            modules: vec![ModuleConfig {
                slot: file.slot,
                isospi_role: file.isospi_role,
                interfaces: file.interfaces,
                cell_count: file.cell_count,
                module_poll_ms: file.module_poll_ms,
                bring_up_can: file.bring_up_can,
                can_bitrate: file.can_bitrate,
                supply_enabled: file.supply_enabled,
                supply_mv: file.supply_mv,
                output: Some(file.output),
            }],
            form: Form::Single,
        }
    }

    /// Read the text of a configuration file, in either form.
    pub fn parse(text: &str) -> Result<Config> {
        let value: Value = serde_json::from_str(text)?;

        let config = if value.get("modules").is_some() {
            let file: ModulesFile = serde_json::from_value(value)?;
            let mut modules = file.modules;
            // A list entry without a role is a master: with two modules of the
            // same hardware revision the firmware default would make both of
            // them one, so the list form always pushes a role.
            for module in &mut modules {
                module.isospi_role.get_or_insert(Role::Master);
            }
            Config {
                publish_interval_s: file.publish_interval_s,
                modules,
                form: Form::List,
            }
        } else {
            Config::from_legacy(serde_json::from_value(value)?)
        };

        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &str) -> Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map_err(|error| match error {
                Error::User(reason) => Error::User(format!("{path}: {reason}")),
                Error::Json(error) => Error::System(format!("cannot read {path}: {error}")),
                other => Error::System(format!("cannot read {path}: {other}")),
            }),
            // The defaults are a working configuration, so a missing file is
            // not a problem worth reporting.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(Error::System(format!("cannot read {path}: {error}"))),
        }
    }

    /// The file as it is written: pretty JSON in the form it was read in.
    pub fn to_text(&self) -> Result<String> {
        let mut text = match self.form {
            Form::Single if self.modules.len() == 1 => {
                let module = &self.modules[0];
                serde_json::to_string_pretty(&LegacyFile {
                    slot: module.slot,
                    interfaces: module.interfaces.clone(),
                    cell_count: module.cell_count,
                    module_poll_ms: module.module_poll_ms,
                    publish_interval_s: self.publish_interval_s,
                    bring_up_can: module.bring_up_can,
                    can_bitrate: module.can_bitrate,
                    output: module.output.clone().unwrap_or_else(default_output),
                    supply_enabled: module.supply_enabled,
                    supply_mv: module.supply_mv,
                    isospi_role: module.isospi_role,
                })?
            }
            _ => serde_json::to_string_pretty(&ModulesFile {
                publish_interval_s: self.publish_interval_s,
                modules: self.modules.clone(),
            })?,
        };
        text.push('\n');
        Ok(text)
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(directory) = Path::new(path).parent() {
            fs::create_dir_all(directory)?;
        }
        let text = self.to_text()?;

        // Replace in one step, so a reader never sees half a document.
        let temporary = format!("{path}.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Whether more than one module is configured, which is what switches
    /// names and published documents over to carrying the slot.
    pub fn is_multi(&self) -> bool {
        self.modules.len() > 1
    }

    pub fn slots(&self) -> Vec<u8> {
        self.modules.iter().map(|module| module.slot).collect()
    }

    fn slot_list(&self) -> String {
        self.slots()
            .iter()
            .map(|slot| slot.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Refuse a file the service could not run. Only the list form is checked:
    /// a one-module file loads exactly as it did before 0.3.0, problems and all.
    fn validate(&self) -> Result<()> {
        if self.form == Form::Single {
            return Ok(());
        }
        if self.modules.is_empty() {
            return Err(Error::user(
                "the modules list is empty; add a module, or delete the file",
            ));
        }

        let mut seen: Vec<u8> = Vec::new();
        for module in &self.modules {
            check_slot(module.slot)?;
            if seen.contains(&module.slot) {
                return Err(Error::user(format!(
                    "slot {} is configured twice; one module per slot",
                    module.slot
                )));
            }
            seen.push(module.slot);
        }

        // Two modules publishing to one file would overwrite each other twice
        // a second, which looks like data that jumps rather than like an error.
        let outputs: Vec<String> = self.setups().iter().map(ModuleSetup::output).collect();
        for (index, output) in outputs.iter().enumerate() {
            if outputs[..index].contains(output) {
                return Err(Error::user(format!(
                    "two modules publish to {output}; give each its own output"
                )));
            }
        }
        Ok(())
    }

    /// One module, by slot.
    pub fn module(&self, slot: u8) -> Option<&ModuleConfig> {
        self.modules.iter().find(|module| module.slot == slot)
    }

    /// The module a command is about. `--slot` may be left out while only one
    /// module is configured, and has to be given once there are more.
    pub fn select(&self, slot: Option<u8>) -> Result<&ModuleConfig> {
        let index = self.select_index(slot)?;
        Ok(&self.modules[index])
    }

    pub fn select_mut(&mut self, slot: Option<u8>) -> Result<&mut ModuleConfig> {
        let index = self.select_index(slot)?;
        Ok(&mut self.modules[index])
    }

    fn select_index(&self, slot: Option<u8>) -> Result<usize> {
        match slot {
            Some(slot) => self
                .modules
                .iter()
                .position(|module| module.slot == slot)
                .ok_or_else(|| {
                    Error::user(format!(
                        "no module is configured in slot {slot}; configured: {}",
                        self.slot_list()
                    ))
                }),
            None if self.modules.len() == 1 => Ok(0),
            None => Err(Error::user(format!(
                "{} modules are configured (slots {}); say which one with --slot",
                self.modules.len(),
                self.slot_list()
            ))),
        }
    }

    /// What a supervisor needs to run one module.
    pub fn setup(&self, module: &ModuleConfig) -> ModuleSetup {
        ModuleSetup {
            module: module.clone(),
            publish_interval_s: self.publish_interval_s,
            multi: self.is_multi(),
        }
    }

    pub fn setups(&self) -> Vec<ModuleSetup> {
        self.modules
            .iter()
            .map(|module| self.setup(module))
            .collect()
    }

    /// Put an edited module back, matched by slot.
    pub fn replace_module(&mut self, module: ModuleConfig) -> Result<()> {
        let slot = module.slot;
        let index = self.select_index(Some(slot))?;
        self.modules[index] = module;
        Ok(())
    }

    /// Add a module with every default. Returns notes worth telling whoever
    /// asked, because a second module changes things about the first one.
    pub fn add_module(&mut self, slot: u8) -> Result<Vec<String>> {
        check_slot(slot)?;
        if self.module(slot).is_some() {
            return Err(Error::user(format!("slot {slot} is already configured")));
        }

        let mut notes = Vec::new();

        if self.form == Form::Single {
            self.form = Form::List;
            notes.push("the file now holds a module list".to_string());
            for module in &mut self.modules {
                // The list form always pushes a role; say so, because on a
                // hardware v1 module the firmware's own default is slave.
                if module.isospi_role.is_none() {
                    module.isospi_role = Some(Role::Master);
                    notes.push(format!(
                        "slot {} had no isospi_role and is now master, the default in a module \
                         list; change it with: go-multibus role --slot {} slave",
                        module.slot, module.slot
                    ));
                }
                // The one-module default path would be shared by every module
                // in a list; leave it out so each gets its own.
                if module.output.as_deref() == Some(OUTPUT_PATH) {
                    module.output = None;
                }
            }
        }

        let was_multi = self.is_multi();
        self.modules.push(ModuleConfig::new(slot));
        self.modules.sort_by_key(|module| module.slot);

        if !was_multi {
            for module in &self.modules {
                let names = Names::for_slot(module.slot);
                notes.push(format!(
                    "slot {} is now named {}..3, {}..3 and {}, and publishes to {}",
                    module.slot,
                    names.can(0),
                    names.serial(1),
                    names.protocol(),
                    self.setup(module).output()
                ));
            }
        }

        self.validate()?;
        Ok(notes)
    }

    /// Remove a module. The last one stays: an empty list is not a working
    /// configuration, and deleting the file is the way back to the defaults.
    pub fn remove_module(&mut self, slot: u8) -> Result<Vec<String>> {
        let index = self.select_index(Some(slot))?;
        if self.modules.len() == 1 {
            return Err(Error::user(format!(
                "slot {slot} is the only module configured; there has to be one"
            )));
        }
        self.modules.remove(index);

        let mut notes = Vec::new();
        if !self.is_multi() {
            let module = &self.modules[0];
            let names = Names::legacy();
            notes.push(format!(
                "one module is left, so slot {} goes back to {}..3, {}..3 and {}, and publishes \
                 to {}",
                module.slot,
                names.can(0),
                names.serial(1),
                names.protocol(),
                self.setup(module).output()
            ));
        }
        Ok(notes)
    }
}

fn check_slot(slot: u8) -> Result<()> {
    if !(1..=SLOT_SPIDEV.len() as u8).contains(&slot) {
        return Err(Error::user(format!(
            "slot {slot} does not exist; slots are 1..{}",
            SLOT_SPIDEV.len()
        )));
    }
    Ok(())
}

/// Write one module's settings back into the file at `path`, leaving every
/// other module as the file has it.
///
/// What the UI uses: each module there has its own supervisor holding its own
/// copy, so saving one must not put the others back to what they were when
/// the UI started.
pub fn store_module(path: &str, module: &ModuleConfig) -> Result<()> {
    let mut config = Config::load(path)?;
    config.replace_module(module.clone())?;
    config.save(path)
}

/// One module together with what the rest of the file decides for it.
#[derive(Clone, Debug)]
pub struct ModuleSetup {
    pub module: ModuleConfig,
    pub publish_interval_s: f64,
    /// More than one module configured: names and documents carry the slot.
    pub multi: bool,
}

impl ModuleSetup {
    /// Where the cell data goes: the configured path, else the one-module
    /// default, else one per slot.
    pub fn output(&self) -> String {
        match &self.module.output {
            Some(path) => path.clone(),
            None if self.multi => slot_output_path(self.module.slot),
            None => OUTPUT_PATH.to_string(),
        }
    }

    pub fn state_path(&self) -> String {
        if self.multi {
            slot_state_path(self.module.slot)
        } else {
            STATE_PATH.to_string()
        }
    }

    pub fn names(&self) -> Names {
        if self.multi {
            Names::for_slot(self.module.slot)
        } else {
            Names::legacy()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What 0.2.0 wrote for a default configuration, byte for byte.
    const LEGACY_DEFAULT_TEXT: &str = r#"{
  "slot": 8,
  "interfaces": [
    "can",
    "rs485",
    "can",
    "rs485",
    "isospi"
  ],
  "cell_count": 12,
  "module_poll_ms": 100,
  "publish_interval_s": 0.5,
  "bring_up_can": true,
  "can_bitrate": 500000,
  "output": "/dev/shm/gocontroll/multibus-cells.json",
  "supply_enabled": false,
  "supply_mv": 5000
}
"#;

    const TWO_MODULES: &str = r#"{
        "publish_interval_s": 1.0,
        "modules": [
            { "slot": 7, "isospi_role": "slave", "interfaces": ["can", "rs232", "rs485", "can", "isospi"],
              "cell_count": 24, "supply_enabled": true, "supply_mv": 9000 },
            { "slot": 8 }
        ]
    }"#;

    #[test]
    fn defaults_round_trip_through_json() {
        let text = Config::default().to_text().unwrap();
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed.form, Form::Single);
        assert_eq!(parsed.modules.len(), 1);
        assert_eq!(parsed.modules[0].slot, 8);
        assert_eq!(parsed.modules[0].interfaces, default_interfaces());
        assert!(!parsed.modules[0].supply_enabled);
    }

    #[test]
    fn a_one_module_file_is_written_back_exactly_as_before() {
        assert_eq!(Config::default().to_text().unwrap(), LEGACY_DEFAULT_TEXT);

        let parsed = Config::parse(LEGACY_DEFAULT_TEXT).unwrap();
        assert_eq!(parsed.to_text().unwrap(), LEGACY_DEFAULT_TEXT);
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
        let parsed = Config::parse(text).unwrap();
        assert!(!parsed.is_multi());
        let module = &parsed.modules[0];
        assert_eq!(module.slot, 6);
        assert_eq!(module.cell_count, 24);
        assert!(!module.supply_enabled);
        assert_eq!(module.supply_mv, 5000);
        assert_eq!(parsed.publish_interval_s, 1.0);
        assert_eq!(parsed.setup(module).output(), "/dev/shm/x.json");
    }

    #[test]
    fn one_module_keeps_the_names_and_documents_it_always_had() {
        let config = Config::default();
        let setup = &config.setups()[0];
        assert!(!setup.multi);
        assert_eq!(setup.output(), OUTPUT_PATH);
        assert_eq!(setup.state_path(), STATE_PATH);
        assert_eq!(setup.names(), Names::legacy());
        // A one-module file that never set a role leaves the module alone.
        assert_eq!(setup.module.isospi_role, None);
    }

    #[test]
    fn a_role_set_in_a_one_module_file_is_kept_at_the_end() {
        let mut config = Config::default();
        config.modules[0].isospi_role = Some(Role::Slave);
        let text = config.to_text().unwrap();
        assert!(text.ends_with("  \"supply_mv\": 5000,\n  \"isospi_role\": \"slave\"\n}\n"));
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed.form, Form::Single);
        assert_eq!(parsed.modules[0].isospi_role, Some(Role::Slave));
    }

    #[test]
    fn a_module_list_loads_with_defaults_filled_in() {
        let config = Config::parse(TWO_MODULES).unwrap();
        assert_eq!(config.form, Form::List);
        assert!(config.is_multi());
        assert_eq!(config.publish_interval_s, 1.0);
        assert_eq!(config.slots(), vec![7, 8]);

        let seven = config.module(7).unwrap();
        assert_eq!(seven.isospi_role, Some(Role::Slave));
        assert_eq!(seven.cell_count, 24);
        assert_eq!(seven.supply_mv, 9000);

        let eight = config.module(8).unwrap();
        assert_eq!(eight.isospi_role, Some(Role::Master), "the list default");
        assert_eq!(eight.interfaces, default_interfaces());
        assert_eq!(eight.cell_count, 12);
        assert_eq!(eight.module_poll_ms, 100);
        assert!(eight.bring_up_can);
        assert_eq!(eight.can_bitrate, 500_000);
        assert!(!eight.supply_enabled);
        assert_eq!(eight.supply_mv, 5000);
        assert_eq!(eight.output, None);
    }

    #[test]
    fn several_modules_get_names_and_documents_per_slot() {
        let config = Config::parse(TWO_MODULES).unwrap();
        let setups = config.setups();
        assert!(setups.iter().all(|setup| setup.multi));
        assert_eq!(setups[0].names().can(0), "mb7_can1");
        assert_eq!(setups[1].names().protocol(), "/dev/mb8_protocol");
        assert_eq!(
            setups[0].output(),
            "/dev/shm/gocontroll/multibus-slot7-cells.json"
        );
        assert_eq!(
            setups[1].state_path(),
            "/dev/shm/gocontroll/multibus-slot8-state.json"
        );
    }

    #[test]
    fn a_one_entry_list_behaves_like_one_module() {
        let config = Config::parse(r#"{ "modules": [ { "slot": 7 } ] }"#).unwrap();
        assert_eq!(config.form, Form::List);
        assert!(!config.is_multi());
        let setup = &config.setups()[0];
        assert_eq!(setup.names(), Names::legacy());
        assert_eq!(setup.output(), OUTPUT_PATH);
        assert_eq!(setup.state_path(), STATE_PATH);
        assert_eq!(config.publish_interval_s, 0.5);

        // And it stays a list when written back.
        let text = config.to_text().unwrap();
        assert!(text.contains("\"modules\""));
        assert!(Config::parse(&text).unwrap().form == Form::List);
    }

    #[test]
    fn a_slot_configured_twice_is_refused() {
        let text = r#"{ "modules": [ { "slot": 7 }, { "slot": 8 }, { "slot": 7 } ] }"#;
        let failure = Config::parse(text).unwrap_err();
        assert!(failure.to_string().contains("slot 7 is configured twice"));
    }

    #[test]
    fn slots_outside_the_controller_are_refused() {
        assert!(Config::parse(r#"{ "modules": [ { "slot": 0 } ] }"#).is_err());
        assert!(Config::parse(r#"{ "modules": [ { "slot": 9 } ] }"#).is_err());
        assert!(Config::parse(r#"{ "modules": [ { "slot": 1 }, { "slot": 8 } ] }"#).is_ok());
    }

    #[test]
    fn a_list_entry_needs_a_slot_and_the_list_an_entry() {
        assert!(Config::parse(r#"{ "modules": [ { "cell_count": 12 } ] }"#).is_err());
        assert!(Config::parse(r#"{ "modules": [] }"#).is_err());
    }

    #[test]
    fn two_modules_cannot_share_an_output() {
        let text = r#"{ "modules": [
            { "slot": 7, "output": "/dev/shm/cells.json" },
            { "slot": 8, "output": "/dev/shm/cells.json" } ] }"#;
        assert!(Config::parse(text).is_err());
    }

    #[test]
    fn an_unknown_role_is_refused() {
        let text = r#"{ "modules": [ { "slot": 7, "isospi_role": "monitor" } ] }"#;
        assert!(Config::parse(text).is_err());
    }

    #[test]
    fn the_slot_has_to_be_named_once_there_are_two() {
        let mut config = Config::parse(TWO_MODULES).unwrap();
        assert!(config.select(None).is_err());
        assert_eq!(config.select(Some(8)).unwrap().slot, 8);
        assert!(config.select(Some(3)).is_err());
        config.select_mut(Some(7)).unwrap().cell_count = 6;
        assert_eq!(config.module(7).unwrap().cell_count, 6);

        let single = Config::default();
        assert_eq!(single.select(None).unwrap().slot, 8);
        assert!(single.select(Some(7)).is_err());
    }

    #[test]
    fn adding_a_second_module_turns_a_one_module_file_into_a_list() {
        let mut config = Config::default();
        let notes = config.add_module(7).unwrap();
        assert!(!notes.is_empty());
        assert_eq!(config.form, Form::List);
        assert_eq!(config.slots(), vec![7, 8]);

        let eight = config.module(8).unwrap();
        assert_eq!(eight.isospi_role, Some(Role::Master));
        assert_eq!(eight.output, None, "the shared default path is dropped");
        assert_eq!(
            config.setup(eight).output(),
            "/dev/shm/gocontroll/multibus-slot8-cells.json"
        );

        let reread = Config::parse(&config.to_text().unwrap()).unwrap();
        assert_eq!(reread.slots(), vec![7, 8]);
        assert!(reread.is_multi());

        assert!(config.add_module(7).is_err(), "already there");
        assert!(config.add_module(9).is_err(), "no such slot");
    }

    #[test]
    fn removing_down_to_one_leaves_a_one_entry_list() {
        let mut config = Config::parse(TWO_MODULES).unwrap();
        config.remove_module(7).unwrap();
        assert_eq!(config.form, Form::List);
        assert_eq!(config.slots(), vec![8]);
        assert!(!config.is_multi());
        assert_eq!(config.setups()[0].names(), Names::legacy());

        assert!(config.remove_module(8).is_err(), "the last one stays");
        assert!(config.remove_module(5).is_err(), "never configured");
    }

    #[test]
    fn can_channels_follow_the_configured_modes() {
        let mut module = ModuleConfig::new(8);
        // can, rs485, can, rs485, isospi -> channels 0 and 1
        assert_eq!(module.can_channels().unwrap(), vec![0, 1]);

        module.set_interface(4, Mode::Can).unwrap();
        assert_eq!(module.can_channels().unwrap(), vec![0, 1, 2]);

        module.set_interface(3, Mode::Rs485).unwrap();
        assert_eq!(module.can_channels().unwrap(), vec![0, 2]);
    }

    #[test]
    fn a_truncated_interfaces_list_is_reported_rather_than_panicking() {
        let mut module = ModuleConfig::new(8);
        module.interfaces.truncate(3);
        assert!(module.set_interface(2, Mode::Rs232).is_err());
        assert!(module.interface_modes().is_err());
        assert!(module.can_channels().is_err());
    }

    #[test]
    fn an_interface_refuses_a_mode_its_hardware_cannot_serve() {
        let mut module = ModuleConfig::new(8);
        assert!(module.set_interface(3, Mode::Rs232).is_err());
        assert!(module.set_interface(1, Mode::Rs485).is_err());
        assert!(module.set_interface(2, Mode::Rs232).is_ok());
    }
}
