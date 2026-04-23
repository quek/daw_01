use common::model::{FxCommand, Note, NoteEvent, Row, Song};
use unicode_width::UnicodeWidthStr;
use vizia::prelude::*;

use crate::app::AppData;

pub struct ArrangementView;

impl ArrangementView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                // Two-line header: track names and column labels.
                Label::new(cx, AppData::tracker_header)
                    .font_family(vec![FamilyOwned::Named("HackGen Console NF".into())])
                    .font_size(13.0)
                    .color(Color::rgb(220, 220, 220))
                    .padding_bottom(Pixels(2.0));

                // One Label per row; background color binds to playhead_row so
                // the sounding row lights up without re-rendering the grid.
                //
                // Fixed row height keeps rows flush (default `auto` height
                // leaves ~1 line of gap between items). `height = font_size +
                // small padding` gives a tight tracker look.
                List::new(cx, AppData::tracker_rows, |cx, row_idx, item| {
                    Label::new(cx, item)
                        .font_family(vec![FamilyOwned::Named("HackGen Console NF".into())])
                        .font_size(13.0)
                        .height(Pixels(17.0))
                        .color(Color::rgb(220, 220, 220))
                        .background_color(AppData::playhead_row.map(move |p: &Option<u32>| {
                            if *p == Some(row_idx as u32) {
                                Color::rgb(40, 80, 50)
                            } else {
                                Color::rgba(0, 0, 0, 0)
                            }
                        }));
                })
                .class("tracker-list")
                .gap(Pixels(0.0));
            })
            .padding(Pixels(8.0))
            .gap(Pixels(0.0));
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

/// Renders the fixed two-line header shown above the tracker grid: one line
/// with track names / clip names (with a `▶` marker on the cursor track) and
/// one line with the per-column labels (NOT / VOL / FX / LYR).
pub fn render_tracker_header(song: &Song, cursor_track: u32) -> String {
    if song.tracks.is_empty() {
        return "No tracks.\nTrack > Add Vocal Track to begin.".to_string();
    }
    let mut out = String::new();

    out.push_str("##  ");
    for (track_idx, track) in song.tracks.iter().enumerate() {
        let clip_name = track
            .clips
            .first()
            .map(|c| c.name.as_str())
            .unwrap_or("(no clip)");
        let marker = if track_idx as u32 == cursor_track {
            "▶"
        } else {
            " "
        };
        out.push_str(&format!("|{marker}{}: {:<12} ", track.name, clip_name));
    }
    out.push('\n');

    out.push_str("    ");
    for _ in &song.tracks {
        out.push_str(&format!("| {CELL_HEADER}  "));
    }
    out
}

/// Renders one string per row of the tracker grid. The cursor row gets a `>`
/// prefix and the cursor cell is wrapped in `[…]`. Returns an empty vec when
/// there are no tracks (the header carries the "No tracks." message).
pub fn render_tracker_rows(song: &Song, cursor_row: u32, cursor_track: u32) -> Vec<String> {
    if song.tracks.is_empty() {
        return Vec::new();
    }
    let visible_rows = song
        .tracks
        .iter()
        .flat_map(|t| t.clips.first())
        .map(|c| c.rows.len())
        .max()
        .unwrap_or(16)
        .max(16);

    let mut rows = Vec::with_capacity(visible_rows);
    for row_idx in 0..visible_rows {
        let is_cursor_row = row_idx as u32 == cursor_row;
        let prefix = if is_cursor_row { '>' } else { ' ' };
        let mut line = format!("{prefix}{row_idx:02X} ");
        for (track_idx, track) in song.tracks.iter().enumerate() {
            let cell = track
                .clips
                .first()
                .and_then(|c| c.rows.get(row_idx))
                .map(format_row)
                .unwrap_or_else(empty_cell);
            if is_cursor_row && track_idx as u32 == cursor_track {
                line.push_str(&format!("|[{cell}] "));
            } else {
                line.push_str(&format!("| {cell}  "));
            }
        }
        rows.push(line);
    }
    rows
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
    fn render_tracker_header_empty_song() {
        let song = Song::default();
        assert_eq!(
            render_tracker_header(&song, 0),
            "No tracks.\nTrack > Add Vocal Track to begin."
        );
    }

    #[test]
    fn render_tracker_rows_empty_song() {
        let song = Song::default();
        assert_eq!(render_tracker_rows(&song, 0, 0), Vec::<String>::new());
    }
}
