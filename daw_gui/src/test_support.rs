//! テスト専用の共通 fixture。 子プロセス無しの `AppData` を組む定型 (`app_tests` /
//! `note_selection` / `save_bundle` / `media` の各 test module が同じ 9 引数を写していた)。

use std::path::Path;
use std::sync::Arc;

use crate::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use crate::state::AppData;

/// 子プロセス無しの `AppData`。 IPC channel の受け側は捨てる (送信は失敗しても
/// 握り潰される) ので、 GUI thread 上のハンドラを直接叩くテストに使う。
/// per-user データフォルダは無し (永続化しない、未保存プロジェクトへは取り込めない)。
pub(crate) fn headless_app() -> AppData {
    build(None)
}

/// [`headless_app`] に per-user データ root `root` (テストの一時フォルダ) を注入したもの。
/// 未保存プロジェクトへの取り込み / Bounce はここの `import_cache` / `bounce_cache` に書く
/// (ユーザーの `%LOCALAPPDATA%\daw_01` には触れない)。
pub(crate) fn headless_app_with_data_root(root: &Path) -> AppData {
    build(Some(common::app_dirs::AppDirs::under(root)))
}

fn build(app_dirs: Option<common::app_dirs::AppDirs>) -> AppData {
    let (audio_tx, _audio_rx) = tokio::sync::mpsc::unbounded_channel();
    let (plugin_tx, _plugin_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        app_dirs,
        common::audio_bridge::DEFAULT_SAMPLE_RATE,
    )
}
