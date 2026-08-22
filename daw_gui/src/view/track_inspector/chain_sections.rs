// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! チェーン直下の 3 セクション (「+ Plugin」 / Parallel Out / Sidechain) を
//! **スクロール viewport の top-down フロー** で描く。
//!
//! 旧実装は 3 つとも `area.y + area.h` からの逆算で inspector 下端に pin して
//! いた (`btns_y = area.y + area.h - btns_h - pad` を起点に上へ積む)。 その結果
//! (a) セクションが空〜少ないときは param viewport の下端とボタンの間に誰も描かない
//! 空白が残り、 (b) 逆算のために描画ループの高さを先に見積もる必要があって高さ式が
//! 描画と二重管理になり、 (c) 予約高に収まらない行を無言で捨てる cap
//! (パラアウト 5 行 / sidechain 4 行) が要って 6 本目以降のパラアウト先・5 個目以降の
//! sidechain source が **設定不能** だった。
//!
//! `modulation_rack::draw_modulation_rack` と同じ `(app, ui, area, pad, y) -> f32`
//! contract に揃えてある。 = inspector の縦位置の SSoT は「1 本の y カーソル」だけ。
//! この規約に乗る限り高さの事前計算も cap も構造的に発生しない。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};

use super::{SC_CTL_H, SC_NAME_H, SC_ROW_GAP, SC_ROW_H, SC_TAP_W};

/// 「+ Plugin」 (master bus は 「+ FX」) をチェーンリストの直下に置く。
///
/// plan_unified_plugin_picker.md: 旧 +Inst / +FX / +MIDI の 3 ボタンを 1 つに統合し、
/// 選んだプラグインの種別で行き先 (Instrument / FX / MIDI FX) を自動振り分けする。
/// master bus は audio fx のみなのでリスト側 (`refresh_picker_visible`) が FX に絞る。
/// ラベルだけ master は 「+ FX」 で期待値を示す。
///
/// チェーン末尾に置くのは 「このチェーンの末尾に足す」 が座標で自明になるため
/// (mixer の 「＋ Send」 が sends flow の最終行として描かれているのと同型)。
pub(super) fn draw_add_plugin_button(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    y: f32,
) -> f32 {
    const BTNS_H: f32 = 26.0;
    let is_master = app.cursor_track_id() == Some(common::model::MASTER_TRACK_ID);
    ui.button_at(
        "inspector_add_plugin",
        if is_master { "+ FX" } else { "+ Plugin" },
        Rect { x: area.x + pad, y, w: area.w - pad * 2.0, h: BTNS_H },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::OpenPluginPicker)),
    );
    y + BTNS_H + 12.0
}

/// 「読み込み失敗」 section — plugin_host での load に失敗した device を
/// **可視化し、 明示的に再 load できる** ようにする。
///
/// 失敗した device は song には残るが host に instance が無いので、 その
/// セッション中ずっと**無音**になる。 従来は status_message に 1 度出るだけで
/// 復旧手段が「project を開き直す」しか無く、 一時的な失敗 (shmem 名衝突など)
/// でもユーザーは原因も対処も分からなかった。
///
/// **自動リトライはしない**: plugin 側の恒常的な失敗 (DLL 欠損 / activate 失敗)
/// で無限ループになるため、 再試行は必ずユーザーの意思 (「再読込」 ボタン) で。
/// チェーン行側は名前に `[未ロード]` を付けて警告色で描くので、 このセクションは
/// 「どれが / なぜ / どう直すか」 を担当する。
pub(super) fn draw_failed_load_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let entries: Vec<crate::app_types::ChainEntry> = app
        .inspector_chain()
        .into_iter()
        .filter(|e| e.load_error.is_some())
        .collect();
    if entries.is_empty() {
        return y;
    }
    let Some(track_id) = app.cursor_track_id() else {
        return y;
    };
    ui.label_at(
        "inspector_loadfail_label",
        "読み込み失敗",
        area.x + pad,
        y,
        12.0,
        p.text_error,
    );
    y += 18.0 + 4.0;

    const NAME_H: f32 = 14.0;
    const ROW_H: f32 = 24.0;
    const ROW_GAP: f32 = 8.0;
    const BTN_W: f32 = 64.0;
    let row_w = area.w - pad * 2.0;
    let name_x = area.x + pad;
    let btn_x = area.x + area.w - pad - BTN_W;
    for (i, entry) in entries.iter().enumerate() {
        ui.label_at_clipped(
            ("inspector_loadfail_name", i),
            &entry.plugin_name,
            Rect { x: name_x, y, w: row_w, h: NAME_H },
            11.0,
            p.text_error,
        );
        // 理由は 1 行で切る (長い FFI エラーでも行が伸びない)。 全文は
        // status bar / ログに出ている。
        let reason = entry.load_error.as_deref().unwrap_or("");
        ui.label_at_clipped(
            ("inspector_loadfail_reason", i),
            reason,
            Rect {
                x: name_x,
                y: y + NAME_H + 4.0,
                w: (btn_x - 6.0 - name_x).max(1.0),
                h: 11.0 * 1.2,
            },
            11.0,
            p.text_dim,
        );
        let device_index = entry.device_index;
        ui.button_at(
            ("inspector_loadfail_reload", i),
            "再読込",
            Rect { x: btn_x, y: y + NAME_H, w: BTN_W, h: ROW_H },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ReloadDevice { track_id, device_index });
                })
            },
        );
        y += NAME_H + ROW_H + ROW_GAP;
    }
    y + 6.0
}

/// r.md #36: 「キーを全部プラグインに送る」 の per-device トグル。
///
/// 通常はホスト側が **プラグインが消化しなかったキーだけ** を拾うので、 エディタ窓に
/// フォーカスがあっても Space で再生 / 停止でき、 かつプラグインの文字入力欄に空白も
/// 打てる。 ただし Dear ImGui / GLFW / 自前 OpenGL 系のエディタは 「今テキスト入力中か」
/// も 「このキーを消化したか」 も外に一切出さないので、 自動判定が効かない。
/// そのプラグインだけ ON にして全キーを譲る (REAPER の FX ごとの同名オプションと同じ)。
///
/// 対象は **埋め込みエディタ窓を開くデバイスだけ**。 インライン param パネルしか持たない
/// デバイス (映像 FX / VOICEVOX / GUI 無し plugin) は別窓にフォーカスが行かないので無関係。
pub(super) fn draw_editor_key_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let entries: Vec<crate::app_types::ChainEntry> = app
        .inspector_chain()
        .into_iter()
        .filter(|e| e.has_embedded_gui && !e.shows_param_panel())
        .collect();
    if entries.is_empty() {
        return y;
    }
    let Some(track_id) = app.cursor_track_id() else {
        return y;
    };
    ui.label_at("inspector_keys_label", "エディタ窓のキー", area.x + pad, y, 12.0, p.text);
    y += 18.0 + 4.0;

    const ROW_H: f32 = 24.0;
    const NAME_H: f32 = 14.0;
    const ROW_GAP: f32 = 6.0;
    let row_w = area.w - pad * 2.0;
    for (i, entry) in entries.iter().enumerate() {
        ui.label_at_clipped(
            ("inspector_keys_name", i),
            &entry.plugin_name,
            Rect { x: area.x + pad, y, w: row_w, h: NAME_H },
            11.0,
            p.text,
        );
        let device_index = entry.device_index;
        let next = !entry.send_all_keys;
        ui.toggle_button_at(
            ("inspector_keys_toggle", i),
            "キーを全部プラグインに送る",
            Rect { x: area.x + pad, y: y + NAME_H, w: row_w, h: ROW_H },
            entry.send_all_keys,
            &super::toggle_audio_style(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetPluginSendAllKeys {
                        track_id,
                        device_index,
                        enabled: next,
                    });
                })
            },
        );
        y += NAME_H + ROW_H + ROW_GAP;
    }
    y + 6.0
}

/// パラアウト (Parallel Out) section (`docs/plan_paraout.md`)。
///
/// multi-out プラグインごとに 「展開」 ボタン (auto-create + group child tracks) と、
/// 展開済みなら port ごとの宛先 dropdown を出す。 行数の cap は無い
/// (viewport 内 top-down フローなのでスクロールで全行に到達できる)。
pub(super) fn draw_parallel_out_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let po_entries = app.parallel_output_entries();
    if po_entries.is_empty() {
        return y;
    }
    let row_h = 24.0;
    let row_gap = 4.0;
    ui.label_at("inspector_po_label", "Parallel Out", area.x + pad, y, 12.0, p.text);
    y += 18.0 + 4.0;

    let dropdown_w = 140.0;
    let name_x = area.x + pad;
    let right_x = area.x + area.w - pad;
    let choices = app.sidechain_source_choices();
    let labels: Vec<String> = choices.iter().map(|c| c.label.clone()).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    for (ei, entry) in po_entries.iter().enumerate() {
        let track_id = entry.track_id;
        let device_index = entry.device_index;
        // Entry row: plugin name (left) + explode button (right).
        let btn_w = 64.0;
        let btn_x = right_x - btn_w;
        // 手書きの 「1 文字 7px」 見積りは実 advance (半角 5.8 / 全角 11.6) と
        // 一致せず、 ASCII 名では過剰に切り CJK 名では溢れていた。 rect 幅で
        // 切る共通 helper に寄せる。
        ui.label_at_clipped(
            ("inspector_po_name", ei),
            &entry.plugin_name,
            Rect {
                x: name_x,
                y: y + 6.0,
                w: (btn_x - 6.0 - name_x).max(1.0),
                h: 11.0 * 1.2,
            },
            11.0,
            p.text,
        );
        ui.button_at(
            ("inspector_po_explode", ei),
            "展開",
            Rect { x: btn_x, y, w: btn_w, h: row_h },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ExplodeParallelOut { track_id, device_index });
                })
            },
        );
        y += row_h + row_gap;
        // Port rows (only once exploded): "Out N" + destination dropdown.
        if !entry.exploded {
            continue;
        }
        for port in 0..entry.aux_output_count as usize {
            let key = ei * 64 + port;
            ui.label_at(
                ("inspector_po_outlabel", key),
                &format!("Out {}", port + 1),
                name_x,
                y + 6.0,
                11.0,
                p.text,
            );
            let dropdown_x = right_x - dropdown_w;
            let selected_idx = match entry.routes.get(port).and_then(|o| *o) {
                None => 0,
                Some(dest) => choices
                    .iter()
                    .position(|c| c.track_id == Some(dest))
                    .unwrap_or(0),
            };
            if let Some(picked) = ui.dropdown(
                ("inspector_po_dropdown", key),
                Rect { x: dropdown_x, y, w: dropdown_w, h: row_h },
                &label_refs,
                selected_idx,
            ) && let Some(choice) = choices.get(picked)
            {
                let dest = choice.track_id;
                let p = port as u8;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetParallelOutputRoute {
                        track_id,
                        device_index,
                        port: p,
                        dest,
                    });
                }));
            }
            y += row_h + row_gap;
        }
    }
    y + 6.0
}

/// Sidechain section (PR4.5 + r.md #8)。
///
/// チェーンのプラグインごとに source picker (任意 track の出力を aux input port 0 へ)
/// と tap point (Pre-FX / Post-FX / Post-Fdr) を出す。 自分自身の track は picker 側で
/// 除外される (feedback cycle になり `compile_schedule` が `GraphError::Cycle` で弾く)。
/// 行数の cap は無い (旧実装は 4 行で打ち切っていて 5 個目以降が設定不能だった)。
pub(super) fn draw_sidechain_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let sc_entries = app.sidechain_entries();
    if sc_entries.is_empty() {
        return y;
    }
    ui.label_at("inspector_sc_label", "Sidechain", area.x + pad, y, 12.0, p.text);
    y += 18.0 + 4.0;

    let choices = app.sidechain_source_choices();
    let labels: Vec<String> = choices.iter().map(|c| c.label.clone()).collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let name_x = area.x + pad;
    let row_w = area.w - pad * 2.0;
    // B8 (r.md #8): tap point (Pre-FX/Post-FX/Post-Fdr) selector を source
    // dropdown の左に置く。
    const SC_TAP_POINTS: [common::model::TapPoint; 3] = [
        common::model::TapPoint::PreFx,
        common::model::TapPoint::PostFx,
        common::model::TapPoint::PostFader,
    ];
    let sc_tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
    // 1 行に [名前 | tap | source] を詰めると、 280px の inspector では名前に
    // 46px しか残らず "VOICEVOX (builtin)" が "VOIC…" になり、 tap も既定の
    // "Post-Fdr" (59px) が 40px の文字領域に入らず "Post…" になって Post-FX と
    // 区別できなかった。 mixer の send slot と同じ 2 段構成にして
    // 「名前は行いっぱい / 操作は下段」 にする。
    let tap_w = SC_TAP_W;
    let tap_x = name_x;
    let dropdown_x = tap_x + tap_w + 6.0;
    let dropdown_w = (row_w - tap_w - 6.0).max(40.0);
    for (i, entry) in sc_entries.iter().enumerate() {
        ui.label_at_clipped(
            ("inspector_sc_name", i),
            &entry.plugin_name,
            Rect { x: name_x, y, w: row_w, h: SC_NAME_H },
            11.0,
            p.text,
        );
        let ctl_y = y + SC_NAME_H;
        let selected_idx = match entry.current_source {
            None => 0,
            Some(src_id) => choices
                .iter()
                .position(|c| c.track_id == Some(src_id))
                .unwrap_or(0),
        };
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_dropdown", i),
            Rect { x: dropdown_x, y: ctl_y, w: dropdown_w, h: SC_CTL_H },
            &label_refs,
            selected_idx,
        ) && let Some(choice) = choices.get(picked)
        {
            let track_id = entry.track_id;
            let device_index = entry.device_index;
            let new_source = choice.track_id;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSidechainSource {
                    track_id,
                    device_index,
                    port: 0,
                    source: new_source,
                });
            }));
        }
        // B8 (r.md #8): tap point selector (source の左)。 port 0 のみ
        // (multi-port は aux_input_count IPC が要る follow-up、 稀なので保留)。
        let tap_sel = SC_TAP_POINTS
            .iter()
            .position(|t| *t == entry.current_tap_point)
            .unwrap_or(2);
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_tap", i),
            Rect { x: tap_x, y: ctl_y, w: tap_w, h: SC_CTL_H },
            &sc_tap_labels,
            tap_sel,
        ) && let Some(&tp) = SC_TAP_POINTS.get(picked)
        {
            let track_id = entry.track_id;
            let device_index = entry.device_index;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAuxInputTapPoint {
                    track_id,
                    device_index,
                    port: 0,
                    tap_point: tp,
                });
            }));
        }
        y += SC_ROW_H + SC_ROW_GAP;
    }
    y + 6.0
}
