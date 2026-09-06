//! handler::virtual_keyboard — r.md #113 PC キーボードによる仮想鍵盤。
//!
//! PC キー → ピッチの翻訳をして **カーソルトラック** (= 選択中のトラック、 録音待機 R は
//! 不要) で鳴らす。 発音 / 録音 / step 入力の各部品は MIDI 入力 (`handler/midi.rs`) と
//! 共有し、 宛先の決め方だけが違う: MIDI 入力は「録音待機トラック全部」、 仮想鍵盤は
//! 「カーソルトラック 1 本」。 録音はそのトラックが録音待機で録音実体が走っているときだけ
//! 書き込む (`docs/plan_virtual_keyboard.md`)。

use daw_ui_core::GrabbedKey;

use crate::event_sampler::SamplerEvent;
use crate::event_virtual_keyboard::VirtualKeyboardEvent;
use crate::state::sampler::wall_clock_ns;
use crate::state::*;
use crate::virtual_keyboard::{
    OCTAVE_DOWN_KEY, OCTAVE_UP_KEY, semitone_for, shift_base_pitch, step_velocity,
};

/// MIDI Capture に溜めるときに名乗るチャンネル (0 始まり = ch 1)。
const CAPTURE_CHANNEL: u8 = 0;

impl AppData {
    /// 仮想鍵盤の宛先 = カーソルトラック (選択中のトラック)。 `None` = 選択なし。
    pub fn virtual_keyboard_target_track(&self) -> Option<u32> {
        let id = self.cursor_track_id()?;
        self.song_doc.song().track_by_id(id).map(|t| t.id)
    }

    /// カーソルトラックで発音し、 録音実体が走っていてそのトラックが録音待機なら
    /// 書き込む。 どちらでもなければ step 入力 (MIDI 入力と同じ順序)。
    fn virtual_keyboard_note_on(&mut self, pitch: u8, velocity: u8) {
        // MIDI Capture (`docs/plan_global_sampler.md` §3.4): MIDI デバイスと同じく、
        // 演奏 / 録音とは独立に到着時刻付きで常に溜める (宛先が無くても)。
        self.handle_sampler_event(SamplerEvent::MidiCaptured {
            at_ns: wall_clock_ns(),
            channel: CAPTURE_CHANNEL,
            pitch,
            velocity: Some(velocity),
        });
        let Some(track_id) = self.virtual_keyboard_target_track() else {
            return;
        };
        self.monitor_note_on_track(track_id, pitch, velocity);
        if self.recording.live {
            let armed = self.song_doc.song().track_by_id(track_id).is_some_and(|t| t.armed);
            if armed {
                self.record_midi_note_on_tracks(&[track_id], pitch, velocity);
            }
        } else if !self.recording.requested {
            self.step_input_note_on(pitch, velocity);
        }
    }

    /// 消音は「鳴らした台帳」 / 「書き込んだ台帳」 を引くので、 押した後にカーソルを
    /// 移しても正しいトラックで止まる。
    fn virtual_keyboard_note_off(&mut self, pitch: u8) {
        self.handle_sampler_event(SamplerEvent::MidiCaptured {
            at_ns: wall_clock_ns(),
            channel: CAPTURE_CHANNEL,
            pitch,
            velocity: None,
        });
        self.monitor_note_off(pitch);
        if self.recording.live {
            self.record_midi_note_off(pitch);
        }
    }

    /// [`AppEvent::VirtualKeyboard`](crate::app::AppEvent::VirtualKeyboard) の入口。
    pub fn handle_virtual_keyboard_event(&mut self, ev: VirtualKeyboardEvent) {
        use VirtualKeyboardEvent as E;
        match ev {
            E::Toggle => {
                if self.virtual_keyboard.open {
                    self.virtual_keyboard_release_all();
                }
                self.virtual_keyboard.open = !self.virtual_keyboard.open;
            }
            E::Key(key) => self.virtual_keyboard_key(key),
            E::ShiftOctave(delta) => {
                self.ui_prefs.virtual_keyboard_base_pitch =
                    shift_base_pitch(self.ui_prefs.virtual_keyboard_base_pitch, delta);
                self.persist_app_config();
            }
            E::SetVelocity { velocity, commit } => {
                self.ui_prefs.virtual_keyboard_velocity = velocity.clamp(1, 127);
                if commit {
                    self.persist_app_config();
                }
            }
            E::MousePitch(next) => {
                let prev = self.virtual_keyboard.mouse_pitch;
                if prev == next {
                    return;
                }
                if let Some(p) = prev {
                    self.virtual_keyboard_note_off(p);
                }
                if let Some(p) = next {
                    self.virtual_keyboard_note_on(p, self.ui_prefs.virtual_keyboard_velocity);
                }
                self.virtual_keyboard.mouse_pitch = next;
            }
            E::ReleaseAll => self.virtual_keyboard_release_all(),
            E::SetRect(rect) => {
                self.ui_prefs.virtual_keyboard_rect = Some(rect);
                self.persist_app_config();
            }
        }
    }

    /// key grab から届いた 1 キー。 押した瞬間のピッチを `held` に控え、 離すときは
    /// そこから引く (押している間にオクターブを変えても stuck note にならない)。
    /// OS の auto-repeat と、 既に押している鍵の二重 press は無視する。
    fn virtual_keyboard_key(&mut self, key: GrabbedKey) {
        if key.key == OCTAVE_DOWN_KEY || key.key == OCTAVE_UP_KEY {
            if !key.pressed || key.repeat {
                return;
            }
            let delta: i8 = if key.key == OCTAVE_UP_KEY { 1 } else { -1 };
            let ev = if key.shift {
                VirtualKeyboardEvent::SetVelocity {
                    velocity: step_velocity(self.ui_prefs.virtual_keyboard_velocity, delta),
                    commit: true,
                }
            } else {
                VirtualKeyboardEvent::ShiftOctave(delta)
            };
            self.handle_virtual_keyboard_event(ev);
            return;
        }
        let Some(semitone) = semitone_for(key.key) else {
            return;
        };
        if key.pressed {
            if key.repeat || self.virtual_keyboard.held.iter().any(|(k, _)| *k == key.key) {
                return;
            }
            let pitch = self
                .ui_prefs
                .virtual_keyboard_base_pitch
                .saturating_add(semitone)
                .min(127);
            self.virtual_keyboard.held.push((key.key, pitch));
            self.virtual_keyboard_note_on(pitch, self.ui_prefs.virtual_keyboard_velocity);
        } else if let Some(i) = self.virtual_keyboard.held.iter().position(|(k, _)| *k == key.key)
        {
            let (_, pitch) = self.virtual_keyboard.held.remove(i);
            self.virtual_keyboard_note_off(pitch);
        }
    }

    /// 押している音 (PC キー + マウス) を全部止める。 窓を閉じる / 窓が非アクティブに
    /// なる (Alt+Tab、 winit は synthetic な release しか送らず runner がそれを捨てる) とき。
    pub(crate) fn virtual_keyboard_release_all(&mut self) {
        for (_, pitch) in std::mem::take(&mut self.virtual_keyboard.held) {
            self.virtual_keyboard_note_off(pitch);
        }
        if let Some(pitch) = self.virtual_keyboard.mouse_pitch.take() {
            self.virtual_keyboard_note_off(pitch);
        }
    }
}
