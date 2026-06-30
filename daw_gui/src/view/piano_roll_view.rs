//! Piano roll: gui_01 の `Ui::piano_roll` widget でノート描画 / 編集 / velocity lane / playhead
//! を行う。
//! - Shift+drag で加算 rect select (widget が自動)
//! - Insert キー / 空白上 dbl-click で AddNote (widget Insert + daw_01 エミュレート、1/16 snap)
//! - drag move / 端 drag resize / Delete キー は widget 内蔵
//! - wheel: Ctrl→ZoomY, Shift→ScrollX, plain→TopPitch (drag 中は無効)

use std::sync::Arc;

use daw_ui_core::{
    ButtonTextAlign, Edit, MoveDelta, Note, NoteStyle, PianoRollEditRequest, PianoRollScale,
    PianoRollScaleMode, PianoRollStyle, PianoRollView, ResizeDelta, ToggleButtonStyle, Ui, note_hit,
};
use daw_ui_renderer::{theme, Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::view::snap::{self, SNAP_LABELS};
use crate::view::track_color;

const KEYBOARD_W: f32 = 56.0;
const VEL_LANE_H: f32 = 60.0;
const RULER_H: f32 = 20.0;
const TOOLBAR_H: f32 = 24.0;
/// 複数クリップ同時表示時に右側へ出す legend パネルの幅 (px)。
const LEGEND_W: f32 = 152.0;

const COLOR_HINT: Color = theme::TEXT_DIM;

const SNAP_TOGGLE_STYLE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: theme::CONTROL,
    on_color: theme::ACCENT,
    border: theme::BORDER,
    border_width: 1.0,
    radius: 3.0,
    font_size: 12.0,
    text_color: theme::TEXT,
    on_text_color: None,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 上部 24 px を Snap toolbar に。残りを widget 本体 (body) に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body_full = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };
    draw_snap_toolbar(app, ui, toolbar_rect);

    // 選択された MIDI クリップを **全部同時表示** する。対象 (target) クリップ =
    // 新規ノートの所属先・凡例で強調される行 = 選択 anchor (`pianoroll_target_clip`、SSoT)。
    // 表示クリップが無ければ placeholder。
    let shown = app.shown_pianoroll_clips();
    let Some(target) = app.pianoroll_target_clip() else {
        // 表示する MIDI クリップが無い (未選択 or 非 MIDI のみ) ときのプレースホルダ。
        // widget が走らないので、 もし歌詞編集 mirror が残っていたら false に戻す
        // (stale-true で Esc が widget へ委ねられ続けて消える事故を防ぐ)。
        if app.piano_roll_lyric_editing {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.piano_roll_lyric_editing = false;
            }));
        }
        ui.panel("pr_bg_empty", body_full, theme::PANEL, 0.0);
        ui.label_at(
            "pr_no_clip",
            "(\u{30af}\u{30ea}\u{30c3}\u{30d7}\u{304c}\u{9078}\u{629e}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093})",
            body_full.x + 12.0,
            body_full.y + 12.0,
            12.0,
            COLOR_HINT,
        );
        return;
    };
    // 複数表示 (2 つ以上) のとき右側に legend パネル (色 / 名前 / 対象 / ロック)
    // を出し、widget 本体 (`body`) をその分狭める。単一表示は body 全幅 = 既存レイアウト不変。
    let multi = shown.len() >= 2;
    let (body, legend_rect) = if multi {
        let lw = LEGEND_W.min(body_full.w * 0.4);
        (
            Rect {
                x: body_full.x,
                y: body_full.y,
                w: (body_full.w - lw).max(0.0),
                h: body_full.h,
            },
            Some(Rect {
                x: body_full.x + body_full.w - lw,
                y: body_full.y,
                w: lw,
                h: body_full.h,
            }),
        )
    } else {
        (body_full, None)
    };

    // widget が ruler / velocity lane を内蔵 (M13 Phase 55 で ruler 追加)、
    // grid 部分は body から keyboard / ruler / vel lane を引いた領域。
    // note hit detection はこの grid_rect を使うので、widget 内部 layout と
    // 揃えておく (rect.y から ruler_h、その下に keyboard+grid、最下段に vel_lane)。
    let grid_h = body.h - VEL_LANE_H - RULER_H;
    let grid_rect = Rect {
        x: body.x + KEYBOARD_W,
        y: body.y + RULER_H,
        w: body.w - KEYBOARD_W,
        h: grid_h,
    };

    // auto-fit (X キー / Fit ボタン / SelectClip 経由) のために、現フレームの grid 領域
    // サイズを記録する。1 frame 遅延で OK (X キー押下の次フレームに反映される)。
    // 同フレーム内で `pending_pianoroll_fit` が立っていたら消費して fit を再実行
    // (Piano Roll タブ未表示で clip 選択 → タブを開いた初回フレームの fit 確定)。
    let grid_size = (grid_rect.w, grid_rect.h);
    if app.last_pianoroll_grid_size != grid_size || app.pending_pianoroll_fit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.last_pianoroll_grid_size = grid_size;
            if app.pending_pianoroll_fit {
                app.pending_pianoroll_fit = false;
                app.handle_event(AppEvent::FitPianoRollToClip);
            }
        }));
    }

    // 表示集合 (shown) が変わったら共有 viewport (`multi_clip_view`) を union-fit し
    // 直す。clip key 列を `multi_clip_view_key` と比較し、違えば 1 度だけ再 fit を要求する
    // (= multi viewport 初期化の唯一の owner = SSoT。Ctrl+Click での集合変更にも追従)。
    if multi {
        let keys: Vec<common::model::ClipKey> =
            shown.iter().filter_map(|r| app.clip_key_of(*r)).collect();
        if app.multi_clip_view_key != keys {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.multi_clip_view_key = keys.clone();
                app.handle_event(AppEvent::FitPianoRollToClip);
            }));
        }
    }

    // dim は **トラック基準** (対象クリップのトラック以外を淡色)。
    let widget_notes = build_widget_notes(app, &shown, Some(target.track));
    let zoom_x = app.pianoroll_zoom_x().max(4.0);
    let zoom_y = app.pianoroll_zoom_y().max(6.0);
    let loop_range = if app.song.loop_end_beat > app.song.loop_start_beat {
        Some((app.song.loop_start_beat, app.song.loop_end_beat))
    } else {
        None
    };
    // Phase 7 B5 (`docs/plan_scale.html` §4.4, gui_01 #042): 編集中 clip の
    // `start_beat` 位置の scale を採用する (= 単一 view 内で動的に scale
    // が変わらないため、 piano_roll が安定して編集できる)。 scale_changes が
    // 空 / 該当 event 無し / selected_clip None なら view.scale = None で旧
    // 挙動互換 (= 機能 OFF、 既存 .daw file の regression なし)。
    // piano roll を song-absolute 座標系に統一する。clip.start_beat を
    // 唯一の絶対オフセット SSoT とし、view 入口で加算 (ruler/grid/playhead/loop が
    // 曲の絶対小節位置を表示)、note の model 書き戻し出口で減算する (note は共有
    // content のため clip-local 保持)。playhead/loop は元々 song-global なので
    // 変換不要 — view を絶対化することで現状のズレ/非表示が同時に治る。
    let clip_start_beat = app
        .song
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .map(|c| c.start_beat)
        .unwrap_or(0.0);
    let scale_beat = clip_start_beat;
    let scale = app.song.scale_at(scale_beat).map(|sc| PianoRollScale {
        root: sc.root,
        in_scale_mask: sc.scale.pitch_class_mask(),
        mode: if app.piano_roll_fold {
            PianoRollScaleMode::Fold
        } else {
            PianoRollScaleMode::Highlight
        },
        prefer_flats: common::scale::prefers_flats(sc.root, sc.scale),
    });

    // widget の note キャッシュ無効化キー。
    // 旧実装は `pianoroll_notes_generation` を 12 箇所で手動 += 1 しており、 新しい
    // note 編集経路を足して bump を書き忘れると編集が画面に出ない取りこぼしが
    // 起きた。 widget へ渡す `widget_notes` の内容そのものを hash し、 note が
    // 変われば必ず無効化する correct-by-construction なキーにする。
    let notes_generation = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        widget_notes.len().hash(&mut h);
        for n in &widget_notes {
            n.id.hash(&mut h);
            n.start_beat.to_bits().hash(&mut h);
            n.len_beats.to_bits().hash(&mut h);
            n.pitch.hash(&mut h);
            n.velocity.hash(&mut h);
            n.lyric.hash(&mut h);
        }
        h.finish()
    };

    // 複数表示は song-absolute scroll (`multi_clip_view`)、左下限は最早クリップ開始拍。
    // 単一表示は従来どおり clip-local scroll + 対象クリップ開始位置 (regression なし)。
    let (view_start_beat, view_min_start) = if multi {
        let earliest = shown
            .iter()
            .filter_map(|r| {
                app.song
                    .tracks
                    .get(r.track as usize)
                    .and_then(|t| t.clips.get(r.clip as usize))
                    .map(|c| c.start_beat)
            })
            .fold(f64::INFINITY, f64::min);
        let earliest = if earliest.is_finite() { earliest } else { 0.0 };
        (f64::from(app.pianoroll_scroll_beat()), earliest)
    } else {
        (
            f64::from(app.pianoroll_scroll_beat()) + clip_start_beat,
            clip_start_beat,
        )
    };

    let view = PianoRollView {
        // song-absolute 左端 (単一 = clip-local scroll + clip 開始、複数 = 絶対 scroll)。
        start_beat: view_start_beat,
        // 左へスクロールできる下限 (単一 = clip 開始、複数 = 最早クリップ開始)。
        min_start_beat: view_min_start,
        len_beats: (grid_rect.w / zoom_x) as f64,
        pitch_top: app.pianoroll_top_pitch() as f32,
        pitch_visible: grid_h / zoom_y,
        keyboard_w: KEYBOARD_W,
        notes_generation,
        velocity_lane_h: VEL_LANE_H,
        playhead_beat: app.playhead_beat.map(|b| b as f64),
        ruler_h: RULER_H,
        bpm: app.song.bpm,
        time_sig: app.song.time_sig,
        snap: snap::piano_roll_snap_config(app),
        // 3 段目グリッド (スナップ細分線) の線間隔 (拍)。
        // `None` = subdivision なし (拍以上に粗いスナップ / OFF)。
        sub_grid_interval_beats: snap::subgrid_interval_beats(
            snap::piano_roll_snap_config(app),
            zoom_x,
        ),
        loop_range,
        scale,
        // Phase 7 B5 follow-up (gui_01 #042 Phase 70b): Highlight mode + Snap
        // on Draw で widget の drag preview pitch も最寄り in-scale に snap。
        // Fold mode / scale = None / Snap on Draw OFF では無関係。
        snap_pitch_during_drag: app.snap_on_draw,
        // 新規 note の既定長 = 直近に描いた / 選択した note 長 (last_note_duration_beats)。
        // Insert と「空白ダブルクリックで即放し」 がこの長さを使う。 ダブルクリックを放さず
        // ドラッグしたときは widget がドラッグ長を優先する (Bitwig 流)。
        default_note_len_beats: app.last_note_duration_beats,
    };
    // 鍵盤のオクターブラベル (C5 / root) のコントラストは widget 側の
    // label 色を背景 (key fill / overlay) の輝度に応じて自動反転させる必要があり
    // (Fold モードは白鍵/黒鍵を跨いで label が出るため単一色では不可)、 daw_01 から
    // 静的色を渡すだけでは解決しない。 gui_01 に WCAG auto-contrast 適用を要望済
    // (docs/plan_pianoroll_label_contrast.md)。 style override はせず default を使う。
    let style = PianoRollStyle::default();
    let resize_handle_px = style.resize_handle_px;

    // 各表示クリップ (clip_slot 順) の song-absolute 開始拍。make_edit が widget の
    // song-absolute 座標を「その note の所属クリップの clip-local」へ戻す (per-note offset) のに使う。
    let clip_starts: Vec<f64> = shown
        .iter()
        .map(|r| {
            app.song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0)
        })
        .collect();

    let make_edit = move |req: PianoRollEditRequest| -> Edit<AppData> {
        match req {
            PianoRollEditRequest::Add(notes) => {
                let Some(n) = notes.into_iter().next() else {
                    return Edit::mutate(|_| {});
                };
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddNote {
                        track: target.track,
                        clip: target.clip,
                        // widget は song-absolute → model は clip-local。
                        start_beat: n.start_beat - clip_start_beat,
                        // widget が決めた長さ (Insert/即放し=既定長、 ドラッグ=ドラッグ長)
                        // を尊重する。 旧 last_note_duration_beats 固定は widget の長さを捨てていた。
                        duration: n.len_beats,
                        pitch: n.pitch,
                    });
                })
            }
            PianoRollEditRequest::Delete(notes) => {
                let ids: Vec<u32> = notes.iter().map(|n| n.id).collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                })
            }
            PianoRollEditRequest::Move(deltas) => {
                // d.3 = next_start_beat は song-absolute → 各 note の所属クリップの clip-local へ。
                // clip_slot は packed id (d.0) 上位 8 bit。d.0 は packed のまま通し、
                // handler が decode して正しいクリップの note に書き戻す。
                let entries: Vec<(u32, f64, u8)> = deltas
                    .iter()
                    .map(|d: &MoveDelta| {
                        let off = clip_starts
                            .get(AppData::note_id_clip_slot(d.0))
                            .copied()
                            .unwrap_or(0.0);
                        (d.0, d.3 - off, d.4)
                    })
                    .collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNotePositions(entries.clone()));
                })
            }
            // gui_01 #054: Ctrl+drag コピー。Move と同形 payload だが、元 note を
            // 据え置いて複製を new 位置へ置く (CopyNotes handler が deep clone)。
            PianoRollEditRequest::Copy(deltas) => {
                // d.3 = next_start_beat は song-absolute → 各 note の所属クリップの clip-local へ。
                // コピー元と同じクリップ内に複製する (handler が decode して同 clip へ)。
                let entries: Vec<(u32, f64, u8)> = deltas
                    .iter()
                    .map(|d: &MoveDelta| {
                        let off = clip_starts
                            .get(AppData::note_id_clip_slot(d.0))
                            .copied()
                            .unwrap_or(0.0);
                        (d.0, d.3 - off, d.4)
                    })
                    .collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::CopyNotes(entries.clone()));
                })
            }
            PianoRollEditRequest::Resize(deltas) => {
                // d.3 = next_start_beat は song-absolute → 各 note の所属クリップの clip-local。
                // d.4 = len は不変。d.0 は packed のまま handler が decode する。
                let entries: Vec<(u32, f64, f64)> = deltas
                    .iter()
                    .map(|d: &ResizeDelta| {
                        let off = clip_starts
                            .get(AppData::note_id_clip_slot(d.0))
                            .copied()
                            .unwrap_or(0.0);
                        (d.0, d.3 - off, d.4)
                    })
                    .collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ResizeNotes(entries.clone()));
                })
            }
            PianoRollEditRequest::Select { next, .. } => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetNoteSelection(next.clone()));
            }),
            PianoRollEditRequest::SetLyrics(updates) => {
                // gui_01 #017 (M14 Phase 59): widget が L キー編集 → Enter
                // commit 時に 1 batch で発行する歌詞分配 request。 widget は
                // 編集対象 clip を context として知らないので、 piano_roll_view
                // が描画中の `target` (ClipRef) を closure に capture して渡す。
                let target_clip = target;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteLyrics {
                        clip_ref: target_clip,
                        lyrics: updates.clone(),
                    });
                })
            }
            PianoRollEditRequest::SetVelocity(updates) => {
                // gui_01 #018 (M14 Phase 64): velocity lane 内 drag の release
                // frame で 1 batch 発行される `Vec<(NoteId, u8)>`。 multi-select
                // 中はすべての selected note が同じ絶対値、 単独 hit は単一
                // note のみ含まれる。 drag<3px は widget 側で除外済。
                let entries: Vec<(u32, u8)> = updates.into_iter().collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteVelocities(entries.clone()));
                })
            }
            // gui_01 #041 (M14 Phase 69): ruler 上 plain click / drag の press +
            // continuation frame で逐次発火する seek 要求。 arrangement と同形で
            // `AppData::seek_playhead_to` に集約 (playhead 更新 + SeekTo)。 clip 内
            // clamp は意図的に行わない (= song-global で自由に動かせる)。
            // seek_playhead_to は「停止で戻るホーム」も更新する。
            PianoRollEditRequest::SetPlayheadBeat(beat) => {
                Edit::mutate(move |app: &mut AppData| {
                    app.seek_playhead_to(beat);
                })
            }
            // gui_01 #041 (M14 Phase 69): Shift + ruler drag release で 1 度
            // だけ発火する loop range commit。 既存 AppEvent::SetLoopRange
            // (arrangement / audio_editor と共通) にそのまま流す。
            PianoRollEditRequest::SetLoopRange { start, end } => {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetLoopRange { start, end });
                })
            }
            // edge auto-scroll の横スクロール (delta 拍)。widget は clip 相対オフセットを
            // 知らないので delta で渡る → ここで `pianoroll_scroll_beat` に加算 (handler が `>= 0` clamp)。
            PianoRollEditRequest::ScrollByBeats(by) => Edit::mutate(move |app: &mut AppData| {
                // f64 で加算してから 1 度だけ f32 に丸める (精度保持)。handler が `>= 0` clamp する。
                #[allow(clippy::cast_possible_truncation)]
                let next = (f64::from(app.pianoroll_scroll_beat()) + by) as f32;
                app.handle_event(AppEvent::SetPianoRollScrollX(next));
            }),
            // edge auto-scroll の縦 (pitch) スクロール (絶対 top_pitch、widget が clamp 済)。
            PianoRollEditRequest::SetTopPitch(p) => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetPianoRollTopPitch(p));
            }),
        }
    };

    let resp = ui.piano_roll(
        "piano_roll",
        body,
        &widget_notes,
        view,
        &app.selected_notes,
        &style,
        make_edit,
    );

    // 複数表示時のみ右側に凡例パネル (色 swatch / クリップ名 / 対象切替 /
    // ロックトグル) を描画。単一表示は legend_rect = None で従来レイアウト不変。
    if let Some(legend_rect) = legend_rect {
        draw_legend(app, ui, legend_rect, &shown, target);
    }

    // 歌詞 inline 編集中フラグを app に mirror する。 root.rs の
    // `dispatch_shortcuts` は piano_roll widget より前に走って `take_shortcut("escape")`
    // を消費してしまうため、 編集中は app 側フラグを見て Esc を消費させず widget に委ねる
    // (= widget が歌詞編集を 1 frame で cancel)。 変化したフレームだけ Edit を発行する
    // (keyboard_active_pitch mirror と同方針、 毎フレーム push を避ける)。
    let lyric_editing = resp.lyric_editing.is_some();
    if lyric_editing != app.piano_roll_lyric_editing {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.piano_roll_lyric_editing = lyric_editing;
        }));
    }

    // gui_01 #055: 鍵盤レーン click のピッチプレビュー。 widget が押下中の pitch を
    // keyboard_active_pitch で返す。 前フレーム値 (app.preview_note の pitch) と
    // 差分し、 変化した frame だけ PreviewPitchChanged を発火する (handler が
    // note-on/off を導出)。 鳴らす track は描画中 clip の track (target.track)。
    if resp.keyboard_active_pitch != app.preview_note.map(|(_, p)| p) {
        let track_idx = target.track;
        let pitch = resp.keyboard_active_pitch;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::PreviewPitchChanged { track_idx, pitch });
        }));
    }

    // ノート paste の配置位置。grid 上のポインタを **clip-local** snapped
    // beat にして毎フレーム mirror。`view.start_beat` は song-absolute なので、
    // dbl-click の AddNote (`snapped_beat - clip_start_beat`) と同様に clip_start_beat を
    // 引いて clip-local に変換する (paste_notes_at は clip-local 前提)。grid 外は None。
    let hover_beat: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !grid_rect.contains(px, py) {
            return None;
        }
        let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
        let beat_raw = view.start_beat + (px - grid_rect.x) as f64 / beat_to_px;
        let cfg = snap::piano_roll_snap_config(app);
        let alt = ui.pointer().modifiers.alt;
        let snapped = cfg.snap_beat(beat_raw, alt, app.pianoroll_zoom_x());
        Some(snapped - clip_start_beat)
    });
    // f キー用に **song-absolute かつ snap なし** の生 beat も mirror する
    // (clip_start_beat を引く前。snap は dispatch 側で live Alt 付きで song-absolute grid
    // に対して行う)。clip_start_beat が snap unit の倍数でなくても、ruler が見せる
    // song-absolute grid 線にプレイヘッドが乗るようにするため、clip-local の
    // `pianoroll_hover_beat` とは別空間で保持する。
    let hover_beat_song_raw: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !grid_rect.contains(px, py) {
            return None;
        }
        let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
        Some(view.start_beat + (px - grid_rect.x) as f64 / beat_to_px)
    });
    if app.pianoroll_hover_beat != hover_beat
        || app.pianoroll_hover_beat_song_raw != hover_beat_song_raw
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.pianoroll_hover_beat = hover_beat;
            app.pianoroll_hover_beat_song_raw = hover_beat_song_raw;
        }));
    }
    // q キー (選択が無ければカーソル直下 note を mute) 用に、ポインタ直下の
    // note index (= clip 内 notes Vec の index、`selected_notes` と同空間) を毎フレーム mirror。
    // grid 外 / note 外は None。widget の `note_hit` を流用して drag hit-test と同じ判定にする。
    let hover_note: Option<u32> = ui.pointer().pos.and_then(|(px, py)| {
        note_hit(&widget_notes, view, grid_rect, px, py, resize_handle_px).map(|(id, _)| id)
    });
    if app.pianoroll_hover_note != hover_note {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.pianoroll_hover_note = hover_note;
        }));
    }

    // 空白上の note 作成 (ダブルクリック + 放さずドラッグで長さ決定、Bitwig 流) は
    // widget 側に一本化した (`take_double_click_press_in_rect` → NoteCreateSession → `Add`)。
    // 旧 release ベース `take_double_click_in_rect` の AddNote はここから撤去 (press を放さず
    // ドラッグを捕捉できないため)。 widget の `Add` request は make_edit で AddNote に変換され、
    // `n.len_beats` (ドラッグ長 or 既定長) を尊重する。

    // wheel handler — note drag / 作成中は無効
    // 一般的な DAW (Ableton Live / Reaper) 流: Ctrl=横ズーム, Alt=縦ズーム,
    // Shift=横スクロール, plain=ピッチスクロール (上下)。
    if resp.dragging.is_none() && !resp.creating {
        let pointer = ui.pointer();
        if let Some((px, py)) = pointer.pos
            && grid_rect.contains(px, py)
        {
            let (sx, sy) = pointer.scroll_delta;
            if sy.abs() > 0.001 || sx.abs() > 0.001 {
                let scroll_beat = app.pianoroll_scroll_beat();
                let top_pitch = app.pianoroll_top_pitch() as i32;
                let modifiers = pointer.modifiers;
                if modifiers.ctrl {
                    // Ctrl+wheel: 横ズーム。マウス位置の拍を anchor として保持する
                    // (一般的な DAW の挙動)。new_scroll = anchor_beat - (px-grid.x)/new_zoom。
                    let factor = (sy * 0.005).exp();
                    let new_zoom = (zoom_x * factor).clamp(8.0, 400.0);
                    if (new_zoom - zoom_x).abs() > 1e-3 {
                        let anchor_beat =
                            f64::from(scroll_beat) + f64::from((px - grid_rect.x) / zoom_x);
                        let new_scroll =
                            (anchor_beat - f64::from((px - grid_rect.x) / new_zoom)).max(0.0)
                                as f32;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollZoomX(new_zoom));
                            app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                        }));
                    }
                } else if modifiers.alt {
                    // Alt+wheel: 縦ズーム。マウス位置のピッチを anchor として保持。
                    // top_pitch は u8 なので round 後 best-effort。
                    let factor = (sy * 0.005).exp();
                    let new_zoom = (zoom_y * factor).clamp(6.0, 40.0);
                    if (new_zoom - zoom_y).abs() > 1e-3 {
                        let anchor_pitch =
                            f32::from(top_pitch as u8) - (py - grid_rect.y) / zoom_y;
                        let new_top_f = anchor_pitch + (py - grid_rect.y) / new_zoom;
                        let new_top = new_top_f.round().clamp(11.0, 127.0) as u8;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollZoomY(new_zoom));
                            app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                        }));
                    }
                } else if modifiers.shift {
                    let dx_beats = -(sx + sy) / zoom_x;
                    let new_scroll = (scroll_beat + dx_beats).max(0.0);
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                    }));
                } else {
                    let delta = (sy / 12.0).round() as i32;
                    if delta != 0 {
                        let new_top = (top_pitch + delta).clamp(11, 127) as u8;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                        }));
                    }
                }
            }
        }
    }

}

/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle][snap unit dropdown][Fit] [Fold][Snap on Draw]。
/// Fold / Snap on Draw は Phase 7 B5 (Scale &amp; Root)。
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    ui.panel("pr_toolbar_bg", rect, theme::HEADER, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;
    let fold_w = 50.0;
    let snap_draw_w = 100.0;

    let mut x = rect.x + pad;

    ui.toggle_button_at(
        "pr_snap_toggle",
        "Snap",
        Rect { x, y, w: toggle_w, h },
        app.pianoroll_snap_enabled,
        &SNAP_TOGGLE_STYLE,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetPianoRollSnapEnabled(new));
            })
        },
    );
    x += toggle_w + pad;

    if let Some(idx) = ui.dropdown(
        "pr_snap_unit",
        Rect { x, y, w: dropdown_w, h },
        SNAP_LABELS,
        app.pianoroll_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetPianoRollSnapChoice(new));
        }));
    }
    x += dropdown_w + pad;

    ui.button_at("pr_fit", "Fit", Rect { x, y, w: fit_w, h }, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitPianoRollToClip);
        })
    });
    x += fit_w + pad * 2.0;

    // Phase 7 B5 (`docs/plan_scale.html` §4.4): Fold to Scale toggle。
    // ON で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)。
    // Song.scale_changes が空のときも toggle 自体は active 化できるが、
    // PianoRollView.scale = None なので visual には影響しない。
    ui.toggle_button_at(
        "pr_fold_to_scale",
        "Fold",
        Rect { x, y, w: fold_w, h },
        app.piano_roll_fold,
        &SNAP_TOGGLE_STYLE,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleFoldToScale);
            })
        },
    );
    x += fold_w + pad;

    // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。
    // ON で note 追加時の pitch を Song.scale_at(beat).snap(pitch) で
    // in-scale に寄せる (Highlight mode 前提、 Fold mode は widget 側で
    // 既に in-scale)。
    ui.toggle_button_at(
        "pr_snap_on_draw",
        "Snap Draw",
        Rect { x, y, w: snap_draw_w, h },
        app.snap_on_draw,
        &SNAP_TOGGLE_STYLE,
        |_| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSnapOnDraw);
            })
        },
    );
}

/// 複数表示時に右側へ出す凡例パネル。各行 = **1 トラック** = [色 swatch][トラック名][L ロック]。
/// トラック行クリックで対象 (target) をそのトラックの (表示中) クリップへ切替、L トグルでそのトラックの
/// ロック (参照専用) を反転。対象トラックの行は左端 accent バー + 通常文字色で強調 (非対象は淡色)。
/// ノート色・dim もトラック基準なので整合する (REAPER / Cakewalk のトラックペイン流)。`target` は
/// 描画中の対象クリップ (その `.track` が対象トラック)。
fn draw_legend(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    shown: &[ClipRef],
    target: ClipRef,
) {
    ui.panel("pr_legend_bg", rect, theme::HEADER, 0.0);
    let pad = 6.0;
    let row_h = 28.0;
    let gap = 4.0;
    // パネル見出し (表示トラック)。
    ui.label_at(
        "pr_legend_title",
        "\u{8868}\u{793a}\u{30c8}\u{30e9}\u{30c3}\u{30af}",
        rect.x + pad,
        rect.y + pad,
        11.0,
        COLOR_HINT,
    );
    // 表示中クリップが乗っている **トラック** を初出順に列挙 (同じトラックの複数クリップは 1 行)。
    let mut track_indices: Vec<u32> = Vec::new();
    for &r in shown {
        if !track_indices.contains(&r.track) {
            track_indices.push(r.track);
        }
    }
    let mut y = rect.y + pad + 18.0;
    for (row_i, &ti) in track_indices.iter().enumerate() {
        if y + row_h > rect.y + rect.h {
            break;
        }
        let Some(track) = app.song.tracks.get(ti as usize) else {
            continue;
        };
        let track_id = track.id;
        let is_target = ti == target.track;
        let locked = app.is_pianoroll_track_locked(track_id);
        // 対象切替先 = このトラックの代表クリップ (anchor がこのトラックなら anchor、 でなければ
        // このトラックの最初の表示クリップ)。target_clip は anchor なので legend 切替で動く。
        let rep_key = if ti == target.track {
            app.clip_key_of(target)
        } else {
            shown
                .iter()
                .copied()
                .find(|r| r.track == ti)
                .and_then(|r| app.clip_key_of(r))
        };
        let row = Rect {
            x: rect.x + pad,
            y,
            w: (rect.w - pad * 2.0).max(0.0),
            h: row_h,
        };
        // 行背景 (対象トラック = accent wash で薄く強調)。
        ui.panel(
            ("pr_legend_row", row_i),
            row,
            if is_target {
                theme::ACCENT_WASH
            } else {
                theme::PANEL_RAISED
            },
            4.0,
        );
        // 対象トラック行の左端 accent バー。
        if is_target {
            ui.push_rect(RectCommand::uniform_radius(
                Rect { x: row.x, y: row.y, w: 3.0, h: row.h },
                theme::ACCENT,
                1.5,
            ));
        }
        // 色 swatch (= トラック実効色)。
        let color = track_color::to_renderer(track_color::effective_track_color(track));
        ui.push_rect(RectCommand {
            rect: Rect {
                x: row.x + 8.0,
                y: row.y + (row.h - 13.0) * 0.5,
                w: 13.0,
                h: 13.0,
            },
            fill: color,
            border: theme::BORDER,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
        // ロックトグル (右端、トラック単位)。
        let lock_w = 26.0;
        let lock_rect = Rect {
            x: row.x + row.w - lock_w - 4.0,
            y: row.y + 4.0,
            w: lock_w,
            h: row.h - 8.0,
        };
        ui.toggle_button_at(
            ("pr_legend_lock", row_i),
            "L",
            lock_rect,
            locked,
            &SNAP_TOGGLE_STYLE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::TogglePianoRollTrackLock(track_id));
                })
            },
        );
        // トラック名 (クリック = 対象トラック切替)。swatch と lock の間の領域。
        let name_x = row.x + 26.0;
        let name_rect = Rect {
            x: name_x,
            y: row.y,
            w: (lock_rect.x - name_x - 4.0).max(10.0),
            h: row.h,
        };
        // 透明ヒット (空テキストの button) でクリックを拾い、テキストは下で label 描画する
        // (button の中央寄せ固定文字でなく ellipsis 付き左寄せラベルを出すため)。
        if ui.button_at_clicked_sized_aligned(
            ("pr_legend_name", row_i),
            "",
            name_rect,
            12.0,
            ButtonTextAlign::Left,
        ) && let Some(k) = rep_key
        {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetPianoRollTargetClip(k));
            }));
        }
        let label = track.name.clone();
        let text_color = if is_target {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        let label_rect = Rect {
            x: name_rect.x + 4.0,
            y: name_rect.y + (name_rect.h - 12.0) * 0.5,
            w: (name_rect.w - 8.0).max(8.0),
            h: 14.0,
        };
        ui.label_at_clipped(("pr_legend_name_label", row_i), &label, label_rect, 12.0, text_color);
        y += row_h + gap;
    }
}

/// 表示対象クリップ群 (`shown`) の note を **すべて** `daw_ui_core::Note` に変換する。
/// 各 note の id は packed global id (`AppData::pack_note_id(clip_slot, index)`) で複数クリップでも
/// 衝突しない。**色はそのクリップが乗っている _トラック_ の実効色** (`effective_track_color`、凡例が
/// トラック単位なのでノートもトラック色)、非対象 _トラック_ のノートは `dimmed`、ロック中トラックの
/// ノートは `locked` (widget 側で hit-test 除外)。`target_track` = 編集対象クリップのトラック index。
/// 毎フレーム alloc だが widget 内 cached で性能 OK。v6 linked clip の notes は
/// `Song.clip_contents` 経由で lookup する。
fn build_widget_notes(app: &AppData, shown: &[ClipRef], target_track: Option<u32>) -> Vec<Note> {
    let mut out: Vec<Note> = Vec::new();
    for (clip_slot, &r) in shown.iter().enumerate() {
        let Some(track) = app.song.tracks.get(r.track as usize) else {
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
        for (i, n) in app.song.clip_notes(clip).iter().enumerate() {
            out.push(Note {
                id: AppData::pack_note_id(clip_slot, i),
                // song-absolute 化: clip-local note + clip 開始位置。
                start_beat: n.start_beat + clip_start,
                len_beats: n.duration_beats,
                pitch: n.pitch,
                velocity: n.velocity,
                lyric: n.lyric.as_deref().filter(|s| !s.is_empty()).map(Arc::from),
                // note mute (dim + 斜線ハッチ表示)。`Note.muted` をそのまま渡す。
                muted: n.muted,
                style: NoteStyle {
                    color: Some(color),
                    dimmed,
                    locked,
                },
            });
        }
    }
    // widget の `note_hit` は note が start_beat 昇順ソート済を仮定して二分探索する。
    // 複数クリップは時間的に交錯するので、id (= 所属 / index) を保ったまま (start_beat, id)
    // で安定ソートする。id は配列順でなく packed 値なので decode に影響しない。
    out.sort_by(|a, b| {
        a.start_beat
            .partial_cmp(&b.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    out
}

