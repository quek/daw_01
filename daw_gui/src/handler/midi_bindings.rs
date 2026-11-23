//! handler::midi_bindings — MIDI Learn の割り当て一覧 (トランスポートの「Learn」を右クリック) の
//! 表示の言葉と、1 件の削除。
use crate::state::*;

use common::model::{AutomationTarget, BindingTarget, MidiBindInput, MidiBinding, TrackBuiltinParam};

impl AppData {
    /// 割り当て `binding` と一致する最初の 1 件を消す (1 undo step)。割り当てに id は無いので値で指す
    /// (`AppEvent::RemoveMidiBinding` の doc)。一致が無ければ何もしない (undo step も積まない)。
    pub(crate) fn remove_midi_binding(&mut self, binding: &MidiBinding) {
        let binding = binding.clone();
        self.edit_song_checked(move |song| {
            let Some(i) = song.midi_bindings.iter().position(|b| *b == binding) else {
                return false;
            };
            song.midi_bindings.remove(i);
            true
        });
    }

    /// 一覧の 1 行の言葉: 入力 (`CC 74` / `ノート 60`)・チャンネル (`ch 1`..`ch 16` / `any`)・宛先。
    #[must_use]
    pub fn midi_binding_row_label(&self, binding: &MidiBinding) -> String {
        let input = match binding.input {
            MidiBindInput::ControlChange(cc) => format!("CC {cc}"),
            MidiBindInput::Note(n) => format!("ノート {n}"),
        };
        let channel = if binding.channel >= 16 { "any".to_string() } else { format!("ch {}", binding.channel + 1) };
        format!("{input}  {channel}  {}", self.midi_binding_target_label(binding.target))
    }

    /// 宛先の言葉。パラメーターはレーン名と同じ名前 (`automation_target_label` — プラグイン / 内蔵 device は
    /// `"Synth: Cutoff"` の完全修飾名、Tempo / Transpose)、トラックの Volume / Pan はトラック名を前に付ける。
    /// ランチャー操作は `launcher_label`。
    #[must_use]
    pub fn midi_binding_target_label(&self, target: BindingTarget) -> String {
        let (track, param) = match target {
            BindingTarget::TrackVolume(id) => (Some(id), AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume)),
            BindingTarget::TrackPan(id) => (Some(id), AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan)),
            BindingTarget::SongTempo => (None, AutomationTarget::SongTempo),
            BindingTarget::SongTranspose => (None, AutomationTarget::SongTranspose),
            BindingTarget::PluginParam { device_id, param_id, .. } => {
                (None, AutomationTarget::PluginParam { device_id, param_id, legacy_device_index: None })
            }
            BindingTarget::NativeParam { device_id, param } => (None, AutomationTarget::NativeParam { device_id, param }),
            BindingTarget::MasterLimiter(p) => (None, AutomationTarget::MasterLimiter(p)),
            launcher => return launcher.launcher_label().unwrap_or_default().into(),
        };
        let name = self.automation_target_label(&param);
        match track {
            Some(id) => format!("{}: {name}", self.cur.song_doc.song().track_display_name(id)),
            None => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use common::model::{BindingTarget, MidiBindInput, MidiBinding};

    use crate::app::AppEvent;

    fn cc(channel: u8, controller: u8, target: BindingTarget) -> MidiBinding {
        MidiBinding { channel, input: MidiBindInput::ControlChange(controller), legacy_controller: None, target }
    }

    /// 一覧の × は値で指した 1 件だけを消す (位置で指すと、一覧を出してから押すまでに前の割り当てが
    /// 消えたとき隣を消す)。一致が無ければ何もせず undo step も積まない。消した 1 件は 1 回の undo で戻る。
    #[test]
    fn 割り当ては値で指した_1_件だけを消し_undo_で戻る() {
        let mut app = crate::test_support::headless_app();
        let (tempo, transpose) = (cc(16, 20, BindingTarget::SongTempo), cc(0, 21, BindingTarget::SongTranspose));
        let (t, tr) = (tempo.clone(), transpose.clone());
        app.edit_song(move |song| song.midi_bindings = vec![t, tr]);
        let shown = app.cur.song_doc.song().midi_bindings[1].clone();
        // 一覧を出した後に先頭の割り当てが消えても、× は見ていた割り当てを消す。
        app.handle_event(AppEvent::RemoveMidiBinding(tempo.clone()));
        app.handle_event(AppEvent::RemoveMidiBinding(shown));
        assert!(app.cur.song_doc.song().midi_bindings.is_empty());

        let undo_depth = app.cur.song_doc.history_labels().len();
        app.handle_event(AppEvent::RemoveMidiBinding(tempo.clone()));
        assert_eq!(app.cur.song_doc.history_labels().len(), undo_depth, "無い割り当ては undo step を積まない");

        app.handle_event(AppEvent::Undo);
        assert_eq!(app.cur.song_doc.song().midi_bindings, vec![transpose]);
    }

    #[test]
    fn 一覧の行は入力とチャンネルと宛先を言葉で出す() {
        let app = crate::test_support::headless_app();
        let track = app.cur.song_doc.song().tracks.first().map(|t| t.id).expect("既定のトラック");
        let name = app.cur.song_doc.song().track_display_name(track).into_owned();
        let row = app.midi_binding_row_label(&cc(16, 7, BindingTarget::TrackVolume(track)));
        assert_eq!(row, format!("CC 7  any  {name}: Volume"));
        let note = MidiBinding { input: MidiBindInput::Note(36), ..cc(9, 0, BindingTarget::SongTempo) };
        assert_eq!(app.midi_binding_row_label(&note), "ノート 36  ch 10  Tempo");
    }
}
