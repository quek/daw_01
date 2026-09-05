//! MIDI Capture の状態 (`docs/plan_global_sampler.md` §3.4)。
//!
//! MIDI 入力ポートに来た全ノートを wall-clock (UNIX ns、engine の走行セグメントと
//! 同じ時計) 付きで溜める。再生中に弾いたノートは `on_beat` (playhead の拍) も持つ。
//! arm / 録音状態は見ない (Q5)。session-only。
//!
//! 切り出し ([`build_clip_notes`]) の規則 (Q6):
//! - 選択内の全ノートが `on_beat` を持てばその拍 (clip 原点 = 選択開始の拍)。
//! - それ以外は `beat = (t - sel_start) * bpm / 60` (現在テンポで wall-clock を拍に)。
//! - clip 長は選択長を小節に切り上げ。

use std::collections::VecDeque;

use common::model::Note;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapturedNote {
    pub on_ns: u64,
    /// `None` = まだ押している。
    pub off_ns: Option<u64>,
    pub pitch: u8,
    pub velocity: u8,
    pub channel: u8,
    /// note-on 到着時に再生中だったときの playhead (拍)。
    pub on_beat: Option<f64>,
    pub off_beat: Option<f64>,
}

impl CapturedNote {
    pub fn end_ns(&self, now_ns: u64) -> u64 {
        self.off_ns.unwrap_or(now_ns).max(self.on_ns)
    }
}

/// [`CapturedNote`] を切り出すときの選択範囲と時間軸。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureWindow {
    pub start_ns: u64,
    pub end_ns: u64,
    /// 停止中のノートを拍に直すテンポ。
    pub bpm: f64,
    pub beats_per_bar: f64,
    /// `off_ns == None` (押しっぱなし) のノートの終端に使う「今」。
    pub now_ns: u64,
}

pub struct MidiCaptureState {
    pub notes: VecDeque<CapturedNote>,
    pub paused: bool,
    /// 選択範囲 `[start, end)` (wall-clock ns)。
    pub selection: Option<(u64, u64)>,
    pub preview_until: Option<std::time::Instant>,
}

impl Default for MidiCaptureState {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiCaptureState {
    pub fn new() -> Self {
        Self {
            notes: VecDeque::with_capacity(4096),
            paused: false,
            selection: None,
            preview_until: None,
        }
    }

    pub fn note_on(&mut self, at_ns: u64, channel: u8, pitch: u8, velocity: u8, beat: Option<f64>) {
        if self.paused {
            return;
        }
        // 同じ鍵の off 無し重複 on は前を閉じる (取りこぼした off / auto-repeat)。
        self.note_off(at_ns, channel, pitch, beat);
        self.notes.push_back(CapturedNote {
            on_ns: at_ns,
            off_ns: None,
            pitch,
            velocity,
            channel,
            on_beat: beat,
            off_beat: None,
        });
    }

    pub fn note_off(&mut self, at_ns: u64, channel: u8, pitch: u8, beat: Option<f64>) {
        if let Some(n) = self
            .notes
            .iter_mut()
            .rev()
            .find(|n| n.off_ns.is_none() && n.pitch == pitch && n.channel == channel)
        {
            n.off_ns = Some(at_ns.max(n.on_ns));
            n.off_beat = beat;
        }
    }

    /// `now - seconds` より前に終わったノートを落とす。押しっぱなしは残す。
    pub fn prune(&mut self, now_ns: u64, seconds: u32) {
        let horizon = now_ns.saturating_sub(u64::from(seconds) * 1_000_000_000);
        while let Some(front) = self.notes.front() {
            if front.off_ns.is_some_and(|off| off < horizon) {
                self.notes.pop_front();
            } else if front.off_ns.is_none() {
                // 押しっぱなしの後ろに古いノートが残っていても、順序を崩さず
                // ここで止める (次の prune で拾う)。
                break;
            } else {
                break;
            }
        }
        if let Some((s, _)) = self.selection
            && s < horizon
        {
            self.selection = None;
        }
    }

    /// 選択 (or 全体) に掛かるノート。
    pub fn notes_in(&self, start_ns: u64, end_ns: u64, now_ns: u64) -> impl Iterator<Item = &CapturedNote> {
        self.notes
            .iter()
            .filter(move |n| n.on_ns < end_ns && n.end_ns(now_ns) > start_ns)
    }
}

/// 選択範囲を持ち運ぶ drag payload (`Ui::begin_drag`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiCaptureDragPayload {
    pub start_ns: u64,
    pub end_ns: u64,
}

pub const MIDI_CAPTURE_DRAG_KIND: &str = "daw_01.midi_capture_range";

/// 描画に使う「wall-clock → x」の写像 (右端 = 今)。
#[derive(Debug, Clone, Copy)]
pub struct WallAxis {
    pub x: f32,
    pub w: f32,
    pub now_ns: u64,
    pub span_ns: u64,
}

impl WallAxis {
    pub fn oldest(&self) -> u64 {
        self.now_ns.saturating_sub(self.span_ns)
    }

    pub fn ns_to_x(&self, ns: u64) -> f32 {
        if self.span_ns == 0 {
            return self.x;
        }
        let rel = ns as f64 - self.oldest() as f64;
        self.x + (rel / self.span_ns as f64) as f32 * self.w
    }

    pub fn x_to_ns(&self, x: f32) -> u64 {
        if self.w <= 0.0 || self.span_ns == 0 {
            return self.now_ns;
        }
        let t = ((x - self.x) / self.w).clamp(0.0, 1.0) as f64;
        self.oldest() + (t * self.span_ns as f64).round() as u64
    }
}

/// 選択範囲のノートを clip ローカルの `Note` 列に直す。戻りは `(notes, clip_length_beats)`。
/// ノートが 1 つも掛からなければ `None`。
pub fn build_clip_notes(state: &MidiCaptureState, win: CaptureWindow) -> Option<(Vec<Note>, f64)> {
    let picked: Vec<&CapturedNote> = state.notes_in(win.start_ns, win.end_ns, win.now_ns).collect();
    if picked.is_empty() {
        return None;
    }
    let bpm = win.bpm.max(1.0);
    let ns_to_beats = |ns: u64| ns as f64 * bpm / 60.0 / 1e9;
    let all_have_beats = picked.iter().all(|n| n.on_beat.is_some());
    let mut notes = Vec::with_capacity(picked.len());
    let sel_len_beats = ns_to_beats(win.end_ns.saturating_sub(win.start_ns));
    if all_have_beats {
        // 選択開始の拍 = 最初のノートの拍から、選択開始までの wall-clock 差を引く。
        let first = picked.iter().min_by_key(|n| n.on_ns)?;
        let first_beat = first.on_beat?;
        let origin_beat = first_beat - ns_to_beats(first.on_ns.saturating_sub(win.start_ns));
        for (i, n) in picked.iter().enumerate() {
            let start = (n.on_beat? - origin_beat).max(0.0);
            let end_beat = match (n.off_beat, n.off_ns) {
                (Some(b), _) => b - origin_beat,
                // 再生中に押して停止後に離した / 押しっぱなし: wall-clock で補う。
                (None, off) => n.on_beat? - origin_beat + ns_to_beats(off.unwrap_or(win.now_ns).saturating_sub(n.on_ns)),
            };
            notes.push(Note {
                id: i as u32 + 1,
                start_beat: start,
                duration_beats: (end_beat - start).max(1.0 / 64.0),
                pitch: n.pitch,
                velocity: n.velocity,
                lyric: None,
                muted: false,
            });
        }
    } else {
        for (i, n) in picked.iter().enumerate() {
            let start = ns_to_beats(n.on_ns.saturating_sub(win.start_ns));
            let end = ns_to_beats(n.end_ns(win.now_ns).saturating_sub(win.start_ns));
            notes.push(Note {
                id: i as u32 + 1,
                start_beat: start,
                duration_beats: (end - start).max(1.0 / 64.0),
                pitch: n.pitch,
                velocity: n.velocity,
                lyric: None,
                muted: false,
            });
        }
    }
    let bar = win.beats_per_bar.max(1.0);
    let len = ((sel_len_beats - 1e-9) / bar).ceil().max(1.0) * bar;
    Some((notes, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn win(start_ns: u64, end_ns: u64) -> CaptureWindow {
        CaptureWindow { start_ns, end_ns, bpm: 120.0, beats_per_bar: 4.0, now_ns: end_ns + S }
    }

    #[test]
    fn stopped_notes_use_wall_clock_at_current_tempo() {
        let mut st = MidiCaptureState::new();
        // 120bpm: 1 拍 = 0.5 秒。選択開始 10s、ノートは 10.5s〜11.0s (拍 1〜2)
        st.note_on(10 * S + S / 2, 0, 60, 100, None);
        st.note_off(11 * S, 0, 60, None);
        let (notes, len) = build_clip_notes(&st, win(10 * S, 13 * S)).unwrap();
        assert_eq!(notes.len(), 1);
        assert!((notes[0].start_beat - 1.0).abs() < 1e-9);
        assert!((notes[0].duration_beats - 1.0).abs() < 1e-9);
        // 選択 3 秒 = 6 拍 → 2 小節に切り上げ
        assert_eq!(len, 8.0);
    }

    #[test]
    fn playing_notes_use_captured_beats() {
        let mut st = MidiCaptureState::new();
        // 選択開始 20s。ノート on は 20.25s (= 0.5 拍後) で拍 16.5 → 原点 16.0
        st.note_on(20 * S + S / 4, 0, 62, 90, Some(16.5));
        st.note_off(21 * S, 0, 62, Some(18.0));
        let (notes, len) = build_clip_notes(&st, win(20 * S, 22 * S)).unwrap();
        assert!((notes[0].start_beat - 0.5).abs() < 1e-9);
        assert!((notes[0].duration_beats - 1.5).abs() < 1e-9);
        assert_eq!(len, 4.0);
    }

    #[test]
    fn mixed_selection_falls_back_to_wall_clock() {
        let mut st = MidiCaptureState::new();
        st.note_on(10 * S, 0, 60, 100, Some(4.0));
        st.note_off(10 * S + S / 2, 0, 60, Some(5.0));
        st.note_on(11 * S, 0, 64, 100, None);
        st.note_off(11 * S + S / 2, 0, 64, None);
        let (notes, _) = build_clip_notes(&st, win(10 * S, 12 * S)).unwrap();
        assert!((notes[1].start_beat - 2.0).abs() < 1e-9);
    }

    #[test]
    fn held_note_extends_to_now_and_prune_keeps_it() {
        let mut st = MidiCaptureState::new();
        st.note_on(S, 0, 60, 100, None);
        st.note_on(2 * S, 0, 61, 100, None);
        st.note_off(2 * S + S / 10, 0, 61, None);
        st.prune(100 * S, 60);
        // 1s のノートは押しっぱなしなので残り、61 (2.1s に終了) は古いので消える…
        // ただし順序保持のため押しっぱなしの後ろは次回に回る。
        assert_eq!(st.notes.len(), 2);
        let (notes, _) = build_clip_notes(&st, CaptureWindow { start_ns: 0, end_ns: 3 * S, bpm: 60.0, beats_per_bar: 4.0, now_ns: 3 * S }).unwrap();
        assert!((notes[0].duration_beats - 2.0).abs() < 1e-9, "1s〜now(3s) = 2 拍 @60bpm");
    }

    #[test]
    fn empty_selection_yields_none() {
        let st = MidiCaptureState::new();
        assert!(build_clip_notes(&st, win(0, S)).is_none());
    }
}
