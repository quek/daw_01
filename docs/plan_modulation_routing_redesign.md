# plan_modulation_routing_redesign.md — lane 非依存・統一モジュレーション（Bitwig 流 + video 拡張）

> [plan_modulation.md](plan_modulation.md) の **routing モデル**を作り直す差分プラン。
> follower エンジン / `mod_scalars` 配送 / tap point / export sidecar（commit `e132bab`〜`9bd44fe`）は
> 土台として活かす。変えるのは「変調 edge をどこに持つか」と「プラグインへどう送るか」。
>
> **フェーズ分けはしない。この文書の全項目を一気に最終形まで実装して完走する**
> （CLAUDE.md 冒頭「理想とベストプラクティスを追求／最終形を一気に完成させる」）。

## 0. なぜ作り直すか

現状（commit `d881b73`/`e132bab`/`7ab1a4c`）は `ModRouting` を `AutomationLane.mod_routings`
に同居させ、変調値を `effective_value` で **1 つの絶対値に畳んで** CLAP/VST3 とも
`PARAM_VALUE`（automation と同じ絶対値イベント）で送っていた。

問題（ユーザー指摘 2026-06-13）:
1. **モジュレーションがオートメーションに従属** — レーンが無いと変調できない。
2. プラグインへ「base⊕mod を畳んだ絶対値」を automation と同じ経路で送る = **破壊的**
   （ノブが動く）。Bitwig / CLAP の「非破壊モジュレーション」になっていない。

## 1. 一次情報調査（2026-06-13、workflow `wxoqmjc29`）

| 規格 | モジュレーション機構 | host は base を知る必要があるか | 破壊的か |
|---|---|---|---|
| **CLAP** | `CLAP_EVENT_PARAM_MOD`(type 6)。`amount` = plain 値デルタ。spec: 「value heard = **param_value + param_mod**」。`CLAP_PARAM_IS_MODULATABLE`(1<<10) で対応表明。 | **不要**（プラグインが base を保持し合成） | **非破壊**（ノブは base のまま） |
| **VST3** | 専用機構**なし**。`IParameterChanges`/`IParamValueQueue` の絶対正規化値 [0,1] のみ。 | **必要**（host が base+mod を合成して最終値を送る） | 破壊的 |

- CLAP の `clap_event_param_mod` は `clap_event_param_value` と同レイアウトで末尾が
  `value`→`amount`。targeting tuple (port,channel,key,note_id) は global なら全 -1。
  clap-sys 0.5.0 に既存（依存 bump 不要）。
- daw_01 は `daw_plugin_host/src/clap_plugin.rs:729-744` で `PARAM_VALUE` のみ構築。
  `CLAP_PARAM_IS_MODULATABLE` は `clap_plugin.rs:405` で既に読んでいる。
- 非プラグイン target の base は**モデル内**: `Track.volume/pan`、`ImageEvent.<field>`、
  `TextEvent.<field>`、`Track.group_transform.<field>`、`Song.bpm`。→ host が base を所有。
- 出典: clap `events.h`/`ext/params.h`、VST3 `IParamValueQueue`、Bitwig CLAP 記事、
  nakst CLAP tutorial（plugin 側 `clamp01(base + offset)`）、daw_01 現コード。

## 2. 正しいモデル — lane 非依存 routing

```rust
// ModRouting に target を持たせ lane から独立。
#[derive(Clone, Copy, PartialEq, /* serde + bincode */)]
pub struct ModRouting {
    pub target: AutomationTarget,  // ★ 変調先 param を直接指す
    pub source_id: u32,            // → Song.mod_sources[*].id
    pub depth: f32,                // 正規化領域の量 (-1..=1)
    pub polarity: Polarity,        // Unipolar | Bipolar
}
```

- **`AutomationLane.mod_routings` を廃止**。
- 保管は automation と同流儀: `Track.mod_routings: Vec<ModRouting>`（target は track 内 param）
  + `Song.song_mod_routings: Vec<ModRouting>`（`SongTempo` 等）。
- `target: AutomationTarget` は既存 enum（TrackBuiltin / PluginParam / ImageBuiltin /
  TextBuiltin / GroupTransform / SongTempo …）。**lane が無くても target を直接指せる**。
- 機能未リリース → 旧 test project の `AutomationLane.mod_routings` は load 時に無視（migration 不要）。

## 3. 実効値の出し方（target 種別で分岐）

**base の出どころ**:
- automation lane が当該 target にあれば `lane_value_at`（base + curve）。
- 無ければ **モデルの現在値**（ノブ）: track.volume/pan、ImageEvent/TextEvent field、
  group_transform field、Song.bpm。
- **plugin param で lane 無し** → CLAP は base 不要（§3.2）、VST3 は **plugin param 値キャッシュ**から base（§3.3）。

**変調量（正規化領域、source 種別共通）**:
```
mod_norm = Σ over target 一致の ModRouting:
             s = clamp(scalar(source_id), 0, 1)
             Unipolar => depth*s,  Bipolar => depth*(2s-1)
```

### 3.1 非プラグイン target（track builtin / image / text / group / song）

```
eff = norm_to_plain(target, clamp(plain_to_norm(target, base) + mod_norm, 0, 1))
```
compose / `fill_track_param_ramps` が base（lane or モデル値）に乗せる。**lane 不要**。

### 3.2 plugin param — seam は **plugin_host**（metadata を全部持つ側）

調査で確定したアーキテクチャ:
- **daw_audio は format 非依存**。plugin param の変調を **正規化オフセット 1 個**
  `offset_norm = Σ depth·polarity·scalar`（depth が既に正規化領域なので min/max 不要）に畳んで
  `push_param_mod(time, param_id, offset_norm)` で送る。**lane が無くても** Track.mod_routings に
  plugin param target があれば emit（= lane-free）。lane があれば従来 PARAM_VALUE(base) も従来通り emit。
- **plugin_host が per-format に変換**（`PluginParamInfo` の min/max/modulatable を所有）:
  - **CLAP modulatable**: `clap_event_param_mod{ amount = offset_norm·(max−min), PCKN 全 -1 }`。
    base 不要・**非破壊**。automation の PARAM_VALUE と**プラグイン内で加算合成**（Bitwig と同じ二層）。
  - **VST3 / 非 modulatable CLAP**: host が `final = base ⊕ offset` を合成して送る（VST3 は mod 機構なし）。
    base = この block で PARAM_VALUE が来ていればそれ、無ければ **plugin_host が保持する last-set 値**
    （§4-B）→ VST3 でも lane 無しで変調可能。
- plugin_host は load 時に `PluginParamInfo`（min/max/modulatable）を**キャッシュ**し、audio-thread の
  `process()` から参照（enumerate は main-thread-only なので process 内で呼ばない）。

## 4. 値キャッシュ / 正規化（調査で判明した現状）

- **B. plugin param 値キャッシュ**:
  - daw_gui は既に `plugin_param_values: HashMap<(track,device,param), f64>`（plain、
    `PluginParamValueChangedFromChild` で更新）を持つ。UI/録音用 base はこれを使う（新設不要）。
  - VST3 lane-free 変調の base は **plugin_host 側に last-set 値の小さなキャッシュ**を新設して使う
    （plugin が持つ現在値 = host が最後に送った値）。
- **A. PluginParam 正規化（critique #5）は変調エンジンの前提ではない**:
  - 変調オフセットは `offset_norm = depth·…`（depth が正規化領域）で、plugin param に `plain_to_norm`
    を**通さない**ので、identity placeholder のままでも変調は正しい。
  - identity placeholder が誤るのは **UI 表示**（inspector knob / arrangement の automation curve /
    widget の depth→plain 換算）。これは daw_gui が `plugin_params` cache の min/max で局所的に正す
    別件（必要なら polish として実施、変調をブロックしない）。
- **値ドメインの既存事情（変調と別件・今回触らない）**: daw_01 は同じ `ev.value` を CLAP には plain、
  VST3 には normalized として送っている（param range [0,1] のときだけ整合）。VST3 の
  `enumerate_params` は `min=0,max=1` 固定。変調の `amount = offset_norm·(max−min)` は
  plugin_host が持つ `PluginParamInfo` の min/max を使うので、この既存事情とは独立に正しい。

## 5. IPC / shmem 改修

変調は `ProcessData::param_mods`（`events_in` とは別枠の専用配列）で運ぶ。器と溢れ規約の
SSoT は `common/src/process_data.rs`（[plan_rmd_88_89_cross_modulation.md](plan_rmd_88_89_cross_modulation.md) §2.2）。

> 旧: `EventKind::ParamMod = 4` を `events_in` に相乗りさせていた。制御グリッド化で
> ノート枠を押し出す事故が起きるため r.md #89 で撤去済み。

- 音声側（**format 非依存**）:
  - 非プラグイン → §3.1（engine 内で完結、IPC 不要）。
  - plugin param に変調があれば `push_param_mod(time, param_id, offset_norm)`（**lane 有無問わず** =
    Track.mod_routings を走査）。lane があれば従来 `push_param(time, param_id, base)` も従来通り emit。
- plugin_host: `TimedParamEvent` に種別（value/mod）を持たせ、`ProcessData::param_mods_iter()` を
  per-format 変換（§3.2）。CLAP modulatable は `clap_event_param_mod`、それ以外は base⊕offset を
  合成して既存 value 経路へ。
- protocol/shmem 型変更 → `cargo build --workspace`（`feedback_workspace_build_for_protocol_changes`）。

## 6. UI — Bitwig 風 widget レベル modulation（最終形）

理想 = Bitwig と同じく **modulation は個々のパラメータコントロール widget の性質**。daw_01 で
per-param に depth 入力を継ぎ足すのは SSoT 分裂 + interim（`feedback_gui_01_request_before_interim`）。
→ **gui_01 の `scrubable_number_at` 自体に modulation 対応を組み込んでもらう**（要望提出済 2026-06-13、
`docs/gui_01_conversation.md`）。

- **Sources rack**（実装済み・据え置き）: source track dropdown + meter + tap(PoF/PrF) + Attack/Release。
  + 各 source に**色**を割当（`Song.mod_sources[*].color`、Bitwig 流）。arm（割当モード）状態を daw が保持。
- **per-param modulation**（lane でなく target param 単位、widget が表示・編集）:
  - 各パラメータ表示の `scrubable_number_at`（inspector vol/pan・画像/テキスト field・plugin param・BPM …）へ、
    当該 target の routing から `entries`（色 + depth）/ `live_value`（mod_scalars から算出）/ arm 中なら
    `mod_edit`（source 色 + 現 depth + on_mod_change）を渡す。
  - widget が変調レンジを色帯で重畳描画 + ライブインジケータ + arm 時の depth ドラッグ編集を担う。
  - source を arm → 任意パラメータのコントロールをドラッグで depth 設定（Bitwig 操作の再現）。**lane 生成不要**。
- events: `AddModRouting{track_id, target, source_id}` / `RemoveModRouting` / `SetModRoutingDepth`
  / `SetModRoutingPolarity`（旧 lane_id 引数を target に置換）。
- **gui_01 landing 待ちの間も backend（§7 の 1–7）は全て進める**（`feedback_progress_while_waiting_gui01`）。
  widget が landing したら inspector の各 `scrubable_number_at` 呼び出しに modulation 引数を wire（parked）。
  interim な depth 入力は作らない。
- **dropdown はみ出し**（長い track 名 / "(Video)" のクリップ）も同要望の補足で gui_01 に報告済（優先度低）。
  → **2026-08-27 に本番で壊れて回収**。「優先度低」と見積もったのは項目数が数十で収まる前提だったが、
  プラグイン 1 個が 47,137 param を報告する例があり、popup は高さを切り詰めないので画面外の候補は
  原理的にクリックできなかった。**候補一覧そのものを撤去**し、ルート指定を ◉ (arm) 一本に統一した。
  以後この節の「add-route dropdown」は存在しない。設計正本は
  [plan_modulation_arm_only.md](plan_modulation_arm_only.md)（r.md #78）。

## 7. 実装手順（一括・フェーズ分けなし）

順序は依存関係のためであり、途中で止めて確認は挟まない。全部終えてから一度だけ実機検証する。

1. **Model**: `ModRouting.target` 追加（Copy 外す）、`Track.mod_routings`/`Song.song_mod_routings` 追加、
   `AutomationLane.mod_routings` 削除。save/load/undo の対称性を全て更新。回帰テスト更新。
2. **Eval**: `effective_value*` → `apply_modulation*`（target でフィルタした routing で base ⊕ mod）。
   base = lane があれば `lane_value_at`、無ければモデル現在値。
3. **非プラグイン配線**: image/text/group compose + `fill_track_param_ramps` を「モデル base ⊕ mod」へ
   （Track.mod_routings から、**lane 無しでも変調**）。
4. **IPC**: `ProcessData::param_mods` + `push_param_mod` + `TimedParamEvent` に種別（value/mod）。`make build`。
5. **daw_audio**: `fill_pd_param_events` が plugin param の `offset_norm` を `param_mods` へ emit
   （**Track.mod_routings 走査、lane 有無問わず**）。lane があれば base を従来 `push_param`。
6. **plugin_host**: load 時に `PluginParamInfo`(min/max/modulatable) を cache + last-set 値 cache を新設。
   `process()` で param modulation を per-format 変換（CLAP modulatable=`clap_event_param_mod`、それ以外=合成）。
7. **(UI polish 任意)** critique #5: daw_gui の plugin-param `plain_to_norm`/curve 表示を `plugin_params`
   cache の min/max で正す（変調はブロックしないが inspector 表示の正確性向上）。
8. **source 色 + arm 状態**: `Song.mod_sources[*].color` 割当、arm（割当モード）state を daw に追加。
9. **gui_01 要望提出済**（2026-06-13）。widget landing 後、各 `scrubable_number_at` に
   `entries`/`live_value`/`mod_edit` を wire（それまで parked、`feedback_progress_while_waiting_gui01`）。
10. 各段で `cargo build --workspace` / `cargo test --workspace` / `cargo clippy -- -D warnings` /
    release build green を維持。全部揃ったら `/review` → 実機検証（レーン無しで音/画が変調されることを目視・可聴）。

## 8. 据え置き / 非対象

- follower エンジン（`EnvelopeFollow` NodeOp + ring）、`AudioBridge.mod_scalars`、30Hz 配送、
  tap point、export sidecar は実装済みで流用。
- per-voice/polyphonic modulation（CLAP の note 単位 param_mod）は将来。今回は global（PCKN 全 -1）。
