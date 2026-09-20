//! go-multibus — run and configure the Multibus module on a GOcontroll controller.
//!
//! Two faces over one state machine. The daemon is what runs at boot; the
//! terminal UI is what you reach for while commissioning a machine. Both drive
//! the same sequence — reset, start the application, configure over SPI, wait
//! for USB, then poll isoSPI — so what the UI shows is what the service does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::Value;

use go_multibus::config::{Config, CONFIG_PATH, STATE_PATH};
use go_multibus::discover;
use go_multibus::error::{Error, Result};
use go_multibus::proto::{format_volts, Mode, INTERFACES};
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
    /// Run as a background service: bring the module up and keep it publishing.
    Daemon,

    /// One-shot summary of what the service is doing.
    Status {
        /// Print the published documents rather than a summary.
        #[arg(long)]
        json: bool,
    },

    /// Show the stored configuration.
    Config,

    /// Change one connector interface and store it.
    Set {
        /// Connector interface, 1..5.
        interface: u8,
        /// can, rs485, rs232, isospi or off.
        mode: String,
    },

    /// Set the external supply on the connector.
    Supply {
        /// `off`, or a voltage such as `9`, `9V` or `9000mv`.
        setting: String,
    },

    /// Show the Multibus ports and CAN interfaces present on this controller.
    Ports,
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
        Some(Command::Status { json }) => status(&cli.config, json),
        Some(Command::Config) => show_config(&cli.config),
        Some(Command::Set { interface, mode }) => set_interface(&cli.config, interface, &mode),
        Some(Command::Supply { setting }) => set_supply(&cli.config, &setting),
        Some(Command::Ports) => ports(),
    }
}

// ── daemon ───────────────────────────────────────────────────────────────────

static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
}

fn daemon(config_path: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let mut supervisor = Supervisor::new(
        config,
        Box::new(|message: &str| eprintln!("{message}")),
    );

    // SAFETY: the handler only sets an atomic flag, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    while !STOPPING.load(Ordering::SeqCst) {
        supervisor.step();
        // A tenth of a second is short enough that the publish interval is
        // honoured accurately, and long enough to keep the loop cheap while the
        // module is only being polled.
        std::thread::sleep(Duration::from_millis(100));
    }

    supervisor.close();
    Ok(())
}

// ── one-shot reporting ───────────────────────────────────────────────────────

fn read_json(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn status(config_path: &str, json: bool) -> Result<()> {
    let config = Config::load(config_path)?;

    let Some(state) = read_json(STATE_PATH) else {
        return Err(Error::user(format!(
            "the service is not running (no {STATE_PATH})"
        )));
    };
    let cells = read_json(&config.output);

    if json {
        let document = serde_json::json!({ "state": state, "cells": cells });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    let name = state["state"].as_str().unwrap_or("unknown");
    let label = [
        State::Reset,
        State::Start,
        State::Configure,
        State::Enumerate,
        State::Running,
        State::Failed,
    ]
    .iter()
    .find(|state| state.as_str() == name)
    .map(|state| state.label())
    .unwrap_or(name);

    println!("state      : {label}");
    println!("slot       : {}", state["slot"]);

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

    match cells {
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

    Ok(())
}

fn show_config(config_path: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

fn set_interface(config_path: &str, interface: u8, mode: &str) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let mode = Mode::parse(mode)?;
    config.set_interface(interface, mode)?;
    config.save(config_path)?;

    println!(
        "interface {interface} is now {} in {config_path}",
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

fn set_supply(config_path: &str, setting: &str) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let (enabled, millivolts) = parse_supply(setting)?;

    if let Some(millivolts) = millivolts {
        config.supply_mv = millivolts;
    }
    config.supply_enabled = enabled;
    config.save(config_path)?;

    // Stored first, then applied. The module keeps its own copy in flash, but
    // the file is what the next bring-up pushes, so storing it is the part that
    // must not be skipped when the module happens to be unreachable.
    let stored = if enabled {
        format_volts(config.supply_mv)
    } else {
        "off".to_string()
    };
    println!("external supply set to {stored} in {config_path}");

    match go_multibus::link::MultibusLink::open(None) {
        Ok(mut link) => {
            let supply = link.set_supply(config.supply_enabled, config.supply_mv)?;
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

fn ports() -> Result<()> {
    let ports = discover::enumerate_ports();
    if ports.is_empty() {
        println!("no Multibus ports found");
    } else {
        for port in &ports {
            let role = if port.is_protocol_port {
                "module protocol".to_string()
            } else {
                match port.connector_interface {
                    Some(number) => {
                        let mode = INTERFACES
                            .iter()
                            .find(|spec| spec.number == number)
                            .map(|spec| spec.pins)
                            .unwrap_or("?");
                        format!("connector interface {number} (pins {mode})")
                    }
                    None => "unknown".to_string(),
                }
            };
            println!(
                "{:<16} usb interface {:<2}  {role}",
                port.device, port.usb_interface
            );
        }
    }

    let can = discover::enumerate_can_interfaces(Duration::ZERO);
    if can.is_empty() {
        println!("no gs_usb CAN interfaces found");
    } else {
        for (channel, name) in can.iter().enumerate() {
            let interface = discover::CAN_CHANNEL_INTERFACE
                .get(channel)
                .map(|number| number.to_string())
                .unwrap_or_else(|| "?".to_string());
            println!("{name:<16} gs_usb channel {channel}   connector interface {interface}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_supply;

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
