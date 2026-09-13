//! チェーン上のデバイス (plugin / 内蔵映像 FX / Parallel / chain) の操作。
//!
//! [`AppEvent::Device`](crate::event::AppEvent::Device) が包む [`DeviceEvent`] 1 本に集約する
//! (`LauncherEvent` / `TabEvent` と同じ「1 arm = 1 サブ enum」)。 `AppData::handle_event` の
//! 巨大 match は実コード行 budget (不変条件 9) の天井に張り付いているので、デバイス系の
//! arm をここへ寄せて 1 arm にしてある。 処理は
//! [`AppData::handle_device_event`](crate::state::AppData::handle_device_event)。
//!
//! r.md #71 (プラグインのコピー / 移動): device を運ぶイベントは **すべて安定 `device_id`**
//! でアドレスする。 positional index だと、 イベント発行と消費の間にチェーンが変わりうる
//! (移動 / 削除) 場面で別 device に効く。

use common::model::{ChainRef, NativeKind, RackPanelKey, Split, TapPoint, TapSource};

use crate::device_addr::{InsertAt, RelocateDevices};
use crate::event_native::{MasterLimiterEdit, NativeEdit};
use crate::handler::parallel::{ChainMixerEdit, ParallelMixerEdit};
use crate::widgets::select_modifier::SelectModifier;

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEvent {
    ToggleSlotGui { device_id: u64 },
    /// 内蔵映像 FX の param 調整パネルから 1 param を編集。
    /// `value_real` は表示の実レンジ値 → lane の保存値 (0..=1) へ逆写像して格納。
    SetVideoFxParam { device_id: u64, param_id: u32, value_real: f32 },
    /// 埋め込み GUI を持たない plugin の「⚙」インライン param パネルで
    /// param を 1 つ編集。 `value_real` は表示の実レンジ値 → host が送った
    /// `PluginParamInfo` の min/max で lane `default_value` (0..=1) へ逆写像。
    /// scrubable の per-frame 発火なので **非 undoable** (`BeginInspectorScrub`
    /// で 1 undo step に bracket)。
    SetPluginParam { device_id: u64, param_id: u32, value_real: f64 },
    /// inspector の x ボタン / Delete / 右クリックメニュー: 選んだ device を
    /// chain から削除する。 複数選択を **1 件にまとめて** 運ぶ (id ごとに送ると
    /// undo が N ステップに割れる)。
    RemoveDevices { device_ids: Vec<u64> },
    /// r.md #71 (プラグインのコピー / 移動): 選んだ device を別のチェーンへ運ぶ。
    /// 既定は移動 (instance を作り直さない = 音が切れない)、 `copy` で複製。
    RelocateDevices(RelocateDevices),
    /// r.md #71: インスペクタのチェーン行を選択する (無修飾 / Ctrl / Shift)。
    SelectDevice {
        device_id: u64,
        modifier: SelectModifier,
    },
    /// inspector 「読み込み失敗」 セクションの「再読込」 ボタン: ロードに
    /// 失敗した device を、 保存済み state 込みで plugin_host に load し直す。
    /// 自動リトライはしない (恒常的失敗で無限ループになる) ので、 再試行の
    /// トリガーは常にこのユーザー操作。 Song は変えない (= 非 undoable)。
    ReloadDevice { device_id: u64 },
    /// PR4 sidechain: wire / unwire the sidechain source for a plugin's
    /// aux input port. `device_id` identifies the plugin instance;
    /// `port` selects the aux input port on that plugin
    /// (0 = first sidechain bus); `source` is `Some(track_id)` to wire
    /// from a track, or `None` to disconnect.
    SetSidechainSource {
        device_id: u64,
        port: u8,
        /// r.md #110: 他 track か同 track の Parallel 内 chain。 `None` = 切断。
        source: Option<TapSource>,
    },
    /// r.md #36: このプラグインのエディタ窓で **キーを一切横取りしない** (= REAPER の
    /// 「Send all keyboard input to plug-in」)。 消化の有無を外に出さない自前描画 GUI
    /// (Dear ImGui / GLFW 系) 用の逃げ道。 値は project に保存される。
    SetPluginSendAllKeys {
        device_id: u64,
        enabled: bool,
    },
    /// r.md #105: device 群を **信号経路から外す / 戻す** (Live の device off)。
    /// engine は bypass 中の device を dispatch せず音声も MIDI も素通し、 映像 FX は
    /// 解決から外れる。 `Q` (選択 device or カーソル直下のチェーン行) と チェーン行の
    /// 右クリックメニュー「無効化 / 有効化」 から。 値は project に保存され undo 対象。
    SetDevicesBypassed {
        device_ids: Vec<u64>,
        bypassed: bool,
    },
    /// パラアウト (docs/plan_paraout.md): one-click "explode" — auto-create a
    /// child track per `is_main=false` output port of the plugin `device_id`,
    /// group them under the source track, and wire
    /// each aux output to its new child. The source track becomes a
    /// group-with-instrument bus (its own main + the children sum through its
    /// FX/fader). Idempotent: ports already routed to a live track are kept.
    ExplodeParallelOut {
        device_id: u64,
    },
    /// パラアウト: route a single aux output port to a destination track (or
    /// `None` = unrouted = silent). Used by the inspector's per-port dropdown
    /// for re-adjustment after (or instead of) explode.
    SetParallelOutputRoute {
        device_id: u64,
        port: u8,
        dest: Option<u32>,
    },
    /// flip an aux-input route's tap point (sidechain plugin input).
    SetAuxInputTapPoint {
        device_id: u64,
        port: u8,
        tap_point: TapPoint,
    },
    // -------- r.md #110 Parallel (`docs/plan_parallel.md` §6.3) ------------------
    /// 空の Parallel (chain 1 本) を `chain` の `at` に挿す。
    AddParallel { chain: ChainRef, at: InsertAt },
    /// Group (Live の Ctrl+G): 選んだ device を 1 本の chain に入れた Parallel で包む。
    GroupDevices { device_ids: Vec<u64> },
    /// Ungroup: Parallel を全 chain の device の直列連結に置換。
    UngroupParallel { parallel_id: u64 },
    AddParallelChain { parallel_id: u64 },
    DuplicateParallelChain { chain_id: u64 },
    RenameParallelChain { chain_id: u64, name: String },
    RenameParallel { parallel_id: u64, name: String },
    SetParallelChainColor { chain_id: u64, color: Option<[f32; 3]> },
    /// Parallel 自体の色 (括弧の帯)。
    SetParallelColor { parallel_id: u64, color: Option<[f32; 3]> },
    /// chain の gain / pan / mute / solo (Song 書き換え + 値のみ IPC)。
    SetChainMixer { chain_id: u64, edit: ChainMixerEdit },
    /// Parallel の出力 trim / gain match (Song 書き換え + 値のみ IPC)。
    SetParallelMixer { parallel_id: u64, edit: ParallelMixerEdit },
    /// r.md #112: Parallel の入力の配り方 (帯域分割など) を切り替える (構造変更、 chain を補完)。
    SetParallelSplit { parallel_id: u64, split: Split },
    /// 見方の都合: Parallel / chain の中身の開閉 (undo 対象外)。 `id` は Parallel か chain。
    ToggleParallelNodeCollapsed { id: u64 },
    // -------- r.md #129 内蔵 device (`docs/plan_rack_native_devices.md` §9.1) --------
    /// picker の内蔵 4 種。chain の既定位置 (Q6) へ追加分として挿す。`open_panel` = Shift なしなら true。
    AddNative { chain: ChainRef, kind: NativeKind, open_panel: bool },
    /// 組み込み・追加分共通の値編集。Song 編集 + 値 IPC + 自動 ON (`NativeEdit::apply`)。
    NativeEdit { device_id: u64, edit: NativeEdit },
    /// master の固定 Limiter (チェーン外)。
    MasterLimiterEdit(MasterLimiterEdit),
    /// SC Listen (聴き方の都合、Song に書かない。bypass 中の Comp を有効化するのだけ Song 編集)。
    SetScListen { device_id: Option<u64> },
    /// Par の開閉 (見方の都合、Song に書かない)。
    ToggleRackPanel(RackPanelKey),
}

impl DeviceEvent {
    /// r.md #29: この event が undo step を積んだときの履歴ラベル。
    /// `AppEvent::undo_label` から委譲される。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        use DeviceEvent as E;
        match self {
            E::RemoveDevices { .. } => "デバイス削除",
            E::RelocateDevices(req) => {
                if req.copy {
                    "デバイスコピー"
                } else {
                    "デバイス移動"
                }
            }
            E::AddParallel { .. } => "Parallel 追加",
            E::GroupDevices { .. } => "Parallel にまとめる",
            E::UngroupParallel { .. } => "Parallel を解除",
            E::AddParallelChain { .. } => "chain 追加",
            E::DuplicateParallelChain { .. } => "chain 複製",
            E::RenameParallelChain { .. } | E::RenameParallel { .. } => "名前変更",
            E::SetParallelChainColor { .. } => "chain の色",
            E::SetParallelColor { .. } => "Parallel の色",
            E::SetChainMixer { edit: ChainMixerEdit::Gain(_), .. } => "chain gain",
            E::SetChainMixer { edit: ChainMixerEdit::Pan(_), .. } => "chain pan",
            E::SetChainMixer { edit: ChainMixerEdit::Muted(_), .. } => "chain mute",
            E::SetChainMixer { edit: ChainMixerEdit::Solo(_), .. } => "chain solo",
            E::SetParallelMixer { edit: ParallelMixerEdit::OutGain(_), .. } => "Parallel 出力",
            E::SetParallelMixer { edit: ParallelMixerEdit::GainMatch(_), .. } => "Parallel gain match",
            E::SetParallelMixer { edit: ParallelMixerEdit::SplitFreq { .. }, .. } => "クロスオーバー周波数",
            E::SetParallelMixer { edit: ParallelMixerEdit::ActiveChain(_), .. } => "Selector のアクティブ chain",
            E::SetParallelMixer { edit: ParallelMixerEdit::SelectorFade(_), .. } => "Selector のフェード時間",
            E::SetParallelSplit { .. } => "Parallel の分割",
            E::SetVideoFxParam { .. } => "映像FX変更",
            E::SetPluginParam { .. } => "プラグインパラメータ変更",
            E::SetSidechainSource { .. } | E::SetAuxInputTapPoint { .. } => "サイドチェイン設定",
            E::SetPluginSendAllKeys { .. } => "プラグインへのキー送出設定",
            E::SetDevicesBypassed { bypassed: true, .. } => "デバイスを無効化",
            E::SetDevicesBypassed { bypassed: false, .. } => "デバイスを有効化",
            E::ExplodeParallelOut { .. } => "パラアウト展開",
            E::SetParallelOutputRoute { .. } => "パラアウト経路変更",
            E::AddNative { kind, .. } => match kind {
                NativeKind::Comp => "Comp 追加",
                NativeKind::Eq => "EQ 追加",
                NativeKind::BusComp => "Bus Comp 追加",
                NativeKind::ToneEq => "Tone EQ 追加",
            },
            E::NativeEdit { edit, .. } => edit.undo_label(),
            E::MasterLimiterEdit(_) => "マスターリミッター変更",
            // Listen 自体は Song に書かない。snapshot が積まれるのは bypass 中の Comp を有効化したときだけ。
            E::SetScListen { .. } => "デバイスを有効化",
            // 非編集 (GUI 窓 / 選択 / 再読込 / 開閉) は snapshot を積まないので
            // ラベルは記録されない (`AppEvent::undo_label` の既定と同じ名前)。
            E::ToggleSlotGui { .. }
            | E::SelectDevice { .. }
            | E::ReloadDevice { .. }
            | E::ToggleParallelNodeCollapsed { .. }
            | E::ToggleRackPanel(_) => "編集",
        }
    }
}
