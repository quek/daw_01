// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! S4c Phase B-E: piano_roll widget の本体 (`piano_roll()` エントリ — view 構築 → press/drag/
//! release state machine → heavy 描画 → 旧 `piano_roll_view` の toolbar / legend / wheel /
//! hover mirror 駆動)。型・helper は `use super::*` で親から継承する。
//!
//! 旧 `ui/` 汎用 widget の `make_edit` 翻訳層を撤去し、各 interaction site が
//! `ui.push_edit(Edit::mutate(|app: &mut AppData| { ... }))` を直接発行する (`AppData` 直結)。

#![allow(clippy::too_many_lines)]

use super::*;

use daw_ui_core::{ButtonTextAlign, ToggleButtonStyle};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::theme::Palette;
use crate::view::snap::{self, SNAP_LABELS};
use crate::view::track_color;
use crate::widgets::select_modifier::{RangeItem, SelectModifier, range_block};

/// Snap toolbar / legend の小さめトグル (標準の角丸 6px・14px 文字より 1 段小さい)。
/// 色は毎フレームのパレットから引く (r.md #48: `const` はテーマ切替に追従できない)。
fn snap_toggle_style(p: &Palette) -> ToggleButtonStyle {
    ToggleButtonStyle { radius: 3.0, font_size: 12.0, ..ToggleButtonStyle::from_palette(p) }
}

pub fn piano_roll(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> PianoRollResponse {
        let p = &*app.theme.core;
        // ---- view 構築 (レイアウト SSoT) + toolbar (常時描画) ----
        let built = view_build::build(app, area);
        draw_snap_toolbar(app, ui, built.toolbar_rect);
        let Some(content) = built.content else {
            // 表示する MIDI クリップが無い (未選択 or 非 MIDI のみ) → placeholder。
            // widget が走らないので、歌詞編集 mirror が残っていたら false に戻す
            // (stale-true で Esc が widget へ委ねられ続けて消える事故を防ぐ)。
            if app.ui_ephemeral.piano_roll_lyric_editing {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.ui_ephemeral.piano_roll_lyric_editing = false;
                }));
            }
            ui.panel("pr_bg_empty", built.body_full, p.panel, 0.0);
            ui.label_at(
                "pr_no_clip",
                "(\u{30af}\u{30ea}\u{30c3}\u{30d7}\u{304c}\u{9078}\u{629e}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093})",
                built.body_full.x + 12.0,
                built.body_full.y + 12.0,
                12.0,
                p.text_dim,
            );
            return PianoRollResponse::default();
        };
        let view_build::PrContent {
            notes: widget_notes,
            view,
            style,
            selected,
            shown,
            target,
            clip_starts,
            clip_origin_beat,
            multi,
            legend_rect,
            grid,
            kbd,
            ruler,
            vel_area,
            zoom_x,
            zoom_y,
        } = content;
        let notes: &[Note] = &widget_notes;
        let selected: &[NoteId] = &selected;
        let style: &PianoRollStyle = &style;
        // grid/kbd/ruler/vel_area は build() で一度だけ算出済 (レイアウト SSoT)。scalar は rect から導出。
        let ruler_h = ruler.h;
        let vel_h = vel_area.h;
        let id = "piano_roll";

        // ---- auto-fit 追跡 + multi union-fit (旧 piano_roll_view::draw L112-139) ----
        // 現フレームの grid 領域サイズを記録 (X キー / Fit ボタン / SelectClip 経由の fit 用、1 frame 遅延 OK)。
        // pending_pianoroll_fit が立っていたら消費して fit を再実行。
        let grid_size = (grid.w, grid.h);
        if app.ui_ephemeral.last_pianoroll_grid_size != grid_size
            || app.ui_ephemeral.pending_pianoroll_fit
        {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.last_pianoroll_grid_size = grid_size;
                if app.ui_ephemeral.pending_pianoroll_fit {
                    app.ui_ephemeral.pending_pianoroll_fit = false;
                    app.handle_event(AppEvent::FitPianoRollToClip);
                }
            }));
        }
        // 表示集合 (shown) が変わったら共有 viewport (`multi_clip_view`) を union-fit し直す。
        if multi {
            let keys: Vec<common::model::ClipKey> =
                shown.iter().filter_map(|r| app.clip_key_of(*r)).collect();
            if app.ui_prefs.multi_clip_view_key != keys {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_prefs.multi_clip_view_key = keys.clone();
                    app.handle_event(AppEvent::FitPianoRollToClip);
                }));
            }
        }

        let wid = WidgetId::ROOT.child((b"piano_roll_widget", &id));
        let pointer = ui.pointer();

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 mode =====
        // Frame 開始時、lyric_editing が selected と sync しているか defensive check。
        // 編集対象 note が消失したら自動で None に戻す (note 削除等のため)。
        let mut lyric_editing: Option<NoteId> = {
            let state: &mut PianoRollState = ui.widget_state(wid);
            if let Some(eid) = state.lyric_editing
                && !notes.iter().any(|n| n.id == eid)
            {
                state.lyric_editing = None;
            }
            state.lyric_editing
        };
        // L キー検知: lyric_editing == None かつ selected.len() == 1 のときのみ起動。
        // `"piano_roll.edit_lyric"` は `is_typing_only_shortcut` に追加済 (M14 Phase 59)。
        // 編集中 (typing_focus = true) は shortcut layer を素通りして text_input に届く
        // (= `'l'` 文字としてタイプ可能)。take_shortcut は frame 頭の typing_lock 判定後
        // pending_shortcuts に積まれた name を引くので、編集中は false を返す。
        // 選択条件 (`selected.len() == 1`) を `take_shortcut` より **先** に評価する —
        // 逆順だと条件を満たさない instance が L を黙って消費し、 同 frame の後続
        // instance (条件を満たす方) から shortcut を奪う (review)。
        if lyric_editing.is_none()
            && selected.len() == 1
            && let Some(name) = style.lyric_edit_shortcut
            && ui.take_shortcut(name)
        {
            lyric_editing = Some(selected[0]);
            // 編集モードに入る瞬間、stale な note_drag セッションを clear (drag 中に L
            // を押した稀なケース対策)。
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.lyric_editing = lyric_editing;
            state.note_drag = None;
        }
        // Esc 検知: 編集モード中の Esc は "escape" shortcut が global で consume するため
        // text_input の自前 Esc ハンドラ経由ではなく piano_roll が明示的に handle する
        // (= take_shortcut("escape") で消費 → lyric_editing = None で即時 cancel)。
        // これで「編集中の Esc → 1 frame で完全 cancel」を保証 (text_input の blur 検出
        // 経路 (resp.focused = false) は外 click 等の defensive fallback として残す)。
        if let Some(edit_id_for_esc) = lyric_editing
            && ui.take_shortcut("escape")
        {
            // text_input の focus を明示的に clear (text_input id は ("piano_roll_lyric", edit_id))
            let ti_wid =
                WidgetId::ROOT.child((b"text_input", &("piano_roll_lyric", edit_id_for_esc)));
            ui.clear_focus_if_focused(ti_wid);
            lyric_editing = None;
            ui.widget_state::<PianoRollState>(wid).lyric_editing = None;
        }
        let editing_mode = lyric_editing.is_some();

        // grid / kbd / ruler / vel_area は build() が算出済 (レイアウト SSoT、上で destructure)。

        // visible filter (二分探索)
        let view_end_beat = view.start_beat + view.len_beats;
        let s_idx =
            notes.partition_point(|n| n.start_beat + n.len_beats < view.start_beat);
        let e_idx = s_idx
            + notes[s_idx..].partition_point(|n| n.start_beat <= view_end_beat);
        let visible: &[Note] = &notes[s_idx..e_idx];

        // ----- press 振り分け (state 更新) -----
        // 空き grid の drag は marquee (take_drag_rect_in_rect) が drag state を握るので (#102、
        // gate の `note_hit().is_none()` で除外)、 ここでは note hit を widget が drag として掴む。
        //
        // r.md #35: 旧実装はここに `!shift` gate があり、 Shift+note press を note drag からも
        // marquee (`note_hit().is_none()` 必須) からも弾いていた。 結果 **Shift+click が完全に
        // 無反応** だった。 gate を外して Shift+press でも drag session を張り、 release の短 click
        // 格下げ経路 (`drag_short_click_pos` が `(ctrl, shift)` を持ち回る) で範囲選択に解決する。
        // M14 Phase 59: editing_mode 中は drag/click を全短絡。
        let just_pressed_on_note = !editing_mode
            && pointer.primary_just_pressed
            && pointer.pos.is_some_and(|(px, py)| grid.contains(px, py));

        if just_pressed_on_note
            && let Some((px, py)) = pointer.pos
            && let Some((hit_id, kind)) =
                note_hit(notes, view, grid, px, py, style.resize_handle_px)
        {
            let drag_ids: Vec<NoteId> = if selected.contains(&hit_id) {
                selected.to_vec()
            } else {
                vec![hit_id]
            };
            let anchors: Vec<NoteDragAnchor> = drag_ids
                .iter()
                .filter_map(|id_target| {
                    notes.iter().find(|n| n.id == *id_target).map(|n| NoteDragAnchor {
                        id: n.id,
                        start_beat: n.start_beat,
                        pitch: n.pitch,
                        len_beats: n.len_beats,
                    })
                })
                .collect();
            if !anchors.is_empty() {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let press_shift = pointer.modifiers.shift;
                let state: &mut PianoRollState = ui.widget_state(wid);
                state.note_drag = Some(NoteDragSession {
                    kind,
                    anchor_mouse: (px, py),
                    anchors,
                    last_mouse: (px, py),
                    last_alt: press_alt,
                    last_ctrl: press_ctrl,
                    last_shift: press_shift,
                });
            }
        }

        // ----- 鍵盤レーン press session (M14 Phase 84 / daw_01 #055) -----
        // press 開始が kbd rect 内なら keyboard preview session を開始する (note drag とは x 領域で
        // 排他、grid.contains で gate される just_pressed_on_note とは独立)。release で終了。
        // editing_mode 中は無効 (歌詞 typing 優先)。pitch は後段の response 計算で毎フレーム算出。
        {
            let state: &mut PianoRollState = ui.widget_state(wid);
            if pointer.primary_just_pressed
                && !editing_mode
                && pointer.pos.is_some_and(|(px, py)| kbd.contains(px, py))
            {
                state.keyboard_pressing = true;
            }
            if pointer.primary_just_released {
                state.keyboard_pressing = false;
            }
        }

        // ----- velocity lane press 振り分け (M14 Phase 64 / daw_01 #018) -----
        // vel_h > 0 のとき vel_area 内 press でかつ velocity bar 上なら velocity_drag 開始。
        // bar 上でなければ何もしない (= lane 余白 click は no-op で selection も変えない)。
        // editing_mode / note_drag 既に active のときは skip (排他)。
        // (r.md #35) 旧実装はここにも `!shift` gate があったが、 これは Shift が矩形選択の
        // 起動修飾だった頃の名残で、 Shift+velocity drag を無反応にするだけだった。 Shift の
        // 意味は「選択の範囲指定」 に一本化したので velocity lane では無修飾と同じ扱いにする。
        let just_pressed_in_vel_lane = !editing_mode
            && pointer.primary_just_pressed
            && vel_h > 0.0
            && pointer.pos.is_some_and(|(px, py)| vel_area.contains(px, py));
        if just_pressed_in_vel_lane
            && ui.widget_state::<PianoRollState>(wid).note_drag.is_none()
            && let Some((px, py)) = pointer.pos
            && let Some(hit_id) = velocity_bar_hit(
                visible,
                view,
                vel_area,
                px,
                style.velocity_bar_width_px,
                4.0,
                // #33: 同じ x に重なった bar のうち選択中 note を優先 hit する。
                |id| selected.contains(&id),
            )
        {
            let target_ids: Vec<NoteId> = if selected.contains(&hit_id) {
                selected.to_vec()
            } else {
                vec![hit_id]
            };
            let anchor_velocities: Vec<(NoteId, u8)> = target_ids
                .iter()
                .filter_map(|id_target| {
                    notes
                        .iter()
                        .find(|n| n.id == *id_target)
                        .map(|n| (n.id, n.velocity))
                })
                .collect();
            if !anchor_velocities.is_empty() {
                let final_targets: Vec<NoteId> =
                    anchor_velocities.iter().map(|(id, _)| *id).collect();
                let state: &mut PianoRollState = ui.widget_state(wid);
                state.velocity_drag = Some(VelocityDragSession {
                    target_ids: final_targets,
                    anchor_velocities,
                    anchor_mouse: (px, py),
                    last_mouse: (px, py),
                });
            }
        }

        // ----- drag continue (描画用 delta を計算) + release 検出 -----
        // 拍は f64、pixel は f32 なので変換を 1 箇所で吸収。 view.len_beats==0 は他の helper
        // ([:369, :651]) と同じく 1e-6 floor で防御 (0 除算で inf が伝播するのを回避)。
        let safe_len_beats = view.len_beats.max(1e-6);
        let beat_per_px: f64 = safe_len_beats / f64::from(grid.w.max(1.0));
        // SnapConfig::Adaptive 用 zoom = grid.w / view.len_beats。
        let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;
        let pitch_per_px = view.pitch_visible / grid.h.max(1.0);

        // ----- 空白ダブルクリック作成 press の検出 -----
        // 「double-click の 2 度目の press」が空白 grid 上 (note_hit なし) なら note 作成 session を
        // 開始する。press 即時に取るので、このままボタンを放さず drag すれば長さを決められる
        // (Bitwig 流「continue to hold the mouse down, and then drag left or right to ... lengthen」)。
        // start_beat (snap) と pitch (行 ceil = Insert と同式) を press 位置で確定。長さ軸 (左右)
        // のみ扱い pitch は固定。editing_mode / note_drag 既存中は skip。`note_create_press` は
        // 下の marquee gate でも参照し、この press を marquee が二重に所有しないよう抑制する。
        let note_create_press: Option<(f32, f32)> = if editing_mode {
            None
        } else {
            ui.take_double_click_press_in_rect(grid)
        };
        if let Some((px, py)) = note_create_press
            && note_hit(notes, view, grid, px, py, style.resize_handle_px).is_none()
            && ui.widget_state::<PianoRollState>(wid).note_drag.is_none()
        {
            let press_alt = pointer.modifiers.alt;
            let raw_start = (view.start_beat + f64::from(px - grid.x) * beat_per_px).max(0.0);
            let start_beat = view
                .snap
                .snap_beat(raw_start, press_alt, zoom_x_px_per_beat)
                .max(0.0);
            // Insert / 旧 dbl-click 作成と同じ ceil 逆写像 (#012)。Fold mode も RowGeometry が吸収。
            let geom = RowGeometry::compute(view, grid);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pitch = (geom.y_to_pitch_f(py).ceil() as i32).clamp(0, 127) as u8;
            // warp 先 = 既定長ノートの右端の screen x (Ableton Live 流)。
            // カーソルをここへ動かし、 anchor をここに置く = カーソル＝掴んでいる右端が一致。
            let default_len = view.default_note_len_beats.max(0.0625);
            #[allow(clippy::cast_possible_truncation)]
            let warp_x = grid.x + ((start_beat + default_len - view.start_beat) / beat_per_px) as f32;
            ui.warp_cursor(warp_x, py);
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.note_create = Some(NoteCreateSession {
                start_beat,
                pitch,
                anchor_mouse: (warp_x, py),
                press_x: px,
                last_mouse: (warp_x, py),
                last_alt: press_alt,
                dragged: false,
                warp_settled: false,
            });
        }

        // ----- ruler press 振り分け (M14 Phase 69 / daw_01 #041) -----
        // arrangement #024 と完全同 idiom: plain (= Shift 非保持) は playhead seek、
        // Shift 押下で loop range edit (NewRange / Start/End/Middle drag)。
        // editing_mode 中 / ruler_h <= 0 のときは一切処理しない (= 既存挙動完全互換)。
        // grid / vel_area とは y 軸で完全分離されているので note_drag / velocity_drag と
        // 競合せず、 振り分け順序の制約はない (= 独立 block)。
        //
        // `view.ruler_h <= 0.0` のとき `ruler_h = view.ruler_h.max(0.0).min(rect.h * 0.5)` が
        // 0.0 になり ruler.h も 0、 ruler.contains は y 1 行を判定するが帯がないので普通の
        // pointer.pos は決して入らない (= defensive で skip しなくても安全)。 ただし明示的に
        // gate しておく方が読みやすいので `ruler_h > 0.0` 条件を入れる。
        let mut press_seek_beat: Option<f64> = None;
        if !editing_mode
            && ruler_h > 0.0
            && pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && ruler.contains(px, py)
        {
            let press_beat =
                view.start_beat + f64::from(px - ruler.x) * beat_per_px;
            let press_alt = pointer.modifiers.alt;
            if pointer.modifiers.shift {
                // Shift + ruler drag → loop range edit (NewRange / Start/End/Middle handle)。
                let kind = if let Some(range) = view.loop_range {
                    match loop_band_hit_kind(
                        range,
                        view.start_beat,
                        view.len_beats,
                        ruler,
                        px,
                        4.0,
                    ) {
                        Some(LoopBandHit::Start) => LoopDragKind::Start,
                        Some(LoopBandHit::End) => LoopDragKind::End,
                        Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                        None => LoopDragKind::NewRange,
                    }
                } else {
                    LoopDragKind::NewRange
                };
                // NewRange の anchor 端点は press 時 snap で grid に着地 (release 端点も
                // `compute_loop_drag_endpoints` で snap される、 arrangement #024 と同 idiom)。
                let anchor_press_beat_for_session = match kind {
                    LoopDragKind::NewRange => view
                        .snap
                        .snap_beat(press_beat, press_alt, zoom_x_px_per_beat),
                    _ => press_beat,
                };
                let anchor_loop = view.loop_range.unwrap_or((
                    anchor_press_beat_for_session,
                    anchor_press_beat_for_session,
                ));
                let state: &mut PianoRollState = ui.widget_state(wid);
                state.loop_drag = Some(LoopDragSession {
                    kind,
                    anchor_loop,
                    anchor_press_beat: anchor_press_beat_for_session,
                    anchor_mouse_x: px,
                    last_mouse_x: px,
                    last_alt: press_alt,
                });
            } else {
                // plain (Shift 非保持) ruler click/drag → playhead seek session。
                let snapped = view
                    .snap
                    .snap_beat(press_beat, press_alt, zoom_x_px_per_beat)
                    .max(0.0);
                let state: &mut PianoRollState = ui.widget_state(wid);
                state.playhead_drag = Some(PlayheadDragSession {
                    last_mouse_x: px,
                    last_emitted_beat: snapped,
                });
                press_seek_beat = Some(snapped);
            }
        }
        if let Some(beat) = press_seek_beat {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.seek_playhead_to(beat);
            }));
        }

        // drag 継続中は毎 continuation frame で `last_mouse` / `last_alt` を update。
        // **release frame の `last_alt` は update しない** — 同 frame に ModifiersChanged(alt=false)
        // が先行する現象 (alt が一瞬 false に化ける) を回避するため、 release 直前 frame の値を保持する。
        // **release frame の `last_mouse` は pointer.pos が anchor と異なる場合のみ update** —
        // winit は release frame で `pointer.pos` を press 位置に戻すことがあり、 そのまま上書きすると
        // delta=0 で commit not pushed (drag が「元に戻る」 ように見える)。 pointer.pos == anchor のときは
        // continuation 由来の last_mouse を保持し、 そうでないときは pointer.pos が真値 (= 通常 release
        // pos、 OR press → 1 frame で release した short drag の release pos) として update する。
        if let Some((px, py)) = pointer.pos {
            let alt_now = pointer.modifiers.alt;
            let ctrl_now = pointer.modifiers.ctrl;
            let shift_now = pointer.modifiers.shift;
            let state: &mut PianoRollState = ui.widget_state(wid);
            if let Some(ref mut nd) = state.note_drag {
                if !pointer.primary_just_released {
                    nd.last_mouse = (px, py);
                    nd.last_alt = alt_now;
                    nd.last_ctrl = ctrl_now;
                    // (r.md #35) shift も同じ careful-update。 release frame は skip。
                    nd.last_shift = shift_now;
                } else if (px, py) != nd.anchor_mouse {
                    nd.last_mouse = (px, py);
                }
            }
            // note_create continuation。 まず warp 着地判定: press_x → anchor (右端) の
            // 中点をカーソルが越えたら settled。 settled までは last_mouse を更新せず anchor のまま
            // 保持する (warp の非同期ジャンプ由来の `PointerMoved` を長さに混入させない = ドラッグ
            // 開始直後の一瞬の最短化を防ぐ)。 settled 後は note_drag と同じ winit release-frame 巻き
            // 戻し対策で last_mouse / last_alt を update し、 左右いずれかに作成閾値 (4px) ぶん動いたら
            // `dragged` を latch (一度立てば解除しない)。 **左方向も latch 対象**: 右端から左へ短縮
            // するとき一度右へ振り直す手間を不要にする (Bitwig「drag left or right to shorten or lengthen」)。
            if let Some(ref mut nc) = state.note_create {
                if !nc.warp_settled {
                    let mid = (nc.press_x + nc.anchor_mouse.0) * 0.5;
                    // 右端 (anchor) は press_x 以上にあるので、 中点以上 = warp が反映された。
                    if px >= mid {
                        nc.warp_settled = true;
                    }
                }
                if nc.warp_settled {
                    if !pointer.primary_just_released {
                        nc.last_mouse = (px, py);
                        nc.last_alt = alt_now;
                        if (px - nc.anchor_mouse.0).abs() >= NOTE_CREATE_DRAG_PX {
                            nc.dragged = true;
                        }
                    } else if (px, py) != nc.anchor_mouse {
                        nc.last_mouse = (px, py);
                        if (px - nc.anchor_mouse.0).abs() >= NOTE_CREATE_DRAG_PX {
                            nc.dragged = true;
                        }
                    }
                }
            }
            // velocity_drag 側も同様に last_mouse update (note_drag と同じ winit release frame
            // pos 巻き戻し対策)。 alt は velocity drag の挙動に影響しない (絶対値 mode 固定)。
            // continuation frame は常に update、 release frame は pointer.pos が anchor と異なる
            // ときのみ update (winit が release frame で pointer.pos を press 位置に巻き戻す bug 対策)。
            if let Some(ref mut vd) = state.velocity_drag
                && (!pointer.primary_just_released || (px, py) != vd.anchor_mouse)
            {
                vd.last_mouse = (px, py);
            }
            // (M14 Phase 69 / daw_01 #041) loop_drag continuation:
            // last_mouse_x / last_alt を update (arrangement と完全同 idiom)。 release frame で alt を
            // 上書きしないのは ModifiersChanged 先行の race 回避 (note_drag と同根)。
            if let Some(ref mut ld) = state.loop_drag {
                if pointer.primary_just_released {
                    // release frame は winit が pointer.pos を press 位置に巻き戻す場合があるため、
                    // anchor_mouse_x と同値 (= exact f32 equality) のときは update を skip し、
                    // continuation 由来の last_mouse_x を保持する (note_drag の `(px, py) != nd.anchor_mouse`
                    // tuple 比較と同 idiom)。 ここは exact equality が意味を持つ (winit の巻き戻しは
                    // bit-perfect な復元なので f32::EPSILON より厳しい比較を要求するわけではない)。
                    #[allow(clippy::float_cmp)]
                    let pos_moved = px != ld.anchor_mouse_x;
                    if pos_moved {
                        ld.last_mouse_x = px;
                    }
                } else {
                    ld.last_mouse_x = px;
                    ld.last_alt = alt_now;
                }
            }
            // playhead_drag は release では emit しないので last_mouse_x の release frame 巻き戻し
            // を気にする必要が無い (continuation の最後の `last_emitted_beat` が真値)。 ただし
            // continuation frame では update して将来の visual debug を可能にする。
            if let Some(ref mut pd) = state.playhead_drag
                && !pointer.primary_just_released
            {
                pd.last_mouse_x = px;
            }
        }

        // ---- ドラッグ端オートスクロール (piano roll) ----
        // drag 中、pointer が grid 端の hot-zone に入ったら view を自動スクロールし、掴んでいる対象が
        // カーソルに追従し続ける。横 (beat) は `ScrollByBeats` delta、縦 (pitch) は `SetTopPitch` 絶対値
        // (top_pitch は u8 なので端数を `edge_pitch_accum` に貯めて整数 semitone 単位で発火)。note drag /
        // create は相対 delta なので実スクロール px ぶん anchor を逆 shift して追従させる。ruler の
        // loop/playhead は絶対 px→beat 再解決で自動追従するため shift しない。`request_redraw` で次フレーム
        // を確保し、カーソルを端で止めたままでもスクロール継続させる。
        if pointer.primary_pressed && !pointer.primary_just_released {
            // 移動量ゲート: press からの移動が ACTIVATE_PX 以上のときのみ端スクロールを許可。
            let moved_enough = {
                let state: &mut PianoRollState = ui.widget_state(wid);
                if pointer.primary_just_pressed {
                    state.edge_scroll_press = pointer.pos;
                    // 新しい drag の開始で pitch アキュムレータをリセット (前 drag の端数が
                    // 残って次 drag 初回フレームで pitch がジャンプするのを防ぐ)。
                    state.edge_pitch_accum = 0.0;
                }
                let gate = daw_ui_core::widgets::edge_scroll::ACTIVATE_PX;
                matches!((state.edge_scroll_press, pointer.pos),
                    (Some(p), Some(c)) if (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2) >= gate * gate)
            };
            let axes: Option<(bool, bool)> = if moved_enough {
                let state: &mut PianoRollState = ui.widget_state(wid);
                if let Some(nd) = state.note_drag.as_ref() {
                    Some(match nd.kind {
                        NoteDragKind::Move => (true, true), // 移動は横 + 縦 (pitch)。
                        NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight => (true, false),
                    })
                } else if state.note_create.as_ref().is_some_and(|nc| nc.warp_settled)
                    || state.loop_drag.is_some()
                    || state.playhead_drag.is_some()
                {
                    // 新規作成 (warp 着地後) / ruler の loop / playhead: いずれも横軸のみ。
                    Some((true, false))
                } else {
                    None
                }
            } else {
                None
            };
            let drag_rect_wid = wid.child(b"rect_select");
            let marquee_active = moved_enough
                && axes.is_none()
                && {
                    let st: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                        ui.widget_state(drag_rect_wid);
                    st.drag_start.is_some()
                };
            if let Some((ax, ay)) = axes.or_else(|| marquee_active.then_some((true, true))) {
                let cfg = daw_ui_core::widgets::edge_scroll::EdgeScrollCfg::default();
                let (dx, dy) = daw_ui_core::widgets::edge_scroll::edge_scroll_delta(
                    pointer.pos,
                    grid,
                    cfg,
                    ax,
                    ay,
                );
                // 横: view を min_start_beat で clamp して **実際に適用される** delta 拍を求める
                // (arrangement と同パターン)。view 層の `SetPianoRollScrollX(_.max(0))` clamp と一致し、
                // 左端で anchor が要求 px 分だけ過剰 shift して対象が飛ぶ runaway を防ぐ。
                // `applied_beat_px` は beat (横) 軸の anchor 補正量 (単位は px)。
                let (scroll_by_beats, applied_beat_px) = if dx == 0.0 || beat_per_px <= 1e-6 {
                    (0.0, 0.0)
                } else {
                    let new_start =
                        (view.start_beat + f64::from(dx) * beat_per_px).max(view.min_start_beat);
                    let actual = new_start - view.start_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = (actual / beat_per_px) as f32;
                    (actual, px)
                };
                // 縦 pitch: 端数を accum に貯め整数 semitone 単位で SetTopPitch。
                let mut new_top_pitch: Option<u8> = None;
                let mut applied_pitch_px = 0.0_f32;
                {
                    let state: &mut PianoRollState = ui.widget_state(wid);
                    if dy != 0.0 && pitch_per_px > 1e-6 {
                        let px_per_semitone = 1.0 / pitch_per_px;
                        state.edge_pitch_accum += dy * pitch_per_px; // 下=正 (lower pitch へ)。
                        #[allow(clippy::cast_possible_truncation)]
                        let step = state.edge_pitch_accum.trunc();
                        if step != 0.0 {
                            state.edge_pitch_accum -= step;
                            // 下スクロール (step > 0) = lower pitch を出す = top_pitch 減。
                            let cur = view.pitch_top;
                            let next = (cur - step).clamp(11.0, 127.0);
                            let applied = cur - next;
                            if applied != 0.0 {
                                #[allow(
                                    clippy::cast_possible_truncation,
                                    clippy::cast_sign_loss
                                )]
                                let next_u = next.round() as u8;
                                new_top_pitch = Some(next_u);
                                applied_pitch_px = applied * px_per_semitone;
                            }
                        }
                    } else {
                        // 縦 zone 外の frame は accum をリセット (stale 防止)。
                        state.edge_pitch_accum = 0.0;
                    }
                }
                let scrolled_x = scroll_by_beats != 0.0;
                if scrolled_x {
                    // edge auto-scroll の横スクロール (delta 拍)。widget は clip 相対オフセットを
                    // 知らないので delta で渡り、ここで `pianoroll_scroll_beat` に加算 (handler が `>= 0` clamp)。
                    let by = scroll_by_beats;
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        #[allow(clippy::cast_possible_truncation)]
                        let next = (f64::from(app.pianoroll_scroll_beat()) + by) as f32;
                        app.handle_event(AppEvent::SetPianoRollScrollX(next));
                    }));
                }
                if let Some(p) = new_top_pitch {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollTopPitch(p));
                    }));
                }
                if scrolled_x || new_top_pitch.is_some() {
                    if marquee_active {
                        let st: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                            ui.widget_state(drag_rect_wid);
                        if let Some(s) = st.drag_start.as_mut() {
                            s.0 -= applied_beat_px;
                            s.1 -= applied_pitch_px;
                        }
                    } else {
                        let state: &mut PianoRollState = ui.widget_state(wid);
                        if let Some(nd) = state.note_drag.as_mut() {
                            nd.anchor_mouse.0 -= applied_beat_px;
                            nd.anchor_mouse.1 -= applied_pitch_px;
                        }
                        if let Some(nc) = state.note_create.as_mut() {
                            // 新規作成は横のみ。anchor と warp 判定基準 press_x を同 shift。
                            nc.anchor_mouse.0 -= applied_beat_px;
                            nc.press_x -= applied_beat_px;
                        }
                        // loop/playhead: 絶対 px→beat 再解決で自動追従 → shift 不要。
                    }
                    ui.request_redraw();
                }
            }
        }

        // (M14 Phase 69 / daw_01 #041) playhead_drag continuation の per-frame live update。
        // press frame は press block 内で発火済 (`press_seek_beat`)、 ここは continuation のみ。
        // release frame は emit せず、 後段で take して discard する (commit-by-release 無し)。
        // `last_emitted_beat` で同値発火を抑制 (1e-6 拍 = ~10μs @ 120BPM 以下は ignore)。
        // editing_mode 中は press block 自体が skip されているので playhead_drag が立つことは無く、
        // ここも naturally skip。
        if !editing_mode
            && let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_pressed
            && !pointer.primary_just_released
        {
            let alt = pointer.modifiers.alt;
            let mut emit_beat: Option<f64> = None;
            {
                let state: &mut PianoRollState = ui.widget_state(wid);
                if let Some(ref mut pd) = state.playhead_drag {
                    let raw = view.start_beat + f64::from(px - ruler.x) * beat_per_px;
                    let next = view
                        .snap
                        .snap_beat(raw, alt, zoom_x_px_per_beat)
                        .max(0.0);
                    if (next - pd.last_emitted_beat).abs() > 1e-6 {
                        emit_beat = Some(next);
                        pd.last_emitted_beat = next;
                    }
                }
            }
            if let Some(beat) = emit_beat {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.seek_playhead_to(beat);
                }));
            }
        }

        let drag_session: Option<NoteDragSession> = {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.note_drag.clone()
        };
        let velocity_drag_session: Option<VelocityDragSession> = {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.velocity_drag.clone()
        };
        // note_create overlay 用 clone と release 用 take。
        let note_create_session: Option<NoteCreateSession> = {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.note_create
        };
        let note_create_release: Option<NoteCreateSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.note_create.take()
        } else {
            None
        };
        // (M14 Phase 69 / daw_01 #041) loop_drag overlay & release 用 clone / take。
        let loop_drag_session: Option<LoopDragSession> = {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.loop_drag
        };
        let loop_drag_release: Option<LoopDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.loop_drag.take()
        } else {
            None
        };
        // playhead_drag は release frame で take して discard (commit-by-release 無し)。
        if pointer.primary_just_released {
            let state: &mut PianoRollState = ui.widget_state(wid);
            let _ = state.playhead_drag.take();
        }
        // drag release で取り出すが、drag 距離が 16px 未満なら **click に格下げ** する
        // (= 短い「press → release」は note 中央上の click として selection 切替に振り向ける)。
        let drag_release_raw: Option<NoteDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.note_drag.take()
        } else {
            None
        };
        // (M14 Phase 64 / daw_01 #018) velocity_drag release: drag<3px は「click 単発 = no-op」
        // として扱い SetVelocity 発行しない。 後段の commit ブロックで dist 判定 + Edit 発行。
        let velocity_drag_release: Option<VelocityDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = ui.widget_state(wid);
            state.velocity_drag.take()
        } else {
            None
        };
        // 短 click 化時は session の careful-update modifier (`last_ctrl` / `last_shift`) も
        // 一緒に持ち回る — release frame の `pointer.modifiers` 生読みは「ModifiersChanged が
        // Released より先に届く」 race で Ctrl/Shift+click が Single に化ける
        // (arrangement の `clip_short_click_pos` と同 idiom、 r.md #35)。
        #[allow(clippy::type_complexity)]
        let (drag_release, drag_short_click_pos): (
            Option<NoteDragSession>,
            Option<((f32, f32), bool, bool)>,
        ) =
            if let Some(nd) = drag_release_raw {
                // dist 判定 / delta 計算は両者とも `nd.last_mouse` を真値とする (pointer.pos の
                // winit-bug 化を上の continuation block で吸収済み)。 click 短縮は pointer.pos
                // ではなく last_mouse 基準。
                // 短 click 化 (drag → click 格下げ) の閾値は **mouse jitter を ignore する程度** (4px)。
                //   - Resize (Left/Right) は常に commit (resize handle 上 click は意味なし)
                //   - Move + Alt なしのみ jitter 閾値で短 click 化 (click=selection / drag=移動 の区別)
                //   - Alt 押下中は Move でも閾値 skip (raw 微調整の明示意図)
                let dist = (nd.last_mouse.0 - nd.anchor_mouse.0).abs()
                    + (nd.last_mouse.1 - nd.anchor_mouse.1).abs();
                let is_move = matches!(nd.kind, NoteDragKind::Move);
                let demote = is_move && !nd.last_alt && dist < 4.0;
                if demote {
                    (None, pointer.pos.map(|p| (p, nd.last_ctrl, nd.last_shift)))
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        // drag 中の delta (pointer から計算)。beat_delta は f64、pitch_delta は i32 (Highlight/Linear:
        // 半音単位、 Fold: scale degree 単位 — `apply_pitch_drag_delta` で吸収)。
        // M9 Phase 45f: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用 (相対関係維持)。 alt 押下で snap 一時無効化。
        // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない
        // (release frame の commit と必ず同一値で確定するため)。
        let drag_overlay: Option<(NoteDragSession, f64, i32)> = drag_session
            .as_ref()
            .and_then(|nd| pointer.pos.map(|p| (nd.clone(), p)))
            .map(|(nd, (px, py))| {
                let dx = px - nd.anchor_mouse.0;
                let dy = py - nd.anchor_mouse.1;
                let raw = f64::from(dx) * beat_per_px;
                // 絶対位置 snap (詳細は `compute_note_drag_beat_delta` 参照、 arrangement と同パターン)。
                let beat_delta = compute_note_drag_beat_delta(
                    &nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                let pitch_delta = compute_pitch_drag_delta(view, grid, dy);
                (nd, beat_delta, pitch_delta)
            });

        // ----- Response 初期値 + hover 計算 -----
        let mut response = PianoRollResponse {
            hovered: pointer
                .pos
                .is_some_and(|(px, py)| grid.contains(px, py)),
            ..Default::default()
        };
        if let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
            && let Some((hover_id, hover_kind)) =
                note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
        {
            response.hovered_note_id = Some(hover_id);
            response.hovered_zone = Some(hover_kind);
        }
        response.dragging = drag_session.as_ref().map(|nd| nd.kind);
        response.velocity_dragging = velocity_drag_session.is_some();
        // 作成 session 中は creating=true (caller の wheel 無効化用)。
        response.creating = note_create_session.is_some();
        // (M14 Phase 84 / daw_01 #055) 鍵盤レーン press 中の pitch を held-value で返す。
        // session 中 (press 開始が kbd) かつ まだ押下中 (primary_pressed) かつ pointer が kbd 内の
        // ときだけ Some。release frame は primary_pressed=false で None (= note-off)、kbd 外への drag
        // も None。pitch は毎フレーム pointer.y から計算するので glissando に追従する。
        if ui.widget_state::<PianoRollState>(wid).keyboard_pressing
            && pointer.primary_pressed
            && let Some((px, py)) = pointer.pos
            && kbd.contains(px, py)
        {
            response.keyboard_active_pitch =
                Some(RowGeometry::compute(view, grid).y_to_pitch(py));
        }

        // hover 中の cursor 形状要求 (drag 中は drag kind、note hover (拡張範囲含む) は
        // hover_cursor)。要求しなかったフレームは `Ui` が自動で Default に戻すので
        // (daw_01 r.md #50 の per-frame セマンティクス)、明示 reset は不要。
        if response.creating {
            // 作成中は右端を伸ばす操作なので EwResize (resize と同じ)。
            ui.set_cursor(CursorIcon::EwResize);
        } else if response.dragging.is_some() {
            let cursor = match response.dragging {
                Some(NoteDragKind::Move) => CursorIcon::Move,
                Some(NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight) => {
                    CursorIcon::EwResize
                }
                None => CursorIcon::Default,
            };
            ui.set_cursor(cursor);
        } else if let Some((cx, cy)) = pointer.pos
            && let Some(cursor) =
                note_hover_cursor(visible, view, grid, cx, cy, style.resize_handle_px)
        {
            ui.set_cursor(cursor);
        }

        // ----- M14 Phase 125 (#102): plain-drag marquee gate (空き grid press を marquee が所有) -----
        // 旧設計は rect-select 起動に Shift 必須だったが、 空き grid を無修飾 drag → 範囲選択にする
        // (標準 DAW 慣習)。 note 上の plain drag は移動のまま。 修飾は release 時の next 計算で
        // plain=REPLACE / Shift=UNION / Ctrl=XOR に分岐。 gate を **pending_click 計算の前** で評価して
        // `marquee_active` を作り、 空き click clear (pending_click) が marquee の zero-rect REPLACE と
        // 同フレーム二重 emit するのを防ぐ (空き clear は下の :2219 で marquee :2380 より先に消費される
        // ため、 前方 bool での抑制が必須 — daw_01 #102「二重 emit 抑制」)。 `note_hit().is_none()` は
        // load-bearing: note MOVE は !shift gate なので hit-test 無しだと Shift+note press が誤って marquee
        // 起動する。 Alt は除外、 `note_drag` が press 時 None (= 真の空き press) を要求。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                ui.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let marquee_press = if !editing_mode
            && pointer.primary_just_pressed
            && !pointer.modifiers.alt
            // この press が「ダブルクリック作成」 のものなら marquee を起動しない
            // (作成 session が press を所有。 二重所有を防ぐ load-bearing gate)。
            && note_create_press.is_none()
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
            && note_hit(notes, view, grid, px, py, style.resize_handle_px).is_none()
        {
            let s: &PianoRollState = ui.widget_state(wid);
            s.note_drag.is_none()
        } else {
            false
        };
        let marquee_active = marquee_press || shift_rect_active;

        // ----- pending click 判定 -----
        // 2 通り: (a) drag が起こらなかった pure release、(b) drag は始まったが <16px で
        // click に格下げされた release。どちらも grid 上の click として selection 切替の
        // trigger に使う。M14 Phase 59: editing_mode 中は click を発火しない。
        // (M14 Phase 64 / daw_01 #018) velocity_drag_release 中も click 扱いしない (drag<3px no-op
        // でも selection を変えない / 通常 release は SetVelocity 発行で完結)。
        // (M14 Phase 64) vel_area / ruler / keyboard 等 grid 外の release は selection に影響させない
        // = `grid.contains(pos)` で gate (旧: 無条件 release で grid 外なら selection clear する
        // latent bug を修正)。 grid 内の空白 release は従来どおり selection clear。
        // (r.md #35) modifier も一緒に持つ: `((x, y), ctrl, shift)`。 note 上の click は drag session
        // の careful-update 値、 空白 release は生読み (drag session が無いので race の余地がない)。
        #[allow(clippy::type_complexity)]
        let pending_click: Option<((f32, f32), bool, bool)> = if editing_mode
            || drag_release.is_some()
            || velocity_drag_release.is_some()
            || marquee_active
            // 作成 release frame は Add で新規 note を選択するので、 ここで
            // 空白 click 扱いして selection clear を emit しない (二重 emit 抑制)。
            || note_create_release.is_some()
        {
            // #102: marquee がこの空き grid press を所有する frame は marquee 側が zero-rect REPLACE で
            // clear する。 ここで pending_click を立てると同フレーム二重 emit になるため None。
            None
        } else if let Some(p) = drag_short_click_pos {
            Some(p)
        } else if pointer.primary_just_released
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
        {
            Some(((px, py), pointer.modifiers.ctrl, pointer.modifiers.shift))
        } else {
            None
        };

        // ----- 描画 (heavy ブロック + cached + 動的 overlay) -----
        // M9 Phase 45c: viewport_key に vel_h を追加 (velocity lane 高さ変化で cache 無効化)。
        // M13 Phase 55: ruler_h / bpm / time_sig を追加 + v2 に bump (cache 構造変化)。
        // tuple Hash impl は 12 要素まで → bpm + time_sig を 1 つの組に纏めて 12 要素に収める。
        // M14 Phase 61b (#011): note 個別の (id, start_beat, len_beats, pitch, velocity, lyric)
        // 変化を widget 側で hash して 2 要素 outer tuple に wrap + v3 に bump (arrangement の
        // clip drag 残像と同根の予防、 caller の notes_generation は note 数や編集 epoch のみで
        // 不十分なケースを吸収)。
        let internal_note_hash = fold_piano_roll_note_hash(visible);
        // (M14 Phase 70 / daw_01 #042) `view.scale` を hash に含めて、 scale 切替 (root / mask / mode)
        // が起きたとき cache invalidate されるようにする。 None は (0, 0, 0) で表現 (= scale OFF
        // が連続するときは同じ hash 寄与で cache hit、 None ↔ Some の遷移時は差分が出る)。
        // (M14 Phase 70 / daw_01 #042) scale 切替 (root / mask / mode) で cache invalidate。
        // (M14 Phase 70b / daw_01 #042 follow-up) snap_pitch_during_drag toggle も cache key に
        // 含める (= drag preview の経路は cached 内には含まれないが、 future-proof と test 容易さ
        // のため含める判断、 cost は u8 1 byte 増加のみ)。
        let scale_key = view.scale.map_or((0_u8, 0_u16, 0_u8), |sc| {
            let mode_tag: u8 = match sc.mode {
                PianoRollScaleMode::Highlight => 1,
                PianoRollScaleMode::Fold => 2,
            };
            (sc.root, sc.in_scale_mask, mode_tag)
        });
        let snap_drag_key: u8 = u8::from(view.snap_pitch_during_drag);
        // (M14 Phase 124 / daw_01 #100) subdivision 間隔を cache key に含める。 cached() は
        // viewport_key 一致時に内側 (bar_beat_grid 含む) を完全 skip するので、 bar_beat_grid 内の
        // input_hash だけでは足りず、 ここで invalidate 経路を張る必要がある。 None=0 / Some=bits。
        let sub_grid_key: u64 = view.sub_grid_interval_beats.map_or(0, f64::to_bits);
        // S4c: `PianoRollView.notes_generation` を撤去。cached 層が依存する note 内容 + 表示状態
        // (dimmed / locked / track color / mute) は `internal_note_hash` (fold_piano_roll_note_hash)
        // が全て覆うので、 view 側の世代 hook は不要 (correct-by-construction)。tuple 構造変更で v5 へ。
        let viewport_key = (
            (
                b"piano_roll_widget_v5" as &[u8],
                view.start_beat.to_bits(),
                view.len_beats.to_bits(),
                view.pitch_top.to_bits(),
                view.pitch_visible.to_bits(),
                grid.w.to_bits(),
                grid.h.to_bits(),
                kbd.w.to_bits(),
                vel_h.to_bits(),
                ruler_h.to_bits(),
                (
                    view.bpm.to_bits(),
                    u32::from(view.time_sig.0),
                    u32::from(view.time_sig.1),
                    scale_key,
                    snap_drag_key,
                    sub_grid_key,
                ),
            ),
            internal_note_hash,
            // cached primitives は絶対座標で再生されるため widget 位置も key に
            // 含める — 「サイズ不変で位置だけ動く」 layout 変化で旧座標に描かれる
            // のを防ぐ (arrangement viewport_key と同 class の同件)。
            (grid.x.to_bits(), grid.y.to_bits()),
        );

        // M13 Phase 55: library `time_ruler` / `bar_beat_grid` を呼ぶための共通 mapping。
        // beat 単位 view を sample 単位 ViewportState1D に変換 (sample_rate = 48k は BarBeat
        // 表示で比例定数として打ち消されるダミー)。
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: f64::from(view.bpm.max(1.0)),
            time_sig: (view.time_sig.0.max(1), view.time_sig.1.max(1)),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let sample_viewport =
            ViewportState1D::new(view.start_beat * spb, view.len_beats.max(1e-6) * spb);
        let grid_style_pr = BarBeatGridStyle {
            bar_color: style.bar_line,
            beat_color: style.beat_line,
            bar_line_width: style.bar_line_width_px,
            beat_line_width: style.beat_line_width_px,
            // M14 Phase 63m (daw_01 #027): zoom 連動の beat 線間引き (default 4px)。
            ..BarBeatGridStyle::from_palette(p)
        };
        // M14 Phase 124 (#100): 3 段目 subdivision。 caller が拍間隔を渡したときだけ構築
        // (ズーム退避は bar_beat_grid 内の px_per_interval 判定に委ねる)。 cache 無効化は
        // viewport_key に interval を含めて行う (下記、 cached が viewport_key で short-circuit
        // するため bar_beat_grid 内の input_hash だけでは効かない)。
        let sub_grid_pr: Option<SubGridSpec> = view.sub_grid_interval_beats.and_then(|iv| {
            (iv > 0.0).then_some(SubGridSpec {
                interval_beats: iv,
                color: style.sub_line,
                line_width: style.sub_line_width_px,
            })
        });
        let ruler_style_pr = TimeRulerStyle {
            bg: style.ruler_bg,
            tick_color: style.bar_line,
            label_color: style.ruler_label_color,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
            // M14 Phase 63m (daw_01 #027): zoom 連動の label / beat tick 間引き (default 60 / 4 px)。
            ..TimeRulerStyle::from_palette(p)
        };
        let id_for_inner: u64 = hash_inputs(id);

        let visible_owned: Vec<Note> = visible.to_vec();
        let style_copy = *style;
        let view_copy = view;
        // selected は heavy 内 borrow 不可なので Vec を所有権渡しで closure に取り込む
        let selected_set: HashSet<NoteId> = selected.iter().copied().collect();
        let drag_overlay_clone = drag_overlay.clone();
        let lyric_editing_for_draw = lyric_editing;
        // M9 Phase 45f: drag overlay の Resize min_len は snap unit に合わせる
        // (snap_unit < 0.05 なら 0.05)。 release 側 min_len と同じ計算で一貫性確保。 alt 真値は
        // drag session の `last_alt` (overlay と release commit が必ず同一 unit で確定する)。
        // overlay 不在時 (drag していない) は min_len 自体使われないので alt = false で適当に初期化。
        let drag_overlay_alt = drag_overlay.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
        let drag_overlay_min_len: f64 = if view.snap.is_active(drag_overlay_alt) {
            view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
        } else {
            0.05
        };
        // (M14 Phase 64 / daw_01 #018) velocity drag preview: drag 中なら target_ids の bar を
        // current pointer.y → 絶対 velocity の値で描画 override。 None のときは note.velocity 通常描画。
        // velocity_drag は press 時に vel_area.h > 0 を gate してあるため vel_area.h > 0 が前提。
        let velocity_drag_overlay: Option<(Vec<NoteId>, u8)> =
            velocity_drag_session.as_ref().map(|vd| {
                let new_vel = velocity_from_y(vd.last_mouse.1, vel_area);
                (vd.target_ids.clone(), new_vel)
            });

        // (M14 Phase 69 / daw_01 #041) loop drag overlay の preview range も snap 適用済
        // (commit と同一値で確定、 release 時の「カクッ」 ずれを回避)。 alt は session の `last_alt`
        // を真値とし、 `pointer.modifiers.alt` を直接見ない (clip_drag / loop_drag in arrangement と
        // 同 pattern)。
        let loop_drag_preview_range: Option<(f64, f64)> = loop_drag_session.map(|ld| {
            let cur_beat =
                view.start_beat + f64::from(ld.last_mouse_x - ruler.x) * beat_per_px;
            compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat)
        });
        let loop_band_color = style.loop_band;
        let loop_handle_color = style.loop_handle;
        let loop_handle_w = style.loop_handle_w;

        // note_create preview: 作成中 note の rect (drag preview と同じ helper で
        // 長さ確定値を計算 → grid clamp)。session 不在なら None。色は resize ghost (selected) と同じ。
        let note_create_preview: Option<Rect> = note_create_session.map(|nc| {
            let (start_beat, len_beats, pitch) =
                note_create_geometry(&nc, view, beat_per_px, zoom_x_px_per_beat);
            note_geometry_to_rect(start_beat, len_beats, pitch, view, grid)
        });

        ui.heavy(("piano_roll_inner", &id), move |hctx| {
            // === cached(): viewport_key 一致時に skip される背景レイヤ ===
            hctx.cached(viewport_key, |hctx| {
                draw_grid_background(hctx, grid, kbd, view_copy, &style_copy);
                hctx.ui_mut().bar_beat_grid(
                    ("pr_grid", id_for_inner),
                    grid,
                    mapping,
                    sample_viewport,
                    grid_style_pr,
                    sub_grid_pr,
                );
                if ruler_h > 0.0 {
                    hctx.ui_mut().time_ruler(
                        ("pr_ruler", id_for_inner),
                        ruler,
                        mapping,
                        sample_viewport,
                        ruler_style_pr,
                    );
                }
                draw_notes(
                    hctx,
                    &visible_owned,
                    view_copy,
                    grid,
                    style_copy.velocity_ramp,
                    style_copy.bg,
                    style_copy.note_border_radius_px,
                    style_copy.note_muted_hatch_color,
                    style_copy.note_muted_hatch_spacing_px,
                    style_copy.note_muted_hatch_width_px,
                );
            });

            // === cached の外: 動的 overlay (selection / velocity lane / drag preview / lyric / cursor / playhead) ===
            // (M14 Phase 64 / daw_01 #018) velocity lane は cached の外に移動。 drag preview の
            // override velocity を毎 frame 反映するため (drag 中はバー高さが pointer.y で変わる)。
            // 静的時は visible 数 ≤ ~100 なので毎 frame 描画でも負荷は軽微 (rect command ~100 個)、
            // model 更新時の cache 無効化を待たずに即時反映するメリットが上回る。
            if vel_h > 0.0 {
                draw_velocity_lane(
                    hctx,
                    &visible_owned,
                    view_copy,
                    vel_area,
                    &style_copy,
                    velocity_drag_overlay.as_ref().map(|(ids, v)| (ids.as_slice(), *v)),
                );
            }
            // selection overlay (note の上、lyric の下)
            if !selected_set.is_empty() {
                draw_selection_overlay(
                    hctx,
                    &visible_owned,
                    &selected_set,
                    view_copy,
                    grid,
                    &style_copy,
                );
            }
            // drag preview (drag 中の shifted rect)
            if let Some((nd, bd, pd)) = drag_overlay_clone {
                draw_drag_preview(
                    hctx,
                    &nd,
                    view_copy,
                    grid,
                    &style_copy,
                    bd,
                    pd,
                    drag_overlay_min_len,
                );
            }
            // note_create preview (作成中の note を selection ghost 色で描画)。
            if let Some(r) = note_create_preview {
                let x_left = r.x.max(grid.x);
                let x_right = (r.x + r.w).min(grid.x + grid.w);
                let y_top = r.y.max(grid.y);
                let y_bot = (r.y + r.h).min(grid.y + grid.h);
                if x_right > x_left && y_bot > y_top {
                    hctx.push_rect(RectCommand {
                        rect: Rect {
                            x: x_left,
                            y: y_top,
                            w: x_right - x_left,
                            h: y_bot - y_top,
                        },
                        fill: style_copy.note_selected_fill,
                        border: style_copy.note_selected_border,
                        border_width: style_copy.note_selected_border_w,
                        radius: [style_copy.note_border_radius_px; 4],
                        clip_rect: None,
                    });
                }
            }
            // M14 Phase 59: lyric 描画 (selection overlay より後 = 黄色 fill に隠れない、
            // 編集中 note は text_input overlay に譲る)。 font_size は note 高さスケール。
            draw_lyrics(
                hctx,
                &visible_owned,
                view_copy,
                grid,
                style_copy.lyric_color,
                style_copy.lyric_font_px,
                lyric_editing_for_draw,
            );
            // (M14 Phase 69 / daw_01 #041) loop band overlay (ruler 上、 cached の外で毎 frame 描画)。
            // drag preview range があれば preview を、 無ければ `view.loop_range` を描画。 ruler_h <= 0
            // のときは `ruler.h = 0` なので描画 helper 内で band_w = 0 となり no-op (= 旧 API 互換)。
            // arrangement と完全同 helper (`crate::widgets::ruler_ops::draw_loop_band`)、 daw_01 が
            // ruler_h > 0 + loop_range Some を渡したときのみ表示される。
            if ruler_h > 0.0
                && let Some(range) = loop_drag_preview_range.or(view_copy.loop_range)
            {
                crate::widgets::ruler_ops::draw_loop_band(
                    hctx,
                    range,
                    view_copy.start_beat,
                    view_copy.len_beats,
                    ruler,
                    loop_band_color,
                    loop_handle_color,
                    loop_handle_w,
                );
            }
            // M9 Phase 45c: playhead 線 (time で動くので cache 対象外、毎フレーム描画)。
            // 範囲外なら描画スキップ。grid と vel_area を縦断する 1 本。
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(grid.w) / view_copy.len_beats.max(1e-6);
                let x = grid.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                let y_top = grid.y;
                let y_bottom = vel_area.y + vel_area.h;
                draw_playhead_line(
                    hctx,
                    x,
                    y_top,
                    y_bottom,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }
        });

        // ----- shortcut: Insert (note 追加) / Delete (selected 削除) -----
        // M14 Phase 59: editing_mode 中は global shortcut が typing_focus で抑制される
        // ため take_shortcut は false を返すはずだが、defensive で明示 guard。
        if !editing_mode
            && ui.take_shortcut("add_note")
            && let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
        {
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            let raw_start = (view.start_beat + f64::from(cx - grid.x) / beat_to_px).max(0.0);
            // M9 Phase 45f: Insert は widget 内発火、grid 吸着が UX 自然 (#010 [Replied])。
            // single frame の click なので drag state は関与せず、 直接 `pointer.modifiers.alt` を読む。
            let start_beat = view
                .snap
                .snap_beat(raw_start, pointer.modifiers.alt, zoom_x_px_per_beat)
                .max(0.0);
            // M14 Phase 70 / daw_01 #042: RowGeometry 経由で Fold mode も対応 (Fold では
            // y_to_pitch_f が在 row index → in-scale pitch を返すので、 ceil で確実に in-scale)。
            let geom = RowGeometry::compute(view, grid);
            let pitch_f = geom.y_to_pitch_f(cy);
            // M14 Phase 61d (#012): 描画式 `y = grid.y + (pitch_top - pitch) * pitch_to_px` の
            // 逆関数として ceil() を使う (pitch P の視覚行 y ∈ [(top-P)*pt, (top-P+1)*pt) なので
            // 逆引きは pitch_f ∈ (P-1, P] のとき P を返す = ceil)。 round() だと判定領域が視覚行
            // に対して半行ぶん上にずれて、 行の下半分にカーソルがあると 1 つ下のピッチに化ける。
            // (Fold mode では y_to_pitch_f が既に in-scale pitch を整数で返すので ceil は no-op)。
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pitch = (pitch_f.ceil() as i32).clamp(0, 127) as u8;
            // 長さは caller の既定長 (= last_note_duration_beats) に統一。下限 1/16。widget は
            // song-absolute → model は clip-local (clip_origin_beat を引く)。id は handler が採番。
            let insert_len = view.default_note_len_beats.max(0.0625);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AddNote {
                    track: target.track,
                    clip: target.clip,
                    start_beat: start_beat - clip_origin_beat,
                    duration: insert_len,
                    pitch,
                });
            }));
        }

        if !editing_mode && ui.take_shortcut("delete") && !selected.is_empty() {
            let sel_set: HashSet<NoteId> = selected.iter().copied().collect();
            let ids: Vec<u32> = notes.iter().filter(|n| sel_set.contains(&n.id)).map(|n| n.id).collect();
            if !ids.is_empty() {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                }));
            }
        }

        // ----- pending click → selection 切替 (Edit 発行のみ、外部 selected は frame 末で apply 後に反映) -----
        if let Some(((cx, cy), click_ctrl, click_shift)) = pending_click {
            let prev: Vec<NoteId> = selected.to_vec();
            // r.md #35: note click も全選択面共通の `SelectModifier` に統一
            // (`docs/plan_selection_modifiers.md` §3)。 無修飾 = Single / Ctrl = Toggle /
            // Shift = RangeFromAnchor (= 音程 × 時間の長方形ブロック)。
            // 旧実装は shift しか見ておらず Ctrl が素通りして **Ctrl+click が無条件置換** になり、
            // Shift+click は 3 経路すべてで弾かれて **無反応** だった。
            //
            // アンカーは `SelectionState.note_anchor` が所有する (SSoT)。 clip / track と同じく
            // Edit closure 内で apply 時に読む。
            let hit = if grid.contains(cx, cy) {
                note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
            } else {
                None
            };
            let modifier = SelectModifier::from_modifiers(click_shift, click_ctrl);
            if let Some((hit_id, _)) = hit {
                // 範囲用の全 note (行 = pitch / 時間 = start〜end)。 lock された clip の note は
                // marquee と同じく範囲選択からも除外する (`note_hit` も locked を弾くので
                // hit 自身が locked になることはない)。 表は Shift のときだけ組む (無修飾 /
                // Ctrl の click ごとに全 note を走査するのは無駄)。
                let items: Vec<RangeItem<NoteId>> =
                    if modifier == SelectModifier::RangeFromAnchor {
                        notes
                            .iter()
                            .filter(|n| !n.style.locked)
                            .map(|n| RangeItem {
                                key: n.id,
                                row: i64::from(n.pitch),
                                start: n.start_beat,
                                end: n.start_beat + n.len_beats,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let anchor = app.selection.note_anchor;
                    let next = modifier
                        .resolve(&prev, hit_id, || range_block(&items, anchor?, hit_id));
                    if next != prev {
                        app.handle_event(AppEvent::SetNoteSelection(next));
                    }
                    // アンカー更新: Single / Toggle で clicked へ、 Range は据え置き (§3.1)。
                    // `SetNoteSelection` はアンカーを触らないので順序依存は無い。
                    if modifier.updates_anchor() {
                        app.selection.note_anchor = Some(hit_id);
                    }
                }));
                response.selection_changed = true;
            } else if !prev.is_empty() {
                // grid の空白 click → 選択クリア + アンカー破棄 (旧挙動と同じ)。
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteSelection(Vec::new()));
                    app.selection.note_anchor = None;
                }));
                response.selection_changed = true;
            }
            // grid 内の short click なら beat/pitch も Response に載せる
            if grid.contains(cx, cy) {
                let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
                let beat = view.start_beat + f64::from(cx - grid.x) / beat_to_px;
                // M14 Phase 70 / daw_01 #042: Fold mode 中も「視覚的な行 → in-scale pitch」 で返す。
                let pitch = RowGeometry::compute(view, grid).y_to_pitch_f(cy);
                response.clicked_at_beat_pitch = Some((beat, pitch));
            }
        }

        // ----- drag release → Move / Resize Edit 発行 -----
        // M9 Phase 60: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用。 Resize の min_len は snap unit に合わせる (snap_unit < 0.05 なら 0.05)。
        // **alt は drag 中の最終 `nd.last_alt` を真値とする** — release frame の `pointer.modifiers.alt`
        // は OS event 順序 (ModifiersChanged が MouseInput(Released) より先に届く) によって false に
        // 化けることがあるため信用しない。 `last_alt` は continuation frame で更新され release frame
        // では `allow_update = false` で保持されるので OS event 順序に依存しない。 overlay の snap
        // 判定とも同一値で確定し、 「release で grid に飛ぶ」 不整合が起きない。
        if let Some(nd) = drag_release {
            let release_alt = nd.last_alt;
            // pointer.pos に頼らず `nd.last_mouse` を使う (winit release frame で pointer.pos が
            // press 位置に戻る既存問題、 arrangement と同パターン)。
            let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
            let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
            let raw = f64::from(dx) * beat_per_px;
            // 絶対位置 snap (overlay と一貫)。
            let beat_delta =
                compute_note_drag_beat_delta(&nd, raw, &view.snap, zoom_x_px_per_beat);
            // pitch も overlay と同一 helper で確定する。 Fold mode では 1 行 =
            // 1 scale degree なので、 ここだけ半音換算 (dy × pitch_per_px) にすると
            // ghost で見た位置と別の pitch に commit してしまう。
            let pitch_delta = compute_pitch_drag_delta(view, grid, dy);
            let min_len = if view.snap.is_active(release_alt) {
                view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };

            match nd.kind {
                NoteDragKind::Move => {
                    let mut deltas: Vec<MoveDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        // M14 Phase 70b / daw_01 #042 follow-up: release commit も `nd.last_alt` を
                        // 渡して overlay と完全同 helper 経由。 alt で snap 無効も両者一致。
                        let new_pitch =
                            apply_pitch_drag_delta(a.pitch, pitch_delta, view, release_alt);
                        if (new_start - a.start_beat).abs() > 1e-6 || new_pitch != a.pitch
                        {
                            deltas.push((a.id, a.start_beat, a.pitch, new_start, new_pitch));
                        }
                    }
                    if !deltas.is_empty() {
                        // d.3 = next_start_beat は song-absolute → 各 note の所属クリップ (clip_slot =
                        // packed id 上位 8bit) の clip-local へ。d.0 は packed のまま handler が decode する。
                        let entries: Vec<(u32, f64, u8)> = deltas
                            .iter()
                            .map(|d| {
                                let off = clip_starts
                                    .get(AppData::note_id_clip_slot(d.0))
                                    .copied()
                                    .unwrap_or(0.0);
                                (d.0, d.3 - off, d.4)
                            })
                            .collect();
                        // M14 Phase 83 / daw_01 #054: Ctrl 保持なら複製 (元据え置き = CopyNotes)、
                        // そうでなければ移動 (SetNotePositions)。 `nd.last_ctrl` は overlay と同じ
                        // careful-update 値なので copy ghost を見て release した結果と必ず一致する。
                        let copy = nd.last_ctrl;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            if copy {
                                app.handle_event(AppEvent::CopyNotes(entries.clone()));
                            } else {
                                app.handle_event(AppEvent::SetNotePositions(entries.clone()));
                            }
                        }));
                    }
                }
                NoteDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(min_len);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                a.start_beat,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        let entries: Vec<(u32, f64, f64)> = deltas
                            .iter()
                            .map(|d| {
                                let off = clip_starts
                                    .get(AppData::note_id_clip_slot(d.0))
                                    .copied()
                                    .unwrap_or(0.0);
                                (d.0, d.3 - off, d.4)
                            })
                            .collect();
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::ResizeNotes(entries.clone()));
                        }));
                    }
                }
                NoteDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual_delta = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual_delta).max(min_len);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                new_start,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        let entries: Vec<(u32, f64, f64)> = deltas
                            .iter()
                            .map(|d| {
                                let off = clip_starts
                                    .get(AppData::note_id_clip_slot(d.0))
                                    .copied()
                                    .unwrap_or(0.0);
                                (d.0, d.3 - off, d.4)
                            })
                            .collect();
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::ResizeNotes(entries.clone()));
                        }));
                    }
                }
            }
        }

        // ----- velocity drag release → SetVelocity Edit 発行 (M14 Phase 64 / daw_01 #018) -----
        // drag<3px は no-op (誤操作防止)。 release frame では last_mouse を真値とする
        // (note_drag と同パターン: winit が release frame で pointer.pos を press 位置に巻き戻す対策)。
        // 絶対値 mode: pointer.y から `velocity_from_y` で 0..=127 計算 → 全 target に同じ値を set。
        // anchor velocity と一致する note は updates から除外 (no-op Edit を avoid)。
        if let Some(vd) = velocity_drag_release {
            let dx = vd.last_mouse.0 - vd.anchor_mouse.0;
            let dy = vd.last_mouse.1 - vd.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            if dist >= 3.0 {
                let new_vel = velocity_from_y(vd.last_mouse.1, vel_area);
                let mut updates: Vec<VelocityUpdate> = Vec::new();
                for (id, anchor_vel) in &vd.anchor_velocities {
                    if *anchor_vel != new_vel {
                        updates.push((*id, new_vel));
                    }
                }
                if !updates.is_empty() {
                    let entries: Vec<(u32, u8)> = updates.into_iter().collect();
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetNoteVelocities(entries.clone()));
                    }));
                }
            }
        }

        // ----- note_create release → Add 発行 (作成 + 長さ確定を 1 undo step に) -----
        // overlay と同じ `note_create_geometry` で長さを確定 (描画と commit の一致)。 ドラッグせず
        // 即放しなら既定長、 ドラッグしていれば pointer 追従長。 id=0 placeholder (caller が採番)。
        // daw_01 は `n.len_beats` を尊重して AddNote { duration } に変換する (旧: last_note_duration_beats
        // 固定だったのを #82 で n.len_beats へ)。 pitch は press 時に確定済み。
        if let Some(nc) = note_create_release {
            let (start_beat, len_beats, pitch) =
                note_create_geometry(&nc, view, beat_per_px, zoom_x_px_per_beat);
            // widget が決めた長さ (即放し=既定長 / ドラッグ=ドラッグ長) を尊重する。id は handler が採番。
            // song-absolute → model は clip-local (clip_origin_beat を引く)。
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AddNote {
                    track: target.track,
                    clip: target.clip,
                    start_beat: start_beat - clip_origin_beat,
                    duration: len_beats,
                    pitch,
                });
            }));
            // 入力完了後、 press 時に既定長ノートの右端へ warp した
            // カーソルを元のクリック位置へ戻す (warp しっぱなしだと「ノートの右端のまま」
            // 残り、 次操作の起点が分かりにくいという要望)。 warp は y を変えない
            // (press 時 `warp_cursor(warp_x, py)`) ので、 復帰先 y は anchor_mouse.1
            // (= press y) をそのまま再利用する (press_y を別フィールドで複製しない = SSoT)。
            ui.warp_cursor(nc.press_x, nc.anchor_mouse.1);
        }

        // ----- loop drag release → SetLoopRange (M14 Phase 69 / daw_01 #041) -----
        // snap 適用済 endpoints を overlay と共通の helper で計算 (release frame で grid に飛ぶ
        // 不整合を構造的に回避、 arrangement #024 と同 idiom)。 alt は `ld.last_alt` を真値とし、
        // release frame の `pointer.modifiers.alt` を直接見ない (ModifiersChanged 先行 race 回避)。
        if let Some(ld) = loop_drag_release {
            let cur_beat =
                view.start_beat + f64::from(ld.last_mouse_x - ruler.x) * beat_per_px;
            let (start, end) =
                compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLoopRange { start, end });
            }));
        }

        // ----- M14 Phase 125 (#102): marquee commit (plain=REPLACE / Shift=UNION / Ctrl=XOR) -----
        // gate `marquee_active` / `drag_rect_wid` は pending_click 計算の前で算出済 (空き grid press のみ)。
        // `take_drag_rect_in_rect` は呼ぶだけで cyan 半透明 overlay を自動描画し、 press 時 modifier を
        // `DragRect.modifiers` に snapshot する。 release frame (`drag.finished`) に inside を集めて修飾で
        // next を分岐 (`sort_unstable` 後に `prev != next` で no-op 抑制)。 REPLACE は inside そのまま
        // (zero-rect → 空 = 選択 clear)。 editing_mode 中は marquee_press が false なので走らない。
        //
        // r.md #35: **右 drag** の marquee も同じ commit を通す。 左 drag が空き grid 専用なのに対し、
        // 右 drag は `grid` 全域 = **note の上からでも** 起動できる (REAPER 既定と同じ配置。
        // 右ボタンなので note の move / resize と衝突しない)。 動かさずに離した右ボタンは
        // context menu 側が拾い、 ここには来ない。 `DragRectState` を共有しないよう id を分ける。
        let left_marquee = if !editing_mode && marquee_active {
            ui.take_drag_rect_in_rect(drag_rect_wid, grid)
        } else {
            None
        };
        // 右ボタンを **動かさずに** 離したフレームは「右クリック = context menu」 なので、
        // 0 サイズ矩形の REPLACE で選択を消してしまわないよう commit を捨てる
        // (session 自体は take して state を畳む必要があるので呼び出しは行う)。
        let secondary_was_click = ui.pending_secondary_click_pos().is_some();
        let right_marquee = if editing_mode {
            None
        } else {
            ui.take_secondary_drag_rect_in_rect(wid.child(b"rect_select_rmb"), grid)
                .filter(|d| !(d.finished && secondary_was_click))
        };
        for drag in [left_marquee, right_marquee].into_iter().flatten() {
            response.rect_select_active = true;
            if drag.finished {
                let drag_rect = drag.rect();
                let mut inside: Vec<NoteId> = Vec::new();
                for n in visible {
                    // lock クリップの note は marquee 矩形選択からも除外。
                    if n.style.locked {
                        continue;
                    }
                    let r = note_to_rect(n, view, grid);
                    if rects_intersect(r, drag_rect) {
                        inside.push(n.id);
                    }
                }
                let prev: Vec<NoteId> = selected.to_vec();
                let mut next: Vec<NoteId> = if drag.modifiers.shift {
                    // UNION: prev に inside の新規だけ append。
                    let mut out = prev.clone();
                    for id in &inside {
                        if !out.contains(id) {
                            out.push(*id);
                        }
                    }
                    out
                } else if drag.modifiers.ctrl {
                    // XOR: prev に在って inside にも在る id を除き、 inside の新規を追加。
                    let mut out: Vec<NoteId> =
                        prev.iter().copied().filter(|id| !inside.contains(id)).collect();
                    for id in &inside {
                        if !prev.contains(id) {
                            out.push(*id);
                        }
                    }
                    out
                } else {
                    inside // REPLACE (zero-rect → 空 = clear)
                };
                next.sort_unstable();
                let mut prev_sorted = prev.clone();
                prev_sorted.sort_unstable();
                if prev_sorted != next {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetNoteSelection(next.clone()));
                    }));
                    response.selection_changed = true;
                }
            }
        }

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 overlay (text_input + commit dispatch) =====
        // lyric_editing が Some なら、編集対象 note の rect 内に text_input を重ね描きし、
        // Enter / NumpadEnter で commit text を `split_into_morae` で分割 → 後続 note へ
        // 1 SetLyrics Edit (1 undo) で分配。Esc は text_input が focus clear → 次 frame で
        // resp.focused == false 検出 → lyric_editing = None (2 frame で UX 完了)。
        if let Some(edit_id) = lyric_editing {
            // borrow conflict 回避: 必要なデータを先にコピーしてから ui.text_input を呼ぶ。
            let edit_data = notes.iter().find(|n| n.id == edit_id).map(|n| {
                let raw_rect = note_to_rect(n, view, grid);
                let prefill = n.lyric.as_deref().unwrap_or("").to_string();
                (raw_rect, prefill)
            });
            if let Some((raw_rect, prefill)) = edit_data {
                // grid 内に clip (note rect が grid 外にはみ出している場合)
                let clipped_x = raw_rect.x.max(grid.x);
                let clipped_y = raw_rect.y.max(grid.y);
                let clipped_w = (raw_rect.x + raw_rect.w).min(grid.x + grid.w) - clipped_x;
                let clipped_h = (raw_rect.y + raw_rect.h).min(grid.y + grid.h) - clipped_y;
                // M14 Phase 59: text_input overlay の最小表示サイズ (8 px)。 旧 `style.lyric_font_px`
                // を threshold にしていたが、 lyric_font_px が MAX cap になったため固定値に変更
                // (text_input は font_size 14 px 既定で 8 px 高あれば最低限読める)。
                if clipped_w < 8.0 || clipped_h < 8.0 {
                    // 表示できないほど小さい (zoom out 過多 etc) → 編集モード解除
                    ui.widget_state::<PianoRollState>(wid).lyric_editing = None;
                    lyric_editing = None;
                } else {
                    let clipped = Rect {
                        x: clipped_x,
                        y: clipped_y,
                        w: clipped_w,
                        h: clipped_h,
                    };
                    // text_input_at_focused: id に edit_id を含めることで note 切替時に
                    // widget id が変化 → was_widget_visible_last_frame == false → 自動 focus +
                    // 全選択 (gained_focus 検知経由)。
                    let resp = ui.text_input_at_focused(
                        ("piano_roll_lyric", edit_id),
                        clipped,
                        &prefill,
                        // on_change は per-keystroke で呼ばれるが、ここでは何もしない
                        // (commit 検出で 1 度だけ SetLyrics 発行 = 1 undo)。
                        |_new_text| Edit::mutate(|_: &mut AppData| {}),
                    );

                    // daw_01 #112「テキスト入力は focus loss で確定」: Enter (committed) と
                    // 外 click (blurred) のどちらでも歌詞を確定する。 違いは確定後の遷移で、
                    // Enter は分配先の次 note へ編集を継続、 外 click はその場で編集終了。
                    // Esc (= committed/blurred でない focus loss) のみ破棄。
                    if resp.committed || resp.blurred {
                        let committed_text = resp.committed_text.unwrap_or_default();
                        let morae: Vec<String> = if committed_text.is_empty() {
                            // 空文字 commit → 起点 note の歌詞を None に (= 削除)
                            Vec::new()
                        } else {
                            common::voicevox::split_into_morae(&committed_text)
                        };
                        // 起点 note の歌詞 update count: 空入力は 1 (起点を None に)、
                        // それ以外は morae.len() 個分の連続 note を取る。
                        let target_count = morae.len().max(1);
                        let target_ids =
                            collect_next_notes_for_lyric(notes, edit_id, target_count);
                        let mut updates: Vec<(NoteId, Option<String>)> =
                            Vec::with_capacity(target_ids.len());
                        for (i, nid) in target_ids.iter().enumerate() {
                            let lyric = morae.get(i).cloned().filter(|s| !s.is_empty());
                            updates.push((*nid, lyric));
                        }
                        // 余り (overflow) を Response に載せる (note 数 < 入力モーラ数の場合)。
                        response.lyric_overflow_morae =
                            morae.len().saturating_sub(target_ids.len());
                        if !updates.is_empty() {
                            // widget は編集対象 clip を context として知らないので、描画中の
                            // `target` (ClipRef) を capture して渡す (歌詞分配は 1 batch = 1 undo)。
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetNoteLyrics {
                                    clip_ref: target,
                                    lyrics: updates.clone(),
                                });
                            }));
                        }
                        if resp.committed {
                            // Enter: 分配し終わった先の note へ移動して編集継続。
                            let all_sorted =
                                collect_next_notes_for_lyric(notes, edit_id, usize::MAX);
                            let next_id = all_sorted.get(target_ids.len()).copied();
                            ui.widget_state::<PianoRollState>(wid).lyric_editing = next_id;
                            lyric_editing = next_id;
                            // selection も自動追従 (daw_01 UI が同期、note 強調が次 note へ)
                            if let Some(nid) = next_id
                                && selected != [nid].as_slice()
                            {
                                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                    app.handle_event(AppEvent::SetNoteSelection(vec![nid]));
                                }));
                                response.selection_changed = true;
                            }
                        } else {
                            // 外 click (blur): 現 note の歌詞を確定して編集終了 (次 note へは進まない)。
                            ui.widget_state::<PianoRollState>(wid).lyric_editing = None;
                            lyric_editing = None;
                        }
                    } else if !resp.focused {
                        // Esc 検出: text_input が clear_focus_if_focused →
                        // 次 frame で resp.focused = false かつ committed/blurred でない。破棄。
                        ui.widget_state::<PianoRollState>(wid).lyric_editing = None;
                        lyric_editing = None;
                    }
                }
            } else {
                // defensive: notes に edit_id が無い (フレーム頭の sync check で本来 None
                // にしているので通常起こらない)
                ui.widget_state::<PianoRollState>(wid).lyric_editing = None;
                lyric_editing = None;
            }
        }
        response.lyric_editing = lyric_editing;

        // ===== 旧 piano_roll_view::draw の widget 呼び出し後ロジックを吸収 =====

        // 複数表示時のみ右側に凡例パネル (色 swatch / クリップ名 / 対象切替 / ロックトグル)。
        if let Some(legend_rect) = legend_rect {
            draw_legend(app, ui, legend_rect, &shown, target);
        }

        // 歌詞 inline 編集中フラグを app に mirror。dispatch_shortcuts が piano_roll より前に走って
        // take_shortcut("escape") を消費してしまうため、編集中は app 側フラグを見て Esc を widget に委ねる。
        // 変化したフレームだけ Edit を発行 (毎フレーム push を避ける)。
        let lyric_editing_now = response.lyric_editing.is_some();
        if lyric_editing_now != app.ui_ephemeral.piano_roll_lyric_editing {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.piano_roll_lyric_editing = lyric_editing_now;
            }));
        }

        // gui_01 #055: 鍵盤レーン click のピッチプレビュー。前フレーム値 (recording.preview_note の
        // pitch) と差分し、変化した frame だけ PreviewPitchChanged を発火。鳴らす track は描画中 clip の track。
        if response.keyboard_active_pitch != app.recording.preview_note.map(|(_, p)| p) {
            let track_idx = target.track;
            let pitch = response.keyboard_active_pitch;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::PreviewPitchChanged { track_idx, pitch });
            }));
        }

        // ノート paste の配置位置。grid 上のポインタを **clip-local** snapped beat にして毎フレーム
        // mirror。view.start_beat は song-absolute なので clip_origin_beat を引いて clip-local に変換 (grid 外は None)。
        let hover_beat: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
            if !grid.contains(px, py) {
                return None;
            }
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            let beat_raw = view.start_beat + f64::from(px - grid.x) / beat_to_px;
            let cfg = snap::piano_roll_snap_config(app);
            let alt = ui.pointer().modifiers.alt;
            let snapped = cfg.snap_beat(beat_raw, alt, app.pianoroll_zoom_x());
            Some(snapped - clip_origin_beat)
        });
        // f キー用に **song-absolute かつ snap なし** の生 beat も mirror (clip_origin_beat を引く前)。
        let hover_beat_song_raw: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
            if !grid.contains(px, py) {
                return None;
            }
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            Some(view.start_beat + f64::from(px - grid.x) / beat_to_px)
        });
        if app.ui_ephemeral.pianoroll_hover_beat != hover_beat
            || app.ui_ephemeral.pianoroll_hover_beat_song_raw != hover_beat_song_raw
        {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.pianoroll_hover_beat = hover_beat;
                app.ui_ephemeral.pianoroll_hover_beat_song_raw = hover_beat_song_raw;
            }));
        }
        // q キー用に、ポインタ直下の note id (packed、selected_notes と同空間) を毎フレーム mirror。
        let hover_note: Option<u32> = ui.pointer().pos.and_then(|(px, py)| {
            note_hit(notes, view, grid, px, py, style.resize_handle_px).map(|(id, _)| id)
        });
        if app.ui_ephemeral.pianoroll_hover_note != hover_note {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.pianoroll_hover_note = hover_note;
            }));
        }

        // wheel handler — note drag / 作成中は無効。Ctrl=横ズーム, Alt=縦ズーム, Shift=横スクロール,
        // plain=ピッチスクロール (Ableton Live / Reaper 流)。
        // (daw_01 #34) grid だけでなく鍵盤レーン (kbd) 上でも wheel を効かせる。kbd は grid と
        // 同じ y 範囲・高さ (view_build のレイアウト SSoT) なので、 縦 (pitch) スクロール / 縦ズーム
        // の anchor は py がそのまま grid 座標系で正しい。 横ズーム (Ctrl) の anchor x のみ kbd 上では
        // px < grid.x で負値化するため grid 内に clamp して左端 (view start) 基準ズームにする。
        if response.dragging.is_none() && !response.creating {
            let pointer = ui.pointer();
            if let Some((px, py)) = pointer.pos
                && (grid.contains(px, py) || kbd.contains(px, py))
            {
                let (sx, sy) = pointer.scroll_delta;
                if sy.abs() > 0.001 || sx.abs() > 0.001 {
                    let scroll_beat = app.pianoroll_scroll_beat();
                    let top_pitch = i32::from(app.pianoroll_top_pitch());
                    let modifiers = pointer.modifiers;
                    if modifiers.ctrl {
                        // Ctrl+wheel: 横ズーム。マウス位置の拍を anchor として保持。
                        // kbd 上 (px < grid.x) は anchor を grid 左端に clamp (負の拍 anchor を回避)。
                        let anchor_px = px.clamp(grid.x, grid.x + grid.w);
                        let factor = (sy * 0.005).exp();
                        let new_zoom = (zoom_x * factor).clamp(8.0, 400.0);
                        if (new_zoom - zoom_x).abs() > 1e-3 {
                            let anchor_beat =
                                f64::from(scroll_beat) + f64::from((anchor_px - grid.x) / zoom_x);
                            #[allow(clippy::cast_possible_truncation)]
                            let new_scroll =
                                (anchor_beat - f64::from((anchor_px - grid.x) / new_zoom)).max(0.0) as f32;
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetPianoRollZoomX(new_zoom));
                                app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                            }));
                        }
                    } else if modifiers.alt {
                        // Alt+wheel: 縦ズーム。マウス位置のピッチを anchor として保持。
                        let factor = (sy * 0.005).exp();
                        let new_zoom = (zoom_y * factor).clamp(6.0, 40.0);
                        if (new_zoom - zoom_y).abs() > 1e-3 {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let anchor_pitch =
                                f32::from(top_pitch as u8) - (py - grid.y) / zoom_y;
                            let new_top_f = anchor_pitch + (py - grid.y) / new_zoom;
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let new_top = new_top_f.round().clamp(11.0, 127.0) as u8;
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetPianoRollZoomY(new_zoom));
                                app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                            }));
                        }
                    } else if modifiers.shift {
                        let dx_beats = -(sx + sy) / zoom_x;
                        let new_scroll = (scroll_beat + dx_beats).max(0.0);
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                        }));
                    } else {
                        #[allow(clippy::cast_possible_truncation)]
                        let delta = (sy / 12.0).round() as i32;
                        if delta != 0 {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let new_top = (top_pitch + delta).clamp(11, 127) as u8;
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                            }));
                        }
                    }
                }
            }
        }

        response
    }

// ============================================================
// 内部ハッシュ (cache 無効化キー)
// ============================================================

/// arrangement と同根。 caller の世代 hook では個別 note の `(id, start_beat, len_beats, pitch,
/// velocity, style)` 変化が漏れて drag 残像 / stale な dim・lock・色が発生する。 widget 内部で
/// 全 visible note を fold して viewport_key に追加する (旧 view 側 `notes_generation` を代替)。
///
/// fold するのは **cached 層 (`draw_notes`) が実際に描画へ使う field だけ**。 `lyric` は含めない
/// (歌詞は cache の外の `draw_lyrics` が毎フレーム描画するので key に不要)。
fn fold_piano_roll_note_hash(notes: &[Note]) -> u64 {
    const PRIME: u64 = 0x100_0000_01B3; // FNV-1a 64bit prime
    let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64bit offset basis
    for n in notes {
        h ^= u64::from(n.id);
        h = h.wrapping_mul(PRIME);
        h ^= n.start_beat.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= n.len_beats.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.pitch);
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.velocity);
        h = h.wrapping_mul(PRIME);
        // mute / dim / lock / クリップ色は cached 層の note fill 描画の入力なので fold する
        // (トグルや対象クリップ切替が scroll 等の別 invalidation を待たず即時反映されるように)。
        h ^= u64::from(n.muted)
            | (u64::from(n.style.dimmed) << 1)
            | (u64::from(n.style.locked) << 2);
        h = h.wrapping_mul(PRIME);
        if let Some(c) = n.style.color {
            h ^= (u64::from(c.r.to_bits()) << 32) | u64::from(c.g.to_bits());
            h = h.wrapping_mul(PRIME);
            h ^= (u64::from(c.b.to_bits()) << 32) | u64::from(c.a.to_bits());
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

// ============================================================
// Snap toolbar / 複数表示 legend (旧 piano_roll_view から吸収)
// ============================================================

/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle][snap unit dropdown][Fit] [Fold][Snap on Draw]。
/// Fold / Snap on Draw は Phase 7 B5 (Scale &amp; Root)。
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &*app.theme.core;
    let toggle_style = snap_toggle_style(p);
    ui.panel("pr_toolbar_bg", rect, p.header, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;
    let fold_w = 50.0;
    let snap_draw_w = 100.0;

    let mut x = rect.x + pad;

    ui.toggle_button_at(
        "pr_snap_toggle",
        "Snap",
        Rect { x, y, w: toggle_w, h },
        app.ui_prefs.pianoroll_snap_enabled,
        &toggle_style,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetPianoRollSnapEnabled(new));
            })
        },
    );
    x += toggle_w + pad;

    if let Some(idx) = ui.dropdown(
        "pr_snap_unit",
        Rect { x, y, w: dropdown_w, h },
        SNAP_LABELS,
        app.ui_prefs.pianoroll_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetPianoRollSnapChoice(new));
        }));
    }
    x += dropdown_w + pad;

    ui.button_at("pr_fit", "Fit", Rect { x, y, w: fit_w, h }, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitPianoRollToClip);
        })
    });
    x += fit_w + pad * 2.0;

    // Phase 7 B5 (`docs/plan_scale.html` §4.4): Fold to Scale toggle。
    // ON で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)。
    // Song.scale_changes が空のときも toggle 自体は active 化できるが、
    // PianoRollView.scale = None なので visual には影響しない。
    ui.toggle_button_at(
        "pr_fold_to_scale",
        "Fold",
        Rect { x, y, w: fold_w, h },
        app.ui_prefs.piano_roll_fold,
        &toggle_style,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleFoldToScale);
            })
        },
    );
    x += fold_w + pad;

    // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。
    // ON で note 追加時の pitch を Song.scale_at(beat).snap(pitch) で
    // in-scale に寄せる (Highlight mode 前提、 Fold mode は widget 側で
    // 既に in-scale)。
    ui.toggle_button_at(
        "pr_snap_on_draw",
        "Snap Draw",
        Rect { x, y, w: snap_draw_w, h },
        app.ui_prefs.snap_on_draw,
        &toggle_style,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSnapOnDraw);
            })
        },
    );
}

/// 複数表示時に右側へ出す凡例パネル。各行 = **1 トラック** = [色 swatch][トラック名][L ロック]。
/// トラック行クリックで対象 (target) をそのトラックの (表示中) クリップへ切替、L トグルでそのトラックの
/// ロック (参照専用) を反転。対象トラックの行は左端 accent バー + 通常文字色で強調 (非対象は淡色)。
/// ノート色・dim もトラック基準なので整合する (REAPER / Cakewalk のトラックペイン流)。`target` は
/// 描画中の対象クリップ (その `.track` が対象トラック)。
fn draw_legend(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    shown: &[ClipRef],
    target: ClipRef,
) {
    let p = &*app.theme.core;
    let toggle_style = snap_toggle_style(p);
    ui.panel("pr_legend_bg", rect, p.header, 0.0);
    let pad = 6.0;
    let row_h = 28.0;
    let gap = 4.0;
    // パネル見出し (表示トラック)。
    ui.label_at(
        "pr_legend_title",
        "\u{8868}\u{793a}\u{30c8}\u{30e9}\u{30c3}\u{30af}",
        rect.x + pad,
        rect.y + pad,
        11.0,
        p.text_dim,
    );
    // 表示中クリップが乗っている **トラック** を初出順に列挙 (同じトラックの複数クリップは 1 行)。
    let mut track_indices: Vec<u32> = Vec::new();
    for &r in shown {
        if !track_indices.contains(&r.track) {
            track_indices.push(r.track);
        }
    }
    // 行は縦スクロール可能にする。 旧実装は viewport に入らない行を無言で break して
    // いたため、 6 トラック以上に跨るクリップを同時選択すると 6 行目以降が描画も
    // ヒットテストもされず、 そのトラックの対象切替 / L ロックが操作不能になっていた。
    let list_rect = Rect {
        x: rect.x,
        y: rect.y + pad + 18.0,
        w: rect.w,
        h: (rect.h - pad - 18.0).max(0.0),
    };
    let content_h = track_indices.len() as f32 * (row_h + gap);
    let scrollbar_w = if content_h > list_rect.h { 10.0 } else { 0.0 };
    ui.scroll_area("pr_legend_scroll", list_rect, (list_rect.w, content_h), |ui, scroll_off| {
        let mut y = list_rect.y - scroll_off.1;
        for (row_i, &ti) in track_indices.iter().enumerate() {
            let Some(track) = app.song_doc.song().tracks.get(ti as usize) else {
                continue;
            };
            let track_id = track.id;
            let is_target = ti == target.track;
            let locked = app.is_pianoroll_track_locked(track_id);
            // 対象切替先 = このトラックの代表クリップ (anchor がこのトラックなら anchor、 でなければ
            // このトラックの最初の表示クリップ)。target_clip は anchor なので legend 切替で動く。
            let rep_key = if ti == target.track {
                app.clip_key_of(target)
            } else {
                shown
                    .iter()
                    .copied()
                    .find(|r| r.track == ti)
                    .and_then(|r| app.clip_key_of(r))
            };
            let row = Rect {
                x: rect.x + pad,
                y,
                w: (rect.w - pad * 2.0 - scrollbar_w).max(0.0),
                h: row_h,
            };
            // 行背景 (対象トラック = accent wash で薄く強調)。
            ui.panel(
                ("pr_legend_row", row_i),
                row,
                if is_target {
                    p.accent_wash
                } else {
                    p.panel_raised
                },
                4.0,
            );
            // 対象トラック行の左端 accent バー。
            if is_target {
                ui.push_rect(RectCommand::uniform_radius(
                    Rect { x: row.x, y: row.y, w: 3.0, h: row.h },
                    p.accent,
                    1.5,
                ));
            }
            // 色 swatch (= トラック実効色)。
            let color = track_color::to_renderer(track_color::effective_track_color(track));
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: row.x + 8.0,
                    y: row.y + (row.h - 13.0) * 0.5,
                    w: 13.0,
                    h: 13.0,
                },
                fill: color,
                border: p.border,
                border_width: 1.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
            // ロックトグル (右端、トラック単位)。
            let lock_w = 26.0;
            let lock_rect = Rect {
                x: row.x + row.w - lock_w - 4.0,
                y: row.y + 4.0,
                w: lock_w,
                h: row.h - 8.0,
            };
            ui.toggle_button_at(
                ("pr_legend_lock", row_i),
                "L",
                lock_rect,
                locked,
                &toggle_style,
                move |_| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::TogglePianoRollTrackLock(track_id));
                    })
                },
            );
            // トラック名 (クリック = 対象トラック切替)。swatch と lock の間の領域。
            let name_x = row.x + 26.0;
            let name_rect = Rect {
                x: name_x,
                y: row.y,
                w: (lock_rect.x - name_x - 4.0).max(10.0),
                h: row.h,
            };
            // 透明ヒット (空テキストの button) でクリックを拾い、テキストは下で label 描画する
            // (button の中央寄せ固定文字でなく ellipsis 付き左寄せラベルを出すため)。
            if ui.button_at_clicked_sized_aligned(
                ("pr_legend_name", row_i),
                "",
                name_rect,
                12.0,
                ButtonTextAlign::Left,
            ) && let Some(k) = rep_key
            {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetPianoRollTargetClip(k));
                }));
            }
            let label = track.name.clone();
            // 行背景は panel_raised / accent_wash (パレット自身のクローム面) なので、
            // 極性固定インクではなく通常の本文色でよい。
            let text_color = if is_target { p.text } else { p.text_dim };
            let label_rect = Rect {
                x: name_rect.x + 4.0,
                y: name_rect.y + (name_rect.h - 12.0) * 0.5,
                w: (name_rect.w - 8.0).max(8.0),
                h: 14.0,
            };
            ui.label_at_clipped(("pr_legend_name_label", row_i), &label, label_rect, 12.0, text_color);
            y += row_h + gap;
        }
    });
}
