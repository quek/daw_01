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

use common::model::{
    MOD_BAND_HZ_MAX, MOD_BAND_HZ_MIN, MOD_FOLLOWER_GAIN_MAX, MOD_FOLLOWER_GAIN_MIN,
    MOD_FOLLOWER_TIME_MS_MAX, MOD_FOLLOWER_TIME_MS_MIN, MOD_RATE_HZ_MAX, MOD_RATE_HZ_MIN,
};
use daw_ui_core::{
    Edit, MsegAction, MsegNode, ScrubCurve, ScrubableNumberFormat, ScrubableNumberStyle, Ui,
};
use daw_ui_renderer::Rect;

use common::modulators::ModTime;

use crate::app::{AppData, AppEvent, ModSourceRow};
use crate::view::track_inspector::{scrub_style, toggle_audio_style};

use super::{
    GAP, MOD_BAND_DEFAULT, MOD_CANVAS_H, MOD_HZ_W, MOD_RATE_DROPDOWN_W, ModBodyCtx, ROW_H,
    ROW_PITCH,
};

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

/// generator の現在位相 (0..=1、 ライブカーソル用)。 MSEG は play_mode の fold も反映。
fn generator_phase(kind: &common::model::ModSourceKind, beat: f64, secs: f64) -> Option<f32> {
    use common::model::{MsegPlayMode, ModSourceKind as K};
    let (rate, retrig) = match kind {
        K::Lfo(c) => (c.rate, c.retrigger),
        K::Random(c) => (c.rate, c.retrigger),
        K::Steps(c) => (c.rate, c.retrigger),
        K::Mseg(c) => (c.rate, c.retrigger),
        K::EnvelopeFollower { .. } => return None,
    };
    let cp = common::modulators::cycle_pos(&rate, ModTime::new(beat, secs), &retrig);
    let q = match kind {
        K::Mseg(c) => match c.play_mode {
            MsegPlayMode::OneShot => cp.clamp(0.0, 1.0),
            MsegPlayMode::Loop => cp.rem_euclid(1.0),
            MsegPlayMode::PingPong => {
                let t = cp.rem_euclid(2.0);
                if t <= 1.0 { t } else { 2.0 - t }
            }
        },
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
    theme: &crate::theme::Theme,
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
    let hz_style = ScrubableNumberStyle {
        range: Some((f64::from(MOD_RATE_HZ_MIN), f64::from(MOD_RATE_HZ_MAX))),
        curve: ScrubCurve::Log,
        unit: "Hz",
        ..scrub_style(theme)
    };
    let resp = ui.scrubable_number_at(
        ("inspector_mod_hz", sid),
        Rect { x: x + MOD_RATE_DROPDOWN_W + 4.0, y, w: MOD_HZ_W, h: ROW_H },
        f64::from(base.hz),
        1.0,
        ScrubableNumberFormat::Significant { digits: 3 },
        &hz_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource {
                    id: sid,
                    edit: crate::app::ModSourceEdit::Rate(ModRate { hz: v as f32, ..base }),
                });
            })
        },
        None,
        None,
    );
    resp.dragging || resp.editing_text
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

    ui.signal_preview(
        ("inspector_lfo_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &generator_cycle_samples(&src.kind, 160, cx.beat, cx.secs),
        generator_phase(&src.kind, cx.beat, cx.secs),
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
    drag |= mod_rate_full(ui, &cx.app.theme, lx + 62.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: φ phase + (Pulse width) + retrig
    ui.label_at(("inspector_lfo_ph_lbl", sid), "\u{03c6}", lx, y + 4.0, 10.0, p.text);
    let ph_resp = ui.scrubable_number_at(
        ("inspector_lfo_phase", sid),
        Rect { x: lx + 12.0, y, w: 50.0, h: ROW_H },
        f64::from(c.phase),
        0.0,
        ScrubableNumberFormat::Decimal(2),
        &cx.unit_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoPhase(v as f32) });
            })
        },
        None,
        None,
    );
    drag |= ph_resp.dragging || ph_resp.editing_text;
    let mut next_x = lx + 68.0;
    if let common::model::LfoShape::Pulse { width } = c.shape {
        ui.label_at(("inspector_lfo_w_lbl", sid), "w", next_x, y + 4.0, 10.0, p.text);
        let w_resp = ui.scrubable_number_at(
            ("inspector_lfo_width", sid),
            Rect { x: next_x + 12.0, y, w: 46.0, h: ROW_H },
            f64::from(width),
            0.5,
            ScrubableNumberFormat::Decimal(2),
            &cx.unit_style,
            move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::LfoShape(common::model::LfoShape::Pulse { width: v as f32 }),
                    });
                })
            },
            None,
            None,
        );
        drag |= w_resp.dragging || w_resp.editing_text;
        next_x += 64.0;
    }
    mod_retrigger_toggle(ui, Rect { x: next_x, y, w: 56.0, h: ROW_H }, &c.retrigger, sid, cx.beat);
    (y + ROW_PITCH, drag)
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

    ui.signal_preview(
        ("inspector_rand_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &generator_cycle_samples(&src.kind, 160, cx.beat, cx.secs),
        generator_phase(&src.kind, cx.beat, cx.secs),
        cx.editor,
    );
    y += MOD_CANVAS_H + 4.0;

    // row A: Stepped↔Smooth morph (0=階段/S&H, 1=滑らか) + rate(+Hz)
    ui.label_at(("inspector_rand_sm_lbl", sid), "Smooth", lx, y + 4.0, 10.0, p.text);
    let sm_resp = ui.scrubable_number_at(
        ("inspector_rand_smooth", sid),
        Rect { x: lx + 48.0, y, w: 44.0, h: ROW_H },
        f64::from(c.smooth),
        1.0,
        ScrubableNumberFormat::Decimal(2),
        &cx.unit_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource {
                    id: sid,
                    edit: E::RandomSmooth(v as f32),
                });
            })
        },
        None,
        None,
    );
    drag |= sm_resp.dragging || sm_resp.editing_text;
    drag |= mod_rate_full(ui, &cx.app.theme, lx + 96.0, y, &c.rate, sid);
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
    let phase = generator_phase(&src.kind, cx.beat, cx.secs);
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
    drag |= mod_rate_full(ui, &cx.app.theme, rate_x, y, &c.rate, sid);
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

    let cp = common::modulators::cycle_pos(
        &c.rate,
        ModTime::new(cx.beat, cx.secs),
        &c.retrigger,
    );
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
    drag |= mod_rate_full(ui, &cx.app.theme, lx + 130.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: slew + retrig
    ui.label_at(("inspector_steps_sl_lbl", sid), "slew", lx, y + 4.0, 10.0, p.text);
    let sl_resp = ui.scrubable_number_at(
        ("inspector_steps_slew", sid),
        Rect { x: lx + 32.0, y, w: 44.0, h: ROW_H },
        f64::from(c.slew),
        0.0,
        ScrubableNumberFormat::Decimal(2),
        &cx.unit_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsSlew(v as f32) });
            })
        },
        None,
        None,
    );
    drag |= sl_resp.dragging || sl_resp.editing_text;
    mod_retrigger_toggle(
        ui,
        Rect { x: lx + 82.0, y, w: 56.0, h: ROW_H },
        &c.retrigger,
        sid,
        cx.beat,
    );
    (y + ROW_PITCH, drag)
}

/// 対数目盛 + 単位付きのスクラバ style。 フォロワーの A / R / gain / 帯域はどれも
/// 下端と上端が 3〜5 桁離れるので、 線形だと実用感度で全域に数万〜数十万 px 要る。
fn log_scrub_style(
    theme: &crate::theme::Theme,
    lo: f32,
    hi: f32,
    unit: &'static str,
) -> ScrubableNumberStyle {
    ScrubableNumberStyle {
        range: Some((f64::from(lo), f64::from(hi))),
        curve: ScrubCurve::Log,
        unit,
        ..scrub_style(theme)
    }
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
    let ar_style =
        log_scrub_style(theme, MOD_FOLLOWER_TIME_MS_MIN, MOD_FOLLOWER_TIME_MS_MAX, "ms");
    ui.label_at(("inspector_mod_a_lbl", sid), "A", rest_x, y + 4.0, 10.0, p.text);
    let a_resp = ui.scrubable_number_at(
        ("inspector_mod_attack", sid),
        Rect { x: rest_x + 12.0, y, w: (half - 12.0).max(20.0), h: ROW_H },
        f64::from(f.attack_ms),
        1.0,
        ScrubableNumberFormat::Significant { digits: 3 },
        &ar_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModSourceAttack { id: sid, ms: v as f32 });
            })
        },
        None,
        None,
    );
    let r_x = rest_x + half + GAP;
    ui.label_at(("inspector_mod_r_lbl", sid), "R", r_x, y + 4.0, 10.0, p.text);
    let r_resp = ui.scrubable_number_at(
        ("inspector_mod_release", sid),
        Rect { x: r_x + 12.0, y, w: (half - 12.0).max(20.0), h: ROW_H },
        f64::from(f.release_ms),
        100.0,
        ScrubableNumberFormat::Significant { digits: 3 },
        &ar_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModSourceRelease { id: sid, ms: v as f32 });
            })
        },
        None,
        None,
    );
    drag |= a_resp.dragging || a_resp.editing_text || r_resp.dragging || r_resp.editing_text;
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
    let gain_style =
        log_scrub_style(theme, MOD_FOLLOWER_GAIN_MIN, MOD_FOLLOWER_GAIN_MAX, "\u{00d7}");
    let g_resp = ui.scrubable_number_at(
        ("inspector_mod_gain", sid),
        Rect { x: lx + 120.0, y, w: 52.0, h: ROW_H },
        f64::from(f.gain),
        1.0,
        ScrubableNumberFormat::Significant { digits: 3 },
        &gain_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModSourceGain { id: sid, gain: v as f32 });
            })
        },
        None,
        None,
    );
    drag |= g_resp.dragging || g_resp.editing_text;
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
    let band_style = log_scrub_style(theme, MOD_BAND_HZ_MIN, MOD_BAND_HZ_MAX, "Hz");
    ui.label_at(("inspector_mod_hp_lbl", sid), "HP", lx, y + 4.0, 10.0, p.text);
    let hp_resp = ui.scrubable_number_at(
        ("inspector_mod_hp", sid),
        Rect { x: lx + 18.0, y, w: 64.0, h: ROW_H },
        f64::from(band.hp_hz),
        f64::from(MOD_BAND_DEFAULT.hp_hz),
        ScrubableNumberFormat::Significant { digits: 3 },
        &band_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                let next = BandFilter { hp_hz: v as f32, lp_hz: band.lp_hz };
                app.handle_event(AppEvent::SetModSourceBand { id: sid, band: Some(next) });
            })
        },
        None,
        None,
    );
    ui.label_at(("inspector_mod_lp_lbl", sid), "LP", lx + 90.0, y + 4.0, 10.0, p.text);
    let lp_resp = ui.scrubable_number_at(
        ("inspector_mod_lp", sid),
        Rect { x: lx + 108.0, y, w: 64.0, h: ROW_H },
        f64::from(band.lp_hz),
        f64::from(MOD_BAND_DEFAULT.lp_hz),
        ScrubableNumberFormat::Significant { digits: 3 },
        &band_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                let next = BandFilter { hp_hz: band.hp_hz, lp_hz: v as f32 };
                app.handle_event(AppEvent::SetModSourceBand { id: sid, band: Some(next) });
            })
        },
        None,
        None,
    );
    drag |= hp_resp.dragging || hp_resp.editing_text || lp_resp.dragging || lp_resp.editing_text;
    (y + ROW_PITCH, drag)
}
