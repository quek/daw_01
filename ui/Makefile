## Makefile — gui_01 のビルド / 起動 / 検証ターゲット
##
## cargo の長いオプションを毎回打たずに `make <target>` で起動できるように。
##
## 使い方:
##   make              # help を表示
##   make daw_prototype  # M7 visual prototype demo を起動
##   make verify         # build + test + clippy をまとめて実行 (CI 相当)

.PHONY: help \
        build check test clippy bench verify \
        mixer waveform_validation sample_editor sample_edit_ops \
        piano_roll arrangement automation embedded_host daw_prototype \
        run release-run \
        clean clean-target

# デフォルトターゲット — make を引数なし
.DEFAULT_GOAL := daw_prototype

help:
	@echo "gui_01 Makefile"
	@echo ""
	@echo "ビルド / 検証:"
	@echo "  make build        - cargo build --workspace"
	@echo "  make check        - cargo check --workspace --benches"
	@echo "  make test         - cargo test --workspace"
	@echo "  make clippy       - cargo clippy --workspace --tests -- -D warnings"
	@echo "  make bench        - cargo bench -p daw-ui-core"
	@echo "  make verify       - build + test + clippy を順に実行 (CI 相当)"
	@echo ""
	@echo "example 起動 (debug build):"
	@echo "  make daw_prototype       - M7 visual prototype (menu / tab / split / scroll / meter)"
	@echo "  make mixer               - M3 8ch fader / button / IME"
	@echo "  make waveform_validation - 128 widget LOD ストレステスト"
	@echo "  make sample_editor       - 選択範囲 + カーソル + RmsBars"
	@echo "  make sample_edit_ops     - 波形 trim / fade in / fade out"
	@echo "  make piano_roll          - 100k notes + heavy() cached"
	@echo "  make arrangement         - 500 widgets + heavy() cached"
	@echo "  make automation          - cubic Bezier flatten + 点ドラッグ"
	@echo "  make embedded_host       - OffscreenRenderer で PNG snapshot"
	@echo ""
	@echo "汎用 (任意 bin 名を指定):"
	@echo "  make run NAME=mixer      - cargo run --bin mixer"
	@echo "  make release-run NAME=daw_prototype - cargo run --release --bin daw_prototype"
	@echo ""
	@echo "クリーン:"
	@echo "  make clean        - cargo clean (target/ を全削除)"

# ----- ビルド / 検証 -----

build:
	cargo build --workspace

check:
	cargo check --workspace --benches

test:
	cargo test --workspace

clippy:
	cargo clippy --workspace --tests -- -D warnings

bench:
	cargo bench -p daw-ui-core

verify: build test clippy
	@echo "verify: all green ✅"

# ----- example 起動 (debug build) -----

mixer:
	cargo run --bin mixer

waveform_validation:
	cargo run --bin waveform_validation

sample_editor:
	cargo run --bin sample_editor

sample_edit_ops:
	cargo run --bin sample_edit_ops

piano_roll:
	cargo run --bin piano_roll

arrangement:
	cargo run --bin arrangement

automation:
	cargo run --bin automation

embedded_host:
	cargo run --bin embedded_host

daw_prototype:
	cargo run --bin daw_prototype

# ----- 汎用 (任意 bin 名) -----

run:
ifndef NAME
	@echo "Usage: make run NAME=<bin_name>" && exit 1
else
	cargo run --bin $(NAME)
endif

release-run:
ifndef NAME
	@echo "Usage: make release-run NAME=<bin_name>" && exit 1
else
	cargo run --release --bin $(NAME)
endif

# ----- クリーン -----

clean:
	cargo clean
