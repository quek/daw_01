// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! MIDI (SMF) import — `.mid` / `.midi` / `.smf` / `.kar` / `.rmi` を読んで
//! daw_01 の `ClipContent::Midi` に入る形へ変換する (r.md #66)。
//! 設計正本は [`docs/plan_midi_import.md`](../../docs/plan_midi_import.md)、
//! 書き出し側 (`midi_export.rs`) の逆写像。
//!
//! このモジュールは **Song を触らない純粋な変換**に閉じる (配置 / track 生成 /
//! テンポ採用は `handler::media::action_import_midi`)。返す `Note::start_beat` は
//! **SMF tick 0 を原点とする content-local 拍**で、clip 側の
//! `content_offset_beats` (= 「clip は content の窓」 v32 / r.md #44) と組み合わせて
//! song 座標へ写す。
//!
//! 仕様上の判断 (詳細と根拠は plan 参照):
//! - velocity 0 の NoteOn は NoteOff (midly は変換しない)
//! - 同 (channel, key) の On/Off 対応は FIFO (Ardour `FirstOnFirstOff` と同じ)
//! - Off の来ないノートは track 末尾まで伸ばす / 孤立 Off は無視
//! - 1 SMF track に複数 channel が混在したら channel ごとに分割する
//!   (`Note` に channel が無いので、分割しないと別楽器が 1 content に混ざる)
//! - 同一ピッチの重なりはモデル不変条件 (`resolve_note_overlaps`) で解消する
//! - Lyric / Text meta は `Note.lyric` へ (VOICEVOX 歌唱にそのまま使える)

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::path::Path;

use midly::{Format, MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

use common::model::Note;

/// D&D / file dialog が受け付ける MIDI 拡張子の SSoT。`rmi` (RMID) は midly が
/// 先頭 RIFF チャンクを剥がして読む (`midly-0.5.3/src/smf.rs:262-267`)。
pub const SUPPORTED_MIDI_EXTENSIONS: &[&str] = &["mid", "midi", "smf", "kar", "rmi"];

// 防御上限。壊れた / 悪意ある SMF で GUI が固まったり、`LoadSong` が wire 上限
// (`common/src/wire.rs` の 16MB) を超えて子プロセスに曲が届かなくなるのを防ぐ。
// 取り込んだ物はすべて Song に載る (= undo snapshot の clone と LoadSong を通る)
// ので、「ファイルが読めるか」ではなく「Song に載せてよい量か」で切る。
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// ノート数。1 ノート ≒ 30 bytes (bincode) なので 10 万で ~3MB。大編成のオーケストラ
/// スコアでも数万ノートなので実用上は当たらない。
const MAX_NOTES: usize = 100_000;
/// tempo breakpoint は 1 点 = `AutomationPoint` 1 個で Song に載る。
/// 1/16 拍解像度のテンポランプを長尺で書き出しても 1 万点程度。
const MAX_TEMPO_POINTS: usize = 20_000;
/// 歌詞 meta は最終的にノート数までしか使われない (解析中の一時確保を抑える)。
const MAX_LYRIC_METAS: usize = MAX_NOTES;
/// meta テキスト (track 名 / 歌詞) 1 個あたりの上限バイト数。
const MAX_META_TEXT_BYTES: usize = 512;
/// 取り込む拍範囲の上限 (200 BPM で ~8 時間)。SMF の delta は 1 個で最大
/// 0x0FFF_FFFF tick 進むので、壊れたファイルが `Song.length_beats` を
/// 数億拍に伸ばして以後の `TempoMap` 構築を破綻させるのを防ぐ。
const MAX_SPAN_BEATS: f64 = 100_000.0;

/// 拡張子で MIDI ファイルを判定する (`import_image::is_supported_extension` と同流儀)。
#[must_use]
pub fn looks_like_midi(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            SUPPORTED_MIDI_EXTENSIONS
                .iter()
                .any(|k| ext.eq_ignore_ascii_case(k))
        })
}

#[derive(Debug)]
pub enum MidiImportError {
    /// ファイルが開けない / 読めない。
    Io(std::io::Error),
    /// midly が SMF として解釈できなかった。
    Parse(midly::Error),
    TooLarge {
        bytes: u64,
    },
    TooManyNotes {
        notes: usize,
    },
    TooManyTempoChanges {
        points: usize,
    },
    TooManyLyrics {
        count: usize,
    },
    /// 曲の長さが常識的な範囲を超えている (壊れた delta / 極端な division)。
    TooLong {
        beats: f64,
    },
    /// division が 0 (= tick の意味が定義できない)。midly は 0 を素通しする
    /// (`midly-0.5.3/src/primitive.rs:534-537`) のでこちらで弾く。
    InvalidDivision,
    /// ノートが 1 つも入っていない (tempo/marker 専用ファイル等)。
    NoNotes,
}

impl fmt::Display for MidiImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "読み込めません ({e})"),
            Self::Parse(e) => write!(f, "SMF として解釈できません ({e})"),
            Self::TooLarge { bytes } => {
                write!(f, "ファイルが大きすぎます ({bytes} bytes)")
            }
            Self::TooManyNotes { notes } => {
                write!(f, "ノートが多すぎます ({notes} 個以上)")
            }
            Self::TooManyTempoChanges { points } => {
                write!(f, "テンポ変化が多すぎます ({points} 個以上)")
            }
            Self::TooManyLyrics { count } => {
                write!(f, "歌詞が多すぎます ({count} 個以上)")
            }
            Self::TooLong { beats } => {
                write!(f, "曲が長すぎます ({beats:.0} 拍)")
            }
            Self::InvalidDivision => write!(f, "ヘッダの division が不正です"),
            Self::NoNotes => write!(f, "ノートが 1 つもありません"),
        }
    }
}

impl From<std::io::Error> for MidiImportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::error::Error for MidiImportError {}

impl From<midly::Error> for MidiImportError {
    fn from(e: midly::Error) -> Self {
        Self::Parse(e)
    }
}

/// 取り込んだ 1 本分 = daw_01 track 1 本になる単位。
#[derive(Debug, Clone)]
pub struct ParsedTrack {
    /// SMF の TrackName meta。無ければ `None` (呼び出し側がファイル名で補う)。
    pub name: Option<String>,
    /// このノート群の MIDI channel (0 始まり)。
    pub channel: u8,
    /// 1 つの SMF track を channel で割った結果か (= track 名に ch 番号を付ける)。
    pub channel_split: bool,
    /// content-local 拍 (SMF tick 0 起点)。start 昇順、id は 1 から採番済み、
    /// 同一ピッチの重なりは解消済み。
    pub notes: Vec<Note>,
}

impl ParsedTrack {
    /// このトラックのノートが 1 つでも歌詞を持つか (clip 名を付けるかの判断に使う)。
    #[must_use]
    pub fn has_lyrics(&self) -> bool {
        self.notes
            .iter()
            .any(|n| n.lyric.as_deref().is_some_and(|l| !l.is_empty()))
    }

    /// 最初のノートの開始拍 / 最後のノートの終端拍 (content-local)。
    #[must_use]
    pub fn span_beats(&self) -> (f64, f64) {
        let start = self
            .notes
            .iter()
            .map(|n| n.start_beat)
            .fold(f64::INFINITY, f64::min);
        let end = self
            .notes
            .iter()
            .map(|n| n.start_beat + n.duration_beats)
            .fold(f64::NEG_INFINITY, f64::max);
        if start.is_finite() && end.is_finite() {
            (start, end)
        } else {
            (0.0, 0.0)
        }
    }
}

/// 1 ファイル分の解析結果。
#[derive(Debug, Clone)]
pub struct ParsedMidi {
    pub tracks: Vec<ParsedTrack>,
    /// テンポの階段 breakpoint `(拍, BPM)`。tempo meta が無ければ空。
    /// 昇順・同値連続は畳んである。
    pub tempo: Vec<(f64, f32)>,
    /// 曲頭の拍子 `(分子, 分母)`。分母は SMF の log2 を実値に直したもの。
    pub time_sig: Option<(u8, u8)>,
    /// モデルに受け皿が無くて捨てたイベント数 (CC / PitchBend / ProgramChange /
    /// Aftertouch / SysEx)。ステータス表示に使う。
    pub dropped_events: usize,
    /// SMPTE timing (division 負値 = 絶対時刻) のファイルか。tick が「秒の細分」
    /// なので拍への換算に**取り込み先プロジェクトのテンポ**を使っている。この
    /// 形式では tempo meta は再生タイミングの正本ではない。
    pub is_smpte: bool,
}

impl ParsedMidi {
    #[must_use]
    pub fn note_count(&self) -> usize {
        self.tracks.iter().map(|t| t.notes.len()).sum()
    }

    /// 最後のノート終端 (content-local 拍)。
    #[must_use]
    pub fn end_beat(&self) -> f64 {
        self.tracks
            .iter()
            .map(|t| t.span_beats().1)
            .fold(0.0, f64::max)
    }
}

/// SMF の tick を拍へ写す基準。metrical は PPQ 割り算、SMPTE は
/// 「tick → 秒 → 取り込み先プロジェクトのテンポで拍」。
enum TimeBasis<'a> {
    Metrical {
        tpqn: f64,
    },
    Smpte {
        ticks_per_second: f64,
        seconds_to_beat: &'a dyn Fn(f64) -> f64,
    },
}

impl TimeBasis<'_> {
    fn beat(&self, tick: u64) -> f64 {
        match self {
            Self::Metrical { tpqn } => tick as f64 / tpqn,
            Self::Smpte {
                ticks_per_second,
                seconds_to_beat,
            } => seconds_to_beat(tick as f64 / ticks_per_second),
        }
    }
}

/// MIDI ファイルを読んで解析する。**サイズ検査は読み込みの前**に行う
/// (`std::fs::read` してから測るのでは防御にならない)。
pub fn read_and_parse(
    path: &Path,
    seconds_to_beat: &dyn Fn(f64) -> f64,
) -> Result<ParsedMidi, MidiImportError> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_FILE_BYTES {
        return Err(MidiImportError::TooLarge { bytes: len });
    }
    let bytes = std::fs::read(path)?;
    parse_midi_bytes(&bytes, seconds_to_beat)
}

/// SMF バイト列を解析する。
///
/// `seconds_to_beat` は SMPTE timing (division 負値) のファイルでだけ使う
/// 「秒 → 拍」変換 (= 取り込み先プロジェクトの `TempoMap::seconds_to_beat`)。
/// metrical timing のファイルでは呼ばれない。
pub fn parse_midi_bytes(
    bytes: &[u8],
    seconds_to_beat: &dyn Fn(f64) -> f64,
) -> Result<ParsedMidi, MidiImportError> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(MidiImportError::TooLarge {
            bytes: bytes.len() as u64,
        });
    }
    let smf = Smf::parse(bytes)?;

    // 歌詞 meta と音符の tick 許容幅。歌詞は同 tick に置くのが慣習だが、
    // 打ち込み / humanize で数 tick ずれるファイルが普通にあるので、
    // 「1/32 拍」相当 (SMPTE では ~60ms) まで同じ音符とみなす。
    let (basis, lyric_tol_ticks, is_smpte) = match smf.header.timing {
        Timing::Metrical(tpqn) => {
            let tpqn = f64::from(tpqn.as_int());
            if tpqn <= 0.0 {
                return Err(MidiImportError::InvalidDivision);
            }
            let tol = ((tpqn / 8.0).round() as u64).max(1);
            (TimeBasis::Metrical { tpqn }, tol, false)
        }
        Timing::Timecode(fps, subframes) => {
            let ticks_per_second = f64::from(fps.as_f32()) * f64::from(subframes);
            if ticks_per_second <= 0.0 {
                return Err(MidiImportError::InvalidDivision);
            }
            let tol = ((ticks_per_second / 16.0).round() as u64).max(1);
            (
                TimeBasis::Smpte {
                    ticks_per_second,
                    seconds_to_beat,
                },
                tol,
                true,
            )
        }
    };

    // ---- 1) 各 SMF track を tick 領域のまま走査 ----
    let mut raw_tracks: Vec<RawTrack> = Vec::new();
    let mut tempo_raw: Vec<(u64, u32)> = Vec::new(); // (tick, microseconds per quarter)
    let mut time_sig_raw: Vec<(u64, u8, u8)> = Vec::new(); // (tick, numerator, denom_log2)
    let mut dropped_events = 0usize;
    let mut total_notes = 0usize;
    for events in &smf.tracks {
        let raw = scan_track(
            events,
            &mut tempo_raw,
            &mut time_sig_raw,
            &mut dropped_events,
            &mut total_notes,
        )?;
        raw_tracks.push(raw);
    }

    let time_sig = time_sig_raw
        .iter()
        .min_by_key(|(tick, _, _)| *tick)
        .and_then(|&(_, num, denom_log2)| sanitize_time_sig(num, denom_log2));

    // ---- 2) Format 2 (独立パターン列) は時間軸に連結して 1 本に畳む ----
    if matches!(smf.header.format, Format::Sequential) && raw_tracks.len() > 1 {
        let bar_ticks = match (&basis, time_sig) {
            (TimeBasis::Metrical { tpqn }, ts) => {
                let (num, denom) = ts.unwrap_or((4, 4));
                let bar = *tpqn * f64::from(num) * 4.0 / f64::from(denom);
                (bar.round() as u64).max(1)
            }
            // SMPTE では小節の tick 数が定義できないので丸めずに連結する。
            (TimeBasis::Smpte { .. }, _) => 1,
        };
        raw_tracks = vec![concat_patterns(raw_tracks, bar_ticks)];
        // 連結でパターンの tick 原点がずれるので、途中のテンポ変化は捨てて曲頭だけ残す。
        tempo_raw.sort_by_key(|(tick, _)| *tick);
        tempo_raw.truncate(1);
    }

    // ---- 3) 歌詞をノートに紐付ける ----
    // Text meta (FF 01) は著作権表示 / 制作者名 / 区間名にも使われるので、
    // ファイルが .kar だと自称している (= `@KMIDI KARAOKE FILE` 等の `@` 行が
    // ある) ときだけ歌詞に昇格させる。Lyric meta (FF 05) は常に歌詞。
    let is_kar = raw_tracks.iter().any(|t| t.kar_marker);
    for track in &mut raw_tracks {
        if is_kar && track.lyrics.is_empty() {
            track.lyrics = std::mem::take(&mut track.text_lyrics);
        } else {
            track.text_lyrics.clear();
        }
    }
    attach_lyrics(&mut raw_tracks, lyric_tol_ticks);

    // ---- 4) channel 分割 + tick → 拍 + 重なり解消 + id 採番 ----
    let mut tracks: Vec<ParsedTrack> = Vec::new();
    for raw in &raw_tracks {
        tracks.extend(build_parsed_tracks(raw, &basis));
    }
    tracks.retain(|t| !t.notes.is_empty());
    if tracks.is_empty() {
        return Err(MidiImportError::NoNotes);
    }

    // ---- 5) テンポ breakpoint ----
    let mut tempo: Vec<(f64, f32)> = Vec::new();
    tempo_raw.sort_by_key(|(tick, _)| *tick);
    for (tick, us_per_quarter) in tempo_raw {
        if us_per_quarter == 0 {
            continue;
        }
        let bpm = (60_000_000.0 / f64::from(us_per_quarter)).clamp(1.0, 1000.0) as f32;
        let beat = basis.beat(tick);
        match tempo.last() {
            // 同じ BPM が続く / 同じ拍に複数ある場合は畳む。
            Some(&(_, prev)) if (prev - bpm).abs() < 1e-4 => {}
            Some(&(prev_beat, _)) if (prev_beat - beat).abs() < 1e-9 => {
                tempo.pop();
                tempo.push((beat, bpm));
            }
            _ => tempo.push((beat, bpm)),
        }
    }

    let parsed = ParsedMidi {
        tracks,
        tempo,
        time_sig,
        dropped_events,
        is_smpte,
    };
    // 壊れた delta / 極端な division で曲が非現実的な長さになったものは
    // Song に持ち込まない (`Song.length_beats` が伸びて TempoMap が破綻する)。
    let span = parsed.end_beat();
    if !span.is_finite() || span > MAX_SPAN_BEATS {
        return Err(MidiImportError::TooLong { beats: span });
    }
    Ok(parsed)
}

/// tick 領域の 1 SMF track。
#[derive(Debug, Default, Clone)]
struct RawTrack {
    name: Option<String>,
    notes: Vec<RawNote>,
    /// Lyric meta (FF 05) 由来の `(tick, 歌詞)`。ノートへの紐付けは
    /// [`attach_lyrics`] で行う。
    lyrics: Vec<(u64, String)>,
    /// Text meta (FF 01) 由来。`.kar` と分かったときだけ `lyrics` に昇格する。
    text_lyrics: Vec<(u64, String)>,
    /// `@` で始まる Text meta (= `@KMIDI KARAOKE FILE` 等 KAR のヘッダ行) を見たか。
    kar_marker: bool,
    end_tick: u64,
}

#[derive(Debug, Clone)]
struct RawNote {
    channel: u8,
    on_tick: u64,
    off_tick: u64,
    pitch: u8,
    velocity: u8,
    lyric: Option<String>,
}

/// 1 SMF track を走査して tick 領域の note / 歌詞 / track 名を取り出す。
/// tempo / time_sig / 捨てたイベント数 / ノート総数は呼び出し側の集約先へ足す
/// (`total_notes` は NoteOn を見た時点で数える = Off の来ないノートが
/// `pending` に無限に溜まるのを上限で止める)。
fn scan_track(
    events: &[midly::TrackEvent<'_>],
    tempo_raw: &mut Vec<(u64, u32)>,
    time_sig_raw: &mut Vec<(u64, u8, u8)>,
    dropped_events: &mut usize,
    total_notes: &mut usize,
) -> Result<RawTrack, MidiImportError> {
    let mut out = RawTrack::default();
    // (channel, key) ごとに鳴りっぱなしの NoteOn を FIFO で保持する
    // (SMF 仕様は On/Off の対応規則を定めていない。Ardour と同じ first-on-first-off)。
    let mut pending: HashMap<(u8, u8), VecDeque<(u64, u8)>> = HashMap::new();
    // Lyric meta が 1 つでもあれば Text meta は歌詞扱いしない (Text は著作権表示等に
    // 使われる)。Lyric が無いときだけ Text を歌詞候補にする (.kar の慣習)。
    let mut lyric_metas: Vec<(u64, String)> = Vec::new();
    let mut text_metas: Vec<(u64, String)> = Vec::new();
    let mut abs_tick: u64 = 0;

    for ev in events {
        abs_tick = abs_tick.saturating_add(u64::from(ev.delta.as_int()));
        match ev.kind {
            TrackEventKind::Midi { channel, message } => {
                let ch = channel.as_int();
                match message {
                    MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                        *total_notes += 1;
                        if *total_notes > MAX_NOTES {
                            return Err(MidiImportError::TooManyNotes {
                                notes: *total_notes,
                            });
                        }
                        pending
                            .entry((ch, key.as_int()))
                            .or_default()
                            .push_back((abs_tick, vel.as_int()));
                    }
                    // velocity 0 の NoteOn は NoteOff (MIDI の慣習。midly は変換しない)。
                    MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                        let key = key.as_int();
                        if let Some((on_tick, velocity)) =
                            pending.get_mut(&(ch, key)).and_then(VecDeque::pop_front)
                        {
                            out.notes.push(RawNote {
                                channel: ch,
                                on_tick,
                                off_tick: abs_tick.max(on_tick),
                                pitch: key,
                                velocity,
                                lyric: None,
                            });
                        }
                        // 対応する NoteOn の無い孤立 NoteOff は無視する。
                    }
                    // CC / PitchBend / ProgramChange / Aftertouch はモデルに受け皿が無い。
                    _ => *dropped_events += 1,
                }
            }
            TrackEventKind::Meta(meta) => match meta {
                MetaMessage::TrackName(raw) if out.name.is_none() => {
                    let name = decode_text(raw);
                    let name = name.trim();
                    if !name.is_empty() {
                        out.name = Some(name.to_string());
                    }
                }
                MetaMessage::Tempo(us) => {
                    if tempo_raw.len() >= MAX_TEMPO_POINTS {
                        return Err(MidiImportError::TooManyTempoChanges {
                            points: tempo_raw.len() + 1,
                        });
                    }
                    tempo_raw.push((abs_tick, us.as_int()));
                }
                MetaMessage::TimeSignature(num, denom_log2, _, _) => {
                    time_sig_raw.push((abs_tick, num, denom_log2));
                }
                MetaMessage::Lyric(raw) => {
                    if let Some(text) = clean_lyric(&decode_text(raw)) {
                        if lyric_metas.len() >= MAX_LYRIC_METAS {
                            return Err(MidiImportError::TooManyLyrics {
                                count: lyric_metas.len() + 1,
                            });
                        }
                        lyric_metas.push((abs_tick, text));
                    }
                }
                MetaMessage::Text(raw) => {
                    let text = decode_text(raw);
                    // KAR のヘッダ行 (`@KMIDI KARAOKE FILE` / `@L` / `@T` …)。
                    // これがあるファイルだけ Text meta を歌詞として扱う。
                    if text.trim_start_matches(['/', '\\']).starts_with('@') {
                        out.kar_marker = true;
                    }
                    if let Some(text) = clean_lyric(&text) {
                        if text_metas.len() >= MAX_LYRIC_METAS {
                            return Err(MidiImportError::TooManyLyrics {
                                count: text_metas.len() + 1,
                            });
                        }
                        text_metas.push((abs_tick, text));
                    }
                }
                MetaMessage::EndOfTrack => out.end_tick = out.end_tick.max(abs_tick),
                _ => {}
            },
            // SysEx / Escape はモデルに受け皿が無い。
            TrackEventKind::SysEx(_) | TrackEventKind::Escape(_) => *dropped_events += 1,
        }
    }

    // Off の来なかったノートは track 末尾まで伸ばす (Ardour の ResolveStuckNotes)。
    let track_end = out
        .end_tick
        .max(abs_tick)
        .max(out.notes.iter().map(|n| n.off_tick).max().unwrap_or(0));
    for ((ch, key), stuck) in pending {
        for (on_tick, velocity) in stuck {
            out.notes.push(RawNote {
                channel: ch,
                on_tick,
                off_tick: track_end.max(on_tick),
                pitch: key,
                velocity,
                lyric: None,
            });
        }
    }
    out.end_tick = track_end;
    out.lyrics = lyric_metas;
    out.text_lyrics = text_metas;
    out.lyrics.sort_by_key(|(tick, _)| *tick);
    out.text_lyrics.sort_by_key(|(tick, _)| *tick);
    Ok(out)
}

/// Format 2 の独立パターン列を時間軸に連結して 1 track に畳む
/// (パターンは同時に鳴らす物ではないため)。境界は小節に切り上げる。
fn concat_patterns(tracks: Vec<RawTrack>, bar_ticks: u64) -> RawTrack {
    let mut merged = RawTrack::default();
    let mut offset = 0u64;
    for t in tracks {
        if merged.name.is_none() {
            merged.name = t.name.clone();
        }
        for mut n in t.notes {
            n.on_tick += offset;
            n.off_tick += offset;
            merged.notes.push(n);
        }
        for (tick, text) in t.lyrics {
            merged.lyrics.push((tick + offset, text));
        }
        let end = offset + t.end_tick;
        offset = end.div_ceil(bar_ticks.max(1)) * bar_ticks.max(1);
        merged.end_tick = merged.end_tick.max(end);
    }
    merged.lyrics.sort_by_key(|(tick, _)| *tick);
    merged
}

/// 歌詞 meta をノートに割り当てる。同 track 内では「歌詞 tick に最も近い、まだ歌詞の
/// 付いていないノート」へ順に配る。ノートを持たない歌詞 track (= .kar の "Words"
/// track と melody track が分かれている構成) の歌詞は、**歌のパートらしい** note
/// track へ回す (SMF の並び順で先頭を取るとドラム track に乗ることがある)。
fn attach_lyrics(tracks: &mut [RawTrack], tol: u64) {
    for track in tracks.iter_mut() {
        if track.notes.is_empty() || track.lyrics.is_empty() {
            continue;
        }
        let lyrics = std::mem::take(&mut track.lyrics);
        assign_lyrics(&mut track.notes, &lyrics, tol);
    }
    // ノートの無い track に残った歌詞を、歌詞未設定の note track へ移す。
    let mut orphans: Vec<(u64, String)> = tracks
        .iter_mut()
        .filter(|t| t.notes.is_empty())
        .flat_map(|t| std::mem::take(&mut t.lyrics))
        .collect();
    if orphans.is_empty() {
        return;
    }
    orphans.sort_by_key(|(tick, _)| *tick);
    // 付け先は「歌詞 tick に音の頭が合う数」が最大の track。同点なら非ドラム →
    // SMF の並び順。
    let target = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.notes.is_empty() && t.notes.iter().all(|n| n.lyric.is_none()))
        .min_by_key(|(i, t)| {
            let onsets = sorted_onsets(t.notes.iter());
            let hits = orphans
                .iter()
                .filter(|(tick, _)| nearest_distance(&onsets, *tick) <= tol)
                .count();
            let all_drums = t.notes.iter().all(|n| n.channel == DRUM_CHANNEL);
            (std::cmp::Reverse(hits), all_drums, *i)
        })
        .map(|(i, _)| i);
    if let Some(i) = target {
        assign_lyrics(&mut tracks[i].notes, &orphans, tol);
    }
}

/// GM のドラムチャンネル (0 始まり)。歌詞の付け先としては最後に選ぶ。
const DRUM_CHANNEL: u8 = 9;

/// ノート列の音の頭 tick を昇順・重複除去で返す (最近傍探索用)。
fn sorted_onsets<'a>(notes: impl Iterator<Item = &'a RawNote>) -> Vec<u64> {
    let mut v: Vec<u64> = notes.map(|n| n.on_tick).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 昇順 `sorted` の中で `tick` に最も近い値との距離。空なら `u64::MAX`。
fn nearest_distance(sorted: &[u64], tick: u64) -> u64 {
    match sorted.binary_search(&tick) {
        Ok(_) => 0,
        Err(i) => {
            let before = if i > 0 { tick - sorted[i - 1] } else { u64::MAX };
            let after = if i < sorted.len() {
                sorted[i] - tick
            } else {
                u64::MAX
            };
            before.min(after)
        }
    }
}

/// 歌詞を付ける channel を 1 つ選ぶ。`None` は notes が空のとき。
///
/// Type 0 (全チャンネルが 1 SMF track) の .kar では、同じ tick にメロディと
/// ドラムが並ぶ。channel を選ばずに「tick の近い最初のノート」へ配ると、
/// ピッチの低いドラム側に歌詞が乗ってしまう (実ファイルで確認)。そこで
/// **歌詞の tick に音の頭が (`tol` 以内で) 合う数が最も多い channel** を
/// 歌のパートとみなす。同点なら距離の合計が小さい方 → 非ドラム → 番号の小さい方。
fn choose_lyric_channel(notes: &[RawNote], lyrics: &[(u64, String)], tol: u64) -> Option<u8> {
    let mut channels: Vec<u8> = notes.iter().map(|n| n.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    if channels.len() <= 1 {
        return channels.first().copied();
    }
    let mut onsets: HashMap<u8, Vec<u64>> = HashMap::new();
    for ch in &channels {
        onsets.insert(*ch, sorted_onsets(notes.iter().filter(|n| n.channel == *ch)));
    }
    // (一致数, 距離合計) — 一致数が多く、より近い channel を歌とみなす。
    let score = |ch: u8| -> (usize, u64) {
        let Some(v) = onsets.get(&ch) else {
            return (0, u64::MAX);
        };
        let mut hits = 0usize;
        let mut dist = 0u64;
        for (tick, _) in lyrics {
            let d = nearest_distance(v, *tick);
            if d <= tol {
                hits += 1;
                dist = dist.saturating_add(d);
            }
        }
        (hits, dist)
    };
    channels.sort_by_key(|&ch| {
        let (hits, dist) = score(ch);
        (std::cmp::Reverse(hits), dist, ch == DRUM_CHANNEL, ch)
    });
    channels.first().copied()
}

/// `lyrics` (tick 昇順) を `notes` へ順に割り当てる。付け先は
/// [`choose_lyric_channel`] が選んだ 1 channel に限る。
///
/// 各歌詞は「まだ歌詞の付いていないノートのうち tick が最も近いもの」へ付ける。
/// 固定の ±1 tick で「歌詞より前のノート」を飛ばす方式だと、歌詞 meta が音の頭より
/// 数 tick 後ろに置かれたファイル (打ち込み由来では普通) で全歌詞が 1 音ずれ、
/// 最後の歌詞が落ちる。ノートは tick 昇順なので、距離が増え始めた時点で打ち切れる。
fn assign_lyrics(notes: &mut [RawNote], lyrics: &[(u64, String)], tol: u64) {
    let Some(channel) = choose_lyric_channel(notes, lyrics, tol) else {
        return;
    };
    let mut order: Vec<usize> = (0..notes.len())
        .filter(|&i| notes[i].channel == channel)
        .collect();
    order.sort_by_key(|&i| (notes[i].on_tick, notes[i].pitch));
    let mut cursor = 0usize;
    for (tick, text) in lyrics {
        let mut best: Option<(usize, u64)> = None;
        let mut i = cursor;
        while i < order.len() {
            let idx = order[i];
            if notes[idx].lyric.is_some() {
                i += 1;
                continue;
            }
            let d = notes[idx].on_tick.abs_diff(*tick);
            match best {
                Some((_, best_d)) if d >= best_d => break,
                _ => best = Some((i, d)),
            }
            i += 1;
        }
        if let Some((i, _)) = best {
            notes[order[i]].lyric = Some(text.clone());
            cursor = i + 1;
        }
    }
}

/// 1 つの tick 領域 track を、channel ごとの `ParsedTrack` へ変換する。
fn build_parsed_tracks(raw: &RawTrack, basis: &TimeBasis<'_>) -> Vec<ParsedTrack> {
    let mut by_channel: BTreeMap<u8, Vec<&RawNote>> = BTreeMap::new();
    for note in &raw.notes {
        by_channel.entry(note.channel).or_default().push(note);
    }
    let channel_split = by_channel.len() > 1;
    by_channel
        .into_iter()
        .map(|(channel, raws)| {
            let mut notes: Vec<Note> = raws
                .iter()
                .map(|r| {
                    let start_beat = basis.beat(r.on_tick);
                    // Off が On と同 tick のノートは 1 tick 分の最小長にする
                    // (長さ 0 のノートはモデル上「鳴らない」)。
                    let min_dur = (basis.beat(r.on_tick + 1) - start_beat).max(f64::EPSILON);
                    Note {
                        id: 0,
                        start_beat,
                        duration_beats: (basis.beat(r.off_tick) - start_beat).max(min_dur),
                        pitch: r.pitch.min(127),
                        velocity: r.velocity.min(127),
                        lyric: r.lyric.clone(),
                        muted: false,
                    }
                })
                .collect();
            notes.sort_by(|a, b| {
                a.start_beat
                    .total_cmp(&b.start_beat)
                    .then(a.pitch.cmp(&b.pitch))
            });
            // 同一ピッチが時間的に重ならないモデル不変条件を満たす (SMF は重なりを許す)。
            truncate_same_pitch_overlaps(&mut notes);
            for (i, note) in notes.iter_mut().enumerate() {
                note.id = i as u32 + 1;
            }
            ParsedTrack {
                name: raw.name.clone(),
                channel,
                channel_split,
                notes,
            }
        })
        .collect()
}

/// 同一ピッチのノートが時間的に重ならないモデル不変条件を、**start 昇順に並んだ
/// bulk のノート列**へ線形時間で適用する。規則は編集操作側の
/// [`crate::app_types::resolve_note_overlaps`] の winner 同士と同じ
/// 「後から始まる方が勝ち、前のノート末尾をトリム (同じ開始なら前を削除)」。
///
/// 一般形 (winner / loser) は pitch ごとの総当たりで O(n²) になり、数万ノートの
/// 取り込みで GUI が数分止まる。取り込みは「全ノートが winner で、かつ既に
/// start 昇順」という特殊形なので、直前の同ピッチ 1 個だけ見れば足りる。
fn truncate_same_pitch_overlaps(notes: &mut Vec<Note>) {
    const EPS: f64 = 1e-9;
    let mut last_by_pitch: HashMap<u8, usize> = HashMap::new();
    let mut dead: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for i in 0..notes.len() {
        let pitch = notes[i].pitch;
        if let Some(&prev) = last_by_pitch.get(&pitch) {
            let prev_end = notes[prev].start_beat + notes[prev].duration_beats;
            if prev_end > notes[i].start_beat + EPS {
                let trimmed = notes[i].start_beat - notes[prev].start_beat;
                if trimmed <= EPS {
                    // 完全被覆 (同じ開始) → 前のノートを捨てる。
                    dead.insert(prev);
                } else {
                    notes[prev].duration_beats = trimmed;
                }
            }
        }
        last_by_pitch.insert(pitch, i);
    }
    if !dead.is_empty() {
        let mut i = 0usize;
        notes.retain(|_| {
            let keep = !dead.contains(&i);
            i += 1;
            keep
        });
    }
}

/// SMF の TimeSignature meta を daw_01 の `(分子, 分母)` へ。分母は SMF では
/// 2 の冪指数 (2 = 4 分音符)。モデルが受け付けない値域は `None` (= 採用しない)。
fn sanitize_time_sig(num: u8, denom_log2: u8) -> Option<(u8, u8)> {
    if !(1..=32).contains(&num) || denom_log2 > 4 {
        return None;
    }
    Some((num, 1u8 << denom_log2))
}

/// meta テキストのデコード。SMF 仕様は文字コードを規定していないので
/// UTF-8 を試し、失敗したら Shift-JIS (日本語 .mid / .kar の実情)。
/// 長さは [`MAX_META_TEXT_BYTES`] で切る (track 名 / 歌詞がそのまま Song に載るため)。
fn decode_text(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(MAX_META_TEXT_BYTES)];
    match std::str::from_utf8(head) {
        Ok(s) => s.to_string(),
        // 上限で multibyte の途中を切っただけなら、有効な部分までを使う
        // (`error_len() == None` = 入力の途中終端)。それ以外は Shift-JIS とみなす。
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
            String::from_utf8_lossy(&head[..e.valid_up_to()]).into_owned()
        }
        Err(_) => encoding_rs::SHIFT_JIS.decode(head).0.into_owned(),
    }
}

/// 歌詞テキストの整形。`/` `\` は .kar の行 / 段落区切り、`@` 始まりは
/// .kar のメタ行 (`@KMIDI KARAOKE FILE` / `@L` / `@T` 等) なので捨てる。
fn clean_lyric(text: &str) -> Option<String> {
    let t = text.trim_start_matches(['/', '\\']);
    if t.starts_with('@') {
        return None;
    }
    let t = t.trim_end_matches(['\r', '\n']);
    (!t.trim().is_empty()).then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::num::{u4, u7, u15, u24, u28};
    use midly::{Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};

    /// 秒 → 拍 (テスト用に 120 BPM 固定 = 2 拍/秒)。
    fn secs_to_beat_120(s: f64) -> f64 {
        s * 2.0
    }

    fn note_on(delta: u32, channel: u8, key: u8, vel: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Midi {
                channel: u4::from(channel),
                message: MidiMessage::NoteOn {
                    key: u7::from(key),
                    vel: u7::from(vel),
                },
            },
        }
    }

    fn note_off(delta: u32, channel: u8, key: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Midi {
                channel: u4::from(channel),
                message: MidiMessage::NoteOff {
                    key: u7::from(key),
                    vel: u7::from(0),
                },
            },
        }
    }

    fn meta(delta: u32, m: MetaMessage<'_>) -> TrackEvent<'_> {
        TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Meta(m),
        }
    }

    fn eot() -> TrackEvent<'static> {
        meta(0, MetaMessage::EndOfTrack)
    }

    /// track 群を SMF バイト列に書き出す (テスト fixture)。
    fn build_smf(format: Format, timing: Timing, tracks: Vec<Vec<TrackEvent<'_>>>) -> Vec<u8> {
        let mut smf = Smf::new(Header::new(format, timing));
        smf.tracks = tracks;
        let mut out = Vec::new();
        smf.write_std(&mut out).unwrap();
        out
    }

    fn parse(bytes: &[u8]) -> ParsedMidi {
        parse_midi_bytes(bytes, &secs_to_beat_120).expect("parse")
    }

    #[test]
    fn ppq_は_ヘッダの値を使う() {
        // PPQ 96 のファイルで 96 tick = 1 拍。480 決め打ちにしていたら 0.2 拍になる。
        for ppq in [96u16, 192, 480, 960] {
            let bytes = build_smf(
                Format::SingleTrack,
                Timing::Metrical(u15::from(ppq)),
                vec![vec![
                    note_on(0, 0, 60, 100),
                    note_off(u32::from(ppq), 0, 60),
                    eot(),
                ]],
            );
            let parsed = parse(&bytes);
            let note = &parsed.tracks[0].notes[0];
            assert!(
                (note.duration_beats - 1.0).abs() < 1e-9,
                "ppq={ppq} duration={}",
                note.duration_beats
            );
        }
    }

    #[test]
    fn velocity0_の_note_on_は_note_off_として扱う() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 0, 60, 100),
                note_on(480, 0, 60, 0), // = NoteOff
                note_on(0, 0, 62, 90),
                note_off(480, 0, 62),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].pitch, 60);
        assert!((notes[0].duration_beats - 1.0).abs() < 1e-9);
        assert_eq!(notes[0].velocity, 100, "velocity は NoteOn 側の値を使う");
    }

    #[test]
    fn note_off_の来ないノートは_track_末尾まで伸ばす() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 0, 60, 100),
                note_on(960, 0, 67, 100),
                note_off(480, 0, 67),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let stuck = parsed.tracks[0]
            .notes
            .iter()
            .find(|n| n.pitch == 60)
            .unwrap();
        // 最終イベント = 1440 tick = 3 拍。
        assert!(
            (stuck.duration_beats - 3.0).abs() < 1e-9,
            "got {}",
            stuck.duration_beats
        );
    }

    #[test]
    fn 孤立した_note_off_は無視する() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_off(0, 0, 60),
                note_on(480, 0, 62, 100),
                note_off(480, 0, 62),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.tracks[0].notes.len(), 1);
        assert_eq!(parsed.tracks[0].notes[0].pitch, 62);
    }

    #[test]
    fn 複数_channel_は別トラックに分かれる() {
        // 1 SMF track に ch0 / ch9 が混在 (Type 0 の典型)。
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 0, 60, 100),
                note_on(0, 9, 36, 110),
                note_off(480, 0, 60),
                note_off(0, 9, 36),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.tracks.len(), 2, "channel ごとに分かれる");
        assert_eq!(parsed.tracks[0].channel, 0);
        assert_eq!(parsed.tracks[1].channel, 9);
        assert!(parsed.tracks[0].channel_split);
        assert_eq!(parsed.tracks[0].notes[0].pitch, 60);
        assert_eq!(parsed.tracks[1].notes[0].pitch, 36);
    }

    #[test]
    fn 単一_channel_なら分割印は立たない() {
        let bytes = build_smf(
            Format::Parallel,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 3, 60, 100),
                note_off(480, 3, 60),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].channel, 3);
        assert!(!parsed.tracks[0].channel_split);
    }

    #[test]
    fn 同一ピッチの重なりは解消される() {
        // 同じ pitch が 2 拍重なる SMF (SMF では合法、daw_01 モデルでは不変条件違反)。
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 0, 60, 100),   // beat 0, 4 拍
                note_on(960, 0, 60, 100), // beat 2 から重なる
                note_off(960, 0, 60),     // beat 4 (FIFO なので 1 本目の Off)
                note_off(960, 0, 60),     // beat 6
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes.len(), 2);
        // 後から始まる方が勝ち、先行ノートの末尾が beat 2 でトリムされる。
        assert!((notes[0].start_beat - 0.0).abs() < 1e-9);
        assert!(
            (notes[0].duration_beats - 2.0).abs() < 1e-9,
            "先行ノートは重なり開始でトリム: {}",
            notes[0].duration_beats
        );
        assert!((notes[1].start_beat - 2.0).abs() < 1e-9);
    }

    #[test]
    fn テンポと拍子を取り出す() {
        let bytes = build_smf(
            Format::Parallel,
            Timing::Metrical(u15::from(480u16)),
            vec![
                vec![
                    meta(0, MetaMessage::Tempo(u24::from(500_000u32))), // 120 BPM
                    meta(0, MetaMessage::TimeSignature(6, 3, 24, 8)),   // 6/8
                    meta(1920, MetaMessage::Tempo(u24::from(400_000u32))), // 150 BPM @ beat 4
                    eot(),
                ],
                vec![note_on(0, 0, 60, 100), note_off(480, 0, 60), eot()],
            ],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.time_sig, Some((6, 8)));
        assert_eq!(parsed.tempo.len(), 2);
        assert!((parsed.tempo[0].0 - 0.0).abs() < 1e-9);
        assert!((parsed.tempo[0].1 - 120.0).abs() < 1e-3);
        assert!((parsed.tempo[1].0 - 4.0).abs() < 1e-9);
        assert!((parsed.tempo[1].1 - 150.0).abs() < 1e-3);
        assert_eq!(parsed.tracks.len(), 1, "ノートの無い meta track は捨てる");
    }

    #[test]
    fn 歌詞は音符に紐付く() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                meta(0, MetaMessage::Lyric(b"\xE3\x81\x8B")), // "か" (UTF-8)
                note_on(0, 0, 60, 100),
                note_off(480, 0, 60),
                meta(0, MetaMessage::Lyric(b"\x82\xA2")), // "い" (Shift-JIS)
                note_on(0, 0, 62, 100),
                note_off(480, 0, 62),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes[0].lyric.as_deref(), Some("か"));
        assert_eq!(
            notes[1].lyric.as_deref(),
            Some("い"),
            "Shift-JIS の歌詞もデコードする"
        );
    }

    #[test]
    fn kar_のメタ行と区切り記号は歌詞にしない() {
        assert_eq!(clean_lyric("@KMIDI KARAOKE FILE"), None);
        assert_eq!(clean_lyric("/さくら"), Some("さくら".to_string()));
        assert_eq!(clean_lyric("\\はな"), Some("はな".to_string()));
        assert_eq!(clean_lyric("  "), None);
        assert_eq!(clean_lyric("ら\r\n"), Some("ら".to_string()));
    }

    #[test]
    fn 歌詞はドラムでなく歌のチャンネルに付く() {
        // Type 0 の .kar 想定: 同じ tick に ch0 メロディと ch9 ドラムが並ぶ。
        // 歌詞は ch0 側に付かなければならない (ピッチの低いドラムに吸われない)。
        let mut events = vec![meta(0, MetaMessage::TrackName(b"karaoke"))];
        for (i, key) in [67u8, 69, 71].into_iter().enumerate() {
            events.push(meta(0, MetaMessage::Lyric(if i == 0 {
                b"sa"
            } else if i == 1 {
                b"ku"
            } else {
                b"ra"
            })));
            events.push(note_on(0, 0, key, 100));
            events.push(note_on(0, 9, 36, 110)); // ドラムは同 tick / 低ピッチ
            events.push(note_off(480, 0, key));
            events.push(note_off(0, 9, 36));
        }
        events.push(eot());
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![events],
        );
        let parsed = parse(&bytes);
        let melody = parsed.tracks.iter().find(|t| t.channel == 0).unwrap();
        let drums = parsed.tracks.iter().find(|t| t.channel == 9).unwrap();
        assert_eq!(
            melody
                .notes
                .iter()
                .filter_map(|n| n.lyric.as_deref())
                .collect::<Vec<_>>(),
            vec!["sa", "ku", "ra"],
            "歌詞はメロディ channel に付く"
        );
        assert!(
            drums.notes.iter().all(|n| n.lyric.is_none()),
            "ドラム channel には付かない"
        );
    }

    #[test]
    fn ノートを持たない歌詞トラックの歌詞も紐付く() {
        // .kar の「melody track と Words track が分かれている」構成。歌詞は Text meta で、
        // 先頭に KAR のヘッダ行 (`@KMIDI KARAOKE FILE`) を持つ (実ファイルと同じ形)。
        let bytes = build_smf(
            Format::Parallel,
            Timing::Metrical(u15::from(480u16)),
            vec![
                vec![
                    note_on(0, 0, 60, 100),
                    note_off(480, 0, 60),
                    note_on(0, 0, 62, 100),
                    note_off(480, 0, 62),
                    eot(),
                ],
                vec![
                    meta(0, MetaMessage::Text(b"@KMIDI KARAOKE FILE")),
                    meta(0, MetaMessage::Text(b"do")),
                    meta(480, MetaMessage::Text(b"re")),
                    eot(),
                ],
            ],
        );
        let parsed = parse(&bytes);
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes[0].lyric.as_deref(), Some("do"));
        assert_eq!(notes[1].lyric.as_deref(), Some("re"));
    }

    /// `.kar` と自称していないファイルの Text meta (著作権表示 / 制作者名 / 区間名) は
    /// 歌詞にしない。conductor track の "Produced by ..." が先頭ノートの歌詞になると、
    /// VOICEVOX がそれを歌い、クリップ表示もその文字列に化ける。
    #[test]
    fn カラオケでないファイルの_text_meta_は歌詞にしない() {
        let bytes = build_smf(
            Format::Parallel,
            Timing::Metrical(u15::from(480u16)),
            vec![
                vec![
                    meta(0, MetaMessage::Text(b"Produced by Example Studio")),
                    meta(0, MetaMessage::Tempo(u24::from(500_000u32))),
                    eot(),
                ],
                vec![
                    meta(0, MetaMessage::TrackName(b"Piano")),
                    note_on(0, 0, 60, 100),
                    note_off(480, 0, 60),
                    eot(),
                ],
            ],
        );
        let parsed = parse(&bytes);
        assert!(
            parsed.tracks[0].notes.iter().all(|n| n.lyric.is_none()),
            "Text meta は歌詞にならない: {:?}",
            parsed.tracks[0].notes[0].lyric
        );
        assert!(!parsed.tracks[0].has_lyrics());
    }

    /// 歌詞 meta が音の頭より数 tick **後ろ**に置かれたファイル (打ち込み由来では
    /// 普通) でも、歌詞が 1 音ずれたり末尾が落ちたりしない。
    #[test]
    fn 歌詞が音符より少し後ろにあってもずれない() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                note_on(0, 0, 60, 100),
                meta(2, MetaMessage::Lyric(b"L1")),
                note_off(478, 0, 60),
                note_on(0, 0, 62, 100),
                meta(2, MetaMessage::Lyric(b"L2")),
                note_off(478, 0, 62),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes[0].lyric.as_deref(), Some("L1"));
        assert_eq!(notes[1].lyric.as_deref(), Some("L2"));
    }

    /// クオンタイズされていない (humanize された) メロディでも歌のパートとして
    /// 選ばれる。伴奏が拍頭ぴったりでも、歌詞と 1:1 に対応する方を選ぶ。
    #[test]
    fn humanize_されたメロディが歌のパートに選ばれる() {
        let mut events = vec![];
        // ch1 = ベース: 2 分音符 2 個 (拍頭ぴったり)。
        events.push(note_on(0, 1, 40, 100));
        events.push(note_off(960, 1, 40));
        events.push(note_on(0, 1, 40, 100));
        events.push(note_off(960, 1, 40));
        // ch3 = メロディ: 4 分音符 4 個を 5 tick 遅らせて配置 + 各音の直前に歌詞。
        let mut t = 0u32;
        for (i, key) in [60u8, 62, 64, 65].into_iter().enumerate() {
            let lyric_at = i as u32 * 480;
            events.push(meta(lyric_at.saturating_sub(t), MetaMessage::Lyric(b"la")));
            t = lyric_at;
            events.push(note_on(5, 3, key, 100));
            t += 5;
            events.push(note_off(470, 3, key));
            t += 470;
        }
        events.push(eot());
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![events],
        );
        let parsed = parse(&bytes);
        let melody = parsed.tracks.iter().find(|t| t.channel == 3).unwrap();
        let bass = parsed.tracks.iter().find(|t| t.channel == 1).unwrap();
        assert_eq!(
            melody.notes.iter().filter(|n| n.lyric.is_some()).count(),
            4,
            "歌詞は humanize されたメロディに付く"
        );
        assert!(
            bass.notes.iter().all(|n| n.lyric.is_none()),
            "ベースには付かない"
        );
    }

    /// 歌詞専用 track の歌詞は、SMF の並び順で先頭の音符 track (= ドラム) ではなく
    /// 歌のパートへ回す。
    #[test]
    fn 歌詞専用トラックの歌詞はドラムでなくメロディに回る() {
        let bytes = build_smf(
            Format::Parallel,
            Timing::Metrical(u15::from(480u16)),
            vec![
                vec![meta(0, MetaMessage::TrackName(b"Conductor")), eot()],
                vec![
                    meta(0, MetaMessage::TrackName(b"Drums")),
                    note_on(0, 9, 36, 110),
                    note_off(480, 9, 36),
                    note_on(0, 9, 38, 110),
                    note_off(480, 9, 38),
                    eot(),
                ],
                vec![
                    meta(0, MetaMessage::TrackName(b"Melody")),
                    note_on(0, 0, 60, 100),
                    note_off(480, 0, 60),
                    note_on(0, 0, 62, 100),
                    note_off(480, 0, 62),
                    eot(),
                ],
                vec![
                    meta(0, MetaMessage::Text(b"@KMIDI KARAOKE FILE")),
                    meta(0, MetaMessage::Text(b"do")),
                    meta(480, MetaMessage::Text(b"re")),
                    eot(),
                ],
            ],
        );
        let parsed = parse(&bytes);
        let melody = parsed.tracks.iter().find(|t| t.channel == 0).unwrap();
        let drums = parsed.tracks.iter().find(|t| t.channel == 9).unwrap();
        assert_eq!(melody.notes[0].lyric.as_deref(), Some("do"));
        assert_eq!(melody.notes[1].lyric.as_deref(), Some("re"));
        assert!(drums.notes.iter().all(|n| n.lyric.is_none()));
    }

    /// 壊れた delta で非現実的な長さになったファイルは取り込まない
    /// (`Song.length_beats` が伸びて TempoMap が破綻するため)。
    #[test]
    fn 長すぎるファイルはエラー() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(1u16)), // 1 tick = 1 拍
            vec![vec![
                note_on(200_000, 0, 60, 100),
                note_off(1, 0, 60),
                eot(),
            ]],
        );
        let err = parse_midi_bytes(&bytes, &secs_to_beat_120).unwrap_err();
        assert!(matches!(err, MidiImportError::TooLong { .. }), "{err}");
    }

    #[test]
    fn note以外のイベントは数えて捨てる() {
        let cc = TrackEvent {
            delta: u28::from(0u32),
            kind: TrackEventKind::Midi {
                channel: u4::from(0),
                message: MidiMessage::Controller {
                    controller: u7::from(7),
                    value: u7::from(100),
                },
            },
        };
        let pc = TrackEvent {
            delta: u28::from(0u32),
            kind: TrackEventKind::Midi {
                channel: u4::from(0),
                message: MidiMessage::ProgramChange {
                    program: u7::from(48),
                },
            },
        };
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                cc,
                pc,
                note_on(0, 0, 60, 100),
                note_off(480, 0, 60),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.dropped_events, 2);
        assert_eq!(parsed.note_count(), 1);
    }

    #[test]
    fn smpte_timing_は秒経由で拍に直す() {
        // 30 fps × 80 subframe = 2400 tick/秒。2400 tick = 1 秒 = 120BPM で 2 拍。
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Timecode(midly::Fps::Fps30, 80),
            vec![vec![
                note_on(0, 0, 60, 100),
                note_off(2400, 0, 60),
                eot(),
            ]],
        );
        let parsed = parse(&bytes);
        let note = &parsed.tracks[0].notes[0];
        assert!(
            (note.duration_beats - 2.0).abs() < 1e-6,
            "got {}",
            note.duration_beats
        );
    }

    #[test]
    fn format2_は時間軸に連結する() {
        // 2 パターン (各 1 拍のノート)。4/4 なので 2 本目は 1 小節目の頭 = beat 4 から。
        let pattern = |key: u8| {
            vec![
                note_on(0, 0, key, 100),
                note_off(480, 0, key),
                eot(),
            ]
        };
        let bytes = build_smf(
            Format::Sequential,
            Timing::Metrical(u15::from(480u16)),
            vec![pattern(60), pattern(72)],
        );
        let parsed = parse(&bytes);
        assert_eq!(parsed.tracks.len(), 1, "パターンは 1 トラックに連結する");
        let notes = &parsed.tracks[0].notes;
        assert_eq!(notes.len(), 2);
        assert!((notes[0].start_beat - 0.0).abs() < 1e-9);
        assert!(
            (notes[1].start_beat - 4.0).abs() < 1e-9,
            "2 本目のパターンは次の小節から: {}",
            notes[1].start_beat
        );
    }

    #[test]
    fn ノートが無いファイルはエラー() {
        let bytes = build_smf(
            Format::SingleTrack,
            Timing::Metrical(u15::from(480u16)),
            vec![vec![
                meta(0, MetaMessage::Tempo(u24::from(500_000u32))),
                eot(),
            ]],
        );
        let err = parse_midi_bytes(&bytes, &secs_to_beat_120).unwrap_err();
        assert!(matches!(err, MidiImportError::NoNotes));
    }

    #[test]
    fn smf_でないバイト列はエラー() {
        let err = parse_midi_bytes(b"not a midi file at all", &secs_to_beat_120).unwrap_err();
        assert!(matches!(err, MidiImportError::Parse(_)));
    }

    #[test]
    fn 拡張子判定() {
        for ok in ["a.mid", "a.MIDI", "a.Smf", "song.kar", "x.rmi"] {
            assert!(looks_like_midi(Path::new(ok)), "{ok}");
        }
        for ng in ["a.wav", "a.mp4", "a.png", "a", "a.midi.wav"] {
            assert!(!looks_like_midi(Path::new(ng)), "{ng}");
        }
    }
}
