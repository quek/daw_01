//! 設定 window の「VOICEVOX」ページ (r.md #75): 合成の塊の長さ (秒)。

use std::cell::Cell;

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use super::page::Page;
use crate::app::{AppData, AppEvent};

/// 数値欄の幅 (px)。
const FIELD_W: f32 = 70.0;

pub(super) fn draw(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let app = pg.app;
    pg.heading(ui, "vox", "歌唱合成", true);
    let control = pg.row(ui, "vv_chunk", "合成の塊の長さ (秒)");
    let field_rect = Rect { w: FIELD_W.min(control.w), ..control };
    // 数値欄は inspector / export range と同じ `scrubable_number_at` の idiom
    // (drag-to-edit + click-to-type)。
    //
    // 値の適用は **確定 (drag release / 数値入力 / ダブルクリックリセット) でだけ**
    // 行う。確定は「曲全体の再合成 + app_config.json への書き込み」なので、drag の
    // per-frame 値でそれをやると掴んでいる間ずっと engine を叩くことになる。
    // `on_change` は Cell に溜めるだけにして、確定かどうかは同じフレームの
    // response (`dragging` / `editing_text`) から決める。
    let pending = Cell::new(None::<f64>);
    let resp = ui.scrubable_number_at(
        "settings_vv_chunk",
        field_rect,
        f64::from(app.ui_prefs.voicevox_chunk_secs),
        f64::from(common::voicevox_phrase::DEFAULT_CHUNK_SECS),
        ScrubableNumberFormat::Integer,
        &ScrubableNumberStyle {
            font_size: 13.0,
            sensitivity: 0.5,
            // クランプは widget 側に持たせる (= 掴んで振り切っても範囲外にならない)。
            range: Some((
                f64::from(common::voicevox_phrase::MIN_CHUNK_SECS),
                f64::from(common::voicevox_phrase::MAX_CHUNK_SECS),
            )),
            ..ScrubableNumberStyle::from_palette(&app.theme.core)
        },
        |v| {
            pending.set(Some(v));
            // 適用は下で行う (確定判定に同フレームの response が要る)。
            Edit::mutate(|_: &mut AppData| {})
        },
        None,
        None,
    );
    let active = resp.dragging || resp.editing_text;
    if let Some(v) = pending.get() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetVoicevoxChunkSecs { secs: v as f32, commit: !active });
        }));
    }
    // drag / 数値入力の**立ち下がり**で確定 (master フェーダーの undo bracket と同じ
    // edge 検出)。release フレームで `on_change` が来ないケースもあるので、確定は
    // 表示値 (`displayed_value`) から送る。
    if active != app.ui_ephemeral.voicevox_chunk_editing {
        let live = resp.displayed_value as f32;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.voicevox_chunk_editing = active;
            if !active {
                app.handle_event(AppEvent::SetVoicevoxChunkSecs { secs: live, commit: true });
            }
        }));
    }
    pg.hint(ui, "vv_chunk_1", "短いほど 1 音直したときが速く、長いほど音量が揃う。既定 60 秒。");
    pg.hint(
        ui,
        "vv_chunk_2",
        "変えると曲全体を合成し直します (前の設定の音はキャッシュに残るので、戻せばすぐ鳴ります)。",
    );
}
