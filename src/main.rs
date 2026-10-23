//! go-multibus — run and configure the Multibus modules on a GOcontroll controller.
//!
//! Two faces over one state machine. The daemon is what runs at boot; the
//! terminal UI is what you reach for while commissioning a machine. Both drive
//! the same sequence — reset, start the application, configure over SPI, wait
//! for USB, then poll isoSPI — so what the UI shows is what the service does.
//!
//! A controller can carry more than one module. Each runs through that
//! sequence on its own, and every command that is about one module takes
//! `--slot`, which may be left out while only one is configured.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::Value;

use go_multibus::config::{Config, ModuleSetup, CONFIG_PATH};
use go_multibus::discover;
use go_multibus::error::{Error, Result};
use go_multibus::link::MultibusLink;
use go_multibus::namemap;
use go_multibus::proto::{format_volts, status as answer, Mode, Role, INTERFACES};
use go_multibus::service::{State, Supervisor};
use go_multibus::tui;

#[derive(Parser)]
#[command(
    name = "go-multibus",
    version,
    about = "GOcontroll Multibus module service and configuration UI",
    long_about = "Run with no arguments for the interactive UI."
)]
struct Cli {
    /// Configuration file.
    #[arg(short, long, default_value = CONFIG_PATH, global = true)]
    config: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run as a background service: bring every module up and keep it publishing.
    Daemon,

    /// One-shot summary of what the service is doing, for every module.
    Status {
        /// Only this module.
        #[arg(long)]
        slot: Option<u8>,
        /// Print the published documents rather than a summary.
        #[arg(long)]
        json: bool,
    },

    /// Show the stored configuration.
    Config,

    /// Change one connector interface and store it.
    Set {
        /// The module, when more than one is configured.
        #[arg(long)]
        slot: Option<u8>,
        /// Connector interface, 1..5.
        interface: u8,
        /// can, rs485, rs232, isospi or off.
        mode: String,
    },

    /// Set the external supply on the connector.
    Supply {
        /// The module, when more than one is configured.
        #[arg(long)]
        slot: Option<u8>,
        /// `off`, or a voltage such as `9`, `9V` or `9000mv`.
        setting: String,
    },

    /// Show the isoSPI role, or set it: stored, and applied straight away.
    Role {
        /// The module, when more than one is configured.
        #[arg(long)]
        slot: Option<u8>,
        /// `master` or `slave`. Shows the role when left out.
        role: Option<String>,
    },

    /// Add a module to the configuration, or remove one.
    Module {
        #[command(subcommand)]
        action: ModuleAction,
    },

    /// Show the Multibus modules present, with their ports and CAN interfaces.
    Ports,
}

#[derive(Subcommand)]
enum ModuleAction {
    /// Add the module in this slot, with every setting at its default.
    Add {
        /// Controller slot, 1..8.
        slot: u8,
    },
    /// Remove the module in this slot from the configuration.
    Remove {
        /// Controller slot, 1..8.
        slot: u8,
    },
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(match run(cli) {
        Ok(()) => 0,
        Err(failure) => {
            eprintln!("error: {failure}");
            failure.exit_code()
        }
    });
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => tui::run_checked(Some(&cli.config)),
        Some(Command::Daemon) => daemon(&cli.config),
        Some(Command::Status { slot, json }) => status(&cli.config, slot, json),
        Some(Command::Config) => show_config(&cli.config),
        Some(Command::Set {
            slot,
            interface,
            mode,
        }) => set_interface(&cli.config, slot, interface, &mode),
        Some(Command::Supply { slot, setting }) => set_supply(&cli.config, slot, &setting),
        Some(Command::Role { slot, role }) => set_role(&cli.config, slot, role.as_deref()),
        Some(Command::Module { action }) => match action {
            ModuleAction::Add { slot } => module_add(&cli.config, slot),
            ModuleAction::Remove { slot } => module_remove(&cli.config, slot),
        },
        Some(Command::Ports) => ports(),
    }
}

// ── daemon ───────────────────────────────────────────────────────────────────

static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
}

/// Stops every module thread when one of them panics, so the process exits and
/// systemd restarts it - the same as a panic did while there was only one
/// module and one thread. Without it the other modules would carry on and the
/// panicked one would just be gone.
struct StopOnPanic;

impl Drop for StopOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            STOPPING.store(true, Ordering::SeqCst);
        }
    }
}

fn daemon(config_path: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let multi = config.is_multi();

    // The name map has to be right before the first module is reset, because
    // udev reads it the moment a module enumerates.
    if multi {
        let prefixes: Vec<String> = config
            .setups()
            .iter()
            .map(|setup| setup.names().prefix().to_string())
            .collect();
        if let Err(failure) = namemap::prepare(&prefixes) {
            eprintln!("{failure}");
        }
    } else {
        // One module is named the way it always was, which is what udev does
        // when there is no map. A map left from a configuration that used to
        // have more modules would give it the slot names instead.
        match namemap::clear() {
            Ok(true) => eprintln!(
                "one module configured: removed the name map left by a configuration with more"
            ),
            Ok(false) => {}
            Err(failure) => eprintln!("{failure}"),
        }
    }

    // SAFETY: the handler only sets an atomic flag, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    // One thread per module. A step can block for seconds - waiting for USB,
    // or for go-can - and one module's bring-up must not hold up another's
    // publishing.
    let mut workers = Vec::new();
    for setup in config.setups() {
        let slot = setup.module.slot;
        let worker = std::thread::Builder::new()
            .name(format!("slot{slot}"))
            .spawn(move || supervise(setup, multi))
            .map_err(|error| {
                STOPPING.store(true, Ordering::SeqCst);
                Error::system(format!("cannot start a thread for slot {slot}: {error}"))
            })?;
        workers.push((slot, worker));
    }

    let mut panicked = Vec::new();
    for (slot, worker) in workers {
        if worker.join().is_err() {
            panicked.push(slot.to_string());
        }
    }
    if !panicked.is_empty() {
        return Err(Error::system(format!(
            "the supervisor for slot {} stopped unexpectedly",
            panicked.join(", ")
        )));
    }
    Ok(())
}

/// Drive one module until the service is asked to stop.
fn supervise(setup: ModuleSetup, multi: bool) {
    let _stop_on_panic = StopOnPanic;
    let slot = setup.module.slot;

    // With several modules every line says which one it is about; with one,
    // the journal reads exactly as it always did.
    let sink: Box<dyn FnMut(&str)> = if multi {
        Box::new(move |message: &str| eprintln!("slot {slot}: {message}"))
    } else {
        Box::new(|message: &str| eprintln!("{message}"))
    };

    let mut supervisor = Supervisor::new(setup, sink);
    while !STOPPING.load(Ordering::SeqCst) {
        supervisor.step();
        // A tenth of a second is short enough that the publish interval is
        // honoured accurately, and long enough to keep the loop cheap while the
        // module is only being polled.
        std::thread::sleep(Duration::from_millis(100));
    }

    supervisor.close();
}

// ── one-shot reporting ───────────────────────────────────────────────────────

fn read_json(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn status(config_path: &str, slot: Option<u8>, json: bool) -> Result<()> {
    let config = Config::load(config_path)?;
    if let Some(slot) = slot {
        config.select(Some(slot))?;
    }
    let setups: Vec<ModuleSetup> = config
        .setups()
        .into_iter()
        .filter(|setup| slot.is_none_or(|wanted| setup.module.slot == wanted))
        .collect();

    let states: Vec<Option<Value>> = setups
        .iter()
        .map(|setup| read_json(&setup.state_path()))
        .collect();

    if states.iter().all(Option::is_none) {
        let paths: Vec<String> = setups.iter().map(ModuleSetup::state_path).collect();
        return Err(Error::user(format!(
            "the service is not running (no {})",
            paths.join(", ")
        )));
    }

    if json {
        let mut documents: Vec<Value> = setups
            .iter()
            .zip(&states)
            .map(|(setup, state)| {
                serde_json::json!({
                    "slot": setup.module.slot,
                    "state": state,
                    "cells": read_json(&setup.output()),
                })
            })
            .collect();

        // One module prints the two documents the way it always did; several
        // get a list.
        let document = if documents.len() == 1 {
            let mut only = documents.remove(0);
            if let Some(object) = only.as_object_mut() {
                object.remove("slot");
            }
            only
        } else {
            serde_json::json!({ "modules": documents })
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    for (index, (setup, state)) in setups.iter().zip(&states).enumerate() {
        if index > 0 {
            println!();
        }
        match state {
            Some(state) => print_status(setup, state),
            None => {
                println!("slot       : {}", setup.module.slot);
                println!("state      : nothing published - is the service running?");
            }
        }
    }
    Ok(())
}

fn print_status(setup: &ModuleSetup, state: &Value) {
    let name = state["state"].as_str().unwrap_or("unknown");
    let label = State::ALL
        .iter()
        .find(|state| state.as_str() == name)
        .map(|state| state.label())
        .unwrap_or(name);

    println!("state      : {label}");
    println!("slot       : {}", state["slot"]);

    // Only published with more than one module, where it matters which is
    // which and which end of the isoSPI link each one drives.
    if setup.multi {
        let role = &state["isospi_role"];
        let configured = role["configured"].as_str().unwrap_or("not set");
        match role["reported"].as_str() {
            Some(reported) if reported != configured => {
                println!("role       : {configured} (module has {reported})")
            }
            _ => println!("role       : {configured}"),
        }
        println!(
            "usb serial : {}",
            state["usb_serial"].as_str().unwrap_or("not known yet")
        );
    }

    let interfaces = state["interfaces"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("interfaces : {interfaces}");

    match state["supply"].as_object() {
        Some(supply) => {
            let enabled = supply["enabled"].as_bool().unwrap_or(false);
            let millivolts = supply["millivolts"].as_u64().unwrap_or(0) as u16;
            let stored = supply["stored"].as_bool().unwrap_or(false);
            println!(
                "supply     : {}{}",
                if enabled {
                    format_volts(millivolts)
                } else {
                    "off".to_string()
                },
                if stored { "" } else { " (not stored in the module)" }
            );
        }
        None => println!("supply     : not reported by this firmware"),
    }

    if let Some(error) = state["last_error"].as_str().filter(|text| !text.is_empty()) {
        println!("last error : {error}");
    }

    match read_json(&setup.output()) {
        Some(cells) if cells["valid"].as_bool().unwrap_or(false) => println!(
            "cells      : {}, pack {:.3} V, spread {:.4} V{}",
            cells["cell_count"].as_u64().unwrap_or(0),
            cells["pack_v"].as_f64().unwrap_or(0.0),
            cells["spread_v"].as_f64().unwrap_or(0.0),
            if cells["simulated"].as_bool().unwrap_or(false) {
                " (simulated)"
            } else {
                ""
            }
        ),
        Some(_) => println!("cells      : no valid data"),
        None => {}
    }
}

fn show_config(config_path: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    print!("{}", config.to_text()?);
    Ok(())
}

/// "slot 7: " in front of a message once there is more than one module to tell
/// apart, nothing while there is one.
fn slot_prefix(config: &Config, slot: u8) -> String {
    if config.is_multi() {
        format!("slot {slot}: ")
    } else {
        String::new()
    }
}

fn set_interface(config_path: &str, slot: Option<u8>, interface: u8, mode: &str) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let mode = Mode::parse(mode)?;
    let module = config.select_mut(slot)?;
    module.set_interface(interface, mode)?;
    let slot = module.slot;
    config.save(config_path)?;

    println!(
        "{}interface {interface} is now {} in {config_path}",
        slot_prefix(&config, slot),
        mode.as_str()
    );
    println!("restart the service for it to take effect: systemctl restart go-multibus");
    Ok(())
}

/// Parse `off`, `9`, `9V`, `9.0v` or `9000mv` into a supply setting.
fn parse_supply(setting: &str) -> Result<(bool, Option<u16>)> {
    let text = setting.trim().to_ascii_lowercase();
    if text == "off" {
        return Ok((false, None));
    }

    let (number, scale) = if let Some(rest) = text.strip_suffix("mv") {
        (rest, 1.0)
    } else if let Some(rest) = text.strip_suffix('v') {
        (rest, 1000.0)
    } else {
        (text.as_str(), 1000.0)
    };

    let value: f64 = number.trim().parse().map_err(|_| {
        Error::user(format!(
            "cannot read {setting} as a voltage; use off, or a value such as 9 or 9V"
        ))
    })?;

    let millivolts = (value * scale).round();
    if !(0.0..=65535.0).contains(&millivolts) {
        return Err(Error::user(format!("{setting} is not a usable voltage")));
    }
    Ok((true, Some(millivolts as u16)))
}

/// The protocol port of one configured module.
///
/// With one module that is whichever Multibus module is present, as it always
/// was. With several it is the one the name map puts in that slot.
fn open_module_link(config: &Config, slot: u8) -> Result<MultibusLink> {
    if config.is_multi() {
        let device = discover::protocol_port_for_slot(slot)?;
        MultibusLink::open(Some(&device))
    } else {
        MultibusLink::open(None)
    }
}

fn set_supply(config_path: &str, slot: Option<u8>, setting: &str) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let (enabled, millivolts) = parse_supply(setting)?;

    let module = config.select_mut(slot)?;
    if let Some(millivolts) = millivolts {
        module.supply_mv = millivolts;
    }
    module.supply_enabled = enabled;
    let slot = module.slot;
    let supply_mv = module.supply_mv;
    config.save(config_path)?;

    // Stored first, then applied. The module keeps its own copy in flash, but
    // the file is what the next bring-up pushes, so storing it is the part that
    // must not be skipped when the module happens to be unreachable.
    let stored = if enabled {
        format_volts(supply_mv)
    } else {
        "off".to_string()
    };
    let prefix = slot_prefix(&config, slot);
    println!("{prefix}external supply set to {stored} in {config_path}");

    match open_module_link(&config, slot) {
        Ok(mut link) => {
            let supply = link.set_supply(enabled, supply_mv)?;
            println!("applied to the module: {}", supply.summary());
            if !supply.stored {
                println!("warning: the module could not store it, so a reset loses it");
            }
        }
        Err(failure) => {
            println!("the module is not reachable ({failure});");
            println!("the setting is stored and will be applied on the next bring-up");
        }
    }
    Ok(())
}

/// Show the isoSPI role, or store one and apply it.
///
/// Like the supply, the role needs no reset: the module switches on the spot.
/// Unlike the supply the module does not remember it, so the file is what
/// carries it over a reset - every bring-up pushes it again.
fn set_role(config_path: &str, slot: Option<u8>, role: Option<&str>) -> Result<()> {
    let mut config = Config::load(config_path)?;

    let Some(role) = role else {
        let module = config.select(slot)?;
        let slot = module.slot;
        match module.isospi_role {
            Some(role) => println!("configured : {role}"),
            None => println!(
                "configured : not set - the module keeps the role its firmware starts in"
            ),
        }
        match open_module_link(&config, slot) {
            Ok(mut link) => match link.get_role() {
                Ok(role) => println!("module     : {role}"),
                Err(failure) if failure.is_unknown_command() => {
                    let info = link.info()?;
                    println!(
                        "module     : {} (fixed: firmware older than 0.3.0 cannot switch)",
                        info.role_name()
                    );
                }
                Err(failure) => return Err(failure),
            },
            Err(failure) => println!("module     : not reachable ({failure})"),
        }
        return Ok(());
    };

    let role = Role::parse(role)?;
    let module = config.select_mut(slot)?;
    module.isospi_role = Some(role);
    let slot = module.slot;
    config.save(config_path)?;

    let prefix = slot_prefix(&config, slot);
    println!("{prefix}isoSPI role set to {role} in {config_path}");

    match open_module_link(&config, slot) {
        Ok(mut link) => match link.set_role(role) {
            Ok(active) => println!("applied to the module: {active}"),
            Err(failure) if failure.is_unknown_command() => {
                println!("this firmware cannot switch its isoSPI role; flash 0.3.0 or newer.");
                println!("the role is stored and will be pushed once it can");
            }
            Err(failure) if failure.is_status(answer::BAD_PARAMETER) => {
                let active = link.get_role()?;
                return Err(Error::module(format!(
                    "the module refused {role}; it runs as {active}"
                )));
            }
            Err(failure) => return Err(failure),
        },
        Err(failure) => {
            println!("the module is not reachable ({failure});");
            println!("the role is stored and will be applied on the next bring-up");
        }
    }
    Ok(())
}

fn module_add(config_path: &str, slot: u8) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let notes = config.add_module(slot)?;
    config.save(config_path)?;

    println!(
        "slot {slot} added to {config_path}; configured slots: {}",
        config
            .slots()
            .iter()
            .map(|slot| slot.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for note in notes {
        println!("{note}");
    }
    println!("restart the service for it to take effect: systemctl restart go-multibus");
    Ok(())
}

fn module_remove(config_path: &str, slot: u8) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let notes = config.remove_module(slot)?;
    config.save(config_path)?;

    println!("slot {slot} removed from {config_path}");
    for note in notes {
        println!("{note}");
    }
    println!("restart the service for it to take effect: systemctl restart go-multibus");
    Ok(())
}

fn ports() -> Result<()> {
    let modules = discover::usb_modules();
    if modules.is_empty() {
        println!("no Multibus modules found");
        return Ok(());
    }

    for (index, module) in modules.iter().enumerate() {
        if index > 0 {
            println!();
        }

        let slot = namemap::slot_for_serial(&module.serial)
            .map(|slot| format!("   slot {slot}"))
            .unwrap_or_default();
        let serial = if module.serial.is_empty() {
            "(no serial number)"
        } else {
            module.serial.as_str()
        };
        println!("module {serial}{slot}   usb {}", module.usb_path);

        for port in discover::module_ports(module) {
            let role = if port.is_protocol_port {
                "module protocol".to_string()
            } else {
                match port.connector_interface {
                    Some(number) => {
                        let pins = INTERFACES
                            .iter()
                            .find(|spec| spec.number == number)
                            .map(|spec| spec.pins)
                            .unwrap_or("?");
                        format!("connector interface {number} (pins {pins})")
                    }
                    None => "unknown".to_string(),
                }
            };
            let link = discover::port_link(&port).unwrap_or_else(|| "-".to_string());
            println!(
                "  {link:<18} {:<14} usb interface {:<2}  {role}",
                port.device, port.usb_interface
            );
        }

        let can = discover::module_can_interfaces(module);
        if can.is_empty() {
            println!("  no gs_usb CAN interfaces");
        }
        for (channel, name) in can.iter().enumerate() {
            let interface = discover::CAN_CHANNEL_INTERFACE
                .get(channel)
                .map(|number| number.to_string())
                .unwrap_or_else(|| "?".to_string());
            println!("  {name:<18} gs_usb channel {channel}   connector interface {interface}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_supply, Cli, Command};
    use clap::{CommandFactory, Parser};

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_slot_goes_before_or_after_the_values() {
        let cli = Cli::parse_from(["go-multibus", "set", "--slot", "7", "3", "can"]);
        assert!(matches!(
            cli.command,
            Some(Command::Set { slot: Some(7), interface: 3, .. })
        ));
        let cli = Cli::parse_from(["go-multibus", "role", "slave", "--slot", "8"]);
        assert!(matches!(cli.command, Some(Command::Role { slot: Some(8), .. })));
        let cli = Cli::parse_from(["go-multibus", "supply", "off"]);
        assert!(matches!(cli.command, Some(Command::Supply { slot: None, .. })));
    }

    #[test]
    fn supply_settings_parse_the_way_they_are_typed() {
        assert_eq!(parse_supply("off").unwrap(), (false, None));
        assert_eq!(parse_supply("9").unwrap(), (true, Some(9000)));
        assert_eq!(parse_supply("9V").unwrap(), (true, Some(9000)));
        assert_eq!(parse_supply(" 12v ").unwrap(), (true, Some(12000)));
        assert_eq!(parse_supply("9000mv").unwrap(), (true, Some(9000)));
        assert!(parse_supply("nine").is_err());
    }
}
