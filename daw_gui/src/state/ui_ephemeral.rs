//! S3b-1: AppData state group (UiEphemeral)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::path::PathBuf;

use crate::app::{
    ArrLabelCache, ArrangeViewSnapshot, ArrangeZoomAnchor, AutomationPointKeyRef, ClipKey,
    ColorPickerTarget, DirtyGuardAction, ExportRangePicker, InspectorScrubField, PluginPickEntry,
    SendPickerState, TempoMapCache, TouchedParam,
};

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

pub struct UiEphemeral {
    /// D3/D4: track/clip 名の `Arc<str>` キャッシュ ([`ArrLabelCache`])。 view から
    /// (`&self`) 更新するので `RefCell`。 AppData は GUI メインスレッド専有なので可。
    pub(crate) arr_label_cache: std::cell::RefCell<ArrLabelCache>,
    /// r.md #56: 秒表示用 `TempoMap` の世代キャッシュ ([`TempoMapCache`])。
    /// `arr_label_cache` と同じく view から (`&self`) 更新するので `RefCell`。
    pub(crate) tempo_map_cache: std::cell::RefCell<TempoMapCache>,
    /// GPU-side video thumbnail textures keyed by `VideoSourceId`.
    /// Written by the runner (P3.5) after a successful texture upload;
    /// read by `arrangement_view.rs` (P3.6) and passed to
    /// `ClipView.thumbnail`.
    /// 直近 `AppData::reset_song_scoped_state` が確定した `Song::project_id`
    /// (プロジェクト同一性の SSoT、v24)。同じ project の再読込ではキャッシュを
    /// 捨てないための照合用。`0` = 未確定。
    pub loaded_project_id: u64,
    /// プロジェクトが切り替わるたびに +1 される世代印。
    ///
    /// GPU テクスチャ (`video_texture_cache` / `image_texture_cache`)・
    /// preview のフレームテクスチャ・デコード ring は `Renderer` を持つ runner
    /// でしか解放できないので、AppData 側からは「捨てろ」を **この値の変化**
    /// として伝える。runner は自分が最後に見た世代と比較して破棄する。
    pub project_generation: u64,
    pub video_texture_cache:
        std::collections::HashMap<common::model::VideoSourceId, daw_ui_renderer::TextureHandle>,
    /// v13: GPU-side image textures keyed by `ImageSourceId`.
    ///
    /// **main window の `Renderer` が払い出した handle だけ**を入れる
    /// (arrangement のクリップサムネイル用)。`TextureHandle` は renderer-local な
    /// id 空間なので、preview 側の handle を混ぜると別名衝突する (r.md #42)。
    /// preview の合成が使う画像テクスチャは `PreviewWindowState::image_textures`。
    pub image_texture_cache: std::collections::HashMap<
        common::model::ImageSourceId,
        daw_ui_renderer::TextureHandle,
    >,
    /// r.md #42: 参照を捨てた **main renderer** の `TextureHandle` の破棄予約。
    ///
    /// `AppData` は `Renderer` を持たない (モデルを GPU に依存させない) ので、
    /// cache を purge する側はここに積むだけにし、runner が毎フレーム drain して
    /// `Renderer::destroy_texture` を呼ぶ。積み忘れると GPU 側 store に entry が
    /// 残り続け、プロジェクトを開き直すたびに VRAM が単調増加する
    /// (サムネイルはネイティブ解像度なので 4K なら 1 枚 33MB)。
    pub pending_texture_destroys: Vec<daw_ui_renderer::TextureHandle>,
    /// Snapped mouse hover beat inside the arrangement canvas. `None`
    /// outside the canvas. `arrangement_view::draw` updates it every
    /// frame using the current `SnapConfig`. Used by Split (E) so the
    /// split lands at the user's pointer (REAPER edit-cursor flavour)
    /// instead of the playhead (`docs/plan_audio_clip.md` §3.3).
    pub arrangement_hover_beat: Option<f64>,
    /// Same as above but **without** snap applied. Used by Alt+E
    /// (split with snap temporarily disabled).
    pub arrangement_hover_beat_raw: Option<f64>,
    /// `(track, clip)` index pair for the clip the mouse is currently
    /// over (or `None` outside any clip). Lets Split work without an
    /// explicit selection — hover over a clip, press `E`, and that
    /// clip is split. Falls back to the existing `selected_clips`
    /// when no clip is under the cursor.
    pub arrangement_hover_clip: Option<ClipKey>,
    /// ポインタ下のトラック id (`ArrangementResponse.hovered_track` の
    /// mirror)。トラック paste の挿入先 (= マウス下トラックの直上) に使う。
    /// `arrangement_view::draw` が毎フレーム更新。ヘッダ列・クリップレーンどちらの
    /// 上でも同じトラック行を返す。
    pub arrange_hovered_track: Option<u32>,
    /// ミキサーでポインタ直下の strip の track id。`mixer_strips::draw`
    /// が毎フレーム更新 (arrangement の `arrange_hovered_track` と同 idiom)。S キーで
    /// マウス直下のストリップを solo するために `dispatch_shortcuts` が読む。master
    /// strip は solo を持たないので None 扱い。
    pub mixer_hovered_track: Option<u32>,
    /// マスターフェーダーを掴んでいるか (undo gesture の edge 検出用)。
    /// `Song.master_gain` を編集するようになったので、drag 全体を 1 undo step に
    /// bracket しないと per-frame の編集が履歴を埋める (group transform /
    /// inspector scrub と同じ罠)。session-only。
    pub master_gain_dragging: bool,
    /// r.md #75: 設定 window の「合成の塊の長さ」を drag / text 編集中か
    /// (確定 = 立ち下がりの edge 検出用)。session-only。
    ///
    /// この値の確定は **曲全体の再合成 + app_config.json への書き込み**を意味するので、
    /// drag の per-frame 値では確定させない (掴んで振っている間ずっと engine を叩き、
    /// 毎フレーム設定ファイルを書くことになる)。マスターフェーダーの undo bracket
    /// (`master_gain_dragging`) と同じ edge 検出の流儀。
    pub voicevox_chunk_editing: bool,
    /// ピアノロール grid 上のポインタ拍 (clip-local, snap 済)。
    /// ノート paste の配置位置に使う。`piano_roll` widget が毎フレーム更新、
    /// grid 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_beat: Option<f64>,
    /// ピアノロール grid 上のポインタ拍を **song-absolute かつ snap なし**
    /// (= clip_start_beat を引く前の生 beat) で mirror。`f` キー (PlayFromCursor) は
    /// song-absolute の grid で snap する必要があるため、`pianoroll_hover_beat`
    /// (clip-local snap 済) とは別に保持する。grid 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_beat_song_raw: Option<f64>,
    /// ピアノロール grid 上のポインタ直下の note index (= clip 内 notes Vec の
    /// index、`selected_notes` と同空間)。`q` キーで「選択が無ければカーソル直下 note を
    /// mute」する対象解決に使う。`piano_roll` widget が `note_hit` で毎フレーム更新、
    /// grid 外 / note 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_note: Option<u32>,
    /// view 層が OS clipboard へ書く保留テキスト。トラック copy/cut は
    /// plugin state 収集が非同期 (`on_all_states_from_child`、Ui 非保持) なので、
    /// そこで serialize した envelope JSON をここに積み、`dispatch_shortcuts` が
    /// 毎フレーム drain して `Ui::set_clipboard_text` する。
    pub pending_clipboard_write: Option<String>,
    /// inline 数値入力中の automation point (`Some` のとき
    /// `arrangement_view` が当該 point の rect に `text_input_at_focused`
    /// overlay を出す)。session-only / Undo・save 対象外。点をダブルクリック
    /// で `Some`、 確定 (Enter) / blur / Esc で `None`。
    pub editing_automation_point: Option<AutomationPointKeyRef>,
    /// gui_01 #028 §7.3: 最後にユーザーが触った parameter。`A` キー
    /// shortcut で「対応 lane を所有 track に追加」 する source。
    /// session-only (起動 None、Undo / save 対象外)。
    pub last_touched_param: Option<TouchedParam>,
    /// r.md #10: `Home` の 2 段トグル state。 直前の `Home` が「先頭 (時間的に
    /// 最初) のクリップの頭」 へ飛んだなら `true`。 次の `Home` はこれを見て
    /// 1.1.1 (beat 0) へ戻す。 **live playhead 位置で判定しない**理由: 再生中は
    /// playhead が毎フレーム進むので位置比較では 2 度押しが成立しない
    /// (レビュー指摘)。 明示 seek (`seek_playhead_to`) / 停止で false にリセット
    /// され、 再生中の playhead poll では触らないので、 再生中でも確実にトグルする。
    pub(crate) home_toggle_at_first: bool,
    /// gui_01 #068: 前フレームに arrangement でホバーされた clip の
    /// `content_id` (= 連動ハイライトの active group 計算に使う held-value)。
    /// widget の `ArrangementResponse.hovered_clip` を毎フレーム解決して
    /// 保持する。 session-only。
    pub arrange_hover_content: Option<common::model::ContentId>,
    /// gui_01 #090: ポインタが今乗っている automation lane の key
    /// (`ArrangementResponse.hovered_automation_lane` を毎フレーム mirror)。
    /// Ctrl+A の「lane の全ポイント選択 → 全クリップへ段階拡大」振り分けに
    /// 使う。 `None` = clip 領域 / lane 外。 1 フレーム遅延だが pointer は
    /// 瞬間移動しないので実用上問題なし (= `arrange_hover_content` と同 idiom)。
    pub arrange_hovered_automation_lane: Option<common::model::AutomationLaneKey>,
    /// arrangement ヘッダのトラック音量スライダを drag 中のトラック id
    /// (`ArrangementResponse.dragging_track_volume` を前フレーム値として mirror)。
    /// None↔Some の edge で `ParamGestureBegin`/`End` を発火し、 mixer フェーダーと
    /// 同じ「1 drag = 1 undo step」 経路 (gesture begin で 1 snapshot) に乗せる。
    /// session-only (`arrange_hover_content` と同 idiom)。
    pub arrange_dragging_track_volume: Option<u32>,
    /// piano_roll widget が歌詞 inline 編集 (gui_01 #017、 note 上の L キー編集) の
    /// text_input overlay を出している間 `true`。 widget 内部状態 (`PianoRollState`) の
    /// session-only ミラーで、 `piano_roll` widget が毎フレーム `resp.lyric_editing`
    /// から更新する (project save には含めない)。
    ///
    /// 用途は root.rs の Esc dispatch との調停。 `dispatch_shortcuts` は
    /// piano_roll widget より前に走って `take_shortcut("escape")` を消費するため、
    /// このミラーが立っている間は Esc を消費せず widget に委ねる (widget 側が歌詞編集を
    /// キャンセルする)。 ミラーは 1 frame 遅延だが、 編集モードは L 押下〜Esc 押下まで
    /// 複数フレーム持続するので調停に支障はない。
    pub piano_roll_lyric_editing: bool,
    /// r.md #67: ピアノロールの note grid の表示範囲 `(拍数, 半音数)`。
    /// `piano_roll` widget が毎フレーム mirror する session-only 値 (project save に含めない)。
    ///
    /// 表示範囲は grid の px サイズ ÷ zoom で決まるので **view しか知らない**。 一方、
    /// カーソルキーで動かしたノートを画面内に追う処理は handler 側にある
    /// (`AppData::nudge_selected_notes_*`)。 そのために「今どれだけ見えているか」 だけを
    /// mirror する。 `None` = ピアノロールがまだ 1 度も描かれていない (追従しない)。
    pub pianoroll_viewport: Option<(f64, f32)>,
    /// `Some(target)` で Audio Editor (= clip ダブルクリックで開く波形
    /// 編集 view) が開いている。 bottom_panel の Piano Roll タブが
    /// audio_editor view に切り替わる (`docs/plan_audio_clip.md` §3.10
    /// 「piano_roll の領域を流用」)。 `None` なら通常の Piano Roll が
    /// 表示される。 audio clip ダブルクリックで `Some` 化、 Esc / Audio
    /// Editor close で `None` に戻る。
    pub audio_editor_clip: Option<ClipKey>,
    /// ピアノロールの**対象 (target) クリップ**の focus ヒント。
    ///
    /// 選択そのものではない (選択の SSoT は `SelectionState::time` 1 本) —
    /// 「同時表示している MIDI クリップのうち、どれを編集の主対象にするか」 という
    /// カーソル位置に相当する。 凡例の行クリック (`SetPianoRollTargetClip`) で動き、
    /// 表示集合に居ない値は [`AppData::pianoroll_target_clip`] が無視するので stale で
    /// 壊れない。 `None` なら「範囲の先頭に最も近いクリップ」 が対象になる。
    pub pianoroll_focus_clip: Option<ClipKey>,
    /// Audio Editor 内のマウス hover 位置を clip 内 beat (clip 始端 = 0)
    /// に変換した値。 audio_editor.rs が毎フレーム push、 マウスが
    /// waveform 領域外なら `None`。 E キー (split) と将来の波形クリック
    /// 系操作で「マウス位置を cursor として使う」 ために保持する。
    pub audio_editor_hover_beat_in_clip: Option<f64>,
    /// `Z` キーの段階ズーム履歴。 1 回目 push で横ズーム前の view、
    /// 2 回目 push で縦ズーム前の view を積む。 `X` が pop して 1 段ずつ戻し、
    /// 空になったら全体フィットに落ちる。 load / new / recovery で clear。
    pub(crate) arrange_zoom_history: Vec<ArrangeViewSnapshot>,
    /// `Z` 段階ズームの現在アンカー (直近 Z が適用した選択 + view + 段数)。
    /// 次の Z で選択 or view が食い違えば段階 0 (横) から仕切り直す。 session-only。
    pub(crate) arrange_zoom_anchor: Option<ArrangeZoomAnchor>,
    /// `Z` の縦ズームがレーンを viewport 高いっぱいへ広げた**その 1 行**。
    ///
    /// 拡大は `ui_prefs.automation_lane_row_overrides` に書くが、あの map には
    /// **fit (`X`) が縮めた行高**も同居する。 どちらが書いたかを持たずに map ごと
    /// 捨てると、fit で縮めたレーンが次の `Z` で model 高さへ跳ね上がる
    /// (実機報告「1 回目の Z でオートメーションレーンが高くなる」)。 誰が書いたかは
    /// ここが持つ。 session-only。
    pub(crate) zoom_lane_fill: Option<common::model::AutomationLaneKey>,
    /// inspector の param セクション (title 下〜chain 上) の実描画高さ
    /// (px)。 immediate-mode なので「前フレームに測った高さ」を `scroll_area` の
    /// content_size として使う (= lag-by-one)。 描画末尾で実測値に更新。
    /// session-only (save / Undo 対象外)。
    pub inspector_body_h: f32,
    /// チェーン行アコーディオンで開いているデバイスの param パネル実高さ
    /// (px、 前フレーム測定値)。 `reorderable_list_expandable` の `row_extra_h` に渡して
    /// 開いた行の直下に確保する展開高に使う (lag-by-one、 `inspector_body_h` と同 idiom)。
    /// session-only。
    pub inspector_device_panel_h: f32,
    /// auto-fit (`X` キー / `Fit` ボタン / SelectClip 経由) で参照する piano_roll
    /// grid 領域サイズ (px)。`view::root` / `view::bottom_panel` が piano_roll タブ
    /// 描画時に毎フレーム書き込む。0 は「未測定」フラグ扱い (auto-fit を skip)。
    pub last_pianoroll_grid_size: (f32, f32),
    /// piano_roll がまだ一度も描画されていない (= `last_pianoroll_grid_size` 未測定)
    /// 状態で auto-fit が要求された場合に立つフラグ。 初回描画で grid_size が
    /// 確定したフレームの Edit 内で消費 → `fit_piano_roll_to_clip` を再実行する。
    /// これが無いと「Piano Roll タブ未表示で clip を選択 → タブを開いても fit
    /// されない、 2 回目以降のみ fit」 という初回 fit 喪失バグになる。
    pub pending_pianoroll_fit: bool,
    /// r.md #63: arrangement widget が分割した **`lanes` Rect の実寸** (px)。
    /// ruler と Arranger (section) 帯を除いた、 track 行が実際に描かれ scissor される領域。
    /// `last_pianoroll_grid_size` と同じ「レイアウト SSoT が返した実 rect を記録する」 idiom で、
    /// widget が毎フレーム書き込む。 `0.0` は「未測定」 (auto-fit を skip)。
    ///
    /// **式で再導出しないこと**。 以前は widget 側が `area.h - RULER_H` と独立に計算しており、
    /// Arranger 帯 18px を引き忘れて `X` の全体表示が常に下へはみ出していた。
    pub last_arrange_lanes_size: (f32, f32),
    /// r.md #63: arrangement widget がこのフレームに縦へ積んだ行の一覧 (描画順、 culling 前)。
    /// `X` の全体表示 / `Z` の縦ズームが行数・行高・行の content-Y を引く唯一の根拠
    /// (= 可視 track / 展開 lane の集合をモデルから再導出しない)。 widget 未描画なら空。
    pub last_arrange_rows: Vec<crate::widgets::arrangement::ArrangementRow>,
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

    /// 「＋ Send」 ボタンで開く宛先トラックピッカーの状態。 `Some` の間
    /// modal が開いており、 宛先選択 or 閉じる操作で `None` に戻る。
    /// plugin picker の `is_plugin_picker_open` と同 idiom。
    pub send_picker: Option<SendPickerState>,
    /// 内蔵映像 FX は plugin window を持たないので、チェーン行の "GUI"
    /// ボタンはインスペクタ内のパラメータ調整パネルを開く。`Some(device_id)`
    /// で 1 つだけ開く（別の FX の GUI を押すと切り替わる）。
    ///
    /// r.md #71 (プラグインのコピー / 移動): cursor track 以外の device を指して
    /// いたら **閉じるのではなく描画側 gate が非表示にする**。 こうすると device を
    /// 別トラックへ移してもパネルが自然に追従する。 `None` に落とすのは device が
    /// song から消えたときだけ。
    pub open_video_fx_params: Option<u64>,
    /// 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し
    /// CLAP・VST3) の「⚙」ボタンで開くインライン param パネル。
    /// `open_video_fx_params` と同 idiom。
    pub open_plugin_params: Option<u64>,
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

    /// rename 中の track の **安定 ID** (positional index ではない)。 index で持つと
    /// track の reorder / delete で別 track に rename がすり替わる SSoT 違反になる
    /// (2026-06-09 の「最上段だけ rename できない / フリーズ」バグの原因)。 None で非 rename。
    pub track_rename_id: Option<u32>,
    pub track_rename_text: String,

    /// 編集中の clip rename。 `Some` のとき該当 clip rect に inline
    /// text_input を重ね描きする (track rename の clip 版)。 `ClipKey` は
    /// 安定 id なので rename 中に並べ替えても対象は動かない。
    pub clip_rename: Option<ClipKey>,
    pub clip_rename_text: String,

    /// v18 (`docs/plan_track_clip_color.md`): color_picker (gui_01 #058) の
    /// 開いている編集対象。`None` で非表示。`open_color_picker` で `Some` に、
    /// picker の `dismissed` で `None` に戻す。
    pub color_picker_target: Option<ColorPickerTarget>,
    /// color_picker overlay の anchor 矩形 (popup を出す基準位置)。開いた場所
    /// (右クリックした header / clip rect、inspector のスウォッチ rect) を保持し、
    /// どの view から開いても同じ位置に popup が出るようにする。
    pub color_picker_anchor: Option<daw_ui_renderer::Rect>,
    /// color_picker session 中に既に undo snapshot を取ったか。`open_color_picker`
    /// で `false` にリセットし、最初の色変更 (`SetTrackColor`/`SetClipColor`) で
    /// 1 度だけ snapshot を取って `true` にする。これで「drag 開始〜終了」 が
    /// 1 undo step にまとまり、 変更しないまま閉じても dead step が増えない。
    pub color_picker_session_dirty: bool,

    /// gui_01 #071: 空きレーン右クリック (空きレーン右クリック SecondaryClickEmpty)
    /// で開く clip 生成コンテキストメニューの stash。`Some((track_id, snap 済み beat,
    /// 右クリック viewport pos))` の間、毎フレーム `ui.context_menu_at` で `pos` に
    /// メニューを描画する (color_picker overlay と同 idiom)。on_select (= Text クリップ
    /// 生成) で `None` に戻す。
    pub clip_create_menu: Option<(u32, f64, (f32, f32))>,
    /// 上記メニューの 1-shot open trigger。`SecondaryClickEmpty` 受信 Edit で `true` に
    /// し、overlay が `open_at = Some(pos)` を 1 フレームだけ渡したら `false` に戻す
    /// (毎フレーム `Some` を渡すと outside-click で閉じても翌フレーム再 open するため)。
    pub clip_create_menu_open: bool,

    /// Arranger セクション帯の右クリックメニュー stash `(section_id, 右クリック pos)`。
    /// `SecondaryClickSection` 受信で set、 overlay が pos にメニュー (ループ / 帯削除 /
    /// 範囲削除) を描画、 on_select で `None` に戻す (`clip_create_menu` と同 idiom)。
    pub section_menu: Option<(u32, (f32, f32))>,
    /// 上記セクションメニューの 1-shot open trigger (`clip_create_menu_open` と同 idiom)。
    pub section_menu_open: bool,
    /// inline 改名中のセクション id (`track_rename_id` の section 版)。`Some` の間、
    /// arrangement view が該当帯 rect に text_input を重ねる。
    pub section_rename_id: Option<u32>,
    /// 上記改名の編集中文字列。
    pub section_rename_text: String,

    /// Transport BPM 入力欄の編集中文字列。 commit (Enter) で parse + clamp +
    /// `song.bpm` に反映、 song を切り替える際 (open / new / undo / redo) は
    /// `resync_song_edit_texts` で formatted な現値に書き戻す。
    pub bpm_edit_text: String,
    /// Transport time_sig numerator 入力欄の編集中文字列。 同上。
    pub time_sig_num_edit_text: String,

    // ---- Audio event 数値 field 編集 buffer (Phase 2 PR2) ---------------
    /// 現 buffer がどの clip 用にロードされているか。 `selected_clip` が
    /// 変わったら view 側が `AppEvent::ResyncClipEditBuffers(target)` を
    /// 発火して `resync_clip_audio_event_edit_buffers` で再生成。 `None`
    /// は「未ロード」 (= 起動直後 / clip 未選択)。 編集 buffer の中身が
    /// この target の現値と整合する保証はないが (= ユーザー入力中はズレる)、
    /// commit / resync で必ず書き戻す。
    /// audio / image inspector の数値 field は scrubable_number
    /// (drag + type) 化されたため、 個別の名前付き edit-buffer
    /// (`clip_gain_db_edit_text` 等 / `clip_image_*_edit_text`) は撤去
    /// (scrubable が編集状態を自前で内包)。 `clip_edit_buffer_target` は
    /// content / font_family 文字列 buffer の resync 判定に引き続き使う。
    pub clip_edit_buffer_target: Option<ClipKey>,

    /// **いま開いている undo bracket の所有者** (`None` = idle)。
    ///
    /// インスペクタ / レーン既定値 / 変調深さ / 変調ラック / グループ変換の
    /// スクラブは、どれも `Song` を毎フレーム書くので 1 本の gesture に束ねる。
    /// 追跡側を面ごとに分けていた頃は、`SongDoc` の bracket が 1 本しか無いのに
    /// tracker が 5 本あり「A が開けたまま B が閉じる」が黙って作れた。
    /// 出入りは [`crate::view::scrub_gesture`] だけを通す。 session-only。
    pub scrub_gesture: Option<ScrubGesture>,
    /// [`Self::scrub_gesture`] の所有者が **今フレームも自分を描いたか**。
    ///
    /// 毎フレーム末に [`crate::view::scrub_gesture::sweep`] が見て、描かれて
    /// いなければ gesture を閉じる。開始と終了の対に頼ると、欄が画面から消えた
    /// (選択が変わる / パネルが閉じる / トラックが削除される) ときに終了が
    /// 来ず、**以降の編集が全部 1 undo step へ束ねられ続ける**。
    pub scrub_gesture_seen: bool,
    /// docs/plan_modulation.md §3: true while an envelope-follower attack /
    /// release scrub is being dragged. The scrub mutates the value + marks
    /// dirty each frame but defers the (recompiling) `flush_song_sync`
    /// to the drag-end edge, avoiding a per-frame LoadSong storm.
    ///
    /// **undo bracket のエッジ検出には使わない** — それは
    /// [`Self::scrub_gesture`] ([`ScrubGesture::ModRack`]) が持つ。ここは
    /// `SetModFollowerScrubbing` が記録する事実だけ。
    pub mod_follower_scrub_active: bool,
    /// docs/plan_modulation_routing_redesign.md §6: the `ModSource` currently
    /// **armed** for assignment (Bitwig 流). `Some(id)` ⇒ every modulatable
    /// inspector param control shows depth-drag edit mode (`scrubable_number_at`
    /// の `Modulation::edit`); dragging a control assigns / sets that source's
    /// depth on the control's target. `None` ⇒ controls show existing routings
    /// (entries + live tick) but aren't editable. session-only (not persisted).
    pub armed_mod_source: Option<u32>,
    /// the set of `ModSource`s whose inspector row is **expanded** to its
    /// full Bitwig 風グラフィカルエディタ (MSEG curve canvas / Steps grid / LFO·Random
    /// preview + 全コントロール). **Multi-expand** — 複数同時に開ける (Bitwig 同様)。
    /// 行頭の disclosure (r.md #74 で `view::disclosure` へ一本化。 rack 行は縦積みで
    /// 中身が下に開くので開示軸 Block = 折り畳み中 ▶ / 展開中 ▼) クリックで toggle。
    /// session-only (not persisted)。
    pub expanded_mod_sources: std::collections::HashSet<u32>,
    /// Export WAV / Video のレンジピッカーモーダルの状態。 `Some` の
    /// 間だけ `export_range_modal` を描画してレンジ確定を待つ。 確定後は元の
    /// export action (file dialog) を `kind` に応じて起動する。 `None` = 非表示。
    pub export_range_picker: Option<ExportRangePicker>,

    /// docs/plan_text_overlay.md §4 P5: text inspector の文字列 edit buffer。
    /// `text` / `font_family` は文字列 field なので text_input のまま
    /// standalone (= scrubable 化されない)。 Enter / focus 喪失で
    /// `CommitClipText{Content,FontFamily}Edit` を発火。
    /// 25 numeric field は scrubable_number 化され、 `clip_text_num_edits`
    /// HashMap は撤去 (scrubable が編集状態を自前で内包)。
    pub clip_text_content_edit_text: String,
    pub clip_text_font_family_edit_text: String,
    /// 起動時 recovery_dir scan + Open 時 sidecar 検出で蓄積される復元候補。
    /// `recovery_modal` が空でない間 modal を出す。
    pub recovery_candidates: Vec<PathBuf>,
    /// `recovery_candidates` を modal に出すかどうか (Dismiss で false)。
    pub show_recovery_modal: bool,
    /// 未保存変更がある状態で「現在のプロジェクトを破棄する操作」 (= 終了 /
    /// New / Open / Open Recent) を行おうとしたとき表示する確認モーダル
    /// (`dirty_guard_modal`)。 `Some(action)` の間モーダルが開き、 「保存」
    /// 「保存しない」「キャンセル」 を選ばせる。 保存 / 破棄が済んでから
    /// `action` を実行する。 `request_guarded_action` で is_dirty なら立てる。
    /// 旧 `show_close_confirm` (bool, 終了専用) を一般化した。
    pub dirty_guard: Option<DirtyGuardAction>,
    /// ガードモーダルで「保存して続行」 を選んだが plugin state 取得待ちで
    /// save が非同期 (`PendingStateRequest::Save`) になっている間
    /// `Some(action)`。 `on_all_states_from_child` で save が完了
    /// (is_dirty=false) したら `action` を実行する。 save 試行が終われば
    /// (= pending Save が消えれば) クリアする (後続の手動 save が誤って
    /// action を実行しないように)。 旧 `quit_after_save` を一般化。
    pub guard_after_save: Option<DirtyGuardAction>,
    /// plugin-state round-trip (`pending_state_queue` の Save /
    /// Deferred edit / Copy) が in-flight の間にガード操作 (New / Open /
    /// Open Recent / 終了) が要求されたとき、 queue が drain するまで保留する操作。
    /// round-trip 完了処理は `self.song` を変更し得る (Deferred edit は track 削除等、
    /// Save 完了は dirty を下ろす) ので、 その最中に破壊操作を走らせると保存待ちの
    /// 編集が別 project に誤適用される / clean 判定が陳腐化する。 queue 完了時に
    /// `recompute_dirty` してから **再評価** する (= clean なら実行、 dirty なら確認)。
    pub guard_pending_action: Option<DirtyGuardAction>,

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

impl UiEphemeral {
    /// inline リネームの入力欄が出ているか (トラック名 / セクション名 / クリップ名)。
    ///
    /// **rename 状態を足したらここへ足す。** `AppData::edit_surface` の手順 0 は
    /// この述語で「リネーム中はどの面も対象にしない」を決めており、漏らすと
    /// 入力欄が画面外へ出た瞬間に typing lock が外れて Delete が編集面へ抜ける
    /// (r.md #87 の列見出し rename は `LauncherUiState` 側にあるので、面の
    /// arbiter がその 1 つを別に足している)。
    #[must_use]
    pub fn inline_rename_active(&self) -> bool {
        self.track_rename_id.is_some()
            || self.section_rename_id.is_some()
            || self.clip_rename.is_some()
    }
}
