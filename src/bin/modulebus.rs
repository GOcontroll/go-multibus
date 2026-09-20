//! gocontroll-modulebus — talk to a module over the controller's module bus (SPI).
//!
//! Useful while bringing a module up, for the things USB cannot answer:
//!
//!   probe       replay the handshake go-modules uses to identify a module and
//!               print the raw reply, which says whether the module answers at
//!               all and whether the byte alignment is right.
//!   start       tell the bootloader its firmware is current so it hands over
//!               to the application. `go-modules scan` does not do this, and
//!               `go-modules update` bails out before sending it when no
//!               firmware file matches, so without this a freshly SWD-flashed
//!               module just sits in the bootloader.
//!   interfaces  read, or write, the interface configuration of a running
//!               application — the route that lets the controller configure the
//!               module before USB comes up.
//!   supply      the same for the external supply on the connector.

use clap::{Parser, Subcommand};

use go_multibus::error::{Error, Result};
use go_multibus::modulebus::{self, ModuleBus, MESSAGE_LENGTH};
use go_multibus::proto::{format_volts, Mode, INTERFACES};

#[derive(Parser)]
#[command(
    name = "gocontroll-modulebus",
    version,
    about = "Talk to a GOcontroll module over the module bus (SPI)"
)]
struct Cli {
    /// Controller slot, 1..8.
    slot: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reset the module and print the bootloader's identify reply.
    Probe,
    /// Hand over from the bootloader to the application.
    Start,
    /// Read, or write, the connector interface configuration.
    Interfaces {
        /// Five modes, for example `can,rs485,can,rs485,isospi`.
        modes: Option<String>,
    },
    /// Read, or write, the external supply.
    Supply {
        /// `off`, or a voltage such as `9` or `9V`.
        setting: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(match run(cli) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("error: {failure}");
            failure.exit_code()
        }
    });
}

fn run(cli: Cli) -> Result<i32> {
    let bus = ModuleBus::open(cli.slot)?;

    match cli.command {
        Command::Probe => probe(&bus),
        Command::Start => start(&bus),
        Command::Interfaces { modes } => interfaces(&bus, modes.as_deref()),
        Command::Supply { setting } => supply(&bus, setting.as_deref()),
    }
}

fn probe(bus: &ModuleBus) -> Result<i32> {
    modulebus::pulse_reset(bus.slot())?;
    let reply = bus.transfer(&modulebus::build_message(modulebus::msg::IDENTIFY, &[]))?;

    println!("rx: {}", hex(&reply));

    let calculated = modulebus::checksum(&reply, MESSAGE_LENGTH - 1);
    let carried = reply[MESSAGE_LENGTH - 1];
    println!(
        "checksum: calculated {calculated:#04x}, carried {carried:#04x} -> {}",
        if calculated == carried { "OK" } else { "MISMATCH" }
    );

    if calculated == carried
        && (reply[0] == modulebus::msg::IDENTIFY || reply[2] == modulebus::msg::IDENTIFY)
    {
        println!(
            "hardware {}-{}-{}-{}, software {}.{}.{}",
            reply[6], reply[7], reply[8], reply[9], reply[10], reply[11], reply[12]
        );
        return Ok(0);
    }

    if reply.iter().all(|&byte| byte == 0) {
        println!(
            "reply is all zeroes: MISO was never driven. Either the module is running \
             an application that does not serve the module bus, or it is not running \
             at all."
        );
    } else {
        println!(
            "reply is malformed. A reply that looks right but sits one byte late means \
             the module clocked out a stale byte before its message."
        );
    }
    Ok(1)
}

fn start(bus: &ModuleBus) -> Result<i32> {
    modulebus::pulse_reset(bus.slot())?;

    if bus.identify()?.is_none() {
        println!(
            "slot {}: no bootloader reply, cannot start the application",
            bus.slot()
        );
        return Ok(1);
    }

    bus.start_application()?;
    println!(
        "slot {}: told the bootloader to start the application",
        bus.slot()
    );
    Ok(0)
}

fn interfaces(bus: &ModuleBus, modes: Option<&str>) -> Result<i32> {
    let active = match modes {
        None => match bus.get_interfaces()? {
            Some(active) => active,
            None => {
                println!(
                    "slot {}: no answer. Is the application running?",
                    bus.slot()
                );
                return Ok(1);
            }
        },
        Some(text) => {
            let wanted: Vec<Mode> = text
                .split(',')
                .map(Mode::parse)
                .collect::<Result<Vec<_>>>()?;
            if wanted.len() != INTERFACES.len() {
                return Err(Error::user(format!(
                    "give {} modes, for example can,rs485,can,rs485,isospi",
                    INTERFACES.len()
                )));
            }

            match bus.set_interfaces(&wanted)? {
                None => {
                    println!(
                        "slot {}: no answer. Is the application running?",
                        bus.slot()
                    );
                    return Ok(1);
                }
                Some((false, active)) => {
                    let kept: Vec<&str> = active.iter().map(|mode| mode.as_str()).collect();
                    println!(
                        "the module refused that combination; it kept: {}",
                        kept.join(",")
                    );
                    return Ok(1);
                }
                Some((true, active)) => active,
            }
        }
    };

    for (index, mode) in active.iter().enumerate() {
        let spec = &INTERFACES[index];
        println!(
            "interface {}  pins {:<7} {}",
            spec.number,
            spec.pins,
            mode.as_str()
        );
    }
    Ok(0)
}

fn supply(bus: &ModuleBus, setting: Option<&str>) -> Result<i32> {
    let active = match setting {
        None => match bus.get_supply()? {
            Some(active) => active,
            None => {
                println!(
                    "slot {}: no answer. Either the application is not running, or its \
                     firmware predates the supply commands.",
                    bus.slot()
                );
                return Ok(1);
            }
        },
        Some(text) => {
            let (enabled, millivolts) = parse_supply(text)?;
            // The module needs a setpoint even when switching off, so keep the
            // one it already has rather than inventing one.
            let keep = bus.get_supply()?.map(|supply| supply.millivolts);
            let wanted = millivolts.or(keep).unwrap_or(5000);

            match bus.set_supply(enabled, wanted)? {
                None => {
                    println!(
                        "slot {}: no answer. Either the application is not running, or \
                         its firmware predates the supply commands.",
                        bus.slot()
                    );
                    return Ok(1);
                }
                Some((false, active)) => {
                    println!(
                        "the module refused {}; it kept {}",
                        format_volts(wanted),
                        active.summary()
                    );
                    return Ok(1);
                }
                Some((true, active)) => active,
            }
        }
    };

    println!("supply   : {}", active.summary());
    println!(
        "range    : {} to {} in steps of {}",
        format_volts(active.min_mv),
        format_volts(active.max_mv),
        format_volts(active.step_mv)
    );
    println!(
        "stored   : {}",
        if active.stored {
            "yes, it comes back after a reset"
        } else {
            "no, a reset loses it"
        }
    );
    Ok(0)
}

/// `off`, `9`, `9V` or `9000mv`.
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

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
