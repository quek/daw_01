# パラアウト (マルチ出力プラグインの個別出力ルーティング) 実装計画

`docs/plan_routing_graph.md` PR4 の最後の未実装ピース。サイドチェイン (aux **入力**)
の対称機能として aux **出力** を実装する。本ファイルがこの機能の SSoT。

## 確定仕様 (2026-06-28 ユーザー確認)

| # | 問い | 決定 |
|---|---|---|
| 1 | 「パラアウト」の意味 | **マルチ出力プラグインの個別出力を別トラックへ振り分ける** (ステム書き出しではない) |
| 2 | 未振分けの aux 出力 | **無音** (業界標準。Ableton/Reaper/Logic/Cubase と同じ。サイドチェインの「未接続 aux 入力 = 無音」と対称) |
| 3 | 分け先トラックの出力 | **元の楽器トラックへ戻してまとめる** (楽器トラック = ドラムバス。Reaper フォルダ流) |
| 4 | 操作方法 | **ワンクリックで自動展開** (出力ポート数だけ子トラックを自動作成・グループ化・結線。後から個別ドロップダウンで再調整も可) |

### トポロジ (確定)

```
▼ Drums (楽器トラック A: サンプラー、メイン出力=キック)   ← 子を合算し、自分の FX/フェーダーでまとめて Master へ
   ├ Snare  (子 B: サンプラー aux 出力1 を受ける、個別EQ) → 親 A へ
   └ HiHat  (子 C: サンプラー aux 出力2 を受ける、個別EQ) → 親 A へ
→ Master
```

A は「**グループ兼楽器トラック**」: 自分の楽器音 (キック) を鳴らしつつ、子 (スネア/ハイハット) を
合算し、自分の FX チェーン (バスコンプ等) とフェーダーで処理して Master へ送る。

### 重要な帰結 (NOTE)

- **メイン出力 (キック) は A の FX を通るが、個別 EQ は持てない**。A のサンプラーより後ろの device は
  すべて「合算後のキット全体」を処理する (= バス FX) ため。キックにも個別 EQ をかけたい場合は、
  プラグイン内でキックも aux 出力に割り当てれば、キックも子トラック化されて個別処理できる
  (その場合 A のメイン出力は無音になり、A は純粋なバスになる)。これは制限ではなく自然な帰結で、
  ユーザーがプラグイン内のパッド割り当てで制御できる。

## アーキテクチャ

### 既存エンジンの2パス実行モデル (実コードで確認済み)

`engine.rs::process_buffer`:
- **パス1** (`dispatch_and_wait`): 全トラックを `process_track_owned` で並列処理 (clips + device chain + strip)。
  index 順・並列。
- **パス2** (`execute_schedule_post_dispatch`): スケジュール nodes をトポロジカル順に歩く
  (Mix / ProcessGroupFx / SidechainTap / MixSend)。

スケジュールは `compile.rs::compile_schedule` が依存 post-order でコンパイル
(`dep_edges_for` がエッジを張り、Kahn 風 DFS で順序付け + cycle 検出)。

**パラアウトの遅延ゼロが成立する理由**: ソースプラグイン (サンプラー) の `process()` はパス1で走り、
`buffer_aux_out` (共有メモリ) を埋める。`dispatch_and_wait` は全完了を wait するので、パス2 開始時に
`buffer_aux_out` は確定済み。パス2 で読んで dest へ mix → **同ブロック・サンプル精度**。
(サイドチェインは逆に `SidechainTap` がパス2で次ブロック用に staging するため1ブロック遅延。)

### トラックの3つのパス1モード (新規)

`process_track_owned` の冒頭 `has_children` 早期リターンを、precompute した per-track モードに置換:

1. **Leaf** (子なし・他からの流入なし): フルチェーン + strip (既存挙動)。
2. **SkipBus** (純グループ / リターン / paraout-dest-bus): scratch をクリアして return。
   device はパス2 (`run_group_fx_chain`) でのみ走る。
   - **既存バグ修正**: 現状リターン (incoming sends, 子なし) は SkipBus にならずパス1で device を
     空入力に走らせ、パス2でも走らせる二重処理 → 時間ベース FX が2倍速。SkipBus に含めて class
     ごと修正 ([[feedback_sibling_occurrence_check]])。pass2 (`run_group_fx_chain`) が strip /
     effective_mute / peak / snapshot を完結して設定するので、パス1スキップは安全。
3. **InstrumentPrefix { split_idx }** (グループ兼楽器トラック): device `[0..split_idx]` (=楽器 prefix、
   aux 出力を出す device まで) を走らせ、メイン出力を scratch に、aux 出力を `buffer_aux_out` に残す。
   **strip は適用しない / scratch をクリアしない**。残り `[split_idx..]` (=バス FX suffix) はパス2。
   - `split_idx = 1 + (routed な aux_outputs を持つ最後の device の index)`。明示ルーティングデータ
     由来で、役割ヒューリスティック不使用 ([[feedback_no_role_classification]] を尊重)。

モードは `compile_schedule` が per-track 配列 (`Vec<Pass1Mode>`) として算出し `Schedule` に格納、
`dispatch_and_wait` 経由で `process_track_owned` に渡す (RT セーフ、毎 buffer 再計算しない)。

### パス2のスケジュール (ドラムバス例)

tracks: 0=A(Drums, 子B/C, sampler.aux_outputs[0]→B [1]→C), 1=B(Snare,親A), 2=C(HiHat,親A)

依存エッジ: A は子を合算するので **A→B, A→C** のみ。B/C は A の aux をパス1で得る (パス2依存なし)
ので **B→A エッジは張らない** → cycle なし。post-order: B, C, A。

emit:
```
# B (paraout-dest-bus, child of A)
Mix { srcs:[], dst:TrackScratch(B) }                       # B をクリア
ParallelOutTap { src_track:A, src_device:k, port:0, dst:B } # A.aux_out[0] を B に加算
ProcessGroupFx { track_idx:B, start_device:0 }             # B の EQ + strip
# C (同様)
Mix { srcs:[], dst:TrackScratch(C) }
ParallelOutTap { src_track:A, src_device:k, port:1, dst:C }
ProcessGroupFx { track_idx:C, start_device:0 }
# A (group-with-instrument): prefix はパス1済 (scratch=キック)
Mix { srcs:[(B,1.0),(C,1.0)], dst:TrackScratch(A), additive:true }  # キックを保持して子を加算
ProcessGroupFx { track_idx:A, start_device:split_idx }     # A の suffix FX (バスコンプ) + strip
# master
Mix { srcs:[(A,1.0)], dst:Master }
```

`Mix` に `additive: bool` を追加 (true=クリアしない)。`ProcessGroupFx` に `start_device: u32` を追加
(suffix のみ走らせる。純グループ/リターンは 0)。

### PDC / cycle / solo

- `compute_path_latency`: paraout は「dest の入力 latency に source の path latency を取り込む」
  形で fan-in。ただし A↔子 は group fan-in で既に処理されるため、paraout 固有の追加は
  「子 B/C の input latency に A の prefix latency を含める」。実装は dep に従い自然に処理。
- cycle: 既存 3-color DFS。paraout で B→A エッジを張らないので、グループ兼楽器でも cycle にならない。
  ただし「aux 出力を子でない任意トラックへ」手動ルーティングした場合 (independent topology) は
  dest→source のパス2依存が要るので、その場合のみ dep エッジを張る (下記)。
- solo/mute: 子 B/C は A の子なので既存 folder-solo / `has_soloed_contributor` がそのまま効く。

### 手動再ルーティング (auto 展開後の調整)

aux 出力の行き先は `PluginInstance.aux_outputs[port]` (Option<dest_track_id>) が SSoT。
- dest が source の**子**: グループ合算経路 (上記)。パス2依存は group の A→child のみ。
- dest が source の**子でない**独立トラック: dest は paraout-dest-bus になり、`dest→source` の
  パス2依存エッジを張る (source prefix がパス1で済むので実質パス1依存だが、念のため emit 順序を
  保証)。dest は SkipBus、`ParallelOutTap` で source.aux を受け、ProcessGroupFx で自分の FX → 自分の
  親 (or Master) へ。

## データモデル (`common/src/`)

### `process_data.rs`
```rust
pub const MAX_AUX_OUT: usize = 4;   // plan_routing_graph.md 決定 (ドラムサンプラー 4 stereo)。後で拡張可
// ProcessData に追加 (buffer_aux_in と対称):
pub buffer_aux_out: [[[f32; MAX_FRAMES]; MAX_CHANNELS]; MAX_AUX_OUT],
pub aux_out_active: [u8; MAX_AUX_OUT],  // 1 = plugin がこの aux 出力ポートを declare (host がバッファ供給)
// padding 調整。empty() 更新。コスト: 4*2*1024*4 = 32KB/plugin (MAX_FRAMES=1024、妥当)。
```

### `model.rs`
```rust
// PluginInstance に追加 (aux_inputs と対称):
pub aux_outputs: Vec<Option<AuxOutputRoute>>,   // aux 出力ポート index → 行き先
pub struct AuxOutputRoute { pub dest_track: u32 }
// bincode/serde derive、#[serde(default)] で旧ファイル forward-migrate (空)、
// ensure_ids で dest_track remap、with_ports 等コンストラクタ更新。
```

### IPC (`protocol.rs`)
- 構造変更 (ルート追加/auto展開) は `LoadSong` 再コンパイルで反映 (サイドチェインと同じ model-embedded)。
- **aux 出力ポート情報の報告**: GUI が auto展開・ドロップダウン表示に「このプラグインの aux 出力ポート数 +
  名前」を知る必要がある。plugin host が plugin load 時に CLAP audio_ports を走査して報告する既存の
  PortConfig 報告経路を拡張 (aux 出力ポートの count + names を運ぶ)。GUI は `PluginInstance` か
  サイドの map に保持。

## Plugin Host (`daw_plugin_host/src/clap_plugin.rs`)

- `query_aux_output_channels()` を `query_aux_input_channels()` の対称で追加
  (`count_fn(plugin, false)` で出力ポート走査、`is_main=false` を aux に分類、`MAX_AUX_OUT` cap)。
- process(): 出力側 `clap_audio_buffer` 配列を `[main_out, aux_out_0, ..]` で構築 (事前確保
  `aux_output_buffers` / `aux_output_ptrs`)。plugin process 後、各 aux 出力ポートを
  `ProcessData.buffer_aux_out[port]` にコピー、declare 済ポートに `aux_out_active=1`。
- port layout 変化通知 (`clap_plugin_audio_ports.changed`/rescan) は将来対応 (plan_routing_graph.md
  リスク#7)。初期は load 時クエリ。

## Audio Engine (`daw_audio/src/`)

### `graph/schedule.rs`
- `BufRef::PluginAuxOut` を slot-keyed `{ track: u32, device_index: u32, port: u8 }` に再定義
  (現状 `{plugin_id}` は compile 時に解決不能)。
- `NodeOp::ParallelOutTap { src_track, src_device, port, dst_track }` を `SidechainTap` の対称で追加。
- `NodeOp::Mix` に `additive: bool`、`NodeOp::ProcessGroupFx` に `start_device: u32` を追加。
- `Schedule` に `pass1_modes: Vec<Pass1Mode>` 追加 (enum Leaf/SkipBus/InstrumentPrefix{split_idx})。

### `graph/compile.rs`
- `incoming_paraout: HashMap<dest_id, Vec<(src_idx, device_idx, port)>>` を `incoming_sends` 同型で構築。
- bus 述語に「incoming_paraout を持つ」を追加 (paraout-dest-bus)。
- group-with-instrument 検出: 子を持ち、かつ自分の device に routed aux_outputs がある → InstrumentPrefix。
- emit: paraout-dest-bus の Mix(クリア) 後に `ParallelOutTap` を emit。group-with-instrument の子 Mix は
  `additive:true`、`ProcessGroupFx{start_device:split_idx}`。
- dep_edges: 独立 dest のみ paraout エッジ (子 dest は group エッジで足りる)。
- `compute_path_latency` に paraout fan-in。
- `pass1_modes` を算出して Schedule に格納。

### `engine.rs`
- `process_track_owned`: `has_children` 早期リターンを `pass1_mode` 分岐に置換
  (Leaf=フルチェーン / SkipBus=クリア+return / InstrumentPrefix=prefix のみ・strip なし)。
  `dispatch_and_wait` と serial fallback に `pass1_modes` を配線。
- `execute_schedule_post_dispatch`: `NodeOp::ParallelOutTap` ハンドラを `SidechainTap` の対称で追加
  (`slot_to_plugin_id` で src plugin 解決 → `buffer_aux_out[port]` を dst scratch.track_l/r に**加算**)。
  `Mix` ハンドラに additive 対応 (`mix_into_track_scratch` に flag)。`ProcessGroupFx` に start_device 配線
  (`run_group_fx_chain` のループ開始 index)。`PluginAuxOut` の no-op Mix arm を整理。
- export.rs (`reduce_master` 系オフライン経路) も同じ schedule を使うか確認・整合。

## GUI (`daw_gui/src/`)

- **auto展開**: 楽器プラグインのインスペクタ行に「パラアウト展開」ボタン。押下で AppEvent::ExplodeParallelOut
  { track_id, device_index }。handler が: aux 出力ポート数分の子トラックを作成 (parent_group_id=楽器トラック、
  名前は port name or "<track> Out N")、`aux_outputs[port]=Some(child)` を設定、LoadSong 再送。undo 対応。
- **手動ドロップダウン**: aux 出力ポートごとに「Out[port]: [None / TrackName]」picker
  (`SidechainEntry`/`sidechain_source_choices` の対称、`track_picker` idiom 流用、
  [[feedback_reuse_inspector_idiom]])。`ParallelOutputEntry` 型 + `parallel_output_entries()`/`choices()`。
- UX 配置: サイドチェイン section の近傍に「Parallel Out」section。インスペクタの 2 分割
  (param viewport + chain band) を崩さない ([[plan_routing_graph.md]] / インスペクタ配置の罠)。

## テスト

- `compile.rs`: ① group-with-instrument の emit 順序 (子 ProcessGroupFx < 親 Mix(additive) <
  親 ProcessGroupFx{start_device}) ② ParallelOutTap が src/port/dst 正しく emit ③ cycle 不発
  ④ pass1_modes 算出 (Leaf/SkipBus/InstrumentPrefix{split_idx}) ⑤ paraout × PDC fan-in。
- `model.rs`: aux_outputs の bincode round-trip + forward-migrate (旧ファイルは空) + ensure_ids remap。
- `process_data.rs`: empty() で aux_out ゼロ。
- engine 数値テスト (compile.rs の PDC impulse テスト idiom): A が aux にインパルス → 子 B 経由で
  A に戻り、master で正しい sample 位置に合算 (遅延ゼロ検証)。
- **リターン二重処理修正の回帰**: 既存 send/return テストが green のまま (SkipBus 化で挙動不変または改善)。

## 検証 (実機)

1. マルチ出力 CLAP (is_main=true 1 stereo + is_main=false 複数 stereo) を A にロード。
2. 「パラアウト展開」→ 子トラックが自動作成・グループ化される。
3. 再生で各子に分離した音、A でキット全体が合算され A の FX/フェーダー → Master。
4. 子で個別 EQ、A でバスコンプが効く。A のフェーダーでキット一括変動。
5. ルート解除 → main のみ。未振分け aux は無音。
6. 子の FX を時間ベース (ディレイ) にして二倍速バグが無いこと (リターンでも同様に確認)。

## ファイル変更一覧

| 層 | ファイル |
|---|---|
| Schema | `common/src/process_data.rs` (MAX_AUX_OUT, buffer_aux_out, aux_out_active) |
| Model | `common/src/model.rs` (aux_outputs, AuxOutputRoute, ensure_ids, migrate) |
| IPC | `common/src/protocol.rs` (aux 出力ポート報告の拡張) |
| Plugin Host | `daw_plugin_host/src/clap_plugin.rs` (query_aux_output_channels, process aux out + readback) |
| Audio | `daw_audio/src/graph/schedule.rs` (BufRef/ParallelOutTap/Mix.additive/ProcessGroupFx.start_device/Pass1Mode) |
| Audio | `daw_audio/src/graph/compile.rs` (incoming_paraout, emit, dep, PDC, pass1_modes) |
| Audio | `daw_audio/src/engine.rs` (ParallelOutTap handler, pass1_mode 分岐, additive mix, start_device) |
| GUI | `daw_gui/src/app.rs` (ExplodeParallelOut, ParallelOutputEntry, choices) |
| GUI | `daw_gui/src/view/track_inspector.rs` (Parallel Out section + 展開ボタン) |
| Plugin Host | `daw_plugin_host/src/vst3_plugin.rs` (extra output bus を aux output 公開) |

## 設計更新 (2026-06-28 実機フィードバック)

当初は「グループ兼楽器トラック」(main = 親トラックの音 + 子) を主モデルとしたが、
MDrummer 等「全パーツが個別出力」のマルチアウトドラムでは全出力を個別子トラックに
分けるのが自然。実機検証でユーザーが「Out 1 (main) も子トラックに」と要望。これを
受けて設計を以下に更新:

- **main 出力を「パラアウトポート 0」として扱う** (port 1.. = aux/extra bus)。
  プラグインの全出力を均等にポート化し、`aux_outputs[0]` で main の行き先を制御:
  - `aux_outputs[0] = Some(子)`: **全部子**。親はメイン無音の通常グループ
    (clearing `Mix` で全子合算)。engine は pass1 で親 scratch から main をクリア。
  - `aux_outputs[0] = None`: **楽器兼バス** (旧主モデル)。main を親に残し子を加算
    (`MixAdditive`、後方互換)。
  - 判定は `Track::paraout_main_to_child()` (= `aux_outputs[0].is_some()`)。役割
    ヒューリスティック不使用、明示ルーティングデータのみ。
- **展開 (explode) は全ポート (main 含む) を子トラック化** = 全部子。MDrummer の
  16 出力 (main + 15 part) → 16 子トラック。

### VST3 マルチアウト対応
VST3 backend は既に extra output bus をプラグインから受け取っていた (捨てていた)。
これを `aux_output_buffer`/`aux_output_port_count` で公開 (port 0 = main bus
`output_buffers`, port 1.. = `extra_output_buffers`)。`MAX_AUX_OUT` を 4→16 に拡大
(MDrummer = main + 15 stereo part bus、128 KB/plugin)。CLAP も同じ port 0 = main
セマンティクスに統一。

### PDC
- 全部子: 全子 (main 含む) を clearing `Mix` の src 整合 (既存 group PDC) で揃える。
- 楽器兼バス: `MixAdditive` の dst (= 親自身の main、相対 latency 0) も子の最大 path
  latency に揃える (`emit_mix_src_alignment` + dst ApplyDelay)。
- 独立 dest (子でない track へ): `compute_path_latency` 後の bounded fixpoint で
  source の path latency を fan-in (相互 paraout の発散を n 回で打ち切り)。
