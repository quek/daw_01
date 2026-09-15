//! チェーン上の plugin 1 個 ([`PluginInstance`]) の型と、blob を構造的に除外する手書きの
//! bincode 表現。
//!
//! `Track.devices` / `Song.master_fx_chain` の要素として `LoadSong` の wire を渡るので
//! `common/build.rs` の `WIRE_SOURCES` に登録している (不変条件 7)。

use serde::{Deserialize, Serialize};

use super::{AuxInputRoute, AuxOutputRoute, base64_opt};
use crate::plugin_format::PluginFormat;

/// Reference to a plugin loaded on a track, with the opaque state blob the
/// plugin itself produced (CLAP `clap_plugin_state.save` or VST3
/// `IComponent::getState`). Paths are NOT stored — `(format, plugin_id)`
/// is resolved through `plugin_db::PluginDatabase` at load time, keeping
/// projects portable across machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstance {
    /// v29: Song-global の安定 device id (`Song.next_device_id` 採番、`0` =
    /// 未採番 sentinel)。IPC / automation / plugin host bookkeeping / shmem 名
    /// / worker dispatch のアドレスはすべてこの id。chain 内 index は表示順序
    /// のみ (`docs/plan_arch_refactor.md` §1)。
    #[serde(default)]
    pub id: u64,
    /// CLAP stable id (reverse-DNS) or VST3 class UUID rendered as hex.
    pub plugin_id: String,
    /// Which backend created this plugin. Defaults to CLAP for projects
    /// saved before VST3 support existed.
    #[serde(default)]
    pub format: PluginFormat,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "base64_opt"
    )]
    /// D2 (r.md #8): `Arc<[u8]>` で保持し undo snapshot 間で共有 (= `Song::clone`
    /// が plugin state を毎回コピーしない)。 plugin が serialize した不透明 state で
    /// undo の編集対象ではないので共有して安全。
    pub state: Option<std::sync::Arc<[u8]>>,
    /// Consumer A (旧 sidechain、 docs/plan_modulation.md §1): aux 入力ポート
    /// ごとのルート。 各 entry は plugin の `is_main=false` aux input port
    /// index → `AudioTap`。 `None` (or 不足 index) はそのポートを無音に。
    /// `Vec` 長 = user が配線した aux port 数 (plugin の実 port 数より短くて
    /// よい — 末尾は無音)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aux_inputs: Vec<Option<AuxInputRoute>>,
    /// Consumer B (パラアウト、 docs/plan_paraout.md): aux **出力**ポートごとの
    /// ルート。 各 entry は plugin の `is_main=false` aux output port index →
    /// `AuxOutputRoute { dest_track }`。 `None` (or 不足 index) はそのポートを
    /// どこにも流さない (= 業界標準: 未振分け aux 出力は無音)。 `Vec` 長 = user が
    /// 配線した aux port 数 (plugin の実 port 数より短くてよい)。 旧 file には
    /// 無いので `#[serde(default)]` で forward-migrate (空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aux_outputs: Vec<Option<AuxOutputRoute>>,
    /// パラアウト (docs/plan_paraout.md): how many `is_main=false` audio output
    /// ports this plugin actually declares (reported by the plugin host at load
    /// via `SlotPluginLoaded`, cached here so it survives reorder and is known
    /// on project reopen). The GUI uses it to know how many child tracks to
    /// create on "explode" and how many routing rows to show. `0` = the common
    /// single-output plugin. Distinct from `aux_outputs.len()` (= how many
    /// ports the user has wired). daw_audio ignores it (it routes via
    /// `aux_outputs` + the plugin host's `aux_out_active`).
    #[serde(default)]
    pub aux_output_count: u8,
    /// r.md #110: `is_main=false` な audio **入力** port の数 (`aux_output_count` と対称、
    /// host が `SlotPluginLoaded` で報告)。 inspector はこれが 1 以上の device にだけ
    /// sidechain (SC) 制御を出す。 engine は `aux_inputs` の配線だけを見る。
    #[serde(default)]
    pub aux_input_count: u8,
    /// v23: この device の port 構成。役割導出の入力。
    #[serde(default)]
    pub ports: crate::port_config::PortConfig,
    /// (r.md #5 ARA2) ARA ドキュメントアーカイブ = プラグインがシリアライズした
    /// 編集状態 (Melodyne のピッチ修正等)。ホストが instance ごとに保持し、
    /// プロジェクトには base64 で保存、ロード時にプラグインへ送り返して編集を
    /// 復元する。`state` (CLAP/VST3 own state) とは独立。
    #[serde(default, skip_serializing_if = "Option::is_none", with = "base64_opt")]
    /// D2 (r.md #8): `Arc<[u8]>` で保持し undo snapshot 間で共有 (Melodyne 等の
    /// ARA アーカイブは MB 級で undo の編集対象でないため)。
    pub ara_archive: Option<std::sync::Arc<[u8]>>,
    /// v43 (r.md #132 残件): `ara_archive` の **目次** — 中にある object (audio source / audio modification) の
    /// 今の id と、アーカイブに書かれている id (`crate::ara_ids::AraArchiveEntry`)。 plug-in host がアーカイブと
    /// 一緒に報告し、旧ファイルの読み込みで作る (v41 以前は旧 persistent id の読み替え、v42 はトラックの document
    /// から)。 document を組むときに `SetupAraDocument.archive_ids` で送り、目次にある object だけをアーカイブから
    /// restore する (無い object は写した元から始められる)。 アーカイブと必ず一緒に置き換える / 捨てる
    /// ([`Self::set_ara_archive`] / [`Self::drop_ara_archive`])。 wire (LoadSong) には載せない。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ara_archive_ids: Vec<crate::ara_ids::AraArchiveEntry>,
    /// r.md #36: このプラグインのエディタ窓では **キーを一切横取りしない**
    /// (= REAPER の 「Send all keyboard input to plug-in」)。
    ///
    /// 既定 `false` = 自動判定に任せる。 通常はプラグイン側が 「消化しなかった」
    /// と表明したキーだけをホストが取るので Space での再生 / 停止とプラグインの
    /// 文字入力が両立する。 ただし Dear ImGui / GLFW / 自前 OpenGL 系のエディタは
    /// 消化の有無を外に一切出さないため、 そういうプラグインではここを `true` にして
    /// 手動で全キーを譲る。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub send_all_keys_to_plugin: bool,
    /// r.md #105: この device を **信号経路から外す** (Live の device off / Bitwig の
    /// power ボタン / REAPER の bypass)。`true` の間、 engine はこの device を dispatch
    /// せず、 音声も MIDI も手前の状態のまま次の device へ流れる (= pass-through)。
    /// 報告 latency も 0 扱いで PDC から外れる。 映像 FX も同じフラグで解決から外れる。
    /// プラグイン instance 自体は host に生きたまま (GUI は開ける、 state も保つ)。
    /// project に保存し undo 対象 (= 「作った中身」)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypassed: bool,
}

impl PluginInstance {
    pub fn new(plugin_id: String, format: PluginFormat) -> Self {
        Self {
            id: 0,
            plugin_id,
            format,
            state: None,
            aux_inputs: Vec::new(),
            aux_outputs: Vec::new(),
            aux_output_count: 0,
            aux_input_count: 0,
            ports: crate::port_config::PortConfig::default(),
            ara_archive: None,
            ara_archive_ids: Vec::new(),
            send_all_keys_to_plugin: false,
            bypassed: false,
        }
    }

    /// プラグインが今書いた ARA アーカイブ (中にある object の今の persistent id `ids`) で置き換える。
    pub fn set_ara_archive(&mut self, archive: std::sync::Arc<[u8]>, ids: Vec<String>) {
        self.ara_archive = Some(archive);
        self.ara_archive_ids = ids.into_iter().map(crate::ara_ids::AraArchiveEntry::stored).collect();
    }

    /// ARA アーカイブを捨てる (読み替え表も一緒に)。 捨てたら `true`。
    pub fn drop_ara_archive(&mut self) -> bool {
        self.ara_archive_ids.clear();
        self.ara_archive.take().is_some()
    }

    pub fn with_ports(
        plugin_id: String,
        format: PluginFormat,
        ports: crate::port_config::PortConfig,
    ) -> Self {
        Self {
            id: 0,
            plugin_id,
            format,
            state: None,
            aux_inputs: Vec::new(),
            aux_outputs: Vec::new(),
            aux_output_count: 0,
            aux_input_count: 0,
            ports,
            ara_archive: None,
            ara_archive_ids: Vec::new(),
            send_all_keys_to_plugin: false,
            bypassed: false,
        }
    }

}

/// wire (bincode / IPC) 表現は手書きで、`state` / `ara_archive` の MB 級 blob を
/// **構造的に除外**する (`docs/plan_arch_refactor.md` §2)。 アーカイブの読み替え表 `ara_archive_ids` も
/// アーカイブと一緒に `SetupAraDocument` が運ぶので載せない。ドキュメント
/// (serde / JSON 保存) は両フィールドを base64 で保持し、blob が必要な IPC
/// 操作は専用メッセージ (`SetSlotPlugin.initial_state` /
/// `SetupAraDocument.archive` / `AllPluginStates`) が個別に運ぶ。これで
/// `LoadSong` は plugin state / ARA アーカイブの肥大に依らず常に小さく、
/// 16MB wire 上限に構造的に到達しない。encode / decode の field 順は一致
/// させること (id → plugin_id → format → aux_inputs → aux_outputs →
/// aux_output_count → ports → send_all_keys_to_plugin → bypassed → aux_input_count)。
impl bincode::Encode for PluginInstance {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        self.id.encode(encoder)?;
        self.plugin_id.encode(encoder)?;
        self.format.encode(encoder)?;
        self.aux_inputs.encode(encoder)?;
        self.aux_outputs.encode(encoder)?;
        self.aux_output_count.encode(encoder)?;
        self.ports.encode(encoder)?;
        self.send_all_keys_to_plugin.encode(encoder)?;
        self.bypassed.encode(encoder)?;
        self.aux_input_count.encode(encoder)
    }
}

impl<Ctx> bincode::Decode<Ctx> for PluginInstance {
    fn decode<D: bincode::de::Decoder<Context = Ctx>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            id: bincode::Decode::decode(decoder)?,
            plugin_id: bincode::Decode::decode(decoder)?,
            format: bincode::Decode::decode(decoder)?,
            state: None,
            aux_inputs: bincode::Decode::decode(decoder)?,
            aux_outputs: bincode::Decode::decode(decoder)?,
            aux_output_count: bincode::Decode::decode(decoder)?,
            ports: bincode::Decode::decode(decoder)?,
            ara_archive: None,
            ara_archive_ids: Vec::new(),
            send_all_keys_to_plugin: bincode::Decode::decode(decoder)?,
            bypassed: bincode::Decode::decode(decoder)?,
            aux_input_count: bincode::Decode::decode(decoder)?,
        })
    }
}

impl<'de, Ctx> bincode::BorrowDecode<'de, Ctx> for PluginInstance {
    fn borrow_decode<D: bincode::de::BorrowDecoder<'de, Context = Ctx>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        <Self as bincode::Decode<Ctx>>::decode(decoder)
    }
}
