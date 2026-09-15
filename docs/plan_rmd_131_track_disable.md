# r.md #131 トラック無効化 (トラックヘッダで Q)

索引: [plan_rmd_130_133_index.md](plan_rmd_130_133_index.md) (分担・統合順・共通規則)。
調査: `scratchpad/rep/131-track-disable.md` (コード地図) / `r131.md` (Bitwig / Cubase / Live / Studio One / Logic / Pro Tools / REAPER)。

## 理想

無効化は **Song が持つトラックの状態** (undo 可・保存される・書き出す音が変わるので dirty)。無効なトラックは
「プロジェクトには残っているが実行系からは消えている」— **プラグインはホストからアンロード** (state は Song に保持し、
有効に戻したら復元)、レンダーグラフ・VOICEVOX 合成・映像デコード・素材の常駐から外れ、**CPU / メモリ / GPU を一切使わない**。
書き出しにも出ない。判定は「実効的に有効か」を返す **1 関数**に集約し、音声・映像・ホスト同期・表示が全部そこを読む。

## 確定仕様 (2026-09-15 ユーザー承認)

| # | 論点 | 決定 |
|---|---|---|
| Q1 | Q キーの効く場所と対象 | **アレンジのトラックヘッダ列 (名前や M/S の列) かミキサーのストリップにポインタを乗せて Q** → その **1 本**を無効 / 有効に切替 (S = ソロと同じ「ポインタ直下」規則)。**右クリックメニューにも「無効化 / 有効化」**を足し、こちらは右クリックしたトラックが選択に含まれていれば選択全体。クリップレーン上の Q は今どおり (クリップ等のミュート) |
| Q2 | 見た目と触れる範囲 | ヘッダ・クリップレーン・ミキサーのストリップを**丸ごと暗く沈め、クリップも灰色**。クリップ / ノート編集・フェーダー・改名は**今どおりできる** (効くのは有効に戻してから)。開いていたプラグイン窓は閉じ、インスペクタのチェーンは「無効中」表示で窓を開けない |
| Q3 | グループを無効化 | **子もまとめて無効** (暗くなり CPU も使わない)。子それぞれの状態は別に保持し、グループを有効に戻すと元どおり (個別に無効だった子は無効のまま)。= 実効状態 = 自分 && 祖先グループ全部 |

### main が決めた細部 (ユーザーに報告済み)
- **送り・参照**: 無効トラックからの send / 無効トラック (return) への send は無音。無効トラックをサイドチェイン元・
  AudioTap (変調のフォロワー) 元・パラアウト元にしている先には無音 / 変調なし。無効トラックが持つ変調ソースは評価しない。
  PDC から外す (device bypass と同じ前例)。solo の判定表からも外す。Global Sampler の Track ソースは無音。
- **再生中に有効へ戻す**: 再生は止めない。プラグインのロードが終わった時点から鳴る。
- **録音待機**: 無効化で解除し、無効中は待機にできない。**ランチャー**: 無効トラックのセルは発火できず (沈めて表示)、
  鳴っているセルは止まる。**マスター**は無効化できない。
- **プロジェクトを開いたとき**: 無効トラックのプラグインはロードしない。
- **Q のショートカット説明文** (`shortcuts.rs:159`) に「トラックヘッダ上ではトラックの無効化」を足す。

## 設計

### model (`common`)
- `Track.enabled: bool` (既定 true、serde default true。`Send.enabled` / `ModSource.enabled` / `AutomationLane.enabled` と同じ語彙)。**v40**。
- `Song::track_effectively_enabled(track_id) -> bool` (祖先走査、`track_visually_silenced` の走査と同形) を唯一の判定口にする。
- `track_visually_silenced` は無効を含める (映像・画像・字幕の decode / 合成 / FX がまとめて止まる)。
  `active_visual_groups` (`group_compose.rs:198-255`) にこの除外が無いので揃える。

### engine (`daw_audio`)
- コンパイル時に実効無効トラックを**グラフから外す** (Process の手を出さない → そのトラックの plugin 依頼が出ない)。
  前例: device bypass のコンパイル時除外 (`program_build.rs:50-55`) / `Pass1Role::Bus` の 0 埋め (`execute.rs:211-222`)。
  「トレースから外す」か「役割 variant を足す」かは参照側 (調査 §4 の表: group の Mix / MixSend / sidechain tap /
  ParallelOutTap / EnvelopeFollow / PDC / solo 表 / sampler tap) を全部辿って、**参照側が無効トラックの古い buffer を
  読まない**ことを保証できる形を選ぶ。
- **注意**: engine はプラグイン登録の無い device を「音声素通り」にする (`execute.rs:129-130`)。アンロードしただけで
  グラフから外さないと、無音ではなく素通しで鳴る。
- **有効に戻したトラックは読み込みが確定するまで無効と同じ** (残件修正): 同じ素通し規則で、読み込み中の FX を持つ
  トラックが dry で鳴っていた。Song は意図 (有効) を持ったまま、engine のグラフだけが `Song::executable_mask`
  (実効的に有効 ∧ 自分と祖先が host への読み込み中の plugin を持たない) で決まる。読み込み中の所有者は daw_gui の
  `pending_plugin_loads` で、`AudioCommand::SetLoadingDevices` が写しを engine へ届ける (live / export の compile が同じ表を
  読む)。順序は GUI が守る: 増える分は `LoadSong` より前、確定した分は `OpenPluginShmem` の後、消えた分は `LoadSong` の後
  (`AppData::sync_loading_devices` / `apply_slot_reconcile_actions`)。失敗で確定した device は表から外れて従来どおり素通し、
  映像 device は host に載らないので関係しない。有効に戻したトラックの読み込みは A7 の一時停止をしない (`LoadPlayback`)。
  待たせるのはその描画で op を出す plugin だけ (`daw_audio::graph::executable_tracks`: bypass 中 / Sources scope の FX は待たない)。
  待っている行は plugin が載ったまま凍るので、外れる瞬間に鳴っている音を止める予約にする (`mixer::silence_disabled_rows`)。
  オフライン描画 (書き出し / 解析 / Bounce / Glue) は読み込みが全部確定してからしか始めない (`reject_offline_render_while_loading`)。
- 無効トラックだけが参照する音声素材はデコード / 常駐しない (`compile_audio_schedule` `audio_clip_renderer.rs:309-371`)。
- 無効化・有効化は構造変更として LoadSong で届く (device bypass と同じ)。

### plugin host 同期 (`daw_gui`)
- ホストに居るべき device の唯一の導出口 `compute_slot_reconcile_actions` (`device_addr.rs:157-201`) で
  **実効無効トラックの device を除外**する (映像 device 除外の前例 `:168-170`)。undo / redo の reconcile も同じ口を通る。
- アンロード前に state を Song へ書き戻す: 削除系と同じ `PendingStateRequest::Deferred` + `RequestAllStates` 往復
  (`app_types.rs:1504-1509`、`handler/devices.rs:850-863`)。`apply_plugin_states_to` は応答に無い device の state を消さない。
- 再ロードは `initial_state` に `PluginInstance.state` (`project.rs:1054-1102`)。`ClosePluginShmem` → `RemoveSlotPlugin` の順序制約を守る。
- ARA document 同期 (`handler/sync.rs:105-205`) から無効トラックの ARA device を外す。

### VOICEVOX
- `sync_vocal_metadata` で無効トラックのメタデータを送らない (**送ると plugin host の synth thread が自動起動する** `builtin/voicevox.rs:868-871`)。
- 有効な vocal トラックが 1 本も無ければ VOICEVOX エンジンを起動しない (`ensure_voicevox_engine`)。口パク再生成も対象外。

### GUI
- **Q の振り分け** (`view/bypass_toggle.rs`): 「アレンジのヘッダ列の hover」か「ミキサーのストリップ hover」を**時間範囲の枝より前**に置く
  (今は範囲選択があるとヘッダ上の Q でも範囲がミュートされる `bypass_toggle.rs:103`)。ヘッダ列だけの hover は今は無いので、
  widget の `ArrangementFrame.header_pane` / `track_header_rects` から立てる。`root.rs::dispatch_shortcuts` (519 / 530) に分岐を足さない。
- **右クリックメニュー** (`view/track_header_menu.rs`、base で切り出し済み): 「無効化」/「有効化」(対象トラックの状態で文言を変える)。
  メニュー順は索引の約束どおり。
- **沈め表示**: テーマの Palette (`row_dim_ink` / `text_faint` / `muted_dim_fill` の既存 idiom、調査 §7) を使う。ヘッダ・レーン帯・クリップ・
  ミキサーのストリップ。明暗両テーマで読めること。widget の `ArrangementTrack` に実効状態を渡す。
- 無効化したトラックのプラグイン窓を閉じ、インスペクタのチェーンに「無効中」を出して窓を開く操作を塞ぐ。
- 録音待機 / ランチャーの発火 / マスター: 上の細部どおり。
- edit は `edit_song` (undo + dirty)。

## テスト (高いレイヤーで)
- compile: 無効トラックの Process が出ない / group 無効で子も出ない / send・sidechain・AudioTap の参照先が無音。
- reconcile: 無効化で RemoveSlotPlugin、有効化で state 付き SetSlotPlugin、undo / redo でも同じ。state が失われない。
- VOICEVOX: 無効トラックのメタデータを送らない。
- load / save 往復 (serde default true)、グループ継承の判定関数。

## 完了条件
索引の共通規則どおり (全件系を回さない、daw_gui を起動しない、branch に commit、逸脱を報告)。
