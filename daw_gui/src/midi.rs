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
use crate::event_sampler::SamplerEvent;

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
    let channel = status & 0x0F;
    match kind {
        0x90 => {
            let (Some(&pitch), Some(&velocity)) = (msg.get(1), msg.get(2)) else {
                return;
            };
            // MIDI Capture (`docs/plan_global_sampler.md` §3.4): 演奏 / 録音 / binding
            // とは独立に、到着時刻 (wall-clock) 付きで常に溜める。コールバック
            // スレッドで時刻を取るのは event loop の遅延を載せないため。
            let _ = proxy.send_event(AppEvent::Sampler(SamplerEvent::MidiCaptured {
                at_ns: crate::state::sampler::wall_clock_ns(),
                channel,
                pitch,
                velocity: (velocity != 0).then_some(velocity),
            }));
            // r.md #87: channel も運ぶ (パッドをノートで撃つ binding が
            // 「ch 10 のノート 36」 のように channel 込みで指すため)。
            let event = if velocity == 0 {
                AppEvent::MidiNoteOff { channel, pitch }
            } else {
                AppEvent::MidiNoteOn { channel, pitch, velocity }
            };
            let _ = proxy.send_event(event);
        }
        0x80 => {
            let Some(&pitch) = msg.get(1) else { return };
            let _ = proxy.send_event(AppEvent::Sampler(SamplerEvent::MidiCaptured {
                at_ns: crate::state::sampler::wall_clock_ns(),
                channel,
                pitch,
                velocity: None,
            }));
            let _ = proxy.send_event(AppEvent::MidiNoteOff { channel, pitch });
        }
        // Phase 7 B1-M Step 1 (2026-05-13): MIDI Control Change (CC)。 status
        // 0xB0..0xBF (= channel 0..15)、 data[0] = controller# (0..127)、
        // data[1] = value (0..127)。 GUI 側で MIDI Learn binding lookup +
        // 該当 BindingTarget へ値送信 (= 段階 1 では dummy で TrackVolume[0]
        // にだけ流す、 段階 2+ で persistable な MidiBinding 経由)。
        0xB0 => {
            let (Some(&controller), Some(&value)) = (msg.get(1), msg.get(2))
            else {
                return;
            };
            // CC 120 / 123 は「全ノート消音」。MIDI Capture の押しっぱなしも閉じる。
            if controller == 120 || controller == 123 {
                let _ = proxy.send_event(AppEvent::Sampler(SamplerEvent::MidiAllNotesOff {
                    at_ns: crate::state::sampler::wall_clock_ns(),
                    channel,
                }));
            }
            let _ = proxy.send_event(AppEvent::MidiControlChange {
                channel,
                controller,
                value,
            });
        }
        _ => {}
    }
}
