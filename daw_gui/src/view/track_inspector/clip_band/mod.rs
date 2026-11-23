//! r.md #158 仕様 3: 選択の帯 (クリップ / セル) の中身 (`docs/plan_rmd_158_inspector.md` §3 仕様 3 / §4)。
//!
//! クリップの設定はここ 1 か所に集める (同じ値の編集面は 1 つ): 帯の先頭に全種類で共通の欄 (名前 /
//! テキストクリップは本文、再生)、続いて種類ごとの欄 — 音声 (`audio.rs`)・画像 (`image.rs`)・歌声の声
//! (`voice.rs`)・読み上げ (`talk.rs`)・字幕 (`subtitle.rs`)。セルを選んでいれば末尾にローンチ (`launch.rs`)。
//! 歌声 / 読み上げ / 字幕は以前 VOICEVOX / 字幕デバイスの Par の中にあった (Par からは消した)。
//!
//! contract は他の帯の中身と同じ「`(app, ui, area, pad, 起点 y) -> 次の y`」。値は描き始めに 1 回だけ作った
//! 選択 ([`InspectorSelection`]) から読む (選択を引き直さない)。

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

use super::form::{self, FormCols};
use super::layout::FORM_ROW_PITCH;
use super::{band, toggle_audio_style, vocab};
use crate::app::{AppData, AppEvent, DiscreteClipEdit};
use crate::handler::inspector_selection::{ClipBand, InspectorSelection};
use crate::handler::selection_view::LiveClip;
use crate::state::inspector_view::ClipBandKind;

/// クリップの帯の欄を描く文脈: このフレームの選択と、その代表クリップ・欄の列。
pub(super) struct ClipFields<'a, 's> {
    pub sel: &'a InspectorSelection<'s>,
    /// 代表クリップ (種類ごとの欄の値の出どころ、変調の持ち主のトラック)。
    pub rep: LiveClip<'s>,
    pub c: FormCols,
    /// 帯の中の幅 (小見出しの罫線)。
    pub w: f32,
}

impl ClipFields<'_, '_> {
    /// 変調 (◉ / 深さドラッグ) の持ち主 = 代表クリップの居るトラック (値の broadcast 先・「A」のレーン・
    /// `*_automated` と同じ。compose はクリップのトラックの mod_routings を読む)。複数のトラックにまたがる
    /// 選択では、変調は代表クリップのトラックにだけ付く (「A」のレーンと同じ規則)。
    pub fn mod_owner(&self) -> u32 {
        self.rep.key.track_id
    }
}

/// 選択の帯の中身: 選んでいるクリップの欄と、セルのローンチ。
pub(super) fn draw_clip_band_body(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    sel: &InspectorSelection<'_>,
    area: Rect,
    pad: f32,
    y: f32,
) -> f32 {
    let w = area.w - pad * 2.0;
    let c = form::cols(area.x + pad, w);
    let y = match (sel.rep(), sel.band) {
        (Some(rep), Some(band)) => draw_clip_fields(app, ui, &ClipFields { sel, rep, c, w }, band, y),
        _ => y,
    };
    launch::draw_cell_launch(app, ui, area, pad, y)
}

/// 共通の欄と種類ごとの欄。値は選んでいるクリップ全部を畳んだもの (割れていれば「—」/ 空 / 消灯)、編集は全部へ
/// (歌声の声 / 読み上げの話者とスケールも)。名前と本文 (と「ここから読む」) はクリップ 1 つの値なので、
/// 1 つだけ選んでいるときに出す (何本ものクリップを同じ名前 / 同じセリフにする操作は無い)。
/// 種類の違うクリップを一緒に選んでいれば、代表の種類の欄の上に「… だけに効く」を出す (共通の欄は全部に効く)。
fn draw_clip_fields(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, band: ClipBand, y: f32) -> f32 {
    let (rep, kind) = (f.rep, band.kind);
    // 表示中の編集バッファ (本文 / フォント) を代表クリップへ向ける。1 フレームだけ古い値が見えるが、
    // 次のフレームで書き戻る。同じクリップを選び直しただけでは走らない。
    if app.cur.peph.clip_edit_buffer_target != Some(rep.key) {
        let key = rep.key;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(if kind == ClipBandKind::Text {
                AppEvent::ResyncClipTextEditBuffers(key)
            } else {
                AppEvent::ResyncClipEditBuffers(key)
            });
        }));
    }

    let mut y = y;
    if f.sel.single() {
        y = if kind == ClipBandKind::Text { text_body_row(app, ui, f, y) } else { name_row(app, ui, f, y) };
    }
    // 音声クリップの欄の出どころ (代表の編集が効く片のひと続き)。無ければ音声の欄は出さない。
    let audio_fade = (kind == ClipBandKind::Audio).then(|| app.audio_first_fade_of(rep.key, rep.clip)).flatten();
    // 逆再生は音声クリップにだけ効く: 種類が混ざっていれば共通の「再生」の行に並べず、音声の欄の先頭へ。
    y = playback_row(app, ui, f, y, audio_fade.is_some() && !band.mixed());
    if band.mixed() {
        let text = vocab::only_kind(kind, band.of_kind(), band.total());
        y = band::sub_heading(app, ui, "inspector_clip_only_kind", f.c.label_x, f.w, y + 2.0, &text, None);
    }
    match kind {
        ClipBandKind::Audio => audio_fade.map_or(y, |fade| audio::draw_audio_fields(app, ui, f, fade.len_beats, y)),
        ClipBandKind::Image => {
            app.inspector_image_event_summary(rep).map_or(y, |s| image::draw_image_fields(app, ui, f, &s, y))
        }
        ClipBandKind::Midi => voice::draw_clip_voice(app, ui, f, y),
        ClipBandKind::Text => {
            let y = talk::draw_talk(app, ui, f, y);
            subtitle::draw_subtitle(app, ui, f, y)
        }
        // 映像クリップの速度は端の Shift ドラッグが唯一の編集面 (`ClipContent::Video` の doc)、
        // オートメーションクリップは中身をレーンで描く。どちらも共通の欄だけ。
        ClipBandKind::Video | ClipBandKind::Automation => y,
    }
}

/// 「名前」: 確定 (Enter / 欄の外のクリック) で改名する。既存の改名の流れ (`BeginRenameClip` →
/// `RenameClipChanged` → `CommitRenameClip`) を 1 つの Edit で通すので、アレンジのクリップの上の改名欄は
/// 出ない (同じ値の入力欄が 2 つ並ばない)。打っている間の文字は入力欄が持つ。
fn name_row(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, y: f32) -> f32 {
    let (rep, c) = (f.rep.key, &f.c);
    form::label(ui, app, "inspector_clip_name_label", vocab::FIELD_NAME, c, y);
    let style = ui.text_input_style();
    // id にクリップを入れる: 入力中に選択が変わっても、別のクリップへ確定しない。
    let resp = ui.text_input_at(
        ("inspector_clip_name", rep.track_id, rep.clip_id),
        c.value(y),
        app.cur.song_doc.song().content_name(f.rep.clip.content_id),
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
///
/// 名前の欄と同じく、入力欄の id と確定先をクリップに束ねる — 入力中にショートカット (Alt+D の複製 /
/// Ctrl+Tab のタブ切り替え) で選択が変わっても、打っていた本文を別のクリップへ書かない。確定する値は入力欄が
/// 持つ文字 (`committed_text`)。窓に字幕の片が 1 つも見えていない (書く先が無い) クリップには欄を出さない。
fn text_body_row(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, y: f32) -> f32 {
    let (rep, c) = (f.rep.key, &f.c);
    if app.text_first_run_of(f.rep.clip).is_none() {
        return y;
    }
    let talks = f.rep.track.is_voicevox_vocal();
    form::label(ui, app, "inspector_text_body_label", if talks { vocab::FIELD_LINE } else { vocab::FIELD_TEXT }, c, y);
    let style = ui.text_input_style();
    let resp = ui.text_input_at(
        ("inspector_text_body", rep.track_id, rep.clip_id),
        c.value(y),
        &app.cur.peph.clip_text_content_edit_text,
        &style,
        |_| Edit::mutate(|_: &mut AppData| {}),
    );
    // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
    if let Some(value) = resp.committed_text.filter(|_| resp.committed || resp.blurred) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetClipTextContent { target: rep, value });
        }));
    }
    y + FORM_ROW_PITCH
}

/// 「再生」: [ミュート] (+ 音声クリップは [逆再生])。どちらも選んでいる全部を畳み、割れていれば消灯で出して、
/// 押すと全部をオンに揃える (ミュートは全種類で `Clip.muted` (SSoT) の 1 本、逆再生は音声クリップの片)。
fn playback_row(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, y: f32, audio: bool) -> f32 {
    let c = &f.c;
    form::label(ui, app, "inspector_clip_playback_label", vocab::FIELD_PLAYBACK, c, y);
    let muted = f.sel.fold_eq(|cl| Some(cl.clip.muted)) == Some(true);
    ui.toggle_button_at(
        "inspector_clip_mute",
        vocab::TOGGLE_MUTE,
        c.value_part(y, 2, 0),
        muted,
        &toggle_audio_style(&app.theme),
        move |_| form::broadcast(DiscreteClipEdit::Muted(!muted)),
    );
    if audio {
        audio::reverse_toggle(app, ui, f, c.value_part(y, 2, 1));
    }
    y + FORM_ROW_PITCH
}
