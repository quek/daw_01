# r.md #71 — プラグインのコピー / 移動 (実装計画)

> **この計画は #71 専用であり、他項目との統合順は `docs/plan_rmd_index.md` を見ること。**

> r.md:11 「71. Plugin をコピペしたいです。他のトラックに同じプラグインを設定・移動できるように。」

この計画書だけで完走できるように書いてある。**フェーズ分けはしない** — §A から §F まで
1 本の変更として仕上げる (途中状態はコンパイルが通らない箇所が多数ある。それでよい)。

---

## 0. 最終形 (これが「できあがり」の定義)

- インスペクタの Chain 行を **選択できる** (無修飾 / Ctrl / Shift クリック)。
- 選択したプラグインに **Ctrl+C / Ctrl+X / Ctrl+V / Delete / D (複製)** が効く。
  対象面は既存の last-selection-wins arbiter (`EditSurface`) で決まる。
- チェーン行の **右クリックメニュー**に コピー / 切り取り / 貼り付け / 複製 / 削除。
- チェーン行を **掴んで別トラックへ運べる**。ドラッグ中に **アレンジのトラックヘッダ**へ
  カーソルを持っていくとインスペクタの表示がそのトラックのチェーンへ切り替わり、
  そのままインスペクタに戻って挿入位置を決めて離せる。トラックヘッダの上で離したら
  そのトラックのチェーン **末尾**に入る。落とした後は落とし先のトラックを表示し続ける。
- **既定は移動。Ctrl を押しながら離すとコピー**。
- **Ctrl+V の挿入位置は「選んでいるプラグインの直前」、無選択なら末尾**。
- 移動はプラグインインスタンスを作り直さない (音が切れない)。automation lane と
  mod routing は一緒に運ぶ。コピーはツマミの現在値 (state) だけ引き継ぎ、
  automation / 変調は複製しない。
- 上を成立させるために、daw_gui の per-device 帳簿と **device を運ぶ `AppEvent` を、
  positional `(track_id, device_index)` から安定 `device_id: u64` へ全面置換**し、
  `scripts/arch_lint_baseline.txt` の POSITIONAL-KEY 4 件を完済する。あわせて
  `scripts/arch_lint.sh` の検出を「1 行の `HashMap<(u32, u32)` しか見ない」状態から
  **連想コンテナのタプルキー (折り返し定義を含む)** を漏れなく見る形に作り直す (§A-10)。

### やらないこと (明示的な決定。実装者が善意で足さないこと)

- 「オートメーションごとコピー」という別コマンドは **作らない**。オートメーションを
  運びたければオートメーションクリップを D&D する (`handler/automation.rs:1277`
  `move_automation_clips` が既にレーン / トラックを跨いだ移動に対応している)。
- ミキサーストリップにチェーン欄は **作らない**。ドラッグ中の表示切り替えのトリガは
  アレンジのトラックヘッダだけ。
- `PluginCommand::UnloadAllPlugins` は **廃止しない** (§E-3 に理由)。

---

## 0.5 触るファイル一覧 (これで全部。ここに無いファイルは触らなくてよい)

**新規**

| ファイル | 何を置くか | 節 |
| --- | --- | --- |
| `ui/crates/ui/src/drag_drop.rs` | widget 跨ぎ drag payload チャネル (型消去) | D-2 |
| `daw_gui/src/view/track_inspector/device_panel.rs` | `mod.rs` の巨大 expansion closure を移す先 (god file budget) | D-4 |
| `daw_gui/tests/app_state/device_relocate.rs` | 移動 / コピーの headless テスト | F-1 |

**既存 (変更)**

| ファイル | 主な変更 | 節 |
| --- | --- | --- |
| `common/src/protocol.rs` | `SetSlotPlugin.track_id` 撤去 / `RemoveTrack` 削除 / doc 3 か所 / roundtrip test | E-1 |
| `common/src/model.rs` | `ensure_ids` の device 重複 id 検出 / `BindingTarget::PluginParam.track` を legacy 化 | A-8, A-9 |
| `common/src/model/tests.rs` | 上の 2 件のテスト | A-8, A-9 |
| `daw_plugin_host/src/main.rs` | `InstanceRecord.track_id` 撤去 / `RemoveTrack` arm 削除 | E-2 |
| `daw_gui/src/state/ipc.rs` | 帳簿 7 フィールドを device_id keyed に + `track_plugin_ids` 削除 | A-1 |
| `daw_gui/src/state/ui_ephemeral.rs` | `open_video_fx_params` / `open_plugin_params` を `Option<u64>` に | A-2 |
| `daw_gui/src/state/selection.rs` | `selected_device_ids` / `device_anchor` 追加 | D-1 |
| `daw_gui/src/state/ui_prefs.rs` | 変更なし (`automation_lane_row_overrides` を **読み書きするだけ**) | B-4 |
| `daw_gui/src/app.rs` | 各 cache の初期化 (`:218-237` / `:407-408`) **+ `handle_event` の device 系 dispatch arm 10 個 (`:1417-1468` / `:1509-1514`)** + 新規 2 arm | A-7 |
| `daw_gui/src/app_types.rs` | `LoadedDeviceInfo` / `SlotReconcileAction` / `TrackRemovalIpc` / `ChainEntry` / `SidechainEntry` / `ParallelOutputEntry` / `InspectorScrubField` / `VideoFxParamsInspector` / `PluginParamsInspector` / `EditSurface` / `DeferredEdit` / `PendingStateRequest` / 新規型 3 つ | A-3, A-7, B-1, C-4, D-1, D-4 |
| `daw_gui/src/event.rs` | device を運ぶ `AppEvent` 10 変種を device_id 化 + 新規 2 変種 + undo ラベル表 | A-7, B-1, D-1 |
| `daw_gui/src/clipboard.rs` | `ClipboardPayload::Devices` / `DeviceCopy` / `sanitize_devices` / blob 上限 | C-1〜C-3 |
| `daw_gui/src/script.rs` | write-only な `plugin_to_track` / `track_plugin_ids` / `resolve_device_coords` 削除、`SetSlotPlugin` 直送から `track_id` を除く | A-7 |
| `daw_gui/src/app_tests.rs` | `mod master_fx_tests` (`:866-894`) の reconcile テストが `SlotReconcileAction::LoadSlot { track_id, index, .. }` を destructure しており **確実にコンパイルが壊れる** | A-11 |
| `daw_gui/src/handler/devices.rs` | 補償コード 3 関数の削除 / 全 API の device_id 化 / `relocate_devices` / `paste_devices` / `copy_devices` / `cut_devices` | A-5, B-2, B-3, C-4 |
| `daw_gui/src/handler/project.rs` | reconcile Phase A 削除 / `restore_device` 集約 / teardown / `send_set_slot_plugin` | A-6 |
| `daw_gui/src/handler/ipc.rs` | param 系 4 arm の逆引き除去 | A-4 |
| `daw_gui/src/handler/mixer.rs` | `pending_added_plugin_finalize` / `send_set_slot_plugin` の引数 | A-7 |
| `daw_gui/src/handler/grouping.rs` | `plan_track_removal_ipc` を唯一の口として復活 + unit test | E-3 |
| `daw_gui/src/handler/tracks.rs` | track 削除の IPC を plan 経由に / `focus_inspector_track` 新設 | B-6, E-3 |
| `daw_gui/src/handler/view_model.rs` | `plugin_params` の引き方 / `plugin_param_range` の引数 / `inspector_chain` | A-7 |
| `daw_gui/src/handler/automation_lanes.rs` | video fx / plugin param パネルの device_id 化 + `set_plugin_param` / `set_plugin_param_on_track` の 2 本を 1 本に畳む | A-7 |
| `daw_gui/src/handler/tick.rs` | `plugin_param_values` の引き方 | A-7 |
| `daw_gui/src/handler/bounce.rs` | VOICEVOX device id の引き方 | A-7 |
| `daw_gui/src/handler/voicevox.rs` | 同上 (2 か所) | A-7 |
| `daw_gui/src/handler/midi.rs` | `set_plugin_param` (統合後の 1 本) / `midi_learn_binding_target` | A-7, A-9 |
| `daw_gui/src/handler/modulation.rs` | `set_aux_input_tap_point(:390)` を device_id 化 (`SetAuxInputTapPoint` の受け口。**初稿の一覧に無かった。ここを直さないと `AppEvent` 変更でコンパイルが壊れる**) | A-7 |
| `daw_gui/src/handler/selection_view.rs` | `edit_surface` / `delete_current_surface` に Devices 面 (対象 id は `live_device_ids()` 経由) | D-1 |
| `daw_gui/src/view/root.rs` | copy / cut / paste / 複製ショートカットに Devices 面 + 運搬プレビュー | C-4, D-6 |
| `daw_gui/src/view/track_inspector/mod.rs` | chain 配線 (選択 / drag / メニュー) + expansion closure の移設 | D-4 |
| `daw_gui/src/view/track_inspector/chain_sections.rs` | `entry.device_index` から撃っている 6 種の AppEvent を device_id に (10 か所) | A-7 |
| `daw_gui/src/view/arrangement_view.rs` | トラックヘッダへの drop / 表示切り替え | D-5 |
| `daw_gui/src/view/resource_monitor.rs` | `track_plugin_ids` の代替 | A-7 |
| `daw_gui/src/widgets/arrangement/run.rs` | drop フレームのトラック選択を抑止 | D-5 |
| `daw_gui/src/widgets/arrangement/view_build.rs` | `plugin_param_range` の呼び出し 2 か所 | A-7 |
| `ui/crates/ui/src/ui.rs` | `UiHost.drag_payload` フィールド + `Ui` へ `&'a mut Option<DragPayload>` を通す + **`frame_to_edits_with_fonts` (`:591`) の頭と末尾**に寿命 hook | D-2 |
| `ui/crates/ui/src/lib.rs` | `pub mod drag_drop;` + `DragPayload` 再 export + **`pub use daw_ui_platform::Modifiers;`** (`:42` の `CursorIcon` 再 export の隣。public フィールドに `Modifiers` を出すのに要る) | D-2, D-3 |
| `ui/crates/ui/src/widgets/reorderable_list.rs` | 複数選択 / 運び出し / 外部 drop / row rect / press 修飾キー | D-3 |
| `ui/crates/ui/tests/ui/pass/basic.rs` | `reorderable_list` の呼び出し (trybuild pass ケース) | D-3 |
| `scripts/arch_lint.sh` | POSITIONAL-KEY 検査の作り直し + canary | A-10 |
| `scripts/arch_lint_baseline.txt` | POSITIONAL-KEY 4 行 + 理由ブロックの削除 | A-10 |
| `daw_gui/tests/reconcile_slot_diff.rs` | `loaded_devices` / 新 action | A-11 |
| `daw_gui/tests/voicevox_progress.rs` | `loaded_slots.insert` | A-11 |
| `daw_gui/tests/app_state/{main.rs,group_track_lifecycle.rs,transform_edit_regress.rs,pending_state_queue.rs,plugin_load_failure.rs}` | 帳簿 / AppEvent / IPC assert の追随 (`main.rs` は `mod device_relocate;` の 1 行)。`support.rs` は **変更不要** | A-11 |
| `daw_gui/tests/scripts/device_chain_smoke.js` | 移動後の `deviceChain(track)` の並び / ports の assert を **足すだけ** (実行はユーザー判断。この target は daw_gui を起動する) | F-4 |

**読むだけ (変更しない)**: `daw_gui/src/widgets/select_modifier.rs` (`range_ordered` を使う)、
`daw_gui/src/handler/sync.rs` (`flush_song_sync` を呼ぶだけ。**`ara_doc_cache` は
触らない** — 理由は B-5)、
`daw_gui/src/view/mixer_strips.rs` (ミキサーには何も足さない)、`daw_audio/**` (Song 追従のみ)。

---

## 1. いまの実装の要点 (実装者が前提として知るべきこと)

- device chain の **リスト**を描いているのは
  `daw_gui/src/view/track_inspector/mod.rs:1087-2492` の 1 か所だけで、
  `ui.reorderable_list_expandable` にカーソルトラック 1 本分を流している。
  ただし呼び出しは `:1116` から **`:2489` まで続いており**、その `expansion`
  クロージャ (`:1195-2488`) がインスペクタ本体のほぼ全部を抱えている (D-4-0 で割る)。
  `daw_gui/src/view/mixer_strips.rs` には `device` の語が 1 つも無い (grep で 0 件)。
- **ただし「device を対象に AppEvent を撃つ view」はもう 1 つある**:
  `daw_gui/src/view/track_inspector/chain_sections.rs` (チェーン直下の
  「読み込み失敗」/「キーを全部送る」/ パラアウト / サイドチェイン セクション) が
  `entry.device_index` から 6 種の AppEvent を 10 か所で撃っている。
  device_id 化はここまでやらないとコンパイルが通らない (A-7)。
- device の追加は `daw_gui/src/handler/mixer.rs:35` `select_plugin_from_db` だけ
  (チェーン末尾 append)。削除は `daw_gui/src/handler/devices.rs:992` `remove_device`。
  **移動 / コピーは存在しない** (`move_device` / `copy_device` の grep が 0 件)。
- daw_gui の per-device 帳簿が positional キー:
  - `daw_gui/src/state/ipc.rs:39` `plugin_param_values: HashMap<(u32,u32,u32), f64>`
  - `daw_gui/src/state/ipc.rs:48` `plugin_params: HashMap<(u32,u32), Vec<PluginParamInfo>>`
  - `daw_gui/src/state/ipc.rs:57` `slot_has_gui: HashMap<(u32,u32), bool>`
  - `daw_gui/src/state/ipc.rs:67` `track_plugin_ids: HashMap<u32, Vec<u64>>`
  - `daw_gui/src/state/ipc.rs:78` `loaded_slots: HashMap<(u32,u32), LoadedSlotInfo>`
  - `daw_gui/src/state/ipc.rs:148` `open_plugin_guis: HashSet<(u32,u32)>`
  - `daw_gui/src/state/ipc.rs:182` `pending_added_plugin_finalize: HashMap<(u32,u32), bool>`
  - `daw_gui/src/state/ipc.rs:187` `gui_open_requests: Vec<(u32,u32)>`
  - `daw_gui/src/state/ui_ephemeral.rs:262` `open_video_fx_params: Option<(u32,u32)>`
  - `daw_gui/src/state/ui_ephemeral.rs:267` `open_plugin_params: Option<(u32,u32)>`
  その補償として `daw_gui/src/handler/devices.rs:1148` `shift_device_caches_after_remove` /
  `:1221` `shift_slot_gui_keys` / `:762` `apply_chain_reorder` が「削除・並べ替えのたびに
  index を 1 つ詰める」コードを持つ。**CLAUDE.md 不変条件 1 が名指しで禁じている
  「削除/並べ替えで参照を貼り替える補償コード」そのもの**で、
  `scripts/arch_lint_baseline.txt:20-31` に 4 件の既知負債として登録済み。
- 子プロセス側は既に device_id 一本:
  - `daw_plugin_host/src/main.rs:109` `InstanceRecord` は `HashMap<u64, InstanceRecord>` の値。
    `track_id` (`:114`) は `PluginCommand::RemoveTrack` (`:1013`) の列挙にしか使わない。
  - `daw_audio/src/engine.rs:407` `pub type PluginRefs = HashMap<u64, Arc<PluginEntry>>`
    (= device_id keyed)。差し替えは `daw_audio/src/main.rs:1197-1219` の
    snapshot-copy-mutate-publish。処理順は Song から `compile_schedule` が導く
    (= `LoadSong` を送れば追従する)。
- automation は **トラック所有**なので device の移動に自動追従しない。
  `daw_audio/src/automation.rs:145` `fill_pd_param_events` は `:169-178` で `track_id` から
  `automation_lanes` / `mod_routings` を引き、`:203` で `device_id` 一致の lane だけ適用する。
  **lane を元トラックに置いたまま device だけ移すと、その lane は永久に効かなくなる。**
- shmem 名は `common/src/plugin_ref.rs:200` `process_data_shmem_id(pid, device_id, incarnation)` で
  incarnation が一意性を担保するので、コピーで新 id を振っても移動で id を据え置いても
  `[[project_shmem_name_reuse_race]]` の再発は構造的に起きない。

---

## §A 基盤: per-device 帳簿を安定 `device_id` へ全面置換する

**これを先にやらないと本体は作れない。** 移動・コピーは `(track_id, device_index)` キーの
帳簿がある限り「貼り替え補償コード」を増やす方向にしか実装できない。

### A-1. `daw_gui/src/state/ipc.rs` — フィールド定義

`(track_id, device_index)` を含む 6 フィールドを書き換える。**タプルキーを新設しない**
(arch-lint のパターンを拡張しても引っかからない形にする = 名前付きの id キーにする)。

```rust
/// `(device_id, param_id)` の複合キー。 生タプルにしないのは、
/// positional キーと見分けが付かなくなる (arch-lint / 読み手の双方) ため。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceParamKey {
    pub device_id: u64,
    pub param_id: u32,
}
```

| 旧 | 新 |
| --- | --- |
| `plugin_param_values: HashMap<(u32,u32,u32), f64>` | `plugin_param_values: HashMap<DeviceParamKey, f64>` |
| `plugin_params: HashMap<(u32,u32), Vec<PluginParamInfo>>` | `plugin_params: HashMap<u64, Vec<PluginParamInfo>>` |
| `slot_has_gui: HashMap<(u32,u32), bool>` | `slot_has_gui: HashMap<u64, bool>` |
| `loaded_slots: HashMap<(u32,u32), LoadedSlotInfo>` | `loaded_devices: HashMap<u64, LoadedDeviceInfo>` (**改名**) |
| `track_plugin_ids: HashMap<u32, Vec<u64>>` | **削除** (A-6 参照) |
| `open_plugin_guis: HashSet<(u32,u32)>` | `open_plugin_guis: HashSet<u64>` |
| `pending_added_plugin_finalize: HashMap<(u32,u32), bool>` | `pending_added_plugin_finalize: HashMap<u64, bool>` |
| `gui_open_requests: Vec<(u32,u32)>` | `gui_open_requests: Vec<u64>` |

`loaded_slots` → `loaded_devices` の改名は必須。"slot" は positional 語彙で、
device_id keyed になった後は誤解の元になる。doc コメントも「(track, device_index) →」を
「device_id →」に書き直し、「更新タイミング」の記述 (`_inner` 関数内で track / device index
単位で remove) も device_id 単位に直す。

`track_plugin_ids` の doc が説明していた責務 (「host に実際に載った device」を保持して
`RemoveTrack` 前に `ClosePluginShmem` を撃つ) は `loaded_devices` が引き継ぐ。

### A-2. `daw_gui/src/state/ui_ephemeral.rs:262/267`

```rust
/// 内蔵映像 FX は plugin window を持たないので、チェーン行の "GUI" ボタンは
/// インスペクタ内のパラメータ調整パネルを開く。`Some(device_id)` で 1 つだけ開く。
/// cursor track 以外の device を指していたら描画側 gate が非表示にする。
pub open_video_fx_params: Option<u64>,
/// 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し CLAP・VST3) の
/// 「⚙」ボタンで開くインライン param パネル。`open_video_fx_params` と同 idiom。
pub open_plugin_params: Option<u64>,
```

「cursor track 以外に切り替えたら閉じる」という旧仕様は、**閉じるのではなく描画 gate**
に変える (`find_device_by_id` で引いた track が cursor track と一致するときだけ描く)。
device が song から消えたときだけ `None` に落とす。これで移動しても開きっぱなしの
パネルが自然に追従する。

### A-3. `daw_gui/src/app_types.rs`

- `:1477-1489` `LoadedSlotInfo` → `LoadedDeviceInfo` に改名し、`device_id` フィールドを
  **削除** (キーになったので値に持つのは複製)。残るのは `plugin_id_str: String` だけ。
  doc (`:1477` の「`loaded_slots` の値: 1 つの (track, device_index) ペア…」) も
  device_id keyed 前提に書き直す。
- `:1491-1508` `SlotReconcileAction` を device_id ベースに:

```rust
/// `reconcile_plugins_with_song` の Phase B が計算する action。
/// v34 (r.md #71): アドレスは安定 `device_id` 一本。track / chain 内 index は
/// 出てこない (host は帰属も順序も持たない)。
#[derive(Debug, Clone, PartialEq)]
pub enum SlotReconcileAction {
    /// host にあるが Song に無い device を host から消す。
    RemoveDevice { device_id: u64 },
    /// Song にあるが host に無い / plugin_id_str が違う device を (再) load する。
    LoadDevice {
        device_id: u64,
        plugin_id_str: String,
        initial_state: Option<Vec<u8>>,
    },
}
```

- `:1517` `compute_slot_reconcile_actions(song, loaded_devices: &HashMap<u64, LoadedDeviceInfo>)`
  を書き直す。track ループは残す (Song を走査するのに使う) が、`host_slot_indices` の
  index 集合は不要になる。**`inst.ports.is_video()` の除外は残す** — 内蔵映像 FX は
  plugin_host に載らない device で、id 集合に混ぜると毎回 `LoadDevice` が出て
  「load 応答が来ない device」が永久に溜まる (現行 `:1531-1540` の doc が理由を書いている):

```rust
pub fn compute_slot_reconcile_actions(
    song: &common::model::Song,
    loaded_devices: &HashMap<u64, LoadedDeviceInfo>,
) -> Vec<SlotReconcileAction> {
    // Song 側で host slot を持つ device (= 映像でない device) の id 集合。
    let mut song_host_ids: std::collections::HashSet<u64> = HashSet::new();
    let mut actions = Vec::new();
    // (2) Song にあるが host に無い / plugin_id_str が違う → LoadDevice
    //     順序は Song の走査順 = 音の処理順で決定的。
    let mut visit = |devices: &[PluginInstance], actions: &mut Vec<_>| { ... };
    for track in &song.tracks { visit(&track.devices, &mut actions); }
    visit(&song.master_fx_chain, &mut actions);
    // (1) host にあるが Song に無い → RemoveDevice。id 昇順 sort で決定的に。
    ...
}
```

**RemoveDevice を先に push すること** (現行と同じ順序: 余剰を落としてから load)。

`id == 0` (未採番) の device は Song 側集合に入っても `send_set_slot_plugin` が
`device id unallocated` で error 出力して skip する (`handler/project.rs:1265-1268`、
既存の guard)。**ここで黙って skip しない** — 0 が来るのは A-8 の `ensure_ids` を
通っていない Song が漏れた設計バグで、握りつぶすと原因が見えなくなる。

- `:19-21` `TrackRemovalIpc::RemoveTrackFromPluginHost { track_id: u32 }` →
  `RemoveHostDevice { device_id: u64 }` に変更 (§E)。

### A-4. `daw_gui/src/handler/ipc.rs` — positional cache の**書き込み側**

- `:288-299` `PluginEvent::PluginParamList` の arm: `find_device_by_id` の逆引きを
  **丸ごと削除**して

```rust
self.ipc.plugin_params.insert(device_id, params);
self.ipc.slot_has_gui.insert(device_id, has_embedded_gui);
```

- `:333-344` `PluginEvent::PluginParamValueChanged` の arm:

```rust
self.ipc
    .plugin_param_values
    .insert(DeviceParamKey { device_id, param_id }, value);
```

- `:300` `PluginParamTouched` / `:346` `PluginParamGestureEnd` は `track` を
  `ParamGestureBegin` / `End` に渡すためだけに逆引きしている。ここは **残す**
  (recording gesture の鍵が `(track_id, target)` なので track が要る。
  `state/recording.rs:73/83/90` の 3 つが `(u32, AutomationTarget)` keyed)。
  `let Some((track, _index)) = ...` の `_index` を捨てる形に整理する。

### A-5. `daw_gui/src/handler/devices.rs`

**削除する関数 (丸ごと)**
- `:1148` `shift_device_caches_after_remove`
- `:1221` `shift_slot_gui_keys`
- `:762` `apply_chain_reorder`

**書き換える関数**

- `:33` `on_gui_closed(device_id)`: `find_device_by_id` の逆引きをやめ
  `self.ipc.open_plugin_guis.remove(&device_id)`。ログの `slot = ?slot` は落として
  `device_id` だけにする。
- `:55` `on_plugin_loaded_from_child`: `coords` の逆引きは **song 再構築に必要なので残す**
  (chain の該当位置に `PluginInstance` を書き戻すため)。ただし
  - `:118-121` `track_plugin_ids` への登録 → 削除
  - `:124-130` → `self.ipc.loaded_devices.insert(device_id, LoadedDeviceInfo { plugin_id_str: id.clone() });`
  - `:273-277` → `if let Some(open_gui) = self.ipc.pending_added_plugin_finalize.remove(&device_id) && open_gui { self.ipc.gui_open_requests.push(device_id); }`
- `:318` `on_plugin_load_failed_from_child` の
  `self.ipc.pending_added_plugin_finalize.remove(&(track, index))` (`:353`) → `remove(&device_id)`。
- `:387` `reload_device(track_id, device_index)` → **`reload_device(device_id: u64)`**。
  `device_at` の代わりに `song.fx_chain_by_track_id` 経由の逆引きを 1 回だけ行う
  (`find_device_by_id` → `device_at`)。`AppEvent::ReloadDevice` も `{ device_id: u64 }` に。
- `:430` `on_plugin_unloaded_from_child`: `track_plugin_ids` の 3 行を削除、
  `loaded_devices.remove(&device_id)` に置換。あわせて `plugin_params` /
  `slot_has_gui` / `plugin_param_values` からも当該 device の entry を落とす
  (positional 時代は「shift で拾えるから」放置していたが、id keyed では明示的に消すのが
  正しい。`plugin_param_values` は `retain(|k, _| k.device_id != device_id)`)。
- `:474` `toggle_slot_gui(index: u32)` → **`toggle_slot_gui(device_id: u64)`**。
  cursor_track_id 依存を消す。device 本体は `find_device_by_id` + `device_at` で引く
  (master も同じ経路で引ける)。`open_video_fx_params` / `open_plugin_params` /
  `open_plugin_guis` はすべて device_id で比較。
- `:550` `open_slot_gui(track_id, index)` → **`open_slot_gui(device_id: u64)`**。
  窓タイトル用の label は `find_device_by_id` で track を引いてから組む。
- `:647` `drain_pending_gui_opens`: `for device_id in take(&mut gui_open_requests) { self.open_slot_gui(device_id); }`
- `:657` `slot_ref_name(track, index)` → `device_display_name(device_id)` に整理して
  `open_slot_gui` から呼ぶ (`#[cfg(windows)]` はそのまま)。
- `:669` `reorder_inspector_chain(order)`: **`fully_loaded` ガード
  (`:700-727`) と status_message を削除**。id keyed になれば「ロード中は並べ替え
  できない」制約の理由 (positional cache の再キーがずれる) が消える。末尾の
  `self.apply_chain_reorder(track_id, moves);` と `moves` の構築も削除。
  残るのは「permutation 検証 → `edit_song` でチェーンを差し替え」だけ。
- `:812` `set_plugin_send_all_keys(track_id, device_index, enabled)` →
  `(device_id, enabled)`。`:839` `set_sidechain_source` / `:877`
  `set_parallel_output_route` / `:914` `explode_parallel_out` も同様に
  第 1・2 引数を `device_id: u64` へ統一する (呼び出し側は inspector view)。
- `:992` `remove_device(index)` → **`remove_devices(device_ids: Vec<u64>)`**
  (複数選択を 1 undo step で消すため)。`DeferredEdit::RemoveDevice { track_id, index }` →
  `DeferredEdit::RemoveDevices { device_ids: Vec<u64> }`。
- `:1012` `remove_device_inner(track_id, index)` → `remove_devices_inner(device_ids: &[u64])`。
  1 回の `edit_song` の中で全 device を削除する。中身の変更点:
  - `cleanup_slot_gui(track_id, index)` → `cleanup_slot_gui(device_id)` (下記)
  - `open_video_fx_params` / `open_plugin_params` は「同 track なら閉じる」から
    「**削除される device を指していたら閉じる**」に厳密化
  - `loaded_devices.remove(&device_id)` / `plugin_params` / `slot_has_gui` /
    `plugin_param_values` の掃除。`failed_plugin_loads.remove(&device_id)` (現行 `:1029`)
    と `RemoveSlotPlugin` 送信 (`:1023`) はそのまま device ごとに行う
  - `shift_device_caches_after_remove` 呼び出しを削除
  - `remap_device_refs_after_remove(track_id, removed_id)` は残す (dangling lane 除去)
  - **VOICEVOX 副作用にガードを足す**: 現行 `:1066-1074` は「VOICEVOX builtin を外したら
    `track.source = None`」を**無条件**で行う (Transform 側 `:1078-1088` だけが
    「同 track に別の Transform が残っていれば保持」を見ている)。複数選択削除で
    「2 本ある VOICEVOX の 1 本だけ消す」が成立するようになるので、VOICEVOX 側にも
    `!track.devices.iter().any(|d| d.plugin_id == BUILTIN_ID_VOICEVOX)` の
    ガードを入れて Transform と対称にする (`[[feedback_sibling_occurrence_check]]`。
    §B-3 の移動側と同じ規則になり、規則が 1 つで済む)
  - `selection.selected_device_ids` から消えた id を落とし、空になったら
    `last_edit_select == Some(EditSurface::Devices)` を降ろす (D-1)
- `:1197` `cleanup_slot_gui(track_id, index)` → `cleanup_slot_gui(device_id: u64)`。
  `open_plugin_guis.remove(&device_id)` して `CloseSlotGui { device_id }` を送るだけ。
  `shift_slot_gui_keys` 呼び出しは無くなる。`#[cfg(not(windows))]` 版も同シグネチャ。

### A-6. `daw_gui/src/handler/project.rs`

- `:97-101` の cache 掃除: `track_plugin_ids.clear()` を削除、
  `plugin_param_values.clear()` / `gui_open_requests.clear()` はそのまま。
- `:1045` `reconcile_plugins_with_song` の **Phase A (`:1046-1084`) を丸ごと削除**する。
  「host にあるが Song に無い track」という概念は device 粒度の diff に吸収される
  (Phase B が `RemoveDevice` を出す)。track 単位の `RemoveTrack` も消える (§E)。
  **`:1088-1096` の `plugin_db.is_none()` 早期 return は残す** (SetSlotPlugin を組み立て
  られないので Phase B ごと skip する既存仕様。下のコード片は match 部分だけを示している)。
  残る Phase B (`:1086-1140`) は:

```rust
let actions = compute_slot_reconcile_actions(self.song_doc.song(), &self.ipc.loaded_devices);
for action in actions {
    match action {
        SlotReconcileAction::RemoveDevice { device_id } => {
            self.cleanup_slot_gui(device_id);
            self.send_audio(AudioCommand::ClosePluginShmem { device_id });
            self.send_plugin(PluginCommand::RemoveSlotPlugin { device_id });
            self.ipc.loaded_devices.remove(&device_id);
            self.ipc.pending_plugin_loads.remove(&device_id);
        }
        SlotReconcileAction::LoadDevice { device_id, plugin_id_str, initial_state } => {
            self.send_set_slot_plugin(device_id, &plugin_id_str, initial_state);
        }
    }
}
```

  **`ClosePluginShmem` を `RemoveSlotPlugin` より先に送る順序は死守**
  (`handler/grouping.rs:113-124` の doc が理由を書いている: audio worker が unmapped
  shmem を踏んで silent terminate → `all_done` 永久 wait)。

  **`RemoveDevice` arm の `ClosePluginShmem` は新規追加で、意図的**。現行の Phase B
  (`:1100-1113`) は `RemoveSlotPlugin` しか送っておらず、`ClosePluginShmem` は Phase A
  (`:1069-1071`) が track 単位でまとめて送っていた。Phase A を消す以上、この責務は
  Phase B が device 単位で引き取らないと落ちる。「消し忘れの追加」ではないので消さないこと。

- `:1206-1240` `teardown_all_loaded_plugins`: 列挙元を Song + `loaded_devices` の
  **和集合**に変える (在庫と Song の両方を拾う。片方だけだと load 応答待ち / Song から
  消えた device のどちらかが漏れる)。

```rust
let mut ids: std::collections::HashSet<u64> = self.ipc.loaded_devices.keys().copied().collect();
for t in &self.song_doc.song().tracks { ids.extend(t.devices.iter().map(|d| d.id)); }
ids.extend(self.song_doc.song().master_fx_chain.iter().map(|d| d.id));
for device_id in ids { self.send_audio(AudioCommand::ClosePluginShmem { device_id }); }
// project 切替。`device_id` は Song スコープの名前なので、帳簿に依存しない
// 「全部捨てろ」でしか塞げない (protocol.rs の UnloadAllPlugins doc 参照)。
self.send_plugin(PluginCommand::UnloadAllPlugins);
```

  以降の cache clear は `loaded_devices` / `plugin_params` / `slot_has_gui` /
  `plugin_param_values` / `open_plugin_guis` / `pending_plugin_loads` /
  `pending_added_plugin_finalize` / `failed_plugin_loads` / `gui_open_requests` を全部落とす。

- `:1248` `send_set_slot_plugin(track_id, device_id, plugin_id, initial_state)` →
  **`send_set_slot_plugin(device_id, plugin_id, initial_state)`** (§E で
  `SetSlotPlugin.track_id` が消える)。ログの `track_id` は落とす。
- `:1285` `restore_plugin_from_song` / `:1322` `restore_plugins_for_tracks` は
  **新しい単一の口 `restore_device` を呼ぶ形に集約**する:

```rust
/// plugin_host にこの device を実体化させる **唯一の口**。
/// 内蔵映像 FX (`ports.is_video()`) は plugin_host に載らない device なので skip し
/// `false` を返す。 project 復元 / paste 復元 / device コピー (r.md #71) が全部ここを通る。
pub(crate) fn restore_device(&mut self, inst: &common::model::PluginInstance) -> bool {
    if inst.ports.is_video() {
        return false;
    }
    self.send_set_slot_plugin(inst.id, &inst.plugin_id, inst.state.as_deref().map(<[u8]>::to_vec))
}
```

  `restore_plugin_from_song` / `restore_plugins_for_tracks` は「対象の
  `PluginInstance` を集めて `restore_device` を呼ぶ」だけに縮む。

### A-7. その他の呼び出し側

| ファイル:行 | いまの姿 | 直し方 |
| --- | --- | --- |
| `app.rs:1417-1468` / `:1509-1514` | `handle_event` の device 系 dispatch arm 10 個 (`ToggleSlotGui:1417` / `SetVideoFxParam:1426` / `SetPluginParam:1429` / `RemoveDevice:1432` / `ReloadDevice:1435` / `ExplodeParallelOut:1441` / `SetParallelOutputRoute:1447` / `SetSidechainSource:1455` / `SetPluginSendAllKeys:1463` / `SetAuxInputTapPoint:1509`) | 全部 `device_id` を受けて渡す形に。`RemoveDevice { index }` → `RemoveDevices { device_ids }` は arm の形ごと変わる。あわせて **新規 `SelectDevice` / `RelocateDevices` の arm をここに足す** (D-1 / B-1) |
| `handler/mixer.rs:92-93` | `pending_added_plugin_finalize.insert((track_id, dest_index), open_gui)` | `insert(device_id, open_gui)` |
| `handler/mixer.rs:94` | `send_set_slot_plugin(track_id, device_id, &entry_id, None)` | `send_set_slot_plugin(device_id, &entry_id, None)` |
| `handler/grouping.rs:125-138` | `plan_track_removal_ipc(track_ids, track_plugin_ids)` | `plan_track_removal_ipc(song: &Song, track_ids: &[u32])` — 列挙元を `song.fx_chain_by_track_id(t)` に変え、`CloseAudioShmem{device_id}` を全 device 分 push した後に `RemoveHostDevice{device_id}` を同数 push |
| `handler/grouping.rs:217-221` | `open_plugin_guis.retain(\|&(t,_)\| ...)` (`#[cfg(windows)]`) / `loaded_slots.retain(...)` | 削除する track の device id 集合 (**Song から** 取得。削除前に列挙して保持) で `open_plugin_guis.remove(&id)` / `loaded_devices.remove(&id)` |
| `handler/grouping.rs:243-250` | `track_plugin_ids.remove(group_id)` → ClosePluginShmem → `RemoveTrack` | `plan_track_removal_ipc` 経由 (§E-3)。**`:222` で `song.tracks.remove(pos)` 済みなので、plan は `:207` の `edit_song` より前に計算して保持する** |
| `handler/grouping.rs:326-330` | 最終 track 削除。`open_plugin_guis.retain` → `RemoveTrack` を送るだけで **`ClosePluginShmem` を送っていない** (= §E-3 が仕様として書いている順序が、この経路だけ守られていない) | `plan_track_removal_ipc` 経由に統一することで穴も塞がる。**`:318` の `song.tracks.pop()` より前に plan を計算する** |
| `handler/tracks.rs:555-585` | `loaded_slots.retain` (`:567`) / `track_plugin_ids.remove` (`:579`) / `RemoveTrack` (`:584`) | `plan_track_removal_ipc` 経由 (§E-3)。**`:557-571` の削除ループより前に plan を計算する** (ループが `song.tracks.remove` するので、後からでは列挙できない) |
| `handler/tracks.rs:312-353` | `build_pasted_tracks` の device_remap (`:317-340`) + `aux_inputs` の dangling 解決 (`:341-353`) | **変更なし** (元から新 id 採番している。§B のコピーが参照すべき既存実装)。ただし **`aux_outputs` はここでは remap されていない** ので、§B / §C はこの穴を真似しないこと (`common/src/model.rs:1956/1974` の `ensure_ids` 側は両方 remap している) |
| `handler/view_model.rs:522-538` | `plugin_param_range(&self, track_id: u32, target)` — `:533` に既に `let _ = track_id;` があり、逆引きを消すと第 1 引数が **完全に未使用** になる (`make clippy` は `-D warnings`) | シグネチャから `track_id` を **削る**。`plugin_params.get(&device_id)` で引き、`find_device_by_id` の逆引きごと消す。呼び出し **7 か所** (grep 実測 2026-08-28) を追随: `handler/midi.rs:231`、`handler/view_model.rs:615`、`view/arrangement_view.rs:351/423/492`、`widgets/arrangement/view_build.rs:249/330` |
| `handler/view_model.rs:566-576` | `plugin_param_name` の `plugin_params.get(&(track_id, device_index))` | `get(&device_id)`。`find_device_by_id` → `device_at` の逆引きは **表示名 (device_label) に必要なので残す** |
| `handler/view_model.rs:961-1008` | `inspector_chain()` が `slot_has_gui` / `plugin_params` を `(track_id, device_index)` で引く | `ChainEntry` に `pub device_id: u64` を足し (`app_types.rs:182-203`)、`slot_has_gui` / `plugin_params` / `failed_plugin_loads` を全部 `p.id` で引く。`device_index` フィールド (`:183`) と `ChainEntry::to_device_index()` (`app_types.rs:206-208`、呼び出し 0 件) は **削除** — 表示順は Vec の位置が持つ |
| `handler/view_model.rs:274-300` | `sidechain_entries()` が `SidechainEntry { track_id, device_index }` を作る | `SidechainEntry` (`app_types.rs:652-661`) の `device_index` → `device_id: u64` (`p.id`)。`track_id` は sidechain source picker が自トラックを除外するのに使うので残す |
| `handler/view_model.rs:349-388` | `parallel_output_entries()` が `ParallelOutputEntry { track_id, device_index }` を作る | `ParallelOutputEntry` (`app_types.rs:679-689`) も同様に `device_id: u64` へ。`track_id` は残す |
| `handler/automation_lanes.rs:510-551` | `inspector_video_fx_params()` が `open_video_fx_params?` から `(track, idx)` を取り、`device_id_at` で id に直している | `Option<u64>` から `find_device_by_id` で track を引き、cursor track 一致を gate。`VideoFxParamsInspector` (`app_types.rs:912-917`) の `device_index` → `device_id: u64` (view 側で `device_id_at` を呼ぶ必要が消える) |
| `handler/automation_lanes.rs:557-590` | `set_video_fx_param(device_index, param_id, value_real)` | `(device_id: u64, param_id, value_real)`。cursor track 依存の解決をやめ `find_device_by_id` + `device_at` で引く |
| `handler/automation_lanes.rs:624-700` | `inspector_plugin_params()` の `open_plugin_params?` / `plugin_params.get(&(track_id, device_index))` | 同上 + `plugin_params.get(&device_id)`。`PluginParamsInspector` (`app_types.rs:923-929`) の `device_index` → `device_id: u64` |
| `handler/automation_lanes.rs:734-800` | `set_plugin_param(device_index, ..)` (`:734`) は `cursor_track_id()` を解決して `set_plugin_param_on_track(track_id, device_index, ..)` (`:748`) へ渡すだけの wrapper | **2 本を 1 本に畳む** (KISS/DRY)。両方が `device_id` を取った瞬間、`track_id` の解決が `find_device_by_id` に移るので wrapper 側の存在理由 (cursor track の解決) が消え、**シグネチャも中身も完全に同じ関数が 2 本**になる。残すのは `set_plugin_param(device_id: u64, param_id: u32, value_real: f64)` **1 本だけ**で、`set_plugin_param_on_track` は削除する。呼び出しは `app.rs:1430` (`SetPluginParam` arm) と `handler/midi.rs:235` の **2 か所**。内部で `song_lanes` / `track.automation_lanes` を選り分けるのに使う `track_id` は `find_device_by_id(song, device_id)` から引く (引けなければ早期 return = 削除済み device への stale event) |
| `handler/automation_lanes.rs:994` | `loaded_slots.clear()` | `loaded_devices.clear()` |
| `handler/tick.rs:505` | `plugin_param_values.get(&(track, idx, param_id))` | `get(&DeviceParamKey { device_id, param_id })` — `find_device_by_id` の逆引きが消える |
| `handler/bounce.rs:290-297` | `loaded_slots.get(&(track.id, idx)).map(\|s\| s.device_id)` | `track.devices.iter().find(VOICEVOX).map(\|d\| d.id).filter(\|id\| self.ipc.loaded_devices.contains_key(id))` |
| `handler/voicevox.rs:231-240` / `:315-327` | 同上 | 同上 |
| `handler/midi.rs:207-236` | `BindingTarget::PluginParam` を destructure (`:207-212`) → `find_device_by_id` で `(resolved_track, device_index)` を出し (`:220-224`)、`let _ = track;` (`:225`)、`plugin_param_range(resolved_track, ..)` (`:231`)、`set_plugin_param_on_track(resolved_track, device_index, ..)` (`:235`) | 逆引き 5 行と `let _ = track;` が**まるごと不要になる** (`plugin_param_range` は track を取らなくなり、統合後の `set_plugin_param` は device_id を取る)。device 実在チェックは `set_plugin_param` 側 (`find_device_by_id` が `None` なら早期 return、`tracing` は出さない = 削除済み device への stale binding は正常系) に一本化する。destructure からは `track` (A-9 で `legacy_track` に改名) を外す |
| `view/resource_monitor.rs:50/263` | `track_plugin_ids.get(&t.id)` | `t.devices.iter().filter(\|d\| app.ipc.loaded_devices.contains_key(&d.id)).count()` / 同 iter |
| `view/track_inspector/mod.rs:1092-1097` | `open_plugin_params.or(open_video_fx_params).filter(track 一致).map(\|(_,idx)\| idx)` | `Option<u64>` を chain 内 index へ解決 (`chain.iter().position(\|e\| e.device_id == id)`)。`:1189` の `chain.get(i).map(\|e\| e.device_index) == open_dev` も device_id 比較に |
| `view/track_inspector/mod.rs:1130/1171/1183` | 行の `device_index` から `ToggleSlotGui` / `RemoveDevice` を撃つ | `entry.device_id` から `ToggleSlotGui { device_id }` / `RemoveDevices { device_ids }` |
| `view/track_inspector/mod.rs:1386-1390` / `:1473-1477` | `view.device_index` → `device_id_at(song, track_id, device_index).unwrap_or(0)` で lane target を組む | view が `device_id` を直接持つので **`device_id_at` 呼び出しごと削除**。`.unwrap_or(0)` の sentinel 分岐も消える |
| `view/track_inspector/mod.rs:1434/1447` / `:1552/1566` | `SetVideoFxParam { device_index, .. }` / `SetPluginParam { device_index, .. }` と `InspectorScrubField::{VideoFx,PluginParam} { device_index, param_id }` | どちらも `device_id: u64` に。`InspectorScrubField` の定義は `app_types.rs:393` (VideoFx) / `:395` (PluginParam) |
| **`view/track_inspector/chain_sections.rs`** (計画に無かった。**確実にコンパイルが壊れる**) | `entry.device_index` から AppEvent を撃つのが 10 か所: `:121/128` (`ReloadDevice`)、`:181/193` (`SetPluginSendAllKeys`)、`:234/259` (`ExplodeParallelOut`)、`:298` (`SetParallelOutputRoute`)、`:377/382` (`SetSidechainSource`)、`:402/406` (`SetAuxInputTapPoint`) | `entry.device_id` を渡す。`entry.track_id` は AppEvent から消えるので、ローカル束縛も不要な分は落とす |
| `widgets/arrangement/view_build.rs:249/330` | `&\|tgt\| app.plugin_param_range(t.id, tgt)` / `(MASTER_TRACK_ID, tgt)` | `&\|tgt\| app.plugin_param_range(tgt)` |
| `script.rs:95-96/192-193/280-286/315-319` | `plugin_to_track` / `track_plugin_ids` (**write-only**。grep で読み出しは 0 件で、doc の「PDC recompute が参照」は事実と食い違う) | 両方削除。読み出しが無いことを着手時にもう一度 grep で確認する。使用元が消える `resolve_device_coords` (`:212`) も削除 (`-D warnings` で dead_code が落ちる) |
| `script.rs:707-715` | `PluginCommand::SetSlotPlugin { device_id, track_id, .. }` を **生で送っている** | 構造体リテラルから `track_id` を除く。JS API 側の引数 `(track, index)` は script のアドレス指定なのでそのまま (`resolve_device_id` が id に直す) |
| `app.rs:218-237` / `:407-408` | 各 cache の初期化 | 新しい型に合わせる (`track_plugin_ids` の行は削除、`loaded_slots` → `loaded_devices`) |

**意図的に残す 3 つの座標ヘルパ** (`daw_gui/src/app_types.rs`)。どれも「Song から
毎回引き直す一時的な解決」であって保持される参照ではないので、不変条件 1 に反しない
(A-10 の検査もこれらを違反として出さない):
- `:1398-1418` `find_device_by_id(song, id) -> Option<(u32, u32)>` — device_id から
  所属 track を知る唯一の口。lane / gesture が track 所有である以上これは要る。
- `:1425-1440` `device_at(song, track_id, index) -> Option<&PluginInstance>` — 上と組。
- `:1448-1456` `device_id_at(song, track_id, index) -> Option<u64>` — 逆方向。
  script mode の JS API (`daw.setSlotPlugin(track, index, ...)`) と
  headless テスト (`tests/app_state/support.rs:128`、他 12 か所) が
  「n 番目の device」でアドレスするのに使う。**残すが、production の view / handler
  から呼ぶ必要は無くなる** (`ChainEntry` / inspector view が device_id を直接持つため)。

**`AppEvent` (`daw_gui/src/event.rs`) — device を運ぶ 10 変種すべてを device_id 化する。**
不変条件 1 は「プロセス境界・**イベント**・永続参照に positional index を使わない」なので、
帳簿だけ直してイベントに `device_index` を残すのは中途半端 (しかも §B の移動でイベント発行と
消費の間にチェーンが変わりうる = 実バグになる)。

- `:788` `ToggleSlotGui { index: u32 }` → `{ device_id: u64 }`
- `:796` `SetVideoFxParam { device_index, param_id, value_real }` → `{ device_id, .. }`
- `:802` `SetPluginParam { device_index, param_id, value_real }` → `{ device_id, .. }`
- `:804` `RemoveDevice { index: u32 }` → `RemoveDevices { device_ids: Vec<u64> }`
- `:809` `ReloadDevice { track_id, device_index }` → `{ device_id: u64 }`
- `:815` `SetSidechainSource { track_id, device_index, port, source }` → `{ device_id, port, source }`
- `:824` `SetPluginSendAllKeys { track_id, device_index, enabled }` → `{ device_id, enabled }`
- `:835` `ExplodeParallelOut { track_id, device_index }` → `{ device_id }`
- `:842` `SetParallelOutputRoute { track_id, device_index, port, dest }` → `{ device_id, port, dest }`
- `:904` `SetAuxInputTapPoint { track_id, device_index, port, tap_point }` → `{ device_id, port, tap_point }`
- 新規 `SelectDevice { device_id, modifier }` (D-1) / `RelocateDevices(RelocateDevices)` (B-1)
- `:1760-1767` の undo ラベル表も追随 (`RemoveDevice` → `RemoveDevices` 「デバイス削除」、
  `RelocateDevices` → `copy` で「デバイスコピー」/「デバイス移動」。
  `SetVideoFxParam` 以下 6 行は variant 名が変わらないので文言そのまま)

**doc コメントだけの後始末** (放置すると次の読み手が「host に track 概念がある」と誤解する。
grep して 1 件も残さない):
`common/src/protocol.rs:511-513` (`SetSlotPlugin.track_id` の説明)、`:536-540`
(`UnloadAllPlugins` が `RemoveTrack` と `loaded_slots` を根拠に使っている)、`:742-747`
(`SlotPluginUnloaded` が「RemoveTrack 経由」「`track_plugin_ids` / `loaded_slots` を片付ける」)、
`daw_gui/src/app_types.rs:15/17`、`daw_gui/src/state/ipc.rs:61/71/91`、
`daw_gui/src/handler/devices.rs:293/427/700-703/1145/1194/1199`、
`daw_gui/src/handler/project.rs:329/1026-1037/1202-1217`、
`daw_gui/src/handler/grouping.rs:113-124/239`、`daw_gui/src/handler/tracks.rs:550/572`、
`daw_gui/src/handler/bounce.rs:117` (`(track_id, device_index)` で解決すると書いている)、
`daw_gui/src/script.rs:200/211`、`daw_gui/src/event.rs:787/803/811/831`、
`daw_gui/src/app_tests.rs:875`、`daw_gui/tests/app_state/group_track_lifecycle.rs:17-21`。

### A-8. `Song::ensure_ids` に device id の重複検出を足す (`common/src/model.rs:1848-1868`)

現状 `alloc_dev` は `id == 0` の埋めと counter bump しかせず、**非 0 の重複 id を
検出しない**。コピー実装が `alloc_device_id()` を呼び忘れる経路が 1 本でもあると、
2 device が同 id を共有し `daw_plugin_host/src/main.rs:1222-1246` の dedup が
「同 device_id + 同 plugin_id」で吸収して 1 instance に silent に merge する
(音は出るので気付けない)。防御を SSoT 側に置く:

```rust
let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
fn alloc_dev(p: &mut PluginInstance, next: &mut u64, seen: &mut HashSet<u64>) {
    // 0 (未採番) と **既出 id** は必ず新採番する。重複を放置すると plugin host の
    // dedup が 2 device を 1 instance に silent に merge する (音は出るので気付けない)。
    if p.id == 0 || !seen.insert(p.id) {
        let new_id = (*next).max(1);
        *next = new_id + 1;
        p.id = new_id;
        seen.insert(p.id);
    } else if p.id >= *next {
        *next = p.id + 1;
    }
}
```

`common/src/model/tests.rs` に「同 id を 2 device に持たせた Song を `ensure_ids` に
通すと片方が再採番される」テストを 1 本足す。

### A-9. `BindingTarget::PluginParam.track` を legacy 化 (`common/src/model.rs:770-781`)

`track` は保存されるが実行時は読まれない (`handler/midi.rs:225` が `let _ = track;` で
明示的に捨て、解決は `find_device_by_id`)。device を移動すると project ファイルに
stale な track が残るので、`legacy_device_index` と同じ **deserialize 専用**にする:

```rust
PluginParam {
    /// v29: 安定 device id (`PluginInstance::id`)。`0` は未解決 sentinel。
    #[serde(default)]
    device_id: u64,
    param_id: u32,
    /// v28 以前 migration 用 (deserialize 専用)。旧 save の chain 内 positional index。
    #[serde(default, rename = "device_index", skip_serializing)]
    legacy_device_index: Option<u32>,
    /// v33 以前 migration 用 (deserialize 専用)。`legacy_device_index` を解決する
    /// ための所属 track。実行時の解決は `find_device_by_id` なので読まれない
    /// (r.md #71: device 移動で stale になるフィールドを保存しない)。
    #[serde(default, rename = "track", skip_serializing)]
    legacy_track: Option<u32>,
}
```

追随: `common/src/model.rs:1912-1923` (`ensure_ids` の migration は
`legacy_track` を使う。`track_devs` の引き方はそのまま)、`common/src/model.rs:185`
(project version の doc に「`BindingTarget::PluginParam` は `device_index` →
`device_id`」とあるので `track` の legacy 化も 1 行足す)、
`daw_gui/src/handler/midi.rs:109-114` (`midi_learn_binding_target` は
`legacy_track: None`)、`:207-212` (destructure から `track` を除く)、
`common/src/model/tests.rs:1820/1827`。
`daw_gui/src/handler/devices.rs:1138` と `daw_gui/src/view/transport.rs:700` は
`{ device_id, .. }` / `{ .. }` の形なので **無変更**。

**wire 変更である** (不変条件 7)。`BindingTarget` は `Encode, Decode` を derive しており
(`common/src/model.rs:753`)、`track: u32` → `legacy_track: Option<u32>` は
**bincode の表現が変わる** (serde 属性は bincode に効かないので `skip_serializing` では
逃げられない)。`common/build.rs:21` に `src/model.rs` が登録済みなので fingerprint は
自動で変わる = **`WIRE_SOURCES` への追加作業は無い**が、
**`cargo build --workspace` で子 exe を作り直すこと** (`[[feedback_workspace_build_for_protocol_changes]]`。
古い exe が残ると decode 失敗 → 「再生が止まる」)。§E-1 と同じ理由で、この 1 回のビルドが
両方を賄う。`Option<u32>` は `Copy` なので `BindingTarget` の `Copy` derive は維持される。

### A-10. `scripts/arch_lint.sh` — POSITIONAL-KEY 検査の作り直し

現行 `POSKEY_RE='HashMap<[(]u32,[[:space:]]*u32[)]'` (`scripts/arch_lint.sh:32`) は
**1 行の `HashMap<(u32, u32)` しか見ない**。3 つ組 / `HashSet` / 複数行折り返しは
素通りするので、baseline 4 行を消しても「完済」の証明にならない
(実際 §A が消す 7 か所のうち、現行パターンが見えているのは 4 か所だけ)。

#### 何を違反とみなすか (この判断が検査の全部)

「`(u32, u32)` という並び」そのものを見るパターン (`'[<(][(]u32,[[:space:]]*u32[,)]'`)
は **repo 全体で 40 件**当たり、うち **32 件は positional キーではない**
(実測 2026-08-28)。内訳は `gui_get_size` / `plugin_view_size` の
`Option<(u32,u32)>` (daw_plugin_host 8 件)、`texture_size` / `scissor_rect`
(ui/crates/renderer 4 件)、`decode_image_to_bgra` / `output_resolution` /
`pending_video_export_dims` (daw_gui 数件)、`Vec<(u32,u32,AutomationClip)>`
(common) …… つまり **(width, height) や (track, clip) の値タプル**で、
型だけからは positional キーと区別できない。これを違反として報告すると
`// arch-lint: allow-positional-key` が 15 ファイル以上に散り、検査そのものが
読まれなくなる (= 守ろうとしているものを壊す)。

不変条件 1 が禁じているのは「**保持される** positional 参照」= 帳簿である。
**連想コンテナ (map / set) のキーがタプル**という形に絞れば、実測で偽陽性ゼロ・
取りこぼしゼロで拾える (下記)。`Vec` / `Option` の生タプルは機械検査の対象外にする
— 型からは寸法と区別できないからで、実体の方は §A が名前付き id へ置換しきることで
消す (`gui_open_requests: Vec<(u32,u32)>` → `Vec<u64>`、
`open_plugin_params: Option<(u32,u32)>` → `Option<u64>`)。
**`find_device_by_id -> Option<(u32,u32)>` (`app_types.rs:1398-1418`) は違反ではない**:
Song から毎回引き直す一時的な解決結果であって、保持される参照ではない
(不変条件 1 の文言「プロセス境界・イベント・**永続**参照」に照らして正当)。

#### 実装

`scripts/arch_lint.sh:32` の `POSKEY_RE` を捨て、**awk 1 本**に置き換える
(折り返し定義は「直前行がコンテナ開きか」を見る必要があり、1 行正規表現では表せない)。
既存の慣習どおり **canary と本体で同じプログラムを共有**し、**バックスラッシュは
使わない** (make 経由で argv から落ちる)。awk は既に check 6 (FILE-BUDGET) で使っている。

```sh
# positional キーの検出 (不変条件 1)。 **連想コンテナのキーがタプル**という形だけを
# 見る。 「(u32, u32) の並び」そのものを見るパターンは repo に 40 件当たり、その 8 割が
# gui_get_size / texture_size / decode_image 等の **寸法タプル** で、型から区別できない。
# 区別できないものを報告すると allow マーカーが散って検査が読まれなくなる。
# Vec / Option の生タプルは対象外 (寸法と区別不能)。 map/set のキーなら偽陽性ゼロ。
# 折り返し (`HashMap<` で改行 → 次行が `(u32, u32),`) も拾うので awk。
POSKEY_AWK='
FNR == 1 { prev = "" }
{
  if ($0 ~ /(HashMap|HashSet|BTreeMap|BTreeSet|IndexMap|IndexSet)[[:space:]]*<[[:space:]]*[(]u32,[[:space:]]*u32[,)]/)
      print FILENAME ":" FNR ":" $0
  else if (prev ~ /(HashMap|HashSet|BTreeMap|BTreeSet|IndexMap|IndexSet)[[:space:]]*<[[:space:]]*$/ && $0 ~ /^[[:space:]]*[(]u32,[[:space:]]*u32[,)]/)
      print FILENAME ":" FNR ":" $0
  prev = $0
}'
```

canary は既存 `canary_ok` ブロックに足す (肯定 3 / 否定 3 / マーカー 1)。
**検査本体と同じ `$POSKEY_AWK` を使うこと** — 別物を試す canary は証明にならない
(既存 `UNTAGGED_RE` の教訓が同ファイルのコメントに残っている):

```sh
# (1) 肯定側 — 1 行 / HashSet / 折り返した 3 つ組。
printf 'x: HashMap<(u32, u32), SlotInfo>,\n' | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
printf 'x: HashSet<(u32,u32)>,\n'            | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
printf 'p: std::collections::HashMap<\n    (u32, u32, u32),\n    f64,\n>,\n' \
    | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
# (2) 否定側 — 寸法タプルと (u64, u32) を拾わないこと (拾うと allow が散る)。
printf 'fn size(&self) -> Option<(u32, u32)> {\n' | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
printf 'let v: Vec<(u32, u32)> = Vec::new();\n'   | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
printf 'x: HashMap<(u64, u32), f64>,\n'           | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
# (3) 行内マーカーが効くこと (video_fx の寸法プールが恒久的に正当な唯一の実例)。
printf 'p: HashMap<(u32, u32), SizePool>, // arch-lint: allow-positional-key\n' \
    | awk "$POSKEY_AWK" | strip_allowed positional-key | grep -q . && canary_ok=0
```

走査本体 (`scripts/arch_lint.sh:259-263` の check 2) を差し替える。
出力形式は `path:line:content` のままなので `strip_allowed` / `strip_comments` /
`record` / fingerprint はそのまま使える:

```sh
hits=$(find $rs_dirs -name '*.rs' -not -path '*/target/*' -print0 2>/dev/null \
    | xargs -0 awk "$POSKEY_AWK" 2>/dev/null \
    | strip_allowed positional-key | strip_comments || true)
record POSITIONAL-KEY grep "positional (u32,u32) キーの map/set。安定 id (device_id: u64 等) でキーする:" "$hits"
```

`arch_lint.sh:244` の ratchet SELFTEST 行
(`classify 'SELFTEST grep selftest/synthetic.rs:1:    pool: HashMap<(u32, u32), Bogus>,'`)
は `classify` に直接食わせる合成行でパターンを通らないので **変更不要**。

#### この検査が今 repo に対して出す結果 (実測 2026-08-28)

8 hits = 違反 7 + 行内マーカー済み 1。

| hit | 始末 |
| --- | --- |
| `daw_gui/src/state/ipc.rs:40` (`plugin_param_values`、折り返し 3 つ組) | A-1 で `DeviceParamKey` |
| `daw_gui/src/state/ipc.rs:49` (`plugin_params`、折り返し) | A-1 で `HashMap<u64, _>` |
| `daw_gui/src/state/ipc.rs:57` (`slot_has_gui`) | A-1 |
| `daw_gui/src/state/ipc.rs:78` (`loaded_slots`) | A-1 (`loaded_devices`) |
| `daw_gui/src/state/ipc.rs:148` (`open_plugin_guis`、`HashSet`) | A-1 |
| `daw_gui/src/state/ipc.rs:182` (`pending_added_plugin_finalize`) | A-1 |
| `daw_gui/src/app_types.rs:1519` (`compute_slot_reconcile_actions` の引数) | A-3 |
| `daw_gui/src/video_fx/mod.rs:360` (`pool: HashMap<(u32,u32), SizePool>`) | **既に `// arch-lint: allow-positional-key` 付き**。寸法キーなので恒久的に正当、触らない |

**§A 完了後は 0 hits になる** (= exit 0 かつ baseline 0 件、§F-4 の受け入れ条件が
達成可能)。もし残ったら **baseline に足さず直す** — それが本件の目的。
恒久的に正当なものが出たら行内マーカー `// arch-lint: allow-positional-key` を
理由コメント付きで付ける (baseline とマーカーの使い分けは
`scripts/arch_lint_baseline.txt:10-13` が定義している)。

- `scripts/arch_lint_baseline.txt:20-31` の **POSITIONAL-KEY 4 行と、その直前の
  理由コメントブロック (`# --- 不変条件 1 (安定 id addressing) 違反 4 件 ---` の段落) を
  削除**する (baseline に載っているのは旧パターンが見えていた 4 件だけ。残り 3 件は
  そもそも表に出ていなかったので、同じ変更で消える以上 baseline に足す必要はない)。

### A-11. §A で確実に壊れるテスト

| ファイル | 壊れる理由 | 直し方 |
| --- | --- | --- |
| `daw_gui/tests/reconcile_slot_diff.rs:14-149` | 5 テストが `loaded_slots` を直接構築 (grep 実測 2026-08-28: 宣言 `:44/67/90/118/138` の 5 か所 + `insert((track_id, n), loaded(..))` `:45/46/68/91/119/120/121` の 7 か所)。import (`:14`) と helper `loaded()` (`:32-33`) も `LoadedSlotInfo` を名指す | `loaded_devices.insert(100, LoadedDeviceInfo { plugin_id_str: "p.comp".into() })` に。helper は `fn loaded(plugin_id_str: &str) -> LoadedDeviceInfo` へ縮む (`device_id` は key になったので値に持たない)。期待 action も `RemoveDevice{device_id}` / `LoadDevice{device_id,..}` へ |
| `daw_gui/tests/app_state/group_track_lifecycle.rs:62/179/212-214/292/296/389-399` | `track_plugin_ids` / `loaded_slots.get(&(track_id, i))` | `loaded_devices` + Song 由来の device id 列で assert。doc コメント (`:17-21`) も更新 |
| `daw_gui/tests/app_state/group_track_lifecycle.rs:54/148/169` | `PluginCommand::SetSlotPlugin { device_id, track_id, .. }` を destructure (§E-1 で `track_id` が消える) | `{ device_id, .. }` に。`track_id == ...` の assert 条件は device_id 側だけで足りる |
| `daw_gui/tests/app_state/group_track_lifecycle.rs:228/262-270` | `PluginCommand::RemoveTrack { track_id } if *track_id == group_id` を assert (§E で variant ごと消える) | 「group の全 device について `ClosePluginShmem` → `RemoveSlotPlugin` がこの順で出る」に書き換える。`plan_track_removal_ipc` の unit test (§E-3) と役割が重複しないよう、こちらは **実際に送られた IPC 列**を見る |
| `daw_gui/tests/app_state/group_track_lifecycle.rs:195` | `AppEvent::RemoveDevice { index: 0 }` | `AppEvent::RemoveDevices { device_ids: vec![dev] }` |
| `daw_gui/tests/app_state/pending_state_queue.rs:74/100/227` | `AppEvent::RemoveDevice { index: 1 }` (+ `:62-64` は `device_id_at` なのでそのまま動く) | 同上。コメント (`:31/72/95/225/257`) の「RemoveDevice」表記も追随 |
| `daw_gui/tests/app_state/plugin_load_failure.rs:55` | `PluginCommand::SetSlotPlugin { device_id, track_id: t, .. }` を destructure | `{ device_id, .. }` |
| `daw_gui/tests/app_state/plugin_load_failure.rs:242` | `AppEvent::ReloadDevice { track_id, device_index }` | `AppEvent::ReloadDevice { device_id }` |
| `daw_gui/tests/app_state/transform_edit_regress.rs:67` | `loaded_slots.insert((..))` | 同上 |
| `daw_gui/tests/voicevox_progress.rs:164` | 同上 | 同上 |
| `daw_gui/tests/app_state/support.rs:124-134` | `fake_plugin_loaded` は `device_id_at` 経由なので **そのまま動く** | 変更不要。`device_id_at` を残す理由の 1 つがこれ (テストは「n 番目の device」でアドレスするのが自然) |
| `common/src/protocol.rs:928-939` | `set_slot_plugin_roundtrip` テストが `track_id: 7` を書いている | その 1 行を消す |
| `daw_gui/src/app_tests.rs:866-894` (`mod master_fx_tests`) | `reconcile_emits_loadslot_for_master_fx` が `SlotReconcileAction::LoadSlot { track_id, index, plugin_id_str, .. }` を destructure して `*track_id == MASTER_TRACK_ID && *index == 0` を assert している。**コメントだけではなく確実なコンパイルエラー** | `LoadDevice { device_id, plugin_id_str, .. }` に。`PluginInstance::new` は `id == 0` を作るので、**assert する前に `song.ensure_ids()` を呼ぶか `push` 後に id を明示代入する** (device_id が assert 対象になったため)。コメント `:875` の `loaded_slots` 表記も追随 |
| `ui/crates/ui/tests/ui/pass/basic.rs:125-134` | trybuild の **pass ケース**が `ui.reorderable_list("rl", rect, &m.chain, None, &rl_style, ...)` を呼ぶ。D-3 で `selected: Option<usize> → &[usize]` + `accept_drag_kind` 追加なので落ちる。`daw-ui-core` は `TEST_PKGS_NO_GUI` に入っているので **`make test-nolaunch` で赤になる** | `&[]` と `None` を渡す形に更新 |
| `ui/crates/ui/src/widgets/reorderable_list.rs` の `mod tests` (`:625/651/703/756/785/833/875/901/939` の 9 呼び出し) | 同上 | 同上。`selected` を渡していたテストは `&[i]` に |

---

## §B モデル操作: `relocate_devices` (移動 / コピーの唯一の口)

### B-1. 型

`daw_gui/src/app_types.rs` に追加:

```rust
/// r.md #71: device の運搬要求 1 件分。 表示順は `device_ids` の並びが決める
/// (呼び出し側がチェーン表示順に整えて渡す)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocateDevices {
    pub device_ids: Vec<u64>,
    /// 落とし先チェーンの所有者。`MASTER_TRACK_ID` なら `Song.master_fx_chain`。
    pub dest_track: u32,
    /// 落とし先チェーン内の挿入位置 (`0..=chain.len()`)。
    pub dest_index: u32,
    /// `true` = コピー (新 device id を採番)、`false` = 移動 (id 据え置き = 音を切らない)。
    pub copy: bool,
}
```

`daw_gui/src/event.rs` に `AppEvent::RelocateDevices(RelocateDevices)` を追加、
`:1759` 付近の undo ラベル表に「デバイス移動」/「デバイスコピー」を追加
(`copy` で文言を分ける)。

`daw_gui/src/app_types.rs:1624` の `DeferredEdit` に
`RelocateDevices(RelocateDevices)` を追加。

### B-2. dispatcher (`daw_gui/src/handler/devices.rs` に置く)

```rust
/// r.md #71: 選んだ device を別のチェーンへ運ぶ (移動 / コピー) 唯一の口。
///
/// 最新の knob 値を Song に書き戻してから実行する必要があるので、host に plugin が
/// 居るときは `RequestAllStates` の round-trip 待ちに積む (track copy/cut/duplicate と
/// 同 idiom、`app_types.rs` の `DeferredEdit` doc 参照)。
/// - コピー: 落とし先 device の `initial_state` が「いまのツマミ」になる。
/// - 移動: instance は作り直さないが、undo snapshot が最新 state を捕まえる。
pub(crate) fn relocate_devices(&mut self, req: RelocateDevices) {
    if req.device_ids.is_empty() { return; }
    if !self.song_has_plugin() {
        self.relocate_devices_inner(&req);
        return;
    }
    self.enqueue_state_request(PendingStateRequest::Deferred(DeferredEdit::RelocateDevices(req)));
}
```

`:1348` `execute_deferred_edit` に arm を追加。

### B-3. `relocate_devices_inner` の中身

**全ての Song 書き換えを 1 回の `edit_song` に入れる** (不変条件 5、undo 1 step、
epoch bump 1 回)。クロージャは以下を返す:

```rust
struct RelocateOutcome {
    /// 挿入順に並んだ結果の device id (移動なら元 id、コピーなら新 id)。選択に使う。
    result_ids: Vec<u64>,
    /// 移送した automation lane の再キー表 `(src_track, old_lane, dest_track, new_lane)`。
    lane_remap: Vec<(u32, u32, u32, u32)>,
    /// recording gesture の再キー用 `(src_track, dest_track, PluginParam target)`。
    moved_targets: Vec<(u32, u32, common::model::AutomationTarget)>,
    /// コピーで新規に作った device (host へ実体化する対象)。
    created: Vec<common::model::PluginInstance>,
}
```

手順 (クロージャ内):

1. **解決**: `device_ids` を `find_device_by_id(song, id)` で `(src_track, index)` に解決。
   解決できないものは捨てる。`dest_track` のチェーンが存在しなければ `None` を返して中止。
2. **無変化の早期 return はしない** — 同一チェーン内の移動は「並べ替え」として
   正当な操作なので普通に処理する。
3. **移動 (`!copy`)**:
   - src チェーンごとに index 降順で `Vec::remove` して `PluginInstance` を取り出す。
   - `dest_index` を補正する: **dest と同じチェーンから、`dest_index` より前の位置で
     抜いた個数だけ引く**。これを忘れると同一チェーン内の移動が 1 個ずれる。
   - `dest_chain.splice(dest_index..dest_index, taken)` で挿入順を保って入れる。
   - 各 device について、`src_track != dest_track` なら:
     - **automation lane の移送**: src 所有者 (`src_track == MASTER_TRACK_ID` なら
       `song.song_lanes`、それ以外は `track.automation_lanes`) から
       `AutomationTarget::PluginParam { device_id: d, .. }` に一致する lane を
       `Vec::retain` ではなく **抜き取り** (`extract_if` 相当の手書きループ) で取り出す。
       dest 所有者へ push する前に **lane id を必ず再採番する**
       (`Track::alloc_lane_id` / `Song::alloc_song_lane_id`)。据え置きは禁止 —
       dest 側の既存 lane と id が衝突すると、選択や行高 override が silent に
       別 lane へ付け替わる。`(src_track, old_id, dest_track, new_id)` を `lane_remap` に積む。
     - **mod_routing の移送**: 同じ条件で `Track.mod_routings` ↔ `Song.song_mod_routings`
       を移す。`ModRouting.source_id` は `Song.mod_sources` の song-global id なので
       そのまま生きる (再キー不要)。
     - **ARA アーカイブを落とす**: `inst.ara_archive = None`。
       `handler/sync.rs:387` の persistent_id は `"{source_id}:{clip_id}:{event_index}"` で
       元トラックのクリップを指しているため、別トラックへ持ち込むと復元できない
       (= 解析し直す)。
     - **aux 参照の自トラック追随**: `inst.aux_inputs[*].tap.source_track == src_track`
       なら `dest_track` に、`inst.aux_outputs[*].dest_track == src_track` なら
       `dest_track` に貼り替える。他トラックを指すものは触らない。
     - `moved_targets` に `(src_track, dest_track, PluginParam{device_id, param_id})` を
       lane / routing / 現存 gesture 由来で積む (再キーは B-4 で行う)。
   - **副作用の対称化** (`[[feedback_trace_full_path_when_mirroring]]`):
     - VOICEVOX builtin (`plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX`) を
       移したら、src track に他の VOICEVOX device が残っていなければ
       `src.source = InstrumentSource::None`、dest が通常 track なら
       `dest.source = InstrumentSource::Vocal`
       (規則の出所: 追加側 `handler/mixer.rs:120-123` / 削除側
       `handler/devices.rs:1066-1074`。**削除側は「他に残っていなければ」を見ていない**
       ので、A-5 でそちらにも同じガードを入れて規則を 1 つにする)。
     - Transform (`common::video_fx::TRANSFORM_ID`) を移したら、src に他の Transform が
       残っていなければ `src.group_transform = None`、dest が `None` なら
       `Some(GroupTransform::default())` を入れる
       (出所: `handler/mixer.rs:106-113` / `handler/devices.rs:1076-1088`)。
     - master (`MASTER_TRACK_ID`) はどちらの副作用も持たない (Track ではない)。
4. **コピー (`copy`)**:
   - 元 `PluginInstance` を clone し `id = song.alloc_device_id()`。
   - `state` は **そのまま引き継ぐ** (`Arc<[u8]>` の clone なのでコストゼロ)。
   - `ara_archive` は `src_track == dest_track` のときだけ引き継ぐ。跨いだら `None`。
   - `aux_inputs` / `aux_outputs` は引き継ぐ。自トラック参照は移動と同じ規則で貼り替える。
   - **automation lane / mod_routing は複製しない** (確定方針)。
   - 副作用の対称化は「dest 側だけ」適用する (src はそのまま残るので降ろさない)。
   - `dest_chain.splice(dest_index..dest_index, copies)`。
   - `created` に clone した instance を積む。
5. `result_ids` を挿入順で返す。

クロージャの外 (`edit_song` の戻り値を受けたあと):

### B-4. lane 再キーに伴う ViewState / 選択 / 録音状態の始末

`lane_remap` の各行 `(src_track, old_lane, dest_track, new_lane)` について、
`from = AutomationLaneKey { track: src_track, lane: old_lane }`、
`to = AutomationLaneKey { track: dest_track, lane: new_lane }` として:

- **`ui_prefs.automation_lane_row_overrides`** (`daw_gui/src/state/ui_prefs.rs:69-70`):
  `remove(&from)` して `insert(to, v)`。
  **これは session-only** — `UiPrefs` は `Serialize` を持たず (`ui_prefs.rs:4`)、
  doc 自身が「session-only (save / Undo 対象外)」と書いており、project load 時に
  `handler/project.rs:182` (`ui_prefs.automation_lane_row_overrides.clear()`。
  grep 実測 2026-08-28 で **182**。レビューが指摘した `:184` は誤りだったので
  この行は据え置く) で clear されるだけで保存経路は無い。したがって
  「永続 ViewState と Song を同時に触るので dirty 判定が壊れる」類の危険は **無い**
  (初稿はここを「永続 ViewState」と誤記していた)。
  移送が要るのは別の理由: **鍵の `AutomationLaneKey{track, lane}` が両方変わる**ので、
  写し替えないと「行高だけ元の位置に取り残されて、別 lane に化ける」。
  Song 側 (lane の所属 track) は `edit_song` で書くので dirty は立つ (正しい —
  lane の所属は「作った中身」、`[[project_dirty_flag_rule]]`)。
- **`ui_ephemeral.arrange_zoom_history`** の各 `ArrangeViewSnapshot.lane_row_overrides`
  にも同じ写像を掛ける (掛けないと X で 1 段戻した瞬間に行高が飛ぶ)。
- **`selection.selected_automation_clips`** (`AutomationClipKey`): `lane_key() == from` の
  要素の `track` / `lane` を `to` に差し替える。`selection.automation_clip_anchor` も同様。
- **`selection.selected_automation_points`** (`AutomationPointKeyRef { track_id, lane_id, .. }`):
  `(track_id, lane_id) == (from.track, from.lane)` を `to` に差し替える。
  `selection.automation_point_anchor` も同様。
- **`ui_ephemeral.arrange_hovered_automation_lane` (`:129`) と
  `arrange_default_scrub_active` (`:140`)** は「いまポインタがどこを指しているか」の
  観測値なので **写像せず `None` に落とす** (次のフレームで再計算される)。

`moved_targets` について (**移送しないと automation 記録が壊れる**):

- `recording.active_param_gestures` / `recording.latched_param_gestures`
  (`HashSet<(u32, AutomationTarget)>`) と `recording.recording_last_beat`
  (`HashMap<(u32, AutomationTarget), f64>`) から `(src_track, target)` の entry を
  取り出して `(dest_track, target)` として入れ直す。
- 最後に `self.sync_recording_lanes_with_audio()` を呼ぶ
  (`daw_gui/src/handler/tick.rs:451`)。これを忘れると Touch/Latch/Write 中に
  device を移したとき、`daw_audio/src/automation.rs:196-199` の skip 判定が
  旧 track でも新 track でも外れ、curve eval とユーザーのノブ操作が二重に効く。

### B-5. 子プロセス同期

- **移動**: plugin_host への IPC は **1 通も要らない** (§E で host から帰属という概念を
  撤去するため、device_id は不変で instance も作り直さない)。daw_audio 側は
  `flush_song_sync` (= `LoadSong`) が `daw_audio/src/main.rs:805` の
  `Topology::Recompile` を発火して処理順を再 compile する。
  `edit_song` の epoch bump を runner の frame flush が拾うので **明示送信は不要**。
  ただし headless / script 経路では frame loop が回らないので、
  `relocate_devices_inner` の末尾で `self.flush_song_sync()` を呼ぶ
  (epoch 未変化なら no-op、GUI でも二重送信にならない)。
- **`ara_doc_cache` は触らない** (初稿は `remove(&device_id)` を指示していた。**取り消す**)。
  `handler/sync.rs:126-136` が毎回 `live` を **Song から作り直して** device_id ごとに
  diff し (`:165-174`)、最後に `ara_doc_cache = live` で丸ごと差し替える (`:181`)。
  persistent_id は `"{source_id}:{clip_id}:{event_index}"` (`:387`) で **clip.id を含む**
  ので、トラックを跨げば clip 集合が変わり rebuild は自動で発火する。
  つまり「落とさないと SetupAraDocument が送られない」は**成立しない** —
  唯一 diff が空になるのは両トラックとも同一 (実質は両方とも空) の clip 集合のときで、
  そのときは送るものが無い。
  むしろ **remove する方が害がある**: cache 不在は無条件 rebuild
  (`sync.rs:174` の `_ => rebuilds.push(..)`) なので、両方空の移動でも
  `SetupAraDocument{clips: []}` が飛んで plugin が無意味に再初期化される。
  `sync.rs` は **読むだけ** (§0.5 の分類どおり)。
- **コピー**: 各 `created` について
  1. `self.ipc.pending_added_plugin_finalize.insert(new_id, false);`
     (load 完了で `LoadSong` 再送。GUI 自動 open はしない)
  2. `self.restore_device(&inst);`
  順序はこの通り (finalize を先に積まないと、load 応答が先に届いたときに取りこぼす)。
  `OpenPluginShmem` は `on_plugin_loaded_from_child` が **live な `self.ipc.audio_tx`**
  から送る既存経路に乗る (`handler/devices.rs:96` のコメント参照。stale clone で
  送ると audio respawn 後に音が出ない)。

### B-6. 選択とカーソルトラックの後始末

```rust
self.selection.selected_device_ids = outcome.result_ids.clone();
self.selection.device_anchor = outcome.result_ids.last().copied();
self.selection.last_edit_select = Some(EditSurface::Devices);
self.focus_inspector_track(req.dest_track);
```

`focus_inspector_track` は **新設** (`daw_gui/src/handler/tracks.rs`):

```rust
/// r.md #71: インスペクタの表示対象トラックだけを動かす (= カーソルトラックの移動)。
///
/// [`Self::set_track_selection`] を使わないのは、あちらが last-wins タグを
/// [`EditSurface::Tracks`] に倒すため。device をドラッグ中 / 落とした直後に
/// トラック面へタグが移ると、次の Delete がトラックを消してしまう。
/// **選択集合は動かすがタグは触らない** のがここの責務。
pub(crate) fn focus_inspector_track(&mut self, track_id: u32) {
    if self.cursor_track_id() == Some(track_id) {
        return;
    }
    self.selection.selected_track_ids = vec![track_id];
    self.selection.track_anchor = Some(track_id);
}
```

---

## §C クリップボード (`daw_gui/src/clipboard.rs`)

### C-1. payload

```rust
pub enum ClipboardPayload {
    Notes(Vec<Note>),
    AutomationPoints(Vec<CopiedPoint>),
    AudioEvents(Vec<AudioEvent>),
    Clips(Vec<ClipCopy>),
    AutomationClips(Vec<AutomationClipCopy>),
    Tracks(Vec<TrackCopy>),
    /// r.md #71: チェーンから選んだプラグイン。
    Devices(Vec<DeviceCopy>),
}

/// 正規化済み device。`order` は選択群内の相対順 (上から 0,1,2...) で、貼り付けで
/// 相対順を保つ。`device.id` は **0 に落として運ぶ** (貼り先で必ず新採番する)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCopy {
    pub order: usize,
    /// コピー元の所属トラック。貼り付け先が同じなら ARA アーカイブを引き継ぐ
    /// (別トラックなら捨てて解析し直す — `handler/sync.rs` の persistent_id が
    /// 元トラックのクリップを指すため)。
    pub source_track: u32,
    pub device: common::model::PluginInstance,
}
```

`CLIPBOARD_MAGIC` (`:22` = `"daw_01.clipboard.v1"`) は **据え置き**。
理由をコメントで明記すること: serde の externally-tagged enum に variant を足すと
新 build が書いた clipboard を旧 build は deserialize できないが、
`ClipboardEnvelope::from_json` (`:154`) は `serde_json::from_str(...).ok()?` なので
**decode 失敗は `None` = 静かな no-op** に落ちる (magic 一致で誤爆はしない)。
version を上げると逆に「旧 build が書いた clipboard を新 build が読めない」を
新規に作ってしまうので上げない。

### C-2. blob の上限

`PluginInstance.state` / `ara_archive` は `#[serde(with = "base64_opt")]`
(`common/src/model.rs:2736-2744` / `:2778-2783`) なので、**base64 テキストとして
OS クリップボードへ流れる**。サンプラー系の state は数十 MB になり得る。

```rust
/// r.md #71: 1 回のコピーで OS クリップボードへ載せる blob の総量上限 (base64 前の生バイト)。
/// OS クリップボードはテキストの通り道であって、数十 MB のサンプラー state を運ぶ場所ではない。
/// **超える分は運ばずに落とし、status_message で何を落としたか明示する** — 黙って
/// 切ると「貼ったら音色が違う」の原因が見えなくなる。大きい state ごと運びたいときは
/// ドラッグ&ドロップ / 複製 (どちらもプロセス内で `Arc` を clone するだけ) を使う。
pub const CLIPBOARD_BLOB_BUDGET: usize = 4 * 1024 * 1024;
```

落とす順序は決定的に: **(1) 全 device の `ara_archive`、(2) 全 device の `state`**。
`(1)` で収まればそこで止める。落としたら
`"クリップボードには大きすぎるため <n> 件のプラグイン設定を除いてコピーしました
(ドラッグで運ぶと設定ごと移せます)"` を `status_message` に出す。

### C-3. sanitize

`sanitize_devices(devices: Vec<DeviceCopy>) -> Vec<DeviceCopy>` を追加
(`sanitize_tracks` の隣、同じ流儀で):

- `device.id = 0` に強制 (外部 clipboard が任意の id を入れてくるのを防ぐ)。
- `aux_inputs[*].tap.source_track` / `aux_outputs[*].dest_track` は貼り付け時に
  dangling を落とすので、ここでは **`Vec` の長さ上限だけ** を見る
  (`aux_inputs.truncate(64)` / `aux_outputs.truncate(64)`)。
- `plugin_id` が空文字なら破棄。
- `ports` はそのまま (`PortConfig` は bool の集合で値域なし)。

`clipboard.rs` の `mod tests` に `envelope_roundtrip_devices` と
`sanitize_devices_drops_id_and_empty_id` を足す (既存テストと同じ形)。

### C-4. `daw_gui/src/view/root.rs`

`copy_for_surface` の `synced` match (`:442-453`) と `cut_for_surface` の同 match
(`:477-488`) は **網羅的** (`_ =>` を持たない。末尾が
`EditSurface::Tracks | EditSurface::Sections => None,`) なので、`EditSurface::Devices` を
足した時点で両方がコンパイルエラーになる = 書き漏らしが構造的に起きない。
`Devices` はここでは `None` 側 (`Tracks | Sections | Devices => None,`) に並べ、
実処理は Tracks と同じく **match の後**の `matches!(surface, ...)` ブロック
(`:462-467` / `:510-515`) の隣に 1 つ足す:

```rust
if matches!(surface, EditSurface::Devices) {
    let ids = app.live_device_ids();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| { app.copy_devices(ids); }));
}
```
(`copy_for_surface` / `cut_for_surface` は `&AppData` しか持たないので、
`&mut self` が要る `copy_devices` / `cut_devices` は `Edit::mutate` 越しに呼ぶ。
Tracks の既存 2 ブロックと同じ形。**`cut_for_surface` の `del` match (`:491-501`) は
`_ => return,` を持つので Devices を足してもエラーにならない** — cut の削除は
`cut_devices` 側が clipboard 書き込みと 1 undo step にまとめるので、そちらには足さない)

- `:438` `copy_for_surface`: `EditSurface::Devices` の arm を足す。
  device の copy は **最新 plugin state が要るので非同期** — トラック copy と同じ
  `PendingStateRequest::CopyToClipboard` 相当が必要。既存の
  `PendingStateRequest::CopyToClipboard { track_ids }` (`app_types.rs:1617`) を
  `CopyToClipboard(ClipboardCopyRequest)` に一般化し、
  `enum ClipboardCopyRequest { Tracks(Vec<u32>), Devices(Vec<u64>) }` を持たせる。
  追随するのは `handler/devices.rs:1319-1323` (dispatch) と
  `handler/tracks.rs:59-71` `copy_tracks` の enqueue。
  view 側は `app.copy_devices(ids)` を呼ぶだけ (結果は既存の
  `ui_ephemeral.pending_clipboard_write` 経由で `root.rs:947` が flush する)。
- `:473` `cut_for_surface`: 同様に `app.cut_devices(ids)`
  (`DeferredEdit::CutDevices { device_ids }` = clipboard 書き込み + 削除を 1 undo step)。
  `copy_devices` / `cut_devices` は `handler/devices.rs` に置き、形は
  `handler/tracks.rs:59-90` の `copy_tracks` / `cut_tracks` をそのまま写す
  (`song_has_plugin()` が false なら即時実行、true なら round-trip 待ちに積む)。
  実体は `copy_devices_inner` / `cut_devices_inner` で、`tracks.rs:94-108` と同じく
  「serialize → `pending_clipboard_write` → (cut なら) 削除」。
- `:520` `paste_from_clipboard`: `P::Devices(devices)` の arm を足す。

```rust
P::Devices(devices) => {
    // 貼り先は「いまインスペクタに出ているチェーン」。挿入位置は
    // **選んでいるプラグインの直前**、選択が無ければ末尾 (Ableton 流)。
    if let Some(dest_track) = app.cursor_track_id() {
        let devices = crate::clipboard::sanitize_devices(devices);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            let n = app.paste_devices(devices, dest_track);
            if n > 0 { app.ui_ephemeral.status_message = format!("貼り付け: {n} プラグイン"); }
        }));
        return;
    }
    paste_noop(ui);
}
```

`paste_devices` (`handler/devices.rs`) は:
1. `dest_index` = cursor track のチェーン内で `live_device_ids()` に含まれる
   device の **最小 index**。無ければ `chain.len()` (D-1 の正規化を通すので、
   別トラックの選択が残っていても末尾に落ちる = 画面と一致する)。
2. `order` 昇順に並べ、`edit_song` の中で各 device に `alloc_device_id()` を振り、
   `source_track != dest_track` なら `ara_archive = None`、
   `aux_inputs` / `aux_outputs` の dangling track 参照を落とす
   (`song.track_by_id(t).is_some()` で判定。`handler/tracks.rs:341-353` と同じ規則。
   ただし **`aux_outputs` は向こうが取りこぼしている**ので真似しない — A-7 参照)、
   `splice` で挿入。VOICEVOX / Transform の dest 側副作用も適用する。
3. `pending_added_plugin_finalize` + `restore_device` (B-5 のコピーと同じ)。
4. 選択と last-wins タグを貼った device に倒す。

- `:1193` / `:1223` の複製ショートカット (`daw.duplicate_clip_shared` / `_unique`):
  `Some(EditSurface::Devices)` の分岐を先頭に足し、両方とも
  `AppEvent::RelocateDevices` の `copy: true`・各 device の直後挿入で処理する
  (対象 id は `app.live_device_ids()`)。
  **リンク / 独立の区別は device には無い** (プラグインインスタンスは共有できない)
  ので D と Alt+D は同じ動作。その旨をコメントで書く。

---

## §D UI

### D-1. `EditSurface::Devices` (`daw_gui/src/app_types.rs:845`)

```rust
pub enum EditSurface {
    AudioEvents,
    Notes,
    AutomationPoints,
    AutomationClips,
    Clips,
    Tracks,
    Sections,
    /// r.md #71: インスペクタの Chain 行 (選択中のプラグイン)。
    /// **明示的に行を click したときだけ**立つので、`edit_surface` の
    /// 非空優先順 fallback には入れない (タグ経由の last-wins だけで足りる)。
    Devices,
}
```

`daw_gui/src/state/selection.rs` に追加:

```rust
/// r.md #71: インスペクタのチェーンで選択中の device (安定 `PluginInstance::id`)。
/// 末尾 = 「最後にクリックした anchor」。session-only (保存しない)。
pub selected_device_ids: Vec<u64>,
/// device 選択のアンカー (Shift+click 範囲選択の基点)。
pub device_anchor: Option<u64>,
```

#### 選択集合は「いま表示しているチェーン」にスコープされる (掃除の SSoT)

device 選択は **カーソルトラックのチェーン**という面の上にある。だから
「device が消えた」だけでなく **「カーソルトラックが変わった」でも stale になる**。
実際 `handler/tracks.rs:804` `set_track_selection` は `selected_device_ids` を触らず、
B-6 の `focus_inspector_track` も last-wins タグを意図的に触らない。放っておくと:

- (a) device をヘッダへ運びかけて途中で release → drag は自動キャンセル (D-2)、
  cursor は運び先のまま、`selected_device_ids` は元トラックの device、タグは Devices の
  まま → **次の Delete が画面に出ていない device を消す**。
- (b) その状態の Shift/Ctrl+click は `inspector_chain()` 由来の `order` と噛み合わず、
  異トラック混在の選択集合ができる。

「cursor track を動かす経路をすべて洗って掃除を挿す」のは **貼り替え補償コード**で、
不変条件 1 が禁じている形そのもの (writer は `set_track_selection` /
`apply_select_tracks` / `focus_inspector_track` / project load / track 削除 …と増える)。
**保持した集合を毎回正規化して読む**のが正しい。この repo には既にその idiom がある —
`handler/tracks.rs:829` `live_track_ids` (「`song.tracks` に実在する id だけを入力順のまま
残し、重複を落とす」) と `:815` `has_deletable_track_selection`。同じ形で
**`daw_gui/src/handler/selection_view.rs` に**作る (この面の arbiter が居るファイル。
`edit_surface` / `delete_current_surface` の両方がここから読む):

```rust
/// r.md #71 (プラグインのコピー / 移動): device 面の一括操作 (削除 / cut / copy /
/// 複製 / 運搬) が受け取る id 集合を正規化する。**いまインスペクタに出ている
/// チェーンに実在する id だけ**を入力順のまま残し、重複を落とす。
///
/// device 選択は「カーソルトラックのチェーン」という面の上にあるので、
/// cursor track が動いた瞬間に元トラックの id が stale になる。 cursor を動かす
/// 経路すべてに掃除を挿す (= 貼り替え補償コード、不変条件 1 が禁じる形) のではなく、
/// **読む側で毎回正規化する**。 `live_track_ids` と同じ流儀。
pub(crate) fn live_device_ids(&self) -> Vec<u64> { ... }
```

- `handler/selection_view.rs:42` `edit_surface`:
  `let devices = !self.live_device_ids().is_empty();` (`:61` の `tracks` の隣)
  last-wins の match (`:83-92`) に `Some(S::Devices) if devices => Some(S::Devices),` を追加。
  **fallback チェーン (`:102-117`) には足さない** (理由は上の doc)。
- `:131` `delete_current_surface` の match (`:135-155`。`_ =>` を持たない網羅 match) に:

```rust
EditSurface::Devices => AppEvent::RemoveDevices { device_ids: self.live_device_ids() },
```

- `SelectDevice` の解決も `prev` に `live_device_ids()` を渡す (下の D-1 末尾のコード)。
  こうすると異トラックの id は最初の click で自動的に落ちる = (b) が構造的に起きない。
- copy / cut / 複製 / 運搬 (§C-4 / D-4-1) も対象 id は `live_device_ids()` から取る。

**そのうえで、device が消える経路では保持側も掃除する** (集合が無限に育たないように):
`remove_devices_inner` / track 削除 / project 切替 / undo-redo で
`selected_device_ids` から実在しない id (`find_device_by_id` が `None`) を落とし、
空になったらタグ (`last_edit_select == Some(Devices)`) を降ろす。
`handler/project.rs:60-75` の選択クリア群にも 2 行足す。
**これは正しさの担保ではなく後始末** — 正しさは上の `live_device_ids()` が持つ。

選択の setter は `set_device_selection(ids: Vec<u64>)` を 1 本作り、
`set_track_selection` と同じ doc の流儀で「ここを通るのは明示的なチェーン操作だけ」
「空になったらタグを降ろす」を書く。

`AppEvent::SelectDevice { device_id: u64, modifier: SelectModifier }` を追加し、
handler は `daw_gui/src/widgets/select_modifier.rs` の
`SelectModifier::resolve` を使う (`Single` / `Toggle` / `RangeFromAnchor`)。

`RangeFromAnchor` の range は **既存ヘルパ `range_ordered(order, anchor, clicked)`
(`widgets/select_modifier.rs:133-141`) をそのまま使う**。これは 1 次元順序面専用で、
トラック / セクション / audio event が既に使っている (自前で min..=max を列挙すると
4 つ目の実装になる)。`order` は cursor track のチェーンの device id 列:

```rust
let order: Vec<u64> = self.inspector_chain().iter().map(|e| e.device_id).collect();
// prev は **正規化済み** を渡す (異トラックの stale id は最初の click で落ちる)。
let prev = self.live_device_ids();
let next = modifier.resolve(&prev, device_id, || {
    self.selection.device_anchor.and_then(|a| range_ordered(&order, a, device_id))
});
self.set_device_selection(next);
if modifier.updates_anchor() {
    self.selection.device_anchor = Some(device_id);
}
```
(アンカー更新規則は `SelectModifier::updates_anchor` が SSoT。`RangeFromAnchor` は
据え置き = 同じ基点から繰り返し Shift+click で範囲を伸縮できる。)

### D-2. daw-ui core: 汎用 drag payload チャネル (新規ファイル)

**`ui/crates/ui/src/drag_drop.rs`** (新規)。
core は DAW を知らない (不変条件 8) ので **型消去した payload** を 1 つ持つだけにする。

```rust
//! widget / view をまたぐ drag&drop の payload チャネル。
//!
//! `reorderable_list` の内部 reorder session が「1 widget の中で閉じた drag」なのに対し、
//! こちらは **掴んだ場所と落とす場所が別 widget** の drag を成立させるための唯一の口。
//! core はペイロードの中身を知らない (型消去 = `Arc<dyn Any>`)。`kind` は
//! 「この drag は誰のものか」を表す静的な札で、drop 側は自分の札とだけ照合する。
//!
//! 寿命: `begin_drag` から `take_drag_payload` / `cancel_drag` まで。誰も取らずに
//! ポインタが release されたフレームの終わりに host が自動で捨てる (= どこにも
//! 落とさなかった drag はキャンセル)。

use std::any::Any;
use std::sync::Arc;

pub struct DragPayload {
    pub(crate) kind: &'static str,
    pub(crate) value: Arc<dyn Any + Send + Sync>,
    /// 掴んだ瞬間のポインタ座標 (drag プレビューの原点)。
    pub origin: (f32, f32),
    /// **ボタンが押されていた最後のフレーム**の修飾キー。 drop 側はこれを読む
    /// (`pointer.modifiers` の生読みではない)。 release フレームの生読みは
    /// `ModifiersChanged` 先行 race で Ctrl が落ちて見え、「Ctrl を押しながら
    /// 離したのに移動になる」が起きる (同じ罠を arrangement が
    /// `ArrangementState.press_modifiers` で回避している —
    /// `daw_gui/src/widgets/arrangement/run.rs:2477-2480` にその理由が書いてある)。
    pub modifiers: Modifiers,
}
```

`ui/crates/ui/src/ui.rs` は **2,794 行**で budget まで 206 行しかないので、
足すのは状態と寿命管理だけにする:
- `UiHost` に `drag_payload: Option<DragPayload>,` を追加 (`new` の初期化も)。
- **`Ui` に `pub(crate) drag_payload: &'a mut Option<DragPayload>,` を追加**して
  `:791` の `Ui { .. }` 構築で `drag_payload: &mut self.drag_payload,` を渡す。
  `Ui` は `&mut UiHost` を持たず **フィールドごとに借用**する構造なので
  (`pending_clipboard_writes: &mut self.transient_clipboard_writes` 等)、
  この 1 行を通さないと `drag_drop.rs` の `impl Ui` から payload に触れない。
- **寿命 hook は `frame_to_edits_with_fonts` (`:591`) に置く** — 下記。

> **`frame_to_edits` (`:569`) に置いてはいけない。** `:569` は
> `owned_font_system` を出し入れして `:582` で `frame_to_edits_with_fonts` を呼ぶだけの
> 6 行 wrapper で、**production はここを通らない**: daw_gui は
> `daw_gui/src/view/runner.rs:1418` で `frame_with_fonts` (`ui.rs:470`) を呼び、
> それが `:481` で `frame_to_edits_with_fonts` を **直接**呼ぶ。
> wrapper 側に置くと (1) `drag_modifiers()` が「押されていた最後のフレーム」に
> 更新されず、(2)「どこにも落とさなかった drag は release フレーム末尾で自動キャンセル」
> という**構造的保証が production で一切効かない** (payload が居座り、次の無関係な
> release が `RelocateDevices` を commit しうる)。しかも D-2 / F-3 のテストは
> `host.frame_to_edits(...)` を呼ぶ既存 idiom (`reorderable_list.rs:625` 等) なので、
> **テストは緑のまま production だけ壊れる** = このリポジトリが最も嫌う false green。
> 本体に置けば wrapper 経由も本体直呼びも同じコードを通るので、テストは
> `frame_to_edits` のままで production を守れる (F-3 参照)。

**`Ui` のメソッドは `drag_drop.rs` 側の `impl<'a, M: ?Sized + 'static> Ui<'a, M>`
ブロックに書く** (widget 群が既にこの流儀。例: `reorderable_list.rs:189`)。API:

```rust
/// この frame から widget をまたぐ drag を始める。`kind` は drop 側と照合する静的な札。
/// `origin` は `self.pointer.pos.unwrap_or((0.0, 0.0))`、`modifiers` は
/// `self.pointer.modifiers` を **この場で** 埋める (= 掴んだフレームの値が初期値。
/// 以後は下の frame 頭 hook が「押されていた最後のフレーム」へ更新し続ける)。
/// 既に payload があれば上書きする (drag は同時に 1 本だけ)。
pub fn begin_drag<T: Any + Send + Sync>(&mut self, kind: &'static str, value: T);
/// 運搬中の payload を **消費せずに** 覗く (札が一致するときだけ `Some`)。
pub fn drag_payload<T: Any + Send + Sync>(&self, kind: &'static str) -> Option<Arc<T>>;
/// 運搬中の payload を取り出して drag を終える (drop の commit)。
pub fn take_drag_payload<T: Any + Send + Sync>(&mut self, kind: &'static str) -> Option<Arc<T>>;
/// 運搬中の札 (`None` = drag していない)。drop target が indicator を出すかの判定に使う。
pub fn dragging_kind(&self) -> Option<&'static str>;
/// ボタンが押されていた最後のフレームの修飾キー。 drop 側の「移動 / コピー」判定は
/// **必ずこれ**を使う (release フレームの `pointer().modifiers` は race で落ちる)。
pub fn drag_modifiers(&self) -> Option<Modifiers>;
pub fn cancel_drag(&mut self);
```

- **frame 頭の hook** — `frame_to_edits_with_fonts` の
  `let FrameInput { pointer, .. } = input;` (`:604`) の直後、**`Ui` 構築 (`:791`) より前**
  (そこで `&mut self.drag_payload` を借りるので、それ以降は `self` から書けない):

```rust
// r.md #71 (プラグインのコピー / 移動): 運搬中の payload の修飾キーを
// 「**ボタンが押されていた最後のフレーム**」に保つ。 release フレームは
// primary_pressed == false なので更新されず、直前の値が残る = drop 側は
// ModifiersChanged 先行 race に晒されない (`DragPayload::modifiers` の doc 参照)。
if let Some(p) = self.drag_payload.as_mut()
    && pointer.primary_pressed
{
    p.modifiers = pointer.modifiers;
}
```
- **frame 末尾の hook** — `drop(ui)` (`:895`) より後、`edits` を返す (`:915`) 直前。
  `pointer` はこの位置でもまだ生きている (`:887` が `primary_just_released` を読んでいる):

```rust
// r.md #71: **どこにも落とさなかった drag は release フレームの末尾で捨てる**。
// これを host 側に置くことで「取り消し忘れ」の分岐を view から構造的に無くす。
// 落とし先の view はこの同じフレームの中で take_drag_payload するので取りこぼさない。
if pointer.primary_just_released {
    self.drag_payload = None;
}
```
- `ui/crates/ui/src/lib.rs` に `pub mod drag_drop;` と `pub use drag_drop::DragPayload;`、
  さらに **`pub use daw_ui_platform::Modifiers;`** (`:42` の `CursorIcon` 再 export の隣) を
  追加する。`Modifiers` は `ui/crates/platform/src/event.rs:47` の型で、`daw-ui-core` は
  `input.rs:5` の **private な `use`** でしか取り込んでおらず `lib.rs` の再 export に無い。
  `DragPayload.modifiers` / `ReorderableListResponse.clicked_modifiers` を public フィールドに
  出す以上、再 export しないと caller が型名を書けない
  (derive は `Debug, Clone, Copy, Default, PartialEq, Eq, Hash` なので `Default` 要件は満たす)。
- `ui/crates/ui/src/drag_drop.rs` の `mod tests` に
  「begin → drag_payload で覗ける → take で消える」「release frame で自動 cancel」
  「kind 不一致では取れない」「押している間に Ctrl を離しても `drag_modifiers()` は
  最後に押されていたフレームの値を返す」の 4 本を書く (`UiHost::no_redraw()` +
  `FrameInput` の既存テスト idiom は `reorderable_list.rs` の `mod tests` を参照)。
  テストは既存 idiom どおり `host.frame_to_edits(...)` を使ってよい —
  `:569` は `:582` で本体 (`frame_to_edits_with_fonts`) に委譲するので、
  hook を本体に置いた以上 **テストの入口と production の入口が同じコードを通る**。
  逆に言えば、hook を wrapper 側に置いたらこのテストは意味を失う (上の警告)。

### D-3. daw-ui: `reorderable_list` を複数選択 + リスト外 drag に対応させる

`ui/crates/ui/src/widgets/reorderable_list.rs`:

1. **複数選択**: `selected: Option<usize>` → **`selected: &[usize]`**
   (`reorderable_list` `:203` / `reorderable_list_expandable` `:239` /
   `reorderable_list_core` `:271`)。描画の `selected == Some(i)` (`:456` / `:516`) は
   `selected.contains(&i)` に。doc の「`selected: Option<usize>` は描画用ハイライトのみ」も
   `&[usize]` 前提に書き直す。
2. **外部 drag の受け入れ札**: 引数を 1 つ足す
   (`accept_drag_kind: Option<&'static str>`、`style` の直前)。
   3 関数すべてに。`None` = 外部 drag を受け付けない (既存の呼び出し互換ではなく
   全 caller を直す)。**caller は 1 か所ではない** — 直す先は
   `daw_gui/src/view/track_inspector/mod.rs:1116`、
   `ui/crates/ui/tests/ui/pass/basic.rs:125` (trybuild の pass ケース。
   `daw-ui-core` は `make test-nolaunch` の対象なので落とすと赤になる)、
   `reorderable_list.rs` 内 `mod tests` の 9 呼び出し
   (`:625/651/703/756/785/833/875/901/939`)。
3. **`ReorderableListResponse` を拡張**。`row_rects` が入るので
   `#[derive(Clone, Copy, Debug, Default)]` から **`Copy` を外す**
   (`:152`。caller は値で 1 回受けるだけなので影響なし):

```rust
#[derive(Clone, Debug, Default)]
pub struct ReorderableListResponse {
    pub clicked: Option<usize>,
    /// `clicked` を起こした **press フレーム**の修飾キー。 選択遷移
    /// (Ctrl / Shift) は必ずこれで決める。 release フレームの生読みは
    /// `ModifiersChanged` 先行 race で修飾が落ちて見え、Ctrl+click が
    /// Single に化ける (arrangement が `press_modifiers` で同じ罠を回避している)。
    pub clicked_modifiers: Modifiers,
    pub hovered: Option<usize>,
    pub dragging: Option<usize>,
    /// 掴んだ行がリスト矩形の **横へ出た最初のフレーム**だけ `Some(index)`。
    /// widget 内部の reorder session はこの時点で破棄され、以後 `Reorder` は
    /// 発行されない。caller はここで [`Ui::begin_drag`] して運搬を引き継ぐ。
    pub dragged_out: Option<usize>,
    /// `accept_drag_kind` と一致する外部 drag が **`rect` の上にある**ときの挿入位置
    /// (`0..=items.len()`)。 リストの外にポインタがあるフレームは `None`
    /// (= indicator を出さない / drop も受けない)。drop indicator は widget が描く。
    pub external_insert_at: Option<usize>,
    /// 上の位置で **このフレームに release された** (= drop 確定)。caller は
    /// [`Ui::take_drag_payload`] して commit する。
    pub external_dropped_at: Option<usize>,
    /// このフレームに描いた行の `(index, 画面座標 rect)`。 caller が
    /// `context_menu_for` / overlay を重ねるため (arrangement の
    /// `track_header_rects` / `clip_rects` と同じ contract)。 可視行のみ。
    pub row_rects: Vec<(usize, Rect)>,
}
```

4. **実装箇所** (`reorderable_list_core`):
   - press 検出 (`:325-357`) で `state.session` を作るときに、その frame の
     `pointer.modifiers` を `ReorderSession` に一緒に保存する
     (`ArrangementState.press_modifiers` と同じ理由)。`clicked` を返す
     `:390-402` の分岐で `clicked_modifiers` に載せる。
   - **運び出し (`dragged_out`) の判定は「横に出たか」だけを見る。** drag continue
     (`:359-366`) の中で、session があり `pointer.pos` の **x** が
     `rect.x - CARRY_OUT_MARGIN_PX .. rect.x + rect.w + CARRY_OUT_MARGIN_PX` の外なら
     `state.session = None` にして `dragged_out = Some(anchor_index)` を立てる。
     **`Reorder` は発行しない**。

```rust
/// リスト外へ運び出す判定の余白 (px)。
///
/// **縦のはみ出しでは運び出さない**: reorder は `compute_reorder_target_index` が
/// y だけを見る **1 次元の gesture** で、リストより上/下にポインタがあることは
/// 「先頭へ / 末尾へ動かす」の途中経過として意味を持つ。 一方 x はリスト内で
/// 何の意味も持たないので、横に出たことが「別の場所へ運ぶ」の曖昧さのない合図になる。
///
/// 余白ゼロだと事故る: inspector の chain は幅 260px 前後 (`area.w - pad*2`) しか
/// 無く、普通に並べ替えているだけで数 px 横に揺れる。 その瞬間に reorder が失われて
/// トラック跨ぎの運搬に化けたら、並べ替えが「たまに効かない」機能になる。
const CARRY_OUT_MARGIN_PX: f32 = 24.0;
```
   - 一度 `dragged_out` した drag は **戻ってきても内部 session は復活しない**。
     戻り先での精密な挿入は `accept_drag_kind` 側 (= 外部 drag の
     `external_insert_at` / `external_dropped_at`) が受ける。同一チェーンへ落とせば
     `RelocateDevices` の src == dest = 並べ替えとして正しく処理される (§B-3 手順 2)
     ので、経路が 2 本あっても結果は 1 つ。
   - 外部 drop 位置の計算は **`tops` (= 表示中のレイアウト)** を使う。
     **`rect.contains` の gate が必須** — 無いと画面のどこで drag していても
     チェーンに indicator が出て、しかも release でその位置に落ちてしまう
     (アレンジのトラックヘッダへ落とす D-5 と二重に発火する)。gate を入れれば
     2 つの drop 経路は幾何的に排他になる:

```rust
// 外部 drag の挿入位置。**ポインタがこのリストの矩形の中にあるフレームだけ**
// 出す (外にあるフレームは None = indicator も drop も無し。 落とし先が
// アレンジのトラックヘッダのときは D-5 側が受ける = 経路は幾何的に排他)。
// 行の中点より上なら手前、下なら後ろ。tops は展開高込みの実表示レイアウトなので、
// アコーディオンが開いていても indicator が行とずれない。
let external_insert_at = accept_drag_kind
    .filter(|k| self.dragging_kind() == Some(*k))
    .and_then(|_| pointer.pos)
    .filter(|&(px, py)| rect.contains(px, py))
    .map(|(_px, py)| {
        let local_y = py - rect.y + self.scroll_offset(("reorderable_list_scroll", &id)).1;
        (0..item_count)
            .filter(|&i| tops[i] + style.row_height * 0.5 < local_y)
            .count()
    });
```
   - `external_insert_at` が `Some` のときは、内部 drag の drop indicator と
     **同じ描画コード** (`:540-580`) を挿入位置 `tops[insert_at]`
     (`insert_at == item_count` なら `content_h`) に対して描く。
     内部 session と外部 drag は同時に成立しないので分岐は排他。
   - release フレームで `external_dropped_at = external_insert_at` を立てる。
   - `row_rects` は描画ループ (`:440-484` の可変高側 / `:503-` の uniform 側) で
     `row_rect` を積むだけ。`Cell<Vec<_>>` か `RefCell` に溜めて最後に response へ移す
     (`hovered` が `Cell` を使っているのと同じ理由 = クロージャ内から書くため)。
5. **テスト** (`mod tests` に追加):
   - `selected` が複数 index を受け取ってハイライトすること (既存の色検証 idiom)。
   - 掴んだまま **横へ** `CARRY_OUT_MARGIN_PX` を超えて出すと `dragged_out` が
     1 度だけ立ち、`Reorder` Edit が発行されないこと。
   - 掴んだまま **縦に** リスト外 (上 / 下) へ出しても `dragged_out` は立たず、
     release で `Reorder` が出ること (= 並べ替えを壊さない回帰テスト)。
   - 横に出しても `CARRY_OUT_MARGIN_PX` 以内なら `dragged_out` が立たないこと。
   - `accept_drag_kind` 一致の drag 中に `external_insert_at` が行の中点で切り替わり、
     release で `external_dropped_at` になること。
   - **ポインタが `rect` の外にあるフレームは `external_insert_at` が `None`** で、
     そこで release しても `external_dropped_at` が立たないこと (D-5 との排他の担保)。
   - Ctrl+click の `clicked_modifiers.ctrl` が true になること
     (press フレームで Ctrl、release フレームで Ctrl を離す入力列を作る)。

### D-4. インスペクタ Chain の配線 (`daw_gui/src/view/track_inspector/mod.rs`)

#### D-4-0. 先に `mod.rs` を割る (不変条件 9。**足す前にやる**)

`daw_gui/src/view/track_inspector/mod.rs` は **現在 2,623 行**で、3,000 行の
god file budget まで 377 行しかない。D-4 が足すもの (選択配線 / 運び出し /
外部 drop / 右クリックメニュー 5 項目 / popup gate) はその大半を食う。
CLAUDE.md の不変条件 9 は「**超過したら分割してから足す**」なので、**先に割る**。

> (当時の指標 = 物理行 3,000。r.md #76 で実コード行 1,000 + 関数 300 行 + インデント 6 段へ
> 置換済み。現在値は `python scripts/loc_budget.py --report`。
> **新指標では `track_inspector/mod.rs` は実コード 2,214 行 = 1,000 行 budget の 2 倍超**、
> `draw` 単体で実コード 2,063 行 = 関数 budget の 6.9 倍で、どちらも
> `scripts/arch_lint_baseline.txt` に登録済み。「377 行しか余裕が無い」という当時の根拠は
> 新指標では更に強くなる。この計画書の他の箇所 (§0.5 の触るファイル一覧の
> 「god file budget」/ §G 流儀の「god file budget (不変条件 9、3,000 行)」) に出てくる
> 3,000 行も同じく当時の値。)

割り方は自明で、`draw` の中の `ui.reorderable_list_expandable(` (`:1116`) は
**`:2489` まで続いており、その `expansion` クロージャ (`:1195-2488`) が約 1,300 行を
抱えている**: Group Transform (`:1201-`)、映像 FX param (`:1377-`)、plugin param
(`:1467-`)、Text Event (`:1588-`)、口パク出力先 binding (`:2412-`) まで。
(`:2490` 以降 — `chain_sections::*` の 5 セクション `:2498-2505`、口パク mapping
`:2509-`、modulation rack — は expansion の **外**なので動かさない。)
インデントが 4 スペースのままクロージャに包まれているので、境界は
`// ====== 開いたデバイスの param パネル本体` (`:1199`) と
`// ====== /device param panel ======` (`:2480`) のコメントで見分ける。これを

**`daw_gui/src/view/track_inspector/device_panel.rs`** (新規) の
`pub(super) fn draw_device_panel(app, ui, area, pad, exp_rect) -> f32`

へそのまま移す (`chain_sections.rs` / `modulation_rack.rs` が既に持っている
`(app, ui, area, pad, y) -> f32` contract の仲間。展開部は起点が `exp_rect.y` なので
引数だけその形)。`mod.rs` 側の expansion クロージャは

```rust
|ui, _exp_i, exp_rect| {
    let measured = device_panel::draw_device_panel(app, ui, area, pad, exp_rect)
        - exp_rect.y;
    if (app.ui_ephemeral.inspector_device_panel_h - measured).abs() > 0.5 { ... }
},
```
だけになる。クロージャが `draw` から借りていたローカル (`p = &app.theme.core`、
`this_id`、`track` 等) は移設先で組み直す (`chain_sections.rs` の各関数が
`let p = &app.theme.core;` から始めているのと同じ)。
移動後 `mod.rs` は約 1,340 行、`device_panel.rs` は約 1,300 行で、
どちらも budget 内に収まる。**挙動は変えない** (純粋な移設 + インデント整形。
移設のみの状態で `make check` を通してから D-4-1 以降に進むと、後の差分が読める)。

#### D-4-1. チェーン行の配線

- `ChainEntry` に `device_id` が入ったので、`selected` は
  (`chain` は cursor track の表示チェーンなので、ここで交差を取ること自体が
  D-1 の `live_device_ids()` と同じ正規化になる = 異トラックの id は出ない)

```rust
let selected: Vec<usize> = chain
    .iter()
    .enumerate()
    .filter(|(_, e)| app.selection.selected_device_ids.contains(&e.device_id))
    .map(|(i, _)| i)
    .collect();
```
- `accept_drag_kind: Some(crate::app_types::DEVICE_DRAG_KIND)` を渡す。
  `DEVICE_DRAG_KIND: &str = "daw_01.device_chain"` を `app_types.rs` に定数で置く。
- `chain_style.drag_handle_w` は現状 `0.0` (行全体 drag)。**このままにする** —
  行内の `Par` / `GUI` / `x` ボタンは `button_at` が click を消費するので、
  行の残り領域を掴んで動かせる (Ableton の「タイトルバーを掴む」に相当)。
  ただし `drag_handle_w == 0.0` のときだけ `Response.clicked` が出る仕様なので、
  選択はその `clicked` で行う。
- 応答の処理:

```rust
// 行 click = 選択 (無修飾 / Ctrl / Shift)。 修飾キーは widget が **press フレームで
// 捕まえた値** (`resp.clicked_modifiers`) を使う — release フレームの生読みは
// ModifiersChanged 先行 race で Ctrl+click が Single に化ける (D-3)。
// 右クリックメニューが開いている frame は評価しない (下の popup gate)。
if !popup_open
    && let Some(i) = resp.clicked
    && let Some(e) = chain.get(i)
{
    let device_id = e.device_id;
    let m = resp.clicked_modifiers;
    let modifier = SelectModifier::from_modifiers(m.shift, m.ctrl);
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SelectDevice { device_id, modifier });
    }));
}
// リスト外へ出た = トラック跨ぎの運搬を始める。運ぶ対象は「掴んだ行が選択に
// 含まれていれば選択全体、含まれていなければその行だけ」(トラックヘッダの
// 右クリックメニューと同じ規則、arrangement_view.rs:587-595)。
if let Some(i) = resp.dragged_out
    && let Some(e) = chain.get(i)
{
    let mut ids: Vec<u64> = if app.selection.selected_device_ids.contains(&e.device_id) {
        chain.iter().filter(|c| app.selection.selected_device_ids.contains(&c.device_id))
            .map(|c| c.device_id).collect()
    } else {
        vec![e.device_id]
    };
    ui.begin_drag(DEVICE_DRAG_KIND, DeviceDragPayload {
        device_ids: std::mem::take(&mut ids),
        source_track: cursor_tid.unwrap_or(common::model::MASTER_TRACK_ID),
    });
}
// 落とした = 挿入位置を確定して移動 / コピー。既定は移動、Ctrl でコピー。
// 修飾キーは payload が持っている「押されていた最後のフレーム」の値 (D-2)。
if let Some(at) = resp.external_dropped_at
    && let Some(copy) = ui.drag_modifiers().map(|m| m.ctrl)
    && let Some(p) = ui.take_drag_payload::<DeviceDragPayload>(DEVICE_DRAG_KIND)
    && let Some(dest_track) = cursor_tid
{
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
            device_ids: p.device_ids.clone(),
            dest_track,
            dest_index: at as u32,
            copy,
        }));
    }));
}
```

`DeviceDragPayload` は `daw_gui/src/app_types.rs` に:

```rust
/// r.md #71: チェーンから掴んだプラグインの運搬中データ (daw-ui の drag payload に載る)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDragPayload {
    /// 運ぶ device (チェーン表示順)。
    pub device_ids: Vec<u64>,
    /// 掴んだときのチェーン所有者 (ドラッグ中の表示切り替えで cursor が動くので、
    /// 「どこから来たか」は payload 側が覚えておく)。
    pub source_track: u32,
}
```

**右クリックメニュー**: `ui.context_menu_for` は **widget の外 (caller 側) で重ねる**。
これがこのリポジトリの確立した idiom で、arrangement は `track_header_rects` /
`clip_rects` / `automation_point_rects` を response で返し、caller が
`for (id, rect) in &resp.x_rects { ui.context_menu_for(*rect, ...) }` する
(`daw_gui/src/widgets/arrangement/mod.rs:17` / `:793-831` が明記)。row callback の
中で開くと (a) その idiom から外れ、(b) 行の描画クロージャが popup のライフサイクルを
抱えることになる。D-3 で足す `resp.row_rects` を使って widget 呼び出しの **後**に:

```rust
for (i, row_rect) in &resp.row_rects {
    let Some(e) = chain.get(*i) else { continue };
    let device_id = e.device_id;
    ui.context_menu_for(*row_rect, &["コピー", "切り取り", "貼り付け", "複製", "削除"], move |idx, ui| {
        ...
    });
}
```

**`[[feedback_popup_click_leaks_to_background]]` の罠**: context menu は
`capture_input=false` の popup なので背景の pointer を mask せず、項目 click が
下の chain 行まで届いて選択が消える / `x` ボタンが誤爆する。**chain ブロックの
先頭 (widget 呼び出しの前)** で

```rust
// 右クリックメニューが開いている frame は行の click / button を評価しない
// (`[[feedback_popup_click_leaks_to_background]]`: capture_input=false の popup は
// 背景の pointer を mask しないので、項目 click が下の行まで届く)。
let popup_open = ui.has_open_popups();
```
を取り、**`resp.clicked` の処理と、row callback 内の `button_at`
(`Par`/`GUI` = `:1160-1174`、`x` = `:1176-1186`) が発行する Edit の両方**を
`!popup_open` で gate する (`button_at` 自体は描いてよい。発行を止める)。

メニュー項目の対象は右クリックした device が選択に含まれるなら選択全体、
含まれないならその device 単独 (トラックヘッダと同規則)。
「貼り付け」は §C の `paste_devices` (挿入位置 = この device の直前)、
「複製」は `RelocateDevices { copy: true, dest_index: この device の次 }`。

### D-5. アレンジのトラックヘッダで表示を切り替える (`daw_gui/src/view/arrangement_view.rs`)

`:571` の `for (track_id, rect) in &resp.track_header_rects` ループの **直前**に:

```rust
// r.md #71: チェーンから掴んだプラグインをトラックヘッダの上に持っていくと、
// インスペクタの表示をそのトラックのチェーンへ切り替える (= 運び先が見える)。
// 切り替えのトリガは **アレンジのトラックヘッダだけ** (ミキサーのストリップでは
// 切り替えない)。ヘッダの上で離したらそのトラックのチェーン末尾に入れる。
if ui.dragging_kind() == Some(DEVICE_DRAG_KIND)
    && let Some((px, py)) = ui.pointer().pos
    && let Some(&(track_id, _)) = resp.track_header_rects.iter().find(|(_, r)| r.contains(px, py))
{
    if ui.pointer().primary_just_released {
        // 修飾キーは payload が持つ「押されていた最後のフレーム」の値 (D-2)。
        let copy = ui.drag_modifiers().is_some_and(|m| m.ctrl);
        if let Some(p) = ui.take_drag_payload::<DeviceDragPayload>(DEVICE_DRAG_KIND) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let dest_index = app.song_doc.song()
                    .fx_chain_by_track_id(track_id).map_or(0, <[_]>::len) as u32;
                app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
                    device_ids: p.device_ids.clone(),
                    dest_track: track_id,
                    dest_index,
                    copy,
                }));
            }));
        }
    } else if app.cursor_track_id() != Some(track_id) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.focus_inspector_track(track_id);
        }));
    }
}
```

master 行も `track_header_rects` に入る (`daw_gui/src/widgets/arrangement/run.rs:2197`) ので、
master へのドロップは追加コードなしで成立する。

**ミキサーには何も足さない** (`daw_gui/src/view/mixer_strips.rs` は無変更)。

#### D-5-2. drop フレームにアレンジ自身のトラック選択を走らせない (`widgets/arrangement/run.rs`)

上のコードは widget 呼び出しの **後**にあるが、widget 側は同じ release フレームに
**自分のトラック選択を発行してしまう**: `run.rs:2205-2209` (master 行) /
`:2383` / `:2455-2457` が `primary_just_released && row.contains(..)` で
`clicked_track_for_select` を立て、`:2476-2500` が `apply_select_tracks(tid, modifier, ..)`
を push する。コピー修飾が **Ctrl** なので `SelectModifier::from_modifiers(false, true)`
= `Toggle` になり、**Ctrl を押しながらヘッダに落とすと、そのトラックの選択が勝手に
反転する** (落とし先を表示し続けるという本件の要件と真っ向から衝突し、
last-wins タグも `Tracks` に倒れて次の Delete がトラックを消しに行く)。

`:2469` の disclosure 抑止と同じ場所・同じ形で 1 段落足す
(既に「priority が高い操作の frame は選択を走らせない」という先例がある):

```rust
// r.md #71: 外部 drag (device の運搬) を落とした frame は、この release を
// 「ヘッダの click」として扱わない。 扱うと Ctrl+drop が Toggle 選択として
// 解決され、落とし先トラックの選択が反転する / last-wins タグが Tracks に倒れる。
// drag の commit 自体は caller (arrangement_view) がこの後で行う。
if ui.dragging_kind().is_some() {
    clicked_track_for_select = None;
}
```

`dragging_kind()` は daw-ui core の汎用 API (札の中身を知らない) なので、
不変条件 8 (core にドメイン知識を持ち込まない) にも触れない。

### D-6. ドラッグ中のプレビュー (`daw_gui/src/view/root.rs`)

view の **最後** (= 一番手前に描かれる位置) に、運搬中のラベルを 1 つ描く:

```rust
// r.md #71: 運んでいる最中に「何を掴んでいるか」を見せる。view の最後に描くので
// 常に最前面。運搬そのものは daw-ui の drag payload が持っているので、
// ここは表示だけ (状態を持たない)。
if let Some(p) = ui.drag_payload::<DeviceDragPayload>(DEVICE_DRAG_KIND)
    && let Some((px, py)) = ui.pointer().pos
{
    let label = if p.device_ids.len() == 1 { "プラグイン 1".to_string() }
                else { format!("プラグイン {}", p.device_ids.len()) };
    // 暗いチップ + 明るい文字でコントラストを保証する
    // (`[[feedback_ui_indicator_contrast_on_variable_bg]]`: 波形やクリップ色の上に
    //  出るので、背景に依存しない下地を必ず敷く)。
    ...push_rect + label_at...
}
```

---

## §E daw_plugin_host / IPC — 帰属 (`track_id`) の二重所有を撤去する

移動を安全にするための本丸。`InstanceRecord.track_id` (`daw_plugin_host/src/main.rs:114`)
は Song が SSoT の情報の複製で、**stale になると「元トラックを削除したら移動先の
device が破棄される」**。列挙は daw_gui 側で既に行っている
(`ClosePluginShmem` を device ごとに撃つため) ので、host 側にもう 1 つ持つ理由が無い。

### E-1. `common/src/protocol.rs`

- `:517-525` `SetSlotPlugin` から `track_id: u32` (`:519`) を **削除**。doc (`:511-513`) の
  「`track_id` は所属 track ... teardown (`RemoveTrack`) 用の帰属情報」の段落も削除。
- `:527-528` `RemoveTrack { track_id: u32 }` を doc ごと **削除**。
- `:529-541` `UnloadAllPlugins` は **残す**。ただし **doc 本文が撤去対象を根拠に
  使っているので書き直す**: 現行 `:536-540` は「`RemoveTrack` の積み重ねでは塞げない:
  列挙元は daw_gui 側の帳簿 (`loaded_slots`)」と、**削除する variant と改名する
  フィールド**の名前で理由を説明している。新しい理由文はこう:
  「r.md #71 で `RemoveTrack` を撤去し、track 削除は Song から列挙した device の
  `RemoveSlotPlugin` に置き換えたが、本 variant は残る — 守っているのは
  **project 切替**で、そこでは列挙元の Song 自体が差し替わり device_id が 1 から
  再採番される。前 project の instance を「列挙して消す」ことが原理的にできない
  (新 Song は旧 id を知らず、旧 Song はもう無い) ので、帳簿にも Song にも依存しない
  『全部捨てろ』が唯一の表現になる。」
- `:742-748` `SlotPluginUnloaded` の doc も直す (「RemoveSlotPlugin / **RemoveTrack**
  経由」「daw_gui ローカルの bookkeeping (**`track_plugin_ids`** / **`loaded_slots`** /
  latency) を片付ける」の 3 語が全部消える)。
- `:928-939` の `set_slot_plugin_roundtrip` テストから `track_id: 7` の行を削除。
- **wire 変更**: `common/build.rs` の `WIRE_SOURCES` には `protocol.rs` が既に
  登録済み (`:20`) なので追加作業は無い。ただし **fingerprint が変わるので
  `cargo build --workspace` で子 exe も作り直すこと**
  (`[[feedback_workspace_build_for_protocol_changes]]`。古い exe が残ると
  decode 失敗 → 「再生が止まる」)。

### E-2. `daw_plugin_host/src/main.rs`

- `:113-114` `track_id: u32,` を削除 (doc コメント「所属 track (`RemoveTrack` の
  帰属情報。アドレスには使わない)」ごと)。
- `:1013-1024` `PluginCommand::RemoveTrack` の arm を削除
  (`rec.track_id == track_id` で列挙している `:1019` もろとも)。
- `:1210` `set_slot_plugin(..., track_id: u32, ...)` の引数を削除。
- `:1221-1228` dedup 分岐の `rec.track_id = track_id;` (`:1228`) と、その理由を
  書いた `:1224-1227` のコメント 4 行を削除 (理由そのものが `RemoveTrack` の列挙で、
  variant ごと消える)。
- `:1367` `InstanceRecord { ..., track_id, ... }` の初期化を削除。
- `:2342-2360` `log_command` の `SetSlotPlugin` destructure / ログから `track_id` を削除。
- `:993/1002` の `set_slot_plugin` 呼び出しから `track_id` を削除。

### E-3. daw_gui 側の帰属列挙

track を消す 3 経路すべてで、**Song から device を列挙する**
(帳簿 `loaded_devices` からではなく) — load 応答待ちの device を取りこぼさないため。
`fx_chain_by_track_id` (`common/src/model.rs:1681`) は削除前の Song からしか引けないので、
**3 経路とも `plan_track_removal_ipc(...)` を「track を Song から外すより前」に呼んで
戻り値を保持し、送信は従来どおり song update / `LoadSong` の後に行う** (順序仕様は不変)。
現行コードはどれも先に Song から外してしまうので、この呼び出し位置がずれると
「plan が空 → IPC が 1 通も出ない」で無言に壊れる。具体的な位置:

| 経路 | いま Song から外している場所 | plan を計算する位置 |
| --- | --- | --- |
| `handler/tracks.rs` 選択トラック削除 | `:571` `edit_song(\|song\| song.tracks.remove(i))` (`:557-571` のループ内) | ループ (`:556`) の直前 |
| `handler/grouping.rs` ungroup | `:222` `edit_song(\|song\| song.tracks.remove(pos))` | `:207` の `edit_song` より前 |
| `handler/grouping.rs` 最終 track 削除 | `:318` `edit_song(\|song\| song.tracks.pop())` | `:318` より前 |

- `handler/grouping.rs:125-138` `plan_track_removal_ipc` は **現時点で呼び出し元が 0 件**
  (grep 実測 2026-08-28: 定義 `:125/128/133/136` と `app_types.rs:10` の doc 参照だけ)。
  3 経路がそれぞれ手で同じ IPC 列を組んでいて、順序仕様を持つはずの純関数が
  使われていない状態。
  本件で **この関数を唯一の口として復活させる** (順序仕様を 1 か所に閉じ込め、
  unit test で固定する)。新しいシグネチャ:

```rust
pub fn plan_track_removal_ipc(
    song: &common::model::Song,
    track_ids: &[u32],
) -> Vec<TrackRemovalIpc> {
    let mut plan = Vec::new();
    for &track_id in track_ids {
        let ids: Vec<u64> = song
            .fx_chain_by_track_id(track_id)
            .map(|c| c.iter().map(|d| d.id).collect())
            .unwrap_or_default();
        // (1) audio engine から先に mapping を落とす (use-after-free deadlock 防止)。
        for &device_id in &ids {
            plan.push(TrackRemovalIpc::CloseAudioShmem { device_id });
        }
        // (2) plugin_host に device 単位で teardown させる。track という単位は
        //     host 側に無い (r.md #71: 帰属の二重所有を撤去した)。
        for device_id in ids {
            plan.push(TrackRemovalIpc::RemoveHostDevice { device_id });
        }
    }
    plan
}
```
  **unit test は現在 存在しないので新設する** (`handler/grouping.rs` の `mod tests`、
  無ければファイル末尾に `#[cfg(test)] mod tests` を足す)。
  **順序 (track ごとに 全 CloseAudioShmem → 全 RemoveHostDevice) は仕様** なので
  「2 track × 2 device で 8 要素が期待順に並ぶ」を assert して固定する。
  doc コメントの 2. も `RemoveTrack(track_id)` から `RemoveHostDevice(device_id)` に
  書き直すこと (説明が古いまま残ると次の読み手が host に track 概念があると誤解する)。
- `handler/tracks.rs:555-585` / `handler/grouping.rs:215-250` / `:311-330` を
  この plan 経由に統一する (3 か所が別々に組み立てているのを 1 本にする)。

---

## §F 検証

### F-1. headless テスト (`daw_gui/tests/app_state/`)

新規 `daw_gui/tests/app_state/device_relocate.rs` (+ `main.rs` に `mod` 追加)。
`support.rs` の `build_app` / `make_plugin_db` / `select_track_single` /
`fake_plugin_loaded` を使う (`group_track_lifecycle.rs` と同型)。

1. `move_between_tracks_keeps_device_id_and_carries_lane`
   - track0 に `test.fx` を載せ、その device に PluginParam lane を 1 本作る
     (`AppEvent` 経由。`handler/automation_lanes.rs` の lane 追加イベントを使う)。
   - `RelocateDevices { copy: false, dest_track: track1, dest_index: 0 }`。
   - assert: device id が不変 / track1.devices に居る / track0.devices が空 /
     **track1.automation_lanes に PluginParam lane が 1 本あり track0 には無い** /
     lane id が track1 の `next_lane_id` 由来で採番し直されている。
2. `move_across_tracks_rekeys_lane_row_override`
   - 移送前に `ui_prefs.automation_lane_row_overrides` に `(track0, lane)` を入れておき、
     移送後に `(track1, new_lane)` へ移っていること / 旧キーが消えていること。
3. `move_across_tracks_drops_ara_archive`
   - `ara_archive = Some(...)` の device を別トラックへ移すと `None` になること。
     同一トラック内の移動では残ること。
4. `copy_allocates_new_id_and_keeps_state`
   - `state = Some(b"abc")` の device をコピー → 新 id ≠ 元 id、`state` は同内容、
     **automation lane と mod_routing は複製されない**、
     `pending_added_plugin_finalize` に新 id が積まれている。
5. `copy_to_other_track_drops_ara_but_same_track_keeps_it`
6. `move_voicevox_moves_vocal_marker`
   - builtin VOICEVOX を track0 → track1 へ移すと
     `track0.source == None` / `track1.source == Vocal`。
6b. `removing_one_of_two_voicevox_keeps_vocal_marker`
   - VOICEVOX を 2 本持つ track で 1 本だけ `RemoveDevices` → `source` は `Vocal` のまま
     (A-5 で足すガード。Transform 側は既存実装が同じ規則を持っているので、
     同じ形のテストを Transform には書かない = 既存挙動の写経にしない)。
7. `deleting_source_track_after_move_keeps_moved_device_loaded`
   - 移動後に元トラックを削除し、`plan_track_removal_ipc` が **移動した device の
     `RemoveHostDevice` を出さない**こと (= 移動先の音が生きる)。
     これが `[[project_plugin_slot_rekey]]` の再発防止テスト。
8. `master_chain_round_trip`
   - track → master → track の往復で lane / device id / 副作用が整合すること。
9. `paste_devices_inserts_before_selection`
   - 3 device のチェーンで 2 番目を選択して paste → 挿入位置が index 1。
     無選択なら末尾。
10. `device_selection_is_scoped_to_displayed_chain` (D-1 の (a)/(b) の回帰テスト)
    - track0 の device を選択 → タグが `Devices` → **`set_track_selection(track1)`**
      (= カーソルトラックだけ動かす、`selected_device_ids` は触らない)。
    - assert: `live_device_ids()` が空 / `edit_surface()` が `Devices` を返さない /
      `delete_current_surface()` が track0 の device を消しに行かない。
    - 続けて track1 の device を Ctrl+click → 選択集合に track0 の id が混ざらないこと。
11. `ensure_ids_reallocates_duplicate_device_ids` は `common/src/model/tests.rs` に
    (A-8)。

### F-2. 既存テストの修正

A-11 の表のとおり。特に落としやすいのは:
- `daw_gui/tests/reconcile_slot_diff.rs` — §A で確実にコンパイルが壊れる。
- `daw_gui/src/app_tests.rs:866-894` (`mod master_fx_tests`) — **`cargo test -p daw_gui --lib`
  でしか出ない**ので、`tests/` だけ直して安心しないこと。`SlotReconcileAction::LoadSlot`
  の destructure が壊れる。
- `daw_gui/tests/app_state/group_track_lifecycle.rs:262-270` — `RemoveTrack` を
  assert しているので **variant 削除でコンパイルが壊れる**。
- `common/src/protocol.rs:928-939` — `track_id: 7` を書いている roundtrip テスト。
- `ui/crates/ui/tests/ui/pass/basic.rs:125` — trybuild の pass ケース。
  `daw-ui-core` は `make test-nolaunch` の対象なので、直さないとそこで赤になる。

### F-3. daw-ui のテスト

- `ui/crates/ui/src/drag_drop.rs` の `mod tests` (D-2)
- `ui/crates/ui/src/widgets/reorderable_list.rs` の `mod tests` 追加 5 本 (D-3)

**どの入口を通るか**: どちらも既存 idiom どおり `host.frame_to_edits(...)` を使う
(`reorderable_list.rs:625` 等)。`frame_to_edits` (`ui.rs:569`) は `:582` で
`frame_to_edits_with_fonts` (`:591`) に委譲し、production (`runner.rs:1418` →
`frame_with_fonts` → `ui.rs:481`) も同じ本体を呼ぶので、**D-2 の hook を本体に置いてある
限り、テストと production は同じコードを通る**。hook を wrapper に置いた瞬間に
このテストは「緑だが production は壊れている」を証明できなくなる (D-2 の警告)。

### F-4. コマンド

```bash
make check
```
```bash
make clippy
```
```bash
make test-nolaunch
```
```bash
make arch-lint
```

`make arch-lint` は **exit 0 かつ「baseline 0 件」**になること
(baseline の POSITIONAL-KEY 4 行を消したので、新規違反が出れば exit 1 で落ちる)。
`ARCH_LINT_STRICT=1 bash scripts/arch_lint.sh` も通ること。

A-10 の検査は **`make` 経由でも素のシェルでも同じ結果**でなければならない
(`[[reference_make_argv_backslash_loss]]`: make → MSYS2 bash → Git の grep で
argv のバックスラッシュが落ちる)。新パターンはバックスラッシュを使わないが、
**両方で 1 回ずつ走らせて hit 数が一致することを確かめる**:

```bash
bash scripts/arch_lint.sh
```
```bash
make arch-lint
```

期待値は **POSITIONAL-KEY 0 件** (`daw_gui/src/video_fx/mod.rs:360` は行内マーカーで
除外されるので出ない)。canary が落ちたら `[SELF-BROKEN]` で exit 1 するので、
「0 件」が検査器の死による偽グリーンでないことはそこで担保される。

protocol を変えたので、実機で動かす前に必ず:
```bash
cargo build --workspace
```

**検証は上のコマンド群で完結させる。`make test` は使わない** —
`make test` と `daw_gui/tests/device_chain_smoke.rs` は
**daw_gui 本体を `--script` で起動し** (`grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`
に載る)、開いているプロジェクトの再生を壊すため、本計画の検証手順には含めない。
`daw_gui/tests/scripts/device_chain_smoke.js` に「device を別トラックへ移した後の
`deviceChain(track)` の並びと ports」を assert する行を足すこと自体は有用なので
**追加だけしておき、実行はユーザーが判断する** (回すなら `DAW01_ALLOW_LAUNCH=1`)。

### F-5. 実機での最終確認 (ユーザーに依頼する項目。自分では起動しない)

1. インスペクタの Chain 行を click / Ctrl+click / Shift+click して選択が変わる。
2. 選択して Delete / Ctrl+C → 別トラックで Ctrl+V。挿入位置が選択の直前。
3. 行を掴んでアレンジのトラックヘッダへ → インスペクタが切り替わる → 戻って離す。
   音が切れない (移動)。Ctrl 押しながらでコピーになる。
   **Ctrl+ヘッダ上で離しても、そのトラックの選択 (ヘッダのハイライト) が
   勝手に反転しないこと** (D-5-2 の回帰確認)。
3b. チェーン内で普通に上下へ並べ替える。**指が少し横に振れても並べ替えが効くこと**
   (D-3 の `CARRY_OUT_MARGIN_PX`)。逆に、はっきり横へ持ち出したら運搬に変わること。
4. automation を書いた plugin を別トラックへ移して、automation がそのまま効く。
5. 移動後に元トラックを削除しても移動先の音が消えない。
6. Melodyne (ARA) を別トラックへ移すと解析がやり直しになる (無音の誤動作をしない)。

---

## §G 流儀 (既存コードに合わせること)

- **doc コメントは日本語**、既存のトーンに合わせる。「なぜそうするか」と
  「そうしないと何が壊れるか」を書く (このリポジトリの doc は挙動の説明より
  失敗のメカニズムを書く密度が高い)。r.md 番号 (`r.md #71`) を根拠として引く。
- 新設した不変条件・順序依存は **その場のコメント**で固定する
  (`ClosePluginShmem` → `RemoveSlotPlugin` の順序、`pending_added_plugin_finalize` を
  `restore_device` より先に積む、など)。
- `let-else` で早期 return、`?` を `match` より優先、`unsafe extern` (Edition 2024)。
- **エラーを握りつぶさない**: `find_device_by_id` が `None` を返す経路は
  「削除済み device の stale event」なのか「設計バグ」なのかを分けて、
  後者は `tracing::error!` を出す (既存 `send_set_slot_plugin` の
  `device id unallocated` と同じ判断)。
- **god file budget (不変条件 9、3,000 行)**。触るファイルの現在行数 (実測 2026-08-28) と
  見込み。**「超えそうなら」ではなく、先に割ると決めているものが 1 つある** (D-4-0):

  | ファイル | 現在 | 見込み | 対処 |
  | --- | --- | --- | --- |
  | `view/track_inspector/mod.rs` | **2,623** | D-4 の追加で 2,900 前後 | **D-4-0 で `device_panel.rs` へ約 1,300 行を移してから足す** (移設後 ~1,340) |
  | `ui/crates/ui/src/ui.rs` | **2,794** | +15 | drag の **`Ui` メソッドは `drag_drop.rs` の `impl Ui` ブロックに置く** (widget 群と同じ流儀)。`ui.rs` に足すのは `UiHost` の 1 フィールド / `Ui` の 1 フィールド + `:791` の構築 1 行 / `frame_to_edits_with_fonts` の hook 2 か所だけ |
  | `common/src/model.rs` | **2,927** | +20 前後 (A-8 / A-9) | 収まるが余裕 73 行。足したあと `make arch-lint` の FILE-BUDGET を必ず見る |
  | `widgets/arrangement/run.rs` | 2,699 | +6 (D-5-2) | 収まる |
  | `common/src/model/tests.rs` | 2,561 | +40 | 収まる |
  | `daw_gui/src/app_types.rs` | 2,327 | +80 | 収まる |
  | `daw_gui/src/event.rs` | 1,912 | +30 | 収まる |
  | `daw_gui/src/handler/project.rs` | 1,907 | ほぼ増減なし (Phase A -45 / `restore_device` +15) | 収まる |
  | `daw_gui/src/view/root.rs` | 1,696 | +80 (C-4 の 3 arm + D-6) | 収まる |
  | `daw_gui/src/handler/devices.rs` | 1,364 | +150 (3 関数 -110 / 新 4 関数 +260) | 収まる |
  | `ui/.../reorderable_list.rs` | 969 | +180 | 収まる |
  | `daw_gui/src/clipboard.rs` | 505 | +130 | 収まる |
- **`r.md #71` という文字列は既に別機能で使われている** — `common/src/model.rs:785/830/856/975`
  ほか計 19 か所が **セクション帯の D&D** (旧 #71) を指している
  (`[[reference_rmd_numbering_reuse]]`: r.md の番号は書き換えで別機能を指す)。
  本件で新しく書くコメントは **`r.md #71 (プラグインのコピー / 移動)`** のように
  機能名を併記して、grep したときに混ざらないようにする。既存 19 か所は触らない。
- **RT スレッドは触らない**: 本件の変更経路はすべて GUI スレッド / off-thread。
  daw_audio 側は `LoadSong` (= `Topology::Recompile`) で追従するだけで、
  新しいコードを RT に入れない。

---

## 参照

- r.md:11 — #71 の要求文
- `docs/plan_linear_chain.md:98-140` — 単一デバイスチェーン設計 (並び替えは全許可)
- `docs/plan_arch_refactor.md` §1 (安定 id addressing) / §2 (blob-less wire) /
  §7.5 (state 分割表) / §11 (arch-lint)
- `docs/plan_fixme_33_clipboard.md` — 統一クリップボードの確定仕様
- `CLAUDE.md`「アーキテクチャ不変条件」1 / 2 / 3 / 5 / 7 / 8 / 9
- `common/src/protocol.rs:530-541` — `UnloadAllPlugins` の doc
  (「`RemoveTrack` の積み重ねでは塞げない ... 帳簿に依存しない唯一の表現」)
- `common/src/plugin_ref.rs:194-203` — `process_data_shmem_id(pid, device_id, incarnation)`
  (「addressing は再利用される id でよいが、リソース名は再利用されてはならない」)
- `daw_audio/src/automation.rs:145-209` — `fill_pd_param_events` が track から lane を
  引き device_id で絞る (= lane を元トラックに残すと automation が死ぬ根拠)
- `daw_gui/src/handler/tracks.rs:229-360` — `build_pasted_tracks`
  (device id 再採番 + lane/mod_routing の device_id 貼り替え + aux 参照解決。
  §B のコピーが手本にする既存実装)
- `daw_gui/src/handler/grouping.rs:113-138` — `ClosePluginShmem` を先に送る理由 (deadlock)
- Ableton Live 12 マニュアル「Working with Instruments and Effects」
  <https://www.ableton.com/en/live-manual/12/working-with-instruments-and-effects/>
  - 「Devices can be moved to other tracks entirely by dragging them from the Device
    View into the Session or Arrangement Views.」
  - 「Edit menu commands such as cut, copy, paste, and duplicate can be used on
    devices. Pasted devices are inserted in front of the selected device.」
  - 「To change the order of devices, drag a device by its title bar and drop it next
    to any of the other devices in the Device View.」
  (WebFetch で verbatim 一致を確認済み。**Ctrl+V の挿入位置と D&D の根拠**)
- Bitwig Studio ユーザーガイド「Working with Devices」
  <https://www.bitwig.com/userguide/latest/working_with_devices/>
  - 「click and drag the device header to the desired position within the Device Panel.」
  - 「CTRL (ALT on Mac) can be added to toggle the move to a copy function.」
  - 「Once selected, all regular Edit functions apply, such as cut, copy, duplicate,
    and delete.」
  (WebFetch で verbatim 一致を確認済み。**既定=移動 / Ctrl=コピー と選択機構の根拠**)
- REAPER ReaScript API `TrackFX_CopyToTrack(src_track, src_fx, dest_track, dest_fx, is_move)`
  <https://www.reaper.fm/sdk/reascript/reascripthelp.html>
  (シグネチャは verbatim 確認済み。**ただし「src_track=dest_track で reorder に使える」
  という説明文はこの URL 上で確認できなかったので、設計の根拠には使っていない。**
  同様に、調査時に v5.27 changelog として引用されていた 5 行は
  <https://www.reaper.fm/whatsnew.txt> の現行配信内容 (v7.53〜7.79) に存在しないため、
  本計画では REAPER の挙動を根拠に採用していない)
