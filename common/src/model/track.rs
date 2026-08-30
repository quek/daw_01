//! Track / Send / GroupTransform / InstrumentSource
//!
//! arch-refactor #9 (god-file budget) で model.rs から分割。pure code movement で
//! 挙動・serialize 形式は不変。sibling 型は `use super::*` 経由で参照する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

/// Maximum linear amplitude for a track's [`Track::volume`], the master gain,
/// and (matching this range) each [`Send::gain`]. `1.0` = unity (0 dB), `2.0`
/// = +6.02 dB — the top of the mixer fader's `MeterScale` (+6 dB, Ardour's
/// default fader ceiling) and the divisor the automation layer already uses to
/// normalize `Volume` / `SendGain` (`plain / 2.0`). Single source of truth for
/// every volume / gain clamp (track volume, master gain, send gain) so a fader
/// pushed to its visual top no longer snaps back to unity (r.md #11).
pub const MAX_TRACK_GAIN: f32 = 2.0;

/// v23 (`docs/plan_linear_chain.md`): a track owns **one** linear CLAP
/// signal chain, `devices: Vec<PluginInstance>`. Roles (MIDI FX / instrument
/// / audio FX) are **not classified at all** — the engine simply connects
/// ports serially (Reaper 流): the track's notes flow into every device with a
/// note input, the track's audio (clips) flows into every device with an audio
/// input, and each device's note / audio outputs feed the next. The behaviour
/// (whether a device acts as a generator, instrument, or effect) emerges from
/// its own `PortConfig` + processing, not a stored or derived label. The final
/// audio flows into the parent — either a `Group` track (when
/// `parent_group_id == Some(id)`) or the master bus (when `None`). Reorder /
/// insert / remove just permute the Vec; nothing else to re-key.
///
/// Older files used three role-keyed fields (`midi_fx_chain` / `instrument`
/// / `fx_chain`); load 時の JSON 前処理 (`project::migrate_legacy_device_chains`) が
/// deserialize の前に `midi_fx ++ instrument? ++ fx` 順で `devices` へ平坦化するので、
/// in-memory 型は legacy デバイスフィールドを持たない。
///
/// v16 (`docs/plan_text_overlay.md`): 旧 `kind: TrackKind { Audio, Video }`
/// を廃止し、 全 track が unified に audio path + visual composite path 両方
/// を保持する (= REAPER 流、 同 track 上で audio / midi / video / image /
/// text clip を混在可能)。 旧 Video track は v16 migration で audio
/// defaults (instrument: None / fx_chain: vec![] / volume: 1.0 / pan: 0.0
/// / armed: false / source: None) を自動補完し、 mixer / engine path に
/// 静かに参加する (v23 以降は空 `devices` に相当)。 旧 v15 file の `kind`
/// field は serde が未知 field として捨てる (= deny_unknown_fields が無いため
/// tolerant)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Track {
    /// Stable id assigned by `Song::alloc_track_id`. `0` is "未採番"
    /// sentinel — reassigned by `Song::ensure_ids` when loading an older
    /// file. Persists across track add/remove and reorder; arrangement
    /// widget addresses tracks by this id, not by index.
    #[serde(default)]
    pub id: u32,
    pub name: String,
    /// v23: 1 本の線形デバイスチェーン。役割は保持せず ports から位置導出。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<PluginInstance>,
    pub volume: f32,
    pub pan: f32,
    /// Track silenced by the user. Additive with the global solo rule (see
    /// `solo` below): `effective_mute = muted || (any_solo_on && !solo)`.
    #[serde(default)]
    pub muted: bool,
    /// When any track has `solo == true`, tracks that don't are silenced
    /// for the duration of playback (classic mixer-strip behaviour).
    #[serde(default)]
    pub solo: bool,
    /// Phase 7 B4 (`docs/plan_b4_midi.md` §3.1): Record-arm 状態。 armed track
    /// のみが MIDI input (および将来の audio input) を受け取り、 録音中は
    /// 該当 track の MIDI clip に note が書き込まれる。 業界標準 (Bitwig /
    /// Live / Reaper) と同 idiom (= 排他性なし、 任意数の track を同時 armed
    /// にできる)。 v8 file は `false` で forward-migrate (serde default)。
    #[serde(default)]
    pub armed: bool,
    /// Future use: VOICEVOX speaker / style etc. Kept distinct from the
    /// `instrument` slot because it selects a rendering backend, not a CLAP
    /// plugin.
    #[serde(default)]
    pub source: InstrumentSource,
    /// アレンジのクリップ。
    ///
    /// **不変条件: 同一トラックのクリップは時間的に重ならない**
    /// (`docs/plan_range_selection.md` §6)。 1 トラックの 1 時点に鳴るクリップは 1 つ。
    /// 重なりを作りうる編集 (移動 / trim / 貼り付け / 複製 / 録音 / import / 新規作成 /
    /// ランチャーからの drop) は **必ず [`Track::place_clip`] を通す** —
    /// そこが上書き規則 (完全被覆で削除 / 端の食い込みで trim / 中央で 2 分割) を
    /// 無条件に適用する。 `clips.push` を直に書くと不変条件に穴が開く。
    ///
    /// 順序は保証しない (住所は `Clip::id`)。 開始拍順が要る処理はその場で sort する。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
    /// Per-track stable id allocator for `Clip`. Bumped each time a new
    /// clip is created on this track.
    #[serde(default)]
    pub next_clip_id: u32,
    /// Parent group track id. `None` ⇒ this track feeds the master bus
    /// directly. Any track can act as a "group" — that role is derived
    /// from whether other tracks point at this one's id, not stored on
    /// the track itself (Reaper's folder-track model). Forms a tree of
    /// arbitrary depth; cycles are rejected by the graph compiler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_group_id: Option<u32>,
    /// Aux sends from this track to other (return / bus) tracks. Each
    /// `Send` is a *parallel* gain-scaled copy of this track's signal
    /// summed into the destination track's input bus — the source's own
    /// signal still flows to its parent / master untouched. Empty for
    /// tracks with no sends. A "return" track is not a distinct kind: it
    /// is derived (a track that has incoming sends), exactly like a
    /// "group" is derived from incoming `parent_group_id`. See `Send` /
    /// `docs/plan_routing_graph.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<Send>,
    /// v29: per-track stable id allocator for `Send::id`。`0` は sentinel、
    /// `1` から採番。
    #[serde(default)]
    pub next_send_id: u32,
    /// Per-target automation lanes attached to this track. Each lane
    /// carries a `default_value` (used outside any clip / when
    /// `enabled = false`) and a list of `AutomationClip` whose
    /// `content_id` resolves into `Song.clip_contents` like MIDI /
    /// Audio clips do. Order is the display order in the inspector
    /// and arrangement (drag-reorderable). Empty for tracks without
    /// any automation. See `docs/plan_automation.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation_lanes: Vec<AutomationLane>,
    /// Per-track stable id allocator for `AutomationLane`. Bumped each
    /// time a new lane is created; never reused even after deletion.
    /// `0` is the sentinel — `Track::ensure_lane_ids` reassigns it on
    /// load.
    #[serde(default)]
    pub next_lane_id: u32,
    /// **lane 非依存モジュレーション** (`docs/plan_modulation_routing_redesign.md`
    /// §2): この track 内の param (`TrackBuiltin` / `PluginParam` / `ImageBuiltin`
    /// / `TextBuiltin` / `GroupTransform`) を変調する `ModRouting` の集合。各
    /// routing の `target` が param を直接指すので automation lane は不要。空 Vec
    /// で変調なし (旧 file は forward-migrate)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_routings: Vec<ModRouting>,
    /// v18 (`docs/plan_track_clip_color.md`): user-facing track color
    /// (RGB, opaque). `None` ⇒ the view layer derives a stable palette
    /// color from `id` (auto-assignment, reorder-stable). `Some(rgb)` ⇒
    /// explicit user override. The color carries no audio/engine meaning;
    /// only `daw_gui` reads it (arrangement header tint + clip inherit).
    /// v17 files forward-migrate to `None` (= derived palette color).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// v19 (`docs/plan_tachie_group_transform.md`): 親グループトラックが合成済み
    /// 立ち絵 (子パーツを z 順に 1 枚へ合成したもの) にかける 2D affine + opacity。
    /// `None` ⇒ transform 無し (= identity、立ち絵グループでない通常 / audio
    /// グループ)。`Some` ⇒ 位置/回転/非一様スケール/任意アンカー/opacity。純粋に
    /// visual で daw_audio は読まない (group の役割は `parent_group_id` 由来で派生、
    /// inspector / 合成は §5.6 `group_has_visual_content` で gate)。v18 files
    /// forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_transform: Option<GroupTransform>,
    /// v21 (`docs/plan_pakupaku.md`): 口パク出力先。`Some(track_id)` ⇒ この
    /// vocal track の notes+歌詞から生成した口画像 `ImageEvent` 列を、指定の
    /// 口 track (= 立ち絵 group 内の子 image track) へ焼き込む。設定が arm に
    /// あたり、notes/歌詞/`mouth_map` 変更で自動再生成される (派生データ)。
    /// vocal track 以外では意味を持たない。v20 files forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lipsync_target_track: Option<u32>,
    /// v21 (`docs/plan_pakupaku.md`): 口形状 → `ImageSourceId` のマッピング。
    /// 口 track (= `lipsync_target_track` が指す側) に持たせ、生成時に各 phoneme
    /// の口形状をこの表で画像へ解決する。`None` ⇒ 未設定 (口パク未割当)。
    /// v20 files forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouth_map: Option<MouthMap>,
    /// v35 (r.md #87 クリップランチャー): この**トラック行**のセル。列は
    /// [`SessionClip::scene_id`] が指す (`Song.scenes` の id、positional index ではない)。
    ///
    /// `clip.id` は `next_clip_id` の **`clips` と同じ id 空間**から採る — これで
    /// `ClipKey { track_id, clip_id }` が arrangement のクリップと同じ形で通り、
    /// 選択 / undo / 色 / mute / 共有 content / ピアノロールがそのまま効く。
    /// 逆に **`clips` と `session_clips` で同じ id を再利用してはいけない**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_clips: Vec<SessionClip>,
    /// v35 (r.md #87): このトラック行の主導権 (アレンジ / ランチャー)。
    /// 停止しても消えず `.daw` に保存する — 停止 → 再生で同じセルが鳴り直し、
    /// 書き出す音もこの状態で決まる (`docs/plan_rmd_87_clip_launcher.md` Q9 / Q10)。
    #[serde(default, skip_serializing_if = "RowPlayback::is_arranger")]
    pub launcher: RowPlayback,
}

/// Where a `Send` taps the source track's signal chain.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode,
)]
pub enum SendMode {
    /// After the track's volume / pan fader (the post-fader scratch).
    /// The send level tracks the source fader — the standard choice for
    /// reverb / delay returns where the wet should follow the dry.
    #[default]
    PostFader,
    /// After the fx chain but before the volume / pan fader. The send is
    /// independent of the source fader — for cue / parallel sends. (A
    /// pre-FX raw tap is the sidechain feature's job, not a send.)
    PreFader,
}

/// A single aux send: a parallel, gain-scaled copy of a track's signal
/// routed to another track that acts as a return / bus. Mirrors Ardour
/// `InternalSend` / a REAPER track send. The source track's main output
/// is unaffected; the copy is summed into `dest_track_id`'s input bus
/// before that destination's fx chain runs. The send level is
/// automatable via `AutomationTarget::TrackBuiltin(TrackBuiltinParam::
/// SendGain { send_id })`, addressed by this send's stable `id` (v29 —
/// positional index addressing は廃止)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Send {
    /// v29: track 内で安定な send id (`Track.next_send_id` 採番、`0` = 未採番
    /// sentinel — `Track::ensure_send_ids` が load 時に採番)。automation
    /// (`TrackBuiltinParam::SendGain`) と IPC は positional index でなく
    /// この id でアドレスする。
    #[serde(default)]
    pub id: u32,
    /// Stable `Track::id` of the destination (return / bus) track.
    pub dest_track_id: u32,
    /// Linear send gain (`0.0` = silent, `1.0` = unity, up to `2.0` =
    /// +6 dB to match the volume-fader range). Automatable.
    pub gain: f32,
    /// Tap point on the source track's signal chain.
    pub mode: SendMode,
    /// Per-send mute. `false` keeps the wiring but silences the send.
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum InstrumentSource {
    #[default]
    None,
    /// 「このトラックは VOICEVOX builtin で歌わせる」印。
    /// 声 (singer + style) は per-clip (`Clip::speaker_id` 等) が SSoT で、
    /// トラックは声を持たない unit marker。旧プロジェクト
    /// (`Vocal { speaker_id, style_name }`) は `project::load` の JSON
    /// 前処理で旧トラック声を全 clip へ焼き込んでから unit `Vocal` に
    /// 移行する (`migrate_vocal_source_to_clips`)。
    Vocal,
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            devices: Vec::new(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            source: InstrumentSource::None,
            clips: Vec::new(),
            next_clip_id: 1,
            parent_group_id: None,
            sends: Vec::new(),
            next_send_id: 1,
            automation_lanes: Vec::new(),
            next_lane_id: 1,
            mod_routings: Vec::new(),
            color: None,
            group_transform: None,
            lipsync_target_track: None,
            mouth_map: None,
            session_clips: Vec::new(),
            launcher: RowPlayback::Arranger,
        }
    }
}

/// v19 (`docs/plan_tachie_group_transform.md` §4.1): 親グループトラックが合成済み
/// 立ち絵 1 枚にかける 2D affine + opacity。AE の Transform プロパティ群
/// (Anchor / Position / Scale / Rotation / Opacity) と同構成。合成式は列ベクトル
/// 左乗算で `M_local = T(pos+anchor)·R(rot)·S(sx,sy)·T(-anchor)`、親子は
/// `M_world = M_parent·M_local` (トップダウン)。Opacity だけは行列に乗せず合成済み
/// quad の alpha に適用 (AE 準拠)。値は plain 単位 (automation lane の正規化は
/// `crate::automation::plain_to_norm` 参照)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct GroupTransform {
    /// 位置 X (normalized project 座標、0..1 が preview 幅)。アンカー基準位置への
    /// オフセット (AE 同様、`pos = 0` でアンカーが home に留まる)。
    pub x: f32,
    /// 位置 Y。
    pub y: f32,
    /// 2D 回転 (radians、clockwise positive)。アンカーを旋回中心とする。
    pub rotation_radians: f32,
    /// 水平スケール倍率 (`1.0` = 等倍)。アンカー中心。非一様可 (`scale_y` と独立)。
    pub scale_x: f32,
    /// 垂直スケール倍率。
    pub scale_y: f32,
    /// アンカー X (合成キャンバスの normalized 0..1、`0.5` = 中央)。回転・スケール
    /// 共通の中心。
    pub anchor_x: f32,
    /// アンカー Y。
    pub anchor_y: f32,
    /// 全体不透明度 (0..1)。transform 行列には乗せず、合成済みグループ quad の
    /// alpha に適用 (AE 準拠)。子個別の opacity は合成前に各子へ焼き込まれる。
    pub opacity: f32,
}

impl Default for GroupTransform {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            rotation_radians: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            anchor_x: 0.5,
            anchor_y: 0.5,
            opacity: 1.0,
        }
    }
}

impl Track {
    /// v35 (r.md #87): arrangement (`clips`) と launcher (`session_clips`) の
    /// **全 Clip**。
    ///
    /// **content の参照数え上げ / GC / content_id 採番はこの 1 本を通すこと。**
    /// 片方を数え落とすと、保存時に GC が「誰も参照していない content」と判定して
    /// **セルの中身が黙って消える**。
    pub fn all_clips(&self) -> impl Iterator<Item = &Clip> {
        self.clips.iter().chain(self.session_clips.iter().map(|s| &s.clip))
    }

    /// [`Self::all_clips`] の可変版。
    pub fn all_clips_mut(&mut self) -> impl Iterator<Item = &mut Clip> {
        self.clips
            .iter_mut()
            .chain(self.session_clips.iter_mut().map(|s| &mut s.clip))
    }

    /// 列 (`scene_id`) にあるこのトラック行のセル。
    #[must_use]
    pub fn session_clip(&self, scene_id: u32) -> Option<&SessionClip> {
        self.session_clips.iter().find(|s| s.scene_id == scene_id)
    }

    /// 列 (`scene_id`) にあるこのトラック行のセル (可変)。
    pub fn session_clip_mut(&mut self, scene_id: u32) -> Option<&mut SessionClip> {
        self.session_clips.iter_mut().find(|s| s.scene_id == scene_id)
    }

    /// `clip_id` でセルを引く (`clip_id` は行内で一意 — `clips` と id 空間を共有する)。
    #[must_use]
    pub fn session_clip_by_id(&self, clip_id: u32) -> Option<&SessionClip> {
        self.session_clips.iter().find(|s| s.clip.id == clip_id)
    }

    /// 列 `cell.scene_id` にセルを置く。 既にセルが居れば**置き換える**
    /// (ドロップ / 移動の「落とした先が埋まっていたら置き換え」規約)。
    ///
    /// 退けたセルがその行で**鳴っていたセル**なら、主導権を新しいセルへ移す。
    /// これをやらないと [`Song::normalize_session`] が「鳴っていたセルが消えた」と
    /// 見て行を停止に落とし、**再生中のセルへ落とした瞬間に音が止まる**。
    /// 置き換えは必ずこの 1 本を通すこと (retain + push を手写しすると必ず漏れる)。
    pub fn put_session_clip(&mut self, cell: SessionClip) {
        let new_id = cell.clip.id;
        let replaced: Option<u32> = self
            .session_clips
            .iter()
            .find(|c| c.scene_id == cell.scene_id)
            .map(|c| c.clip.id);
        self.session_clips.retain(|c| c.scene_id != cell.scene_id);
        self.session_clips.push(cell);
        if let Some(old) = replaced
            && self.launcher == (RowPlayback::Launcher { clip_id: old })
        {
            self.launcher = RowPlayback::Launcher { clip_id: new_id };
        }
    }

    /// このトラックが VOICEVOX で歌う vocal トラックか。 SSoT は
    /// 「builtin VOICEVOX device を実際に持つか」。 旧 `InstrumentSource::Vocal`
    /// marker は device 挿入と別管理で out-of-sync になり得る (旧プロジェクトで
    /// source=None + device の実例あり) ため、 装置の実在を真実として判定する。
    pub fn is_voicevox_vocal(&self) -> bool {
        self.devices.iter().any(|d| {
            d.format == crate::plugin_format::PluginFormat::Builtin
                && d.plugin_id == crate::plugin_db::BUILTIN_ID_VOICEVOX
        })
    }

    /// (talk) このトラックが字幕(テキスト表示)デバイスを持つか。SSoT は
    /// 「builtin 字幕 device を実際に持つか」(`is_voicevox_vocal` と同思想)。
    /// `true` のときだけ、このトラック上の `ClipContent::Text` clip が画面に
    /// overlay 表示される (`docs/plan_voicevox_talk.md` §2、`text_compose` が gate)。
    pub fn has_subtitle_device(&self) -> bool {
        self.devices.iter().any(|d| {
            d.format == crate::plugin_format::PluginFormat::Builtin
                && d.plugin_id == crate::plugin_db::SUBTITLE_ID
        })
    }

    /// パラアウト (`docs/plan_paraout.md`): the device-chain split point for a
    /// **group-with-instrument** track. `Some(k)` when at least one device has
    /// a routed aux output (= this track is a parallel-out source); `k` is the
    /// index **one past** the last such device. The audio engine runs the
    /// "instrument prefix" `devices[0..k]` in pass 1 (producing the main signal
    /// and the aux outputs) and the "bus FX suffix" `devices[k..]` in pass 2 on
    /// the summed bus (own main plus routed children). `None` when no device
    /// routes an aux output (a plain leaf / pure group). Derived purely from the
    /// explicit routing data (`aux_outputs`), never a role heuristic.
    pub fn paraout_split_device(&self) -> Option<u32> {
        let mut last: Option<u32> = None;
        for (i, d) in self.devices.iter().enumerate() {
            if d.aux_outputs.iter().any(Option::is_some) {
                last = Some(i as u32);
            }
        }
        last.map(|i| i + 1)
    }

    /// パラアウト (`docs/plan_paraout.md`): true if this track's instrument
    /// routes its **main** output (parallel-out port 0) to a child track. When
    /// true the parent is a "pure splitter": its main signal goes to its own
    /// child (the first part / Out 1), so the engine must NOT keep main in the
    /// parent's scratch — the parent sums ALL children via a clearing `Mix`.
    /// When false (port 0 unrouted) the parent keeps its main as its own bus
    /// signal and sums children on top (instrument-bus, `MixAdditive`). Decided
    /// purely from explicit routing data (`aux_outputs[0]`), no role heuristic.
    pub fn paraout_main_to_child(&self) -> bool {
        self.devices
            .iter()
            .any(|d| matches!(d.aux_outputs.first(), Some(Some(_))))
    }

    /// Allocate a new stable clip id, bumping the per-track counter.
    ///
    /// **既に使われている id は返さない。** counter が実際のクリップより遅れている
    /// (旧ファイル / テスト fixture / 手組みの `Track`) と、採番が既存 id と衝突して
    /// [`Track::place_clip`] の「同じ id を置き直す」規則に巻き込まれ、無関係な
    /// クリップが黙って消える。 採番のたびに実在 id の最大を見て counter を押し上げる
    /// (クリップ数は小さいので線形走査で足りる)。
    pub fn alloc_clip_id(&mut self) -> u32 {
        let max_used = self.all_clips().map(|c| c.id).max().unwrap_or(0);
        let id = self.next_clip_id.max(1).max(max_used.saturating_add(1));
        self.next_clip_id = id.saturating_add(1);
        id
    }

    /// Re-assign stable ids to all clips. Idempotent (clips with non-zero
    /// ids are left alone, counter is bumped above the max seen).
    /// v35 (r.md #87): arrangement の `clips` と launcher の `session_clips` は
    /// **同じ id 空間**なので、両方を 1 回の走査で採番する。未採番 (`0`) と
    /// **重複**を採り直すので冪等 (2 回呼んでも同じ結果 = 開いただけで `*` が立たない)。
    pub fn ensure_clip_ids(&mut self) {
        let mut next = self.next_clip_id.max(1);
        for clip in self.all_clips() {
            if clip.id != 0 {
                next = next.max(clip.id.saturating_add(1));
            }
        }
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for clip in self.all_clips_mut() {
            if clip.id == 0 || !seen.insert(clip.id) {
                clip.id = next;
                seen.insert(next);
                next = next.saturating_add(1);
            }
        }
        self.next_clip_id = next.max(1);
    }

    pub fn clip_index_by_id(&self, clip_id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == clip_id)
    }

    /// トラック内で id からクリップを引く。**アレンジのクリップ (`clips`) と
    /// ランチャーのセル (`session_clips`) の両方**を見る (v35 以降、両者は 1 つの
    /// id 空間を共有するので「この id のクリップ」は一意)。 GUI の住所
    /// (`ClipKey`) の解決はここが唯一の口で、これによりピアノロール /
    /// オーディオエディタ / 名前 / 色 がセルにもそのまま効く。
    pub fn clip_by_id(&self, clip_id: u32) -> Option<&Clip> {
        self.all_clips().find(|c| c.id == clip_id)
    }

    pub fn clip_by_id_mut(&mut self, clip_id: u32) -> Option<&mut Clip> {
        self.all_clips_mut().find(|c| c.id == clip_id)
    }

    /// id でクリップを取り除く。返り値は `(取り除いたクリップ, セルだったか)` で、
    /// アレンジのクリップ (`clips`) とランチャーのセル (`session_clips`) の
    /// **どちらに居ても**消せる (住所は 1 つの id 空間なので呼ぶ側は区別しない)。
    pub fn remove_clip_by_id(&mut self, clip_id: u32) -> Option<(Clip, bool)> {
        if let Some(i) = self.clips.iter().position(|c| c.id == clip_id) {
            return Some((self.clips.remove(i), false));
        }
        let i = self.session_clips.iter().position(|c| c.clip.id == clip_id)?;
        Some((self.session_clips.remove(i).clip, true))
    }

    /// アレンジにクリップを置く**唯一の口**。
    ///
    /// `clip` の占有区間に掛かる既存クリップを上書き規則で削り取ってから push する
    /// (`Track::clips` の非重なり不変条件)。 同じ id のクリップが既に居れば取り除いてから
    /// 置き直すので、「このクリップをここへ動かす」にもそのまま使える (冪等)。
    /// `id == 0` なら採番する。 返り値は確定した `Clip::id`。
    pub fn place_clip(&mut self, mut clip: Clip) -> u32 {
        if clip.id == 0 {
            clip.id = self.alloc_clip_id();
        }
        let id = clip.id;
        self.clips.retain(|c| c.id != id);
        let (start, end) = clip.song_window();
        self.carve_clip_range(start, end, None);
        self.clips.push(clip);
        id
    }

    /// `[start, end)` に掛かる既存クリップを**上書き規則**で削り取る。
    /// 規則の実装は [`crate::model::carve_range`] 1 本
    /// (automation クリップと共有 — 2 か所に書くと片方だけ直って静かに食い違う)。
    ///
    /// `except_clip_id` はその id のクリップを対象から外す (自分自身を削らないため)。
    /// 返り値は 1 つでも変更したか。
    pub fn carve_clip_range(
        &mut self,
        start: f64,
        end: f64,
        except_clip_id: Option<u32>,
    ) -> bool {
        // 分割断片の id は **実在 id と衝突させない** ([`Track::alloc_clip_id`] と同じ規則)。
        // counter だけを信じると、counter が遅れている Track (旧ファイル / 手組み) で
        // 断片が既存クリップと同じ id を持ち、`place_clip` の「同じ id を置き直す」
        // 規則に巻き込まれて無関係なクリップが黙って消える。
        let mut next = self.next_clip_id.max(1).max(
            self.all_clips().map(|c| c.id).max().unwrap_or(0).saturating_add(1),
        );
        let changed = crate::model::carve_range(&mut self.clips, start, end, except_clip_id, || {
            let id = next;
            next = next.saturating_add(1);
            id
        });
        self.next_clip_id = next;
        changed
    }

    /// 既存の重なりを解消する (プロジェクト読み込み時の移行)。
    ///
    /// **配列順で後ろにあるもの (= 描画で前面に来ているもの) が勝つ。** 返り値は
    /// 1 つでも変更したか (= `*` を立てるか)。 **冪等** — 2 回目以降は重なりが無いので
    /// `false` を返す (開き直すだけで `*` が立たない)。
    pub fn resolve_clip_overlaps(&mut self) -> bool {
        let mut next = self.next_clip_id.max(1).max(
            self.all_clips().map(|c| c.id).max().unwrap_or(0).saturating_add(1),
        );
        let changed = crate::model::resolve_overlaps(&mut self.clips, || {
            let id = next;
            next = next.saturating_add(1);
            id
        });
        self.next_clip_id = next;
        changed
    }

    /// Allocate a new stable lane id, bumping the per-track counter.
    pub fn alloc_lane_id(&mut self) -> u32 {
        let id = self.next_lane_id.max(1);
        self.next_lane_id = id.saturating_add(1);
        id
    }

    /// Re-assign stable ids to all automation lanes and the clips
    /// inside each lane. Idempotent (lanes / clips with non-zero ids
    /// are left alone, counters are bumped above the max seen).
    pub fn ensure_lane_ids(&mut self) {
        for lane in &mut self.automation_lanes {
            if lane.id == 0 {
                lane.id = self.next_lane_id.max(1);
                self.next_lane_id = lane.id + 1;
            } else if lane.id >= self.next_lane_id {
                self.next_lane_id = lane.id + 1;
            }
            lane.ensure_clip_ids();
        }
        if self.next_lane_id == 0 {
            self.next_lane_id = 1;
        }
    }

    pub fn lane_index_by_id(&self, lane_id: u32) -> Option<usize> {
        self.automation_lanes.iter().position(|l| l.id == lane_id)
    }

    pub fn lane_by_id(&self, lane_id: u32) -> Option<&AutomationLane> {
        self.automation_lanes.iter().find(|l| l.id == lane_id)
    }

    pub fn lane_by_id_mut(&mut self, lane_id: u32) -> Option<&mut AutomationLane> {
        self.automation_lanes.iter_mut().find(|l| l.id == lane_id)
    }

    /// v23 migration: 旧 3-split を devices へ平坦化 (midi_fx ++ instrument? ++ fx) し、
    /// automation lane の旧 slot を device_index へ写像する。新形式 (devices 既存) は no-op。
    /// v29: send 安定 id を採番する。 sentinel (0) のみ上書き、 既存非 0 id
    /// は counter を bump するだけ。 `Song::ensure_ids` が load 時に呼ぶ。
    pub(crate) fn ensure_send_ids(&mut self) {
        for send in &mut self.sends {
            if send.id == 0 {
                let new_id = self.next_send_id.max(1);
                self.next_send_id = new_id + 1;
                send.id = new_id;
            } else if send.id >= self.next_send_id {
                self.next_send_id = send.id + 1;
            }
        }
        if self.next_send_id == 0 {
            self.next_send_id = 1;
        }
    }

    /// v29: 新規 send 用の安定 id を採番する。
    pub fn alloc_send_id(&mut self) -> u32 {
        let id = self.next_send_id.max(1);
        self.next_send_id = id.saturating_add(1);
        id
    }
}
