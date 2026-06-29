# ARA2 ホスト実装プラン (r.md #5)

VOICEVOX 歌声 DAW daw_01 に ARA2 (Audio Random Access 2, Celemony) のホスト側サポートを
ground-zero から実装する。Rust 製 ARA ホストは前例が無い (plugin 側の ara2-bridge のみ)。

## 1. ゴールとスコープ (確定)

| 論点 | 確定 |
|---|---|
| 用途 | 合成/録音した歌声・オーディオを Melodyne 等で編集 → 再生・書き出しに反映 (フル UX) |
| 形式 | CLAP + VST3 両対応。純 C の CLAP を先行実装 → VST3 (COM) を続けて |
| スコープ | プロジェクト保存/再読込で編集 (ピッチ修正等) が残る所まで (ARA archive 永続化) |
| 紐づけ | トラックに device として挿すと、そのトラックのオーディオクリップが自動で編集対象 (Reaper/Studio One 流)。SSoT = `Track.devices` |
| アーキ | 3 プロセス分離維持 (out-of-process)。ARA モデルは daw_plugin_host が所有 |

## 2. ARA の要点 (一次情報: Celemony/ARA_API)

- ARA は VST3/CLAP の**上に乗る拡張**。Melodyne 等が「ブロック単位の RT 処理」でなく
  **タイムライン全体の音にランダムアクセス**して解析・編集できる。Apache-2.0、登録不要。
- **DocumentController はプラグイン側**が実装。ホストはそれを**駆動**し、5 つの host controller を提供:
  - `ARAAudioAccessControllerInterface` — **必須**。`readAudioSamples` で元 PCM を供給。
  - `ARAArchivingControllerInterface` — **必須** (生成時)。store/restore の時しか呼ばれない → 最初は stub 可。
  - `ARAModelUpdateControllerInterface` — 推奨。解析進捗 / content 変更通知。
  - `ARAContentAccessControllerInterface` — 任意。tempo/key 等をプラグインへ。歌同期に有用。
  - `ARAPlaybackControllerInterface` — 任意。プラグイン→ホストの transport 要求。NULL 可。
- モデル生成は **host 駆動・plugin 実体化**: host が `createXxx(hostRef)` を呼ぶと plugin が `xxxRef` を返す。
  - 永続: `ARAAudioSource` / `ARAAudioModification`。非永続(ホスト再生成): `ARAMusicalContext` /
    `ARARegionSequence` / `ARAPlaybackRegion`。
- **編集後の音は plugin の `process()` を通すしか出口が無い**。bind を `kARAPlaybackRendererRole` で行い
  `addPlaybackRegion` で region を割当 → 再生時は通常 `process()` を `kPlaying` で呼ぶ。
  playback renderer は**入力を無視し ARA 編集信号で出力を置換** → daw_01 の freewheel export 方針と一致。
- DocumentController はシングルスレッド (host の model スレッド)。`readAudioSamples` は別ワーカースレッドから。
  編集は必ず `beginEditing()`/`endEditing()`、非編集時に周期 `notifyModelUpdates()`。

### CLAP バインディング (主経路, ARACLAP.h, 純 C)
- descriptor features に `"ara:supported"` / `"ara:required"` で**非インスタンス化検出**。
- `entry.get_factory("org.ara-audio.ara.factory/2")` → `clap_ara_factory_t`
  → `get_ara_factory(i)` で `ARAFactory*`、`get_plugin_id(i)` が CLAP id ↔ ARA factory を対応付け。
- `plugin.get_extension("org.ara-audio.ara.pluginextension/2")` → `clap_ara_plugin_extension_t`
  → `bind_to_document_controller(plugin, dcRef, knownRoles, assignedRoles)`。
  **activate / state load / GUI 生成より前に 1 回だけ**。

### VST3 バインディング (ARAVST3.h + COM)
- `ARA::IMainFactory` (category `"ARA Main Factory Class"`) から `getFactory()`、または
  audio-effect の `IComponent` を `IPlugInEntryPoint`/`IPlugInEntryPoint2` に queryInterface。
- ARA2 束縛 = `IPlugInEntryPoint2::bindToDocumentControllerWithRoles(...)`。
  同一 `IComponent` が `IAudioProcessor` (通常 process) と `IPlugInEntryPoint2` (ARA) の両方を実装。

## 3. アーキテクチャ (out-of-process の肝)

ARA モデルグラフは **daw_plugin_host プロセス側** (= プラグインと同居) に生きる。host controller の
コールバック (`readAudioSamples` 等) は**プラグインから同期的に呼ばれる**ので、応答に必要なデータは
daw_plugin_host が**自前で持つ**のが理想 (IPC 越しの PCM 同期転送はレイテンシ/複雑性ともに最悪):

- **AudioAccessController = daw_plugin_host が WAV を直接読む**。daw_gui から「この AudioSource は
  このファイルパス・SR・ch・frames」というメタを ARA document 構築時に送る (PCM 本体は送らない)。
  プラグインが `readAudioSamples(start, count)` を呼んだら、host 側で**そのファイルを mmap/seek read**
  して返す。daw_audio が再生用に読むのとは独立 (読み取り専用なので競合しない)。
  - 長尺対応: ファイルハンドルを AudioSource ごとに保持し、要求範囲だけ decode。RT スレッドではない
    (ARA の audio access は非 RT ワーカー) のでヒープ確保・I/O 可。
- ContentAccess (tempo/musical context) は Song のテンポ情報を daw_gui→host で送って供給。
- ModelUpdate (解析進捗・content 変更) は host→daw_gui へ pipe で通知 → UI 反映。

Celemony 公式も out-of-process ARA を `TestHost/IPC` でデモしており、この分担が定石。

## 4. SDK バインディング方式

- **新 crate `ara-sys`** を workspace に追加 (clap-sys と同列の sys crate)。
- ARA_API のヘッダ (`ARAInterface.h` / `ARACLAP.h` / `ARAVST3.h`) を **`third_party/ARA_API/` に vendoring**。
  - ffmpeg と異なりヘッダは小さく安定 → **git 追跡に入れる** (.gitignore しない)。worktree に自動で伝わる。
  - ライセンスは Apache-2.0 (LICENSE.txt 同梱)。
- `ara-sys/build.rs` で **bindgen** 生成 (vst3 crate が com-scrape/libclang を既に要求する系統なので
  ツールチェーン増分なし)。allowlist で host が使う型に絞る。
  - 生成物が libclang に依存するのを嫌う環境向けに、生成 bindings を `ara-sys/src/generated.rs` に
    commit するフォールバックも可 (bindgen を一度回して vendoring)。まず build.rs 方式で進める。
- C ABI なので `#[repr(C)]` 関数ポインタ構造体。先頭 `ARASize structSize` を**必ず正しく設定** (ABI version 耐性)。

## 5. ホスト側 controller 実装 (daw_plugin_host)

`daw_plugin_host/src/ara/` を新設:
- `mod.rs` — ARA ホスト全体の orchestration。document controller のライフサイクル。
- `host_controllers.rs` — 5 つの `#[repr(C)]` controller vtable と Rust 実装。
  - `audio_access.rs` — WAV 直読み (hound or 既存 decode 経路を流用)。
  - `archiving.rs` — IBStream 相当の archive read/write。最初 stub → 永続化フェーズで実装。
  - `model_update.rs` — 解析進捗 → pipe で daw_gui へ。
  - `content_access.rs` — Song tempo/bar → plugin。
- `document.rs` — `beginEditing`/`createMusicalContext`/`createRegionSequence`/`createAudioSource`/
  `createAudioModification`/`createPlaybackRegion`/`endEditing` の配線 (MiniHost.c の順序が事実上の仕様)。
- `binding_clap.rs` / `binding_vst3.rs` — factory 取得 + `bind_to_document_controller`。
- host_data ポインタ復元は既存 `clap_host.rs` の `Box<Host>` 固定パターンに倣う。

## 6. モデル配線 (SSoT = Track.devices)

- `common::PluginInstance` に **`is_ara: bool`** (descriptor features 由来、scan 時に確定) を追加。
- ARA device を持つ Track の `ClipContent::Audio(AudioEvent)` 群を **ARA playback region に写像**:
  - `AudioEvent.source_id` → `ARAAudioSource` (Song.audio_sources のファイルパスで AudioAccess が読む)
  - `AudioEvent` の clip 内時間 (`event_start_in_clip_beats` + Clip.start_beat) → playback region の
    playback time、`source_start/end_frames` → modification time range。
  - 1 トラック = 1 `ARARegionSequence` (musical context に紐付け)。
- **役割判定しない** (memory `feedback_no_role_classification`): ARA device があれば、そのトラックの
  audio クリップを機械的に全部 region 化するだけ。instrument/effect 等の分類はしない。

## 7. IPC プロトコル拡張 (common)

`MainToChild` / `ChildToMain` に追加 (全型 bincode derive、変更後 `cargo build --workspace`):
- `M→C SetupAraDocument { slot, region_sequences: Vec<AraRegionSeq>, audio_sources: Vec<AraAudioSrc>, musical_ctx }`
  - `AraAudioSrc { id, path, sample_rate, channel_count, frame_count }`
  - `AraPlaybackRegion { audio_source_id, mod_start_frames, mod_end_frames, playback_start_beats, playback_dur_beats }`
- `M→C UpdateAraDocument {...}` (クリップ編集・移動時の差分再構築)
- `M→C GetAraArchive { slot } -> C→M AraArchive { slot, bytes }` (保存)
- `M→C RestoreAraArchive { slot, bytes }` (読込)
- `C→M AraAnalysisProgress { slot, source_id, progress }` / `AraContentChanged { slot }` (ModelUpdate 反映)

## 8. 再生レンダリング経路

- bind 時に `assignedRoles = kARAPlaybackRendererRole | kARAEditorRendererRole | kARAEditorViewRole`。
- `playbackRendererInterface.addPlaybackRegion(rendererRef, regionRef)` を **plugin 非 active 時 +
  document スレッド**から呼ぶ (制約: ARAInterface.h L3484)。
- 既存 `process_server.rs:730` の `process()` 経路はそのまま。transport (song_pos_beats 等は ProcessData に既存)
  が `kPlaying` を伝えれば playback renderer が編集済み信号を出力。**入力 buffer は renderer が無視**。
- export: 既存 freewheel + 同一 instance 流用 (project_export_clean_start) がそのまま効く。

## 9. 編集 GUI

- bind の `kARAEditorViewRole` で plugin の ARA エディタ (Melodyne 画面) が有効化。
- 表示は既存の plugin GUI 埋め込み経路 (`editor_window.rs` / `view/plugin_embed.rs`) を流用。
  トラックの ARA device の GUI を開く = Melodyne エディタが出る。
- `ARAEditorViewInterface` の selection 通知等は最小実装 (まず GUI が出て編集できる所まで)。

## 10. 永続化 (archive)

- 保存時: `GetAraArchive` で plugin の document archive (= 全 audio modification の編集) を bytes 取得 →
  既存 `PluginInstance.state` とは別の **`PluginInstance.ara_archive: Option<Vec<u8>>`** に格納し Song と一緒に保存。
- 読込時: LoadSong → ARA document 再構築 (audio source/region) → `RestoreAraArchive` で編集復元。
- archive 順序: ARA spec の store/restore (ARAArchivingController) に従い IBStream 相当を実装。

## 11. 実装順序 (MiniHost.c の配線順 = 事実上の仕様)

1. `ara-sys` crate + ヘッダ vendoring + bindgen (CLAP + core)。`cargo build` 通過。
2. CLAP ARA 検出: scan で `ara:supported` を拾い `PluginInstance.is_ara` を立てる。
3. host controllers: AudioAccess (WAV 直読) 実装 + Archiving/ModelUpdate stub。
4. document 配線: musical context → region sequence → audio source → modification → playback region。
5. bind (PlaybackRenderer role) + addPlaybackRegion → **再生で Melodyne 編集音が出る** (第一の動作確認点)。
6. 編集 GUI (EditorView role) を plugin_embed に load → ピッチ編集できる。
7. ContentAccess (tempo) + ModelUpdate (解析進捗 UI)。
8. 永続化 (archive store/restore)。
9. VST3 経路 (ARAVST3.h + COM、`IPlugInEntryPoint2`)。
10. クリップ編集/移動時の document 差分更新。

CLAUDE.md「最終形まで実装する」に従い 1→10 を完走する (途中での承認待ちはしない)。

## 12. 検証

- ユニット: ara-sys の structSize/ABI、AudioEvent→region 写像ロジック (非自明な所だけ)。
- headless: `daw_gui --script` に ARA document 構築 → process → 出力 PCM 検証経路を追加できるか検討
  (Melodyne 実体が要るので、まずは ARA 対応の軽量 CLAP/VST3 で smoke)。
- 実機: ARA 対応プラグイン (要ユーザー環境確認) をトラックに挿し、歌声クリップを編集 → 再生で反映 →
  保存/再読込で編集維持、を最終 sign-off。
- 既存の `--smoke-test` (video) は無関係だが、ARA 経路が既存 process パスを壊さないことを確認。

## 未確定 / 要ユーザー確認 (実装をブロックしない範囲)

- ユーザーが実際に持っている ARA 対応プラグイン (Melodyne / VocAlign / RX 等) — VST3 優先度の最終判断材料。
- 実機検証はプラグイン実体が要るので最終バッチで一度だけ依頼 (memory `feedback_no_redundant_verification`)。
