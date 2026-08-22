<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: Bounce In Place / Bounce (with FX) 再設計 (FIXME #42)

## ゴール

「Bounce In Place」「Bounce (with FX)」を、**全クリップ種別 (Audio / MIDI / VOICEVOX 歌唱)** に
対して実際に機能させる。現状は `ClipContent::Audio` 専用で MIDI/歌唱クリップは入口で silent
reject され「全く無反応」になる。

- **Bounce In Place** = クリップの音源/synth 出力を **トラックの insert FX を通さず** 焼き、
  **同じ位置に置換**。
- **Bounce (with FX)** = 音源/synth + **そのトラックの insert FX のみ** (master/group/bus は
  通さない) を焼き、**そのクリップ/トラックだけを isolate** して **新トラックに複製**、
  **元トラックは自動ミュート** (非破壊、二重再生回避)。

## 確定設計 (インタビュー済、再議論しない)

- 対象は全クリップ種別 (Automation/Video/Image/Text は対象外)。MIDI/歌唱は音源/synth を通して
  オーディオ化。
- In Place = insert FX 抜き・同位置置換。with FX = insert FX 込み・新トラック複製・元ミュート。
- 両方ともそのクリップ/トラックだけを isolate (他トラックの音を混ぜない。現状 with FX が
  時間範囲の全ミックスを焼くのはバグ)。

## 検証済の致命的バグと必須修正 (adversarial review、これらが naive 実装を上書きする)

### C-1. insert-FX バイパスで device を削除してはいけない (最重要)

`track.devices: Vec<PluginInstance>` から has_audio_input device を削除すると Vec が compact
され、生き残った instrument の index がズレる。engine は plugin を **(track_id, device_index)**
で `slot_to_plugin_id` 経由解決する (engine.rs ~1282-1290 の device ループが
`0..devices.len()` を回し `(track_id, i)` を引く)。`slot_to_plugin_id` は OpenPluginShmem/reorder
でのみ維持され (engine.rs ~484-581、main.rs ~187)、**LoadSong では re-key されない**
(daw_audio/src/main.rs ~144-166 は audio_clip_renderer 再構築 + song 格納のみ)。よって
instrument の前にある effect を削除すると instrument の index がズレて `(track_id, new_idx)` が
登録済 `(track_id, old_idx)` を外し、instrument が skip され (engine.rs ~1288 else continue) →
無音 render → frames==0。device 順は任意 (devices.push の追加順 app.rs ~15406、reorder 対応)
なので instrument が index 0 とは限らない。

**正解**: device Vec の長さ/位置を保ったまま、各 insert device (`d.ports.has_audio_input == true`)
を **その場で中和**する: `d.ports = PortConfig::default()` (4 bool すべて false)。engine は
dispatch するが audio in をコピーせず (~1328 false)、audio out を書かず (~1369 false)、MIDI も
出さない (~1316/1340 false) ので完全バイパス。instrument は元の index を保持し
`slot_to_plugin_id.get(&(track_id, idx))` が解決する。**新たな bypass flag を PluginInstance に
足さない** (存在しない。port-model が SSoT、役割判定を作らない方針)。

### C-2. In Place を is_undoable から外す

`AppEvent::BounceClipInPlace(_)` は is_undoable リスト (app.rs ~2544) にある。In Place が
同期だった今は dispatch 時 auto-push が正しく bracket するが、**async 化すると** dispatch 時
auto-push (IPC 往復の前) + 共有 completion handler の push (~11901) で **二重スナップ**、
さらに render 失敗時にも spurious スナップ。`BounceClipWithFx` は正しく is_undoable に無い
(~2485-2640 で確認)。In Place も ~2544 から **削除** し、completion handler が**成功時のみ
1 回** push_undo_snapshot を持つ (with FX と同じ。両 branch がこの単一 push を共有)。

### C-3. VOICEVOX 歌唱は freewheel で無音になる → 事前合成が必須

builtin VOICEVOX synth (daw_plugin_host/src/builtin/voicevox.rs ~5-21,134-150) は HTTP で
**非同期合成**して `synth_result: Arc<ArcSwapOption<SynthResult>>` に publish し、process() は
それが埋まってから鳴る。offline render は数 ms で完了し HTTP 合成完了より早いので
synth_result==None → 無音 → frames==0 → WAV 削除/失敗。

**必須手順**: BounceClipFxOnline の **前に**、対象クリップ (または対象トラックの builtin
VOICEVOX device) の note metadata を flush (SetBuiltinPluginNoteMetadata) し、**合成完了を待つ**
(または歌唱を audio source に事前 render)。既存の `daw.synthesize_vocal` (V キー) の事前合成/
キャッシュ経路を調査して再利用する。これは「歌唱対応」確定設計のための **必須ステップ**
(open question ではない)。

### C-4. completion handler で LoadSong(full) 復元

今の with FX は LoadSong(self.song.clone()) full を送る (app.rs ~11839-11840) ので engine は
full song を保持する。両 path を LoadSong(isolated) に切り替えると engine が単一トラック song を
保持したままになり、bounce 後のライブ再生が bounce 対象トラックしか鳴らない。completion
handler で LoadSong(full) を送って復元する (両 branch で必要)。

### C-5. pending 中の LoadSong を gate

isolated snapshot は completion まで shared.song に居る。autosave/seek/SetTrackVolume 等は
LoadSong を送らない (軽量 id ベース) が、構造編集/send 追加など **LoadSong を送る操作** が
trigger と completion の間に挟まると isolated snapshot を上書きし full mix を焼いてしまう。
pending guard は二度目の bounce しか止めない。`pending_clip_fx_bounce.is_some()` の間は LoadSong
送信 (少なくとも autosave/構造編集由来) を gate する。

### C-6. isolated_bounce_song の自己完結化

isolated_bounce_song では retain したトラック以外に、`master_fx_chain` をクリアし、各 retained
device の `sidechain_sources` (PluginInstance.sidechain_sources model.rs ~1801)、retained
トラックの sends + `parent_group_id` をクリアする。これで compile_schedule (compile.rs
~69-106) が group/send/sidechain ノードを emit せず DanglingReference も出ない。
(execute_schedule_post_dispatch engine.rs ~1551 は Mix+ApplyDelay+ProcessGroupFx+SidechainTap を
走らせるが process_master_fx_chain (~931、process_buffer 内) は走らせない。単一トラック isolate
+ 参照クリアで group/send 発生を防ぐ。)

### C-7. is-Audio guard 撤去

bounce_clip_in_place の is-Audio guard (~11465-11470) と bounce_clip_with_fx の guard
(~11740-11745) を撤去。Audio/Midi/vocal を受け、Automation/Video/Image/Text を reject。これが
「全く無反応」(MIDI/歌唱の silent-reject) の主因の修正。is_voicevox_vocal は model.rs ~1655、
has_audio_input は port_config.rs ~28-34。

### C-8. with FX の自動ミュート重複

model のミュート + LoadSong(full) が muted=true を運ぶので、別途 SetTrackMuted (protocol.rs
~359、stable Track::id) は冗長。余分な送信を**落とす**か LoadSong→SetTrackMuted の順序を保つ。
double-source-of-truth に注意。

## 実装方針 (経路)

- menu: arrangement_view.rs ~614 (BounceClipInPlace idx6) / ~619 (BounceClipWithFx idx7)。
- dispatch: app.rs ~5424-5444。
- PendingClipFxBounce (~712-721) に GUI 側 `BounceMode { InPlace, WithFx }` を持たせ、共有
  completion handler (handle_bounce_clip_fx_complete ~11859-11980、undo push ~11901、frames==0
  guard ~11893) を branch する。
- `isolated_bounce_song(song, target_track, target_clip, bypass_inserts: bool) -> Song` を新設:
  対象トラックだけ retain、`bypass_inserts` なら insert device を C-1 の方式で中和、C-6 の
  参照クリアを実施。**新 IPC protocol field は不要** (isolate + 中和を LoadSong 前の song
  snapshot 内で完結。BounceClipFxOnline は既存の `{ path, source_track, source_clip,
  start_frame, end_frame }` ~310-316、ChildToMain::BounceClipFxComplete ~75-81 をそのまま使う)。
- daw_audio: bounce thread main.rs ~450、export.rs run_export の range 対応 ~60-145
  (render_loop は master FX を焼かない、確認済)。
- In Place は bypass_inserts=true、with FX は false。両方 isolated_bounce_song を LoadSong し、
  C-3 の事前合成後に BounceClipFxOnline、completion で C-4 復元。In Place は結果クリップを
  同位置置換、with FX は新トラック + 元ミュート (C-8)。

> 注: In Place は元の同期 GUI-side サンプルミックス (bounce_clip_in_place ~11456-11718) を
> 廃し、engine offline render 経路へ移行する (MIDI/歌唱を synth 経由で鳴らすため async 化は
> 不可避)。

## エッジケース

- instrument が device[0] でないトラック: C-1 の中和方式で index 不変 → 正しく鳴る。
- group トラック (children あり): engine は has_children のトラックの instrument/sequencer を
  skip (engine.rs ~1193-1203)。isolated 単一トラック song では children が無く has_children==false
  なので普通に render。bounce はクリップ自身のトラック内容のみを対象 (folder semantics に注意)。
- bypass_inserts=false の with FX で sidechain/send 参照が残ると compile で drop される →
  C-6 でクリア済。
- render 失敗 (frames==0): WAV 削除 + 失敗報告 (~11893)。undo は push されない (C-2)。

## ビルド/検証

- protocol field は増やさない方針だが、isolated_bounce_song が Song を組むので **`cargo build
  --workspace`** で daw_audio と整合 (Song は bincode 型、念のため workspace ビルド)。
- `cargo clippy --workspace -- -D warnings`、`cargo test --workspace`。
- 実機 (最終バッチで一度): MIDI クリップを In Place → 同位置にオーディオ化 (FX 抜き)。
  歌唱クリップを In Place → 事前合成後にオーディオ化 (無音でないこと)。with FX → 新トラックに
  FX 込みオーディオ + 元トラックミュート、二重再生しないこと。instrument が device[0] でない
  トラックでも無音にならないこと。bounce 中に autosave が走っても full mix を焼かないこと。
- `/review` を commit 前に実行。commit 後 `cargo build --workspace --release` green 確認。

## 待機中の進め方

gui_01 非依存。ただし最も複雑なので、C-3 (VOICEVOX 事前合成) の既存経路調査を最初に行い、
それから isolated_bounce_song → In Place async 化 → with FX isolate/mute → gate/restore の順で
実装する。
