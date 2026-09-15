//! r.md #129: 内蔵 device のチェーン op ([`ChainOp::Native`](super::ChainOp::Native)) の scratch と
//! RT 実行 (`docs/plan_rack_native_devices.md` §8.2 / §8.4)。
//!
//! - [`NativeScratch`] — device 1 台ぶんの DSP 状態 / bypass の crossfade / SC の受け皿 /
//!   Listen の受け皿 / GR。compile 時に `ChainProgram::natives` へ確保し、device id で引き継ぐ。
//! - [`run_native`] — op 1 つの実行 (値の解決 → crossfade → SC → DSP → scope)。
//! - [`apply_listen_override`] — SC Listen: トラックのチェーン出力 (PostFx 点) を検出信号で置き換える。
//! - [`stage_native_sidechain`] — `NodeOp::NativeSidechainTap` の staging (post-dispatch)。
//! - [`NativeIo`] — 「聴き方・見方」の状態 (Listen / device scope)。**書き出しは常に既定値**なので、
//!   Listen の音やスペクトラム用の書き込みが WAV に混ざることは構造的に無い。
//!
//! RT 規約: 確保・ロック・I/O なし。バッファはすべて compile 時に確保済み。

use common::device_scope_bridge::{DeviceScopeBridgeHandle, MAX_DEVICE_SCOPES};
use common::model::{NativeDevice, NativeKind, TapPoint, TapSource};

use super::mix::{program_tap_owner, resolve_program_tap, resolve_scratch_tap};
use super::program::{ChainProgram, ProgramCtx};
use super::schedule::{BufRef, MASTER_OWNER};
use crate::mixer::{MAX_FRAMES, TrackScratch};
use crate::native_dsp::{NativeBlock, NativeDsp};

/// bypass を切り替えたときの crossfade の長さ (ms)。
pub const NATIVE_BYPASS_FADE_MS: f32 = 5.0;

/// 検出信号をどこから取るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScMode {
    /// 自分の入力で検出する (未配線 / SC を受けない種類 / 処理しえない / 自トラックの
    /// PostFx・PostFader = feedback / 行き先が解決できない)。
    #[default]
    None,
    /// 自トラックの Pre-FX (同じ pass の `ProgramCtx::own_pre_fx`、lag 0)。build 時に決まる。
    OwnPreFx,
    /// `NodeOp::NativeSidechainTap` が staging した [`ScStage`]。`emit_sidechain_taps` が決める。
    Staged,
    /// 無音で検出する: 読み元が実効的に無効なトラック (r.md #131)。plugin の aux port が inactive (= 無音) で届くのと
    /// 同じ。`emit_sidechain_taps` が決める。
    Silent,
}

/// bypass の wet 量 (0 = 素通し、1 = 効いている)。[`NATIVE_BYPASS_FADE_MS`] で線形に動かす。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BypassFade {
    mix: f32,
}

impl BypassFade {
    /// compile 時の初期値 (Song の静的な `!bypassed`)。切り替えの瞬間ではないのでフェードしない。
    #[must_use]
    pub fn new(active: bool) -> Self {
        Self { mix: if active { 1.0 } else { 0.0 } }
    }

    /// この buffer ぶん `active` の側へ進め、`(buffer 頭の wet, buffer 末の wet)` を返す。
    pub fn advance(&mut self, active: bool, n: usize, sample_rate: f32) -> (f32, f32) {
        let from = self.mix;
        let fade_frames = NATIVE_BYPASS_FADE_MS * 0.001 * sample_rate;
        #[allow(clippy::cast_precision_loss)]
        let step = if fade_frames > 1.0 { n as f32 / fade_frames } else { 1.0 };
        let to = if active { (from + step).min(1.0) } else { (from - step).max(0.0) };
        self.mix = to;
        (from, to)
    }
}

/// サイドチェインの受け皿 (`MAX_FRAMES` × 2ch、compile 時確保)。
pub struct ScStage {
    l: Vec<f32>,
    r: Vec<f32>,
    /// staging した長さ。消費側は `min(n, frames)` まで読み、残りは 0 とみなす。
    frames: u32,
}

impl ScStage {
    #[must_use]
    pub fn new() -> Self {
        Self { l: vec![0.0; MAX_FRAMES], r: vec![0.0; MAX_FRAMES], frames: 0 }
    }

    /// `l` / `r` の先頭 `n` サンプルを写す。
    pub fn stage(&mut self, l: &[f32], r: &[f32], n: usize) {
        let n = n.min(l.len()).min(r.len()).min(self.l.len());
        self.l[..n].copy_from_slice(&l[..n]);
        self.r[..n].copy_from_slice(&r[..n]);
        #[allow(clippy::cast_possible_truncation)]
        {
            self.frames = n as u32;
        }
    }

    /// 消費する検出信号 (`n` と staging した長さの短い方)。
    #[must_use]
    pub fn signal(&self, n: usize) -> (&[f32], &[f32]) {
        let m = n.min(self.frames as usize);
        (&self.l[..m], &self.r[..m])
    }
}

impl Default for ScStage {
    fn default() -> Self {
        Self::new()
    }
}

/// SC Listen の受け皿 (Comp だけが持つ。`MAX_FRAMES` × 2ch、compile 時確保)。
pub struct ListenBuf {
    l: Vec<f32>,
    r: Vec<f32>,
}

impl ListenBuf {
    #[must_use]
    pub fn new() -> Self {
        Self { l: vec![0.0; MAX_FRAMES], r: vec![0.0; MAX_FRAMES] }
    }
}

impl Default for ListenBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// 内蔵 device 1 台ぶんの RT scratch (`ChainProgram::natives`、`ChainOp::Native::native_slot` の先)。
pub struct NativeScratch {
    /// 再 compile を跨ぐ状態移送のキー (= `NativeDevice::id`)。
    pub device_id: u64,
    pub dsp: NativeDsp,
    pub fade: BypassFade,
    pub sc_mode: ScMode,
    /// `sc_mode == Staged` のときだけ `Some`。
    pub sc: Option<ScStage>,
    /// Comp のときだけ `Some`。
    pub listen: Option<ListenBuf>,
    /// 直前 buffer の GR (dB、0 以下)。処理していなければ 0。
    pub gr_db: f32,
    /// GR 面へ publish するか (= GR を持つ種類。面の容量は曲が要る数から決まるので枠は無い)。
    pub meter: bool,
}

impl NativeScratch {
    /// `nd` の scratch を作る (off-RT)。`track_id` = この program の持ち主 (自トラック Pre-FX の判定)。
    #[must_use]
    pub fn new(nd: &NativeDevice, track_id: u32) -> Self {
        let kind = nd.kind();
        // 自トラックの Pre-FX は同じ pass の snapshot を直接読む (staging も依存辺も要らない)。
        // 処理しうるか (`can_activate`) は見ない — snapshot は bypass と無関係に取られる。
        let own_prefx = nd
            .sidechain_input()
            .is_some_and(|r| r.tap.source == TapSource::Track(track_id) && r.tap.tap_point == TapPoint::PreFx);
        Self {
            device_id: nd.id,
            dsp: NativeDsp::new(kind),
            fade: BypassFade::new(!nd.bypassed),
            sc_mode: if own_prefx { ScMode::OwnPreFx } else { ScMode::None },
            sc: None,
            listen: kind.accepts_listen().then(ListenBuf::new),
            gr_db: 0.0,
            meter: kind.has_gain_reduction(),
        }
    }

    /// 再 compile を跨ぐ引き継ぎ (RT 上 = 固定長のコピーと `Vec` の swap だけ)。同じ種類のときだけ。
    /// SC の受け皿も swap する — leaf の 1 buffer 遅れの staging を編集のたびに失わない。
    pub fn adopt_state_from(&mut self, old: &mut NativeScratch) {
        if self.dsp.adopt_state_from(&old.dsp) {
            self.fade = old.fade;
            self.gr_db = old.gr_db;
            if let (Some(a), Some(b)) = (self.sc.as_mut(), old.sc.as_mut()) {
                std::mem::swap(a, b);
            }
        }
    }
}

/// 「聴き方・見方」の状態。Song の外にあり、**書き出し / ラウドネス解析 / bounce は常に既定値**。
#[derive(Clone, Copy, Default)]
pub struct NativeIo<'a> {
    /// SC Listen 中の Comp の device id (`0` = 無し)。
    pub sc_listen: u64,
    /// device scope (EQ Par のスペクトラム) の書き先。scope project の live 描画だけ `Some`。
    pub scopes: Option<DeviceScopeTap<'a>>,
}

/// device scope の書き先と、各 slot が見る device id。
#[derive(Clone, Copy)]
pub struct DeviceScopeTap<'a> {
    pub bridge: &'a DeviceScopeBridgeHandle,
    pub watch: &'a [u64; MAX_DEVICE_SCOPES],
}

/// `ChainOp::Native` 1 つを現在のバスへ適用する (audio 置換・MIDI 素通し)。
///
/// 1. device を引く (無い / 種類違いは素通し、GR 0)
/// 2. 値を解決する (レーン → 変調、block-rate)
/// 3. crossfade を進める (落ち着いた bypass はここで抜ける。OFF → ON は DSP を無音から再開)
/// 4. SC を解決する / 5. Listen / 6. DSP / 7. フェード中なら dry と混ぜる / 8. GR / 9. scope
#[allow(clippy::too_many_arguments)]
pub fn run_native(
    ns: &mut NativeScratch,
    native_slot: u32,
    dry: (&mut [f32], &mut [f32]),
    listen_pending: &mut Option<u32>,
    track_id: u32,
    bus_l: &mut [f32],
    bus_r: &mut [f32],
    n: usize,
    ctx: &ProgramCtx<'_>,
) {
    let n = n.min(bus_l.len()).min(bus_r.len());
    let Some(dev) =
        ctx.song.and_then(|s| ctx.index.native_in(s, ctx.owner, ns.device_id)).filter(|d| d.kind() == ns.dsp.kind())
    else {
        ns.gr_db = 0.0;
        return;
    };
    let v = resolve_values(ctx, dev, track_id);
    let active = !v.bypassed;
    #[allow(clippy::cast_precision_loss)]
    let sample_rate = ctx.sample_rate as f32;
    let (from, to) = ns.fade.advance(active, n, sample_rate);
    if from == 0.0 && to == 0.0 {
        ns.gr_db = 0.0;
        write_scope(ctx, ns.device_id, bus_l, bus_r, n);
        return;
    }
    if from == 0.0 {
        ns.dsp.reset();
    }
    let (dry_l, dry_r) = dry;
    let fading = (from != 1.0 || to != 1.0) && dry_l.len() >= n && dry_r.len() >= n;
    if fading {
        dry_l[..n].copy_from_slice(&bus_l[..n]);
        dry_r[..n].copy_from_slice(&bus_r[..n]);
    }
    let NativeScratch { device_id, dsp, sc_mode, sc, listen, gr_db, .. } = ns;
    let sidechain = match *sc_mode {
        ScMode::None => None,
        ScMode::OwnPreFx => ctx.own_pre_fx,
        ScMode::Staged => sc.as_ref().map(|s| s.signal(n)),
        // 長さ 0 = 全サンプル 0 とみなす (`NativeBlock::sidechain`)。
        ScMode::Silent => Some((&[][..], &[][..])),
    };
    let listen_out = if active && dsp.kind() == NativeKind::Comp && ctx.native.sc_listen == *device_id {
        listen.as_mut().map(|b| (&mut b.l[..], &mut b.r[..]))
    } else {
        None
    };
    if listen_out.is_some() {
        *listen_pending = Some(native_slot);
    }
    let gr = dsp.process(&v.params, NativeBlock { l: bus_l, r: bus_r, n, sample_rate, sidechain, listen_out });
    if fading {
        #[allow(clippy::cast_precision_loss)]
        let inv = 1.0 / n as f32;
        for i in 0..n {
            #[allow(clippy::cast_precision_loss)]
            let w = from + (to - from) * ((i + 1) as f32 * inv);
            bus_l[i] = dry_l[i] + (bus_l[i] - dry_l[i]) * w;
            bus_r[i] = dry_r[i] + (bus_r[i] - dry_r[i]) * w;
        }
    }
    *gr_db = if active { gr } else { 0.0 };
    write_scope(ctx, *device_id, bus_l, bus_r, n);
}

/// この buffer で効く値。store が空なら静的な値そのまま (§8.8-5: 解決を省く)。
fn resolve_values(ctx: &ProgramCtx<'_>, dev: &NativeDevice, owner: u32) -> NativeDevice {
    let store = ctx.owner_store();
    let Some(song) = ctx.song else { return *dev };
    if store.lanes.is_empty() && store.routings.is_empty() {
        return *dev;
    }
    crate::automation::resolve_native_device(
        &song.clip_contents,
        store,
        dev,
        owner,
        ctx.rows,
        ctx.playhead_beats,
        ctx.recording_lanes,
        ctx.mod_plane,
    )
}

/// device scope の対象なら、この device の最終出力 (bypass 中は素通しの音) を書く。
fn write_scope(ctx: &ProgramCtx<'_>, device_id: u64, l: &[f32], r: &[f32], n: usize) {
    let Some(tap) = ctx.native.scopes else { return };
    for (k, id) in tap.watch.iter().enumerate() {
        if *id == device_id {
            tap.bridge.write_block(k, &l[..n], &r[..n]);
        }
    }
}

/// SC Listen の置換 (K25d)。この buffer で検出信号を書いた Comp があれば、それでバス
/// (= トラックのチェーン出力、PostFx 点) を置き換える。呼ぶのは PostFx 点の 3 か所
/// (leaf の pre-fader snapshot の前 / group の同じ位置 / master の fx chain の後)。
pub fn apply_listen_override(program: &mut ChainProgram, bus_l: &mut [f32], bus_r: &mut [f32], n: usize) {
    let Some(slot) = program.listen_pending.take() else { return };
    let Some(buf) = program.natives.get(slot as usize).and_then(|ns| ns.listen.as_ref()) else {
        return;
    };
    let n = n.min(bus_l.len()).min(bus_r.len()).min(buf.l.len());
    bus_l[..n].copy_from_slice(&buf.l[..n]);
    bus_r[..n].copy_from_slice(&buf.r[..n]);
}

/// `NodeOp::NativeSidechainTap` の staging の書き込み: `l` / `r` を内蔵 device の SC 受け皿へ写す
/// (読み元の借用の分け方は `graph::step::stage_native_sidechain`)。受け皿が無ければ何もしない。
pub(crate) fn stage_into(ns: Option<&mut NativeScratch>, l: &[f32], r: &[f32], n: usize) {
    if let Some(stage) = ns.and_then(|ns| ns.sc.as_mut()) {
        stage.stage(l, r, n);
    }
}

#[cfg(test)]
mod tests;
