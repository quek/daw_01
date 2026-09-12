//! S3b-1: AppData state group (UiEphemeral)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::path::PathBuf;

use crate::app::{ClipKey, DirtyGuardAction, ExportRangePicker, InspectorScrubField, PluginPickEntry};

/// **いま 1 本の undo step へ束ねている UI ジェスチャの所有者。**
///
/// スクラブ / ドラッグ中の欄は `Song` を毎フレーム書くので、
/// [`SongDoc::begin_gesture`](crate::state::SongDoc::begin_gesture) で束ねないと
/// 1 ドラッグで数十 undo step が積まれ `UNDO_LIMIT` を溢れさせる。
///
/// **開始と終了の「対」に頼らない。** 欄が画面から消えると (選択が変わる /
/// パネルが閉じる / トラックが削除される) 終了側が二度と呼ばれず、
/// **以降の編集が全部 1 undo step に束ねられ続ける**。所有者をこの 1 本で持ち、
/// 「所有者が今フレームも描かれている間だけ生きる」
/// ([`crate::view::scrub_gesture`]) に縛ることで、消えたら必ず閉じる。
///
/// 面ごとに別フィールドを持たないのも同じ理由 —
/// `SongDoc` の bracket は 1 本しか無いので、追跡側が 5 本あると
/// 「A が開けたまま B が閉じる」が黙って作れてしまう。
#[derive(Debug, Clone, PartialEq)]
pub enum ScrubGesture {
    /// インスペクタ (audio / image / text / plugin param / ローンチ) の数値欄。
    Inspector(InspectorScrubField),
    /// アレンジャーのオートメーションレーン見出しの「既定値」欄。
    LaneDefault(common::model::AutomationLaneKey),
    /// ツマミ / フェーダーの **変調深さ** ドラッグ (`docs/plan_modulation.md`)。
    ModDepth { track_id: u32, target: common::model::AutomationTarget },
    /// 変調ラックの数値欄 (ラック内で同時にドラッグできる欄は 1 つなので集約 1 本)。
    ModRack,
    /// 立ち絵グループ変換 (`docs/plan_tachie_group_transform.md`)。
    GroupTransform(common::model::GroupTransformParam),
}

/// r.md #115: 変調ラックでポインタが乗っているもの (`Q` のバイパス対象)。 住所は安定 id。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModRackHover {
    /// モジュレーターのヘッダ行 / 展開した本体。
    Source(u32),
    /// routing 1 行 (`ModRouting::id`)。
    Routing(u32),
}

pub struct UiEphemeral {
    /// プロジェクトが切り替わるたびに +1 される世代印。
    ///
    /// GPU テクスチャ (`video_texture_cache` / `image_texture_cache`)・
    /// preview のフレームテクスチャ・デコード ring は `Renderer` を持つ runner
    /// でしか解放できないので、AppData 側からは「捨てろ」を **この値の変化**
    /// として伝える。runner は自分が最後に見た世代と比較して破棄する。
    pub project_generation: u64,
    /// `docs/plan_project_tabs.md` §5.6: ドラッグ中に Ctrl+Tab / Ctrl+Shift+Tab が押された
    /// ときの切替先。すぐ切り替えずにアレンジ widget に渡し、運んでいるクリップ /
    /// トラックを payload に昇格させてから切り替える (widget が消費して `None` に戻す)。
    pub pending_tab_switch: Option<common::protocol::ProjectKey>,
    /// §5.6: 閉じたタブの retained widget state (arrangement / piano roll) を捨てる予約。
    /// `AppData` は `Ui` を持たないので、root が毎フレーム drain して
    /// `Ui::remove_widget_state` を呼ぶ。
    pub retained_state_to_drop: Vec<common::protocol::ProjectKey>,
    /// r.md #42: 参照を捨てた **main renderer** の `TextureHandle` の破棄予約。
    ///
    /// `AppData` は `Renderer` を持たない (モデルを GPU に依存させない) ので、
    /// cache を purge する側はここに積むだけにし、runner が毎フレーム drain して
    /// `Renderer::destroy_texture` を呼ぶ。積み忘れると GPU 側 store に entry が
    /// 残り続け、プロジェクトを開き直すたびに VRAM が単調増加する
    /// (サムネイルはネイティブ解像度なので 4K なら 1 枚 33MB)。
    pub pending_texture_destroys: Vec<daw_ui_renderer::TextureHandle>,
    /// r.md #75: 設定 window の「合成の塊の長さ」を drag / text 編集中か
    /// (確定 = 立ち下がりの edge 検出用)。session-only。
    ///
    /// この値の確定は **曲全体の再合成 + app_config.json への書き込み**を意味するので、
    /// drag の per-frame 値では確定させない (掴んで振っている間ずっと engine を叩き、
    /// 毎フレーム設定ファイルを書くことになる)。マスターフェーダーの undo bracket
    /// (`master_gain_dragging`) と同じ edge 検出の流儀。
    pub voicevox_chunk_editing: bool,
    /// Global Sampler の「長さ (秒)」欄をドラッグ / 入力中か (立ち下がりで確定)。
    pub sampler_secs_editing: bool,
    /// view 層が OS clipboard へ書く保留テキスト。トラック copy/cut は
    /// plugin state 収集が非同期 (`on_all_states_from_child`、Ui 非保持) なので、
    /// そこで serialize した envelope JSON をここに積み、`dispatch_shortcuts` が
    /// 毎フレーム drain して `Ui::set_clipboard_text` する。
    pub pending_clipboard_write: Option<String>,
    /// 詳細パネルが開いているか (session-only、 Esc / 再クリックで閉じる)。
    pub resource_panel_open: bool,
    /// r.md #48: 設定画面に出すテーマ一覧のキャッシュ (session-only)。
    ///
    /// **毎フレーム作り直してはいけない** — 実体は `themes/` の `read_dir` +
    /// 各ファイルの JSON パースで、描画ループでディスク I/O を回すことになる。
    /// 設定 window を **開いたとき**に 1 回だけ更新する (= 開き直せば新しく置いた
    /// テーマファイルが出る。再起動は不要)。
    pub available_themes: Vec<crate::theme::Theme>,
    /// 履歴パネルが最後に auto-scroll で追従した履歴 index。 現在位置
    /// ([`crate::state::SongDoc::history_current`]) がこれと変わったフレームだけ
    /// current 行が見えるよう scroll offset を合わせ、 手動 scroll は妨げない。
    pub undo_history_follow_pos: usize,
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    pub plugin_picker_visible: Vec<PluginPickEntry>,
    /// プラグインピッカーの検索ボックスに入力中の絞り込みクエリ。
    /// 1 文字毎に [`AppEvent::SetPluginPickerQuery`] で更新し、
    /// [`AppData::refresh_picker_visible`] で subsequence マッチに使う。
    pub plugin_picker_query: String,
    pub is_plugin_picker_open: bool,
    /// r.md #110: picker を開いた `+ Plugin` の chain (挿入先)。 `None` = cursor track の
    /// top-level 末尾。
    pub plugin_picker_target: Option<common::model::ChainRef>,
    /// 検索結果リスト ([`plugin_picker_visible`]) 内のカーソル位置 (0-based)。
    /// `text_input` focus 中の ↑↓ (gui_01 #057 / Phase 86 `TextInputResponse::nav_up/nav_down`)
    /// で [`AppEvent::MovePluginPickerCursor`] を発火して移動し、 Enter で
    /// `plugin_picker_visible.get(cursor)` を確定する。 `refresh_picker_visible` が
    /// 呼ばれる度 (絞り込み再計算 / モーダル open / rescan 完了) に 0 にリセット。
    pub plugin_picker_cursor: usize,

    // -------- Font picker (Text クリップのフォント選択) ----------
    /// `available_font_families()` で列挙したシステムフォント名 (キャッシュ)。
    /// 初回 open 時に background thread で 1 度だけ読む (~20-860ms)。
    pub font_picker_families: Vec<String>,
    /// 検索 + デフォルト行で絞り込んだ表示用リスト。先頭 `""` = renderer
    /// default (=「デフォルト」行)。
    pub font_picker_visible: Vec<String>,
    pub font_picker_query: String,
    pub font_picker_cursor: usize,
    pub is_font_picker_open: bool,
    /// background のフォント列挙が走行中。
    pub font_picker_loading: bool,
    /// 編集対象の text クリップ (open 時に anchor から確定)。
    pub font_picker_target: Option<ClipKey>,
    /// open 時の元フォント。cancel / commit の undo 復元元。
    pub font_picker_restore: String,

    /// スピナー回転位相の基準時刻 (construction で固定、単調増加)。
    pub anim_epoch: std::time::Instant,
    /// 現フレームの時刻。`render_frame` 冒頭で 1 度設定し、その frame の
    /// overlay / clip スピナー / engine 未接続判定がすべて**同じ時刻**を読むことで、
    /// 「スピナー描画」と「再描画を続けるか (`voicevox_animating`)」の判定が 5s 境界で
    /// 食い違わないようにする (= 警告へ切り替わる frame を確実に 1 枚描く)。
    pub frame_now: std::time::Instant,
    pub status_message: String,
    /// r.md #71 (プラグインのコピー / 移動): view から **解決済みの shortcut 名** を
    /// 次フレームへ注入する queue。 runner の frame loop が drain して
    /// `UiHost::inject_shortcut` に流す。
    ///
    /// 右クリックメニューの「貼り付け」のように、 キー入力でないのに
    /// 「Ctrl+V と完全に同じ経路」 を起こしたい場面のための seam。 view から
    /// OS クリップボードを別途読むと paste の経路が 2 本になるので、
    /// **shortcut レイヤという一段上の高さ**に注入する (r.md #36 の
    /// `EditorKey` 転送と同じ idiom)。
    pub pending_shortcut_injections: Vec<&'static str>,







    // ---- Audio event 数値 field 編集 buffer (Phase 2 PR2) ---------------

    /// Export WAV / Video のレンジピッカーモーダルの状態。 `Some` の
    /// 間だけ `export_range_modal` を描画してレンジ確定を待つ。 確定後は元の
    /// export action (file dialog) を `kind` に応じて起動する。 `None` = 非表示。
    pub export_range_picker: Option<ExportRangePicker>,

    /// 起動時 recovery_dir scan + Open 時 sidecar 検出で蓄積される復元候補。
    /// `recovery_modal` が空でない間 modal を出す。
    pub recovery_candidates: Vec<PathBuf>,
    /// `recovery_candidates` を modal に出すかどうか (Dismiss で false)。
    pub show_recovery_modal: bool,
    /// 未保存変更がある状態で「タブを破棄する操作」 (= 終了 / タブを閉じる) を
    /// 行おうとしたとき表示する確認モーダル (`dirty_guard_modal`)。 `Some(action)` の間
    /// モーダルが開き、 「保存」「保存しない」「キャンセル」 を選ばせる。
    /// **モーダルは常にアクティブなタブについて出る** ので、呼び出し側は対象タブへ
    /// 切り替えてから立てる (`docs/plan_project_tabs.md` §5.2)。
    pub dirty_guard: Option<DirtyGuardAction>,
    /// ガードモーダルで「保存して続行」 を選んだが plugin state 取得待ちで
    /// save が非同期 (`PendingStateRequest::Save`) になっている間
    /// `Some(action)`。 `on_all_states_from_child` で save が完了
    /// (is_dirty=false) したら `action` を実行する。 save 試行が終われば
    /// (= pending Save が消えれば) クリアする (後続の手動 save が誤って
    /// action を実行しないように)。
    pub guard_after_save: Option<DirtyGuardAction>,

    /// Windows: main window の `HWND` (`with_owner_window` と同じ isize 表現)。
    /// runner が window 生成直後にセットする。 native file save dialog を
    /// background thread で **owner-modal** に開くための parent handle に使う
    /// (`action_open_export_mp4_dialog`)。 None = まだ window 未生成 / 非対応。
    #[cfg(windows)]
    pub main_window_hwnd: Option<isize>,
    /// video export の保存先選択 dialog (background thread) が開いている間 true。
    /// 二重起動防止。 `FileDialogResult { kind: ExportMp4 }` 受信でクリアする。
    pub export_dialog_open: bool,
    /// Save As dialog (background thread) が開いている間 true。 二重起動防止に加え、
    /// ガードの「保存して続行」 が新規 project で Save As を非同期に開いたとき、
    /// dialog 解決後 (`SaveAsResolved`) の begin_save 完了で action を実行するよう
    /// `guard_after_save` を立てる判定に使う。 `SaveAsResolved` 受信でクリアする。
    pub save_as_dialog_open: bool,
}

