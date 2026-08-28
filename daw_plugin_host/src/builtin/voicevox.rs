//! VOICEVOX 内蔵 instrument plugin (`docs/plan_voicevox_synth.md` PR-V2)。
//!
//! Split-half (`docs/plan_arch_refactor.md` §6): [`VoicevoxBuiltin`] が
//! main half (state / lyrics / synth thread pipeline)、[`VoicevoxAudioHalf`]
//! が audio half (出力 buffer / voice / transport edge)。両 half は
//! `Arc<ArcSwapOption<SynthResult>>` (lock-free) だけを共有するので、
//! `set_note_metadata` (main) と `process()` (worker) の並行実行は Rust の
//! aliasing 的にも RT 的にも安全。
//!
//! - `set_note_metadata(bpm, chunk_secs, entries, talk)` で歌詞 + note 配列を
//!   受信、background synth thread に job を投げる (VocalSynth capability 経由)
//! - synth thread が [`super::voicevox_render`] に委譲して **フレーズ単位**で
//!   合成し、完成した部分から mono PCM を `ArcSwapOption` に格納する
//! - audio half の `process()` が note_on で synth result から voice を
//!   起こして stereo output に mix
//!
//! ## 合成の単位 (r.md #75)
//!
//! 旧実装は「声 (speaker) 単位で曲全体を 1 query + 1 synth」だったので、1 ノート
//! 直すと曲全体を再合成し、5 分の曲では `/frame_synthesis` が GPU メモリ不足で
//! 失敗した。現在は **フレーズ (= 隙間ゼロで続く note の極大列) が合成の最小単位**、
//! **塊 (= 既定 60 秒ぶんのフレーズ) がクエリの単位**。分割の定義は
//! `common::voicevox_phrase`、オーケストレーションは [`super::voicevox_render`]。
//!
//! ## state save / restore
//!
//! `VoicevoxState { speaker_id, style_name }` のみ bincode で保存。合成済
//! wav cache は保存しない (= load 時に re-synth。ただしディスクキャッシュが
//! 効くので HTTP は走らない)。
//!
//! ## 合成状態の報告
//!
//! 合成状態は `HostCallbacks::on_vocal_synth_status` で報告する (旧
//! `set_voicevox_status_reporter` の第 2 callback 登録機構は廃止 — load 時に
//! device_id を capture した callback が渡ってくる)。payload は
//! [`VocalSynthProgress`] で、busy / 失敗種別 / 残フレーズ数 / クリップ帰属を
//! 1 つの型に統一している (callback の引数と IPC の payload が同じ型 = SSoT)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use bincode::{Decode, Encode};
use common::model::TalkParams;
use common::plugin_db::BUILTIN_ID_VOICEVOX;
use common::plugin_format::PluginFormat;
use common::plugin_metadata::{NoteMetadata, TalkMetadata};
use common::protocol::{RenderMode, VocalSynthFailure, VocalSynthProgress};
use common::voicevox_phrase::{MAX_CHUNK_SECS, MIN_CHUNK_SECS};
use crate::builtin::voicevox_render::{MemoryCache, PhraseRenderer, RenderOutcome};

use crate::plugin_instance::{
    AudioHalf, AudioProcessorHalf, AuxInputBuf, LoadedPlugin, TimedNoteEvent, VocalSynth,
};

/// 合成状態 + 進捗の報告先 (= `HostCallbacks::on_vocal_synth_status`)。
/// payload は [`VocalSynthProgress`] (busy / 失敗種別 / 残フレーズ数 / クリップ帰属)。
type StatusFn = Arc<dyn Fn(VocalSynthProgress) + Send + Sync>;

/// `VoicevoxBuiltin` の persistent state (project file に bincode で埋め込み)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct VoicevoxState {
    /// VOICEVOX engine の `/singers` 経由で取得する speaker id。
    pub speaker_id: u32,
    /// `style_name` は表示用 (= plugin GUI / Inspector で見える)。
    pub style_name: String,
}

impl Default for VoicevoxState {
    fn default() -> Self {
        // VOICEVOX `frame_synthesis` (= sing 合成) は歌唱可能な singer
        // style id を要求する (talk speaker id を渡すと 500)。
        Self {
            speaker_id: common::voicevox::DEFAULT_SINGER_ID,
            style_name: "ノーマル".to_string(),
        }
    }
}

/// 合成 thread に渡す 1 job。フレーズ分割は synth thread が行う (純粋・安価なので
/// plugin-main を重くしない)。
struct SynthJob {
    bpm: f32,
    /// 塊 (= 1 クエリ) の長さ (秒)。GUI の設定から来る (クランプ済)。
    chunk_secs: f32,
    /// フレーズ分割の入力 (= flush された全 note)。
    entries: Vec<NoteMetadata>,
    /// (talk) `ClipContent::Text` 由来の読み上げ群。
    talk: Vec<TalkSynthSpec>,
    /// この job の世代 (= `synth_queued_gen` の値)。
    generation: u64,
}

/// (talk) 1 件の読み上げの合成指定。
pub(super) struct TalkSynthSpec {
    pub(super) event_id: u32,
    pub(super) start_beat: f64,
    pub(super) text: String,
    pub(super) speaker_id: u32,
    pub(super) scales: TalkParams,
    /// 進捗のクリップ帰属 (`VocalSynthProgress::pending_clips`)。
    pub(super) clip_id: u32,
}

/// 歌グループ wav の配置位置 = **wav frame 0 が来る曲 sample 位置**
/// (= 基準ノート [`common::voicevox::sing_base_beat`] の `REST_FRAMES` 手前)。
///
/// r.md #39 の単一契約「合成 buffer の index N = 曲の sample 位置 N」を満たすため、
/// query が wav 先頭に入れた leading rest は **配置側がここで吸収する**。曲頭付近では
/// 負になる (= 先頭無音が曲より手前) ので符号付き。
pub(super) fn sing_place_samples(base_beat: f64, bpm: f32, sample_rate: u32) -> i64 {
    let spb = f64::from(sample_rate) * 60.0 / f64::from(bpm.max(0.001));
    (common::voicevox::sing_head_beat(base_beat, bpm) * spb).round() as i64
}

/// talk wav の `(配置位置 = wav frame 0 の曲 sample, 発話開始の曲 sample)`。
///
/// talk wav の先頭無音は [`common::voicevox::TALK_PRE_PHONEME_LENGTH`] (= 0) で消して
/// あるので通常は両者一致 (= クリップ位置がそのまま発話開始)。定数を変えたときも配置が
/// 追従するよう、無音量は式で引く (歌の leading rest と同じ扱い)。
pub(super) fn talk_place_samples(
    start_beat: f64,
    speed_scale: f32,
    bpm: f32,
    sample_rate: u32,
) -> (i64, i64) {
    let spb = f64::from(sample_rate) * 60.0 / f64::from(bpm.max(0.001));
    let pre_silence = common::voicevox::frames_to_samples(
        f64::from(common::voicevox::talk_pre_silence_frames(speed_scale)),
        sample_rate,
    )
    .round() as i64;
    let speech_start = (start_beat * spb).round() as i64;
    (speech_start - pre_silence, speech_start)
}

/// 合成完了時に共有される結果 (synth thread が store、audio half が
/// lock-free load)。**逐次 publish** されるので、job 完了前は部分ミックスが入る
/// (完了の判定は `synth_done_gen`、= bounce / 書き出しはそちらを待つ)。
#[derive(Clone)]
pub(super) struct SynthResult {
    pub(super) samples: Arc<Vec<f32>>, // mono
    pub(super) sample_rate: u32,
    /// buffer を配置したときの **synth 時 (job.bpm) の samples-per-beat** (= sample_rate*60/bpm)。
    /// 連続再生は playhead 拍 → buffer 位置写像にこれを使う (再生時の transport.bpm ではなく)。
    /// buffer は job.bpm で拍配置されており、 tempo 変更直後の re-synth 完了までの過渡で
    /// transport.bpm と一致しないことがあるため、 配置時の値を SSoT として持ち回る (r.md #23)。
    pub(super) samples_per_beat: f64,
    /// `note_id → synth wav 内 frame offset`。
    pub(super) note_offsets: Arc<HashMap<u32, u64>>,
}

/// 停止中 (transport stopped) の鍵盤プレビュー用 voice。 note_on で張り替え、
/// synth wav の該当 note 位置から free-run 再生する。 **再生中 (playing) はこの
/// voice を使わない** — playing は playhead 追従の連続再生 (r.md #23)。
struct PreviewVoice {
    /// 次に samples から read する**分数**位置 (buffer sample 単位、song-absolute)。
    cursor: f64,
    /// note velocity (0..1 正規化済)。
    velocity: f32,
    /// declick 用フェード包絡 (0.0 開始 → fade-in で 1.0)。 突入クリックを消す。
    gain: f32,
}

/// preview voice の fade-in 時間 (ms)。 波形途中から full amp で突入する click を消す。
const PREVIEW_FADE_MS: f64 = 5.0;

// ====================================================================
// Audio half
// ====================================================================

/// Audio-thread half: 出力 buffer / active voice / transport edge のみ。
/// synth 結果は `ArcSwapOption` の lock-free load で受け取る。
struct VoicevoxAudioHalf {
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    sample_rate: f64,
    /// 停止中の鍵盤プレビュー voice (再生中は使わない、 playing は playhead 連続再生)。
    preview: Option<PreviewVoice>,
    /// 前 buffer の transport 再生状態 (stopped→playing edge で preview を捨てる)。
    was_playing: bool,
    /// 共有 synth 結果 (synth thread が store、ここは load のみ)。
    synth_result: Arc<ArcSwapOption<SynthResult>>,
    /// RT quiescence epoch。`process()` の入口 / 出口で +1 する (入口後 = 奇数、
    /// 出口後 = 偶数)。synth thread は publish 直後にこれを読み、
    /// 「store より前に始まった `process()` が終わった」ことを確認してから旧 Arc を
    /// 回収する。**RT 側のコストは atomic RMW 2 回だけ** (確保・ロック・I/O は無い)。
    rt_epoch: Arc<AtomicU64>,
}

/// `process()` の入口で作り、Drop で出口の +1 を行う RAII guard。
/// 早期 return / panic でも出口の +1 が必ず走るようにするためのもの。**確保はしない**。
struct RtEpochGuard<'a>(&'a AtomicU64);

impl Drop for RtEpochGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

impl VoicevoxAudioHalf {
    fn new(synth_result: Arc<ArcSwapOption<SynthResult>>, rt_epoch: Arc<AtomicU64>) -> Self {
        Self {
            out_l: Vec::new(),
            out_r: Vec::new(),
            sample_rate: 0.0,
            preview: None,
            was_playing: false,
            synth_result,
            rt_epoch,
        }
    }
}

impl AudioProcessorHalf for VoicevoxAudioHalf {
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        _param_events: &[crate::plugin_instance::TimedParamEvent],
        _input_audio: &[&[f32]],
        _aux_inputs: &[AuxInputBuf<'_>],
        transport: &crate::plugin_instance::TransportContext,
    ) -> Result<i32> {
        // RT quiescence epoch: 入口で +1 (奇数)、Drop で出口の +1 (偶数)。
        // synth thread の publish がこれを見て旧 buffer の回収可否を決める。
        self.rt_epoch.fetch_add(1, Ordering::Release);
        let _epoch_guard = RtEpochGuard(&self.rt_epoch);

        let frames_usize = frames as usize;
        // 出力 buffer を 0 fill。
        let n_l = frames_usize.min(self.out_l.len());
        for v in &mut self.out_l[..n_l] {
            *v = 0.0;
        }
        let n_r = frames_usize.min(self.out_r.len());
        for v in &mut self.out_r[..n_r] {
            *v = 0.0;
        }

        let out_max = self.out_l.len().min(self.out_r.len());
        let out_n = frames_usize.min(out_max);
        let host_sr = self.sample_rate;

        // stopped→playing edge で停止中 preview を捨てる (再生は playhead 追従へ)。
        let started_edge = !self.was_playing && transport.is_playing;
        self.was_playing = transport.is_playing;
        if started_edge {
            self.preview = None;
        }

        // 共有 synth 結果 (lock-free load)。 `load()` (Guard 借用) を使い `load_full()` は
        // 使わない: buffer は multi-MB なので、 audio thread が snapshot の最後の所有者になって
        // process() 末尾で drop = RT スレッド上で解放、 を避ける (CLAUDE.md「解放禁止」)。
        // daw_audio の RT パスと同じ idiom (automation.rs / audio_worker.rs)。
        //
        // **`load()` だけでは解放を防ぎ切れない** (r.md #75 で判明。以前ここには
        // 「retired 値は writer 側で drop される」と書いてあったが誤り)。arc-swap 1.9.1 の
        // 既定 (hybrid) 戦略では、`store` した writer が `Debt::pay_all`
        // (`src/debt/mod.rs:81-108`) で未払い debt ごとに strong count を +1 し、その後
        // `HybridProtection::drop` (`src/strategy/hybrid.rs:105-125`) が「debt が既に
        // 払われていたら自分で `ManuallyDrop::drop`」= **RT が最後の所有者になり得る**。
        // 正しくは「`load()` は Arc を clone しないので**通常は** RT が所有者にならない。
        // ただし store と同時に走った `process()` は debt を払われて所有者になり得るので、
        // **writer 側の quiesce 付き retire (`voicevox_render`) と対で初めて解放が起きない**」。
        // 上の `rt_epoch` がその quiesce 判定の材料。
        let snapshot = self.synth_result.load();

        if transport.is_playing {
            // === REAPER 式 連続再生 (r.md #23) =====================================
            // synth buf は song-absolute (index = song sample @ res.sample_rate、 beat 0
            // 起点)。 **playhead 位置から buf を直接連続読み出し**する。 旧実装は note_on
            // ごとに voice を張り替え cursor を jump させていたため、 連続録音である歌声を
            // 音 (ノート) の境界で毎回切り貼りして click が乗っていた (REAPER は 1 本の
            // クリップとして通し再生するので click が無い)。 連続読み出しなら retrigger が
            // 消え、 録音そのものを再生するので click は原理的に発生しない。
            //
            // 拍 → buffer 位置: `buf_pos(beat) = beat * samples_per_beat`。
            // **buffer index N = 曲の sample 位置 N** が唯一の契約 (r.md #39)。各ソースの
            // 先頭無音 (歌の leading rest / talk の pre-silence) は synth thread が配置時に
            // 吸収済みなので、読み出し側は補正オフセットを **一切持たない**。旧実装は歌用の
            // lead-in を全ソース共通に足しており、talk が話速依存で -53〜+96ms ずれていた。
            // `samples_per_beat` は配置時 (synth 時 job.bpm) の値を SynthResult に持ち回る
            // (再生時 transport.bpm ではなく = tempo 変更過渡の drift を避ける、 下記参照)。
            if let Some(res) = snapshot.as_ref() {
                let src_len = res.samples.len();
                if host_sr > 0.0 && res.sample_rate > 0 && res.samples_per_beat > 0.0 && src_len > 0
                {
                    let src_sr = f64::from(res.sample_rate);
                    // 拍→buffer 位置は **synth 時の** samples_per_beat を使う (transport.bpm ではない)。
                    // buffer は job.bpm で配置され、 song_pos_beats は tempo 積分済の真の拍位置なので、
                    // これで定テンポは常に厳密、 tempo 変更過渡でも base が各 buffer で song_pos_beats から
                    // 再同期する。
                    let base = transport.song_pos_beats * res.samples_per_beat;
                    let ratio = src_sr / host_sr;
                    for k in 0..out_n {
                        let pos = base + k as f64 * ratio;
                        if pos < 0.0 {
                            continue;
                        }
                        let i0 = pos.floor() as usize;
                        if i0 >= src_len {
                            // pos は単調増加なので以降も全て範囲外 = 無音。
                            break;
                        }
                        let frac = (pos - i0 as f64) as f32;
                        // 末尾サンプルは自身を hold (端の補間を安定化)。
                        let s1 = if i0 + 1 < src_len {
                            res.samples[i0 + 1]
                        } else {
                            res.samples[i0]
                        };
                        let s = res.samples[i0] * (1.0 - frac) + s1 * frac;
                        self.out_l[k] += s;
                        self.out_r[k] += s;
                    }
                }
            }
        } else {
            // === 停止中: 鍵盤プレビュー (note_on 試聴) ==============================
            // note_on で preview voice を該当 note 位置 (synth_offset = song-absolute) に
            // 張り替え、 free-run 再生。 波形途中から突入するので fade-in で declick する。
            for ev in events {
                if let crate::plugin_instance::NoteTransition::On { note_id, velocity, .. } =
                    ev.event
                    && let Some(res) = snapshot.as_ref()
                    // offset が無い note (= まだ合成されていない / 重なりで query から
                    // 落ちた) は試聴しない。 `unwrap_or(0)` にすると曲頭から再生されて
                    // しまい「押した音と違う音が出る」ため (r.md #39)。
                    && let Some(&synth_offset) = res.note_offsets.get(&note_id)
                {
                    let synth_offset = synth_offset as usize;
                    if synth_offset < res.samples.len() {
                        self.preview = Some(PreviewVoice {
                            cursor: synth_offset as f64,
                            velocity: velocity as f32,
                            gain: 0.0,
                        });
                    }
                }
                // note_off は無視 (VOICEVOX 出力は envelope 内包、 wav 終端で自動停止)。
            }

            // preview voice を mix。 別フィールド (out_l / out_r と preview) の disjoint
            // borrow なので RT 安全 (alloc / lock なし)。
            let mut drop_preview = false;
            if let (Some(pv), Some(res)) = (self.preview.as_mut(), snapshot.as_ref()) {
                let src_len = res.samples.len();
                if host_sr > 0.0 && res.sample_rate > 0 && src_len > 0 {
                    let ratio = f64::from(res.sample_rate) / host_sr;
                    let amp = pv.velocity.clamp(0.0, 1.0);
                    let fade_step = (1000.0 / (PREVIEW_FADE_MS * host_sr)).min(1.0) as f32;
                    let mut produced = 0usize;
                    for k in 0..out_n {
                        let pos = pv.cursor + k as f64 * ratio;
                        let i0 = pos.floor() as usize;
                        if i0 >= src_len {
                            break;
                        }
                        pv.gain = (pv.gain + fade_step).min(1.0);
                        let frac = (pos - i0 as f64) as f32;
                        let s1 = if i0 + 1 < src_len {
                            res.samples[i0 + 1]
                        } else {
                            res.samples[i0]
                        };
                        let s = (res.samples[i0] * (1.0 - frac) + s1 * frac) * amp * pv.gain;
                        self.out_l[k] += s;
                        self.out_r[k] += s;
                        produced = k + 1;
                    }
                    pv.cursor += produced as f64 * ratio;
                    if pv.cursor.floor() as usize >= src_len {
                        drop_preview = true;
                    }
                }
            }
            if drop_preview {
                self.preview = None;
            }
        }

        Ok(0)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        match channel {
            0 => Some(&self.out_l),
            1 => Some(&self.out_r),
            _ => None,
        }
    }

    fn drain_out_notes_into(&mut self, _out: &mut Vec<TimedNoteEvent>) {}

    fn on_activate(&mut self, sample_rate: f64, max_frames: u32) {
        self.sample_rate = sample_rate;
        let cap = max_frames as usize;
        self.out_l.clear();
        self.out_l.resize(cap, 0.0);
        self.out_r.clear();
        self.out_r.resize(cap, 0.0);
    }

    fn on_deactivate(&mut self) {
        self.preview = None;
    }

    fn set_processing(&mut self, on: bool) {
        if !on {
            self.preview = None;
        }
    }
}

// ====================================================================
// Main half
// ====================================================================

pub struct VoicevoxBuiltin {
    state: VoicevoxState,
    activated: bool,

    /// `note_id → 歌詞` (debug / introspection 用に保持)。実際の synth は
    /// `set_note_metadata` 受信時に synth thread に丸投げ。
    lyrics: HashMap<u32, String>,

    // --- synth pipeline ---------------------------------------------------
    /// synth thread への job 投入口。
    synth_tx: Option<mpsc::Sender<SynthJob>>,
    /// synth thread の join handle (deactivate / drop で join)。
    synth_thread: Option<JoinHandle<()>>,
    /// synth processing thread への shutdown 通知。
    synth_shutdown: Arc<AtomicBool>,
    /// 共有 synth 結果 (audio half と synth thread が共有)。
    synth_result: Arc<ArcSwapOption<SynthResult>>,
    /// `set_note_metadata` が synth job を queue するたびに +1 する世代。
    synth_queued_gen: Arc<AtomicU64>,
    /// synth thread が job を完了した世代。
    synth_done_gen: Arc<AtomicU64>,
    /// synth thread が完了フレーズごとに +1 する heartbeat。bounce / 書き出しの完了待ちが
    /// 「固まったのか、まだ進んでいるのか」を区別するために使う (30 秒の固定 deadline
    /// では 5 分の曲が部分ミックスで書き出されてしまう)。
    synth_heartbeat: Arc<AtomicU64>,
    /// 再生ヘッド優先ヒント (`f64::to_bits`。`f64::NAN` = 未設定)。
    /// `SetVocalSynthPriority` が書き、synth thread が合成順序の決定で読む。
    /// **再合成はトリガしない。**
    priority_beats: Arc<AtomicU64>,
    /// audio half と共有する RT quiescence epoch。`new()` で 1 本作り、
    /// `VoicevoxAudioHalf::new` と `start_synth_thread` の両方へ `Arc::clone` する
    /// (`synth_result` と同じ配り方)。
    rt_epoch: Arc<AtomicU64>,
    /// 合成状態 + 進捗の報告 callback
    /// (`HostCallbacks::on_vocal_synth_status`、load 時に device_id capture 済)。
    report: StatusFn,

    /// Audio half (worker registry と共有)。
    audio: Arc<AudioHalf>,
}

impl VoicevoxBuiltin {
    pub(super) fn new(report: StatusFn) -> Self {
        let synth_result: Arc<ArcSwapOption<SynthResult>> = Arc::new(ArcSwapOption::from(None));
        let rt_epoch = Arc::new(AtomicU64::new(0));
        let audio = AudioHalf::new(Box::new(VoicevoxAudioHalf::new(
            Arc::clone(&synth_result),
            Arc::clone(&rt_epoch),
        )));
        Self {
            state: VoicevoxState::default(),
            activated: false,
            lyrics: HashMap::new(),
            synth_tx: None,
            synth_thread: None,
            synth_shutdown: Arc::new(AtomicBool::new(false)),
            synth_result,
            synth_queued_gen: Arc::new(AtomicU64::new(0)),
            synth_done_gen: Arc::new(AtomicU64::new(0)),
            synth_heartbeat: Arc::new(AtomicU64::new(0)),
            // `f64::NAN` = 未設定 (トランスポートを一度も動かしていない)。
            priority_beats: Arc::new(AtomicU64::new(f64::NAN.to_bits())),
            rt_epoch,
            report,
            audio,
        }
    }

    /// synth thread 用の dedup 付き状態報告。`last` と一致するときは送らない。
    fn synth_report(
        reporter: &StatusFn,
        last: &mut Option<VocalSynthProgress>,
        progress: VocalSynthProgress,
    ) {
        if last.as_ref() != Some(&progress) {
            *last = Some(progress.clone());
            reporter(progress);
        }
    }

    /// 状態遷移だけの報告 (進捗を持たない場面 = idle / engine 未接続 / 終端)。
    fn synth_report_state(
        reporter: &StatusFn,
        last: &mut Option<VocalSynthProgress>,
        busy: bool,
        failure: VocalSynthFailure,
    ) {
        Self::synth_report(
            reporter,
            last,
            VocalSynthProgress {
                busy,
                failure,
                ..VocalSynthProgress::default()
            },
        );
    }

    #[cfg(test)]
    pub fn lyrics_for_test(&self) -> &HashMap<u32, String> {
        &self.lyrics
    }

    fn start_synth_thread(&mut self) {
        if self.synth_tx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel::<SynthJob>();
        let result_arc = Arc::clone(&self.synth_result);
        // 完了世代を進める Arc。失敗 (retry) では進めない。
        let done_gen = Arc::clone(&self.synth_done_gen);
        // 直前セッションの stop で立った flag を必ずリセットしてから spawn。
        self.synth_shutdown.store(false, Ordering::SeqCst);
        let shutdown = Arc::clone(&self.synth_shutdown);
        // 直近 job だけ処理する (連続 flush は最後の 1 件だけ synth)。失敗
        // job は同 slot に残して retry pending にする。
        let coalesce = Arc::new(Mutex::new(None::<SynthJob>));
        let coalesce_recv = Arc::clone(&coalesce);
        // 合成状態 (busy / failing / 進捗) を daw_gui へ報告する callback。
        let status_reporter = Arc::clone(&self.report);
        // フレーズ 1 本ごとに +1 する heartbeat (完了待ちの「停滞」判定用)。
        let heartbeat = Arc::clone(&self.synth_heartbeat);
        // 再生ヘッド優先ヒント (合成順序だけを変える。再合成はトリガしない)。
        let priority = Arc::clone(&self.priority_beats);
        // audio half と共有する RT quiescence epoch (publish の retire 判定)。
        let rt_epoch = Arc::clone(&self.rt_epoch);

        let spawn_result = std::thread::Builder::new()
            .name("voicevox-builtin-synth".into())
            .spawn(move || {
                // 受信 thread: job が来たら coalesce slot に最新を上書き。
                let _ = std::thread::Builder::new()
                    .name("voicevox-builtin-synth-recv".into())
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            if let Ok(mut slot) = coalesce_recv.lock() {
                                *slot = Some(job);
                            }
                        }
                    });
                // 処理 thread: poll で coalesce slot を取り出して synth。
                //
                // busy/failure の遷移時のみ報告 (`synth_report` が dedup)。
                // Unreachable は成功するまで sticky (engine 未起動の retry でも
                // failing_since がリセットされず、daw_gui 側で警告が貯まる)。
                let mut last_status: Option<VocalSynthProgress> = None;
                let mut retry_after = std::time::Instant::now();
                // decode 済みフレーズ WAV の in-memory キャッシュ。job をまたいで生き、
                // job 開始時に「今回の job のキー集合」で retain する (= 自然な GC)。
                // これが「1 ノート編集 = 未変更フレーズは decode すらしない」の本体。
                let mut mem_cache: MemoryCache = MemoryCache::new();
                loop {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    // retry backoff を尊重。sleep 中も shutdown を即拾う。
                    loop {
                        if shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        let now = std::time::Instant::now();
                        if now >= retry_after {
                            break;
                        }
                        let wait = (retry_after - now)
                            .min(std::time::Duration::from_millis(20));
                        std::thread::sleep(wait);
                    }
                    let job_opt = {
                        let Ok(mut slot) = coalesce.lock() else {
                            return;
                        };
                        slot.take()
                    };
                    let Some(job) = job_opt else {
                        if shutdown.load(Ordering::SeqCst)
                            || Arc::strong_count(&coalesce) <= 1
                        {
                            // shutdown or 受信 thread exit (= sender drop)。
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    };
                    if job.entries.is_empty() && job.talk.is_empty() {
                        // 旧 buffer は **retire 経由で**手放す。`store(None)` を直呼び
                        // すると arc-swap が writer 側で即 drop するので、store と同時に
                        // 走っていた `process()` の Guard が最後の所有者になり、
                        // audio thread 上で数十 MB の解放が起きる (r.md #75)。
                        crate::builtin::voicevox_render::clear_published(
                            &result_arc,
                            &rt_epoch,
                        );
                        mem_cache.clear();
                        done_gen.store(job.generation, Ordering::SeqCst);
                        // 歌唱/読み上げ無し = 合成しない = idle。
                        Self::synth_report_state(
                            &status_reporter,
                            &mut last_status,
                            false,
                            VocalSynthFailure::None,
                        );
                        continue;
                    }
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    // blocking HTTP 合成に入る = busy。engine 起動待ち (Unreachable) は
                    // 直前値を維持して 5s 警告を貯める。Rejected は新 job = 新入力なので
                    // 引き継がず None にリセット (古い内容エラー表示を消す)。
                    let prev_failure = match last_status.as_ref() {
                        Some(VocalSynthProgress {
                            failure: VocalSynthFailure::Unreachable,
                            ..
                        }) => VocalSynthFailure::Unreachable,
                        _ => VocalSynthFailure::None,
                    };

                    // 再生ヘッド優先ヒント (未設定なら NaN)。
                    let hint = f64::from_bits(priority.load(Ordering::SeqCst));
                    let priority_beats = hint.is_finite().then_some(hint);

                    let mut renderer = PhraseRenderer::new(
                        job.bpm,
                        job.chunk_secs,
                        &job.entries,
                        &job.talk,
                        priority_beats,
                        Arc::clone(&rt_epoch),
                    );
                    // 今回の job で使うキーだけ残す (= メモリキャッシュの自然な GC)。
                    let live = renderer.live_keys();
                    mem_cache.retain(|k, _| live.contains(k));

                    Self::synth_report(
                        &status_reporter,
                        &mut last_status,
                        VocalSynthProgress {
                            busy: true,
                            failure: prev_failure.clone(),
                            pending: renderer.state_mut().total(),
                            total: renderer.state_mut().total(),
                            pending_clips: renderer.state_mut().pending_clips(),
                        },
                    );

                    // より新しい job が来たか (= この job は捨ててよいか)。
                    let superseded = {
                        let coalesce = Arc::clone(&coalesce);
                        move || coalesce.lock().map(|s| s.is_some()).unwrap_or(false)
                    };
                    let outcome = {
                        let reporter = Arc::clone(&status_reporter);
                        let heartbeat = Arc::clone(&heartbeat);
                        let result_arc = Arc::clone(&result_arc);
                        let failure = prev_failure.clone();
                        // フレーズ 1 本完了ごと: heartbeat / 進捗報告 / 必要なら publish。
                        // ここでの報告は dedup を通さない (件数は毎回変わるので必ず届く)。
                        let mut on_done = move |state: &mut crate::builtin::voicevox_render::PhraseRenderState| {
                            heartbeat.fetch_add(1, Ordering::SeqCst);
                            reporter(VocalSynthProgress {
                                busy: true,
                                failure: failure.clone(),
                                pending: state.pending(),
                                total: state.total(),
                                pending_clips: state.pending_clips(),
                            });
                            state.publish_if_due(&result_arc);
                        };
                        renderer.render(&mut mem_cache, &shutdown, &superseded, &mut on_done)
                    };

                    // `on_done` は dedup (`synth_report`) を通さず直接 reporter を呼ぶ
                    // (件数は毎回変わるので必ず届けたい)。その分 `last_status` が実際の
                    // 最後の報告からずれるので、ここで揃える。揃えないと、次 job の開始
                    // 報告が「前回と同じ」と判定されて抑止され、GUI に古い残件数と
                    // クリップスピナーが残ることがある (job が supersede され続ける
                    // ドラッグ中に起きやすい)。
                    last_status = Some(VocalSynthProgress {
                        busy: true,
                        failure: prev_failure.clone(),
                        pending: renderer.state_mut().pending(),
                        total: renderer.state_mut().total(),
                        pending_clips: renderer.state_mut().pending_clips(),
                    });

                    match outcome {
                        RenderOutcome::Shutdown => return,
                        RenderOutcome::Superseded => {
                            // より新しい入力が来た。この job は捨てる (done_gen は進めない)。
                            tracing::debug!("VoicevoxBuiltin: job superseded, dropping");
                        }
                        RenderOutcome::Unreachable(reason) => {
                            tracing::warn!(
                                reason = %reason,
                                "VoicevoxBuiltin: engine unreachable (localhost:50021 起動待ち), retry"
                            );
                            // engine 未接続 = busy のまま Unreachable。job を戻して retry。
                            // done_gen は進めない (bounce 待ちは engine 復帰まで待つ)。
                            Self::synth_report_state(
                                &status_reporter,
                                &mut last_status,
                                true,
                                VocalSynthFailure::Unreachable,
                            );
                            if let Ok(mut slot) = coalesce.lock()
                                && slot.is_none()
                            {
                                *slot = Some(job);
                            }
                            retry_after = std::time::Instant::now()
                                + std::time::Duration::from_millis(1500);
                        }
                        RenderOutcome::Done { rejected } => {
                            renderer.state_mut().publish_final(&result_arc);
                            tracing::info!(
                                phrases = renderer.state_mut().total(),
                                "VoicevoxBuiltin: synth complete (phrase-level, song-absolute)"
                            );
                            // **`done_gen` は全フレーズ (+ talk) 完了後にだけ進める。**
                            // 逐次 publish していてもここを緩めると、bounce / 書き出し
                            // (`PrepareVocalSynth` → `VocalSynthReady`) が **部分ミックスを
                            // 掴む**。
                            done_gen.store(job.generation, Ordering::SeqCst);
                            // 次 job が pending 中は終端報告を送らない (coalesce 中の点滅回避)。
                            let pending =
                                coalesce.lock().map(|s| s.is_some()).unwrap_or(false);
                            if !pending {
                                if let Some(detail) = rejected {
                                    tracing::warn!(
                                        detail = %detail,
                                        "VoicevoxBuiltin: synth rejected (bad input; not retrying)"
                                    );
                                    Self::synth_report_state(
                                        &status_reporter,
                                        &mut last_status,
                                        false,
                                        VocalSynthFailure::Rejected { detail },
                                    );
                                } else {
                                    Self::synth_report_state(
                                        &status_reporter,
                                        &mut last_status,
                                        false,
                                        VocalSynthFailure::None,
                                    );
                                }
                            }
                        }
                    }
                }
            });

        // spawn 失敗で plugin-main を panic で落とさない。
        match spawn_result {
            Ok(handle) => {
                self.synth_tx = Some(tx);
                self.synth_thread = Some(handle);
            }
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    "VoicevoxBuiltin: synth thread spawn 失敗 (synth 無効化)"
                );
            }
        }
    }

    fn stop_synth_thread(&mut self) {
        // shutdown flag を立てると、engine 未起動で永久 retry 中の thread も
        // 即 return する。
        self.synth_shutdown.store(true, Ordering::SeqCst);
        // sender を drop すると recv thread が抜け → 処理 thread も exit。
        self.synth_tx = None;
        if let Some(handle) = self.synth_thread.take() {
            let _ = handle.join();
        }
        // 停止したら必ず idle を報告する (busy のまま deactivate すると
        // daw_gui の overlay / clip スピナーが残り続けるため)。
        (self.report)(VocalSynthProgress::default());
    }
}

impl VocalSynth for VoicevoxBuiltin {
    fn set_note_metadata(
        &mut self,
        bpm: f32,
        chunk_secs: f32,
        entries: &[NoteMetadata],
        talk: &[TalkMetadata],
    ) {
        // 内部 lyrics map を完全置換 (introspection 用)。
        self.lyrics.clear();
        for e in entries {
            self.lyrics.insert(e.note_id, e.lyric.clone());
        }

        // synth thread を起動していなければ作る (activate 前の flush 対応)。
        if self.synth_tx.is_none() {
            self.start_synth_thread();
        }

        // 分割 (フレーズ / 塊) は synth thread が行う。ここは entries をそのまま載せる
        // だけ — speaker グルーピングは合成の単位ではなくなった (r.md #75)。
        // (talk) 読み上げ群を TalkSynthSpec に。
        let talk_specs: Vec<TalkSynthSpec> = talk
            .iter()
            .map(|t| TalkSynthSpec {
                event_id: t.event_id,
                start_beat: t.start_beat,
                text: t.text.clone(),
                speaker_id: if t.speaker_id != 0 {
                    t.speaker_id
                } else {
                    common::voicevox::DEFAULT_TALK_SPEAKER_ID
                },
                scales: TalkParams {
                    speed_scale: t.speed_scale,
                    pitch_scale: t.pitch_scale,
                    intonation_scale: t.intonation_scale,
                    volume_scale: t.volume_scale,
                },
                clip_id: t.clip_id,
            })
            .collect();

        // GUI 側でクランプ済みだが、壊れた / 古い値が来ても engine を落とさないよう
        // ここでも畳む (合成結果を変える入力なので黙って通さない)。
        let chunk_secs = chunk_secs.clamp(MIN_CHUNK_SECS, MAX_CHUNK_SECS);

        // 世代を 1 進めてから job に乗せる。
        let generation = self.synth_queued_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(tx) = self.synth_tx.as_ref() {
            let _ = tx.send(SynthJob {
                bpm,
                chunk_secs,
                entries: entries.to_vec(),
                talk: talk_specs,
                generation,
            });
        }
        tracing::debug!(
            count = self.lyrics.len(),
            talk = talk.len(),
            bpm,
            chunk_secs,
            "VoicevoxBuiltin: lyrics buffer updated, synth job queued"
        );
    }

    /// 再生ヘッド優先ヒント。**再合成はトリガしない** (合成順序だけを変える)。
    /// atomic store のみなので、トランスポート中に毎 tick 来ても無害。
    fn set_priority_beats(&mut self, playhead_beats: f64) {
        self.priority_beats
            .store(playhead_beats.to_bits(), Ordering::SeqCst);
    }

    /// 合成完了待ち用に `(queued_gen, done_gen, phrase_heartbeat)` を公開。
    fn synth_progress(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>) {
        (
            Arc::clone(&self.synth_queued_gen),
            Arc::clone(&self.synth_done_gen),
            Arc::clone(&self.synth_heartbeat),
        )
    }
}

impl LoadedPlugin for VoicevoxBuiltin {
    fn id(&self) -> &str {
        BUILTIN_ID_VOICEVOX
    }

    fn name(&self) -> &str {
        "VOICEVOX (builtin)"
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Builtin
    }

    fn audio_half(&self) -> Arc<AudioHalf> {
        Arc::clone(&self.audio)
    }

    fn activate(
        &mut self,
        sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> Result<()> {
        // SAFETY: quiesced window (install / reinit call sites).
        unsafe { self.audio.get().on_activate(sample_rate, max_frames) };
        self.activated = true;
        self.start_synth_thread();
        Ok(())
    }

    fn deactivate(&mut self) {
        self.activated = false;
        // SAFETY: quiesced window.
        unsafe { self.audio.get().on_deactivate() };
        self.stop_synth_thread();
    }

    fn start_processing(&mut self) -> Result<()> {
        // SAFETY: quiesced window.
        unsafe { self.audio.get().set_processing(true) };
        Ok(())
    }

    fn stop_processing(&mut self) {
        // SAFETY: quiesced window. Voice drop も担う。
        unsafe { self.audio.get().set_processing(false) };
    }

    fn set_render_mode(&mut self, _mode: RenderMode) -> bool {
        true
    }

    fn query_latency(&mut self) -> u32 {
        0
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&self.state, cfg)
            .context("VoicevoxBuiltin: encode state")?;
        Ok(Some(bytes))
    }

    fn state_load(&mut self, data: &[u8]) -> Result<()> {
        let cfg = bincode::config::standard();
        let (decoded, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(data, cfg)
                .context("VoicevoxBuiltin: decode state")?;
        self.state = decoded;
        // 合成 cache は state に含まれないので、restore 直後は cache miss =
        // 無音。次の set_note_metadata (= load 完了後の initial flush) で
        // synth が走る。
        Ok(())
    }

    fn as_vocal_synth(&mut self) -> Option<&mut dyn VocalSynth> {
        Some(self)
    }

    // --- Embedded GUI: 意図的に持たない (r.md #8 B7 = no-op が最終形) ------
    // builtin VOICEVOX の設定はホスト (daw_gui) の native UI に統合されて
    // いる方が SSoT で一貫する (per-clip voice picker / clip スピナー)。
    fn gui_is_embed_supported(&self) -> bool {
        false
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        // supported=false なので到達しないが、到達しても benign no-op。
        Ok(())
    }

    fn gui_get_size(&self) -> Option<(u32, u32)> {
        None
    }

    fn gui_set_scale(&self, _scale: f64) -> Result<bool> {
        Ok(false)
    }

    fn gui_set_parent_hwnd(&self, _hwnd: u64) -> Result<()> {
        Ok(())
    }

    fn gui_show(&self) -> Result<bool> {
        Ok(false)
    }

    fn gui_hide(&self) -> Result<()> {
        Ok(())
    }

    fn gui_destroy(&mut self) {}
}

impl Drop for VoicevoxBuiltin {
    fn drop(&mut self) {
        self.stop_synth_thread();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_report() -> StatusFn {
        Arc::new(|_: VocalSynthProgress| {})
    }

    /// audio half を直接組んで activate 相当まで進める test helper。
    fn mk_half(sample_rate: f64) -> (VoicevoxAudioHalf, Arc<ArcSwapOption<SynthResult>>) {
        let result: Arc<ArcSwapOption<SynthResult>> = Arc::new(ArcSwapOption::from(None));
        let mut h = VoicevoxAudioHalf::new(Arc::clone(&result), Arc::new(AtomicU64::new(0)));
        h.on_activate(sample_rate, 256);
        h.set_processing(true);
        (h, result)
    }

    fn transport() -> crate::plugin_instance::TransportContext {
        crate::plugin_instance::TransportContext::from_process_data(
            &common::process_data::ProcessData::empty(),
        )
    }

    /// 再生中 (playing) transport を指定 playhead 拍位置で組む (連続再生パス検証用)。
    fn transport_playing(song_pos_beats: f64) -> crate::plugin_instance::TransportContext {
        let mut pd = common::process_data::ProcessData::empty();
        pd.playing = 1;
        pd.bpm = 120.0;
        pd.song_pos_beats = song_pos_beats;
        crate::plugin_instance::TransportContext::from_process_data(&pd)
    }

    #[test]
    fn default_state_uses_sing_speaker() {
        let s = VoicevoxState::default();
        assert_eq!(s.speaker_id, common::voicevox::DEFAULT_SINGER_ID);
        assert_eq!(s.style_name, "ノーマル");
    }

    #[test]
    fn state_bincode_roundtrip() {
        let s = VoicevoxState {
            speaker_id: 3,
            style_name: "あまあま".to_string(),
        };
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&s, cfg).unwrap();
        let (decoded, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn voicevox_id_and_format() {
        let p = VoicevoxBuiltin::new(noop_report());
        assert_eq!(p.id(), BUILTIN_ID_VOICEVOX);
        assert_eq!(p.format(), PluginFormat::Builtin);
        assert_eq!(p.name(), "VOICEVOX (builtin)");
    }

    #[test]
    fn voicevox_process_silent_when_no_synth_result() {
        let (mut h, _result) = mk_half(48_000.0);
        h.out_l[0] = 0.5;
        // 合成結果なしで note_on (stopped preview) を投げても preview は付かない。
        let events = vec![TimedNoteEvent {
            time: 0,
            event: crate::plugin_instance::NoteTransition::On {
                note_id: 0,
                key: 60,
                velocity: 100.0,
            },
        }];
        h.process(64, &events, &[], &[], &[], &transport()).unwrap();
        assert!(h.out_l[..64].iter().all(|&v| v == 0.0));
        assert!(h.preview.is_none());
    }

    #[test]
    fn voicevox_state_save_returns_bytes() {
        let p = VoicevoxBuiltin::new(noop_report());
        let bytes = p.state_save().unwrap().expect("Some bytes");
        assert!(!bytes.is_empty());
        let cfg = bincode::config::standard();
        let (s, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(s, VoicevoxState::default());
    }

    #[test]
    fn set_note_metadata_replaces_lyrics_buffer() {
        let mut p = VoicevoxBuiltin::new(noop_report());
        let mk = |note_id: u32, lyric: &str| NoteMetadata {
            note_id,
            lyric: lyric.to_string(),
            ..NoteMetadata::default()
        };
        // synth_tx は activate 前の flush でも自動起動する (HTTP は engine
        // 不在で失敗ログのみ、test 自体には影響なし)。
        p.set_note_metadata(120.0, 60.0, &[mk(0, "あ"), mk(1, "い")], &[]);
        let lyrics = p.lyrics_for_test();
        assert_eq!(lyrics.len(), 2);
        assert_eq!(lyrics.get(&1).map(String::as_str), Some("い"));
        // 空 flush で全消去。
        p.set_note_metadata(120.0, 60.0, &[], &[]);
        assert_eq!(p.lyrics_for_test().len(), 0);
        // synth thread を停止 (= test process が hang しないよう)。
        p.stop_synth_thread();
    }

    /// (talk) builtin の talk 経路を実 VOICEVOX engine に対して end-to-end
    /// 検証。engine (localhost:50021) が要るので通常 test では無視。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn talk_metadata_synthesizes_via_real_engine() {
        let mut p = VoicevoxBuiltin::new(noop_report());
        p.activate(48000.0, 0, 256).unwrap();
        let eid = common::plugin_metadata::talk_event_id(1, 0);
        let talk = vec![TalkMetadata {
            event_id: eid,
            start_beat: 0.0,
            text: "こんにちは".to_string(),
            speaker_id: common::voicevox::DEFAULT_TALK_SPEAKER_ID,
            speed_scale: 1.0,
            pitch_scale: 0.0,
            intonation_scale: 1.0,
            volume_scale: 1.0,
            clip_id: 1,
        }];
        p.set_note_metadata(120.0, 60.0, &[], &talk);
        // synth thread (background, blocking HTTP) の完了を待つ。
        let (queued, done, _heartbeat) = p.synth_progress();
        let target = queued.load(Ordering::SeqCst);
        let mut ok = false;
        for _ in 0..100 {
            if done.load(Ordering::SeqCst) >= target {
                ok = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ok, "synth thread completed within 10s");
        let res = p.synth_result.load_full().expect("synth_result present after talk synth");
        assert!(!res.samples.is_empty(), "talk synth produced samples");
        let rms =
            (res.samples.iter().map(|s| s * s).sum::<f32>() / res.samples.len() as f32).sqrt();
        assert!(rms > 0.001, "talk audio is non-silent (rms={rms})");
        assert!(
            res.note_offsets.contains_key(&eid),
            "note_offsets keyed by talk event_id"
        );
        p.deactivate();
    }

    /// buf は song-absolute (index N = 曲 sample N) なので、 十分長い ramp buf を
    /// 作れば連続再生の写像を厳密検証できる。
    fn ramp_buf(len: usize) -> Vec<f32> {
        (0..len).map(|i| i as f32).collect()
    }

    /// r.md #39: 再生中の読み出しは `buf index = 曲 sample` の恒等写像。読み出し側は
    /// 補正オフセットを **一切持たない** (先頭無音は配置側が吸収済み)。
    #[test]
    fn continuous_playback_maps_song_sample_to_buf_index() {
        let (mut h, result) = mk_half(48_000.0);
        result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(ramp_buf(8192)),
            sample_rate: 48_000,
            samples_per_beat: 24_000.0, // 48000*60/120
            note_offsets: Arc::new(HashMap::new()),
        })));
        // playhead = 拍 0 → base = 0。 host==synth なので ratio=1 → out[k] = buf[k]。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.0)).unwrap();
        assert!((h.out_l[0] - 0.0).abs() < 1e-3, "out_l[0]={}", h.out_l[0]);
        assert!((h.out_l[10] - 10.0).abs() < 1e-3, "out_l[10]={}", h.out_l[10]);
        // 左右同一 (mono → stereo)。
        assert_eq!(h.out_l[10], h.out_r[10]);
        // playhead = 拍 0.25 → base = 0.25*24000 = 6000。 note_on 無しでも playhead
        // から鳴る (mid-phrase 再生開始でも欠落しない)。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.25)).unwrap();
        assert!((h.out_l[0] - 6000.0).abs() < 1e-3, "out_l[0]={}", h.out_l[0]);
        assert!((h.out_l[7] - 6007.0).abs() < 1e-3, "out_l[7]={}", h.out_l[7]);
    }

    /// A9 (r.md #8): 合成 wav (48000Hz) を非48kHz 出力デバイス (24000Hz) へ連続再生
    /// 中も linear resample する (ratio = synth_sr / host_sr = 2.0)。
    #[test]
    fn continuous_playback_resamples_to_host_rate() {
        let (mut h, result) = mk_half(24_000.0);
        result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(ramp_buf(8192)),
            sample_rate: 48_000,
            samples_per_beat: 24_000.0, // 48000*60/120
            note_offsets: Arc::new(HashMap::new()),
        })));
        // base = 拍 0.25 × 24000 = 6000。 ratio 2.0 → out frame k = buf[6000 + 2k]。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.25)).unwrap();
        assert!((h.out_l[1] - 6002.0).abs() < 1e-3, "out_l[1]={}", h.out_l[1]);
        assert!((h.out_l[10] - 6020.0).abs() < 1e-3, "out_l[10]={}", h.out_l[10]);
    }

    // ---- 配置 × 読み出しの合成写像 (r.md #39) -------------------------------

    /// 48kHz で歌 query の leading rest が占める sample 数 (= 10 frame / 93.75fps)。
    fn lead_samples(sr: u32) -> usize {
        common::voicevox::frames_to_samples(f64::from(common::voicevox::REST_FRAMES), sr)
            .round() as usize
    }

    /// 「配置 (synth thread) → mix → 読み出し (process)」の合成が、曲の sample 位置と
    /// 厳密に一致することの回帰テスト。読み出し側に補正オフセットが復活したら落ちる。
    #[test]
    fn sing_placement_and_reader_compose_to_song_sample_identity() {
        let sr = 48_000u32;
        let bpm = 120.0f32; // spb = 24 000
        let base_beat = 2.0;
        // 配置 = 基準ノートの leading rest ぶん手前 = 48 000 − 5 120。
        let place = sing_place_samples(base_beat, bpm, sr);
        assert_eq!(place, 42_880);

        // wav: 先頭 leading rest は無音、その直後 (= 基準ノート) から 1,2,3...。
        let lead = lead_samples(sr);
        let mut wav = vec![0.0f32; lead + 1_000];
        for (i, v) in wav[lead..].iter_mut().enumerate() {
            *v = (i + 1) as f32;
        }
        let note_abs = place + lead as i64;
        let buf = crate::builtin::voicevox_render::mix_placed_for_test(&[(place, wav)]);
        // note の曲位置 = 拍 2.0 の sample (= 2.0 × 24 000)。
        let offsets: HashMap<u32, u64> = HashMap::from([(7u32, note_abs as u64)]);
        assert_eq!(offsets[&7], 48_000);

        // 読み出し: playhead 拍 2.0 の 1 sample 目が「基準ノートの 1 sample 目」。
        let (mut h, result) = mk_half(f64::from(sr));
        result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(buf),
            sample_rate: sr,
            samples_per_beat: 24_000.0,
            note_offsets: Arc::new(offsets),
        })));
        h.process(64, &[], &[], &[], &[], &transport_playing(2.0)).unwrap();
        assert!(
            (h.out_l[0] - 1.0).abs() < 1e-3,
            "拍 2.0 で基準ノートの先頭が鳴る: out_l[0]={}",
            h.out_l[0]
        );
        assert!((h.out_l[9] - 10.0).abs() < 1e-3, "out_l[9]={}", h.out_l[9]);
    }

    #[test]
    fn talk_placement_is_speed_independent() {
        // r.md #39 原因 2: 先頭無音を 0 にしたので、話速を変えても「クリップ位置 =
        // 発話開始」。旧実装は engine 既定 0.1s の無音と歌用 lead-in の差で
        // -53〜+96ms 動いていた。
        for speed in [0.5f32, 0.8, 1.0, 1.2, 1.5, 2.0] {
            assert_eq!(
                talk_place_samples(1.0, speed, 120.0, 48_000),
                (24_000, 24_000),
                "speed={speed}"
            );
        }
    }

    #[test]
    fn placement_before_song_start_is_trimmed_not_shifted() {
        // 曲頭 (拍 0) の歌は leading rest が曲より手前に来る。捨てるのはその無音だけで、
        // 実音は 1 sample もずらさない。
        let place = sing_place_samples(0.0, 120.0, 48_000);
        assert_eq!(place, -5_120);
        let lead = lead_samples(48_000);
        let wav: Vec<f32> = (0..lead + 500).map(|i| i as f32).collect();
        let buf = crate::builtin::voicevox_render::mix_placed_for_test(&[(place, wav)]);
        assert!(
            (buf[0] - lead as f32).abs() < 1e-3,
            "曲 sample 0 に来るのは wav[5120]: {}",
            buf[0]
        );
        // 有効長 (= 書かれた最終 sample) は 500 (曲頭より前の 5 120 sample を捨てる)。
        let written = buf.iter().rposition(|v| *v != 0.0).map_or(0, |i| i + 1);
        assert_eq!(written, 500);
    }

    #[test]
    fn overlapping_groups_are_summed() {
        // フレーズ / 読み上げが重なる区間は加算 mix。
        let buf = crate::builtin::voicevox_render::mix_placed_for_test(&[
            (0, vec![1.0, 1.0, 1.0]),
            (2, vec![10.0, 10.0]),
        ]);
        assert_eq!(buf, vec![1.0, 1.0, 11.0, 10.0]);
    }

    /// 停止中の鍵盤プレビュー: note_on で該当 note 位置から free-run 再生し、
    /// fade-in で declick、 buf 終端で自動停止する。
    #[test]
    fn preview_voice_on_note_when_stopped() {
        let (mut h, result) = mk_half(48_000.0);
        let mut offsets = HashMap::new();
        offsets.insert(0, 0); // note 0 → buf offset 0
        result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(ramp_buf(128)),
            sample_rate: 48_000,
            samples_per_beat: 24_000.0, // 48000*60/120
            note_offsets: Arc::new(offsets),
        })));
        let events = vec![TimedNoteEvent {
            time: 0,
            event: crate::plugin_instance::NoteTransition::On {
                note_id: 0,
                key: 60,
                velocity: 127.0,
            },
        }];
        // stopped transport → preview パス。
        h.process(64, &events, &[], &[], &[], &transport()).unwrap();
        // 音が乗る (fade-in で振幅は下がるが非ゼロ)。
        assert!(h.out_l[1..64].iter().any(|&v| v != 0.0));
        // cursor は fade に依らず ratio 1.0 で 64 進む。
        assert_eq!(h.preview.as_ref().unwrap().cursor, 64.0);
        // 次 process (note 無し) で残り 64 流して終端 → preview 破棄。
        h.process(64, &[], &[], &[], &[], &transport()).unwrap();
        assert!(h.preview.is_none());
    }
}
