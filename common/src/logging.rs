// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// 保持する日次ログファイルの本数 (= 直近何日分を残すか)。 これより古い
/// `<process>.YYYY-MM-DD` は appender が起動時/ローテーション時に削除する。
const LOG_RETAIN_DAYS: usize = 7;

/// 1 プロセス分の tracing を初期化する。
///
/// - **常に** 日次ローテーションのファイル `<log_dir>/<process_name>.YYYY-MM-DD`
///   に出力する (UTC 日付を tracing-appender が付与)。 書き込みは non-blocking
///   (背景スレッド) なので呼び出し側はディスクを待たない。
/// - **debug ビルドのみ** stdout にも出す (`cargo run` + ログ grep の動線を維持)。
///   release では stdout layer は付かない。
///
/// ファイル appender の構築は失敗しうる (read-only FS / ACL / AV ロック / 予約名
/// 等) ので **panic させず** `None` に縮退する: その場合ファイル layer は付かず、
/// debug の stdout layer (あれば) だけが残る。 `rolling::daily()` は内部で
/// `.expect()` するため使わない (release = windows-subsystem = コンソール無し
/// では起動時に無言で死ぬ。 まさに #48 が避けたい挙動)。
///
/// 返り値の [`WorkerGuard`] は **`main` で名前付きローカルに束縛し、 プログラム
/// 全体の生存期間まで保持する**こと (`let _guard = ...`)。 drop でバッファを
/// flush するため、 早期 drop すると異常終了時にログが失われる。 `None` のとき
/// (= ファイルログ不可) はガードも無い。
///
/// `log_dir` は呼び出し側が決める per-user ディレクトリ (appender が必要なら作る)。
#[must_use = "WorkerGuard はプログラム全体で保持しないとログが失われる"]
pub fn init_tracing(process_name: &str, log_dir: &Path) -> Option<WorkerGuard> {
    // panic-free な builder().build() を使う (rolling::daily() は init 失敗で panic)。
    // build() は親ディレクトリが無ければ作る。 失敗は eprintln して None に縮退。
    // `max_log_files` で古い日次ファイルを自動 prune し、 ログディレクトリが
    // 際限なく肥大するのを防ぐ (r.md #16。 tracing_appender は size ベース
    // ローテーションを持たないので「日数」で上限を掛ける — 直近 LOG_RETAIN_DAYS
    // 日分だけ残す)。
    let file = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(process_name)
        .max_log_files(LOG_RETAIN_DAYS)
        .build(log_dir)
        .map_err(|e| eprintln!("init_tracing: cannot open log file in {log_dir:?}: {e}"))
        .ok()
        .map(tracing_appender::non_blocking);

    // ファイル layer は appender 構築に成功したときだけ付ける (`Option<Layer>` の
    // None は no-op)。 ANSI を切ってファイルにエスケープ列を残さない。
    let (file_layer, guard) = match file {
        Some((writer, guard)) => (Some(fmt::layer().with_ansi(false).with_writer(writer)), Some(guard)),
        None => (None, None),
    };

    // RUST_LOG (無ければ "info") を全 layer 共通の global filter として使う。
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // stdout layer は debug のみ。 `Option<Layer>` の None は no-op になる。
    let stdout_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stdout));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    guard
}

/// production 用の一行ヘルパ。 ログ先を [`AppDirs`](crate::app_dirs::AppDirs) の
/// SSoT (`%LOCALAPPDATA%\daw_01\logs\`) から解決し、 解決できない極端な環境では
/// temp 配下へフォールバックして [`init_tracing`] を呼ぶ。 各プロセスの `main`
/// 冒頭で `let _guard = common::logging::init_tracing_for("daw_gui");` のように使う。
#[must_use = "WorkerGuard はプログラム全体で保持しないとログが失われる"]
pub fn init_tracing_for(process_name: &str) -> Option<WorkerGuard> {
    let log_dir = crate::app_dirs::AppDirs::production()
        .map(|d| d.logs_dir())
        .unwrap_or_else(|| std::env::temp_dir().join("daw_01").join("logs"));
    init_tracing(process_name, &log_dir)
}
