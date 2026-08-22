# A6: tempo / time_sig 変更 UI

> 注: daw_01 の実装計画ファイルは通常 `F:/dev/daw_01/docs/plan_<feature>.md` に置く運用 (memory: "Plan files in docs/")。
> 本ファイルは plan mode の制約で `~/.claude/plans/` に置いているが、 ExitPlanMode 承認後に
> `F:/dev/daw_01/docs/plan_a6_tempo_timesig.md` へ移送して実装を進める。

## Context

`docs/plan.html` の Phase 6 における 4 タスク中、A2 / A3 / A7 が完了し、次が **A6 (tempo / time_sig 変更 UI)**。

- `Song { bpm: f32, time_sig: (u8, u8) }` schema は既に完備
- ruler / arrangement_view / piano_roll_view は **既に `app.song.bpm` / `app.song.time_sig` を毎フレーム参照** (gui_01 #014 で対応済) → モデル更新だけで表示は自動追従
- transport bar には `BPM 120.0` の **read-only label しか無く**、 time_sig は表示すら無い

**ゴール**: transport から BPM / time_sig を編集可能にする。 編集すると ruler / grid / 再生テンポが追従し、 Ctrl+Z で巻き戻る。

優先度: 軽量 / 独立。 plugin_host 側の transport ext 実装 (CLAP/VST3 plugin に bpm を渡す) は M2 範囲なので **本タスクの対象外**。

## 実装方針

### 1. UI 配置 ([daw_gui/src/view/transport.rs](daw_gui/src/view/transport.rs))

現状の BPM read-only label (transport.rs:24-32) を `text_input_at` に置き換え、 後ろに time_sig 入力 (numerator + "/" + denominator) を追加。 残りの button 列はそのまま右にずらす。

```
[ BPM input(70px) ] [ TS num(40px) ] / [ TS den dropdown(50px) ] | Play | Stop | Loop | Synth | +Vocal | +Inst | playhead
```

- **BPM**: `text_input_at` で 1..=400.0 範囲 (commit 時に clamp)、 表示は `format!("{:.1}", v)` (小数 1 桁)
- **time_sig numerator**: `text_input_at` で 1..=32 範囲 (commit 時 clamp)
   - → 設計判断: plan.html は範囲のみ規定 (UI 種別は曖昧)。 `dropdown(1..=16)` だと項目過多 + 31/16 等の異常拍子が打てない。 BPM と同じ text_input 方式で統一
- **time_sig denominator**: `dropdown(["2","4","8","16"])` (gui_01 dropdown widget、 4 項目固定)

### 2. AppData フィールド追加 ([daw_gui/src/app.rs](daw_gui/src/app.rs:117 周辺の AppData struct))

text_input は「表示文字列」を caller が毎フレーム供給する設計 (`text` 引数)。 編集中の途中文字列を AppData に保持する必要がある。

```rust
// AppData
pub bpm_edit_text: String,           // default: "120.0"
pub time_sig_num_edit_text: String,  // default: "4"
```

(denominator は dropdown なので edit buffer 不要、 song.time_sig.1 を直接 selected index に変換)

### 3. AppEvent + handle_event ([daw_gui/src/app.rs](daw_gui/src/app.rs:606 AppEvent enum, 730 handle_event))

```rust
// AppEvent (File / playback セクション付近に追加)
BpmEditChanged(String),
CommitBpmEdit,
TimeSigNumEditChanged(String),
CommitTimeSigNumEdit,
SetSongTimeSigDenominator(u8),

// is_undoable に追加 (commit 系のみ — text 変更途中は undo 対象外)
| AppEvent::CommitBpmEdit
| AppEvent::CommitTimeSigNumEdit
| AppEvent::SetSongTimeSigDenominator(_)
```

handler:

```rust
fn bpm_edit_changed(&mut self, s: String) {
    self.bpm_edit_text = s;
}
fn commit_bpm_edit(&mut self) {
    if let Ok(v) = self.bpm_edit_text.trim().parse::<f32>() {
        let clamped = v.clamp(1.0, 400.0);
        self.song.bpm = clamped;
        self.sync_song_to_plugin_host();
    }
    // parse 失敗 / clamp 適用後どちらの場合も bpm_edit_text を formatted に書き戻す
    self.bpm_edit_text = format!("{:.1}", self.song.bpm);
}

fn time_sig_num_edit_changed(&mut self, s: String) {
    self.time_sig_num_edit_text = s;
}
fn commit_time_sig_num_edit(&mut self) {
    if let Ok(v) = self.time_sig_num_edit_text.trim().parse::<u8>() {
        let clamped = v.clamp(1, 32);
        self.song.time_sig.0 = clamped;
        self.sync_song_to_plugin_host();
    }
    self.time_sig_num_edit_text = self.song.time_sig.0.to_string();
}

fn set_song_time_sig_denominator(&mut self, den: u8) {
    if !matches!(den, 2 | 4 | 8 | 16) { return; }
    self.song.time_sig.1 = den;
    self.sync_song_to_plugin_host();
}
```

### 4. song 切り替え時の edit text 同期

undo / redo / `action_new` / `action_open` 等で `self.song` が差し替わった後、 edit_text を同期させる必要がある。

```rust
fn resync_song_edit_texts(&mut self) {
    self.bpm_edit_text = format!("{:.1}", self.song.bpm);
    self.time_sig_num_edit_text = self.song.time_sig.0.to_string();
}
```

呼び出し箇所 (既存コードを `Grep song =` 等で特定して追加):
- `AppData::new` 末尾 (初期化)
- `action_new` / `action_open` / `action_load_recent`
- `undo` / `redo` (history pop 後)

### 5. 既存ユーティリティの再利用

- `sync_song_to_plugin_host()` (app.rs:1013-1020) — Song 変更を audio engine に publish + dirty flag。 そのまま使う
- `push_undo_snapshot()` — handle_event 冒頭で `is_undoable` 判定により自動呼び出し。 追加コードは不要
- audio engine 側 (`daw_audio/src/main.rs:131`) は `MainToChild::LoadSong` を `shared.song.store(Arc::new(song))` で wait-free publish。 BPM / time_sig が次フレームの sequencer から見える
- ruler / arrangement_view / piano_roll_view は毎フレーム `app.song.bpm` / `app.song.time_sig` を view 構築時に参照 (arrangement_view.rs:105-106、 piano_roll_view.rs:65-66)。 追加配線不要

## Plugin host 側について

`grep MainToChild::LoadSong daw_plugin_host/` の結果 0 件 + `clap_plugin.rs:556` で `transport: std::ptr::null()`。 つまり **plugin に bpm/time_sig は渡していない**。 BPM 変更で plugin が tempo 同期する機能は M2 範囲 (CLAP `clap_event_transport_t` の実装) なので本タスクでは対象外。

## 主な変更ファイル

- [daw_gui/src/view/transport.rs](daw_gui/src/view/transport.rs:15-98) — BPM label を text_input に置換 + time_sig num / den 追加 + 後続ボタン列の x 再計算
- [daw_gui/src/app.rs](daw_gui/src/app.rs)
  - AppData struct に `bpm_edit_text` / `time_sig_num_edit_text` 追加
  - `AppData::new` で 2 フィールドを初期値に
  - AppEvent enum に 5 variant 追加
  - `is_undoable` に commit 系 3 variant 追加
  - handle_event match arm 5 つ追加
  - 5 つの handler メソッド + `resync_song_edit_texts` helper
  - `action_new` / `action_open` / `action_load_recent` / `undo` / `redo` で `resync_song_edit_texts` 呼び出し

## 検証

```bash
cargo check --workspace
cargo clippy --workspace -- -D warnings
cargo build -p daw_gui
cargo run -p daw_gui
```

smoke test シナリオ:
1. **BPM 編集**: 起動 → transport BPM 欄をクリック → "180" タイプ → Enter → ruler 数値変化、 Play で速度変化
2. **BPM 範囲外入力**: "999" → Enter で 400.0 に clamp、 "abc" → Enter で元値に戻る (parse 失敗)
3. **numerator 編集**: 既定 4/4 で "3" → Enter → arrangement view の bar 線が 3 拍ごとに変わる
4. **denominator 変更**: dropdown を開いて "8" 選択 → 6/8 / 7/8 等の表記が ruler / grid に反映
5. **Undo/Redo**: BPM 180 / TS 7/8 にしてから Ctrl+Z 連打 → 元に戻る、 Ctrl+Y で再適用
6. **Open/New**: 別 .daw を Open → BPM / TS の表示文字列が新 song 値に更新される
7. **rt-assert**: `cargo test --features rt-assert -p daw_audio` で audio thread RT 違反が出ないこと

## スコープ外 (将来課題)

- CLAP/VST3 plugin への transport / tempo 通知 (M2 範囲)
- bar 番号位置の編集 (時計合わせ的な offset)
- tempo automation / tempo map (途中で tempo が変わる楽曲)
- 拍子記号 automation / 途中変更
