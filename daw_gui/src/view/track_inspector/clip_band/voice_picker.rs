//! VOICEVOX の声の 2 段 dropdown (キャラ ▾ → スタイル ▾) と「声の一覧を取り直す」。
//!
//! 歌声 (歌唱クリップ、`voice.rs`) と読み上げの話者 (Text クリップ、`talk.rs`) が同じ 1 本を使う
//! (一覧が歌唱用 / 読み上げ用で違うだけ)。選んだ声は `SetClipVoice` でクリップに焼き込む
//! (`Clip::speaker_id` / `singer_name` / `style_name` が SSoT)。複数のクリップを選んでいれば全部へ書き、
//! 声が割れていれば dropdown は空で出す (他の欄の「—」と同じ — 選べば全部その声に揃う)。

use daw_ui_core::{Edit, Ui};

use super::super::form::{self, FormCols};
use super::super::layout::FORM_ROW_PITCH;
use super::super::vocab;
use crate::app::{AppData, AppEvent, ClipKey};
use crate::voicevox_client::VoiceVoxSinger;

/// 声の欄の中身。
pub(super) struct VoicePick<'a> {
    /// widget id の頭 (歌声 / 読み上げで別)。
    pub id: &'static str,
    /// 選べる声 (キャラ → スタイル)。空 = 一覧をまだ取れていない。
    pub list: &'a [VoiceVoxSinger],
    /// いまの声 (キャラ名 / スタイル名。空なら一覧から `speaker` で引く)。`None` = 選んでいるクリップで
    /// 声が割れている。
    pub current: Option<((&'a str, &'a str), u32)>,
    /// 書き込む先のクリップ (選んでいる同じ種類のクリップ全部、`AppData::inspector_voice_targets`)。選んだとき
    /// だけ解決する (描くたびに宛先の列を作らない。`refetch` と同じ形)。
    pub targets: fn(&AppData) -> Vec<ClipKey>,
    /// 「声の一覧を取り直す」で送る event。
    pub refetch: fn() -> AppEvent,
}

/// キャラ / スタイルの 2 行と取り直すボタン (一覧が空なら今の声と「取得中」の 1 行)。返り値 = 次の行の y。
pub(super) fn draw_voice_pick(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, mut y: f32, v: &VoicePick<'_>) -> f32 {
    let (cur_char, cur_style) = v.current.map_or(("", ""), |cur| current_names(v.list, cur));
    if v.list.is_empty() {
        form::label(ui, app, (v.id, "current_label"), vocab::FIELD_CHARACTER, c, y);
        // 幅 360 の値の欄 (約 200px) に収まる長さ (今の声の名前を切らない)。
        let text = if cur_char.is_empty() {
            "(声の一覧を取得中…)".to_string()
        } else {
            format!("{cur_char} - {cur_style} (取得中…)")
        };
        form::value_text(ui, app, (v.id, "current"), &text, c, y);
        return y + FORM_ROW_PITCH;
    }
    // 上段: キャラ。名前で引き、無ければ声の id を持つキャラ。声が割れていれば選択なし (空表示)。
    let char_labels: Vec<&str> = v.list.iter().map(|s| s.name.as_str()).collect();
    let cur_char_idx = v.current.map(|(_, speaker)| {
        v.list
            .iter()
            .position(|s| s.name == cur_char)
            .or_else(|| v.list.iter().position(|s| s.styles.iter().any(|st| st.id == speaker)))
            .unwrap_or(0)
    });
    form::label(ui, app, (v.id, "char_label"), vocab::FIELD_CHARACTER, c, y);
    let picked_char = ui.dropdown((v.id, "char"), c.value(y), &char_labels, cur_char_idx.unwrap_or(usize::MAX));
    y += FORM_ROW_PITCH;

    // 下段: スタイル (上段で選んだ / 今のキャラのもの)。声が割れていてキャラも決まらないときは空の一覧。
    let singer = picked_char.or(cur_char_idx).and_then(|i| v.list.get(i));
    let style_labels: Vec<&str> = singer.map(|s| s.styles.iter().map(|st| st.name.as_str()).collect()).unwrap_or_default();
    let cur_style_idx = singer
        .zip(v.current)
        .and_then(|(s, (_, speaker))| s.styles.iter().position(|st| st.id == speaker))
        .unwrap_or(if v.current.is_some() { 0 } else { usize::MAX });
    form::label(ui, app, (v.id, "style_label"), vocab::FIELD_STYLE, c, y);
    let picked_style = ui.dropdown((v.id, "style"), c.value(y), &style_labels, cur_style_idx);
    y += FORM_ROW_PITCH;

    // 確定: キャラを変えたらそのキャラの先頭のスタイル、スタイルを変えたらそのスタイル。
    let chosen = match (picked_char, picked_style) {
        (Some(pc), _) => v.list.get(pc).and_then(|s| s.styles.first().map(|st| (st.id, s.name.clone(), st.name.clone()))),
        (None, Some(ps)) => singer.and_then(|s| s.styles.get(ps).map(|st| (st.id, s.name.clone(), st.name.clone()))),
        (None, None) => None,
    };
    if let Some((speaker_id, singer_name, style_name)) = chosen {
        let targets = v.targets;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            let clips = targets(app);
            app.handle_event(AppEvent::SetClipVoice { clips, speaker_id, singer_name, style_name });
        }));
    }

    // 取り直す (新しいキャラを入れたとき)。
    let refetch = v.refetch;
    if ui.button_at_clicked((v.id, "refetch"), vocab::REFETCH_VOICES, c.value(y)) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.handle_event(refetch())));
    }
    y + FORM_ROW_PITCH
}

/// 2 つのクリップが同じ声か (声の id と焼き込んだ名前、`SetClipVoice` が書く 3 値)。
pub(super) fn same_voice(a: &common::model::Clip, b: &common::model::Clip) -> bool {
    a.speaker_id == b.speaker_id && a.singer_name == b.singer_name && a.style_name == b.style_name
}

/// いまの声の表示名: クリップに焼き込んだ名前 → 一覧を声の id で引いた名前 → 空。
fn current_names<'a>(list: &'a [VoiceVoxSinger], (names, speaker): ((&'a str, &'a str), u32)) -> (&'a str, &'a str) {
    if !names.0.is_empty() {
        return names;
    }
    list.iter()
        .find_map(|s| s.styles.iter().find(|st| st.id == speaker).map(|st| (s.name.as_str(), st.name.as_str())))
        .unwrap_or(("", ""))
}
