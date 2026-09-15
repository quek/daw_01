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
        crate::clipboard::ClipboardEnvelope::new(song.project_id, payload).with_media(media)
    }

    /// OS クリップボードへ書いた envelope `json` が運ぶ audio の take の Melodyne の編集を、**写した時点の状態** で
    /// plug-in host に取っておかせる (`PluginCommand::SnapshotAraClipboard`)。 元のプロジェクトを閉じた後に貼っても、
    /// 貼った take は写した元の編集から始まる。 写しの event は先頭の `take_origins` が写した元 (このプロジェクトの
    /// take) を指している (写す側が付ける、`ClipContent::copied_from`)。 audio の take を運ばない写しでは何も送らない。
    pub fn snapshot_ara_for_clipboard(&self, json: &str) {
        let Some(envelope) = crate::clipboard::ClipboardEnvelope::from_json(json) else {
            return;
        };
        let project_id = self.cur.song_doc.song().project_id;
        let mut modifications: Vec<String> = envelope
            .payload
            .audio_events()
            .into_iter()
            .filter_map(|e| e.take_origins.first())
            .filter(|o| o.project_id == project_id)
            .map(common::ara_ids::origin_modification_id)
            .collect();
        modifications.sort_unstable();
        modifications.dedup();
        if !modifications.is_empty() {
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
