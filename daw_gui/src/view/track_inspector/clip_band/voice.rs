//! VOICEVOX の歌唱クリップの声 (r.md #158 で VOICEVOX の Par からクリップの帯へ移した。旧「Clip Voice」)。
//!
//! 歌声トラック (`Track::is_voicevox_vocal`) の MIDI クリップを選んでいるとき、小見出し「歌声」の下に
//! キャラ / スタイルを出す。声はクリップごと (`Clip::speaker_id` が SSoT)。複数選んでいれば、歌声トラックの
//! MIDI クリップ全部へ書く (声が割れていれば dropdown は空)。宛先の絞り込みは `AppData::is_voice_target` の 1 本。

use daw_ui_core::Ui;

use super::super::band;
use super::super::vocab;
use super::ClipFields;
use super::voice_picker::{VoicePick, draw_voice_pick, same_voice};
use crate::app::{AppData, AppEvent};
use crate::handler::inspector_selection::VoiceKind;

/// 小見出し「歌声」+ キャラ / スタイル。選んでいるクリップに歌声トラックの MIDI クリップが無ければ何も描かない。
/// 選択を 1 回だけ歩き、宛先の列は作らない (書き込み先は選んだときに解決する)。
pub(super) fn draw_clip_voice(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, y: f32) -> f32 {
    let mut targets = f.sel.items.iter().filter(|c| app.is_voice_target(c.track, c.clip, VoiceKind::Sing));
    let Some(first) = targets.next().map(|c| c.clip) else {
        return y;
    };
    let same = targets.all(|c| same_voice(c.clip, first));
    let singers = &app.voicevox.singers;
    // いまの声: クリップに焼き込んだ名前 → 一覧を声の id で引く (picker) → アプリ既定。
    let known = singers.iter().any(|s| s.styles.iter().any(|st| st.id == first.speaker_id));
    let names = if !first.singer_name.is_empty() {
        (first.singer_name.as_str(), first.style_name.as_str())
    } else if known {
        ("", "")
    } else {
        (common::voicevox::DEFAULT_SINGER_NAME, common::voicevox::DEFAULT_STYLE_NAME)
    };
    let y = band::sub_heading(app, ui, "inspector_clip_voice_head", f.c.label_x, f.w, y + 2.0, vocab::SUB_VOICE, None);
    let pick = VoicePick {
        id: "inspector_clip_voice",
        list: singers,
        current: same.then_some((names, first.speaker_id)),
        targets: |app| app.inspector_voice_targets(VoiceKind::Sing),
        refetch: || AppEvent::RefetchSingers,
    };
    draw_voice_pick(app, ui, &f.c, y, &pick)
}
