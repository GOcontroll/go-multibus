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
//!
//! With more than one module present, say which one: `--slot` looks the slot up
//! in the name map go-multibus writes, `--serial` takes the USB serial number
//! that `list` shows, and `--device` names the port outright.

use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use go_multibus::config::OUTPUT_PATH;
use go_multibus::discover;
use go_multibus::error::{Error, Result};
use go_multibus::link::MultibusLink;
use go_multibus::namemap;
use go_multibus::proto::{format_volts, link_test, status, Mode, Role, INTERFACES};
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

    /// The module with this USB serial number, as `list` shows it.
    #[arg(long, global = true, conflicts_with = "device")]
    serial: Option<String>,

    /// The module in this controller slot, looked up in the name map.
    #[arg(long, global = true, conflicts_with_all = ["device", "serial"])]
    slot: Option<u8>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the Multibus modules on this controller and their ports.
    List,
    /// Identity, role and capabilities.
    Info,
    /// USB serial number and chip UID.
    Identity,
    /// Show the isoSPI role, or switch it until the next reset.
    Role {
        /// `master` or `slave`.
        role: Option<String>,
    },
    /// Bounce test patterns off the slave and count how they came back.
    LinkTest {
        /// How many round trips.
        #[arg(long, default_value_t = 100)]
        count: u32,
        /// Shortest payload in bytes, 1..80. Lengths cycle from min to max;
        /// either defaults to the other, both to 16.
        #[arg(long, value_name = "L")]
        min: Option<usize>,
        /// Longest payload in bytes, 1..80.
        #[arg(long, value_name = "L")]
        max: Option<usize>,
    },
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

/// The protocol port the options point at, or `None` to find the only one.
fn chosen_device(cli: &Cli) -> Result<Option<String>> {
    if let Some(device) = &cli.device {
        return Ok(Some(device.clone()));
    }
    if let Some(serial) = &cli.serial {
        return discover::protocol_port_for_serial(serial).map(Some);
    }
    if let Some(slot) = cli.slot {
        return discover::protocol_port_for_slot(slot).map(Some);
    }
    Ok(None)
}

fn run(cli: Cli) -> Result<()> {
    if matches!(cli.command, Command::List) {
        return list();
    }

    // Arguments are checked before anything is opened, so a typo is not
    // reported as a missing module.
    if let Command::LinkTest { count, min, max } = &cli.command {
        link_test_lengths(*count, *min, *max)?;
    }

    let device = chosen_device(&cli)?;
    let mut link = MultibusLink::open(device.as_deref())?;

    match cli.command {
        Command::List => unreachable!("handled above"),

        Command::Identity => {
            let identity = link.identity().map_err(older_than_0_3_0)?;
            println!("device         : {}", link.device());
            println!("usb serial     : {}", identity.serial);
            println!("chip uid       : {}", identity.uid_hex());
            match namemap::slot_for_serial(&identity.serial) {
                Some(slot) => println!("slot           : {slot} (from the name map)"),
                None => println!("slot           : not in the name map"),
            }
        }

        Command::Role { role } => {
            let active = match role {
                None => match link.get_role() {
                    Ok(role) => role,
                    Err(failure) if failure.is_unknown_command() => {
                        let info = link.info()?;
                        println!(
                            "role           : {} (fixed: firmware older than 0.3.0 cannot switch)",
                            info.role_name()
                        );
                        return Ok(());
                    }
                    Err(failure) => return Err(failure),
                },
                Some(text) => {
                    let wanted = Role::parse(&text)?;
                    match link.set_role(wanted) {
                        Ok(active) => active,
                        Err(failure) if failure.is_status(status::BAD_PARAMETER) => {
                            let active = link.get_role()?;
                            return Err(Error::module(format!(
                                "the module refused {wanted}; it runs as {active}"
                            )));
                        }
                        Err(failure) => return Err(older_than_0_3_0(failure)),
                    }
                }
            };
            println!("role           : {active}");
        }

        Command::LinkTest { count, min, max } => link_test_run(&mut link, count, min, max)?,

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

/// Say what an unknown command means for the commands 0.3.0 brought.
fn older_than_0_3_0(failure: Error) -> Error {
    if failure.is_unknown_command() {
        Error::module(format!("{failure}: the module runs firmware older than 0.3.0"))
    } else {
        failure
    }
}

fn list() -> Result<()> {
    let modules = discover::usb_modules();
    if modules.is_empty() {
        println!("no Multibus modules found");
        return Ok(());
    }

    for (index, module) in modules.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let serial = if module.serial.is_empty() {
            "(no serial number)"
        } else {
            module.serial.as_str()
        };
        let slot = match namemap::slot_for_serial(&module.serial) {
            Some(slot) => format!("slot {slot}"),
            None => "slot not in the name map".to_string(),
        };
        println!("serial {serial}   {slot}   usb {}", module.usb_path);

        for port in discover::module_ports(module) {
            let role = if port.is_protocol_port {
                "module protocol".to_string()
            } else {
                match port.connector_interface {
                    Some(number) => format!("connector interface {number}"),
                    None => "unknown".to_string(),
                }
            };
            let link = discover::port_link(&port).unwrap_or_else(|| "-".to_string());
            println!(
                "  {link:<18} {:<14} usb interface {:<2}  {role}",
                port.device, port.usb_interface
            );
        }
    }
    Ok(())
}

/// The payload lengths a link test cycles through. Either bound defaults to
/// the other, and both to 16 bytes.
fn link_test_lengths(count: u32, min: Option<usize>, max: Option<usize>) -> Result<(usize, usize)> {
    const DEFAULT_LENGTH: usize = 16;
    let shortest = min.or(max).unwrap_or(DEFAULT_LENGTH);
    let longest = max.or(min).unwrap_or(DEFAULT_LENGTH);
    if shortest == 0 || longest > link_test::MAX_PAYLOAD || shortest > longest {
        return Err(Error::user(format!(
            "payload lengths must satisfy 1 <= min <= max <= {}",
            link_test::MAX_PAYLOAD
        )));
    }
    if count == 0 {
        return Err(Error::user("count must be at least 1"));
    }
    Ok((shortest, longest))
}

/// Bounce `count` patterns off the slave and report how they came back.
///
/// The bytes count up from a different start every round, and the length
/// cycles from `min` to `max`, so a stuck bit, a dropped byte and a length
/// the link cannot carry each show up as something other than "ok".
fn link_test_run(
    link: &mut MultibusLink,
    count: u32,
    min: Option<usize>,
    max: Option<usize>,
) -> Result<()> {
    let (shortest, longest) = link_test_lengths(count, min, max)?;

    // ok, no answer, crc, mismatch, any other result code
    let mut results = [0u32; 5];
    let mut request_failures = 0u32;
    let mut latencies: Vec<u32> = Vec::new();
    let span = longest - shortest + 1;

    for round in 0..count {
        let length = shortest + round as usize % span;
        let payload: Vec<u8> = (0..length)
            .map(|index| (round as usize + index) as u8)
            .collect();

        let test = match link.link_test(&payload) {
            Ok(test) => test,
            // These do not get better by repeating them.
            Err(failure) if failure.is_status(status::NOT_SUPPORTED) => {
                return Err(Error::module(
                    "this module is the isoSPI slave; run the link test on the master",
                ))
            }
            Err(failure) if failure.is_status(status::LINK_DOWN) => {
                return Err(Error::module(
                    "the isoSPI transceiver is off; set interface 5 to isospi first",
                ))
            }
            Err(failure) if failure.is_unknown_command() => return Err(older_than_0_3_0(failure)),
            Err(_) => {
                request_failures += 1;
                continue;
            }
        };

        // The master compares the echo itself; checking it here as well
        // catches anything between the master and USB.
        let code = if test.ok() && test.echoed != payload {
            link_test::MISMATCH
        } else {
            test.result
        };
        results[usize::from(code.min(4))] += 1;
        if code == link_test::OK {
            latencies.push(test.micros);
        }
    }

    println!("round trips    : {count}, {shortest}..{longest} bytes");
    for code in [
        link_test::OK,
        link_test::NO_ANSWER,
        link_test::CRC,
        link_test::MISMATCH,
    ] {
        println!(
            "{:<15}: {}",
            link_test::result_name(code),
            results[usize::from(code)]
        );
    }
    if results[4] > 0 {
        println!("{:<15}: {}", "other result", results[4]);
    }
    if request_failures > 0 {
        println!("{:<15}: {request_failures}", "no USB answer");
    }

    if let (Some(lowest), Some(highest)) = (latencies.iter().min(), latencies.iter().max()) {
        let mean = latencies.iter().map(|&value| u64::from(value)).sum::<u64>()
            / latencies.len() as u64;
        println!("latency        : min {lowest} us, mean {mean} us, max {highest} us");
    }

    let failed = count - results[0];
    if failed > 0 {
        return Err(Error::module(format!(
            "{failed} of {count} round trips failed"
        )));
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

#[cfg(test)]
mod tests {
    use super::{link_test_lengths, Cli, Command};
    use clap::{CommandFactory, Parser};

    #[test]
    fn link_test_lengths_default_to_each_other() {
        assert_eq!(link_test_lengths(1, None, None).unwrap(), (16, 16));
        assert_eq!(link_test_lengths(1, Some(4), None).unwrap(), (4, 4));
        assert_eq!(link_test_lengths(1, None, Some(80)).unwrap(), (80, 80));
        assert_eq!(link_test_lengths(1, Some(1), Some(80)).unwrap(), (1, 80));
        assert!(link_test_lengths(1, Some(0), Some(8)).is_err());
        assert!(link_test_lengths(1, Some(1), Some(81)).is_err());
        assert!(link_test_lengths(1, Some(9), Some(8)).is_err());
        assert!(link_test_lengths(0, None, None).is_err());
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_module_is_picked_by_slot_or_by_serial_but_not_both() {
        let cli = Cli::parse_from(["gocontroll-cellmon", "--slot", "7", "identity"]);
        assert_eq!(cli.slot, Some(7));
        assert!(matches!(cli.command, Command::Identity));

        let cli = Cli::parse_from(["gocontroll-cellmon", "role", "slave", "--serial", "206435A63245"]);
        assert_eq!(cli.serial.as_deref(), Some("206435A63245"));

        assert!(Cli::try_parse_from([
            "gocontroll-cellmon", "--slot", "7", "--serial", "206435A63245", "info"
        ])
        .is_err());
    }

    #[test]
    fn a_link_test_takes_a_count_and_a_length_range() {
        let cli = Cli::parse_from([
            "gocontroll-cellmon", "link-test", "--count", "50", "--min", "1", "--max", "80",
        ]);
        assert!(matches!(
            cli.command,
            Command::LinkTest { count: 50, min: Some(1), max: Some(80) }
        ));
    }
}
