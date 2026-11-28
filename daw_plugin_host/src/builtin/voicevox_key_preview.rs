//! 鍵盤プレビューの 1 音 — 書かれていない高さを VOICEVOX の声で鳴らす。
//!
//! 歌声の buffer ([`super::voicevox::SynthResult`]) は「合成済みのノート」しか持たないので、ピアノロールの
//! 鍵盤レーンの click・ノート移動の試聴・MIDI 入力のモニター (どれも engine の鍵盤プレビュー =
//! `note_id == NOTE_ID_NONE` の note-on) には鳴らす音が無い。ここが **その高さの 1 音** を別に合成して持つ。
//!
//! - 合成は押されたとき (`PluginCommand::PrepareVocalKeyPreview`) に 1 音ずつ ([`KeyPreviewWorker`]、非 RT)。
//!   初めての鍵だけ合成の時間 (実測 約 0.15 秒) 遅れ、結果はディスクキャッシュに残るので次からは読むだけ。
//!   先に合成しておく範囲は持たない (歌手ごとの音域を engine が返さないので、決め打ちになる)。
//! - 発音と消音は engine から届く note-on / note-off ([`KeyPreviewPlayer`]、audio thread)。合成が note-on より
//!   後に届いたら、届いたところから鳴らす (短い click で先に離されていても、[`MIN_SOUND_MS`] は鳴らす)。
//! - 両者が共有するのは [`KeyBank`] の `ArcSwapOption` だけ。差し替えは歌声の buffer と同じ quiesce 付き
//!   retire ([`super::voicevox_render::release_when_quiesced`]。audio thread 上で解放しないための契約と、
//!   待ちの上限を超えたときの扱いはそちら)。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use arc_swap::ArcSwapOption;

use common::model::Note;
use common::process_data::NOTE_ID_NONE;
use common::voicevox::{DEFAULT_SINGER_ID, REST_FRAMES, build_sing_query, frames_to_samples};
use common::voicevox_cache::{CacheKind, VoiceVoxDiskCache, key_for_key_preview};

use super::voicevox_render::release_when_quiesced;
use super::voicevox_synth::{
    SynthError, decode_wav_to_f32, fetch_sing_frame_query, frame_synthesis,
    synth_client_with_timeout,
};
use crate::plugin_instance::NoteTransition;

/// MIDI の鍵盤の数。
const KEY_COUNT: usize = 128;

/// 1 音の長さ (秒)。押し続けてもこの長さで終わる (歌声は伸ばし続けられない)。
const KEY_PREVIEW_SECS: f64 = 1.0;

/// 1 音の歌詞。子音を持たない「あ」にする — 子音は音の頭より前に置かれるので、頭から鳴らすと欠ける。
const KEY_PREVIEW_LYRIC: &str = "あ";

/// 合成 HTTP 1 本の timeout。遅れて届いたプレビューは鳴らす相手がもう居ないので、歌の合成より短く切る。
/// [`KeyPreviewWorker::stop`] の join が待つのも、進行中の HTTP 1 本ぶん (この時間) まで。
const KEY_PREVIEW_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// 鳴り始めのフェード (ms)。
const FADE_IN_MS: f64 = 5.0;
/// 鍵を離したときのフェード (ms)。
const RELEASE_MS: f64 = 40.0;
/// 鳴り始めたら最低これだけは鳴らす (ms)。短い click と、合成が離した後に届いた音が聞き取れる長さ。
const MIN_SOUND_MS: f64 = 200.0;
/// 離された鍵の合成を待つ上限 (ms)。これより遅れて届いた音は、押した操作と結び付かないので鳴らさない。
const LATE_SAMPLE_WAIT_MS: f64 = 2000.0;

/// 1 音ぶんの mono PCM (**音の頭から** — query の先頭の無音は落としてある)。
pub(super) struct KeySample {
    /// 身元 (process 内で一意、`0` にはならない)。鳴らしている途中で別のサンプルへ差し替わったかを
    /// audio thread が比べる (番地は、解放直後の確保で同じ値になり得るので使わない)。
    id: u64,
    samples: Vec<f32>,
    sample_rate: u32,
}

impl KeySample {
    fn new(samples: Vec<f32>, sample_rate: u32) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), samples, sample_rate }
    }
}

type KeySlots = [Option<Arc<KeySample>>; KEY_COUNT];

fn empty_slots() -> KeySlots {
    [const { None }; KEY_COUNT]
}

/// いまの歌手の、合成済みの鍵。worker が差し替え、audio thread が読む。
pub(super) struct KeyBank {
    keys: KeySlots,
}

pub(super) type SharedKeyBank = Arc<ArcSwapOption<KeyBank>>;

// ====================================================================
// 合成 (非 RT)
// ====================================================================

struct KeyRequest {
    key: u8,
    singer_id: u32,
}

/// bank の中身の正本 (bank は読み戻さない)。合成 thread が動いている間は thread が、止まっている間は
/// [`KeyPreviewWorker`] が持つ — 止めて起こし直しても、合成済みの鍵と歌手を引き継ぐ。
struct BankContents {
    /// `keys` を合成した歌手 (`0` = まだ何も合成していない)。
    singer_id: u32,
    keys: KeySlots,
}

/// 鍵盤プレビューの合成 thread の持ち主 (plugin-main half が持つ)。thread は最初の要求で起こす。
pub(super) struct KeyPreviewWorker {
    bank: SharedKeyBank,
    rt_epoch: Arc<AtomicU64>,
    /// thread へ「溜まった要求と残りの HTTP を捨てて抜けろ」と伝える ([`Self::stop`])。
    shutdown: Arc<AtomicBool>,
    tx: Option<mpsc::Sender<KeyRequest>>,
    thread: Option<JoinHandle<BankContents>>,
    /// 止まっている間の bank の中身 (次に起こす thread へ渡す)。
    parked: Option<BankContents>,
}

impl KeyPreviewWorker {
    pub(super) fn new(bank: SharedKeyBank, rt_epoch: Arc<AtomicU64>) -> Self {
        Self {
            bank,
            rt_epoch,
            shutdown: Arc::new(AtomicBool::new(false)),
            tx: None,
            thread: None,
            parked: None,
        }
    }

    /// `key` の 1 音を歌手 `singer_id` (`0` = 既定) で用意させる。既に持っていれば thread 側で何もしない。
    pub(super) fn request(&mut self, key: u8, singer_id: u32) {
        // 鍵 0 は「音程なし」(`NoteMetadata::pitch`) で、VOICEVOX に渡せない。
        if key == 0 || usize::from(key) >= KEY_COUNT {
            return;
        }
        let singer_id = if singer_id != 0 { singer_id } else { DEFAULT_SINGER_ID };
        if self.send(KeyRequest { key, singer_id }).is_err() {
            // thread が落ちていた。起こし直して、この要求を送り直す (この 1 回を押し損ねにしない)。
            self.stop();
            let _ = self.send(KeyRequest { key, singer_id });
        }
    }

    fn send(&mut self, req: KeyRequest) -> Result<(), mpsc::SendError<KeyRequest>> {
        if self.tx.is_none() {
            self.start();
        }
        // thread を起こせなかったとき (`start` が error を出している) は、送る相手が居ない。
        self.tx.as_ref().map_or(Ok(()), |tx| tx.send(req))
    }

    fn start(&mut self) {
        // 前の thread が残っていたら (`request_stop` だけで `stop` していない)、先に畳んで中身を受け取る。
        self.stop();
        let (tx, rx) = mpsc::channel::<KeyRequest>();
        let bank = Arc::clone(&self.bank);
        let rt_epoch = Arc::clone(&self.rt_epoch);
        let shutdown = Arc::clone(&self.shutdown);
        shutdown.store(false, Ordering::SeqCst);
        // 引き継ぐ中身が無い (初回 / 前の thread が落ちた) なら空から。bank に前の中身が残っていても、
        // 歌手 0 から始まるので最初の要求で空へ差し替わる。
        let contents =
            self.parked.take().unwrap_or(BankContents { singer_id: 0, keys: empty_slots() });
        match std::thread::Builder::new()
            .name("voicevox-builtin-key-preview".into())
            .spawn(move || run(&rx, &bank, &rt_epoch, &shutdown, contents))
        {
            Ok(handle) => {
                self.tx = Some(tx);
                self.thread = Some(handle);
            }
            Err(e) => {
                tracing::error!(error = ?e, "VoicevoxBuiltin: 鍵盤プレビューの thread を起こせない");
                // 引き継ぐはずだった中身は closure ごと失われた。bank も空にして揃える (残すと、歌手を
                // 変えた後も前の歌手の声で鳴る)。
                let prev = self.bank.swap(None);
                let epoch = self.rt_epoch.load(Ordering::Acquire);
                if let Some(prev) = prev {
                    release_when_quiesced(prev, epoch, &self.rt_epoch);
                }
            }
        }
    }

    /// thread に止まるよう伝える (待たない)。`stop` の前に呼んでおくと、他の thread の停止待ちと並行して進む。
    pub(super) fn request_stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.tx = None;
    }

    /// thread を止めて待つ。待つのは進行中の HTTP 1 本まで ([`KEY_PREVIEW_HTTP_TIMEOUT`]) — 溜まった要求と
    /// 残りの HTTP は捨てる。合成済みの鍵は次に起こす thread が引き継ぐ。
    pub(super) fn stop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.thread.take() {
            // panic で落ちていたら中身は失われる (次は空から)。
            self.parked = handle.join().ok();
        }
    }
}

impl Drop for KeyPreviewWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    rx: &mpsc::Receiver<KeyRequest>,
    bank: &ArcSwapOption<KeyBank>,
    rt_epoch: &AtomicU64,
    shutdown: &AtomicBool,
    mut contents: BankContents,
) -> BankContents {
    let mut client: Option<reqwest::blocking::Client> = None;
    while let Ok(mut req) = rx.recv() {
        // 溜まっていたら最後の 1 件だけ合成する (鍵盤をなぞって通り過ぎた鍵は、もう鳴らす相手が居ない)。
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        // sender が落ちても、溜まっていた要求は `recv` が返す。止められた後のぶんは合成しない。
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if req.singer_id != contents.singer_id {
            // 歌手が変わった。合成を待つ間に前の歌手の声で鳴らないよう、先に空へ差し替える。
            contents.singer_id = req.singer_id;
            contents.keys = empty_slots();
            publish(bank, rt_epoch, &contents.keys);
        }
        let slot = usize::from(req.key);
        if contents.keys[slot].is_some() {
            continue;
        }
        match load_or_synth(&mut client, req.key, contents.singer_id, shutdown) {
            Ok(Some(sample)) => {
                contents.keys[slot] = Some(Arc::new(sample));
                publish(bank, rt_epoch, &contents.keys);
            }
            // 合成の途中で止められた。
            Ok(None) => break,
            // 失敗は憶えない (engine が起動中なら、次に押したときに取り直せる)。
            Err(e) => tracing::warn!(
                key = req.key,
                singer_id = contents.singer_id,
                error = %e,
                "VoicevoxBuiltin: 鍵盤プレビューを合成できない"
            ),
        }
    }
    contents
}

/// bank を差し替え、旧 bank は audio thread が手放してからこの thread で解放する。
fn publish(bank: &ArcSwapOption<KeyBank>, rt_epoch: &AtomicU64, keys: &KeySlots) {
    let prev = bank.swap(Some(Arc::new(KeyBank { keys: keys.clone() })));
    let e = rt_epoch.load(Ordering::Acquire);
    if let Some(prev) = prev {
        release_when_quiesced(prev, e, rt_epoch);
    }
}

/// 1 音だけの楽譜 (`/sing_frame_audio_query` の body)。歌の query と同じ builder を通す。
fn key_preview_score(key: u8) -> String {
    let note = Note {
        id: 0,
        start_beat: 0.0,
        // bpm 60 で組むので、拍の長さ = 秒。
        duration_beats: KEY_PREVIEW_SECS,
        pitch: key,
        velocity: 100,
        lyric: Some(KEY_PREVIEW_LYRIC.to_string()),
        muted: false,
    };
    build_sing_query(&[note], 60.0).json
}

/// ディスクキャッシュから読む。無ければ合成して置く。`Ok(None)` = 途中で止められた (`shutdown`)。
fn load_or_synth(
    client: &mut Option<reqwest::blocking::Client>,
    key: u8,
    singer_id: u32,
    shutdown: &AtomicBool,
) -> Result<Option<KeySample>, SynthError> {
    let score = key_preview_score(key);
    let cache = VoiceVoxDiskCache::production();
    let cache_key = key_for_key_preview(&score, singer_id);
    // hit は「鳴らせる音になる」と確かめて初めて採用する (壊れた / 空のエントリを信じると、その鍵は
    // 永久に鳴らない)。
    if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key, CacheKind::Wav)) {
        match decode_wav_to_f32(&hit).map_err(|e| format!("{e:#}")).and_then(trim_leading_rest) {
            Ok(sample) => return Ok(Some(sample)),
            Err(e) => {
                tracing::warn!(error = %e, "voicevox cache: 壊れた鍵盤プレビューの WAV を無視して再合成");
            }
        }
    }
    let client = match client {
        Some(c) => c,
        None => client.insert(synth_client_with_timeout(KEY_PREVIEW_HTTP_TIMEOUT)?),
    };
    let frame_query = fetch_sing_frame_query(client, &score)?;
    if shutdown.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let wav = frame_synthesis(client, &frame_query, singer_id)?;
    let sample = decode_wav_to_f32(&wav)
        .map_err(|e| format!("{e:#}"))
        .and_then(trim_leading_rest)
        .map_err(SynthError::Rejected)?;
    if let Some(c) = cache.as_ref() {
        c.put(cache_key, CacheKind::Wav, &wav);
    }
    Ok(Some(sample))
}

/// query が先頭に置く無音 ([`REST_FRAMES`]) を落とし、**音の頭**を index 0 にする
/// (押してから鳴るまでに無音を挟まない)。落とした後に音が残らない WAV は採らない。
fn trim_leading_rest((mut samples, sample_rate): (Vec<f32>, u32)) -> Result<KeySample, String> {
    let lead = frames_to_samples(f64::from(REST_FRAMES), sample_rate).round() as usize;
    samples.drain(..lead.min(samples.len()));
    if samples.is_empty() {
        return Err("先頭の無音より短い WAV".to_string());
    }
    Ok(KeySample::new(samples, sample_rate))
}

// ====================================================================
// 発音 (audio thread)
// ====================================================================

struct KeyVoice {
    key: u8,
    /// 次に読む分数位置 (サンプルの sample 単位)。
    cursor: f64,
    /// フェードの包絡 (0 から始めて 1 へ、離したら 0 へ)。
    gain: f32,
    /// 鍵が離された。
    released: bool,
    /// 鳴らしているサンプルの身元 ([`KeySample::id`]。`0` = まだ鳴り始めていない)。
    sample_id: u64,
    /// 鳴り始めてから出した host frame 数 ([`MIN_SOUND_MS`] の判定)。
    sounded: u64,
    /// 離されてから、鳴り始めないまま待った host frame 数 ([`LATE_SAMPLE_WAIT_MS`] の判定)。
    waited: u64,
}

impl KeyVoice {
    fn new(key: u8) -> Self {
        Self { key, cursor: 0.0, gain: 0.0, released: false, sample_id: 0, sounded: 0, waited: 0 }
    }

    /// `out_l` / `out_r` へ加算する。`false` = 鳴り終えた (捨ててよい)。
    fn render(
        &mut self,
        bank: Option<&KeyBank>,
        host_sr: f64,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> bool {
        let sample = bank
            .and_then(|b| b.keys.get(usize::from(self.key)))
            .and_then(Option::as_deref)
            .filter(|s| s.sample_rate > 0);
        let sample_id = sample.map_or(0, |s| s.id);
        if self.sample_id != 0 && self.sample_id != sample_id {
            // 鳴っている途中でサンプルが消えた / 差し替わった (歌手が変わった)。前の声の続きとしては
            // 鳴らさない: 離された音はそこで終え、押されている音は新しい声を頭から。
            if self.released {
                return false;
            }
            self.cursor = 0.0;
            self.gain = 0.0;
            self.sounded = 0;
        }
        self.sample_id = sample_id;
        let Some(sample) = sample else {
            // まだ合成が届いていない。離された鍵は、届くのを上限まで待つ。
            if self.released {
                self.waited = self.waited.saturating_add(out_l.len() as u64);
                return (self.waited as f64) < LATE_SAMPLE_WAIT_MS * host_sr / 1000.0;
            }
            return true;
        };
        let ratio = f64::from(sample.sample_rate) / host_sr;
        let fade_in = (1000.0 / (FADE_IN_MS * host_sr)).min(1.0) as f32;
        let release = (1000.0 / (RELEASE_MS * host_sr)).min(1.0) as f32;
        let min_sound = (MIN_SOUND_MS * host_sr / 1000.0) as u64;
        let src = sample.samples.as_slice();
        let mut produced = 0usize;
        let mut finished = false;
        for (k, (l, r)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
            let pos = self.cursor + k as f64 * ratio;
            let i0 = pos.floor() as usize;
            if i0 >= src.len() {
                finished = true;
                break;
            }
            if self.released && self.sounded.saturating_add(k as u64) >= min_sound {
                self.gain -= release;
                if self.gain <= 0.0 {
                    finished = true;
                    break;
                }
            } else {
                self.gain = (self.gain + fade_in).min(1.0);
            }
            let frac = (pos - i0 as f64) as f32;
            let s1 = src.get(i0 + 1).copied().unwrap_or(src[i0]);
            let s = (src[i0] * (1.0 - frac) + s1 * frac) * self.gain;
            *l += s;
            *r += s;
            produced = k + 1;
        }
        self.cursor += produced as f64 * ratio;
        self.sounded = self.sounded.saturating_add(produced as u64);
        !finished
    }
}

/// 鍵盤プレビューの発音。歌声と同じく単音 (後から押した鍵が勝つ)。確保・ロック・I/O をしない。
#[derive(Default)]
pub(super) struct KeyPreviewPlayer {
    voice: Option<KeyVoice>,
    /// 次の鍵に押し出された音の、消え際 (鍵盤をなぞったときに段差で切らない)。
    tail: Option<KeyVoice>,
}

impl KeyPreviewPlayer {
    /// engine の鍵盤プレビュー (`note_id == NOTE_ID_NONE`) だけを受ける。曲のノート (安定 id 付き) は歌声の
    /// buffer が鳴らすので、ここで拾うと再生中に二重に鳴る。
    pub(super) fn on_event(&mut self, event: &NoteTransition) {
        match *event {
            NoteTransition::On { note_id: NOTE_ID_NONE, key, .. } => {
                // 鳴っている音は離された扱いでフェードさせる (最低の長さは待たない)。鳴り始めていない音と、
                // 既に消えかけている tail は捨てる。
                if let Some(mut prev) = self.voice.take().filter(|v| v.sample_id != 0) {
                    prev.released = true;
                    prev.sounded = u64::MAX;
                    self.tail = Some(prev);
                }
                self.voice = Some(KeyVoice::new(key));
            }
            NoteTransition::Off { note_id: NOTE_ID_NONE, key } => {
                if let Some(v) = self.voice.as_mut().filter(|v| v.key == key) {
                    v.released = true;
                }
            }
            _ => {}
        }
    }

    pub(super) fn reset(&mut self) {
        self.voice = None;
        self.tail = None;
    }

    /// 鳴っている音を `out_l` / `out_r` (同じ長さ) へ加算する。
    pub(super) fn render(
        &mut self,
        bank: Option<&KeyBank>,
        host_sr: f64,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        if host_sr <= 0.0 {
            self.reset();
            return;
        }
        for slot in [&mut self.voice, &mut self.tail] {
            if let Some(v) = slot.as_mut()
                && !v.render(bank, host_sr, out_l, out_r)
            {
                *slot = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank_with(key: u8, samples: Vec<f32>) -> KeyBank {
        let mut keys = empty_slots();
        keys[usize::from(key)] = Some(Arc::new(KeySample::new(samples, 48_000)));
        KeyBank { keys }
    }

    fn on(key: u8, note_id: u32) -> NoteTransition {
        NoteTransition::On { note_id, key, velocity: 1.0 }
    }

    fn render(p: &mut KeyPreviewPlayer, bank: Option<&KeyBank>, frames: usize) -> Vec<f32> {
        let mut l = vec![0.0f32; frames];
        let mut r = vec![0.0f32; frames];
        p.render(bank, 48_000.0, &mut l, &mut r);
        l
    }

    /// 合成が note-on より後に届いたら、そこから鳴る。先に離されていても (短い click) 最低の長さは鳴って止まる。
    #[test]
    fn late_sample_still_sounds() {
        let bank = bank_with(60, vec![1.0; 48_000]);

        let mut held = KeyPreviewPlayer::default();
        held.on_event(&on(60, NOTE_ID_NONE));
        assert!(render(&mut held, None, 64).iter().all(|s| *s == 0.0));
        assert!(render(&mut held, Some(&bank), 64).iter().any(|s| *s > 0.0));

        let mut tapped = KeyPreviewPlayer::default();
        tapped.on_event(&on(60, NOTE_ID_NONE));
        tapped.on_event(&NoteTransition::Off { note_id: NOTE_ID_NONE, key: 60 });
        render(&mut tapped, None, 64);
        // MIN_SOUND_MS = 200 ms = 9600 sample、RELEASE_MS = 40 ms = 1920 sample @ 48 kHz。
        let out = render(&mut tapped, Some(&bank), 24_000);
        assert!(out[9_000] > 0.9);
        assert!(out[12_000..].iter().all(|s| *s == 0.0));
        assert!(tapped.voice.is_none());
    }

    /// 離された鍵の合成が上限より遅れて届いたら鳴らさない (押した操作と結び付かない音を出さない)。
    #[test]
    fn too_late_sample_is_dropped() {
        let bank = bank_with(60, vec![1.0; 48_000]);
        let mut p = KeyPreviewPlayer::default();
        p.on_event(&on(60, NOTE_ID_NONE));
        p.on_event(&NoteTransition::Off { note_id: NOTE_ID_NONE, key: 60 });
        // LATE_SAMPLE_WAIT_MS = 2000 ms = 96000 sample @ 48 kHz。
        render(&mut p, None, 96_000);
        assert!(render(&mut p, Some(&bank), 64).iter().all(|s| *s == 0.0));
    }

    /// 曲のノート (安定 id 付き) では鳴らさない — 歌声の buffer と二重になる。
    #[test]
    fn song_notes_do_not_trigger() {
        let bank = bank_with(60, vec![1.0; 48_000]);
        let mut p = KeyPreviewPlayer::default();
        p.on_event(&on(60, 7));
        assert!(render(&mut p, Some(&bank), 64).iter().all(|s| *s == 0.0));
    }

    /// 離すとフェードして止まる (サンプルの終わりまで鳴り続けない)。
    #[test]
    fn release_fades_out() {
        let bank = bank_with(60, vec![1.0; 48_000]);
        let mut p = KeyPreviewPlayer::default();
        p.on_event(&on(60, NOTE_ID_NONE));
        // MIN_SOUND_MS (9600 sample) を過ぎてから離す。
        render(&mut p, Some(&bank), 12_000);
        p.on_event(&NoteTransition::Off { note_id: NOTE_ID_NONE, key: 60 });
        // RELEASE_MS = 40 ms = 1920 sample @ 48 kHz。
        let tail = render(&mut p, Some(&bank), 4096);
        assert!(tail[0] > 0.9);
        assert!(tail[2048..].iter().all(|s| *s == 0.0));
        assert!(p.voice.is_none());
    }
}
