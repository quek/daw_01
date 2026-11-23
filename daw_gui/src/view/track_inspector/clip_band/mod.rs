//! r.md #158 仕様 3: 選択の帯 (クリップ / セル) の中身 (`docs/plan_rmd_158_inspector.md` §3 仕様 3 / §4)。
//!
//! クリップの設定はここ 1 か所に集める (同じ値の編集面は 1 つ): 帯の先頭に全種類で共通の欄 (名前 /
//! テキストクリップは本文、再生)、続いて種類ごとの欄 — 音声 (`audio.rs`)・画像 (`image.rs`)・歌声の声
//! (`voice.rs`)・読み上げ (`talk.rs`)・字幕 (`subtitle.rs`)。セルを選んでいれば末尾にローンチ (`launch.rs`)。
//! 歌声 / 読み上げ / 字幕は以前 VOICEVOX / 字幕デバイスの Par の中にあった (Par からは消した)。
//!
//! contract は他の帯の中身と同じ「`(app, ui, area, pad, 起点 y) -> 次の y`」。

mod audio;
mod image;
mod launch;
mod subtitle;
mod talk;
mod voice;
mod voice_picker;

pub(super) use launch::draw_scene_launch;

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use common::model::ClipKey;

use super::form::{self, FormCols};
use super::layout::FORM_ROW_PITCH;
use super::{toggle_audio_style, vocab};
use crate::app::{AppData, AppEvent, DiscreteClipEdit};
use crate::state::inspector_view::ClipBandKind;

/// 選択の帯の中身: 選んでいるクリップの欄と、セルのローンチ。
pub(super) fn draw_clip_band_body(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, pad: f32, y: f32) -> f32 {
    let w = area.w - pad * 2.0;
    let c = form::cols(area.x + pad, w);
    let y = draw_clip_fields(app, ui, &c, w, y);
    launch::draw_cell_launch(app, ui, area, pad, y)
}

/// 共通の欄と種類ごとの欄。値は代表クリップ (`selected_clip_ref`) から、編集は選んでいるクリップ全部へ
/// (声 / 読み上げ / 本文 / 名前のように 1 つのクリップにしか書けない欄は、1 つだけ選んでいるときに出す)。
fn draw_clip_fields(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, y: f32) -> f32 {
    let song = app.cur.song_doc.song();
    let Some(rep) = app.selected_clip_ref() else {
        return y;
    };
    let Some(kind) = song.clip_by_key(rep).and_then(|cl| song.clip_contents.get(&cl.content_id)).map(ClipBandKind::of)
    else {
        return y;
    };
    let single = app.inspector_target_refs().len() == 1;
    // 表示中の編集バッファ (本文 / フォント) を代表クリップへ向ける。1 フレームだけ古い値が見えるが、
    // 次のフレームで書き戻る。同じクリップを選び直しただけでは走らない。
    if app.cur.peph.clip_edit_buffer_target != Some(rep) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(if kind == ClipBandKind::Text {
                AppEvent::ResyncClipTextEditBuffers(rep)
            } else {
                AppEvent::ResyncClipEditBuffers(rep)
            });
        }));
    }

    let mut y = y;
    if single {
        y = if kind == ClipBandKind::Text { text_body_row(app, ui, c, y) } else { name_row(app, ui, c, rep, y) };
    }
    let audio = (kind == ClipBandKind::Audio).then(|| app.inspector_audio_event_summary()).flatten();
    y = playback_row(app, ui, c, y, audio.as_ref());
    match kind {
        ClipBandKind::Audio => audio.map_or(y, |s| audio::draw_audio_fields(app, ui, c, &s, y)),
        ClipBandKind::Image => app.inspector_image_event_summary().map_or(y, |s| image::draw_image_fields(app, ui, c, &s, y)),
        ClipBandKind::Midi if single => voice::draw_clip_voice(app, ui, c, w, rep, y),
        ClipBandKind::Text => {
            let y = if single { talk::draw_talk(app, ui, c, w, rep, y) } else { y };
            subtitle::draw_subtitle(app, ui, c, w, y)
        }
        // 映像クリップの速度は端の Shift ドラッグが唯一の編集面 (`ClipContent::Video` の doc)、
        // オートメーションクリップは中身をレーンで描く。どちらも共通の欄だけ。
        ClipBandKind::Midi | ClipBandKind::Video | ClipBandKind::Automation => y,
    }
}

/// 「名前」: 確定 (Enter / 欄の外のクリック) で改名する。既存の改名の流れ (`BeginRenameClip` →
/// `RenameClipChanged` → `CommitRenameClip`) を 1 つの Edit で通すので、アレンジのクリップの上の改名欄は
/// 出ない (同じ値の入力欄が 2 つ並ばない)。打っている間の文字は入力欄が持つ。
fn name_row(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, rep: ClipKey, y: f32) -> f32 {
    let song = app.cur.song_doc.song();
    let Some(clip) = song.clip_by_key(rep) else {
        return y;
    };
    form::label(ui, app, "inspector_clip_name_label", vocab::FIELD_NAME, c, y);
    let style = ui.text_input_style();
    // id にクリップを入れる: 入力中に選択が変わっても、別のクリップへ確定しない。
    let resp = ui.text_input_at(
        ("inspector_clip_name", rep.track_id, rep.clip_id),
        c.value(y),
        song.content_name(clip.content_id),
        &style,
        |_| Edit::mutate(|_: &mut AppData| {}),
    );
    if let Some(name) = resp.committed_text.filter(|_| resp.committed || resp.blurred) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::BeginRenameClip(rep));
            app.handle_event(AppEvent::RenameClipChanged(name));
            app.handle_event(AppEvent::CommitRenameClip);
        }));
    }
    y + FORM_ROW_PITCH
}

/// テキストクリップの本文 (読み上げがあるトラックでは「セリフ」、無ければ「文字」)。名前は本文そのものなので
/// 「名前」の欄は出さない (同じ値の編集面は 1 つ)。読み上げと字幕が同じ本文を使う。
fn text_body_row(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, y: f32) -> f32 {
    let talks = app
        .selected_clip_ref()
        .and_then(|r| app.cur.song_doc.song().track_by_id(r.track_id))
        .is_some_and(common::model::Track::is_voicevox_vocal);
    form::label(ui, app, "inspector_text_body_label", if talks { vocab::FIELD_LINE } else { vocab::FIELD_TEXT }, c, y);
    let style = ui.text_input_style();
    let resp = ui.text_input_at(
        "inspector_text_body",
        c.value(y),
        &app.cur.peph.clip_text_content_edit_text,
        &style,
        |s| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::ClipTextContentEditChanged(s))),
    );
    // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
    if resp.committed || resp.blurred {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CommitClipTextContentEdit)));
    }
    y + FORM_ROW_PITCH
}

/// 「再生」: [ミュート] (+ 音声クリップは [逆再生])。ミュートは全種類で `Clip.muted` (SSoT) の 1 本。
/// 値が割れていれば消灯で出し、押すと全部ミュートに揃える。
fn playback_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    y: f32,
    audio: Option<&crate::app::InspectorAudioEventSummary>,
) -> f32 {
    form::label(ui, app, "inspector_clip_playback_label", vocab::FIELD_PLAYBACK, c, y);
    let muted = app.inspector_fold(|a, t| a.clip_at(t).map(|cl| f64::from(u8::from(cl.muted)))) == Some(1.0);
    ui.toggle_button_at(
        "inspector_clip_mute",
        vocab::TOGGLE_MUTE,
        c.value_part(y, 2, 0),
        muted,
        &toggle_audio_style(&app.theme),
        move |_| form::broadcast(DiscreteClipEdit::Muted(!muted)),
    );
    if let Some(summary) = audio {
        audio::reverse_toggle(app, ui, c.value_part(y, 2, 1), summary);
    }
    y + FORM_ROW_PITCH
}
