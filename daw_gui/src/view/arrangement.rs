use common::model::{FxCommand, Note, NoteEvent, Row, Song};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
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

/// Columns inside each track cell are laid out as
/// `<NOT:3> <VOL:3> <FX:3> <LYR:3>` with single-space separators, for a
/// total display width of 15 before the surrounding separators.
const CELL_HEADER: &str = "NOT VOL FX  LYR";
/// Display width of `CELL_HEADER` (and of each data row's cell payload).
const CELL_WIDTH: usize = 15;
/// Display width reserved for the lyric sub-column. CJK moras are 2 cells,
/// ASCII placeholders 1 — both get padded to this width.
const LYRIC_WIDTH: usize = 3;
/// Display width of each track cell *including* its leading separator style
/// (either `" "` / `"  "` for non-cursor or `"["` / `"] "` for cursor).
/// Chosen so every shape — `| {cell}  `, `|[{cell}] `, and the track-header
/// cell — occupies the same number of cells after the `|`.
const TRACK_HEADER_WIDTH: usize = CELL_WIDTH + 3;

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
/// result's display width is exactly `target`. Used to clamp variable-width
/// header content (track name + clip name, possibly CJK) to a fixed cell.
fn pad_or_truncate_display(s: &str, target: usize) -> String {
    let w = s.width();
    if w == target {
        return s.to_string();
    }
    if w < target {
        return format!("{s}{}", " ".repeat(target - w));
    }
    // Truncate: accumulate chars while display width stays <= target.
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

/// Renders the fixed two-line header shown above the tracker grid: one line
/// with track names / clip names (with a `>` marker on the cursor track) and
/// one line with the per-column labels (NOT / VOL / FX / LYR).
pub fn render_tracker_header(
    song: &Song,
    cursor_track: u32,
    vis_start: u32,
    vis_count: u32,
) -> String {
    if song.tracks.is_empty() {
        return "No tracks.\nTrack > Add Vocal Track to begin.".to_string();
    }
    let vis_end = (vis_start + vis_count) as usize;
    let mut out = String::new();

    out.push_str("##  ");
    for (track_idx, track) in song.tracks.iter().enumerate() {
        if track_idx < vis_start as usize || track_idx >= vis_end {
            continue;
        }
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
        out.push('|');
        let content = format!("{marker}{}: {}", track.name, clip_name);
        out.push_str(&pad_or_truncate_display(&content, TRACK_HEADER_WIDTH));
    }
    out.push('\n');

    out.push_str("    ");
    for track_idx in 0..song.tracks.len() {
        if track_idx < vis_start as usize || track_idx >= vis_end {
            continue;
        }
        out.push_str(&format!("| {CELL_HEADER}  "));
    }
    out
}

/// Renders one string per row of the tracker grid. The cursor row gets a `>`
/// prefix and the cursor cell is wrapped in `[…]`. Returns an empty vec when
/// there are no tracks (the header carries the "No tracks." message).
pub fn render_tracker_rows(
    song: &Song,
    cursor_row: u32,
    cursor_track: u32,
    vis_start: u32,
    vis_count: u32,
) -> Vec<String> {
    if song.tracks.is_empty() {
        return Vec::new();
    }
    let vis_end = (vis_start + vis_count) as usize;
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
            if track_idx < vis_start as usize || track_idx >= vis_end {
                continue;
            }
            let cell = track
                .clips
                .first()
                .and_then(|c| c.rows.get(row_idx))
                .map(format_row)
                .unwrap_or_else(empty_cell);
            if is_cursor_row && track_idx as u32 == cursor_track {
                // `|[ cell ] ` → `|` + 17-wide content: `[` + 15 cell + `]` + ` `.
                line.push_str(&format!("|[{cell}] "));
            } else {
                // `|  cell   ` → `|` + 18-wide content: ` ` + 15 cell + `  `.
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
    // `{vol:<3}` pads hex (2 chars) with a trailing space so the VOL column
    // occupies 3 cells; NOT / FX / LYR are already 3 cells wide.
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
        // 15 display chars: "--- " + "--  " + "--- " + "-  "
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
        // "こ" has display width 2 → padded with one trailing space to
        // reach LYRIC_WIDTH=3.
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
    fn cell_header_width_is_15() {
        assert_eq!(CELL_HEADER.width(), CELL_WIDTH);
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
        // "あいうえお" is 10 display cells; target 7 truncates to "あいう "
        // (3 wide chars = 6 cells, plus 1 space = 7 cells).
        assert_eq!(pad_or_truncate_display("あいうえお", 7), "あいう ");
    }

    #[test]
    fn pad_or_truncate_display_exact_fit() {
        assert_eq!(pad_or_truncate_display("hello", 5), "hello");
    }

    #[test]
    fn render_tracker_header_empty_song() {
        let song = Song::default();
        assert_eq!(
            render_tracker_header(&song, 0, 0, 32),
            "No tracks.\nTrack > Add Vocal Track to begin."
        );
    }

    #[test]
    fn render_tracker_rows_empty_song() {
        let song = Song::default();
        assert_eq!(render_tracker_rows(&song, 0, 0, 0, 32), Vec::<String>::new());
    }
}
