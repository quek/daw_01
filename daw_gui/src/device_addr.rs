//! device の **宛先解決** と plugin host の load 差分 — 不変条件 1 (安定 id
//! addressing) の daw_gui 側の中枢。
//!
//! `app_types.rs` から分けてあるのは不変条件 9 (サイズ budget) のため。 r.md #71
//! (プラグインのコピー / 移動) で device 帳簿が positional `(track_id, index)` から
//! 安定 `device_id` (`PluginInstance::id`) 一本になり、 「その id はいまどこの
//! 持ち物か」 を引き直す口がこのファイルの責務として独立した。
//!
//! r.md #110 (Parallel): device は Parallel の中の chain にも居る。 「どの chain か」 は
//! [`ChainRef`] (`Track(id)` = top-level / `Chain(id)` = Parallel 内) で表し、 位置は
//! その chain 内 index。 **座標を保持しない** — 引きたくなったら都度
//! [`find_device_by_id`] で引き直す (保持すると削除 / 並べ替えで stale になり、
//! 貼り替え補償コードが生える = 不変条件 1 が禁じる形)。
use std::collections::HashMap;

use common::model::ChainRef;

/// 安定 `device_id` (plugin / Parallel) から **いまの** 所属を引き直す:
/// `(所有 track id, 居る chain 内の index)`。 master bus の device は
/// 所有 track = `MASTER_TRACK_ID`。 見つからなければ `None` (= 削除済み device への
/// stale event 等は呼び出し側で無視する)。 どの chain かは [`common::model::Song::find_device`]。
///
/// **返り値は保持しないこと。** これは「Song から毎回引き直す一時的な解決」で
/// あって参照ではない。
pub fn find_device_by_id(
    song: &common::model::Song,
    device_id: u64,
) -> Option<(u32, u32)> {
    if device_id == 0 {
        return None;
    }
    let (chain, index) = song.find_device(device_id)?;
    let owner = song.chain_owner_track(chain)?;
    Some((owner, index as u32))
}

/// `device_id` の所有 track (`MASTER_TRACK_ID` = master)。
#[must_use]
pub fn device_owner_track(song: &common::model::Song, device_id: u64) -> Option<u32> {
    if device_id == 0 {
        return None;
    }
    song.device_owner_track(device_id)
}

/// `track_id` の **信号順 (pre-order、Parallel の中も含む)** で `index` 番目の plugin の
/// 安定 id。 headless script / テストが「N 番目に挿した plugin」 を指すための口。
/// device が存在しない / id 未採番 (0) なら `None`。
#[must_use]
pub fn device_id_at(
    song: &common::model::Song,
    track_id: u32,
    device_index: u32,
) -> Option<u64> {
    let chain = song.fx_chain_by_track_id(track_id)?;
    common::model::plugins(chain)
        .nth(device_index as usize)
        .map(|d| d.id)
        .filter(|&id| id != 0)
}

/// device を chain へ挿す位置 ([`RelocateDevices::dest_index`] / `DeviceEvent::AddParallel`)。
/// handler は Song を書き換える closure の中でこれを chain 内 index へ解決する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertAt {
    /// chain 内の位置 (`0..=chain.len()`)。
    Index(u32),
    /// 新規デバイスの既定位置 (Q6: 組み込みの手前)。**実行時の Song** から
    /// `Song::default_insert_index` で解決する (deferred 実行でも古くならない)。
    Default,
}

impl InsertAt {
    /// `song` の `chain` 内 index へ解決する (`None` = chain が無い)。`Index` は chain 長で頭打ち。
    #[must_use]
    pub fn resolve(self, song: &common::model::Song, chain: ChainRef) -> Option<usize> {
        match self {
            Self::Index(i) => song.chain_devices(chain).map(|d| (i as usize).min(d.len())),
            Self::Default => song.default_insert_index(chain),
        }
    }
}

/// r.md #71 (プラグインのコピー / 移動): device の運搬要求 1 件分。 表示順は
/// `device_ids` の並びが決める (呼び出し側がチェーン表示順に整えて渡す)。
/// r.md #110: 落とし先は [`ChainRef`] (top-level か Parallel 内 chain か) + その chain 内位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocateDevices {
    /// 運ぶ device (plugin / Parallel の安定 id)。
    pub device_ids: Vec<u64>,
    /// 落とし先チェーン。
    pub dest: ChainRef,
    /// 落とし先チェーン内の挿入位置。
    pub dest_index: InsertAt,
    /// `true` = コピー (新 device id を採番)、`false` = 移動 (id 据え置き = 音を切らない)。
    pub copy: bool,
}

/// r.md #71 (プラグインのコピー / 移動): チェーンから掴んだプラグインの
/// 運搬中データ (daw-ui の drag payload に載る)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDragPayload {
    /// 運ぶ device (チェーン表示順)。
    pub device_ids: Vec<u64>,
    /// 掴んだときのチェーン所有者 (ドラッグ中の表示切り替えで cursor が動くので、
    /// 「どこから来たか」 は payload 側が覚えておく)。
    pub source_track: u32,
}

/// r.md #71 (プラグインのコピー / 移動): チェーン行の drag payload に付ける札。
/// drop 側 (インスペクタのチェーン / アレンジのトラックヘッダ) はこの札とだけ
/// 照合する (daw-ui core はペイロードの中身を知らない)。
pub const DEVICE_DRAG_KIND: &str = "daw_01.device_chain";

/// `loaded_devices` の値: 1 つの device (`PluginInstance::id`) に対する load 情報。
/// r.md #71 (プラグインのコピー / 移動): キーが安定 `device_id` になったので、
/// 値に id を複製して持たない (SSoT)。
#[derive(Debug, Clone)]
pub struct LoadedDeviceInfo {
    /// stable string id (= `PluginInstance::plugin_id` と同じ値)。
    /// reconcile の device-level diff で「Song と host で同じ plugin が
    /// 居るか」 を判定するキー。
    pub plugin_id_str: String,
    /// plugin_host が採番した instance token (`docs/plan_project_tabs.md` §1.3)。
    /// `OpenPluginShmem` で daw_audio へ渡し、resource monitor の per-plugin μs もこれで引く。
    pub token: common::protocol::InstanceToken,
}

/// `reconcile_plugins_with_song` の Phase B が計算する action。
/// IPC dispatch から独立した純粋データ型にすることで unit test しやすく
/// する (4dc982c で導入した device-level diff の regression 防止)。
///
/// v34 (r.md #71 プラグインのコピー / 移動): アドレスは安定 `device_id` 一本。
/// track / chain 内 index は出てこない (host は帰属も順序も持たない)。
#[derive(Debug, Clone, PartialEq)]
pub enum SlotReconcileAction {
    /// host にあるが Song に無い device を host から消す
    /// (= `PluginCommand::RemoveSlotPlugin` 相当)。
    RemoveDevice { device_id: u64 },
    /// Song にあるが host に無い、 もしくは plugin_id_str が違う device を
    /// (再) load する (= `PluginCommand::SetSlotPlugin` 相当)。 caller が
    /// `plugin_db` から format / path を解決して IPC を組み立てる。
    LoadDevice {
        device_id: u64,
        plugin_id_str: String,
        initial_state: Option<Vec<u8>>,
    },
}

impl SlotReconcileAction {
    /// action が指す device。
    #[must_use]
    pub fn device_id(&self) -> u64 {
        match self {
            Self::RemoveDevice { device_id } | Self::LoadDevice { device_id, .. } => *device_id,
        }
    }
}

/// Phase B 純粋関数化。 song と現在の `loaded_devices` cache を見て、 host
/// と Song を揃えるための action 列を返す。 副作用なし (IPC は呼ばない、
/// AppData にも触らない)。
///
/// 走査順は Song 順 (track → master_fx_chain、Parallel の中は chain 順 = 音の処理順)
/// なので `LoadDevice` の並びは決定的。 `RemoveDevice` は host 側 map の iteration
/// 順に依存しないよう id 昇順に sort する。
///
/// **host に居るべき device の唯一の導出口** (r.md #131): 実効的に無効なトラックの device は
/// 居るべきでない (`Song::live_plugins`) — 無効化で `RemoveDevice`、有効化で state 付きの `LoadDevice`、
/// undo / redo も同じ口。`pending_loads` (= load 応答待ち、`AppData::pending_plugin_loads` の鍵) のうち
/// 居るべきでないものも `RemoveDevice` にする (読み込み中に無効化すると、応答後に載ったまま残らない)。
/// 送る順 (engine への構造の同期をどこに挟むか) は適用する側 `AppData::apply_slot_reconcile_actions` が持つ。
pub fn compute_slot_reconcile_actions(
    song: &common::model::Song,
    loaded_devices: &HashMap<u64, LoadedDeviceInfo>,
    pending_loads: &HashMap<u64, u64>,
) -> Vec<SlotReconcileAction> {
    // (1) host にあるが Song に無い device → RemoveDevice。 **余剰を落として
    //     から load する** 順序は現行仕様なので、 先頭に置く。
    let mut out: Vec<SlotReconcileAction> = compute_slot_removals(song, loaded_devices, pending_loads)
        .into_iter()
        .map(|device_id| SlotReconcileAction::RemoveDevice { device_id })
        .collect();
    for inst in host_plugins(song) {
        // 応答待ちの load がある device は送り直さない (編集の直後に実体化した device をここで二重に load しない)。
        let need_load = !pending_loads.contains_key(&inst.id)
            && loaded_devices.get(&inst.id).is_none_or(|info| info.plugin_id_str != inst.plugin_id);
        if need_load {
            out.push(SlotReconcileAction::LoadDevice {
                device_id: inst.id,
                plugin_id_str: inst.plugin_id.clone(),
                initial_state: inst.state.as_deref().map(<[u8]>::to_vec),
            });
        }
    }
    out
}

/// host に載っている / 読み込み応答待ちの device のうち、`song` では host に居るべきでないもの (id 昇順 = host 側 map の
/// iteration 順に依存しない)。[`compute_slot_reconcile_actions`] の `RemoveDevice` と同じ集合で、履歴ジャンプが
/// **動かす前に**「このジャンプで host から降りる device があるか」(= 降ろす前に plugin state を取り寄せるか) を
/// 行き先の Song から求めるのにも使う。
#[must_use]
pub fn compute_slot_removals(
    song: &common::model::Song,
    loaded_devices: &HashMap<u64, LoadedDeviceInfo>,
    pending_loads: &HashMap<u64, u64>,
) -> Vec<u64> {
    let host: std::collections::HashSet<u64> = host_plugins(song).map(|p| p.id).collect();
    let mut extra: Vec<u64> =
        loaded_devices.keys().chain(pending_loads.keys()).copied().filter(|id| !host.contains(id)).collect();
    extra.sort_unstable();
    extra.dedup();
    extra
}

/// `song` で plugin host に居るべき plugin (`Song::live_plugins` のうち映像でないもの)。内蔵映像効果は plugin_host に
/// 載らない device なので、ここに混ぜると毎回 `LoadDevice` が出て「load 応答が来ない device」が永久に溜まる。
fn host_plugins(song: &common::model::Song) -> impl Iterator<Item = &common::model::PluginInstance> {
    song.live_plugins().filter(|p| !p.ports.is_video())
}
