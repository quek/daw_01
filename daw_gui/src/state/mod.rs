//! S3b-1: AppData の state group 分割 (docs/plan_arch_refactor.md §7.5)。
//!
//! AppData は 9 つの group struct の合成になった。 フィールドの帰属は
//! 「undo 対象か / 発火元は何か」 で判断する (§7.5 の表)。 group struct は
//! 純データ (メソッドは AppData 側 / SongDoc のみ振る舞いを持つ)。

pub mod loudness;
pub mod song_doc;
pub mod transport;
pub mod selection;
pub mod ipc;
pub mod voicevox;
pub mod media;
pub mod recording;
pub mod ui_prefs;
pub mod ui_ephemeral;
pub mod activity;
/// r.md #87: クリップランチャーの session-only な UI 状態。
pub mod launcher_ui;

pub use loudness::{LoudnessPhase, LoudnessState};
pub use song_doc::{EditScope, SongDoc, StreamGesture};
pub use activity::ActivityState;
pub use transport::TransportState;
pub use selection::SelectionState;
pub use ipc::{DeviceParamKey, IpcState};
pub use voicevox::VoicevoxState;
pub use media::MediaState;
pub use recording::RecordingState;
pub use ui_prefs::UiPrefs;
pub use ui_ephemeral::{ScrubGesture, UiEphemeral};
pub use launcher_ui::{LauncherFocus, LauncherUiState};

/// GUI プロセスの全アプリ状態 (composition of state groups)。
pub struct AppData {
    /// Song 文書 + undo/redo + dirty/epoch (編集は `SongDoc::edit` 経由のみ)。
    pub song_doc: SongDoc,
    /// 再生 / metering / export 進行。
    pub transport: TransportState,
    /// 選択集合 (clip / note / automation / track / section) + last-wins tier。
    pub selection: SelectionState,
    /// 子プロセス IPC (tx / supervisor / plugin bookkeeping / sync cache)。
    pub ipc: IpcState,
    /// VOICEVOX (歌唱/トーク/口パク) 状態。
    pub voicevox: VoicevoxState,
    /// メディア import staging / decode cache。
    pub media: MediaState,
    /// MIDI 録音 / step 入力 / param gesture 録音。
    pub recording: RecordingState,
    /// View 構成 (zoom / scroll / snap / panel / recent)。
    pub ui_prefs: UiPrefs,
    /// 一時 UI 状態 (hover / picker / rename / menu / modal / scrub)。
    pub ui_ephemeral: UiEphemeral,
    /// r.md #87: クリップランチャーの一時状態 (フォーカス / hover / 列名の
    /// 編集中テキスト / MIDI bind 表)。曲の中身は `Song`、見方の都合は
    /// `UiPrefs` なので、ここは **保存しないもの**だけ。
    pub launcher: LauncherUiState,
    /// r.md #49: アプリの窓がアクティブか (省電力判定の材料)。
    pub activity: ActivityState,
    /// r.md #61: 終了シーケンス。全終了経路 (✕ / Alt+F4 / File > 終了 /
    /// Ctrl+Q / smoke test / OS のセッション終了) がここに合流し、子プロセスの
    /// graceful teardown を有界に待ってから `event_loop.exit()` する。
    pub shutdown: crate::shutdown::ShutdownState,
    /// r.md #54: 範囲ラウドネス解析の進行とレポート (session-only)。
    pub loudness: LoudnessState,
    /// r.md #50: テレメトリスレッドの `MasterAnalyzer` へ渡す設定とリセット要求。
    /// UI スレッドが書き、解析スレッドが 1 ティック 1 回読む唯一の口
    /// (逆向きは `AppEvent::MasterMeterTick`)。
    pub meter_control: std::sync::Arc<std::sync::Mutex<crate::master_meter::settings::MeterControl>>,
    /// r.md #48: いま有効なテーマ (汎用パレット + DAW 固有トークン) の **SSoT**。
    /// view は `app.theme.core.<token>` / `app.theme.daw.<token>` で色を読む。
    /// `theme.core` は `UiHost` が持つ実体と同じ `Arc` で、runner が毎フレーム
    /// `UiHost::set_palette` に流し込む (変化していたら描画キャッシュを捨てる)。
    /// 選択中の id は `theme.id` が持つので別フィールドに複製しない。
    pub theme: crate::theme::Theme,
}

impl AppData {
    /// Song 編集の標準口: 現在 dispatch 中 event の ambient scope で
    /// [`SongDoc::edit`] を呼ぶ。 1 event 内の複数呼び出しは 1 undo step に
    /// squash され、 Begin*/End* gesture 中は drag 全体が 1 step になる。
    /// export 中は `None` (編集拒否 + status message 予約)。
    pub fn edit_song<R>(&mut self, f: impl FnOnce(&mut common::model::Song) -> R) -> Option<R> {
        self.sync_export_lock();
        let scope = self.song_doc.event_scope();
        self.song_doc.edit(scope, f)
    }

    /// no-op 検出付き [`AppData::edit_song`] ([`SongDoc::edit_checked`] 参照)。
    /// 戻り値 = 「実際に編集が起きたか」 (export 中拒否は false)。
    pub fn edit_song_checked(
        &mut self,
        f: impl FnOnce(&mut common::model::Song) -> bool,
    ) -> bool {
        self.sync_export_lock();
        let scope = self.song_doc.event_scope();
        self.song_doc.edit_checked(scope, f) == Some(true)
    }

    /// タイムラインを ripple する Song 編集 (セクションの移動 / 複製 / 範囲削除) の
    /// 標準口。 closure が返した [`common::model::Ripple`] 列を、 **`Song` の外に住む
    /// 時間位置** — 再生ループ範囲 (session state、`common::model::LoopRegion`) —
    /// にも同じ規則で適用する。
    ///
    /// ループが Song を出た以上、 「時間を挿入 / 削除したらループ範囲も一緒に動く」
    /// 責務はここが負う。 呼び出し側が帯の幾何 (`[a,b)` / dest の詰め直し) を再計算
    /// する補償コードを書かないよう、 ripple は `Song` 側が「実際に適用したもの」 を
    /// 返す。 戻り値 = 実際に編集が起きたか (export 中拒否は false)。
    pub fn edit_song_rippling(
        &mut self,
        f: impl FnOnce(&mut common::model::Song) -> Vec<common::model::Ripple>,
    ) -> bool {
        let mut ripples = Vec::new();
        let changed = self.edit_song_checked(|song| {
            ripples = f(song);
            !ripples.is_empty()
        });
        if !changed {
            return false;
        }
        let mut region = self.transport.loop_region;
        for r in &ripples {
            region.apply_ripple(*r);
        }
        if region != self.transport.loop_region {
            self.set_loop_region(region);
        }
        true
    }

    /// 派生データ正規化 / save 後 path 書換など undo 非対象の song 変更
    /// ([`SongDoc::normalize`]) の標準口。 `edit_song` と同じく **編集直前に
    /// export_lock を transport から同期する**。 これを経由せず
    /// `song_doc.normalize()` を直呼びすると export_lock が stale なままになり、
    /// export 中の口パク再生成適用等が render 中に LoadSong を送って書き出しを
    /// 壊す (song mutation の遮断は edit / normalize 双方でこの同期に依存する)。
    pub fn normalize_song<R>(&mut self, f: impl FnOnce(&mut common::model::Song) -> R) -> Option<R> {
        self.sync_export_lock();
        self.song_doc.normalize(f)
    }

    /// no-op 検出付き [`AppData::normalize_song`] ([`SongDoc::normalize_checked`]
    /// 参照)。 closure が `false` を返した (= 実際には song が変わらなかった)
    /// ときは epoch を bump しない = dirty 化 / 子プロセス再 sync を起こさない。
    /// `SlotPluginLoaded` backfill 等、 保存ファイルと同一な派生 re-write が
    /// 「開いただけで '*'」 を招くのを防ぐ (r.md #9)。 戻り値 = `Some(changed)`
    /// (export 中拒否は `None`)。
    pub fn normalize_song_checked(
        &mut self,
        f: impl FnOnce(&mut common::model::Song) -> bool,
    ) -> Option<bool> {
        self.sync_export_lock();
        self.song_doc.normalize_checked(f)
    }

    /// song 凍結の単一保証点 (§7.5): export (audio freewheel / video render) 中は
    /// `SongDoc::edit` が編集を拒否する。 handle_event の gate を block-list へ反転
    /// した (song 遮断を event 単位の allow-list で担わない) 代わりに、 song
    /// mutation の遮断はこの 1 箇所 (edit_song / normalize_song チョークポイント) に
    /// 集約する。 export 状態は transport が SSoT なので、 編集直前に毎回同期する
    /// (別途 toggle する scatter を作らない = 「解除し忘れ」故障モードを消す)。
    /// `SongDoc` の編集ロックを現在のオフライン走査状況に合わせる。
    ///
    /// `edit_song` の入口だけでなく、**走査の開始 / 終了時にも呼ぶ**
    /// (`handler::loudness` / `handler::export`)。`edit_song` を経由せず
    /// `song_doc.edit` を直接呼ぶ経路 (BPM スクラブ等) があるので、入口同期だけだと
    /// 「走査が始まった直後の編集が素通りする」窓が残るため。
    pub(crate) fn sync_export_lock(&mut self) {
        self.song_doc.set_export_lock(self.offline_render_busy());
    }

    /// **書き出し / 解析**が進行中か (WAV / video 書き出し・範囲ラウドネス解析)。
    ///
    /// [`Self::offline_render_busy`] との違いは clip bounce / Glue の焼き込みを
    /// 含まないこと。「走行中の plugin instance を再構成する round-trip を捨てる」
    /// 判断 (`app.rs` の block-list) はこちらを見る — bounce / 焼き込みは
    /// **自分の完了通知でその round-trip を行う**ので、広い述語で捨てると
    /// 自分の完了を握り潰して永久に終わらなくなる。
    #[must_use]
    pub fn export_or_analysis_busy(&self) -> bool {
        self.transport.pending_video_export.is_some()
            || self.transport.export_stage.is_some()
            || self.loudness.phase.is_busy()
    }

    /// engine の offline render (`export_running`) を占有する処理が進行中か。
    /// 書き出し / 解析に加えて **clip bounce と `J` (Glue) の焼き込み**を含む。
    ///
    /// engine は offline render を 1 本しか走らせられず、しかも bounce / 焼き込みは
    /// 「対象トラックだけを残した song」を `LoadSong` してから焼く。走行中に GUI が
    /// 編集すると frame flush が `LoadSong(full song)` を送って **render 中の song を
    /// 差し替える**ので、焼く対象が途中から変わる。よって「編集を止める / 再生を
    /// 止める / 別の走査を始めさせない」判断はこの 1 つの述語に集約する。
    #[must_use]
    pub fn offline_render_busy(&self) -> bool {
        self.export_or_analysis_busy()
            || self.ipc.pending_clip_fx_bounce.is_some()
            || self.ipc.pending_glue_bake.is_some()
    }
}
