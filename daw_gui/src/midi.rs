//! MIDI input bridge.
//!
//! 起動時に最初に見えた MIDI 入力ポートを開き、NoteOn/NoteOff を
//! `AppEvent::MidiNoteOn` / `MidiNoteOff` として GUI へ流す。
//! step-input 用 — NoteOn が 1 個来るたびに選択中クリップへ note を 1 つ追加する。
//!
//! `midir` のコールバックは別スレッドで動く。`EventLoopProxy<AppEvent>` は
//! `Send + Sync + Clone` なのでそのままクロージャに渡せる。

use anyhow::{Context, Result};
use midir::{Ignore, MidiInput, MidiInputConnection};
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;

pub struct MidiInputHandle {
    pub port_name: String,
    _connection: MidiInputConnection<()>,
}

pub fn open_default_input(proxy: EventLoopProxy<AppEvent>) -> Result<Option<MidiInputHandle>> {
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
                dispatch(msg, &proxy);
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

fn dispatch(msg: &[u8], proxy: &EventLoopProxy<AppEvent>) {
    let Some(&status) = msg.first() else { return };
    let kind = status & 0xF0;
    match kind {
        0x90 => {
            let (Some(&pitch), Some(&velocity)) = (msg.get(1), msg.get(2)) else {
                return;
            };
            let event = if velocity == 0 {
                AppEvent::MidiNoteOff { pitch }
            } else {
                AppEvent::MidiNoteOn { pitch, velocity }
            };
            let _ = proxy.send_event(event);
        }
        0x80 => {
            let Some(&pitch) = msg.get(1) else { return };
            let _ = proxy.send_event(AppEvent::MidiNoteOff { pitch });
        }
        _ => {}
    }
}
