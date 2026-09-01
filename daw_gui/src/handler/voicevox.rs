//! handler::voicevox — VOICEVOX 歌唱/トーク status + vocal metadata + lipsync
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
//! 口パクの**生成物を組み立てる純関数**は [`mouth_rebuild`] に分けてある
//! (`&mut Song` を目標形へ畳むだけの側と、発注 / debounce / IPC の側)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use common::model::Clip;
use common::plugin_format::PluginFormat;
use common::protocol::{PluginCommand, VocalSynthFailure};

/// 口パクの生成物 (アレンジの clip / 列のセル) を目標形へ畳む純関数群。
/// `AppData` 側の面倒 (発注 / debounce / binding) とは分けて持つ。
mod mouth_rebuild;

use mouth_rebuild::{rebuild_mouth_clip, rebuild_mouth_containers, split_spans_by_container};

/// 口パクの入力 clip / セル 1 つを fingerprint へ混ぜる
/// ([`AppData::lipsync_input_fingerprint`] の 1 要素ぶん)。
///
/// 混ぜるのは **出力を変える値だけ**。位置はセルとアレンジで意味が違う —
/// セルは song 絶対位置を持たない (`start_beat` は常に 0) ので列 id が位置そのもので、
/// アレンジの clip は `start_beat` が効く。
fn hash_lipsync_clip(
    h: &mut std::collections::hash_map::DefaultHasher,
    clip: &Clip,
    place: &common::lipsync::FlatPlacement,
    source: &common::lipsync::LipsyncSource<'_>,
) {
    use std::hash::Hash;
    place.container.hash(h);
    if place.container == common::lipsync::LipsyncContainer::Arrangement {
        clip.start_beat.to_bits().hash(h);
    }
    // r.md #44: 窓 (offset) も配置に効くので fingerprint に含める
    // (含めないと端 trim しても口パクが再生成されない)。
    clip.length_beats.to_bits().hash(h);
    clip.content_offset_beats.to_bits().hash(h);
    match source {
        // sing: build_sing_query が読む note フィールドのみ
        // (`velocity` / `muted` は phoneme へ影響しないので含めない)。
        common::lipsync::LipsyncSource::Sing { notes, .. } => {
            for n in *notes {
                n.start_beat.to_bits().hash(h);
                n.duration_beats.to_bits().hash(h);
                n.pitch.hash(h);
                n.lyric.hash(h);
            }
        }
        // talk: 先頭の非空 TextEvent + 声 + 話速のみ (pitch / intonation / volume は
        // phoneme 長に効かないので含めない)。
        common::lipsync::LipsyncSource::Talk(ev) => {
            ev.text.hash(h);
            ev.event_start_in_clip_beats.to_bits().hash(h);
            clip.speaker_id.hash(h);
            clip.talk.unwrap_or_default().speed_scale.to_bits().hash(h);
        }
    }
}

/// 歌の phoneme query 1 件ぶんの発注:
/// `(clip_start_beat, clip_len_beats, first_phoneme_local_beat, priority, notes)`。
/// 前 3 つの意味は [`LipsyncClipResult`] と同じ (背景スレッドがそのまま詰め替える)。
type SingSnap = (f64, f64, f64, u32, Vec<common::model::Note>);

/// 読み上げ (talk) の発注。[`SingSnap`] の notes を
/// `(text, speaker_id, talk スケール)` に置き換えたもの。
type TalkSnap = (f64, f64, f64, u32, String, u32, common::model::TalkParams);

/// 口 track `target_id` の口パクを作り直すのに要る phoneme query の発注を組む。
///
/// (talk) target 中心: 出力先が `target_id` の **全ソーストラック**をまとめる
/// (`docs/plan_voicevox_talk.md`)。トラック並び順 index を priority にし、apply 側で
/// 重なりを上位優先で解決する。各 clip は notes (歌唱) があれば sing、無く Text なら talk。
///
/// r.md #87: アレンジの clip とランチャーのセルを **1 本の平坦化タイムライン**へ並べる
/// ([`common::lipsync::LipsyncLayout`] の doc に理由)。セルの区間は「撃った瞬間 = 0」の
/// 位相座標で、`place.shift` (= セルの `content_offset_beats`) を引くことで**窓の外の
/// note が隣の列の帯へはみ出さない**。
fn collect_lipsync_snaps(
    song: &common::model::Song,
    target_id: u32,
    bpm: f32,
) -> (Vec<SingSnap>, Vec<TalkSnap>) {
    let mut snaps: Vec<SingSnap> = Vec::new();
    let mut talk_snaps: Vec<TalkSnap> = Vec::new();
    let layout = common::lipsync::LipsyncLayout::build(song, target_id);
    for (idx, src) in song.tracks.iter().enumerate() {
        if src.lipsync_target_track != Some(target_id) {
            continue;
        }
        let priority = idx as u32;
        for (clip, place) in layout.placements(src) {
            match common::lipsync::lipsync_source_of(song, clip) {
                // sing: phoneme 列 frame 0 は「基準ノートの `REST_FRAMES` 手前」に来る
                // (= 合成 wav 先頭と同じ位置、r.md #39)。r.md #44: phoneme は
                // content-local 起点なので原点基準で置き、長さの上限は窓の末尾にする。
                Some(common::lipsync::LipsyncSource::Sing { notes, base_beat }) => {
                    let head = common::voicevox::sing_head_beat(base_beat, bpm) - place.shift;
                    snaps.push((place.origin, place.window_len, head, priority, notes.to_vec()));
                }
                // talk: phoneme 列 frame 0 = wav 先頭 = 「発話開始の pre-silence 分手前」
                // (現行 pre-silence は 0 なので event 開始)。
                Some(common::lipsync::LipsyncSource::Talk(ev)) => {
                    let scales = clip.talk.unwrap_or_default();
                    let pre_frames = common::voicevox::talk_pre_silence_frames(scales.speed_scale);
                    let pre = common::voicevox::frames_to_beats(f64::from(pre_frames), bpm);
                    let head = ev.event_start_in_clip_beats - pre - place.shift;
                    talk_snaps.push((
                        place.origin,
                        place.window_len,
                        head,
                        priority,
                        ev.text.clone(),
                        clip.speaker_id,
                        scales,
                    ));
                }
                None => {}
            }
        }
    }
    (snaps, talk_snaps)
}


/// r.md #87: ランチャーのセル用の仮想区間どうし (およびアレンジとの間) に空ける隙間 (秒)。
///
/// フレーズ分割 (`common::voicevox_phrase::split_into_phrases`) は note の隙間で必ず
/// 切れ、継ぎ目のクロスフェード (`phrase_window`) も `PHRASE_PAD_FRAMES` (= 47 frame
/// ≒ 0.5 秒) 程度までしか届かない。2 秒あればセル同士・セルとアレンジのフレーズが
/// 混ざらない (歌の先頭 rest `REST_FRAMES` ≒ 0.107 秒も含めて余裕がある)。
const CELL_REGION_GAP_SECS: f64 = 2.0;

/// この track の「合成対象クリップ」と、それを合成タイムラインへ置く**区間の原点** (拍)。
///
/// - アレンジのクリップ (`Track::clips`) は原点 `0.0` = song 拍そのまま。
/// - ランチャーのセル (`Track::session_clips`) は song 絶対位置を持たない
///   (`clip.start_beat` は常に 0) ので、**アレンジの終端より後ろ**へセルごとの
///   専用区間を確保し、その先頭を原点にする。ここを 0 のままにすると、撃って
///   いないセルの歌声が曲頭から鳴る (= 無音を幻の歌声に置き換えるだけ)。
///
/// 配置は `clip.id` 昇順の決定論。フレーズ WAV のキャッシュキーは平行移動不変
/// (`voicevox_render` の「単体 query は平行移動不変」) なので、アレンジを伸ばして
/// 区間がずれても再合成は起きない (mix し直すだけ)。
fn synth_clips_with_base<'a>(
    song: &common::model::Song,
    track: &'a common::model::Track,
    bpm: f32,
) -> Vec<(&'a Clip, f64)> {
    let bpm = bpm.max(0.001);
    let gap = CELL_REGION_GAP_SECS * f64::from(bpm) / 60.0;
    let mut out: Vec<(&Clip, f64)> = track.clips.iter().map(|c| (c, 0.0)).collect();
    if track.session_clips.is_empty() {
        return out;
    }
    // アレンジが占める拍の終端 (クリップが 1 つも無ければ曲頭)。
    let arrangement_end = track
        .clips
        .iter()
        .map(|c| arrangement_synth_end_beat(song, c, bpm))
        .fold(0.0_f64, f64::max);
    let mut cells: Vec<&common::model::SessionClip> = track.session_clips.iter().collect();
    cells.sort_by_key(|s| s.clip.id);
    let mut cursor = arrangement_end + gap;
    for s in cells {
        out.push((&s.clip, cursor));
        cursor += s.clip.length_beats.max(0.0) + gap;
    }
    out
}

/// 読み上げ (talk) の WAV 長は **合成してみるまで分からない** (テキスト → 秒の
/// 予測式を持たない) ので、Text クリップを持つトラックではセル区間をこの秒数ぶん
/// 余分に後ろへずらす。1 つの `TextEvent` = 字幕 1 行なので、これを超える発話は
/// 現実的に無い。足りなかった場合も builtin 側が区間の境界で書き込みを打ち切るので
/// **セルの音に混ざることはない** (末尾が切れるだけ)。
const TALK_TAIL_ALLOWANCE_SECS: f64 = 60.0;

/// アレンジのクリップ 1 つが合成タイムラインで占める終端の拍。
///
/// **クリップの窓だけでは足りない** — [`collect_sing_metadata`] /
/// [`collect_talk_metadata`] は窓の外にある content の note / TextEvent も
/// metadata に載せるので、窓の終端で測るとセルの仮想区間がアレンジの尻尾に
/// 食い込む (= セルの歌に前の歌が混ざる)。実際に置かれる拍で測る。
fn arrangement_synth_end_beat(song: &common::model::Song, clip: &Clip, bpm: f32) -> f64 {
    let mut end = clip.start_beat + clip.length_beats;
    let Some(content) = song.clip_contents.get(&clip.content_id) else {
        return end;
    };
    if let Some(notes) = content.notes() {
        for n in notes {
            end = end.max(clip.content_to_song_beat(n.start_beat) + n.duration_beats);
        }
    }
    if let Some(events) = content.text_events() {
        let allowance = TALK_TAIL_ALLOWANCE_SECS * f64::from(bpm) / 60.0;
        for ev in events.iter().filter(|e| !e.text.is_empty()) {
            end = end
                .max(clip.content_to_song_beat(ev.event_start_in_clip_beats) + allowance);
        }
    }
    end
}

/// 1 clip の notes を builtin VOICEVOX 向けの [`NoteMetadata`] へ変換する。
///
/// `note_id` は **安定 id** (アーキ不変条件 1): `(clip.id, note.id)` から
/// [`common::plugin_metadata::sing_note_id`] で決定論的に導出する。daw_audio の
/// sequencer が **同じ関数**で同じ値を作るので、「クリップ先頭に 1 音足すと以降の
/// 全 note_id がずれる」が起きない。
///
/// `start_beat` は content-local → song-absolute (r.md #44: note は content 原点基準)、
/// さらに `base_beat` (= [`synth_clips_with_base`] が割り当てた区間の原点) を足す。
/// アレンジのクリップは `base_beat == 0.0` なので従来と同じ値になる。
/// `clip_id` は `note_id` の導出元であり、合成進捗のクリップ帰属にも使う (r.md #75)。
/// `speaker_id` は per-clip 歌唱声 (`0` = builtin 側で `DEFAULT_SINGER_ID` へフォールバック)。
fn collect_sing_metadata(
    song: &common::model::Song,
    clip: &Clip,
    base_beat: f64,
) -> Vec<common::plugin_metadata::NoteMetadata> {
    let notes: &[common::model::Note] = song
        .clip_contents
        .get(&clip.content_id)
        .and_then(|c| c.notes())
        .unwrap_or(&[]);
    notes
        .iter()
        .map(|n| common::plugin_metadata::NoteMetadata {
            note_id: common::plugin_metadata::sing_note_id(clip.id, n.id),
            start_beat: base_beat + clip.content_to_song_beat(n.start_beat),
            duration_beats: n.duration_beats,
            pitch: n.pitch,
            velocity: n.velocity,
            lyric: n.lyric.clone().unwrap_or_default(),
            clip_id: clip.id,
            speaker_id: clip.speaker_id,
            cell_base_beat: base_beat,
        })
        .collect()
}

/// (talk) 1 clip の `ClipContent::Text` を [`TalkMetadata`] へ変換する
/// (`docs/plan_voicevox_talk.md` §3.2)。
///
/// `event_id` は `talk_event_id(clip.id, event_index)` で決定論的に導出する
/// (sequencer の talk-trigger と同式)。**空テキストは両側で skip** して event_id の
/// 対応を保つ。声は per-clip (`clip.speaker_id` を talk style として解釈)、
/// スケールは `clip.talk`。`clip_id` は合成進捗のクリップ帰属 (r.md #75) —
/// これが無いと Text クリップにスピナーが一切点かない。
fn collect_talk_metadata(
    song: &common::model::Song,
    clip: &Clip,
    base_beat: f64,
) -> Vec<common::plugin_metadata::TalkMetadata> {
    let Some(events) = song
        .clip_contents
        .get(&clip.content_id)
        .and_then(|c| c.text_events())
    else {
        return Vec::new();
    };
    let scales = clip.talk.unwrap_or_default();
    events
        .iter()
        .enumerate()
        .filter(|(_, ev)| !ev.text.is_empty())
        .map(|(event_index, ev)| common::plugin_metadata::TalkMetadata {
            event_id: common::plugin_metadata::talk_event_id(clip.id, event_index as u32),
            start_beat: base_beat + clip.content_to_song_beat(ev.event_start_in_clip_beats),
            text: ev.text.clone(),
            speaker_id: clip.speaker_id,
            speed_scale: scales.speed_scale,
            pitch_scale: scales.pitch_scale,
            intonation_scale: scales.intonation_scale,
            volume_scale: scales.volume_scale,
            clip_id: clip.id,
            cell_base_beat: base_beat,
        })
        .collect()
}

impl AppData {
    // ===== VOICEVOX 生成状態の可視化 ==========================

    /// `VoicevoxSynthStatus` IPC handler。per-plugin の busy + 失敗種別 + 進捗を更新する。
    /// `Unreachable` の立上りで `failing_since` を記録 (継続中は維持) して 5s 後に
    /// 「engine 未接続」へ、`Rejected` は `rejected` に理由を積んで即時「合成できない歌詞」
    /// 表示へ。成功 (`None`) で両方クリア。idle かつ失敗なしの entry は掃除して
    /// `voicevox_any_generating` 等の判定を軽く保つ (= 進捗もクリアされる)。
    pub(crate) fn apply_voicevox_synth_status(
        &mut self,
        device_id: u64,
        progress: common::protocol::VocalSynthProgress,
    ) {
        let now = std::time::Instant::now();
        let failure = progress.failure.clone();
        let entry = self
            .voicevox
            .voicevox_synth_status
            .entry(device_id)
            .or_default();
        entry.progress = progress;
        match failure {
            VocalSynthFailure::None => {
                entry.failing_since = None;
                entry.rejected = None;
            }
            VocalSynthFailure::Unreachable => {
                entry.failing_since.get_or_insert(now);
                entry.rejected = None;
            }
            VocalSynthFailure::Rejected { detail } => {
                entry.failing_since = None;
                entry.rejected = Some(detail);
            }
        }
        if !entry.progress.busy && entry.failing_since.is_none() && entry.rejected.is_none() {
            self.voicevox.voicevox_synth_status.remove(&device_id);
        }
    }

    /// engine には到達できているが歌詞等を拒否した「内容エラー」の理由 (あれば最初の 1 件)。
    /// overlay が「合成できない歌詞があります」表示に使う。
    pub fn voicevox_rejected_detail(&self) -> Option<&str> {
        self.voicevox
            .voicevox_synth_status
            .values()
            .find_map(|s| s.rejected.as_deref())
    }

    /// track が持つ builtin VOICEVOX device の安定 device id (= load 済なら)。
    /// `sync_vocal_metadata` の lookup と同じ (device 実在 → `loaded_devices` で
    /// load 確認)。
    pub fn voicevox_plugin_id_for_track(&self, track: &common::model::Track) -> Option<u64> {
        track
            .devices
            .iter()
            .find(|d| {
                d.format == PluginFormat::Builtin
                    && d.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX
            })
            .map(|d| d.id)
            .filter(|id| self.ipc.loaded_devices.contains_key(id))
    }

    /// track の歌唱/読み上げ WAV 合成が進行中か (= 所属 builtin VOICEVOX が busy)。
    pub fn track_wav_synthesizing(&self, track_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else {
            return false;
        };
        self.voicevox.voicevox_synth_status.get(&pid).is_some_and(|s| s.progress.busy)
    }

    /// このクリップに **未完了フレーズが掛かっているか** (= クリップ上スピナーの点灯条件)。
    /// 「トラックが busy」ではないので、1 ノート直しただけで同トラックの全クリップが
    /// 回ることがなくなる (r.md #75)。
    pub fn clip_wav_synthesizing(&self, track_id: u32, clip_id: u32) -> bool {
        let Some(track) = self.song_doc.song().tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pid) = self.voicevox_plugin_id_for_track(track) else {
            return false;
        };
        self.voicevox
            .voicevox_synth_status
            .get(&pid)
            .is_some_and(|s| s.progress.pending_clips.binary_search(&clip_id).is_ok())
    }

    /// 曲中の builtin VOICEVOX device の安定 id (load 済のものだけ、track 順)。
    /// 書き出し前の合成完了ゲートが「誰を待つか」を決めるのに使う。
    pub(crate) fn all_vocal_synth_device_ids(&self) -> Vec<u64> {
        self.song_doc
            .song()
            .tracks
            .iter()
            .filter(|t| t.is_voicevox_vocal())
            .filter_map(|t| self.voicevox_plugin_id_for_track(t))
            .collect()
    }

    /// 出力先 (口 track) が口パク再生成中か。
    pub fn lipsync_target_generating(&self, track_id: u32) -> bool {
        self.voicevox.lipsync_inflight.contains(&track_id)
    }

    /// いずれかの VOICEVOX 生成 (WAV 合成 / 口パク) が進行中か。
    pub fn voicevox_any_generating(&self) -> bool {
        !self.voicevox.lipsync_inflight.is_empty()
            || self.voicevox.voicevox_synth_status.values().any(|s| s.progress.busy)
    }

    /// 合成待ちのフレーズ総数 (= 全体オーバーレイの「残り N フレーズ」)。
    /// r.md #75 で合成の最小単位がフレーズになったので、旧
    /// `voicevox_synth_busy_count` (= busy な track 数) は意味を失った。
    pub fn voicevox_pending_phrase_count(&self) -> u32 {
        self.voicevox
            .voicevox_synth_status
            .values()
            .map(|s| s.progress.pending)
            .sum()
    }

    /// r.md #75: 合成の塊の長さ (秒) を設定する。
    ///
    /// ドラッグ中 (`commit = false`) は表示値だけ動かし、確定 (release / 数値入力 /
    /// ダブルクリックリセット) でだけ保存と再合成を行う。**これは合成結果を変える入力**
    /// なので、確定時は差分キャッシュを落として全 device へ流し直す (= 曲全体の再合成)。
    /// 落とさないと再送されず、フレーズ WAV のキャッシュキーに混ぜてある以上
    /// 「設定が効かない」ように見える。
    pub(crate) fn set_voicevox_chunk_secs(&mut self, secs: f32, commit: bool) {
        let next = secs.clamp(
            common::voicevox_phrase::MIN_CHUNK_SECS,
            common::voicevox_phrase::MAX_CHUNK_SECS,
        );
        if (next - self.ui_prefs.voicevox_chunk_secs).abs() >= 1e-3 {
            self.ui_prefs.voicevox_chunk_secs = next;
        }
        if !commit {
            return;
        }
        self.persist_app_config();
        self.voicevox.voicevox_metadata_sent.clear();
        self.sync_vocal_metadata();
    }

    /// 別プロジェクトへ切り替えるときに捨てる VOICEVOX の in-flight / 送信記憶。
    ///
    /// device_id は project ごとに再割当されるので、stale entry が誤 hit すると
    /// 新 project の vocal device に seed 合成が飛ばず無音になる。書き出しの
    /// 合成完了ゲートも同様に畳む (前の曲の device を待ち続けない)。
    pub(crate) fn reset_voicevox_sync_state(&mut self) {
        self.voicevox.voicevox_metadata_sent.clear();
        self.voicevox.priority_sent.clear();
        self.ipc.pending_vocal_synth_export.clear();
    }

    /// アプリ設定の「合成の塊の長さ」(秒) を有効範囲へクランプして返す。
    /// SSoT は `app_config.json` の `voicevox_chunk_secs`。
    pub(crate) fn voicevox_chunk_secs(&self) -> f32 {
        self.ui_prefs.voicevox_chunk_secs.clamp(
            common::voicevox_phrase::MIN_CHUNK_SECS,
            common::voicevox_phrase::MAX_CHUNK_SECS,
        )
    }

    /// engine 未接続警告を出すべきか (= busy のまま failing が閾値以上継続)。
    /// engine boot (数秒) の間は failing でも警告せず「合成中」に見せ、閾値超過で切り替える。
    pub fn voicevox_engine_unreachable(&self, now: std::time::Instant) -> bool {
        self.voicevox.voicevox_synth_status.values().any(|s| {
            s.progress.busy
                && s.failing_since
                    .is_some_and(|t| now.duration_since(t) >= VOICEVOX_ENGINE_WARNING)
        })
    }

    /// スピナーを回し続ける (= 連続再描画を要求する) べきか。engine 未接続が
    /// 確定したら static 警告にして再描画を止める (CPU spin させない)。
    pub fn voicevox_animating(&self, now: std::time::Instant) -> bool {
        self.voicevox_any_generating() && !self.voicevox_engine_unreachable(now)
    }

    /// PR-V3: track.source = Vocal で instrument に builtin VOICEVOX が
    /// load されている全 track の clip notes を `NoteMetadata` 配列に
    /// 変換し、 plugin host に `SetBuiltinPluginNoteMetadata` で送る。
    /// plugin_id 未確定 (= load 完了通知前) の track はスキップ、
    /// `SlotPluginLoadedFromChild` 受信時に再呼び出しされる。
    ///
    /// PR-V4 follow-up: vocal track が 1 つでも存在するなら VOICEVOX
    /// engine を lazy spawn する。 旧 `begin_vocal_synth` 内にあった
    /// 起動 logic を移植 (= localhost:50021 が起動済でなければ自動で
    /// spawn、 builtin plugin の HTTP synth を成功させる前提)。
    pub fn sync_vocal_metadata(&mut self) {
        let bpm = self.song_doc.song().bpm;
        // r.md #75: 塊 (= `/sing_frame_audio_query` 1 回) の長さ。アプリ設定が SSoT。
        let chunk_secs = self.voicevox_chunk_secs();
        let has_vocal_track = self.song_doc.song().tracks.iter().any(|t| t.is_voicevox_vocal());
        if has_vocal_track {
            self.ensure_voicevox_engine();
        }
        for track in &self.song_doc.song().tracks {
            if !track.is_voicevox_vocal() {
                continue;
            }
            // builtin VOICEVOX を chain 内に持つ device の安定 id
            // (`loaded_devices` に居ること = load 完了確認込み)。
            let Some(host_plugin_id) = self.voicevox_plugin_id_for_track(track) else {
                continue;
            };

            let song = self.song_doc.song();
            // 全 clip (アレンジ + ランチャーのセル) の notes / TextEvent を metadata
            // 配列へ flatten する (組み立ての規則は下の 3 つの純粋関数が持つ)。
            // **セルを落とすと撃っても無音**、**原点を 0 のまま足すと撃たなくても
            // 曲頭で歌い出す** ので、走査と原点は必ず `synth_clips_with_base` 1 本で
            // 揃える (r.md #87)。
            let clips = synth_clips_with_base(song, track, bpm);
            let entries: Vec<common::plugin_metadata::NoteMetadata> = clips
                .iter()
                .flat_map(|&(clip, base)| collect_sing_metadata(song, clip, base))
                .collect();
            let talk: Vec<common::plugin_metadata::TalkMetadata> = clips
                .iter()
                .flat_map(|&(clip, base)| collect_talk_metadata(song, clip, base))
                .collect();
            // r.md #27: この device の歌唱/読み上げ入力 (bpm/notes/talk) が前回送信から
            // 変わっていなければ再送しない (= builtin plugin が不要な再合成を走らせない)。
            // `sync_vocal_metadata` は epoch bump のたびに呼ばれるので、Transform 等の
            // 非 vocal 編集ではここが cache hit → send skip になる。`sync_ara_documents`
            // の差分キャッシュと同 idiom。
            // r.md #75: `chunk_secs` は **合成結果を変える入力**なので比較に含める
            // (含めないと設定つまみを動かしても再送されない)。**playhead はここに
            // 入れない** — 入れるとトランスポートのたびに再合成になる (順序ヒントは
            // 専用の `SetVocalSynthPriority`)。
            if self
                .voicevox
                .voicevox_metadata_sent
                .get(&host_plugin_id)
                .is_some_and(|(b, c, e, t)| {
                    *b == bpm && *c == chunk_secs && *e == entries && *t == talk
                })
            {
                continue;
            }
            self.voicevox.voicevox_metadata_sent.insert(
                host_plugin_id,
                (bpm, chunk_secs, entries.clone(), talk.clone()),
            );
            // r.md #87: 順序ヒントは metadata より **先に**送る。合成順序は job を
            // 積んだ瞬間に 1 度だけ決まる (`PhraseRenderer::new` が hint を読む) ので、
            // 「busy になってから送る」経路 (`send_vocal_synth_priority_if_moved`) だけ
            // だと、**その job には前回の位置が効く**。セルに歌詞を書いた最初の 1 回が
            // 一番待たされるのに、そこだけ効かないことになる。同じ channel の FIFO
            // なので、先に送れば host は必ず先に atomic へ書く。
            if let Some(beat) = self.vocal_synth_priority_beat(track, bpm) {
                self.voicevox.priority_sent.insert(host_plugin_id, beat);
                self.send_plugin(PluginCommand::SetVocalSynthPriority {
                    device_id: host_plugin_id,
                    playhead_beats: beat,
                });
            }
            self.send_plugin(PluginCommand::SetBuiltinPluginNoteMetadata {
                device_id: host_plugin_id,
                bpm,
                chunk_secs,
                entries,
                talk,
            });
        }
    }

    /// 合成中の builtin VOICEVOX へ「その行がいま鳴らしている位置」を送る
    /// (r.md #75 / #87)。前回送信から **1 拍以上動いたときだけ**送るので、
    /// トランスポート中でも IPC は数 Hz 以下に収まる。busy な device が 1 つも
    /// 無ければ何もしない。
    ///
    /// **再合成はトリガしない** (`SetVocalSynthPriority` は順序ヒント専用)。停止 / seek でも
    /// 位置が動くので、同じ経路で届く。
    pub(crate) fn send_vocal_synth_priority_if_moved(&mut self) {
        if self.voicevox.voicevox_synth_status.is_empty() {
            return;
        }
        let bpm = self.song_doc.song().bpm;
        // 位置は **device (= track) ごと**に違う (行ごとに時間軸の供給元が違うため)。
        // 借用を跨がないよう先に解いてから送る。
        let targets: Vec<(u64, f64)> = self
            .song_doc
            .song()
            .tracks
            .iter()
            .filter_map(|track| {
                let device_id = self.voicevox_plugin_id_for_track(track)?;
                if !self
                    .voicevox
                    .voicevox_synth_status
                    .get(&device_id)
                    .is_some_and(|s| s.progress.busy)
                {
                    return None;
                }
                Some((device_id, self.vocal_synth_priority_beat(track, bpm)?))
            })
            .collect();
        for (device_id, beat) in targets {
            let moved = self
                .voicevox
                .priority_sent
                .get(&device_id)
                .is_none_or(|prev| (beat - prev).abs() >= 1.0);
            if !moved {
                continue;
            }
            self.voicevox.priority_sent.insert(device_id, beat);
            self.send_plugin(PluginCommand::SetVocalSynthPriority {
                device_id,
                playhead_beats: beat,
            });
        }
    }

    /// この track の合成順序ヒント (`SetVocalSynthPriority`) に載せる
    /// **合成タイムラインの拍** — [`synth_clips_with_base`] と同じ座標系。
    ///
    /// 送るのは「合成タイムライン上の距離」ではなく **ユーザーがいま聴こうとして
    /// いるもの**の位置。ランチャーのセルはアレンジの終端より後ろの仮想区間に
    /// 置かれるので、song の playhead をそのまま送ると距離が常に最大 =
    /// **セルは撃っても最後まで合成されない** (「▶ を押しても当分歌わない」)。
    ///
    /// 判断の SSoT は行の主導権 ([`common::model::RowPlayback`]) —
    /// 「この行がいま何を鳴らす行か」そのものだから:
    ///
    /// 1. 行がセルを鳴らしている (engine 観測、届いていなければ `Song` の起点) →
    ///    **そのセルの位相**。この行のアレンジのクリップはそもそも鳴らないので、
    ///    playhead を送る意味が無い
    /// 2. 行がアレンジのままでも、**この track のセルを選んでいる** → そのセルの先頭
    ///    (= これから撃つ / 歌詞を書いている最中)。 セル選択はアレンジの範囲選択と
    ///    排他 (`drop_cell_selection_if_arrangement`) なので、古い選択が残って
    ///    アレンジ側の合成を後回しにすることは無い
    /// 3. どちらでもない → song の playhead (= 従来どおり。アレンジを普通に作って
    ///    いる間はセルが遠いので自然に後回しになる)
    fn vocal_synth_priority_beat(&self, track: &common::model::Track, bpm: f32) -> Option<f64> {
        let playhead = self.transport.playhead_beat.map(f64::from);
        let Some((clip_id, phase)) = self.focused_synth_cell(track) else {
            return playhead;
        };
        let base = synth_clips_with_base(self.song_doc.song(), track, bpm)
            .into_iter()
            .find_map(|(c, base)| (c.id == clip_id).then_some(base));
        match base {
            Some(base) => Some(base + phase),
            None => playhead,
        }
    }

    /// この track で **ユーザーがいま聴こうとしているセル** と、その中の位相
    /// (`None` = アレンジを聴いている)。判断の順は
    /// [`Self::vocal_synth_priority_beat`] の doc。
    fn focused_synth_cell(&self, track: &common::model::Track) -> Option<(u32, f64)> {
        use crate::event_launcher::{LauncherCellKey, LauncherRow};
        let row = LauncherRow::Track(track.id);
        if let Some(clip_id) = self.running_playback(row).playing_clip_id()
            && track.session_clip_by_id(clip_id).is_some()
        {
            // 位相は編集面のプレイヘッドと同じ 1 本で解く (式を写さない)。
            // 走行状態が届いていない (= 撃った起点のまま停止中) なら先頭。
            let phase = self
                .editor_playhead_beat(common::model::ClipKey {
                    track_id: track.id,
                    clip_id,
                })
                .unwrap_or(0.0);
            return Some((clip_id, phase));
        }
        // 面の判定は **セル選択が生きているか** 1 つ (`shown_pianoroll_clips` と同じ
        // 判定)。 last-wins タグは Delete / Cut の宛先を持つ別の関心事で、セルの
        // ピアノロールで歌詞を打つ (= ノートを選ぶ) だけで `Notes` へ倒れるので、
        // これを使うと **歌詞を書いている最中のセルだけが合成の後回し** になる。
        // アレンジの面を選べばセル選択は排他で降りるので、古い選択が残って
        // アレンジ側の合成を後回しにすることも無い。
        let cells = self.live_launcher_cells();
        // 末尾 = 最後に click したセル。
        let clip_id = cells
            .iter()
            .rev()
            .find_map(|c| match c {
                LauncherCellKey::Track(k) if k.track_id == track.id => Some(k.clip_id),
                _ => None,
            })?;
        track
            .session_clip_by_id(clip_id)
            .is_some()
            .then_some((clip_id, 0.0))
    }

    /// `target` (口 track) の口パク出力を決定する入力すべての 64-bit fingerprint。
    /// `regenerate_lipsync_for_track` が phoneme query / `build_mouth_events` に渡す
    /// 入力と **厳密に一致** させる (= ここに含めた値が変わったときだけ再生成が要る):
    ///
    /// - song の `bpm`
    /// - `target` の `mouth_map` (7 slot の `ImageSourceId`)
    /// - `target` を出力先にする全ソーストラックの **並び順 (priority)** と、各 clip の
    ///   `start_beat` / `length_beats`、および
    ///     - sing clip: notes の `start_beat` / `duration_beats` / `pitch` / `lyric`
    ///       (= `build_sing_query` が読むフィールド。`velocity` / `muted` は phoneme へ
    ///       影響しないので **含めない**)
    ///     - talk clip: 先頭の非空 `TextEvent` の `text` / `event_start_in_clip_beats`、
    ///       および clip の `speaker_id` / `talk.speed_scale` (= `query_talk_phonemes` が
    ///       phoneme 長に使う値。pitch / intonation / volume は無関係なので含めない)
    ///
    /// - r.md #87: 上の走査は**ランチャーのセルも含む** (アレンジの clip と同じ規則。
    ///   ただしセルは song 絶対位置を持たないので `start_beat` の代わりに列 id)。
    ///   加えて、列ごとに作る口パクセルの**長さと発火設定**
    ///   ([`common::lipsync::mouth_cell_shape`]) も入力に含める
    ///
    /// track 名 / 色 / mute / volume / plugin 等の **非入力** は含めないので、それらの
    /// 編集では fingerprint が不変 → `LipsyncDebounceFired` が再生成をスキップする。
    /// 走査は `regenerate_lipsync_for_track` の snap 収集と**同じ**
    /// [`common::lipsync::LipsyncLayout::placements`] /
    /// [`common::lipsync::lipsync_source_of`] を通す (対象が食い違うと、
    /// 「入力を変えたのに再生成されない」が静かに起きる)。
    pub(crate) fn lipsync_input_fingerprint(song: &common::model::Song, target_id: u32) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        song.bpm.to_bits().hash(&mut h);
        if let Some(t) = song.tracks.iter().find(|t| t.id == target_id)
            && let Some(m) = &t.mouth_map
        {
            [m.a, m.i, m.u, m.e, m.o, m.n, m.closed].hash(&mut h);
        }
        // r.md #18: 閉じ口の充填範囲は立ち絵 body clip の範囲に、 各 event の
        // aspect-fit rect は video 解像度に依存する。 これらを fingerprint に含めて、
        // body の resize/move や解像度変更で口パクが再生成されるようにする (かつ
        // load 時の再構成と同一入力になり dirty-on-open を防ぐ、 code review)。
        if let Some((lo, hi)) = common::lipsync::tachie_body_range(song, target_id) {
            lo.to_bits().hash(&mut h);
            hi.to_bits().hash(&mut h);
        }
        song.video_resolution.0.hash(&mut h);
        song.video_resolution.1.hash(&mut h);
        // r.md #87: アレンジの clip とランチャーのセルの**両方**が入力。走査は
        // `LipsyncLayout::placements` 1 本で、`regenerate_lipsync_for_track` の
        // snap 収集と**同じ関数**を通る (片方だけ条件が変わる余地を無くす)。
        let layout = common::lipsync::LipsyncLayout::build(song, target_id);
        for (idx, src) in song.tracks.iter().enumerate() {
            if src.lipsync_target_track != Some(target_id) {
                continue;
            }
            (idx as u32).hash(&mut h); // priority (= トラック並び順)
            for (clip, place) in layout.placements(src) {
                // snap を生成する clip (notes 有り / 非空 text 有り) だけが出力に効く。
                if let Some(source) = common::lipsync::lipsync_source_of(song, clip) {
                    hash_lipsync_clip(&mut h, clip, &place, &source);
                }
            }
        }
        // 列ごとに作る口パクセルの形 (長さ + 写す発火設定) も出力を決める入力。
        // ここを落とすと、立ち絵セルを伸ばしたりローンチ量子化を変えても口パクセルが
        // 追従しない (歌と口の発火がズレたまま直らない)。
        for scene in &song.scenes {
            if let Some(shape) = common::lipsync::mouth_cell_shape(song, target_id, scene.id) {
                scene.id.hash(&mut h);
                shape.len_beats.to_bits().hash(&mut h);
                shape.quantize.hash(&mut h);
                shape.looping.hash(&mut h);
                shape.legato.hash(&mut h);
            }
        }
        h.finish()
    }

    /// 全 target (口) track の現在の入力 fingerprint を記録する。load 直後に呼び、
    /// 保存済み口パク clip を生成した入力をベースライン化することで、開いた直後の
    /// 非入力編集 (track rename 等) で口パクが再生成されないようにする。
    pub(crate) fn seed_lipsync_fingerprints(&mut self) {
        self.voicevox.lipsync_fingerprints.clear();
        let mut targets: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter_map(|t| t.lipsync_target_track)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        for target in targets {
            let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target);
            self.voicevox.lipsync_fingerprints.insert(target, fp);
        }
    }

    /// load / recovery 時に呼ぶ (r.md #17/#18): 既存プロジェクトの口 track を
    /// 「高々 1 本の連続 auto_lipsync clip・隙間は閉じ口」 という現行の不変条件へ
    /// **決定論的に畳み直す** (VOICEVOX 再合成は不要 — 既存 image event を並べ直す
    /// だけ)。 旧バージョンで生成した重なり合う複数 auto clip (#17) や、 隙間で口が
    /// 消える clip (#18) がここで正規化される。 既に目標形なら `rebuild_mouth_clip`
    /// が no-op を返し `normalize_song_checked` が epoch を進めないので、 正しく
    /// 保存済みのプロジェクトを開いても '*' は付かない (r.md #9 の contract を守る)。
    /// `seed_lipsync_fingerprints` の後に呼ぶ (baseline 確定後なので、 実際に畳んだ
    /// legacy プロジェクトだけが dirty = 要再保存になる)。
    pub(crate) fn normalize_lipsync_clips_on_load(&mut self) {
        let mut mouth_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter_map(|t| t.lipsync_target_track)
            .collect();
        mouth_ids.sort_unstable();
        mouth_ids.dedup();
        for mouth_id in mouth_ids {
            self.normalize_song_checked(|song| {
                let Some(m_idx) = song.tracks.iter().position(|t| t.id == mouth_id) else {
                    return false;
                };
                let closed_id = song.tracks[m_idx]
                    .mouth_map
                    .as_ref()
                    .map_or(0, |m| m.resolve(common::model::MouthShape::Closed));
                // 既存 auto clip の **開き口** event を song-absolute span へ展開
                // (priority = clip 順、 旧 per-clip 生成で重なっていても上位優先で畳む)。
                // 閉じ口は fill が敷き直すので merge に載せない (live 経路と対称、 code review)。
                let mut spans: Vec<(f64, f64, u32, u32)> = Vec::new();
                for (i, clip) in song.tracks[m_idx].clips.iter().enumerate() {
                    if !clip.auto_lipsync {
                        continue;
                    }
                    let Some(events) = song
                        .clip_contents
                        .get(&clip.content_id)
                        .and_then(|c| c.image_events())
                    else {
                        continue;
                    };
                    // clip は content への「窓」なので、 窓の外に隠れている event は
                    // 見えていない = 存在しないものとして畳む。 clamp を忘れると、
                    // 端を trim した口 clip を開くたびに隠れ event が復活して目標形が
                    // 現状と食い違い、 **開くだけで毎回 '*'** が付く (r.md #9)。
                    let win_lo = clip.start_beat;
                    let win_hi = clip.start_beat + clip.length_beats;
                    for ev in events {
                        if closed_id != 0 && ev.source_id == closed_id {
                            continue;
                        }
                        let s = clip
                            .content_to_song_beat(ev.event_start_in_clip_beats)
                            .max(win_lo);
                        let e = (clip.content_to_song_beat(ev.event_start_in_clip_beats)
                            + ev.event_length_beats)
                            .min(win_hi);
                        if e > s {
                            spans.push((s, e, ev.source_id, i as u32));
                        }
                    }
                }
                // 開き口が無くても、 既存 auto clip があれば rebuild を通して閉じ口だけの
                // 単一 clip へ畳む (旧 clip が全部閉じ口だった場合も 1 本に正規化)。
                let has_auto = song.tracks[m_idx].clips.iter().any(|c| c.auto_lipsync);
                if spans.is_empty() && !has_auto {
                    return false;
                }
                // 重なりを解決した非重複区間 (既存 open/closed 両方を含む)。 これを
                // rebuild に渡すと、 残った隙間 (旧 clip 間) と立ち絵余白が閉じ口で
                // 埋められ、 全体が 1 本の連続 clip に畳まれる。
                let spans = merge_lipsync_events_by_priority(spans);
                rebuild_mouth_clip(song, mouth_id, spans)
            });
        }
    }

    /// load 時に呼ぶ (r.md #39): 保存済みの `auto_lipsync` clip が **古い配置ルール**
    /// ([`common::lipsync::PLACEMENT_GEN`]) で作られていたら、そのソース vocal track の
    /// 口パクを一度だけ再生成する。
    ///
    /// 口パク event は project に永続化される派生データで、通常の再生成トリガは
    /// `lipsync_input_fingerprint` の差分だけ。配置ルールを変えても入力は変わらないので、
    /// この経路が無いと旧タイミング (talk なら音声より ~100ms 遅れ) が温存され、
    /// 「直したのに変わらない」になる。合成 WAV 側の `CACHE_SCHEMA_VERSION` と対の仕組み。
    ///
    /// r.md #9 の dirty-on-open contract とは「**実際に古い世代のときだけ** dirty になる」
    /// 形で両立する (現行世代の clip しか無い project では何もしない)。engine 未起動等で
    /// phoneme query が失敗した場合は既存 clip が温存され、世代も古いままなので次回 open で
    /// 再試行される。
    pub(crate) fn regenerate_outdated_lipsync_on_load(&mut self) {
        let vocal_ids =
            common::lipsync::vocal_tracks_with_outdated_lipsync(self.song_doc.song());
        for vid in vocal_ids {
            tracing::info!(
                vocal_track_id = vid,
                placement_gen = common::lipsync::PLACEMENT_GEN,
                "口パク配置ルールが更新されているため再生成する (r.md #39)"
            );
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// 口パク (lip-sync) を再生成する (docs/plan_pakupaku.md §7)。`vocal_track_id`
    /// の各 clip の notes を snapshot し、背景スレッドで `query_phonemes`
    /// (`sing_frame_audio_query` のみ) を叩いて結果を `AppEvent::LipsyncGenerated`
    /// で main thread へ返す。binding (`lipsync_target_track`) 未設定 / 口 track の
    /// `mouth_map` 未設定 / notes を持つ clip 無し のときは no-op。歌唱のみ (Q6)。
    pub fn regenerate_lipsync_for_track(&mut self, vocal_track_id: u32) {
        let Some(target_id) = self
            .song_doc.song()
            .tracks
            .iter()
            .find(|t| t.id == vocal_track_id)
            .and_then(|t| t.lipsync_target_track)
        else {
            return;
        };
        // この target の現在の入力 fingerprint をベースラインとして記録する。
        // `LipsyncDebounceFired` はこの値と現在値を比べ、変化した target だけ
        // 再生成する (= rename 等の非入力編集では再生成しない)。直接呼び出し
        // (binding / mouth_map 変更) はここで必ず最新値へ更新されるので、直後の
        // debounce 発火は fingerprint 一致で二重再生成にならない。
        let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target_id);
        self.voicevox.lipsync_fingerprints.insert(target_id, fp);
        // 口 track が存在し mouth_map が設定済みか (= 生成する意味があるか)。
        let configured = self.song_doc.song().tracks.iter().any(|t| {
            t.id == target_id && t.mouth_map.as_ref().is_some_and(|m| m.is_configured())
        });
        if !configured {
            return;
        }
        let bpm = self.song_doc.song().bpm;
        let (snaps, talk_snaps) = collect_lipsync_snaps(self.song_doc.song(), target_id, bpm);
        if snaps.is_empty() && talk_snaps.is_empty() {
            // r.md #18: 開き口ソースが 1 つも無くても、 立ち絵が映っている間は口を
            // 消さない。 HTTP は不要 (phoneme が無い) ので、 立ち絵範囲を閉じ口だけで
            // 埋めた単一 clip を同期適用する (body / closed 未設定なら rebuild が
            // 既存 auto clip を掃除するだけ)。r.md #87: 列の生成物も同じ規則で
            // 通す (歌のセルを最後の 1 つまで消したときに取り残さない)。
            self.normalize_song_checked(|song| {
                rebuild_mouth_containers(song, target_id, Vec::new(), Vec::new())
            });
            return;
        }
        self.ensure_voicevox_engine();
        // 出力先 (口 track) を in-flight に登録 = クリップ上スピナー +
        // 全体オーバーレイ「口パク生成中」を点灯。完了イベントで必ず外す。
        self.voicevox.lipsync_inflight.insert(target_id);
        // spawn 時点の世代を snapshot し、 結果と一緒に返す。 HTTP が遅延して
        // いる間に project が切り替わる (reset_saved_baseline が gen を bump) と
        // handler 側で破棄される。
        let generation = self.voicevox.lipsync_gen;
        let target_track_id = target_id;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            let mut clips = Vec::with_capacity(snaps.len() + talk_snaps.len());
            for (clip_start_beat, clip_len_beats, first_phoneme_local_beat, priority, notes) in snaps {
                match crate::voicevox_client::query_phonemes(&notes, bpm) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_phoneme_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "lip-sync phoneme query failed");
                    }
                }
            }
            // (talk) 読み上げ phoneme を `query_talk_phonemes` で取り、同じ
            // `LipsyncClipResult` に詰める (= apply 経路は歌唱と共通)。
            for (clip_start_beat, clip_len_beats, first_phoneme_local_beat, priority, text, speaker_id, scales) in
                talk_snaps
            {
                match crate::voicevox_client::query_talk_phonemes(&text, speaker_id, &scales) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_phoneme_local_beat,
                        priority,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "talk lip-sync phoneme query failed");
                    }
                }
            }
            // 成功/失敗/空に関わらず **必ず** 送る。handler が
            // `lipsync_inflight` から target を外してスピナーを止める。clips が空なら
            // (= 全 HTTP 失敗) handler 側で「既存 clip を温存」して反映だけスキップする。
            proxy.send(AppEvent::LipsyncGenerated {
                vocal_track_id,
                target_track_id,
                bpm,
                clips,
                generation,
            });
        });
    }

    /// `AppEvent::LipsyncGenerated` handler。口 track の自動生成 clip
    /// (`auto_lipsync == true`) を全て差し替える。派生データなので Undo
    /// snapshot は積まない (user 編集側が Undo 対象、手編集は保持しない = Q8)。
    pub(crate) fn apply_lipsync_generated(
        &mut self,
        vocal_track_id: u32,
        bpm: f32,
        results: Vec<LipsyncClipResult>,
    ) {
        // 口パクは派生データの自動再生成なので undo 履歴に入れない
        // (normalize_song)。 epoch は進む = dirty + frame flush の再 sync は走る。
        // normalize_song 経由で export_lock を同期する: export (freewheel /
        // video render) 中に口パク再生成が完了しても render 中の LoadSong を
        // 送らない (H2)。 `_checked` 版で、 生成結果が既存 clip と同一なら epoch を
        // 進めない (= 再生成トリガのたびに無変更で '*' が付くのを防ぐ)。
        self.normalize_song_checked(|song| {
            // HTTP 中に song が変わっている可能性があるため id ベースで再解決。
            let Some(target_id) = song
                .tracks
                .iter()
                .find(|t| t.id == vocal_track_id)
                .and_then(|t| t.lipsync_target_track)
            else {
                return false;
            };
            let Some(mouth_map) = song.track_by_id(target_id).and_then(|t| t.mouth_map.clone())
            else {
                return false;
            };
            let closed_id = mouth_map.resolve(common::model::MouthShape::Closed);
            // (talk) 全ソースの **開き口** event を song-absolute (start, end, image_id,
            // priority) に展開する。複数ソース (歌唱 Vox / 読み上げ Talk) が同じ口 track を
            // 共有しても、次の merge で重なりが上位 (priority 小 = 上のトラック) 優先で解決
            // される (`docs/plan_voicevox_talk.md`)。 閉じ口 padding は merge に載せない
            // ——上位ソースの閉じ口が下位ソースの開き口を潰すのを防ぐため。 隙間の閉じ口は
            // rebuild 内の fill が立ち絵範囲で一括して敷き直す (code review)。
            let mut spans: Vec<(f64, f64, u32, u32)> = Vec::new();
            for r in &results {
                let events = common::lipsync::build_mouth_events(
                    &r.phonemes,
                    &mouth_map,
                    bpm,
                    r.first_phoneme_local_beat,
                    r.clip_len_beats,
                );
                for ev in events {
                    if closed_id != 0 && ev.source_id == closed_id {
                        continue; // 閉じ口は fill が敷くので merge から除外
                    }
                    let s = r.clip_start_beat + ev.event_start_in_clip_beats;
                    let e = s + ev.event_length_beats;
                    if e > s {
                        spans.push((s, e, ev.source_id, r.priority));
                    }
                }
            }
            // 上位優先で重なりを解決した非重複な開き口区間。 これを立ち絵範囲まで
            // 閉じ口で埋めて **1 本の連続 auto_lipsync clip** に畳む (r.md #17/#18)。
            let open = merge_lipsync_events_by_priority(spans);
            // r.md #87: マージは 1 本の平坦化タイムラインの上でやり (= 複数ソースの
            // 重なりが列を跨いでも上位優先で正しく解ける)、ここで帯ごとに切り分けて
            // 入れ物へ戻す。帯は重ならないので、切り分けはマージ結果を壊さない。
            let layout = common::lipsync::LipsyncLayout::build(song, target_id);
            let (arrangement, cells) = split_spans_by_container(&layout, open);
            rebuild_mouth_containers(song, target_id, arrangement, cells)
        });
    }

    /// **入力を失った口パクの生成物** (`auto_lipsync` の clip / セル) と、
    /// **実在しない口 track を指す binding** を掃除する。戻り値 = 掃除したか。
    ///
    /// 生成物の存在条件は「その口 track を出力先にするソーストラックが在ること」
    /// 1 つ。ソースがゼロになった口 track の生成物は**誰も作り直さない** —
    /// [`Self::mark_lipsync_dirty`] は binding を持つ track が 1 つも無ければ
    /// 何もしないので、再生成の経路に落ちてこない。残ると **歌が無いのに口だけ
    /// 動く** (アレンジの clip でも列のセルでも同じ)。
    ///
    /// **どの経路が孤児を作ったかを数え上げない。** 「入力の無い生成物は在っては
    /// いけない」という規則 1 本で終端させる — producer 側 (トラック削除 /
    /// binding 解除 / トラックの貼り付け…) に散らすと必ず 1 つ漏れる
    /// ([`feedback_sibling_occurrence_check`])。呼ぶのは**ソースが消え得る編集の
    /// 直後**だけで、load 時には呼ばない (開いただけで `*` を立てない、r.md #9)。
    ///
    /// 「閉じ口だけ残す」(= 空の入力で作り直す) ではなく**消す**のは、binding が
    /// どこにも無くなった状態は *一度も bind していない状態* と同じだから。口の
    /// 画像は auto clip だけが置くので、bind していない立ち絵にはそもそも口の層が
    /// 無い。ここで閉じ口を残すと、以後**誰も追従させない**置き土産になる
    /// (body を動かしても付いてこない = r.md #9 の「静かに食い違う」側)。
    ///
    /// 派生データの後始末なので undo step は積まない (`normalize_song_checked`)。
    /// 消した生成物を主導権が指していた行は `normalize_session` が停止へ落とす。
    pub(crate) fn reap_orphan_lipsync(&mut self) -> bool {
        self.normalize_song_checked(|song| {
            let live: Vec<u32> = song.tracks.iter().map(|t| t.id).collect();
            let mut changed = false;
            // (a) 消えた口 track を指す binding。残すと編集のたびに口パク再生成の
            //     debounce (400ms の thread) が空回りし続ける。
            for t in &mut song.tracks {
                if t.lipsync_target_track.is_some_and(|id| !live.contains(&id)) {
                    t.lipsync_target_track = None;
                    changed = true;
                }
            }
            // (b) ソースを 1 つも持たない口 track の生成物。
            let sourced: Vec<u32> = song
                .tracks
                .iter()
                .filter_map(|t| t.lipsync_target_track)
                .collect();
            let mut cells_removed = false;
            for t in &mut song.tracks {
                if sourced.contains(&t.id) {
                    continue;
                }
                let before = (t.clips.len(), t.session_clips.len());
                t.clips.retain(|c| !c.auto_lipsync);
                t.session_clips.retain(|c| !c.clip.auto_lipsync);
                cells_removed |= t.session_clips.len() != before.1;
                changed |= (t.clips.len(), t.session_clips.len()) != before;
            }
            if cells_removed {
                song.normalize_session();
            }
            if changed {
                song.gc_clip_contents();
            }
            changed
        }) == Some(true)
    }

    /// `SetLipsyncTarget` handler。vocal track の出力先 binding を更新し、
    /// 設定時は口パクを再生成する (snapshot は `is_undoable` 経由で handler
    /// 前に取得済み = binding 変更を undo 可能)。
    pub(crate) fn set_lipsync_target(&mut self, track_id: u32, target: Option<u32>) {
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            if t.lipsync_target_track == target {
                return false; // 無変更 → no-op (dirty 化しない)
            }
            t.lipsync_target_track = target;
            true
        }) == Some(true);
        if !applied {
            return;
        }
        // r.md #17 (retarget leak): 出力先を変更/解除したら、入力を失った旧口 track の
        // 生成物を掃除する (他の vocal がまだ旧 track を出力先にしていれば温存)。
        // 判断は [`Self::reap_orphan_lipsync`] 1 本 (旧出力先を控えて数えない)。
        self.reap_orphan_lipsync();
        if target.is_some() {
            self.regenerate_lipsync_for_track(track_id);
        }
    }

    /// `SetMouthMapSlot` handler。口 track の `mouth_map` の 1 slot を更新し、
    /// この口 track を出力先にしている vocal track の口パクを再生成する。
    pub(crate) fn set_mouth_map_slot(
        &mut self,
        track_id: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    ) {
        let applied = self.edit_song(|song| {
            let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) else {
                return false;
            };
            let map = t
                .mouth_map
                .get_or_insert_with(common::model::MouthMap::default);
            match shape {
                common::model::MouthShape::A => map.a = source_id,
                common::model::MouthShape::I => map.i = source_id,
                common::model::MouthShape::U => map.u = source_id,
                common::model::MouthShape::E => map.e = source_id,
                common::model::MouthShape::O => map.o = source_id,
                common::model::MouthShape::N => map.n = source_id,
                common::model::MouthShape::Closed => map.closed = source_id,
            }
            true
        }) == Some(true);
        if !applied {
            return;
        }
        // この口 track を出力先にしている vocal track を再生成する。
        let vocal_ids: Vec<u32> = self
            .song_doc.song()
            .tracks
            .iter()
            .filter(|v| v.lipsync_target_track == Some(track_id))
            .map(|v| v.id)
            .collect();
        for vid in vocal_ids {
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// song 変更時に呼ぶ (= `flush_song_sync` から)。binding を持つ
    /// vocal track があれば debounce timer を立て、quiet period (400ms) 後に
    /// `LipsyncDebounceFired` を送る。rapid 編集 (歌詞タイプ等) は世代カウンタで
    /// coalesce され、最後の 1 回だけ再生成される。
    /// 進行中 (debounce 待ち) の口パク自動再生成を無効化する。 generation
    /// counter を bump するだけで、 既にスケジュール済みの `LipsyncDebounceFired`
    /// は世代不一致になり handler 側で no-op になる (新しい timer は spawn しない)。
    /// `reset_saved_baseline` (= load / new / recovery) から呼び、 開いた直後の
    /// spurious dirty を防ぐ。
    pub(crate) fn cancel_pending_lipsync_regen(&mut self) {
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
    }

    pub(crate) fn mark_lipsync_dirty(&mut self) {
        if !self
            .song_doc.song()
            .tracks
            .iter()
            .any(|t| t.lipsync_target_track.is_some())
        {
            return;
        }
        self.voicevox.lipsync_gen = self.voicevox.lipsync_gen.wrapping_add(1);
        let generation = self.voicevox.lipsync_gen;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            proxy.send(AppEvent::LipsyncDebounceFired(generation));
        });
    }

}

#[cfg(test)]
mod cell_region_tests {
    //! r.md #87: ランチャーのセルを合成タイムラインへ置く区間の割り当て。
    //!
    //! ここが壊れると**静かに**壊れる — 区間がアレンジと重なれば「撃っていないのに
    //! 曲頭で歌い出す」、セル同士が重なれば「別のセルの声が混ざる」。どちらも
    //! コンパイルもテストも素通りして、実機で耳で気付くしかない種類の欠陥。
    use super::{CELL_REGION_GAP_SECS, synth_clips_with_base};
    use common::model::{Clip, SessionClip, Track};

    fn cell(id: u32, len: f64) -> SessionClip {
        SessionClip {
            scene_id: id,
            clip: Clip { id, start_beat: 0.0, length_beats: len, ..Default::default() },
            launch: common::model::LaunchSettings::default(),
        }
    }

    #[test]
    fn cell_regions_never_overlap_the_arrangement_or_each_other() {
        let bpm = 120.0_f32;
        let gap = CELL_REGION_GAP_SECS * f64::from(bpm) / 60.0;
        let track = Track {
            clips: vec![
                Clip { id: 1, start_beat: 0.0, length_beats: 8.0, ..Default::default() },
                Clip { id: 2, start_beat: 16.0, length_beats: 4.0, ..Default::default() },
            ],
            // わざと id 昇順でない順で持たせる (並び順ではなく id で決まること)。
            session_clips: vec![cell(5, 4.0), cell(3, 2.0)],
            ..Default::default()
        };
        let placed = synth_clips_with_base(&common::model::Song::default(), &track, bpm);
        assert_eq!(placed.len(), 4);
        // アレンジのクリップは原点 0 (= song 拍そのまま)。
        assert!(placed[..2].iter().all(|&(_, base)| base == 0.0));
        // セルは id 昇順で、アレンジ終端 (20 拍) の後ろへ隙間を空けて並ぶ。
        let cells: Vec<(u32, f64)> = placed[2..].iter().map(|&(c, b)| (c.id, b)).collect();
        assert_eq!(cells[0].0, 3);
        assert_eq!(cells[1].0, 5);
        assert!(cells[0].1 >= 20.0 + gap, "セル 3 がアレンジに食い込んでいる: {cells:?}");
        // セル 3 は 2 拍ぶん占めるので、次の原点はそれ + 隙間より後ろ。
        assert!(cells[1].1 >= cells[0].1 + 2.0 + gap, "セル同士が重なっている: {cells:?}");
    }

    #[test]
    fn cells_stay_after_the_arrangement_even_when_it_is_empty() {
        // アレンジのクリップが 1 つも無い (= セルだけのトラック) でも、原点は必ず
        // 正 — `cell_base_beat > 0.0` が builtin 側の「セルか」の判定なので、0 に
        // なると再生側がアレンジと取り違える。
        let track = Track { session_clips: vec![cell(1, 4.0)], ..Default::default() };
        let placed = synth_clips_with_base(&common::model::Song::default(), &track, 120.0);
        assert_eq!(placed.len(), 1);
        assert!(placed[0].1 > 0.0, "原点が 0 だとアレンジと区別できない");
    }
}
