//! press フレームの振り分け。 ゾーンごとの優先順位 (splitter → clip zone → arranger →
//! ruler → header → automation) を **制御フローの値** (`PressClaim`) として持ち回る。

use super::*;

/// press フレームの座標と modifier のスナップショット (旧 `run.rs` のローカル群)。
#[derive(Clone, Copy)]
pub(super) struct PressHit {
    pub px: f32,
    pub py: f32,
    pub in_lanes: bool,
    pub in_ruler: bool,
    pub shift: bool,
    pub ctrl: bool,
}

/// 「この押下を誰が消費したか」。 旧実装が `ui.widget_state` を読み直して判定していた
/// 優先順位を、 **制御フローの値**として持ち回る。
///
/// 旧実装との対応 (挙動を 1:1 に保つための表):
/// - `splitter`      = 旧 `splitter_press`。 以降のゲート
///   (audio grip / clip / arranger / ruler / point / 区間 bend / automation clip / lasso)。
/// - `point`         = 旧 `already_taken_by_point`。
///   **r.md #73 で「立てる条件」だけを広げた** — 旧実装は point drag session が
///   起動したときにしか立たず、 Alt+クリック (削除) では立たなかったので、
///   同フレームに後続の press (automation clip / 区間 bend) が二重起動していた。
///   seed (`from_live` の `automation_point_drag.is_some()`) は据え置きで、
///   `press_lanes::point` が今フレームの当たりでも立てる = **単調に強くなる方向**
///   なので、 r.md #77 の等価性の根拠は壊れない。
/// - `session`       = 旧 `no_session` の否定。
///
/// **`session` の列挙は 11 種で、 `section_drag` / `header_resize_drag` /
/// `automation_lasso_drag` を意図的に含まない** (旧実装と同一)。 含まなくて正しい理由:
/// - `section_drag`: arranger 帯は `lanes` / `header_pane` と y 排他なので、 この bit を読む
///   2 つの fallback のゲート (`in_arr` / `in_lanes`) が先に false になる。
/// - `header_resize_drag`: 起動時に必ず `splitter` が立つので `!splitter` ゲートで弾かれる。
/// - `automation_lasso_drag`: この bit を読む 2 か所より後で起動する。
///
/// `session` / `point` は press 分岐に入る**前**の live session (前フレームからの残存を含む) で
/// seed し、 以降は 11 種のいずれかを起動した分岐が `= true` を立てる。 旧実装が各ゲートで
/// `widget_state` を読み直していたのと**厳密に等価**: press ブロック内で session を `None` に
/// 戻す箇所は 1 つも無い ので、 「seed + 単調に立てる」 と「毎回読み直す」 の結果は必ず一致する。
///
/// `splitter` は live seed **しない** (旧 `splitter_press` も `false` から始まるローカル)。
#[derive(Clone, Copy)]
pub(super) struct PressClaim {
    pub splitter: bool,
    pub point: bool,
    pub session: bool,
}

impl PressClaim {
    /// press 分岐に入る前の live session から seed する。
    /// **旧 `no_session` の 11 列挙はここ 1 か所だけ。**
    /// **11 種を 14 種に「直さない」** (退化ケースで挙動が変わる)。
    fn from_live(s: &ArrangementState) -> Self {
        Self {
            splitter: false,
            point: s.automation_point_drag.is_some(),
            session: s.track_volume_drag.is_some()
                || s.track_reorder.is_some()
                || s.audio_drag.is_some()
                || s.clip_drag.is_some()
                || s.automation_point_drag.is_some()
                || s.automation_clip_drag.is_some()
                || s.automation_lane_resize_drag.is_some()
                || s.track_row_resize_drag.is_some()
                || s.playhead_drag.is_some()
                || s.loop_drag.is_some()
                // r.md #73: 旧 `automation_curve_param_drag` (中央ハンドル) の差し替え。
                || s.automation_segment_bend.is_some(),
        }
    }
}

/// press ブロック内では `widget_state` の借用が走って `push_edit` を呼べないため、
/// 発行すべき `Edit` を貯めるスロット。
///
/// **発火は同一フレーム内**。 次フレームに倒すとドラッグが 1 フレーム遅れて追従する
/// 体感劣化になる。
#[derive(Default)]
pub(super) struct PressActions {
    /// M14 Phase 63j (#024): ruler の plain click は press frame で `SetPlayheadBeat` を 1 度
    /// 発火する (continuation は `drag::emit_playhead` 経由)。
    pub seek_beat: Option<f64>,
    /// M14 Phase 63n-1 (#028): track 行右端の lane disclosure click 検出。
    pub lane_toggle: Option<u32>,
    /// M14 Phase 63n-2 (#028): lane header の icon click action。
    /// None で何も起きず、 Some で 1 度発行 (重複 click は最初の lane を優先 = early break)。
    pub lane_button: Option<Edit<AppData>>,
    /// M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints。
    pub delete_point: Option<Edit<AppData>>,
}

impl PressActions {
    /// 旧 `no_press_action` の否定。
    pub(super) fn any(&self) -> bool {
        self.seek_beat.is_some()
            || self.lane_toggle.is_some()
            || self.lane_button.is_some()
            || self.delete_point.is_some()
    }

    /// 旧実装と **同じ順** (seek → lane_toggle → lane_button → delete_point) で発行。
    /// `push_edit` の発行順が `Edit` の適用順を決めるので、 この順序は挙動そのもの。
    fn emit(self, ui: &mut Ui<'_, AppData>) {
        // M14 Phase 63j (#024): press block で貯めた playhead seek を 1 度発行 (state borrow 終了後)。
        if let Some(beat) = self.seek_beat {
            ui.push_edit({
                let v_beat = beat;
                Edit::mutate(move |app: &mut AppData| {
                    app.seek_playhead_to(v_beat);
                })
            });
        }
        // M14 Phase 63n-1 (#028): track 行右端の lane disclosure click を 1 度発行 (同上)。
        if let Some(track) = self.lane_toggle {
            ui.push_edit({
                let v_track = track;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackAutomationCollapsed {
                        track_id: v_track,
                    });
                })
            });
        }
        // M14 Phase 63n-2 (#028): lane header button (★/👁/✕) の click を 1 度発行。
        if let Some(req) = self.lane_button {
            ui.push_edit(req);
        }
        // M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints を 1 度発行 (即時)。
        if let Some(req) = self.delete_point {
            ui.push_edit(req);
        }
    }
}

/// press フレームの振り分け全体。 呼び出し順は旧実装と 1:1
/// (splitter → clip zone → arranger → ruler → header → automation → 遅延発火)。
///
/// **`claim` は全分岐に `&mut` で渡す。** 旧実装は `no_session` を読む地点で
/// `widget_state` を読み直しており、 **同フレームでそれより前の分岐が起動した session も
/// 見えていた**。 これを値で持ち回るには、 11 列挙のどれかを起動した分岐がその場で
/// `claim.session = true` を立てる必要がある。 共有参照で渡すと立てられず、 旧挙動を再現できない。
pub(super) fn dispatch(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    if !f.pointer.primary_just_pressed {
        return;
    }
    let Some((px, py)) = f.pointer.pos else { return };
    let hit = PressHit {
        px,
        py,
        in_lanes: f.lanes.contains(px, py),
        in_ruler: f.ruler.contains(px, py),
        shift: f.pointer.modifiers.shift,
        ctrl: f.pointer.modifiers.ctrl,
    };
    snapshot_modifiers(ui, f);
    let mut claim = {
        let s: &ArrangementState = ui.widget_state(f.wid);
        PressClaim::from_live(s)
    };
    let mut actions = PressActions::default();
    splitter(ui, f, &hit, &mut claim);
    press_lanes::clip_zone(ui, f, &hit, &mut claim);
    arranger(ui, f, &hit, &mut claim);
    ruler(ui, f, &hit, &mut claim, &mut actions);
    press_header::dispatch(ui, f, &hit, &mut claim, &mut actions);
    press_lanes::automation(ui, f, &hit, &mut claim, &mut actions);
    actions.emit(ui);
}

/// release frame で確定する click 系 (track header のトラック選択) 用に、
/// press 時の modifier を `ArrangementState.press_modifiers` に記録する。
/// 読むのは `header::commit_clicks`。 release フレームの `pointer.modifiers` 生読みは
/// ModifiersChanged 先行 race で Ctrl/Shift が落ちる。
fn snapshot_modifiers(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    state.press_modifiers = f.pointer.modifiers;
}

/// lane 下端 → track 行下端 → header/lanes 境界 の 3 段 splitter 判定。
/// **`claim.splitter` = 旧 `splitter_press`** で、 以降 9 か所のゲートになる。
///
/// M14 Phase 63n-5 (#030): lane 下端 splitter hit (= body x range × lane bottom edge ±handle)
/// を **最優先** で判定。 hit したら resize drag session を起動して以降の press logic を skip
/// (= audio grip / clip drag / point hit / track header と排他)。 modifier 無視 (Shift+drag /
/// Ctrl+drag でも resize は同じ意味で、 既存 modifier semantics と衝突する余地が無い)。
///
/// `automation_lane_resize_drag` / `track_row_resize_drag` は 11 列挙内なので起動したら
/// `claim.session` も立てる。 `header_resize_drag` は 11 列挙外なので立てない。
fn splitter(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    let splitter_lane = automation_lane_resize_splitter_at(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        px,
        py,
    );
    claim.splitter = if let Some(lane_key) = splitter_lane {
        let anchor_h = f
            .visible_tracks
            .iter()
            .find(|t| t.id == lane_key.track)
            .and_then(|t| t.automation_lanes.iter().find(|l| l.id == lane_key.lane))
            .map_or(0_u16, |l| l.height_px);
        if anchor_h > 0 {
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.automation_lane_resize_drag = Some(AutomationLaneResizeDragSession {
                lane: lane_key,
                anchor_height_px: anchor_h,
                anchor_mouse_y: py,
                last_mouse_y: py,
                last_emitted_height: anchor_h,
            });
            claim.session = true;
            true
        } else {
            false
        }
    } else if let Some(row_idx) = track_row_resize_splitter_at(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.lanes.x,
        f.lanes.w,
        f.style,
        px,
        py,
    ) {
        // M14 Phase 63n-6 (#031): track row 下端 splitter hit (lane splitter 不在の場合のみ)
        // → **per-track** row resize session 起動 (= splitter で hit した track のみが伸び縮み)。
        let t = &f.visible_tracks[row_idx];
        let anchor_row_h = effective_track_row_h(t, f.view.track_row_h);
        if anchor_row_h > 0.0 {
            let track = t.id;
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                track,
                anchor_row_h,
                anchor_mouse_y: py,
                last_mouse_y: py,
                last_emitted_height: anchor_row_h,
            });
            claim.session = true;
            true
        } else {
            false
        }
    } else if header_resize_splitter_at(f.rect, f.header_w, f.style, px, py) {
        // M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter hit (lane/row splitter 不在の
        // 場合のみ = lanes 左端 4px の角は lane/row resize を優先)。 → header 幅 resize session 起動。
        // 境界は arrangement 全高に張るので clip drag (in_lanes) / ruler seek (in_ruler) より優先
        // させる (両者は後段で `!claim.splitter` gate 済)。
        let header_w = f.header_w;
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.header_resize_drag = Some(HeaderResizeDragSession {
            anchor_header_w: header_w,
            anchor_mouse_x: px,
            last_mouse_x: px,
            last_emitted_w: header_w,
        });
        // `header_resize_drag` は 11 列挙外なので `claim.session` は立てない。
        true
    } else {
        false
    };
}

/// M14 Phase 127 (daw_01 #105): Arranger レーン press 振り分け。 arranger_rect は ruler /
/// lanes / header_pane と y 領域が排他なので独立 block で扱う。 header 幅 splitter
/// (全高に張る) との競合のみ `!claim.splitter` で回避 (clip / ruler と同 gate)。
///
/// **`section_drag` は 11 列挙外なので `claim.session` は立てない。**
fn arranger(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    if !(!claim.splitter && f.arranger_lane_h > 0.0 && f.arranger_rect.contains(px, py)) {
        return;
    }
    let press_alt = f.pointer.modifiers.alt;
    let press_ctrl = f.pointer.modifiers.ctrl;
    let press_shift = f.pointer.modifiers.shift;
    if let Some((sid, kind)) =
        section_hit(f.sections, f.arranger_rect, f.view, px, py, f.style.resize_handle_px)
        && let Some(s) = f.sections.iter().find(|s| s.id == sid)
    {
        // 既存 section 上 → Move / Resize session (Ctrl は release で Duplicate に分岐)。
        let gesture = match kind {
            ClipDragKind::Move => SectionGesture::Move,
            ClipDragKind::ResizeLeft => SectionGesture::ResizeLeft,
            ClipDragKind::ResizeRight => SectionGesture::ResizeRight,
        };
        let anchor_start = s.start_beat;
        let anchor_len = s.len_beats;
        let anchor_press_beat = px_to_beat(px, f.arranger_rect.x, f.arranger_rect.w, f.view);
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.section_drag = Some(SectionDragSession {
            kind: gesture,
            section_id: sid,
            anchor_start,
            anchor_len,
            anchor_press_beat,
            anchor_mouse: (px, py),
            last_mouse: (px, py),
            last_alt: press_alt,
            last_ctrl: press_ctrl,
            last_shift: press_shift,
        });
    } else {
        // 空きレーン → 範囲 drag による新規作成 session (press 端を snap で grid に着地)。
        // 単純 click (drag 距離 < 4px) は release で no-op、 新規作成は dblclick が担当する。
        let raw = px_to_beat(px, f.arranger_rect.x, f.arranger_rect.w, f.view);
        let anchor = f.view.snap.snap_beat(raw, press_alt, f.zoom_x_px_per_beat).max(0.0);
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.section_drag = Some(SectionDragSession {
            kind: SectionGesture::Create,
            section_id: 0,
            anchor_start: anchor,
            anchor_len: 0.0,
            anchor_press_beat: anchor,
            anchor_mouse: (px, py),
            last_mouse: (px, py),
            last_alt: press_alt,
            last_ctrl: press_ctrl,
            last_shift: press_shift,
        });
    }
}

/// ruler の press。 Shift で loop 編集 (Start/End/Middle/NewRange)、 plain で playhead seek
/// session + `actions.seek_beat`。 `loop_drag` / `playhead_drag` は 11 列挙内なので
/// 起動したら `claim.session = true`。
///
/// M14 Phase 117 (daw_01 #091): header splitter は arrangement 全高に張るので ruler 行の
/// 左端 (boundary ±handle/2) で `claim.splitter` が立つ。 その frame は header 幅 resize を
/// 優先し playhead seek / loop edit は起動しない。
fn ruler(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    if !hit.in_ruler || claim.splitter {
        return;
    }
    let px = hit.px;
    let press_beat = px_to_beat(px, f.ruler.x, f.ruler.w, f.view);
    let press_alt = f.pointer.modifiers.alt;
    // M14 Phase 63j (#024): plain (= Shift 非保持) ruler 操作は **playhead seek**
    // (Bitwig / Reaper / Ableton 流)、 Shift 修飾で従来の loop range edit に振り分け。
    //
    // 旧設計 (M9 Phase 45e〜): plain ruler drag = loop NewRange / Middle drag、 loop 端
    // ハンドル drag = Start/End。 これでは「再生中の任意位置で split したい」 UX で
    // ruler 上に playhead を置く手段が無く、 daw_01 #024 で「ユーザビリティが壊滅的」 と報告。
    //
    // 新設計:
    //   - **plain ruler click/drag** → `SetPlayheadBeat` 連続発火 (snap 適用 + clamp ≥ 0)
    //   - **Shift + ruler drag** → loop edit (NewRange / 既存 loop の Start/End/Middle)
    // multi-track 系 widget で Shift は加算選択用なので潰さない設計判断、 ruler は
    // 単一軸で Shift の他用途が無いので loop ops 専用 modifier として再利用する。
    if hit.shift {
        let kind = if let Some(range) = f.view.loop_range {
            match loop_band_hit_kind(range, f.view.start_beat, f.view.len_beats, f.ruler, px, 4.0)
            {
                Some(LoopBandHit::Start) => LoopDragKind::Start,
                Some(LoopBandHit::End) => LoopDragKind::End,
                Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                None => LoopDragKind::NewRange,
            }
        } else {
            LoopDragKind::NewRange
        };
        // M14 Phase 63j (#024): NewRange の anchor 端点は press 時 snap で
        // grid に着地させる (caller 側 boilerplate を強要しない設計、 release 端点も
        // `compute_loop_drag_endpoints` で snap される)。 既存 loop の Start/End/Middle
        // drag は anchor が `view.loop_range` 由来 (= 既に commit 済 = grid 上前提) なので
        // press 時 snap 不要、 raw `press_beat` を保持して Middle の delta 計算に使う。
        let anchor_press_beat_for_session = match kind {
            LoopDragKind::NewRange => {
                f.view.snap.snap_beat(press_beat, press_alt, f.zoom_x_px_per_beat)
            }
            _ => press_beat,
        };
        let anchor_loop = f
            .view
            .loop_range
            .unwrap_or((anchor_press_beat_for_session, anchor_press_beat_for_session));
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.loop_drag = Some(LoopDragSession {
            kind,
            anchor_loop,
            anchor_press_beat: anchor_press_beat_for_session,
            anchor_mouse_x: px,
            last_mouse_x: px,
            last_alt: press_alt,
        });
        claim.session = true;
    } else {
        // playhead seek session 開始 + press frame で 1 度発火 (continuation 発火は
        // `drag::emit_playhead` が担当)。 snap は `MoveClips` と同 policy: alt 押下で
        // 一時 OFF、 zoom_x_px_per_beat に対する Adaptive grid。
        let snapped = f.view.snap.snap_beat(press_beat, press_alt, f.zoom_x_px_per_beat).max(0.0);
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.playhead_drag =
            Some(PlayheadDragSession { last_mouse_x: px, last_emitted_beat: snapped });
        claim.session = true;
        actions.seek_beat = Some(snapped);
    }
}
