//! clipboard / タブ間ドラッグが運ぶ **媒体の写し** (`docs/plan_project_tabs.md` §5.6)。
//!
//! `ClipContent` は音源 / 映像 / 画像を id で指すので、別プロジェクトへ content だけを
//! 貼ると `source_id` が宙に浮き、クリップの殻だけが貼られて中身が鳴らない。envelope に
//! [`common::model::MediaManifest`] を同梱し、貼り先が別プロジェクトなら
//! `Song::import_media` で取り込んで id を張り替える。

use crate::state::AppData;

impl AppData {
    /// clipboard / タブ間ドラッグの envelope に、content が参照する媒体の写しを載せる
    /// (`docs/plan_project_tabs.md` §5.6)。パスは **このプロジェクトのフォルダ** で絶対化
    /// する (貼り先のフォルダでは `ProjectRelative` を解けない)。
    pub(crate) fn envelope_with_media(
        &self,
        payload: crate::clipboard::ClipboardPayload,
    ) -> crate::clipboard::ClipboardEnvelope {
        let song = self.cur.song_doc.song();
        let mut media = song.media_manifest_for(payload.clip_contents());
        media.absolutize(self.project_dir().as_deref());
        crate::clipboard::ClipboardEnvelope::new(self.clipboard_source(), payload).with_media(media)
    }

    /// 写し (clipboard / タブ間ドラッグ) の出どころ: この文書の `(Song.project_id, SongDoc::id_session)`。
    #[must_use]
    pub(crate) fn clipboard_source(&self) -> (u64, u64) {
        (self.cur.song_doc.song().project_id, self.cur.song_doc.id_session())
    }

    /// 貼る写しの出どころ `source` (`ClipboardEnvelope::source`) を、この文書から見た **同じ id 空間か** の判定に使う
    /// project id にしたもの: 同じ開き方 (`SongDoc::id_session`) の写しなら写し元の project id、違えば 0 (どの project
    /// とも一致しない = 別のプロジェクトとして貼る — content は複製し、外の id の参照とコードの記録は捨てる)。保存せずに
    /// 閉じた / Revert した / Save As で分けた版の写しは、同じ project id でも採番が戻って同じ id が別の物を指しうる。
    #[must_use]
    pub(crate) fn paste_source(&self, source: (u64, u64)) -> u64 {
        let (project_id, session) = source;
        if session == self.cur.song_doc.id_session() { project_id } else { 0 }
    }

    /// OS クリップボードへ書いた envelope `json` が運ぶ audio の take の Melodyne の編集を、**写した時点の状態** で
    /// plug-in host に取っておかせる (`PluginCommand::SnapshotAraClipboard`)。 元のプロジェクトを閉じた後に貼っても、
    /// 貼った take は写した元の編集から始まる。 写しの event の `take_origins` は先頭が写した元 (このプロジェクトの
    /// take、写す側が付ける `ClipContent::copied_from`)、続きがその祖先。 貼った take は近い元から順に状態を探すので
    /// (`daw_plugin_host::ara::graph_plan::modification_start`)、写した元が document に居ない (ARA トラックに載って
    /// いない複製) ときに祖先の状態で始まれるよう、このプロジェクトの祖先も全部取っておかせる (host は状態のあるものだけを
    /// 持つ)。 無効のトラック (host に document が無い) の take は、その device の保存したアーカイブを先に預ける。 audio の
    /// take を運ばない写しでは何も送らない。
    pub fn snapshot_ara_for_clipboard(&self, json: &str) {
        let Some(envelope) = crate::clipboard::ClipboardEnvelope::from_json(json) else {
            return;
        };
        let project_id = self.cur.song_doc.song().project_id;
        let mut modifications: Vec<String> = envelope
            .payload
            .audio_events()
            .into_iter()
            .flat_map(|e| &e.take_origins)
            .filter(|o| o.project_id == project_id)
            .map(common::ara_ids::origin_modification_id)
            .collect();
        modifications.sort_unstable();
        modifications.dedup();
        if !modifications.is_empty() {
            let wanted: Vec<_> = modifications.iter().map(|id| (self.pk(), id.as_str())).collect();
            for command in self.dormant_ara_archive_commands(&wanted, None) {
                self.send_plugin(command);
            }
            self.send_plugin(common::protocol::PluginCommand::SnapshotAraClipboard {
                project: self.pk(),
                project_id,
                modifications,
            });
        }
    }

    /// 取り込む直前の正規化: 運んできた写しは絶対パスなので、**貼り先のフォルダ基準**へ
    /// 戻してから [`common::model::Song::import_media`] へ渡す (でないと同じ音源が
    /// `ProjectRelative` と `Absolute` の 2 本になる — 元のタブへ戻したときに必ず起きる)。
    /// 別の文書の未保存の置き場にある媒体は、この文書の置き場へ複製してから指す
    /// ([`Self::adopt_foreign_unsaved_media`] — 借りたままだと持ち主の保存 / 破棄で消える)。
    #[must_use]
    pub(crate) fn media_for_import(
        &mut self,
        media: &common::model::MediaManifest,
    ) -> common::model::MediaManifest {
        let mut out = media.clone();
        out.relativize(self.project_dir().as_deref());
        self.adopt_foreign_unsaved_media(&mut out);
        out
    }

    /// 別プロジェクトから媒体を取り込んだ直後: GUI 側の decode cache (波形 / サムネイル)
    /// を埋める。未 decode のものだけを対象にする冪等な work-list。
    pub(crate) fn decode_imported_media(&mut self, media: &common::model::MediaManifest) {
        if !media.is_empty() {
            self.begin_asset_decode("メディアを読込中");
        }
    }

    /// 一番下に空のトラックを足して id を返す (行の無い場所へ落としたときの受け皿)。
    /// `None` = 編集が拒否された (書き出し中)。
    pub(crate) fn append_empty_track(&mut self) -> Option<u32> {
        self.append_empty_tracks(1)
    }

    /// 一番下に空のトラックを `n` 本足して **先頭の id** を返す。
    /// **1 回の `edit_song`** で積むので undo も 1 手 (行の無い余白へ複数行ぶんを
    /// 落としたときに、トラックの本数だけ undo を押させない)。
    /// `None` = 編集が拒否された (書き出し中) / `n == 0`。
    pub(crate) fn append_empty_tracks(&mut self, n: usize) -> Option<u32> {
        self.append_empty_tracks_ids(n).first().copied()
    }

    /// [`Self::append_empty_tracks`] の全 id 版 (作った順)。`n == 0` なら空 Vec で
    /// **編集も起こさない**。編集が拒否された (書き出し中) ときも空 Vec。
    pub(crate) fn append_empty_tracks_ids(&mut self, n: usize) -> Vec<u32> {
        if n == 0 {
            return Vec::new();
        }
        let ids = self
            .edit_song(move |song| {
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = song.alloc_track_id();
                    song.tracks.push(crate::app_types::track_with(|t| t.id = id));
                    ids.push(id);
                }
                ids
            })
            .unwrap_or_default();
        if !ids.is_empty() {
            self.resize_track_peak_display();
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use crate::clipboard::{ClipboardEnvelope, ClipboardPayload};

    /// 写した後に Song を差し替える (保存版に戻す / 開き直す) と、同じ project id でも別の id 空間の写しとして貼る
    /// (採番がファイルの値へ戻り、写しの id が別の物を指しうる)。
    #[test]
    fn copies_from_before_the_song_was_replaced_paste_as_another_project() {
        let mut app = crate::test_support::headless_app();
        let env = ClipboardEnvelope::new(app.clipboard_source(), ClipboardPayload::Notes(Vec::new()));
        let env = ClipboardEnvelope::from_json(&env.to_json().expect("json")).expect("envelope");
        let project_id = app.cur.song_doc.song().project_id;
        assert_eq!(app.paste_source(env.source()), project_id, "同じ開き方の写しは同じプロジェクト");
        let song = app.cur.song_doc.song().clone();
        app.cur.song_doc.replace_song(song);
        assert_eq!(app.cur.song_doc.song().project_id, project_id);
        assert_eq!(app.paste_source(env.source()), 0, "差し替えた後は別のプロジェクトとして貼る");
    }
}
