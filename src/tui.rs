//! The terminal UI.
//!
//! Deliberately the same shape as `go-can` and `go-modules`, down to the
//! separator, the orange header and the cyan caret, so the three tools read as
//! one set: a list you walk with the arrow keys, Enter to open a picker, Enter
//! again to apply, Esc or Left to go back. Esc or q in the list quits.
//!
//! What it shows is what the service does — both drive
//! [`crate::service::Supervisor`] — with one difference: this supervisor
//! attaches to a running module instead of resetting one, so opening the UI to
//! look at a machine does not interrupt it. `r` asks for a reset explicitly.
//!
//! With more than one module configured there is one supervisor per module,
//! all of them looked after on every idle tick, and Tab moves the screen from
//! one to the next. The header says which module the list is about.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::rc::Rc;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};

use crate::config::{store_module, Config, CONFIG_PATH};
use crate::error::{Error, Result};
use crate::proto::{format_volts, Mode, Role, Supply, INTERFACES};
use crate::service::{State, Supervisor};

const SEP: &str = "  ------------------------------------\r\n";
const ORANGE: Color = Color::Rgb {
    r: 255,
    g: 102,
    b: 0,
};

/// How many log lines are kept, and how many are shown under the list. Enough
/// to hold a whole bring-up sequence, so the reason a module is not reachable
/// is still on screen once the sequence has moved on.
const LOG_KEPT: usize = 40;
const LOG_SHOWN: usize = 6;

/// How long the loop waits for a key before letting the supervisor work. Short
/// enough that the cell view and the state line stay live, long enough that an
/// idle UI costs nothing.
const TICK: Duration = Duration::from_millis(200);

/// The rows of the main list: the five connector interfaces, the isoSPI role
/// right under interface 5 it belongs to, then the supply.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Row {
    Interface(usize),
    Role,
    Supply,
}

const ROWS: [Row; 7] = [
    Row::Interface(0),
    Row::Interface(1),
    Row::Interface(2),
    Row::Interface(3),
    Row::Interface(4),
    Row::Role,
    Row::Supply,
];

/// Where a row sits in the list, for putting the cursor back after a picker.
fn row_index(row: Row) -> usize {
    ROWS.iter().position(|candidate| *candidate == row).unwrap_or(0)
}

enum View {
    List {
        cursor: usize,
    },
    PickMode {
        row: usize,
        cursor: usize,
        error: Option<String>,
    },
    PickRole {
        cursor: usize,
        error: Option<String>,
    },
    PickSupply {
        cursor: usize,
        error: Option<String>,
    },
    Cells,
}

enum Action {
    Update(View),
    Quit,
}

type Log = Rc<RefCell<VecDeque<String>>>;

/// One module on screen: its supervisor and the log lines it has produced.
/// Each module keeps its own log, so what is under the list is always about the
/// module the list is about.
struct Attached {
    supervisor: Supervisor,
    log: Log,
}

/// One choice in the supply picker: off, or a setpoint the module accepts.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum SupplyChoice {
    Off,
    Volts(u16),
}

impl SupplyChoice {
    fn label(self) -> String {
        match self {
            SupplyChoice::Off => "off".to_string(),
            SupplyChoice::Volts(millivolts) => format_volts(millivolts),
        }
    }
}

pub fn run(config_path: &str) -> Result<()> {
    let config = Config::load(config_path)?;

    let mut modules: Vec<Attached> = config
        .setups()
        .into_iter()
        .map(|setup| {
            let log: Log = Rc::new(RefCell::new(VecDeque::new()));
            let sink = {
                let log = Rc::clone(&log);
                Box::new(move |message: &str| {
                    let mut lines = log.borrow_mut();
                    lines.push_back(message.to_string());
                    while lines.len() > LOG_KEPT {
                        lines.pop_front();
                    }
                })
            };
            Attached {
                supervisor: Supervisor::attached(setup, sink),
                log,
            }
        })
        .collect();

    let mut selected = 0usize;
    let mut view = View::List { cursor: 0 };
    let mut stdout = io::stdout();

    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = event_loop(
        &mut stdout,
        &mut modules,
        &mut selected,
        &mut view,
        config_path,
    );

    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();

    result
}

fn event_loop(
    stdout: &mut impl Write,
    modules: &mut [Attached],
    selected: &mut usize,
    view: &mut View,
    config_path: &str,
) -> Result<()> {
    let several = modules.len() > 1;

    loop {
        let shown = &modules[*selected];
        draw(stdout, &shown.supervisor, view, &shown.log, several)?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Tab moves to the next module from the list and from the
                    // cell view; inside a picker it would leave a half-made
                    // choice behind, so it is ignored there.
                    if key.code == KeyCode::Tab
                        && several
                        && matches!(view, View::List { .. } | View::Cells)
                    {
                        *selected = (*selected + 1) % modules.len();
                        continue;
                    }

                    let current = std::mem::replace(view, View::List { cursor: 0 });
                    let supervisor = &mut modules[*selected].supervisor;
                    match handle_key(key.code, current, supervisor, config_path) {
                        Action::Update(next) => *view = next,
                        Action::Quit => return Ok(()),
                    }
                }
                _ => {}
            }
        } else {
            // No key waiting, so let the modules be looked after - all of them,
            // so the ones not on screen keep publishing too. Every step is
            // bounded, which is what keeps the keyboard responsive.
            for module in modules.iter_mut() {
                module.supervisor.step();
            }
        }
    }
}

// ── keys ─────────────────────────────────────────────────────────────────────

fn handle_key(
    key: KeyCode,
    view: View,
    supervisor: &mut Supervisor,
    config_path: &str,
) -> Action {
    match view {
        View::List { mut cursor } => match key {
            KeyCode::Up => {
                cursor = cursor.saturating_sub(1);
                Action::Update(View::List { cursor })
            }
            KeyCode::Down => {
                if cursor + 1 < ROWS.len() {
                    cursor += 1;
                }
                Action::Update(View::List { cursor })
            }
            KeyCode::Enter | KeyCode::Right => Action::Update(open_picker(cursor, supervisor)),
            KeyCode::Char('c') | KeyCode::Char('C') => Action::Update(View::Cells),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                supervisor.restart(true);
                Action::Update(View::List { cursor })
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Action::Quit,
            _ => Action::Update(View::List { cursor }),
        },

        View::PickMode {
            row,
            mut cursor,
            error,
        } => {
            let choices = INTERFACES[row].modes;
            match key {
                KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    Action::Update(View::PickMode { row, cursor, error: None })
                }
                KeyCode::Down => {
                    if cursor + 1 < choices.len() {
                        cursor += 1;
                    }
                    Action::Update(View::PickMode { row, cursor, error: None })
                }
                KeyCode::Enter => match apply_mode(supervisor, config_path, row, choices[cursor]) {
                    Ok(()) => Action::Update(View::List { cursor: row }),
                    Err(failure) => Action::Update(View::PickMode {
                        row,
                        cursor,
                        error: Some(failure.to_string()),
                    }),
                },
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                    Action::Update(View::List { cursor: row })
                }
                _ => Action::Update(View::PickMode { row, cursor, error }),
            }
        }

        View::PickRole { mut cursor, error } => match key {
            KeyCode::Up => {
                cursor = cursor.saturating_sub(1);
                Action::Update(View::PickRole { cursor, error: None })
            }
            KeyCode::Down => {
                if cursor + 1 < Role::ALL.len() {
                    cursor += 1;
                }
                Action::Update(View::PickRole { cursor, error: None })
            }
            KeyCode::Enter => {
                let role = Role::ALL[cursor.min(Role::ALL.len() - 1)];
                match apply_role(supervisor, config_path, role) {
                    Ok(()) => Action::Update(View::List {
                        cursor: row_index(Row::Role),
                    }),
                    Err(failure) => Action::Update(View::PickRole {
                        cursor,
                        error: Some(failure.to_string()),
                    }),
                }
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => Action::Update(View::List {
                cursor: row_index(Row::Role),
            }),
            _ => Action::Update(View::PickRole { cursor, error }),
        },

        View::PickSupply { mut cursor, error } => {
            let choices = supply_choices(supervisor.supply);
            match key {
                KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    Action::Update(View::PickSupply { cursor, error: None })
                }
                KeyCode::Down => {
                    if cursor + 1 < choices.len() {
                        cursor += 1;
                    }
                    Action::Update(View::PickSupply { cursor, error: None })
                }
                KeyCode::Enter => {
                    let choice = choices[cursor.min(choices.len().saturating_sub(1))];
                    match apply_supply(supervisor, config_path, choice) {
                        Ok(()) => Action::Update(View::List {
                            cursor: row_index(Row::Supply),
                        }),
                        Err(failure) => Action::Update(View::PickSupply {
                            cursor,
                            error: Some(failure.to_string()),
                        }),
                    }
                }
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => Action::Update(View::List {
                    cursor: row_index(Row::Supply),
                }),
                _ => Action::Update(View::PickSupply { cursor, error }),
            }
        }

        View::Cells => match key {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('c') | KeyCode::Char('C') => {
                Action::Update(View::List { cursor: 0 })
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
            _ => Action::Update(View::Cells),
        },
    }
}

fn open_picker(cursor: usize, supervisor: &Supervisor) -> View {
    match ROWS[cursor] {
        Row::Interface(index) => {
            let wanted = supervisor
                .config
                .interface_modes()
                .ok()
                .and_then(|modes| modes.get(index).copied());
            let choices = INTERFACES[index].modes;
            let start = wanted
                .and_then(|mode| choices.iter().position(|choice| *choice == mode))
                .unwrap_or(0);
            View::PickMode {
                row: index,
                cursor: start,
                error: None,
            }
        }
        Row::Role => {
            // Start on the configured role; with none configured, on the one
            // the module reports, so Enter alone pins down what it already is.
            let current = supervisor.config.isospi_role.or(supervisor.reported_role);
            let start = current
                .and_then(|role| Role::ALL.iter().position(|choice| *choice == role))
                .unwrap_or(0);
            View::PickRole {
                cursor: start,
                error: None,
            }
        }
        Row::Supply => {
            let choices = supply_choices(supervisor.supply);
            let current = if supervisor.config.supply_enabled {
                SupplyChoice::Volts(supervisor.config.supply_mv)
            } else {
                SupplyChoice::Off
            };
            let start = choices
                .iter()
                .position(|choice| *choice == current)
                .unwrap_or(0);
            View::PickSupply {
                cursor: start,
                error: None,
            }
        }
    }
}

/// The supply picker's entries.
///
/// The range comes from the module when it is reachable, and falls back to the
/// one the firmware has today when it is not — so the picker still works on a
/// controller whose module is out of its slot.
fn supply_choices(reported: Option<Supply>) -> Vec<SupplyChoice> {
    let steps = match reported {
        Some(supply) => supply.steps(),
        None => (5..=15).map(|volts| volts * 1000).collect(),
    };

    let mut choices = vec![SupplyChoice::Off];
    choices.extend(steps.into_iter().map(SupplyChoice::Volts));
    choices
}

/// Store an interface mode and restart the module so it takes effect.
///
/// An interface change means re-doing the transceiver setup and, on the Linux
/// side, a different set of CAN channels and serial ports — which is a
/// bring-up, not a live edit. The supply is the opposite; see below.
fn apply_mode(
    supervisor: &mut Supervisor,
    config_path: &str,
    index: usize,
    mode: Mode,
) -> Result<()> {
    let number = INTERFACES[index].number;
    supervisor.config.set_interface(number, mode)?;
    store_module(config_path, &supervisor.config)?;
    supervisor.restart(true);
    Ok(())
}

/// Store an isoSPI role and switch the module to it straight away.
///
/// No restart, like the supply: the module changes role on the spot. It does
/// not keep the role over a reset, but the file does, and every bring-up
/// pushes it again.
fn apply_role(supervisor: &mut Supervisor, config_path: &str, role: Role) -> Result<()> {
    supervisor.config.isospi_role = Some(role);
    store_module(config_path, &supervisor.config)?;
    supervisor.apply_role(role).map(|_| ())
}

/// Store a supply setting and apply it straight away.
///
/// No restart: the module takes the setpoint immediately and writes it to its
/// own flash, so it is also what comes back after the next reset. When the
/// module is not reachable the setting is still stored — the next bring-up
/// pushes it — and that is reported rather than treated as a failure.
fn apply_supply(
    supervisor: &mut Supervisor,
    config_path: &str,
    choice: SupplyChoice,
) -> Result<()> {
    let (enabled, millivolts) = match choice {
        SupplyChoice::Off => (false, supervisor.config.supply_mv),
        SupplyChoice::Volts(millivolts) => (true, millivolts),
    };

    supervisor.config.supply_enabled = enabled;
    supervisor.config.supply_mv = millivolts;
    store_module(config_path, &supervisor.config)?;

    supervisor.apply_supply(enabled, millivolts).map(|_| ())
}

// ── drawing ──────────────────────────────────────────────────────────────────

fn draw(
    stdout: &mut impl Write,
    supervisor: &Supervisor,
    view: &View,
    log: &Log,
    several: bool,
) -> Result<()> {
    queue!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

    queue!(
        stdout,
        SetForegroundColor(ORANGE),
        Print(format!(
            "  GOcontroll Moduline Multibus tool  v{}\r\n",
            crate::VERSION
        )),
        ResetColor,
    )?;
    queue!(stdout, Print(SEP))?;

    let state_colour = match supervisor.state {
        State::Running => Color::Green,
        State::Failed => Color::Red,
        _ => Color::DarkGrey,
    };
    queue!(
        stdout,
        Print(format!("  Module: slot {}   ", supervisor.config.slot)),
        SetForegroundColor(state_colour),
        Print(supervisor.state.label()),
        ResetColor,
    )?;
    if several {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("   Tab next module"),
            ResetColor,
        )?;
    }
    queue!(stdout, Print("\r\n"))?;
    queue!(stdout, Print(SEP))?;

    match view {
        View::List { cursor } => {
            draw_list(stdout, supervisor, *cursor)?;
            draw_log(stdout, log)?;
        }
        View::PickMode { row, cursor, error } => {
            draw_mode_picker(stdout, supervisor, *row, *cursor, error.as_deref())?
        }
        View::PickRole { cursor, error } => {
            draw_role_picker(stdout, supervisor, *cursor, error.as_deref())?
        }
        View::PickSupply { cursor, error } => {
            draw_supply_picker(stdout, supervisor, *cursor, error.as_deref())?
        }
        View::Cells => draw_cells(stdout, supervisor)?,
    }

    stdout.flush()?;
    Ok(())
}

fn caret(stdout: &mut impl Write, selected: bool) -> Result<()> {
    if selected {
        queue!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("  \u{25ba} "),
            ResetColor
        )?;
    } else {
        queue!(stdout, ResetColor, Print("    "))?;
    }
    Ok(())
}

/// Print a value the way go-can does: plain on the selected row, green when it
/// is set and dark grey when it is not, everywhere else.
fn value(stdout: &mut impl Write, text: &str, selected: bool, active: bool) -> Result<()> {
    let padded = format!("{text:<12}");
    if selected {
        queue!(stdout, ResetColor, Print(padded))?;
    } else if active {
        queue!(
            stdout,
            SetForegroundColor(Color::Green),
            Print(padded),
            ResetColor
        )?;
    } else {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(padded),
            ResetColor
        )?;
    }
    Ok(())
}

fn hint(stdout: &mut impl Write, text: &str) -> Result<()> {
    queue!(
        stdout,
        Print(SEP),
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  {text}\r\n")),
        ResetColor,
    )?;
    Ok(())
}

fn draw_list(stdout: &mut impl Write, supervisor: &Supervisor, cursor: usize) -> Result<()> {
    let wanted = supervisor.config.interface_modes().unwrap_or_default();

    // What the module actually reports, so a refused configuration is visible
    // rather than looking like it was applied.
    let active: Option<&Vec<Mode>> = if supervisor.applied_interfaces.len() == INTERFACES.len() {
        Some(&supervisor.applied_interfaces)
    } else {
        None
    };

    for (index, row) in ROWS.iter().enumerate() {
        let selected = index == cursor;
        caret(stdout, selected)?;

        match row {
            Row::Interface(interface) => {
                let spec = &INTERFACES[*interface];
                let mode = wanted.get(*interface).copied().unwrap_or(Mode::Off);

                queue!(stdout, Print(format!("{:<14}", format!("Interface {}", spec.number))))?;
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print(format!("{:<9}", spec.pins)),
                    ResetColor,
                )?;
                value(stdout, mode.label(), selected, mode != Mode::Off)?;

                if let Some(active) = active {
                    let live = active[*interface];
                    if live != mode {
                        queue!(
                            stdout,
                            SetForegroundColor(Color::Red),
                            Print(format!("module has {}", live.label())),
                            ResetColor,
                        )?;
                    } else if spec.modes.len() == 1 {
                        queue!(
                            stdout,
                            SetForegroundColor(Color::DarkGrey),
                            Print("fixed"),
                            ResetColor
                        )?;
                    }
                } else if spec.modes.len() == 1 {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::DarkGrey),
                        Print("fixed"),
                        ResetColor
                    )?;
                }
            }

            Row::Role => {
                // "default" when a one-module file never set a role: the module
                // then keeps whatever its firmware starts in.
                let configured = supervisor.config.isospi_role;
                let text = configured.map(Role::as_str).unwrap_or("default");

                queue!(stdout, Print(format!("{:<14}", "isoSPI role")))?;
                queue!(stdout, Print(format!("{:<9}", "")))?;
                value(stdout, text, selected, configured.is_some())?;
                draw_role_note(stdout, supervisor)?;
            }

            Row::Supply => {
                let range = match supervisor.supply {
                    Some(supply) => format!(
                        "{}-{}",
                        supply.min_mv / 1000,
                        format_volts(supply.max_mv)
                    ),
                    None => "5-15 V".to_string(),
                };
                let text = if supervisor.config.supply_enabled {
                    format_volts(supervisor.config.supply_mv)
                } else {
                    "off".to_string()
                };

                queue!(stdout, Print(format!("{:<14}", "Supply")))?;
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print(format!("{range:<9}")),
                    ResetColor,
                )?;
                value(stdout, &text, selected, supervisor.config.supply_enabled)?;
                draw_supply_note(stdout, supervisor)?;
            }
        }

        queue!(stdout, ResetColor, Print("\r\n"))?;
    }

    hint(
        stdout,
        "\u{2191}/\u{2193} navigate   Enter select   c cells   r restart   q quit",
    )?;
    Ok(())
}

/// The trailing note on the role row: whether the module agrees, and what it
/// runs as when no role is configured.
fn draw_role_note(stdout: &mut impl Write, supervisor: &Supervisor) -> Result<()> {
    let Some(reported) = supervisor.reported_role else {
        return Ok(());
    };

    match supervisor.config.isospi_role {
        Some(wanted) if wanted != reported => {
            let text = if supervisor.role_fixed {
                format!("module has {reported}, fixed by its firmware")
            } else {
                format!("module has {reported}")
            };
            queue!(
                stdout,
                SetForegroundColor(Color::Red),
                Print(text),
                ResetColor
            )?;
        }
        Some(_) => {}
        None => {
            queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(format!("module runs as {reported}")),
                ResetColor,
            )?;
        }
    }
    Ok(())
}

/// The trailing note on the supply row: whether the module agrees, and whether
/// the setting survives a reset.
fn draw_supply_note(stdout: &mut impl Write, supervisor: &Supervisor) -> Result<()> {
    let Some(supply) = supervisor.supply else {
        // Only worth saying once the module is up: before that, "unknown" is
        // just where the bring-up has got to.
        if supervisor.state == State::Running {
            queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print("not supported by this firmware"),
                ResetColor,
            )?;
        }
        return Ok(());
    };

    let agrees = supply.enabled == supervisor.config.supply_enabled
        && (!supply.enabled || supply.millivolts == supervisor.config.supply_mv);

    if !agrees {
        queue!(
            stdout,
            SetForegroundColor(Color::Red),
            Print(format!("module has {}", supply.summary())),
            ResetColor,
        )?;
    } else if supply.stored {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("kept over a reset"),
            ResetColor,
        )?;
    } else {
        queue!(
            stdout,
            SetForegroundColor(Color::Red),
            Print("not stored in the module"),
            ResetColor,
        )?;
    }
    Ok(())
}

fn draw_log(stdout: &mut impl Write, log: &Log) -> Result<()> {
    let lines = log.borrow();
    if lines.is_empty() {
        return Ok(());
    }

    queue!(stdout, Print("\r\n"))?;
    for line in lines.iter().rev().take(LOG_SHOWN).rev() {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(format!("  {line}\r\n")),
            ResetColor,
        )?;
    }
    Ok(())
}

fn draw_mode_picker(
    stdout: &mut impl Write,
    supervisor: &Supervisor,
    row: usize,
    cursor: usize,
    error: Option<&str>,
) -> Result<()> {
    let spec = &INTERFACES[row];
    let current = supervisor
        .config
        .interface_modes()
        .ok()
        .and_then(|modes| modes.get(row).copied());

    queue!(
        stdout,
        Print(format!(
            "  Interface {} (pins {})  \u{2014}  Select Mode\r\n\r\n",
            spec.number, spec.pins
        ))
    )?;

    for (index, mode) in spec.modes.iter().enumerate() {
        draw_choice(stdout, mode.label(), index == cursor, current == Some(*mode))?;
    }

    hint(
        stdout,
        "\u{2191}/\u{2193} navigate   Enter apply (resets the module)   \u{2190}/Esc back",
    )?;

    if spec.modes.len() == 1 {
        queue!(
            stdout,
            Print("\r\n"),
            SetForegroundColor(Color::DarkGrey),
            Print("  This interface is not configurable.\r\n"),
            ResetColor,
        )?;
    }

    draw_error(stdout, error)
}

fn draw_role_picker(
    stdout: &mut impl Write,
    supervisor: &Supervisor,
    cursor: usize,
    error: Option<&str>,
) -> Result<()> {
    queue!(
        stdout,
        Print("  isoSPI role  \u{2014}  Select Role\r\n\r\n")
    )?;

    for (index, role) in Role::ALL.iter().enumerate() {
        draw_choice(
            stdout,
            role.as_str(),
            index == cursor,
            supervisor.config.isospi_role == Some(*role),
        )?;
    }

    hint(
        stdout,
        "\u{2191}/\u{2193} navigate   Enter apply   \u{2190}/Esc back",
    )?;

    queue!(
        stdout,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("  The master drives the isoSPI link, the slave answers it. The module\r\n"),
        Print("  switches straight away; the controller pushes the role on every bring-up.\r\n"),
        ResetColor,
    )?;

    draw_error(stdout, error)
}

fn draw_supply_picker(
    stdout: &mut impl Write,
    supervisor: &Supervisor,
    cursor: usize,
    error: Option<&str>,
) -> Result<()> {
    let choices = supply_choices(supervisor.supply);
    let current = if supervisor.config.supply_enabled {
        SupplyChoice::Volts(supervisor.config.supply_mv)
    } else {
        SupplyChoice::Off
    };

    queue!(
        stdout,
        Print("  External supply  \u{2014}  Select Voltage\r\n\r\n")
    )?;

    for (index, choice) in choices.iter().enumerate() {
        draw_choice(
            stdout,
            &choice.label(),
            index == cursor,
            *choice == current,
        )?;
    }

    hint(
        stdout,
        "\u{2191}/\u{2193} navigate   Enter apply   \u{2190}/Esc back",
    )?;

    queue!(
        stdout,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("  The module stores this, so it comes back on its own after a reset.\r\n"),
        ResetColor,
    )?;

    draw_error(stdout, error)
}

fn draw_choice(
    stdout: &mut impl Write,
    label: &str,
    selected: bool,
    is_current: bool,
) -> Result<()> {
    if selected {
        queue!(
            stdout,
            SetForegroundColor(Color::Cyan),
            Print("  \u{25ba} "),
            ResetColor,
            Print(label),
        )?;
    } else if is_current {
        queue!(
            stdout,
            ResetColor,
            Print("    "),
            SetForegroundColor(Color::Green),
            Print(label),
            ResetColor,
        )?;
    } else {
        queue!(stdout, ResetColor, Print("    "), Print(label))?;
    }
    queue!(stdout, ResetColor, Print("\r\n"))?;
    Ok(())
}

fn draw_error(stdout: &mut impl Write, error: Option<&str>) -> Result<()> {
    if let Some(message) = error {
        queue!(
            stdout,
            Print("\r\n"),
            SetForegroundColor(Color::Red),
            Print(format!("  {message}\r\n")),
            ResetColor,
        )?;
    }
    Ok(())
}

fn draw_cells(stdout: &mut impl Write, supervisor: &Supervisor) -> Result<()> {
    let Some(snapshot) = supervisor.snapshot.as_ref().filter(|s| !s.cells_100uv.is_empty())
    else {
        queue!(stdout, Print("  Cells  \u{2014}  no data\r\n\r\n"))?;
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  The module has not reported a cell snapshot yet.\r\n"),
            ResetColor,
        )?;
        return hint(stdout, "\u{2190}/Esc back   q quit");
    };

    let mut flags = Vec::new();
    if snapshot.simulated() {
        flags.push("SIMULATED");
    }
    if snapshot.stale() {
        flags.push("STALE");
    }
    if snapshot.undervoltage() {
        flags.push("UNDERVOLTAGE");
    }
    if snapshot.overvoltage() {
        flags.push("OVERVOLTAGE");
    }

    queue!(
        stdout,
        Print(format!(
            "  Cells  \u{2014}  pack {:.3} V   spread {:.4} V   ",
            snapshot.pack_v(),
            snapshot.spread_v()
        )),
    )?;
    if !flags.is_empty() {
        queue!(
            stdout,
            SetForegroundColor(Color::Red),
            Print(flags.join(" ")),
            ResetColor,
        )?;
    }
    queue!(stdout, Print("\r\n\r\n"))?;

    let voltages = snapshot.cells_v();
    let lowest = index_of_extreme(&voltages, true);
    let highest = index_of_extreme(&voltages, false);

    // Four to a row keeps a 24 cell pack on one screen.
    for chunk in (0..voltages.len()).collect::<Vec<_>>().chunks(4) {
        queue!(stdout, Print("  "))?;
        for &index in chunk {
            let mark = if Some(index) == lowest {
                "v"
            } else if Some(index) == highest {
                "^"
            } else {
                " "
            };
            let text = format!("{:2} {:.4}{}   ", index + 1, voltages[index], mark);
            if Some(index) == lowest || Some(index) == highest {
                queue!(
                    stdout,
                    SetForegroundColor(Color::Cyan),
                    Print(text),
                    ResetColor
                )?;
            } else {
                queue!(stdout, Print(text))?;
            }
        }
        queue!(stdout, Print("\r\n"))?;
    }

    let temperatures = snapshot.temperatures_c();
    if !temperatures.is_empty() {
        let text: Vec<String> = temperatures
            .iter()
            .map(|value| format!("{value:.1} C"))
            .collect();
        queue!(
            stdout,
            Print("\r\n"),
            Print(format!("  Temperatures: {}\r\n", text.join("   ")))
        )?;
    }

    hint(stdout, "\u{2190}/Esc back   q quit")
}

fn index_of_extreme(values: &[f64], lowest: bool) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .reduce(|best, current| {
            let take = if lowest {
                current.1 < best.1
            } else {
                current.1 > best.1
            };
            if take {
                current
            } else {
                best
            }
        })
        .map(|(index, _)| index)
}

/// Whether the daemon is running and therefore already owns the protocol port.
///
/// Nothing stops two processes opening the same tty, so without this check the
/// UI and the service would both write requests into one port and read each
/// other's answers - which looks like a flaky module rather than like a
/// conflict. No systemd, or a systemd that does not answer, counts as free.
fn service_is_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "go-multibus"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether a terminal is attached on both ends.
///
/// Without one, crossterm fails deep inside its setup with an error that says
/// nothing about the cause, so this is checked first and answered plainly.
pub fn have_terminal() -> bool {
    // SAFETY: isatty only reads the descriptor's type.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// The UI, or a clear refusal when this is not a terminal.
pub fn run_checked(config_path: Option<&str>) -> Result<()> {
    if !have_terminal() {
        return Err(Error::user(
            "go-multibus with no arguments starts the interactive UI, which needs a \
             terminal. Use `go-multibus status` instead.",
        ));
    }
    if service_is_active() {
        return Err(Error::user(
            "the go-multibus service is running and already holds the protocol port.\n\
             Stop it to use the UI:   systemctl stop go-multibus\n\
             Or read what it reports: go-multibus status",
        ));
    }
    run(config_path.unwrap_or(CONFIG_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supply_picker_offers_off_plus_every_step() {
        let supply = Supply {
            enabled: true,
            millivolts: 9000,
            min_mv: 5000,
            max_mv: 15000,
            step_mv: 1000,
            stored: true,
        };
        let choices = supply_choices(Some(supply));
        assert_eq!(choices.len(), 12);
        assert_eq!(choices[0], SupplyChoice::Off);
        assert_eq!(choices[1], SupplyChoice::Volts(5000));
        assert_eq!(choices[11], SupplyChoice::Volts(15000));
        assert_eq!(choices[3].label(), "7 V");
    }

    #[test]
    fn without_a_module_the_picker_still_covers_the_documented_range() {
        let choices = supply_choices(None);
        assert_eq!(choices.first(), Some(&SupplyChoice::Off));
        assert_eq!(choices.last(), Some(&SupplyChoice::Volts(15000)));
    }

    #[test]
    fn the_role_row_sits_between_interface_5_and_the_supply() {
        assert_eq!(row_index(Row::Interface(4)) + 1, row_index(Row::Role));
        assert_eq!(row_index(Row::Role) + 1, row_index(Row::Supply));
        assert_eq!(row_index(Row::Supply), ROWS.len() - 1);
    }

    #[test]
    fn the_extremes_of_a_cell_list_are_found() {
        let values = [3.6, 3.4, 3.9, 3.7];
        assert_eq!(index_of_extreme(&values, true), Some(1));
        assert_eq!(index_of_extreme(&values, false), Some(2));
        assert_eq!(index_of_extreme(&[], true), None);
    }
}
