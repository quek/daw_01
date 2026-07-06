//! S3b-1: AppData の state group 分割 (docs/plan_arch_refactor.md §7.5)。
//!
//! AppData は 9 つの group struct の合成になった。 フィールドの帰属は
//! 「undo 対象か / 発火元は何か」 で判断する (§7.5 の表)。 group struct は
//! 純データ (メソッドは AppData 側 / SongDoc のみ振る舞いを持つ)。

pub mod song_doc;
pub mod transport;
pub mod selection;
pub mod ipc;
pub mod voicevox;
pub mod media;
pub mod recording;
pub mod ui_prefs;
pub mod ui_ephemeral;

pub use song_doc::{EditScope, SongDoc, StreamGesture};
pub use transport::TransportState;
pub use selection::SelectionState;
pub use ipc::IpcState;
pub use voicevox::VoicevoxState;
pub use media::MediaState;
pub use recording::RecordingState;
pub use ui_prefs::UiPrefs;
pub use ui_ephemeral::UiEphemeral;

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
    fn sync_export_lock(&mut self) {
        let exporting =
            self.transport.pending_video_export.is_some() || self.transport.export_stage.is_some();
        self.song_doc.set_export_lock(exporting);
    }
}
