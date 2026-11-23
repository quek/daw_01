//! VOICEVOX の歌唱クリップの声 (r.md #158 で VOICEVOX の Par からクリップの帯へ移した。旧「Clip Voice」)。
//!
//! 歌声トラック (`Track::is_voicevox_vocal`) の MIDI クリップを選んでいるとき、小見出し「歌声」の下に
//! キャラ / スタイルを出す。声はクリップごと (`Clip::speaker_id` が SSoT)。複数選んでいれば、歌声トラックの
//! MIDI クリップ全部へ書く (声が割れていれば dropdown は空)。

use daw_ui_core::Ui;

use common::model::{Clip, ClipContent, ClipKey};

use super::super::band;
use super::super::form::FormCols;
use super::super::vocab;
use super::voice_picker::{VoicePick, draw_voice_pick, same_voice};
use crate::app::{AppData, AppEvent};

/// 小見出し「歌声」+ キャラ / スタイル。`refs` (選んでいるクリップ) に歌声トラックの MIDI クリップが無ければ
/// 何も描かない。
pub(super) fn draw_clip_voice(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, refs: &[ClipKey], y: f32) -> f32 {
    let song = app.cur.song_doc.song();
    let vocal_midi = |k: &ClipKey| -> Option<&Clip> {
        let clip = song.track_by_id(k.track_id).filter(|t| t.is_voicevox_vocal())?.clip_by_id(k.clip_id)?;
        matches!(song.clip_contents.get(&clip.content_id), Some(ClipContent::Midi(_))).then_some(clip)
    };
    let targets: Vec<(ClipKey, &Clip)> = refs.iter().filter_map(|k| vocal_midi(k).map(|cl| (*k, cl))).collect();
    let Some(&(_, first)) = targets.first() else {
        return y;
    };
    let same = targets.iter().all(|(_, cl)| same_voice(cl, first));
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
    let y = band::sub_heading(app, ui, "inspector_clip_voice_head", c.label_x, w, y + 2.0, vocab::SUB_VOICE, None);
    let pick = VoicePick {
        id: "inspector_clip_voice",
        list: singers,
        current: same.then_some((names, first.speaker_id)),
        clips: targets.iter().map(|(k, _)| *k).collect(),
        refetch: || AppEvent::RefetchSingers,
    };
    draw_voice_pick(app, ui, c, y, &pick)
}
