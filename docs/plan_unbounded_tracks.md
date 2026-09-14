# トラック数の上限撤廃 (32 本の固定長を含む「数に比例する固定長の器」の全廃)

## 0. 背景と根本原因

`MAX_TRACKS = 32` で描画が先頭 32 本までしか処理せず、33 本目以降は **無音** (プラグインも呼ばれない)。
GUI 側に上限チェックは無く、ユーザーに知らされないまま消える。実測: 40 本中 1 本だけにクリップを
置いて書き出すと、32 本目 = ピーク 0.600 / 33 本目・40 本目 = 0.000。

**根:** トラック数と、それに比例して増える数 (ランチャー行 / 内蔵メーター / 変調ソース / プラグイン
インスタンス) を **起動時に決めた固定長の器** に入れ、溢れた分を黙って捨てている。

## 1. 全件表

| 器 | 場所 | 溢れたとき | 直し方 (§) |
|---|---|---|---|
| トラック 32 | `engine.rs` / `execute.rs` / `export.rs` / `project_ctl.rs` の `min(MAX_TRACKS)` | 33 本目以降が無音 | §2 |
| 同上 | `project_ctl.rs` stretch engine 配送 | 33 本目以降に届かない | §2.4 |
| 同上 | `project_ctl::preview_track_index` | 鍵盤プレビューが届かない | §2 |
| 同上 | `mix::has_soloed_contributor` の固定長 BFS | solo-safe な return が壊れる | §2.3 |
| 同上 | `RowSourceTable::offsets` (容量 32+2) | 33 本目以降にランチャーが効かない | §2.5 |
| 同上 | `ProjectTelemetry::track_peaks` / `voice_*` (index 位置) | メーター / per-voice カーソルが出ない | §3 |
| ランチャー行 512 | `MAX_LAUNCHER_ROWS` / `launcher::MAX_ROWS` | 513 行目以降でランチャーを持てない / 表示されない | §2.5 / §3 |
| 内蔵 GR メーター 256 | `MAX_NATIVE_METERS` | 257 個目以降の GR が出ない | §2.6 / §3 |
| 変調ソース 64 | `MAX_MOD_SOURCES` (emit / mod_tick / shmem / sidecar) | 65 個目以降の変調が効かない | §2.7 / §3 |
| プラグイン計測 512 | `MAX_PLUGINS` | CPU 表示が出ず、RT worker が毎 buffer 線形再試行 | §4 |
| stretch 配送 ring (32×2) | `STRETCH_POOL_RING_CAP` | 1 publish で埋まり再送待ち | §2.4 |
| heartbeat (debug) | `heartbeat_*` の容量 | debug ビルドで RT 再確保 | §2 |

**対象外** (トラック数に比例しない): タブ `MAX_PROJECTS` / シーン `MAX_SCENES` / device scope
`MAX_DEVICE_SCOPES` / 1 トラック内の同時 stretch `MAX_STRETCH_ENGINES_PER_TRACK` / worker
`MAX_WORKERS` / 1 buffer のイベント数 (`MAX_EVENTS` / `MAX_PARAM_MODS`) / 1 track の表示ボイス数
`MAX_PUBLISHED_VOICES`。

## 2. RT 側の器: song と同じ便で必要量まで伸ばす

原則: **必要量は off-thread で曲から数え、器は off-thread で確保し、song と同じ `RtBundle` で届ける**
(既存の `scratch_growth` と同じ型)。RT は move / swap / 容量内 push だけ。伸ばす方向にだけ動かす。

### 2.1 per-track scratch の成長便は「追加分だけ」

旧: 本数が増えるたびに **全本数ぶん** の `TrackScratch` を新規確保し、既存を swap で移していた。
上限を外すと 200 本の曲でトラックを 1 本足すたびに 200 本ぶん確保し直す。

新: `ScratchGrowth { base, rows: Vec<TrackScratch> }` — `rows` は index `base..` の **追加分だけ** を
持ち、容量は「伸ばした後の総本数」。RT は既存の行を `rows` の末尾へ push (容量内) → `rotate_left(追加数)` で
既存を先頭へ戻す → `self.scratch` と swap。既存と重なる行 (テストの全本数便) は既存の走行状態を残し、
便の新品を捨て側へ回す。押し出した Vec は recycle。

`supersede` で 2 便を畳むとき: 新しい便の容量は `旧便の base + 旧便の追加 + 新便の追加` 以上なので、
旧便の行を新便へ push → rotate で順序を「旧便の追加 → 新便の追加」に揃える (確保なし)。

**便の順序は送り手が守る。** `BundlePublisher::send` は park 中の便が ring に入れなかったら、新しい便を
ring へ入れずに畳み込んで park し直す — 先に入れると、flush の失敗と push の間に RT が drain したとき
新しい便が古い便より先に届き、追加分の並び (`base`) が抜けたまま戻らない。

### 2.2 `TrackScratch` の PDC 用 1 秒 prealloc を廃止

1 本 ~450 KB のうち 384 KB が入力遅延線の 1 秒 prealloc。`Schedule` を載せる便は **遅延が要る
全 track の遅延線** を off-thread で確保して同梱する (`input_delay_replacements`、index = track index)。
RT は容量が足りない行だけ swap。新 schedule の便は自分の遅延線を必ず全部持つので、畳み込みで
古い便の遅延線に頼らない。

### 2.3 solo-safe 判定は compile 時の表

`ChainProgram::solo_contributors` = その track へ **流れ込む track index の推移閉包**
(子 → group、send 元 → return)、`ChainProgram::solo_ancestors` = 祖先 group の track index (folder solo)。
RT は表の track の `solo` を見るだけ (`mix::any_soloed`。Song の走査も固定長配列も無い)。
solo / mute は値のみ更新だが、`parent_group_id` / send 先の変更は topology 変更 (再 compile) なので
表と song は同じ便で整合する。

再 compile の走行状態の移送 (`Schedule::adopt_state_from`、RT) も delay line / follower / program を
前回の一致位置から探す (`find_near`) — 並びは再 compile を跨いでほぼ保たれるので線形。

### 2.4 stretch engine の配送は 1 publish = 1 便

`StretchPoolDelivery { per_track: Vec<(usize, Vec<StretchEngine>)> }`。ring の深さはトラック数に
依存しない小さな定数。RT は「便の最大 track index が scratch に収まる」まで pop を待つ (従来規約)。

### 2.5 ランチャーの器

`RowSourceTable` (`sources` / `offsets`) と `LauncherRuntime::rows` の容量を便で伸ばす
(`LauncherGrowth`)。行数 = トラック数 + 全トラックのレーン数 + master レーン数、行群数 = トラック数 + 2。
書き出しは off-RT なので曲から数えて確保する。

毎 buffer の行の突き合わせを行数の二乗にしない: `LauncherRuntime::rows` を `for_each_launcher_row` と
**同じ並び**に揃え (`sync_rows`、消えた行を落としてから増えた行を差し込む — 器は曲の行数ぶんなので逆順だと
削除と追加が同じ便で届いたとき足した行が入らない)、`resync_cells` / `seed_from_song` / `sync_saved_rows` /
`launch_scene` / `build_table` は先頭から進むカーソルで組にする (`for_each_row` / `take_row`)。

### 2.6 内蔵 GR メーター

`assign_native_meters` の枠を撤廃 (GR を持つ device は全部 publish)。面の容量は §3。

### 2.7 変調ソース

`emit` の `take(MAX_MOD_SOURCES)` / `mod_tick::MAX_SLOTS` を撤廃。`ModTickRunner` の RT 器
(値面・行・深さ・follower 係数・publish 面) は plan と一緒に off-thread で確保する
(`ModTickBuffers::for_plan`、動くフォロワー / 動く深さの列もここで解く)。`FollowerMaps` も plan / schedule と
同じ便で届ける。sidecar の読み込みは定数上限ではなく **ファイル長との整合 (checked 算術)** で壊れた入力を弾く。

per-sample の id → 列は `ModTickPlane` の id 昇順索引 (`col_of`) の二分探索。worker pool 経路は刻み面を
**丸ごと 1 本のポインタ**で渡す (`DispatchShared::mod_plane_ptr`) — id 表と値だけ渡して worker が組み直すと、
索引 / 動く深さ / buffer 先頭のサンプル位置が pool 経路でだけ落ちる。

## 3. GUI への telemetry: 固定ヘッダ + 伸びる面

`ProjectTelemetry` (固定 shmem、`MAX_PROJECTS` 個) からトラック数に比例する配列を全部外し、
**プロジェクトごとの伸びる面 (`TelemetryPlane`)** へ移す。

- 面 = ヘッダ (各容量) + トラック面 / 内蔵 GR 面 / 変調値面 / ランチャー行面。
  **位置ではなく id で引く** (track id / device id / `ModSource::id` / row key。不変条件 1)。
  トラック面・GR 面・変調値面は面ごとの seqlock (id 表と値を組で読む)。ランチャー行は従来の
  「`row_key` を最後に書く」規約。
- 書き手 daw_audio が off-thread (`publish_bundle`) で必要容量を数え、足りなければ
  **2 冪に切り上げた容量で作り直す**。名前 = `{audio shmem id}_plane_{daw_audio pid}_{世代}`
  (世代は daw_audio プロセス内の単調カウンタ。`feedback`: 再利用される id を OS リソース名にしない)。
  死んだ同じ pid のプロセスの面を GUI がまだ開いていると名前が衝突するので、作成に失敗したら世代を
  進めて数回作り直す。それでも作れなかった要求は同じ要求のまま作り直さない (値だけの publish のたびに
  失敗を積まない)。
- 新しい面は `RtBundle` で RT に届き、RT が **その面へ一式 publish し終えた buffer の末尾で**
  `ProjectTelemetry::plane_id` (`pid << 32 | 世代`、`0` = 面なし) を store する (install の瞬間に出すと
  GUI はまだ 0 の面を空の曲として読み、変調値が 0 に跳ねランチャーの行が 1 フレーム消える)。GUI poller は
  `plane_id` が変わったら開き直す (開けなければ次の tick で読み直す。面に依らない tick は止めない)。
  旧面は recycle で off-thread に閉じる — GUI が握っている間はカーネルが保持するので読み手は壊れない。
- claim / release (`reset`) は `plane_id = 0`。

## 4. プラグイン計測: ロード時に枠を割り当てる伸びる面

`MetricsBridge::plugin_metrics` (512 固定、RT worker が CAS claim + 毎 buffer 線形再試行、GUI が
reclaim) を廃止。

- daw_plugin_host の plugin-main が **instance を registry に入れるとき** 空き枠を割り当てる
  (off-RT)。足りなければ 2 冪で作り直し、live な全 entry を新しい面 (同じ枠番号) で publish し直す。
  名前 = `{metrics shmem id}_plugins_{daw_plugin_host pid}_{世代}`、`MetricsBridge::plugin_plane_id`
  で GUI に知らせる。
- `PluginEntry` は枠番号を持ち、面 (`Arc<PluginMetricsPlane>`) は registry snapshot と同じ世代の組で
  worker に届く。worker は store するだけ。面の容量の規則は `plugin_plane_capacity` 1 本。
- unload (`registry_remove` + quiesce 後) で plugin-main が枠を空ける。GUI は読むだけ
  (poller が `token → μs` を読み出して UI へ流す)。

## 5. テスト

- 書き出し: 40 トラック中 33 本目 / 40 本目のクリップが鳴る。33 本目以降の track から return への
  send があり、送り元を solo しても return が鳴る。
- ランチャー: 40 本目のトラックのセルを撃つと鳴る (書き出し経路)。
- 変調: 100 個の変調ソースで 65 個目以降が効く。
- 伸びる面: 作り直した後の新しい面を id で往復できる / 旧面を開いている読み手が壊れない /
  容量超過で作り直しが走る。
- プラグイン計測: 600 インスタンスで全部に枠が割り当たり、unload で空く。
- 成長便: 追加分だけの便と畳み込みで、既存行の走行状態 (入力遅延リング / ノート) を保つ。
- worker pool 経路と直列経路で、動く深さの変調が bit 一致する。
- ランチャー: 器が満杯の曲で行の削除と追加が同じ `Song` で届いても、足した行が同じ buffer で鳴る。
