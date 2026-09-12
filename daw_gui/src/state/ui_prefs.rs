//! S3b-1: AppData state group (UiPrefs)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

#[derive(Debug)]
pub struct UiPrefs {
    /// docs/plan_video.md P4: video preview window の表示フラグ。 menu
    /// "View → Video Preview" / shortcut で toggle、 runner が毎フレーム
    /// この値を見て第二 winit::Window を create / destroy する。 false で
    /// 起動するので video import 前は preview は出ない (= MV 開始時は
    /// preview 不要、 user が import 後に明示的に開く)。
    pub preview_window_visible: bool,

    // -------- View state --------

    // -------- Grid snap state --------
    /// status bar の常駐メーター表示 on/off (app_config.json で永続化)。
    pub resource_monitor_enabled: bool,

    /// r.md #29: 編集履歴 window が開いているか (app_config で永続、 再起動を跨いで
    /// 復元)。 View メニュー / Ctrl+Alt+Z / Esc / ✕ で toggle。
    pub undo_history_open: bool,
    /// r.md #29: 編集履歴 window の位置・サイズ (app_config で永続)。 `None` =
    /// 未配置 (初回は既定の右上)。 drag / resize 確定時に更新して保存する。
    pub undo_history_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #54: ラウドネスレポート window が開いているか (app_config で永続)。
    /// 解析メニュー / `Ctrl+L` / Esc / ✕ で toggle。
    pub loudness_report_open: bool,
    /// r.md #54: ラウドネスレポート window の位置・サイズ (app_config で永続)。
    pub loudness_report_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #48: 設定 window が開いているか (app_config で永続)。
    /// Edit メニュー「設定...」 / Esc / ✕ で toggle。
    pub settings_open: bool,
    /// r.md #48: 設定 window の位置・サイズ (app_config で永続)。 `None` = 未配置。
    pub settings_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #50: 画面右端のマスターパネルを出すか (app_config で永続)。
    /// View メニュー / `Ctrl+Alt+M` で toggle。
    pub master_panel_open: bool,
    /// マスターパネルの幅 (px、app_config で永続)。左端ドラッグで変わる。
    pub master_panel_w: f32,
    /// マスターパネルのセクション高さ配分 (MASTER / スペクトラム / オシロ / ゴニオ)。
    /// 合計 1.0 に正規化されている。境界ドラッグで変わる。
    pub master_panel_sections: [f32; 4],
    /// 各メーターの設定 (右クリックメニューで変える、app_config で永続)。
    /// テレメトリスレッドの解析器へは `AppData::meter_control` 経由で渡る。
    pub meter_settings: crate::master_meter::settings::MeterSettings,

    /// r.md #75: VOICEVOX 歌唱合成の「塊」(= `/sing_frame_audio_query` 1 回) の長さ (秒)。
    /// 曲の内容ではなく **合成品質のつまみ**なのでプロジェクトではなく app_config に持つ。
    /// 読むときは `AppData::voicevox_chunk_secs()` (有効範囲へクランプ) を通す。
    pub voicevox_chunk_secs: f32,
    /// Global Sampler / MIDI Capture が溜める長さ (秒)。SSoT は `app_config.json`。
    pub sampler_seconds: u32,

    /// r.md #113: 仮想鍵盤ウィンドウの位置 (app_config で永続)。 `None` = 未配置
    /// (初回は画面中央下)。 サイズは固定なので rect の w / h は描画側が決める。
    pub virtual_keyboard_rect: Option<daw_ui_renderer::Rect>,
    /// r.md #113: 仮想鍵盤の下段 `Z` のピッチ (C 揃え、 app_config で永続)。
    pub virtual_keyboard_base_pitch: u8,
    /// r.md #113: 仮想鍵盤の打鍵ベロシティ `1..=127` (app_config で永続)。
    pub virtual_keyboard_velocity: u8,

    pub is_help_open: bool,

    /// r.md #60: ヘルプ > バージョン情報 (About) が開いているか。
    /// GPLv3 §0 の Appropriate Legal Notices を表示する画面。 `is_help_open` と同じく
    /// セッション内だけの状態で、 app_config には永続しない (起動のたびに開いても困る)。
    pub is_about_open: bool,

    /// per-user データディレクトリ (recent / recent_saved / recovery /
    /// window_state の永続化先) の **Single Source of Truth**。 production は
    /// `AppDirs::production()` (= `%LOCALAPPDATA%/daw_01/`)、 test は
    /// `AppDirs::under(tempdir)` か `None`。 `None` は「永続化しない」 を
    /// 意味し、 実ユーザー状態を汚染しない (= dispatcher と同じ DI パターン)。
    pub app_dirs: Option<common::app_dirs::AppDirs>,
    /// 「最近開いたファイル」 (= Open ダイアログ / OpenRecent 経由で読み込んだ
    /// .daw)。 File メニュー「Open Recent ►」 に表示。 永続化先は
    /// `app_dirs.recent()` (= `%LOCALAPPDATA%/daw_01/recent.json`)。
    pub recent_files: crate::recent::RecentFiles,
    /// 「最近保存したファイル」 (= Save / Save As で書き込んだ先)。 File
    /// メニュー「Recently Saved ►」 に表示。 永続化先は
    /// `app_dirs.recent_saved()` (= `%LOCALAPPDATA%/daw_01/recent_saved.json`)。
    /// 開いた履歴と分離して「保存先だけ覚えておく」 UX を提供する。
    pub recent_saved: crate::recent::RecentFiles,
    /// `recent_files` の filename だけ抽出したキャッシュ。 gui_01 `menu_bar`
    /// API が label に `&'a str` を要求し、 'a が `Ui` の borrow 寿命
    /// (= `&AppData` の寿命) と一致するため、 label 文字列も AppData 内に
    /// 持っておく必要がある。 frame 内で `&app.recent_files_labels[i]` を
    /// 渡せば lifetime が解決する。 `push_recent` / load 時に更新。
    pub recent_files_labels: Vec<String>,
    /// `recent_saved` の filename キャッシュ。 同じ理由。
    pub recent_saved_labels: Vec<String>,



    // ---- クリップランチャー (r.md #87、`docs/plan_rmd_87_clip_launcher.md`) ----
    // どれも「見方の都合」なので dirty は立てない (`project_dirty_flag_rule`)。
    // 曲の一部 (列・セル・主導権) は `Song` 側に居る。永続は `ViewState` の
    // 同名 field。
    /// **オートメーションをクリップに追従させるか**
    /// (`docs/plan_range_selection.md` §5)。 Cubase の *Automation Follows Events* /
    /// REAPER の *Move envelope points with media items* に相当し、アレンジャー上部の
    /// Snap toolbar にトグルとして常時出る。既定 ON。
    ///
    /// **効くのは編集だけ** — 範囲のハイライトは常に「ドラッグが実際に横切った行」で、
    /// この設定では変わらない。ON のとき、トラック行に掛かった範囲への Delete / Cut /
    /// Copy / 移動 / Duplicate / `J` が、**閉じているレーンも含めて**そのトラックの
    /// automation に同じ範囲で適用される。オートメーションレーン行を直接選んだ場合は
    /// 設定に関係なくその automation だけが対象。
    pub automation_follows_clips: bool,
}
