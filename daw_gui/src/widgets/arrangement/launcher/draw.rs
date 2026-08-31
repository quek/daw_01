//! ランチャー帯の描画と、caller 向け rect の収集。
//!
//! **cached を張らない。** 帯に出るセルは「見えている行 × 見えている列」だけで数十個、
//! しかも走行状態 (どのセルを握っているか / 進捗) が毎フレーム変わりうるので、
//! cache key を作るコストのほうが大きい (アレンジ本体は数千クリップなので事情が逆)。
//!
//! 標識 (▶ / 停止 / 録音 / 進捗) は **暗いチップ + そのチップから導いたインク**で描く。
//! セルの塗りはユーザー着色の可変背景なので、固定トークンを置くと必ずどちらかの
//! 極性で沈む (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
//! チップを 1 枚敷けば、記号のコントラストは背景に依らず [`indicator_on`] が決める。

use super::*;

use daw_ui_core::color::composite_over;
use daw_ui_core::theme::Palette;

/// 可変背景の上に置く標識の材料。
#[derive(Clone, Copy, Debug)]
pub(super) struct Indicator {
    /// 敷くチップ (半透明)。
    pub chip: Color,
    /// チップを敷いた**あと**の実効背景。この上に置くものは全部ここを基準に色を選ぶ。
    pub eff_bg: Color,
    /// 記号のインク。
    pub ink: Color,
}

/// このフレームのポインタ状態 (どこに乗っているか / 何を押しているか)。
///
/// hover は **押せる場所の判定 ([`press::zone_at`]) をそのまま通す**ので、光る場所と
/// 効く場所が構造的に一致する (「光ったのに効かない」/「効くのに光らない」が
/// 起きない)。
#[derive(Clone, Copy, Default)]
pub(super) struct LauncherFeedback {
    pub hover: Option<press::Zone>,
    /// 押されたままのボタン (離すまで出しっぱなし)。
    pub held: Option<LauncherButton>,
    /// 予約中の標識を光らせるフレームか。
    ///
    /// **位相は壁時計ではなく拍から取る** (`dispatch`)。量子化の待ちは音楽的な
    /// 長さなので、点滅もテンポに乗っているほうが「あと何拍か」が体で分かる。
    /// 壁時計だとテンポと無関係に瞬いて、カウントダウンの数字と噛み合わない。
    pub blink: bool,
}

impl LauncherFeedback {
    /// `btn` に対応する押下中フラグ。
    #[must_use]
    fn is_held(self, btn: LauncherButton) -> bool {
        self.held == Some(btn)
    }

    /// セルの ▶ に乗っているか。
    #[must_use]
    fn hovers_cell_launch(self, key: LauncherCellKey) -> bool {
        self.hover == Some(press::Zone::CellLaunch(key))
    }

    /// セル本体 (▶ を除く) に乗っているか。
    #[must_use]
    fn hovers_cell_body(self, key: LauncherCellKey) -> bool {
        self.hover == Some(press::Zone::CellBody(key))
    }
}

/// ボタンの「押されている / 乗っている」を、可変背景の上でも沈まない形で出す。
///
/// **チップの濃さだけで表す。** 記号の色や大きさを変えると、クリップ色によって
/// 見え方が反転する (明るい塗りの上では濃くしたつもりが薄く見える)。チップは
/// [`Palette::scrim`] の重ね塗りなので、どの塗りの上でも「押すほど沈む」向きが
/// 保たれる。
#[must_use]
fn interactive_indicator(p: &Palette, bg: Color, hovered: bool, held: bool) -> Indicator {
    let base = indicator_on(p, bg);
    if !hovered && !held {
        return base;
    }
    // 押下 = チップ 2 枚重ね / hover = 1.5 枚相当。合成後の実効背景から
    // インクを引き直すので、濃くしてもコントラストは保たれる。
    let extra = if held { p.scrim } else { p.scrim.with_alpha(p.scrim.a * 0.5) };
    let eff_bg = composite_over(extra, base.eff_bg);
    Indicator { chip: base.chip, eff_bg, ink: p.adapt_on(eff_bg, p.ink_for(eff_bg)) }
}

/// 可変背景 `bg` の上に標識を置くための材料を作る。
///
/// チップ (`Palette::scrim`) を 1 枚敷いて背景を正規化し、**その合成結果から**
/// インクを決める。呼び出し側が明暗を選ばないので、明るいクリップ色でも暗い
/// クリップ色でも記号が沈まない。
///
/// **極性 2 択 (`ink_for`) だけでは足りない。** 半透明チップを明るい面に敷くと
/// 実効背景が明暗の境目付近に来ることがあり、そこでは 2 択のどちらを選んでも
/// 3:1 に届かない (ライトテーマのレーン色で実測 2.54:1)。極性を選んだうえで
/// [`Palette::adapt_on`] で必要なぶんだけ明度を寄せる。
#[must_use]
pub(super) fn indicator_on(p: &Palette, bg: Color) -> Indicator {
    let chip = p.scrim;
    let eff_bg = composite_over(chip, bg);
    Indicator { chip, eff_bg, ink: p.adapt_on(eff_bg, p.ink_for(eff_bg)) }
}

/// ランチャー帯を描き、`response.launcher` の rect 群を埋める。
///
/// `run.rs` では `header::draw_rows` の **後**に呼ぶ (帯はヘッダ / レーンと x が
/// 排他なので z の競合は無いが、行の減光だけはアレンジのクリップの上に乗る)。
pub(crate) fn dispatch(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    f: &ArrangementFrame<'_>,
    sessions: &LauncherSessions,
    // アレンジのクリップドラッグ。帯の上へ持ってきたときの着地プレビューに要る
    // (帯とレーンは別 session なので、帯側は自分の session だけでは何も知れない)。
    clip_drag: Option<&ClipDragSession>,
    response: &mut ArrangementResponse,
) {
    response.launcher.pane_rect = f.launcher.pane;
    response.launcher.grid_rect = f.launcher.grid;
    response.launcher.col_w = f.launcher.col_w;
    response.launcher.scroll_scene = f.launcher.scroll_scene;
    if f.launcher.pane.w <= 0.0 {
        return;
    }
    if f.launcher.collapsed {
        // つかみ代だけまで畳んだ状態。格子は描かないが、**ランチャー主導の行の減光は
        // 出す** — 帯を隠していても「この行はアレンジを鳴らしていない」は要る情報。
        ui.heavy(("arrangement_launcher", &f.id), |hctx| {
            chrome(hctx, f);
            dim_launcher_rows(hctx, f);
        });
        return;
    }
    let tempo_map = common::audio_render::TempoMap::from_song(app.song_doc.song());
    // 押下 / hover / 点滅位相は 1 度だけ解いて、全部の描画関数へ同じ値を配る。
    // hover の判定は press と **同じ `zone_at`** を通す (光る場所 = 効く場所)。
    let fb = LauncherFeedback {
        hover: f.pointer.pos.and_then(|(x, y)| press::zone_at(f, x, y)),
        held: sessions.live_held_button,
        blink: app
            .transport
            .playhead_beat
            .is_none_or(|b| f64::from(b).rem_euclid(1.0) < 0.5),
    };
    let out = &mut response.launcher;
    ui.heavy(("arrangement_launcher", &f.id), |hctx| {
        chrome(hctx, f);
        dim_launcher_rows(hctx, f);
        head_row(hctx, f, fb, out);
        grid_rows(hctx, app, &tempo_map, f, fb, out);
        drag_overlays(hctx, f, sessions, clip_drag);
    });
}

// ============================================================
// 背景
// ============================================================

/// 帯の下地 (見出し行 = クローム面 / 格子 = レーン面 / 停止列・返す列 = クローム面)。
fn chrome(hctx: &mut HeavyCtx<'_, '_, AppData>, f: &ArrangementFrame<'_>) {
    let l = &f.launcher;
    let style = f.style;
    push_filled_rect(hctx, Rect { x: l.pane.x, y: l.pane.y, w: l.pane.w, h: l.head.h }, style.header_bg);
    push_filled_rect(
        hctx,
        Rect { x: l.pane.x, y: l.grid.y, w: l.pane.w, h: l.grid.h },
        style.bg,
    );
    push_filled_rect(hctx, l.stop_col, style.header_bg);
    push_filled_rect(hctx, l.return_col, style.header_bg);
    // 帯の右端 (= 幅を変えるつかみ代) を 1px の縦線で示す。
    push_filled_rect(
        hctx,
        Rect { x: l.pane.x + l.pane.w - 1.0, y: l.pane.y, w: 1.0, h: l.pane.h },
        style.lane_line,
    );
}

/// 計画書 Q6: **ランチャーが主導権を持つ行は、アレンジ側のクリップを減光する。**
///
/// クリップ 1 つずつではなく **行の帯**をまとめて沈める (`Palette::row_dim_ink` は
/// 「行を沈める overlay」そのもののトークン)。主導権がランチャーにある行では
/// アレンジのタイムラインが鳴っていないので、その行のクリップもグリッドも
/// 再生ヘッドも等しく「いま鳴っていないもの」で、区別して残す理由が無い。
fn dim_launcher_rows(hctx: &mut HeavyCtx<'_, '_, AppData>, f: &ArrangementFrame<'_>) {
    if f.lanes.w <= 0.0 {
        return;
    }
    let ink = hctx.palette().row_dim_ink;
    let lanes = f.lanes;
    hctx.with_clip_rect(lanes, |hctx| {
        for row in &f.rows {
            if !f.launcher_view.rows.get(&row.key).is_some_and(LauncherRowView::launcher_owns) {
                continue;
            }
            let top = layout::row_screen_top(f, row);
            if top + row.height < lanes.y || top > lanes.y + lanes.h {
                continue;
            }
            push_filled_rect(
                hctx,
                Rect { x: lanes.x, y: top, w: lanes.w, h: row.height },
                ink,
            );
        }
    });
}

// ============================================================
// 見出し行 (シーン + グローバルボタン)
// ============================================================

fn head_row(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    out: &mut LauncherResponse,
) {
    let l = &f.launcher;
    let p = hctx.palette();
    // グローバル停止 / グローバル「アレンジへ返す」。停止列 / 返す列の上端に置く
    // (各行のボタンの真上 = 「この列を全部」という読み方が視線どおり)。
    let bg = f.style.header_bg;
    let stop_hit = Rect { x: l.stop_col.x, y: l.head.y, w: l.stop_col.w, h: l.head.h };
    let stop_ind =
        interactive_indicator(p, bg, fb.hover == Some(press::Zone::GlobalStop), false);
    push_stop_glyph(hctx, square_in(stop_hit, 10.0), stop_ind);
    let ret_hit = Rect { x: l.return_col.x, y: l.head.y, w: l.return_col.w, h: l.head.h };
    let ret_ind =
        interactive_indicator(p, bg, fb.hover == Some(press::Zone::GlobalReturn), false);
    push_return_glyph(hctx, square_in(ret_hit, 12.0), ret_ind, false, f.style);

    if l.scene_head.w <= 0.0 {
        return;
    }
    let scene_head = l.scene_head;
    let (first, last) = l.visible_cols();
    hctx.with_clip_rect(scene_head, |hctx| {
        for i in first..last {
            // 実体のある列はそのまま、無い列は placeholder を組んで **必ず描く**。
            // 描かないと、シーンが 0 個のプロジェクトで見出し行が丸ごと空になり
            // 「名前も ▶ も無い」ように見える。
            let placeholder;
            let scene = match f.launcher_view.scenes.get(i) {
                Some(s) => s,
                None => {
                    // 色は付けない (= 見出しのクローム面と同色にして
                    // 「まだ実体が無い列」を色ストライプの不在で示す)。
                    placeholder = LauncherSceneView::placeholder(i, f.style.header_bg);
                    &placeholder
                }
            };
            let r = Rect {
                x: l.col_x(i) + 1.0,
                y: scene_head.y + 1.0,
                w: (l.col_w - 2.0).max(2.0),
                h: (scene_head.h - 2.0).max(2.0),
            };
            if r.x + r.w < scene_head.x || r.x > scene_head.x + scene_head.w {
                continue;
            }
            // **返す rect は見出し帯でクリップする。** 未クリップのまま返すと、
            // 右端の部分表示列の rect がグローバル「アレンジへ返す」ボタンや
            // つかみ代の上まで伸び、そこを右クリックするとシーンのメニューが出る
            // (描画は clip されているので見た目と食い違う)。
            let hit = r.intersect(scene_head);
            if hit.w <= 1.0 {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            out.scene_rects.push((scene.id, i as u32, hit));
            draw_scene_head(hctx, r, scene, f.style, fb);
        }
    });
}

fn draw_scene_head(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    r: Rect,
    scene: &LauncherSceneView,
    style: &ArrangementStyle,
    fb: LauncherFeedback,
) {
    let p = hctx.palette();
    // 色ストライプ (左端 3px) — 列の identity。塗り全面ではなく帯にするのは、
    // 見出しの文字がクローム面の上に乗ったままになるようにするため。
    push_filled_rect(hctx, Rect { w: 3.0, ..r }, p.adapt_on(style.header_bg, scene.color));
    let btn = layout::launch_button_rect(Rect { x: r.x + 3.0, w: (r.w - 3.0).max(2.0), ..r });
    let ind = interactive_indicator(
        p,
        style.header_bg,
        fb.hover == Some(press::Zone::SceneLaunch(scene.id)),
        fb.is_held(LauncherButton::Scene(scene.id)),
    );
    push_launch_glyph(hctx, btn, ind, scene.follow);
    let label = Rect {
        x: btn.x + btn.w + 2.0,
        y: r.y,
        w: (r.w - (btn.x + btn.w + 2.0 - r.x)).max(0.0),
        h: r.h,
    };
    if label.w > 8.0 {
        hctx.push_text(GlyphArea {
            text: scene.name.clone(),
            left: label.x,
            top: label.y + (label.h - style.track_text_size * 1.2) * 0.5,
            font_size: style.track_text_size,
            line_height: style.track_text_size * 1.2,
            color: style.track_text_color,
            clip_rect: Some(label),
            ..GlyphArea::default()
        });
    }
    // 選択中の列は枠で示す。**セルの選択枠と同じ語彙**にするので、
    // 「いまインスペクタが何を出しているか」が帯の上だけで読める。
    if scene.selected {
        push_selection_ring(hctx, r, style, CELL_RADIUS, None);
    }
}

/// 予約の残り拍を小さく出す (`anchor` の下端に右寄せ)。
///
/// 高さが足りない行では **何も描かない** — 潰れた数字を出すより、点滅だけで
/// 「待っている」を伝えるほうが読み違えが少ない。
fn push_countdown(hctx: &mut HeavyCtx<'_, '_, AppData>, anchor: Rect, bg: Color, text: &str) {
    const FONT: f32 = 9.0;
    if anchor.h < FONT * 2.2 || anchor.w < FONT * 1.5 {
        return;
    }
    let ind = indicator_on(hctx.palette(), bg);
    #[allow(clippy::cast_precision_loss)]
    let w = (text.chars().count() as f32 * FONT * 0.62).min(anchor.w - 2.0);
    let chip = Rect {
        x: anchor.x + anchor.w - w - 1.0,
        y: anchor.y + anchor.h - FONT * 1.3 - 1.0,
        w,
        h: FONT * 1.3,
    };
    push_rounded(hctx, chip, ind.chip, Color::TRANSPARENT, 2.0);
    hctx.push_text(GlyphArea {
        text: text.into(),
        left: chip.x,
        top: chip.y,
        font_size: FONT,
        line_height: FONT * 1.3,
        color: ind.ink,
        clip_rect: Some(chip),
        ..GlyphArea::default()
    });
}

// ============================================================
// 格子 (行 × 列)
// ============================================================

fn grid_rows(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    app: &AppData,
    tempo_map: &common::audio_render::TempoMap,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    out: &mut LauncherResponse,
) {
    let l = &f.launcher;
    let band = Rect { x: l.pane.x, y: l.grid.y, w: l.pane.w, h: l.grid.h };
    hctx.with_clip_rect(band, |hctx| {
        for row in &f.rows {
            if !layout::row_visible(f, row) {
                continue;
            }
            let top = layout::row_screen_top(f, row);
            let Some(view) = f.launcher_view.rows.get(&row.key) else {
                continue;
            };
            // 行の y 帯は **セルを置けない行も含めて**返す (`LauncherResponse::row_bands`
            // の doc)。格子で上下をクリップするので、シーン見出しやアレンジ側へは
            // はみ出さない。
            let y0 = top.max(l.grid.y);
            let y1 = (top + row.height).min(l.grid.y + l.grid.h);
            if y1 > y0 {
                out.row_bands.push((
                    row.key,
                    Rect { x: l.grid.x, y: y0, w: l.grid.w, h: y1 - y0 },
                ));
            }
            // 行下端の区切り (アレンジのレーンと同じ語彙)。
            push_filled_rect(
                hctx,
                Rect {
                    x: l.pane.x,
                    y: top + row.height - f.style.lane_line_width_px,
                    w: l.pane.w,
                    h: f.style.lane_line_width_px,
                },
                f.style.lane_line,
            );
            // マスター行はクリップを持たない (`song_lanes` = オートメーションのみ) ので、
            // セルも停止 / 返すボタンも出さない。行としては並ぶ (縦位置を欠かさない)。
            // テンポ / 拍子レーン行 (`launchable == false`) も同じ — engine が行として
            // 登録しないので、押せるものを出すと「押しても何も起きない」だけになる
            // (`AutomationTarget::accepts_launcher_cells` の doc: GUI は描かない /
            // 落とせない / 作れない)。
            if row.key == ArrangementRowKey::Track(MASTER_TRACK_ID) || !view.launchable {
                continue;
            }
            row_buttons(hctx, f, fb, row.key, top, row.height, view);
            // **セルは格子の中だけに描く。** 帯全体でクリップすると、右端の
            // 部分表示列が「アレンジへ返す」ボタンと幅ドラッグのつかみ代を
            // 塗りつぶし、見えている場所 (セル) と押せる場所 (返す列 /
            // スプリッタ) が最大 28px ズレる。
            let grid_band = Rect { x: l.grid.x, y: l.grid.y, w: l.grid.w, h: l.grid.h };
            hctx.with_clip_rect(grid_band, |hctx| {
                row_cells(hctx, app, tempo_map, f, fb, row, top, view, out);
            });
        }
    });
}

/// 停止列 / 返す列の 1 行ぶんのボタン。
#[allow(clippy::too_many_arguments)]
fn row_buttons(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    row_key: ArrangementRowKey,
    top: f32,
    height: f32,
    view: &LauncherRowView,
) {
    let l = &f.launcher;
    let p = hctx.palette();
    let bg = f.style.header_bg;
    // この行に「止める」/「アレンジへ返す」の予約が入っているか。予約中は点滅させる
    // — 量子化が 1 小節なら押してから最大 1 小節ぶん何も起きないので、押せたことが
    // その場で見えないと「効いていない」としか読めない。
    let queued = f.launcher_view.queued.get(&row_key).copied();
    let stop_queued = queued.is_some_and(QueuedView::is_stop);
    let ret_queued = queued.is_some_and(QueuedView::is_arranger);
    let stop_rect = Rect { x: l.stop_col.x, y: top, w: l.stop_col.w, h: height };
    let stop_ind = interactive_indicator(
        p,
        bg,
        fb.hover == Some(press::Zone::RowStop(row_key)),
        stop_queued && fb.blink,
    );
    push_stop_glyph(hctx, square_in(stop_rect, 8.0), stop_ind);
    // 「アレンジへ返す」は主導権がランチャーにある行だけ点灯する (Bitwig と同じ =
    // 押して意味がある行が一目で分かる)。
    let ret_rect = Rect { x: l.return_col.x, y: top, w: l.return_col.w, h: height };
    let ret_ind = interactive_indicator(
        p,
        bg,
        fb.hover == Some(press::Zone::RowReturn(row_key)),
        ret_queued && fb.blink,
    );
    push_return_glyph(hctx, square_in(ret_rect, 10.0), ret_ind, view.launcher_owns(), f.style);
    // **残り拍の数字はここに出さない。** 停止列 / 返す列は 16px しか無く、
    // `2.3` (9px フォントで約 17px) が必ず切れる。数字は `row_cells` が
    // 「これから止まる当のセル」 の上に出す — 変わる対象の上に出るほうが、
    // 列の記号の下に潰れた数字を置くより読み違えが少ない。
}

#[allow(clippy::too_many_arguments)]
fn row_cells(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    app: &AppData,
    tempo_map: &common::audio_render::TempoMap,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    row: &ArrangementRow,
    top: f32,
    view: &LauncherRowView,
    out: &mut LauncherResponse,
) {
    let l = &f.launcher;
    let (first, last) = l.visible_cols();
    let playing = view.playing_clip_id();
    let progress = f.launcher_view.progress.get(&row.key).copied();
    // 「このセルが予約されている」は行に高々 1 件 (engine の予約は行ごとに 1 つ)。
    let queued = f.launcher_view.queued.get(&row.key).copied();
    // caller へ rect を返してよい行か。**押下側と同じ 1 本** (`row_takes_cells`) を通す
    // — グループ行の rect を返すと、`cell_overlays` の右クリックメニューと
    // `cell_drop_target` のファイル drop だけが `drop_cell_at` / `zone_at` の除外を
    // 素通りし、見えず撃てず鳴らないセルを作れてしまう。
    let takes_cells = layout::row_takes_cells(f, row.key);
    for i in first..last {
        let r = layout::cell_rect(l, top, row.height, i);
        if r.x + r.w < l.grid.x || r.x > l.grid.x + l.grid.w {
            continue;
        }
        let key = layout::cell_key(f.launcher_view, row.key, i);
        // **返す rect は格子でクリップする。** 描画は `grid_band` で切られるので
        // 見た目は正しいが、未クリップの rect は右端の部分表示列で「アレンジへ返す」
        // ボタン / つかみ代 / アレンジのレーンの上まで、縦スクロール中はシーン見出し
        // 行まで伸びる。そこを右クリックするとセルのメニューが出る
        // (`head_row` の `scene_rects` が同じ理由で既にクリップ済)。
        let hit = r.intersect(l.grid);
        if takes_cells && hit.w > 1.0 && hit.h > 1.0 {
            out.cell_rects.push((key, hit));
        }
        if view.group {
            draw_group_cell(hctx, f, fb, r, key, i);
            continue;
        }
        let cell = (key.scene_id != 0).then(|| view.cells.get(&key.scene_id)).flatten();
        let Some(cell) = cell else {
            draw_empty_cell(hctx, f, fb, r, key, view.armed);
            continue;
        };
        let is_playing = playing == Some(cell.clip_id);
        let d = CellDraw {
            rect: r,
            key,
            playing: is_playing,
            progress: if is_playing { progress } else { None },
            queued: queued_for_cell(queued, cell.clip_id, is_playing),
        };
        draw_filled_cell(hctx, app, tempo_map, f, fb, d, cell);
    }
}

/// このセルに残り拍を出すか。
///
/// 発火予約は **そのセル自身**に、停止 / アレンジへ返す予約は **いま鳴っている
/// セル** (= これから止まる当のもの) に出す。後者を行の停止列に出さないのは、
/// 16px の列では数字が必ず切れるため (`row_buttons` の doc)。
#[must_use]
fn queued_for_cell(
    queued: Option<QueuedView>,
    clip_id: u32,
    is_playing: bool,
) -> Option<QueuedView> {
    queued.filter(|q| {
        if q.is_stop() || q.is_arranger() { is_playing } else { q.clip_id == clip_id }
    })
}

// ============================================================
// セル
// ============================================================

/// 空セル。アーム中の行なら録音の丸、非アームなら停止の四角 (Bitwig)。
fn draw_empty_cell(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    r: Rect,
    key: LauncherCellKey,
    armed: bool,
) {
    let p = hctx.palette();
    let bg = f.style.bg;
    // 本体に乗っているだけでも薄く起こす (「ここは押せる場所です」を示す面)。
    // **記号の上と本体では濃さを変える** — 押して起きることが違うので
    // (記号 = その行を止める / 本体 = 焦点を移すだけ)、同じ見た目にしない。
    let body_hover = fb.hovers_cell_body(key);
    let base = if body_hover { 0.40 } else { 0.25 };
    push_rounded(hctx, r, p.control.with_alpha(base), Color::TRANSPARENT, 0.0);
    let btn = layout::launch_button_rect(r);
    let btn_ind = interactive_indicator(
        p,
        bg,
        fb.hovers_cell_launch(key),
        fb.is_held(LauncherButton::Cell(key)),
    );
    push_rounded(hctx, btn, btn_ind.chip, Color::TRANSPARENT, 2.0);
    let s = (btn.w * 0.6).max(3.0);
    let inner = Rect { x: btn.x + (btn.w - s) * 0.5, y: btn.y + (btn.h - s) * 0.5, w: s, h: s };
    if armed {
        // 録音の丸。**赤は「アーム」の意味色なので、面から離れる明度へ寄せて必ず立たせる**
        // (`adapt_on` は色相を保つので赤のままコントラストだけが上がる)。
        let dot = p.adapt_on(btn_ind.eff_bg, p.meter_red);
        push_rounded(hctx, inner, dot, Color::TRANSPARENT, s * 0.5);
    } else {
        push_rounded(hctx, inner, btn_ind.ink, Color::TRANSPARENT, 0.0);
    }
}

/// [`draw_filled_cell`] に渡す「このセルの今の状態」。
///
/// 個別引数で並べると 10 個を超えて、呼び出し側で順番を取り違えても型が同じ
/// (`bool` が 2 つ) ので気付けない。
#[derive(Clone, Copy)]
struct CellDraw {
    rect: Rect,
    key: LauncherCellKey,
    /// 走行中 (engine が握って鳴らしている)。
    playing: bool,
    /// 走行中セルの進捗 `0..1`。
    progress: Option<f32>,
    /// **このセル**の発火予約 (量子化境界待ち)。
    queued: Option<QueuedView>,
}

/// クリップのあるセル。
fn draw_filled_cell(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    app: &AppData,
    tempo_map: &common::audio_render::TempoMap,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    d: CellDraw,
    cell: &LauncherCellView,
) {
    let (r, key) = (d.rect, d.key);
    let fill = if cell.muted { muted_dim_fill(cell.color) } else { cell.color };
    push_rounded(hctx, r, fill, f.style.clip_border, CELL_RADIUS);
    if cell.muted {
        push_muted_hatch(
            hctx,
            r,
            r.intersect(f.launcher.grid),
            f.style.clip_muted_hatch_color,
            f.style.clip_muted_hatch_spacing_px,
            f.style.clip_muted_hatch_width_px,
        );
    }
    let btn = layout::launch_button_rect(r);
    let label = Rect {
        x: btn.x + btn.w + 2.0,
        y: r.y,
        w: (r.w - (btn.x + btn.w + 2.0 - r.x) - 1.0).max(0.0),
        h: r.h,
    };
    cell_content(hctx, app, tempo_map, f, key, cell, label, fill);
    let text_color = clip_text_color_for(hctx.palette(), f.style, fill, f.style.bg);
    draw_clip_label(hctx, label, &cell.name, cell.linked, text_color, f.style);
    // ▶ は hover / 押下 / **発火待ちの点滅** を同じチップ 1 枚で表す。
    // 予約中の点滅が無いと、量子化 1 小節で押した直後は画面が 1px も変わらず
    // 「押せていない」としか読めない (実際は engine が既に受け取っている)。
    // ▶ を点滅させるのは **このセルの発火予約**だけ。停止 / 返す予約で ▶ を
    // 点滅させると「もうすぐ鳴る」に読めてしまう (実際は止まる) ので、
    // そちらは数字だけ出して行の停止 / 返すボタン側を点滅させる。
    let blink_queued =
        d.queued.is_some_and(|q| !q.is_stop() && !q.is_arranger()) && fb.blink;
    let ind = interactive_indicator(
        hctx.palette(),
        fill,
        fb.hovers_cell_launch(key),
        fb.is_held(LauncherButton::Cell(key)) || blink_queued,
    );
    push_launch_glyph(hctx, btn, ind, cell.follow);
    // 残り拍 (発火まで)。セル右端に置く = ラベルの右、進捗バーの上。
    if let Some(q) = d.queued {
        push_countdown(hctx, Rect { h: (r.h - PROGRESS_BAR_H).max(0.0), ..r }, fill, &q.label());
    }
    let selected = is_cell_selected(f, key);
    if d.playing {
        // **走行中の印は縁**。進捗バーは束 B が `audio_bridge` で位置を publish する
        // まで出ないので、これが無いと「どのセルが鳴っているか」が一切見えない。
        // 色は再生の意味色を実塗り色から寄せる (`adapt_on` は色相を保つ)。
        //
        // **選択枠と同じ矩形には描かない。** `push_selection_ring` は外 2px + 内 2px で
        // `r` の縁を完全に覆うので、同じ矩形に描くと選択中のセルでは走行中リングが
        // 1px も残らず、「選択した停止セル」と見分けが付かなくなる。選択と走行は
        // 直交する状態なので、走行側を選択枠の内側へ寄せて両方残す。
        let inset = if selected { f.style.clip_selected_border_w * 2.0 } else { 0.0 };
        let ring_rect =
            Rect { x: r.x + inset, y: r.y + inset, w: r.w - inset * 2.0, h: r.h - inset * 2.0 };
        let p = hctx.palette();
        let ring = p.adapt_on(fill, p.meter_green);
        if ring_rect.w > 2.0 && ring_rect.h > 2.0 {
            hctx.push_rect(RectCommand {
                rect: ring_rect,
                fill: Color::TRANSPARENT,
                border: ring,
                border_width: 2.0,
                radius: [(CELL_RADIUS - inset).max(0.0); 4],
                clip_rect: Some(f.launcher.grid),
            });
        }
    }
    if let Some(t) = d.progress {
        push_progress(hctx, r, fill, t);
    }
    if selected {
        push_selection_ring(hctx, r, f.style, CELL_RADIUS, Some(f.launcher.grid));
    }
}

/// セルが選択集合に入っているか。
///
/// 見るのは **帯が自分で運んできた集合** ([`LauncherView::selected`]) 1 本。
/// アレンジ側の `selected_clips` / `selected_automation_clips` は別の面なので
/// 覗かない (`LauncherView::selected` の doc)。
fn is_cell_selected(f: &ArrangementFrame<'_>, key: LauncherCellKey) -> bool {
    key.model_key().is_some_and(|k| f.launcher_view.selected.contains(&k))
}

/// セルの中身のミニ表示 (行が低いときは何も描かない = 名前だけになる)。
#[allow(clippy::too_many_arguments)]
fn cell_content(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    app: &AppData,
    tempo_map: &common::audio_render::TempoMap,
    f: &ArrangementFrame<'_>,
    key: LauncherCellKey,
    cell: &LauncherCellView,
    area: Rect,
    fill: Color,
) {
    let inset = clip_content_inset_top(f.style);
    if area.w <= 8.0 || area.h <= inset + 4.0 || cell.len_beats <= 0.0 {
        return;
    }
    // セルの窓 (`[content_offset, content_offset + len)`) を `area` の内側幅いっぱいに
    // 引き伸ばした写像。アレンジと違いビューのズームには依存しない — セルは
    // 「撃った瞬間を原点とするループ 1 周」で、時間軸上の位置を持たないため。
    let inner_x = area.x + 2.0;
    let inner_w = f64::from((area.w - 4.0).max(1.0));
    let px_per_beat = inner_w / cell.len_beats;
    #[allow(clippy::cast_possible_truncation)]
    let map = ContentMap {
        origin_x: inner_x - (cell.content_offset_beats * px_per_beat) as f32,
        px_per_beat,
    };
    if !cell.curve.is_empty() {
        draw_cell_curve(hctx, f, area, inset, cell, map, fill);
        return;
    }
    let Some(clip_key) = key.clip_key() else {
        return;
    };
    let Some(mc) = app
        .song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == clip_key.track_id)
        .and_then(|t| t.session_clip_by_id(clip_key.clip_id))
    else {
        return;
    };
    let mut spans = Vec::new();
    let Some(content) = content_build::build_one(app, tempo_map, &mc.clip, None, &mut spans) else {
        return;
    };
    match content {
        ClipContentDraw::Audio { events } => {
            draw_clip_waveform_inner(
                hctx,
                clip_key,
                area,
                map,
                &events,
                false,
                f.launcher.grid,
                inset,
                fill,
                f.style,
                // アレンジ側の同一クリップと LOD 状態を共有しないための弁別子
                // (同じ id で 2 度描くと pyramid が毎フレーム作り直される)。
                "launcher_cell_wf",
            );
        }
        ClipContentDraw::Midi { notes } => {
            draw_clip_midi_inner(hctx, area, &notes, map, fill, f.style, f.launcher.grid.x, inset);
        }
    }
}

/// オートメーションレーン行のセルの曲線 (正規化値の折れ線)。
fn draw_cell_curve(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    area: Rect,
    inset: f32,
    cell: &LauncherCellView,
    map: ContentMap,
    fill: Color,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};
    let top = area.y + inset;
    let h = (area.h - inset - 2.0).max(1.0);
    let ink = clip_ink_for(hctx.palette(), fill, fill);
    let mut segs: Vec<LineSegment> = Vec::new();
    let mut prev: Option<[f32; 2]> = None;
    for &(t, v) in &cell.curve {
        let x = map.x(t + cell.content_offset_beats);
        let y = top + (1.0 - v.clamp(0.0, 1.0)) * h;
        if let Some(prev) = prev {
            segs.push(LineSegment { a: prev, b: [x, y], color: ink });
        }
        prev = Some([x, y]);
    }
    if !segs.is_empty() {
        hctx.push_lines(LineBatch {
            segments: segs.into(),
            line_width_px: f.style.automation_curve_line_width_px,
            clip_rect: Some(area),
        });
    }
}

/// グループトラック行の「まとめセル」。子行のセル色を縞にして重ね、シーン名を載せる。
/// daw_01 のグループトラックは自分のクリップを鳴らさない (`process_track_owned` が
/// `track_has_children` で pass 1 を抜ける) ので、行の意味が衝突しない。
fn draw_group_cell(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    fb: LauncherFeedback,
    r: Rect,
    key: LauncherCellKey,
    col: usize,
) {
    let ArrangementRowKey::Track(group_id) = key.row else {
        return;
    };
    if key.scene_id == 0 {
        // 実体の無い列 = 空セルと同じ見た目 (押すと子行が一斉停止するので、
        // 「押せるのに何も見えない」状態を作らない)。
        draw_empty_cell(hctx, f, fb, r, key, false);
        return;
    }
    let stripes: Vec<Color> = f
        .tracks
        .iter()
        .filter(|t| press::is_group_descendant(f.tracks, t.id, group_id))
        .filter_map(|t| {
            f.launcher_view
                .rows
                .get(&ArrangementRowKey::Track(t.id))
                .and_then(|row| row.cells.get(&key.scene_id))
                .map(|c| c.color)
        })
        .collect();
    if stripes.is_empty() {
        // 子が誰もこの列にセルを持たない = 押すと一斉停止。空セルとして描く。
        draw_empty_cell(hctx, f, fb, r, key, false);
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let sh = r.h / stripes.len() as f32;
    for (i, c) in stripes.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = r.y + i as f32 * sh;
        push_filled_rect(hctx, Rect { x: r.x, y, w: r.w, h: sh }, *c);
    }
    let base = stripes[0];
    let btn = layout::launch_button_rect(r);
    // **まとめセルも押せる場所なので、押せるように見せる。** `zone_at` は
    // グループ行のセル本体ぜんぶを `Zone::CellLaunch` に倒すのに、ここだけ
    // `indicator_on` (= フィードバック無し) で描いていたので、押下中も hover 中も
    // 画面が 1px も変わらなかった (量子化 1 小節なら最大 1 小節「押せていない」
    // ようにしか見えず、連打で二重発火する)。本体全体がボタンなので判定は
    // `CellLaunch(key)` だけで足りる。
    let ind = interactive_indicator(
        hctx.palette(),
        base,
        fb.hovers_cell_launch(key),
        fb.is_held(LauncherButton::Cell(key)),
    );
    push_launch_glyph(hctx, btn, ind, false);
    let label = Rect {
        x: btn.x + btn.w + 2.0,
        y: r.y,
        w: (r.w - (btn.x + btn.w + 2.0 - r.x) - 1.0).max(0.0),
        h: r.h,
    };
    if let Some(scene) = f.launcher_view.scenes.get(col) {
        let text_color = clip_text_color_for(hctx.palette(), f.style, base, f.style.bg);
        draw_clip_label(hctx, label, &scene.name, false, text_color, f.style);
    }
}

// ============================================================
// 標識 (すべて「チップ + そこから導いたインク」)
// ============================================================

/// `r` の中央に一辺 `size` の正方形を取る。
fn square_in(r: Rect, size: f32) -> Rect {
    let s = size.min(r.w - 2.0).min(r.h - 2.0).max(3.0);
    Rect { x: r.x + (r.w - s) * 0.5, y: r.y + (r.h - s) * 0.5, w: s, h: s }
}

fn push_rounded(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    r: Rect,
    fill: Color,
    border: Color,
    radius: f32,
) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: if border.a > 0.0 { 1.0 } else { 0.0 },
        radius: [radius; 4],
        clip_rect: None,
    });
}

/// ▶ (発火)。`striped` でチップに斜線を重ねる = フォローアクションが設定されている印
/// (Live と同じ視覚言語)。
fn push_launch_glyph(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    btn: Rect,
    ind: Indicator,
    striped: bool,
) {
    push_rounded(hctx, btn, ind.chip, Color::TRANSPARENT, 2.0);
    if striped {
        push_muted_hatch(hctx, btn, btn, ind.ink.with_alpha(0.55), 3.0, 1.0);
    }
    let ink = ind.ink;
    let size = (btn.h * 0.8).min(btn.w);
    hctx.push_text(GlyphArea {
        text: "▶".into(),
        left: btn.x + (btn.w - size) * 0.5,
        top: btn.y + (btn.h - size * 1.2) * 0.5,
        font_size: size,
        line_height: size * 1.2,
        color: ink,
        clip_rect: Some(btn),
        ..GlyphArea::default()
    });
}

/// ■ (Stop Clips)。**グリフではなく矩形で描く** — 記号の有無をフォントに依存させると、
/// フォールバックが無い環境で「見えないのに押せるボタン」になる
/// (`arrangement/header.rs` が `▽` / `▷` で踏んだのと同じ罠)。
fn push_stop_glyph(hctx: &mut HeavyCtx<'_, '_, AppData>, btn: Rect, ind: Indicator) {
    push_rounded(hctx, btn, ind.chip, Color::TRANSPARENT, 2.0);
    let s = (btn.w * 0.55).max(3.0);
    push_rounded(
        hctx,
        Rect { x: btn.x + (btn.w - s) * 0.5, y: btn.y + (btn.h - s) * 0.5, w: s, h: s },
        ind.ink,
        Color::TRANSPARENT,
        0.0,
    );
}

/// ▶| (Switch Playback to Arranger)。`lit` でランチャー主導の行を強調する。
fn push_return_glyph(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    btn: Rect,
    ind: Indicator,
    lit: bool,
    style: &ArrangementStyle,
) {
    push_rounded(hctx, btn, ind.chip, Color::TRANSPARENT, 2.0);
    // 点灯色も **チップを敷いたあとの実効背景**から寄せる (チップの半透明色を
    // そのまま背景として渡すと、明るい面の上で寄せ足りずに沈む)。
    let accent =
        if lit { hctx.palette().adapt_on(ind.eff_bg, style.playhead_color) } else { ind.ink };
    let size = (btn.h * 0.7).min(btn.w * 0.7);
    hctx.push_text(GlyphArea {
        text: "▶".into(),
        left: btn.x + 1.0,
        top: btn.y + (btn.h - size * 1.2) * 0.5,
        font_size: size,
        line_height: size * 1.2,
        color: accent,
        clip_rect: Some(btn),
        ..GlyphArea::default()
    });
    // 右端の縦棒で「アレンジ (右) へ返す」を表す。
    push_rounded(
        hctx,
        Rect { x: btn.x + btn.w - 3.0, y: btn.y + 2.0, w: 1.5, h: (btn.h - 4.0).max(1.0) },
        accent,
        Color::TRANSPARENT,
        0.0,
    );
}

/// 走行中セルの進捗バー (セル下端)。
///
/// 溝はチップ、伸びる側は再生の意味色を **チップから導いた明度**に寄せる
/// (`adapt_on` は色相を保つので緑のまま、コントラストだけが上がる)。
fn push_progress(hctx: &mut HeavyCtx<'_, '_, AppData>, cell: Rect, fill: Color, t: f32) {
    let p = hctx.palette();
    let ind = indicator_on(p, fill);
    let track = Rect {
        x: cell.x + 1.0,
        y: cell.y + cell.h - PROGRESS_BAR_H - 1.0,
        w: (cell.w - 2.0).max(1.0),
        h: PROGRESS_BAR_H,
    };
    if track.w <= 1.0 || track.h <= 0.0 {
        return;
    }
    push_rounded(hctx, track, ind.chip, Color::TRANSPARENT, 1.0);
    let bar = p.adapt_on(ind.eff_bg, p.meter_green);
    push_rounded(
        hctx,
        Rect { w: (track.w * t.clamp(0.0, 1.0)).max(1.0), ..track },
        bar,
        Color::TRANSPARENT,
        1.0,
    );
    // **セルを横切る縦線 (プレイヘッド)。** 下端の細いバーだけだと「いまセルの
    // どこを鳴らしているか」がひと目で分からない (アレンジのレーンには縦線が
    // 出るのに、セルだけ出ないのは非対称)。色は再生の意味色を実塗り色から
    // 寄せるので、どの塗りの上でもコントラストが立つ
    // (`feedback_ui_indicator_contrast_on_variable_bg`)。
    let x = cell.x + (cell.w * t.clamp(0.0, 1.0)).clamp(0.0, (cell.w - PLAYHEAD_W).max(0.0));
    push_filled_rect(
        hctx,
        Rect { x, y: cell.y + 1.0, w: PLAYHEAD_W, h: (cell.h - 2.0).max(1.0) },
        bar,
    );
}

// ============================================================
// ドラッグ中の overlay
// ============================================================

fn drag_overlays(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    sessions: &LauncherSessions,
    clip_drag: Option<&ClipDragSession>,
) {
    // アレンジから帯へ運んできているクリップの着地プレビュー。
    //
    // **帯側に描く口が無いと何も出ない。** アレンジのゴーストはレーンの中だけに
    // 描かれるので、ポインタが帯へ入った瞬間にプレビューが消え、「どのスロットに
    // 落ちるのか分からないまま離す」ことになっていた。着地先は release と同じ
    // `plan_clip_drops` から取る。
    if let Some(nd) = clip_drag {
        let slots: Vec<(ArrangementRowKey, u32)> = release::plan_clip_drops(f, nd)
            .into_iter()
            .map(|d| (d.to_row, d.to_scene_index))
            .collect();
        if !slots.is_empty() {
            let mode = ClipCopyMode::from_modifiers(nd.last_ctrl, nd.last_shift);
            push_slot_ghosts(hctx, f, &slots, ghost_style(f, mode));
        }
    }
    if let Some(sr) = sessions.live_scene_reorder.as_ref() {
        // **preview と commit は同じ閾値を使う** (`geometry::REORDER_DRAG_THRESHOLD_PX`
        // の doc)。押しただけで線が出ると「線は出ているのに離しても動かない」に
        // 見える (確定は 16px 動かしてから)。
        let dx = sr.last_mouse.0 - sr.anchor_mouse.0;
        let dy = sr.last_mouse.1 - sr.anchor_mouse.1;
        if (dx * dx + dy * dy).sqrt() < REORDER_DRAG_THRESHOLD_PX {
            return;
        }
        // 落とし先の縦線 (commit と同じ `drop_scene_index` を通すので指す位置 = 着地位置)。
        //
        // **帯でクリップする。** `drag_overlays` は `with_clip_rect` の外で呼ばれ、
        // `push_filled_rect` は `clip_rect: None` なので、そのまま積むと線が帯の外へ出る
        // — 横スクロール中にポインタをヘッダ側へ引くと `col_x` が負方向へ回り、
        // トラックヘッダ列や widget の左外に縦線が立つ (`push_slot_ghosts` は
        // 既に `with_clip_rect(grid, ..)` を張っているのに、ここだけ非対称だった)。
        let idx = drag::drop_scene_index(f, sr.last_mouse.0);
        let x = f.launcher.col_x(idx);
        let line = Rect {
            x: x - f.style.reorder_drop_indicator_h * 0.5,
            y: f.launcher.head.y,
            w: f.style.reorder_drop_indicator_h,
            h: f.launcher.pane.h,
        };
        let pane = f.launcher.pane;
        hctx.with_clip_rect(pane, |hctx| {
            push_filled_rect(hctx, line, f.style.reorder_drop_indicator);
        });
    }
    if let Some(cd) = sessions.live_cell_drag.as_ref() {
        let dx = cd.last_mouse.0 - cd.anchor_mouse.0;
        let dy = cd.last_mouse.1 - cd.anchor_mouse.1;
        if dx.abs() + dy.abs() < CELL_DRAG_SLOP_PX {
            return;
        }
        let style = ghost_style(f, ClipCopyMode::from_modifiers(cd.last_ctrl, cd.last_shift));
        // 格子の上ならスロットへスナップする。**着地先は commit と同じ
        // `plan_cell_moves` から取る**ので、ゴーストが乗っているスロットが
        // そのまま落ちる先になる。
        let slots: Vec<(ArrangementRowKey, u32)> = release::plan_cell_moves(f, cd)
            .into_iter()
            .map(|m| (m.to_row, m.to_scene_index))
            .collect();
        if slots.is_empty() {
            // 格子の外 (= アレンジのレーンへ持ち出している最中 / 停止列の上)。
            // 落ちるスロットが無いので、従来どおりカーソルに付くゴーストを出す。
            let w = (f.launcher.col_w - 2.0).max(8.0);
            let h = f.view.track_row_h.max(8.0) - 4.0;
            let ghost = Rect { x: cd.last_mouse.0 - w * 0.5, y: cd.last_mouse.1 - h * 0.5, w, h };
            push_rounded(hctx, ghost, style.0, style.1, CELL_RADIUS);
            return;
        }
        push_slot_ghosts(hctx, f, &slots, style);
    }
}

/// ドラッグ中のゴーストの `(塗り, 縁)`。運び方 (移動 / リンク複製 / 独立複製) で
/// 色が変わるのはアレンジのクリップドラッグと同じ語彙。
#[must_use]
fn ghost_style(f: &ArrangementFrame<'_>, mode: ClipCopyMode) -> (Color, Color) {
    let (fill, border) = match mode {
        ClipCopyMode::Move => (f.style.clip_selected_fill, f.style.clip_selected_border),
        ClipCopyMode::CloneLinked => {
            (f.style.clip_clone_linked_fill, f.style.clip_clone_linked_border)
        }
        ClipCopyMode::CloneIndependent => {
            (f.style.clip_clone_indep_fill, f.style.clip_clone_indep_border)
        }
    };
    (fill.with_alpha(DRAG_PREVIEW_FILL_ALPHA), border)
}

/// 着地先のスロットにゴーストを敷く。
///
/// rect は **描画と当たり判定が共有する `layout::cell_rect`** から取るので、
/// ゴーストは実際のセルとぴったり重なる。見えていない行 / 列は描かない。
fn push_slot_ghosts(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    slots: &[(ArrangementRowKey, u32)],
    (fill, border): (Color, Color),
) {
    let grid = f.launcher.grid;
    hctx.with_clip_rect(grid, |hctx| {
        for (row_key, col) in slots {
            let Some(row) = f.rows.iter().find(|r| r.key == *row_key) else {
                continue;
            };
            if !layout::row_visible(f, row) {
                continue;
            }
            let top = layout::row_screen_top(f, row);
            let r = layout::cell_rect(&f.launcher, top, row.height, *col as usize);
            if r.x + r.w < grid.x || r.x > grid.x + grid.w {
                continue;
            }
            push_rounded(hctx, r, fill, border, CELL_RADIUS);
        }
    });
}
