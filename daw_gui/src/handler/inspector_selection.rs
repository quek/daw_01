//! handler::inspector_selection — インスペクタが描くたびに見る「選んでいるクリップ」(r.md #158)。
//!
//! タイトル・帯の種類・欄の値 (選んでいる全部を畳んだもの)・スクロールの主語は、どれも選択から導く。
//! それぞれが key から選択を引き直すと、1 フレームに選択の導出が何度も走り、どれもトラックとクリップを
//! 線形に探す (範囲で 1 万クリップを選ぶと 1 フレーム数 ms)。そこで描き始めに [`InspectorSelection`] を
//! **1 回だけ** 作り、タイトルも帯も欄もそれを受け取る (key から引き直さない)。
//!
//! 編集の宛先は描画の値と別に持たない: 編集が起きたときに [`AppData::inspector_target_refs`] で解決する
//! (1 ストローク中に選択は変わらない)。

use common::model::{AudioEvent, Clip, ClipContent, ClipKey, EventFade, ImageEvent, TextEvent};

use crate::app_types::{TextNumField, text_event_num_value};
use crate::handler::selection_view::LiveClip;
use crate::state::inspector_view::{ClipBandKind, InspectorSubject};
use crate::state::*;

/// 選択の帯 (クリップ / セル) の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipBand {
    /// 代表クリップの種類 (帯の開閉の鍵と、帯に描く種類ごとの欄)。オートメーションレーンのセルだけなら Automation。
    pub kind: ClipBandKind,
    /// 選んでいるクリップの種類ごとの数 ([`ClipBandKind::ALL`] の並び)。
    pub counts: [usize; ClipBandKind::ALL.len()],
}

impl ClipBand {
    /// 種類の違うクリップが混ざっている (代表の種類の欄は一部にしか効かない)。
    #[must_use]
    pub fn mixed(&self) -> bool {
        self.counts.iter().filter(|n| **n > 0).count() > 1
    }

    /// 代表と同じ種類のクリップの数。
    #[must_use]
    pub fn of_kind(&self) -> usize {
        self.counts[self.kind.index()]
    }

    /// 選んでいるクリップの数。
    #[must_use]
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }
}

/// インスペクタの 1 フレームぶんの選択 ([`AppData::inspector_selection`])。
#[derive(Debug, Default)]
pub struct InspectorSelection<'s> {
    /// 選んでいるクリップ (開始拍順、先頭が代表 = [`AppData::selected_clip_ref`])。
    pub items: Vec<LiveClip<'s>>,
    /// ランチャーのセル選択から来たか (タイトルの「セル」)。
    pub from_cells: bool,
    /// 選択の帯。クリップもセルも選んでいなければ `None`。
    pub band: Option<ClipBand>,
    /// 選んでいる列の表示名 (タイトルと列の帯が使う、[`AppData::selected_scene_names`])。列を選んでいなければ `None`。
    pub scene_names: Option<String>,
}

impl<'s> InspectorSelection<'s> {
    /// 代表クリップ。
    #[must_use]
    pub fn rep(&self) -> Option<LiveClip<'s>> {
        self.items.first().copied()
    }

    /// クリップを 1 つだけ選んでいる (名前 / 本文 / 「ここから読む」の欄はクリップ 1 つの値)。
    #[must_use]
    pub fn single(&self) -> bool {
        self.items.len() == 1
    }

    /// 選んでいる全部を `extract` で畳む: 値が全部 `same` なら `Some(値)`、割れていれば `None` (= mixed)。
    /// `extract` が `None` を返すクリップ (その欄を持たない種類) は数えない。割れたら残りは見ない。
    pub fn fold_by<T>(&self, extract: impl Fn(&LiveClip<'s>) -> Option<T>, same: impl Fn(&T, &T) -> bool) -> Option<T> {
        let mut acc: Option<T> = None;
        for c in &self.items {
            let Some(v) = extract(c) else { continue };
            match &acc {
                None => acc = Some(v),
                Some(a) if !same(a, &v) => return None,
                Some(_) => {}
            }
        }
        acc
    }

    /// 数値の欄の [`Self::fold_by`] (1e-6 までの差は同じ値)。
    pub fn fold_num(&self, extract: impl Fn(&LiveClip<'s>) -> Option<f64>) -> Option<f64> {
        self.fold_by(extract, |a, b| (a - b).abs() <= 1e-6)
    }

    /// 離散値 (トグル / dropdown) の [`Self::fold_by`]。
    pub fn fold_eq<T: PartialEq>(&self, extract: impl Fn(&LiveClip<'s>) -> Option<T>) -> Option<T> {
        self.fold_by(extract, |a, b| a == b)
    }
}

/// VOICEVOX の声の欄が効くクリップの種類 ([`AppData::is_voice_target`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    /// 歌声: VOICEVOX を持つトラックの MIDI クリップ。
    Sing,
    /// 読み上げ: VOICEVOX を持つトラックの Text クリップ。
    Talk,
}

impl AppData {
    /// `clip` (トラック `track` の) が `want` の声の欄の宛先か。描画の畳み込みと編集の宛先が同じこの 1 本で絞る
    /// (`SetClipVoice` / `SetClipTalkParam` の handler は種類で絞らない — 絞らずに渡すと歌声以外のクリップにも
    /// 声 / 読み上げの値を書く)。
    #[must_use]
    pub fn is_voice_target(&self, track: &common::model::Track, clip: &Clip, want: VoiceKind) -> bool {
        let content = self.cur.song_doc.song().clip_contents.get(&clip.content_id);
        let kind_matches = match want {
            VoiceKind::Sing => matches!(content, Some(ClipContent::Midi(_))),
            VoiceKind::Talk => matches!(content, Some(ClipContent::Text(_))),
        };
        kind_matches && track.is_voicevox_vocal()
    }

    /// 声の欄の書き込み先: 選んでいるクリップのうち `want` の宛先 ([`Self::is_voice_target`])。編集が起きたときに
    /// 解決する (描画のたびに宛先の列を作らない)。
    pub fn inspector_voice_targets(&self, want: VoiceKind) -> Vec<ClipKey> {
        self.selected_clips().items.into_iter().filter(|c| self.is_voice_target(c.track, c.clip, want)).map(|c| c.key).collect()
    }

    /// インスペクタの 1 フレームぶんの選択 (選択を 1 回だけ歩き、種類を 1 回だけ数える)。
    #[must_use]
    pub fn inspector_selection(&self) -> InspectorSelection<'_> {
        let crate::handler::selection_view::SelectedClips { items, from_cells } = self.selected_clips();
        let song = self.cur.song_doc.song();
        let kind_of = |c: &LiveClip<'_>| song.clip_contents.get(&c.clip.content_id).map(ClipBandKind::of);
        let band = (!items.is_empty() || from_cells).then(|| {
            let mut counts = [0; ClipBandKind::ALL.len()];
            for k in items.iter().filter_map(kind_of) {
                counts[k.index()] += 1;
            }
            ClipBand { kind: items.first().and_then(kind_of).unwrap_or(ClipBandKind::Automation), counts }
        });
        InspectorSelection { items, from_cells, band, scene_names: self.selected_scene_names() }
    }

    /// インスペクタの主語 (スクロールの床を持ち越してよい範囲、`InspectorScrollFloor`)。
    #[must_use]
    pub fn inspector_subject(&self, sel: &InspectorSelection<'_>) -> InspectorSubject {
        InspectorSubject {
            cursor_track: self.cursor_track_id(),
            tracks: self.cur.selection.selected_track_ids.clone(),
            clips: sel.items.iter().map(|c| c.key).collect(),
            scenes: self.cur.selection.selected_scene_ids.clone(),
        }
    }

    /// inspector の編集対象クリップ群 = [`Self::selected_clip_refs`] そのもの
    /// (セルを選んでいればそのセル群、 アレンジなら**範囲と交差するクリップ全体** —
    /// `docs/plan_range_selection.md` 質問 27b)。 範囲をかすめただけのクリップも
    /// 対象に入る — 属性操作 (改名 / 色 / ゲイン / 声 / 画像の位置) はクリップという
    /// オブジェクトに効くので、範囲操作 (Delete / `J` / ミュート) とは対象の決め方が違う。
    /// 編集が起きたときに解決する (描画は [`Self::inspector_selection`] を使う)。
    pub fn inspector_target_refs(&self) -> Vec<ClipKey> {
        self.selected_clip_refs()
    }

    /// `target` clip の編集が効く最初の `ImageEvent` に `f` を適用 (image clip でない / 窓に見えている
    /// event が無ければ `None`、`handler::clip_window`)。
    /// fade は片ではなくひと続きの値なので [`Self::image_first_fade`] で読む。
    pub fn image_first_event<R>(&self, target: ClipKey, f: impl FnOnce(&ImageEvent) -> R) -> Option<R> {
        self.image_first_event_of(self.cur.song_doc.song().clip_by_key(target)?, f)
    }

    /// [`Self::image_first_event`] の、クリップ本体を手に持っている版 (インスペクタの畳み込み)。
    pub fn image_first_event_of<R>(&self, clip: &Clip, f: impl FnOnce(&ImageEvent) -> R) -> Option<R> {
        self.image_first_run_of(clip).map(|(e, _)| f(e))
    }

    /// `target` clip の編集が効く最初のひと続きの fade (位置・長さ・両端)。
    pub fn image_first_fade(&self, target: ClipKey) -> Option<EventFade> {
        self.image_first_fade_of(self.cur.song_doc.song().clip_by_key(target)?)
    }

    /// [`Self::image_first_fade`] の、クリップ本体を手に持っている版。
    pub fn image_first_fade_of(&self, clip: &Clip) -> Option<EventFade> {
        self.image_first_run_of(clip).map(|(_, fade)| fade)
    }

    fn image_first_run_of(&self, clip: &Clip) -> Option<(&ImageEvent, EventFade)> {
        let events_of = ClipContent::image_events;
        self.clip_edit_anchor(self.clip_shown_targets_of(clip, events_of)?, events_of, None)
    }

    /// `target` clip の編集が効く最初の `TextEvent` に `f` を適用 (text clip でなければ `None`)。
    pub fn text_first_event<R>(&self, target: ClipKey, f: impl FnOnce(&TextEvent) -> R) -> Option<R> {
        self.text_first_run_of(self.cur.song_doc.song().clip_by_key(target)?).map(|(e, _)| f(e))
    }

    /// clip の窓に見えている最初の `TextEvent` と、そのひと続きの fade。
    pub(crate) fn text_first_run_of(&self, clip: &Clip) -> Option<(&TextEvent, EventFade)> {
        let events_of = ClipContent::text_events;
        self.clip_edit_anchor(self.clip_shown_targets_of(clip, events_of)?, events_of, None)
    }

    /// text num field の、クリップ 1 本の値 (畳み込みの 1 項)。fade の長さは片ではなくひと続きの窓に見えている
    /// ランプの長さ (編集が効くのと同じ単位)。
    pub fn text_num_value_of(&self, clip: &Clip, field: TextNumField) -> Option<f64> {
        let (e, fade) = self.text_first_run_of(clip)?;
        Some(match field {
            TextNumField::FadeInBeats => fade.visible_fade_in_beats(),
            TextNumField::FadeOutBeats => fade.visible_fade_out_beats(),
            _ => text_event_num_value(e, field),
        })
    }

    /// `target` clip の編集が効く最初の `AudioEvent` に `f` を適用 (audio clip でなければ `None`)。
    /// Audio Editor がこの clip を開いて event を選んでいれば、その代表 (Inspector の summary と同じ)。
    pub fn audio_first_event<R>(&self, target: ClipKey, f: impl FnOnce(&AudioEvent) -> R) -> Option<R> {
        self.audio_first_event_of(target, self.cur.song_doc.song().clip_by_key(target)?, f)
    }

    /// [`Self::audio_first_event`] の、クリップ本体を手に持っている版 (`target` は Audio Editor の判定に使う)。
    pub fn audio_first_event_of<R>(&self, target: ClipKey, clip: &Clip, f: impl FnOnce(&AudioEvent) -> R) -> Option<R> {
        self.audio_first_run_of(target, clip).map(|(e, _)| f(e))
    }

    /// `target` clip の編集が効く最初のひと続きの fade (位置・長さ・両端)。
    pub fn audio_first_fade(&self, target: ClipKey) -> Option<EventFade> {
        self.audio_first_fade_of(target, self.cur.song_doc.song().clip_by_key(target)?)
    }

    /// [`Self::audio_first_fade`] の、クリップ本体を手に持っている版。
    pub fn audio_first_fade_of(&self, target: ClipKey, clip: &Clip) -> Option<EventFade> {
        self.audio_first_run_of(target, clip).map(|(_, fade)| fade)
    }

    fn audio_first_run_of(&self, target: ClipKey, clip: &Clip) -> Option<(&AudioEvent, EventFade)> {
        let anchor = (self.cur.peph.audio_editor_clip == Some(target)).then(|| self.audio_editor_anchor_event()).flatten();
        self.clip_edit_anchor(self.audio_edit_targets_of(target, clip)?, ClipContent::audio_events, anchor)
    }
}
