//! プロジェクト 1 つぶんの **伸びる telemetry 面** (`docs/plan_unbounded_tracks.md` §3)。
//!
//! 固定 shmem の [`super::ProjectTelemetry`] にはトラック数に比例しない値 (再生位置 / transport /
//! Limiter の GR) だけを残し、**数が曲で決まるもの** (トラックのピークと鳴っているボイス / 内蔵
//! device の GR / 変調ソースの値 / ランチャー行) をここへ置く。容量は曲が要る分から決まり、足りなく
//! なったら書き手 (daw_audio) が **別名で作り直す** — 固定長の器に溢れた分を黙って捨てない。
//!
//! # 同一性と受け渡し
//!
//! - 面の名前は [`plane_shmem_id`] = 固定 shmem の id + 作成プロセスの pid + 世代。世代は作成プロセス内の
//!   単調カウンタなので、同じ名前が二度作られることはない (`crate::shmem` の命名契約)。
//! - 書き手は新しい面を RT へ届け、RT が書き始めた時点で `ProjectTelemetry::plane_id` を差し替える。
//!   読み手 (daw_gui) は `plane_id` が変わったら開き直す。旧面は書き手が閉じても読み手が握っている間は
//!   カーネルが保持するので、読み手の途中で消えない。
//!
//! # 並びではなく id で引く (アーキ不変条件 1)
//!
//! 各面の slot の並びは書き手のその buffer 限りの都合 (曲の順) で、意味を持つのは slot に一緒に書く id。
//! トラック面・GR 面・変調値面は id 表と値を**組で**読む seqlock を面ごとに持つ (奇数 = 書き込み中)。
//! ランチャー行は「`row_key` を最後に書く」規約で読む。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use anyhow::Result;

use super::{LauncherRowSnapshot, MAX_PUBLISHED_VOICES, VoiceSnapshot};
use crate::mod_plane::ModPlane;
use crate::shmem::NamedShmem;

/// 面の先頭に置く印 (壊れた / 別レイアウトの面を開いたら弾く)。
const PLANE_MAGIC: u32 = 0x4441_5450; // "DATP"

/// 読み手が seqlock の読み直しを諦めるまでの回数。書き手は 1 buffer に 1 回しか面を触らないので、
/// 30Hz の読み手が 8 回連続で書き込み中に当たることは実質ない (当たったら「今回は更新なし」)。
const READ_RETRIES: usize = 8;

/// 作り直すときの各面の最小容量 (曲が小さいうちに 1 本ずつ作り直さない)。
const MIN_CAPACITY: PlaneCapacity = PlaneCapacity { tracks: 16, native_meters: 16, mod_sources: 16, launcher_rows: 32 };

/// 面ごとの容量 (slot 数)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaneCapacity {
    pub tracks: u32,
    pub native_meters: u32,
    pub mod_sources: u32,
    pub launcher_rows: u32,
}

impl PlaneCapacity {
    /// `need` を全部収められるか。
    #[must_use]
    pub fn covers(&self, need: &Self) -> bool {
        self.tracks >= need.tracks
            && self.native_meters >= need.native_meters
            && self.mod_sources >= need.mod_sources
            && self.launcher_rows >= need.launcher_rows
    }

    /// `need` を収めるために作り直す容量: 面ごとに「今の容量」と「`need` を 2 冪に切り上げた値」の
    /// 大きい方 (縮めない)。
    #[must_use]
    pub fn grown_for(&self, need: &Self) -> Self {
        let grow = |have: u32, need: u32, min: u32| have.max(need.max(min).checked_next_power_of_two().unwrap_or(need));
        Self {
            tracks: grow(self.tracks, need.tracks, MIN_CAPACITY.tracks),
            native_meters: grow(self.native_meters, need.native_meters, MIN_CAPACITY.native_meters),
            mod_sources: grow(self.mod_sources, need.mod_sources, MIN_CAPACITY.mod_sources),
            launcher_rows: grow(self.launcher_rows, need.launcher_rows, MIN_CAPACITY.launcher_rows),
        }
    }
}

/// `pid` のプロセスが作った `generation` 番目の面の id。`0` は「面なし」の印なので世代は 1 から。
#[must_use]
pub fn plane_id(pid: u32, generation: u32) -> u64 {
    debug_assert!(generation != 0, "世代 0 は「面なし」の印");
    (u64::from(pid) << 32) | u64::from(generation)
}

/// 面の shmem 名。`base` は固定 shmem (`AudioBridge`) の id。
#[must_use]
pub fn plane_shmem_id(base: &str, plane_id: u64) -> String {
    format!("{base}_plane_{}_{}", plane_id >> 32, plane_id & u64::from(u32::MAX))
}

#[repr(C)]
struct Header {
    magic: u32,
    tracks: u32,
    native_meters: u32,
    mod_sources: u32,
    launcher_rows: u32,
    _pad0: u32,
    track_generation: AtomicU64,
    n_tracks: AtomicU32,
    _pad1: u32,
    native_generation: AtomicU64,
    mod_generation: AtomicU64,
}

/// 鳴っているボイス 1 つ (値は `f64::to_bits`、`off_secs` の `NaN` = まだ押している)。
#[repr(C)]
struct VoiceSlot {
    on_beat: AtomicU64,
    on_secs: AtomicU64,
    off_secs: AtomicU64,
}

/// `VoiceSlot` 1 個を読む (`off_secs` の NaN = まだ鳴っている)。
fn read_voice(vs: &VoiceSlot) -> VoiceSnapshot {
    let off = f64::from_bits(vs.off_secs.load(Ordering::Relaxed));
    VoiceSnapshot {
        on_beat: f64::from_bits(vs.on_beat.load(Ordering::Relaxed)),
        on_secs: f64::from_bits(vs.on_secs.load(Ordering::Relaxed)),
        off_secs: (!off.is_nan()).then_some(off),
    }
}

/// トラック 1 本ぶん: post-fader ピーク (`f32::to_bits`) と、chain の最初の plugin のボイス表
/// (変調ラックの per-voice カーソル、r.md #117)。
#[repr(C)]
struct TrackSlot {
    track_id: AtomicU32,
    peak_l: AtomicU32,
    peak_r: AtomicU32,
    voice_len: AtomicU32,
    voices: [VoiceSlot; MAX_PUBLISHED_VOICES],
}

/// 内蔵 device (Comp / Bus Comp) の GR (dB、0 以下、`f32::to_bits`)。
#[repr(C)]
struct NativeMeterSlot {
    device_id: AtomicU64,
    gr: AtomicU32,
    _pad: u32,
}

/// 変調ソース 1 つの値 (`ModSource::id`、`f32::to_bits`)。
#[repr(C)]
struct ModSlot {
    source_id: AtomicU32,
    value: AtomicU32,
}

/// r.md #87: 1 行ぶんの走行状態 (表示専用)。
///
/// **`Song` には入れない** — フォローアクションで移った先を保存すると「何秒鳴らしてから書き出したか」で
/// 出力が変わり、Q9 の再現性が壊れる (`docs/plan_rmd_87_clip_launcher.md` §1.4)。
#[repr(C)]
struct LauncherRowState {
    /// 行の安定 id を 1 ワードに詰めた `(track_id as u64) << 32 | lane_id` **の +1**
    /// (`lane_id == 0` がトラック行)。**`0` = 空きスロット** — `row_key` そのものを入れると
    /// `track_id` も `lane_id` も 0 の行が空きと見分けられず、そこで読み取りが打ち切られて
    /// 以降の行が丸ごと届かない。
    row_key: AtomicU64,
    /// `LAUNCHER_STATE_*` のいずれか。
    state: AtomicU32,
    /// いま鳴っているセルの `clip.id` (0 = なし)。
    playing_clip_id: AtomicU32,
    /// 量子化境界待ちのセルの `clip.id` (0 = なし / `LAUNCHER_QUEUED_STOP` / `LAUNCHER_QUEUED_ARRANGER`)。
    queued_clip_id: AtomicU32,
    /// 予約が **発火する song 拍** (`f64::to_bits`)。GUI のカウントダウンはこれを引き算するだけに
    /// する — 境界を GUI 側で解き直すとフォローアクション経由の予約で必ず食い違う。
    queued_at_beat_bits: AtomicU64,
    /// いま鳴っているセルの中の進捗 `0..1` (`f32::to_bits`)。停止中は 0。
    progress_bits: AtomicU32,
    /// いま鳴っているセルを **撃った song 拍** (`f64::to_bits`)。映像側が自分のフレーム時刻から
    /// 音と同じ式で位相を解くのに使う (計画書 §3.6)。停止中は 0。
    launch_beat_bits: AtomicU64,
}

/// 各配列の開始 offset と面全体の大きさ。容量は信頼境界の外 (他プロセスが書いた値) から来るので
/// checked 算術で組む。
#[derive(Debug, Clone, Copy)]
struct Layout {
    tracks: usize,
    native_meters: usize,
    mod_sources: usize,
    launcher_rows: usize,
    total: usize,
}

impl Layout {
    fn of(cap: PlaneCapacity) -> Option<Self> {
        fn array_end<T>(start: usize, n: u32) -> Option<usize> {
            std::mem::size_of::<T>().checked_mul(n as usize)?.checked_add(start)
        }
        fn align8(x: usize) -> Option<usize> {
            Some(x.checked_add(7)? & !7)
        }
        let tracks = align8(std::mem::size_of::<Header>())?;
        let native_meters = align8(array_end::<TrackSlot>(tracks, cap.tracks)?)?;
        let mod_sources = align8(array_end::<NativeMeterSlot>(native_meters, cap.native_meters)?)?;
        let launcher_rows = align8(array_end::<ModSlot>(mod_sources, cap.mod_sources)?)?;
        let total = array_end::<LauncherRowState>(launcher_rows, cap.launcher_rows)?;
        Some(Self { tracks, native_meters, mod_sources, launcher_rows, total })
    }
}

/// 伸びる telemetry 面 1 枚の handle。書き手 (daw_audio) は [`Self::create`]、読み手 (daw_gui) は
/// [`Self::open`]。全フィールドが atomic なので `&self` のまま両プロセス・複数スレッドから触れる。
pub struct TelemetryPlane {
    shmem: NamedShmem,
    id: u64,
    cap: PlaneCapacity,
    layout: Layout,
}

// SAFETY: 写像された領域は atomic だけを持ち (容量は作成時に 1 度だけ書く)、読み手は観測した
// どの値にも耐える (`AudioBridgeHandle` と同じ理由)。
unsafe impl Send for TelemetryPlane {}
unsafe impl Sync for TelemetryPlane {}

impl TelemetryPlane {
    /// 書き手: `cap` の面を新規に作る (全 slot 空き)。同じ名前が既にあれば失敗する。
    pub fn create(base: &str, id: u64, cap: PlaneCapacity) -> Result<Self> {
        anyhow::ensure!(id != 0, "plane id 0 は「面なし」の印");
        let layout = Layout::of(cap).ok_or_else(|| anyhow::anyhow!("telemetry plane の容量が大きすぎる: {cap:?}"))?;
        let shmem = NamedShmem::create(&plane_shmem_id(base, id), layout.total)?;
        // SAFETY: 作ったばかりの領域は `layout.total` バイトあり、まだ誰とも共有していない。
        unsafe {
            std::ptr::write_bytes(shmem.as_ptr(), 0, layout.total);
            let h = &mut *shmem.as_ptr().cast::<Header>();
            h.magic = PLANE_MAGIC;
            h.tracks = cap.tracks;
            h.native_meters = cap.native_meters;
            h.mod_sources = cap.mod_sources;
            h.launcher_rows = cap.launcher_rows;
        }
        Ok(Self { shmem, id, cap, layout })
    }

    /// 読み手: `id` の面を開く。印と容量を検証し、写像が容量ぶんの大きさを持たなければ失敗する。
    pub fn open(base: &str, id: u64) -> Result<Self> {
        anyhow::ensure!(id != 0, "plane id 0 は「面なし」の印");
        let name = plane_shmem_id(base, id);
        let shmem = NamedShmem::open(&name, std::mem::size_of::<Header>())?;
        // SAFETY: 写像は `Header` 以上の大きさ (open が検証済み)、view は 64 KiB 境界に align される。
        let h = unsafe { &*shmem.as_ptr().cast::<Header>() };
        anyhow::ensure!(h.magic == PLANE_MAGIC, "{name} は telemetry plane ではない");
        let cap = PlaneCapacity {
            tracks: h.tracks,
            native_meters: h.native_meters,
            mod_sources: h.mod_sources,
            launcher_rows: h.launcher_rows,
        };
        let layout = Layout::of(cap).ok_or_else(|| anyhow::anyhow!("{name} の容量が壊れている: {cap:?}"))?;
        anyhow::ensure!(shmem.len() >= layout.total, "{name} が容量より小さい: {} < {}", shmem.len(), layout.total);
        Ok(Self { shmem, id, cap, layout })
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn capacity(&self) -> PlaneCapacity {
        self.cap
    }

    fn header(&self) -> &Header {
        // SAFETY: create / open が `layout.total` (>= Header) の写像を検証済み。
        unsafe { &*self.shmem.as_ptr().cast::<Header>() }
    }

    /// SAFETY 契約: `offset` から `n` 個の `T` が写像に収まること (`Layout::of` が保証する組だけを渡す)。
    fn slice<T>(&self, offset: usize, n: u32) -> &[T] {
        // SAFETY: `Layout::of` の offset / 容量の組は `layout.total` 以内で、写像はそれ以上の大きさを持つ
        // (create / open が検証)。要素は atomic だけなので任意のビット列が有効。
        unsafe { std::slice::from_raw_parts(self.shmem.as_ptr().add(offset).cast::<T>(), n as usize) }
    }

    fn tracks(&self) -> &[TrackSlot] {
        self.slice(self.layout.tracks, self.cap.tracks)
    }

    fn native_meters(&self) -> &[NativeMeterSlot] {
        self.slice(self.layout.native_meters, self.cap.native_meters)
    }

    fn mod_sources(&self) -> &[ModSlot] {
        self.slice(self.layout.mod_sources, self.cap.mod_sources)
    }

    fn launcher_rows(&self) -> &[LauncherRowState] {
        self.slice(self.layout.launcher_rows, self.cap.launcher_rows)
    }

    // ---- トラック面 --------------------------------------------------------

    /// 書き手 (audio thread、毎 buffer 1 回): トラック面を**丸ごと** publish する。
    /// 各要素 = `(track id, peak L, peak R, 鳴っているボイス)`。容量を超えたぶん / 1 track あたり
    /// [`MAX_PUBLISHED_VOICES`] を超えたボイスは捨てる (容量は曲から決めるので通常は溢れない)。
    ///
    /// RT 安全: atomic store のみ。
    pub fn publish_tracks<I, V>(&self, tracks: I)
    where
        I: Iterator<Item = (u32, f32, f32, V)>,
        V: Iterator<Item = VoiceSnapshot>,
    {
        let h = self.header();
        let g = h.track_generation.load(Ordering::Relaxed);
        h.track_generation.store(g.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let slots = self.tracks();
        let mut n = 0usize;
        for ((id, l, r, voices), slot) in tracks.zip(slots) {
            slot.track_id.store(id, Ordering::Relaxed);
            slot.peak_l.store(l.to_bits(), Ordering::Relaxed);
            slot.peak_r.store(r.to_bits(), Ordering::Relaxed);
            let mut k = 0usize;
            for (v, vs) in voices.zip(&slot.voices) {
                vs.on_beat.store(v.on_beat.to_bits(), Ordering::Relaxed);
                vs.on_secs.store(v.on_secs.to_bits(), Ordering::Relaxed);
                vs.off_secs.store(v.off_secs.unwrap_or(f64::NAN).to_bits(), Ordering::Relaxed);
                k += 1;
            }
            #[allow(clippy::cast_possible_truncation)]
            slot.voice_len.store(k as u32, Ordering::Relaxed);
            n += 1;
        }
        #[allow(clippy::cast_possible_truncation)]
        h.n_tracks.store(n as u32, Ordering::Relaxed);
        h.track_generation.store(g.wrapping_add(2), Ordering::Release);
    }

    /// 読み手 (GUI の 30Hz poller): `peaks` に `(track id, L, R)`、`voices` に `(track id, ボイス)` を
    /// 積み直す。書き込み中に当たったら読み直し、[`READ_RETRIES`] 回とも破れたら `false`
    /// (両方とも空になるので呼び側は前回値を保つ)。`Vec` は使い回すので確保は起きない。
    pub fn read_tracks(&self, peaks: &mut Vec<(u32, f32, f32)>, voices: &mut Vec<(u32, VoiceSnapshot)>) -> bool {
        let h = self.header();
        for _ in 0..READ_RETRIES {
            let g0 = h.track_generation.load(Ordering::Acquire);
            if g0 & 1 != 0 {
                continue;
            }
            peaks.clear();
            voices.clear();
            let n = (h.n_tracks.load(Ordering::Relaxed) as usize).min(self.cap.tracks as usize);
            for slot in &self.tracks()[..n] {
                let id = slot.track_id.load(Ordering::Relaxed);
                let l = f32::from_bits(slot.peak_l.load(Ordering::Relaxed));
                let r = f32::from_bits(slot.peak_r.load(Ordering::Relaxed));
                peaks.push((id, l, r));
                let k = (slot.voice_len.load(Ordering::Relaxed) as usize).min(MAX_PUBLISHED_VOICES);
                voices.extend(slot.voices[..k].iter().map(|vs| (id, read_voice(vs))));
            }
            fence(Ordering::Acquire);
            if h.track_generation.load(Ordering::Relaxed) == g0 {
                return true;
            }
        }
        peaks.clear();
        voices.clear();
        false
    }

    // ---- 内蔵 device の GR 面 (r.md #129) ------------------------------------

    /// 書き手 (audio thread、毎 buffer 1 回): GR 面 (device id + GR dB) を**丸ごと** publish する。
    /// 残りの slot は `id = 0` (空き) で潰す (処理していない device の GR が前の値のまま残らない)。
    ///
    /// RT 安全: atomic store のみ。
    pub fn publish_native_meters(&self, it: impl Iterator<Item = (u64, f32)>) {
        let h = self.header();
        let g = h.native_generation.load(Ordering::Relaxed);
        h.native_generation.store(g.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let slots = self.native_meters();
        let mut n = 0usize;
        for ((id, gr), slot) in it.zip(slots) {
            slot.device_id.store(id, Ordering::Relaxed);
            slot.gr.store(gr.to_bits(), Ordering::Relaxed);
            n += 1;
        }
        for slot in &slots[n..] {
            slot.device_id.store(0, Ordering::Relaxed);
            slot.gr.store(0f32.to_bits(), Ordering::Relaxed);
        }
        h.native_generation.store(g.wrapping_add(2), Ordering::Release);
    }

    /// 読み手: GR 面を `(device id, GR dB)` で `out` に積み直す。破れたら `false` (`out` は空)。
    pub fn read_native_meters(&self, out: &mut Vec<(u64, f32)>) -> bool {
        let h = self.header();
        for _ in 0..READ_RETRIES {
            let g0 = h.native_generation.load(Ordering::Acquire);
            if g0 & 1 != 0 {
                continue;
            }
            out.clear();
            for slot in self.native_meters() {
                let id = slot.device_id.load(Ordering::Relaxed);
                if id != 0 {
                    out.push((id, f32::from_bits(slot.gr.load(Ordering::Relaxed))));
                }
            }
            fence(Ordering::Acquire);
            if h.native_generation.load(Ordering::Relaxed) == g0 {
                return true;
            }
        }
        out.clear();
        false
    }

    /// トラック面と GR 面を空にする。park (= 再生も録音もしていないので publish を止める) の直前に
    /// 呼ぶ — 無いと GUI は最後の値を読み続け、止まったメーターが点いたまま凍る。
    pub fn clear_meters(&self) {
        self.publish_tracks(std::iter::empty::<(u32, f32, f32, std::iter::Empty<VoiceSnapshot>)>());
        self.publish_native_meters(std::iter::empty());
    }

    // ---- 変調値面 (r.md #89) --------------------------------------------------

    /// 書き手 (audio thread、毎 buffer 1 回): 変調ソースの値面 (id 表 + 値) を**丸ごと** publish する。
    /// 溢れなかった残りの slot は `id = 0` (空き) で潰す — 消えたソースの値が残り続けると、id を
    /// 使い回した別のソースがその値を拾う。
    ///
    /// RT 安全: atomic store のみ。
    pub fn publish_mod_plane(&self, plane: &ModPlane) {
        let h = self.header();
        let g = h.mod_generation.load(Ordering::Relaxed);
        h.mod_generation.store(g.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let (ids, values) = (plane.ids(), plane.values());
        for (i, slot) in self.mod_sources().iter().enumerate() {
            slot.source_id.store(ids.get(i).copied().unwrap_or(0), Ordering::Relaxed);
            slot.value.store(values.get(i).copied().unwrap_or(0.0).to_bits(), Ordering::Relaxed);
        }
        h.mod_generation.store(g.wrapping_add(2), Ordering::Release);
    }

    /// 読み手: 値面を `out` に積み直す。破れたら `false` を返して `out` は触らない (= 前回値を保つ)。
    pub fn read_mod_plane(&self, out: &mut ModPlane) -> bool {
        let h = self.header();
        for _ in 0..READ_RETRIES {
            let g0 = h.mod_generation.load(Ordering::Acquire);
            if g0 & 1 != 0 {
                continue;
            }
            out.clear();
            for slot in self.mod_sources() {
                let id = slot.source_id.load(Ordering::Relaxed);
                if id != 0 {
                    out.push(id, f32::from_bits(slot.value.load(Ordering::Relaxed)));
                }
            }
            fence(Ordering::Acquire);
            if h.mod_generation.load(Ordering::Relaxed) == g0 {
                return true;
            }
        }
        false
    }

    // ---- ランチャー行 (r.md #87) ---------------------------------------------

    /// 書き手 (audio thread): 1 行ぶんの走行状態を publish する。`slot` は engine が毎 buffer 詰め直す
    /// その buffer 限りの並びで、意味を持つのは `row_key`。範囲外は捨てる。
    ///
    /// RT 安全: atomic store のみ。
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
        let Some(cell) = self.launcher_rows().get(slot) else {
            return;
        };
        cell.state.store(state, Ordering::Release);
        cell.playing_clip_id.store(playing_clip_id, Ordering::Release);
        cell.queued_clip_id.store(queued_clip_id, Ordering::Release);
        cell.queued_at_beat_bits.store(queued_at_beat.to_bits(), Ordering::Release);
        cell.progress_bits.store(progress.to_bits(), Ordering::Release);
        cell.launch_beat_bits.store(launch_beat.to_bits(), Ordering::Release);
        // `row_key` は **最後に**書く — 読み手は「使用中か」を見てから残りを読むので、先に書くと 1 tick だけ
        // 古い state と新しい key の組を見せてしまう。格納値は `row_key + 1` (0 = 空きスロット)。
        cell.row_key.store(row_key.saturating_add(1), Ordering::Release);
    }

    /// 書き手: `slot` 以降を「空き」にする (engine が publish した行数より後ろ)。
    pub fn clear_launcher_rows_from(&self, slot: usize) {
        for cell in self.launcher_rows().iter().skip(slot) {
            if cell.row_key.load(Ordering::Acquire) == 0 {
                break;
            }
            cell.row_key.store(0, Ordering::Release);
        }
    }

    /// 読み手: 行の走行状態を安定 id で引く (`lane_id == 0` がトラック行)。見つからなければ `None`。
    #[must_use]
    pub fn launcher_row(&self, track_id: u32, lane_id: u32) -> Option<LauncherRowSnapshot> {
        let want = ((u64::from(track_id) << 32) | u64::from(lane_id)).saturating_add(1);
        for cell in self.launcher_rows() {
            let key = cell.row_key.load(Ordering::Acquire);
            if key == 0 {
                break;
            }
            if key == want {
                return Some(launcher_snapshot(cell));
            }
        }
        None
    }

    /// 読み手 (GUI の 30Hz poller): publish 済みの行を **まとめて** `(row_key, snapshot)` で読み出す。
    /// 1 行ずつ引く [`Self::launcher_row`] を行数ぶん呼ぶと O(n²) になるので、全行ぶん要る表示はこちら。
    pub fn launcher_row_snapshots(&self, out: &mut Vec<(u64, LauncherRowSnapshot)>) {
        out.clear();
        for cell in self.launcher_rows() {
            // 書き手は `row_key` を最後に書くので、ここで先に読めば「新しい key と古い値」の組は見えない。
            let stored = cell.row_key.load(Ordering::Acquire);
            if stored == 0 {
                break;
            }
            out.push((stored - 1, launcher_snapshot(cell)));
        }
    }
}

fn launcher_snapshot(cell: &LauncherRowState) -> LauncherRowSnapshot {
    LauncherRowSnapshot {
        state: cell.state.load(Ordering::Acquire),
        playing_clip_id: cell.playing_clip_id.load(Ordering::Acquire),
        queued_clip_id: cell.queued_clip_id.load(Ordering::Acquire),
        queued_at_beat: f64::from_bits(cell.queued_at_beat_bits.load(Ordering::Acquire)),
        progress: f32::from_bits(cell.progress_bits.load(Ordering::Acquire)),
        launch_beat: f64::from_bits(cell.launch_beat_bits.load(Ordering::Acquire)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{LAUNCHER_STATE_PLAYING, LAUNCHER_STATE_STOPPED};
    use super::*;

    fn base(tag: &str) -> String {
        format!("daw01_test_plane_{tag}_{}", std::process::id())
    }

    fn cap(tracks: u32) -> PlaneCapacity {
        PlaneCapacity { tracks, native_meters: 4, mod_sources: 4, launcher_rows: 4 }
    }

    /// トラック面は id で往復し、容量ぶん (曲の本数) 全部が届く。publish し直したら減ったぶんは残らない。
    #[test]
    fn トラック面は_id_で往復し本数ぶん全部届く() {
        let base = base("tracks");
        let w = TelemetryPlane::create(&base, plane_id(1, 1), cap(200)).expect("create");
        let held = VoiceSnapshot { on_beat: 4.0, on_secs: 2.0, off_secs: None };
        let released = VoiceSnapshot { on_beat: 5.0, on_secs: 2.5, off_secs: Some(3.0) };
        w.publish_tracks((0..200u32).map(|i| {
            let voices: Vec<VoiceSnapshot> = if i == 199 { vec![held, released] } else { Vec::new() };
            (i + 1000, i as f32 / 200.0, 0.5, voices.into_iter())
        }));

        let r = TelemetryPlane::open(&base, w.id()).expect("open");
        let (mut peaks, mut voices) = (Vec::new(), Vec::new());
        assert!(r.read_tracks(&mut peaks, &mut voices));
        assert_eq!(peaks.len(), 200);
        assert_eq!(peaks[199], (1199, 199.0 / 200.0, 0.5), "200 本目も id 付きで届く");
        assert_eq!(voices, vec![(1199, held), (1199, released)]);

        w.publish_tracks([(7u32, 0.25f32, 0.125f32, std::iter::empty())].into_iter());
        assert!(r.read_tracks(&mut peaks, &mut voices));
        assert_eq!(peaks, vec![(7, 0.25, 0.125)], "消えたトラックは残らない");
        assert!(voices.is_empty());
    }

    /// 書き込み中 (世代が奇数) は読めず、前回値を保てるよう空で `false` を返す。
    #[test]
    fn 書き込み中のトラック面は読めない() {
        let w = TelemetryPlane::create(&base("seqlock"), plane_id(1, 1), cap(4)).expect("create");
        let h = w.header();
        h.track_generation.store(1, Ordering::Relaxed);
        let (mut peaks, mut voices) = (vec![(1, 1.0, 1.0)], Vec::new());
        assert!(!w.read_tracks(&mut peaks, &mut voices));
        assert!(peaks.is_empty());
    }

    /// 作り直した面は別名。**旧面を開いている読み手は、書き手が旧面を閉じた後も読める**
    /// (GUI が `plane_id` の差し替えより遅れて読むことがある)。
    #[test]
    fn 作り直しても旧面を開いている読み手は壊れない() {
        let base = base("regrow");
        let old = TelemetryPlane::create(&base, plane_id(1, 1), cap(2)).expect("old");
        old.publish_native_meters([(11, -3.0)].into_iter());
        let reader = TelemetryPlane::open(&base, old.id()).expect("open old");
        let grown = old.capacity().grown_for(&cap(40));
        let new = TelemetryPlane::create(&base, plane_id(1, 2), grown).expect("new");
        drop(old);
        let mut out = Vec::new();
        assert!(reader.read_native_meters(&mut out));
        assert_eq!(out, vec![(11, -3.0)], "書き手が閉じても読み手の写像は生きている");
        assert_eq!(TelemetryPlane::open(&base, new.id()).expect("open new").capacity().tracks, 64);
    }

    /// 容量は 2 冪に切り上げ、縮めない。最小容量より小さい曲でも 1 本ずつ作り直さない。
    #[test]
    fn 容量は_2_冪に切り上げて縮めない() {
        let need = PlaneCapacity { tracks: 33, native_meters: 1, mod_sources: 65, launcher_rows: 513 };
        let grown = PlaneCapacity::default().grown_for(&need);
        assert_eq!(grown, PlaneCapacity { tracks: 64, native_meters: 16, mod_sources: 128, launcher_rows: 1024 });
        assert!(grown.covers(&need));
        let shrink = PlaneCapacity { tracks: 2, ..need };
        assert_eq!(grown.grown_for(&shrink), grown, "小さい要求で縮めない");
        assert!(!PlaneCapacity { tracks: 32, ..grown }.covers(&need));
    }

    /// 変調値面・GR 面は id で引き、消えた id は次の publish で残らない。
    #[test]
    fn 変調値面と_gr_面は_id_で往復する() {
        let w = TelemetryPlane::create(&base("mod"), plane_id(1, 1), cap(1)).expect("create");
        let mut plane = ModPlane::default();
        plane.push(11, 0.25);
        plane.push(4, 0.5);
        w.publish_mod_plane(&plane);
        let mut got = ModPlane::default();
        assert!(w.read_mod_plane(&mut got));
        assert_eq!((got.len(), got.scalar(4)), (2, 0.5));
        let mut next = ModPlane::default();
        next.push(11, 0.1);
        w.publish_mod_plane(&next);
        assert!(w.read_mod_plane(&mut got));
        assert_eq!((got.len(), got.scalar(4)), (1, 0.0), "消えたソースは残らない");

        let mut out = Vec::new();
        w.publish_native_meters([(11, -3.0), (4, -1.0)].into_iter());
        w.publish_native_meters([(4, -2.0)].into_iter());
        assert!(w.read_native_meters(&mut out));
        assert_eq!(out, vec![(4, -2.0)]);
        w.clear_meters();
        assert!(w.read_native_meters(&mut out) && out.is_empty());
    }

    /// r.md #87: `row_key == 0` の行 (track_id 0 / lane_id 0) も読み出せる。
    #[test]
    fn 行キー_0_の行も読み出せる() {
        let w = TelemetryPlane::create(&base("rows"), plane_id(1, 1), cap(1)).expect("create");
        w.set_launcher_row(0, 0, LAUNCHER_STATE_PLAYING, 7, 0, 0.0, 0.25, 4.0);
        w.set_launcher_row(1, (1_u64 << 32) | 2, LAUNCHER_STATE_STOPPED, 0, 9, 12.0, 0.0, 0.0);
        w.clear_launcher_rows_from(2);
        let mut out = Vec::new();
        w.launcher_row_snapshots(&mut out);
        assert_eq!(out.len(), 2, "2 行とも届く: {out:?}");
        assert_eq!((out[0].0, out[0].1.playing_clip_id), (0, 7));
        assert_eq!((out[1].0, out[1].1.queued_clip_id), ((1_u64 << 32) | 2, 9));
        assert!((out[1].1.queued_at_beat - 12.0).abs() < 1e-9);
        assert_eq!(w.launcher_row(0, 0).expect("row_key 0 も引ける").playing_clip_id, 7);
    }

    /// 印の合わない面 / 容量より小さい写像は開けない (信頼境界の外の値で範囲外を読まない)。
    #[test]
    fn 壊れた面は開けない() {
        let base = base("corrupt");
        let id = plane_id(1, 1);
        let shmem = NamedShmem::create(&plane_shmem_id(&base, id), std::mem::size_of::<Header>()).expect("raw");
        // SAFETY: 作ったばかりの `Header` 大の領域。
        unsafe {
            let h = &mut *shmem.as_ptr().cast::<Header>();
            h.magic = PLANE_MAGIC;
            h.tracks = 1 << 30;
        }
        assert!(TelemetryPlane::open(&base, id).is_err(), "容量より小さい写像");
        // SAFETY: 同上。
        unsafe { (*shmem.as_ptr().cast::<Header>()).magic = 0 };
        assert!(TelemetryPlane::open(&base, id).is_err(), "印が違う");
    }
}
