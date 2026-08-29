# 依存の脆弱性 triage (`make audit`)

`make audit` が出す指摘を 1 件ずつ一次情報で調べた記録。**同じ調査を二度やらないため**に残す。
新しい指摘が出たらここに追記する。運用方針は `CLAUDE.md`「依存の脆弱性 / 供給網攻撃」節、
設定は `deny.toml` の `[advisories]`。

## 0. なぜ `make audit` を足したか (2026-08-20 の arrayref 汚染)

crates.io の **arrayref 0.3.10** が汚染された (**RUSTSEC-2026-0260**)。typosquat の
`proc-macro1` への依存が足され、その build script が **コンパイル中にリモートのバイナリを取得
して実行**する。同じ攻撃者が 23 分の間に `internment` 0.8.7 と `append-only-vec` 0.1.9 も汚染
した。Rust Security Response Team は作者の端末 / 資格情報の侵害と見ている。

**daw_01 は無事だった** — `Cargo.lock` の arrayref が 0.3.9 のままで、`cargo update` を走らせて
いなかったから。つまり守ったのは検査ではなく **lock を commit していたこと**で、当時この
リポジトリには依存の脆弱性検査が 1 つも無かった。運が良かっただけなので `make audit` を足した。

帰結として運用の中心は 2 つ:

1. **`Cargo.lock` の commit が一次防御**。`cargo update` は「更新したい理由があるとき」だけ
   意図的に打ち、打ったら必ず `make audit` を通す。
2. **新しい汚染事件を知ったら `scripts/lockfile_guard.py` の `KNOWN_COMPROMISED` に
   `(name, version, 出典)` を 1 行足す**。完全一致判定なので誤検知が無く、ネットワークも要らない。

「なぜ lock の commit が一次防御なのか」の詳しい説明は `scripts/lockfile_guard.py` の
module docstring が正本。

## 現状 (2026-08-22): **GREEN**

検出は 9 件。**全件が daw_01 の使い方では露出しない**ことを確認したうえで、
5 件は版を上げて解消、2 件は根拠を書いて `ignore`、2 件は範囲設定で対象外。

| ID | crate | 種別 | 露出 | 処置 |
|---|---|---|---|---|
| RUSTSEC-2026-0190 | anyhow 1.0.102 | unsound | なし | **1.0.104 へ更新** |
| RUSTSEC-2026-0204 | crossbeam-epoch 0.9.18 | vulnerability | なし | **0.9.20 へ更新** |
| RUSTSEC-2026-0258 | h2 0.4.13 | vulnerability | なし | **0.4.18 へ更新** |
| RUSTSEC-2026-0194 | quick-xml 0.39.2 | vulnerability | なし | **0.41.0 へ更新** |
| RUSTSEC-2026-0195 | quick-xml 0.39.2 | vulnerability | なし | 同上 |
| RUSTSEC-2025-0141 | bincode 2.0.1 | unmaintained | なし | **ignore** (上げ先が墓標) |
| (yanked) | rusty_ffmpeg 0.16.5+ffmpeg.7.1 | yanked | なし | **ignore** (原因は feature gate の誤り) |
| RUSTSEC-2024-0436 | paste 1.0.15 | unmaintained | なし | 対象外 (transitive) |
| RUSTSEC-2026-0206 | rustybuzz 0.20.1 | unmaintained | なし | 対象外 (transitive) |
| RUSTSEC-2026-0192 | ttf-parser 0.25.1 | unmaintained | なし | 対象外 (transitive) |

`unmaintained = "workspace"` にしてあるので、自分で版を選べない transitive の保守停止は
報告されない (実測: all=4 / transitive=3 / workspace=1 / none=0)。3 件とも調査済みで
**露出なし・patched 版なし**。rustybuzz だけは resvg 0.47→0.48 で外せる可能性がある。

## 版を上げた 5 件 — 更新時に踏んだ手順

供給網攻撃が進行中の時期 (arrayref 0.3.10 の 2 日後) の更新なので、次を厳守した。
**次回も同じ手順で行うこと。**

1. `cargo update -p <crate>` を **1 crate ずつ**。引数なしの `cargo update` は禁止。
2. 毎回 `git diff Cargo.lock` を確認。出てよいのは意図した crate の version / checksum 行と、
   それが必要とする推移的 bump だけ。
3. **新しい package が 1 つでも現れたら停止して報告**。typosquat の混入はこの形で起きる
   (arrayref 0.3.10 は `proc-macro1` という新規依存を足していた)。
4. 新版に **`build.rs` が新しく生えていないか**、**build-dependencies が増えていないか**を確認。
   生えていたら停止して報告。攻撃の実行経路はそこ。

実測結果: 5 件すべてで **新規 package ゼロ / 消滅ゼロ**。lock の差分は version + checksum の行のみ。

### crossbeam-epoch 0.9.20 で手順 4 が実際に発火した

0.9.18 に無かった `build.rs` が 0.9.20 に出現したので停止して調査した。**結論は無害**:

- build.rs は 443 バイト全文が「環境変数 `CARGO_CFG_SANITIZE` を読んで
  `cargo:rustc-cfg=crossbeam_sanitize_thread` を出す」だけ。ネットワーク / プロセス起動 /
  `include!` / `from_raw` / `libloading` は grep で 0 件。
- **build-dependencies なし**。
- 退行ではなく上流の履歴どおり: CHANGELOG 0.9.16「Remove build script. (#1037)」→
  0.9.19「Improve compatibility with ThreadSanitizer. (#998)」。TSan 用 cfg のための再導入。
- 出所: `.cargo_vcs_info.json` の sha1 `239bae00257967a109911b9ebe7c0554d6333501` は
  GitHub crossbeam-rs/crossbeam に実在 (2026-07-06, author = Taiki Endo)。
  PR #998 は 2026-02-20 merge。
- 0.9.20 の CHANGELOG が RUSTSEC-2026-0204 そのものの修正を記載。

## ignore した 2 件

### bincode 2.0.1 (RUSTSEC-2025-0141)

開発チームが doxxing / harassment を受けて開発を恒久停止した、という informational
advisory。**CVE も CVSS も「こう壊れる」という記述も無い**。crates.io の
**bincode 3.0.0 は墓標リリースで lib.rs の中身は `compile_error!` 1 行** — 上げると即ビルド不能。

daw_01 での用途は (1) 名前付きパイプ経由の IPC (相手は自分で spawn した子プロセス)、
(2) builtin VOICEVOX の state 復元 — こちらが**プロジェクトファイル由来の任意バイト列が
デコーダに届く唯一の経路**。ただし bincode 2.0.1 は safe Rust の逐次デコーダで長さ claim
ガードを持ち、`state_load` の失敗も握りつぶさず daw_gui へ返す。踏むべき欠陥が無い。

wincode への移行は、technical failure mode が無いのに IPC の中核を差し替えることになり、
`common/build.rs` の `WIRE_SOURCES` fingerprint 機構との整合も未検証。churn なので採らない。

**見直しトリガ** (時間ではなくイベント): bincode 2.x に informational でない advisory が
出たとき / edition や serde の破壊的更新でコンパイル不能になったとき。

### rusty_ffmpeg 0.16.5+ffmpeg.7.1 (yanked)

**yank の理由はセキュリティではない。** 上流の履歴で確定した:

| 日付 | 出来事 |
|---|---|
| 2025-08-18 | 0.16.4 公開 |
| 2025-08-19 | commit `55512dba` "feat: add AVChannelLayout definitions" → **0.16.5 公開** |
| 2025-08-23 | "Adapt for FFmpeg 8.0" → 0.16.6 公開 |
| 2025-08-23 | PR #147 "fix(avutil): correct FFmpeg version of ch_layout" → **0.16.7 公開** |

0.16.5 が `channel_layout` を **無条件で** 公開したため、`ffmpeg6` feature が OFF のビルドで
存在しないシンボルを参照して**コンパイルエラー**になる。PR #147 が
`#[cfg(feature = "ffmpeg6")]` を付けて修正し、0.16.5 と 0.16.6 が yank された。
crates.io で yank されているのはこの 2 版だけ (全 42 版中)。

**daw_01 では起こり得ない**: `cargo tree -e features -i rusty_ffmpeg` の実測で
`ffmpeg5` / `ffmpeg6` / `ffmpeg7` がすべて有効 (daw_gui → rsmpeg/default → ffmpeg7 → ffmpeg6)。
失敗条件そのものが成立しない。また yank は「新規に解決されなくなる」だけで、
lock 済みの既存ビルドは動く。

**上げ先が無い**: 後継の 0.16.6 / 0.16.7 は `+ffmpeg.8` 系。こちらは 7.1 ABI に固定している
(`daw_gui/ffmpeg/binding_ffmpeg_7.1.rs` と `third_party/ffmpeg` の pin)。7.1 系で yank されて
いない最後は 0.16.4 だが、それは AVChannelLayout 追加より前の版。

**見直しトリガ**: FFmpeg の pin を 8.x 系へ上げるとき
(`docs/ffmpeg_mirror.md` の版更新と同時に rusty_ffmpeg も 0.16.7+ / rsmpeg 0.17.0+ffmpeg.8.1 へ)。

## この調査で分かった検査側の欠陥 (修正済み)

`scripts/dep_licenses.py` は既定 feature の依存グラフしか見ておらず、
**optional dependency のライセンスを取りこぼしていた**。
`assert_no_alloc` (BSD-1-Clause、`daw_audio` の `rt-assert` feature、`make test-rt` で使う) が
許可リスト検査から漏れており、cargo-deny を入れて初めて発覚した。
以後 **許可リストの検査は `--all-features`** で行う (一覧の生成は既定 feature のまま =
実際に配る exe の中身)。BSD-1-Clause は著作権表示の保持と無保証のみの最も弱い BSD で
GPLv3 と互換なので `deny.toml` の allow に追加した。
