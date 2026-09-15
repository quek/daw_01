# r.md #130 / #131 / #132 / #133 — 分担・統合順・共通規則

4 項目の仕様は 2026-09-15 にユーザーと 1 問ずつ確定済み (grill-me)。**各計画書の「確定仕様」はユーザー承認済みなので、
implement スキルの §3「要件一覧をユーザーに提示し承認」は省く**。計画書に無い判断が「作るものを変える」
規模で出たら、作業を止めて最終メッセージに質問を書く (main セッションが読んで答える)。

| 項目 | 計画書 | worktree / branch | herdr agent 名 |
|---|---|---|---|
| #130 グローバルトランスポーズ | [plan_rmd_130_transpose.md](plan_rmd_130_transpose.md) | `.claude/worktrees/rmd-130-transpose` / `worktree-rmd-130-transpose` | `rmd-130` |
| #131 トラック無効化 (Q) | [plan_rmd_131_track_disable.md](plan_rmd_131_track_disable.md) | `.claude/worktrees/rmd-131-track-disable` / `worktree-rmd-131-track-disable` | `rmd-131` |
| #132 Shift+E グリッド分割 | [plan_rmd_132_grid_split.md](plan_rmd_132_grid_split.md) | `.claude/worktrees/rmd-132-grid-split` / `worktree-rmd-132-grid-split` | `rmd-132` |
| #133 既定名を数字だけに | [plan_rmd_133_default_names.md](plan_rmd_133_default_names.md) | `.claude/worktrees/rmd-133-default-names` / `worktree-rmd-133-default-names` | `rmd-133` |

4 本とも base branch `rmd-130-133-base` から分岐する。base には次の準備 commit だけが入っている:
- この索引と 4 計画書 (`docs/`)
- `daw_gui/src/view/arrangement_view.rs` (実コード 981 / 1,000 行) からトラックヘッダの右クリックメニューと
  改名 overlay を `daw_gui/src/view/track_header_menu.rs` へ切り出し (#130 と #131 が両方ここへ項目を足すため、
  先に budget の余地を作った)

調査レポート (実装前調査 + 類似 DAW の一次情報、file:line / URL 付き) は main セッションの scratchpad にある。
各自の項目のものを最初に読むこと:
`F:/tmp/claude/F--dev-daw-01/fc83ec5f-42cf-4b3a-ad7a-b264e42025cb/scratchpad/rep/`
- `130-transpose.md` / `r130.md`、`131-track-disable.md` / `r131.md`、`132-split-notes.md` / `r132.md`、
  `133-default-names.md` / `r133.md`

## 統合順

**#133 → #132 → #131 → #130** (main セッションが `git merge-tree` で衝突を実測してから 1 本ずつ入れる。
完了順が前後したら実測で入れ替える)。

- #133 は表示名を 1 関数へ集約し、多数の view ファイルに浅く触る。先に入れて、後続が「表示名はこの関数」を
  前提にできるようにする。
- #130 と #131 は model (Track / Song / 版番号) とヘッダ (メニュー / 印 / 沈め表示) を両方触る最大の組。

## 重なるファイルと約束

| ファイル | #130 | #131 | #132 | #133 | 約束 |
|---|---|---|---|---|---|
| `common/src/model.rs` `CURRENT_VERSION` | v41 | v40 | — | (要るなら v42) | **版番号は事前割当**。先に入った側に欠番があっても可。統合時に main が履歴コメントを整える |
| `common/src/model/track.rs` | `Track.follow_transpose` | `Track.enabled` | — | — | 行単位マージ |
| `daw_gui/src/view/track_header_menu.rs` | 「移調に追従」 | 「無効化 / 有効化」 | — | — | メニュー順は `Rename / 複製 (独立) / 複製 (リンク) / 色... / クリップ色をトラックに揃える / 移調に追従 / 無効化 / Delete` |
| `daw_gui/src/widgets/arrangement/header.rs` / `mod.rs` (`ArrangementTrack`) | 名前横の印 | 行を沈める | — | 表示名 | 行単位マージ |
| `daw_gui/src/view/mixer_strips.rs` | — | 沈める | — | 表示名 | 行単位マージ |
| `daw_gui/src/handler/voicevox.rs` (`sync_vocal_metadata`) | pitch に移調 | 無効トラックを除外 | #132 は無し | — | 意味的に独立 (移調量の計算と対象トラックの選別) |
| `daw_gui/src/view/root.rs::dispatch_shortcuts` (519 / 530) | — | (Q は `bypass_toggle.rs` 側) | Shift+E / E / J | — | **#132 は root.rs に分岐を足さず関数へ出す** |
| `daw_gui/src/view/shortcuts.rs` | — | Q の説明文 | Shift+E 追加 | — | 行単位マージ |
| `AppData::handle_event` (1531 / 1605) | arm 追加 | arm 追加 | arm 追加 | — | arm は 1 行で handler 関数へ委譲 |

## 全 worktree 共通の規則

### 着手時 (この順で)
1. `third_party/` を main から**実コピー**する (herdr の worktree は `.worktreeinclude` を経由しない。junction / symlink 禁止):
   `cp -r F:/dev/daw_01/third_party <worktree>/third_party`
2. `cargo build -p daw_audio -p daw_plugin_host` (子 exe が要る)
3. 自分の計画書と調査レポートを読む

### 禁止
- **`make test` / `make test-nolaunch` / `make clippy` / `make arch-lint` / `make gates` を回さない** (全件系は統合後に main が 1 回)。
  完了判定は `cargo check -p <触った crate>` と、**daw_gui を起動しない**関連テスト target を名指しで回すことだけ。
  起動するかの判定は `grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs` (名前で判断しない)。
- daw_gui を起動しない (`--script` / `--smoke-test` 含む)。実機確認は統合後に main がまとめて依頼する。
- `r.md` を編集しない。main checkout (`F:/dev/daw_01`) のファイルに書かない (読むのは可)。
- `cargo update` をしない。

### 完了時
- 自分の branch に commit する (日本語メッセージ、`git add` はパス全列挙、`-A` / `.` 禁止)。main へは入れない。
- 最終メッセージに: commit sha / 変更の要約 / 計画書から逸脱した判断 (小さくても 1 行) / 回したテストと結果 /
  統合時に main が知るべき注意 (他項目との意味的な結合点)。

### 他セッションとの相談
他項目の担当に聞きたいことがあれば `herdr agent prompt <agent 名> "$(cat <ファイル>)"` で送る
(長文・バッククォートを含む文は必ずファイル経由)。相手の作業を止めさせる依頼はしない。
