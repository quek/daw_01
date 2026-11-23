//! handler::inspector_title — r.md #158 のインスペクタの「何を見ているか」(`docs/plan_rmd_158_inspector.md`
//! 仕様 8 / 13)。
//!
//! - タイトル: 選んでいるもの (クリップ → 列 → トラックの優先順)。クリップならクリップ名とクリップ色、
//!   クリップの居るトラックの名前と色見本は 2 行目に小さく添える (カーソルのトラックではない — クリップを
//!   選んだまま別のトラックのヘッダを押しても、クリップの居場所は変わらない)。名前の無いクリップは
//!   「(名前なし)」で、種類は 2 行目に残す。複数なら「N クリップ」/「N トラック」。トラック色を変える色見本は
//!   いつもタイトルの中にある (スクロールしない場所)。
//! - 帯の対象: 帯の見出しの右に出す「この帯が効く相手」。入力は選択中のトラック全部、Rack / 口パク /
//!   モジュレーションはカーソルのトラック 1 本だけ (複数選択なら「… のみ」)。タイトルの 1 行目と同じ語は
//!   帯の側で省く (`track_inspector::draw_bands`)。
//!
//! 文言は `view::track_inspector::vocab` (インスペクタの語の SSoT)。handler から view の定数を読むのは
//! `view::track_color` / `view::native_device` と同じ向き。

use common::model::{ClipKey, MASTER_TRACK_ID};

use crate::color_target::ColorPickerTarget;
use crate::event_launcher::LauncherCellKey;
use crate::state::inspector_view::{ClipBandKind, InspectorSubject};
use crate::state::*;
use crate::view::track_color;
use crate::view::track_inspector::vocab;

/// タイトルの色見本 (クリックで色を選ぶ)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TitleSwatch {
    pub rgb: [f32; 3],
    pub target: ColorPickerTarget,
}

/// インスペクタのタイトル 2 行。高さは中身に関係なく一定 (選択を変えても帯の起点が動かない)。
#[derive(Debug, Clone, PartialEq)]
pub struct InspectorTitle {
    /// 1 行目 (クリップ名 / 「3 クリップ」/ 列名 / トラック名 / 「3 トラック」)。
    pub main: String,
    pub main_swatch: Option<TitleSwatch>,
    /// 2 行目 (「音声クリップ · トラック Audio」など)。空でも行は取る。
    pub sub: String,
    pub sub_swatch: Option<TitleSwatch>,
}

impl AppData {
    /// インスペクタの主語 (スクロールの床を持ち越してよい範囲、`InspectorScrollFloor`)。
    #[must_use]
    pub fn inspector_subject(&self) -> InspectorSubject {
        InspectorSubject {
            cursor_track: self.cursor_track_id(),
            tracks: self.cur.selection.selected_track_ids.clone(),
            clips: self.inspector_target_refs(),
            scenes: self.cur.selection.selected_scene_ids.clone(),
        }
    }

    /// 選択の帯 (クリップ / セル) の種類と対象の表示。クリップもセルも選んでいなければ `None`。
    /// 種類は代表クリップ (`selected_clip_ref`) の中身、オートメーションレーンのセルだけなら Automation。
    #[must_use]
    pub fn inspector_clip_band(&self) -> Option<(ClipBandKind, String)> {
        let refs = self.inspector_target_refs();
        if refs.is_empty() && !self.has_live_launcher_cells() {
            return None;
        }
        let song = self.cur.song_doc.song();
        let kind = self
            .selected_clip_ref()
            .and_then(|k| song.clip_by_key(k))
            .and_then(|c| song.clip_contents.get(&c.content_id))
            .map_or(ClipBandKind::Automation, ClipBandKind::of);
        let target = match refs.as_slice() {
            [one] => song.clip_by_key(*one).map(|c| clip_name(song, c)).unwrap_or_default(),
            [] => String::new(),
            many => vocab::n_clips(many.len()),
        };
        Some((kind, target))
    }

    /// 列の帯の対象 (選んでいる列の名前、複数ならカンマ区切り。無名なら並び順の番号)。
    /// 列を選んでいなければ `None`。
    #[must_use]
    pub fn inspector_scene_band_target(&self) -> Option<String> {
        let ids = &self.cur.selection.selected_scene_ids;
        if ids.is_empty() {
            return None;
        }
        let song = self.cur.song_doc.song();
        let names: Vec<String> = song
            .scenes
            .iter()
            .enumerate()
            .filter(|(_, s)| ids.contains(&s.id))
            .map(|(i, s)| s.display_name(i))
            .collect();
        Some(names.join(", "))
    }

    /// トラックの帯の対象。`all_selected` = 選択中の全トラックに効く帯 (入力) か、カーソルの 1 本だけの帯か。
    /// 入力の帯の書き込み先は録れるトラックだけ (`input_target_track_ids`) なので、対象もその名前で出す
    /// (複数選択のうち録れるのが 1 本なら、カーソルではなくその 1 本)。
    #[must_use]
    pub fn inspector_track_band_target(&self, all_selected: bool) -> String {
        let song = self.cur.song_doc.song();
        if all_selected {
            return match self.input_target_track_ids().as_slice() {
                [] => String::new(),
                [one] => song.track_display_name(*one).into_owned(),
                many => vocab::target_selected_tracks(many.len()),
            };
        }
        let Some(cursor) = self.cursor_track_id() else {
            return String::new();
        };
        let name = song.track_display_name(cursor).into_owned();
        if self.cur.selection.selected_track_ids.len() > 1 { vocab::target_cursor_only(&name) } else { name }
    }

    /// インスペクタのタイトル (仕様 8)。
    #[must_use]
    pub fn inspector_title(&self) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let refs = self.inspector_target_refs();
        if !refs.is_empty() {
            return self.clip_title(&refs);
        }
        let scenes = &self.cur.selection.selected_scene_ids;
        if !scenes.is_empty() {
            let main_swatch = match scenes.as_slice() {
                [one] => song.scenes.iter().position(|s| s.id == *one).map(|i| TitleSwatch {
                    rgb: song.scenes[i].color.unwrap_or(track_color::PALETTE[i % track_color::PALETTE.len()]),
                    target: ColorPickerTarget::Scene(*one),
                }),
                _ => None,
            };
            // 1 行目で「列」と分かるようにする (無名の列の表示名は番号だけなので、名前だけでは何か読めない)。
            let main = if scenes.len() > 1 {
                vocab::n_scenes(scenes.len())
            } else {
                vocab::scene_title(&self.inspector_scene_band_target().unwrap_or_default())
            };
            let cursor = self.cursor_track_id();
            return InspectorTitle {
                main,
                main_swatch,
                sub: self.track_line(cursor, &[]),
                sub_swatch: cursor.and_then(|id| self.track_swatch(id)),
            };
        }
        self.track_title()
    }

    /// クリップを選んでいるときのタイトル。2 行目のトラック名と色見本は **クリップの居るトラック**。
    fn clip_title(&self, refs: &[ClipKey]) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let kind = self.inspector_clip_band().map_or(ClipBandKind::Midi, |(k, _)| k);
        let kind_label = vocab::clip_band_title(kind);
        // ランチャーのセルか (アレンジのクリップと見分ける。どちらも同じクリップの帯に出る)。
        let all_cells =
            refs.iter().all(|r| self.cur.selection.selected_launcher_cells.contains(&LauncherCellKey::Track(*r)));
        let place = if all_cells { Some(vocab::PLACE_CELL) } else { None };
        if let [one] = refs
            && let (Some(track), Some(clip)) = (song.track_by_id(one.track_id), song.clip_by_key(*one))
        {
            // 名前の無いクリップは「(名前なし)」。種類は 2 行目に残す (1 行目と帯の見出しに同じ語を並べない)。
            let name = clip_name(song, clip);
            let main = if name.is_empty() { vocab::UNNAMED_CLIP.to_string() } else { name };
            let heads: Vec<&str> = std::iter::once(kind_label).chain(place).collect();
            return InspectorTitle {
                main,
                main_swatch: Some(TitleSwatch {
                    rgb: track_color::effective_clip_color(track, clip),
                    target: ColorPickerTarget::Clip(*one),
                }),
                sub: self.track_line(Some(one.track_id), &heads),
                sub_swatch: self.track_swatch(one.track_id),
            };
        }
        // 複数: 全部が同じトラックのときだけ、そのトラックの名前と色見本を添える。
        let heads: Vec<&str> = std::iter::once(kind_label).chain(place).collect();
        let same_track = refs.iter().all(|r| r.track_id == refs[0].track_id);
        let track = same_track.then_some(refs[0].track_id);
        InspectorTitle {
            main: vocab::n_clips(refs.len()),
            main_swatch: None,
            sub: self.track_line(track, &heads),
            sub_swatch: track.and_then(|id| self.track_swatch(id)),
        }
    }

    /// 2 行目: `heads` (種類 / 置き場) と「トラック 名前」を「 · 」で繋ぐ。`track` がマスター / 無いなら省く。
    fn track_line(&self, track: Option<u32>, heads: &[&str]) -> String {
        let song = self.cur.song_doc.song();
        let track = track
            .filter(|id| *id != MASTER_TRACK_ID && song.track_by_id(*id).is_some())
            .map(|id| vocab::track_label(&song.track_display_name(id)));
        heads.iter().map(|h| (*h).to_string()).chain(track).collect::<Vec<_>>().join(vocab::SEP)
    }

    fn track_title(&self) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let selected = &self.cur.selection.selected_track_ids;
        if selected.len() > 1 {
            let names: Vec<String> = selected.iter().map(|id| song.track_display_name(*id).into_owned()).collect();
            return InspectorTitle {
                main: vocab::n_tracks(selected.len()),
                main_swatch: None,
                sub: names.join(", "),
                sub_swatch: None,
            };
        }
        let Some(id) = self.cursor_track_id().filter(|id| *id == MASTER_TRACK_ID || song.track_by_id(*id).is_some())
        else {
            return InspectorTitle {
                main: vocab::TITLE_NO_TRACK.to_string(),
                main_swatch: None,
                sub: String::new(),
                sub_swatch: None,
            };
        };
        // 2 行目はトラックの種類。マスターは 1 行目の名前がそのまま種類なので出さない (同じ語を重ねない)。
        let mut sub = if id == MASTER_TRACK_ID {
            String::new()
        } else if song.track_has_children(id) {
            let n = song.tracks.iter().filter(|t| t.parent_group_id == Some(id)).count();
            vocab::group_kind(n)
        } else {
            vocab::KIND_TRACK.to_string()
        };
        if !song.track_effectively_enabled(id) {
            sub = if sub.is_empty() { vocab::DISABLED.to_string() } else { format!("{sub}{}{}", vocab::SEP, vocab::DISABLED) };
        }
        InspectorTitle {
            main: song.track_display_name(id).into_owned(),
            main_swatch: self.track_swatch(id),
            sub,
            sub_swatch: None,
        }
    }

    /// トラックの色見本 (マスターは色を持たないので無し)。
    fn track_swatch(&self, id: u32) -> Option<TitleSwatch> {
        if id == MASTER_TRACK_ID {
            return None;
        }
        let track = self.cur.song_doc.song().track_by_id(id)?;
        Some(TitleSwatch { rgb: track_color::effective_track_color(track), target: ColorPickerTarget::Track(id) })
    }
}

/// クリップの表示名 (アレンジのクリップに出る名前と同じ 1 本)。
fn clip_name(song: &common::model::Song, clip: &common::model::Clip) -> String {
    crate::widgets::arrangement::view_build::clip_display_label(clip, song).to_string()
}
