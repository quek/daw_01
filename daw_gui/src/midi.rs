//! MIDI input bridge.
//!
//! Opens the first available system MIDI input port and forwards
//! NoteOn / NoteOff messages to the GUI as `AppEvent::MidiNoteOn` /
//! `AppEvent::MidiNoteOff`. Used for step-input — every NoteOn drops a
//! note at the current step cursor inside the selected clip.
//!
//! `midir` runs the receive callback on its own thread. The Vizia
//! `ContextProxy` we hand to `MidiInput::connect` already has its
//! emitter sender wrapped in a Send/Sync handle, so cross-thread
//! delivery is safe without extra locking.

use anyhow::{Context, Result};
use midir::{Ignore, MidiInput, MidiInputConnection};
use vizia::prelude::ContextProxy;

use crate::app::AppEvent;

/// Owns the live `MidiInputConnection`. Dropping this struct closes the
/// port — `MidiInputConnection` keeps the OS handle alive for as long
/// as the value lives, so we stash one inside `AppData` to keep the
/// callback thread running.
pub struct MidiInputHandle {
    pub port_name: String,
    _connection: MidiInputConnection<()>,
}

/// Open the first available MIDI input port and route its events to
/// `proxy` as `AppEvent::MidiNoteOn` / `MidiNoteOff`. Returns `None`
/// when no input ports are present (no warning — many systems just
/// have no MIDI hardware connected).
pub fn open_default_input(proxy: ContextProxy) -> Result<Option<MidiInputHandle>> {
    let mut input = MidiInput::new("daw_01")
        .context("failed to create MidiInput")?;
    input.ignore(Ignore::None);
    let ports = input.ports();
    let Some(port) = ports.first() else {
        return Ok(None);
    };
    let port_name = input
        .port_name(port)
        .unwrap_or_else(|_| "(unnamed MIDI input)".into());
    let connection = input
        .connect(
            port,
            "daw_01-input",
            move |_stamp, msg, _| {
                // The closure is invoked on midir's own thread; clone
                // the `ContextProxy` so each callback sees its own
                // mutable handle for `emit`.
                let mut proxy = proxy.clone();
                dispatch(msg, &mut proxy);
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("failed to open MIDI input port: {e}"))?;
    tracing::info!(port_name = %port_name, "opened MIDI input");
    Ok(Some(MidiInputHandle {
        port_name,
        _connection: connection,
    }))
}

fn dispatch(msg: &[u8], proxy: &mut ContextProxy) {
    // Strip the channel nibble: we treat all 16 channels the same.
    let Some(&status) = msg.first() else { return };
    let kind = status & 0xF0;
    match kind {
        0x90 => {
            // NoteOn — but a NoteOn with velocity 0 is the running-status
            // shorthand for NoteOff in the MIDI 1.0 spec.
            let (Some(&pitch), Some(&velocity)) = (msg.get(1), msg.get(2)) else {
                return;
            };
            let event = if velocity == 0 {
                AppEvent::MidiNoteOff { pitch }
            } else {
                AppEvent::MidiNoteOn { pitch, velocity }
            };
            let _ = proxy.emit(event);
        }
        0x80 => {
            let Some(&pitch) = msg.get(1) else { return };
            let _ = proxy.emit(AppEvent::MidiNoteOff { pitch });
        }
        _ => {
            // CC / pitch-bend / aftertouch — ignored for now.
        }
    }
}
