//! S4b Phase D: arrangement widget の描画 helper 群 (heavy 内で呼ぶ pure draw fn)。
//! private fn に触れる子モジュールとして分離。 型・幾何 helper は `use super::*` で参照。

#![allow(clippy::too_many_arguments)]

// 型・外部クレート import・幾何 helper はすべて親 (`mod.rs`) の可視名を継承する。
use super::*;

// r.md #48: 色は runtime パレット (`HeavyCtx::palette`) から読む。可変背景 (ユーザー着色
// クリップ / 波形 / 映像) の上に置くインクの明暗 2 択は呼び出し側では選ばず、
// `Palette::ink_for` / `Palette::waveform_for` に畳んである。
use daw_ui_core::theme::{Palette, WaveformInk};

pub(super) fn push_filled_rect<M: ?Sized + 'static>(hctx: &mut HeavyCtx<'_, '_, M>, r: Rect, fill: Color) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

// M13 Phase 55: ruler / lanes grid の bar/beat 縦線 + 小節番号テキスト描画は library
// `Ui::time_ruler` / `Ui::bar_beat_grid` (heavy.rs delegate 経由) に統合した。
// この関数は lanes 背景 + per-row 背景 (selection / mute/solo hint / lane separator) のみ。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_lanes_bg<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    lanes: Rect,
    tracks: &[ArrangementTrack],
    visible_tops: &[f32],
    view: ArrangementView,
    selected_tracks: &[u32],
    style: &ArrangementStyle,
) {
    push_filled_rect(hctx, lanes, style.bg);

    // 各 track row 背景 (selection ハイライト + video tint; #085 で group 専用 tint は撤去)。
    // M14 Phase 63c (#016): collapsed 親配下は描画 skip (visible 列のみ index で row を計算)。
    // M14 Phase 63n-6 (#031): per-track row 高さ override 反映のため `visible_tops` (prefix sum) を
    // 受け取り、 row_y / row_h を per-track で算出する (= override 済 track の backdrop fill が正しく
    // 行高さに追従)。
    //
    // r.md #77: 旧実装はここで `compute_visible_indices(tracks)` を呼び直していたが、
    // 唯一の呼び出し元 (`render::dispatch`) が渡すのは **既に filter 済の
    // `ArrangementFrame::visible_tracks`** なので、 この呼び出しは恒等
    // (`compute_visible_indices(visible) == (0..visible.len())`) だった。
    // 根拠: `visible_tracks` は `is_visible_track(t, full_tracks)` で filter 済で、
    // 親チェーンの全 ancestor も同じ条件を満たすので `visible_tracks` 内に存在し、
    // いずれも `collapsed == false`。 synthetic master は `parent_id == None` で即 true。
    // 撤去で cached ブロック内の毎フレーム `Vec<usize>` 確保も消える。
    for (visible_i, t) in tracks.iter().enumerate() {
        let row_y = visible_tops.get(visible_i).copied().unwrap_or(lanes.y);
        let row_h = effective_track_row_h(t, view.track_row_h);
        let row = Rect { x: lanes.x, y: row_y, w: lanes.w, h: row_h };
        if row.y + row.h < lanes.y || row.y > lanes.y + lanes.h {
            continue;
        }
        // selection priority > video > 通常 (selection は overlay layer で再描画される
        // が、 lanes_bg では下塗りとして塗る = visual hint としての役割)。
        // M14 Phase 113 (daw_01 #085): group track 専用の背景 tint は撤去 (= 他 track と同じ
        // neutral 背景)。 group であることは indent (`depth * indent_px`) と disclosure ▶▼ の
        // 構造手掛かりだけで識別する。 video / selection 背景は不変。
        if selected_tracks.contains(&t.id) {
            push_filled_rect(hctx, row, style.track_selected_bg);
        } else if matches!(t.kind, TrackKind::Video) {
            // M14 Phase 72 (#044): video track の行背景は暗青で audio と視覚区別 (selection は
            // 優先度高いまま、 通常 audio 行は base bg のまま)。
            push_filled_rect(hctx, row, style.track_background_video);
        }
        // row 下端 separator
        push_filled_rect(
            hctx,
            Rect {
                x: row.x,
                y: row.y + row.h - style.lane_line_width_px,
                w: row.w,
                h: style.lane_line_width_px,
            },
            style.lane_line,
        );
    }
}

/// M14 Phase 89 (daw_01 #060): clip が実際に塗る `fill` の輝度から名前 / link glyph のテキスト色を
/// 自動選択する (SSoT = widget が唯一 fill を知る)。 `fill.a < 1.0` (share clip の半透明 fill 等) は
/// `lane_bg` と alpha 合成した実効色で判定する。 `style.clip_auto_contrast_text == false` の opt-out
/// 時は常に `clip_text_color` を返す。
///
/// r.md #48: clip 色は **ユーザーが自由に着色できる可変背景** なので、ここで選ぶのはテーマ従属の
/// `text` ではなく **極性固定インク** ([`Palette::ink_for`])。明暗 2 色を呼び出し側が渡す旧
/// `pick_contrast(bg, light, dark)` は廃止した (引数を取り違えて「どちらも暗色」になり、ライト
/// テーマでクリップ名が消える事故が構造的に起きなくなる)。輝度計算 / alpha 合成 / 閾値判定は
/// piano_roll の鍵盤ラベル (#093) と共有する SSoT。
pub(super) fn clip_text_color_for(
    p: &Palette,
    style: &ArrangementStyle,
    fill: Color,
    lane_bg: Color,
) -> Color {
    if !style.clip_auto_contrast_text {
        return style.clip_text_color;
    }
    clip_ink_for(p, fill, lane_bg)
}

/// r.md #73: **クリップ面の上に置く標識のインク**。
///
/// クリップの塗りはユーザー着色 / 選択の黄 (`clip_selected_fill = selection_warm`) /
/// レーン背景 (ライトテーマでは明るい) で変わる **可変背景**なので、極性固定インクを
/// そのまま置くと片方の極性で必ず消える。実効背景の輝度から極性を決めるのが唯一の解。
///
/// バッジ glyph (`⇌` / `+`) / ゲインのハンドル線 / ドラッグ中のゴーストラベルが
/// これを共有する。濃さ (alpha) だけは役割ごとに call site が決める
/// (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
#[must_use]
pub(super) fn clip_ink_for(p: &Palette, fill: Color, lane_bg: Color) -> Color {
    p.ink_for(daw_ui_core::color::composite_over(fill, lane_bg))
}

/// automation clip の矩形 (lane body 内、縦 padding 適用済)。
///
/// `draw_automation_lane` の描画と、cached 外の bend overlay
/// (`render::resolve_bend_segment`) が **同じ式**を使うための SSoT。
/// 2 か所で別式にすると、強調 / preview が base の 1px 隣に出たり
/// scissor が食い違ったりする (#028 user 指摘 2 と同じ形の再発)。
#[must_use]
pub(super) fn automation_clip_rect(
    body_rect: Rect,
    view: ArrangementView,
    clip_start_beat: f64,
    clip_len_beats: f64,
    style: &ArrangementStyle,
) -> Rect {
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let x = body_rect.x + ((clip_start_beat - view.start_beat) * beat_to_px) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let w = ((clip_len_beats * beat_to_px) as f32).max(2.0);
    let pad = style.automation_clip_v_pad_px;
    Rect { x, y: body_rect.y + pad, w, h: (body_rect.h - pad * 2.0).max(2.0) }
}

/// automation clip の `(fill, border)`。優先順は **selected > disabled > lane 識別色**。
///
/// M14 Phase 114 (daw_01 #086): automation clip は専用の `color` field を持たず `lane.color` が
/// fill / border の唯一 source (audio clip の `clip.color` に相当)。 `share_group_color` は fill /
/// border を上書きせず、 リンク識別は ⇌ glyph + #068 hover 強調のみが担う (#086)。 disabled lane は
/// **clip rect の fill / border のみ灰色** (= bypass marker)、 中身 (curve / point / clip 名) は元の
/// lane 識別色のままにして可読性を保つ (Bitwig / Live と同パターン、 #028 user 指摘 3)。
/// r.md #48: 識別色は lane 面に合わせて明度を寄せた `lane_ink` (ダークでは恒等)。
///
/// r.md #73: **cached 側の curve と cached 外の強調 / preview が共有する SSoT**。
/// 「その clip が実際に何色で塗られているか」を 1 か所で決めないと、
/// 強調の色を実効背景から導けない (= 塗りと同色になって中抜けする)。
#[must_use]
pub(super) fn automation_clip_colors(
    p: &Palette,
    lane: &ArrangementAutomationLane,
    is_selected: bool,
    style: &ArrangementStyle,
) -> (Color, Color) {
    if is_selected {
        return (style.clip_selected_fill, style.clip_selected_border);
    }
    if lane.enabled {
        let lane_ink = p.adapt_on(style.automation_lane_bg, lane.color);
        return (lane_ink.with_alpha(0.20), lane_ink);
    }
    // disabled: fill = 灰色 alpha 0.10 (lane_bg がほぼ透ける、 中身可読) + border = 灰色
    // alpha 1.0 (識別 marker、 不透明で確実に見える)。 fill alpha 0 だと renderer が rect 全体を
    // skip する可能性があるので非ゼロを保つ (#028 user 指摘 3 = 配色で見えない)。
    let dc = style.automation_lane_disabled_color;
    (Color { r: dc.r, g: dc.g, b: dc.b, a: 0.10 }, Color { r: dc.r, g: dc.g, b: dc.b, a: 1.0 })
}

/// automation clip の塗りを lane 面に合成した **実効背景**。
/// curve のインク・point dot の極性・bend 強調の色は、すべてこの輝度から決める。
#[must_use]
pub(super) fn automation_clip_eff_bg(fill: Color, style: &ArrangementStyle) -> Color {
    daw_ui_core::color::composite_over(fill, style.automation_lane_bg)
}

/// r.md #73: **automation clip の面の上に置くアクセント線の色**。
///
/// hover 強調と bend preview は「いまこの区間を触っている」を示す暖色だが、
/// **選択中の automation clip の塗り (`clip_selected_fill`) は同じ `selection_warm`** である
/// (`mod.rs` のトークン定義参照)。固定トークンをそのまま置くと、選択中のクリップの上では
/// 線の芯が塗りと完全に同化し、逆極性の縁取りだけが両側に残って **平行 2 本線**に見える
/// (r.md #73「曲げている最中に線が 2 重に見える」の実体。実ピクセルで確認済)。
///
/// 解き方はクリップ標識と同じ — 面の色から導く。`adapt_on` は色相・彩度を保ったまま
/// 図形コントラスト 3:1 を満たす明度まで寄せる (足りていれば恒等 = ダークの非選択では
/// 従来と 1 bit も変わらない)。これで「芯が背景に沈む」状態が原理的に作れなくなる
/// (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
#[must_use]
pub(super) fn automation_accent_on(p: &Palette, eff_bg: Color, accent: Color) -> Color {
    p.adapt_on(eff_bg, accent)
}

/// r.md #73: 選択中 automation point の dot の `(fill, border)`。
///
/// dot は automation clip の面の上に乗る。旧実装は fill / border とも `ink_on_dark`
/// (明インク) 固定で、**ライトテーマの明るいレーン**でも**クリップ選択中の黄**でも
/// 両方まとめて沈んでいた (縁が同色なので輪郭も残らない)。
///
/// 直し方は非選択の dot と同じ idiom — 非選択は `fill = ink_for(bg)` /
/// `border = ink_for(fill)` の **逆極性の縁**を持つので、可変背景の上でも輪郭が残る。
/// 選択 dot は「明るく大きい」という視覚言語を保ちたいので **fill は明インクのまま**、
/// 縁だけを逆極性にする。fill と border が逆極性である限り、
/// **どんな背景でも必ずどちらか一方が読める**。
#[must_use]
pub(super) fn automation_point_selected_colors(
    p: &Palette,
    style: &ArrangementStyle,
) -> (Color, Color) {
    let fill = style.automation_point_selected_fill;
    (fill, p.ink_for(fill))
}

/// clip が **実際に塗る** fill 色 (`clip.color` 既定 / muted の減光込み)。
///
/// r.md #45 / #46: clip の上に重ねる中身 (波形 / MIDI ノート / fade envelope) の
/// コントラストは、必ずこの「実際に塗られる背景色」 から導く
/// (`feedback_ui_indicator_contrast_on_variable_bg`)。 clip クロームを塗る
/// `draw_clip` / `draw_video_clip` と **同じ 1 本** にすることで、色を変える改修で
/// 片方だけ古い前提が残る事故を防ぐ。
///
/// 未着色 clip の既定色は track 種別で違う (audio = `clip_default_fill`、
/// video/image/text = `video_clip_loading`) ので `kind` を取る。
pub(super) fn clip_effective_fill(
    clip: &ClipView,
    kind: TrackKind,
    style: &ArrangementStyle,
) -> Color {
    let default = match kind {
        TrackKind::Video => style.video_clip_loading,
        TrackKind::Audio => style.clip_default_fill,
    };
    let base = clip.color.unwrap_or(default);
    if clip.muted { muted_dim_fill(base) } else { base }
}

/// r.md #45: clip 上に描く波形の (通常色, クリップピーク色)。
///
/// clip 色はユーザーが自由に着色できる **可変背景** なので、固定の寒色ブルーだけだと
/// 明るい clip 色 (既定パレットの黄 / アンバー等) の上で沈んで見えなくなる。 clip ラベル
/// (`clip_text_color_for`) や MIDI ノートプレビュー (r.md #20) と同じ WCAG 輝度 2 択で、
/// 暗背景用 / 明背景用を選ぶ。
///
/// r.md #48: 2 択そのものは [`Palette::waveform_for`] が持つ (色相 = 寒色ブルー / 警告赤 は
/// 保ったまま明暗だけ切り替わる極性固定インク)。呼び出し側が明暗 2 色を渡す旧 API は廃止。
pub(super) fn waveform_colors_for(
    p: &Palette,
    clip_bg: Color,
    lane_bg: Color,
    is_selected: bool,
) -> (Color, Color) {
    let bg = daw_ui_core::color::composite_over(clip_bg, lane_bg);
    let body = if is_selected { WaveformInk::Selected } else { WaveformInk::Normal };
    (p.waveform_for(bg, body), p.waveform_for(bg, WaveformInk::Peak))
}

/// r.md #46: fade envelope / 掴む正方形の (前景色, 裏打ち色)。
///
/// fade は clip 色の上にも **波形の上にも**乗るので、単層だと下地次第で消える。
/// 前景は背景輝度から明暗 2 択、裏打ちはその逆極性を敷いて、どちらの下地でも
/// 縁が立つようにする (無音ベースライン / slice 区切り線と同じ 2 層 idiom)。
///
/// r.md #48: 極性の判定は [`Palette::ink_for`] 1 本に畳んだ。実際に引く 2 色は線の太さに
/// 合わせた alpha 込みの style 値 (明インク / 暗インクで濃さが違う) なので、ink_for が選んだ
/// 極性で **前景と裏打ちの役割を入れ替える** だけにしてある。
pub(super) fn fade_colors_for(
    p: &Palette,
    style: &ArrangementStyle,
    clip_bg: Color,
    lane_bg: Color,
) -> (Color, Color) {
    let bg = daw_ui_core::color::composite_over(clip_bg, lane_bg);
    let light = style.audio_fade_overlay_color;
    let dark = style.audio_fade_overlay_color_dark;
    if p.ink_for(bg) == p.ink_on_bright {
        // 明るい下地 → 暗インクを前景に、明インクを裏打ちに。
        (dark, light)
    } else {
        (light, dark)
    }
}

/// 1 つの clip に敷くサムネイルタイルの上限。
///
/// 可視域カリング後の枚数なので通常は 2 桁に収まる (16:9 / 行高 46px なら 1 枚 82px、
/// lanes 幅 1920px でも 24 枚)。 上限に当たるのは「極端に縦長のソース × 高ズーム」
/// だけで、 そのとき残りはタイルを描かず base fill のまま残る (= 黙って全部描くのを
/// やめる代わりに、 描けた分は正しい位相で並ぶ)。
pub(super) const MAX_THUMBNAIL_TILES: u32 = 512;

/// M14 Phase 72 (daw_01 #044) / r.md #68: video / image clip のサムネイル敷き詰め方。
///
/// **clip 矩形にフィットさせない**。 サムネイルも clip の「中身」 なので、
/// (a) 1 枚の寸法は **行高 × native aspect** で決まり clip の長さには一切依存しない、
/// (b) 並べる位相は **content 原点**に固定する、 (c) はみ出す分は clip 矩形で切り抜く。
///
/// 旧実装 (`aspect_fit_rect`) は clip 矩形の中央に 1 枚を letterbox 配置していたため、
/// (a) clip を伸ばすとサムネイルが水平に滑り、 (b) 細い clip では拡大縮小し、
/// (c) 長い clip を横スクロールして先頭が画面外に出ると 1 枚も見えなくなっていた。
///
/// 敷き詰めは REAPER の **Preferences → Video/REX/Misc → still image thumbnail
/// display mode = "Center/tile image"** と同じ流儀 (同じ 1 枚を隙間なく繰り返す)。
/// 位相を clip の左端ではなく content 原点に取るのが肝で、 これにより
/// 「トリムしてもタイルの絶対時間位置が変わらない」 = 絵が滑らない が成り立つ
/// (左端 trim は `start_beat` と `content_offset_beats` が同量動くので content 原点は不変)。
///
/// `visible_x0` / `visible_x1` は描画対象の可視域 (= `clip_rect ∩ lanes` の x 範囲)。
/// ここでカリングするので、 長い clip でもタイル数は画面幅で頭打ちになる。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ThumbnailTiling {
    /// 可視域に掛かる **1 枚目** の左端 x (content 原点からの整数枚目)。
    pub first_x: f32,
    /// タイル 1 枚の幅 px (= 行高 × native aspect)。
    pub tile_w: f32,
    /// 高さ px (= clip 矩形の高さ)。
    pub tile_h: f32,
    /// 描く枚数 ([`MAX_THUMBNAIL_TILES`] で頭打ち)。
    pub count: u32,
    /// 上限で打ち切ったか (= 可視域を覆いきれていない)。
    pub truncated: bool,
}

/// r.md #68: サムネイルの敷き詰め幾何。 可視域が無い / 退化した寸法では `None`。
///
/// - `tex_w` / `tex_h` = 0 は 1 に clamp (`u32` の 0 を許容しつつ ZeroDiv を回避)
/// - `clip_rect.h` 0 近傍も 0.001 に clamp (= 0px 高さの異常 case で幅を 0 に押さえる)
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub(super) fn thumbnail_tiling(
    clip_rect: Rect,
    visible_x0: f32,
    visible_x1: f32,
    map: ContentMap,
    thumb: ClipThumbnail,
) -> Option<ThumbnailTiling> {
    if visible_x1 <= visible_x0 {
        return None;
    }
    let texture_width = thumb.width.max(1) as f32;
    let texture_height = thumb.height.max(1) as f32;
    let tile_h = clip_rect.h.max(0.001);
    let tile_w = tile_h * (texture_width / texture_height);
    if !tile_w.is_finite() || tile_w <= 0.0 {
        return None;
    }
    // 位相の原点 = この thumbnail が表す event の content 上の開始拍。
    let origin_x = map.x(thumb.start_in_content_beats);
    // 可視域に掛かる最初の整数枚目 (原点より左でも負の index で正しく続く)。
    let k0 = ((visible_x0 - origin_x) / tile_w).floor();
    let first_x = origin_x + k0 * tile_w;
    let span = visible_x1 - first_x;
    // 壊れた project (NaN の start_beat 等) で NaN 座標の quad を積まない。
    // `NaN <= 0.0` は false をすり抜けるので、 有限性を明示的に見る。
    if !first_x.is_finite() || !span.is_finite() || span <= 0.0 {
        return None;
    }
    let needed = (span / tile_w).ceil().max(1.0);
    let cap = f32::from(u16::try_from(MAX_THUMBNAIL_TILES).unwrap_or(u16::MAX));
    Some(ThumbnailTiling {
        first_x,
        tile_w,
        tile_h,
        count: if needed >= cap { MAX_THUMBNAIL_TILES } else { needed as u32 },
        truncated: needed > cap,
    })
}

// ---------------------------------------------------------------------------
// S4b Phase C: clip 上端ラベル帯 + 中身 (波形 / MIDI プレビュー) の共有インセット
// ---------------------------------------------------------------------------

/// clip 名の上パディング (px)。name は `r.y + CLIP_NAME_PAD_TOP` に描く。
pub(super) const CLIP_NAME_PAD_TOP: f32 = 2.0;

/// r.md #24: drag preview (中身入りコピー) の fill 半透明度。 元 clip 色の alpha に乗じて「動いて
/// いるコピー」 と分かる薄さにする (中身の波形 / MIDI / thumbnail は上に重ねて視認性を保つ)。 元
/// clip はその場に不透明のまま残るので、 薄い方が「掴んで動かしている複製」 と直感的に読める。
pub(super) const DRAG_PREVIEW_FILL_ALPHA: f32 = 0.6;

/// clip の中身 (波形 / MIDI ノートプレビュー) が始まる上インセット (px)。
///
/// ラベル帯 (`CLIP_NAME_PAD_TOP` + 文字高 `style.clip_text_size` + 1px gap) の直下から
/// 始まる。 **name 帯と同じ値から導出する唯一の SSoT** で、 旧実装 (widget が name 帯を
/// `clip_text_size` から算出し、 app 側 overlay が `inset_top = 14.0` を hardcode する二重
/// 同期バグ) を解消する。 `clip_text_size` を変えても波形 / MIDI が自動追従する。
pub(super) fn clip_content_inset_top(style: &ArrangementStyle) -> f32 {
    CLIP_NAME_PAD_TOP + style.clip_text_size + 1.0
}

/// MIDI mini-preview 用のノート 1 件 (widget 内描画に必要な最小フィールド)。
#[derive(Clone, Copy)]
pub(super) struct MidiNoteDraw {
    pub(super) pitch: u8,
    /// **content-local** 開始拍 (r.md #68。 窓の offset は [`ContentMap`] が持つ)。
    pub(super) start_beat: f64,
    pub(super) duration_beats: f64,
    pub(super) velocity: u8,
}

/// clip 内の audio event 1 件ぶんの波形描画データ (r.md #41)。
///
/// `spans` は [`common::audio_render::event_wave_spans`] が返す「実際に鳴る
/// event-local 拍区間 → source 範囲」 の列。 engine と同じ時間写像なので、 これを
/// そのまま並べれば Slice のスライス配置 / gap も warp 区間も逆再生も正しく出る。
pub(super) struct AudioEventDraw {
    pub(super) buffer: Arc<AudioSourceBuffer>,
    pub(super) source_id: u32,
    /// 波形 widget の id 弁別子 (= `AudioEvent.id`、 未採番なら model index 由来)。
    /// LOD ピラミッドはこの id で frame を跨いで保持されるので、 decode 完了で
    /// 描画対象の並びが変わっても入れ替わらない安定値でなければならない。
    pub(super) key: u64,
    /// event の **content-local** 開始拍 (複数 event / 分割 clip に対応)。
    /// r.md #68: 窓ローカルではなく content-local (= model の
    /// `AudioEvent::event_start_in_clip_beats` そのもの)。 窓の offset は
    /// [`ContentMap`] の原点側が持つ。
    pub(super) start_in_clip_beats: f64,
    /// event の長さ (拍)。 span が張られていない末尾 (= 鳴り終わったあと) を
    /// 無音ベースラインで示すのに使う。
    pub(super) len_beats: f64,
    pub(super) stretch_mode: common::model::StretchMode,
    pub(super) spans: Vec<common::audio_render::WaveSpan>,
}

/// clip rect 内に重ねる中身 (波形 / MIDI)。`&AppData` (model + audio cache) から 1 フレーム分だけ
/// 集めて heavy closure に move する (`Arc<AudioSourceBuffer>` は refcount clone で安価)。
pub(super) enum ClipContentDraw {
    /// clip 内の **全** audio event (旧実装は先頭 1 件だけを描いており、 分割 / glue で
    /// 複数 event になった clip は 2 件目以降が見えなかった)。
    Audio { events: Vec<AudioEventDraw> },
    /// r.md #68: `len_beats` (= clip の長さ) は持たない。 ノートの x 写像に clip 長を
    /// 使っていたのが「トリムなのに中身がストレッチ表示される」 の主因だった
    /// (ゴーストは drag 後の矩形 × drag 前の長さで描いていたので、 ちょうど
    /// `stretch_remap` と pixel 一致する time-stretch の絵になっていた)。
    Midi { notes: Vec<MidiNoteDraw> },
}

/// clip 名 + (share clip なら) 名前左の link glyph を描く共通 helper (audio / video 経路で共有)。
/// `text_color` は呼び出し側が実 fill から `clip_text_color_for` で導出済を渡す。 `has_link == true` で
/// `share_group_link_glyph` (⇌) を clip 名と 1 つの text run に統合して描く。 文字を描けない小ささ
/// (`r.w <= 24` or `r.h <= clip_text_size + 2`) では何も描かない。
pub(super) fn draw_clip_label<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    name: &Arc<str>,
    has_link: bool,
    text_color: Color,
    style: &ArrangementStyle,
) {
    if !(r.w > 24.0 && r.h > style.clip_text_size + 2.0) {
        return;
    }
    // M14 Phase 126 (daw_01 #104): share clip の link glyph (⇌) と clip 名を 1 つの text run に統合。
    // 旧実装は name の left を `r.x + 4.0 + clip_text_size + 2.0` に置き、glyph 幅を実 advance ではなく
    // `clip_text_size` (= font size = em 幅) で近似 + 固定 `+2.0` パッドを足していたため、⇌ の実描画幅より
    // 広く名前を右送りして二重に隙間が空いていた。glyph / name は同色・同 font_size・同 top・同 clip_rect
    // なので 1 run に統合してレイアウトエンジンに advance を委ねれば情報を失わず隙間が消える。
    // `has_link == false` は従来どおり name のみ。
    let text: Arc<str> = if has_link {
        Arc::from(format!("{}{name}", style.share_group_link_glyph))
    } else {
        name.clone()
    };
    hctx.push_text(GlyphArea {
        text,
        left: r.x + 4.0,
        top: r.y + CLIP_NAME_PAD_TOP,
        font_size: style.clip_text_size,
        line_height: style.clip_text_size * 1.2,
        color: text_color,
        clip_rect: Some(r),
        ..GlyphArea::default()
    });
}

/// S4b Phase C: 1 つの audio clip rect 内に波形を描く (旧 app 側 clip 波形 overlay の
/// widget 内版)。 `inset_top` はラベル帯と共有する [`clip_content_inset_top`] の値。
///
/// r.md #41: clip 内の **全 audio event** を、 それぞれ
/// [`common::audio_render::event_wave_spans`] が返す span 列で描く。 「content 拍 → x」 は
/// `map` 1 本で、 mode 別の分岐は持たない
/// (Slice のスライス配置 / gap、 warp 区間、 逆再生はすべて span 側の情報)。
/// span の無い区間 (= 無音) には薄いベースラインを引き、 Slice はスライス頭に
/// 区切り線を出す。
///
/// r.md #68: x 写像は [`ContentMap`] (= ビューのズーム) のみ。 旧実装は
/// `content.w / clip_len_beats` = 「インセット済み表示幅 ÷ クリップ長」 で、
/// (a) 波形がルーラーのグリッドと 2px ずれ、 (b) 原点が窓の左端だったため左端 drag で
/// 波形が掴んだ端に付いて滑っていた (確定時は `content_offset_beats += δ` で絶対時間に
/// 留まるので preview ≠ commit)。 clip 矩形は縦レイアウトと切り抜きにだけ使う。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_clip_waveform_inner<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    key: ClipKey,
    clip_rect: Rect,
    // content-local 拍 → 画面 x (clip の表示幅には依存しない)。
    map: ContentMap,
    events: &[AudioEventDraw],
    is_selected: bool,
    lanes: Rect,
    inset_top: f32,
    // r.md #45: この clip が実際に塗られている色 (`clip_effective_fill`)。
    // 波形色をここからの auto-contrast で選ぶ。
    clip_bg: Color,
    style: &ArrangementStyle,
    // waveform widget の id 弁別子。 base 描画 (content path) は `"audio_clip_wf"`、 drag ghost は
    // 別 tag を渡す。 `hctx.waveform_segments` は id ごとに LOD 状態を持つので、 同一 (track, clip)
    // の波形を 1 フレームで 2 度描く (元 clip + ghost) 場合に同 id だと state 衝突 → LOD が毎フレーム再構築される。
    wf_id_tag: &'static str,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};

    let p = hctx.palette();
    let inset_lr: f32 = 2.0;
    let content = Rect {
        x: clip_rect.x + inset_lr,
        y: clip_rect.y + inset_top,
        w: (clip_rect.w - inset_lr * 2.0).max(0.0),
        h: (clip_rect.h - inset_top - inset_lr).max(0.0),
    };
    if content.w <= 0.0
        || content.h <= 0.0
        || !map.px_per_beat.is_finite()
        || map.px_per_beat <= 0.0
        || events.is_empty()
    {
        return;
    }
    // 波形の scissor = clip の中身領域 ∩ lanes (widget 側のセグメントカリングも
    // この rect を使うので、 画面外にはみ出す span の pixel ループが有界になる)。
    let cx0 = content.x.max(lanes.x);
    let cy0 = content.y.max(lanes.y);
    let cx1 = (content.x + content.w).min(lanes.x + lanes.w);
    let cy1 = (content.y + content.h).min(lanes.y + lanes.h);
    if cx1 <= cx0 || cy1 <= cy0 {
        return;
    }
    let scissor = Rect { x: cx0, y: cy0, w: cx1 - cx0, h: cy1 - cy0 };

    // r.md #45: 波形色は clip の実塗り色からの auto-contrast (固定ブルーだと
    // 明るい clip 色の上で消えていた)。 alpha は従来どおり選択で少し濃く。
    let (base_fg, fg_clipped) = waveform_colors_for(p, clip_bg, style.bg, is_selected);
    let fg = base_fg.with_alpha(if is_selected { 0.95 } else { 0.85 });
    let wstyle = WaveformStyle {
        fg,
        fg_clipped,
        fill: None,
        baseline: None,
        channel_layout: ChannelLayout::Overlay,
        render_mode: WaveformRenderMode::Auto,
        line_width_px: 1.0,
    };
    // 無音区間のベースライン (= 波形が 0 の直線) と slice 区切り線。
    // clip 色はユーザーが任意色に設定できる可変背景なので、 中央線も区切り線も
    // 暗い backing + 明色の 2 層で描く (`feedback_ui_indicator_contrast_on_variable_bg`)。
    // 単層だと明るい clip 色 (既定パレットの黄 / アンバー等) の上で消え、
    // 「隙間があるのか描画が抜けているのか」 が判別できなくなる。
    // r.md #48: 裏打ちは「常に暗い」 ことが意味そのものなので極性固定の `scrim`
    // (テーマを切り替えても反転しない)。 濃さだけは線の役割ごとに call site で決める。
    let silent_color = fg.with_alpha(0.85);
    let backing_color = p.scrim.with_alpha(0.55);
    let mid_y = content.y + content.h * 0.5;
    let mut silent_backing: Vec<LineSegment> = Vec::new();
    let mut silent: Vec<LineSegment> = Vec::new();
    let mut div_backing: Vec<LineSegment> = Vec::new();
    let mut div_bright: Vec<LineSegment> = Vec::new();

    hctx.with_clip_rect(scissor, |hctx| {
        for ev in events {
            let x_at = |beat: f64| map.x(ev.start_in_clip_beats + beat);
            let ev_x0 = x_at(0.0);
            let mut segs: Vec<WaveformSegment> = Vec::with_capacity(ev.spans.len());
            let show_dividers = ev.spans.len() > 1
                && matches!(ev.stretch_mode, common::model::StretchMode::Slice);
            let mut divider_xs: Vec<f32> = Vec::new();
            let mut prev_end_x: Option<f32> = None;
            let mut push_silent = |x0: f32, x1: f32| {
                silent_backing.push(LineSegment {
                    a: [x0, mid_y],
                    b: [x1, mid_y],
                    color: backing_color,
                });
                silent.push(LineSegment {
                    a: [x0, mid_y],
                    b: [x1, mid_y],
                    color: silent_color,
                });
            };
            for sp in &ev.spans {
                let x0 = x_at(sp.start_beat);
                let x1 = x_at(sp.end_beat);
                // 1px 未満のスライスも「そこに音がある」 ことは描く (0 幅だと消える)。
                let w = (x1 - x0).max(1.0);
                segs.push(WaveformSegment {
                    rect: Rect { x: x0, y: content.y, w, h: content.h },
                    view: WaveformView {
                        start_sample: sp.source_start,
                        len_samples: sp.source_end.saturating_sub(sp.source_start).max(1),
                        vertical_gain: 1.0,
                        reversed: sp.reversed,
                    },
                });
                if let Some(pe) = prev_end_x
                    && x0 > pe + 0.5
                {
                    push_silent(pe, x0);
                }
                // 区切り線は「スライスとスライスの間」 に出す。 tempo 曲線で分割された
                // 継続 span (`head == false`) は音の切れ目ではないので対象外。
                // event 左端と一致する先頭スライスの頭は clip 境界そのものなので出さない。
                if show_dividers && sp.head && (x0 - ev_x0).abs() > 0.5 {
                    divider_xs.push(x0);
                }
                prev_end_x = Some(x1);
            }
            // 密なスライスは線だけで領域が埋まるので間引く (audio editor と同規約)。
            for x in crate::widgets::thin_slice_dividers(divider_xs) {
                div_backing.push(LineSegment {
                    a: [x, content.y],
                    b: [x, content.y + content.h],
                    color: p.scrim.with_alpha(0.65),
                });
                div_bright.push(LineSegment {
                    a: [x, content.y],
                    b: [x, content.y + content.h],
                    color: style.slice_divider_color,
                });
            }
            // event 冒頭 / 末尾の無音 (= 最初の trigger 前 / 鳴り終わり後) もベースラインで示す。
            let ev_x1 = x_at(ev.len_beats);
            if let (Some(first), Some(last)) = (segs.first(), prev_end_x) {
                if first.rect.x > ev_x0 + 0.5 {
                    push_silent(ev_x0, first.rect.x);
                }
                if ev_x1 > last + 0.5 {
                    push_silent(last, ev_x1);
                }
            }
            if segs.is_empty() {
                continue;
            }
            // SampleSlices::Planar 用の &[&[f32]] (毎フレーム alloc は GUI 描画 path なので許容)。
            let planes_borrowed: Vec<&[f32]> =
                ev.buffer.samples.iter().map(Vec::as_slice).collect();
            let source = WaveformSource {
                samples: SampleSlices::Planar(&planes_borrowed),
                valid_len: ev.buffer.frames as usize,
                generation: u64::from(ev.source_id),
                sample_rate: ev.buffer.sample_rate,
            };
            let _ = hctx.waveform_segments(
                (wf_id_tag, key.track, key.clip, ev.key),
                source,
                &segs,
                wstyle,
            );
        }
        if !silent.is_empty() {
            hctx.push_lines(LineBatch {
                segments: Arc::<[LineSegment]>::from(silent_backing),
                line_width_px: 2.0,
                clip_rect: None,
            });
            hctx.push_lines(LineBatch {
                segments: Arc::<[LineSegment]>::from(silent),
                line_width_px: 1.0,
                clip_rect: None,
            });
        }
        // 可変背景 (clip 色 / 波形) 上でも読めるよう暗い backing + 明色の 2 層
        // (`feedback_ui_indicator_contrast_on_variable_bg`)。 warp marker
        // (audio editor の空色 3px/1.5px) とは色・太さで区別する。
        if !div_backing.is_empty() {
            hctx.push_lines(LineBatch {
                segments: Arc::<[LineSegment]>::from(div_backing),
                line_width_px: 2.0,
                clip_rect: None,
            });
            hctx.push_lines(LineBatch {
                segments: Arc::<[LineSegment]>::from(div_bright),
                line_width_px: 1.0,
                clip_rect: None,
            });
        }
    });
}

/// S4b Phase C: 1 つの MIDI clip rect 内にノートプレビューを描く (旧 app 側
/// 旧 app 側 clip MIDI overlay の widget 内版、 共有 `inset_top`)。`clip_bg` は clip の実効塗り色
/// (selected なら `clip_selected_fill`)、 ノート色は背景輝度から auto-contrast で選ぶ。
///
/// r.md #68: 横方向の写像は [`ContentMap`] (= ビューのズーム) だけ。 clip 矩形は
/// (a) 縦レイアウト (pitch レンジ) と (b) 左右の切り抜きにしか使わない。 旧実装の
/// `view_rect.w / clip_len_beats` は「表示幅 ÷ クリップ長」 そのもので、 ドラッグ
/// ゴーストがこれに drag 前の長さを渡していたためトリム中に time-stretch の絵になっていた。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_clip_midi_inner<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    notes: &[MidiNoteDraw],
    map: ContentMap,
    clip_bg: Color,
    style: &ArrangementStyle,
    lanes_x: f32,
    inset_top: f32,
) {
    if notes.is_empty() {
        return;
    }
    let p = hctx.palette();
    let inset_lr: f32 = 2.0;
    let inset_bottom: f32 = 2.0;
    let view_rect = Rect {
        x: clip_rect.x + inset_lr,
        y: clip_rect.y + inset_top,
        w: (clip_rect.w - inset_lr * 2.0).max(0.0),
        h: (clip_rect.h - inset_top - inset_bottom).max(0.0),
    };
    if view_rect.w <= 0.0 || view_rect.h <= 0.0 {
        return;
    }
    let visible_left = lanes_x.max(view_rect.x);
    let visible_right = view_rect.x + view_rect.w;
    if visible_right <= visible_left {
        return;
    }
    let mut min_pitch: u8 = 127;
    let mut max_pitch: u8 = 0;
    for n in notes {
        if n.pitch < min_pitch {
            min_pitch = n.pitch;
        }
        if n.pitch > max_pitch {
            max_pitch = n.pitch;
        }
    }
    let pad: u8 = 2;
    let min_p = min_pitch.saturating_sub(pad);
    let max_p = max_pitch.saturating_add(pad).min(127);
    let pitch_span = (i32::from(max_p) - i32::from(min_p)).max(1) as f32;
    let row_h = (view_rect.h / pitch_span).max(1.0);
    let effective_bg = daw_ui_core::color::composite_over(clip_bg, style.bg);
    // r.md #48: ノートが乗るのはユーザー着色の clip = 可変背景なので、テーマ従属の `text`
    // ではなく極性固定インク (`ink_for`)。 ライトテーマでも「明 clip には暗ノート」 が保たれる。
    let base_fill = p.ink_for(effective_bg);
    for n in notes {
        let nx = map.x(n.start_beat);
        let nw = map.w(n.duration_beats).max(1.0);
        let drawn_x = nx.max(visible_left);
        let drawn_x_end = (nx + nw).min(visible_right);
        if drawn_x_end <= drawn_x {
            continue;
        }
        let row_from_top = (i32::from(max_p) - i32::from(n.pitch)).clamp(0, pitch_span as i32) as f32;
        let ny = view_rect.y + row_from_top * row_h;
        if ny + row_h <= view_rect.y || ny >= view_rect.y + view_rect.h {
            continue;
        }
        let mut fill = base_fill;
        let v = (f32::from(n.velocity) / 127.0).clamp(0.0, 1.0);
        fill.a = 0.5 + v * 0.5;
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: drawn_x,
                y: ny,
                w: (drawn_x_end - drawn_x).max(1.0),
                h: row_h.max(1.0),
            },
            fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
}

/// M14 Phase 127 (#105): section 帯 1 件を描く (色 fill + border + 名前ラベル、 clip ラベルと同
/// 左寄せ + 4px inset + auto-contrast idiom)。 名前が空 / 帯が狭いときはラベルを省く。
/// M14 Phase 128 (#106): `selected` の帯は選択 clip と同 idiom の明るい太枠 (`clip_selected_border`)、
/// 非選択は neutral 1px (`clip_border`)。
pub(super) fn draw_section_band<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    name: &Arc<str>,
    color_rgb: [f32; 3],
    r: Rect,
    selected: bool,
    style: &ArrangementStyle,
) {
    let fill = Color::rgb(color_rgb[0], color_rgb[1], color_rgb[2]);
    push_filled_rect(hctx, r, fill);
    // 選択帯は fill 色に依らず見える 2 重リング (clip と同 idiom、 帯は
    // 角丸 0)。 非選択は neutral な 1px 枠。 これで白 / 黄の section でも選択が判別できる。
    if selected {
        push_selection_ring(hctx, r, style, 0.0, Some(r));
    } else {
        push_section_border(hctx, r, style.clip_border, 1.0);
    }
    if r.w > 8.0 && r.h > style.clip_text_size + 2.0 && !name.is_empty() {
        let text_color = clip_text_color_for(hctx.palette(), style, fill, style.arranger_lane_bg);
        hctx.push_text(GlyphArea {
            // 毎フレーム描画なので `Arc::from(&str)` (byte copy) でなく Arc refcount clone
            // (draw_clip_label と同じ安価経路、 section 数が多い曲で per-frame alloc を避ける)。
            text: name.clone(),
            left: r.x + 4.0,
            top: r.y + (r.h - style.clip_text_size) * 0.5,
            font_size: style.clip_text_size,
            line_height: style.clip_text_size * 1.2,
            color: text_color,
            clip_rect: Some(r),
            ..GlyphArea::default()
        });
    }
}

/// M14 Phase 127 (#105): section 帯 / preview の border (M14 Phase 128 で width 可変化 = selected 太枠)。
pub(super) fn push_section_border<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    border: Color,
    width: f32,
) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill: Color::TRANSPARENT,
        border,
        border_width: width,
        radius: [0.0; 4],
        clip_rect: Some(r),
    });
}

/// 選択枠を fill 色に依存せず描く 2 重リング。 clip / video clip /
/// section 帯の選択表示に共通で使う。 呼び出し側は fill を **clip 本来の色**で
/// 描き、 ここは枠だけを重ねる: 外側の明線 (`clip_selected_border`) と内側の暗線
/// (`clip_selected_border_inner`) は **極性固定** のペア (r.md #48 の
/// `selection_ring_outer` / `selection_ring_inner`) なので、 暗い lane 背景でも
/// 黄 / 白の明るい fill でも必ずどちらかが立つ。 どんな clip 色 (選択色と同色を
/// 含む) でも、 どのテーマでも選択を判別できる。
/// `radius` は枠の角丸 (clip = `clip_radius`、 section 帯 = 0)。
/// rect 内側に描く SDF ボーダー (rect.wgsl) なので、 外側リングを `r`、 内側
/// リングを `r` を線幅ぶん inset した矩形に描けば 2 本が隣接して並ぶ。
pub(super) fn push_selection_ring<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    style: &ArrangementStyle,
    radius: f32,
    clip_rect: Option<Rect>,
) {
    let w = style.clip_selected_border_w;
    // 外側: 明線 (暗い lane 背景 / 暗い clip 色に対して光る)。
    hctx.push_rect(RectCommand {
        rect: r,
        fill: Color::TRANSPARENT,
        border: style.clip_selected_border,
        border_width: w,
        radius: [radius; 4],
        clip_rect,
    });
    // 内側: 暗線 (黄 / 白など明るい fill に対してコントラスト)。 r を線幅ぶん
    // inset し角丸も縮める。 inset が潰れる極小 clip では外側リングのみ。
    let inner = Rect { x: r.x + w, y: r.y + w, w: r.w - w * 2.0, h: r.h - w * 2.0 };
    if inner.w > 0.0 && inner.h > 0.0 {
        let ir = (radius - w).max(0.0);
        hctx.push_rect(RectCommand {
            rect: inner,
            fill: Color::TRANSPARENT,
            border: style.clip_selected_border_inner,
            border_width: w,
            radius: [ir; 4],
            clip_rect,
        });
    }
}

/// M14 Phase 127 (#105): drag 中対象 section の preview `(start, len)` を返す (Move/Resize。 非対象 /
/// Create / Ctrl+drag (複製) の元帯は base を返す = 複製は元帯を残し ghost を別途描く)。 draw と release が
/// 同じ `compute_section_drag_beat_delta` を通すことで overlay == commit を保証する。
pub(super) fn section_preview_start_len(
    s: &SectionView,
    // r.md #71: Move の落とし先解決に他帯の位置が要る (食い込む位置は境界へ寄せる)。
    sections: &[SectionView],
    section_drag: Option<SectionDragSession>,
    beat_per_px: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> (f64, f64) {
    let Some(sd) = section_drag else {
        return (s.start_beat, s.len_beats);
    };
    if sd.kind == SectionGesture::Create || sd.section_id != s.id {
        return (s.start_beat, s.len_beats);
    }
    let raw = f64::from(sd.last_mouse.0 - sd.anchor_mouse.0) * beat_per_px;
    let delta = compute_section_drag_beat_delta(&sd, raw, snap, zoom_x_px_per_beat);
    match sd.kind {
        SectionGesture::Move => {
            if sd.last_ctrl {
                (s.start_beat, s.len_beats)
            } else {
                // r.md #71: release commit と **同じ** `section_move_dest` を通す。
                // 片方だけ解決すると「見えていた位置と違う所に落ちる」。
                (section_move_dest(sections, &sd, delta), s.len_beats)
            }
        }
        SectionGesture::ResizeLeft => {
            let right = sd.anchor_start + sd.anchor_len;
            let ns = (sd.anchor_start + delta).clamp(0.0, (right - SECTION_MIN_LEN_BEATS).max(0.0));
            (ns, (right - ns).max(SECTION_MIN_LEN_BEATS))
        }
        SectionGesture::ResizeRight => {
            (sd.anchor_start, (sd.anchor_len + delta).max(SECTION_MIN_LEN_BEATS))
        }
        SectionGesture::Create => (s.start_beat, s.len_beats),
    }
}

/// M14 Phase 127 (daw_01 #105): Arranger レーン全体 (背景 + "Arranger" 見出し + section 色帯群 + drag
/// preview) を描く overlay helper。 loop band と同じく cached 外で毎フレーム描画する (section データ
/// 変化に cache busting 不要、 selection / loop band と同流儀)。 drag 中は対象 section を
/// `section_preview_start_len` の preview geometry で描き、 overlay == release commit を helper 共有で
/// 構造保証する。 Ctrl+drag (複製) は元帯 + 複製先 ghost、 範囲 drag (Create) は preview 帯を描く。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_sections_lane<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    sections: &[SectionView],
    section_drag: Option<SectionDragSession>,
    view: ArrangementView,
    arranger: Rect,
    arranger_header: Rect,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
    style: &ArrangementStyle,
) {
    if arranger.h <= 0.0 {
        return;
    }
    // 背景 + header 見出し ("Arranger")。
    if arranger_header.w > 0.0 {
        push_filled_rect(hctx, arranger_header, style.arranger_lane_bg);
        if arranger_header.h > style.clip_text_size + 2.0 {
            hctx.push_text(GlyphArea {
                text: Arc::from("Arranger"),
                left: arranger_header.x + 4.0,
                top: arranger_header.y + (arranger_header.h - style.clip_text_size) * 0.5,
                font_size: style.clip_text_size,
                line_height: style.clip_text_size * 1.2,
                color: style.arranger_label_color,
                clip_rect: Some(arranger_header),
                ..GlyphArea::default()
            });
        }
    }
    push_filled_rect(hctx, arranger, style.arranger_lane_bg);

    let beat_per_px = view.len_beats / f64::from(arranger.w.max(1.0));
    hctx.with_clip_rect(arranger, |hctx| {
        // r.md #70: **base パス → drag パス** の 2 パス。 掴んでいる帯を base ループから
        // 除いて最後に描く。
        //
        // レンダラは call order = z-order (`ui/crates/renderer/src/scene.rs`: 要素単位の z
        // 指定は無い)。 `sections` は `Song::normalize_sections` が `start_beat` 昇順に
        // 正規化した順なので、 帯を右へ動かす / 右端を右へ伸ばすと、 重なる相手 (= より
        // 後に始まる帯) が **後から** 不透明 fill で描かれ、 掴んでいる帯を食っていた
        // (`draw_section_band` は帯名を preview rect にクリップするので、 最初に消えるのは
        // 帯名)。 clip / track 並べ替え / fade の各 drag は既に 2 パス構造なので、 それに揃える。
        let dragged_id = section_drag
            .filter(|sd| sd.kind != SectionGesture::Create)
            .map(|sd| sd.section_id);
        let draw_band = |hctx: &mut HeavyCtx<'_, '_, M>, s: &SectionView| {
            let (start, len) = section_preview_start_len(
                s,
                sections,
                section_drag,
                beat_per_px,
                snap,
                zoom_x_px_per_beat,
            );
            let r = section_rect_from(start, len, view, arranger);
            draw_section_band(hctx, &s.name, s.color, r, s.selected, style);
        };
        // base パス: drag 対象以外を元の並び順で。
        for s in sections {
            if Some(s.id) == dragged_id {
                continue;
            }
            draw_band(hctx, s);
        }
        // drag パス: 掴んでいる帯を最前面に。
        if let Some(id) = dragged_id
            && let Some(s) = sections.iter().find(|s| s.id == id)
        {
            draw_band(hctx, s);
        }
        let Some(sd) = section_drag else {
            return;
        };
        let raw = f64::from(sd.last_mouse.0 - sd.anchor_mouse.0) * beat_per_px;
        let delta = compute_section_drag_beat_delta(&sd, raw, snap, zoom_x_px_per_beat);
        match sd.kind {
            // Ctrl+drag (複製): 複製先に半透明 ghost 帯。
            SectionGesture::Move if sd.last_ctrl => {
                // r.md #71 同件: release commit と同じ `section_duplicate_dest`。
                let dest = section_duplicate_dest(sections, &sd, delta);
                let r = section_rect_from(dest, sd.anchor_len, view, arranger);
                push_filled_rect(hctx, r, style.arranger_preview_fill);
                push_section_border(hctx, r, style.clip_border, 1.0);
            }
            // 範囲 drag (Create): まだ存在しない section の preview 帯。
            SectionGesture::Create => {
                let other = (sd.anchor_press_beat + delta).max(0.0);
                let lo = sd.anchor_press_beat.min(other);
                let hi = sd.anchor_press_beat.max(other);
                if hi > lo {
                    let r = section_rect_from(lo, hi - lo, view, arranger);
                    push_filled_rect(hctx, r, style.arranger_preview_fill);
                    push_section_border(hctx, r, style.clip_border, 1.0);
                }
            }
            _ => {}
        }
    });
}

/// M14 Phase 72 (daw_01 #044): video track の clip 描画 (audio path とは別 helper)。
///
/// 描画順:
/// 1. base fill: 常に `clip.color` (未指定 None なら `video_clip_loading` =
///    絵が覆わない余白の背景としても兼用)。 選択でも fill は潰さない。
/// 2. thumbnail = Some なら [`content_thumbnail_rect`] で texture overlay
///    (`HeavyCtx::push_textured_quad`)。 r.md #68 で **clip 矩形への aspect-fit を
///    やめ、 content 原点に固定して clip 矩形で切り抜く** ように変えた (端 drag で
///    絵が滑る / 細い clip で縮む のが video 版の #68 だった)。
/// 3. name + (share clip なら) link glyph 描画 (`draw_clip_label`、 audio 経路と共通)
///
/// 選択表示 (2 重リング) はこの関数では描かない。 `draw_selection_overlay` が cache + content
/// の上に別レイヤで重ねる (r.md #20: 選択で thumbnail/プレビューを潰さないため)。
///
/// M14 Phase 108 (daw_01 #080): share マーク (⇌) は「content 共有」 の意味で track kind と直交するため、
/// video clip でも `share_group_color.is_some()` で link glyph を描く。
/// M14 Phase 114 (daw_01 #086): `share_group_color` は fill / border を上書きしない (リンク識別は ⇌ glyph
/// と #068 hover 強調のみ)。 fill は audio clip と同じく `clip.color` が唯一の source。 `audio_edit`
/// overlay は引き続き video clip では描画しない (= caller 責任で audio 用 field を video clip に詰めない)。
pub(super) fn draw_video_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ClipView,
    style: &ArrangementStyle,
    lanes: Rect,
    // r.md #68: thumbnail も「中身」 なので content 原点基準で置く (clip 矩形にフィット
    // させると端 drag で絵が滑る)。
    map: ContentMap,
) {
    let has_link = clip.share_group_color.is_some();
    // M14 Phase 114 (daw_01 #086): video clip も `clip.color` を唯一の fill source にする
    // (`share_group_color` は fill / border を上書きしない)。 `color` 未指定 (None) のときは従来の
    // 余白 / loading 背景 `video_clip_loading` を使う (= 既存の非 share video clip と互換)。
    // thumbnail があればその上に敷き詰めた texture を重ねる (fill は覆われない余白に残る)。
    // リンク識別は ⇌ glyph + #068 hover 強調が担う (track kind に依らず share マークが出る、 #080 不変)。
    // fill は常に clip 本来の色 (選択でも潰さない)。 選択表示 (2 重リング) は `draw_selection_overlay`
    // が cache + content の上に別レイヤで重ねるので、 ここでは選択を扱わない (r.md #20)。
    // 実塗り色は `clip_effective_fill` が SSoT (中身のコントラスト計算と共通)。
    let fill = clip_effective_fill(clip, TrackKind::Video, style);
    let (border, border_w) = (style.clip_border, style.clip_border_w);
    // M14 Phase 89 (daw_01 #060): 名前色は fill 輝度から auto-contrast (selected の黄 fill → 暗文字、
    // loading の暗 fill → 明文字)。 video lane bg と合成した実効色で判定 (不透明 fill は no-op、
    // 半透明 fill は track_background_video と合成して実効色を得る)。
    let text_color = clip_text_color_for(hctx.palette(), style, fill, style.track_background_video);
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    // r.md #68: サムネイルは content 原点を位相に **敷き詰める** (REAPER の
    // "Center/tile image" と同じ)。 clip 矩形からはみ出した分は **窓で切り抜く**
    // (縮小しない)。 可視域は `clip_rect ∩ lanes` で、 ここでカリングするので
    // 長い clip でもタイル数は画面幅で頭打ちになる。
    if let Some(thumb) = clip.thumbnail {
        let visible = r.intersect(lanes);
        if let Some(t) = thumbnail_tiling(r, visible.x, visible.x + visible.w, map, thumb) {
            for k in 0..t.count {
                hctx.push_textured_quad(TexturedQuad {
                    rect: Rect::new(
                        t.first_x + t.tile_w * k as f32,
                        r.y,
                        t.tile_w,
                        t.tile_h,
                    ),
                    texture: thumb.texture,
                    alpha: 1.0,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: Some(visible),
                    rotation_radians: 0.0,
                    rotation_pivot: None,
                });
            }
        }
    }
    // muted は thumbnail の上に斜線ハッチを重ねる (label の下)。
    if clip.muted {
        push_muted_hatch(
            hctx,
            r,
            r.intersect(lanes),
            style.clip_muted_hatch_color,
            style.clip_muted_hatch_spacing_px,
            style.clip_muted_hatch_width_px,
        );
    }
    // name + (share clip なら) link glyph。 thumbnail の **後** に描くので texture の上に乗る。
    draw_clip_label(hctx, r, &clip.name, has_link, text_color, style);
}

pub(super) fn draw_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ClipView,
    style: &ArrangementStyle,
    lanes: Rect,
    track_kind: TrackKind,
    // r.md #68: video / image clip の thumbnail 配置に使う (audio / MIDI clip では未使用)。
    view: ArrangementView,
) {
    if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
        return;
    }
    // M14 Phase 72 (daw_01 #044): video track の clip は thumbnail + loading 色の専用 path。
    // M14 Phase 108 (daw_01 #080): share_group_color は video clip でも honor する (Text / Image clip
    // の共有マーク)。 audio_edit のみ無視 (video clip では意味を持たない、 caller 責任)。
    if matches!(track_kind, TrackKind::Video) {
        draw_video_clip(hctx, r, clip, style, lanes, content_map(clip, view, lanes));
        return;
    }
    // M14 Phase 114 (daw_01 #086): 静的な fill / border は **`clip.color` を唯一の source** にする。
    // selected は selection 色を最優先 (link glyph の有無に依らず)。 `share_group_color` は #086 で
    // 役割を「リンク識別」 に絞り、 fill / border を一切上書きしない (= ⇌ glyph + #068 hover 強調
    // 専用)。 これにより「clip で色を選べば共有クリップ全部がその色になる」「トラックに揃えれば
    // その色になる」 が成立する (#019/#022 で hue fill が `color` を握り潰していた問題の解消)。
    // fill は常に clip 本来の色 (選択でも潰さない)。 選択表示 (2 重リング) は `draw_selection_overlay`
    // が cache + content パスの上に別レイヤで重ねるので、 ここでは選択を扱わない (r.md #20)。
    // 実塗り色は `clip_effective_fill` が SSoT (波形 / ノート / fade のコントラストも同じ値)。
    let fill = clip_effective_fill(clip, TrackKind::Audio, style);
    let (border, border_w) = (style.clip_border, style.clip_border_w);
    // M14 Phase 89 (daw_01 #060): 名前 + link glyph 色を fill 輝度から auto-contrast。 不透明 fill は
    // no-op、 半透明 fill (alpha < 1) は lane bg (audio lane = `style.bg`) と合成した実効色で判定する。
    let text_color = clip_text_color_for(hctx.palette(), style, fill, style.bg);
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    // muted は斜線ハッチを fill の上・label の下に重ねる (label は読める)。
    if clip.muted {
        push_muted_hatch(
            hctx,
            r,
            r.intersect(lanes),
            style.clip_muted_hatch_color,
            style.clip_muted_hatch_spacing_px,
            style.clip_muted_hatch_width_px,
        );
    }
    // share clip は name の左に link glyph (`⇌` 等) を 1 文字描画 (selection と独立 = selected でも
    // shared なら描画、 #022)。 等幅 (HackGen Console NF) では `clip_text_size` ~= 1 文字幅。 描画
    // ロジックは video 経路と共通の `draw_clip_label` に集約 (M14 Phase 108、 daw_01 #080)。
    let has_link = clip.share_group_color.is_some();
    draw_clip_label(hctx, r, &clip.name, has_link, text_color, style);
}

pub(super) fn draw_clips<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes, t.kind, view);
        }
    }
}

pub(super) fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    selected: &HashSet<ClipKey>,
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    // r.md #20: 選択は **リングのみ** を重ねる (旧: `draw_clip(.., selected=true)` で clip 全体を
    // 再描画)。 base fill / label は cache (`draw_clips`) が、 波形 / MIDI ノート / video thumbnail の
    // プレビューは content パス (`render.rs` の cached 外ブロック) が既に描いている。 ここで clip 全体を
    // 再描画すると `draw_clip` の不透明 fill (`push_rect`) が content パスの上に来て、 選択クリップ
    // だけプレビューが塗り潰される (S4b リファクタで content 描画を widget 内に移した際の regression)。
    // fill は選択でも潰さない設計 (`draw_clip` のコメント参照)。 選択表示に必要なのは 2 重リング
    // だけ (cache が selected=false で描いた 1px border は 2px リングが覆う。 label 色は fill 由来で
    // 選択非依存なので cache 済みのもので正しい)。
    if selected.is_empty() {
        return;
    }
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let key = ClipKey { track: t.id, clip: c.id };
            if !selected.contains(&key) {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            push_selection_ring(hctx, r, style, style.clip_radius, Some(lanes));
        }
    }
}

/// M14 Phase 96 (daw_01 #068): 共有グループ「連動ハイライト」overlay。
/// `clip.in_active_group == true` かつ `share_group_color.is_some()` の clip に、 selection
/// (黄塗り) とは **別レイヤ** の強調 (glow wash + bright thick border) を重ねる。
/// M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color` に変更
/// (旧: グループ hue を流用)。 #086 で clip fill が user 指定色になったため、 hue wash だと user の色と
/// 喧嘩する。 hover 中は 1 グループしか強調しないので色でグループを区別する必要は無い。
///
/// - **`in_active_group == false` / `share_group_color == None` の clip は一切描画しない**
///   (= 既存挙動と pixel 完全一致、 常に false で渡せば移行安全、 非 share clip は強調しない defensive)。
/// - **selection overlay より前** に呼ぶ: 選択中の同グループ member は黄塗りが上書き優先され
///   (#068 の「黄塗り優先で OK」)、 非選択 member が neutral 強調の主役になる。
/// - **cached 外で毎フレーム描画**: active group は hover / 選択で毎フレーム変わるため
///   viewport_key (heavy cache key) には含めない (hover 由来の変化で heavy cache を無効化しない =
///   selection overlay と同 idiom)。 描画は `draw_clips` / `draw_selection_overlay` と同じ culling。
pub(super) fn draw_active_group_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            if !c.in_active_group {
                continue;
            }
            // share group member (= `share_group_color.is_some()`) でなければ強調しない
            // (video clip 等は share_group_color = None、 defensive)。 M14 Phase 114 (#086) で hue 値は
            // 強調色に使わなくなったが、 「リンクされた clip だけ」 を強調する guard は維持する。
            if c.share_group_color.is_none() {
                continue;
            }
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            // M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color`
            // (bright 中立色) に変更。 #086 で clip fill が user 指定色になったため、 旧 hue wash だと
            // ユーザの選んだ色と喧嘩する (hover 中は 1 グループしか強調しない = どのグループかを色で
            // 区別する必要が無い)。 selection の黄塗りとは別レイヤの「明度上げ + 明るい中立枠」。
            // (1) glow wash: neutral color を低 alpha で clip 全体に敷いて「明るくする」。 alpha=0 なら
            //     no-op (= ring のみの強調)。 透明 fill push を避けるため alpha>0 の時だけ積む。
            if style.share_group_active_glow_alpha > 0.0 {
                let ac = style.share_group_active_color;
                let glow = Color { r: ac.r, g: ac.g, b: ac.b, a: style.share_group_active_glow_alpha };
                hctx.push_rect(RectCommand {
                    rect: r,
                    fill: glow,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [style.clip_radius; 4],
                    clip_rect: Some(lanes),
                });
            }
            // (2) bright thick border: 同 neutral color を太枠で outline。 透明 fill なので
            //     clip 名 / 既存 fill は隠さず、 枠だけ強調 (= 「束ねられている」 印象)。
            if style.share_group_active_border_w > 0.0 {
                hctx.push_rect(RectCommand {
                    rect: r,
                    fill: Color::TRANSPARENT,
                    border: style.share_group_active_color,
                    border_width: style.share_group_active_border_w,
                    radius: [style.clip_radius; 4],
                    clip_rect: Some(lanes),
                });
            }
        }
    }
}

pub(super) fn drag_preview_geometry(
    anchor: ClipDragAnchor,
    kind: ClipDragKind,
    beat_delta: f64,
    track_delta: i32,
    min_idx: usize,
    n_tracks: usize,
    min_len: f64,
) -> (f64, f64, usize) {
    match kind {
        ClipDragKind::Move => {
            let new_start = (anchor.start_beat + beat_delta).max(0.0);
            // `min_idx` = master row があれば 1 (release commit の `min_idx_i32` と
            // 同じ下限)。 揃えないと最上端で preview が master 行に ghost を描き
            // preview ≠ commit になる (review — overlay/commit 完全一致の原則)。
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let new_idx = (anchor.track_index as i32 + track_delta).clamp(
                min_idx as i32,
                ((n_tracks.saturating_sub(1)) as i32).max(min_idx as i32),
            );
            #[allow(clippy::cast_sign_loss)]
            let new_idx_u = new_idx.max(0) as usize;
            (new_start, anchor.len_beats, new_idx_u)
        }
        // 端 drag は `resize_preview_start_len` が SSoT (release commit / ゴーストの
        // 中身も同じ関数を通る)。 track は動かない。
        ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => {
            let (start, len) = resize_preview_start_len(
                anchor.start_beat,
                anchor.len_beats,
                kind,
                beat_delta,
                min_len,
            );
            (start, len, anchor.track_index)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    nd: &ClipDragSession,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
    n_tracks: usize,
    beat_delta: f64,
    track_delta: i32,
    min_len: f64,
    // r.md #24: ゴーストに描く中身 (波形 / MIDI)。 base 描画と同じ map。
    clip_content: &std::collections::HashMap<ClipKey, ClipContentDraw>,
    // r.md #68: Shift + 端 drag (= time-stretch) のときだけ入る「伸縮済みの中身」。
    // caller (`run.rs`) が commit と同じ `stretch_remap` で content を写像し、 audio は
    // engine と同じ `event_wave_spans` で span を引き直しているので、 プレビューと確定結果が
    // Slice / Raw→Stretch 昇格まで含めて一致する。 トリム / 移動では確定後の中身が base と
    // 同一なので空で、 ゴーストも `clip_content` をそのまま描く (= 中身は 1px も動かない)。
    stretch_ghost_content: &std::collections::HashMap<ClipKey, ClipContentDraw>,
) {
    // r.md #24: drag preview は **掴んだ clip の中身入り半透明コピー** を描く (旧: 中身の無い不透明
    // ghost が元 clip を覆い隠し、 press / drag 中に名前 / 波形 / MIDI が消える #24 の主因だった)。
    // 元 clip はその場に不透明のまま残り (cached + content path)、 preview は薄いコピーとして重なる/
    // 動くので「どれを掴んで動かしているか」 が中身ごと見える。
    //
    // M14 Phase 63e (#019): Ctrl / Ctrl+Shift drag は clone なので border 色 + badge glyph で
    // 「move / linked clone / independent clone」 の 3 種を視覚区別する。 Resize 中は Ctrl 関与なし。
    // commit / overlay の判定はどちらも `nd.last_*` を真値とするので、 release frame の OS event
    // 順序問題に依存せず一致する。
    let is_move_clone = matches!(nd.kind, ClipDragKind::Move) && nd.last_ctrl;
    let (clone_border, badge_glyph) = if is_move_clone {
        if nd.last_shift {
            (Some(style.clip_clone_indep_border), Some('+'))
        } else {
            (Some(style.clip_clone_linked_border), Some('⇌'))
        }
    } else {
        (None, None)
    };
    let inset = clip_content_inset_top(style);

    // master row prepend 済みなら drop 先下限は 1 (release commit と同じ guard)。
    let min_idx = usize::from(
        visible_tracks.first().is_some_and(|t| t.id == MASTER_TRACK_ID),
    );
    for a in &nd.anchors {
        let (start, len, new_idx) = drag_preview_geometry(
            *a, nd.kind, beat_delta, track_delta, min_idx, n_tracks, min_len,
        );
        // drag_preview_geometry が n_tracks 範囲内に clamp 済なので tops から必ず取れる前提。
        // 万一範囲外なら preview を skip (clip 描画消失だけで panic はしない、 defensive)。
        let Some(row_top) = tops.get(new_idx).copied() else {
            continue;
        };
        // ghost も drop 先 track の per-track 実効行高で描く (commit 後の実描画と一致)。
        let ghost_row_h = visible_tracks
            .get(new_idx)
            .map_or(view.track_row_h, |t| effective_track_row_h(t, view.track_row_h));
        // 元 clip の実データ (kind / 色 / 名前 / thumbnail) を lookup (中身入りコピーの source)。
        // drag 中に clip が消える等の異常時は選択色でフォールバック (content なしの薄い枠)。
        let src = visible_tracks
            .iter()
            .find(|t| t.id == a.key.track)
            .and_then(|t| t.clips.iter().find(|c| c.id == a.key.clip).map(|c| (t.kind, c)));
        let src_color =
            src.map_or(style.clip_selected_fill, |(_, c)| c.color.unwrap_or(style.clip_default_fill));
        // preview fill = 元 clip 色を半透明化 (中身が透けて「コピー」 と分かる)。 元色が既に半透明
        // (share clip 等) なら更に薄くなるよう alpha を乗算する。
        let preview_fill = src_color.with_alpha(src_color.a * DRAG_PREVIEW_FILL_ALPHA);
        let src_kind = src.map_or(TrackKind::Audio, |(k, _)| k);
        // r.md #68: ゴーストの **content 原点** を確定後と一致させる。
        // - Move: 窓ごと動く = offset 据え置き (原点も同じだけ動く)。
        // - ResizeLeft / ResizeRight: `resize_clip` が `content_offset_beats += Δstart` を
        //   行うので原点は不動 (`stretch_clip_content` も offset を同量進めるので stretch でも同じ)。
        // これで「トリムしても中身は 1px も動かない」 が式として成り立ち、 中身は
        // ゴースト矩形で切り抜かれるだけになる。
        let src_offset = src.map_or(0.0, |(_, c)| c.content_offset_beats);
        let preview_offset = match nd.kind {
            ClipDragKind::Move => src_offset,
            ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => {
                src_offset + (start - a.start_beat)
            }
        };
        let preview_clip = ClipView {
            id: a.key.clip,
            start_beat: start,
            len_beats: len,
            content_offset_beats: preview_offset,
            name: src.map_or_else(|| Arc::from(""), |(_, c)| c.name.clone()),
            color: Some(preview_fill),
            // 共有マーク / 連動ハイライトは transient な preview では出さない (元 clip 側で描画済)。
            share_group_color: None,
            // fade envelope は transient な preview では出さない (元 clip 側で描画済)。
            fades: Vec::new(),
            // video / image / text clip の thumbnail はコピーにも見せる (それが中身なので)。
            thumbnail: src.and_then(|(_, c)| c.thumbnail),
            in_active_group: false,
            // drag ghost は半透明プレビューなので mute ハッチは出さない (元 clip 側で描画済)。
            muted: false,
        };
        let r = clip_to_rect(row_top, ghost_row_h, &preview_clip, view, lanes);
        if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
            continue;
        }
        // (1) clip クローム (半透明 fill + border + 名前 + video thumbnail) を base 描画と同じ経路で。
        draw_clip(hctx, r, &preview_clip, style, lanes, src_kind, view);
        // (2) 中身 (波形 / MIDI) を上に重ねる。 波形は ghost 専用 id で LOD state 衝突を避ける
        //     (元 clip の波形と同一フレームに 2 度描くため。 `draw_clip_waveform_inner` 参照)。
        //
        // r.md #68: x 写像は base 描画とまったく同じ `content_map` (= ビューのズーム)。
        // ゴースト矩形の幅は一切分母に入らないので、 トリム中は中身が動かず、
        // Shift ストレッチ中は `ghost_content` 側が既に伸縮済みの中身を持っている。
        let ghost_map = content_map(&preview_clip, view, lanes);
        match stretch_ghost_content.get(&a.key).or_else(|| clip_content.get(&a.key)) {
            Some(ClipContentDraw::Audio { events }) => {
                draw_clip_waveform_inner(
                    hctx, a.key, r, ghost_map, events, true, lanes, inset, preview_fill, style,
                    "drag_ghost_wf",
                );
            }
            Some(ClipContentDraw::Midi { notes }) => {
                // ノート色コントラストは実際に塗る preview_fill (半透明) を背景として計算 (#20 と同 idiom)。
                draw_clip_midi_inner(hctx, r, notes, ghost_map, preview_fill, style, lanes.x, inset);
            }
            None => {}
        }
        // (3) 枠: move は選択 2 重リング、 clone は clone 色の枠 (中身は上で描画済なので枠のみ差替え)。
        if let Some(cb) = clone_border {
            hctx.push_rect(RectCommand {
                rect: r,
                fill: Color::TRANSPARENT,
                border: cb,
                border_width: style.clip_selected_border_w,
                radius: [style.clip_radius; 4],
                clip_rect: Some(lanes),
            });
        } else {
            push_selection_ring(hctx, r, style, style.clip_radius, Some(lanes));
        }
        // clone のときだけ rect 左上に badge glyph (`⇌` / `+`)。 rect が小さすぎるときは省略。
        if let Some(g) = badge_glyph
            && r.w > style.clip_clone_badge_size + 4.0
            && r.h > style.clip_clone_badge_size + 2.0
        {
            // r.md #73: 下地は ghost の塗り (= ユーザー着色 / 選択の黄) なので極性は
            // 実効背景から決める。 固定インクだと片方の極性でバッジが消える。
            let badge_ink = clip_ink_for(hctx.palette(), preview_fill, style.bg);
            hctx.push_text(GlyphArea {
                text: Arc::from(g.to_string()),
                left: r.x + 4.0,
                top: r.y + 2.0,
                font_size: style.clip_clone_badge_size,
                line_height: style.clip_clone_badge_size * 1.2,
                color: badge_ink,
                clip_rect: Some(r),
                ..GlyphArea::default()
            });
        }
    }
}

// r.md #73: `db_to_handle_y` (gain_db → clip 内の横線 y) は dB ハンドル線ごと撤去した。
// doc は「描画と hit-test の SSoT」を名乗っていたが、hit-test 側 (`geometry::audio_grip_hit`)
// は実際にはこれを呼ばず clip の縦中央を固定で使っていたので、gain が 0 dB を外れると
// 線と掴む帯がずれていた。線が不可視 (`Color::TRANSPARENT` 固定) だったので誰も気付けなかった。

/// r.md #38: 1 event の fade envelope (カーブ + 掴む正方形) を描画
/// (event 左端 = In / 右端 = Out)。
///
/// r.md #58: 「カーブ + 掴む正方形」 をまとめて描く合成関数。
///
/// **通常の描画経路はこれを使わない**。 カーブは曲の状態 (= Song の関数) なので
/// cached 層 (`draw_clip_fade_curves`)、 掴む正方形はポインタ位置の関数なので
/// cached 外の overlay (`draw_fade_handle_overlay`)、 と層が分かれている。
/// ここに残っているのは **ドラッグ中の ghost** (`draw_audio_drag_ghost`) 用 —
/// ghost は「今どうなるか」 のプレビューなので、 カーブと掴み位置の両方を同時に
/// 出す必要がある。
pub(super) fn draw_fade_envelope<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    // r.md #68: content-local 拍 → x (clip の表示幅に依存しない写像)。
    map: ContentMap,
    fade: &ClipEventFade,
    edge: FadeEdge,
    // r.md #46: この clip が実際に塗られている色 (`clip_effective_fill`)。
    clip_bg: Color,
    style: &ArrangementStyle,
) {
    // 正方形 → カーブの順 (= 分割前と同じ z 順。 カーブの線が正方形の上に乗る)。
    draw_fade_handle(hctx, clip_rect, map, fade, edge, clip_bg, style);
    draw_fade_curve(hctx, clip_rect, map, fade, edge, clip_bg, style);
}

/// 掴む正方形だけを描く (hit zone と同じ rect = `fade_geometry` が SSoT)。
///
/// r.md #38: 正方形は **fade の終端**に置く (fade 長で横に動く)。 以前は clip の角に
/// 固定されていて、 fade を伸ばしても動かなかった。 `fade_beats = 0` でも描く
/// (= 「ここを掴めばフェードを付けられる」 の hint)。 このとき正方形は event の角に
/// ちょうど一致する。
pub(super) fn draw_fade_handle<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    // r.md #68: content-local 拍 → x (clip の表示幅に依存しない写像)。
    map: ContentMap,
    fade: &ClipEventFade,
    edge: FadeEdge,
    clip_bg: Color,
    style: &ArrangementStyle,
) {
    let g = fade_geometry(clip_rect, map, fade, edge, style);
    if g.event_rect.w <= 0.0 || g.event_rect.h <= 0.0 {
        return;
    }
    // r.md #46: clip 色 (可変背景) と波形の上のどちらでも縁が立つよう、
    // 前景 + 逆極性の裏打ちの 2 層で描く。
    let (fg, backing) = fade_colors_for(hctx.palette(), style, clip_bg, style.bg);
    // 裏打ちを 1 周り大きく敷いてから前景を重ねる = 1px の縁取り。
    push_filled_rect(hctx, g.handle_rect, backing);
    let inner = Rect {
        x: g.handle_rect.x + 1.0,
        y: g.handle_rect.y + 1.0,
        w: (g.handle_rect.w - 2.0).max(0.0),
        h: (g.handle_rect.h - 2.0).max(0.0),
    };
    if inner.w > 0.0 && inner.h > 0.0 {
        push_filled_rect(hctx, inner, fg);
    }
}

/// フェードカーブ (斜めの線) だけを描く。 fade 長 0 なら何も描かない。
///
/// r.md #38: **音・映像・画像・字幕に実際に掛かる envelope をそのまま描く**。
/// 無音 (ゲイン 0) = event 矩形の**下端**、 フル (ゲイン 1) = **上端**
/// (#38 以前は上下が逆で、 fade in が「頭で最大 → 下がる」 という fade out の絵に
/// なっていた)。 線の形は `common::audio_render::fade_curve_at` を刻んだ折れ線で、
/// Linear / Exponential / SCurve が形に出る (以前は直線 1 本だったので curve を
/// 切り替えても線が変わらなかった)。
pub(super) fn draw_fade_curve<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    // r.md #68: content-local 拍 → x (clip の表示幅に依存しない写像)。
    map: ContentMap,
    fade: &ClipEventFade,
    edge: FadeEdge,
    clip_bg: Color,
    style: &ArrangementStyle,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};

    let g = fade_geometry(clip_rect, map, fade, edge, style);
    if g.event_rect.w <= 0.0 || g.event_rect.h <= 0.0 {
        return;
    }
    if g.width_px <= 0.5 {
        return;
    }
    let (fg, backing) = fade_colors_for(hctx.palette(), style, clip_bg, style.bg);
    let curve = match edge {
        FadeEdge::In => fade.fade.fade_in_curve,
        FadeEdge::Out => fade.fade.fade_out_curve,
    };
    // 曲線を px 単位で刻む。 分割粒度は既存の lane curve flatten と同じ 2px
    // (`curve::flatten_clip_curve` の呼び出し側 `max_segment_px`)。
    const MAX_SEGMENT_PX: f32 = 2.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = ((g.width_px / MAX_SEGMENT_PX).ceil() as usize).clamp(1, 512);
    let bottom = g.event_rect.y + g.event_rect.h;
    let mut segments: Vec<LineSegment> = Vec::with_capacity(steps);
    // t = 0 が無音側 (anchor)、 t = 1 がフル側 (handle)。
    let point_at = |t: f32| -> [f32; 2] {
        let gain = common::audio_render::fade_curve_at(t, curve);
        [
            g.anchor[0] + (g.handle[0] - g.anchor[0]) * t,
            bottom - g.event_rect.h * gain,
        ]
    };
    let mut prev = point_at(0.0);
    let mut backing_segments: Vec<LineSegment> = Vec::with_capacity(steps);
    for i in 1..=steps {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 / steps as f32;
        let cur = point_at(t);
        segments.push(LineSegment { a: prev, b: cur, color: fg });
        backing_segments.push(LineSegment { a: prev, b: cur, color: backing });
        prev = cur;
    }
    // 裏打ちを一回り太く敷いてから前景を重ねる (無音ベースライン / slice 区切り線と同 idiom)。
    hctx.push_lines(LineBatch {
        segments: Arc::<[LineSegment]>::from(backing_segments),
        line_width_px: style.audio_fade_overlay_width_px + 2.0,
        clip_rect: Some(clip_rect),
    });
    hctx.push_lines(LineBatch {
        segments: Arc::<[LineSegment]>::from(segments),
        line_width_px: style.audio_fade_overlay_width_px,
        clip_rect: Some(clip_rect),
    });
}

/// r.md #38: clip の全 event の fade **カーブ**を描画 (content 種別に依らず共通)。
///
/// r.md #58: 掴む正方形はここでは描かない。 カーブは曲の状態 (どこにどんなフェードが
/// 掛かっているか) なので cached 層 (`viewport_key` は `fold_arrangement_clip_hash` 経由で
/// fade の全パラメータを含む) が正しい置き場所。 正方形は「掴める場所」 = ポインタ位置の
/// 関数なので cached 外の `draw_fade_handle_overlay` が描く。
pub(super) fn draw_clip_fade_curves<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    // r.md #68: content-local 拍 → x (clip の表示幅に依存しない写像)。
    map: ContentMap,
    fades: &[ClipEventFade],
    // r.md #46: この clip が実際に塗られている色 (`clip_effective_fill`)。
    clip_bg: Color,
    style: &ArrangementStyle,
) {
    for f in fades {
        for edge in [FadeEdge::In, FadeEdge::Out] {
            draw_fade_curve(hctx, clip_rect, map, f, edge, clip_bg, style);
        }
    }
}

/// r.md #58: 掴む正方形を **マウスが乗っているクリップだけ** に描く overlay。
///
/// - **cached 外で毎フレーム描画**: hover は毎フレーム変わるので `viewport_key`
///   (heavy cache key) に含めない — 含めた瞬間、 マウスを動かすたびにアレンジ全体
///   (グリッド + 全クリップ + 全オートメーションレーン) が再構築される。
///   `draw_active_group_overlay` / `draw_selection_overlay` と同 idiom。
/// - **選択中かどうかは見ない**: 要望どおり「マウスオーバーしている時だけ」
///   (Ableton Live / Ardour / Bitwig と同じ。 Cubase だけが「hover または選択」)。
/// - **`drag_clip` を OR 条件に入れるのが必須**: フェードのカーブ切替は縦ドラッグなので
///   カーソルが行の外へ出て `clip_hit` が None を返す。 hover だけを条件にすると、
///   掴んでいる正方形が指の下で消える。
/// - フェードを持つ全 clip 種別が対象 (r.md #38 で fade は content 非依存になった)。
///   MIDI / オートメーションクリップは `fades` が空なので元から無関係。
pub(super) fn draw_fade_handle_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    hovered_clip: Option<ClipKey>,
    drag_clip: Option<ClipKey>,
    style: &ArrangementStyle,
) {
    if hovered_clip.is_none() && drag_clip.is_none() {
        return;
    }
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            if c.fades.is_empty() {
                continue;
            }
            let key = ClipKey { track: t.id, clip: c.id };
            if Some(key) != hovered_clip && Some(key) != drag_clip {
                continue;
            }
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            // cached 層のカーブ描画 (render.rs) と同じ「細すぎる clip には掴み所を
            // 出さない」 閾値。 hit-test (`audio_grip_hit`) 側とも揃っている。
            if r.w < style.audio_min_clip_w_for_handles_px {
                continue;
            }
            let bg = clip_effective_fill(c, t.kind, style);
            for f in &c.fades {
                // hit-test (`audio_grip_hit`) は handle が重なったとき **width_px の
                // 大きい方**を採るので、 描画も「大きい方を後に (= 手前に) 描く」 に
                // 揃える (SSoT)。 この規則は正方形にだけ意味がある。
                let mut edges = [FadeEdge::In, FadeEdge::Out];
                if f.fade.fade_in_beats > f.fade.fade_out_beats {
                    edges.swap(0, 1);
                }
                for edge in edges {
                    draw_fade_handle(hctx, r, content_map(c, view, lanes), f, edge, bg, style);
                }
            }
        }
    }
}

/// M14 Phase 63k (#025): audio_drag 中の ghost overlay (cached 外、 drag 中の preview 値を最新表示)。
/// `compute_audio_drag_outcome` の結果を視覚化:
/// - `FadeLength { edge, next_beats }` → 新 fade 範囲を `draw_fade_envelope` で描く + label 省略
///   (envelope の長さ自体が visual feedback)。
/// - `FadeCurve { edge, next_curve }` → curve 名を ghost label「Curve: Exponential」 で描く。
/// - `None` (sticky 未確定) → label「Move」 (= drag が始まったが方向未確定の hint)。
pub(super) fn draw_audio_drag_ghost<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    ad: &AudioDragSession,
    beat_per_px: f64,
    style: &ArrangementStyle,
) {
    let r = ad.clip_rect_anchor;
    if r.w <= 0.0 || r.h <= 0.0 {
        return;
    }
    let clip_bg = ad.clip_bg_anchor;
    let outcome = compute_audio_drag_outcome(ad, beat_per_px);
    let label_text: Option<String> = match (ad.kind, outcome) {
        (_, Some(AudioDragOutcome::FadeLength { edge, next_beats })) => {
            if let Some(mut preview) = ad.anchor_fade {
                match edge {
                    FadeEdge::In => preview.fade.fade_in_beats = next_beats,
                    FadeEdge::Out => preview.fade.fade_out_beats = next_beats,
                }
                draw_fade_envelope(hctx, r, ad.content_map_anchor, &preview, edge, clip_bg, style);
            }
            None
        }
        (_, Some(AudioDragOutcome::FadeCurve { edge, next_curve })) => {
            // r.md #38: curve drag 中も **形** をプレビューする (以前はラベルだけで、
            // 線が直線のままだったので何が変わるのか見えなかった)。
            if let Some(mut preview) = ad.anchor_fade {
                match edge {
                    FadeEdge::In => preview.fade.fade_in_curve = next_curve,
                    FadeEdge::Out => preview.fade.fade_out_curve = next_curve,
                }
                draw_fade_envelope(hctx, r, ad.content_map_anchor, &preview, edge, clip_bg, style);
            }
            Some(format!("Curve: {}", next_curve.name()))
        }
        // commit すべき変化なし (drag 距離不足 or anchor 同値) — anchor 値の preview を出さない。
        // sticky 未確定の場合は label だけで「drag しているけど未確定」 を示す。
        (AudioDragKind::FadeIn | AudioDragKind::FadeOut, None) if ad.locked_horizontal.is_none() => {
            Some("Drag horizontally for length, vertically for curve".to_string())
        }
        _ => None,
    };

    if let Some(text) = label_text {
        // ghost label は clip rect の中央上端に 1 行 (= 既存 clip name と被るが、 drag 中のみ表示で問題なし)。
        let font_size = style.audio_ghost_label_size;
        // r.md #73: 下地は掴んでいる clip の実塗り色 (可変) なので極性をそこから決める。
        let ink = clip_ink_for(hctx.palette(), clip_bg, style.bg);
        hctx.push_text(GlyphArea {
            text: Arc::from(text),
            left: r.x + 4.0,
            top: r.y + r.h - font_size - 4.0,
            font_size,
            line_height: font_size * 1.2,
            color: ink,
            clip_rect: Some(r),
            ..GlyphArea::default()
        });
    }
}

/// lane row (= header + body) を 1 つ描画。 `header_rect` は左 (track header と同 x 範囲)、
/// `body_rect` は右 (clip 描画域と同 x 範囲)。 `view` は arrangement の global view (start_beat /
/// len_beats / track_top 等を渡す、 lane 描画では `start_beat` / `len_beats` のみ参照)。
/// disabled lane (`enabled = false`) は curve / clip / point を `automation_lane_disabled_color` で描画。
/// M14 Phase 114 (daw_01 #086): clip fill / border は `lane.color` が唯一 source (`share_group_color` は
/// fill を上書きしない、 リンク識別は ⇌ glyph + #068 hover 強調のみ)。
/// M14 Phase 63n-3 (#028): `selected_clips_set` に含まれる `AutomationClipKey` は `clip_selected_fill` /
/// `clip_selected_border` で描画 (selected priority 最高)、 share_group_color = Some の clip は名前の左に
/// `share_group_link_glyph` (`⇌`) を 1 文字描画 (MIDI clip と同 idiom)。 `track_id` は selection lookup 用。
///
/// r.md #73: `bend_skip` = いま Alt+ドラッグで曲げている区間 (その point を終点とする 1 本)。
/// **その区間だけ base curve を描かない** — 描くと preview と形が食い違って 2 重線に見える。
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn draw_automation_lane<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    track_id: u32,
    lane: &ArrangementAutomationLane,
    header_rect: Rect,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    lanes_clip: Rect,
    selected_clips_set: &HashSet<AutomationClipKey>,
    bend_skip: Option<AutomationPointIdKey>,
) {
    let p = hctx.palette();
    // r.md #48: `lane.color` は「どのパラメータのレーンか」 を運ぶ **アイデンティティ色** で
    // テーマ非従属。ただしライトテーマの薄い lane 面の上ではそのままだと沈むので、
    // 色相・彩度を保ったままコントラストが足りるまで明度だけ寄せる (`adapt_on` は既に
    // 足りていれば恒等 = ダークテーマでは何も変わらない)。 icon glyph / clip 枠 / clip の
    // 薄い wash はすべてこの 1 本から引く (= 同じレーンが 3 箇所で違う色にならない)。
    let lane_ink = p.adapt_on(style.automation_lane_bg, lane.color);
    // ---- 背景 (lane 行 全幅) ----
    push_filled_rect(
        hctx,
        Rect {
            x: header_rect.x,
            y: header_rect.y,
            w: header_rect.w + body_rect.w,
            h: header_rect.h,
        },
        style.automation_lane_bg,
    );

    // ---- header: ★ icon label slider 帯 👁▣✕ (描画 + Phase 63n-2 hit-test 対応) ----
    // M14 Phase 63n-2 (#028): 描画と hit-test の SSoT を `automation_lane_header_layout` に集約。
    // header_rect.w が極狭の場合 (`< automation_lane_header_min_w_px`) は layout が `None` で描画 skip。
    // curve line / point dot の色は「lane.color 直塗り」 をやめ、 clip ごとに実際の
    // `fill` 輝度から白/黒 neutral を auto-contrast する (= clip 名 `clip_text_color_for` と同 SSoT)。
    // 黄など明るい識別色でも常にコントラストを確保する狙い。 実際の色決定は下の clip ループ内
    // (fill 確定後) で行う。 header の icon glyph 色は lane 識別色 (`lane_ink`) を使う。
    if let Some(layout) = automation_lane_header_layout(header_rect, style) {
        let icon_size = style.automation_lane_icon_size.max(4.0);
        let pad = 4.0_f32;
        // ★ enabled marker (lane.enabled で星塗りつぶし切替)
        hctx.push_text(GlyphArea {
            text: Arc::from(if lane.enabled { "★" } else { "☆" }),
            left: layout.enabled_icon_rect.x,
            top: layout.enabled_icon_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: style.automation_lane_text_color,
            clip_rect: Some(header_rect),
            ..GlyphArea::default()
        });
        // [V] icon glyph (lane.icon_glyph、 lane 識別色)
        hctx.push_text(GlyphArea {
            text: Arc::from(lane.icon_glyph.to_string()),
            left: layout.icon_glyph_rect.x,
            top: layout.icon_glyph_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: lane_ink,
            clip_rect: Some(header_rect),
            ..GlyphArea::default()
        });
        // label (icon_glyph の右、 visible_icon の左までの帯)
        let label_x = layout.icon_glyph_rect.x + layout.icon_glyph_rect.w + pad;
        let label_clip = Rect {
            x: label_x,
            y: header_rect.y,
            w: (layout.visible_icon_rect.x - label_x - pad).max(0.0),
            h: header_rect.h,
        };
        hctx.push_text(GlyphArea {
            text: Arc::clone(&lane.label),
            left: label_x,
            top: layout.icon_glyph_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: style.automation_lane_text_color,
            clip_rect: Some(label_clip),
            ..GlyphArea::default()
        });
        // default value はレーンヘッダの数値入力フィールド (`default_field_rect`) を
        // caller が scrubable_number_at で overlay する (= 旧スライダー帯描画は廃止)。 widget は
        // ここで何も描かない (フィールドの bg / 値は overlay 側が持つ)。 本体の水平ガイド線
        // (下記 default_value_norm 位置) は残す (default 値の視覚位置の手がかり)。
        // 右寄せ icon 群 (👁 ▣ ✕、 Phase 63n-2 で hit-test 対応)
        for &(g, r) in &[
            ('👁', layout.visible_icon_rect),
            ('▣', layout.mute_icon_rect),
            ('✕', layout.delete_icon_rect),
        ] {
            hctx.push_text(GlyphArea {
                text: Arc::from(g.to_string()),
                left: r.x,
                top: r.y,
                font_size: icon_size,
                line_height: icon_size * 1.2,
                color: style.automation_lane_text_color,
                clip_rect: Some(header_rect),
                ..GlyphArea::default()
            });
        }
    }

    // ---- body 背景 (header と区切り線) ----
    push_filled_rect(hctx, body_rect, style.automation_lane_bg);
    // default_value 水平線。 point dot と同じ **縦 padding スケール** で描く
    // (= clip_rect の `[body.y+pad, body.y+H-pad]`、 5221 と SSoT)。 旧実装は body 全高を使って
    // いたため、 同じ値でも point とガイド線が `pad*(2v-1)` だけ縦にずれていた (user 報告)。
    let default_pad = style.automation_clip_v_pad_px;
    let default_clip_h = (body_rect.h - default_pad * 2.0).max(2.0);
    let default_y = body_rect.y
        + default_pad
        + (1.0 - lane.default_value_norm.clamp(0.0, 1.0)) * default_clip_h;
    hctx.push_lines(daw_ui_renderer::LineBatch {
        segments: vec![daw_ui_renderer::LineSegment {
            a: [body_rect.x, default_y],
            b: [body_rect.x + body_rect.w, default_y],
            color: style.automation_default_line_color,
        }]
        .into(),
        line_width_px: style.automation_default_line_width_px,
        clip_rect: Some(body_rect),
    });

    // ---- clips: rect + curve flatten + point dots ----
    let view_end = view.start_beat + view.len_beats;
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    for c in &lane.clips {
        let end = c.start_beat + c.len_beats;
        if end < view.start_beat || c.start_beat > view_end {
            continue;
        }
        // clip rect (lane body 内、 縦 padding 適用)。式は `automation_clip_rect` が SSoT
        // (cached 外の bend overlay も同じ関数から引く)。
        let clip_rect = automation_clip_rect(body_rect, view, c.start_beat, c.len_beats, style);
        let (w, ch) = (clip_rect.w, clip_rect.h);

        // M14 Phase 63n-3 (#028): selected な automation clip は selected_fill / selected_border で
        // 描画 (priority: selected > disabled > share_group > lane.color)。
        let clip_key = AutomationClipKey { track: track_id, lane: lane.id, clip: c.id };
        let is_selected = selected_clips_set.contains(&clip_key);
        // 塗り / 枠の決め方は `automation_clip_colors` が SSoT (cached 外の bend 強調が
        // 「実際に塗られた色」を必要とするので、式をここに閉じ込めない)。
        let (fill, border) = automation_clip_colors(p, lane, is_selected, style);
        hctx.push_rect(RectCommand {
            rect: clip_rect,
            fill,
            border,
            border_width: style.clip_border_w,
            radius: [style.clip_radius; 4],
            clip_rect: Some(lanes_clip),
        });

        // clip name + share group link glyph (⇌) — MIDI clip と同 idiom (`draw_clip` と対称)。
        // share_group_color = Some(hue) のとき名前の左に link glyph を 1 文字描画 + name を glyph 幅 +
        // 2px gap 分ずらす。 selection / disabled とは独立に描画 (link 関係は bypass / 選択と直交)。
        // M14 Phase 91 (daw_01 #062): 名前 / link glyph の表示を MIDI clip (`draw_clip`) と完全に揃える。
        // (1) 表示しきい値 / font_size / line_height を MIDI と同値に (旧 `w >= 28.0` + `* 0.85` +
        //     line_height = clip_text_size 直値を撤去)。 (2) 文字色は enabled lane なら fill 輝度由来の
        //     auto-contrast (`clip_text_color_for`、 alpha 0.20 の半透明 fill は automation_lane_bg と
        //     合成して実効色判定)。 disabled lane は従来どおり `automation_lane_disabled_color` 固定
        //     (= bypass marker、 #060 の selected 統合とは別文脈) で auto-contrast 対象外。 opt-out
        //     (`clip_auto_contrast_text == false`) は automation 専用の `automation_lane_text_color` に
        //     フォールバック (= clip 全般の `clip_text_color` ではなく従来色を維持)。
        if w > 24.0 && ch > style.clip_text_size + 2.0 {
            let glyph_color = if !lane.enabled {
                style.automation_lane_disabled_color
            } else if style.clip_auto_contrast_text {
                clip_text_color_for(p, style, fill, style.automation_lane_bg)
            } else {
                style.automation_lane_text_color
            };
            let font_size = style.clip_text_size;
            let has_link = c.share_group_color.is_some();
            // M14 Phase 126 (daw_01 #104) と同じく ⇌ と名前を 1 つの text run に
            // 統合する。 automation clip 経路だけ旧実装 (glyph 幅を em 幅で近似 +
            // 固定 +2.0 パッド) が残っており、 ⇌ の実 advance より広く名前を右送り
            // して、 その分だけ名前の末尾が余計に切れていた。
            let text: Arc<str> = if has_link {
                Arc::from(format!("{}{}", style.share_group_link_glyph, c.name))
            } else {
                Arc::clone(&c.name)
            };
            hctx.push_text(GlyphArea {
                text,
                left: clip_rect.x + 4.0,
                top: clip_rect.y + 2.0,
                font_size,
                line_height: style.clip_text_size * 1.2,
                color: glyph_color,
                clip_rect: Some(clip_rect),
                ..GlyphArea::default()
            });
        }

        // curve line / point dot を背景輝度から明/暗 neutral で auto-contrast する。
        // enabled lane は実際に塗った `fill` (selected = 黄不透明 / 非選択 = lane 識別色 alpha 0.20 を
        // lane_bg と合成) の実効輝度から極性固定インク (`ink_for`) を選び、 dot の枠は **その逆極性**
        // (= `ink_for(neutral)`) にして「line から浮いた node」 として常に縁が見えるようにする。
        // clip fill は選択 / ユーザー識別色で動く可変背景なので、テーマ従属の `text` ではなく
        // 極性固定インクを使う (r.md #48)。 disabled lane は bypass marker として従来の灰色
        // (`automation_lane_disabled_color`) を維持 (= clip 名と同方針)、 その枠だけはクローム面
        // の上に乗る細線なので `text` (テーマ従属) で薄く縁取る。
        let (curve_line_color, point_fill, point_border) = if lane.enabled {
            let neutral = p.ink_for(automation_clip_eff_bg(fill, style));
            let edge = p.ink_for(neutral);
            (neutral, neutral, edge)
        } else {
            let dc = style.automation_lane_disabled_color;
            (dc, dc, p.text.with_alpha(0.4))
        };

        // curve flatten (clip 内描画域 = clip_rect 全体)。 caller の screen-wide な beat_to_px
        // (= body_rect.w / view.len_beats) を渡すことで、 curve x 座標が point dot 描画と完全一致。
        // r.md #73: 形の評価は `curve.rs` (= `common::automation::apply_curve`) 1 本を通る。
        let map = curve::LaneValueMap::from_lane(lane, clip_rect);
        // r.md #73: bend ドラッグ中の 1 区間は **描かない** (preview が唯一の線になる)。
        // 描くと形の食い違いがそのまま 2 重線として見える。
        let skip = bend_skip
            .filter(|k| k.clip.track == track_id && k.clip.lane == lane.id && k.clip.clip == c.id)
            .map(|k| k.point_id);
        let runs = curve::flatten_clip_curve(
            c,
            map,
            view.start_beat,
            body_rect.x,
            beat_to_px,
            2.0,
            skip,
        );
        // `LineBatch.segments` は独立した線分の集合なので、run を跨いで 1 batch に畳んでよい
        // (run の切れ目に線分を作らないことだけが本質)。
        let segments: Vec<daw_ui_renderer::LineSegment> = runs
            .iter()
            .flat_map(|run| {
                run.windows(2).map(|w| daw_ui_renderer::LineSegment {
                    a: [w[0].0, w[0].1],
                    b: [w[1].0, w[1].1],
                    color: curve_line_color,
                })
            })
            .collect();
        if !segments.is_empty() {
            hctx.push_lines(daw_ui_renderer::LineBatch {
                segments: segments.into(),
                line_width_px: style.automation_curve_line_width_px,
                clip_rect: Some(clip_rect),
            });
        }
        // 各 point を 角丸円 (= 正方形 + 大 radius) で描画。 x の origin は **body_rect.x** (= 0
        // beat の screen x)、 そこから abs_beat * beat_to_px で point 位置を出す。 旧設計は
        // `clip_rect.x + (abs_beat - view.start_beat) * beat_to_px` で c.start_beat を 2 度足して
        // (clip_rect.x が既に c.start_beat 反映済) point dot が curve からずれる bug の根本原因
        // (#028 user 指摘 2 = curve 線が point を通らない)。
        let r = style.automation_point_radius_px;
        for p in &c.points {
            let abs_beat = c.start_beat + p.time_beat;
            #[allow(clippy::cast_possible_truncation)]
            let px = body_rect.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
            // r.md #73: y は `LaneValueMap` 経由 (同じ式を 2 度書かない)。
            let py = map.norm_to_y(p.value_norm);
            hctx.push_rect(RectCommand {
                rect: Rect { x: px - r, y: py - r, w: r * 2.0, h: r * 2.0 },
                fill: point_fill,
                border: point_border,
                border_width: 1.0,
                radius: [r; 4],
                clip_rect: Some(clip_rect),
            });
        }
    }
}
