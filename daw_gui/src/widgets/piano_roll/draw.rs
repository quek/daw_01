//! S4c Phase B-E: piano_roll widget の描画 helper 群 (heavy/cached 内の背景・note・
//! 歌詞・selection overlay・drag preview・velocity lane・鍵盤ラベル)。
//! `HeavyCtx` は `<M: ?Sized>` 汎用のまま (描画は Model 型に依存しない = arrangement と同idiom)。
//! 型・pure helper は `use super::*` で親から継承する。

#![allow(clippy::too_many_arguments)]

use super::*;

use crate::theme::Palette;

/// note の最終 fill を決める (color/dim/lock/mute を統合)。`draw_notes` と
/// `draw_velocity_lane` が共有して描画一致を保証する。`bg` は dim の寄せ先 (grid 背景)。
#[must_use]
pub(super) fn note_fill_color(note: &Note, velocity_ramp: VelocityRamp, bg: Color) -> Color {
    let base = match note.style.color {
        Some(c) => shade_by_velocity(c, note.velocity),
        None => velocity_ramp.fill(note.velocity),
    };
    // lock は dim より強く沈める (参照専用を明示)、次いで非対象 dimmed。
    let base = if note.style.locked {
        dim_toward(base, bg, 0.72)
    } else if note.style.dimmed {
        dim_toward(base, bg, 0.48)
    } else {
        base
    };
    if note.muted {
        daw_ui_core::widgets::muted_dim_fill(base)
    } else {
        base
    }
}

/// note rect を生成 (角丸 + radius 指定)。
pub(super) fn note_rect_command(rect: Rect, fill: Color, radius_px: f32) -> RectCommand {
    RectCommand {
        rect,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [radius_px; 4],
        clip_rect: None,
    }
}

// ============================================================
// Internal drawing helpers
// ============================================================

/// M14 Phase 117 (daw_01 #093): 鍵盤オクターブラベルの色を、 その行の **実効背景** から決める。
/// `key_fill` (白鍵 / 黒鍵 fill) の上に `overlay` (root_row_overlay / out overlay、 無ければ `None`) を
/// alpha 合成した「実際に目に入る色」 を [`Palette::ink_for`] に渡し、 明背景なら暗インク・暗背景なら
/// 明インクを得る。 鍵盤 fill は白鍵 / 黒鍵 / scale overlay で **可変** なので、 ここは `p.text` では
/// なく極性固定インク (r.md #48)。 `label_auto_contrast == false` なら `fallback` をそのまま返す。
pub(super) fn keyboard_label_color(
    p: &Palette,
    style: &PianoRollStyle,
    key_fill: Color,
    overlay: Option<Color>,
    fallback: Color,
) -> Color {
    if !style.label_auto_contrast {
        return fallback;
    }
    let bg = match overlay {
        Some(ov) => daw_ui_core::color::composite_over(ov, key_fill),
        None => key_fill,
    };
    p.ink_for(bg)
}

/// r.md #73: **歌詞はノートの塗りの上に乗る標識**なので、色は塗りから決める。
///
/// ノートの塗りは `note_fill_color` = ユーザー着色 × ベロシティ × dim / lock で、
/// locked は背景側へ 0.72 寄せるので暗くなる。 固定の暗インク (旧 `style.lyric_color`
/// = `ink_on_bright`) だと暗いノートの上で歌詞が読めない。 VOICEVOX 歌唱がこの
/// プロジェクトの中心機能なので、ここが沈むと実害が大きい
/// (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
///
/// 鍵盤ラベルと同じ `label_auto_contrast` で opt-out できる (どちらも「ラベルは
/// 下地に追従する」という 1 つの規則)。 opt-out 時は従来の固定色。
#[must_use]
pub(super) fn lyric_color_for(p: &Palette, style: &PianoRollStyle, note_fill: Color) -> Color {
    if !style.label_auto_contrast {
        return style.lyric_color;
    }
    p.ink_for(note_fill)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::too_many_lines)]
pub(super) fn draw_grid_background<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    grid: Rect,
    kbd: Rect,
    view: PianoRollView,
    style: &PianoRollStyle,
) {
    // 鍵盤ラベルの極性判定に使う (戻りは host の `'a` 借用なので push_rect と衝突しない)。
    let p = hctx.palette();

    // (a) 主領域背景
    hctx.push_rect(RectCommand {
        rect: grid,
        fill: style.bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    let geom = RowGeometry::compute(view, grid);
    let scale = view.scale;
    let root_pc_opt = geom.root_pc();

    // Per-row 背景 (黒鍵 overlay + scale overlay 3rd pass)。 mode で iter 方法が変わる。
    if geom.fold {
        // (b') Fold: in-scale 行のみ、 等高 row_h
        for (idx, &pitch) in geom.fold_rows.iter().enumerate() {
            let y = grid.y + idx as f32 * geom.row_h;
            // 黒鍵 row overlay (in-scale 行のうち黒鍵 = root が黒鍵 / pentatonic 等で発生)
            if is_black_key(pitch) {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.black_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
            // root row overlay
            if Some(pitch % 12) == root_pc_opt {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.root_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
        }
    } else {
        // (b) Linear (None / Highlight): 12 行 1 octave、 旧 iteration
        let pitch_top_int = view.pitch_top.floor() as i32;
        let pitch_visible_int = view.pitch_visible.ceil() as i32;
        for i in 0..=pitch_visible_int {
            let pitch_i = pitch_top_int - i;
            if !(0..=127).contains(&pitch_i) {
                continue;
            }
            let pitch = pitch_i as u8;
            let y = grid.y + (view.pitch_top - pitch_i as f32) * geom.row_h;
            // (b-1) 黒鍵 row overlay
            if is_black_key(pitch) {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.black_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
            // (b-2) M14 Phase 70 / daw_01 #042: scale overlay 3rd pass (Highlight mode)
            if let Some(sc) = scale {
                let pc = pitch % 12;
                if pc == sc.root % 12 {
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                        fill: style.root_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                } else if !sc.is_in_scale(pitch) {
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                        fill: style.out_of_scale_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }
            }
        }
    }

    // (c) 拍縦線 (1 拍ごと細線、bar 縦線) — M13 Phase 55 で library `Ui::bar_beat_grid` に統合。
    // この関数の caller (piano_roll cached layer) で `hctx.bar_beat_grid` を呼ぶ。

    // (d) keyboard widget (左端、kbd.w > 0 のみ)
    //
    // 以下の鍵盤描画はすべて `clip_rect: Some(kbd)` で鍵盤矩形に閉じる。 Linear
    // 分岐は `0..=ceil(pitch_visible)` を回すので最終行の rect が必ず kbd 下端を
    // 1 行分 (row_h = zoom_y、 最大 40px) はみ出し、 その下の「鍵盤の真下 ×
    // velocity レーンの左」 = どの pass も塗らないガターに白鍵 / 黒鍵の帯と
    // C ラベルが残っていた。
    if kbd.w == 0.0 {
        return;
    }
    // 背景
    hctx.push_rect(RectCommand {
        rect: kbd,
        fill: style.keyboard_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: Some(kbd),
    });

    if geom.fold {
        // Fold: in-scale 行のみ、 全行にラベル
        for (idx, &pitch) in geom.fold_rows.iter().enumerate() {
            let y = grid.y + idx as f32 * geom.row_h;
            let key_rect = Rect {
                x: kbd.x,
                y,
                w: (kbd.w - 1.0).max(0.0),
                h: (geom.row_h - 1.0).max(0.0),
            };
            let fill =
                if is_black_key(pitch) { style.black_key } else { style.white_key };
            hctx.push_rect(RectCommand {
                rect: key_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(kbd),
            });
            // root row overlay
            if Some(pitch % 12) == root_pc_opt {
                hctx.push_rect(RectCommand {
                    rect: key_rect,
                    fill: style.root_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(kbd),
                });
            }
            // Label: 全 in-scale 行に
            if geom.row_h >= 8.0 {
                let pc = pitch % 12;
                let name = pitch_class_name_spelled(pc, scale.is_some_and(|s| s.prefer_flats));
                let is_root = Some(pc) == root_pc_opt;
                let (text, fallback) = if is_root {
                    let octave = (i32::from(pitch) / 12) - 1;
                    (format!("{name}{octave}"), style.root_label_fg)
                } else {
                    (name.to_string(), style.in_scale_label_fg)
                };
                // M14 Phase 117 (daw_01 #093): root 行は root_row_overlay 重畳、 in-scale 行は key fill のみ。
                // 実効背景の輝度で dark/light を選ぶ (白鍵 in-scale 行で明文字が潰れる旧 symptom も解消)。
                let overlay = if is_root { Some(style.root_row_overlay) } else { None };
                let color = keyboard_label_color(p, style, fill, overlay, fallback);
                hctx.push_text(GlyphArea {
                    text: text.into(),
                    left: kbd.x + 4.0,
                    top: y,
                    font_size: style.c_label_font_px,
                    line_height: style.c_label_font_px * 1.2,
                    color,
                    clip_rect: Some(kbd),
                    ..GlyphArea::default()
                });
            }
        }
    } else {
        // Linear (None / Highlight)
        let pitch_top_int = view.pitch_top.floor() as i32;
        let pitch_visible_int = view.pitch_visible.ceil() as i32;
        for i in 0..=pitch_visible_int {
            let pitch_i = pitch_top_int - i;
            if !(0..=127).contains(&pitch_i) {
                continue;
            }
            let pitch = pitch_i as u8;
            let y = grid.y + (view.pitch_top - pitch_i as f32) * geom.row_h;
            let key_rect = Rect {
                x: kbd.x,
                y,
                w: (kbd.w - 1.0).max(0.0),
                h: (geom.row_h - 1.0).max(0.0),
            };
            let fill =
                if is_black_key(pitch) { style.black_key } else { style.white_key };
            hctx.push_rect(RectCommand {
                rect: key_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(kbd),
            });
            // scale overlay (Highlight)
            if let Some(sc) = scale {
                let pc = pitch % 12;
                if pc == sc.root % 12 {
                    hctx.push_rect(RectCommand {
                        rect: key_rect,
                        fill: style.root_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: Some(kbd),
                    });
                } else if !sc.is_in_scale(pitch) {
                    hctx.push_rect(RectCommand {
                        rect: key_rect,
                        fill: style.out_of_scale_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: Some(kbd),
                    });
                }
            }
            // Label
            if geom.row_h >= 8.0 {
                if let Some(root_pc) = root_pc_opt {
                    // Highlight mode: root pitch class のオクターブだけ label
                    if pitch % 12 == root_pc {
                        let octave = (pitch_i / 12) - 1;
                        let name =
                            pitch_class_name_spelled(root_pc, scale.is_some_and(|s| s.prefer_flats));
                        // M14 Phase 117 (daw_01 #093): Highlight mode の root 行は常に root_row_overlay
                        // 重畳 (warm cream)。 その実効背景で auto-contrast → warm-on-warm 潰れを解消。
                        let color = keyboard_label_color(
                            p,
                            style,
                            fill,
                            Some(style.root_row_overlay),
                            style.root_label_fg,
                        );
                        hctx.push_text(GlyphArea {
                            text: format!("{name}{octave}").into(),
                            left: kbd.x + 4.0,
                            top: y,
                            font_size: style.c_label_font_px,
                            line_height: style.c_label_font_px * 1.2,
                            color,
                            clip_rect: Some(kbd),
                            ..GlyphArea::default()
                        });
                    }
                } else if pitch.is_multiple_of(12) {
                    // 旧挙動: C オクターブだけ
                    let octave = (pitch_i / 12) - 1;
                    // M14 Phase 117 (daw_01 #093): scale=None の C 行は overlay 無し (key fill のみ)。
                    // C は白鍵なので auto-contrast で暗文字が選ばれ、 旧 `c_label_color` (dark) と整合。
                    let color = keyboard_label_color(p, style, fill, None, style.c_label_color);
                    hctx.push_text(GlyphArea {
                        text: format!("C{octave}").into(),
                        left: kbd.x + 4.0,
                        top: y,
                        font_size: style.c_label_font_px,
                        line_height: style.c_label_font_px * 1.2,
                        color,
                        clip_rect: Some(kbd),
                        ..GlyphArea::default()
                    });
                }
            }
        }
    }
}

/// visible note を grid 内に clip して描画。M9 Phase 44c 以降 `lyric: Some(...)` が
/// あれば note rect の左端に重ねて描画する (note 高さが lyric font 1 行分以上あるときのみ)。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_notes<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    velocity_ramp: VelocityRamp,
    bg: Color,
    radius_px: f32,
    muted_hatch_color: Color,
    muted_hatch_spacing_px: f32,
    muted_hatch_width_px: f32,
) {
    for note in visible {
        let r = note_to_rect(note, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        let clipped = Rect {
            x: x_left,
            y: y_top,
            w: x_right - x_left,
            h: y_bot - y_top,
        };
        // クリップ色 (色 None なら velocity 色) → dim/lock 沈め → mute 沈め。
        let fill = note_fill_color(note, velocity_ramp, bg);
        hctx.push_rect(note_rect_command(clipped, fill, radius_px));
        if note.muted {
            daw_ui_core::widgets::push_muted_hatch(
                hctx,
                clipped,
                clipped,
                muted_hatch_color,
                muted_hatch_spacing_px,
                muted_hatch_width_px,
            );
        }
    }
}

/// (M14 Phase 59) note 上に歌詞を描画する独立 pass。 selection overlay / drag preview の
/// **後** に呼んで lyric を最前面に置く (旧設計では cached 内 draw_notes 内で描いていたため
/// selection の黄色 fill に覆われていた、 daw_01 #017 動作確認で発覚)。
///
/// font_size は note rect の高さに連動 (`note_h * 0.7` を `lyric_font_px_max` で cap)。
/// 縦方向中央寄せで note 内に収める。 lyric_editing 中の note は text_input overlay が
/// 出ているので skip (= 編集中歌詞は text_input 内で表示される)。
pub(super) fn draw_lyrics<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
    selected: &std::collections::HashSet<NoteId>,
    skip_note_id: Option<NoteId>,
) {
    // r.md #73: 歌詞の色は 1 音ごとに **その音の塗り** から決めるので palette が要る。
    // `HeavyCtx::palette` は host 寿命の参照なので、以降 `hctx` を可変で使っても衝突しない。
    let p = hctx.palette();
    let lyric_font_px_max = style.lyric_font_px;
    for note in visible {
        if Some(note.id) == skip_note_id {
            continue;
        }
        let Some(lyric) = note.lyric.as_ref() else {
            continue;
        };
        let r = note_to_rect(note, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        let clipped = Rect {
            x: x_left,
            y: y_top,
            w: x_right - x_left,
            h: y_bot - y_top,
        };
        // note 高さに比例した font (cap = lyric_font_px_max)、 最低 7px 以上で描画。
        // caller が lyric_font_px < 7.0 を style に設定した場合、 floor を cap まで下げて
        // `f32::clamp(min > max)` の panic を防ぐ (cap が勝つ動作)。
        let floor = 7.0_f32.min(lyric_font_px_max);
        let font_size = (clipped.h * 0.75).clamp(floor, lyric_font_px_max);
        if clipped.h < font_size + 1.0 || clipped.w < font_size {
            continue;
        }
        // 縦方向中央寄せ: top = clipped.y + (clipped.h - font_size) / 2
        let top = clipped.y + ((clipped.h - font_size) * 0.5).max(0.0);
        // r.md #73: 歌詞の下地は **このノートの塗り**。 選択中は selection overlay が
        // 上塗りされた後に歌詞を描くので、そちらが実効背景になる。
        let note_bg = if selected.contains(&note.id) {
            style.note_selected_fill
        } else {
            note_fill_color(note, style.velocity_ramp, style.bg)
        };
        hctx.push_text(GlyphArea {
            text: lyric.clone(),
            left: clipped.x + 2.0,
            top,
            font_size,
            line_height: font_size * 1.1,
            color: lyric_color_for(p, style, note_bg),
            clip_rect: Some(clipped),
            ..GlyphArea::default()
        });
    }
}

/// selected note に黄色ハイライト + 白枠 overlay を描画 (cached の外、毎フレーム)。
pub(super) fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    selected_set: &HashSet<NoteId>,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
) {
    for note in visible {
        if !selected_set.contains(&note.id) {
            continue;
        }
        let r = note_to_rect(note, view, grid);
        let pad = style.note_selected_pad_px;
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: r.x - pad,
                y: r.y - pad,
                w: r.w + pad * 2.0,
                h: r.h + pad * 2.0,
            },
            fill: style.note_selected_fill,
            border: style.note_selected_border,
            border_width: style.note_selected_border_w,
            radius: [3.0; 4],
            // grid で clip する — 他 pass (draw_notes / drag preview / lyrics) は
            // 全て clamp/clip 済みで、 ここだけ無 clip だと視界端の選択 note の
            // ハイライトが keyboard / ruler / velocity lane にはみ出す (review)。
            clip_rect: Some(grid),
        });
    }
}

/// drag 中の shifted note rect (drag preview) を描画。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    nd: &NoteDragSession,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
    beat_delta: f64,
    pitch_delta: i32,
    min_len: f64,
) {
    // M14 Phase 83 / daw_01 #054: Ctrl 保持の copy drag は ghost を clone 色 (緑系) で描き、
    // move drag (黄) と視覚区別する。`nd.last_ctrl` は release commit と同じ careful-update 値。
    let (ghost_fill, ghost_border) = if nd.last_ctrl {
        (style.note_clone_ghost_fill, style.note_clone_ghost_border)
    } else {
        (style.note_selected_fill, style.note_selected_border)
    };
    for a in &nd.anchors {
        let (start_beat, len_beats, pitch) =
            drag_preview_geometry(*a, nd.kind, beat_delta, pitch_delta, min_len, view, nd.last_alt);
        let r = note_geometry_to_rect(start_beat, len_beats, pitch, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: x_left,
                y: y_top,
                w: x_right - x_left,
                h: y_bot - y_top,
            },
            fill: ghost_fill,
            border: ghost_border,
            border_width: style.note_selected_border_w,
            radius: [style.note_border_radius_px; 4],
            clip_rect: None,
        });
    }
}

/// (M9 Phase 45c / M14 Phase 64) velocity lane の描画。`vel_area` は keyboard を除いた grid と同じ x 範囲。
/// 各 visible note の start_beat 位置に幅 `style.velocity_bar_width_px` の縦 bar を、
/// `velocity / 127` の比率で高さを決めて bottom-aligned で描画する。
///
/// (M14 Phase 64 / daw_01 #018) `velocity_override` が `Some((ids, new_vel))` のとき、
/// 含まれる id の note は `n.velocity` の代わりに `new_vel` で bar を描画する (drag preview)。
/// drag 中はこの override が active になり、release で None に戻る (= cache 経由で実値が反映)。
pub(super) fn draw_velocity_lane<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    vel_area: Rect,
    style: &PianoRollStyle,
    velocity_override: Option<(&[NoteId], u8)>,
) {
    // 背景塗り
    hctx.push_rect(RectCommand {
        rect: vel_area,
        fill: style.velocity_lane_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let beat_to_px = f64::from(vel_area.w) / view.len_beats.max(1e-6);
    let half_w = style.velocity_bar_width_px * 0.5;
    for n in visible {
        let vel = match velocity_override {
            Some((ids, ov)) if ids.contains(&n.id) => ov,
            _ => n.velocity,
        };
        let bar_h = vel_area.h * (f32::from(vel) / 127.0);
        if bar_h <= 0.0 {
            continue;
        }
        let cx = vel_area.x + ((n.start_beat - view.start_beat) * beat_to_px) as f32;
        // grid 範囲外は skip (visible は端に半分はみ出る note も含み得る)
        if cx + half_w < vel_area.x || cx - half_w > vel_area.x + vel_area.w {
            continue;
        }
        // bar はそのクリップの色 (色 None なら従来の velocity_bar_color)。
        // バー高さが既に velocity を表すので velocity 陰影は掛けず、dim/lock のみ反映。
        let bar_fill = match n.style.color {
            Some(c) if n.style.locked => dim_toward(c, style.velocity_lane_bg, 0.72),
            Some(c) if n.style.dimmed => dim_toward(c, style.velocity_lane_bg, 0.48),
            Some(c) => c,
            None => style.velocity_bar_color,
        };
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: cx - half_w,
                y: vel_area.y + vel_area.h - bar_h,
                w: style.velocity_bar_width_px,
                h: bar_h,
            },
            fill: bar_fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            // bar は cx を中心に ±half_w なので、 view 先頭 (start_beat == 拍 0) の
            // note では左半分がレーン外 = 鍵盤の真下の未描画ガターへ出る。
            // selection overlay (clip_rect: Some(grid)) と同じ SSoT でレーンに閉じる。
            clip_rect: Some(vel_area),
        });
    }
}

/// **時間範囲の帯**をピアノロールに描く (`docs/plan_range_selection.md` §4)。
///
/// アレンジャーと同じ見た目 — 半透明の明色で塗り、左右端に縦線。 ただし「レーン」 は
/// **鍵盤の行** (Live の "key track") なので、範囲に入っている鍵盤の行だけを塗る。
/// ノートより手前に重ねるので、範囲がノートを部分的に覆っているときも境目が見える。
pub(super) fn draw_time_range_overlay<M: ?Sized + 'static>(
    hctx: &mut daw_ui_core::widgets::heavy::HeavyCtx<'_, '_, M>,
    grid: Rect,
    view: PianoRollView,
    range: (f64, f64),
    pitches: &[u8],
    style: &PianoRollStyle,
) {
    if pitches.is_empty() {
        return;
    }
    let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
    let x0 = (grid.x + ((range.0 - view.start_beat) * beat_to_px) as f32).max(grid.x);
    let x1 = (grid.x + ((range.1 - view.start_beat) * beat_to_px) as f32).min(grid.x + grid.w);
    if x1 <= x0 {
        return;
    }
    let geom = RowGeometry::compute(view, grid);
    for pitch in pitches {
        let (y, h) = geom.pitch_to_y_and_h(*pitch);
        // 行の実効高 (fold では圧縮される) ぶんを塗る。 grid の外は clip する。
        let y0 = y.max(grid.y);
        let y1 = (y + h).min(grid.y + grid.h);
        if y1 <= y0 {
            continue;
        }
        hctx.push_rect(note_rect_command(
            Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 },
            style.time_range_fill,
            0.0,
        ));
    }
    // 左右端の縦線は grid の縦いっぱいに 1 本ずつ (どこからどこまでが範囲かを示す)。
    for x in [x0, x1 - 1.0] {
        hctx.push_rect(note_rect_command(
            Rect { x, y: grid.y, w: 1.0, h: grid.h },
            style.time_range_edge,
            0.0,
        ));
    }
}
