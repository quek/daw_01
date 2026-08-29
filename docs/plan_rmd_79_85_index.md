# r.md #79-#85 — 第 2 波 (規範とハーネスの整備) の索引

第 1 波 (#71 / #73-#77 = 機能・UI) の索引は [plan_rmd_index.md](plan_rmd_index.md)。
本書は **#79-#85 = 「規範・ゲート・ハーネス」側**の波の索引で、記録として残す価値があるのは
項目一覧そのものより **着手前に回した反証の結果**のほうである (下の §2)。

作業上の禁止事項はここに複製しない。全 worktree に効く規範は `CLAUDE.md` と
`.claude/guards.jsonl` (追跡下) が持つ。plan 側にコピーを置くと、第 1 波でそうなったように
「どちらが正か分からない 2 つ目の正本」ができる。

## 1. 項目

| # | 主題 | 主な変更先 |
|---|---|---|
| 79 | 検査スクリプトのゲート化 (未配線の検査を常設ゲートへ) | `Makefile` / `scripts/test_guards.py` |
| 80 | reflection 検出の偽陽性 | `scripts/reflect.py` / `.claude/hooks/stop_session_reflect.sh` |
| 81 | make recipe の環境 scrub (NVIDIA litter / `expanduser("~")`) | `Makefile` |
| 82 | 規範文書の二重管理 | `CLAUDE.md` / `ui/CLAUDE.md` / `README.md` / `DESIGN.md` / `.claude/skills/` |
| 83 | 「テスト」節が死文 (js テストが存在しない) | `CLAUDE.md` |
| 84 | 「最終形まで実装する」が実運用と食い違う | `CLAUDE.md` |
| 85 | 規範文書の分量整理 | `CLAUDE.md` / `ui/CLAUDE.md` + 移送先 docs |

#82-#85 は同じファイル群を触るので 1 worktree にまとめた。

## 2. 着手前の反証で覆った主要結論 (2026-08-29)

8 本の調査に **1 本ずつ独立の反証**を当てた。主要結論のうち **6 本が覆った**。
これが `CLAUDE.md`「まず調べる → 調査結果の採用条件」の実測根拠である。

| # | 調査が書いた結論 | 反証が確定させた事実 | 失敗の型 |
|---|---|---|---|
| 79 | test_guards.py をそのまま常設ゲートにできる | make 配下では Python の `expanduser("~")` がリテラル `~` を返し、`PASS 230 / FAIL 0` が `FAIL 27` になる。**#81 を先に直さないと新ゲートが即座に赤くなる** | 結論が使われる経路 (make) で一度も実行していない / 隣の項目との衝突に気付いていない |
| 80 | 誤検出 45 → 0、真陽性は全維持 | 提案の allowlist が落とせるのは 27/91 で、偽陽性率は **96.7% → 95.3%** (1.4 ポイント)。「真陽性 32 件」の 78% (25 件) は herdr のエージェント報告で人間の発話ではなかった | 見出しと測定値が合っていない / 母数の取り違え |
| 81 | NVIDIA litter の原因は `LOCALAPPDATA` (r.md の診断も同じ) | `env -i` の交絡なし二分探索で **`ProgramData`** と確定。`ProgramData` を偽ディレクトリへ向けると litter がそこへ出るところまで機構を確定 | 測定器 (recipe 内の `env` が Git 版 = 二度目の cross-runtime scrub) が交絡していた |
| 81 | recipe に残る環境変数は 7 個 | MSYS2 自身の `env.exe` で測ると 13 個 (実 Makefile 込みで 15)。POSIX env は 1 個も継承されず、Win32 の forced セットだけが渡る | 同上 |
| 82 | `CLAUDE.md:380` の引用欠落は「劣化コピー」= r.md → CLAUDE.md → memory の伝播 | `git log -S` で **片側更新の drift**。`28e1aec` (2026-05-25) は当時の原文を一字も違えず引用しており、`573a084` (2026-06-09) が**原本 1 行だけ**を更新して引用側を放置した。memory も当時の正文を引いている | git 履歴を引かずに因果を書いた |
| — | transcript 走査は「42 セッション全数」 | 実際は 892 transcript・39,432 user 行の **19.7%**。除外された subagent 側 844 ファイルには `isSidechain=true` が 31,576 行あり、「True は 0 件」を直接反証する | 範囲の誇張 |

補足 2 件 (結論は生き残ったが根拠が差し替わったもの):

- #82 の「CLAUDE.md はコピーで SSoT ではない」は全称命題としては偽。`scripts/fetch_ffmpeg.sh` /
  `scripts/loc_budget.py` / `scripts/arch_lint.sh` / `docs/plan_plugin_editor_topwindow.md` /
  `docs/dependency_audit.md` / `docs/plan_arch_refactor.md` / `DESIGN.md` / `README.md` の
  **9 箇所が逆に CLAUDE.md を正本として指名**している。よって #85 の圧縮は
  「節見出しを残したまま本文だけ 1 行要約 + リンクに落とす」形でなければ、指す先が消える。
- #85 の削減見積り (429 / 318 / 253 行) は **差し替えテキストの行数を引いていなかった**。
  実測での到達点は §3 のとおり。

### 反証で潰すべき型 (再利用可能な形)

1. **実行して確かめずに書いた主張** — 結論が使われるその経路で動かす。
2. **測定器そのものが交絡している主張** — `env -i` 相当の対照を取る。
3. **範囲の誇張** — 母数と分母を数える。
4. **隣の項目との衝突** — 同じ波の他項目が未解決でも成立するかを見る。

同型の実測は memory `project_rmd_39_43_parallel` にもある (「実装よりレビューで出た指摘の
ほうが重かった。24-33 件検出 → 敵対的検証で 6-13 件確定」)。2 回続けて起きているので、
偶然ではなく構造として扱う。

## 3. #85 の到達点 (実測)

| | before | after | 備考 |
|---|---|---|---|
| `CLAUDE.md` | 541 | §「実測」を参照 | 目標 318 は差し替えテキスト分を引いていない見積り |
| `ui/CLAUDE.md` | 222 | 同上 | root と完全一致する 18 行 + 低頻度の罠 4 節を移送 |

移送先と、外へ出した経緯の所在:

| 経緯 | 移送先 |
|---|---|
| arrayref 0.3.10 汚染 (RUSTSEC-2026-0260) | `scripts/lockfile_guard.py` 冒頭 + [dependency_audit.md](dependency_audit.md) §0 |
| ffmpeg n7.1 の消滅 / 固定への反転 | [ffmpeg_mirror.md](ffmpeg_mirror.md) §1 |
| third_party junction で本体を消した事故 (2026-06-14) | [ffmpeg_mirror.md](ffmpeg_mirror.md) §6 |
| JUCE owner / `WS_EX_TOOLWINDOW` / `GWLP_HWNDPARENT` | [plan_plugin_editor_topwindow.md](plan_plugin_editor_topwindow.md) §背景 / §1-1 |
| CLAP GUI 仕様の落とし穴 (VCV Rack) | [plan_plugin_editor_topwindow.md](plan_plugin_editor_topwindow.md) §9 |
| guards.jsonl 消失事故 (2026-08-22) | `scripts/guard_engine.py` docstring "Registry" 節 |
| arch-lint の backslash 消失 / 偽グリーン | `scripts/arch_lint.sh` 冒頭 |
| arch-lint baseline / ratchet の運用 | `scripts/arch_lint_baseline.txt` 冒頭 |
| smoke test の仕組み | `daw_gui/src/smoke_test.rs` module doc |
| `make test` が daw_gui を起動する詳細 | `scripts/preflight_no_running_app.sh` 冒頭 |
| daw-ui の低頻度な罠 (line pipeline / TSF / wgpu offscreen / text_input) | [../ui/docs/known_traps.md](../ui/docs/known_traps.md) |

**本文に残した経緯 4 件** (移すと効かなくなるもの。判定基準は「機械強制が無い」AND
「規範が直観と逆」):
妥協の実例 (gui_01 #045 Phase 74) / keyed-mutex 削除事故 / Makefile SSoT の自己言及 /
`make arch-lint` の exit 0 の意味。
