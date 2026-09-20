//! The documents the service publishes to `/dev/shm`.
//!
//! Both are replaced atomically, so a reader either sees the previous document
//! or the new one and never half of either. Anything on the controller may read
//! them — Node-RED, go-simulink, a shell script — so the shapes here are a
//! contract; `docs/gocontroll-cellmon.md` describes them field by field.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::error::Result;
use crate::proto::{CellSnapshot, LinkStatus, Mode, Supply};

/// Seconds since the epoch, as a float, matching what the readers already parse.
pub fn timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or(0.0)
}

/// Replace `path` in one step so a reader never sees a partial document.
pub fn write_atomic(path: &str, document: &Value) -> Result<()> {
    if let Some(directory) = Path::new(path).parent() {
        fs::create_dir_all(directory)?;
    }

    let temporary = format!("{path}.tmp");
    {
        let mut file = fs::File::create(&temporary)?;
        serde_json::to_writer(&mut file, document)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn round(value: f64, places: u32) -> f64 {
    let scale = 10f64.powi(places as i32);
    (value * scale).round() / scale
}

fn mode_names(modes: &[Mode]) -> Vec<&'static str> {
    modes.iter().map(|mode| mode.as_str()).collect()
}

/// The cell document.
///
/// Voltages appear twice on purpose: `cells_v` for anything that displays them,
/// `cells_100uv` for anything that wants the raw device values back without a
/// rounding step in between.
pub fn snapshot_document(
    snapshot: &CellSnapshot,
    status: &LinkStatus,
    device: &str,
    interfaces: &[Mode],
    supply: Option<&Supply>,
) -> Value {
    let voltages = snapshot.cells_v();
    let lowest = voltages.iter().cloned().fold(f64::INFINITY, f64::min);
    let highest = voltages.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    json!({
        "schema": 1,
        "timestamp": timestamp(),
        "source": {
            "device": device,
            "interfaces": mode_names(interfaces),
        },
        "valid": snapshot.link_ok() && !snapshot.stale(),
        "link_ok": snapshot.link_ok(),
        "stale": snapshot.stale(),
        "simulated": snapshot.simulated(),
        "sequence": snapshot.sequence,
        "age_ms": snapshot.age_ms,
        "cell_count": voltages.len(),
        "cells_v": voltages.iter().map(|value| round(*value, 4)).collect::<Vec<_>>(),
        "cells_100uv": snapshot.cells_100uv,
        "pack_v": round(snapshot.pack_v(), 3),
        "min_v": if voltages.is_empty() { Value::Null } else { json!(round(lowest, 4)) },
        "max_v": if voltages.is_empty() { Value::Null } else { json!(round(highest, 4)) },
        "spread_v": round(snapshot.spread_v(), 4),
        "temperatures_c": snapshot.temperatures_c().iter()
            .map(|value| round(*value, 1)).collect::<Vec<_>>(),
        "alarms": {
            "undervoltage": snapshot.undervoltage(),
            "overvoltage": snapshot.overvoltage(),
        },
        "link": {
            "poll_ms": status.poll_ms,
            "tx_frames": status.tx_frames,
            "rx_frames": status.rx_frames,
            "crc_errors": status.crc_errors,
            "spi_errors": status.spi_errors,
            "resyncs": status.resyncs,
            "last_rx_age_ms": status.last_rx_age_ms,
        },
        "supply": supply.map(supply_document).unwrap_or(Value::Null),
    })
}

/// The external supply, as published and as `status --json` prints it.
pub fn supply_document(supply: &Supply) -> Value {
    json!({
        "enabled": supply.enabled,
        "volts": round(supply.volts(), 3),
        "millivolts": supply.millivolts,
        "min_mv": supply.min_mv,
        "max_mv": supply.max_mv,
        "step_mv": supply.step_mv,
        "stored": supply.stored,
    })
}

/// The document published when a poll failed.
///
/// A reader that only ever sees a stale file cannot tell a dead service from a
/// dead battery link, so a failure is published rather than left out.
pub fn failure_document(state: &str, failures: u32, last_error: &str) -> Value {
    json!({
        "schema": 1,
        "timestamp": timestamp(),
        "valid": false,
        "link_ok": false,
        "state": state,
        "errors": {
            "read_failures": failures,
            "last_error": last_error,
        },
    })
}

/// A small document describing where the bring-up sequence stands.
pub fn state_document(
    state: &str,
    slot: u8,
    module_running: bool,
    interfaces: &[Mode],
    supply: Option<&Supply>,
    failures: u32,
    last_error: &str,
) -> Value {
    json!({
        "schema": 1,
        "timestamp": timestamp(),
        "state": state,
        "slot": slot,
        "module_running": module_running,
        "interfaces": mode_names(interfaces),
        "supply": supply.map(supply_document).unwrap_or(Value::Null),
        "failures": failures,
        "last_error": last_error,
    })
}
