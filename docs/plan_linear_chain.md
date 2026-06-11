# plan: 単一デバイスチェーンへの作り直し（役割の位置導出）

2026-06-11 開始。FIXME #32 の振り返りで「役割をデータ構造に焼き込むモデルでは
slot/role 遷移パターンを網羅できない」と確認したことを受けた、プラグインチェーンの
根本作り直し。ユーザー合意済み（単一チェーン化を選択）。

## 1. 根本原因（なぜ網羅できないか）

現行: `Track { midi_fx_chain: Vec, instrument: Option, fx_chain: Vec }` ＋
`PluginSlot = MidiFx(i) | Instrument | Fx(i)`。プラグインの**役割が「どの入れ物に
入っているか」で決まる**ため、役割をまたぐ操作が全部個別特殊ケースになる:

- dual-role を音源に → 別音源を足すと降格（FIXME #29/#31、専用 IPC）
- **音源を消すと降格済み生成器が宙ぶらりんで無音（promote-back ハンドラ不在）**
- セクション跨ぎ並び替え（FIXME #32、能力チェック＋3プロセス再キー）

操作 × 役割遷移 × 3プロセス の組合せを手書きするので穴が消えない。

## 2. 一次情報（調査結果）

- **Ableton/Bitwig/Ardour は単一線形チェーン**。役割は宣言せず**位置で自動導出**。
  音源 = 最初に MIDI→audio へ変換する機。その前が MIDI FX、後が audio FX。
  並び替え/挿入/削除で役割は自動伝播（専用 state 管理なし）。
  - Ableton manual: "signals following the instrument are audio … preceding are MIDI"
  - Bitwig: bucket-brigade。note FX は note を通し、instrument は note→audio。
  - Ardour: I/O 数で信号型が trickle down。
- **実 DAW に「dual-role 機」は無い**。Scaler 等の MIDI 生成器は **MIDI FX** 扱い。
  ただし daw_01 のプラグインは note-out と audio-out を**両方**宣言する（Scaler は
  単体で鳴る＝audio-out あり、降格できる＝note-out あり）。→ **位置による導出規則**で
  dual-capable を吸収する（実 DAW の手動分類より上位互換）。
- **CLAP/VST3**: port 宣言（note-in/out, audio-out）は static。dual-capable の扱いは
  **host policy**（spec は規定しない）。host が「下流が MIDI を欲しがるか」で
  生成器/音源を決める。
- **sing_like_coding（前作）**は既に `Track { modules: Vec<Module> }` の単一 Vec、
  `(track, index)` アドレス、reorder/add/remove は Vec 操作＋参照 index 再マップ。
  → テンプレートとして流用可（`prepare_module_audio` のバッファ pull ループ）。

参照: `docs/plan_plugin_reorder.md`（FIXME #32 の経緯）, port は
`common/src/port_config.rs`（3-bool SSoT: has_note_input/has_note_output/has_audio_output）。

## 3. 新データモデル

```rust
// common/src/model.rs
struct Track {
    devices: Vec<PluginInstance>,   // 旧 midi_fx_chain + instrument + fx_chain を一本化
    // ...他は不変
}
struct PluginInstance {
    plugin_id: String,
    format: PluginFormat,
    state: Option<Vec<u8>>,
    sidechain_sources: Vec<Option<u32>>,
    ports: PortConfig,              // ★追加: 役割導出の入力（load 時に DB から解決）
}
```

- **`PluginSlot` enum 撤廃**。アドレスは `(track_id, device_index: u32)`。
- `master_fx_chain: Vec<PluginInstance>` は既に単一 Vec＝そのまま（全 audio FX 扱い、
  音源境界なし）。非 master トラックだけ作り直す。
- `AutomationTarget::PluginParam { slot } → { device_index: u32 }`。
- `PortConfig` を `PluginInstance` に持たせ **LoadSong で daw_audio に運ぶ**ことで、
  daw_audio が DB 無しに役割導出できる（SSoT: 導出入力は song が持つ）。

## 4. 役割導出ルール（設計の心臓・SSoT）

各 device の `PortConfig (N_i=note_in, N_o=note_out, A_o=audio_out)` から、チェーンを
左→右に1回歩いて役割を決める純粋関数。**保持しない・毎回導出**。

```text
has_note_in_after[i] = OR{ device[j].N_i : j > i }     // 後方1パスで前計算
signal = MIDI
for i in 0..n:
    d = device[i]
    if signal == AUDIO:
        role[i] = AudioEffect if d.A_o else Inactive    // audio 区間
        continue
    // MIDI 区間
    can_instrument = d.N_i && d.A_o
    pass_midi      = d.N_o && (has_note_in_after[i] || !can_instrument)
    if pass_midi:
        role[i] = Generator        // MIDI を出す。signal は MIDI のまま
    elif can_instrument:
        role[i] = Instrument       // audio を出す。signal = AUDIO に遷移
    else:
        role[i] = Inactive         // 音源前の audio FX 等（入力が来ない）
```

検証（ユーザーのケース）:
- `[Scaler(N_i,N_o,A_o)]` → 下流に note 消費者なし → pass_midi=F → **Instrument**（鳴る）
- `[Scaler, AnalogLab(N_i,A_o)]` → Scaler の下流に N_i あり → pass_midi=T → **Generator**、
  AnalogLab は **Instrument**（Scaler が AnalogLab を駆動。Scaler 自身の audio は不使用）
- AnalogLab を削除 → `[Scaler]` → 再び **Instrument**（**無音バグが構造的に解消**）
- `[Reverb(A_o), AnalogLab]` → Reverb は音源前で Inactive、AnalogLab が Instrument
  （実 DAW と同じ＝音源前の audio FX は無効）
- 並び替えは**全許可**。`[AnalogLab, Scaler]` にしても crash せず役割が再導出される
  （AnalogLab=音源、Scaler=audio 区間で audio FX か Inactive）。
  → FIXME #32 の「Scaler↔AnalogLab を入れ替えられない」も**棄却ロジックごと消えて解決**。

ロール = {Generator, Instrument, AudioEffect, Inactive}。`Inactive` は process skip。

## 5. 各プロセスの変更（migration surface）

### common/model + protocol
- `Track.devices: Vec<PluginInstance>`、`PluginInstance.ports: PortConfig`。
- `AutomationTarget::PluginParam { device_index }`。
- `PluginSlot` 撤廃。protocol の slot 担持メッセージを index 化:
  `SetSlotPlugin→SetDevice{track,index,..}`, `RemoveSlotPlugin→RemoveDevice{track,index}`,
  `OpenSlotGuiEmbedded/CloseSlotGui/RequestSlotState→ index`,
  `SlotPluginLoaded→ index`。`MoveSlot`/`DemoteInstrumentToGenerator` は**廃止**
  （reorder＝Vec 入れ替えに吸収）。`ReorderChain { moves: Vec<(u32,u32)> }`（old→new index）。

### daw_gui (app.rs)
- caches を `(track, u32)` 化: `loaded_slots`, `open_plugin_guis`, `plugin_params`。
- `inspector_chain()`: flat な device 列を返し、**セクション見出し（生成器/音源/FX）は
  役割導出で動的に付与**（§4 の role を表示用に流用）。
- `reorder_inspector_chain()`: **棄却なしの純 index 並べ替え**。任意 order を受け、
  `moves` を作って `ReorderChain` を両 child へ。能力チェック撤廃。
- `select_plugin_from_db()`: 挿入 index を決めて `SetDevice`。降格/昇格ロジック撤廃
  （位置で役割が決まるので不要）。`DemoteInstrumentToGenerator`/should_demote 撤廃。
- `reconcile_*`: `(index, &PluginInstance)` で diff。

### daw_audio (engine.rs, automation.rs, graph/compile.rs, schedule.rs)
- `slot_to_plugin_id: HashMap<(u32,u32),u32>`（track,index→pid）。ReorderChain で再キー。
- `process_track_owned`: 3段（midi_fx/instrument/fx）を**単一 device ループ＋signal_type
  状態機械**へ。各 device の `ports`（song 由来）で §4 を実行し、MIDI/audio をルーティング。
- `fill_pd_param_events`: slot→device_index。
- sidechain tap / PDC compile / group fx: セクション別 walk を device walk へ統一。
  ReorderChain でも PDC 再 compile をトリガ（src track 並べ替えで latency table を再構築）。

### daw_plugin_host (main.rs)
- `Chain { midi_fx_chain, instrument, fx_chain } → Vec<Box<dyn LoadedPlugin>>`。
- `plugin_lookup`/`editor_windows`/`loaded_*_for_slot` を `(track,u32)` 化。
- `SetDevice` は index 挿入（既存 shift）。`ReorderChain` は Vec permute＋全 map 再キー
  （FIXME #32 で実装した live-move ロジックを index ベースに一般化、Box の heap 維持）。
- `move_plugin`/`DemoteInstrumentToGenerator` 撤廃。`collect_all_states` は flat 走査。

## 6. automation / sidechain / save migration

- **automation**: lane の `PluginParam { slot } → { device_index }`。
  reorder 時は ReorderChain と同じ `moves` で再キー（FIXME #32 の `remap_lane_slots` を
  index 版に）。
- **sidechain**: `sidechain_sources` は track id 参照（不変）。tap は device index で再キー。
- **save migration**: `CURRENT_VERSION 22→23`。保存形式は **serde-JSON**
  （`common/src/project.rs:31` `serde_json::to_string_pretty`）＝field 名ベースで
  additive 移行が容易。
  - 旧 3 fields（`midi_fx_chain`/`instrument`/`fx_chain`）を `#[serde(default)]` の
    deserialize-only に残し、load 時（ensure_ids）に `midi_fx_chain ++ instrument? ++
    fx_chain` の順で `devices` へ平坦化。新規 save は `devices` のみ（旧 fields は
    `skip_serializing` で書かない）。
  - automation lane の旧 `slot` → 新 `device_index`: 平坦化と同じ順序写像で解決。
    解決不能（plugin 削除済み）は warn して lane を残す/落とす（要決定）。

## 7. 段階（壊れた中間状態を避ける）

大きいが、`PluginSlot` が型として全層に出るので「型を変えてコンパイルを通す」流れが
自然なドライバになる。推奨順:

1. **model + PortConfig**: `Track.devices` 化、`PluginInstance.ports`、migration（load 平坦化）。
   既存テスト（group_track_lifecycle 等）を新モデルへ。
2. **役割導出関数**（common か daw_audio に純粋関数 + 単体テスト、§4 の全ケース）。
3. **protocol**: slot→index、不要コマンド削除。
4. **plugin_host**: Chain→Vec、map 再キー、SetDevice/ReorderChain/状態収集。
5. **daw_audio**: process_track_owned 単一ループ化、slot_to_plugin_id index 化、automation/
   sidechain/PDC。
6. **daw_gui**: caches index 化、inspector 動的見出し、reorder 棄却撤廃、picker 挿入 index 化。
7. 実機検証（Scaler 単体→AnalogLab 追加→AnalogLab 削除→Scaler 再発音、任意並び替え）。

各段でビルド green を維持できないなら、`devices` を旧 3 fields の**導出 view** として
一時的に両持ちし、最後に旧 fields を撤去する手も可（migration を安全にするなら推奨）。

## 8. 決定事項 / ユーザー確認

- **role 導出の見える挙動**（§4 の検証ケース）でよいか — ★最優先で確認。
- inspector はセクション見出し（生成器/音源/FX）を**位置から自動表示**で良いか
  （フラットなだけより既存 UX 維持）。デフォルト: 自動表示。
- 並び替えは**全許可・棄却なし**（位置で役割再導出）で良いか。デフォルト: 全許可。
- 保存形式（serde/bincode）の確認 → migration 方式確定。
