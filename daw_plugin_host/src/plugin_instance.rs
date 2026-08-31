//! Format-agnostic plugin interfaces (`docs/plan_arch_refactor.md` §6).
//!
//! Split-half design (clack 方式): 1 つのロード済み plugin は 2 つの Rust
//! オブジェクトで表現される。
//!
//! - [`LoadedPlugin`] — **main half**。plugin-main thread が所有する
//!   `Box<dyn LoadedPlugin>`。lifecycle (activate / deactivate /
//!   start・stop_processing)、state save/load、GUI、ARA、param 列挙など
//!   main-thread API を持つ。
//! - [`AudioProcessorHalf`] — **audio half**。`process()` が触る状態
//!   (入出力 planar buffer、event scratch、param cache、collected out
//!   events) だけを持つ別 heap allocation。worker pool の registry には
//!   こちら ([`AudioHalf`] = `Arc<UnsafeCell<Box<dyn AudioProcessorHalf>>>`
//!   相当) を渡す。
//!
//! これにより「worker が `&mut *raw` で process() 実行中に、plugin-main が
//! 同一オブジェクトへ `&mut` / `&` を発行する」旧構造の aliasing UB が型で
//! 消える: 並行に走り得る main-thread 呼び出し (state_save / ARA notify /
//! GUI / set_note_metadata) は main half のフィールドしか触らず、worker は
//! audio half のフィールドしか触らない。両 half は生の FFI ポインタ
//! (CLAP `*const clap_plugin` / VST3 `ComPtr`) を共有するが、その先の状態は
//! CLAP / VST3 仕様の thread partitioning (main-thread API vs audio-thread
//! API) が分離を保証する (Rust の aliasing model の外)。
//!
//! # `AudioHalf` の動的排他契約
//!
//! audio half への `&mut` は 2 経路からしか作られない:
//!
//! 1. **worker** — registry で entry を resolve した dispatch-critical
//!    section 内 (`DispatchCounter::enter`/`exit` で囲まれる)。
//! 2. **plugin-main** — その entry を registry から外し
//!    (`registry_remove`) `WorkerPool::quiesce` を済ませた *quiesced
//!    window* 内 (activate のバッファ再確保 / start・stop の gate 更新)。
//!
//! 両者は quiesce プロトコルで動的に直列化される (process_server.rs の
//! module docs 参照)。`AudioHalf::get` はこの契約を `unsafe fn` の
//! Safety 節として要求する。

use std::cell::UnsafeCell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::Result;

use common::plugin_format::PluginFormat;
use common::plugin_metadata::{NoteMetadata, TalkMetadata};
use common::protocol::{PluginParamInfo, RenderMode};

use crate::builtin;
use crate::clap_plugin::ClapPlugin;
use crate::vst3_plugin::Vst3Plugin;

/// One MIDI-style transition pushed into the next `process()` call.
///
/// `note_id` is the **stable per-note identifier** used to look up
/// `NoteMetadata` (= 歌詞 / phoneme) and per-note synthesis cache in
/// builtin plugins. CLAP / VST3 backends map it onto the formats' note-id
/// fields.
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { note_id: u32, key: u8, velocity: f64 },
    Off { note_id: u32, key: u8 },
}

/// A note transition scheduled at a specific frame offset inside the next
/// process buffer.
#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Whether a [`TimedParamEvent`] carries an absolute value or a normalized
/// modulation offset (`docs/plan_modulation_routing_redesign.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParamEventKind {
    /// Absolute parameter value (automation / direct set).
    #[default]
    Value,
    /// Normalized (`-1..=1`) modulation offset. CLAP modulatable params get a
    /// non-destructive `clap_event_param_mod`; VST3 / non-modulatable params
    /// have it folded into the absolute value the host sends.
    Mod,
}

/// One plugin-parameter automation event. `time` is the buffer-relative
/// sample offset, `param_id` is CLAP `clap_id` / VST3 `ParamID` (both u32).
#[derive(Debug, Clone, Copy)]
pub struct TimedParamEvent {
    pub time: u32,
    pub param_id: u32,
    pub value: f64,
    pub kind: ParamEventKind,
}

/// PR4 sidechain: one aux input port worth of buffers handed to
/// [`AudioProcessorHalf::process`]. Stereo only.
#[derive(Clone, Copy)]
pub struct AuxInputBuf<'a> {
    /// Whether the audio engine wrote real audio into `l` / `r` this buffer.
    pub active: bool,
    pub l: &'a [f32],
    pub r: &'a [f32],
}

/// Host callbacks plugins may trigger on *any* thread (usually the
/// plugin's GUI thread). Implementations must be `Send + Sync` and must
/// not block the caller — plugins often hold an internal lock across these.
///
/// v29: すべての callback は load 時に **安定 device id** を capture した
/// closure として `main.rs::make_callbacks(device_id)` が生成する。旧
/// `(track, index)` capture は削除 / 並べ替えで stale になり「別デバイスの
/// GUI を destroy する」class のバグ源だった。
#[derive(Clone)]
pub struct HostCallbacks {
    pub on_request_resize: Arc<dyn Fn(u32, u32) + Send + Sync>,
    pub on_closed: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_gui.request_show` / `request_hide`.
    pub on_request_show: Arc<dyn Fn() + Send + Sync>,
    pub on_request_hide: Arc<dyn Fn() + Send + Sync>,
    /// VST3 `IComponentHandler::restartComponent(flags)`.
    pub on_restart_component: Arc<dyn Fn(i32) + Send + Sync>,
    /// CLAP `clap_host.request_restart` — deactivate → activate の全 reinit
    /// 要求。plugin-main の quiesced-reinit 経路 (per-plugin cooldown 付き)
    /// へ配線される。
    pub on_request_restart: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host.request_callback` — plugin-main thread で
    /// [`LoadedPlugin::on_main_thread`] を 1 回呼ぶ要求 (JUCE 系の
    /// main-thread task 駆動)。
    pub on_request_callback: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_latency.changed` — latency 再 query +
    /// `PluginLatencyChanged` 再 emit の要求。
    pub on_latency_changed: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_params.rescan` — param 一覧再送 (`PluginParamList`)
    /// の要求。
    pub on_params_rescan: Arc<dyn Fn() + Send + Sync>,
    /// VST3 only: plugin GUI param gesture begin (`beginEdit`).
    pub on_param_gesture_begin: Arc<dyn Fn(u32) + Send + Sync>,
    /// VST3 only: plugin GUI param value change (`performEdit`).
    pub on_param_value: Arc<dyn Fn(u32, f64) + Send + Sync>,
    /// VST3 only: plugin GUI param gesture end (`endEdit`).
    pub on_param_gesture_end: Arc<dyn Fn(u32) + Send + Sync>,
    /// builtin VOICEVOX の合成状態 + 進捗の報告。旧「第 2 の
    /// callback 登録機構」(`set_voicevox_status_reporter`) を廃止して
    /// ここに統合 (`docs/plan_arch_refactor.md` §6)。synth thread が任意
    /// スレッドから呼ぶ。payload は `common::protocol::VocalSynthProgress`
    /// (busy / 失敗種別 / 残フレーズ数 / クリップ帰属) で、**IPC の payload と同じ型**。
    pub on_vocal_synth_status:
        Arc<dyn Fn(common::protocol::VocalSynthProgress) + Send + Sync>,
    /// r.md #65: このインスタンスのエディタ**コンテナ窓の HWND** (`0` = 未 open)。
    ///
    /// **`on_request_resize` (非同期 channel) では VST3 の契約を満たせない**ので置く。
    /// `iplugview.h` の "Sizing of a view" は「`IPlugFrame::resizeView` の後、
    /// **同じコールスタックで** ホストが窓をリサイズして `IPlugView::onSize` を呼ぶ」
    /// と規定していて、次周回に回すと `getSize` が旧サイズを返し続ける。実測では
    /// Renoise Redux がこれを見て **自分の view をコンテナから切り離し WS_POPUP の
    /// owned top-level に変える** (2026-08-22、`--editor-selftest` で確認)。
    ///
    /// 書き込むのは `gui_set_parent_hwnd` / `gui_destroy` の 1 対だけ (= 「今どの窓に
    /// attach しているか」がそのまま値になる = SSoT)。読むのは `Vst3PlugFrame` /
    /// CLAP `Host` の resize callback。窓の所有は plugin-main の `EditorWindow` のまま。
    pub editor_hwnd: Arc<AtomicU64>,
}

/// `canResize` / `can_resize` の問い合わせ結果**と、その根拠** (r.md #65)。
///
/// 生の戻り値を捨てて `bool` だけ返すと、ログには「false と判定した」しか残らず
/// **「プラグインが本当に不可と答えた」のか「そもそも問い合わせられなかった」のか**が
/// 区別できない。実際 Renoise Redux は REAPER では枠リサイズできるのに daw_01 では
/// `resizable=false` になっており、どちらなのかログから判定できなかった。
#[derive(Debug, Clone, Copy)]
pub struct ResizableProbe {
    /// ホストの判定 (窓スタイルに使う値)。
    pub verdict: bool,
    /// プラグインへ実際に問い合わせられたか。`false` = view / gui 拡張が無く
    /// 呼べていない (= `verdict` は「不可」ではなく「不明」の既定値)。
    pub queried: bool,
    /// 生の戻り値。VST3 は `tresult` (`kResultTrue == kResultOk == 0`)、
    /// CLAP は `bool` を `0` / `1` で入れる。`queried == false` なら無意味。
    pub raw: i32,
    /// **その format の仕様が、窓枠ドラッグを `verdict` の前提条件として
    /// 規定しているか。**
    ///
    /// 一次情報で 2 フォーマットの扱いが違う (2026-08-22 調査):
    /// - **CLAP は規定している** (`clap/ext/gui.h` L41-45):
    ///   *"Resizing the window (drag, if embedded): 1. **Only possible if**
    ///   clap_plugin_gui->can_resize() returns true"* → `true`
    /// - **VST3 は規定していない** (`iplugview.h` L102-124)。`canResize` が
    ///   `kResultTrue` のときホストがどうするかの**記述**があるだけで、
    ///   `kResultFalse` のときの禁止 (must not / shall not) はヘッダのどこにも無い。
    ///   同じ段落がホストに対して *"has to call IPlugView::onSize ()"* と義務語を
    ///   使い分けている以上、書かれていないのは未規定であって禁止ではない。
    ///   Steinberg 自身の適合性検査 (host-checker) も `canResize` は呼び出しの
    ///   有無を INFO ログするだけで、false の view をリサイズさせるホストを
    ///   エラーにしない → `false`
    pub drag_requires_verdict: bool,
}

impl ResizableProbe {
    /// 問い合わせられなかった (view / gui 拡張が無い)。
    /// 仕様の前提条件は保守的に「有り」として扱う (未知のときに枠を出さない)。
    pub const fn unavailable() -> Self {
        Self { verdict: false, queried: false, raw: 0, drag_requires_verdict: true }
    }
}

/// r.md #65: プラグインの申告と format の仕様から、**窓枠でのリサイズを許すか**を決める。
///
/// 方針をここ 1 箇所に閉じ込める。format ごとに答えが違うのは気分ではなく、
/// **一次情報の規範性が実際に違う**から ([`ResizableProbe::drag_requires_verdict`]):
///
/// - **CLAP**: ヘッダが「drag は `can_resize()` が true のときだけ可能」と前提条件を
///   明示している。申告を尊重する。
/// - **VST3**: 禁止規定が無い。Renoise Redux は `canResize()` に `kResultFalse` を
///   返すのに **REAPER では窓枠でリサイズでき UI も追従する** (ユーザーの実機確認)。
///   申告を尊重すると「ユーザーが実際にできるはずのことができない窓」になるので、
///   枠を出して `checkSizeConstraint` に丸めさせる (同 API が
///   *"if not adjust the rect to the allowed size"* とまさにその用途で規定されている)。
///
/// **これは多数派の選択ではない**: VST3 SDK の editorhost / JUCG / Ardour / ossia score は
/// いずれも申告を尊重して枠を出さない (Qtractor は枠を出すがプラグインへ伝えない、
/// Carla は `canResize` を見ない)。「枠を出して追従もさせる」OSS ホストは見つかっていない。
/// spec 違反ではないが慣習からは外れる選択で、`onSize` に追従しないプラグインでは
/// 枠だけ伸びて中身が残るリスクをホストが引き受ける。ユーザーの要件
/// (「Redux でリサイズしたい」/ 参照 DAW が REAPER) を優先した判断。
///
/// **申告値は捨てていない** — `ResizableProbe` はログに残るので、将来
/// 「VST3 でも申告を尊重する」設定を足すならこの関数だけを分岐させればよい。
#[must_use]
pub fn should_offer_resize_frame(probe: &ResizableProbe) -> bool {
    if probe.drag_requires_verdict {
        probe.verdict
    } else {
        true
    }
}

/// CLAP `clap_gui_resize_hints` 相当 (`clap/ext/gui.h` L91-103)。VST3 に対応 API は
/// 無いので `None` を返す。
///
/// `preserve_aspect_ratio` は **両軸ともリサイズ可のときだけ**意味を持ち、`false` の
/// ときは 2 つの ratio 値は未使用 (= 読んではいけない) — ヘッダのコメントどおり。
#[derive(Debug, Clone, Copy)]
pub struct ResizeHints {
    pub can_resize_horizontally: bool,
    pub can_resize_vertically: bool,
    pub preserve_aspect_ratio: bool,
    pub aspect_ratio_width: u32,
    pub aspect_ratio_height: u32,
}

/// エディタ窓の WNDPROC が **同じコールスタックで** プラグインへ問い合わせる口
/// (r.md #65)。
///
/// なぜ必要か: ホスト起点 (ユーザーのドラッグ) もプラグイン起点 (`resizeView` /
/// `request_resize`) も、両フォーマットが **同期**のシーケンスを規定している
/// (`iplugview.h` "Sizing of a view" / `clap/ext/gui.h` L35-45)。窓メッセージを
/// channel 経由で plugin-main の次周回へ回すと live resize が 1 周期遅れ、
/// modal size ループ中は永久に遅れる。
///
/// # 所有と生存
///
/// 実装は **borrowed な FFI ポインタしか持たない**。view / plugin instance の所有は
/// [`LoadedPlugin`] 側にあり (SSoT — `ComPtr` を二重に AddRef して WNDPROC 側にも
/// 持たせると `gui_destroy` の `removed()` と競合して UAF になる)。
/// `gui_destroy` は **先頭で** `alive` を落とす契約で、以後 [`Self::is_alive`] が
/// `false` を返し WNDPROC は一切 FFI を呼ばない。`pump_pending_messages` 由来の
/// nested dispatch で `gui_destroy` 実行中に WM_SIZE が再入しても、この 1 点で塞がる。
pub trait EditorSizer: Send {
    /// ユーザーのドラッグ矩形 (client px) を、プラグインが受け入れるサイズへ矯正する。
    /// VST3 `IPlugView::checkSizeConstraint` / CLAP `clap_plugin_gui.adjust_size`。
    /// 矯正できなければ入力をそのまま返す。**ホスト起点ドラッグ専用** —
    /// プラグイン起点の resize には掛けない (どちらのシーケンス図にも出てこない)。
    fn constrain_client_size(&self, w: u32, h: u32) -> (u32, u32);
    /// **プラグインの view が今名乗っているサイズ** (VST3 `getSize` /
    /// CLAP `get_size`)。**コンテナ窓の client 領域ではない。**
    ///
    /// 用途は 1 つだけ: 「プラグインへ `onSize` / `set_size` を通知する必要があるか」
    /// (= 既に同じサイズを知っているなら通知しない) の判定。**窓を動かすかどうかの
    /// 判定に使ってはいけない** — VST3 spec は *"if the host calls
    /// IPlugView::getSize () before calling IPlugView::onSize (), it will get the
    /// current (old) size not the wanted one!!"* と規定しており、
    /// 規定どおりなら古い値、規定に反して先に自分を更新するプラグイン
    /// (Renoise Redux) なら新しい値が返る。**どちらにせよ窓の実寸とは無関係**。
    ///
    /// 窓の寸法が要るときは `GetClientRect` を見ること
    /// (r.md #65: ここを取り違えて、コンテナが 880x162 のまま 1538x736 の要求に
    /// 「既にそのサイズ」と答え、窓を 1px も動かさず成功を返していた)。
    fn plugin_view_size(&self) -> Option<(u32, u32)>;
    /// 確定した client サイズを通知する (VST3 `onSize` / CLAP `set_size`)。
    fn notify_client_size(&self, w: u32, h: u32);
    /// ユーザーが窓枠でリサイズしてよいか (VST3 `canResize` / CLAP `can_resize`)。
    /// **生の戻り値ごと**返す ([`ResizableProbe`]) — 「false だった」ではなく
    /// 「なぜ false になったか」をログに残せるようにするため。
    fn can_resize(&self) -> ResizableProbe;
    /// CLAP `get_resize_hints` 相当。VST3 は `None`。
    fn resize_hints(&self) -> Option<ResizeHints>;
    /// `gui_destroy` 済みなら `false`。`false` の間は他のメソッドを呼んではいけない。
    fn is_alive(&self) -> bool;
}

impl HostCallbacks {
    pub fn noop() -> Self {
        Self {
            on_request_resize: Arc::new(|_, _| {}),
            on_closed: Arc::new(|| {}),
            on_request_show: Arc::new(|| {}),
            on_request_hide: Arc::new(|| {}),
            on_restart_component: Arc::new(|_| {}),
            on_request_restart: Arc::new(|| {}),
            on_request_callback: Arc::new(|| {}),
            on_latency_changed: Arc::new(|| {}),
            on_params_rescan: Arc::new(|| {}),
            on_param_gesture_begin: Arc::new(|_| {}),
            on_param_value: Arc::new(|_, _| {}),
            on_param_gesture_end: Arc::new(|_| {}),
            on_vocal_synth_status: Arc::new(|_| {}),
            editor_hwnd: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Per-buffer transport snapshot fed into [`AudioProcessorHalf::process`].
///
/// 曲の再生位置は `song_pos_beats` 一本 (= daw_audio が tempo automation を
/// 積分した累積拍位置)。r.md #87 以降はこれと別に**行の時間軸** (`row`) が
/// 載る — plugin が musical time として見るのはそちら。sample / seconds / bar 表現は
/// [`crate::process_scaffold::TransportBlock`] がここから導出する
/// (`ProcessData::steady_time` は engine が設定しない = 常に 0 なので
/// sample 由来の位置は運ばない)。
#[derive(Debug, Clone, Copy)]
pub struct TransportContext {
    pub bpm: f32,
    pub sample_rate: u32,
    /// 累積拍位置 (= daw_audio が tempo automation を積分した真の song 位置)。
    pub song_pos_beats: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub is_playing: bool,
    pub is_looping: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    /// r.md #87 (クリップランチャー): **この device が載っている行の時間軸**
    /// ([`common::process_data::RowTransport`])。ランチャーが行の主導権を
    /// 握っている間はセルの実効拍で、アレンジ主導の行では `song_pos_beats`
    /// と一致する。plugin へ渡す musical time はこちら
    /// ([`crate::process_scaffold::TransportBlock::derive`]) — `song_pos_beats`
    /// は「曲のどこか」を意味する用途のためにそのまま残してある。
    pub row: common::process_data::RowTransport,
    /// musical time を **曲全体の位置に固定**するか
    /// ([`crate::process_server::PluginEntry::transport_pinned_to_song`])。
    /// ARA を bind した instance だけ `true`。
    pub pin_to_song: bool,
}

impl TransportContext {
    /// Build from a `ProcessData` populated by daw_audio. `pin_to_song` comes
    /// from the registry entry (ARA-bound instances keep the song timeline).
    pub fn from_process_data(
        pd: &common::process_data::ProcessData,
        pin_to_song: bool,
    ) -> Self {
        Self {
            bpm: pd.bpm.max(1.0),
            sample_rate: pd.sample_rate.max(1),
            song_pos_beats: pd.song_pos_beats,
            tsig_num: pd.tsig_num.max(1),
            tsig_denom: pd.tsig_denom.max(1),
            is_playing: pd.playing != 0,
            is_looping: pd.looping != 0,
            loop_start_beats: pd.loop_start_beats,
            loop_end_beats: pd.loop_end_beats,
            row: pd.row,
            pin_to_song,
        }
    }

    /// plugin へ渡す musical timeline (r.md #87)。
    ///
    /// 既定はこの device が載っている **行**の時間軸。セルを鳴らしている行は
    /// 位置もループも**セルの窓** — 位置だけ差し替えて曲のループ区間を名乗ると、
    /// plugin から見て「拍がループの外を回っている」不整合になる。
    /// アレンジ主導の行と停止した行は従来どおり曲の位置 + 曲のループなので、
    /// ランチャーを使わない曲の transport は byte 単位で従来と同じ。
    ///
    /// ARA を bind した instance (`pin_to_song`) だけは常に曲の時間軸 — ARA の
    /// playback region は song 時間に固定で、行の拍を渡すと Melodyne が別の
    /// 位置を鳴らす (ARA のセル対応は未実装)。
    #[must_use]
    pub fn plugin_timeline(&self) -> PluginTimeline {
        if self.pin_to_song || self.row.is_arrangement() {
            return PluginTimeline {
                pos_beats: if self.pin_to_song { self.song_pos_beats } else { self.row.pos_beats },
                loop_start_beats: self.loop_start_beats,
                loop_end_beats: self.loop_end_beats,
                is_looping: self.is_looping,
            };
        }
        PluginTimeline {
            pos_beats: self.row.pos_beats,
            loop_start_beats: self.row.loop_start_beats,
            loop_end_beats: self.row.loop_end_beats,
            // ワンショットのセルはループを名乗らない (窓が 0/0)。
            is_looping: self.row.has_loop(),
        }
    }
}

/// [`TransportContext::plugin_timeline`] の結果 — plugin へ実際に渡る
/// 「どの時間軸のどこを鳴らしているか」。
#[derive(Debug, Clone, Copy)]
pub struct PluginTimeline {
    pub pos_beats: f64,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    /// user がループを有効にしているか (CLAP `IS_LOOPING` / VST3 `kCycleActive`
    /// の材料。区間が定義済みかは別途 `end > start` で判定する)。
    pub is_looping: bool,
}

// ====================================================================
// Audio half
// ====================================================================

/// Audio-thread half of a loaded plugin: **`process()` が触る状態の全て**
/// (入出力 planar buffer / event scratch / param cache / collected out
/// events)。worker registry にはこの trait object ([`AudioHalf`] 経由) を
/// 渡す。main half ([`LoadedPlugin`]) からは lifecycle hook (`on_activate`
/// / `on_deactivate` / `set_processing`) のみ、**quiesced window 内で**
/// 呼ばれる。
pub trait AudioProcessorHalf: Send {
    /// Runs one buffer. `events` / `param_events` must be sorted by
    /// ascending `time` (CLAP requirement, also honoured for VST3).
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        param_events: &[TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[AuxInputBuf<'_>],
        transport: &TransportContext,
    ) -> Result<i32>;

    /// Planar output. `None` means "no such channel".
    fn output_buffer(&self, channel: usize) -> Option<&[f32]>;

    /// パラアウト: planar aux output (port 0 = main bus when the plugin is
    /// multi-out). `None` = no such port / channel.
    fn aux_output_buffer(&self, _port: usize, _channel: usize) -> Option<&[f32]> {
        None
    }

    /// Moves MIDI-style events emitted during the previous `process()` into
    /// `out` (capacity-preserving drain).
    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>);

    /// Drain plugin-emitted PARAM_GESTURE_BEGIN param ids. Default no-op.
    fn drain_out_param_touches_into(&mut self, _out: &mut Vec<u32>) {}

    /// Drain plugin-emitted PARAM_VALUE `(param_id, value)`. Default no-op.
    fn drain_out_param_values_into(&mut self, _out: &mut Vec<(u32, f64)>) {}

    /// Drain plugin-emitted PARAM_GESTURE_END param ids. Default no-op.
    fn drain_out_param_releases_into(&mut self, _out: &mut Vec<u32>) {}

    // --- lifecycle hooks (plugin-main thread, quiesced window のみ) ------

    /// (Re)allocate the process buffers for the new activation params.
    /// Called by the main half right after the format-level activate
    /// succeeded, inside a quiesced window.
    fn on_activate(&mut self, _sample_rate: f64, _max_frames: u32) {}

    /// Free / clear activation-scoped buffers. Quiesced window のみ。
    fn on_deactivate(&mut self) {}

    /// Mirror of the main half's processing gate (defensive check inside
    /// `process()`). Quiesced window のみ。
    fn set_processing(&mut self, _on: bool) {}
}

/// Shared cell that owns the [`AudioProcessorHalf`] allocation. The worker
/// registry and the main half ([`LoadedPlugin::audio_half`]) each hold an
/// `Arc`; the allocation therefore outlives any stale registry snapshot
/// (no dangling `Box` reads), while *access* is serialized dynamically by
/// the quiesce protocol (module docs).
pub struct AudioHalf {
    inner: UnsafeCell<Box<dyn AudioProcessorHalf>>,
}

// SAFETY: the inner Box is only dereferenced through `get()`, whose
// contract requires dynamically exclusive access (registry dispatch window
// XOR quiesced window). `dyn AudioProcessorHalf` is `Send`.
unsafe impl Send for AudioHalf {}
unsafe impl Sync for AudioHalf {}

impl AudioHalf {
    pub fn new(inner: Box<dyn AudioProcessorHalf>) -> Arc<Self> {
        Arc::new(Self {
            inner: UnsafeCell::new(inner),
        })
    }

    /// # Safety
    ///
    /// The caller must hold dynamically exclusive access per the module-doc
    /// contract: either (a) a worker inside its dispatch-critical section
    /// with this entry resolved from the *current* registry snapshot, or
    /// (b) the plugin-main thread inside a quiesced window (entry removed
    /// from the registry + `WorkerPool::quiesce` completed) or before the
    /// entry was ever published.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut (dyn AudioProcessorHalf + 'static) {
        unsafe { &mut **self.inner.get() }
    }
}

// ====================================================================
// VOICEVOX capability (docs/plan_arch_refactor.md §6)
// ====================================================================

/// Vocal-synthesis capability implemented by builtin VOICEVOX. External
/// CLAP / VST3 plugins have no equivalent concept, so the capability is an
/// opt-in downcast ([`LoadedPlugin::as_vocal_synth`]) instead of a set of
/// default-no-op methods on every plugin.
pub trait VocalSynth {
    /// Per-note metadata flush (歌詞 + talk + 塊の長さ)。plugin-main thread から、
    /// GUI 側で歌詞 / phoneme が編集されるたびに呼ばれる。
    ///
    /// `chunk_secs` は `/sing_frame_audio_query` 1 回にまとめる長さ (秒)。
    /// **合成結果を変える入力**なので、フレーズ WAV のキャッシュキーにも混ざる。
    fn set_note_metadata(
        &mut self,
        bpm: f32,
        chunk_secs: f32,
        entries: &[NoteMetadata],
        talk: &[TalkMetadata],
    );

    /// 再生ヘッド優先ヒント。再生位置に近いフレーズから合成させる。
    /// **再合成はトリガしない** (順序だけを変える)。
    fn set_priority_beats(&mut self, playhead_beats: f64);

    /// `(queued_gen, done_gen, phrase_heartbeat)`。bounce / 書き出し前の合成完了待ち
    /// (`PrepareVocalSynth`) は `done >= queued` を待ち、**heartbeat が動いている間は
    /// 打ち切らない** (総時間で切ると長い曲が部分ミックスで書き出される)。
    fn synth_progress(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>);
}

/// 合成完了待ちの「停滞」判定 (r.md #75)。フレーズ 1 本の最大実測は 546 ms、engine の
/// コールドスタートを見ても 60 秒進捗が無ければ異常。**総時間では打ち切らない**
/// (5 分の曲の初回合成は実測 30 秒超で、旧 30 秒固定 deadline は部分ミックスを
/// 書き出していた)。
const SYNTH_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// [`VocalSynth::synth_progress`] の 3 つ組が「呼び出し時点の queued 世代」まで進むのを
/// 別スレッドで待ち、完了 (または停滞での打ち切り) で `VocalSynthReady` を `emit` へ渡す。
///
/// 戻り値は **スレッドを起こせたか**。`false` なら呼び出し側が即 ready を返すこと
/// (待ち手が居ないまま bounce / 書き出しを止めない)。
pub fn spawn_vocal_synth_wait(
    device_id: u64,
    progress: (Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>),
    emit: impl FnOnce(common::protocol::PluginEvent) + Send + 'static,
) -> bool {
    use std::sync::atomic::Ordering;
    let (queued, done, heartbeat) = progress;
    let target_gen = queued.load(Ordering::SeqCst);
    let spawn = std::thread::Builder::new()
        .name("voicevox-bounce-synth-wait".into())
        .spawn(move || {
            let sample = || (done.load(Ordering::SeqCst), heartbeat.load(Ordering::SeqCst));
            let mut last_seen = sample();
            let mut last_change = std::time::Instant::now();
            while done.load(Ordering::SeqCst) < target_gen {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let now = sample();
                if now != last_seen {
                    last_seen = now;
                    last_change = std::time::Instant::now();
                    continue;
                }
                if last_change.elapsed() > SYNTH_STALL_TIMEOUT {
                    tracing::warn!(
                        device_id,
                        target_gen,
                        done = now.0,
                        "vocal synth が 60 秒進捗しないため待機を打ち切る"
                    );
                    break;
                }
            }
            emit(common::protocol::PluginEvent::VocalSynthReady { device_id });
        });
    spawn.is_ok()
}

// ====================================================================
// Main half
// ====================================================================

/// The host-side main-thread handle to a loaded plugin. Lives on the
/// plugin-main thread. The audio thread never touches this object — it
/// works on the separate [`AudioProcessorHalf`] obtained at publish time
/// via [`Self::audio_half`].
#[allow(dead_code)] // `format()` is wired up for future UI display.
pub trait LoadedPlugin: Send {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn format(&self) -> PluginFormat;

    /// The audio half backing this instance (Arc clone). Published into the
    /// worker registry; also used by the main half's own lifecycle hooks.
    fn audio_half(&self) -> Arc<AudioHalf>;

    // --- lifecycle (plugin-main thread; quiesced window when live) -------
    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()>;
    fn deactivate(&mut self);
    fn start_processing(&mut self) -> Result<()>;
    fn stop_processing(&mut self);

    /// Clear the plugin's audio processing state (tails / voices) without
    /// touching parameters. CLAP forwards to `clap_plugin.reset()`; others
    /// default no-op. Quiesced window のみ。
    fn reset(&mut self) {}

    /// CLAP `clap_plugin.on_main_thread()` — plugin が `request_callback`
    /// で予約した main-thread task を 1 回実行する。他 format は no-op。
    fn on_main_thread(&mut self) {}

    // --- render-mode hint -------------------------------------------------
    /// Realtime / Offline hint. CLAP は `clap_plugin_render.set`、VST3 は
    /// per-buffer `ProcessData::processMode` 切替 (audio half と共有の
    /// atomic 経由)。
    fn set_render_mode(&mut self, mode: RenderMode) -> bool;

    /// PDC: query the plugin's reported processing latency in samples.
    /// Requires the plugin to be active.
    fn query_latency(&mut self) -> u32;

    /// Enumerate every parameter the plugin exposes (plugin-main thread).
    fn enumerate_params(&self) -> Vec<PluginParamInfo> {
        Vec::new()
    }

    // --- persistence (plugin-main thread) --------------------------------
    fn state_save(&self) -> Result<Option<Vec<u8>>>;
    fn state_load(&mut self, data: &[u8]) -> Result<()>;

    /// パラアウト: how many parallel-out ports this plugin declared.
    fn aux_output_port_count(&self) -> usize {
        0
    }

    /// VOICEVOX capability downcast. Default `None` (external plugins).
    fn as_vocal_synth(&mut self) -> Option<&mut dyn VocalSynth> {
        None
    }

    // --- ARA (r.md #5) ----------------------------------------------------
    /// If ARA-capable, create the document controller and bind the instance
    /// (before the first activate / state load / GUI, per ARA spec).
    fn bind_ara_if_capable(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Update the bound ARA document to expose `clips` (deactivate →
    /// set_clips → restore archive → reactivate は
    /// [`crate::ara::run_setup_ara`] に一本化)。
    fn setup_ara(
        &mut self,
        _clips: &[common::protocol::AraClipSpec],
        _bpm: f64,
        _time_sig: (u16, u16),
        _archive: Option<&[u8]>,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Update only the placement / stretch of existing ARA regions
    /// (safe while rendering — no deactivate).
    fn update_ara_regions(&self, _regions: &[common::protocol::AraRegionUpdate]) {}

    /// Tear down this instance's ARA session, if any.
    fn clear_ara(&mut self) {}

    /// Drive the bound ARA document's deferred work / analysis
    /// (plugin-main timer).
    fn notify_ara_model_updates(&self) {}

    /// Whether this instance currently holds a live ARA session.
    fn has_ara_session(&self) -> bool {
        false
    }

    /// Serialise this instance's ARA edit state for project save.
    fn store_ara_archive(&self) -> Option<Vec<u8>> {
        None
    }

    // --- embedded Win32 GUI (plugin-main thread) --------------------------
    fn gui_is_embed_supported(&self) -> bool;
    fn gui_create_embedded(&mut self) -> Result<()>;
    fn gui_get_size(&self) -> Option<(u32, u32)>;
    fn gui_set_scale(&self, scale: f64) -> Result<bool>;
    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()>;
    fn gui_show(&self) -> Result<bool>;
    fn gui_hide(&self) -> Result<()>;
    fn gui_destroy(&mut self);
    /// r.md #65: エディタ窓の WNDPROC がサイズ交渉に使う借用ハンドル
    /// ([`EditorSizer`])。`gui_create_embedded` 済みでないと `None`。
    /// builtin / VOICEVOX は GUI を持たないので常に `None`。
    ///
    /// 旧 `gui_can_resize` / `gui_set_size` はここへ吸収した: 「サイズ交渉」の
    /// FFI 面が trait と WNDPROC の 2 箇所に割れていると、ホスト起点とプラグイン
    /// 起点で別々の実装が育つ (実際に `checkSizeConstraint` を掛ける / 掛けないが
    /// 食い違っていた)。
    fn gui_sizer(&self) -> Option<Box<dyn EditorSizer>> {
        None
    }
}

/// Loads a plugin at `path` using the backend selected by `format`.
/// `plugin_id` narrows to a specific descriptor inside a multi-plugin
/// library; empty means "pick the first descriptor".
pub fn load_plugin(
    format: PluginFormat,
    path: &Path,
    plugin_id: &str,
    callbacks: HostCallbacks,
) -> Result<Box<dyn LoadedPlugin>> {
    match format {
        PluginFormat::Clap => {
            let plugin = ClapPlugin::load(path, plugin_id, callbacks)?;
            Ok(Box::new(plugin) as Box<dyn LoadedPlugin>)
        }
        PluginFormat::Vst3 => {
            let plugin = Vst3Plugin::load(path, plugin_id, callbacks)?;
            Ok(Box::new(plugin) as Box<dyn LoadedPlugin>)
        }
        PluginFormat::Builtin => {
            // `path` here is a `builtin://...` URI. `plugin_id` is unused —
            // the URI itself is the descriptor id.
            let _ = plugin_id;
            let uri = path.to_string_lossy();
            builtin::load_builtin(&uri, callbacks)
        }
    }
}

#[cfg(test)]
mod resize_frame_policy_tests {
    use super::{ResizableProbe, should_offer_resize_frame};

    /// r.md #65: **format ごとに一次情報の規範性が違う**ので、方針も分かれる。
    /// このテストはその判断そのものを固定する (将来変えるなら意識的に更新される)。
    #[test]
    fn frame_policy_follows_each_formats_spec() {
        let vst3 = |verdict| ResizableProbe {
            verdict,
            queried: true,
            raw: 0,
            drag_requires_verdict: false,
        };
        let clap = |verdict| ResizableProbe {
            verdict,
            queried: true,
            raw: 0,
            drag_requires_verdict: true,
        };

        // VST3: 禁止規定が無いので、申告が false でも枠を出す
        // (Renoise Redux は canResize=false なのに REAPER で枠リサイズできる)。
        assert!(should_offer_resize_frame(&vst3(true)));
        assert!(
            should_offer_resize_frame(&vst3(false)),
            "VST3 は canResize=false でも枠を出す (iplugview.h に禁止規定が無い)"
        );

        // CLAP: gui.h が「drag は can_resize()==true のときだけ可能」と
        // **前提条件として**規定しているので申告を尊重する。
        assert!(should_offer_resize_frame(&clap(true)));
        assert!(
            !should_offer_resize_frame(&clap(false)),
            "CLAP は can_resize()==false なら枠を出さない (gui.h L41-45 が前提条件を明示)"
        );

        // 問い合わせられなかったときは保守的に枠を出さない。
        assert!(!should_offer_resize_frame(&ResizableProbe::unavailable()));
    }
}
