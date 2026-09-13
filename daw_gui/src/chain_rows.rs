//! インスペクタの chain list (Rack) の **行モデル** — device ツリーを縦 1 列に flatten した
//! view-model 型。 flatten 本体は [`AppData::chain_rows`](crate::state::AppData::chain_rows)、
//! 描画と入力は `view::track_inspector::chain_list`。
//!
//! `app_types.rs` から分けてあるのは不変条件 9 (サイズ budget) のため。 呼び出し側は
//! `crate::app::*` 経由で今までどおり名前を引ける (`app_types.rs` が再輸出する)。

/// 単一デバイスチェーン上の 1 行 (`docs/plan_linear_chain.md` §5)。役割は持たず
/// (判定もしない)、安定 `device_id` (`PluginInstance::id`) でアドレスする。
/// 表示順は Vec の位置が持つ (r.md #71 プラグインのコピー / 移動:
/// positional index はイベントにも帳簿にも出さない)。表示は plugin 名のみ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    pub device_id: u64,
    pub plugin_name: String,
    /// チェーン行ボタンの分岐用。 埋め込み GUI (editor window) を持つ
    /// plugin か (`PluginParamList` の `has_embedded_gui`、 未受信は楽観的に true)。
    pub has_embedded_gui: bool,
    /// この device が内蔵映像 FX (= `ports.is_video()`) か。 映像 FX は専用の
    /// インライン param パネル (`open_video_fx_params`) を持つ。
    pub is_video: bool,
    /// この device が VOICEVOX builtin か (= 声選択パネルを出す対象)。
    pub is_voicevox: bool,
    /// host から param 一覧が届いていて 1 つ以上 param があるか (= 汎用 param
    /// パネルに出す中身がある)。
    pub has_params: bool,
    /// r.md #36: 「キーを全部プラグインに送る」 (= ホストがキーを一切横取りしない)。
    pub send_all_keys: bool,
    /// plugin_host での load が失敗した device の理由 (`SlotPluginLoadFailed`)。
    /// `Some` = song には居るが host には instance が無い = **無音**。
    /// チェーン行に警告色 + ⚠ を出し、 「読み込み失敗」 セクションで理由と
    /// 「再読込」 ボタンを出す (自動リトライはしない)。
    pub load_error: Option<String>,
    /// r.md #105: 信号経路から外れている (`PluginInstance::bypassed`)。 行の名前を
    /// dim 色で描く。 切替は `Q` / 右クリックメニュー (行にトグルは置かない)。
    pub bypassed: bool,
    /// r.md #110: host が報告した aux 入力 port 数。 1 以上の device にだけ `SC` を出す。
    pub aux_input_count: u8,
    /// r.md #110: sidechain が 1 port でも配線済み (`SC` ボタンの強調)。
    pub sc_wired: bool,
}

/// r.md #129 (`docs/plan_rack_native_devices.md` §10.5): 内蔵 device (`Device::Native`) の行の表示情報。
/// 値 (params) は持たない — 行ミニ表示と Par は描画時に Song から live 値を引く (複製しない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRowEntry {
    pub device_id: u64,
    pub kind: common::model::NativeKind,
    /// 組み込み (× を出さない / Parallel に入れない / 普通のドラッグで他トラックへ運べない)。
    pub builtin: bool,
    /// 行の名前 (`NativeDevice::display_name`: 「Comp」 / 「Comp 2」)。
    pub name: std::borrow::Cow<'static, str>,
    /// OFF (`NativeDevice::bypassed`)。 名前と小表示を薄く描く。
    pub bypassed: bool,
    /// 外部サイドチェインが配線済み (`SC▾` の強調)。 SC を受けない種類は常に `false`。
    pub sc_wired: bool,
}

impl NativeRowEntry {
    #[must_use]
    pub fn of(n: &common::model::NativeDevice) -> Self {
        Self {
            device_id: n.id,
            kind: n.kind(),
            builtin: n.builtin,
            name: n.display_name(),
            bypassed: n.bypassed,
            sc_wired: n.sidechain_input().is_some(),
        }
    }
}

/// r.md #110 (`docs/plan_parallel.md` §6.1): インスペクタの chain list の 1 行の種類。
/// device ツリーを「縦回転 Live 型」に flatten したもの。
#[derive(Debug, Clone, PartialEq)]
pub enum ChainRowKind {
    Plugin(ChainEntry),
    /// r.md #129: 内蔵 device (組み込み / 追加分)。
    Native(NativeRowEntry),
    /// `╭ Parallel名` (開始行)。 `open` = 中身 (chain 行 〜 終了行) を出しているか。
    ParallelBegin {
        parallel_id: u64,
        name: String,
        bypassed: bool,
        color: Option<[f32; 3]>,
        open: bool,
        /// 出力 trim (linear) と gain match (ヘッダ行の knob / Match トグル)。
        out_gain: f32,
        gain_match: bool,
        /// r.md #112: 入力の配り方 (ヘッダ行の dropdown)。
        split: common::model::Split,
    },
    /// r.md #112: Split の param 行 (ヘッダ行の直下、 `Split::None` 以外のときだけ)。
    /// `Frequency3` なら `Low [hz] Mid [hz] High`。
    SplitParams { parallel_id: u64, split: common::model::Split },
    /// Parallel の chain 1 本 (名前 / 色 / gain / pan / M / S)。
    Chain {
        parallel_id: u64,
        chain_id: u64,
        name: String,
        color: Option<[f32; 3]>,
        gain: f32,
        pan: f32,
        muted: bool,
        solo: bool,
        /// 中身 (device 行) を展開しているか。
        open: bool,
        /// preview 四角の数 (= chain 直下の device 数)。
        n_devices: usize,
        /// r.md #114: Selector で **非アクティブ** な chain (名前を薄く出す。 Selector 以外は常に
        /// `false`)。 表示だけで、 切替はヘッダ直下の `Active` 欄 (編集面は 1 つ)。
        inactive: bool,
    },
    /// `+ chain`。
    AddChain { parallel_id: u64 },
    /// `+ Plugin` (その chain の末尾へ)。
    AddPlugin { chain: common::model::ChainRef },
    /// `╰` (終了行)。
    ParallelEnd { parallel_id: u64, color: Option<[f32; 3]> },
}

/// chain list の 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct ChainRow {
    pub kind: ChainRowKind,
    /// この行が属する chain (ドロップ先 / 挿入先の解決に使う)。
    pub chain: common::model::ChainRef,
    /// `chain` 内での位置 (Plugin / ParallelBegin 行 = その device の index、 ParallelEnd = 直後、
    /// AddPlugin = 末尾)。 ドロップの挿入位置に使う。
    pub index: u32,
    /// ネスト深さ (top-level = 0)。
    pub depth: u32,
    /// 左端の色帯 (外側の Parallel から順に、 展開中 chain の色)。 深さぶんの本数。
    pub bars: Vec<Option<[f32; 3]>>,
    /// この行が Parallel の直接の行 (開始 / Split / chain / `+ chain` / 終了) なら、 その Parallel の
    /// 色。 描画は `bars` の次の位置 (= 行内容の左端、 chain 行の色見本と同じ x / 幅) に Parallel 色の
    /// 帯を通し、 開始行の `「` から終了行の `L` まで 1 本に繋げる (chain の帯はその上に乗る)。
    /// chain の中身の行は chain の帯が同じ x を占めるので `None`。
    pub parallel_band: Option<Option<[f32; 3]>>,
}

impl ChainRow {
    /// 選択集合 (`selected_device_ids`) に入る id (plugin / 内蔵 / Parallel / chain)。 操作行は `None`。
    pub fn select_id(&self) -> Option<u64> {
        match &self.kind {
            ChainRowKind::Plugin(e) => Some(e.device_id),
            ChainRowKind::Native(n) => Some(n.device_id),
            ChainRowKind::ParallelBegin { parallel_id, .. } => Some(*parallel_id),
            ChainRowKind::Chain { chain_id, .. } => Some(*chain_id),
            _ => None,
        }
    }

    /// この行を掴んだときに一緒に運ぶ device の id (Parallel 行は Parallel 1 つ = 中身ごと)。
    /// 組み込みも掴める (並べ替えは自由、 運べる先は `handler::device_guard` が絞る)。
    pub fn drag_id(&self) -> Option<u64> {
        match &self.kind {
            ChainRowKind::Plugin(e) => Some(e.device_id),
            ChainRowKind::Native(n) => Some(n.device_id),
            ChainRowKind::ParallelBegin { parallel_id, .. } => Some(*parallel_id),
            _ => None,
        }
    }

    /// この行の直下に開く Par の鍵 (Par を持たない行は `None`)。
    pub fn panel_key(&self) -> Option<common::model::RackPanelKey> {
        match &self.kind {
            ChainRowKind::Plugin(e) if e.shows_param_panel() => Some(common::model::RackPanelKey::Device(e.device_id)),
            ChainRowKind::Native(n) => Some(common::model::RackPanelKey::Device(n.device_id)),
            _ => None,
        }
    }

    /// この行に hover して `Q` を押したときの宛先 (plugin / 内蔵 / Parallel)。
    pub fn bypass_target(&self) -> Option<crate::handler::bypass_target::BypassTarget> {
        use crate::handler::bypass_target::BypassTarget;
        match &self.kind {
            ChainRowKind::Plugin(e) => Some(BypassTarget::Device(e.device_id)),
            ChainRowKind::Native(n) => Some(BypassTarget::Device(n.device_id)),
            ChainRowKind::ParallelBegin { parallel_id, .. } => Some(BypassTarget::Device(*parallel_id)),
            _ => None,
        }
    }
}

/// r.md #110: sidechain (aux 入力) 1 port の配線 (SC パネルの 1 行)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidechainPort {
    pub port: u8,
    pub source: Option<common::model::TapSource>,
    pub tap_point: common::model::TapPoint,
}

impl ChainEntry {
    /// チェーン行ボタンが「埋め込み GUI window を開く」 のではなく
    /// 「インライン param パネルをトグルする」 種類か。 映像 FX / VOICEVOX /
    /// 埋め込み GUI を持たないが param がある plugin が該当。
    pub fn shows_param_panel(&self) -> bool {
        self.is_video || self.is_voicevox || (!self.has_embedded_gui && self.has_params)
    }

    /// チェーン行にボタンを出すか。 GUI も param パネルも無い device
    /// (= Silence 等の no-op builtin) はボタンを出さない。
    pub fn shows_button(&self) -> bool {
        (self.has_embedded_gui && !self.is_video) || self.shows_param_panel()
    }
}
