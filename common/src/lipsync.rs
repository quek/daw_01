//! 口パク (lip-sync) 生成ロジック。docs/plan_pakupaku.md §6。
//!
//! VOICEVOX の phoneme 列 (各 `frame_length` 付き) を、口形状画像の `ImageEvent`
//! 列へ変換する純粋関数。REAPER `pakupaku.lua` の挙動を移植する:
//!   - 子音は次の母音の口形状を借用 (pau / cl で打ち切り、無ければ閉口)
//!   - cl (促音) / pau (ポーズ) は閉口
//!   - 連続同形はマージ (1 つの長い `ImageEvent` にまとめる)
//!   - phoneme 列の frame 0 を呼び出し側が指定した位置に置く (音声 WAV の先頭と同じ位置)
//!   - clip 範囲外はクランプ、前後の隙間は閉口で埋める
//!
//! 副作用なし・純粋関数なので unit test しやすい。実際の HTTP 取得は
//! `crate::voicevox::query_phonemes`、生成先 clip への適用は daw_gui 側。

use crate::model::{
    Clip, ClipContent, ImageEvent, LaunchQuantize, LaunchSettings, MouthMap, MouthShape, Note,
    Song, TextEvent, Track,
};
use crate::voicevox::{Phoneme, frames_to_beats};

/// 口パク **配置ルール** の世代。生成した clip に [`crate::model::Clip::lipsync_gen`]
/// として焼き込み、load 時にこれより古い clip を見つけたら一度だけ再生成する。
///
/// **phoneme 列 → clip-local beat の対応を変えたら必ず +1 する。** 入力
/// (notes / text / bpm / mouth_map) が同じでも出力が変わる変更は fingerprint では
/// 検出できないため (合成 WAV 側の `CACHE_SCHEMA_VERSION` と対になる仕組み)。
///
/// - 1: r.md #39 — anchor を「phoneme 列 frame 0 が来る位置」に統一し、talk の
///   先頭 pau (prePhonemeLength 由来 ~96ms) を廃止
pub const PLACEMENT_GEN: u32 = 1;

/// phoneme 文字列が母音 (a/i/u/e/o/N) なら対応する口形状。子音 / cl / pau は
/// `None`。REAPER の VOWELS セット (撥音 N を含む) と同じ判定。
fn vowel_shape(phoneme: &str) -> Option<MouthShape> {
    match phoneme {
        "a" => Some(MouthShape::A),
        "i" => Some(MouthShape::I),
        "u" => Some(MouthShape::U),
        "e" => Some(MouthShape::E),
        "o" => Some(MouthShape::O),
        "N" => Some(MouthShape::N),
        _ => None,
    }
}

fn is_pause(phoneme: &str) -> bool {
    phoneme == "pau" || phoneme == "cl"
}

/// `phonemes[i]` が実際に表示すべき口形状。母音/撥音はそのまま、cl/pau は閉口、
/// 子音は次の母音を先読みして借用する (pau/cl に当たったら打ち切り = 閉口、
/// 末尾まで母音が無ければ閉口)。REAPER `pakupaku.lua` の effective_phoneme 同等。
fn effective_shape(phonemes: &[Phoneme], i: usize) -> MouthShape {
    let p = phonemes[i].phoneme.as_str();
    if let Some(s) = vowel_shape(p) {
        return s;
    }
    if is_pause(p) {
        return MouthShape::Closed;
    }
    // 子音: 次の母音の口形状を借用。
    for next in &phonemes[i + 1..] {
        if let Some(s) = vowel_shape(&next.phoneme) {
            return s;
        }
        if is_pause(&next.phoneme) {
            break;
        }
    }
    MouthShape::Closed
}

/// phoneme 列 → 口画像 `ImageEvent` 列 (clip-local beats)。
///
/// 引数:
/// - `phonemes`: `query_phonemes` の生出力 (先頭/末尾の pau 込み、frame 0 起点)。
/// - `mouth_map`: 口形状 → `ImageSourceId`。未割当 slot は閉口へ fallback。
/// - `bpm`: 曲の BPM (frame → beat 変換に使用)。
/// - `first_phoneme_local_beat`: **phoneme 列の frame 0** が来る clip-local beat。
///   これは音声 WAV の先頭が来る位置と同一 (r.md #39: 合成 buffer / phoneme 列 /
///   曲位置を「先頭を揃える」1 本の契約で結ぶ)。呼び出し側が渡す値:
///     - 歌: `voicevox::sing_head_beat(sing_base_beat(notes), bpm)` (= 基準 note の
///       `REST_FRAMES` 手前)
///     - talk: `TextEvent 開始 − voicevox::talk_pre_silence_frames()` 相当の beats
///
///   ここで先頭 pau を引く等の経路別補正は **しない** (多重 SSoT を作らない)。
/// - `clip_len_beats`: 生成先 clip の長さ。範囲外の event はクランプ/破棄する。
///
/// 戻り値の `ImageEvent` は `source_id` / `event_start_in_clip_beats` /
/// `event_length_beats` のみ設定し、rect は全画面・opacity 1・fade 0
/// (`ImageEvent::default()`)。
pub fn build_mouth_events(
    phonemes: &[Phoneme],
    mouth_map: &MouthMap,
    bpm: f32,
    first_phoneme_local_beat: f64,
    clip_len_beats: f64,
) -> Vec<ImageEvent> {
    if phonemes.is_empty() || clip_len_beats <= 0.0 || bpm <= 0.0 {
        return Vec::new();
    }

    // phoneme 列 frame 0 = 音声 WAV 先頭。以降は frame_length を積むだけ。
    let mut cursor = first_phoneme_local_beat;

    // 1) phoneme ごとの raw 区間 (source_id 付き、まだクランプ/マージ前)。
    struct Raw {
        start: f64,
        end: f64,
        source_id: u32,
    }
    let mut raw: Vec<Raw> = Vec::with_capacity(phonemes.len() + 2);
    for (i, p) in phonemes.iter().enumerate() {
        let dur = frames_to_beats(p.frame_length as f64, bpm);
        let source_id = mouth_map.resolve(effective_shape(phonemes, i));
        raw.push(Raw {
            start: cursor,
            end: cursor + dur,
            source_id,
        });
        cursor += dur;
    }

    // 2) phoneme 列が覆わない前後の隙間を閉口で埋める。
    let closed_id = mouth_map.resolve(MouthShape::Closed);
    let span_start = raw.first().map(|r| r.start).unwrap_or(0.0);
    let span_end = raw.last().map(|r| r.end).unwrap_or(0.0);
    if span_start > 0.0 {
        raw.insert(
            0,
            Raw {
                start: 0.0,
                end: span_start,
                source_id: closed_id,
            },
        );
    }
    if span_end < clip_len_beats {
        raw.push(Raw {
            start: span_end,
            end: clip_len_beats,
            source_id: closed_id,
        });
    }

    // 3) [0, clip_len] にクランプ → 連続同形マージ → ImageEvent 化。
    let mut events: Vec<ImageEvent> = Vec::new();
    for r in raw {
        let start = r.start.max(0.0);
        let end = r.end.min(clip_len_beats);
        if end - start <= 1e-9 {
            continue;
        }
        if let Some(last) = events.last_mut() {
            let last_end = last.event_start_in_clip_beats + last.event_length_beats;
            if last.source_id == r.source_id && (start - last_end).abs() <= 1e-6 {
                // 連続する同形 → 直前 event を延長してマージ。
                last.event_length_beats = end - last.event_start_in_clip_beats;
                continue;
            }
        }
        events.push(ImageEvent {
            source_id: r.source_id,
            event_start_in_clip_beats: start,
            event_length_beats: end - start,
            ..ImageEvent::default()
        });
    }

    events
}

/// `out` の末尾へ `(s, e, img)` を push する。 末尾が同一 image で連続 (端が
/// 一致) していれば区間を延長して 1 本に coalesce する。 長さ ≈ 0 は捨てる。
fn push_coalesced(out: &mut Vec<(f64, f64, u32)>, s: f64, e: f64, img: u32) {
    if e - s <= 1e-9 {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.2 == img
        && (s - last.1).abs() <= 1e-6
    {
        last.1 = e;
        return;
    }
    out.push((s, e, img));
}

/// 開き口区間 `open_spans` (song-absolute、 start 昇順・非重複) を、 `range`
/// 全体を隙間なく覆う `(start, end, image_id)` 列にする。 open span 同士の隙間と、
/// `range` 先頭 / 末尾の余りは `closed_id` (閉じ口) で埋める
/// (r.md #18: 歌もセリフも無い間は閉じ口を置く → 口が消えない)。
///
/// `closed_id == 0` (閉じ口が未割当) のときは埋めず、 `range` にクランプした
/// open span だけを返す (= 従来どおり隙間は口なし)。 隣接する同一 image は
/// 1 区間に coalesce する。 返す区間は `range` 内で非重複・start 昇順。
#[must_use]
pub fn fill_mouth_timeline(
    open_spans: &[(f64, f64, u32)],
    range: (f64, f64),
    closed_id: u32,
) -> Vec<(f64, f64, u32)> {
    let (r0, r1) = range;
    if r1 - r0 <= 1e-9 {
        return Vec::new();
    }
    let mut out: Vec<(f64, f64, u32)> = Vec::new();
    let mut cursor = r0;
    for &(s, e, img) in open_spans {
        let s = s.max(r0);
        let e = e.min(r1);
        if e - s <= 1e-9 {
            continue;
        }
        // 直前 open span との隙間を閉じ口で埋める (closed_id == 0 なら空けたまま)。
        if closed_id != 0 && s - cursor > 1e-9 {
            push_coalesced(&mut out, cursor, s, closed_id);
        }
        // open span 本体 (直前区間と重ならないよう cursor 以降にクランプ)。
        push_coalesced(&mut out, s.max(cursor), e, img);
        cursor = cursor.max(e);
    }
    // range 末尾の余りを閉じ口で埋める。
    if closed_id != 0 && r1 - cursor > 1e-9 {
        push_coalesced(&mut out, cursor, r1, closed_id);
    }
    out
}

/// **古い配置ルール** ([`PLACEMENT_GEN`]) で作られた `auto_lipsync` clip を持つ口 track の、
/// ソース vocal track id 群 (昇順・重複なし)。
///
/// r.md #39: 口パク event は project に永続化される派生データで、通常の再生成トリガは
/// 入力 fingerprint の差分だけ。配置ルール自体を変えると入力が同じままなので、この世代
/// チェックが「開いたときに一度だけ作り直す」唯一のトリガになる。現行世代しか無い
/// project では空 Vec (= 何もしない → dirty-on-open しない、r.md #9)。
///
/// r.md #87: 走査は [`Track::all_clips`] — 数えているのは口パクの**生成物**で、
/// アレンジの clip とランチャーのセルの**両方**に生えるようになった
/// (列ごとの口パクセル、[`MouthCellShape`])。`clips` だけを見ると、
/// セルにだけ口パクがある project の
/// 世代更新が黙って効かない (時間軸を扱う関数ではないので `all_clips` が正しい側)。
#[must_use]
pub fn vocal_tracks_with_outdated_lipsync(song: &Song) -> Vec<u32> {
    let outdated: Vec<u32> = song
        .tracks
        .iter()
        .filter(|t| {
            t.all_clips()
                .any(|c| c.auto_lipsync && c.lipsync_gen < PLACEMENT_GEN)
        })
        .map(|t| t.id)
        .collect();
    if outdated.is_empty() {
        return Vec::new();
    }
    let mut vocals: Vec<u32> = song
        .tracks
        .iter()
        .filter(|t| {
            t.lipsync_target_track
                .is_some_and(|target| outdated.contains(&target))
        })
        .map(|t| t.id)
        .collect();
    vocals.sort_unstable();
    vocals.dedup();
    vocals
}

/// 口 track (`mouth_track_id`) が属する立ち絵 group の「body が映っている」
/// 時間範囲 (song-absolute beats) を返す。 = 同じ group (親 = 口 track の
/// `parent_group_id`) に属する track 群 — group track 自身と直下の子 — が持つ
/// **`auto_lipsync` でない Image clip** の `start..end` の和。
///
/// 口 track が group に属さない、 または body となる Image clip が 1 つも無ければ
/// `None`。 r.md #18 (option 1「立ち絵が映っている間ずっと閉じ口」) で、 閉じ口を
/// 敷き詰める範囲を決めるのに使う。 生成物である口 clip (`auto_lipsync`) は body に
/// 含めない (自己参照を避ける)。 subtitle 等の Text clip も body ではないので除く。
#[must_use]
pub fn tachie_body_range(song: &Song, mouth_track_id: u32) -> Option<(f64, f64)> {
    let group_id = song.track_by_id(mouth_track_id)?.parent_group_id?;
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for t in &song.tracks {
        // 同じ立ち絵 group = group track 自身、 またはその直下の子 (siblings)。
        if t.id != group_id && t.parent_group_id != Some(group_id) {
            continue;
        }
        for c in &t.clips {
            if c.auto_lipsync {
                continue;
            }
            if matches!(
                song.clip_contents.get(&c.content_id),
                Some(ClipContent::Image(_))
            ) {
                lo = lo.min(c.start_beat);
                hi = hi.max(c.start_beat + c.length_beats);
            }
        }
    }
    (hi - lo > 1e-9).then_some((lo, hi))
}

// ===========================================================================
// r.md #87 — ランチャーのセルで歌ったときの口パク
//
// 設計は `docs/plan_rmd_87_clip_launcher.md` §3.7。アレンジでは「歌唱トラックの歌
// → 立ち絵トラックに 1 本の `auto_lipsync` Image clip」だが、セルは song 絶対位置を
// 持たない (`SessionClip::clip` の契約: `start_beat` は常に 0) ので、同じ形を
// **列 (`Scene`) ごとの口パクセル**として作る。撃つ契機はシーン発火で、そのとき
// 歌唱セルと同じ列の口パクセルが一緒に撃たれる。
// ===========================================================================

/// 口パクの入力になる clip の中身。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LipsyncSource<'a> {
    /// 歌唱。`base_beat` は `voicevox::sing_base_beat` の解 (= 歌う note の
    /// いちばん早い開始、content-local)。**呼び側が引き直さなくて済むよう**
    /// ここへ載せる (分類と基準拍で条件が食い違わない)。
    Sing { notes: &'a [Note], base_beat: f64 },
    /// 読み上げ。clip の**先頭の非空** `TextEvent` 1 つ。
    Talk(&'a TextEvent),
}

/// `clip` が口パクの入力になるなら、その中身。
///
/// **入力 fingerprint と phoneme query の snap 収集は必ずこの 1 本で分類すること。**
/// 片方だけ条件がずれると「歌詞を変えたのに口パクが再生成されない」/「無関係な
/// 編集のたびに再生成される」が静かに起きる (どちらもログにも `*` にも出ない)。
#[must_use]
pub fn lipsync_source_of<'a>(song: &'a Song, clip: &Clip) -> Option<LipsyncSource<'a>> {
    let content = song.clip_contents.get(&clip.content_id)?;
    if let Some(notes) = content.notes()
        && let Some(base_beat) = crate::voicevox::sing_base_beat(notes)
    {
        return Some(LipsyncSource::Sing { notes, base_beat });
    }
    let ev = content.text_events()?.iter().find(|e| !e.text.is_empty())?;
    Some(LipsyncSource::Talk(ev))
}

/// 口パクの生成結果が最終的に置かれる**入れ物**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LipsyncContainer {
    /// 口 track のアレンジ (`Track::clips` の 1 本の `auto_lipsync` clip)。
    Arrangement,
    /// 口 track の列 `scene_id` のセル (`Track::session_clips`)。
    Scene(u32),
}

/// **平坦化タイムライン**上で 1 つの入力 clip をどこへ置くか。
///
/// phoneme 列は clip-local (content 原点基準) で出てくるので、口 event の位置は
/// `origin + (content-local 拍 - shift)` で決まる。`window_len` は
/// `build_mouth_events` に渡す「その座標系での窓の長さ」。
///
/// - アレンジの clip: `origin = content_origin_beat()` / `shift = 0` →
///   従来どおり song 絶対拍。
/// - ランチャーのセル: `origin = 帯の原点` / `shift = content_offset_beats` →
///   **セル内の位相** (0 = 撃った瞬間) が帯の原点に乗る。窓の外の note が
///   隣の帯へはみ出さないよう、窓の長さはセルの `length_beats` そのもの。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatPlacement {
    pub container: LipsyncContainer,
    pub origin: f64,
    pub window_len: f64,
    pub shift: f64,
}

/// 帯どうし / 帯とアレンジの間に空ける隙間 (拍)。
///
/// 平坦化タイムラインは **口 event の区間がどの入れ物のものか判別するためだけの
/// 合成座標**で、音にも保存内容にもならない。区間は必ず自分の帯の内側に収まる
/// ([`FlatPlacement::window_len`] で clamp する) ので隙間は本来 0 でも足りるが、
/// 境界の float 比較を安全側に倒すために 1 拍空ける。
pub const LIPSYNC_BAND_GAP_BEATS: f64 = 1.0;

/// 平坦化タイムラインの 1 帯 = ランチャーの 1 列ぶん。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LipsyncBand {
    pub scene_id: u32,
    /// 帯の原点 (= その列のセルの位相 0)。
    pub base_beat: f64,
    /// 帯の長さ = その列にあるソースセルの**最大長**。
    pub len_beats: f64,
}

/// アレンジと各列を 1 本の拍軸へ重ならないように並べた表。
///
/// # なぜ平坦化するのか
///
/// phoneme query は背景スレッドで走り、結果は `AppEvent::LipsyncGenerated` で
/// 戻ってくる。複数のソーストラックが同じ口 track を共有するので、戻ってきた区間は
/// **優先度つきで 1 本にマージ**してから入れ物へ配る (上のトラックが勝つ)。
/// マージは 1 本の拍軸の上でしか定義できないので、アレンジと列を重ならない帯へ
/// 並べ、マージ後に帯で切り分けて入れ物へ戻す。
///
/// # 前提 (これが崩れると列を取り違える)
///
/// 発注時と適用時で **同じ `Song` から同じ表**が引けること。口パクの再生成は
/// `AppData::mark_lipsync_dirty` が Song 編集のたびに世代を上げ、
/// `LipsyncGenerated` は世代一致のときだけ適用されるので、飛行中に Song が
/// 変わった結果はそもそも捨てられる。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LipsyncLayout {
    /// アレンジの寄与が収まる上限拍 (= ソースのアレンジ clip の右端の最大)。
    pub arrangement_end: f64,
    /// 列の帯 (表示順、`base_beat` 昇順)。ソースセルの無い列は入らない。
    pub bands: Vec<LipsyncBand>,
}

impl LipsyncLayout {
    /// 口 track `mouth_track_id` を出力先にする全ソーストラックから表を組む。
    #[must_use]
    pub fn build(song: &Song, mouth_track_id: u32) -> Self {
        let mut arrangement_end = 0.0_f64;
        for src in lipsync_sources(song, mouth_track_id) {
            for clip in &src.clips {
                if lipsync_source_of(song, clip).is_some() {
                    arrangement_end = arrangement_end.max(clip.start_beat + clip.length_beats);
                }
            }
        }
        let mut bands = Vec::new();
        let mut cursor = arrangement_end + LIPSYNC_BAND_GAP_BEATS;
        for scene in &song.scenes {
            let Some(len) = source_cell_len(song, mouth_track_id, scene.id) else {
                continue;
            };
            bands.push(LipsyncBand { scene_id: scene.id, base_beat: cursor, len_beats: len });
            cursor += len + LIPSYNC_BAND_GAP_BEATS;
        }
        Self { arrangement_end, bands }
    }

    /// 列 `scene_id` の帯。
    #[must_use]
    pub fn band(&self, scene_id: u32) -> Option<&LipsyncBand> {
        self.bands.iter().find(|b| b.scene_id == scene_id)
    }

    /// この source track の口パク入力 clip と、平坦化タイムラインへの置き方。
    ///
    /// **fingerprint と snap 収集はこの 1 本を通す** (走査対象がずれると、
    /// 「入力が変わったのに再生成されない」が静かに起きる)。帯を持たない列
    /// (= その列にソースセルが 1 つも無い = 呼び側の表が古い) のセルは落とす。
    #[must_use]
    pub fn placements<'a>(&self, src: &'a Track) -> Vec<(&'a Clip, FlatPlacement)> {
        let mut out: Vec<(&Clip, FlatPlacement)> = src
            .clips
            .iter()
            .map(|c| {
                (
                    c,
                    FlatPlacement {
                        container: LipsyncContainer::Arrangement,
                        origin: c.content_origin_beat(),
                        window_len: c.content_offset_beats + c.length_beats,
                        shift: 0.0,
                    },
                )
            })
            .collect();
        for cell in &src.session_clips {
            let Some(band) = self.band(cell.scene_id) else {
                continue;
            };
            out.push((
                &cell.clip,
                FlatPlacement {
                    container: LipsyncContainer::Scene(cell.scene_id),
                    origin: band.base_beat,
                    window_len: cell.clip.length_beats,
                    shift: cell.clip.content_offset_beats,
                },
            ));
        }
        out
    }

    /// 平坦化タイムライン上の拍 `beat` (= 口 event 区間の開始) がどの入れ物のものか。
    /// どの帯にもアレンジにも属さない (帯の隙間) なら `None` = 捨てる。
    #[must_use]
    pub fn container_at(&self, beat: f64) -> Option<LipsyncContainer> {
        if let Some(b) = self
            .bands
            .iter()
            .find(|b| beat >= b.base_beat && beat < b.base_beat + b.len_beats)
        {
            return Some(LipsyncContainer::Scene(b.scene_id));
        }
        let first_base = self.bands.first().map_or(f64::INFINITY, |b| b.base_beat);
        (beat < first_base).then_some(LipsyncContainer::Arrangement)
    }
}

/// 口 track を出力先にするソーストラック (並び順 = 口パクの優先度)。
fn lipsync_sources(song: &Song, mouth_track_id: u32) -> impl Iterator<Item = &Track> {
    song.tracks
        .iter()
        .filter(move |t| t.lipsync_target_track == Some(mouth_track_id))
}

/// 列 `scene_id` にあるソースセル (口パクの入力を持つセル) の最大長。
fn source_cell_len(song: &Song, mouth_track_id: u32, scene_id: u32) -> Option<f64> {
    let mut len = f64::NEG_INFINITY;
    for src in lipsync_sources(song, mouth_track_id) {
        for cell in src.session_clips.iter().filter(|c| c.scene_id == scene_id) {
            if lipsync_source_of(song, &cell.clip).is_some() {
                len = len.max(cell.clip.length_beats);
            }
        }
    }
    (len > 1e-9).then_some(len)
}

/// 列 `scene_id` の口パクセルの形 = 長さと発火設定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouthCellShape {
    /// セルの長さ (= ループ周期)。
    pub len_beats: f64,
    pub quantize: LaunchQuantize,
    pub looping: bool,
    pub legato: bool,
}

impl MouthCellShape {
    /// 生成する口パクセルに載せる発火設定。
    ///
    /// **フォローアクションは写さない。** `quantize` / `looping` / `legato` は
    /// 「いつ始まって何拍で回るか」= 写さないと歌と口が最初の 1 発でズレる。
    /// 対してフォローアクションは「次にどのセルへ行くか」で、口 track の行と歌の行
    /// では空セルの位置 (= Q13 のグループ) が違うため `Next` の行き先が食い違い、
    /// `Any` / `Other` は行ごとに独立に抽選される。写すと**別の歌詞の口が動く**
    /// (= 動かないより悪い)。行を歌へ従属させる形は設計正本 §3.7 の残課題。
    #[must_use]
    pub fn launch(&self) -> LaunchSettings {
        LaunchSettings {
            quantize: self.quantize,
            looping: self.looping,
            legato: self.legato,
            ..LaunchSettings::default()
        }
    }
}

/// 列 `scene_id` に置くべき口パクセルの形。置くものが無ければ `None`。
///
/// - 長さ: その列に**ソースセルがあればその最大長** (口は歌と同じ周期で回らないと
///   2 周目からズレる)。無ければ立ち絵 body セル (同じ立ち絵 group の
///   `auto_lipsync` でない Image セル) の最大長 = 立ち絵が映っている間ずっと
///   閉じ口を置く (r.md #18 のセル版)。
/// - 発火設定: いちばん上のソーストラックのセル、無ければ body セルから写す。
#[must_use]
pub fn mouth_cell_shape(song: &Song, mouth_track_id: u32, scene_id: u32) -> Option<MouthCellShape> {
    let lead = lipsync_sources(song, mouth_track_id)
        .filter_map(|src| src.session_clip(scene_id))
        .find(|cell| lipsync_source_of(song, &cell.clip).is_some());
    if let (Some(len), Some(lead)) = (source_cell_len(song, mouth_track_id, scene_id), lead) {
        return Some(MouthCellShape {
            len_beats: len,
            quantize: lead.launch.quantize,
            looping: lead.launch.looping,
            legato: lead.launch.legato,
        });
    }
    // 歌が無い列でも、立ち絵が映っているなら閉じ口を置く。
    let (len_beats, launch) = tachie_body_cell(song, mouth_track_id, scene_id)?;
    Some(MouthCellShape {
        len_beats,
        quantize: launch.quantize,
        looping: launch.looping,
        legato: launch.legato,
    })
}

/// 口 track が属する立ち絵 group の、列 `scene_id` の **body セル**
/// (`auto_lipsync` でない Image セル) の最大長と、その代表セルの発火設定。
///
/// [`tachie_body_range`] の列版。**あちらは song 絶対拍の範囲**を返すので、
/// `start_beat` が常に 0 のセルを混ぜてはいけない ([`Track::all_clips`] の契約)。
/// こちらは範囲ではなく長さ 1 つ — セルは撃った瞬間が原点なので、始まりは常に 0。
fn tachie_body_cell(
    song: &Song,
    mouth_track_id: u32,
    scene_id: u32,
) -> Option<(f64, LaunchSettings)> {
    let group_id = song.track_by_id(mouth_track_id)?.parent_group_id?;
    let mut best: Option<(f64, LaunchSettings)> = None;
    for t in &song.tracks {
        if t.id != group_id && t.parent_group_id != Some(group_id) {
            continue;
        }
        for cell in t.session_clips.iter().filter(|c| c.scene_id == scene_id) {
            if cell.clip.auto_lipsync || cell.clip.length_beats <= 1e-9 {
                continue;
            }
            if !matches!(
                song.clip_contents.get(&cell.clip.content_id),
                Some(ClipContent::Image(_))
            ) {
                continue;
            }
            if best.as_ref().is_none_or(|(len, _)| cell.clip.length_beats > *len) {
                best = Some((cell.clip.length_beats, cell.launch.clone()));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ph(p: &str, f: u32) -> Phoneme {
        Phoneme {
            phoneme: p.into(),
            frame_length: f,
        }
    }

    fn full_map() -> MouthMap {
        MouthMap {
            a: 1,
            i: 2,
            u: 3,
            e: 4,
            o: 5,
            n: 6,
            closed: 7,
        }
    }

    /// frame → beat (テスト用、本体と同式)。
    fn b(frames: f64, bpm: f32) -> f64 {
        frames_to_beats(frames, bpm)
    }

    fn ids(events: &[ImageEvent]) -> Vec<u32> {
        events.iter().map(|e| e.source_id).collect()
    }

    /// event 列が [0, clip_len] を隙間なく連続で覆うこと。
    fn assert_contiguous(events: &[ImageEvent], clip_len: f64) {
        assert!(!events.is_empty());
        assert!((events[0].event_start_in_clip_beats).abs() < 1e-6, "starts at 0");
        let mut cur = 0.0;
        for e in events {
            assert!(
                (e.event_start_in_clip_beats - cur).abs() < 1e-6,
                "event starts where previous ended"
            );
            cur = e.event_start_in_clip_beats + e.event_length_beats;
        }
        assert!((cur - clip_len).abs() < 1e-6, "covers up to clip_len");
    }

    #[test]
    fn consonant_borrows_next_vowel_and_merges() {
        let bpm = 120.0;
        // 先頭 pau(10) → k(5) → a(20) → 末尾 pau(10)。
        let phs = vec![ph("pau", 10), ph("k", 5), ph("a", 20), ph("pau", 10)];
        // frame 0 を clip 頭に置く。
        let head = 0.0;
        let clip_len = b(45.0, bpm); // 10+5+20+10
        let events = build_mouth_events(&phs, &full_map(), bpm, head, clip_len);
        // k(子音)→a を借用し id=1、a も id=1 → マージ。前後 pau は閉口 id=7。
        assert_eq!(ids(&events), vec![7, 1, 7]);
        assert_contiguous(&events, clip_len);
        // 借用+マージ区間は frame [10,35] = b(10)..b(35)。
        assert!((events[1].event_start_in_clip_beats - b(10.0, bpm)).abs() < 1e-6);
        assert!((events[1].event_length_beats - b(25.0, bpm)).abs() < 1e-6);
    }

    #[test]
    fn distinct_vowels_are_separate_events() {
        let bpm = 100.0;
        let phs = vec![ph("pau", 10), ph("a", 10), ph("i", 10), ph("o", 10), ph("pau", 10)];
        let clip_len = b(50.0, bpm);
        let events = build_mouth_events(&phs, &full_map(), bpm, 0.0, clip_len);
        assert_eq!(ids(&events), vec![7, 1, 2, 5, 7]);
        assert_contiguous(&events, clip_len);
    }

    #[test]
    fn frame_zero_anchors_at_head_beat_and_phonemes_follow_in_order() {
        // r.md #39: 引数は「phoneme 列 frame 0 が来る clip-local beat」。歌なら
        // `sing_head_beat` (= 基準 note の REST_FRAMES 手前) を渡すので、先頭 pau(10)
        // の直後 = 基準 note 位置に最初の母音が来る。
        let bpm = 120.0;
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        let first_note = 2.0;
        let head = crate::voicevox::sing_head_beat(first_note, bpm);
        let clip_len = 8.0;
        let events = build_mouth_events(&phs, &full_map(), bpm, head, clip_len);
        let a_event = events.iter().find(|e| e.source_id == 1).expect("has 'a' event");
        assert!(
            (a_event.event_start_in_clip_beats - first_note).abs() < 1e-6,
            "first vowel starts at the base note, got {}",
            a_event.event_start_in_clip_beats
        );
        // 先頭は [0, head] の閉口 fill + 先頭 pau [head, first_note] が同じ閉口で
        // merge され、ちょうど [0, first_note] の 1 本になる。
        // (この長さ assert は先頭 fill / 連続同形マージの回帰検出そのもの。
        //  contiguous / start≈0 だけでは merge が壊れても素通りする。)
        assert_eq!(events[0].source_id, 7);
        assert!((events[0].event_start_in_clip_beats).abs() < 1e-6);
        assert!(
            (events[0].event_length_beats - first_note).abs() < 1e-6,
            "先頭閉口は [0, {first_note}] の 1 本にマージされる: {}",
            events[0].event_length_beats
        );
        assert_contiguous(&events, clip_len);
    }

    #[test]
    fn trailing_gap_filled_with_closed() {
        let bpm = 120.0;
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        // clip_len を phoneme 終端 (b(40)) より長く。
        let clip_len = b(40.0, bpm) + 1.0;
        let events = build_mouth_events(&phs, &full_map(), bpm, 0.0, clip_len);
        assert_contiguous(&events, clip_len);
        // 末尾は閉口。
        assert_eq!(events.last().unwrap().source_id, 7);
    }

    #[test]
    fn unmapped_shape_falls_back_to_closed() {
        let bpm = 120.0;
        // closed のみ割当、母音は未割当 (0)。
        let map = MouthMap {
            a: 0,
            i: 0,
            u: 0,
            e: 0,
            o: 0,
            n: 0,
            closed: 9,
        };
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        let clip_len = b(40.0, bpm);
        let events = build_mouth_events(&phs, &map, bpm, 0.0, clip_len);
        // 全部 closed(9) に解決 → 1 つにマージ。
        assert_eq!(ids(&events), vec![9]);
    }

    #[test]
    fn empty_or_invalid_inputs_yield_nothing() {
        assert!(build_mouth_events(&[], &full_map(), 120.0, 0.0, 4.0).is_empty());
        assert!(build_mouth_events(&[ph("a", 10)], &full_map(), 120.0, 0.0, 0.0).is_empty());
        assert!(build_mouth_events(&[ph("a", 10)], &full_map(), 0.0, 0.0, 4.0).is_empty());
    }

    // ---- fill_mouth_timeline (r.md #18) ------------------------------------

    #[test]
    fn fill_timeline_fills_gaps_head_and_tail_with_closed() {
        // open [2,4) img1, [6,8) img2、 range [0,10)、 closed=9。
        let filled = fill_mouth_timeline(&[(2.0, 4.0, 1), (6.0, 8.0, 2)], (0.0, 10.0), 9);
        assert_eq!(
            filled,
            vec![
                (0.0, 2.0, 9),  // 先頭埋め
                (2.0, 4.0, 1),
                (4.0, 6.0, 9),  // 中間 (歌/セリフ無し) を閉じ口で埋める
                (6.0, 8.0, 2),
                (8.0, 10.0, 9), // 末尾埋め
            ]
        );
    }

    #[test]
    fn fill_timeline_closed_unassigned_leaves_gaps() {
        // closed_id == 0 (閉じ口未割当) → 埋めず open だけ (range クランプのみ)。
        let filled = fill_mouth_timeline(&[(2.0, 4.0, 1), (6.0, 8.0, 2)], (0.0, 10.0), 0);
        assert_eq!(filled, vec![(2.0, 4.0, 1), (6.0, 8.0, 2)]);
    }

    #[test]
    fn fill_timeline_empty_open_is_all_closed() {
        // 歌もセリフも無い立ち絵 → 範囲全体を閉じ口 1 本で覆う。
        assert_eq!(fill_mouth_timeline(&[], (0.0, 4.0), 7), vec![(0.0, 4.0, 7)]);
    }

    #[test]
    fn fill_timeline_open_equal_to_closed_coalesces_whole_range() {
        // open の image が閉じ口と同じなら、 先頭埋め・本体・末尾埋めが 1 本に融合。
        assert_eq!(fill_mouth_timeline(&[(4.0, 6.0, 7)], (0.0, 10.0), 7), vec![(0.0, 10.0, 7)]);
    }

    #[test]
    fn fill_timeline_open_covers_full_range_no_closed() {
        // open が range 全体を覆う → 閉じ口は 1 つも入らない。
        assert_eq!(fill_mouth_timeline(&[(0.0, 10.0, 3)], (0.0, 10.0), 9), vec![(0.0, 10.0, 3)]);
    }

    #[test]
    fn fill_timeline_clamps_open_outside_range() {
        // range 外へはみ出す open はクランプ、 range 外は捨てる。
        let filled = fill_mouth_timeline(&[(-2.0, 3.0, 1), (8.0, 20.0, 2)], (0.0, 10.0), 9);
        assert_eq!(filled, vec![(0.0, 3.0, 1), (3.0, 8.0, 9), (8.0, 10.0, 2)]);
    }

    // ---- tachie_body_range (r.md #18) --------------------------------------

    fn img_track(id: u32, parent: Option<u32>, clips: Vec<crate::model::Clip>) -> crate::model::Track {
        crate::model::Track {
            id,
            parent_group_id: parent,
            clips,
            ..Default::default()
        }
    }

    fn img_clip(song: &mut Song, start: f64, len: f64, auto: bool) -> crate::model::Clip {
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Image(crate::model::ImageContent { events: vec![] }),
        );
        crate::model::Clip {
            id: 1,
            start_beat: start,
            length_beats: len,
            content_id: cid,
            auto_lipsync: auto,
            ..Default::default()
        }
    }

    #[test]
    fn body_range_is_union_of_group_image_clips_excluding_auto() {
        // group G(1) 直下に body image track(2) [0,8) と 口 track(3)(auto clip [0,12))。
        // body 範囲は body track の Image clip だけの和 = [0,8)。 auto_lipsync は除外。
        let mut song = Song::default();
        let body = img_clip(&mut song, 0.0, 8.0, false);
        let auto = img_clip(&mut song, 0.0, 12.0, true);
        song.tracks.push(img_track(1, None, vec![]));        // group container
        song.tracks.push(img_track(2, Some(1), vec![body])); // body 立ち絵
        song.tracks.push(img_track(3, Some(1), vec![auto])); // 口 track (auto)
        assert_eq!(tachie_body_range(&song, 3), Some((0.0, 8.0)));
    }

    #[test]
    fn body_range_none_without_group() {
        let mut song = Song::default();
        song.tracks.push(img_track(5, None, vec![]));
        assert_eq!(tachie_body_range(&song, 5), None);
    }

    // ---- 配置ルールの世代 (r.md #39) ---------------------------------------

    /// vocal track(1) → 口 track(2)。口 track に指定世代の auto clip を 1 本。
    fn lipsync_gen_song(generation: u32) -> Song {
        let mut song = Song::default();
        let mut auto = img_clip(&mut song, 0.0, 8.0, true);
        auto.lipsync_gen = generation;
        song.tracks.push(crate::model::Track {
            id: 1,
            lipsync_target_track: Some(2),
            ..Default::default()
        });
        song.tracks.push(img_track(2, None, vec![auto]));
        song
    }

    #[test]
    fn outdated_lipsync_generation_is_detected_and_current_is_not() {
        // 旧世代 (0 = 世代を持たない旧 file) → ソース vocal track を再生成対象に。
        assert_eq!(
            vocal_tracks_with_outdated_lipsync(&lipsync_gen_song(0)),
            vec![1]
        );
        // 現行世代 → 何もしない (= 開いただけで dirty にならない、r.md #9)。
        assert!(
            vocal_tracks_with_outdated_lipsync(&lipsync_gen_song(PLACEMENT_GEN)).is_empty()
        );
    }

    #[test]
    fn hand_placed_clips_never_trigger_regeneration() {
        // auto_lipsync == false の手置き clip は世代 0 でも対象外。
        let mut song = Song::default();
        let manual = img_clip(&mut song, 0.0, 8.0, false);
        song.tracks.push(crate::model::Track {
            id: 1,
            lipsync_target_track: Some(2),
            ..Default::default()
        });
        song.tracks.push(img_track(2, None, vec![manual]));
        assert!(vocal_tracks_with_outdated_lipsync(&song).is_empty());
    }

    // ---- 平坦化タイムライン (r.md #87) --------------------------------------

    /// 歌 track(1, 出力先 = 口 track 9) + 口 track(9)。`cells` は
    /// `(scene_id, length_beats)`。アレンジには `[0, arr_len)` の歌 clip を 1 本。
    fn launcher_song(arr_len: f64, cells: &[(u32, f64)]) -> Song {
        use crate::model::{Clip, LaunchSettings, MidiContent, Note, SessionClip, Track};
        let mut song = Song::default();
        let sung = |song: &mut Song| {
            let cid = song.alloc_content_id();
            song.clip_contents.insert(
                cid,
                ClipContent::Midi(MidiContent {
                    notes: vec![Note {
                        start_beat: 1.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        ..Default::default()
                    }],
                    next_note_id: 2,
                }),
            );
            cid
        };
        let arr_cid = sung(&mut song);
        let mut session_clips = Vec::new();
        for (i, &(scene_id, len)) in cells.iter().enumerate() {
            let cid = sung(&mut song);
            song.scenes.push(crate::model::Scene::new(scene_id));
            session_clips.push(SessionClip {
                scene_id,
                clip: Clip {
                    id: 10 + i as u32,
                    start_beat: 0.0,
                    length_beats: len,
                    content_id: cid,
                    ..Default::default()
                },
                launch: LaunchSettings::default(),
            });
        }
        song.tracks = vec![
            Track {
                id: 1,
                lipsync_target_track: Some(9),
                clips: vec![Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: arr_len,
                    content_id: arr_cid,
                    ..Default::default()
                }],
                session_clips,
                ..Default::default()
            },
            Track { id: 9, ..Default::default() },
        ];
        song
    }

    #[test]
    fn 帯はアレンジとも互いとも重ならず入れ物へ戻せる() {
        // ここが壊れると **静かに** 壊れる — 別の列の口が動く / 曲頭に口パクが湧く。
        let mut song = launcher_song(8.0, &[(7, 4.0), (8, 2.0)]);
        // 左端を trim したセル (窓の外に note がある) を混ぜる — `shift` を効かせないと
        // 窓の外の口が手前の帯へはみ出し、**別の列の口が動く**。
        song.tracks[0].session_clips[1].clip.content_offset_beats = 1.0;
        let layout = LipsyncLayout::build(&song, 9);
        assert_eq!(layout.arrangement_end, 8.0);
        assert_eq!(layout.bands.len(), 2);
        // 帯は昇順で、隣とアレンジの間に隙間がある。
        let b0 = layout.bands[0];
        let b1 = layout.bands[1];
        assert!(b0.base_beat > layout.arrangement_end);
        assert!(b1.base_beat >= b0.base_beat + b0.len_beats);
        // 発注 (placements) → 適用 (container_at) の往復で入れ物が一致する。
        // 発注側が置く区間は `origin + [0, window_len]` (= apply 側の
        // `clip_start_beat + event_start_in_clip_beats`) なので、その全域を見る。
        let src = song.track_by_id(1).unwrap();
        for (clip, place) in layout.placements(src) {
            for frac in [0.0, 0.5, 0.999] {
                let beat = place.origin + place.window_len * frac;
                assert_eq!(
                    layout.container_at(beat),
                    Some(place.container),
                    "clip {} の {frac} 地点が別の入れ物へ落ちた",
                    clip.id
                );
            }
        }
    }

    #[test]
    fn 歌の無い列には帯を作らない() {
        use crate::model::{Clip, LaunchSettings, SessionClip};
        // 列 7 に「歌わないセル」(空 content) だけ置く → 帯なし = 発注対象外。
        let mut song = launcher_song(4.0, &[]);
        let cid = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Image(crate::model::ImageContent { events: vec![] }));
        song.scenes.push(crate::model::Scene::new(7));
        song.tracks[0].session_clips.push(SessionClip {
            scene_id: 7,
            clip: Clip { id: 10, length_beats: 4.0, content_id: cid, ..Default::default() },
            launch: LaunchSettings::default(),
        });
        let layout = LipsyncLayout::build(&song, 9);
        assert!(layout.bands.is_empty());
        // 帯の無い列のセルは placements にも出ない (fingerprint と snap が揃う)。
        let src = song.track_by_id(1).unwrap();
        assert!(
            layout
                .placements(src)
                .iter()
                .all(|(_, p)| p.container == LipsyncContainer::Arrangement)
        );
    }

    #[test]
    fn 列の口パクセルの長さは歌のセルに従い立ち絵セルへは伸びない() {
        use crate::model::{Clip, LaunchQuantize, LaunchSettings, SessionClip, Track};
        // 立ち絵 group G(20): body track(21) と 口 track(9)。
        let mut song = launcher_song(0.0, &[(7, 4.0)]);
        song.tracks[0].clips.clear();
        song.tracks[1].parent_group_id = Some(20);
        let body_cid = song.alloc_content_id();
        song.clip_contents
            .insert(body_cid, ClipContent::Image(crate::model::ImageContent { events: vec![] }));
        song.tracks.push(Track { id: 20, ..Default::default() });
        song.tracks.push(Track {
            id: 21,
            parent_group_id: Some(20),
            session_clips: vec![SessionClip {
                scene_id: 7,
                clip: Clip { id: 1, length_beats: 16.0, content_id: body_cid, ..Default::default() },
                launch: LaunchSettings { quantize: LaunchQuantize::Off, ..Default::default() },
            }],
            ..Default::default()
        });
        // 歌のセル (4 拍) がある列 → 口も 4 拍で回る (立ち絵の 16 拍へ伸ばすと
        // 2 周目以降で口が閉じたままになる)。
        let shape = mouth_cell_shape(&song, 9, 7).expect("歌のセルがある列");
        assert_eq!(shape.len_beats, 4.0);
        // 歌のセルを消すと、立ち絵セルの長さで閉じ口だけを敷く (r.md #18 の列版)。
        song.tracks[0].session_clips.clear();
        let shape = mouth_cell_shape(&song, 9, 7).expect("立ち絵セルがある列");
        assert_eq!(shape.len_beats, 16.0);
        assert_eq!(shape.quantize, LaunchQuantize::Off, "立ち絵セルの発火設定を写す");
        // 立ち絵セルも無い列には何も置かない。
        assert_eq!(mouth_cell_shape(&song, 9, 8), None);
    }

    #[test]
    fn outdated_mouth_track_without_source_yields_nothing() {
        // 口 track だけ残ってソース vocal が消えている project では再生成できない。
        let mut song = Song::default();
        let mut auto = img_clip(&mut song, 0.0, 8.0, true);
        auto.lipsync_gen = 0;
        song.tracks.push(img_track(2, None, vec![auto]));
        assert!(vocal_tracks_with_outdated_lipsync(&song).is_empty());
    }
}
