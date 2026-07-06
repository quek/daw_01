//! VOICEVOX 内蔵 instrument plugin (`docs/plan_voicevox_synth.md` PR-V2)。
//!
//! Split-half (`docs/plan_arch_refactor.md` §6): [`VoicevoxBuiltin`] が
//! main half (state / lyrics / synth thread pipeline)、[`VoicevoxAudioHalf`]
//! が audio half (出力 buffer / voice / transport edge)。両 half は
//! `Arc<ArcSwapOption<SynthResult>>` (lock-free) だけを共有するので、
//! `set_note_metadata` (main) と `process()` (worker) の並行実行は Rust の
//! aliasing 的にも RT 的にも安全。
//!
//! - `set_note_metadata(bpm, entries, talk)` で歌詞 + note 配列を受信、
//!   background synth thread に job を投げる (VocalSynth capability 経由)
//! - synth thread が HTTP 合成 → mono PCM を `ArcSwapOption` に格納
//! - audio half の `process()` が note_on で synth result から voice を
//!   起こして stereo output に mix
//!
//! ## state save / restore
//!
//! `VoicevoxState { speaker_id, style_name }` のみ bincode で保存。合成済
//! wav cache は保存しない (= load 時に re-synth)。
//!
//! ## 合成状態の報告
//!
//! 合成状態 `(busy, failure)` は `HostCallbacks::on_vocal_synth_status` で
//! 報告する (旧 `set_voicevox_status_reporter` の第 2 callback 登録機構は
//! 廃止 — load 時に device_id を capture した callback が渡ってくる)。

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
use common::protocol::{RenderMode, VocalSynthFailure};
use crate::builtin::voicevox_synth::{
    BuiltinNoteSpec, BuiltinSynthOutput, SynthError, lead_in_frames,
    synthesize_notes_for_builtin, synthesize_talk_for_builtin,
};

use crate::plugin_instance::{
    AudioHalf, AudioProcessorHalf, AuxInputBuf, LoadedPlugin, TimedNoteEvent, VocalSynth,
};

/// 合成状態 `(busy, failure)` の報告先 (= `HostCallbacks::on_vocal_synth_status`)。
/// `failure` は engine 到達可否を区別する ([`VocalSynthFailure`])。
type StatusFn = Arc<dyn Fn(bool, VocalSynthFailure) + Send + Sync>;

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

/// 合成 thread に渡す 1 job。**声 (speaker) 単位**でグループ化した spec 列。
struct SynthJob {
    bpm: f32,
    groups: Vec<SpeakerSynthSpec>,
    /// (talk) `ClipContent::Text` 由来の読み上げ群。
    talk: Vec<TalkSynthSpec>,
    /// この job の世代 (= `synth_queued_gen` の値)。
    generation: u64,
}

/// 1 声 (speaker) 分の合成指定。
struct SpeakerSynthSpec {
    speaker_id: u32,
    notes: Vec<BuiltinNoteSpec>,
}

/// (talk) 1 件の読み上げの合成指定。
struct TalkSynthSpec {
    event_id: u32,
    start_beat: f64,
    text: String,
    speaker_id: u32,
    scales: TalkParams,
}

/// 合成済み 1 声グループの配置結果。
/// `(buffer 配置サンプル位置, mono WAV, [(note_id, buffer 内絶対 offset)])`。
type PlacedGroup = (usize, Vec<f32>, Vec<(u32, u64)>);

/// 合成完了時に共有される結果 (synth thread が store、audio half が
/// lock-free load)。
#[derive(Clone)]
struct SynthResult {
    samples: Arc<Vec<f32>>, // mono
    sample_rate: u32,
    /// buffer を配置したときの **synth 時 (job.bpm) の samples-per-beat** (= sample_rate*60/bpm)。
    /// 連続再生は playhead 拍 → buffer 位置写像にこれを使う (再生時の transport.bpm ではなく)。
    /// buffer は job.bpm で拍配置されており、 tempo 変更直後の re-synth 完了までの過渡で
    /// transport.bpm と一致しないことがあるため、 配置時の値を SSoT として持ち回る (r.md #23)。
    samples_per_beat: f64,
    /// `note_id → synth wav 内 frame offset`。
    note_offsets: Arc<HashMap<u32, u64>>,
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
}

impl VoicevoxAudioHalf {
    fn new(synth_result: Arc<ArcSwapOption<SynthResult>>) -> Self {
        Self {
            out_l: Vec::new(),
            out_r: Vec::new(),
            sample_rate: 0.0,
            preview: None,
            was_playing: false,
            synth_result,
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
        // process() 末尾で drop = RT スレッド上で解放、 を避ける (CLAUDE.md「解放禁止」)。 Guard は
        // Arc を clone せず借用し、 retired 値は writer (synth thread、 非 RT) 側で drop される
        // — daw_audio の RT パスと同じ idiom (automation.rs / audio_worker.rs)。
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
            // 拍 → buffer 位置: `buf_pos(beat) = beat * samples_per_beat + lead_in`。
            // `samples_per_beat` は配置時 (synth 時 job.bpm) の値を SynthResult に持ち回る
            // (再生時 transport.bpm ではなく = tempo 変更過渡の drift を避ける、 下記参照)。
            // `lead_in` は build_sing_query が wav 先頭に入れる attack 用無音で、 note 音声開始は
            // beat-grid から lead_in 分後ろにある → 読み出しを lead_in 進める (`lead_in_frames`
            // を synth と共有 = SSoT)。
            if let Some(res) = snapshot.as_ref() {
                let src_len = res.samples.len();
                if host_sr > 0.0 && res.sample_rate > 0 && res.samples_per_beat > 0.0 && src_len > 0
                {
                    let src_sr = f64::from(res.sample_rate);
                    // 拍→buffer 位置は **synth 時の** samples_per_beat を使う (transport.bpm ではない)。
                    // buffer は job.bpm で配置され、 song_pos_beats は tempo 積分済の真の拍位置なので、
                    // これで定テンポは常に厳密、 tempo 変更過渡でも base が各 buffer で song_pos_beats から
                    // 再同期する。 talk は自前の pre-silence を持ち lead_in を配置に含めないため、 この
                    // lead_in 加算で ~7ms 早まるが (VOICEVOX 既定 prePhonemeLength≈lead_in)、 sing の
                    // 拍合わせ (歌声が主用途) を優先する。
                    let base =
                        transport.song_pos_beats * res.samples_per_beat + lead_in_frames(res.sample_rate);
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
                {
                    let synth_offset =
                        res.note_offsets.get(&note_id).copied().unwrap_or(0) as usize;
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
    /// 合成状態 `(busy, failure)` の報告 callback
    /// (`HostCallbacks::on_vocal_synth_status`、load 時に device_id capture 済)。
    report: StatusFn,

    /// Audio half (worker registry と共有)。
    audio: Arc<AudioHalf>,
}

impl VoicevoxBuiltin {
    pub(super) fn new(report: StatusFn) -> Self {
        let synth_result: Arc<ArcSwapOption<SynthResult>> = Arc::new(ArcSwapOption::from(None));
        let audio = AudioHalf::new(Box::new(VoicevoxAudioHalf::new(Arc::clone(&synth_result))));
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
            report,
            audio,
        }
    }

    /// synth thread 用の dedup 付き状態報告。`last` と一致するときは送らない。
    fn synth_report(
        reporter: &StatusFn,
        last: &mut Option<(bool, VocalSynthFailure)>,
        busy: bool,
        failure: VocalSynthFailure,
    ) {
        if last.as_ref() != Some(&(busy, failure.clone())) {
            *last = Some((busy, failure.clone()));
            reporter(busy, failure);
        }
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
        // 合成状態 (busy / failing) を daw_gui へ報告する callback。
        let status_reporter = Arc::clone(&self.report);

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
                let mut last_status: Option<(bool, VocalSynthFailure)> = None;
                let mut retry_after = std::time::Instant::now();
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
                    if job.groups.iter().all(|c| c.notes.is_empty())
                        && job.talk.is_empty()
                    {
                        result_arc.store(None);
                        done_gen.store(job.generation, Ordering::SeqCst);
                        // 歌唱/読み上げ無し = 合成しない = idle。
                        Self::synth_report(
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
                        Some((_, VocalSynthFailure::Unreachable)) => VocalSynthFailure::Unreachable,
                        _ => VocalSynthFailure::None,
                    };
                    Self::synth_report(&status_reporter, &mut last_status, true, prev_failure);
                    // clip ごとに自分の speaker で合成し、各 clip の mono WAV
                    // を song-absolute なサンプル位置に配置した 1 本のバッファ
                    // を作る。engine 到達済で一部 group が拒否 (不正歌詞等) されても、
                    // 合成できた group は placed に積んで再生する (1 歌詞で全滅させない)。
                    let mut out_sr: u32 = 0;
                    let mut placed: Vec<PlacedGroup> = Vec::new();
                    // engine 未到達 (retry 対象) と 内容拒否 (retry 無意味) を区別する。
                    let mut unreachable_reason: Option<String> = None;
                    let mut rejected_detail: Option<String> = None;
                    for spec in &job.groups {
                        if spec.notes.is_empty() {
                            continue;
                        }
                        if shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        match synthesize_notes_for_builtin(
                            &spec.notes,
                            job.bpm,
                            spec.speaker_id,
                        ) {
                            Ok(BuiltinSynthOutput {
                                samples,
                                sample_rate,
                                note_offsets,
                            }) => {
                                out_sr = sample_rate;
                                let spb = f64::from(sample_rate) * 60.0
                                    / f64::from(job.bpm.max(0.001));
                                // placement = この声グループ先頭ノートの
                                // song-absolute サンプル位置。全グループを
                                // earliest*spb 配置に統一すると cursor 差が
                                // lead_in で揃い、rolling voice が境界の手前で
                                // 次グループを先取りしない。
                                let earliest = spec
                                    .notes
                                    .iter()
                                    .map(|n| n.start_beat)
                                    .fold(f64::INFINITY, f64::min);
                                let place_samples = if earliest.is_finite() {
                                    (earliest * spb).max(0.0).round() as usize
                                } else {
                                    0
                                };
                                // 各ノートの絶対 offset = placement + local offset。
                                let abs: Vec<(u32, u64)> = note_offsets
                                    .iter()
                                    .map(|(nid, off)| {
                                        (*nid, (place_samples as u64).saturating_add(*off))
                                    })
                                    .collect();
                                placed.push((place_samples, samples, abs));
                            }
                            // engine 未到達 = 全 group に影響するので即中断して retry へ。
                            Err(SynthError::Unreachable(e)) => {
                                unreachable_reason = Some(format!("{e:#}"));
                                break;
                            }
                            // 内容拒否 = この group だけ諦めて他は続行。
                            Err(SynthError::Rejected(d)) => {
                                rejected_detail.get_or_insert(d);
                            }
                        }
                    }
                    // (talk) 読み上げ群を同じ placed バッファへ (engine 到達済のときのみ)。
                    if unreachable_reason.is_none() {
                        for tspec in &job.talk {
                            if tspec.text.is_empty() {
                                continue;
                            }
                            if shutdown.load(Ordering::SeqCst) {
                                return;
                            }
                            match synthesize_talk_for_builtin(
                                &tspec.text,
                                tspec.speaker_id,
                                &tspec.scales,
                            ) {
                                Ok((samples, sample_rate)) => {
                                    out_sr = sample_rate;
                                    let spb = f64::from(sample_rate) * 60.0
                                        / f64::from(job.bpm.max(0.001));
                                    let place_samples =
                                        (tspec.start_beat * spb).max(0.0).round() as usize;
                                    placed.push((
                                        place_samples,
                                        samples,
                                        vec![(tspec.event_id, place_samples as u64)],
                                    ));
                                }
                                Err(SynthError::Unreachable(e)) => {
                                    unreachable_reason = Some(format!("{e:#}"));
                                    break;
                                }
                                Err(SynthError::Rejected(d)) => {
                                    rejected_detail.get_or_insert(d);
                                }
                            }
                        }
                    }
                    if let Some(reason) = unreachable_reason {
                        tracing::warn!(
                            reason = %reason,
                            "VoicevoxBuiltin: engine unreachable (localhost:50021 起動待ち), retry"
                        );
                        // engine 未接続 = busy のまま Unreachable。job を戻して retry。
                        // done_gen は進めない (bounce 待ちは engine 復帰まで待つ)。
                        Self::synth_report(
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
                    } else {
                        // engine には到達済。合成できた group で 1 本のバッファを作る
                        // (placed が空なら無音 = None)。この job は終端 (retry しない)。
                        if placed.is_empty() {
                            result_arc.store(None);
                        } else {
                            // track 長 = max(placement + WAV 長)。
                            let total = placed
                                .iter()
                                .map(|(p, s, _)| p + s.len())
                                .max()
                                .unwrap_or(0);
                            let mut buf = vec![0.0f32; total];
                            let mut global_offsets: HashMap<u32, u64> = HashMap::new();
                            for (place, samples, abs) in &placed {
                                // 重なる clip は mix (加算)。
                                for (i, s) in samples.iter().enumerate() {
                                    buf[place + i] += *s;
                                }
                                for (nid, off) in abs {
                                    global_offsets.insert(*nid, *off);
                                }
                            }
                            // 配置は各 group で spb = out_sr*60/job.bpm を使っている (sing/talk 共通)。
                            // 連続再生の拍→buffer 写像に同じ値を持ち回る (r.md #23)。
                            let samples_per_beat =
                                f64::from(out_sr) * 60.0 / f64::from(job.bpm.max(0.001));
                            let res = SynthResult {
                                samples: Arc::new(buf),
                                sample_rate: out_sr,
                                samples_per_beat,
                                note_offsets: Arc::new(global_offsets),
                            };
                            result_arc.store(Some(Arc::new(res)));
                            tracing::info!(
                                speaker_groups = placed.len(),
                                "VoicevoxBuiltin: synth complete (per-speaker-group, song-absolute)"
                            );
                        }
                        done_gen.store(job.generation, Ordering::SeqCst);
                        // 次 job が pending 中は終端報告を送らない (coalesce 中の点滅回避)。
                        let pending = coalesce.lock().map(|s| s.is_some()).unwrap_or(false);
                        if !pending {
                            if let Some(detail) = rejected_detail {
                                tracing::warn!(
                                    detail = %detail,
                                    "VoicevoxBuiltin: synth rejected (bad input; not retrying)"
                                );
                                Self::synth_report(
                                    &status_reporter,
                                    &mut last_status,
                                    false,
                                    VocalSynthFailure::Rejected { detail },
                                );
                            } else {
                                Self::synth_report(
                                    &status_reporter,
                                    &mut last_status,
                                    false,
                                    VocalSynthFailure::None,
                                );
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
        (self.report)(false, VocalSynthFailure::None);
    }
}

impl VocalSynth for VoicevoxBuiltin {
    fn set_note_metadata(&mut self, bpm: f32, entries: &[NoteMetadata], talk: &[TalkMetadata]) {
        // 内部 lyrics map を完全置換 (introspection 用)。
        self.lyrics.clear();
        for e in entries {
            self.lyrics.insert(e.note_id, e.lyric.clone());
        }

        // synth thread を起動していなければ作る (activate 前の flush 対応)。
        if self.synth_tx.is_none() {
            self.start_synth_thread();
        }

        // entries を **声 (speaker) でグルーピング**して spec に (同じ声の
        // clip 群を 1 query でまとめると声内で音量が一貫する)。
        let mut by_speaker: HashMap<u32, Vec<BuiltinNoteSpec>> = HashMap::new();
        for e in entries {
            let speaker = if e.speaker_id != 0 {
                e.speaker_id
            } else {
                common::voicevox::DEFAULT_SINGER_ID
            };
            by_speaker.entry(speaker).or_default().push(BuiltinNoteSpec {
                note_id: e.note_id,
                start_beat: e.start_beat,
                duration_beats: e.duration_beats,
                pitch: e.pitch,
                velocity: e.velocity,
                lyric: e.lyric.clone(),
            });
        }
        let groups: Vec<SpeakerSynthSpec> = by_speaker
            .into_iter()
            .map(|(speaker_id, notes)| SpeakerSynthSpec { speaker_id, notes })
            .collect();

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
            })
            .collect();

        // 世代を 1 進めてから job に乗せる。
        let generation = self.synth_queued_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(tx) = self.synth_tx.as_ref() {
            let _ = tx.send(SynthJob {
                bpm,
                groups,
                talk: talk_specs,
                generation,
            });
        }
        tracing::debug!(
            count = self.lyrics.len(),
            talk = talk.len(),
            bpm,
            "VoicevoxBuiltin: lyrics buffer updated, synth job queued"
        );
    }

    /// 歌唱 bounce の合成完了待ち用に `(queued_gen, done_gen)` を公開。
    fn synth_progress(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>) {
        (
            Arc::clone(&self.synth_queued_gen),
            Arc::clone(&self.synth_done_gen),
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

    fn gui_can_resize(&self) -> bool {
        false
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

    fn gui_set_size(&self, _width: u32, _height: u32) -> Result<()> {
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
        Arc::new(|_, _: VocalSynthFailure| {})
    }

    /// audio half を直接組んで activate 相当まで進める test helper。
    fn mk_half(sample_rate: f64) -> (VoicevoxAudioHalf, Arc<ArcSwapOption<SynthResult>>) {
        let result: Arc<ArcSwapOption<SynthResult>> = Arc::new(ArcSwapOption::from(None));
        let mut h = VoicevoxAudioHalf::new(Arc::clone(&result));
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
        p.set_note_metadata(120.0, &[mk(0, "あ"), mk(1, "い")], &[]);
        let lyrics = p.lyrics_for_test();
        assert_eq!(lyrics.len(), 2);
        assert_eq!(lyrics.get(&1).map(String::as_str), Some("い"));
        // 空 flush で全消去。
        p.set_note_metadata(120.0, &[], &[]);
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
        }];
        p.set_note_metadata(120.0, &[], &talk);
        // synth thread (background, blocking HTTP) の完了を待つ。
        let (queued, done) = p.synth_progress();
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

    /// lead_in は 48kHz で 5120 sample (= 10 frame / 93.75fps × 48000)。 buf は
    /// song-absolute なので、 十分長い ramp buf を作れば連続再生の写像を厳密検証できる。
    fn ramp_buf(len: usize) -> Vec<f32> {
        (0..len).map(|i| i as f32).collect()
    }

    /// r.md #23: 再生中は playhead 位置から buf を連続読み出しする (note_on retrigger
    /// を廃止)。 拍 → buffer 位置 = `beat*spb + lead_in`。 lead_in の反映と連続性を検証。
    #[test]
    fn continuous_playback_reads_buf_at_playhead() {
        let (mut h, result) = mk_half(48_000.0);
        // lead_in(5120) を含むよう十分長い buf。 buf[i] = i。
        result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(ramp_buf(8192)),
            sample_rate: 48_000,
            samples_per_beat: 24_000.0, // 48000*60/120
            note_offsets: Arc::new(HashMap::new()),
        })));
        // playhead = 拍 0 → base = 0*spb + lead_in(5120)。 host==synth なので ratio=1。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.0)).unwrap();
        assert!((h.out_l[0] - 5120.0).abs() < 1e-3, "out_l[0]={}", h.out_l[0]);
        assert!((h.out_l[10] - 5130.0).abs() < 1e-3, "out_l[10]={}", h.out_l[10]);
        // 左右同一 (mono → stereo)。
        assert_eq!(h.out_l[10], h.out_r[10]);
        // playhead = 拍 0.01 → base = 0.01*24000 + 5120 = 5360。 note_on 無しでも
        // playhead から鳴る (mid-phrase 再生開始でも欠落しない)。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.01)).unwrap();
        assert!((h.out_l[0] - 5360.0).abs() < 1e-3, "out_l[0]={}", h.out_l[0]);
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
        // base = 0*spb + lead_in(5120)。 ratio 2.0 → out frame k = buf[5120 + 2k]。
        h.process(64, &[], &[], &[], &[], &transport_playing(0.0)).unwrap();
        assert!((h.out_l[1] - 5122.0).abs() < 1e-3, "out_l[1]={}", h.out_l[1]);
        assert!((h.out_l[10] - 5140.0).abs() < 1e-3, "out_l[10]={}", h.out_l[10]);
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
