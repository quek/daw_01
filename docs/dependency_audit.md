# 依存の脆弱性 triage (`make audit`)

`make audit` が出す指摘を 1 件ずつ一次情報で調べた結果。**同じ調査を二度やらないための記録**。
新しい指摘が出たらここに追記する。運用方針は `CLAUDE.md`「依存の脆弱性 / 供給網攻撃」節、
設定は `deny.toml` の `[advisories]`。

## 現状 (2026-08-22 時点): **RED**。7 件が残っている

`deny.toml` の `ignore` は **空のまま**にしてある。下記のうち 5 件は
`cargo update -p <crate>` (lockfile 1 行の semver 互換更新) で消えるが、
**依存のバージョンを動かす判断はこの作業の範囲外**なので手を付けていない。

| ID | crate | 種別 | daw_01 での露出 | 消し方 |
|---|---|---|---|---|
| RUSTSEC-2026-0190 | anyhow 1.0.102 | unsound | **なし** | `cargo update -p anyhow` (→ 1.0.103+) |
| RUSTSEC-2026-0204 | crossbeam-epoch 0.9.18 | vulnerability | **なし** | `cargo update -p crossbeam-epoch` (→ 0.9.20) |
| RUSTSEC-2026-0258 | h2 0.4.13 | vulnerability | **なし** | `cargo update -p h2` (→ 0.4.18) |
| RUSTSEC-2026-0194 | quick-xml 0.39.2 | vulnerability | **なし** | `cargo update -p wayland-scanner` (→ quick-xml 0.41.0) |
| RUSTSEC-2026-0195 | quick-xml 0.39.2 | vulnerability | **なし** | 同上 |
| (yanked) | rusty_ffmpeg 0.16.5+ffmpeg.7.1 | yanked | — | 上流が取り下げ済み。**要判断** (下記) |
| RUSTSEC-2025-0141 | bincode 2.0.1 | unmaintained | **なし** | **上げ先が無い**。要判断 (下記) |

`unmaintained = "workspace"` にしてあるので、直接依存でない paste 1.0.15
(RUSTSEC-2024-0436) / rustybuzz 0.20.1 (RUSTSEC-2026-0206) / ttf-parser 0.25.1
(RUSTSEC-2026-0192) は報告されない。3 件とも調査済みで **露出なし・patched 版なし**
(rustybuzz だけは resvg 0.47→0.48 で外せる可能性がある)。

## 判断が要る 2 件

### bincode 2.0.1 — 上げ先が存在しない

開発チームが doxxing / harassment を受けて開発を恒久停止した、という informational
advisory。**CVE も CVSS も「こう壊れる」という記述も無い**。crates.io の
**bincode 3.0.0 は墓標リリースで、lib.rs の中身は `compile_error!` 1 行**。
上げるとビルドが即死するので **絶対に上げてはいけない**。

daw_01 での用途は 2 つ。(1) 名前付きパイプ経由の IPC (相手は自分で spawn した子プロセス)、
(2) builtin VOICEVOX の state 復元 — こちらは**プロジェクトファイル由来の任意バイト列が
デコーダに届く**唯一の経路。ただし bincode 2.0.1 は safe Rust の逐次デコーダで長さ claim
ガードを持ち、`state_load` の失敗も握りつぶさず daw_gui へ返す。踏むべき欠陥が無い。

選択肢: (a) `ignore` に理由付きで載せる、(b) `wincode` 等へ移行する
(ワイヤ互換を謳うが `common/build.rs` の `WIRE_SOURCES` fingerprint 機構との整合は未検証)。
**再評価のトリガは時間ではなくイベント**: bincode 2.x に informational でない advisory が
出たとき / Rust edition や serde の破壊的更新でコンパイル不能になったとき。

### rusty_ffmpeg 0.16.5+ffmpeg.7.1 — yanked

vendored FFmpeg の pin (7.1 ABI) と紐づいているので、単独では動かせない。
`docs/ffmpeg_mirror.md` の FFmpeg 版更新と一緒に扱う話。

## この調査で分かった検査側の欠陥 (修正済み)

`scripts/dep_licenses.py` は既定 feature の依存グラフしか見ておらず、
**optional dependency のライセンスを取りこぼしていた**。
`assert_no_alloc` (BSD-1-Clause、`daw_audio` の `rt-assert` feature、`make test-rt` で使う) が
許可リスト検査から漏れており、cargo-deny を入れて初めて発覚した。
以後 **許可リストの検査は `--all-features`** で行う (一覧の生成は既定 feature のまま =
実際に配る exe の中身)。BSD-1-Clause は著作権表示の保持と無保証のみの最も弱い BSD で
GPLv3 と互換なので `deny.toml` の allow に追加した。
