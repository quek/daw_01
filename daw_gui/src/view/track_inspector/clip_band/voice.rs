//! VOICEVOX の歌唱クリップの声 (r.md #158 で VOICEVOX の Par からクリップの帯へ移した。旧「Clip Voice」)。
//!
//! 歌声トラック (`Track::is_voicevox_vocal`) の MIDI クリップを 1 つ選んでいるときだけ、小見出し「歌声」の
//! 下にキャラ / スタイルを出す。声はクリップごと (`Clip::speaker_id` が SSoT)。

use daw_ui_core::Ui;

use common::model::ClipKey;

use super::super::band;
use super::super::form::FormCols;
use super::super::vocab;
use super::voice_picker::{VoicePick, draw_voice_pick};
use crate::app::{AppData, AppEvent};

/// 小見出し「歌声」+ キャラ / スタイル。歌声トラックの MIDI クリップでなければ何も描かない。
pub(super) fn draw_clip_voice(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, clip: ClipKey, y: f32) -> f32 {
    let song = app.cur.song_doc.song();
    let Some(track) = song.track_by_id(clip.track_id).filter(|t| t.is_voicevox_vocal()) else {
        return y;
    };
    let Some(cl) = track.clip_by_id(clip.clip_id) else {
        return y;
    };
    let singers = &app.voicevox.singers;
    // いまの声: クリップに焼き込んだ名前 → 一覧を声の id で引く (picker) → アプリ既定。
    let known = singers.iter().any(|s| s.styles.iter().any(|st| st.id == cl.speaker_id));
    let current = if !cl.singer_name.is_empty() {
        (cl.singer_name.as_str(), cl.style_name.as_str())
    } else if known {
        ("", "")
    } else {
        (common::voicevox::DEFAULT_SINGER_NAME, common::voicevox::DEFAULT_STYLE_NAME)
    };
    let y = band::sub_heading(app, ui, "inspector_clip_voice_head", c.label_x, w, y + 2.0, vocab::SUB_VOICE, None);
    let pick = VoicePick {
        id: "inspector_clip_voice",
        list: singers,
        current,
        speaker: cl.speaker_id,
        clip,
        refetch: || AppEvent::RefetchSingers,
    };
    draw_voice_pick(app, ui, c, y, &pick)
}
