//! トラック inspector のモジュレーションラックの **枠**。
//!
//! LFO / Random / MSEG / Steps / EnvelopeFollower の各ソースを折りたたみ行で並べ、
//! ソース直下に routing (depth / polarity / remove) を出す。 親モジュール
//! `track_inspector` の `draw()` から scroll viewport 末尾に 1 回だけ呼ばれる
//! (`draw_modulation_rack`)。
//!
//! **展開したときの中身は種別ごとに [`bodies`] へ割る。** 全種別を 1 つの `match` に
//! 書くと `draw_modulation_rack` だけが太り、 種別を足すたびにサイズ budget
//! (1 ファイル 1,000 実コード行 / 1 関数 300 行 / インデント 6 段) を押し上げる
//! (不変条件 9)。

mod bodies;
mod preview;

use daw_ui_core::{Edit, MsegEditorStyle, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::view::disclosure::{RevealAxis, disclosure_glyph};

use bodies::{
    draw_follower_body, draw_lfo_body, draw_mseg_body, draw_random_body, draw_steps_body,
};

// 数値 field の base style は inspector 全体で共有するので親モジュールから借りる
// (ラベル色は `app.theme.core.text` を直接読む)。
use super::scrub_style;

/// 行の高さ (px) と、 次の行までの送り (px)。 ラック全体で共通。
const ROW_H: f32 = 20.0;
const ROW_PITCH: f32 = 22.0;
/// rate dropdown の幅 (px)。
const MOD_RATE_DROPDOWN_W: f32 = 52.0;
/// Hz スクラバの幅 (px)。 `"0.0123"` + 単位 `"Hz"` が font 11 で収まる幅。
const MOD_HZ_W: f32 = 66.0;
/// 隣り合うコントロールの間隔 (px)。
const GAP: f32 = 6.0;

/// 帯域フィルタを ON にしたときの初期値。 キック抽出の定番帯域 (30–200Hz)。
const MOD_BAND_DEFAULT: common::model::BandFilter =
    common::model::BandFilter { hp_hz: 30.0, lp_hz: 200.0 };



/// モジュレーター用グラフィカルエディタの色味。
///
/// 面 (`inset_bg`) / カーブ (`curve`) / ノード (`text`) は ui-core の
/// [`MsegEditorStyle::from_palette`] のまま。 ラック固有なのはグリッドの濃度と、
/// 操作中アフォーダンス / 再生位置カーソルの 4 色だけ。
fn mod_editor_style(theme: &crate::theme::Theme) -> MsegEditorStyle {
    MsegEditorStyle {
        // automation の最弱段 (`grid_line_faint`) より濃い中間段。 canvas が窪み 1 枚
        // しか無いので、 これ以上薄いと波形の縦位置 (0 / 0.5 / 1) が読めなくなる。
        grid: theme.core.grid_line.with_alpha(0.06),
        // node の hover (淡黄) / drag (珊瑚) は curve エディタ固有の操作中アフォーダンス。
        node_hover_color: theme.daw.node_hover,
        node_drag_color: theme.daw.node_drag,
        // tension は line_color と同 hue の半透明版なので curve 由来で揃える。
        tension_color: theme.core.curve.with_alpha(0.7),
        // ライブカーソルは transport の再生位置そのものなので playhead と同色。
        cursor_color: theme.daw.playhead.with_alpha(0.85),
        ..MsegEditorStyle::from_palette(&theme.core)
    }
}

/// グラフィカルエディタのカーブ描画高さ (px)。
const MOD_CANVAS_H: f32 = 96.0;

/// modulation rack の header 行左端に置く開閉 disclosure ボタンの幅 (px)。
const MOD_DISCLOSURE_W: f32 = 16.0;
/// disclosure ボタンと、 その右に来る名前 / track dropdown / routing ラベルの間隔 (px)。
const MOD_DISCLOSURE_GAP: f32 = 2.0;
/// 行左端 (`lx`) から名前 / routing ラベル左端までの距離 (px)。
/// **disclosure ボタンの実寸から導出する** — 旧実装は `18.0` を header の名前起点・
/// routing 行の x・同じ行の幅の項の 3 か所に手写ししていて、 ボタン幅を変えると
/// routing 行だけ黙ってずれた (r.md #74)。
const MOD_NAME_INSET: f32 = MOD_DISCLOSURE_W + MOD_DISCLOSURE_GAP;










/// 種別ごとの本体描画 (`draw_*_body`) が共有する面と時刻。 可変なのは呼び出し側の
/// y カーソルだけなので、 それだけを引数で受け渡す。
struct ModBodyCtx<'a> {
    app: &'a AppData,
    /// canvas 系 (signal_preview / mseg_editor / step_grid) のスタイル。
    editor: MsegEditorStyle,
    area: Rect,
    pad: f32,
    /// 行左端。
    lx: f32,
    /// 行の幅 (= canvas 幅)。
    row_w: f32,
    /// ライブカーソル用 transport 位置。 `beat` と `secs` は同じ tempo map の 1 点。
    beat: f64,
    secs: f64,
    /// 輪 / tier / 入力辺の **唯一の判定者** (`mod_graph::build_plan` の結果)。
    /// GUI は読むだけで、 自前で輪や tier を判定しない。
    plan: &'a common::mod_graph::ModPlan,
}

/// プレビューの transport 位置 `(beat, secs)` を **engine / export と同じ写像** で出す。
///
/// engine は `song_secs = playhead / sample_rate`、 `playhead_beats` は bpm の積分
/// (`daw_audio/src/engine.rs`)、 export も同じ組
/// (`samples_to_beats` → `playhead / sample_rate`)。 GUI だけが秒を
/// `beat * 60 / bpm` で自作していて、 テンポカーブのある曲では 3 経路の値が割れていた
/// (r.md #88-7)。 ここで beat をサンプル位置へ写して秒を出せば、 GUI の `(beat, secs)` も
/// 同じ tempo map の 1 点になる。
///
/// **beat と秒は必ず同時に寄せる**。 秒だけを直すと、 定数 bpm での往復相殺
/// (`playhead_to_beat` は定数 bpm 換算なので `beat * 60 / bpm == playhead / sr`) で
/// たまたま合っていた再生中の一致まで壊れる。 テンポカーブが無い曲では
/// `beats_to_samples` が閉形式に落ちるので、 既存曲のプレビューは 1 サンプルも動かない。
fn mod_preview_pos(app: &AppData) -> (f64, f64) {
    let beat = f64::from(app.transport.playhead_beat.unwrap_or(0.0));
    (beat, song_secs_at_beat(app, beat))
}

/// `beat` を engine / export と同じ写像で song 秒へ落とす。
///
/// テンポカーブのある曲では `beats_to_samples` が 1/64 拍刻みの積分 = **O(拍)** で、
/// 描画は毎フレーム走る。 拍は 1 フレームの間に何度も同じ値で問われる (transport 位置 +
/// `build_plan` の anchor) ので、 **直近 1 件だけ memo** する。 曲を編集したら
/// `edit_epoch` が変わるので、 テンポカーブを引き直したときは自動で無効になる。
fn song_secs_at_beat(app: &AppData, beat: f64) -> f64 {
    let sr = app.ipc.sample_rate;
    if sr == 0 || !beat.is_finite() {
        return 0.0;
    }
    let epoch = app.song_doc.edit_epoch();
    if let Some((e, b, secs)) = app.ui_ephemeral.preview_secs_memo.get()
        && e == epoch
        && b.to_bits() == beat.to_bits()
    {
        return secs;
    }
    let samples = common::automation::beats_to_samples(app.song_doc.song(), sr, beat);
    #[allow(clippy::cast_precision_loss)]
    let secs = samples as f64 / f64::from(sr);
    app.ui_ephemeral.preview_secs_memo.set(Some((epoch, beat, secs)));
    secs
}

/// モジュレーションラックを **スクロール viewport の top-down
/// フロー** で描く (旧: 下端 pinned・2 個 cap)。各ソースは折りたたみ行で、クリックで
/// クリックで大きなグラフィカルエディタに展開する (`app.ui_ephemeral.expanded_mod_sources`、 複数同時可)。
/// 戻り値は描画後の `y`。
#[allow(clippy::too_many_lines)]
pub(super) fn draw_modulation_rack(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    use common::model::ModSourceKind as K;

    let p = &app.theme.core;
    let lx = area.x + pad;
    let row_w = area.w - pad * 2.0;
    let mod_sources = app.mod_source_display();
    // ライブカーソル用の transport 位置。 beat と秒は **同じ tempo map の 1 点**
    // ([`mod_preview_pos`])。
    let (beat, secs) = mod_preview_pos(app);
    // 輪 (⟳) と位置依存 (audio tier) の **判定はここではしない**。
    // `mod_graph::build_plan` が唯一の判定者で、 GUI / engine / export が同じ 1 本を
    // 引く規約 (片側だけで判定すると、 バッジが出ていないのに音が位置依存になる /
    // その逆が起きる)。 バッジは plan を読むだけ。
    //
    // `anchor_secs` は `FromBeat` の anchor の秒換算。 バッジ (`in_cycle` / `tier`) は
    // 読まないが、 **この plan はプレビューの評価入力でもある** (`preview::cross_mod_window`
    // → `mod_graph::tick`)。 0 で埋めると Hz モードの ⟲here がプレビュー上だけ
    // FreeRun として描かれるので、 transport 位置と同じ写像で解決する。
    let plan = common::mod_graph::build_plan(app.song_doc.song(), 0, |anchor_beat| {
        song_secs_at_beat(app, anchor_beat)
    });
    let cx = ModBodyCtx {
        app,
        plan: &plan,
        editor: mod_editor_style(&app.theme),
        area,
        pad,
        lx,
        row_w,
        beat,
        secs,
    };

    ui.label_at("inspector_mod_label", "Modulation", lx, y, 12.0, p.text);
    // [+ ▾] add-menu — 種別を選んで作成。
    {
        let add_w = 64.0;
        let add_rect = Rect { x: area.x + area.w - pad - add_w, y: y - 2.0, w: add_w, h: 20.0 };
        // 先頭は現在の選択として表示されるラベル。 dropdown widget が右端に
        // シェブロンを描くので、 ここに `\u{25be}` を入れると下矢印が二重になる → "+" のみ。
        let add_labels = ["+", "Follow", "LFO", "Random", "MSEG", "Steps"];
        if let Some(picked) = ui.dropdown("inspector_mod_add_src", add_rect, &add_labels, 0)
            && picked > 0
        {
            let tag = match picked {
                1 => crate::app::ModSourceKindTag::Follower,
                2 => crate::app::ModSourceKindTag::Lfo,
                3 => crate::app::ModSourceKindTag::Random,
                4 => crate::app::ModSourceKindTag::Mseg,
                _ => crate::app::ModSourceKindTag::Steps,
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AddModSource { kind: tag });
            }));
        }
    }
    y += 24.0;

    let mod_track_choices = app.mod_source_track_choices();
    let mod_track_labels: Vec<&str> = mod_track_choices.iter().map(|(_, l)| l.as_str()).collect();
    let mut any_mod_drag = false;

    for src in &mod_sources {
        let sid = src.id;
        let expanded = app.ui_ephemeral.expanded_mod_sources.contains(&sid);

        // --- header row: [▶/▼][name/track] [meter] [arm] [×] ---
        //
        // widget id は **すべて安定 id `sid`**。 位置 index で採番すると、 ソースを
        // 消したり並べ替えたりした瞬間にドラッグ状態 / テキストバッファ / 開いている
        // dropdown popup が別ソースの欄へ乗り移る (不変条件 1)。
        let rm_rect = Rect { x: area.x + area.w - pad - 20.0, y, w: 20.0, h: 20.0 };
        let meter_w = 50.0;
        let meter_x = rm_rect.x - 4.0 - meter_w;
        let filled = ((src.scalar.clamp(0.0, 1.0) * 6.0).round() as usize).min(6);
        let meter: String = "\u{25ae}".repeat(filled) + &"\u{25af}".repeat(6 - filled);
        ui.label_at(("inspector_mod_src_meter", sid), &meter, meter_x, y + 4.0, 11.0, p.text);
        let arm_w = 24.0;
        let arm_x = meter_x - 4.0 - arm_w;
        let node = plan.nodes.iter().find(|n| n.source_id == sid);
        let badge_w = draw_mod_badges(ui, &app.theme, sid, node, arm_x - 4.0, y);
        let armed = app.ui_ephemeral.armed_mod_source == Some(sid);
        ui.button_at(
            ("inspector_mod_src_arm", sid),
            if armed { "\u{25c9}" } else { "\u{25cb}" },
            Rect { x: arm_x, y, w: arm_w, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    let next = if app.ui_ephemeral.armed_mod_source == Some(sid) { None } else { Some(sid) };
                    app.handle_event(AppEvent::SetArmedModSource(next));
                })
            },
        );
        // 展開トグル。 rack 行は縦積みで中身は下に開く → 開示軸は Block
        // (r.md #74 で全 disclosure の glyph を `view::disclosure` へ一本化した。
        // 旧実装はここだけ小三角 ▸/▾ を使っていて、 同じ意味のマークが 2 系統
        // 存在していた)。 ▸/▾ と ▶/▼ は既定フォントで advance が同一 (0.527 em)
        // なので、 族を変えても行のレイアウトは 1px も動かない。
        ui.button_at(
            ("inspector_mod_src_expand", sid),
            disclosure_glyph(!expanded, RevealAxis::Block),
            Rect { x: lx, y, w: MOD_DISCLOSURE_W, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    // multi-expand: 既に開いていれば閉じる、 でなければ追加 (複数同時可)。
                    if !app.ui_ephemeral.expanded_mod_sources.insert(sid) {
                        app.ui_ephemeral.expanded_mod_sources.remove(&sid);
                    }
                })
            },
        );
        let name_x = lx + MOD_NAME_INSET;
        // バッジが出ている分だけ名前 / track dropdown を詰める (行は太らせない)。
        let name_rect =
            Rect { x: name_x, y, w: (arm_x - 4.0 - badge_w - name_x).max(40.0), h: 20.0 };
        if let K::EnvelopeFollower { tap, .. } = &src.kind {
            let sel = mod_track_choices
                .iter()
                .position(|(tid, _)| *tid == tap.source_track)
                .unwrap_or(0);
            if let Some(picked) =
                ui.dropdown(("inspector_mod_src_track", sid), name_rect, &mod_track_labels, sel)
                && let Some(&(tid, _)) = mod_track_choices.get(picked)
            {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetModSourceTrack { id: sid, source_track: tid });
                }));
            }
        } else {
            ui.label_at(
                ("inspector_mod_src_kind", sid),
                src.kind.short_label(),
                name_rect.x,
                y + 4.0,
                12.0,
                p.text,
            );
        }
        ui.button_at(("inspector_mod_src_rm", sid), "\u{00d7}", rm_rect, move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::RemoveModSource { id: sid });
            })
        });
        y += 22.0;

        if expanded {
            // --- 展開: グラフィカルエディタ + 種別別コントロール ---
            //
            // 種別ごとに `draw_*_body` へ割る。 1 つの `match` に全種別を書くと
            // `draw_modulation_rack` だけが太り続け、 種別を足すたびにサイズ budget
            // (1 関数 300 実コード行 / インデント 6 段) を押し上げる (不変条件 9)。
            let (next_y, drag) = match &src.kind {
                K::EnvelopeFollower { tap, follower } => {
                    draw_follower_body(ui, &cx, sid, tap, follower, y)
                }
                K::Lfo(c) => draw_lfo_body(ui, &cx, src, c, y),
                K::Random(c) => draw_random_body(ui, &cx, src, c, y),
                K::Mseg(c) => draw_mseg_body(ui, &cx, src, c, y),
                K::Steps(c) => draw_steps_body(ui, &cx, src, c, y),
            };
            y = next_y;
            any_mod_drag |= drag;
        }

        // --- このソースが駆動している routing をソース直下に表示 (畳んでも見える) ---
        //
        // r.md #78: 対象が**どのトラックにあっても**ここに並ぶ (`mod_source_routings`)。
        // ◉ は他トラックのツマミにも効くので、 ソース所有トラック以外の routing が
        // 実際に作れる。 旧実装はカーソルトラックの routing しか描かなかったため、
        // それらはどこにも出ず削除できない孤児になっていた。
        for row in app.mod_source_routings(sid) {
            let g = RoutingRowGeom { area, pad, lx, row_w, y };
            any_mod_drag |= draw_routing_row(ui, app, g, &row, sid);
            y += ROW_PITCH;
        }
        y += 4.0;
    }

    // drag-end edge で sync (scrub 中は dirty のみ)。
    //
    // **同じ edge で undo も bracket する** — ラックの数値欄は毎フレーム
    // `EditModSource` / `SetModRoutingDepth` (= `edit_song`) を撃つので、束ねないと
    // 1 ドラッグで数十 undo step が積まれ `UNDO_LIMIT` を溢れさせる。ラック内で
    // 同時にドラッグできる欄は 1 つなので、集約フラグ 1 本で足りる。
    crate::view::scrub_gesture::push(
        ui,
        app,
        crate::app::ScrubGesture::ModRack,
        any_mod_drag,
    );

    y
}

/// ヘッダ行の右寄せバッジ。 **判定はしない** — `mod_graph::build_plan` が出した
/// [`ModNode`] を読んで描くだけ (輪 → `\u{27f3}` / audio tier → 「位置依存」)。
///
/// `right` から左へ積み、 **消費した幅** を返す (呼び側が名前欄をその分詰める)。
/// 暗いチップ + `ink_for` の自動コントラスト文字なので、 ライト / ダークどちらの
/// テーマでも読める (色を直書きすると片方のテーマで沈む)。
fn draw_mod_badges(
    ui: &mut Ui<'_, AppData>,
    theme: &crate::theme::Theme,
    sid: u32,
    node: Option<&common::mod_graph::ModNode>,
    right: f32,
    y: f32,
) -> f32 {
    let Some(node) = node else { return 0.0 };
    let mut labels: Vec<&'static str> = Vec::new();
    if node.in_cycle {
        // 輪の 1 箇所が 1 制御刻み前の値で回っている (r.md #89 Q2)。
        labels.push("\u{27f3}");
    }
    if node.tier == common::mod_graph::ModTier::Audio {
        // 速さの鎖にフォロワーが居る = 位相が「どこから再生したか」に依存する (Q7)。
        labels.push("位置依存");
    }
    if labels.is_empty() {
        return 0.0;
    }
    let p = &theme.core;
    let chip = p.inset_bg;
    let ink = p.ink_for(chip);
    const FONT: f32 = 9.0;
    const PAD: f32 = 4.0;
    let mut x = right;
    let mut used = 0.0;
    for (i, label) in labels.iter().enumerate() {
        let w = ui.measure_text(label, FONT) + PAD * 2.0;
        x -= w;
        ui.push_rect(daw_ui_renderer::RectCommand {
            rect: Rect { x, y: y + 3.0, w, h: 14.0 },
            fill: chip,
            border: daw_ui_renderer::Color::TRANSPARENT,
            border_width: 0.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        ui.label_at(("inspector_mod_badge", sid, i), label, x + PAD, y + 4.0, FONT, ink);
        used += w + 3.0;
        x -= 3.0;
    }
    used
}

/// routing 1 行の配置 (呼び側の y カーソルと幅をそのまま渡す)。
struct RoutingRowGeom {
    area: Rect,
    pad: f32,
    lx: f32,
    row_w: f32,
    y: f32,
}

/// ソース直下の routing 1 行 (対象名 / depth / 極性 / 削除) を描く。
///
/// 戻り値 = depth 欄をドラッグ / 数値入力中か。呼び側が集約して
/// **1 ドラッグ = 1 undo step** の bracket を張る (束ねないと
/// `SetModRoutingDepth` が毎フレーム undo step を積む)。
fn draw_routing_row(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    g: RoutingRowGeom,
    row: &crate::app::ModRoutingRow,
    sid: u32,
) -> bool {
    let RoutingRowGeom { area, pad, lx, row_w, y: row_y } = g;
    let p = &app.theme.core;
    // widget id は **1 本の変調の安定 id** (`ModRouting::id`)。 旧実装は全ソースを
    // 通したグローバル連番 `route_i` で、 routing を 1 本消すとそれ以降の行の depth
    // ドラッグ / テキストバッファが 1 つ隣へずれた (不変条件 1)。
    let rid = row.id;
    ui.label_at_clipped(
        ("inspector_mod_rt_lbl", rid),
        &format!("\u{2192} {}", row.label),
        Rect {
            x: lx + MOD_NAME_INSET,
            y: row_y + 4.0,
            // 右側の depth / 極性 / × と重ねない (幅は下の x 計算と対)。
            w: (row_w - MOD_NAME_INSET - 4.0 - 46.0 - 4.0 - 22.0 - 4.0 - 20.0).max(1.0),
            h: 11.0 * 1.2,
        },
        11.0,
        p.text,
    );
    let rm_x = area.x + area.w - pad - 20.0;
    let pol_x = rm_x - 4.0 - 22.0;
    let depth_x = pol_x - 4.0 - 46.0;
    let (t, s) = (row.track_id, sid);
    let tgt = row.target.clone();
    // r.md #89 Q9: **深さ欄も普通のツマミ**として扱う (◉ 待受中にドラッグすれば
    // 深さ自体が変調先になる)。 置き場はその変調が置かれている所そのもの
    // (`mod_routing_owner`)。 target だけから決める全域関数は作らない。
    let depth_style = ScrubableNumberStyle {
        // 深さは -1..=1。 帯 / live tick の写像に range が要る (無いと帯が出ない)。
        range: Some((-1.0, 1.0)),
        sensitivity: 0.006,
        ..scrub_style(&app.theme)
    };
    let depth_owner = app
        .song_doc
        .song()
        .mod_routing_owner(row.id)
        .unwrap_or(common::model::MASTER_TRACK_ID);
    let depth_mod = crate::view::modulation::build_mod(
        app,
        common::model::AutomationTarget::ModRoutingDepth { routing_id: row.id },
        f64::from(row.depth),
        crate::view::modulation::PLAIN_IDENT,
        depth_owner,
    );
    let depth_resp = ui.scrubable_number_at(
        ("inspector_mod_rt_depth", rid),
        Rect { x: depth_x, y: row_y, w: 46.0, h: 20.0 },
        f64::from(row.depth),
        1.0,
        ScrubableNumberFormat::Decimal(2),
        &depth_style,
        move |v| {
            let tgt = tgt.clone();
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModRoutingDepth {
                    track_id: t,
                    target: tgt,
                    source_id: s,
                    depth: v as f32,
                });
            })
        },
        None,
        Some(depth_mod.modulation()),
    );
    let bip = row.bipolar;
    let tgt_pol = row.target.clone();
    ui.button_at(
        ("inspector_mod_rt_pol", rid),
        if bip { "\u{00b1}" } else { "+" },
        Rect { x: pol_x, y: row_y, w: 22.0, h: 20.0 },
        move || {
            let tgt_pol = tgt_pol.clone();
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModRoutingPolarity {
                    track_id: t,
                    target: tgt_pol,
                    source_id: s,
                    bipolar: !bip,
                });
            })
        },
    );
    let tgt_rm = row.target.clone();
    ui.button_at(
        ("inspector_mod_rt_rm", rid),
        "\u{00d7}",
        Rect { x: rm_x, y: row_y, w: 20.0, h: 20.0 },
        move || {
            let tgt_rm = tgt_rm.clone();
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::RemoveModRouting {
                    track_id: t,
                    target: tgt_rm,
                    source_id: s,
                });
            })
        },
    );
    // 深さ欄も他の per-control と同じく、 立ち下がりで ◉ を解除する
    // (`mod_param_field` と同じ理由で `any_mod_drag` には混ぜない)。
    crate::view::modulation::push_mod_depth_bracket(
        ui,
        app,
        depth_owner,
        &common::model::AutomationTarget::ModRoutingDepth { routing_id: row.id },
        depth_resp.mod_dragging,
    );
    depth_resp.dragging || depth_resp.editing_text
}

// =====================================================================
// 種別ごとの本体描画
// =====================================================================
//
// いずれも `(次の y, ドラッグ中か)` を返す。 ドラッグ中フラグは呼び側が 1 本に集約し、
// **1 ドラッグ = 1 undo step** の bracket を張る (`ScrubGesture::ModRack`)。






