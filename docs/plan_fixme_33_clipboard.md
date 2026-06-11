# plan_fixme_33 — cut / copy / paste (C-x / C-c / C-v) 統一クリップボード

FIXME #33「一般的な DAW と同様に C-x, C-c, C-v での cut, copy, paste を実装」。
grill-me (2026-06-11) で対象範囲・文脈判定・ペースト位置・共有/独立・トラックの忠実度まで詰めた。

## 現状 (2026-06-11)

- **コピー/ペーストは note と automation point のみ実装済み**。OS クリップボードに JSON を書く
  方式 (`Ui::set_clipboard_text` / `Ui::take_clipboard_paste`、
  [gui_01 ui.rs:1732,1742](F:/dev/gui_01/crates/ui/src/ui.rs))。
  - copy: `copy_selected_automation_points_as_json` ([app.rs:7891](F:/dev/daw_01/daw_gui/src/app.rs)) →
    無ければ `copy_selected_notes_as_json` ([app.rs:2480](F:/dev/daw_01/daw_gui/src/app.rs)) の
    **automation 優先固定**。
  - paste: clipboard text を **trial-decode** (note JSON → ダメなら automation JSON)。
    selected automation がある時だけ automation 優先 ([root.rs:382-417](F:/dev/daw_01/daw_gui/src/view/root.rs))。
  - note paste は **再生ヘッド位置** に貼る ([app.rs:2555-2567](F:/dev/daw_01/daw_gui/src/app.rs))。
- **cut (Ctrl+X) はどこにも未実装**。gui_01 shortcut map には `cut`/`copy`/`paste` が
  Ctrl+X/C/V で bind 済み・typing-aware
  ([gui_01 shortcut.rs:223-225,300](F:/dev/gui_01/crates/ui/src/shortcut.rs))。daw_01 の
  `dispatch_shortcuts` ([root.rs:276](F:/dev/daw_01/daw_gui/src/view/root.rs)) は `cut` を
  一切 take していない。
- **clip / track / audio event のコピペは未実装**。
- `Delete` は **選択優先順** (audio event > automation point > note > automation clip > clip) で
  対象を決める ([root.rs:418-453](F:/dev/daw_01/daw_gui/src/view/root.rs))。`Ctrl+A` は
  **ポインタ位置** で決める ([root.rs:535-575](F:/dev/daw_01/daw_gui/src/view/root.rs))。両者は不統一。
- データモデル:
  - `Clip.content_id: ContentId` → `Song.clip_contents: HashMap<ContentId, ClipContent>` で
    notes を共有 store 化済み (`docs/plan_clip_share_clone.md`)。共有/独立は ContentId で表現。
  - `Track.devices: Vec<PluginInstance>` ([model.rs:1386](F:/dev/daw_01/common/src/model.rs))、
    `PluginInstance.state: Option<Vec<u8>>` ([model.rs:1748](F:/dev/daw_01/common/src/model.rs))。
    state は **保存/操作時に plugin_host から取得して書き戻す** (live では stale な可能性)。
  - `common::ClipKey { track_id, clip_id }` が選択 SSoT (安定 ID、`docs/plan_select_all.md`)。
- **最新 plugin state を取ってから実行する仕組みが既にある**:
  `RequestAllStates` → `AllPluginStates` → `PendingStateRequest::Deferred(DeferredEdit)`
  ([app.rs:2764-2793](F:/dev/daw_01/daw_gui/src/app.rs)、
  `on_all_states_from_child` [app.rs:13836](F:/dev/daw_01/daw_gui/src/app.rs)、
  `execute_deferred_edit` [app.rs:13911](F:/dev/daw_01/daw_gui/src/app.rs))。DeleteTrack 等が
  これを使い、削除直前の knob を Undo に残す。
- `add_track_insert_index` ([app.rs:16519](F:/dev/daw_01/daw_gui/src/app.rs)) は現状
  **選択トラックの直下** (bottom-most selected + 1、無選択は末尾)。

## 確定仕様 (grill-me 2026-06-11) — 見える挙動

### 対象範囲

**選択できるものすべて**を cut/copy/paste 対象にする:

| 対象 | copy | cut | paste |
|---|---|---|---|
| ノート (ピアノロール) | 既存を再設計 | 新規 | 既存を再設計 (位置=マウス拍) |
| オートメーションの点 | 既存を再設計 | 新規 | 既存を再設計 (位置=マウス拍) |
| オーディオエディタのイベント | 新規 | 新規 | 新規 |
| クリップ (MIDI/Audio/歌声/automation) | 新規 | 新規 | 新規 |
| トラックまるごと (クリップ・エフェクト・音源込み) | 新規 | 新規 | 新規 |

### 文脈判定 (copy / cut / paste / delete で共有)

**ポインタが乗っている編集面で対象種別を決める。その面に選択が無ければ選択優先順に
フォールバック**する。

- 1st: ポインタが乗っている面 (Ctrl+A と同じ idiom)
  - ピアノロール (下部パネル) → ノート
  - オーディオエディタ (下部パネル) → イベント
  - アレンジのオートメーションレーン上 → 点 (or automation clip)
  - アレンジのクリップレーン上 → クリップ
  - トラックヘッダ列 → (copy/cut 対象としての) トラック
- 2nd (フォールバック): ポインタが面の外なら、既存 Delete と同じ選択優先順
  (audio event > automation point > note > automation clip > clip)。

→ **Delete もこのハイブリッド arbiter に統一**する (ポインタが面に乗っていればその面を優先、
外れていれば従来の優先順)。従来 Delete 挙動は包含されるので回帰しない。

### ペースト位置 — すべてマウス駆動

**クリップボードの中身には種類があり、本来入る面が決まっている。ポインタがその「合う面」の
上に無いときは no-op + status メッセージ** (再生ヘッドへのフォールバックはしない)。

| クリップボードの種類 | 合う面 | 位置 |
|---|---|---|
| ノート | ピアノロール | 編集中クリップへ、時間=マウス拍 (clip-local)、pitch は元のまま |
| イベント | オーディオエディタ | 編集中クリップへ、時間=マウス位置 |
| 点 | アレンジのオートメーションレーン | マウス下のレーンへ、時間=マウス拍 |
| クリップ | アレンジのクリップレーン | **マウス下のトラック**へ、時間=マウス拍 |
| トラック | トラックヘッダ列 **または** クリップレーン | **マウス下のトラックの直上**に挿入。トラックの外 (末尾の空白) なら末尾追加 |

- 複数コピー時は**相対位置を保持**: 時間は最早 (earliest) を、トラックは最上段を、マウス位置に
  合わせて他を相対配置。
- ペースト直後、**貼ったものが新しい選択になる** (直後の移動 / 連続ペーストの起点)。
- マウスがトラックレーン上のとき、クリップボード=クリップ→そのトラックに貼る、=トラック→その上に
  新トラック挿入。**クリップボードの種類でアクションが一意に分岐**する。

### 共有 / 独立

- **クリップ paste**: 同じプロジェクト内ならコピー元と**共有 (リンク)** (同 ContentId、片方の
  編集が両方に反映、共有グループに加わる)。別プロジェクト/別ウィンドウへは**独立** (中身を
  inline 復元して新 ContentId)。D キー (共有複製) と役割が揃い、Alt+D / cross-project が独立。
- **トラック paste**: 中のクリップは元トラックのクリップと**共有 (リンク)** (同プロジェクト)。
  別プロジェクトへは独立。**プラグインは仕組上つねに新規インスタンス** (各々が別 state を持つ)。
- **cut → paste で取り消した独立クリップを再リンク**できるよう、clipboard は ContentId 参照と
  inline 中身の両方を持つ (下記 §clipboard envelope)。

### トラックコピーの忠実度

トラックを copy/cut するとき、**最新の plugin state を捕まえてから serialize** する。
貼ったトラックは音 (knob 位置) まで元と同一になる。Save と同じ忠実度 (fresh plugin への妥協は
しない)。既存の `RequestAllStates` → `AllPluginStates` → `DeferredEdit` 機構に乗せる。

### cut セマンティクス

**cut = いまの対象を clipboard に入れて即削除、Undo 1 回で戻る**。削除は同じ arbiter で対象を
決める。トラック cut は deferred (state 取得 → clipboard 書込 → 削除) で 1 undo step。共有クリップの
cut はその 1 インスタンスだけ削除 (refcount--、既存 delete と同じ)。

### Ctrl+T の変更 (FIXME #33 に付随)

`Ctrl+T` のトラック追加を、現状の「選択トラックの直下」から **「選択トラックの直上」** に変更
(top-most selected の手前に挿入)。無選択時は末尾追加のまま。

## 内部設計 (ユーザー確認不要 / SSoT・DRY)

### 1. 統一クリップボード envelope (型タグ付き JSON, OS clipboard)

trial-decode を廃止し、discriminant で分岐する単一 envelope に作り直す。

```rust
// daw_gui (or common) — OS clipboard text として serde_json で round-trip。
#[derive(Serialize, Deserialize)]
struct ClipboardEnvelope {
    /// daw_01 clipboard を他アプリ text と区別するマーカー + format version。
    magic: String,            // 例 "daw_01.clipboard.v1"
    /// コピー元 document。paste 時の 同プロジェクト判定 (共有/独立分岐) に使う。
    source_project_id: u64,
    payload: ClipboardPayload,
}

#[derive(Serialize, Deserialize)]
enum ClipboardPayload {
    Notes(Vec<Note>),                       // 既存 copy_selected_notes_as_json を吸収
    AutomationPoints(Vec<AutomationPointCopy>),
    AudioEvents(Vec<AudioEventCopy>),
    Clips(Vec<ClipCopy>),                   // ContentId 参照 + inline ClipContent
    Tracks(Vec<TrackCopy>),                 // devices(state 込み) + clips(ClipCopy)
}
```

- 各 payload は**正規化済み相対位置**で持つ (時間は最早=0、トラックは最上段=0)。
- paste 時に **安定 ID をすべて再採番** (note idx / clip id / track id / content id)。
- 外部 (他アプリ/改竄) clipboard 由来の値は既存 note paste の validation
  ([app.rs:2519-2540](F:/dev/daw_01/daw_gui/src/app.rs)) を全 payload に拡張して
  clamp / 破棄 (NaN・Inf・範囲外を model に入れない)。
- `magic` 不一致 / decode 失敗は silently no-op (他アプリ text を貼ろうとしたケース)。

### 2. document 識別子 `Song.project_id`

同プロジェクト判定 (共有/独立分岐) のため、Song に安定 ID を持たせる。

- `Song.project_id: u64` を新設。`New` で採番、`Save`/`Load` で保持 (= 同じ .daw を開けば同 id)。
- `CURRENT_VERSION` を 1 つバンプ + migration (旧 file は load 時に採番)。
- paste 判定: `envelope.source_project_id == self.song.project_id` かつ参照先 ContentId が
  現存 → 共有。それ以外 → 独立 (inline 中身から新 ContentId)。
- 別ウィンドウで同 .daw を二重起動するケースは未サポート (IPC 衝突、`feedback_no_duplicate_app_launch`)
  なので考慮外。

### 3. 文脈 arbiter の一本化

`dispatch_shortcuts` に「ポインタ→対象種別」を返す 1 ヘルパを作り、copy / cut / paste / delete が
共有する。ポインタ→面の判定材料:

- 下部パネル: `is_pianoroll_active` / `audio_editor_clip.is_some()` (既存)。
- アレンジ: `arrange_hovered_automation_lane` (既存) / `hovered_clip` / `hovered_track` を mirror
  ([gui_01 arrangement.rs:847-907](F:/dev/gui_01/crates/ui/src/widgets/arrangement.rs))。
- トラックヘッダ列: 既存 `track_header_rects` (clip_share plan で使用) で判定。
- 面の外: 選択優先順にフォールバック。

### 4. ペースト位置の供給 (ポインタ拍) — gui_01 非依存

paste-at-mouse に必要な「ポインタ拍」は全て daw_01 側で取得済み/取得可能 (gui_01 依存なし)。

- **アレンジ**: `arrangement_hover_beat` (snap 済) / `arrangement_hover_beat_raw` (raw, song-absolute) /
  `arrangement_hover_clip` を `arrangement_view.rs:977-986` が毎フレーム mirror 済。
- **ピアノロール**: `piano_roll_view.rs:309-314` が pointer→beat 算出済。`pianoroll_hover_beat: Option<f64>`
  mirror を view に 1 つ足す (clip-local, snap 済)。
- **トラック挿入先**: `ArrangementResponse.hovered_track` を `arrange_hovered_track: Option<u32>` に mirror 追加。
- **オーディオエディタ**: `audio_editor_hover_beat_in_clip` (clip-local) を `audio_editor.rs:745-757` が mirror 済。

### 5. model 操作 (AppData) — 既存 helper を流用

- ノート: 既存 `copy_selected_notes_as_json` / `paste_notes_from_json` を envelope 経由に作り直し、
  paste 位置を playhead → マウス拍に変更。`notes_in_clip_mut` ([app.rs:2571](F:/dev/daw_01/daw_gui/src/app.rs))。
- クリップ複製ロジックは既存 `DuplicateClipsShared` / `DuplicateClipsUnique` (共有/独立) の
  内部関数を流用し、配置先を「マウストラック+拍」に差し替え。
- トラック: copy/cut は `PendingStateRequest::Deferred(DeferredEdit::CopyTracks{..} / CutTracks{..})`
  を新設し、`on_all_states_from_child` 後に最新 state 込みで serialize (+ cut は続けて削除)。
  paste は `add_track_insert_index` 系の挿入 + clips の共有/独立解決。
- すべて `is_undoable` 登録、cut は 1 step。

## 依存

**gui_01 依存なし** (要望 #098 は調査の結果不要と判明し取り下げ、2026-06-11)。grounding workflow で確認:
paste-at-mouse に必要な「ポインタ拍」は §4 の通り全て daw_01 側で取得済み/取得可能。トラック paste 先用の
`arrange_hovered_track` mirror 追加のみ daw_01 内で完結する小作業。

## 実装順 (全工程 daw_01 完結、gui_01 待ちゼロ)

1. `common`: `Song.project_id: u64` (uuid v4) + `ensure_project_id` (`normalize_after_load` で呼ぶ) +
   `CURRENT_VERSION` 23→24 + migration test。
2. `daw_gui/clipboard.rs` (新): `ClipboardEnvelope { magic, source_project_id, payload }` +
   `ClipboardPayload` enum + 正規化 copy struct (`CopiedPoint` / `ClipCopy` / `TrackCopy`) + serialize/parse。
3. AppData フィールド追加: `pending_clipboard_write: Option<String>` / `pianoroll_hover_beat: Option<f64>` /
   `arrange_hovered_track: Option<u32>`。view 2 箇所で mirror。
4. copy helper (envelope 経由): notes / automation points / audio events / clips。既存 note/automation
   helper を envelope へ作り直し。
5. paste helper (payload 種別で分岐 + マウス位置配置): notes(pianoroll_hover_beat) /
   points(arrange/lane hover) / events(audio_editor_hover) / clips(arrange hover + hovered_track,
   project_id でリンク/独立) / tracks(hovered_track 直上, clips リンク/独立, plugin 新インスタンス)。
6. トラック copy/cut の async 機構: `PendingStateRequest::CopyToClipboard{track_ids}` (非 undo) +
   `DeferredEdit::CutTracks{track_ids}` (undo)。完了は `on_all_states_from_child` で serialize →
   `pending_clipboard_write` へ。
7. arbiter 一本化: `edit_surface(app, is_pianoroll_active) -> EditSurface` を root.rs に新設、
   copy/cut/paste/delete を統一。`cut` shortcut wire。`pending_clipboard_write` を毎フレーム drain。
8. `Ctrl+T` を選択トラック直上に変更 (`add_track_insert_index` の caller を確認の上「top-most selected の手前」へ)。
9. テスト + 検証 → commit → release build green。

## 受け入れ基準

- ノート/点/イベント/クリップ/トラックを、ポインタが「合う面」の上で Ctrl+C → 別の場所で Ctrl+V →
  マウス位置に貼れる。合わない面の上では no-op + status。
- クリップ paste: 同プロジェクト内は元と共有 (link アイコン/共有色が付き、片方編集が両方に反映)、
  別プロジェクト (New/Open 後) へは独立。
- トラック paste: マウス下トラックの直上に挿入。中のクリップは元と共有、プラグインは新インスタンスで
  **knob 位置まで元と同一** (deferred state)。複数コピーは相対順を保持。
- Ctrl+X: 対象が clipboard に入り即削除、Ctrl+Z 1 回で復元。
- Ctrl+T: 選択トラックの直上に新トラック (無選択は末尾)。
- 文字入力中の Ctrl+X/C/V は文字編集 (gui_01 が typing-aware で奪う)。
- Delete のハイブリッド化で既存 Delete 挙動が回帰しない。
- `cargo test --workspace` pass / `cargo clippy --workspace -- -D warnings` clean / release build green。

## 実装状況 (2026-06-11 完了)

全フェーズ実装・green (`cargo build --workspace` / `cargo clippy --workspace --tests -- -D warnings` /
`cargo test -p common`(228) / `-p daw_gui --lib`(125) / release build)。**未 commit・実機検証は未実施**。

- grounding workflow (6 サブシステム並列読取) → 実装 → **敵対的レビュー workflow (5 観点並列)** で
  bug を検出し修正。検出 → 修正した主な指摘:
  - 🔴 **ノート paste 座標系**: piano roll は song-absolute (FIXME #3) なので mirror で
    `snapped - clip_start_beat` の clip-local 化が必要だった (dbl-click AddNote と同じ変換)。修正済。
  - 🟠 **トラック paste の plugin 未ロード**: `sync_song_to_plugin_host` (LoadSong) は plugin host で
    no-op。`restore_plugins_for_tracks` (SetSlotPlugin + state) を新設し paste 後に呼ぶ。修正済。
  - 🟠 **cross-project クリップ paste のリンク切れ**: `content_remap` で同一 content_id を 1 度だけ
    採番し dedup (linked クリップ群を複数貼ってもリンク保持)。clip / track 両 paste で統一。修正済。
  - 🟡 **Clips/Tracks paste の sanitize 欠如**: `sanitize_clips` / `sanitize_tracks` / `sanitize_content`
    を新設し root.rs paste で適用 (length_beats finite>0、volume/pan clamp、内部 notes/events/points)。修正済。
  - 🟡 **paste_clips_at の空 undo snapshot**: 貼り付け対象 0 件なら push_undo せず return。修正済。
  - 🟡 **トラック content の same-project 検証 + cross-track leak**: content_remap 統一 (現存なら流用、
    欠落/別プロジェクトは採番、dedup で orphan content リーク解消)。修正済。
  - 🟢 `edit_surface` の毎フレーム Vec 確保を安価な空判定に。修正済。

## 非範囲 / 既知の限界

- automation point / audio event の **ID 安定化** (不安定 index のまま、別 plan)。
- automation clip (lane 上の箱) の copy/cut/paste は **未対応** (copy/cut で status 通知のみ、delete は従来どおり)。
  別 payload + lane targeting が要るため別 phase。
- **cross-project paste の media source**: audio/image/video clip を別プロジェクトへ paste すると、
  `AudioEvent.source_id` / image・video source / `mouth_map` の参照先 source pool が同梱されないため
  解決できず (波形・口画像等が欠落)。同一プロジェクト内 paste と MIDI/automation の cross-project は
  影響なし。media 同梱は envelope 肥大化のため別 feature。
- **hover mirror の 1 フレーム遅延**: ポインタが面に入った最初の 1 フレームで Ctrl+V を押すと稀に
  空振り (mirror が前フレーム値)。次フレームで自己回復。実害は 1 フレーム限定。
- paste_points_at で同一 (time, value) の点を複数貼ると、貼り直後の選択集合の idx が一部重複しうる
  (データ自体は正しい、選択のみ軽微にずれる)。
- 別ウィンドウで同 .daw 二重起動時の cross-window 共有 (二重起動自体が未サポート)。
- ペースト時の paste-special (反転/transpose 等の変換) や repeat-paste の自動オフセット。
