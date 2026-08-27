//! トラック inspector のモジュレーションラック描画。
//!
//! LFO / Random / MSEG / Steps / EnvelopeFollower の各ソースを折りたたみ行 +
//! グラフィカルエディタで描き、 ソース直下に routing (depth / polarity / remove) を
//! 出す。 親モジュール `track_inspector` の `draw()` から scroll viewport 末尾に
//! 1 回だけ呼ばれる (`draw_modulation_rack`)。

use daw_ui_core::{
    Edit, MsegAction, MsegEditorStyle, MsegNode, ScrubableNumberFormat, ScrubableNumberStyle, Ui,
};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};

// 数値 field の base style は inspector 全体で共有するので親モジュールから借りる
// (ラベル色は `app.theme.core.text` を直接読む)。
use super::scrub_style;

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

/// rate ドロップダウン (tempo 同期 division + Free)。pick で `EditModSource::Rate` を emit。
fn mod_rate_control(
    ui: &mut Ui<'_, AppData>,
    id_seed: u32,
    rect: Rect,
    rate: &common::model::ModRate,
    sid: u32,
) {
    use common::model::ModRate;
    let mut labels: Vec<&str> = MOD_RATE_DIVS.iter().map(|(l, _, _)| *l).collect();
    labels.push("Free");
    let sel = match rate {
        ModRate::Sync {
            numerator,
            denominator,
        } => MOD_RATE_DIVS
            .iter()
            .position(|(_, n, d)| n == numerator && d == denominator)
            .unwrap_or(4),
        ModRate::Free { .. } => MOD_RATE_DIVS.len(),
    };
    if let Some(picked) = ui.dropdown(("inspector_mod_rate", id_seed), rect, &labels, sel) {
        let new_rate = if picked < MOD_RATE_DIVS.len() {
            let (_, n, d) = MOD_RATE_DIVS[picked];
            ModRate::Sync {
                numerator: n,
                denominator: d,
            }
        } else {
            ModRate::Free { hz: 1.0 }
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource {
                id: sid,
                edit: crate::app::ModSourceEdit::Rate(new_rate),
            });
        }));
    }
}

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
    use common::model::{ModRate, RetriggerMode};
    match rate {
        ModRate::Sync { numerator, denominator } => {
            let period = 4.0 * f64::from(*numerator) / f64::from((*denominator).max(1));
            let anchor = match retrig {
                RetriggerMode::FromBeat { anchor_beat } => *anchor_beat,
                RetriggerMode::FreeRun => 0.0,
            };
            (cp * period + anchor, 0.0)
        }
        ModRate::Free { hz } => (0.0, cp / f64::from(hz.max(1e-6))),
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
        let cp_now = common::modulators::cycle_pos(&rate, beat, secs, &retrig);
        (cp_now - span * 0.5).max(0.0)
    } else {
        0.0
    };
    (0..=n)
        .map(|i| {
            let f = i as f32 / n as f32;
            let cp_s = win_start + f64::from(f) * span;
            let (b, s) = cp_to_pos(&rate, &retrig, cp_s);
            let v = common::modulators::generator_scalar(kind, b, s).unwrap_or(0.0);
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
    let cp = common::modulators::cycle_pos(&rate, beat, secs, &retrig);
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

/// rate dropdown + (Free のとき) Hz scrub を描く。 Hz drag 中は true を返す。
fn mod_rate_full(
    ui: &mut Ui<'_, AppData>,
    theme: &crate::theme::Theme,
    id_seed: u32,
    x: f32,
    y: f32,
    rate: &common::model::ModRate,
    sid: u32,
) -> bool {
    use common::model::ModRate;
    mod_rate_control(ui, id_seed, Rect { x, y, w: 52.0, h: 20.0 }, rate, sid);
    if let ModRate::Free { hz } = rate {
        let hz_style = ScrubableNumberStyle {
            range: Some((0.01, 50.0)),
            sensitivity: 0.05,
            ..scrub_style(theme)
        };
        let resp = ui.scrubable_number_at(
            ("inspector_mod_hz", id_seed),
            Rect { x: x + 56.0, y, w: 54.0, h: 20.0 },
            f64::from(*hz),
            1.0,
            ScrubableNumberFormat::Decimal(2),
            &hz_style,
            move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: crate::app::ModSourceEdit::Rate(ModRate::Free { hz: v as f32 }),
                    });
                })
            },
            None,
            None,
        );
        resp.dragging || resp.editing_text
    } else {
        false
    }
}

/// retrigger トグル: Free ⇄ 「再生位置を起点に restart」 (FromBeat{playhead})。
fn mod_retrigger_toggle(
    ui: &mut Ui<'_, AppData>,
    id_seed: u32,
    rect: Rect,
    retrig: &common::model::RetriggerMode,
    sid: u32,
    playhead_beat: f64,
) {
    use common::model::RetriggerMode;
    let from = matches!(retrig, RetriggerMode::FromBeat { .. });
    ui.button_at(
        ("inspector_mod_retrig", id_seed),
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
    use crate::app::ModSourceEdit as E;

    let p = &app.theme.core;
    let mod_style = mod_editor_style(&app.theme);
    let lx = area.x + pad;
    let row_w = area.w - pad * 2.0;
    let mod_sources = app.mod_source_display();
    let unit_style = ScrubableNumberStyle {
        range: Some((0.0, 1.0)),
        sensitivity: 0.006,
        ..scrub_style(&app.theme)
    };
    // ライブカーソル用の transport 位置。
    let beat = f64::from(app.transport.playhead_beat.unwrap_or(0.0));
    let secs = beat * 60.0 / f64::from(app.song_doc.song().bpm.max(1.0));

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
    // routing は各ソースの直下に出す。 widget id 用のグローバル連番。
    let mut route_i = 0usize;

    for (i, src) in mod_sources.iter().enumerate() {
        let sid = src.id;
        let i_u = i as u32;
        let expanded = app.ui_ephemeral.expanded_mod_sources.contains(&sid);

        // --- header row: [▸/▾][name/track] [meter] [arm] [×] ---
        let rm_rect = Rect { x: area.x + area.w - pad - 20.0, y, w: 20.0, h: 20.0 };
        let meter_w = 50.0;
        let meter_x = rm_rect.x - 4.0 - meter_w;
        let filled = ((src.scalar.clamp(0.0, 1.0) * 6.0).round() as usize).min(6);
        let meter: String = "\u{25ae}".repeat(filled) + &"\u{25af}".repeat(6 - filled);
        ui.label_at(("inspector_mod_src_meter", i), &meter, meter_x, y + 4.0, 11.0, p.text);
        let arm_w = 24.0;
        let arm_x = meter_x - 4.0 - arm_w;
        let armed = app.ui_ephemeral.armed_mod_source == Some(sid);
        ui.button_at(
            ("inspector_mod_src_arm", i),
            if armed { "\u{25c9}" } else { "\u{25cb}" },
            Rect { x: arm_x, y, w: arm_w, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    let next = if app.ui_ephemeral.armed_mod_source == Some(sid) { None } else { Some(sid) };
                    app.handle_event(AppEvent::SetArmedModSource(next));
                })
            },
        );
        // 展開トグル (chevron)。
        ui.button_at(
            ("inspector_mod_src_expand", i),
            if expanded { "\u{25be}" } else { "\u{25b8}" },
            Rect { x: lx, y, w: 16.0, h: 20.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    // multi-expand: 既に開いていれば閉じる、 でなければ追加 (複数同時可)。
                    if !app.ui_ephemeral.expanded_mod_sources.insert(sid) {
                        app.ui_ephemeral.expanded_mod_sources.remove(&sid);
                    }
                })
            },
        );
        let name_x = lx + 18.0;
        let name_rect = Rect { x: name_x, y, w: (arm_x - 4.0 - name_x).max(40.0), h: 20.0 };
        if let K::EnvelopeFollower { tap, .. } = &src.kind {
            let sel = mod_track_choices
                .iter()
                .position(|(tid, _)| *tid == tap.source_track)
                .unwrap_or(0);
            if let Some(picked) =
                ui.dropdown(("inspector_mod_src_track", i), name_rect, &mod_track_labels, sel)
                && let Some(&(tid, _)) = mod_track_choices.get(picked)
            {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetModSourceTrack { id: sid, source_track: tid });
                }));
            }
        } else {
            ui.label_at(
                ("inspector_mod_src_kind", i),
                src.kind.short_label(),
                name_rect.x,
                y + 4.0,
                12.0,
                p.text,
            );
        }
        ui.button_at(("inspector_mod_src_rm", i), "\u{00d7}", rm_rect, move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::RemoveModSource { id: sid });
            })
        });
        y += 22.0;

        if expanded {
        // --- 展開: グラフィカルエディタ + 種別別コントロール ---
        match &src.kind {
            K::EnvelopeFollower { tap, follower } => {
                // follower は cycle を持たない → canvas は出さず tap/A/R を出す。
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
                if let Some(picked) = ui.dropdown(
                    ("inspector_mod_src_tap", i),
                    Rect { x: lx, y, w: tap_w, h: 20.0 },
                    &tap_labels,
                    tap_sel,
                ) && let Some(&tp) = TAP_POINTS.get(picked)
                {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetModSourceTapPoint { id: sid, tap_point: tp });
                    }));
                }
                let rest_x = lx + tap_w + 6.0;
                let half = (area.x + area.w - pad - rest_x - 6.0) / 2.0;
                let ar_style = ScrubableNumberStyle {
                    range: Some((0.0, 60_000.0)),
                    sensitivity: 0.04,
                    ..scrub_style(&app.theme)
                };
                ui.label_at(("inspector_mod_a_lbl", i), "A", rest_x, y + 4.0, 10.0, p.text);
                let a_resp = ui.scrubable_number_at(
                    ("inspector_mod_attack", i),
                    Rect { x: rest_x + 12.0, y, w: (half - 12.0).max(20.0), h: 20.0 },
                    f64::from(follower.attack_ms),
                    1.0,
                    ScrubableNumberFormat::Decimal(1),
                    &ar_style,
                    move |v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetModSourceAttack { id: sid, ms: v as f32 });
                        })
                    },
                    None,
                    None,
                );
                let r_x = rest_x + half + 6.0;
                ui.label_at(("inspector_mod_r_lbl", i), "R", r_x, y + 4.0, 10.0, p.text);
                let r_resp = ui.scrubable_number_at(
                    ("inspector_mod_release", i),
                    Rect { x: r_x + 12.0, y, w: (half - 12.0).max(20.0), h: 20.0 },
                    f64::from(follower.release_ms),
                    100.0,
                    ScrubableNumberFormat::Decimal(1),
                    &ar_style,
                    move |v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetModSourceRelease { id: sid, ms: v as f32 });
                        })
                    },
                    None,
                    None,
                );
                any_mod_drag |=
                    a_resp.dragging || a_resp.editing_text || r_resp.dragging || r_resp.editing_text;
                y += 22.0;
            }
            K::Lfo(c) => {
                let canvas = Rect { x: lx, y, w: row_w, h: MOD_CANVAS_H };
                ui.signal_preview(
                    ("inspector_lfo_prev", sid),
                    canvas,
                    &generator_cycle_samples(&src.kind, 160, beat, secs),
                    generator_phase(&src.kind, beat, secs),
                    mod_style,
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
                let cur_width = if let common::model::LfoShape::Pulse { width } = c.shape {
                    width
                } else {
                    0.5
                };
                if let Some(p) =
                    ui.dropdown(("inspector_lfo_shape", i), Rect { x: lx, y, w: 56.0, h: 20.0 }, &shapes, ssel)
                {
                    let shape = match p {
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
                any_mod_drag |= mod_rate_full(ui, &app.theme, i_u, lx + 62.0, y, &c.rate, sid);
                y += 22.0;
                // row B: φ phase + (Pulse width) + retrig
                ui.label_at(("inspector_lfo_ph_lbl", i), "\u{03c6}", lx, y + 4.0, 10.0, p.text);
                let ph_resp = ui.scrubable_number_at(
                    ("inspector_lfo_phase", i),
                    Rect { x: lx + 12.0, y, w: 50.0, h: 20.0 },
                    f64::from(c.phase),
                    0.0,
                    ScrubableNumberFormat::Decimal(2),
                    &unit_style,
                    move |v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoPhase(v as f32) });
                        })
                    },
                    None,
                    None,
                );
                any_mod_drag |= ph_resp.dragging || ph_resp.editing_text;
                let mut next_x = lx + 68.0;
                if let common::model::LfoShape::Pulse { width } = c.shape {
                    ui.label_at(("inspector_lfo_w_lbl", i), "w", next_x, y + 4.0, 10.0, p.text);
                    let w_resp = ui.scrubable_number_at(
                        ("inspector_lfo_width", i),
                        Rect { x: next_x + 12.0, y, w: 46.0, h: 20.0 },
                        f64::from(width),
                        0.5,
                        ScrubableNumberFormat::Decimal(2),
                        &unit_style,
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
                    any_mod_drag |= w_resp.dragging || w_resp.editing_text;
                    next_x += 64.0;
                }
                mod_retrigger_toggle(ui, i_u, Rect { x: next_x, y, w: 56.0, h: 20.0 }, &c.retrigger, sid, beat);
                y += 22.0;
            }
            K::Random(c) => {
                let canvas = Rect { x: lx, y, w: row_w, h: MOD_CANVAS_H };
                ui.signal_preview(
                    ("inspector_rand_prev", sid),
                    canvas,
                    &generator_cycle_samples(&src.kind, 160, beat, secs),
                    generator_phase(&src.kind, beat, secs),
                    mod_style,
                );
                y += MOD_CANVAS_H + 4.0;
                // row A: Stepped↔Smooth morph (0=階段/S&H, 1=滑らか) + rate(+Hz)
                ui.label_at(("inspector_rand_sm_lbl", i), "Smooth", lx, y + 4.0, 10.0, p.text);
                let sm_resp = ui.scrubable_number_at(
                    ("inspector_rand_smooth", i),
                    Rect { x: lx + 48.0, y, w: 44.0, h: 20.0 },
                    f64::from(c.smooth),
                    1.0,
                    ScrubableNumberFormat::Decimal(2),
                    &unit_style,
                    move |v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::RandomSmooth(v as f32) });
                        })
                    },
                    None,
                    None,
                );
                any_mod_drag |= sm_resp.dragging || sm_resp.editing_text;
                any_mod_drag |= mod_rate_full(ui, &app.theme, i_u, lx + 96.0, y, &c.rate, sid);
                y += 22.0;
                // row B: 「別の乱数パターンを引き直す」 ボタン (raw seed は内部値なので隠す) + retrig
                ui.button_at(
                    ("inspector_rand_reroll", i),
                    "\u{21bb} Randomize",
                    Rect { x: lx, y, w: 100.0, h: 20.0 },
                    move || {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::RerollSeed });
                        })
                    },
                );
                mod_retrigger_toggle(ui, i_u, Rect { x: lx + 108.0, y, w: 56.0, h: 20.0 }, &c.retrigger, sid, beat);
                y += 22.0;
            }
            K::Mseg(c) => {
                let canvas = Rect { x: lx, y, w: row_w, h: MOD_CANVAS_H };
                let nodes = mseg_nodes(c);
                let samples = mseg_samples(c, 160);
                let phase = generator_phase(&src.kind, beat, secs);
                let resp = ui.mseg_editor(
                    ("inspector_mseg_canvas", sid),
                    canvas,
                    &nodes,
                    &samples,
                    phase,
                    mod_style,
                    move |act| {
                        Edit::mutate(move |app: &mut AppData| {
                            let edit = match act {
                                MsegAction::Move { index, time, value } => {
                                    E::MsegMovePoint { index, time, value }
                                }
                                MsegAction::Add { time, value } => E::MsegAddPoint { time, value },
                                MsegAction::SetCurve { segment, curve } => {
                                    E::MsegSetCurve { segment, curve }
                                }
                                MsegAction::Delete { index } => E::MsegRemovePoint(index),
                            };
                            app.handle_event(AppEvent::EditModSource { id: sid, edit });
                        })
                    },
                );
                any_mod_drag |= resp.dragging;
                y += MOD_CANVAS_H + 4.0;
                // controls: play_mode + rate(+Hz) + retrig
                let pmodes = ["1shot", "Loop", "Ping"];
                let psel = match c.play_mode {
                    common::model::MsegPlayMode::OneShot => 0,
                    common::model::MsegPlayMode::Loop => 1,
                    common::model::MsegPlayMode::PingPong => 2,
                };
                if let Some(p) =
                    ui.dropdown(("inspector_mseg_play", i), Rect { x: lx, y, w: 56.0, h: 20.0 }, &pmodes, psel)
                {
                    let pm = match p {
                        0 => common::model::MsegPlayMode::OneShot,
                        1 => common::model::MsegPlayMode::Loop,
                        _ => common::model::MsegPlayMode::PingPong,
                    };
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegPlayMode(pm) });
                    }));
                }
                any_mod_drag |= mod_rate_full(ui, &app.theme, i_u, lx + 62.0, y, &c.rate, sid);
                mod_retrigger_toggle(ui, i_u, Rect { x: lx + 176.0, y, w: 56.0, h: 20.0 }, &c.retrigger, sid, beat);
                y += 22.0;
            }
            K::Steps(c) => {
                let canvas = Rect { x: lx, y, w: row_w, h: MOD_CANVAS_H };
                let current = Some(common::modulators::steps_active_index(c, beat, secs));
                let resp = ui.step_grid(
                    ("inspector_steps_grid", sid),
                    canvas,
                    &c.values,
                    current,
                    mod_style,
                    move |idx, v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource {
                                id: sid,
                                edit: E::StepValue { index: idx, value: v },
                            });
                        })
                    },
                );
                any_mod_drag |= resp.dragging;
                y += MOD_CANVAS_H + 4.0;
                // row A: [-][+] count + dir + rate(+Hz)
                let n = c.values.len();
                ui.button_at(("inspector_steps_dec", i), "\u{2212}", Rect { x: lx, y, w: 22.0, h: 20.0 }, move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource {
                            id: sid,
                            edit: E::StepsCount(n.saturating_sub(1)),
                        });
                    })
                });
                ui.button_at(("inspector_steps_inc", i), "+", Rect { x: lx + 26.0, y, w: 22.0, h: 20.0 }, move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsCount(n + 1) });
                    })
                });
                ui.label_at(("inspector_steps_n", i), &format!("{n}"), lx + 52.0, y + 4.0, 10.0, p.text);
                let dirs = ["Fwd", "Bwd", "Ping"];
                let dsel = match c.direction {
                    common::model::StepsDirection::Forward => 0,
                    common::model::StepsDirection::Backward => 1,
                    common::model::StepsDirection::PingPong => 2,
                };
                if let Some(p) =
                    ui.dropdown(("inspector_steps_dir", i), Rect { x: lx + 74.0, y, w: 50.0, h: 20.0 }, &dirs, dsel)
                {
                    let dir = match p {
                        0 => common::model::StepsDirection::Forward,
                        1 => common::model::StepsDirection::Backward,
                        _ => common::model::StepsDirection::PingPong,
                    };
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsDirection(dir) });
                    }));
                }
                any_mod_drag |= mod_rate_full(ui, &app.theme, i_u, lx + 130.0, y, &c.rate, sid);
                y += 22.0;
                // row B: slew + retrig
                ui.label_at(("inspector_steps_sl_lbl", i), "slew", lx, y + 4.0, 10.0, p.text);
                let sl_resp = ui.scrubable_number_at(
                    ("inspector_steps_slew", i),
                    Rect { x: lx + 32.0, y, w: 44.0, h: 20.0 },
                    f64::from(c.slew),
                    0.0,
                    ScrubableNumberFormat::Decimal(2),
                    &unit_style,
                    move |v| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsSlew(v as f32) });
                        })
                    },
                    None,
                    None,
                );
                any_mod_drag |= sl_resp.dragging || sl_resp.editing_text;
                mod_retrigger_toggle(ui, i_u, Rect { x: lx + 82.0, y, w: 56.0, h: 20.0 }, &c.retrigger, sid, beat);
                y += 22.0;
            }
        }
        } // end `if expanded`

        // --- このソースが駆動している routing をソース直下に表示 (畳んでも見える) ---
        //
        // r.md #78: 対象が**どのトラックにあっても**ここに並ぶ (`mod_source_routings`)。
        // ◉ は他トラックのツマミにも効くので、 ソース所有トラック以外の routing が
        // 実際に作れる。 旧実装はカーソルトラックの routing しか描かなかったため、
        // それらはどこにも出ず削除できない孤児になっていた。
        for row in app.mod_source_routings(sid) {
            let row_y = y;
            ui.label_at_clipped(
                ("inspector_mod_rt_lbl", route_i),
                &format!("\u{2192} {}", row.label),
                Rect {
                    x: lx + 18.0,
                    y: row_y + 4.0,
                    // 右側の depth / 極性 / × と重ねない (幅は下の x 計算と対)。
                    w: (row_w - 18.0 - 4.0 - 46.0 - 4.0 - 22.0 - 4.0 - 20.0).max(1.0),
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
            ui.scrubable_number_at(
                ("inspector_mod_rt_depth", route_i),
                Rect { x: depth_x, y: row_y, w: 46.0, h: 20.0 },
                f64::from(row.depth),
                1.0,
                ScrubableNumberFormat::Decimal(2),
                &scrub_style(&app.theme),
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
                None,
            );
            let bip = row.bipolar;
            let tgt_pol = row.target.clone();
            ui.button_at(
                ("inspector_mod_rt_pol", route_i),
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
                ("inspector_mod_rt_rm", route_i),
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
            y += 22.0;
            route_i += 1;
        }
        y += 4.0;
    }

    // drag-end edge で sync (scrub 中は dirty のみ)。
    if any_mod_drag != app.ui_ephemeral.mod_follower_scrub_active {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetModFollowerScrubbing(any_mod_drag));
        }));
    }

    y
}
