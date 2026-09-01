//! ラックの **種別ごとの本体描画** (展開したときの中身)。
//!
//! 親モジュールがヘッダ行 / 振り分け / routing 行を描き、 ここは LFO / Random /
//! MSEG / Steps / EnvelopeFollower それぞれの canvas とコントロールだけを持つ。
//! いずれの `draw_*_body` も **`(次の y, ドラッグ中か)`** を返し、 ドラッグ中フラグは
//! 呼び側が 1 本に集約して **1 ドラッグ = 1 undo step** の bracket を張る
//! (`ScrubGesture::ModRack`)。
//!
//! widget id は **すべて安定 id (`ModSource::id`)**。 位置 index で採番すると、
//! ソースを消したり並べ替えたりした瞬間にドラッグ状態 / テキストバッファ / 開いて
//! いる dropdown popup が別ソースの欄へ乗り移る (不変条件 1)。

use common::model::{AutomationTarget, ModParam};
use daw_ui_core::{
    Edit, MsegAction, MsegNode, ScrubCurve, ScrubableNumberFormat, ScrubableNumberStyle, Ui,
};
use daw_ui_renderer::Rect;

use common::modulators::ModTime;

use crate::app::{AppData, AppEvent, ModSourceRow};

use super::preview::Series;
use crate::view::track_inspector::{scrub_style, toggle_audio_style};

use super::{
    GAP, MOD_BAND_DEFAULT, MOD_CANVAS_H, MOD_HZ_W, MOD_RATE_DROPDOWN_W, ModBodyCtx, ROW_H,
    ROW_PITCH,
};

// ラベルについて: ラックのツマミは **1 文字〜数文字の記号**で呼ぶ (`φ` / `w` / `A` /
// `R` / `G` / `HP` / `LP` / `Smooth` / `slew`)。 インスペクタの実効幅は 256px しかなく、
// `ModParam::label()` の全名 (`位相` / `なめらかさ` / `Attack` …) を並べると 1 行に
// 収まらない。 **`label()` は「幅のある場所」 の SSoT** — オートメーションレーン名と
// `automation_target_label` — で、 ここはその短縮表示という関係。 どちらかを直すときは
// 両方を見ること (ラックだけ変えると、 レーン名と呼び名が食い違う)。

/// モジュレーターのツマミ 1 本の記述 ([`mod_param_field`] の引数)。
struct ModParamField<'a> {
    sid: u32,
    param: ModParam,
    rect: Rect,
    /// dblclick リセット先 (plain 単位)。
    default_plain: f64,
    /// 値の後ろに出す単位 (`"Hz"` / `"ms"` / `"\u{00d7}"`)。恒等 0..=1 の欄は空。
    unit: &'static str,
    /// 値を書き戻す `Edit` を作る (種別ごとに撃つ event が違うので caller が渡す)。
    on_change: &'a dyn Fn(f64) -> Edit<AppData>,
}

/// モジュレーターのツマミ 1 本を描く。 **値もレンジも SSoT を引き、 そのまま変調先になる。**
///
/// - 値 = `common::mod_graph::param_plain` (ラック / レーンの既定値 / 変調の base が同じ 1 本)。
/// - レンジと対数 / 恒等の別 = `common::automation::mod_param_range`。 `plain_to_norm` が
///   同じ 1 本を引くので、 **ツマミの端と変調の端が必ず一致する** (片方に数値を写すと、
///   深さ 1.0 の変調がツマミの端に届かない / 届く前に飽和する形で静かに壊れる)。
/// - 変調は既存の per-control idiom (`build_mod` → `Modulation`) をそのまま渡すだけ。
///   ◉ で待受中は press+drag が depth 編集に切り替わる (bespoke な widget は作らない)。
///
/// 戻り値 = ドラッグ / 数値入力中か。
fn mod_param_field(ui: &mut Ui<'_, AppData>, cx: &ModBodyCtx<'_>, f: ModParamField<'_>) -> bool {
    // `mod_param_range` の `None` は「レンジ無し」 ではなく **0..=1 の恒等** (同関数の契約)。
    // widget へ `None` をそのまま渡すと `clamp_opt` が素通しになり、 (a) 0..=1 の外の値が
    // モデルへ入り、 (b) 値→x 写像が立たないので変調の色帯と live tick が描かれない。
    // 対数かどうかの判定だけを別に持ち、 widget には必ず実レンジを渡す。
    let log = common::automation::mod_param_range(f.param);
    let style = ScrubableNumberStyle {
        range: Some(log.unwrap_or((0.0, 1.0))),
        curve: if log.is_some() { ScrubCurve::Log } else { ScrubCurve::Linear },
        unit: f.unit,
        // 対数欄は正規化領域の units_per_pixel なので、 何桁またいでも全域 250px。
        // 恒等 0..=1 の欄は従来の細かい感度を保つ。
        sensitivity: if log.is_some() { 1.0 / 250.0 } else { 0.006 },
        ..scrub_style(&cx.app.theme)
    };
    let fmt = if log.is_some() {
        ScrubableNumberFormat::Significant { digits: 3 }
    } else {
        ScrubableNumberFormat::Decimal(2)
    };
    let base = cx.app.mod_param_plain_value(f.sid, f.param);
    // 置き場は「そのソースが属するトラック」。 target だけから決める全域関数は作らない。
    let track_id = cx
        .app
        .song_doc
        .song()
        .mod_source_owner(f.sid)
        .unwrap_or(common::model::MASTER_TRACK_ID);
    let target = AutomationTarget::ModSourceParam { source_id: f.sid, param: f.param };
    let mb = crate::view::modulation::build_mod(
        cx.app,
        target.clone(),
        base,
        crate::view::modulation::PLAIN_IDENT,
        track_id,
    );
    let resp = ui.scrubable_number_at(
        ("inspector_mod_param", f.sid, f.param),
        f.rect,
        base,
        f.default_plain,
        fmt,
        &style,
        f.on_change,
        None,
        Some(mb.modulation()),
    );
    // 深さドラッグの立ち下がりが **◉ を解除する唯一の口** (`ScrubGesture::ModDepth` →
    // `connect_armed_mod_source_to`)。 他の全 per-control 呼び出し元と同じ idiom。
    //
    // **`mod_dragging` を `any_mod_drag` (= `ScrubGesture::ModRack`) に混ぜないこと** —
    // gesture の所有者は 1 度に 1 本で `open()` が既存を必ず `close()` するので、 同じ
    // フレームで両方 active にすると後勝ちで ModDepth が即 close され、 ドラッグの初回
    // フレームで arm が外れて以降の深さが追従しなくなる。
    crate::view::modulation::push_mod_depth_bracket(
        ui,
        cx.app,
        track_id,
        &target,
        resp.mod_dragging,
    );
    resp.dragging || resp.editing_text
}

// generator の rate (tempo 同期 division か Free Hz) 選択肢。
const MOD_RATE_DIVS: [(&str, u32, u32); 9] = [
    ("1/16", 1, 16),
    ("1/8T", 1, 12),
    ("1/8", 1, 8),
    ("1/4T", 1, 6),
    ("1/4", 1, 4),
    ("1/2", 1, 2),
    ("1bar", 1, 1),
    ("2bar", 2, 1),
    ("4bar", 4, 1),
];

/// rate ドロップダウン (tempo 同期 division + 絶対周波数)。pick で `EditModSource::Rate` を emit。
///
/// **末尾は `"Hz"`** (旧 `"Free"`)。 retrigger トグルの `"Free"` (= FreeRun) と語が衝突し、
/// LFO では 22px 上下に並んで「どちらの Free か」 が読めなかった。 意味が違う 2 つに
/// 別の名前を与える (r.md #88-5)。
fn mod_rate_control(
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    rate: &common::model::ModRate,
    sid: u32,
) {
    use common::model::{ModRate, ModRateMode};
    let mut labels: Vec<&str> = MOD_RATE_DIVS.iter().map(|(l, _, _)| *l).collect();
    labels.push("Hz");
    let sel = match rate.mode {
        ModRateMode::Sync => MOD_RATE_DIVS
            .iter()
            .position(|(_, n, d)| *n == rate.numerator && *d == rate.denominator)
            .unwrap_or(4),
        ModRateMode::Free => MOD_RATE_DIVS.len(),
    };
    if let Some(picked) = ui.dropdown(("inspector_mod_rate", sid), rect, &labels, sel) {
        // r.md #88 Q5: 拍と Hz の値は **両方保持**する。 切り替えても値が消えない。
        let base = *rate;
        let new_rate = if picked < MOD_RATE_DIVS.len() {
            let (_, n, d) = MOD_RATE_DIVS[picked];
            ModRate { mode: ModRateMode::Sync, numerator: n, denominator: d, ..base }
        } else {
            ModRate { mode: ModRateMode::Free, ..base }
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource {
                id: sid,
                edit: crate::app::ModSourceEdit::Rate(new_rate),
            });
        }));
    }
}

/// MSEG を q∈[0,1] で `n+1` 点サンプルする (描画 == 評価の SSoT、 `mseg_sample` 直呼び)。
fn mseg_samples(c: &common::model::MsegConfig, n: usize) -> Vec<(f32, f32)> {
    (0..=n)
        .map(|i| {
            let q = i as f32 / n as f32;
            (q, common::modulators::mseg_sample(&c.points, q))
        })
        .collect()
}

/// MSEG breakpoint を widget の [`MsegNode`] へ写す。
fn mseg_nodes(c: &common::model::MsegConfig) -> Vec<MsegNode> {
    c.points
        .iter()
        .map(|p| MsegNode { time: p.time, value: p.value, curve: p.curve })
        .collect()
}

/// プレビューに描く cycle_pos の範囲 (= 何ステップ/周期分を横幅いっぱいに見せるか)。
/// **Random は 1 周期 = 1 ステップ** なので、 1 だと「a→b の 1 区間」 しか出ず坂に見える。
/// 8 ステップ並べてランダムらしさ (Smooth で階段⇄波) を見せる。 LFO/MSEG/Steps は 1 周期で
/// 1 波形/全シーケンスが収まるので 1。
fn preview_cycles(kind: &common::model::ModSourceKind) -> f64 {
    match kind {
        common::model::ModSourceKind::Random(_) => 8.0,
        _ => 1.0,
    }
}

/// cycle_pos の値 `cp` に対応する (beat, secs) を rate/retrig から逆算する。 preview の
/// スクロール窓を **実際の transport 位置** で評価して「波形 == 実値」 を保つため。
fn cp_to_pos(
    rate: &common::model::ModRate,
    retrig: &common::model::RetriggerMode,
    cp: f64,
) -> (f64, f64) {
    use common::model::{ModRateMode, RetriggerMode};
    match rate.mode {
        ModRateMode::Sync => {
            let period = rate.period_beats();
            let anchor = match retrig {
                RetriggerMode::FromBeat { anchor_beat } => *anchor_beat,
                RetriggerMode::FreeRun => 0.0,
            };
            (cp * period + anchor, 0.0)
        }
        ModRateMode::Free => (0.0, cp / f64::from(rate.hz.max(1e-6))),
    }
}

/// generator をプレビュー用に `n+1` 点サンプルする。
/// - 周期波 (LFO/MSEG/Steps) は 1 周期 `[0,1]` を固定表示 (周期的なのでカーソル位置の値 =
///   実値で常に一致する)。
/// - **Random は非周期** (各 step が seed から決まる別値) なので、 再生位置 `cp_now` を中心に
///   `span` ステップの窓を取って **スクロール** させる。 こうすると表示波形が常に実際に鳴って
///   いる値そのものになり、 カーソルが実値の上に乗る (固定窓だと 8 step 超で実値とずれる)。
fn generator_cycle_samples(
    kind: &common::model::ModSourceKind,
    n: usize,
    beat: f64,
    secs: f64,
) -> Vec<(f32, f32)> {
    use common::model::ModSourceKind as K;
    let (rate, retrig) = match kind {
        K::Lfo(c) => (c.rate, c.retrigger),
        K::Random(c) => (c.rate, c.retrigger),
        K::Steps(c) => (c.rate, c.retrigger),
        K::Mseg(c) => (c.rate, c.retrigger),
        K::EnvelopeFollower { .. } => return Vec::new(),
    };
    let span = preview_cycles(kind);
    // Random だけ再生位置中心の窓 (左端 0 未満は clamp)。 周期波は 0 起点固定。
    let win_start = if matches!(kind, K::Random(_)) {
        let cp_now = common::modulators::cycle_pos(&rate, ModTime::new(beat, secs), &retrig);
        (cp_now - span * 0.5).max(0.0)
    } else {
        0.0
    };
    (0..=n)
        .map(|i| {
            let f = i as f32 / n as f32;
            let cp_s = win_start + f64::from(f) * span;
            let (b, s) = cp_to_pos(&rate, &retrig, cp_s);
            let v = common::modulators::generator_scalar(kind, ModTime::new(b, s)).unwrap_or(0.0);
            (f, v)
        })
        .collect()
}

/// MSEG の周期位置 → カーソルの 0..=1 位置 (play_mode の折り返しを含む)。
/// 閉形式と積分位相の **どちらを渡しても同じ折り方**になるよう 1 本にしてある。
fn mseg_cursor_phase(c: &common::model::MsegConfig, cp: f64) -> Option<f32> {
    use common::model::MsegPlayMode;
    let q = match c.play_mode {
        MsegPlayMode::OneShot => cp.clamp(0.0, 1.0),
        MsegPlayMode::Loop => cp.rem_euclid(1.0),
        MsegPlayMode::PingPong => {
            let t = cp.rem_euclid(2.0);
            if t <= 1.0 { t } else { 2.0 - t }
        }
    };
    #[allow(clippy::cast_possible_truncation)]
    Some(q as f32)
}

/// generator の現在位相 (0..=1、 ライブカーソル用)。 MSEG は play_mode の fold も反映。
fn generator_phase(kind: &common::model::ModSourceKind, beat: f64, secs: f64) -> Option<f32> {
    use common::model::ModSourceKind as K;
    let (rate, retrig) = match kind {
        K::Lfo(c) => (c.rate, c.retrigger),
        K::Random(c) => (c.rate, c.retrigger),
        K::Steps(c) => (c.rate, c.retrigger),
        K::Mseg(c) => (c.rate, c.retrigger),
        K::EnvelopeFollower { .. } => return None,
    };
    let cp = common::modulators::cycle_pos(&rate, ModTime::new(beat, secs), &retrig);
    let q = match kind {
        K::Mseg(c) => f64::from(mseg_cursor_phase(c, cp).unwrap_or(0.0)),
        // Random は preview が再生位置中心のスクロール窓なので、 カーソルも同じ窓内の相対位置
        // (= 常に実値の上)。 generator_cycle_samples の win_start と一致させる。
        K::Random(_) => {
            let span = preview_cycles(kind);
            let win_start = (cp - span * 0.5).max(0.0);
            (cp - win_start) / span
        }
        _ => cp.rem_euclid(1.0),
    };
    Some(q as f32)
}

/// rate dropdown + (Hz のとき) 対数 Hz スクラバを描く。 Hz drag 中は true を返す。
///
/// Hz 欄は **対数目盛 + 単位 "Hz" + 有効数字 3 桁** (設計正本 Q6、 Vital 準拠)。
/// 旧実装は線形 `sensitivity 0.05` の `0.01..=50` で、 (a) 全域に約 1000px のドラッグが
/// 要り、 (b) `Decimal(2)` なので下端 2 桁が `"0.00"` に潰れ、 (c) 単位表示が無く
/// 兄弟欄 (`φ` / `w` / `Smooth`) と非対称だった (r.md #88-1/2)。
fn mod_rate_full(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    x: f32,
    y: f32,
    rate: &common::model::ModRate,
    sid: u32,
) -> bool {
    use common::model::{ModRate, ModRateMode};
    mod_rate_control(ui, Rect { x, y, w: MOD_RATE_DROPDOWN_W, h: ROW_H }, rate, sid);
    if rate.mode != ModRateMode::Free {
        return false;
    }
    // 音価に戻しても `hz` は残る (`ModRate` が両方持つ) ので、 `..base` で他方を保つ。
    let base = *rate;
    mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::Rate,
            rect: Rect { x: x + MOD_RATE_DROPDOWN_W + 4.0, y, w: MOD_HZ_W, h: ROW_H },
            default_plain: 1.0,
            unit: "Hz",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: crate::app::ModSourceEdit::Rate(ModRate { hz: v as f32, ..base }),
                    });
                })
            },
        },
    )
}

/// rate 行の総幅 (dropdown + 間隔 + Hz 欄)。 隣に何かを置く行 (MSEG / Steps) が
/// **この 1 か所から** 次の x を出す (手写しした定数がずれて欄が重なるのを止める)。
const MOD_RATE_FULL_W: f32 = MOD_RATE_DROPDOWN_W + 4.0 + MOD_HZ_W;

/// retrigger トグル: Free ⇄ 「再生位置を起点に restart」 (FromBeat{playhead})。
fn mod_retrigger_toggle(
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    retrig: &common::model::RetriggerMode,
    sid: u32,
    playhead_beat: f64,
) {
    use common::model::RetriggerMode;
    let from = matches!(retrig, RetriggerMode::FromBeat { .. });
    ui.button_at(
        ("inspector_mod_retrig", sid),
        if from { "\u{27f2}here" } else { "Free" },
        rect,
        move || {
            Edit::mutate(move |app: &mut AppData| {
                let next = if from {
                    RetriggerMode::FreeRun
                } else {
                    RetriggerMode::FromBeat { anchor_beat: playhead_beat }
                };
                app.handle_event(AppEvent::EditModSource {
                    id: sid,
                    edit: crate::app::ModSourceEdit::Retrigger(next),
                });
            })
        },
    );
}

/// ソースの **周期位置** (未ラップ、 cycles)。 MSEG のカーソルと Steps の点灯段が
/// これを引く。
///
/// 速さが変調されている間、 閉形式の `cycle_pos` は engine の積分位相と食い違う —
/// カーブ / 段の形は base 値の静的表示なので正しいのに、 **カーソルと点灯段だけが嘘**
/// になる。 変調が掛かっているソースは窓と同じ walk で積分位相を読む
/// (`preview::cross_mod_phase`)。 掛かっていなければ従来どおり閉形式 (O(1))。
fn source_cycle_pos(
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    rate: &common::model::ModRate,
    retrig: &common::model::RetriggerMode,
) -> f64 {
    if is_cross_modulated(cx, src.id)
        && let Some(p) = super::preview::cross_mod_phase(cx.app, cx.plan, src.id, cx.beat, cx.secs)
    {
        return p;
    }
    common::modulators::cycle_pos(rate, ModTime::new(cx.beat, cx.secs), retrig)
}

/// このソースに変調が掛かっているか (plan の入力辺が 1 本でもあるか)。
fn is_cross_modulated(cx: &ModBodyCtx<'_>, sid: u32) -> bool {
    cx.plan
        .nodes
        .iter()
        .find(|n| n.source_id == sid)
        .is_some_and(|n| !n.in_edges.is_empty())
}

/// プレビューの 2 系列 `(前景, 薄く重ねる基準)` とカーソル位置を出す。
///
/// **クロス変調が掛かっているソース** (= plan の入力辺が 1 本でもある) は、 周期的で
/// なくなるので固定窓では今鳴っている形を描けない。 再生位置中心の時間窓へ倒し、
/// 変調前の形を薄く重ねる (r.md #89 Q8)。 掛かっていなければ従来どおり 1 周期を固定表示
/// (基準系列は空 = 描かない)。
fn preview_series(
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
) -> (Series, Series, Option<f32>) {
    if is_cross_modulated(cx, src.id) {
        let (fg, ghost) =
            super::preview::cross_mod_window(cx.app, cx.plan, src.id, cx.beat, cx.secs);
        let cursor = super::preview::cross_mod_cursor(cx.app, cx.plan, src.id, cx.secs);
        (fg, ghost, cursor)
    } else {
        (
            generator_cycle_samples(&src.kind, 160, cx.beat, cx.secs),
            Series::new(),
            generator_phase(&src.kind, cx.beat, cx.secs),
        )
    }
}

/// LFO 本体 (プレビュー / shape / rate / φ / Pulse width / retrigger)。
pub(super) fn draw_lfo_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::LfoConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx, p) = (src.id, cx.lx, &cx.app.theme.core);
    let mut drag = false;

    let (fg, ghost, cursor) = preview_series(cx, src);
    ui.signal_preview(
        ("inspector_lfo_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &fg,
        &ghost,
        cursor,
        cx.editor,
    );
    y += MOD_CANVAS_H + 4.0;

    // row A: shape + rate(+Hz)
    let shapes = ["Sin", "Tri", "SawU", "SawD", "Sqr", "Pulse"];
    let ssel = match c.shape {
        common::model::LfoShape::Sine => 0,
        common::model::LfoShape::Triangle => 1,
        common::model::LfoShape::SawUp => 2,
        common::model::LfoShape::SawDown => 3,
        common::model::LfoShape::Square => 4,
        common::model::LfoShape::Pulse { .. } => 5,
    };
    // Pulse の現 width を保持して shape 切替時に維持。
    let cur_width = if let common::model::LfoShape::Pulse { width } = c.shape { width } else { 0.5 };
    if let Some(pick) = ui.dropdown(
        ("inspector_lfo_shape", sid),
        Rect { x: lx, y, w: 56.0, h: ROW_H },
        &shapes,
        ssel,
    ) {
        let shape = match pick {
            0 => common::model::LfoShape::Sine,
            1 => common::model::LfoShape::Triangle,
            2 => common::model::LfoShape::SawUp,
            3 => common::model::LfoShape::SawDown,
            4 => common::model::LfoShape::Square,
            _ => common::model::LfoShape::Pulse { width: cur_width },
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoShape(shape) });
        }));
    }
    drag |= mod_rate_full(ui, cx, lx + 62.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: φ phase + (Pulse width) + retrig
    ui.label_at(("inspector_lfo_ph_lbl", sid), "\u{03c6}", lx, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::LfoPhase,
            rect: Rect { x: lx + 12.0, y, w: 50.0, h: ROW_H },
            default_plain: 0.0,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::LfoPhase(v as f32),
                    });
                })
            },
        },
    );
    let mut next_x = lx + 68.0;
    if matches!(c.shape, common::model::LfoShape::Pulse { .. }) {
        ui.label_at(("inspector_lfo_w_lbl", sid), "w", next_x, y + 4.0, 10.0, p.text);
        drag |= draw_lfo_width_field(ui, cx, sid, next_x + 12.0, y);
        next_x += 64.0;
    }
    mod_retrigger_toggle(ui, Rect { x: next_x, y, w: 56.0, h: ROW_H }, &c.retrigger, sid, cx.beat);
    (y + ROW_PITCH, drag)
}

/// Pulse の duty (`w`) 欄。 **`draw_lfo_body` の `if` の中に置かない** — 閉包 + 構造体
/// リテラルが重なってインデントが 7 段に届き、 1 関数 6 段の budget を割る (不変条件 9)。
fn draw_lfo_width_field(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    sid: u32,
    x: f32,
    y: f32,
) -> bool {
    use crate::app::ModSourceEdit as E;
    mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::LfoPulseWidth,
            rect: Rect { x, y, w: 46.0, h: ROW_H },
            default_plain: 0.5,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::LfoShape(common::model::LfoShape::Pulse { width: v as f32 }),
                    });
                })
            },
        },
    )
}

/// Random 本体 (プレビュー / Smooth / rate / 引き直し / retrigger)。
pub(super) fn draw_random_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::RandomConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx, p) = (src.id, cx.lx, &cx.app.theme.core);
    let mut drag = false;

    let (fg, ghost, cursor) = preview_series(cx, src);
    ui.signal_preview(
        ("inspector_rand_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &fg,
        &ghost,
        cursor,
        cx.editor,
    );
    y += MOD_CANVAS_H + 4.0;

    // row A: Stepped↔Smooth morph (0=階段/S&H, 1=滑らか) + rate(+Hz)
    ui.label_at(("inspector_rand_sm_lbl", sid), "Smooth", lx, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::RandomSmooth,
            rect: Rect { x: lx + 48.0, y, w: 44.0, h: ROW_H },
            default_plain: 1.0,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::RandomSmooth(v as f32),
                    });
                })
            },
        },
    );
    drag |= mod_rate_full(ui, cx, lx + 96.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: 「別の乱数パターンを引き直す」 ボタン (raw seed は内部値なので隠す) + retrig
    ui.button_at(
        ("inspector_rand_reroll", sid),
        "\u{21bb} Randomize",
        Rect { x: lx, y, w: 100.0, h: ROW_H },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::RerollSeed });
            })
        },
    );
    mod_retrigger_toggle(
        ui,
        Rect { x: lx + 108.0, y, w: 56.0, h: ROW_H },
        &c.retrigger,
        sid,
        cx.beat,
    );
    (y + ROW_PITCH, drag)
}

/// MSEG 本体 (curve エディタ / play_mode / rate / retrigger)。
pub(super) fn draw_mseg_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::MsegConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx) = (src.id, cx.lx);

    let nodes = mseg_nodes(c);
    let samples = mseg_samples(c, 160);
    let phase = mseg_cursor_phase(c, source_cycle_pos(cx, src, &c.rate, &c.retrigger));
    let resp = ui.mseg_editor(
        ("inspector_mseg_canvas", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &nodes,
        &samples,
        phase,
        cx.editor,
        move |act| {
            Edit::mutate(move |app: &mut AppData| {
                let edit = match act {
                    MsegAction::Move { index, time, value } => {
                        E::MsegMovePoint { index, time, value }
                    }
                    MsegAction::Add { time, value } => E::MsegAddPoint { time, value },
                    MsegAction::SetCurve { segment, curve } => E::MsegSetCurve { segment, curve },
                    MsegAction::Delete { index } => E::MsegRemovePoint(index),
                };
                app.handle_event(AppEvent::EditModSource { id: sid, edit });
            })
        },
    );
    let mut drag = resp.dragging;
    y += MOD_CANVAS_H + 4.0;

    // controls: play_mode + rate(+Hz) + retrig
    let pmodes = ["1shot", "Loop", "Ping"];
    let psel = match c.play_mode {
        common::model::MsegPlayMode::OneShot => 0,
        common::model::MsegPlayMode::Loop => 1,
        common::model::MsegPlayMode::PingPong => 2,
    };
    if let Some(pick) = ui.dropdown(
        ("inspector_mseg_play", sid),
        Rect { x: lx, y, w: 56.0, h: ROW_H },
        &pmodes,
        psel,
    ) {
        let pm = match pick {
            0 => common::model::MsegPlayMode::OneShot,
            1 => common::model::MsegPlayMode::Loop,
            _ => common::model::MsegPlayMode::PingPong,
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegPlayMode(pm) });
        }));
    }
    // retrig の x は rate 行の実寸から出す (手写しの定数だと Hz 欄と重なる)。
    let rate_x = lx + 62.0;
    drag |= mod_rate_full(ui, cx, rate_x, y, &c.rate, sid);
    mod_retrigger_toggle(
        ui,
        Rect { x: rate_x + MOD_RATE_FULL_W + GAP, y, w: 56.0, h: ROW_H },
        &c.retrigger,
        sid,
        cx.beat,
    );
    (y + ROW_PITCH, drag)
}

/// Steps 本体 (step grid / 段数 / 進行方向 / rate / slew / retrigger)。
pub(super) fn draw_steps_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::StepsConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx, p) = (src.id, cx.lx, &cx.app.theme.core);

    let cp = source_cycle_pos(cx, src, &c.rate, &c.retrigger);
    let current = Some(common::modulators::steps_active_index(c, cp));
    let resp = ui.step_grid(
        ("inspector_steps_grid", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &c.values,
        current,
        cx.editor,
        move |idx, v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource {
                    id: sid,
                    edit: E::StepValue { index: idx, value: v },
                });
            })
        },
    );
    let mut drag = resp.dragging;
    y += MOD_CANVAS_H + 4.0;

    // row A: [-][+] count + dir + rate(+Hz)
    let n = c.values.len();
    ui.button_at(
        ("inspector_steps_dec", sid),
        "\u{2212}",
        Rect { x: lx, y, w: 22.0, h: ROW_H },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource {
                    id: sid,
                    edit: E::StepsCount(n.saturating_sub(1)),
                });
            })
        },
    );
    ui.button_at(
        ("inspector_steps_inc", sid),
        "+",
        Rect { x: lx + 26.0, y, w: 22.0, h: ROW_H },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsCount(n + 1) });
            })
        },
    );
    ui.label_at(("inspector_steps_n", sid), &format!("{n}"), lx + 52.0, y + 4.0, 10.0, p.text);
    let dirs = ["Fwd", "Bwd", "Ping"];
    let dsel = match c.direction {
        common::model::StepsDirection::Forward => 0,
        common::model::StepsDirection::Backward => 1,
        common::model::StepsDirection::PingPong => 2,
    };
    if let Some(pick) = ui.dropdown(
        ("inspector_steps_dir", sid),
        Rect { x: lx + 74.0, y, w: 50.0, h: ROW_H },
        &dirs,
        dsel,
    ) {
        let dir = match pick {
            0 => common::model::StepsDirection::Forward,
            1 => common::model::StepsDirection::Backward,
            _ => common::model::StepsDirection::PingPong,
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsDirection(dir) });
        }));
    }
    drag |= mod_rate_full(ui, cx, lx + 130.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: slew + retrig
    ui.label_at(("inspector_steps_sl_lbl", sid), "slew", lx, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::StepsSlew,
            rect: Rect { x: lx + 32.0, y, w: 44.0, h: ROW_H },
            default_plain: 0.0,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::StepsSlew(v as f32),
                    });
                })
            },
        },
    );
    mod_retrigger_toggle(
        ui,
        Rect { x: lx + 82.0, y, w: 56.0, h: ROW_H },
        &c.retrigger,
        sid,
        cx.beat,
    );
    (y + ROW_PITCH, drag)
}

/// エンベロープフォロワー本体 (tap / A / R / mode / rectify / gain / 帯域)。
///
/// r.md #88: `gain` / `mode` / `rectify` / `band_filter` には **UI からの編集経路が
/// まったく無かった** (`event.rs` の Set 系は track / attack / release / tap_point の
/// 4 つだけ)。 変調先にできるのに素の値を触れない非対称を閉じる。
pub(super) fn draw_follower_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    sid: u32,
    tap: &common::model::AudioTap,
    f: &common::model::FollowerConfig,
    mut y: f32,
) -> (f32, bool) {
    use common::model::{BandFilter, FollowerMode};
    let (lx, p) = (cx.lx, &cx.app.theme.core);
    let theme = &cx.app.theme;
    let right = cx.area.x + cx.area.w - cx.pad;
    let mut drag = false;

    // --- row 1: tap 点 + A / R ---
    //
    // follower は cycle を持たない → canvas は出さない。
    // 既定表示 "Post-Fdr" = 8 字 * 14 * 0.527 = 59.1px。 dropdown の文字領域は
    // w - PAD_X(8) - ARROW_W(16) なので 84px 以上必要 (旧 64px では 40px しか
    // 無く、 ▼ シェブロンに完全に重なった上で右枠を 2.3px 越えていた)。
    let tap_w = 88.0;
    const TAP_POINTS: [common::model::TapPoint; 3] = [
        common::model::TapPoint::PreFx,
        common::model::TapPoint::PostFx,
        common::model::TapPoint::PostFader,
    ];
    let tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
    let tap_sel = TAP_POINTS.iter().position(|t| *t == tap.tap_point).unwrap_or(2);
    if let Some(pick) = ui.dropdown(
        ("inspector_mod_src_tap", sid),
        Rect { x: lx, y, w: tap_w, h: ROW_H },
        &tap_labels,
        tap_sel,
    ) && let Some(&tp) = TAP_POINTS.get(pick)
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetModSourceTapPoint { id: sid, tap_point: tp });
        }));
    }
    let rest_x = lx + tap_w + GAP;
    let half = (right - rest_x - GAP) / 2.0;
    let field_w = (half - 12.0).max(20.0);
    ui.label_at(("inspector_mod_a_lbl", sid), "A", rest_x, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::FollowerAttack,
            rect: Rect { x: rest_x + 12.0, y, w: field_w, h: ROW_H },
            default_plain: 1.0,
            unit: "ms",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetModSourceAttack { id: sid, ms: v as f32 });
                })
            },
        },
    );
    let r_x = rest_x + half + GAP;
    ui.label_at(("inspector_mod_r_lbl", sid), "R", r_x, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::FollowerRelease,
            rect: Rect { x: r_x + 12.0, y, w: field_w, h: ROW_H },
            default_plain: 100.0,
            unit: "ms",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetModSourceRelease { id: sid, ms: v as f32 });
                })
            },
        },
    );
    y += ROW_PITCH;

    // --- row 2: 検出モード + 整流 + ゲイン + 帯域の on/off ---
    const MODES: [FollowerMode; 2] = [FollowerMode::Peak, FollowerMode::Rms];
    let mode_labels = ["Peak", "RMS"];
    let mode_sel = MODES.iter().position(|m| *m == f.mode).unwrap_or(0);
    if let Some(pick) = ui.dropdown(
        ("inspector_mod_mode", sid),
        Rect { x: lx, y, w: 52.0, h: ROW_H },
        &mode_labels,
        mode_sel,
    ) && let Some(&mode) = MODES.get(pick)
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetModSourceMode { id: sid, mode });
        }));
    }
    let toggle_style = toggle_audio_style(theme);
    // 整流 / 帯域は真偽 2 状態なので、 ラベルを差し替える button ではなく
    // **点灯する toggle** を使う (inspector 共通の on/off アフォーダンス)。
    ui.toggle_button_at(
        ("inspector_mod_rectify", sid),
        "Rect",
        Rect { x: lx + 58.0, y, w: 44.0, h: ROW_H },
        f.rectify,
        &toggle_style,
        move |next| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModSourceRectify { id: sid, rectify: next });
            })
        },
    );
    ui.label_at(("inspector_mod_g_lbl", sid), "G", lx + 108.0, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::FollowerGain,
            rect: Rect { x: lx + 120.0, y, w: 52.0, h: ROW_H },
            default_plain: 1.0,
            unit: "\u{00d7}",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetModSourceGain { id: sid, gain: v as f32 });
                })
            },
        },
    );
    let band_on = f.band_filter.is_some();
    ui.toggle_button_at(
        ("inspector_mod_band", sid),
        "Band",
        Rect { x: lx + 178.0, y, w: 44.0, h: ROW_H },
        band_on,
        &toggle_style,
        move |next| {
            Edit::mutate(move |app: &mut AppData| {
                let band = next.then_some(MOD_BAND_DEFAULT);
                app.handle_event(AppEvent::SetModSourceBand { id: sid, band });
            })
        },
    );
    y += ROW_PITCH;

    // --- row 3: 帯域の cutoff (ON のときだけ) ---
    //
    // OFF のとき行ごと畳む。 灰色の触れない欄を出しておくより、 「無い = 全帯域」 が
    // そのまま見えるほうが読みやすい (行を増やしすぎない)。
    let Some(band) = f.band_filter else {
        return (y, drag);
    };
    ui.label_at(("inspector_mod_hp_lbl", sid), "HP", lx, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::FollowerHpHz,
            rect: Rect { x: lx + 18.0, y, w: 64.0, h: ROW_H },
            default_plain: f64::from(MOD_BAND_DEFAULT.hp_hz),
            unit: "Hz",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    let next = BandFilter { hp_hz: v as f32, lp_hz: band.lp_hz };
                    app.handle_event(AppEvent::SetModSourceBand { id: sid, band: Some(next) });
                })
            },
        },
    );
    ui.label_at(("inspector_mod_lp_lbl", sid), "LP", lx + 90.0, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::FollowerLpHz,
            rect: Rect { x: lx + 108.0, y, w: 64.0, h: ROW_H },
            default_plain: f64::from(MOD_BAND_DEFAULT.lp_hz),
            unit: "Hz",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    let next = BandFilter { hp_hz: band.hp_hz, lp_hz: v as f32 };
                    app.handle_event(AppEvent::SetModSourceBand { id: sid, band: Some(next) });
                })
            },
        },
    );
    (y + ROW_PITCH, drag)
}
