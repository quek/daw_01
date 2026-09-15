//! handler::clip_window — **クリップへの編集が効く範囲** = そのクリップの窓に見えている片 (r.md #132 残件、
//! 2026-09-15 決定)。
//!
//! Inspector の値 (gain / pan / 移調 / フォルマント / 逆再生 / 伸縮 mode / fade / 画像・字幕の属性)、
//! Auto-Fade、Auto-Warp / onset 検出、字幕の読み上げの開始が、ここを通って event を選ぶ。 窓の中でひと続きの
//! 片は 1 つの event として編集し、表示もひと続きから読む (規則の正本は `common::model::window_edit`)。
//! content を共有する反対側のクリップ (分割の片) には効かない。 content を共有して **同じ窓** を見る
//! linked clip には、同じ片なので一緒に効く (`docs/plan_clip_content_window.md`)。

use crate::app_types::*;
use crate::state::*;
use common::model::{ClipContent, ContentId, EventFade, TimedEvent, edit_runs, piece_runs, run_fade, shown_indices};

/// content から時間軸を持つ event 列を引く口 (`ClipContent::audio_events` 等)。
pub(crate) type EventsOf<E> = fn(&ClipContent) -> Option<&[E]>;
/// [`EventsOf`] の可変版 (`ClipContent::audio_events_mut` 等)。
pub(crate) type EventsMut<E> = fn(&mut ClipContent) -> Option<&mut Vec<E>>;

impl AppData {
    /// クリップ `target` の content と、編集が効く event の index (開始拍順)。 窓に見えている片が無ければ
    /// 空。 audio は Audio Editor がこのクリップを開いて event を選んでいれば、その選択が優先する
    /// ([`Self::audio_edit_targets`])。
    pub(crate) fn clip_shown_targets<E: TimedEvent>(
        &self,
        target: ClipKey,
        events_of: EventsOf<E>,
    ) -> Option<(ContentId, Vec<usize>)> {
        let song = self.cur.song_doc.song();
        let clip = song.clip_by_key(target)?;
        let events = events_of(song.clip_contents.get(&clip.content_id)?)?;
        Some((clip.content_id, shown_indices(events, clip.content_window())))
    }

    /// audio 版 [`Self::clip_shown_targets`]: Audio Editor が `target` を開いて event を選んでいれば選んだ
    /// event (範囲外の index は捨てる)、そうでなければ窓に見えている片。
    pub(crate) fn audio_edit_targets(&self, target: ClipKey) -> Option<(ContentId, Vec<usize>)> {
        let (content_id, shown) = self.clip_shown_targets(target, ClipContent::audio_events)?;
        if self.cur.peph.audio_editor_clip != Some(target) {
            return Some((content_id, shown));
        }
        let n = self.cur.song_doc.song().clip_contents.get(&content_id)?.audio_events()?.len();
        let selected: Vec<usize> = self.selected_audio_event_indices().into_iter().filter(|&i| i < n).collect();
        Some((content_id, if selected.is_empty() { shown } else { selected }))
    }

    /// `targets` (content `content_id` の event の index) を、窓の中でひと続きの片ごとに `f` で編集する
    /// (1 回の `edit_song`)。 値が変わったら `true` (変わらなければ履歴にも dirty にも残さない)。
    pub(crate) fn edit_event_runs<E: TimedEvent>(
        &mut self,
        (content_id, targets): (ContentId, Vec<usize>),
        events_mut: EventsMut<E>,
        f: impl FnMut(&mut E),
    ) -> bool {
        if targets.is_empty() {
            return false;
        }
        self.edit_song_checked(|song| {
            let Some(events) = song.clip_contents.get_mut(&content_id).and_then(events_mut) else {
                return false;
            };
            let before: Vec<E> = targets.iter().filter_map(|&i| events.get(i).cloned()).collect();
            edit_runs(events, &targets, f);
            targets.iter().filter_map(|&i| events.get(i)).ne(before.iter())
        })
    }

    /// 字幕クリップ `clip` の窓で始まる片 (読み上げの門 `Clip::window_has_onset` を通る片) の index。
    fn text_onsets_in_window(&self, clip: ClipKey) -> Option<(ContentId, Vec<usize>)> {
        let song = self.cur.song_doc.song();
        let c = song.clip_by_key(clip)?;
        let events = song.clip_contents.get(&c.content_id)?.text_events()?;
        let (content_id, shown) = self.clip_shown_targets(clip, ClipContent::text_events)?;
        let onsets: Vec<usize> =
            shown.into_iter().filter(|&i| c.window_has_onset(events[i].event_start_in_clip_beats)).collect();
        (!onsets.is_empty()).then_some((content_id, onsets))
    }

    /// 字幕クリップ `clip` が **窓の頭から読み上げる** か (`Some(true)`)、窓で始まる最初の片が読み上げない
    /// 続きの片 (`TextEvent::continuation`、`Some(false)`) か。 窓で始まる字幕が無い / 字幕クリップでなければ
    /// `None`。 アレンジの「続き」の印と Inspector の「ここから読む」が同じこれを読む。
    pub fn clip_text_reads(&self, clip: ClipKey) -> Option<bool> {
        let (content_id, onsets) = self.text_onsets_in_window(clip)?;
        let events = self.cur.song_doc.song().clip_contents.get(&content_id)?.text_events()?;
        Some(!events.get(*onsets.first()?)?.continuation)
    }

    /// 「ここから読む」 (`AppEvent::SetClipTextReads`): 字幕クリップ `clip` の窓で始まる片を、普通の読み上げ
    /// (`reads = true`) / 読み上げない続きの片 (`false`) にする。 窓の中でひと続きの片は 1 つの文として
    /// 窓の頭から読み、後ろの片は続きのまま (`common::model::edit_run`)。 ノートの歌詞を「ー」と書き換え
    /// られるのと同じ対称性で、分割の片に限らずどの字幕にも効く。 読み上げの一覧と口パクを送り直す。
    pub(crate) fn set_clip_text_reads(&mut self, clip: ClipKey, reads: bool) {
        let Some(onsets) = self.text_onsets_in_window(clip) else {
            return;
        };
        if self.edit_event_runs(onsets, ClipContent::text_events_mut, |e| e.continuation = !reads) {
            self.sync_vocal_metadata();
            self.mark_lipsync_dirty();
        }
    }

    /// 表示用: 編集が効く片の **最初のひと続き** の先頭の event と、そのひと続きの fade (位置・長さ・両端)。
    /// `anchor` (Audio Editor の選択の代表) があれば、それを含むひと続き。
    pub(crate) fn clip_edit_anchor<E: TimedEvent>(
        &self,
        (content_id, targets): (ContentId, Vec<usize>),
        events_of: EventsOf<E>,
        anchor: Option<usize>,
    ) -> Option<(&E, EventFade)> {
        let events = events_of(self.cur.song_doc.song().clip_contents.get(&content_id)?)?;
        let runs = piece_runs(events, &targets);
        let run = anchor
            .and_then(|a| runs.iter().find(|run| run.contains(&a)))
            .or_else(|| runs.first())?;
        let shown = anchor.filter(|a| run.contains(a)).unwrap_or(*run.first()?);
        Some((events.get(shown)?, run_fade(events, run)?))
    }
}
