use common::model::{FxCommand, Note, NoteEvent, Row, Song};
use unicode_width::UnicodeWidthStr;
use vizia::prelude::*;

use crate::app::AppData;

pub struct ArrangementView;

impl ArrangementView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            Label::new(cx, AppData::song.map(render_tracker_text))
                .padding(Pixels(8.0))
                .font_size(13.0)
                .font_family(vec![FamilyOwned::Named("HackGen Console NF".to_string())])
                .color(Color::rgb(220, 220, 220));
        })
        .background_color(Color::rgb(32, 32, 36))
    }
}

impl View for ArrangementView {
    fn element(&self) -> Option<&'static str> {
        Some("arrangement-view")
    }
}

const CELL_HEADER: &str = "NOT VOL FX  LYR";
const LYRIC_WIDTH: usize = 2;

fn empty_cell() -> String {
    format!("--- -- --- {}", pad_to_width("-", LYRIC_WIDTH))
}

fn pad_to_width(s: &str, target: usize) -> String {
    let w = s.width();
    if w >= target {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target - w))
    }
}

fn render_tracker_text(song: &Song) -> String {
    if song.tracks.is_empty() {
        return "No tracks.\nTrack > Add Vocal Track to begin.".to_string();
    }
    let visible_rows = song
        .tracks
        .iter()
        .flat_map(|t| t.clips.first())
        .map(|c| c.rows.len())
        .max()
        .unwrap_or(16)
        .max(16);

    let mut out = String::new();

    // Track header
    out.push_str("## ");
    for track in &song.tracks {
        let clip_name = track
            .clips
            .first()
            .map(|c| c.name.as_str())
            .unwrap_or("(no clip)");
        out.push_str(&format!("| {}: {:<12} ", track.name, clip_name));
    }
    out.push('\n');

    // Column labels
    out.push_str("   ");
    for _ in &song.tracks {
        out.push_str(&format!("| {CELL_HEADER}  "));
    }
    out.push('\n');

    // Rows
    for row_idx in 0..visible_rows {
        out.push_str(&format!("{row_idx:02X} "));
        for track in &song.tracks {
            let cell = track
                .clips
                .first()
                .and_then(|c| c.rows.get(row_idx))
                .map(format_row)
                .unwrap_or_else(empty_cell);
            out.push_str(&format!("| {cell}  "));
        }
        out.push('\n');
    }

    out
}

fn format_row(row: &Row) -> String {
    let note = match &row.note {
        Some(NoteEvent::On(Note { key, .. })) => format_note(*key),
        Some(NoteEvent::Off) => "OFF".to_string(),
        None => "---".to_string(),
    };
    let vol = row
        .volume
        .map(|v| format!("{v:02X}"))
        .unwrap_or_else(|| "--".to_string());
    let fx = row
        .fx
        .first()
        .map(|f: &FxCommand| format!("{:X}{:02X}", f.cmd, f.value))
        .unwrap_or_else(|| "---".to_string());
    let lyric_raw = row.lyric.as_deref().unwrap_or("-");
    let lyric = pad_to_width(lyric_raw, LYRIC_WIDTH);
    format!("{note} {vol} {fx} {lyric}")
}

fn format_note(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
    ];
    let octave = (i16::from(key) / 12) - 1;
    let name = NAMES[(key as usize) % 12];
    format!("{name}{octave}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_note_middle_c() {
        assert_eq!(format_note(60), "C-4");
    }

    #[test]
    fn format_note_sharp() {
        assert_eq!(format_note(61), "C#4");
    }

    #[test]
    fn format_row_empty() {
        // lyric "-" pad to width 2 → "- "
        assert_eq!(format_row(&Row::default()), "--- -- --- - ");
    }

    #[test]
    fn format_row_note_on_with_lyric() {
        let row = Row {
            note: Some(NoteEvent::On(Note {
                key: 60,
                velocity: 100,
            })),
            lyric: Some("こ".into()),
            ..Default::default()
        };
        // lyric "こ" has display width 2, no pad
        assert_eq!(format_row(&row), "C-4 -- --- こ");
    }

    #[test]
    fn format_row_note_off() {
        let row = Row {
            note: Some(NoteEvent::Off),
            ..Default::default()
        };
        assert_eq!(format_row(&row), "OFF -- --- - ");
    }

    #[test]
    fn format_row_with_volume_and_fx() {
        let row = Row {
            volume: Some(0x40),
            fx: vec![FxCommand {
                cmd: 0xA,
                value: 0x04,
            }],
            ..Default::default()
        };
        assert_eq!(format_row(&row), "--- 40 A04 - ");
    }

    #[test]
    fn pad_to_width_ascii() {
        assert_eq!(pad_to_width("-", 2), "- ");
        assert_eq!(pad_to_width("ab", 2), "ab");
    }

    #[test]
    fn pad_to_width_japanese_already_fits() {
        assert_eq!(pad_to_width("こ", 2), "こ");
    }

    #[test]
    fn render_tracker_text_empty_song() {
        let song = Song::default();
        assert_eq!(
            render_tracker_text(&song),
            "No tracks.\nTrack > Add Vocal Track to begin."
        );
    }
}
