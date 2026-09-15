//! Walks the song's clips/notes and emits MIDI transitions for the next
//! audio buffer. Owned by daw_audio; called from each track's worker
//! before handing events off to the plugin host.
//!
//! Migrated from `daw_plugin_host` as part of A2 (audio-engine refactor).

#![allow(dead_code)]

use common::model::{Clip, Note, Song};
use common::process_data::MAX_EVENTS;
use common::song_index::{RangeIndex, SongIndex};

/// `active_notes` の RT-safe な上限。 push 前にこの値でクランプして
/// `Vec` 再確保 (= RT 違反) を防ぐ。 `midi_bus_a` の `MAX_EVENTS` (=256)
/// と同等にして、 1 buffer 内で出力しうる On 数を吸収する。
/// SSoT: backing の `PerTrackState::with_capacity` (mixer.rs) も同じ
/// `MAX_EVENTS` で確保しているので、 clamp が効く限り再確保は起きない。
const ACTIVE_NOTES_CAP: usize = MAX_EVENTS;

/// PR-V2.4: `note_id` を追加。 値は
/// `common::plugin_metadata::sing_note_id(clip.id, note.id)` = **安定 id**
/// (r.md #75、アーキ不変条件 1)。 plugin host (= builtin VOICEVOX) はこの id で
/// `NoteMetadata` (歌詞 / phoneme) や合成 wav frame offset を引く。 daw_gui の
/// `sync_vocal_metadata` が **同じ関数**で同じ値を flush するので、clip の追加 /
/// 削除 / 並べ替え / muted で番号がずれない。 CLAP / VST3 backend はこの field を
/// 無視する (= 既存 MIDI pipeline はそのまま動く)。
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { note_id: u32, key: u8, velocity: f64 },
    Off { note_id: u32, key: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Phase 2 (`docs/plan_automation.md` §8.3): plugin parameter automation
/// 用の 1 イベント。`time` は buffer 内 sample offset、`param_id` は
/// CLAP `clap_id` / VST3 `ParamID` (共に u32)、`value` は plain 単位
/// (= plugin の `min_value..=max_value` スケール)。 plugin host 側で
/// CLAP `clap_event_param_value` / VST3 `IParameterChanges` に変換して
/// `plugin.process()` の input events に流す。
#[derive(Debug, Clone, Copy)]
pub struct TimedParamEvent {
    pub time: u32,
    pub param_id: u32,
    pub value: f64,
}

/// Per-track state owned exclusively by the audio worker that processes
/// the track. Survives across buffers so notes don't get cut on Stop /
/// loop-wrap.
#[derive(Default)]
pub struct PerTrackState {
    /// **発音台帳**: この track で鳴っている note の `(note_id, 送った鍵盤)`。Stop / loop wrap / seek の一括消音と、
    /// 通常の Off / 鳴らし直しが引く。**note_id を持ち回るのが要点** — CLAP / VST3 のプラグインは
    /// note-off をノート id で voice に当てる (`-1` だけが「未指定」) ので、 `0` や別 id の
    /// Off は無視されて鳴りっぱなしになる (Surge XT で停止しても止まらなかった)。
    ///
    /// **鍵盤は送った値を持つ** (r.md #130)。Off をその時点の `note.pitch` / 移調量から計算し直すと、鳴っている
    /// 間に音程や移調が変わったとき別の鍵盤を止めにいき、旧鍵盤が停止まで残る。照合は note_id だけで行う。
    pub active_notes: Vec<(u32, u8)>,
    /// NoteOffs `(note_id, key)` that must fire at frame 0 of the *next* buffer (after
    /// Stop / clip-end) so notes don't hang.
    pub pending_offs: Vec<(u32, u8)>,
    /// 鍵盤レーン click のプレビュー note (on/off)。 engine の `pump_commands`
    /// が `EngineCommand::PreviewNote*` を受けてここに積み、
    /// `process_track_owned` が frame 0 で `midi_bus_a` に注入して clear する。
    /// transport に関係なく注入されるので停止中でも発音する。 `active_notes`
    /// とは独立 (= sequencer の note 追跡を汚さない)。 lifecycle は GUI 所有
    /// (= mouse release で note-off を送る、 held-value + caller diff)。
    pub pending_preview: Vec<NoteTransition>,
    /// r.md #117: この track の device chain 入力で **最後に鳴った** note-on の起点 (と、 その
    /// note-off の秒)。 `Note` 起点のソースを **global** に落とすときの「最新ノート」。
    /// `process_track_owned` が毎 buffer `midi_bus_a` から更新し、 engine が刻みごとに
    /// `ModRuntime::set_note_anchor` へ写す。
    pub latest_note: Option<common::mod_graph::NoteAnchor>,
    /// `latest_note` の `(note_id, key)` (note-off の対応付け。 同じ key の重なりでも別ノートの
    /// Off で release を書かない)。
    pub latest_id: (u32, u8),
}

impl PerTrackState {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            active_notes: Vec::with_capacity(cap),
            pending_offs: Vec::with_capacity(cap),
            pending_preview: Vec::with_capacity(cap),
            latest_note: None,
            latest_id: (0, 0),
        }
    }

    /// r.md #117: この buffer の MIDI バスから最新ノートを更新する。 `beat0` / `secs0` は
    /// buffer 先頭の曲位置、 `beats_per_frame` / `sample_rate` で frame を換算する。
    pub fn observe_latest_note(
        &mut self,
        midi: &[TimedNoteEvent],
        beat0: f64,
        secs0: f64,
        beats_per_frame: f64,
        sample_rate: u32,
    ) {
        let sr = f64::from(sample_rate.max(1));
        for ev in midi {
            let t = f64::from(ev.time);
            match ev.event {
                NoteTransition::On { note_id, key, .. } => {
                    self.latest_note = Some(common::mod_graph::NoteAnchor {
                        beat: beat0 + t * beats_per_frame,
                        secs: secs0 + t / sr,
                        release_secs: None,
                    });
                    self.latest_id = (note_id, key);
                }
                NoteTransition::Off { note_id, key } => {
                    if (note_id, key) == self.latest_id
                        && let Some(a) = self.latest_note.as_mut()
                        && a.release_secs.is_none()
                    {
                        a.release_secs = Some(secs0 + t / sr);
                    }
                }
            }
        }
    }
}

/// 1 buffer (またはランチャー区間) の時間窓。 `collect_events_for_buffer` が note ごとの
/// 発音判定 ([`emit_note_events`]) に渡す。
#[derive(Clone, Copy)]
struct BufferWindow {
    /// 窓の先頭拍 (= 区間の実効拍)。
    playhead_beats: f64,
    samples_per_beat: f64,
    /// 窓の長さ (frame)。
    frames: u32,
    /// 窓の先頭 frame の buffer 内 offset (アレンジ行は 0、 ランチャー区間はその開始 frame)。
    time_offset: u32,
}

impl BufferWindow {
    /// 拍 `beat` の境界の、窓の先頭からの frame (`common::timing::boundary_frame`、負 = 窓より前)。
    fn offset(&self, beat: f64) -> f64 {
        common::timing::boundary_frame(beat - self.playhead_beats, self.samples_per_beat)
    }

    /// 境界がこの窓の frame に落ちるなら、その buffer 内 frame。窓への振り分けも frame で行う — 拍で振り分けて
    /// frame を別に丸めると、窓の端ちょうどの note が「この窓の範囲外の frame」や隣の窓と 1 sample ずれた位置に出る。
    fn frame_in(&self, beat: f64) -> Option<u32> {
        let f = self.offset(beat);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (f >= 0.0 && f < f64::from(self.frames)).then(|| f as u32 + self.time_offset)
    }

    /// この窓で On / chase / Off / 読み上げを出しうる拍の範囲 (**広めに**取る。`None` = 解けないので全件を見る)。
    ///
    /// `offset(b) >= 0` は `b > playhead - 1 sample` を、`offset(b) < frames` は `b < playhead + frames sample` を含意する
    /// ([`common::timing::boundary_frame`] の吸着は 1e-6 sample)。拍の丸め誤差ぶん両端に 2 sample の余白を足す。
    fn beat_span(&self) -> Option<(f64, f64)> {
        let spb = self.samples_per_beat;
        if !(spb.is_finite() && spb > 0.0) {
            return None;
        }
        let margin = 2.0 / spb;
        Some((self.playhead_beats - margin, self.playhead_beats + f64::from(self.frames) / spb + margin))
    }
}

/// 1 窓で見る clip / note の候補の上限 (スタックに置く。超えたら全件を元の並びで見る)。
const MAX_WINDOW_CLIPS: usize = 64;
const MAX_WINDOW_NOTES: usize = 256;

/// `items` のうち拍の範囲 `window` に掛かりうるものを **元の並び順で** `f(位置, 要素)` に渡す。索引が無い / 別の
/// snapshot の索引 / 候補が `N` を超える / 範囲が解けないときは全件を渡す (呼び出し側の判定はどちらでも同じ)。
/// RT 安全: 候補はスタックの配列に置く。
fn for_each_in_window<T, const N: usize>(
    items: &[T],
    ranges: Option<&RangeIndex>,
    window: Option<(f64, f64)>,
    mut f: impl FnMut(usize, &T),
) {
    if let (Some(ranges), Some((lo, hi))) = (ranges.filter(|r| r.built_for(items.len())), window) {
        let mut candidates = [0u32; N];
        if let Some(n) = ranges.overlapping(lo, hi, &mut candidates) {
            for &i in &candidates[..n] {
                if let Some(item) = items.get(i as usize) {
                    f(i as usize, item);
                }
            }
            return;
        }
    }
    for (i, item) in items.iter().enumerate() {
        f(i, item);
    }
}

/// 窓 1 つぶんの **発音台帳** ([`PerTrackState::active_notes`]) の読み書き口。
///
/// 台帳が「送った鍵盤」の SSoT で、Off と鳴らし直しは台帳の鍵盤で出す (r.md #130)。照合は note_id だけ。
/// `seen[i]` は「`notes[i]` がこの窓でもまだ鳴っているべきだと確かめた」印で、窓の終わりに印の無い発音
/// (= 鳴らすべき note がもう無い: 再生中にミュートした / 消した / 後ろへ動かした / clip ごと外れた) は
/// [`Self::sweep`] が窓の先頭で止める。止めないと、その Off は二度と来ず停止まで鳴り続ける。
///
/// RT 安全: 印はスタックの配列、台帳は `ACTIVE_NOTES_CAP` を超えて伸ばさない。
struct Ledger<'a> {
    notes: &'a mut Vec<(u32, u8)>,
    seen: [bool; ACTIVE_NOTES_CAP],
}

impl<'a> Ledger<'a> {
    fn new(notes: &'a mut Vec<(u32, u8)>) -> Self {
        Self { notes, seen: [false; ACTIVE_NOTES_CAP] }
    }

    fn find(&self, note_id: u32) -> Option<usize> {
        self.notes.iter().position(|&(id, _)| id == note_id)
    }

    fn key(&self, i: usize) -> u8 {
        self.notes[i].1
    }

    fn is_full(&self) -> bool {
        self.notes.len() >= ACTIVE_NOTES_CAP
    }

    /// 印を付ける。容量の外 (テストが上限を超えて積んだ分) は印を持たない = 掃除しない側に倒す。
    fn mark(&mut self, i: usize) {
        if let Some(s) = self.seen.get_mut(i) {
            *s = true;
        }
    }

    /// 呼び出し側が [`Self::is_full`] を確かめてから呼ぶ。
    fn push(&mut self, note_id: u32, key: u8) {
        self.notes.push((note_id, key));
        self.mark(self.notes.len() - 1);
    }

    fn remove(&mut self, i: usize) {
        let last = self.notes.len() - 1;
        self.notes.swap_remove(i);
        let moved = self.seen.get(last).copied().unwrap_or(true);
        if let Some(s) = self.seen.get_mut(i) {
            *s = moved;
        }
        if let Some(s) = self.seen.get_mut(last) {
            *s = false;
        }
    }

    /// 印の無い発音を `at` frame で止める。`out` が満杯なら止めずに残す (次の窓 / 停止の一括消音が拾う。
    /// 握りつぶして台帳から外すと stuck note になる)。
    fn sweep(&mut self, out: &mut Vec<TimedNoteEvent>, at: u32) {
        for i in (0..self.notes.len()).rev() {
            if self.seen.get(i).copied().unwrap_or(true) {
                continue;
            }
            if out.len() >= MAX_EVENTS {
                return;
            }
            let (note_id, key) = self.notes[i];
            out.push(TimedNoteEvent { time: at, event: NoteTransition::Off { note_id, key } });
            self.remove(i);
        }
    }
}

/// 1 つの note の On / chase / 鳴らし直し / Off をこの窓で emit する ([`collect_events_for_buffer`] の本体)。
/// `note` は呼び出し側で muted / 長さ 0 / clip 窓外を除外済み。`key` はこの窓で鳴るべき鍵盤
/// (移調込み、範囲外で鳴らさないなら `None`)。
#[allow(clippy::too_many_arguments)]
fn emit_note_events(
    win: BufferWindow,
    clip: &Clip,
    clip_end_beats: f64,
    note: &Note,
    note_id: u32,
    key: Option<u8>,
    out: &mut Vec<TimedNoteEvent>,
    ledger: &mut Ledger<'_>,
) {
    // beat-domain で note の絶対 beat 位置を求める。 Off は clip 末端
    // で clamp (= 旧 sample-domain ロジックと同 idiom)。
    let on_abs_beat = clip.content_to_song_beat(note.start_beat);
    let raw_off_abs_beat = clip.content_to_song_beat(note.start_beat + note.duration_beats);
    let off_abs_beat = raw_off_abs_beat.min(clip_end_beats);
    let velocity = f64::from(note.velocity) / 127.0;
    let active = ledger.find(note_id);

    if let Some(on_frame) = win.frame_in(on_abs_beat) {
        // この窓で発音が始まる。同じ note の古い発音 (再生中に後ろへ動かした等) が台帳に残っていれば、
        // 台帳の鍵盤で先に止める (同時刻は Off → On の順に並ぶ)。
        if let Some(i) = active {
            if out.len() >= MAX_EVENTS {
                ledger.mark(i);
                return;
            }
            out.push(TimedNoteEvent { time: on_frame, event: NoteTransition::Off { note_id, key: ledger.key(i) } });
            ledger.remove(i);
        }
        // RT-safe: 容量超過分は drop し `Vec` 再確保を避ける。 On を drop したら台帳にも積まず
        // 整合を保つ (= 後で flush しても残らない)。
        let Some(key) = key else { return };
        if out.len() >= MAX_EVENTS || ledger.is_full() {
            return;
        }
        out.push(TimedNoteEvent { time: on_frame, event: NoteTransition::On { note_id, key, velocity } });
        ledger.push(note_id, key);
    } else if win.offset(on_abs_beat) < 0.0 {
        // 残りが 1 sample 未満の note は追わない / 鳴らし直さない (鳴らしても 1 sample の断片)。残りが
        // 1 sample 以上なら Off は `boundary_frame` で frame 1 以降に落ちるので、同時刻の Off → On
        // (`collect_events_for_buffer` 末尾の sort 契約) で On が Off の後に残る stuck note にもならない。
        let remains = (off_abs_beat - win.playhead_beats) * win.samples_per_beat >= 1.0 - 1e-6;
        match (active, key) {
            // 終わりかけ: この窓の Off (下、台帳の鍵盤) が止める。Off を取りこぼしていた発音は印が無いので
            // 窓の終わりの掃除が止める。
            (Some(_), _) if !remains => {}
            // 鳴っている鍵盤のまま続く (定常再生)。
            (Some(i), Some(k)) if ledger.key(i) == k => ledger.mark(i),
            // バスが満杯なら鳴らし直しは次の窓に回す (印を付けて掃除させない)。
            (Some(i), _) if out.len() + 2 > MAX_EVENTS => ledger.mark(i),
            // r.md #130 (確定仕様 Q3): 鳴っている鍵盤と今鳴るべき鍵盤が違う (再生中に音程 / 移調 / 追従が
            // 変わった) → 台帳の鍵盤を窓の先頭で止め、新しい鍵盤で鳴らし直す (範囲外になったら止めるだけ)。
            (Some(i), next) => {
                out.push(TimedNoteEvent {
                    time: win.time_offset,
                    event: NoteTransition::Off { note_id, key: ledger.key(i) },
                });
                ledger.remove(i);
                if let Some(k) = next {
                    out.push(TimedNoteEvent { time: win.time_offset, event: NoteTransition::On { note_id, key: k, velocity } });
                    ledger.push(note_id, k);
                }
            }
            // r.md #120 (note chase): 窓の先頭を **跨いで鳴っているはずなのに台帳に無い** note は、その場で
            // On を出す。「台帳に無い」は Play の起点 / seek / loop wrap / ランチャー区間の切れ目 (どれも台帳を
            // 空にする) と、範囲書き出しの走査開始を全部同じ条件で拾う (= 経路ごとの「chase して」 flag を
            // 配らない)。定常再生中は On を出した時点で台帳に入るので二度は鳴らない。
            (None, Some(k)) if remains && out.len() < MAX_EVENTS && !ledger.is_full() => {
                out.push(TimedNoteEvent { time: win.time_offset, event: NoteTransition::On { note_id, key: k, velocity } });
                ledger.push(note_id, k);
            }
            (None, _) => {}
        }
    }
    if off_abs_beat > on_abs_beat
        && let Some(off_frame) = win.frame_in(off_abs_beat)
        && let Some(i) = ledger.find(note_id)
    {
        // RT-safe: `out` 容量超過時は Off を emit せず、台帳からも除かない (= 後続の Stop / loop-wrap flush で
        // NoteOff が送られ note が残らない)。push できない Off を握りつぶして追跡解除すると stuck note になる。
        if out.len() >= MAX_EVENTS {
            return;
        }
        out.push(TimedNoteEvent { time: off_frame, event: NoteTransition::Off { note_id, key: ledger.key(i) } });
        ledger.remove(i);
    }
}

/// Walk every clip on `track_idx` and emit `On` / `Off` events whose frame (`common::timing::boundary_frame`)
/// falls inside the buffer `[0, frames)`.
///
/// Phase 5 follow-up (MIDI tempo follow): beat-domain comparison。 caller の
/// engine が SongTempo lane を評価した `current_bpm` と、 累積 `playhead_beats`
/// を渡す。 sample-domain の `playhead: u64` は使わず (= 変動 tempo で sample
/// ↔ beat の線形変換が破綻するため)、 buffer 内の time offset (= note の sample
/// 位置) は `current_bpm` で beat → sample 換算する。 sub-buffer の tempo
/// 変化は scope 外 (= 1 buffer 内 constant tempo、 ~5..20ms なので user 体感 OK)。
///
/// `active_notes` is the audio worker's running set of pitches currently
/// sounding for this track — the caller maintains it across buffers so it
/// can flush stuck notes on Stop / loop wrap.
///
/// RT-safe: pushes into the caller-provided `out` (pre-allocated capacity)
/// and uses `sort_unstable_by_key` (in-place pdqsort).
/// r.md #87 (クリップランチャー): `clips` が **イベント源**、`time_offset` が
/// **buffer 内の書き出し位置**。アレンジ行は `&track.clips` と `0` を渡す
/// (従来と完全に同じ挙動)。ランチャー行は「セル 1 つのスライス」と、
/// ループ端で割った区間の開始 frame を渡す。`playhead_beats` はその区間の実効拍。
/// 割り方は `crate::launcher::render` が唯一の口 (ここには分岐を持ち込まない)。
///
/// r.md #130: 鍵盤は `note.pitch + transpose` (`common::transpose::sounding_key`、範囲外は鳴らさない)。
/// `transpose` は呼び出し側が buffer 頭で解いた曲の移調量で、追従しないトラックは 0。値が変わった窓では、
/// 鳴っている note を台帳の鍵盤で止めて新しい鍵盤で鳴らし直す ([`emit_note_events`])。窓の終わりに、鳴らすべき
/// note が見つからなかった台帳の発音を窓の先頭で止める ([`Ledger::sweep`])。
#[allow(clippy::too_many_arguments)]
pub fn collect_events_for_buffer(
    song: Option<&Song>,
    // `song` と同じ snapshot の索引 (窓に掛かる clip / note / event だけを見るため)。
    index: &SongIndex,
    track_idx: u32,
    clips: &[Clip],
    // `clips` の区間索引 (アレンジ行は `SongIndex::track_clips`、ランチャーのセル 1 つは `None`)。
    clip_ranges: Option<&RangeIndex>,
    sample_rate: u32,
    playhead_beats: f64,
    current_bpm: f32,
    frames: u32,
    time_offset: u32,
    transpose: i32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<(u32, u8)>,
) {
    let Some(song) = song else { return };
    if song.tracks.get(track_idx as usize).is_none() || current_bpm <= 0.0 {
        return;
    }

    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    let win = BufferWindow { playhead_beats, samples_per_beat, frames, time_offset };
    let window = win.beat_span();
    let mut ledger = Ledger::new(active_notes);

    // note_id は `(clip.id, note.id)` からの決定論的導出
    // (`common::plugin_metadata::sing_note_id`)。daw_gui の `sync_vocal_metadata` が
    // **同じ関数**で同じ値を flush するので、clip の追加 / 削除 / 並べ替え / muted で
    // 番号がずれない (旧「track 内通し index」の欠陥、アーキ不変条件 1)。
    // 通し番号の bookkeeping はもう要らない (どの clip を skip しても影響しない)。
    for_each_in_window::<_, MAX_WINDOW_CLIPS>(clips, clip_ranges, window, |_, clip| {
        // muted clip は全 note を skip。
        if clip.muted {
            return;
        }

        if clip.length_beats <= 0.0 {
            return;
        }
        let clip_end_beats = clip.start_beat + clip.length_beats;
        // clip が窓の外なら skip。note の On / Off と **同じ frame の規則** で判定する — 拍で判定すると、窓の端
        // ちょうどで終わる clip は「この窓では Off の frame が窓の外、次の窓では拍が clip の外」になり、Off が一度も
        // 出ない (stuck note)。終端が窓の frame 0 に落ちる clip は通す (その Off はこの窓で出る)。
        if win.offset(clip_end_beats) < 0.0 || win.offset(clip.start_beat) >= f64::from(frames) {
            return;
        }
        // v6 linked clip: notes は Song.clip_contents から取り出す。
        // 共有 clip 群は同じ content から同じ notes を見るので、 別々の
        // 配置位置 (clip.start_beat) で同じ内容が再生される。
        let notes: &[Note] = song.clip_contents.get(&clip.content_id).and_then(|c| c.notes()).unwrap_or(&[]);
        // r.md #44: clip は content への窓。 鳴らす note は content-local 拍で
        // `[content_offset_beats, +length_beats)` に **発音開始が入る** ものだけ
        // (= 左端 trim で隠れた note は鳴らない)。 linked clip は content を
        // 共有するが窓は clip ごとに独立する。
        let (win_start, win_end) = clip.content_window();

        // 非重なり不変条件 (`Track::clips`) が入ったので、同じトラックで 2 つの clip が
        // 同時に鳴ることは無い。 これに依存しているのが下の `active_notes` —
        // pitch を refcount せず `swap_remove` するので、重なった clip が同ピッチを
        // 鳴らすと Off が 1 本だけ外れて早切れ / stuck になる
        // (`docs/plan_range_selection.md` §10)。 重なりを許す方向へ戻すなら、
        // ここを (pitch, clip) の多重集合にすること。
        let local = window.map(|(lo, hi)| (clip.song_to_content_beat(lo), clip.song_to_content_beat(hi)));
        let note_ranges = index.content_ranges(clip.content_id);
        for_each_in_window::<_, MAX_WINDOW_NOTES>(notes, note_ranges, local, |_, note| {
            let note_id = common::plugin_metadata::sing_note_id(clip.id, note.id);
            // muted note は On/Off を一切 emit しない (On を出さないので
            // stuck note にならない)。note_id は note.id 由来なので影響を受けない。
            if note.muted {
                return;
            }
            if note.duration_beats <= 0.0 {
                return;
            }
            // Skip notes whose On is outside the clip — otherwise we could
            // emit On but lose Off to clamping, leaving a stuck note.
            if note.start_beat < win_start || note.start_beat >= win_end {
                return;
            }
            let key = common::transpose::sounding_key(note.pitch, transpose);
            emit_note_events(win, clip, clip_end_beats, note, note_id, key, out, &mut ledger);
        });
    });
    ledger.sweep(out, time_offset);

    // (talk) 読み上げトリガ (`docs/plan_voicevox_talk.md` §3.4)。VOICEVOX デバイス付き
    // トラックの `ClipContent::Text` の各 TextEvent 開始位置で、合成 note_on を発火する。
    // note_id = `talk_event_id(clip.id, event_index)` (= builtin の note_offsets と対応する
    // high band id)。builtin は wav 終端で自動 drain するので note_off は不要 (= active_notes
    // にも積まない)。空テキストは flush 側 (sync_vocal_metadata) と同条件で skip して
    // event_id の対応を保つ。歌唱 MIDI clip と talk Text clip が混在しても、
    // note_id (= `sing_note_id`、`[0, TALK_EVENT_ID_BASE)`) と event_id (= high band) は
    // 衝突しない。
    if index.is_voicevox_vocal(track_idx as usize) {
        // 発火する event は clip の窓 `[start, start + length)` の中で始まるので、窓に掛からない clip は何も出さない。
        for_each_in_window::<_, MAX_WINDOW_CLIPS>(clips, clip_ranges, window, |_, clip| {
            // muted な Text(読み上げ) clip は talk note_on を発火しない。
            if clip.muted {
                return;
            }
            let Some(events) = song.clip_contents.get(&clip.content_id).and_then(|c| c.text_events()) else {
                return;
            };
            // r.md #44: 読み上げも clip の窓の中で始まる event だけ発火する。
            let (win_start, win_end) = clip.content_window();
            let local = window.map(|(lo, hi)| (clip.song_to_content_beat(lo), clip.song_to_content_beat(hi)));
            let event_ranges = index.content_ranges(clip.content_id);
            for_each_in_window::<_, MAX_WINDOW_NOTES>(events, event_ranges, local, |event_index, ev| {
                if ev.text.is_empty() {
                    return;
                }
                if ev.event_start_in_clip_beats < win_start || ev.event_start_in_clip_beats >= win_end {
                    return;
                }
                let on_abs_beat = clip.content_to_song_beat(ev.event_start_in_clip_beats);
                if let Some(time) = win.frame_in(on_abs_beat)
                    && out.len() < MAX_EVENTS
                {
                    out.push(TimedNoteEvent {
                        time,
                        event: NoteTransition::On {
                            note_id: common::plugin_metadata::talk_event_id(clip.id, event_index as u32),
                            key: 0,
                            velocity: 1.0,
                        },
                    });
                }
            });
        });
    }

    // CLAP requires in-events sorted by time. At equal times, Off must come
    // before On so a re-attack at the same frame doesn't drop because the
    // synth saw On→Off in the same buffer.
    out.sort_unstable_by_key(|e| {
        let priority: u8 = match e.event {
            NoteTransition::Off { .. } => 0,
            NoteTransition::On { .. } => 1,
        };
        (e.time, priority)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{ClipContent, MidiContent, Track};

    /// テスト用の薄いラッパ: アレンジ行 (= その track の `clips` 全部) を
    /// buffer 先頭から集める (`clips` = 源、`time_offset` = 0)。
    /// ランチャー行の分割は `crate::launcher::render` 側でテストする。
    #[allow(clippy::too_many_arguments)]
    fn collect(
        song: Option<&Song>,
        track_idx: u32,
        sample_rate: u32,
        playhead_beats: f64,
        current_bpm: f32,
        frames: u32,
        out: &mut Vec<TimedNoteEvent>,
        active_notes: &mut Vec<(u32, u8)>,
    ) {
        collect_transposed(song, track_idx, sample_rate, playhead_beats, current_bpm, frames, 0, out, active_notes);
    }

    /// [`collect`] の移調つき版。
    #[allow(clippy::too_many_arguments)]
    fn collect_transposed(
        song: Option<&Song>,
        track_idx: u32,
        sample_rate: u32,
        playhead_beats: f64,
        current_bpm: f32,
        frames: u32,
        transpose: i32,
        out: &mut Vec<TimedNoteEvent>,
        active_notes: &mut Vec<(u32, u8)>,
    ) {
        let empty: &[Clip] = &[];
        let clips = song
            .and_then(|s| s.tracks.get(track_idx as usize))
            .map_or(empty, |t| t.clips.as_slice());
        let index = song.map_or_else(SongIndex::default, SongIndex::build);
        collect_events_for_buffer(
            song,
            &index,
            track_idx,
            clips,
            index.track_clips(track_idx as usize),
            sample_rate,
            playhead_beats,
            current_bpm,
            frames,
            0,
            transpose,
            out,
            active_notes,
        );
    }

    /// v23 single-chain: `Track` の `legacy_*` migration fields は `common`
    /// に `pub(crate)` で閉じているので、 downstream の test では
    /// `Track { .., ..Track::default() }` が E0451。 `Track::default()` を
    /// mutator で埋める helper で回避する。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    fn one_note_song(start_beat: f64, duration_beats: f64, pitch: u8) -> Song {
        // v6: notes は Song.clip_contents に置く。 inline の `notes:` は
        // legacy field (空) のままで、 ensure_clip_contents が migrate する
        // 想定だが、 ここでは直接 clip_contents を構築して migrate を挟まず
        // production と同形にする。
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    id: 1,
                    start_beat,
                    duration_beats,
                    pitch,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                }],
                next_note_id: 2,
            }),
        );
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 8.0,
                content_id,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
        }));
        song
    }

    /// 120 BPM, 48 kHz: samples_per_beat = 24000.
    const SR: u32 = 48000;
    const SPB: u64 = 24_000;

    /// engine と同じく buffer ごとに拍を足して (`+=`、浮動小数の誤差が溜まる) 進めても、clip 末端で切れる note の
    /// Off は必ず 1 回だけ出る。clip の範囲外判定だけ拍のままだと、末端が buffer の境界ちょうどに乗ったとき
    /// 「前の buffer では Off の frame が窓の外、次の buffer では拍が clip の外」になって Off が出ず、鳴りっぱなしになった。
    #[test]
    fn clip_末端で切れる_note_の_off_は_buffer_の切り方に依らず出る() {
        let song = one_note_song(7.0, 5.0, 60); // clip は 0..8 拍、note の Off は clip 末端 (8 拍) で切れる
        for frames in [256u32, 441, 480, 512, 1024] {
            let mut active = Vec::new();
            let (mut ons, mut offs) = (0, 0);
            let mut playhead = 0.0f64;
            while playhead < 9.0 {
                let mut out = Vec::new();
                collect(Some(&song), 0, SR, playhead, 120.0, frames, &mut out, &mut active);
                for e in &out {
                    assert!(e.time < frames, "frames {frames}: buffer の外の frame {e:?}");
                    match e.event {
                        NoteTransition::On { .. } => ons += 1,
                        NoteTransition::Off { .. } => offs += 1,
                    }
                }
                playhead += f64::from(frames) / SPB as f64;
            }
            assert_eq!((ons, offs), (1, 1), "frames {frames}: On / Off は 1 回ずつ");
            assert!(active.is_empty(), "frames {frames}: 鳴りっぱなしの note が残った");
        }
    }

    #[test]
    fn note_starting_at_buffer_zero_emits_on_at_time_zero() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert_eq!(active, vec![(common::plugin_metadata::sing_note_id(1, 1), 60)]);
    }

    /// muted clip は note イベントを 1 つも emit しない。
    #[test]
    fn muted_clip_emits_no_note_events() {
        let mut song = one_note_song(0.0, 1.0, 60);
        song.tracks[0].clips[0].muted = true;
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty(), "muted clip must emit no events");
        assert!(active.is_empty());
    }

    /// muted note を skip しても、 同 clip 内の sibling note の `note_id`
    /// (= `sing_note_id(clip.id, note.id)`) はずれない (builtin VOICEVOX の
    /// note_id ↔ 合成 wav frame offset 対応を壊さないための不変条件)。
    #[test]
    fn muted_note_skipped_but_sibling_keeps_stable_note_id() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&cid) {
            // idx 0 = mute、 idx 1 = 鳴る (pitch 64)。
            m.notes[0].muted = true;
            m.notes.push(Note {
                id: 2,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        }
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        let ons: Vec<_> = out
            .iter()
            .filter(|e| matches!(e.event, NoteTransition::On { .. }))
            .collect();
        assert_eq!(ons.len(), 1, "only the unmuted sibling emits On");
        let clip_id = song.tracks[0].clips[0].id;
        match ons[0].event {
            NoteTransition::On { note_id, key, .. } => {
                assert_eq!(key, 64);
                assert_eq!(
                    note_id,
                    common::plugin_metadata::sing_note_id(clip_id, 2),
                    "unmuted sibling keeps its stable note_id (clip.id, note.id)"
                );
            }
            NoteTransition::Off { .. } => unreachable!(),
        }
        assert_eq!(active, vec![(common::plugin_metadata::sing_note_id(clip_id, 2), 64)]);
    }

    /// r.md #75 が直した欠陥の直接の回帰テスト: clip の**先頭に 1 音足しても**、
    /// 既存 note の `note_id` は変わらない (旧「通し index」では以降が全部ずれ、
    /// builtin VOICEVOX のフレーズキャッシュも停止中プレビューも壊れていた)。
    #[test]
    fn note_id_is_unaffected_by_inserting_a_note_before_it() {
        let collect_id_for_pitch = |song: &Song, pitch: u8| -> u32 {
            let mut out = Vec::new();
            let mut active = Vec::new();
            collect(Some(song), 0, SR, 0.0, 120.0, 4096, &mut out, &mut active);
            out.iter()
                .find_map(|e| match e.event {
                    NoteTransition::On { note_id, key, .. } if key == pitch => Some(note_id),
                    _ => None,
                })
                .expect("note on for pitch")
        };

        let mut song = one_note_song(0.0, 1.0, 60);
        let before = collect_id_for_pitch(&song, 60);

        // 先頭 (時間的にも Vec 上も前) に別の note を足す。
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&cid) {
            m.notes.insert(
                0,
                Note {
                    id: 7,
                    start_beat: 0.0,
                    duration_beats: 0.25,
                    pitch: 48,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                },
            );
            m.next_note_id = 8;
        }
        let after = collect_id_for_pitch(&song, 60);
        assert_eq!(before, after, "既存 note の note_id は前挿入で変わらない");
    }

    #[test]
    fn note_off_emitted_in_buffer_containing_end() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = vec![(common::plugin_metadata::sing_note_id(1, 1), 60u8)];
        // SPB-100 samples ≈ beat 0.9958 (= 1 beat 直前)、 buffer 200 frames で
        // beat 1.0 の note off を捕まえる。 sample→beat 換算は `samples / SPB`。
        let playhead_beats = (SPB - 100) as f64 / SPB as f64;
        collect(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            200,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60, .. }));
        assert!(active.is_empty(), "active set must drop the off note");
    }

    #[test]
    fn note_entirely_inside_buffer_emits_on_then_off() {
        let song = one_note_song(0.0, 0.01, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert!(matches!(out[1].event, NoteTransition::Off { key: 60, .. }));
        assert!(out[0].time < out[1].time);
        assert!(active.is_empty());
    }

    #[test]
    fn chord_emits_two_ons_at_same_time() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents
            .get_mut(&cid)
            .unwrap()
            .notes_mut()
            .expect("Midi variant")
            .push(Note {
                id: 2,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 2);
        for e in &out {
            assert_eq!(e.time, 0);
            assert!(matches!(e.event, NoteTransition::On { .. }));
        }
        let mut keys: Vec<u8> = active.iter().map(|&(_, k)| k).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![60, 64]);
    }

    #[test]
    fn no_song_returns_empty() {
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            None,
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    /// r.md #120: note の途中から再生を始めても (= buffer 先頭が note を跨ぐ) その note は
    /// 鳴る。 On は区間先頭の frame に出て追跡集合へ入り、 次の buffer では二度と出ない。
    /// Off は本来の位置に出る。
    #[test]
    fn note_straddling_buffer_start_is_chased_once() {
        let song = one_note_song(1.0, 2.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        // 1.5 拍目 (= note の真ん中) から 1 buffer。
        collect(Some(&song), 0, SR, 1.5, 120.0, 1024, &mut out, &mut active);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert_eq!(active.len(), 1);

        // 追跡集合に居る間は再度 chase しない。
        out.clear();
        collect(Some(&song), 0, SR, 1.5 + 1024.0 / SPB as f64, 120.0, 1024, &mut out, &mut active);
        assert!(out.is_empty(), "定常再生中に On を出し直さない: {out:?}");

        // Off は本来の位置 (3.0 拍) に出て追跡集合から外れる。
        out.clear();
        collect(Some(&song), 0, SR, 3.0 - 100.0 / SPB as f64, 120.0, 200, &mut out, &mut active);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 100);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60, .. }));
        assert!(active.is_empty());

        // ランチャー区間 (`time_offset > 0`) では区間の開始 frame に出る。
        out.clear();
        let index = SongIndex::build(&song);
        collect_events_for_buffer(
            Some(&song), &index, 0, &song.tracks[0].clips, index.track_clips(0), SR, 1.5, 120.0, 512, 512, 0,
            &mut out, &mut active,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 512);
    }

    /// r.md #120: 残りが 1 sample 未満の note は追わない (同 frame の Off → On で stuck になる)。
    #[test]
    fn note_ending_at_buffer_start_is_not_chased() {
        let song = one_note_song(1.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(Some(&song), 0, SR, 2.0 - 0.5 / SPB as f64, 120.0, 1024, &mut out, &mut active);
        assert!(out.iter().all(|e| matches!(e.event, NoteTransition::Off { .. })), "{out:?}");
        assert!(active.is_empty());
    }

    /// `(time, On か, key)` の列。
    fn transitions(out: &[TimedNoteEvent]) -> Vec<(u32, bool, u8)> {
        out.iter()
            .map(|e| match e.event {
                NoteTransition::On { key, .. } => (e.time, true, key),
                NoteTransition::Off { key, .. } => (e.time, false, key),
            })
            .collect()
    }

    /// r.md #130 (確定仕様 Q3 / Q4): 再生中に移調が変わると、鳴っている note は **台帳の鍵盤で** 止まり新しい
    /// 鍵盤で鳴り直す。範囲外 (127 超) へ出たら止まるだけで、戻れば鳴り直す。最後の Off も台帳の鍵盤で出て、
    /// 台帳に何も残らない。
    #[test]
    fn 再生中に移調が変わると台帳の鍵盤で止めて新しい鍵盤で鳴らし直す() {
        let song = one_note_song(0.0, 4.0, 120);
        let id = common::plugin_metadata::sing_note_id(1, 1);
        let mut active = Vec::new();
        let mut step = |beat: f64, frames: u32, transpose: i32| {
            let mut out = Vec::new();
            collect_transposed(Some(&song), 0, SR, beat, 120.0, frames, transpose, &mut out, &mut active);
            (transitions(&out), active.clone())
        };
        assert_eq!(step(0.0, 512, 0), (vec![(0, true, 120)], vec![(id, 120)]));
        assert_eq!(step(1.0, 512, 2), (vec![(0, false, 120), (0, true, 122)], vec![(id, 122)]), "鳴らし直し");
        assert_eq!(step(1.5, 512, 2), (vec![], vec![(id, 122)]), "同じ移調のままなら何も出さない");
        assert_eq!(step(2.0, 512, 12), (vec![(0, false, 122)], vec![]), "範囲外へ出たら止めるだけ");
        assert_eq!(step(2.5, 512, 12), (vec![], vec![]), "範囲外の間は鳴らさない");
        assert_eq!(step(3.0, 512, -1), (vec![(0, true, 119)], vec![(id, 119)]), "範囲に戻れば途中から鳴る");
        let end = 4.0 - 100.0 / SPB as f64;
        assert_eq!(step(end, 200, -1), (vec![(100, false, 119)], vec![]), "Off は台帳の鍵盤 (本来の位置)");
    }

    /// r.md #130 が一緒に直した既存の欠陥: 再生中にノートの音程を変えると (↑↓ / スケール補正)、旧鍵盤を止めて
    /// 新しい鍵盤で鳴らし直す。以前は Off を `note.pitch` から計算し直していたので、旧鍵盤が停止まで鳴り残った。
    #[test]
    fn 再生中にノートの音程を変えても旧鍵盤が残らない() {
        let mut song = one_note_song(0.0, 4.0, 60);
        let mut active = Vec::new();
        let mut out = Vec::new();
        collect(Some(&song), 0, SR, 0.0, 120.0, 512, &mut out, &mut active);
        assert_eq!(transitions(&out), vec![(0, true, 60)]);

        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents.get_mut(&cid).unwrap().notes_mut().expect("Midi")[0].pitch = 64;
        out.clear();
        collect(Some(&song), 0, SR, 1.0, 120.0, 512, &mut out, &mut active);
        assert_eq!(transitions(&out), vec![(0, false, 60), (0, true, 64)]);

        out.clear();
        collect(Some(&song), 0, SR, 4.0 - 100.0 / SPB as f64, 120.0, 200, &mut out, &mut active);
        assert_eq!(transitions(&out), vec![(100, false, 64)]);
        assert!(active.is_empty(), "鳴り残りが無い: {active:?}");
    }

    /// 再生中に鳴っている note をミュート / 削除 / clip ごとミュートすると、次の窓の先頭で台帳の鍵盤が止まる。
    /// 止めないと、その note の Off は二度と来ず停止まで鳴り続ける。
    #[test]
    fn 再生中に鳴らなくなった_note_は次の窓の先頭で止まる() {
        let edits: [(&str, fn(&mut Song)); 3] = [
            ("note をミュート", |s| {
                let cid = s.tracks[0].clips[0].content_id;
                s.clip_contents.get_mut(&cid).unwrap().notes_mut().expect("Midi")[0].muted = true;
            }),
            ("note を削除", |s| {
                let cid = s.tracks[0].clips[0].content_id;
                s.clip_contents.get_mut(&cid).unwrap().notes_mut().expect("Midi").clear();
            }),
            ("clip をミュート", |s| s.tracks[0].clips[0].muted = true),
        ];
        for (label, edit) in edits {
            let mut song = one_note_song(0.0, 4.0, 60);
            let mut active = Vec::new();
            let mut out = Vec::new();
            collect(Some(&song), 0, SR, 0.0, 120.0, 512, &mut out, &mut active);
            edit(&mut song);
            out.clear();
            collect(Some(&song), 0, SR, 1.0, 120.0, 512, &mut out, &mut active);
            assert_eq!(transitions(&out), vec![(0, false, 60)], "{label}");
            assert!(active.is_empty(), "{label}: {active:?}");
        }
    }

    #[test]
    fn note_outside_buffer_emits_nothing() {
        let song = one_note_song(2.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1000,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn note_extending_past_clip_end_is_clamped() {
        let mut song = one_note_song(7.0, 4.0, 60);
        song.tracks[0].clips[0].length_beats = 8.0;
        let playhead = 8 * SPB - 100;
        let frames = 200u32;
        let mut out = Vec::new();
        let mut active = vec![(common::plugin_metadata::sing_note_id(1, 1), 60u8)];
        // playhead (samples) を beat に変換: samples / SPB。
        let playhead_beats = playhead as f64 / SPB as f64;
        collect(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            frames,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60, .. }));
        assert!(active.is_empty());
    }

    /// r.md #44: clip は content への窓。左端を trim (= `content_offset_beats` を
    /// 進める) した clip は、窓より前の note を鳴らさず、窓内の note は
    /// **song 上の同じ位置** で鳴る (= content が動いていない証拠)。
    #[test]
    fn left_trimmed_clip_hides_notes_before_the_window_and_keeps_the_rest_in_place() {
        // content: note@0 (1 拍) と note@2 (1 拍)。clip は [0,4) を見せている。
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents
            .get_mut(&cid)
            .unwrap()
            .notes_mut()
            .expect("Midi variant")
            .push(Note {
                id: 2,
                start_beat: 2.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        // 左端を 2 拍 trim: start 0→2 / length 4→2 / 窓 offset 0→2。
        {
            let clip = &mut song.tracks[0].clips[0];
            clip.start_beat = 2.0;
            clip.length_beats = 2.0;
            clip.content_offset_beats = 2.0;
        }
        // 窓の前 (content 0 拍 = song 0 拍) は鳴らない。
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(Some(&song), 0, SR, 0.0, 120.0, 1024, &mut out, &mut active);
        assert!(out.is_empty(), "trim で隠した note は発音しない: {out:?}");
        // 窓内の note は song 2 拍のまま (= content が動いていない)。
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(Some(&song), 0, SR, 2.0, 120.0, 1024, &mut out, &mut active);
        assert!(
            matches!(out.first().map(|e| &e.event), Some(NoteTransition::On { key: 64, .. })),
            "窓内の note は song 上の元の位置で鳴る: {out:?}"
        );
        assert_eq!(out[0].time, 0, "playhead=2 拍ちょうどで発音");
    }

    /// linked clip (= 同 `content_id`) が別々の窓を持てること。同じ content から
    /// 片方は前半 note を、もう片方は後半 note を鳴らす。
    #[test]
    fn linked_clips_sound_their_own_windows_of_the_shared_content() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents
            .get_mut(&cid)
            .unwrap()
            .notes_mut()
            .expect("Midi variant")
            .push(Note {
                id: 2,
                start_beat: 2.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        {
            let clip = &mut song.tracks[0].clips[0];
            clip.start_beat = 0.0;
            clip.length_beats = 2.0;
            clip.content_offset_beats = 0.0;
        }
        // content を共有する 2 本目: 窓は [2,4) を song 8 拍に置く。
        let mut linked = song.tracks[0].clips[0].clone();
        linked.id = 2;
        linked.start_beat = 8.0;
        linked.length_beats = 2.0;
        linked.content_offset_beats = 2.0;
        song.tracks[0].clips.push(linked);

        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(Some(&song), 0, SR, 0.0, 120.0, 1024, &mut out, &mut active);
        assert!(
            matches!(out.first().map(|e| &e.event), Some(NoteTransition::On { key: 60, .. })),
            "clip 1 の窓は前半 note だけ: {out:?}"
        );

        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(Some(&song), 0, SR, 8.0, 120.0, 1024, &mut out, &mut active);
        assert!(
            matches!(out.first().map(|e| &e.event), Some(NoteTransition::On { key: 64, .. })),
            "clip 2 の窓は後半 note だけを 8 拍で鳴らす: {out:?}"
        );
    }

    #[test]
    fn note_past_clip_end_is_skipped_entirely() {
        let mut song = one_note_song(10.0, 1.0, 60);
        song.tracks[0].clips[0].length_beats = 4.0;
        let mut out = Vec::new();
        let mut active = Vec::new();
        let playhead_beats = (10 * SPB - 100) as f64 / SPB as f64;
        collect(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            200,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn output_is_sorted_with_off_before_on_at_same_time() {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note {
                        id: 1,
                        start_beat: 0.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                        muted: false,
                    },
                    Note {
                        id: 2,
                        start_beat: 1.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                        muted: false,
                    },
                ],
                next_note_id: 3,
            }),
        );
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
        }));
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            (2 * SPB) as u32,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].event, NoteTransition::On { .. }));
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[1].event, NoteTransition::Off { .. }));
        assert!(matches!(out[2].event, NoteTransition::On { .. }));
        assert_eq!(out[1].time, out[2].time);
    }
}
