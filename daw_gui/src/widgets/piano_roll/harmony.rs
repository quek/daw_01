//! r.md #157: ピアノロールのコード適合 (`docs/plan_chord_track.md` §3.3)。
//!
//! - **色分け** (Cubase の Event Colors: Scale/Chords): ノートの開始位置のコードに対して
//!   「構成音 / スケール内 / どちらでもない」を、トラック色を潰さない **標識** で示す
//!   (構成音 = 左端の帯、どちらでもない = 輪郭。スケール内は何も足さない)。標識の色はノートの塗りから
//!   決める極性固定インク (`Palette::ink_for`) なので、明るいクリップ色でも暗いクリップ色でも読める。
//! - **背景のガイド**: 各コードの箱の時間範囲に、その構成音の行を薄く塗る (スケールのハイライト
//!   `draw_grid_background` と同じ「行の上に半透明」の流儀)。上の 2 つはツールバーの「Chord」で ON/OFF。
//! - **鳴る音の薄い枠** (Q5): 追従するトラックで、書いた音と鳴る音が違うノートに、鳴る音の行へ枠を重ねる
//!   (編集するのは書いた音)。こちらはトグルに関係なく常に出す (書いた音と鳴る音の食い違いを隠さない)。
//!
//! 判定はすべて `common::chord_follow` / `common::model::Chord` の式を引く (ここに式を写さない)。
//! 色分けの判定に使う音は **鳴る音** — 追従するトラックで「鳴ったときコードに合うか」を示す
//! (書いた音で判定すると、C で書いて追従させたトラックが Am の箱の上で全部「外れ」に見える)。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use common::chord_follow::chord_scale_mask;
use common::model::{Chord, Song};
use daw_ui_core::widgets::heavy::HeavyCtx;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, ClipKey};
use crate::handler::chord_follow_view::ClipSoundingKeys;

use super::{Note, NoteId, PianoRollStyle, PianoRollView, RowGeometry, note_fill_color, note_geometry_to_rect, note_to_rect};

/// ノートがその位置のコードにどう合うか。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Fit {
    /// コードの構成音 (ベース音を含む)。
    ChordTone,
    /// 構成音ではないが、コードに合うスケール (`chord_scale_mask`) の音。
    Scale,
    /// どちらでもない。
    Outside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NoteMark {
    /// `None` = 判定しない (コードの箱の外 / トグル OFF)。
    fit: Option<Fit>,
    /// 書いた音と違うときだけ、鳴る音の鍵盤。
    sounding: Option<u8>,
}

/// 背景のガイド 1 区間 (コードの箱 1 つ、song-absolute 拍)。
#[derive(Clone, Copy, Debug)]
struct GuideSpan {
    start: f64,
    end: f64,
    /// 構成音 (+ ベース) の pitch class の集合。
    tones: u16,
    root: u8,
}

/// 1 フレーム分のコード適合 (view_build で作り、heavy の cached 層が描く)。
#[derive(Default)]
pub(super) struct Harmony {
    guide: Vec<GuideSpan>,
    marks: HashMap<NoteId, NoteMark>,
    /// cached 層の再描画キー (上の中身の hash)。
    key: u64,
}

/// 構成音 + ベース音の集合 (ガイドと色分けの「構成音」の定義を 1 つにする)。
fn tone_mask(chord: &Chord) -> u16 {
    chord.pitch_class_mask() | (1 << chord.bass_pc())
}

/// `pitch` (鳴る音) が `chord` にどう合うか。
fn classify(song: &Song, chord: &Chord, pitch: u8, beat: f64) -> Fit {
    let pc = pitch % 12;
    if (tone_mask(chord) >> pc) & 1 == 1 {
        Fit::ChordTone
    } else if (chord_scale_mask(chord, song.scale_at(beat)) >> pc) & 1 == 1 {
        Fit::Scale
    } else {
        Fit::Outside
    }
}

impl Harmony {
    /// `notes` (song-absolute 拍、id は packed) と表示クリップ `shown` から作る。
    /// ランチャーのセルのノートは曲の時間軸に無いので判定しない。
    pub(super) fn build(app: &AppData, notes: &[Note], shown: &[ClipKey], view: PianoRollView) -> Self {
        let song = app.cur.song_doc.song();
        if song.chords.is_empty() {
            return Self::default();
        }
        let enabled = app.cur.view.piano_roll_chord_guide;
        // 表示クリップ (clip_slot 順) ごとに 1 度だけ: 曲の時間軸のクリップなら、鳴る鍵盤の読み口
        // (「鳴る音で確定」と同じ口。素材のコードの検出込み) と、その content のノート。
        let slots: Vec<Option<(ClipSoundingKeys<'_>, &[common::model::Note])>> = shown
            .iter()
            .map(|k| {
                let clip = song.track_by_id(k.track_id)?.clip_by_id(k.clip_id)?;
                Some((ClipSoundingKeys::of(song, k.track_id, clip), song.clip_notes(clip)))
            })
            .collect();
        let view_end = view.start_beat + view.len_beats;
        let mut marks = HashMap::new();
        // 描くのは見えているノートだけ (`notes` は start_beat 昇順)。
        let visible = notes
            .iter()
            .take_while(|n| n.start_beat <= view_end)
            .filter(|n| n.start_beat + n.len_beats >= view.start_beat);
        for n in visible {
            let Some(Some((keys, written))) = slots.get(AppData::note_id_clip_slot(n.id)) else { continue };
            // 追従するトラックは content のノート (content-local 拍) から鳴る鍵盤を引く。範囲外は `None` = 鳴らない。
            let sounds = match written.get(AppData::note_id_local_index(n.id)) {
                Some(note) if keys.is_active() => keys.key(note),
                _ => Some(n.pitch),
            };
            let fit = enabled
                .then(|| song.chord_at_onset(n.start_beat))
                .flatten()
                .zip(sounds)
                .map(|(ev, p)| classify(song, &ev.chord, p, n.start_beat));
            let mark = NoteMark { fit, sounding: sounds.filter(|p| *p != n.pitch) };
            if mark.fit.is_some() || mark.sounding.is_some() {
                marks.insert(n.id, mark);
            }
        }
        let guide: Vec<GuideSpan> = if enabled && slots.iter().any(Option::is_some) {
            let first = song.chords.partition_point(|c| c.end_beat() <= view.start_beat);
            song.chords[first..]
                .iter()
                .take_while(|c| c.start_beat < view_end)
                .map(|c| GuideSpan {
                    start: c.start_beat,
                    end: c.end_beat(),
                    tones: tone_mask(&c.chord),
                    root: c.chord.root % 12,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for g in &guide {
            (g.start.to_bits(), g.end.to_bits(), g.tones, g.root).hash(&mut h);
        }
        let mut sorted: Vec<_> = marks.iter().collect();
        sorted.sort_unstable_by_key(|(id, _)| **id);
        sorted.hash(&mut h);
        Self { guide, marks, key: h.finish() }
    }

    /// cached 層の viewport_key に足す値 (中身が変わったら背景を描き直す)。
    pub(super) fn cache_key(&self) -> u64 {
        self.key
    }

    /// 背景のガイド (行の塗りの上、拍線とノートの下)。
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(super) fn draw_guide<M: ?Sized + 'static>(
        &self,
        hctx: &mut HeavyCtx<'_, '_, M>,
        view: PianoRollView,
        grid: Rect,
        style: &PianoRollStyle,
    ) {
        if self.guide.is_empty() {
            return;
        }
        let p = hctx.palette();
        let (tone_fill, root_fill) = guide_fills(p, style.bg);
        let geom = RowGeometry::compute(view, grid);
        let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
        let lo = (view.pitch_top - view.pitch_visible).floor().max(0.0) as u8;
        let hi = view.pitch_top.ceil().min(127.0) as u8;
        for g in &self.guide {
            let x0 = (grid.x + ((g.start - view.start_beat) * beat_to_px) as f32).max(grid.x);
            let x1 = (grid.x + ((g.end - view.start_beat) * beat_to_px) as f32).min(grid.x + grid.w);
            if x1 <= x0 {
                continue;
            }
            for pitch in lo..=hi {
                let pc = pitch % 12;
                if (g.tones >> pc) & 1 == 0 {
                    continue;
                }
                // Fold は畳んだ行が無いので、スケール外の構成音はガイドを出さない (隣の行を塗ってしまう)。
                if geom.fold && geom.scale.is_some_and(|s| !s.is_in_scale(pitch)) {
                    continue;
                }
                let (y, _) = geom.pitch_to_y_and_h(pitch);
                let y0 = y.max(grid.y);
                let y1 = (y + geom.row_h).min(grid.y + grid.h);
                if y1 <= y0 {
                    continue;
                }
                hctx.push_rect(RectCommand {
                    rect: Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 },
                    fill: if pc == g.root { root_fill } else { tone_fill },
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(grid),
                });
            }
        }
    }

    /// ノートの上の標識 (色分け) と、鳴る音の薄い枠。`visible` は描いたノート (`draw_notes` と同じ集合)。
    pub(super) fn draw_marks<M: ?Sized + 'static>(
        &self,
        hctx: &mut HeavyCtx<'_, '_, M>,
        visible: &[Note],
        view: PianoRollView,
        grid: Rect,
        style: &PianoRollStyle,
    ) {
        if self.marks.is_empty() {
            return;
        }
        let p = hctx.palette();
        let frame_border = sounding_frame_border(p, style);
        for note in visible {
            let Some(mark) = self.marks.get(&note.id) else { continue };
            let fill = note_fill_color(note, style.velocity_ramp, style.bg);
            if let Some(sp) = mark.sounding {
                let r = note_geometry_to_rect(note.start_beat, note.len_beats, sp, view, grid);
                if let Some(r) = clip_to(r, grid) {
                    hctx.push_rect(RectCommand {
                        rect: r,
                        fill: fill.with_alpha(SOUNDING_FILL_ALPHA),
                        border: frame_border,
                        border_width: 1.0,
                        radius: [style.note_border_radius_px; 4],
                        clip_rect: Some(grid),
                    });
                }
            }
            let Some(r) = clip_to(note_to_rect(note, view, grid), grid) else { continue };
            let ink = mark_ink(p, fill);
            match mark.fit {
                Some(Fit::ChordTone) => hctx.push_rect(RectCommand {
                    rect: Rect { w: CHORD_TONE_BAR_W.min(r.w * 0.4), ..r },
                    fill: ink,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [style.note_border_radius_px, 0.0, 0.0, style.note_border_radius_px],
                    clip_rect: Some(grid),
                }),
                Some(Fit::Outside) => hctx.push_rect(RectCommand {
                    rect: r,
                    fill: Color::TRANSPARENT,
                    border: ink,
                    border_width: OUTSIDE_RING_W,
                    radius: [style.note_border_radius_px; 4],
                    clip_rect: Some(grid),
                }),
                Some(Fit::Scale) | None => {}
            }
        }
    }
}

/// 構成音の標識 (左端の帯) の幅 (px)。
const CHORD_TONE_BAR_W: f32 = 3.0;
/// 「どちらでもない」の輪郭の幅 (px)。ふだんの輪郭 (1px・薄め) と見分けられる太さ。
const OUTSIDE_RING_W: f32 = 1.5;
/// 鳴る音の枠の塗りの濃さ (ノートの塗りを薄く敷き、どのトラックの音かが分かる程度)。
const SOUNDING_FILL_ALPHA: f32 = 0.18;

/// 標識の色: ノートの塗りに対して読める極性固定インク。
pub(super) fn mark_ink(p: &crate::theme::Palette, note_fill: Color) -> Color {
    p.ink_for(note_fill)
}

/// 鳴る音の枠線: grid の地 (白鍵の行) に対して読めるインク (1px)。「薄い」のは塗り
/// ([`SOUNDING_FILL_ALPHA`]) の方で、線まで薄めるとライトで地に溶ける (alpha 0.7 で 2.5:1 だった)。
pub(super) fn sounding_frame_border(p: &crate::theme::Palette, style: &PianoRollStyle) -> Color {
    p.ink_for(style.bg)
}

/// ガイドの行と地の差 (コントラスト比) の目標。構成音の行 / ルートの行。
const GUIDE_TONE_CONTRAST: f32 = 1.18;
const GUIDE_ROOT_CONTRAST: f32 = 1.35;

/// ガイドの塗り (構成音の行 / ルートの行)。accent の半透明 (スケールの root 行の暖色と別の系統)。
///
/// 濃さは固定 alpha ではなく **地 (`style.bg`) との差が目標に届く最小の alpha** で決める。同じ alpha だと
/// ダークでは強すぎ、ライトでは地に溶ける (alpha 0.10 でライト 1.09:1)。
pub(super) fn guide_fills(p: &crate::theme::Palette, bg: Color) -> (Color, Color) {
    let reach = |target: f32| {
        let mut a = 0.04;
        while a < 0.5
            && daw_ui_core::theme::contrast_ratio(daw_ui_core::color::composite_over(p.accent.with_alpha(a), bg), bg)
                < target
        {
            a += 0.02;
        }
        p.accent.with_alpha(a)
    };
    (reach(GUIDE_TONE_CONTRAST), reach(GUIDE_ROOT_CONTRAST))
}

/// snap toolbar の「Chord」トグル (色分け + 構成音のガイドの ON/OFF。見方の都合なので dirty は立てない)。
pub(super) fn draw_toggle(
    app: &AppData,
    ui: &mut daw_ui_core::ui::Ui<'_, AppData>,
    rect: Rect,
    style: &daw_ui_core::ToggleButtonStyle,
) {
    ui.toggle_button_at("pr_chord_guide", "Chord", rect, app.cur.view.piano_roll_chord_guide, style, |on| {
        daw_ui_core::edit::Edit::mutate(move |app: &mut AppData| app.cur.view.piano_roll_chord_guide = on)
    });
    // 「鳴る音で確定」(Q5): 追従するトラックのクリップを開いているときだけ、トグルの右隣に出す。
    // 選んだノートだけ (無ければ表示中のクリップのノート全部) の書いた音を、薄い枠の位置へ書き換える。
    if app.can_freeze_shown_pitch() {
        let freeze = Rect { x: rect.x + rect.w + 4.0, w: 84.0, ..rect };
        ui.toggle_button_at("pr_freeze_pitch", "鳴る音で確定", freeze, false, style, |_| {
            daw_ui_core::edit::Edit::mutate(|app: &mut AppData| app.freeze_shown_pitch())
        });
    }
}

fn clip_to(r: Rect, grid: Rect) -> Option<Rect> {
    let x0 = r.x.max(grid.x);
    let x1 = (r.x + r.w).min(grid.x + grid.w);
    let y0 = r.y.max(grid.y);
    let y1 = (r.y + r.h).min(grid.y + grid.h);
    (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}
