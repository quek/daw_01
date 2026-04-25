//! Tracker grid renderers — pure functions that turn a `Song` plus a
//! cursor / visible-window descriptor into the strings the unified
//! tracker-mixer view binds to. Output is shaped per-track so each track
//! gets its own column widget in `view::tracker_mixer`.

use common::model::{FxCommand, Note, NoteEvent, Row, Song};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of the data payload inside one tracker cell:
/// `<NOT:3> <VOL:3> <FX:3> <LYR:3>` with single-space separators.
const CELL_WIDTH: usize = 15;
const LYRIC_WIDTH: usize = 3;
/// Display width of one track column (the cell payload + 1 cell of left
/// padding for the cursor `[` / blank, + 1 cell of right padding for the
/// matching `]` / blank). Bracketed and non-bracketed cells stay the same
/// width so the column never reflows when the cursor moves.
const TRACK_COL_WIDTH: usize = CELL_WIDTH + 2;
/// Minimum row count shown in the tracker grid even when no clip has any
/// rows yet.
const MIN_TRACKER_ROWS: usize = 16;

fn empty_cell() -> String {
    format_row(&Row::default())
}

fn pad_to_width(s: &str, target: usize) -> String {
    let w = s.width();
    if w >= target {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target - w))
    }
}

/// Pad with trailing spaces, or truncate on a character boundary so the
/// result's display width is exactly `target`.
fn pad_or_truncate_display(s: &str, target: usize) -> String {
    let w = s.width();
    if w == target {
        return s.to_string();
    }
    if w < target {
        return format!("{s}{}", " ".repeat(target - w));
    }
    let mut acc = 0;
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + cw > target {
            break;
        }
        out.push(ch);
        acc += cw;
    }
    while acc < target {
        out.push(' ');
        acc += 1;
    }
    out
}

/// One header string per visible slot. Slot `i` corresponds to
/// `song.tracks[vis_start + i]`. Cursor track gets a `>` marker; empty
/// slots (slot index past the end of the song) get blank strings padded
/// to the column width so the layout doesn't collapse.
pub fn render_visible_track_headers(
    song: &Song,
    cursor_track: u32,
    vis_start: u32,
    vis_count: u32,
) -> Vec<String> {
    let mut out = Vec::with_capacity(vis_count as usize);
    for slot in 0..vis_count {
        let track_idx = vis_start as usize + slot as usize;
        let Some(track) = song.tracks.get(track_idx) else {
            out.push(pad_or_truncate_display("", TRACK_COL_WIDTH));
            continue;
        };
        let clip_name = track
            .clips
            .first()
            .map(|c| c.name.as_str())
            .unwrap_or("(no clip)");
        let marker = if track_idx as u32 == cursor_track {
            '>'
        } else {
            ' '
        };
        let content = format!("{marker}{}: {}", track.name, clip_name);
        out.push(pad_or_truncate_display(&content, TRACK_COL_WIDTH));
    }
    out
}

/// Per-cell text for the visible window, shaped as `[slot][row]`. The
/// cursor cell (cursor row × cursor track) is wrapped in `[…]`; every
/// other cell is wrapped in spaces, so each cell occupies the same display
/// width regardless of cursor state.
pub fn render_visible_tracker_cells(
    song: &Song,
    cursor_row: u32,
    cursor_track: u32,
    vis_start: u32,
    vis_count: u32,
) -> Vec<Vec<String>> {
    let row_count = visible_row_count(song);
    let mut out = Vec::with_capacity(vis_count as usize);
    for slot in 0..vis_count {
        let track_idx = vis_start as usize + slot as usize;
        let mut col = Vec::with_capacity(row_count);
        let track = song.tracks.get(track_idx);
        for row_idx in 0..row_count {
            let cell = track
                .and_then(|t| t.clips.first())
                .and_then(|c| c.rows.get(row_idx))
                .map(format_row)
                .unwrap_or_else(empty_cell);
            let is_cursor =
                track.is_some() && row_idx as u32 == cursor_row && track_idx as u32 == cursor_track;
            if is_cursor {
                col.push(format!("[{cell}]"));
            } else {
                col.push(format!(" {cell} "));
            }
        }
        out.push(col);
    }
    out
}

/// How many rows the tracker grid currently shows (max across all tracks,
/// floored at `MIN_TRACKER_ROWS`).
pub fn visible_row_count(song: &Song) -> usize {
    song.tracks
        .iter()
        .flat_map(|t| t.clips.first())
        .map(|c| c.rows.len())
        .max()
        .unwrap_or(MIN_TRACKER_ROWS)
        .max(MIN_TRACKER_ROWS)
}

/// Row-number labels: `"  00"`, `"  01"`, …. Cursor row gets `"> NN"`.
/// `row_count` is the number of entries returned.
pub fn render_row_numbers(row_count: usize, cursor_row: u32) -> Vec<String> {
    (0..row_count)
        .map(|row_idx| {
            let marker = if row_idx as u32 == cursor_row { '>' } else { ' ' };
            format!("{marker}{row_idx:02X} ")
        })
        .collect()
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
    format!("{note} {vol:<3} {fx} {lyric}")
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
    use common::model::{Clip, Track};

    fn clip_with_rows(rows: Vec<Row>) -> Clip {
        Clip {
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            rows_per_beat: 4,
            rows,
        }
    }

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
        assert_eq!(format_row(&Row::default()), "--- --  --- -  ");
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
        assert_eq!(format_row(&row), "C-4 --  --- こ ");
    }

    #[test]
    fn format_row_note_off() {
        let row = Row {
            note: Some(NoteEvent::Off),
            ..Default::default()
        };
        assert_eq!(format_row(&row), "OFF --  --- -  ");
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
        assert_eq!(format_row(&row), "--- 40  A04 -  ");
    }

    #[test]
    fn format_row_cell_width_is_15() {
        for row in [
            Row::default(),
            Row {
                note: Some(NoteEvent::On(Note {
                    key: 60,
                    velocity: 100,
                })),
                lyric: Some("こ".into()),
                ..Default::default()
            },
            Row {
                volume: Some(0x40),
                fx: vec![FxCommand {
                    cmd: 0xA,
                    value: 0x04,
                }],
                lyric: Some("-".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(format_row(&row).width(), CELL_WIDTH, "row={row:?}");
        }
    }

    #[test]
    fn cell_width_constant_matches_format_row_output() {
        assert_eq!(format_row(&Row::default()).width(), CELL_WIDTH);
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
    fn pad_or_truncate_display_pads_short() {
        assert_eq!(pad_or_truncate_display("AB", 5), "AB   ");
    }

    #[test]
    fn pad_or_truncate_display_truncates_long() {
        assert_eq!(pad_or_truncate_display("あいうえお", 7), "あいう ");
    }

    #[test]
    fn pad_or_truncate_display_exact_fit() {
        assert_eq!(pad_or_truncate_display("hello", 5), "hello");
    }

    #[test]
    fn render_visible_track_headers_empty_slots_padded() {
        let song = Song::default();
        let headers = render_visible_track_headers(&song, 0, 0, 3);
        assert_eq!(headers.len(), 3);
        for h in headers {
            assert_eq!(h.width(), TRACK_COL_WIDTH);
        }
    }

    #[test]
    fn render_visible_track_headers_marks_cursor() {
        let mut song = Song::default();
        song.tracks.push(Track {
            name: "T1".into(),
            clips: vec![clip_with_rows(vec![])],
            ..Default::default()
        });
        song.tracks.push(Track {
            name: "T2".into(),
            clips: vec![clip_with_rows(vec![])],
            ..Default::default()
        });
        let headers = render_visible_track_headers(&song, 1, 0, 2);
        assert!(headers[0].starts_with(' '));
        assert!(headers[1].starts_with('>'));
    }

    #[test]
    fn render_visible_tracker_cells_widths_consistent() {
        let mut song = Song::default();
        song.tracks.push(Track {
            name: "T".into(),
            clips: vec![clip_with_rows(vec![Row::default(); 4])],
            ..Default::default()
        });
        let cells = render_visible_tracker_cells(&song, 0, 0, 0, 1);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].len(), MIN_TRACKER_ROWS);
        for c in &cells[0] {
            assert_eq!(c.width(), TRACK_COL_WIDTH);
        }
    }

    #[test]
    fn render_visible_tracker_cells_brackets_only_at_cursor_intersection() {
        let mut song = Song::default();
        for n in 0..2 {
            song.tracks.push(Track {
                name: format!("T{n}"),
                clips: vec![clip_with_rows(vec![Row::default(); 4])],
                ..Default::default()
            });
        }
        let cells = render_visible_tracker_cells(&song, 2, 1, 0, 2);
        // Cursor cell wrapped in brackets.
        assert!(cells[1][2].starts_with('['));
        assert!(cells[1][2].ends_with(']'));
        // Same row but different track: spaces, not brackets.
        assert!(cells[0][2].starts_with(' '));
        assert!(cells[0][2].ends_with(' '));
        // Same track but different row: spaces, not brackets.
        assert!(cells[1][0].starts_with(' '));
        assert!(cells[1][0].ends_with(' '));
    }

    #[test]
    fn render_row_numbers_marks_cursor() {
        let nums = render_row_numbers(4, 2);
        assert_eq!(nums, vec![" 00 ", " 01 ", ">02 ", " 03 "]);
        // Every label is exactly 4 display cells (`> NN ` shape).
        for n in nums {
            assert_eq!(n.width(), 4);
        }
    }

    #[test]
    fn visible_row_count_floors_at_min() {
        let song = Song::default();
        assert_eq!(visible_row_count(&song), MIN_TRACKER_ROWS);
    }
}
