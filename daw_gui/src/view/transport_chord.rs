//! r.md #157: トランスポートの「今と次のコード」(Studio One の Chord Display、`docs/plan_chord_track.md` §3.3)。
//!
//! Key の右隣に、playhead の位置のコード (帯の箱と同じ色のチップ) と次のコード (薄い文字) を出す。
//! **曲にコードが 1 つも無ければ幅を取らない** — トランスポートの左詰め列は既定の窓幅で既に
//! いっぱいなので、コードを使わない曲のボタン配置を動かさない。
//! 名前は `AppData::chord_label`、色は `widgets::chord_colors` (帯と同じ口)。

use daw_ui_core::Ui;
use daw_ui_renderer::{Rect, RectCommand};

use crate::app::AppData;
use crate::widgets::chord_colors::{chord_box_fill, chord_box_ink};

/// 今のコードのチップの幅 / 次のコードの欄の幅 (px)。長い名前は末尾を省略する。
const NOW_W: f32 = 64.0;
const NEXT_W: f32 = 58.0;
const NOW_FONT: f32 = 13.0;
const NEXT_FONT: f32 = 12.0;

/// 今と次のコードを描き、次の要素の x を返す (コードの無い曲では `x` をそのまま返す)。
pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, x: f32, cy: f32, bh: f32) -> f32 {
    let song = app.cur.song_doc.song();
    if song.chords.is_empty() {
        return x;
    }
    let p = &*app.theme.core;
    let beat = app.cur.transport.playhead_beat.map(f64::from).unwrap_or(0.0).max(0.0);

    let chip = Rect { x, y: cy, w: NOW_W, h: bh };
    let (fill, text) = match song.chord_at(beat) {
        Some(ev) => (chord_box_fill(p, ev), app.chord_label(&ev.chord, ev.start_beat)),
        // 箱の無い区間 (どのトラックも追従しない) は無地のチップに「—」。
        None => (p.control, "\u{2014}".to_string()),
    };
    ui.push_rect(RectCommand::uniform_radius(chip, fill, 3.0));
    let text_rect = |r: Rect, font: f32| Rect {
        x: r.x + 6.0,
        y: area.y + (area.h - font * 1.2) * 0.5,
        w: (r.w - 10.0).max(0.0),
        h: font * 1.2,
    };
    ui.label_at_clipped("transport_chord_now", &text, text_rect(chip, NOW_FONT), NOW_FONT, chord_box_ink(p, fill));

    let next_slot = Rect { x: chip.x + chip.w + 2.0, y: cy, w: NEXT_W, h: bh };
    let next = song
        .next_chord_after(beat)
        .map_or_else(|| "\u{2192} \u{2014}".to_string(), |ev| format!("\u{2192} {}", app.chord_label(&ev.chord, ev.start_beat)));
    // クローム面 (transport の地) の上なので二次テキスト色。
    ui.label_at_clipped("transport_chord_next", &next, text_rect(next_slot, NEXT_FONT), NEXT_FONT, p.text_dim);
    next_slot.x + next_slot.w + 12.0
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordEvent, ChordKind};
    use daw_ui_core::{FrameInput, UiHost};
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Color, Primitive, Scene};

    use super::*;

    fn render(app: &mut AppData) -> (Scene, f32) {
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        host.set_palette(app.theme.core.clone());
        let mut scene = Scene::new();
        let mut next_x = 0.0;
        host.frame(app, &mut scene, PhysicalSize { width: 600, height: 40 }, FrameInput::default(), |app, ui| {
            next_x = draw(app, ui, Rect::new(0.0, 0.0, 600.0, 40.0), 10.0, 6.0, 28.0);
        });
        (scene, next_x)
    }

    fn glyph(scene: &Scene, text: &str) -> Option<(f32, Color)> {
        scene.primitives.iter().find_map(|p| match p {
            Primitive::Glyph(g) if &*g.text == text => Some((g.left, g.color)),
            _ => None,
        })
    }

    /// playhead の位置のコードと次のコードを出し、今のコードは帯と同じ塗りの上で 4.5:1 で読める
    /// (ダーク / ライト)。箱の間の空白では「—」と次のコード。コードの無い曲は幅を取らない。
    #[test]
    fn shows_the_chord_at_the_playhead_and_the_next_one() {
        for theme in ["dark", "light"] {
            let mut app = crate::test_support::headless_app();
            app.handle_event(crate::app::AppEvent::SetTheme(theme.to_string()));
            let (_, next_x) = render(&mut app);
            assert_eq!(next_x, 10.0, "{theme}: コードの無い曲では幅を取らない");

            app.edit_song(|s| {
                for (start, chord) in [(0.0, Chord::new(9, ChordKind::Minor)), (8.0, Chord::new(5, ChordKind::Major))] {
                    s.place_chord(ChordEvent { id: 0, start_beat: start, len_beats: 4.0, chord, color: None });
                }
            });
            app.cur.transport.playhead_beat = Some(1.0);
            let (scene, next_x) = render(&mut app);
            assert!(next_x > 10.0 + NOW_W);
            let (_, ink) = glyph(&scene, "Am").expect("今のコード");
            let fill = chord_box_fill(&app.theme.core, &app.cur.song_doc.song().chords[0]);
            let ratio = daw_ui_core::theme::contrast_ratio(ink, fill);
            assert!(ratio >= 4.5, "{theme}: 今のコードの文字 {ratio:.2}");
            assert!(glyph(&scene, "\u{2192} F").is_some(), "{theme}: 次のコード");

            // 箱の間 (拍 5): 今は無く、次は F。
            app.cur.transport.playhead_beat = Some(5.0);
            let (scene, _) = render(&mut app);
            assert!(glyph(&scene, "\u{2014}").is_some(), "{theme}: 箱の無い区間は —");
            assert!(glyph(&scene, "\u{2192} F").is_some());
        }
    }
}
