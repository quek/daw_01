//! S4c Phase B-E: piano_roll widget の入力ビュー構築 (旧 `view/piano_roll_view.rs::draw`
//! の view 構築部を widget 内へ移設)。
//!
//! `AppData` (`song_doc` / `selection` / `ui_prefs` / `ui_ephemeral`) から widget が描画・
//! hit-test に使うビュー構造体 ([`BuiltPianoRoll`]) を **直接** 組み立てる。旧設計は `ui/` の
//! 汎用 widget が `common::model` を触れないため mirror 型 + `make_edit` 翻訳層を挟んでいたが、
//! widget が `daw_gui` に移ったので model 直読。
//!
//! **レイアウト SSoT**: grid / kbd / ruler / vel_area の内部レイアウトはここで **一度だけ**
//! 計算し [`PrContent`] に格納する。`run.rs` の state machine と、旧 `piano_roll_view` に
//! あった hover / hit-test mirror は同じ `grid` を参照する (= 定数からの再計算 drift を排除)。

use std::sync::Arc;

use daw_ui_renderer::Rect;

use crate::app::{AppData, ClipRef};
use crate::view::snap;
use crate::view::track_color;

use super::{Note, NoteId, NoteStyle, PianoRollScale, PianoRollScaleMode, PianoRollStyle, PianoRollView};

/// 鍵盤レーンの幅 (px)。
pub(super) const KEYBOARD_W: f32 = 56.0;
/// velocity lane の高さ (px)。
pub(super) const VEL_LANE_H: f32 = 60.0;
/// ruler 領域の高さ (px)。
pub(super) const RULER_H: f32 = 20.0;
/// 上部 Snap toolbar の高さ (px)。
pub(super) const TOOLBAR_H: f32 = 24.0;
/// 複数クリップ同時表示時に右側へ出す legend パネルの幅 (px)。
pub(super) const LEGEND_W: f32 = 152.0;

/// `piano_roll()` が受け取る、`AppData` から派生した 1 フレーム分のビュー。
///
/// `toolbar_rect` / `body_full` は表示クリップの有無に関わらず常に確定する (toolbar は常時描画、
/// placeholder も `body_full` に描く)。`content` は表示する MIDI クリップがあるときのみ `Some`。
pub(super) struct BuiltPianoRoll {
    pub toolbar_rect: Rect,
    pub body_full: Rect,
    /// `None` = 表示する MIDI クリップが無い (未選択 / 非 MIDI のみ)。
    pub content: Option<PrContent>,
}

/// 表示クリップがあるときの widget 入力一式 (レイアウト rect 込み = 内部レイアウト SSoT)。
pub(super) struct PrContent {
    pub notes: Vec<Note>,
    pub view: PianoRollView,
    pub style: PianoRollStyle,
    pub selected: Vec<NoteId>,
    pub shown: Vec<ClipRef>,
    pub target: ClipRef,
    /// 各表示クリップ (clip_slot 順) の song-absolute 開始拍 (per-note offset で clip-local へ戻す)。
    pub clip_starts: Vec<f64>,
    /// 対象 (target) クリップの song-absolute 開始拍。
    pub clip_start_beat: f64,
    /// 2 クリップ以上を同時表示中か (legend パネル + union-fit)。
    pub multi: bool,
    /// 複数表示時の legend パネル rect (単一表示は `None`)。
    pub legend_rect: Option<Rect>,
    /// toolbar を除いた widget 本体 (legend を除いた分)。
    pub body: Rect,
    /// note grid (レイアウト SSoT。hover / hit-test / drag / 描画すべてが参照)。
    pub grid: Rect,
    pub kbd: Rect,
    pub ruler: Rect,
    pub vel_area: Rect,
    /// 横ズーム (px/beat)。wheel handler が anchor 保持ズームに使う。
    pub zoom_x: f32,
    /// 縦ズーム (px/semitone)。wheel handler が anchor 保持ズームに使う。
    pub zoom_y: f32,
}

/// `AppData` から widget 入力を組み立てる (旧 `piano_roll_view::draw` L42-286)。
#[allow(clippy::too_many_lines)]
pub(super) fn build(app: &AppData, area: Rect) -> BuiltPianoRoll {
    // 上部 24 px を Snap toolbar に。残りを widget 本体 (body) に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body_full = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };

    // 選択された MIDI クリップを全部同時表示。対象 (target) = 新規ノート所属先・凡例の強調行。
    let shown = app.shown_pianoroll_clips();
    let Some(target) = app.pianoroll_target_clip() else {
        return BuiltPianoRoll { toolbar_rect, body_full, content: None };
    };

    // 複数表示 (2 つ以上) のとき右側に legend パネルを出し、widget 本体をその分狭める。
    let multi = shown.len() >= 2;
    let (body, legend_rect) = if multi {
        let lw = LEGEND_W.min(body_full.w * 0.4);
        (
            Rect { x: body_full.x, y: body_full.y, w: (body_full.w - lw).max(0.0), h: body_full.h },
            Some(Rect { x: body_full.x + body_full.w - lw, y: body_full.y, w: lw, h: body_full.h }),
        )
    } else {
        (body_full, None)
    };

    // 内部レイアウト (grid / kbd / ruler / vel_area) を **一度だけ** 算出 (SSoT)。
    // widget が実際に描画・hit-test する rect と一致する式 (旧 widget 内部 layout と同じ clamp)。
    let kbd_w = KEYBOARD_W;
    let ruler_h = RULER_H.max(0.0).min(body.h * 0.5);
    let vel_h = VEL_LANE_H.max(0.0).min((body.h - ruler_h) * 0.5);
    let main_h = (body.h - ruler_h - vel_h).max(1.0);
    let ruler = Rect { x: body.x + kbd_w, y: body.y, w: (body.w - kbd_w).max(1.0), h: ruler_h };
    let grid = Rect { x: body.x + kbd_w, y: body.y + ruler_h, w: (body.w - kbd_w).max(1.0), h: main_h };
    let kbd = Rect { x: body.x, y: body.y + ruler_h, w: kbd_w, h: main_h };
    let vel_area = Rect {
        x: body.x + kbd_w,
        y: body.y + ruler_h + main_h,
        w: (body.w - kbd_w).max(1.0),
        h: vel_h,
    };

    // dim は **トラック基準** (対象クリップのトラック以外を淡色)。
    let notes = build_widget_notes(app, &shown, Some(target.track));
    let zoom_x = app.pianoroll_zoom_x().max(4.0);
    let zoom_y = app.pianoroll_zoom_y().max(6.0);
    let loop_range = app.transport.loop_region.range();
    // piano roll を song-absolute 座標系に統一。clip.start_beat を唯一の絶対オフセット SSoT とし、
    // view 入口で加算 (ruler/grid/playhead/loop が曲の絶対小節位置)、note の model 書き戻し出口で
    // 減算する (note は共有 content のため clip-local 保持)。
    let clip_start_beat = app
        .song_doc
        .song()
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .map(|c| c.start_beat)
        .unwrap_or(0.0);
    // 編集中 clip の start_beat 位置の scale を採用 (単一 view 内で動的に scale が変わらない)。
    let scale = app.song_doc.song().scale_at(clip_start_beat).map(|sc| PianoRollScale {
        root: sc.root,
        in_scale_mask: sc.scale.pitch_class_mask(),
        mode: if app.ui_prefs.piano_roll_fold {
            PianoRollScaleMode::Fold
        } else {
            PianoRollScaleMode::Highlight
        },
        prefer_flats: common::scale::prefers_flats(sc.root, sc.scale),
    });

    // 複数表示は song-absolute scroll (`multi_clip_view`)、左下限は最早クリップ開始拍。
    // 単一表示は clip-local scroll + 対象クリップ開始位置。
    let (view_start_beat, view_min_start) = if multi {
        let earliest = shown
            .iter()
            .filter_map(|r| {
                app.song_doc
                    .song()
                    .tracks
                    .get(r.track as usize)
                    .and_then(|t| t.clips.get(r.clip as usize))
                    .map(|c| c.start_beat)
            })
            .fold(f64::INFINITY, f64::min);
        let earliest = if earliest.is_finite() { earliest } else { 0.0 };
        (f64::from(app.pianoroll_scroll_beat()), earliest)
    } else {
        (f64::from(app.pianoroll_scroll_beat()) + clip_start_beat, clip_start_beat)
    };

    let snap_cfg = snap::piano_roll_snap_config(app);
    #[allow(clippy::cast_possible_truncation)]
    let view = PianoRollView {
        start_beat: view_start_beat,
        min_start_beat: view_min_start,
        len_beats: f64::from(grid.w) / f64::from(zoom_x),
        pitch_top: f32::from(app.pianoroll_top_pitch()),
        pitch_visible: main_h / zoom_y,
        keyboard_w: KEYBOARD_W,
        velocity_lane_h: VEL_LANE_H,
        playhead_beat: app.transport.playhead_beat.map(f64::from),
        ruler_h: RULER_H,
        bpm: app.song_doc.song().bpm,
        time_sig: app.song_doc.song().time_sig,
        snap: snap_cfg,
        sub_grid_interval_beats: snap::subgrid_interval_beats(snap_cfg, zoom_x),
        loop_range,
        scale,
        snap_pitch_during_drag: app.ui_prefs.snap_on_draw,
        default_note_len_beats: app.ui_prefs.last_note_duration_beats,
    };

    // 各表示クリップ (clip_slot 順) の song-absolute 開始拍。emit が widget の song-absolute
    // 座標を「その note の所属クリップの clip-local」へ戻す (per-note offset) のに使う。
    let clip_starts: Vec<f64> = shown
        .iter()
        .map(|r| {
            app.song_doc
                .song()
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0)
        })
        .collect();

    let selected: Vec<NoteId> = app.selection.selected_notes.clone();

    BuiltPianoRoll {
        toolbar_rect,
        body_full,
        content: Some(PrContent {
            notes,
            view,
            style: PianoRollStyle::default(),
            selected,
            shown,
            target,
            clip_starts,
            clip_start_beat,
            multi,
            legend_rect,
            body,
            grid,
            kbd,
            ruler,
            vel_area,
            zoom_x,
            zoom_y,
        }),
    }
}

/// 表示対象クリップ群 (`shown`) の note を **すべて** `super::Note` に変換する。
///
/// 各 note の id は packed global id (`AppData::pack_note_id(clip_slot, index)`) で複数クリップでも
/// 衝突しない。**色はそのクリップが乗っている _トラック_ の実効色** (`effective_track_color`、凡例が
/// トラック単位なのでノートもトラック色)、非対象 _トラック_ のノートは `dimmed`、ロック中トラックの
/// ノートは `locked` (widget 側で hit-test 除外)。`target_track` = 編集対象クリップのトラック index。
/// v6 linked clip の notes は `Song.clip_contents` 経由で lookup する。
pub(super) fn build_widget_notes(app: &AppData, shown: &[ClipRef], target_track: Option<u32>) -> Vec<Note> {
    let mut out: Vec<Note> = Vec::new();
    for (clip_slot, &r) in shown.iter().enumerate() {
        let Some(track) = app.song_doc.song().tracks.get(r.track as usize) else {
            continue;
        };
        let Some(clip) = track.clips.get(r.clip as usize) else {
            continue;
        };
        let color = track_color::to_renderer(track_color::effective_track_color(track));
        // トラック基準の dim: 編集対象トラック以外のノートを淡色に。
        let dimmed = Some(r.track) != target_track;
        let locked = app.is_pianoroll_clip_locked(r);
        let clip_start = clip.start_beat;
        for (i, n) in app.song_doc.song().clip_notes(clip).iter().enumerate() {
            out.push(Note {
                id: AppData::pack_note_id(clip_slot, i),
                // song-absolute 化: clip-local note + clip 開始位置。
                start_beat: n.start_beat + clip_start,
                len_beats: n.duration_beats,
                pitch: n.pitch,
                velocity: n.velocity,
                lyric: n.lyric.as_deref().filter(|s| !s.is_empty()).map(Arc::from),
                muted: n.muted,
                style: NoteStyle { color: Some(color), dimmed, locked },
            });
        }
    }
    // widget の `note_hit` は note が start_beat 昇順ソート済を仮定して二分探索する。
    // 複数クリップは時間的に交錯するので、id を保ったまま (start_beat, id) で安定ソートする。
    out.sort_by(|a, b| {
        a.start_beat
            .partial_cmp(&b.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    out
}
