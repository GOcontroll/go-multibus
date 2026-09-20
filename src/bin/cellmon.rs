//! gocontroll-cellmon — talk to a Multibus module over its protocol port.
//!
//! The module sits in a controller slot, talks isoSPI to the battery over its
//! LTC6820, and hands the result to Linux over USB. This is both the command
//! line tool you reach for while bringing the chain up, and the service that
//! publishes the measurements for other software on the controller.
//!
//! Where `go-multibus` owns the module — resetting it, configuring it, keeping
//! it alive — this talks to one that is already running. That makes it the
//! right tool for looking at a module without changing anything, and the wrong
//! one for bringing a machine up.

use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use go_multibus::config::OUTPUT_PATH;
use go_multibus::discover;
use go_multibus::error::{Error, Result};
use go_multibus::link::MultibusLink;
use go_multibus::proto::{format_volts, Mode, INTERFACES};
use go_multibus::shm;

#[derive(Parser)]
#[command(
    name = "gocontroll-cellmon",
    version,
    about = "Read battery cell voltages from a GOcontroll Multibus module"
)]
struct Cli {
    /// Protocol port to use. Found automatically when left out.
    #[arg(short, long, global = true)]
    device: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the Multibus ports on this controller.
    List,
    /// Identity, role and capabilities.
    Info,
    /// isoSPI link counters.
    Status,
    /// Connector interface configuration.
    Interfaces {
        /// Five modes, for example `can,rs485,can,rs485,isospi`.
        #[arg(long, value_name = "M1,M2,M3,M4,M5")]
        set: Option<String>,
    },
    /// External supply on the connector.
    Supply {
        /// `off`, or a voltage such as `9` or `9V`. Reads it out when left out.
        setting: Option<String>,
    },
    /// Read one cell snapshot.
    Cells {
        /// Print the published document rather than a summary.
        #[arg(long)]
        json: bool,
    },
    /// Keep printing snapshots.
    Watch {
        #[arg(short, long, default_value_t = 1.0)]
        interval: f64,
    },
    /// Set the cell count and the poll interval.
    Config {
        #[arg(long, value_name = "N")]
        cells: u8,
        /// Poll interval in ms, 100..10000.
        #[arg(long, value_name = "MS")]
        poll: u16,
    },
    /// Pin a simulated cell, on a simulator build.
    Sim {
        /// Cell number, starting at 1.
        #[arg(long)]
        cell: u8,
        /// Voltage in mV.
        #[arg(long)]
        mv: f64,
    },
    /// Raw isoSPI transfer, bytes in hex.
    Raw {
        #[arg(value_name = "HEX", required = true)]
        bytes: Vec<String>,
    },
    /// Publish snapshots to /dev/shm.
    Serve {
        #[arg(short, long, default_value = OUTPUT_PATH)]
        output: String,
        #[arg(short, long, default_value_t = 0.5)]
        interval: f64,
        /// Publish once and exit.
        #[arg(long)]
        once: bool,
        /// No progress on stderr.
        #[arg(short, long)]
        quiet: bool,
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
    if matches!(cli.command, Command::List) {
        return list();
    }

    let mut link = MultibusLink::open(cli.device.as_deref())?;

    match cli.command {
        Command::List => unreachable!("handled above"),

        Command::Info => {
            let info = link.info()?;
            println!("device         : {}", link.device());
            println!("protocol       : {}", info.protocol_version);
            println!("role           : {}", info.role_name());
            println!("hardware       : {}", info.hardware_name());
            println!("software       : {}", info.software_name());
            println!("interfaces     : {}", info.interface_count);
            println!("protocol port  : CDC {}", info.protocol_port);
            println!("max cells      : {}", info.max_cells);
            println!("max temps      : {}", info.max_temperatures);
            println!("max transfer   : {} bytes", info.max_transfer);
        }

        Command::Status => {
            let status = link.link_status()?;
            println!("link           : {}", if status.link_ok { "up" } else { "down" });
            println!("poll interval  : {} ms", status.poll_ms);
            println!("tx frames      : {}", status.tx_frames);
            println!("rx frames      : {}", status.rx_frames);
            println!("crc errors     : {}", status.crc_errors);
            println!("spi errors     : {}", status.spi_errors);
            println!("resyncs        : {}", status.resyncs);
            println!("last rx age    : {} ms", status.last_rx_age_ms);
        }

        Command::Interfaces { set } => {
            let config = match set {
                None => link.get_interfaces()?,
                Some(text) => {
                    let modes = parse_modes(&text)?;
                    link.set_interfaces(&modes)?
                }
            };
            for (index, mode) in config.modes.iter().enumerate() {
                let spec = &INTERFACES[index];
                println!(
                    "interface {}  pins {:<7} {}",
                    spec.number,
                    spec.pins,
                    mode.as_str()
                );
            }
        }

        Command::Supply { setting } => {
            let supply = match setting {
                None => link.get_supply()?,
                Some(text) => {
                    let (enabled, millivolts) = parse_supply(&text)?;
                    let current = link.get_supply()?;
                    link.set_supply(enabled, millivolts.unwrap_or(current.millivolts))?
                }
            };
            println!("supply         : {}", supply.summary());
            println!(
                "range          : {} to {} in steps of {}",
                format_volts(supply.min_mv),
                format_volts(supply.max_mv),
                format_volts(supply.step_mv)
            );
            println!(
                "stored         : {}",
                if supply.stored {
                    "yes, it comes back after a reset"
                } else {
                    "no, a reset loses it"
                }
            );
        }

        Command::Cells { json } => {
            let snapshot = link.read_cells()?;
            if json {
                let status = link.link_status()?;
                let supply = link.get_supply().ok();
                let device = link.device().to_string();
                let interfaces = link.get_interfaces()?.modes;
                let document = shm::snapshot_document(
                    &snapshot,
                    &status,
                    &device,
                    &interfaces,
                    supply.as_ref(),
                );
                println!("{}", serde_json::to_string_pretty(&document)?);
            } else {
                print_snapshot(&snapshot);
            }
        }

        Command::Watch { interval } => {
            let period = Duration::from_secs_f64(interval.max(0.05));
            loop {
                let snapshot = link.read_cells()?;
                print_snapshot(&snapshot);
                println!();
                std::thread::sleep(period);
            }
        }

        Command::Config { cells, poll } => {
            link.configure_cells(cells, poll)?;
            println!("cell count {cells}, poll interval {poll} ms");
        }

        Command::Sim { cell, mv } => {
            if cell == 0 {
                return Err(Error::user("cell numbers start at 1"));
            }
            // The wire unit is 100 uV, the native step of the LTC681x family.
            let hundred_uv = (mv * 10.0).round();
            if !(0.0..=65535.0).contains(&hundred_uv) {
                return Err(Error::user(format!("{mv} mV is out of range")));
            }
            link.simulate_cell(cell - 1, hundred_uv as u16)?;
            println!("cell {cell} pinned to {mv} mV");
        }

        Command::Raw { bytes } => {
            let request = parse_hex(&bytes)?;
            let answer = link.isospi_transfer(&request)?;
            println!("tx: {}", hex(&request));
            println!("rx: {}", hex(&answer));
        }

        Command::Serve {
            output,
            interval,
            once,
            quiet,
        } => serve(&mut link, &output, interval, once, quiet)?,
    }

    Ok(())
}

fn list() -> Result<()> {
    let ports = discover::enumerate_ports();
    if ports.is_empty() {
        println!("no Multibus ports found");
        return Ok(());
    }
    for port in ports {
        let role = if port.is_protocol_port {
            "module protocol".to_string()
        } else {
            match port.connector_interface {
                Some(number) => format!("connector interface {number}"),
                None => "unknown".to_string(),
            }
        };
        println!(
            "{:<16} usb interface {:<2}  {role}",
            port.device, port.usb_interface
        );
    }
    Ok(())
}

fn parse_modes(text: &str) -> Result<Vec<Mode>> {
    let modes: Vec<Mode> = text
        .split(',')
        .map(Mode::parse)
        .collect::<Result<Vec<_>>>()?;
    if modes.len() != INTERFACES.len() {
        return Err(Error::user(format!(
            "give {} modes, for example can,rs485,can,rs485,isospi",
            INTERFACES.len()
        )));
    }
    Ok(modes)
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

fn parse_hex(values: &[String]) -> Result<Vec<u8>> {
    // Accept both "03 60" and "0360", because both are what you get when
    // copying a frame out of a log.
    let joined: String = values.join("").replace("0x", "");
    if joined.len() % 2 != 0 {
        return Err(Error::user("hex input must have an even number of digits"));
    }
    (0..joined.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&joined[index..index + 2], 16)
                .map_err(|_| Error::user(format!("{} is not hex", &joined[index..index + 2])))
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_snapshot(snapshot: &go_multibus::proto::CellSnapshot) {
    let mut flags = Vec::new();
    if snapshot.simulated() {
        flags.push("simulated");
    }
    if snapshot.stale() {
        flags.push("stale");
    }
    if snapshot.undervoltage() {
        flags.push("undervoltage");
    }
    if snapshot.overvoltage() {
        flags.push("overvoltage");
    }

    println!(
        "pack {:.3} V   spread {:.4} V   age {} ms   seq {}{}",
        snapshot.pack_v(),
        snapshot.spread_v(),
        snapshot.age_ms,
        snapshot.sequence,
        if flags.is_empty() {
            String::new()
        } else {
            format!("   [{}]", flags.join(" "))
        }
    );

    for (index, volts) in snapshot.cells_v().iter().enumerate() {
        print!("{:2} {:.4}   ", index + 1, volts);
        if (index + 1) % 4 == 0 {
            println!();
        }
    }
    if snapshot.cells_v().len() % 4 != 0 {
        println!();
    }

    let temperatures = snapshot.temperatures_c();
    if !temperatures.is_empty() {
        let text: Vec<String> = temperatures
            .iter()
            .map(|value| format!("{value:.1} C"))
            .collect();
        println!("temperatures: {}", text.join("   "));
    }
}

fn serve(
    link: &mut MultibusLink,
    output: &str,
    interval: f64,
    once: bool,
    quiet: bool,
) -> Result<()> {
    let period = Duration::from_secs_f64(interval.max(0.05));
    let device = link.device().to_string();
    let interfaces = link.get_interfaces()?.modes;
    let supply = link.get_supply().ok();

    loop {
        let started = Instant::now();
        let snapshot = link.read_cells()?;
        let status = link.link_status()?;

        let document =
            shm::snapshot_document(&snapshot, &status, &device, &interfaces, supply.as_ref());
        shm::write_atomic(output, &document)?;

        if !quiet {
            eprintln!(
                "published {} cells, pack {:.3} V",
                snapshot.cells_100uv.len(),
                snapshot.pack_v()
            );
        }
        if once {
            return Ok(());
        }

        let spent = started.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}
