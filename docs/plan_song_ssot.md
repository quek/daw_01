# plan_song_ssot.md — `Song` を 3 プロセス重複から canonical + cache へ

## 動機

現状、`common::model::Song` が 3 プロセスで独立に複製されている。

```text
daw_gui            ← canonical (user 編集)
daw_audio          ← full clone, 編集ごとに IPC で broadcast
daw_plugin_host    ← full clone, 編集ごとに IPC で broadcast
```

- 編集 1 回ごとに `MainToChild::SetSong(Song)` 全送信が走る (大プロジェクトで
  数百 KB 〜 MB)
- Undo/Redo の rollback タイミングで 3 プロセスの song 状態が瞬間ズレる
  リスク
- recovery 時、どの状態を canonical とするかの仕様が暗黙
- IPC 帯域が編集頻度に比例して増え、autosave + edit の同時実行で contention

## 目標

- **canonical = daw_gui** のみ。 Audio / Plugin host は **必要なフィールドの
  サブセット** を view-only cache として持つ
- 編集は **差分プロトコル** で送る (`MainToChild::SongDelta { ... }`)
  - 例: `TrackAdded { id, name, color, ... }` / `ClipMoved { track, clip, new_pos }`
  - full snapshot は loadProject / recovery 時のみ
- 各子プロセスが必要とするビューを cache 型で型表現

## 子プロセスごとの最小ビュー

### daw_audio が必要とする情報

- `tracks: Vec<TrackAudioCache>`
  - `id, mute, solo, volume, pan, send_routes, ...`
- `clips: Vec<ClipAudioCache>` (track_id 別)
  - `id, content_id, start, length, fade_in, fade_out, ...`
- `audio_sources: HashMap<ContentId, AudioSourceRef>` (cache miss 時に
  daw_gui から fetch)
- `tempo_map`, `loop_bounds`, `time_sig`

→ `track_params.rs` / `audio_render.rs` の流用で大半カバー。 `Song` 全体を
持つ必要は無い。

### daw_plugin_host が必要とする情報

- `plugins: HashMap<PluginId, PluginInstanceState>`
  - `loaded_path, format, persistent_state, latency`
- `chain_routing: Vec<ChainEntry>` (track_id 別)
- ノートイベントスケジュール (現状の note routing 経路維持)

→ Song 全体ではなく **plugin chain と route 情報** のみ。

## 段階移行 (4 ステップ)

```text
Step 1  Audio 側に AudioSongCache 型を導入し、SetSong の代わりに
        SetAudioCache(AudioSongCache) を送る
Step 2  PluginHost 側に PluginRouteCache を導入、同様に切り替え
Step 3  差分プロトコル (SongDelta) を追加。 各 edit イベント で
        broadcast 内容を full → delta に
Step 4  full SetSong の使用箇所を loadProject / recovery 専用に縮退
```

各ステップは backward-compat な enum 拡張で進める (旧 variant を残しつつ
新 variant を優先)。

## 差分プロトコル設計指針

- **idempotent** であること (recovery 時に同じ delta を 2 回流しても
  破綻しない)
- **ordered** であること (受信順を保証する monotonic seq number)
- **bounded buffer** で送る (lockfree ring または shared memory + sem)。
  CLAUDE.md の RT 規約上、audio thread 上でブロックしない経路にする
- delta 例:
  ```rust
  enum SongDelta {
      TrackAdded { id: TrackId, name: String, ... },
      TrackRemoved { id: TrackId },
      ClipMoved { track: TrackId, clip: ClipId, new_start: u64 },
      VolumeChanged { track: TrackId, db: f32 },
      PluginChainEdited { track: TrackId, op: ChainOp },
      // ...
  }
  ```

## リスク / 制約

- 既存 IPC 経路 (`SetSong` 送信箇所) は app.rs 全域に散在。 個別書き換え
  ではなく Step 1 で broadcast helper を 1 箇所に集約してから切り替える
- Audio thread は **常に最新 cache を即時参照する** ため、cache 更新は
  ArcSwap 等で wait-free に
- delta 順序保証を pipe / shared memory で実現するなら、現状の
  `common::wire` / `common::pipe` の seq 機構を確認・拡張
- `bincode::Encode/Decode` derive をすべての cache 型・delta 型に必要

## 完了基準

- `MainToChild::SetSong(Song)` が `loadProject` / `recovery` のみ
- 編集 1 回あたり IPC 送信量 < 1 KB (メタ + 1 delta) を目標
- audio thread 上で song snapshot を heap 確保せず参照可能
  (ArcSwap + Cache 型)
- 既存 smoke test (再生 / 録音 / autosave / undo) が通る

## 関連

- [docs/plan_appdata_split.md](plan_appdata_split.md) — daw_gui 内の
  AppData 解体。 `TracksState` 切り出しと並行して進めると edit イベント
  経路が type-safe に
