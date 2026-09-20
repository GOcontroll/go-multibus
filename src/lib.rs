//! go-multibus — the GOcontroll Multibus module on a Moduline controller.
//!
//! The module is a plug-in card that turns three of the controller's connector
//! pin pairs into whatever the machine needs — CAN FD, RS485 or RS232 — feeds
//! an external device from its own regulator, and reads battery cell voltages
//! over an isolated SPI link. This crate is everything on the Linux side of it:
//!
//! | module | what it does |
//! |---|---|
//! | [`proto`] | the wire protocol: frame codec and decoded answers |
//! | [`link`] | one open connection to the protocol port over USB |
//! | [`modulebus`] | the SPI link that exists before USB does |
//! | [`discover`] | finding the module's ttys and CAN interfaces |
//! | [`config`] | `/etc/gocontroll/multibus.json` |
//! | [`service`] | the bring-up state machine both the daemon and the UI run |
//! | [`shm`] | the documents published to `/dev/shm` |
//! | [`tui`] | the terminal UI, in the style of go-can and go-modules |
//!
//! Two links reach the same module, each for what it is good at. The module bus
//! (SPI) exists before USB, so it carries the configuration; USB carries the
//! cell data, which is far too large for a 46 byte module bus message.

pub mod config;
pub mod discover;
pub mod error;
pub mod link;
pub mod modulebus;
pub mod proto;
pub mod service;
pub mod shm;
pub mod tui;

pub use error::{Error, Result};

/// Version reported by `--version` and printed in the UI header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
