# gui_01 ↔ daw_01 conversation

daw_01 Claude Code から gui_01 Claude Code への要望・バグ報告・API 質問と、
gui_01 Claude からの返信を時系列に蓄積するログ。

## 運用ルール

- **daw_01 Claude**: 新規エントリを末尾に追加。番号は連番、ステータスは `[Open]` で開始
- **gui_01 Claude**: `### gui_01 →` ブロックに返信を書き、ステータスを `[Replied]` に変更
- **daw_01 Claude**: 返信を読んで対応完了したらステータスを `[Resolved]` に更新
- 解決済みは履歴として削除せず、`[Resolved]` 確定したら都度
  `docs/gui_01_conversation_archive_NNN.md` (現行 `_archive_001.md`) に切り出す。
  archive のエントリ数が 100 を超えたら `_archive_002.md` を新規作成して以降を貯める
- daw_01 Claude は gui_01 のバグ・不足 API に気づいたら、**勝手に回避策を書く前に**
  ここに相談エントリを追加する（CLAUDE.md の "外部 API の挙動を先に理解する" 原則）

## エントリテンプレート

```markdown
## #NNN [Open] YYYY-MM-DD [種別] 件名 1 行

### daw_01 →
- 種別: [要望] / [バグ報告] / [質問] / [相談] のどれか
- 関連ファイル: `daw_gui/src/view/foo.rs:42`
- 本文（再現手順・期待挙動・想定 API イメージ等）
- gui_01 側で見るべきソースの当たり: `crates/core/src/heavy.rs` 等

### gui_01 →
（gui_01 Claude が記入）

---
```

