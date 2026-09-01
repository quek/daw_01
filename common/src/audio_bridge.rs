use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use anyhow::Result;

use crate::mod_plane::ModPlane;
use crate::shmem::NamedShmem;

/// (A1 r.md #8) フォールバック既定サンプルレート。 通常はランタイムで
/// `AudioSession.sample_rate` = デバイス実レート (daw_audio が Hello で報告) が
/// SSoT で、 この const はデバイス query 失敗時の保険値としてのみ使う。
pub const DEFAULT_SAMPLE_RATE: u32 = 48000;
/// 1 バッファの最大 frame 数。 SSoT は `process_data::MAX_FRAMES` (プラグイン
/// process shmem のバッファ次元) — audio bridge 側の u32 view として re-export。
/// 二重定義で乖離すると RT パスの `assert!(frames <= MAX_FRAMES)` が panic する。
pub const MAX_FRAMES: u32 = crate::process_data::MAX_FRAMES as u32;
pub const CHANNELS: u32 = 2;
/// Hard cap for the per-track peak meter ring in shmem. Tracks beyond this
/// index are still processed, they just don't publish a meter. 32 matches
/// Renoise's default Mixer column count.
pub const MAX_TRACKS: usize = 32;
/// docs/plan_modulation.md §4.2: hard cap for the modulation-scalar ring in
/// shmem. `Song::mod_sources` beyond this index don't publish a scalar (their
/// `EnvelopeFollow` node isn't emitted). スロットの**並び**は engine の compile 順
/// だが、読み手は並びではなく [`AudioBridge::mod_slot_ids`] の id で引く
/// (`docs/plan_rmd_88_89_cross_modulation.md` §4-2、アーキ不変条件 1)。
pub const MAX_MOD_SOURCES: usize = 64;

/// [`AudioBridgeHandle::read_mod_plane`] が seqlock の読み直しを諦めるまでの回数。
/// 書き手は 1 buffer に 1 回しか面を触らないので、30Hz の読み手が 8 回連続で
/// 書き込み中に当たることは実質ない (当たったら「今回は更新なし」に倒す)。
const MOD_PLANE_READ_RETRIES: usize = 8;

/// r.md #87 (クリップランチャー): 走行状態を publish できる行数の上限。
/// 行 = トラック行 + オートメーションレーン行なので、`MAX_TRACKS` (= 32) では足りない。
///
/// **engine 側の走行状態の器 (`launcher::MAX_ROWS`) も同じ値**なので、溢れると
/// 表示が出ないだけでは済まず、その行はランチャーを持てない (= セルを撃っても
/// アレンジのクリップが鳴り続ける)。32 トラック × 15 レーンぶんを確保して、
/// 現実的な曲で溢れないようにする (1 行 48 バイト = 24 KB)。
pub const MAX_LAUNCHER_ROWS: usize = 512;

/// [`LauncherRowState::state`] の値。**engine と GUI が共有する唯一の定義**。
pub const LAUNCHER_STATE_ARRANGER: u32 = 0;
/// ランチャーが握っていてセルが鳴っている。
pub const LAUNCHER_STATE_PLAYING: u32 = 1;
/// ランチャーが握っているが無音 (Stop Clips)。
pub const LAUNCHER_STATE_STOPPED: u32 = 2;

/// [`LauncherRowState::queued_clip_id`] が「停止の予約」を表す値。
/// `clip.id` は 1 から採番されるので実 id と衝突しない。
pub const LAUNCHER_QUEUED_STOP: u32 = u32::MAX;
/// [`LauncherRowState::queued_clip_id`] が「アレンジへ返す予約」を表す値。
pub const LAUNCHER_QUEUED_ARRANGER: u32 = u32::MAX - 1;

/// r.md #87: 1 行ぶんの走行状態 (表示専用)。
///
/// **`Song` には入れない** — フォローアクションで移った先を保存すると
/// 「何秒鳴らしてから書き出したか」で出力が変わり、Q9 の再現性が壊れる
/// (`docs/plan_rmd_87_clip_launcher.md` §1.4)。ここは plugin latency と同じ
/// 「実行時の観測値」の置き場。
#[repr(C)]
pub struct LauncherRowState {
    /// 行の安定 id を 1 ワードに詰めたもの: `(track_id as u64) << 32 | lane_id`。
    /// `lane_id == 0` がトラック行。**`0` = 空きスロット** (`track_id` は 1 から
    /// 採番されるので実在の行と衝突しない)。GUI は index ではなくこの値で引く
    /// (アーキ不変条件 1 — 行の並びは折りたたみ / 追加で動く)。
    pub row_key: AtomicU64,
    /// `LAUNCHER_STATE_*` のいずれか。
    pub state: AtomicU32,
    /// いま鳴っているセルの `clip.id` (0 = なし)。
    pub playing_clip_id: AtomicU32,
    /// 量子化境界待ちのセルの `clip.id` (0 = なし / `LAUNCHER_QUEUED_STOP` /
    /// `LAUNCHER_QUEUED_ARRANGER`)。
    pub queued_clip_id: AtomicU32,
    /// 予約が **発火する song 拍** (`f64::to_bits`)。`queued_clip_id == 0` の
    /// ときは意味を持たない (0)。
    ///
    /// **GUI のカウントダウンはこれを引き算するだけ**にするためにある。engine は
    /// 量子化境界・シーンのフォローアクション・legato・repeat を全部畳んだ結果の
    /// 「実際に鳴る拍」を既に持っているので、GUI が
    /// [`LaunchQuantize`](crate::model::LaunchQuantize) から境界を解き直すと
    /// **同じ答えを 2 本の式で出すことになり、フォローアクション経由の予約
    /// (グローバル量子化を迂回する) で必ず食い違う**。
    pub queued_at_beat_bits: AtomicU64,
    /// いま鳴っているセルの中の進捗 `0..1` (`f32::to_bits`)。停止中は 0。
    pub progress_bits: AtomicU32,
    /// いま鳴っているセルを **撃った song 拍** (`f64::to_bits`)。停止中は 0。
    ///
    /// 進捗 (`progress_bits`) は 30Hz でしか届かないので、映像側がこれだけで
    /// 位相を出すと 1/30 秒刻みでカクつく。撃った拍が分かれば、映像は自分の
    /// フレーム時刻から `daw_gui::launcher_time::cell_phase` で **音と同じ式**を
    /// 使って滑らかに解ける (計画書 §3.6)。
    pub launch_beat_bits: AtomicU64,
}

/// Shared memory telemetry plane: daw_audio (writer) → daw_gui (30Hz
/// polling reader)。
///
/// `playhead_samples` is published by **daw_audio** at the end of every
/// buffer so daw_gui can poll it (once per UI tick) for playhead-row
/// highlighting.
///
/// マスター出力のメーターはここに**居ない** (r.md #50)。マスターのピーク /
/// VU / ラウドネス / スペクトラム等はすべて daw_gui 側の `MasterAnalyzer` が
/// `scope_bridge` のサンプルリングから導くので、値を shmem に複製しない
/// (SSoT)。ここに残るのは per-track の peak — こちらは mixer strip 用に
/// 「track ごとの post-fader ピーク」だけが要るスカラー面。
///
/// All fields are lock-free Acquire/Release atomics — readers tolerate any
/// value they happen to observe.
///
/// v29 (`docs/plan_arch_refactor.md` §2): 旧 `frames_requested` / `samples`
/// 面 (M0 時代の request/ready セマフォ往復データプレーン) は writer /
/// reader とも存在しない死んだ protocol だったため削除。音声データは
/// per-plugin の `ProcessData` shmem + `WorkerBridge` dispatch が運ぶ。
#[repr(C)]
pub struct AudioBridge {
    pub playhead_samples: AtomicU64,
    /// Per-track post-fader peaks, `[track][0=L, 1=R]`, as `f32::to_bits`.
    /// Written by **daw_audio** after summing each track into the master
    /// bus (`engine.rs` / `main.rs` の `set_track_peak`); read by daw_gui on
    /// its UI tick。(v29: 旧 daw_plugin_host request/ready data plane 撤去に伴い
    /// writer は daw_audio に一本化 — module doc 参照。)
    pub track_peaks: [[AtomicU32; 2]; MAX_TRACKS],
    /// docs/plan_modulation.md §4.2: per-`ModSource` modulator scalar
    /// (`f32::to_bits`), block-rate. Written by the audio engine every buffer,
    /// polled by the GUI at ~30Hz alongside `track_peaks` and applied to
    /// modulated params.
    ///
    /// **slot の意味は [`AudioBridge::mod_slot_ids`] が持つ。** 位置で引かないこと
    /// (`docs/plan_rmd_88_89_cross_modulation.md` §4-2)。
    pub mod_scalars: [AtomicU32; MAX_MOD_SOURCES],
    /// r.md #89: `mod_scalars[i]` が**どのソースの値か** (`ModSource::id`)。
    /// `0` = 空きスロット。
    ///
    /// これが無かった頃は「engine が書いた slot 番号」= 「GUI が持つ
    /// `Song::mod_sources` の位置」という暗黙の前提で読んでいた。ソースを 1 つ
    /// 消して LoadSong が届くまでの間 (数フレーム) は前提が崩れ、**変調が別の
    /// ソースの値で動く**。id を値と同じ面に載せてその窓を閉じる (不変条件 1)。
    pub mod_slot_ids: [AtomicU32; MAX_MOD_SOURCES],
    /// r.md #89: `mod_slot_ids` + `mod_scalars` を**組で**読むための seqlock 世代。
    ///
    /// 奇数 = 書き込み中。読み手は「世代を読む → 面を読む → 世代を読み直して
    /// 一致を確認」で、id 表と値面が別 buffer のものになった組み合わせを弾く。
    /// 面は 2 配列に分かれているので、単発の atomic では「新しい id と古い値」の
    /// 組を防げない。
    pub mod_plane_generation: AtomicU64,
    /// Phase 7 B4 Step C (2026-05-13): count-in 残り samples mirror (audio
    /// thread が `process_buffer` で書く、 GUI が on_tick で poll)。
    /// 0 = count-in 中ではない / 完了済。 `StartRecording` 受信時に audio
    /// thread が値を立てる。 **これ単体で「count-in が終わったか」を判定しては
    /// いけない** — 0 は「まだ始まっていない」も意味するので、録音実体の開始判定は
    /// [`AudioBridge::recording_live`] を見る (r.md #51)。
    pub preroll_remaining_samples: AtomicU64,
    /// r.md #51: engine が今 transport を回しているか (0/1)。
    ///
    /// **「再生中か」の唯一の所有者は engine** で、GUI はこれを観測して
    /// `transport.is_playing` に写すだけ。GUI 側で「Play を送った記憶」を持つと、
    /// engine が自分で止まったとき (曲末 auto-stop / 書き出し) に食い違う。
    pub playing: AtomicU32,
    /// r.md #51: 今この瞬間 MIDI ノートを記録してよいか (0/1)。
    ///
    /// `録音要求あり && 再生中 && count-in 完了` を engine が判定して publish する。
    /// GUI が preroll ミラーの 0 を見て自前で導出すると、`StartRecording` 送信直後に
    /// 届いた stale な Tick (まだ preroll が立つ前の 0) で count-in を丸ごと飛ばす。
    pub recording_live: AtomicU32,
    /// r.md #87 (クリップランチャー): 行ごとの走行状態 (鳴っているセル / 予約 / 進捗)。
    /// 書き手は audio thread (毎 buffer)、読み手は GUI の poller。
    /// 使っていないスロットは `row_key == 0`。
    pub launcher_rows: [LauncherRowState; MAX_LAUNCHER_ROWS],
}

impl AudioBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// r.md #87: [`AudioBridgeHandle::launcher_row`] が返す 1 行ぶんの値
/// ([`LauncherRowState`] の atomic を全部読んだ結果の組)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LauncherRowSnapshot {
    /// `LAUNCHER_STATE_*`。
    pub state: u32,
    pub playing_clip_id: u32,
    pub queued_clip_id: u32,
    /// 予約が発火する song 拍 (`queued_clip_id == 0` のときは `0.0`)。
    pub queued_at_beat: f64,
    /// セル内の進捗 `0..1`。
    pub progress: f32,
    /// 鳴っているセルを撃った song 拍 (停止中は `0.0`)。
    pub launch_beat: f64,
}

/// Owning handle to the audio shared memory region.
pub struct AudioBridgeHandle {
    shmem: NamedShmem,
}

impl AudioBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, AudioBridge::SIZE)?;
        // Zero-initialize so the AtomicU32 starts at 0 and samples are silent.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, AudioBridge::SIZE) };
        let handle = Self { shmem };
        // Publish the "not playing" sentinel before any reader polls, so the
        // GUI highlight is off until plugin_host announces a real playhead.
        handle.set_playhead_samples(u64::MAX);
        Ok(handle)
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, AudioBridge::SIZE)?;
        Ok(Self { shmem })
    }

    fn ptr(&self) -> *mut AudioBridge {
        self.shmem.as_ptr() as *mut AudioBridge
    }

    pub fn bridge(&self) -> &AudioBridge {
        unsafe { &*self.ptr() }
    }

    pub fn set_playhead_samples(&self, n: u64) {
        self.bridge().playhead_samples.store(n, Ordering::Release);
    }

    pub fn playhead_samples(&self) -> u64 {
        self.bridge().playhead_samples.load(Ordering::Acquire)
    }

    /// Publishes one track's post-fader peak pair. Out-of-range track
    /// indices (beyond `MAX_TRACKS`) are silently dropped — the track is
    /// still mixed, it just doesn't get a meter.
    pub fn set_track_peak(&self, track: usize, l: f32, r: f32) {
        let Some(slot) = self.bridge().track_peaks.get(track) else {
            return;
        };
        slot[0].store(l.to_bits(), Ordering::Release);
        slot[1].store(r.to_bits(), Ordering::Release);
    }

    pub fn track_peak(&self, track: usize) -> (f32, f32) {
        let Some(slot) = self.bridge().track_peaks.get(track) else {
            return (0.0, 0.0);
        };
        let l = f32::from_bits(slot[0].load(Ordering::Acquire));
        let r = f32::from_bits(slot[1].load(Ordering::Acquire));
        (l, r)
    }

    /// Fills `out` with `(L, R)` peaks for tracks 0..`out.len()`.
    /// Out-of-range tracks are reported as `(0.0, 0.0)`.
    /// Phase 7 B4 Step C: count-in 残り samples を audio thread が更新。
    /// `StartRecording` 受信時に audio thread が preroll を立て、
    /// `process_buffer` が preroll > 0 ループ内で毎 buffer 更新する。
    /// 0 到達で通常再生に戻る。
    pub fn set_preroll_remaining(&self, n: u64) {
        self.bridge()
            .preroll_remaining_samples
            .store(n, Ordering::Release);
    }

    pub fn preroll_remaining(&self) -> u64 {
        self.bridge()
            .preroll_remaining_samples
            .load(Ordering::Acquire)
    }

    /// r.md #51: engine の transport 走行状態を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。読み手は GUI の playhead poller。
    pub fn set_playing(&self, playing: bool) {
        self.bridge()
            .playing
            .store(u32::from(playing), Ordering::Release);
    }

    pub fn playing(&self) -> bool {
        self.bridge().playing.load(Ordering::Acquire) != 0
    }

    /// r.md #51: 「今ノートを記録してよいか」を publish する (audio thread が
    /// 毎 buffer 呼ぶ)。count-in 明けの立ち上がりもここが唯一の合図。
    pub fn set_recording_live(&self, live: bool) {
        self.bridge()
            .recording_live
            .store(u32::from(live), Ordering::Release);
    }

    pub fn recording_live(&self) -> bool {
        self.bridge().recording_live.load(Ordering::Acquire) != 0
    }

    pub fn track_peaks(&self, out: &mut Vec<(f32, f32)>) {
        out.clear();
        for i in 0..MAX_TRACKS {
            let slot = &self.bridge().track_peaks[i];
            let l = f32::from_bits(slot[0].load(Ordering::Acquire));
            let r = f32::from_bits(slot[1].load(Ordering::Acquire));
            out.push((l, r));
        }
    }

    /// r.md #89: 変調ソースの値面 (id 表 + 値) を**丸ごと** publish する。
    /// audio thread が毎 buffer 1 回だけ呼ぶ (seqlock の書き手)。
    ///
    /// `MAX_MOD_SOURCES` を超えるぶんは捨てる。溢れなかった残りの slot は
    /// `id = 0` (空き) で潰す — 消えたソースの値が面に残り続けると、id を
    /// 使い回した別のソースがその値を拾う。
    ///
    /// RT 安全: atomic store のみ (確保・ロック・I/O なし)。
    pub fn publish_mod_plane(&self, plane: &ModPlane) {
        let b = self.bridge();
        let g = b.mod_plane_generation.load(Ordering::Relaxed);
        // 奇数 = 書き込み中。以降のデータ書き込みがこの store より前へ回らない
        // ように Release fence で仕切る (store 自体は Relaxed で十分)。
        b.mod_plane_generation
            .store(g.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let ids = plane.ids();
        let values = plane.values();
        for i in 0..MAX_MOD_SOURCES {
            let id = ids.get(i).copied().unwrap_or(0);
            let v = values.get(i).copied().unwrap_or(0.0);
            b.mod_slot_ids[i].store(id, Ordering::Relaxed);
            b.mod_scalars[i].store(v.to_bits(), Ordering::Relaxed);
        }
        b.mod_plane_generation
            .store(g.wrapping_add(2), Ordering::Release);
    }

    /// r.md #89: 値面を seqlock で読む (GUI の 30Hz poller)。
    ///
    /// 書き込み中に当たったら読み直し、[`MOD_PLANE_READ_RETRIES`] 回とも
    /// 破れたら `false` を返して `out` は触らない (= 前回値を保つ)。
    /// `out` は使い回すので確保は起きない。
    pub fn read_mod_plane(&self, out: &mut ModPlane) -> bool {
        let b = self.bridge();
        for _ in 0..MOD_PLANE_READ_RETRIES {
            let g0 = b.mod_plane_generation.load(Ordering::Acquire);
            if g0 & 1 != 0 {
                continue; // 書き込み中
            }
            out.clear();
            for i in 0..MAX_MOD_SOURCES {
                let id = b.mod_slot_ids[i].load(Ordering::Relaxed);
                let v = f32::from_bits(b.mod_scalars[i].load(Ordering::Relaxed));
                if id != 0 {
                    out.push(id, v);
                }
            }
            fence(Ordering::Acquire);
            if b.mod_plane_generation.load(Ordering::Relaxed) == g0 {
                return true;
            }
        }
        false
    }

    /// r.md #87: 1 行ぶんの走行状態を publish する。`slot` は engine が毎 buffer
    /// 詰め直す **その buffer 限りの並び**で、意味を持つのは `row_key` の方
    /// (GUI は key で引く)。範囲外は黙って捨てる (`set_mod_scalar` と同じ規約)。
    ///
    /// RT 安全: atomic store のみ (確保・ロック・I/O なし)。
    #[allow(clippy::too_many_arguments)]
    pub fn set_launcher_row(
        &self,
        slot: usize,
        row_key: u64,
        state: u32,
        playing_clip_id: u32,
        queued_clip_id: u32,
        queued_at_beat: f64,
        progress: f32,
        launch_beat: f64,
    ) {
        let Some(cell) = self.bridge().launcher_rows.get(slot) else {
            return;
        };
        cell.state.store(state, Ordering::Release);
        cell.playing_clip_id.store(playing_clip_id, Ordering::Release);
        cell.queued_clip_id.store(queued_clip_id, Ordering::Release);
        cell.queued_at_beat_bits.store(queued_at_beat.to_bits(), Ordering::Release);
        cell.progress_bits.store(progress.to_bits(), Ordering::Release);
        cell.launch_beat_bits.store(launch_beat.to_bits(), Ordering::Release);
        // `row_key` は **最後に**書く — GUI は「使用中か」を見てから残りを読むので、
        // 先に書くと 1 tick だけ古い state と新しい key の組を見せてしまう。
        //
        // 保存するのは **`row_key + 1`**。0 を「空きスロット」の印に使うので、
        // `row_key` そのものを入れると `RowKey::packed() == 0` の行
        // (= `track_id` も `lane_id` も 0) が空きと見分けられず、そこで読み取りが
        // 打ち切られて**以降の行が丸ごと GUI に届かない**。
        cell.row_key.store(row_key.saturating_add(1), Ordering::Release);
    }

    /// r.md #87: `slot` 以降を「空き」にする (engine が publish した行数より後ろ)。
    pub fn clear_launcher_rows_from(&self, slot: usize) {
        for cell in self.bridge().launcher_rows.iter().skip(slot) {
            if cell.row_key.load(Ordering::Acquire) == 0 {
                break;
            }
            cell.row_key.store(0, Ordering::Release);
        }
    }

    /// r.md #87: 行の走行状態を安定 id で引く
    /// (`lane_id == 0` がトラック行)。見つからなければ `None`。
    #[must_use]
    pub fn launcher_row(&self, track_id: u32, lane_id: u32) -> Option<LauncherRowSnapshot> {
        // 格納値は `row_key + 1` (0 = 空きスロット)。`row_key` が 0 の行
        // (track_id / lane_id とも 0) も**正しく引ける**ようにここで +1 する。
        let want = ((u64::from(track_id) << 32) | u64::from(lane_id)).saturating_add(1);
        for cell in &self.bridge().launcher_rows {
            let key = cell.row_key.load(Ordering::Acquire);
            if key == 0 {
                break;
            }
            if key == want {
                return Some(LauncherRowSnapshot {
                    state: cell.state.load(Ordering::Acquire),
                    playing_clip_id: cell.playing_clip_id.load(Ordering::Acquire),
                    queued_clip_id: cell.queued_clip_id.load(Ordering::Acquire),
                    queued_at_beat: f64::from_bits(
                        cell.queued_at_beat_bits.load(Ordering::Acquire),
                    ),
                    progress: f32::from_bits(cell.progress_bits.load(Ordering::Acquire)),
                    launch_beat: f64::from_bits(
                        cell.launch_beat_bits.load(Ordering::Acquire),
                    ),
                });
            }
        }
        None
    }

    /// r.md #87: publish 済みの行を **まとめて** 読み出す (GUI の 30Hz poller 用)。
    /// `out` は `(row_key, snapshot)` で、`row_key` は `(track_id << 32) | lane_id`
    /// (`lane_id == 0` がトラック行)。呼び側の `Vec` を使い回すので確保は起きない。
    ///
    /// 1 行ずつ引く [`Self::launcher_row`] は毎回配列を線形走査するので、
    /// 行数ぶん呼ぶと O(n²) になる。表示側は全行ぶん要るのでこちらを使う。
    pub fn launcher_row_snapshots(&self, out: &mut Vec<(u64, LauncherRowSnapshot)>) {
        out.clear();
        for cell in &self.bridge().launcher_rows {
            // publisher は `row_key` を最後に書くので、ここで先に読めば
            // 「新しい key と古い値」の組は見えない (set_launcher_row の doc)。
            // 格納値は `row_key + 1` (0 = 空きスロット)。
            let stored = cell.row_key.load(Ordering::Acquire);
            if stored == 0 {
                break;
            }
            let key = stored - 1;
            out.push((
                key,
                LauncherRowSnapshot {
                    state: cell.state.load(Ordering::Acquire),
                    playing_clip_id: cell.playing_clip_id.load(Ordering::Acquire),
                    queued_clip_id: cell.queued_clip_id.load(Ordering::Acquire),
                    queued_at_beat: f64::from_bits(
                        cell.queued_at_beat_bits.load(Ordering::Acquire),
                    ),
                    progress: f32::from_bits(cell.progress_bits.load(Ordering::Acquire)),
                    launch_beat: f64::from_bits(
                        cell.launch_beat_bits.load(Ordering::Acquire),
                    ),
                },
            ));
        }
    }
}

// The underlying shared memory is safe to share across threads; every
// field is a lock-free atomic and readers tolerate any observed value.
unsafe impl Send for AudioBridgeHandle {}
unsafe impl Sync for AudioBridgeHandle {}

pub fn shmem_id(parent_pid: u32) -> String {
    format!("daw_01_audio_{parent_pid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #87: **`row_key == 0` の行 (track_id 0 / lane_id 0) も GUI へ届く。**
    /// 空きスロットの印と実 key が衝突していたときは、その行で読み取りが打ち切られ、
    /// 以降の行が丸ごと届かなかった (= セルの進捗が一切出ない)。
    #[test]
    fn 行キー_0_の行も読み出せる() {
        let name = format!("daw01_test_bridge_{}", std::process::id());
        let h = AudioBridgeHandle::create(&name).expect("bridge");
        h.set_launcher_row(0, 0, LAUNCHER_STATE_PLAYING, 7, 0, 0.0, 0.25, 4.0);
        h.set_launcher_row(1, (1_u64 << 32) | 2, LAUNCHER_STATE_STOPPED, 0, 9, 12.0, 0.0, 0.0);
        h.clear_launcher_rows_from(2);

        let mut out = Vec::new();
        h.launcher_row_snapshots(&mut out);
        assert_eq!(out.len(), 2, "2 行とも届く: {out:?}");
        assert_eq!(out[0].0, 0, "1 行目の key は 0 (track_id 0 / lane_id 0)");
        assert_eq!(out[0].1.playing_clip_id, 7);
        assert_eq!(out[1].0, (1_u64 << 32) | 2);
        // 予約は「どのセルか」と「いつ鳴るか」が組で届く (GUI のカウントダウン)。
        assert_eq!(out[1].1.queued_clip_id, 9);
        assert!((out[1].1.queued_at_beat - 12.0).abs() < 1e-9);
        // 単発引きも同じ行を引ける。
        let one = h.launcher_row(0, 0).expect("row_key 0 も引ける");
        assert_eq!(one.playing_clip_id, 7);
    }

    /// r.md #89: **値面は id で引ける。** slot の並びが変わっても、消えたソースの
    /// 値が残っても、id が一致するものだけが返る。
    #[test]
    fn 変調値面は_id_で往復し消えたソースは残らない() {
        let name = format!("daw01_test_modplane_{}", std::process::id());
        let h = AudioBridgeHandle::create(&name).expect("bridge");

        let mut published = crate::mod_plane::ModPlane::default();
        published.push(11, 0.25);
        published.push(4, 0.5);
        published.push(9, 0.75);
        h.publish_mod_plane(&published);

        let mut got = crate::mod_plane::ModPlane::default();
        assert!(h.read_mod_plane(&mut got), "seqlock が破れていない");
        assert_eq!(got.len(), 3);
        assert_eq!(got.scalar(4), 0.5);
        assert_eq!(got.scalar(9), 0.75);

        // ソースを 1 つ消して並べ替えた面を publish し直す。
        let mut next = crate::mod_plane::ModPlane::default();
        next.push(9, 0.1);
        next.push(11, 0.2);
        h.publish_mod_plane(&next);
        assert!(h.read_mod_plane(&mut got));
        assert_eq!(got.len(), 2, "空いた slot は id 0 で潰れている");
        assert_eq!(got.scalar(9), 0.1);
        assert_eq!(got.scalar(11), 0.2);
        // 消えた id は 0 (= 変調なし) に倒れる。前の値 0.5 が残っていない。
        assert_eq!(got.scalar(4), 0.0);
    }
}
