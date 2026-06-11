//! VOICEVOX 内蔵 instrument plugin (`docs/plan_voicevox_synth.md` PR-V2)。
//!
//! ## 現状 (PR-V2.3 完了時点)
//!
//! - `set_note_metadata(bpm, entries)` で歌詞 + note 配列を受信、
//!   background synth thread に job を投げる
//! - synth thread が `common::voicevox::synthesize_notes_for_builtin` で
//!   HTTP call → 合成済 mono PCM を `Arc<ArcSwapOption<SynthResult>>`
//!   に格納 (= audio thread は lock-free `load()` で snapshot 取得、
//!   write contention で blocking しない)
//! - `process()` で MIDI note_on event を受信したら synth result を
//!   playhead として持ち上げ、 stereo output に mix。 1 voice (= 全
//!   合成 wav を 1 連続再生) という MVP。 per-note voice / global
//!   transport 同期は PR-V2.4 / V2.5 で改善
//!
//! ## state save / restore
//!
//! `VoicevoxState { speaker_id, style_name }` のみ bincode で保存。
//! 合成済 wav cache は **保存しない** (= load 時に re-synth)。 これは
//! project file size を抑えるため + speaker / style 変更時に必ず
//! re-synth 必要なので「最初から re-synth」 で統一。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use arc_swap::ArcSwapOption;
use bincode::{Decode, Encode};
use common::plugin_db::BUILTIN_ID_VOICEVOX;
use common::plugin_format::PluginFormat;
use common::plugin_metadata::NoteMetadata;
use common::protocol::RenderMode;
use common::voicevox::{BuiltinNoteSpec, BuiltinSynthOutput, synthesize_notes_for_builtin};

use crate::plugin_instance::{AuxInputBuf, LoadedPlugin, TimedNoteEvent};

/// `VoicevoxBuiltin` の persistent state (project file に bincode で埋め込み)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct VoicevoxState {
    /// VOICEVOX engine の `/singers` 経由で取得する speaker id。
    /// default = 6 (= ずんだもん「ノーマル」)、 user が GUI で変えると
    /// plugin parameter として保持される。
    pub speaker_id: u32,
    /// `style_name` は表示用 (= plugin GUI / Inspector で見える)。
    /// 内部処理は speaker_id だけで足りるが、 user に「どの speaker か」
    /// を見せるため keep。
    pub style_name: String,
}

impl Default for VoicevoxState {
    fn default() -> Self {
        // VOICEVOX `frame_synthesis` (= sing 合成) は **歌唱可能な
        // singer style id** を要求する。 talk speaker id (= 6 等) を
        // 渡すと 500 Internal Server Error。 既存 `common::voicevox::
        // DEFAULT_SINGER_ID = 3061` (= 春日部つむぎ ノーマル相当) を
        // builtin plugin の default に揃える。 user が GUI で speaker
        // を変えた値は state_save で project file に永続化される
        // (PR-V2.5)。
        Self {
            speaker_id: common::voicevox::DEFAULT_SINGER_ID,
            style_name: "ノーマル".to_string(),
        }
    }
}

/// 合成 thread に渡す 1 job。 (FIXME #36) **声 (speaker) 単位**でグループ化した
/// spec 列を持つ。 同じ声の clip 群を 1 query でまとめて合成することで、 旧
/// single-WAV と同じく声内で音量が一貫する (clip 別に合成すると VOICEVOX の
/// レンダ差で clip ごとに音量がばらつく)。 声違いの clip は別 spec (= 別 WAV)。
struct SynthJob {
    bpm: f32,
    groups: Vec<SpeakerSynthSpec>,
}

/// (FIXME #36) 1 声 (speaker) 分の合成指定。 その声を使う全 clip の note を
/// song-absolute timing で 1 つにまとめ、 1 回の `frame_synthesis` で合成する。
struct SpeakerSynthSpec {
    speaker_id: u32,
    notes: Vec<BuiltinNoteSpec>,
}

/// 合成済み 1 声グループの配置結果。
/// `(buffer 配置サンプル位置, mono WAV, [(note_id, buffer 内絶対 offset)])`。
type PlacedGroup = (usize, Vec<f32>, Vec<(u32, u64)>);

/// 合成完了時に共有される結果。 `Arc<Mutex<Option<...>>>` で audio thread と
/// synth thread が共有 (= synth が新結果を書く時のみ lock、 audio thread
/// は voice 開始時 1 度 clone してから unlock — process loop は lock 無し)。
#[derive(Clone)]
struct SynthResult {
    samples: Arc<Vec<f32>>, // mono
    sample_rate: u32,
    /// `note_id → synth wav 内 frame offset`。 PR-V2.4 で per-note voice
    /// に拡張するときに使う (= 現在 MVP では未参照、 logging のみ)。
    #[allow(dead_code)]
    note_offsets: Arc<HashMap<u32, u64>>,
}

/// PR-V2.4: 再生中 voice。 note_on event 受信時に synth_result.note_offsets
/// から該当 note の wav 開始 frame を取得して push、 process() で mix、
/// wav 終端で自動停止 (= note_off は無視、 VOICEVOX の出力は envelope を
/// 内包するので wav 自体の終わり = 歌い終わりが自然)。 同 note_id の
/// retrigger は voice を上書き (= 後勝ち、 「歌詞編集で同 note を再合成」
/// した場合に古い voice を切って新 voice に差し替える挙動)。
struct Voice {
    /// 起動 note_id (debug / introspection 用に保持。 PR-V2.4 MVP は
    /// 単一 voice rolling なので識別には現状使われていない)。
    #[allow(dead_code)]
    note_id: u32,
    samples: Arc<Vec<f32>>,
    /// synth wav の sample_rate。 PR-V2.4 polish の linear resample 実装で
    /// 出力 sample_rate とのミスマッチ補正に使う予定 (現状 mix は等倍)。
    #[allow(dead_code)]
    sample_rate: u32,
    velocity: f32,
    /// 次に samples から read する位置 (samples の絶対 index、
    /// `synth_offset` を起点に毎フレーム +1 する)。
    cursor: usize,
}

pub struct VoicevoxBuiltin {
    state: VoicevoxState,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    sample_rate: f64,
    activated: bool,

    /// `note_id → 歌詞` (debug / introspection 用に保持)。 実際の synth
    /// は `set_note_metadata` 受信時にすべて synth thread に丸投げ。
    lyrics: HashMap<u32, String>,

    // --- synth pipeline (PR-V2.3) ---------------------------------------
    /// synth thread への job 投入口。 deactivate で `Drop` (= synth
    /// thread が channel 閉鎖を見て exit)。
    synth_tx: Option<mpsc::Sender<SynthJob>>,
    /// synth thread の join handle (deactivate / drop で join)。
    synth_thread: Option<JoinHandle<()>>,
    /// synth processing thread への shutdown 通知。 engine 未起動で job が
    /// 永久 retry し続けるケースでも、 `stop_synth_thread` がこれを
    /// `store(true)` すれば processing thread が次のループ先頭 / synth
    /// 分岐前で検知して即 return し、 `join()` が無限ブロックしない。
    synth_shutdown: Arc<AtomicBool>,
    /// 共有 synth 結果。 synth thread が完了で `store(Some(...))`、 audio
    /// thread が note_on で `load()` する。 `ArcSwapOption` は lock-free
    /// (= 内部は atomic ptr + RCU-style epoch reclamation)、 audio thread
    /// が write contention で blocking しない (旧 `Arc<RwLock<_>>` は
    /// write 中に read が block する RT 違反だった)。
    synth_result: Arc<ArcSwapOption<SynthResult>>,

    // --- process state (audio thread) -----------------------------------
    /// 再生中 voice 群 (PR-V2.4)。 note_on で push、 wav 終端で自動 drain、
    /// 同 note_id の retrigger は既存 entry を上書き (後勝ち)。
    active_voices: Vec<Voice>,
    /// 前 buffer の transport 再生状態。 playing→stopped の edge を検出して
    /// Stop の瞬間に voice を drop するために保持する (process 参照)。
    was_playing: bool,
}

impl VoicevoxBuiltin {
    pub(super) fn new() -> Self {
        Self {
            state: VoicevoxState::default(),
            out_l: Vec::new(),
            out_r: Vec::new(),
            sample_rate: 0.0,
            activated: false,
            lyrics: HashMap::new(),
            synth_tx: None,
            synth_thread: None,
            synth_shutdown: Arc::new(AtomicBool::new(false)),
            synth_result: Arc::new(ArcSwapOption::from(None)),
            active_voices: Vec::new(),
            was_playing: false,
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
        // 直前セッションの stop で立った flag を必ずリセットしてから spawn。
        self.synth_shutdown.store(false, Ordering::SeqCst);
        let shutdown = Arc::clone(&self.synth_shutdown);
        // 直近 job だけ処理する (= 連続 flush は最後の 1 件だけ synth、
        // 古い job は drop)。 mpsc は順序保証だが coalescing は手動。
        // また、 synth 失敗 (= engine 起動中の接続失敗) で job を捨てると
        // 永久に音が鳴らないので、 失敗した job は同 slot に残して
        // 「retry pending」 にする。
        let coalesce = Arc::new(Mutex::new(None::<SynthJob>));
        let coalesce_recv = Arc::clone(&coalesce);

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
                // 真面目に condvar するほどのレイテンシ要求は無い (歌詞編集
                // から数百 ms ずれて synth 開始でも user 知覚しない)。
                let mut retry_after = std::time::Instant::now();
                loop {
                    // shutdown 通知が来ていれば即 exit (= engine 未起動で
                    // 永久 retry 中でも join() が無限ブロックしない)。
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    // retry backoff を尊重 (= 失敗直後の即時再 try を防ぐ)。
                    // backoff sleep 中も shutdown を即拾えるよう小刻みに分割。
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
                    // 新 job が来てれば take、 無ければ「前回失敗 job が
                    // pending か」 を見るために peek。
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
                            // shutdown 通知 or 受信 thread exit (= sender
                            // drop)、 keep alive する理由なし。
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    };
                    if job.groups.iter().all(|c| c.notes.is_empty()) {
                        result_arc.store(None);
                        continue;
                    }
                    // synth (= blocking HTTP) に入る直前に shutdown を再確認。
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    // (FIXME #36) clip ごとに自分の speaker で合成し、 各 clip の
                    // mono WAV を **song-absolute なサンプル位置** に配置した 1 本の
                    // バッファを作る (clip 間のギャップ = 無音、 旧 single-WAV と同じ
                    // 時間軸正しさを per-clip 声で再現)。 単純連結だと clip 間ギャップが
                    // 消えて rolling voice がギャップ中に次 clip を早鳴りするため不可。
                    // note_id は track 内通し番号で一意なので global map で衝突しない。
                    // process() は従来どおり単一 samples + note_offsets[note_id] の
                    // cursor jump のみ (= RT path 不変)。
                    let mut out_sr: u32 = 0;
                    // (placement_samples, mono WAV, [(note_id, abs_offset)]) を clip ごと収集。
                    let mut placed: Vec<PlacedGroup> = Vec::new();
                    let mut failed: Option<anyhow::Error> = None;
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
                                // placement = この声グループ先頭ノートの song-absolute
                                // サンプル位置。 各グループの WAV をこの位置から配置し、
                                // WAV 内 leading rest (REST_FRAMES) はそのまま buffer に
                                // 残す。 こうすると全グループで不変条件「buffer 位置 =
                                // note.start_beat*spb + lead_in」 (= rolling voice の
                                // cursor と playhead の差が常に lead_in) が揃う。
                                // note.start_beat は sync_vocal_metadata で song-absolute 化済み。
                                //
                                // 旧実装は placement から WAV 内 local offset (= 先頭ノートの
                                // lead_in) を引いていた。 beat 0 始まりのグループは placement が
                                // 負 → max(0) で潰れて k=lead_in、 一方 beat 8 始まりのグループは
                                // 引かれて k=0 となり、 cursor 差がグループ間で食い違う。 結果、
                                // 声違いで別 WAV になった次グループ先頭ノートが、 前グループ由来の
                                // rolling voice に note_on より lead_in (≈107ms) 早く鳴らされ、
                                // note_on の retrigger と二重発音になっていた (= 「ななつの」)。
                                // 全グループを earliest*spb 配置に統一すると cursor 差が lead_in で
                                // 揃い、 rolling voice が境界の手前で次グループを先取りしない。
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
                                // 各ノートの絶対 offset = placement + WAV 内 local offset。
                                let abs: Vec<(u32, u64)> = note_offsets
                                    .iter()
                                    .map(|(nid, off)| {
                                        (*nid, (place_samples as u64).saturating_add(*off))
                                    })
                                    .collect();
                                placed.push((place_samples, samples, abs));
                            }
                            Err(e) => {
                                failed = Some(e);
                                break;
                            }
                        }
                    }
                    if let Some(e) = failed {
                        tracing::error!(
                            error = ?e,
                            "VoicevoxBuiltin: synth failed (engine 起動済? localhost:50021)"
                        );
                        // engine 起動中で接続失敗の可能性が高いので、 job を
                        // coalesce slot に戻して 1.5s 後に retry (clip の 1 つでも
                        // 失敗したら job 全体を再試行 = 部分結果は保存しない)。
                        // 受信 thread が新 job を入れたら override される。
                        if let Ok(mut slot) = coalesce.lock()
                            && slot.is_none()
                        {
                            *slot = Some(job);
                        }
                        retry_after = std::time::Instant::now()
                            + std::time::Duration::from_millis(1500);
                    } else if placed.is_empty() {
                        result_arc.store(None);
                    } else {
                        // track 長 = max(placement + WAV 長)。 clip 間ギャップは 0 (無音)。
                        let total = placed
                            .iter()
                            .map(|(p, s, _)| p + s.len())
                            .max()
                            .unwrap_or(0);
                        let mut buf = vec![0.0f32; total];
                        let mut global_offsets: HashMap<u32, u64> = HashMap::new();
                        for (place, samples, abs) in &placed {
                            // 重なる clip は mix (加算)。 通常 clip は重ならないので copy 相当。
                            for (i, s) in samples.iter().enumerate() {
                                buf[place + i] += *s;
                            }
                            for (nid, off) in abs {
                                global_offsets.insert(*nid, *off);
                            }
                        }
                        let res = SynthResult {
                            samples: Arc::new(buf),
                            sample_rate: out_sr,
                            note_offsets: Arc::new(global_offsets),
                        };
                        result_arc.store(Some(Arc::new(res)));
                        tracing::info!(
                            speaker_groups = placed.len(),
                            "VoicevoxBuiltin: synth complete (per-speaker-group, song-absolute)"
                        );
                    }
                }
            });

        // spawn 失敗 (= OS thread 上限等) で plugin-main を panic で落とさない。
        // synth_tx / synth_thread は None のままにして、 次回 flush で再試行
        // 可能にする (= synth_shutdown は spawn 前に false へ戻してある)。
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
        // shutdown flag を立てると、 engine 未起動で job が永久 retry
        // 中の processing thread もループ先頭 / synth 分岐前で検知して
        // 即 return する (= sender drop だけでは止まらないケースを潰す)。
        self.synth_shutdown.store(true, Ordering::SeqCst);
        // sender を drop すると recv thread が抜け → 処理 thread も exit。
        self.synth_tx = None;
        if let Some(handle) = self.synth_thread.take() {
            // join は deactivate path で同期的に待つ。 plugin は plugin-
            // main thread 上にあるので audio thread から呼ばれることはない。
            let _ = handle.join();
        }
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

    fn activate(
        &mut self,
        sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> Result<()> {
        self.sample_rate = sample_rate;
        let cap = max_frames as usize;
        self.out_l.clear();
        self.out_l.resize(cap, 0.0);
        self.out_r.clear();
        self.out_r.resize(cap, 0.0);
        self.activated = true;
        self.start_synth_thread();
        Ok(())
    }

    fn deactivate(&mut self) {
        self.activated = false;
        self.active_voices.clear();
        self.stop_synth_thread();
    }

    fn start_processing(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop_processing(&mut self) {
        self.active_voices.clear();
    }

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

        // Transport gating (playing→stopped edge): Stop した瞬間に再生中の
        // 歌声 voice を drop する。 engine の Instrument dispatch は playing で
        // gate されず (= release tail を鳴らすため)、 さらに本 plugin は
        // note_off を無視して voice.cursor を playhead と独立に drain するため、
        // ここで止めないと Stop しても合成 wav が末尾まで鳴り続ける
        // (= audio clip が engine.rs の `if playing` で gate されているのと同義)。
        // 毎 buffer 無条件 clear ではなく edge 検出にするのは、 piano_roll の
        // 鍵盤プレビュー (transport 停止中でも note_on を注入して試聴する) を
        // 潰さないため。 停止後は sequencer が note_on を出さないので edge 1 回の
        // clear で十分。 RT 安全: bool 1 個の読み書き + Vec::clear (truncate のみ、
        // 容量保持・dealloc なし)、 出力は上で 0 fill 済。
        let stopped_edge = self.was_playing && !transport.is_playing;
        self.was_playing = transport.is_playing;
        if stopped_edge {
            self.active_voices.clear();
        }

        // PR-V2.4: per-note voice。 note_on を受信したら synth_result の
        // note_offsets から該当 note_id の wav 開始 frame を引き、 既存
        // voice (同 note_id) があれば置き換え、 無ければ push。 note_off
        // は無視 (= VOICEVOX 出力は envelope を内包、 wav 終端まで自然停
        // 止が歌唱として自然)。 同 buffer 内で event.time を考慮する
        // resample は polish。
        for ev in events {
            if let crate::plugin_instance::NoteTransition::On { note_id, velocity, .. } = ev.event {
                // Phase 6 review (RT 安全性): 旧 `Arc<RwLock<_>>` の
                // `.read()` は write 競合で audio thread を blocking
                // させる可能性があった。 `ArcSwapOption::load_full()` は
                // lock-free な atomic load + Arc clone (= refcount bump)
                // で snapshot を取得、 heap 確保 / lock なし。
                let snapshot = self.synth_result.load_full();
                let Some(res) = snapshot else {
                    continue;
                };
                let synth_offset = res
                    .note_offsets
                    .get(&note_id)
                    .copied()
                    .unwrap_or(0) as usize;
                if synth_offset >= res.samples.len() {
                    // synth から note_id が外れている (= 歌詞数とずれた)
                    // 場合は無視。
                    continue;
                }
                // VOICEVOX wav は「全 note 連続録音」 として返される
                // (= note 1 区間 [t1..t2]、 note 2 区間 [t2..t3]、 …)。
                // 複数 voice を並行再生すると、 同じ wav の被る区間が
                // 複数の cursor で同時に流れて位相干渉 → 音量低下 +
                // 不自然な音。 note_on を「現在再生中 voice の置換 (=
                // rolling)」 として扱い、 active_voices を 1 voice
                // 単一に保つ。 wav は連続なので、 cursor を新 note の
                // synth_offset に jump させれば実質「VOICEVOX が想定
                // した連続再生」 と等価。
                let voice = Voice {
                    note_id,
                    samples: Arc::clone(&res.samples),
                    sample_rate: res.sample_rate,
                    velocity: velocity as f32,
                    cursor: synth_offset,
                };
                self.active_voices.clear();
                self.active_voices.push(voice);
            }
            // note_off は無視。
        }

        // active voices ごとに audio を mix。 sample_rate ミスマッチは
        // 現状 resample しない (PR-V2.4 polish で linear resample 予定)。
        // 出力 buffer は max_frames で確保しているが、 frames > max_frames
        // でも panic しないよう out 側 capacity でもクランプする (Silence
        // path と同様)。
        let out_max = self.out_l.len().min(self.out_r.len());
        let mut i = 0usize;
        while i < self.active_voices.len() {
            let drop = {
                let voice = &mut self.active_voices[i];
                // PR-V2.4 fix: NoteTransition::On.velocity は既に
                // sequencer で `note.velocity / 127.0` (= 0..1 範囲) に
                // 正規化されている。 ここで更に 127 で割ると 0.0062 倍
                // (約 -44 dB) になり「とても小さい」 という症状になる。
                let amp = voice.velocity.clamp(0.0, 1.0);
                let remaining = voice.samples.len().saturating_sub(voice.cursor);
                let copy_n = remaining.min(frames_usize).min(out_max);
                for k in 0..copy_n {
                    let s = voice.samples[voice.cursor + k] * amp;
                    self.out_l[k] += s;
                    self.out_r[k] += s;
                }
                voice.cursor += copy_n;
                voice.cursor >= voice.samples.len()
            };
            if drop {
                self.active_voices.swap_remove(i);
            } else {
                i += 1;
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
        // 合成 cache は state に含まれないので、 restore 直後は cache miss
        // = 無音再生。 次の set_note_metadata (= project load 完了後の
        // initial flush) で synth が走り cache が温まる。
        Ok(())
    }

    fn set_note_metadata(&mut self, bpm: f32, entries: &[NoteMetadata]) {
        // 内部 lyrics map を完全置換 (introspection 用)。
        self.lyrics.clear();
        for e in entries {
            self.lyrics.insert(e.note_id, e.lyric.clone());
        }

        // synth thread を起動していなければ作る (activate 前に来た flush
        // にも対応)。
        if self.synth_tx.is_none() {
            self.start_synth_thread();
        }

        // BuiltinNoteSpec 配列を組み立てて synth thread に送る。 entries が
        // 空なら synth result を None にする job を送る (= 再生停止信号
        // 兼ねる)。
        // (FIXME #36) entries を **声 (speaker) でグルーピング**して spec に。
        // 同じ声の clip 群を 1 query でまとめて合成すると、 旧 single-WAV と同じく
        // 声内で音量が一貫する (clip 別合成は VOICEVOX のレンダ差で clip ごとに
        // 音量ばらつき)。 speaker_id 0 = 未設定は DEFAULT_SINGER_ID に解決して
        // からグループ化する (= 未設定 clip と既定声 clip を同声としてまとめる)。
        // note の song-absolute timing は entry が保持。
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

        if let Some(tx) = self.synth_tx.as_ref() {
            let _ = tx.send(SynthJob { bpm, groups });
        }
        tracing::debug!(
            count = self.lyrics.len(),
            bpm,
            "VoicevoxBuiltin: lyrics buffer updated, synth job queued"
        );
    }

    // --- Embedded GUI (PR-V2.4 で speaker picker / progress bar) -----------
    fn gui_is_embed_supported(&self) -> bool {
        false
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        bail!("VoicevoxBuiltin: GUI 未実装 (PR-V2.4 予定)")
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
        bail!("VoicevoxBuiltin: GUI 未実装")
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

    #[test]
    fn default_state_uses_sing_speaker() {
        let s = VoicevoxState::default();
        // sing 合成可能 speaker (= common::voicevox::DEFAULT_SINGER_ID)
        // が default。 talk speaker id (= 6) を渡すと frame_synthesis が
        // 500 を返すので注意。
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
        let p = VoicevoxBuiltin::new();
        assert_eq!(p.id(), BUILTIN_ID_VOICEVOX);
        assert_eq!(p.format(), PluginFormat::Builtin);
        assert_eq!(p.name(), "VOICEVOX (builtin)");
    }

    #[test]
    fn voicevox_process_silent_when_no_synth_result() {
        let mut p = VoicevoxBuiltin::new();
        p.activate(48000.0, 0, 256).unwrap();
        p.start_processing().unwrap();
        p.out_l[0] = 0.5;
        // 合成結果なしで note_on を投げても active_voice は付かない。
        let events = vec![TimedNoteEvent {
            time: 0,
            event: crate::plugin_instance::NoteTransition::On {
                note_id: 0,
                key: 60,
                velocity: 100.0,
            },
        }];
        let transport = crate::plugin_instance::TransportContext::from_process_data(
            &common::process_data::ProcessData::empty(),
        );
        p.process(64, &events, &[], &[], &[], &transport).unwrap();
        assert!(p.out_l[..64].iter().all(|&v| v == 0.0));
        assert!(p.active_voices.is_empty());
        p.deactivate();
    }

    #[test]
    fn voicevox_state_save_returns_bytes() {
        let p = VoicevoxBuiltin::new();
        let bytes = p.state_save().unwrap().expect("Some bytes");
        assert!(!bytes.is_empty());
        let cfg = bincode::config::standard();
        let (s, _): (VoicevoxState, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(s, VoicevoxState::default());
    }

    #[test]
    fn set_note_metadata_replaces_lyrics_buffer() {
        let mut p = VoicevoxBuiltin::new();
        let mk = |note_id: u32, lyric: &str| NoteMetadata {
            note_id,
            lyric: lyric.to_string(),
            ..NoteMetadata::default()
        };
        // synth_tx は activate 前の flush でも自動起動するので、 thread が
        // 起動して channel に送信される (= HTTP は呼ばれるが engine 不在で
        // 失敗ログのみ、 test 自体には影響なし)。
        p.set_note_metadata(120.0, &[mk(0, "あ"), mk(1, "い")]);
        let lyrics = p.lyrics_for_test();
        assert_eq!(lyrics.len(), 2);
        assert_eq!(lyrics.get(&1).map(String::as_str), Some("い"));
        // 空 flush で全消去。
        p.set_note_metadata(120.0, &[]);
        assert_eq!(p.lyrics_for_test().len(), 0);
        // synth thread を停止 (= test process が hang しないよう)。
        p.stop_synth_thread();
    }

    #[test]
    fn voice_drains_synth_result() {
        // synth_result に手で SynthResult を仕込んで、 process が note_on
        // で voice を起こして wav を流すかを確認 (HTTP を介さない)。
        let mut p = VoicevoxBuiltin::new();
        p.activate(48000.0, 0, 256).unwrap();
        let samples: Vec<f32> = (0..128).map(|i| (i as f32) * 0.001).collect();
        let mut offsets = HashMap::new();
        offsets.insert(0, 0);
        p.synth_result.store(Some(Arc::new(SynthResult {
            samples: Arc::new(samples),
            sample_rate: 48000,
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
        let transport = crate::plugin_instance::TransportContext::from_process_data(
            &common::process_data::ProcessData::empty(),
        );
        p.process(64, &events, &[], &[], &[], &transport).unwrap();
        // out_l に値が乗っていることを確認 (= sample[0] = 0.0 はゼロだが
        // sample[1] 以降は非ゼロ)。
        assert!(p.out_l[1..64].iter().any(|&v| v != 0.0));
        // voice が cursor を進めていること。
        assert_eq!(p.active_voices[0].cursor, 64);
        // 次 process で残り 64 frame 流して voice 終了。
        p.process(64, &[], &[], &[], &[], &transport).unwrap();
        assert!(p.active_voices.is_empty());
        p.deactivate();
    }
}
