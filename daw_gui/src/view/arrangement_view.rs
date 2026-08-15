//! Arrangement view (track headers / ruler / lanes / clip drag) を gui_01 の
//! `Ui::arrangement` widget 1 呼び出しに集約。
//!
//! AppData は引き続き track / clip を index ベースで持つので、ここで stable id
//! (Track.id / Clip.id) と index の変換層を担う。

use crate::widgets::arrangement::{clip_key_to_ref, ClipKey};
use daw_ui_core::{ColorPickerStyle, Edit, ScrubableNumberStyle, ToggleButtonStyle, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ClipRef, ColorPickerTarget, ImportTrackTarget};
use crate::theme::Theme;
use crate::view::track_color;
use crate::view::snap::{self, SNAP_LABELS};

/// カーソル / ドロップの Y 座標が乗っている track の `song.tracks` index を、
/// widget が返す実際の header rect (`ArrangementResponse.track_header_rects`)
/// から解決する。各 rect は縦スクロール (`arrange_track_top`) / 個別行高
/// override / master 行 (Reaper 流 at top) を反映した実描画 Y なので、naive な
/// `(y - canvas_top) / row_h` と違い下方 track でも正しく当たる。
///
/// **file drop target と Split (E) の hover clip 判定の両方が使う** (= 行 → track
/// の Y 判定の single source of truth)。別々にコピーすると一方だけ直して
/// off-by-one が残る事故が起きる (実際に発生したため共有 helper に統一)。
/// master 行 (= `song.tracks` に居ない) や、どの行にも当たらない Y は `None`。
fn track_index_at_y(
    track_header_rects: &[(u32, Rect)],
    tracks: &[common::model::Track],
    y: f32,
) -> Option<usize> {
    track_header_rects
        .iter()
        .find(|(_, r)| y >= r.y && y < r.y + r.h)
        .and_then(|(track_id, _)| tracks.iter().position(|t| t.id == *track_id))
}


// track header 幅は固定定数ではなく `AppData.arrange_header_w` を SSoT
// とし (default 160.0)、 gui_01 widget の右端 splitter drag で可変。
const RULER_H: f32 = 20.0;
const TOOLBAR_H: f32 = 24.0;

/// Snap toolbar の toggle スタイル。 面 / 枠 / 文字 / ON 色 (accent) はすべて
/// パレット既定で、 toolbar の 18 px 行に収まるよう角丸と font だけ詰める。
fn snap_toggle_style(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        radius: 3.0,
        font_size: 12.0,
        ..ToggleButtonStyle::from_palette(&theme.core)
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    // 上部 24 px を Snap toolbar に。残りを arrangement widget に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };
    draw_snap_toolbar(app, ui, toolbar_rect);
    let area = body;

    // S4b: widget が AppData から view を直接構築し、interaction を Edit<AppData> 直発行する。
    // 描画用 rect (clip 波形 / MIDI プレビュー含む) も widget 内で完結し、caller は返る
    // ArrangementResponse の rect を hit-test / context-menu / inline 数値入力 overlay にだけ使う。
    let resp = crate::widgets::arrangement::arrangement(app, ui, area);

    // gui_01 #068 連動ハイライト: 今フレームの hovered clip の content_id を
    // 次フレームの active group 計算用に保持 (変化時のみ Edit を発火、 毎フレーム
    // の無駄な mutate を避ける)。
    let hover_content = resp.hovered_clip.and_then(|k| {
        let t = app.song_doc.song().tracks.iter().find(|t| t.id == k.track)?;
        t.clips.iter().find(|c| c.id == k.clip).map(|c| c.content_id)
    });
    if hover_content != app.ui_ephemeral.arrange_hover_content {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.arrange_hover_content = hover_content;
        }));
    }

    // gui_01 #090: ポインタ下の automation lane を次フレームの Ctrl+A 振り分け
    // 用に mirror (= hover_content と同 idiom、 変化時のみ Edit)。 gui_01 の
    // AutomationLaneKey を common の同型へ field コピー。
    let hover_lane = resp
        .hovered_automation_lane
        .map(|k| common::model::AutomationLaneKey { track: k.track, lane: k.lane });
    if hover_lane != app.ui_ephemeral.arrange_hovered_automation_lane {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.arrange_hovered_automation_lane = hover_lane;
        }));
    }

    // primary 選択 automation clip のレーンの「実描画 content-Y 上端」を
    // widget の実 lane rect から算出して次フレームへ mirror。 `Z` 縦ズームがレイアウトを
    // 複製せず「レーンを viewport 上端へ」 scroll する基準にする (lanes pane 上端 =
    // `area.y + RULER_H`、 content 絶対 y = 現 scroll + 画面オフセット)。 変化時のみ Edit。
    let lanes_pane_top = area.y + RULER_H;
    let cur_track_top = app.ui_prefs.arrange_track_top;
    let primary_lane_top = app.selection.selected_automation_clips.last().and_then(|k| {
        let lane_key = k.lane_key();
        resp.automation_lane_rects.iter().find_map(|(rk, rect)| {
            (rk.track == lane_key.track && rk.lane == lane_key.lane)
                .then_some((lane_key, cur_track_top + (rect.y - lanes_pane_top)))
        })
    });
    if primary_lane_top != app.ui_ephemeral.arrange_primary_lane_content_top {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.arrange_primary_lane_content_top = primary_lane_top;
        }));
    }

    // arrangement ヘッダのトラック音量スライダ drag を mixer フェーダーと
    // 同じ gesture 経路に乗せ、 「1 drag = 1 undo step」 にする。 widget が返す
    // `dragging_track_volume` (drag 中のトラック id) を前フレーム値と差分し、
    // None→Some で `ParamGestureBegin` (gesture 先頭で 1 snapshot)、 Some→None で
    // `ParamGestureEnd` を発火する (`push_param_gesture_edges` と同じ edge 検知を
    // response field 経由で行う)。 これが無いとスライダ操作が undo に積まれず、
    // mixer フェーダーと同じ「Undo がクリップ移動まで巻き戻る」 症状になる。
    let drag_vol = resp.dragging_track_volume;
    if drag_vol != app.ui_ephemeral.arrange_dragging_track_volume {
        let prev = app.ui_ephemeral.arrange_dragging_track_volume;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            use common::model::{AutomationTarget, TrackBuiltinParam};
            if let Some(t) = prev {
                app.handle_event(AppEvent::ParamGestureEnd {
                    track_id: t,
                    target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                });
            }
            if let Some(t) = drag_vol {
                app.handle_event(AppEvent::ParamGestureBegin {
                    track_id: t,
                    target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    display_name: "Volume".to_string(),
                });
            }
            app.ui_ephemeral.arrange_dragging_track_volume = drag_vol;
        }));
    }

    // gui_01 #020 (M14 Phase 63f): clip 上の右クリックメニュー (Make Unique)。
    // widget が `clip_rects: Vec<(ClipKey, Rect)>` を返してくれるので、
    // track_header_rects と同じパターンで context_menu_for を重ねる。
    // refcount==1 の clip では `MakeClipUnique` handler が「すでに独立 clip」
    // status_message を出すだけなので、 context menu はすべての clip に
    // 同形で出す (条件分岐で項目を省くと UX が分かりにくい)。
    //
    // Phase 1 PR4: audio clip の波形描画も同 loop で重ね描き。
    // Ui::arrangement が描いた clip rect の上に `Ui::waveform` を配置し、
    // ContentId が `ClipContent::Audio` の場合だけ rect 内に波形を表示する。
    // gui_01 #023 で drop position が取れるようになったらここに resolve
    // ロジックも追加する (PR4 範囲外)。
    // Phase 2 PR5: Auto-Fade / Auto-Crossfade を context_menu に追加
    // (`docs/plan_audio_clip.md` §3.5)。 選択 clip 群に対して動くので、
    // 右クリックされた clip 自体の selection を変える/変えないは handler
    // 側に任せる (= MakeClipUnique も同 pattern)。
    // rename overlay 判定用に clip_rename (index ベース ClipRef) を 1 回だけ
    // ClipKey (id ベース) に解決する (selected_clips と同 idiom)。 track rename
    // の renaming_track_id と同パターンで、 ループ内で clip_key_to_ref を毎
    // clip 呼ぶ線形探索を避ける。
    let renaming_clip_key = app.ui_ephemeral.clip_rename.and_then(|r| {
        let t = app.song_doc.song().tracks.get(r.track as usize)?;
        let c = t.clips.get(r.clip as usize)?;
        Some(ClipKey { track: t.id, clip: c.id })
    });
    for (clip_key, rect) in &resp.clip_rects {
        let key = *clip_key;
        // color_picker の anchor 用に clip rect を Copy で捕捉 (closure へ move)。
        let menu_rect = *rect;
        ui.context_menu_for(
            *rect,
            &[
                "Rename",
                "Make Unique",
                "共有を一括選択",
                "Auto-Fade",
                "Auto-Crossfade",
                "Reverse",
                "Bounce In Place",
                "Bounce (with FX)",
                "色...",
            ],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let Some(target) = clip_key_to_ref(app, key) else {
                        return;
                    };
                    match idx {
                        // 右クリック対象 clip を inline rename (track rename の
                        // clip 版、 F2 でも起動)。
                        0 => app.handle_event(AppEvent::BeginRenameClip(target)),
                        1 => app.handle_event(AppEvent::MakeClipUnique(target)),
                        // 共有を一括選択: 同 content_id の linked clip group を
                        // まとめて選択 (`docs/plan_clip_shared_name.md` §2)。
                        2 => app.handle_event(AppEvent::SelectLinkedClips(target)),
                        3 => app.handle_event(AppEvent::AutoFadeSelectedClips),
                        4 => app.handle_event(AppEvent::AutoCrossfadeSelectedClips),
                        // Reverse は右クリック対象 clip 1 つだけを toggle
                        // (Auto-Fade と違って selection 全体ではなく当該
                        // clip のみ。 Bitwig clip メニューでも同様)。
                        5 => app.handle_event(AppEvent::ToggleClipReversed(target)),
                        // Bounce In Place: Pre-FX (= plugin chain 通さず)、
                        // 当該 clip の content を 1 event の baked audio に
                        // 置換 (= 元 track 内で同 path)。 Phase 2 PR9
                        // (`docs/plan_audio_clip.md` §3.8)。
                        6 => app.handle_event(AppEvent::BounceClipInPlace(target)),
                        // Bounce (with FX): plugin chain を **通した** 結果を
                        // **新 track + 新 Clip** に書き出す (元 clip は不変)。
                        // async (= IPC freewheel render → 完了通知)。
                        // Phase 2 PR-C (`docs/plan_audio_followup.md`)。
                        7 => app.handle_event(AppEvent::BounceClipWithFx(target)),
                        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
                        // (anchor = 右クリックした clip rect)。個別 clip 色の上書き。
                        // 「トラック色に戻す」 (継承へ) は Ableton と同様に track 側
                        // context menu (= 全 clip 一括) に置く。
                        8 => app.open_color_picker(ColorPickerTarget::Clip(target), menu_rect),
                        _ => {}
                    }
                }));
            },
        );
        // S4b Phase C: audio 波形 / MIDI ノートプレビューは widget が同一レイアウトパスで
        // clip クローム (ラベル帯) と一緒に描く (共有 inset `CLIP_CONTENT_INSET_TOP`)。旧
        // 旧 app 側 clip 波形 / MIDI overlay の rect + hardcode inset 重ね描きは撤去。
        // lanes 左端 (= track header の右) より左には描かせない。 左スクロールで
        // clip の左端がヘッダの下に潜っている状態でも、 値ラベルが M/S/R ボタンの
        // 上に載らないようにするための下限。
        let lanes_left = area.x + app.ui_prefs.arrange_header_w.max(0.0);
        draw_audio_clip_value_overlay(app, ui, *clip_key, *rect, lanes_left);

        // VOICEVOX 生成中マーカー。歌唱/読み上げトラックが合成中なら
        // そのトラックの全 clip に、口パク再生成中なら口 track の auto_lipsync clip に、
        // 右上角へ回転スピナーを出す (= このクリップはまだ最新を反映していない の合図)。
        draw_clip_synth_spinner(app, ui, *clip_key, *rect);

        // clip rename mode 中はこの clip rect の上端に text_input を重ね描き。
        // track rename と同 idiom (text_input_at_focused が click で focus 取得、
        // Enter commit / Esc は root の escape handler が CancelRenameClip 発行)。
        // renaming_clip_key (ループ前に 1 回解決した id ベース key) と比較。
        if Some(key) == renaming_clip_key {
            let input_rect = Rect {
                x: rect.x + 2.0,
                y: rect.y + 2.0,
                w: (rect.w - 4.0).max(0.0),
                h: 18.0,
            };
            let edit_resp = ui.text_input_at_focused(
                ("clip_rename", key.track, key.clip),
                input_rect,
                &app.ui_ephemeral.clip_rename_text,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RenameClipChanged(new.clone()));
                    })
                },
            );
            // Enter (committed) でも外クリック (blurred = focus loss) でも確定する。
            // Esc は root の escape handler が CancelRenameClip を出す (blurred には乗らない)。
            if edit_resp.committed || edit_resp.blurred {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitRenameClip);
                }));
            }
        }
    }

    // gui_01 #028 (M14 Phase 63n-2): automation point 上の右クリック →
    // Hold / Linear / Bezier の curve type popup。 widget が
    // `automation_point_rects: Vec<(AutomationPointKey, Rect)>` を返すので
    // clip_rects と同 idiom で `context_menu_for` を毎 frame 重ねる。
    // popup 選択 → ArrangementCurveKind を `SetAutomationCurveType` に
    // 変換、 prev は popup open 時点の `clip.points[idx].curve` を retrieve。
    //
    // **重要 (visual feedback fix 2026-05-09)**: automation_point_rects は
    // automation_clip_rects と空間的に overlap している (= point は clip
    // 内に居る)。 `context_menu_for` は rect 内右クリックで popup を open
    // するため、 同位置に **両方の popup が同 frame で open される**
    // bug があった。 user が point の "Linear" (idx=1) を click すると
    // clip popup の "Delete" (idx=1) も同時発火 → clip 消失。
    //
    // 対策: point popup を **先に** ループで register し、 同 frame で
    // 右クリックが point rect 上で起きていたら clip popup ループを **skip**
    // する。 これで point popup だけが新規 open され、 clip popup の
    // open_popup が呼ばれない。
    // popup は daw_01 側で完結する (= widget の `ArrangementCurveKind` を
    // 介さず直接 `common::model::AutomationCurve` を構築する)。 gui_01 #033
    // Phase 63n-7 で widget に Exponential variant が追加されたので 4 種
    // 完全描画 + 評価。
    //
    // popup 選択時 default 値:
    //  - Bezier { tension: 0.5 } — 新式 SSoT で `tension=0.0` は Linear と
    //    完全に同じ直線、 「Bezier を選んだのに直線のまま」 という bug-like
    //    UX を避けるため 0.5 (= 中程度の S 字) を default に。
    //  - Exponential { bend: 0.5 } — 同様に、 `bend=0.0` は Linear 等価。
    //    +0.5 で前半遅・後半速 default (Exponential らしい形状をすぐ視認)。
    //
    // 数値を ±1.0 まで動かす UI は Phase 63n-9 (tension/bend handle) で
    // landing 予定。 それまでは popup で curve type を選んで default で固定。
    for (point_key, rect) in &resp.automation_point_rects {
        let key = *point_key;
        ui.context_menu_for(
            *rect,
            &["Hold", "Linear", "Bezier", "Exponential"],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let next = match idx {
                        0 => common::model::AutomationCurve::Hold,
                        1 => common::model::AutomationCurve::Linear,
                        2 => common::model::AutomationCurve::Bezier { tension: 0.5 },
                        3 => common::model::AutomationCurve::Exponential { bend: 0.5 },
                        _ => return,
                    };
                    // prev curve を retrieve (Undo 用)。 lookup できなかった
                    // ら no-op で抜ける (= 編集中に lane / clip が削除された
                    // race を防ぐ)。
                    let prev = app
                        .song_doc.song()
                        .track_by_id(key.clip.track)
                        .and_then(|t| t.lane_by_id(key.clip.lane))
                        .and_then(|l| l.clip_by_id(key.clip.clip))
                        .and_then(|c| app.song_doc.song().clip_contents.get(&c.content_id))
                        .and_then(|cc| cc.automation_points())
                        .and_then(|pts| pts.get(key.point_idx as usize))
                        .map(|p| p.curve);
                    let Some(prev) = prev else { return };
                    app.handle_event(AppEvent::SetAutomationCurveType {
                        track_id: key.clip.track,
                        lane_id: key.clip.lane,
                        clip_id: key.clip.clip,
                        point_idx: key.point_idx,
                        prev,
                        next,
                    });
                }));
            },
        );
    }

    // ===== automation 値の数値入力 overlay 群 =====
    // widget は heavy 内で text_input / scrubable を出せないので、 widget が返す rect /
    // drag info を使って daw_01 が heavy の外で描く (clip rename / 歌詞編集と同 idiom)。
    // 値の表示/解釈は `automation_value` (人間可読単位 SSoT) を 1 経路で使う。

    // (a) 編集中 point の inline 数値入力欄 (点をダブルクリックで開始)。
    if let Some(edit_key) = app.ui_ephemeral.editing_automation_point {
        let point_rect = resp
            .automation_point_rects
            .iter()
            .find(|(k, _)| {
                k.clip.track == edit_key.track_id
                    && k.clip.lane == edit_key.lane_id
                    && k.clip.clip == edit_key.clip_id
                    && k.point_idx == edit_key.point_idx
            })
            .map(|(_, r)| *r);
        let lane_target = app
            .song_doc.song()
            .automation_lane_by_key(edit_key.track_id, edit_key.lane_id)
            .map(|l| l.target.clone());
        if let (Some(rect), Some(target)) = (point_rect, lane_target) {
            let plugin_range = app.plugin_param_range(edit_key.track_id, &target);
            let desc = crate::automation_value::automation_value_display(&target, plugin_range);
            let cur = app.automation_point_value(&edit_key).unwrap_or(0.0);
            let prefill = desc.format_number(cur);
            // 点 dot (8px) より広い入力欄を、 点の少し上に center 配置。
            let field_w = 60.0_f32;
            let input_rect = Rect {
                x: rect.x + rect.w * 0.5 - field_w * 0.5,
                y: (rect.y - 22.0).max(area.y),
                w: field_w,
                h: 18.0,
            };
            let edit_resp = ui.text_input_at_focused(
                (
                    "automation_point_value",
                    edit_key.track_id,
                    edit_key.lane_id,
                    edit_key.clip_id,
                    edit_key.point_idx,
                ),
                input_rect,
                &prefill,
                |_new| Edit::mutate(|_| {}),
            );
            if edit_resp.committed || edit_resp.blurred {
                // Enter / 外クリックで確定。 数値が読めれば plain で上書き、 読めなければ
                // 値は変えずに編集終了。
                let text = edit_resp.committed_text.unwrap_or_default();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    if let Some(plain) = desc.parse_to_plain(&text) {
                        app.handle_event(AppEvent::SetAutomationPointValue {
                            key: edit_key,
                            value: plain,
                        });
                    } else {
                        app.ui_ephemeral.editing_automation_point = None;
                    }
                }));
            } else if !edit_resp.focused {
                // Esc / focus 喪失 (commit でない) → キャンセル。
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.ui_ephemeral.editing_automation_point = None;
                }));
            }
        } else {
            // 点が画面外 / 削除済 → 編集状態を破棄。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.ui_ephemeral.editing_automation_point = None;
            }));
        }
    }

    // (b) 各 lane のデフォルト値を BPM 風 scrubable_number で編集 (旧スライダー帯を置換、
    // ドラッグスクラブ + クリックで数値タイプ)。
    for (lane_key, rect) in &resp.automation_lane_default_rects {
        let lk = *lane_key;
        let model_key = common::model::AutomationLaneKey {
            track: lk.track,
            lane: lk.lane,
        };
        let Some(target) = app
            .song_doc.song()
            .automation_lane_by_key(lk.track, lk.lane)
            .map(|l| l.target.clone())
        else {
            continue;
        };
        let default_value = app
            .song_doc.song()
            .automation_lane_by_key(lk.track, lk.lane)
            .map_or(0.0, |l| l.default_value);
        let plugin_range = app.plugin_param_range(lk.track, &target);
        let desc = crate::automation_value::automation_value_display(&target, plugin_range);
        let display_value = (desc.to_display)(default_value);
        let span = (desc.range.1 - desc.range.0).abs();
        // 256 px ドラッグで表示レンジ全体 (BPM 1 px = 0.5 BPM 相当の感覚)。
        #[allow(clippy::cast_possible_truncation)]
        let sensitivity = ((span / 256.0).max(1e-4)) as f32;
        let style = ScrubableNumberStyle {
            bg_color_hovered: p.control,
            // drag 中の hint band。 `accent` の electric azure とは別物の控えめな
            // blue tint = 汎用の `scrub_drag_bg` (transport のテンポ / 拍子だけが
            // 暖色版 `scrub_drag_bg_warm` を使う)。
            bg_color_dragging: p.scrub_drag_bg,
            radius: 3.0,
            font_size: 11.0,
            sensitivity,
            range: Some(desc.range),
            ..ScrubableNumberStyle::from_palette(p)
        };
        let target_for_change = target.clone();
        let resp_s = ui.scrubable_number_at(
            ("automation_lane_default", lk.track, lk.lane),
            *rect,
            display_value,
            // dblclick reset = 現値 (= no-op、 意図しないリセットを避ける)。
            display_value,
            desc.format,
            &style,
            move |display_v| {
                let target = target_for_change.clone();
                Edit::mutate(move |app: &mut AppData| {
                    let plain = (desc.from_display)(display_v.clamp(desc.range.0, desc.range.1));
                    let norm = common::automation::plain_to_norm(&target, plain);
                    app.handle_event(AppEvent::SetLaneDefault {
                        track_id: lk.track,
                        lane_id: lk.lane,
                        prev_norm: norm,
                        next_norm: norm,
                    });
                })
            },
            None,
            None,
        );
        // undo bracket: drag / text 編集の active edge で BeginInspectorScrub (= Song snapshot)
        // / EndInspectorScrub を発火し、 一連の SetLaneDefault を undo 1 step にまとめる
        // (scrub_field と同 idiom)。
        let active = resp_s.dragging || resp_s.editing_text;
        let was_active = app.ui_ephemeral.arrange_default_scrub_active == Some(model_key);
        if active && !was_active {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.arrange_default_scrub_active = Some(model_key);
                app.handle_event(AppEvent::BeginInspectorScrub);
            }));
        } else if !active && was_active {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.arrange_default_scrub_active = None;
                app.handle_event(AppEvent::EndInspectorScrub);
            }));
        }
    }

    // (c) point drag 中の現値表示 (人間可読単位、 カーソル近傍)。
    if let Some(drag) = resp.automation_point_drag
        && let Some(target) = app
            .song_doc.song()
            .automation_lane_by_key(drag.key.clip.track, drag.key.clip.lane)
            .map(|l| l.target.clone())
    {
        let plugin_range = app.plugin_param_range(drag.key.clip.track, &target);
        let desc = crate::automation_value::automation_value_display(&target, plugin_range);
        let plain = common::automation::norm_to_plain(&target, drag.value_norm);
        let text = desc.format_with_unit(plain);
        let (cx, cy) = drag.cursor;
        let pad = 4.0_f32;
        #[allow(clippy::cast_precision_loss)]
        let w = (text.chars().count() as f32) * 7.0 + pad * 2.0;
        let label_rect = Rect {
            x: cx + 10.0,
            y: (cy - 20.0).max(area.y),
            w,
            h: 16.0,
        };
        // 読み値は不透明に近いクローム面 (`panel`) のチップに載せる = 背景は
        // パレット自身の面なので、 文字は極性固定インクでなく `text` でよい。
        ui.push_rect(RectCommand {
            rect: label_rect,
            fill: p.panel.with_alpha(0.92),
            border: p.border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        ui.label_at(
            "automation_point_drag_readout",
            &text,
            label_rect.x + pad,
            label_rect.y + 2.0,
            11.0,
            p.text,
        );
    }

    // gui_01 #028 (M14 Phase 63n-3): automation clip 上の右クリック →
    // Make Unique / Delete。 ただし上で point popup を先に register してい
    // て、 同 frame で右クリックが **point rect 上** だったら clip popup の
    // 登録を skip する (= 同位置で 2 つの popup が同時 open する bug 回避)。
    // r.md #35: context menu は右ボタン **release** (かつ移動 4px 未満) で開くようになったので、
    // 抑制判定も同じ「右クリック確定フレーム × press 位置」 で見る (旧: `secondary_just_pressed`)。
    let suppress_clip_menu = ui.pending_secondary_click_pos().is_some_and(|(px, py)| {
        resp.automation_point_rects
            .iter()
            .any(|(_, r)| r.contains(px, py))
    });
    if !suppress_clip_menu {
        for (auto_key, rect) in &resp.automation_clip_rects {
            let widget_key = *auto_key;
            ui.context_menu_for(
                *rect,
                &["Make Unique", "Delete"],
                move |idx, ui| {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        let model_key = common::model::AutomationClipKey {
                            track: widget_key.track,
                            lane: widget_key.lane,
                            clip: widget_key.clip,
                        };
                        match idx {
                            0 => app.handle_event(AppEvent::MakeAutomationClipUnique(
                                model_key,
                            )),
                            1 => app.handle_event(AppEvent::DeleteAutomationClips {
                                keys: vec![model_key],
                            }),
                            _ => {}
                        }
                    }));
                },
            );
        }
    }

    // track header の右クリックメニュー (Rename / Delete) を widget 外で重ねる。
    // widget は track_header_rects の収集までを担い、 メニュー項目の発行は view 側。
    // rename mode 中の track には text_input を rect に重ね描きする。
    // rename 対象は安定 ID で直接持つ (index 経由の解決はしない = reorder/delete で
    // 別 track にすり替わらない、 SSoT)。
    let renaming_track_id = app.ui_ephemeral.track_rename_id;
    for (track_id, rect) in &resp.track_header_rects {
        let track_id = *track_id;
        let rect = *rect;
        ui.context_menu_for(
            rect,
            &[
                "Rename",
                "複製 (独立)",
                "複製 (リンク)",
                "色...",
                "クリップ色をトラックに揃える",
                "Delete",
            ],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    // 複製 (r.md #30) / 削除 (r.md #43) の対象: 右クリック track が
                    // 選択集合に含まれるなら選択全体、 含まれないなら右クリック track
                    // 単独 (REAPER / Ableton 流)。 メニュー内で規則を割らない。
                    let target_ids = || {
                        if app.selection.selected_track_ids.contains(&track_id) {
                            app.selection.selected_track_ids.clone()
                        } else {
                            vec![track_id]
                        }
                    };
                    match idx {
                        0 => app.handle_event(AppEvent::BeginRenameTrack(track_id)),
                        // 独立複製 (Alt+D 相当): 元と切り離した別コピー。
                        1 => app.handle_event(AppEvent::DuplicateTracksUnique(target_ids())),
                        // リンク複製 (D 相当): クリップ中身を元と content_id 共有。
                        2 => app.handle_event(AppEvent::DuplicateTracksShared(target_ids())),
                        // v18 (`docs/plan_track_clip_color.md`): color_picker を開く
                        // (anchor = 右クリックした track header rect)。
                        3 => app.open_color_picker(ColorPickerTarget::Track(track_id), rect),
                        // Ableton 流: track の全 clip の色上書きを外して track 色継承に戻す。
                        4 => app.handle_event(AppEvent::ResetTrackClipColors {
                            track: track_id,
                        }),
                        5 => app.handle_event(AppEvent::DeleteTracks(target_ids())),
                        _ => {}
                    }
                }));
            },
        );

        if Some(track_id) == renaming_track_id {
            // text_input は track header rect の上端に被せる (M/S トグル等は隠れる)。
            // text_input widget が click で focus を取る。Enter で commit、Esc は
            // root の escape shortcut handler が CancelRenameTrack を発行する。
            let input_rect = Rect {
                x: rect.x + 2.0,
                y: rect.y + 2.0,
                w: rect.w - 4.0,
                h: 22.0,
            };
            let resp = ui.text_input_at_focused(
                ("track_rename", track_id),
                input_rect,
                &app.ui_ephemeral.track_rename_text,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RenameTrackChanged(new.clone()));
                    })
                },
            );
            // Enter (committed) でも外クリック (blurred = focus loss) でも確定する。
            // Esc は root の escape handler が CancelRenameTrack を出す (blurred には乗らない)。
            if resp.committed || resp.blurred {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitRenameTrack);
                }));
            }
        }
    }

    // v18 (`docs/plan_track_clip_color.md`, gui_01 #058): color_picker overlay。
    // `color_picker_target` が Some の間 1 フレームごとに `ui.color_picker` を
    // 呼んで overlay 描画する。anchor は track header / clip の rect を id で
    // 引き直す (= scroll off で rect が無ければ picker を閉じる)。`picked` を
    // live で model に反映 (open 中 widget 側は current を無視するので flicker
    // しない)、`dismissed` で target を None に戻す。
    // セクション帯の inline 改名。section_rename_id の帯 rect に text_input を重ねる
    // (track rename と同 idiom)。Enter で commit、 Esc は root の escape handler が CancelRenameSection。
    if let Some(rename_id) = app.ui_ephemeral.section_rename_id {
        for (sid, rect) in &resp.section_rects {
            if *sid == rename_id {
                let input_rect = Rect {
                    x: rect.x + 2.0,
                    y: rect.y + 1.0,
                    w: (rect.w - 4.0).max(8.0),
                    h: (rect.h - 2.0).max(12.0),
                };
                let r = ui.text_input_at_focused(
                    ("section_rename", *sid),
                    input_rect,
                    &app.ui_ephemeral.section_rename_text,
                    |new| {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::RenameSectionChanged(new.clone()));
                        })
                    },
                );
                // Enter (committed) でも外クリック (blurred = focus loss) でも確定する。
                // Esc は press 無しなので blurred には乗らず、 root の escape handler が cancel する。
                // (daw_01 #112: 以前ここで手書きしていた clicked_outside 判定は widget の
                // `blurred` に一本化。 SSoT で他の rename / inspector / 数値欄と同じ挙動。)
                if r.committed || r.blurred {
                    ui.push_edit(Edit::mutate(|app: &mut AppData| {
                        app.handle_event(AppEvent::CommitRenameSection);
                    }));
                }
            }
        }
    }

    render_color_picker_overlay(app, ui);

    // gui_01 #071: 空きレーン右クリック (`SecondaryClickEmpty`) → clip 生成 context menu。
    render_clip_create_menu_overlay(app, ui);
    render_section_menu_overlay(app, ui);

    // file drop の hint frame は widget の上に被せる。canvas (lanes) のみ受け付け。
    let canvas_area = Rect {
        x: area.x + app.ui_prefs.arrange_header_w,
        y: area.y + RULER_H,
        w: area.w - app.ui_prefs.arrange_header_w,
        h: area.h - RULER_H,
    };
    // S4b: widget が内部で構築する view 相当の pixel→beat 変換パラメータを AppData から直接再導出
    // (file drop / Split hover 用)。widget と同じ SSoT (`arrange_scroll_beat` / `arrange_zoom_x` /
    // `arrange_snap_config`) を読むので座標変換は完全一致する。
    // r.md #53: widget 側は表示原点をピクセル境界にスナップして描くので、pixel→beat の
    // 逆変換もスナップ後の原点を使う (= 見えている位置とドロップ / Split 位置が一致する)。
    let zoom = app.ui_prefs.arrange_zoom_x.max(1.0);
    let scroll_beat = crate::widgets::arrangement::pixel_snapped_scroll_beat(
        f64::from(app.ui_prefs.arrange_scroll_beat),
        (area.w - app.ui_prefs.arrange_header_w).max(1.0),
        zoom,
    );
    let arr_snap = snap::arrange_snap_config(app);
    if ui.is_file_hovering_in_rect(canvas_area) {
        ui.panel_with_border(
            "arr_file_drop_hint",
            canvas_area,
            Color::TRANSPARENT,
            p.loop_band,
            2.0,
            0.0,
        );
    }
    if let Some(drop) = ui.take_file_drop_in_rect(canvas_area) {
        // drop target 解決: drop 位置 (position.y) が乗っている track を、 widget が
        // 返す実際の header rect (`resp.track_header_rects`) で hit-test する。
        // header_rects は縦スクロール (`arrange_track_top`) / 個別行高 override /
        // master 行を反映した実描画 Y なので、 naive な `local_y / row_h` と違い
        // 下方トラックでも正しく当たる (= スクロール時や master 行ぶんのズレで
        // 「Track9 にドロップしても新規 track が作られる」バグの修正)。 lanes 側
        // drop でも各行の Y レンジは header と共通なので Y のみで判定する。 当たった
        // track_id を song.tracks の index に変換し、 master 行 (song.tracks に居ない)
        // や、 どの行にも当たらない (= track の無い下の余白) は `NewTrackBottom` =
        // 一番下に新規 track を作って貼る (r.md #31: 以前は audio=cursor/先頭 track・
        // image=一番上 insert とバラバラだったのを「ドロップ位置どおり一番下」へ統一)。
        //
        // docs/plan_video.md P2: 同じ drop 内で audio file と video file が
        // 混在する場合は extension で partition して個別 AppEvent を発火する。
        // `import_video::looks_like_video` が `mp4 / mov / mkv / webm / m4v /
        // avi` を判定 (= P2.7 wire)。 マッチしない path は従来通り Audio
        // import パイプラインに流す (= `common::audio_decode` が WAV / AIFF /
        // FLAC / MP3 / OGG / M4A をコンテンツ判定でデコード、 r.md #19)。
        let drop_y = drop.position.1;
        let target =
            match track_index_at_y(&resp.track_header_rects, &app.song_doc.song().tracks, drop_y) {
                Some(idx) => ImportTrackTarget::Track(idx as u32),
                None => ImportTrackTarget::NewTrackBottom,
            };
        // ドロップ X 位置 → beat。 import で生成する clip を「先頭 (playhead) では
        // なくドロップしたカーソル位置」 に置く。 hover-beat (下) と同じ pixel→beat
        // 変換 (canvas 左端基準) + 既存 snap 設定を適用。 header 上に落とした等で
        // canvas 左外なら 0 に clamp。 `None` は dialog / File メニュー経由 (位置情報
        // 無し → handler 側で playhead fallback)。
        let target_beat: Option<f64> = {
            let raw = scroll_beat
                + ((drop.position.0 - canvas_area.x) as f64 / zoom as f64);
            Some(arr_snap.snap_beat(raw.max(0.0), /* alt: */ false, zoom))
        };
        // docs/plan_image_overlay.md P2: 3-way partition (video →
        // image → audio). Video on Windows only (= libav/rsmpeg dependency);
        // image is OS-neutral (image crate); audio is the OS-neutral fallback
        // bucket (`common::audio_decode` handles WAV/AIFF/FLAC/MP3/OGG/M4A,
        // r.md #19).
        #[cfg(windows)]
        let (video_paths, non_video_paths): (Vec<_>, Vec<_>) = drop
            .paths
            .into_iter()
            .partition(|p| crate::import_video::looks_like_video(p));
        #[cfg(not(windows))]
        let (video_paths, non_video_paths): (Vec<std::path::PathBuf>, Vec<_>) =
            (Vec::new(), drop.paths);
        let (image_paths, audio_paths): (Vec<_>, Vec<_>) = non_video_paths
            .into_iter()
            .partition(|p| crate::import_image::is_supported_extension(p));

        if !audio_paths.is_empty() {
            let paths = audio_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportAudio {
                    paths,
                    target,
                    target_beat,
                });
            }));
        }
        if !video_paths.is_empty() {
            let paths = video_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportVideo { paths, target_beat });
            }));
        }
        if !image_paths.is_empty() {
            let paths = image_paths;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ImportImage {
                    paths,
                    target,
                    target_beat,
                });
            }));
        }
    }

    // Phase 1 PR7 (`docs/plan_audio_clip.md` §3.3): Split (E) は
    // 「マウスカーソル位置」 で分割するため、 毎フレーム mouse pos →
    // beat と (track, clip) を計算して `AppData` に push する。
    // - `arrangement_hover_beat` は snap 適用版 (E 用)
    // - `arrangement_hover_beat_raw` は snap なし版 (Alt+E 用)
    // - `arrangement_hover_clip` はマウスが乗っている clip の (track, clip)
    //   index — Split が selection 不要で動くために使う
    let raw_beat: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !canvas_area.contains(px, py) {
            return None;
        }
        let beat =
            scroll_beat + ((px - canvas_area.x) as f64 / zoom as f64);
        Some(beat.max(0.0))
    });
    let snapped_beat: Option<f64> = raw_beat
        .map(|raw| arr_snap.snap_beat(raw, /* alt: */ false, zoom));
    let hover_clip: Option<ClipRef> = raw_beat.and_then(|beat| {
        // カーソル Y が乗っている track を、 widget が返す実際の header rect
        // (`resp.track_header_rects`) で hit-test する (file drop target と同じ手法)。
        // naive な `(py - canvas_top) / row_h` は master 行 (Reaper 流 at top) /
        // 縦スクロール (`arrange_track_top`) / 個別行高 override ぶんズレ、 Split (E)
        // が 1 つ下の track の clip を対象にしてしまうため使わない。 lanes 側でも
        // 各行の Y レンジは header と共通なので Y のみで判定する。
        let (_, py) = ui.pointer().pos?;
        let track_idx = track_index_at_y(&resp.track_header_rects, &app.song_doc.song().tracks, py)?;
        let track = app.song_doc.song().tracks.get(track_idx)?;
        for (clip_idx, clip) in track.clips.iter().enumerate() {
            if beat >= clip.start_beat && beat < clip.start_beat + clip.length_beats {
                return Some(ClipRef {
                    track: track_idx as u32,
                    clip: clip_idx as u32,
                });
            }
        }
        None
    });
    // トラック paste の挿入先 (= マウス下トラックの直上)。ヘッダ列でも
    // クリップレーン上でも効くよう、X はアレンジ全幅 (`area`)、Y は実 header rect で
    // 判定する (hover_clip と同じ Y-only 手法、master 行 / ruler 上は None)。
    let hovered_track_id: Option<u32> = ui.pointer().pos.and_then(|(px, py)| {
        if !area.contains(px, py) {
            return None;
        }
        let idx = track_index_at_y(&resp.track_header_rects, &app.song_doc.song().tracks, py)?;
        app.song_doc.song().tracks.get(idx).map(|t| t.id)
    });
    if app.ui_ephemeral.arrangement_hover_beat != snapped_beat
        || app.ui_ephemeral.arrangement_hover_beat_raw != raw_beat
        || app.ui_ephemeral.arrangement_hover_clip != hover_clip
        || app.ui_ephemeral.arrange_hovered_track != hovered_track_id
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.arrangement_hover_beat = snapped_beat;
            app.ui_ephemeral.arrangement_hover_beat_raw = raw_beat;
            app.ui_ephemeral.arrangement_hover_clip = hover_clip;
            app.ui_ephemeral.arrange_hovered_track = hovered_track_id;
        }));
    }
}

/// gui_01 #071 (`docs/plan_text_clip_creation.md`): 空きレーン右クリック
/// (`SecondaryClickEmpty`) で stash した `(track_id, snap 済み beat, 右クリック pos)` を
/// 使い、 毎フレーム `ui.context_menu_at` で `pos` に clip 生成メニューを描画する
/// (REAPER の右クリック空きエリア → Insert new item idiom)。`open_at` は 1-shot flag で
/// 1 フレームだけ `Some(pos)` を渡す (毎フレーム `Some` だと outside-click で閉じても翌
/// フレーム再 open するため)。 項目選択で `AddTextClipAt` を発火して stash を `None` に戻す。
fn render_clip_create_menu_overlay(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let Some((track, beat, pos)) = app.ui_ephemeral.clip_create_menu else {
        return;
    };
    let open_at = if app.ui_ephemeral.clip_create_menu_open {
        Some(pos)
    } else {
        None
    };
    if app.ui_ephemeral.clip_create_menu_open {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.clip_create_menu_open = false;
        }));
    }
    ui.context_menu_at(
        "arrange_clip_create_menu",
        open_at,
        &["Text クリップ"],
        move |idx, ui| {
            if idx == 0 {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddTextClipAt {
                        track,
                        start_beat: beat,
                    });
                    app.ui_ephemeral.clip_create_menu = None;
                }));
            }
        },
    );
}

/// Arranger セクション帯の右クリックメニュー。`SecondaryClickSection` で stash した
/// `(section_id, pos)` を使い、 毎フレーム `ui.context_menu_at` で pos にメニューを描画する
/// (`render_clip_create_menu_overlay` と同 idiom)。 項目: このセクションをループ / 帯のみ削除 /
/// 範囲ごと削除。 選択で stash を `None` に戻す。
fn render_section_menu_overlay(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let Some((section_id, pos)) = app.ui_ephemeral.section_menu else {
        return;
    };
    let open_at = if app.ui_ephemeral.section_menu_open { Some(pos) } else { None };
    if app.ui_ephemeral.section_menu_open {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.section_menu_open = false;
        }));
    }
    ui.context_menu_at(
        "arrange_section_menu",
        open_at,
        &["改名", "色...", "このセクションをループ", "帯のみ削除", "範囲ごと削除"],
        move |idx, ui| {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                match idx {
                    0 => app.handle_event(AppEvent::BeginRenameSection(section_id)),
                    1 => {
                        let anchor = Rect { x: pos.0, y: pos.1, w: 1.0, h: 1.0 };
                        app.open_color_picker(ColorPickerTarget::Section(section_id), anchor);
                    }
                    2 => app.apply_loop_section(section_id),
                    3 => app.apply_delete_section_band(section_id),
                    4 => app.apply_delete_section_range(section_id),
                    _ => {}
                }
                app.ui_ephemeral.section_menu = None;
            }));
        },
    );
}

/// v18 (`docs/plan_track_clip_color.md`, gui_01 #058): `color_picker_target` が
/// `Some` の間、保存した anchor (開いた場所 = header / clip / inspector swatch の
/// rect) に color_picker overlay を描画する。`picked` は live で
/// `SetTrackColor`/`SetClipColor` に流す (open 中 widget 側は `current` を無視
/// するので flicker しない)、`dismissed` で target を `None` に戻す。対象 track /
/// clip が削除された (= 現在色を引けない) ときは picker を閉じる。
fn render_color_picker_overlay(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let (Some(target), Some(anchor)) = (app.ui_ephemeral.color_picker_target, app.ui_ephemeral.color_picker_anchor)
    else {
        return;
    };
    let style = ColorPickerStyle::from_palette(&app.theme.core);
    let palette = track_color::palette_colors();

    // 対象の現在色を引く。対象が消えていれば picker を閉じる。
    let current: Option<Color> = match target {
        ColorPickerTarget::Track(track_id) => app
            .song_doc.song()
            .track_by_id(track_id)
            .map(|t| track_color::to_renderer(track_color::effective_track_color(t))),
        ColorPickerTarget::Clip(clip_ref) => app
            .song_doc.song()
            .tracks
            .get(clip_ref.track as usize)
            .and_then(|t| {
                t.clips.get(clip_ref.clip as usize).map(|c| {
                    track_color::to_renderer(track_color::effective_clip_color(t, c))
                })
            }),
        ColorPickerTarget::Section(id) => app
            .song_doc.song()
            .sections
            .iter()
            .find(|s| s.id == id)
            .map(|s| Color { r: s.color[0], g: s.color[1], b: s.color[2], a: 1.0 }),
    };

    let Some(current) = current else {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.color_picker_target = None;
        }));
        return;
    };

    let r = ui.color_picker(("arr_color_picker", target_id_hash(target)), anchor, current, &palette, &style);
    if let Some(c) = r.picked {
        let rgb = track_color::from_renderer(c);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| match target {
            ColorPickerTarget::Track(track) => {
                app.handle_event(AppEvent::SetTrackColor { track, color: Some(rgb) });
            }
            ColorPickerTarget::Clip(clip_ref) => {
                app.handle_event(AppEvent::SetClipColor { target: clip_ref, color: Some(rgb) });
            }
            ColorPickerTarget::Section(id) => {
                app.handle_event(AppEvent::SetSectionColor { id, color: rgb });
            }
        }));
    }
    if r.dismissed {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.color_picker_target = None;
        }));
    }
}

/// color_picker の widget id 用に target を一意な数値へ畳む (track / clip で衝突
/// しないよう track は最上位 bit を立てる)。
fn target_id_hash(target: ColorPickerTarget) -> u64 {
    match target {
        ColorPickerTarget::Track(id) => (1u64 << 63) | id as u64,
        ColorPickerTarget::Clip(r) => ((r.track as u64) << 32) | r.clip as u64,
        ColorPickerTarget::Section(id) => (1u64 << 62) | id as u64,
    }
}
















/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle 60px] [snap unit dropdown 90px] [Fit button 50px]
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    // クロームのバー類は `header` (transport / ruler / menu bar と同じ面)。
    ui.panel("arr_toolbar_bg", rect, app.theme.core.header, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;

    let toggle_rect = Rect { x: rect.x + pad, y, w: toggle_w, h };
    let dropdown_rect = Rect {
        x: toggle_rect.x + toggle_rect.w + pad,
        y,
        w: dropdown_w,
        h,
    };
    let fit_rect = Rect {
        x: dropdown_rect.x + dropdown_rect.w + pad,
        y,
        w: fit_w,
        h,
    };

    ui.toggle_button_at(
        "arr_snap_toggle",
        "Snap",
        toggle_rect,
        app.ui_prefs.arrange_snap_enabled,
        &snap_toggle_style(&app.theme),
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeSnapEnabled(new));
            })
        },
    );

    if let Some(idx) = ui.dropdown(
        "arr_snap_unit",
        dropdown_rect,
        SNAP_LABELS,
        app.ui_prefs.arrange_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeSnapChoice(new));
        }));
    }

    ui.button_at("arr_fit", "Fit", fit_rect, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitArrangeToContent);
        })
    });
}

// ---------------------------------------------------------------------------
// VOICEVOX 生成中クリップの右上スピナー
// ---------------------------------------------------------------------------

/// VOICEVOX 生成中のクリップ右上角に回転スピナーを重ねる。
///
/// 歌唱/読み上げトラックが合成中ならそのトラックの全 clip に、口パク再生成中なら
/// 出力先 (口) track の `auto_lipsync` clip に出す。印が消える = そのクリップが最新を
/// 反映した、の合図 (grill-me 確定)。rect が小さい (zoom out) ときは名前/枠に被るので省略する。
fn draw_clip_synth_spinner(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
) {
    // idle フレーム (生成なし) は per-clip の track 探索を一切しない。
    if app.voicevox.voicevox_synth_status.is_empty() && app.voicevox.lipsync_inflight.is_empty() {
        return;
    }
    // バッジ (チップ+スピナー) が名前/枠に被らない最小サイズ。狭い/低い clip は省略。
    if clip_rect.w < 30.0 || clip_rect.h < 24.0 {
        return;
    }
    let wav = app.track_wav_synthesizing(clip_key.track);
    let lip = app.lipsync_target_generating(clip_key.track)
        && app
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == clip_key.track)
            .and_then(|t| t.clips.iter().find(|c| c.id == clip_key.clip))
            .is_some_and(|c| c.auto_lipsync);
    if !wav && !lip {
        return;
    }
    let phase = super::voicevox_overlay::spinner_phase(
        app.ui_ephemeral.frame_now.duration_since(app.ui_ephemeral.anim_epoch),
        super::voicevox_overlay::SPINNER_PERIOD,
    );
    // バッジ (暗チップ + 明スピナー) で、明/暗どのクリップ色でもコントラスト保証。
    let r = 6.0;
    let chip = r + 3.0;
    let cx = clip_rect.x + clip_rect.w - (chip + 2.0);
    let cy = clip_rect.y + chip + 2.0;
    super::voicevox_overlay::draw_spinner_badge(
        ui,
        (b"vox_clip_spin", clip_key.track, clip_key.clip),
        cx,
        cy,
        r,
        phase,
    );
}

// ---------------------------------------------------------------------------
// Phase 1 PR4: audio clip 内の波形描画 (`Ui::waveform` を clip rect に重ねる)
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Phase 2 PR8: audio clip rect 上に値ラベルを重ね描き (read-only feedback)
// ---------------------------------------------------------------------------

/// audio clip 上に「Gain dB / Fade In / Fade Out」 を small font で
/// オーバーレイ表示する。 grip drag UI (gui_01 #025) が来るまでの
/// 視覚 feedback として、 ユーザーが Inspector に行かなくても値が
/// 確認できるようにする。 値が default (0 dB / 0 fade) の clip では
/// 描かない (= 視覚ノイズを抑える)。 clip rect が 60 px より狭い場合も
/// 描かない (= ラベルが入らない)。
fn draw_audio_clip_value_overlay(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
    lanes_left: f32,
) {
    if clip_rect.w < 60.0 || clip_rect.h < 24.0 {
        return;
    }
    let Some(t_idx) = app.song_doc.song().tracks.iter().position(|t| t.id == clip_key.track) else {
        return;
    };
    let Some(c_idx) = app.song_doc.song().tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        return;
    };
    let track = &app.song_doc.song().tracks[t_idx];
    let clip = &track.clips[c_idx];
    let Some(content) = app.song_doc.song().clip_contents.get(&clip.content_id) else {
        return;
    };
    let Some(events) = content.audio_events() else {
        return;
    };
    let Some(event) = events.first() else {
        return;
    };

    // Default 値は無表示 (= clip 名で混雑するのを避ける)。
    let show_gain = event.gain_db.abs() > 0.05;
    let show_fade_in = event.fade_in_beats > 0.0;
    let show_fade_out = event.fade_out_beats > 0.0;
    if !(show_gain || show_fade_in || show_fade_out) {
        return;
    }

    // 描画位置: clip rect の右下 (= name は左上、 重ならないように)。
    // 背景は**ユーザーが着色しうるクリップ面** = 可変なので、 テーマ従属の `text` では
    // なく極性固定インクを `ink_for` で選ぶ (明るいクリップ色 / ライトテーマで
    // ラベルが消えるのを構造的に防ぐ。 clip の実効色は widget 側の描画と同じ
    // `effective_clip_color` を SSoT にする)。
    let clip_bg = track_color::to_renderer(track_color::effective_clip_color(track, clip));
    let text_color = app.theme.core.ink_for(clip_bg).with_alpha(0.85);
    let font_size = 9.0;
    let pad = 3.0;
    let mut x_right = clip_rect.x + clip_rect.w - pad;
    let y = clip_rect.y + clip_rect.h - font_size - 2.0;

    // 右から左に並べる: [Fade Out] [Fade In] [Gain]。 幅は実 advance で測り、
    // clip 左端 (+pad) を下限に clamp する。 旧実装は「1 文字 = font*0.55」 の
    // 概算幅で無制限に左へ積んでいたため、 狭い clip や左へスクロールして
    // 右端だけが見えている clip では、 ラベルがトラックヘッダの M/S/R ボタンや
    // 隣のクリップの上に描かれていた (label_at は clip_rect を持たず、 この
    // 経路には親の with_clip_rect も無い)。
    let x_left_limit = (clip_rect.x + pad).max(lanes_left + pad);
    let emit = |ui: &mut Ui<'_, AppData>, id: &'static str, s: &str, x_right: &mut f32| {
        let w = ui.measure_text(s, font_size);
        let x = *x_right - w;
        if x < x_left_limit {
            // 収まらないラベルは出さない (途中で切れた数値は誤読の元)。
            return;
        }
        *x_right = x;
        ui.label_at_clipped(
            (id, clip_key.track, clip_key.clip),
            s,
            Rect { x, y, w, h: font_size * 1.2 },
            font_size,
            text_color,
        );
        *x_right -= 6.0;
    };
    if show_fade_out {
        let s = format!("Fo {:.2}b", event.fade_out_beats);
        emit(ui, "audio_clip_lbl_fo", &s, &mut x_right);
    }
    if show_fade_in {
        let s = format!("Fi {:.2}b", event.fade_in_beats);
        emit(ui, "audio_clip_lbl_fi", &s, &mut x_right);
    }
    if show_gain {
        let s = format!("{:+.1} dB", event.gain_db);
        emit(ui, "audio_clip_lbl_gain", &s, &mut x_right);
    }
}

// ---------------------------------------------------------------------------
// MIDI clip mini piano-roll overlay
// ---------------------------------------------------------------------------


#[cfg(test)]
mod tests {
    use super::track_index_at_y;
    use crate::widgets::arrangement::view_build::clip_display_label;
    use daw_ui_renderer::Rect;

    fn rect_at(y: f32, h: f32) -> Rect {
        Rect { x: 0.0, y, w: 100.0, h }
    }

    /// クリップ表示名の導出優先順位: Text 本文 → 明示名 (rename) →
    /// ノート歌詞 (start_beat 順) → 無名 (空)。 明示名は歌詞より優先される
    /// (ユーザーが付けた名前は常に表示、 歌詞は無名クリップの既定表示)。
    #[test]
    fn clip_display_label_priority() {
        use common::model::{
            Clip, ClipContent, MidiContent, Note, Song, TextContent, TextEvent,
        };

        let mut song = Song::default();

        // content 1: MIDI clip、 歌詞付きノート (start_beat 逆順で挿入して
        // ソートを検証)。
        song.clip_contents.insert(
            1,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note { start_beat: 1.0, lyric: Some("か".into()), ..Note::default() },
                    Note { start_beat: 0.0, lyric: Some("こんに".into()), ..Note::default() },
                    Note { start_beat: 2.0, lyric: None, ..Note::default() },
                ],
                ..Default::default()
            }),
        );
        let midi_clip = Clip { id: 1, content_id: 1, ..Clip::default() };
        // start_beat 順: 0.0 "こんに" → 1.0 "か"。 名前無しなので歌詞を表示。
        assert_eq!(&*clip_display_label(&midi_clip, &song), "こんにか");

        // content 2: Text clip → 本文を表示 (名前 == 本文)。
        song.clip_contents.insert(
            2,
            ClipContent::Text(TextContent {
                events: vec![TextEvent { text: "Hello".into(), ..TextEvent::default() }],
            }),
        );
        let text_clip = Clip { id: 2, content_id: 2, ..Clip::default() };
        assert_eq!(&*clip_display_label(&text_clip, &song), "Hello");

        // Text 本文は content_name より優先される (Text の rename は本文を
        // 編集するので本文が名前。 レガシーで "Title" 等が残っていても本文が出る)。
        song.set_content_name(2, "MyName".into());
        assert_eq!(&*clip_display_label(&text_clip, &song), "Hello");
        // 明示名優先: 歌詞付き MIDI クリップ (Bell トラックの
        // 「あかねに」 等) でも、 ユーザーが付けた明示名があれば歌詞より優先して
        // それを表示する (DAW 標準挙動)。 名前を付けても歌詞のまま変わらなかった
        // 不具合の回帰テスト。
        song.set_content_name(1, "Bell".into());
        assert_eq!(&*clip_display_label(&midi_clip, &song), "Bell");
        // 明示名を消せば再び歌詞に戻る (名前は content_id 単位の共有名)。
        song.clip_content_names.remove(&1);
        assert_eq!(&*clip_display_label(&midi_clip, &song), "こんにか");

        // content 3: 歌詞も本文も無い MIDI → content_name (無ければ空)。
        song.clip_contents.insert(
            3,
            ClipContent::Midi(MidiContent {
                notes: vec![Note { start_beat: 0.0, lyric: None, ..Note::default() }],
                ..Default::default()
            }),
        );
        let empty_clip = Clip { id: 3, content_id: 3, ..Clip::default() };
        assert_eq!(&*clip_display_label(&empty_clip, &song), "");
        // 本文/歌詞が無ければ content_name が fallback として表示される。
        song.set_content_name(3, "Drums".into());
        assert_eq!(&*clip_display_label(&empty_clip, &song), "Drums");
    }

    /// off-by-one 回帰固定: arrangement 最上段に master 行 (Reaper 流) があると、
    /// 「画面上の行番号」を index にする naive 方式では song.tracks[k] が +1 ずれて
    /// 1 つ下の track を指し、最下段は範囲外になる。実際の header rect を hit-test
    /// する `track_index_at_y` はこのズレを起こさないことを固定する。
    #[test]
    fn track_index_at_y_maps_via_actual_rects_not_visual_row() {
        // 行高 20px。master 行 (id=u32::MAX) が y=0..20 の先頭、その下に
        // song.tracks の 3 本 (id 10/11/12)。
        let rects = [
            (u32::MAX, rect_at(0.0, 20.0)), // master row at top
            (10, rect_at(20.0, 20.0)),      // song.tracks[0]
            (11, rect_at(40.0, 20.0)),      // song.tracks[1]
            (12, rect_at(60.0, 20.0)),      // song.tracks[2] (最下段)
        ];
        let tracks = [
            crate::app::track_with(|t| t.id = 10),
            crate::app::track_with(|t| t.id = 11),
            crate::app::track_with(|t| t.id = 12),
        ];
        // master 行の Y → song.tracks に居ないので None (新規 track / split 対象外)。
        assert_eq!(track_index_at_y(&rects, &tracks, 10.0), None);
        // 各 track 行の Y → その track の index (+1 ズレ無し)。
        assert_eq!(track_index_at_y(&rects, &tracks, 25.0), Some(0));
        assert_eq!(track_index_at_y(&rects, &tracks, 45.0), Some(1));
        // 最下段も範囲外にならず正しく当たる (Track9=新トラック化バグの固定)。
        assert_eq!(track_index_at_y(&rects, &tracks, 65.0), Some(2));
        // 境界 (行の上端は含む / 下端は含まない half-open)。
        assert_eq!(track_index_at_y(&rects, &tracks, 20.0), Some(0));
        assert_eq!(track_index_at_y(&rects, &tracks, 40.0), Some(1));
        // どの行にも当たらない Y (全行より下) → None。
        assert_eq!(track_index_at_y(&rects, &tracks, 999.0), None);
    }
}
