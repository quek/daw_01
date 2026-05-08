//! VOICEVOX 内蔵 instrument plugin (`docs/plan_voicevox_synth.md` PR-V2)。
//!
//! ## 現状 (PR-V2.3 完了時点)
//!
//! - `set_note_metadata(bpm, entries)` で歌詞 + note 配列を受信、
//!   background synth thread に job を投げる
//! - synth thread が `common::voicevox::synthesize_notes_for_builtin` で
//!   HTTP call → 合成済 mono PCM を `Arc<RwLock<Option<SynthResult>>>`
//!   に格納
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
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
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
        Self {
            speaker_id: 6,
            style_name: "ノーマル".to_string(),
        }
    }
}

/// 合成 thread に渡す 1 job。
struct SynthJob {
    bpm: f32,
    speaker_id: u32,
    notes: Vec<BuiltinNoteSpec>,
}

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

/// 再生中 voice (= synth wav の playhead)。 PR-V2.3 MVP では 1 plugin
/// instance に高々 1 voice (= 「最初の note_on で全 wav 再生開始、 後続
/// note_on は ignore」)。 PR-V2.4 で per-note voice に拡張。
struct Voice {
    samples: Arc<Vec<f32>>,
    sample_rate: u32,
    velocity: f32,
    /// 次に samples から read する位置。
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
    /// 共有 synth 結果。 synth thread が完了で書き込む、 audio thread
    /// が note_on で read する。
    synth_result: Arc<RwLock<Option<SynthResult>>>,

    // --- process state (audio thread) -----------------------------------
    /// 再生中 voice。 1 個まで保持 (= MVP 単一 voice 設計)。
    active_voice: Option<Voice>,
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
            synth_result: Arc::new(RwLock::new(None)),
            active_voice: None,
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
        // 直近 job だけ処理する (= 連続 flush は最後の 1 件だけ synth、
        // 古い job は drop)。 mpsc は順序保証だが coalescing は手動。
        let coalesce = Arc::new(Mutex::new(None::<SynthJob>));
        let coalesce_recv = Arc::clone(&coalesce);

        let handle = std::thread::Builder::new()
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
                loop {
                    let job = {
                        let Ok(mut slot) = coalesce.lock() else {
                            return;
                        };
                        slot.take()
                    };
                    let Some(job) = job else {
                        if Arc::strong_count(&coalesce) <= 1 {
                            // 受信 thread が exit (= sender drop)、 keep
                            // alive する理由なし。
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    };
                    if job.notes.is_empty() {
                        if let Ok(mut w) = result_arc.write() {
                            *w = None;
                        }
                        continue;
                    }
                    match synthesize_notes_for_builtin(
                        &job.notes,
                        job.bpm,
                        job.speaker_id,
                    ) {
                        Ok(BuiltinSynthOutput {
                            samples,
                            sample_rate,
                            note_offsets,
                        }) => {
                            let res = SynthResult {
                                samples: Arc::new(samples),
                                sample_rate,
                                note_offsets: Arc::new(note_offsets),
                            };
                            if let Ok(mut w) = result_arc.write() {
                                *w = Some(res);
                            }
                            tracing::info!("VoicevoxBuiltin: synth complete");
                        }
                        Err(e) => {
                            tracing::error!(
                                error = ?e,
                                "VoicevoxBuiltin: synth failed (engine 起動済? localhost:50021)"
                            );
                        }
                    }
                }
            })
            .expect("spawn voicevox-builtin-synth thread");

        self.synth_tx = Some(tx);
        self.synth_thread = Some(handle);
    }

    fn stop_synth_thread(&mut self) {
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
        self.active_voice = None;
        self.stop_synth_thread();
    }

    fn start_processing(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop_processing(&mut self) {
        self.active_voice = None;
    }

    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        _input_audio: &[&[f32]],
        _aux_inputs: &[AuxInputBuf<'_>],
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

        // note_on event を受信したら synth_result から voice を起こす。
        // PR-V2.3 MVP: 単一 voice、 後続 note_on は ignore (= 既に流して
        // いる wav に subsume される)。
        for ev in events {
            if let crate::plugin_instance::NoteTransition::On { velocity, .. } =
                ev.event
                && self.active_voice.is_none()
            {
                let snapshot = self
                    .synth_result
                    .read()
                    .ok()
                    .and_then(|guard| guard.clone());
                if let Some(res) = snapshot {
                    self.active_voice = Some(Voice {
                        samples: res.samples,
                        sample_rate: res.sample_rate,
                        velocity: velocity as f32,
                        cursor: 0,
                    });
                }
            }
            // note_off は無視 (= synth wav 自体が envelope を含むので、
            // wav 終端に到達したら voice 自然停止)。
        }

        // active voice の audio を mix。 sample_rate ミスマッチは現状
        // resample しない (= VOICEVOX 出力 24000 Hz、 engine 48000 Hz の
        // 場合は再生速度が倍になる旨のログを 1 度だけ出す)。 PR-V2.4 で
        // 簡易 linear resample を入れる。
        if let Some(voice) = &mut self.active_voice {
            let sr_ratio_warning =
                voice.sample_rate as f64 != self.sample_rate;
            if sr_ratio_warning {
                tracing::debug!(
                    voice_sr = voice.sample_rate,
                    engine_sr = self.sample_rate,
                    "VoicevoxBuiltin: sample rate mismatch (PR-V2.4 で resample 予定)"
                );
            }
            let amp = (voice.velocity / 127.0).clamp(0.0, 1.0);
            let copy_n = (voice.samples.len() - voice.cursor).min(frames_usize);
            for i in 0..copy_n {
                let s = voice.samples[voice.cursor + i] * amp;
                self.out_l[i] += s;
                self.out_r[i] += s;
            }
            voice.cursor += copy_n;
            if voice.cursor >= voice.samples.len() {
                self.active_voice = None;
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
        let notes: Vec<BuiltinNoteSpec> = entries
            .iter()
            .map(|e| BuiltinNoteSpec {
                note_id: e.note_id,
                start_beat: e.start_beat,
                duration_beats: e.duration_beats,
                pitch: e.pitch,
                velocity: e.velocity,
                lyric: e.lyric.clone(),
            })
            .collect();

        if let Some(tx) = self.synth_tx.as_ref() {
            let _ = tx.send(SynthJob {
                bpm,
                speaker_id: self.state.speaker_id,
                notes,
            });
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
    fn default_state_is_zundamon_normal() {
        let s = VoicevoxState::default();
        assert_eq!(s.speaker_id, 6);
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
                key: 60,
                velocity: 100.0,
            },
        }];
        p.process(64, &events, &[], &[]).unwrap();
        assert!(p.out_l[..64].iter().all(|&v| v == 0.0));
        assert!(p.active_voice.is_none());
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
        if let Ok(mut guard) = p.synth_result.write() {
            *guard = Some(SynthResult {
                samples: Arc::new(samples),
                sample_rate: 48000,
                note_offsets: Arc::new(offsets),
            });
        }
        let events = vec![TimedNoteEvent {
            time: 0,
            event: crate::plugin_instance::NoteTransition::On {
                key: 60,
                velocity: 127.0,
            },
        }];
        p.process(64, &events, &[], &[]).unwrap();
        // out_l に値が乗っていることを確認 (= sample[0] = 0.0 はゼロだが
        // sample[1] 以降は非ゼロ)。
        assert!(p.out_l[1..64].iter().any(|&v| v != 0.0));
        // voice が cursor を進めていること。
        assert_eq!(p.active_voice.as_ref().unwrap().cursor, 64);
        // 次 process で残り 64 frame 流して voice 終了。
        p.process(64, &[], &[], &[]).unwrap();
        assert!(p.active_voice.is_none());
        p.deactivate();
    }
}
