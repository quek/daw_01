//! handler::inspector_title — r.md #158 のインスペクタの「何を見ているか」(`docs/plan_rmd_158_inspector.md`
//! 仕様 8 / 13)。
//!
//! - タイトル: 選んでいるもの (クリップ → 列 → トラックの優先順)。クリップならクリップ名とクリップ色、
//!   トラック名は 2 行目に小さく添える。複数なら「N クリップ」/「N トラック」。トラック色を変える色見本は
//!   いつもタイトルの中にある (スクロールしない場所)。
//! - 帯の対象: 帯の見出しの右に出す「この帯が効く相手」。入力は選択中のトラック全部、Rack / 口パク /
//!   モジュレーションはカーソルのトラック 1 本だけ (複数選択なら「… のみ」)。

use common::model::{ClipKey, MASTER_TRACK_ID};

use crate::color_target::ColorPickerTarget;
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
    #[must_use]
    pub fn inspector_track_band_target(&self, all_selected: bool) -> String {
        let song = self.cur.song_doc.song();
        let Some(cursor) = self.cursor_track_id() else {
            return String::new();
        };
        let name = song.track_display_name(cursor).into_owned();
        if all_selected {
            let n = self.input_target_track_ids().len();
            return if n > 1 { vocab::target_selected_tracks(n) } else { name };
        }
        if self.cur.selection.selected_track_ids.len() > 1 { vocab::target_cursor_only(&name) } else { name }
    }

    /// インスペクタのタイトル (仕様 8)。
    #[must_use]
    pub fn inspector_title(&self) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let refs = self.inspector_target_refs();
        let cursor_swatch = self.cursor_track_swatch();
        // 2 行目: 「種類 · トラック 名前」。種類が 1 行目に出ているとき (`head` が空) はトラックだけ。
        let cursor_line = |head: &str| -> String {
            let track = match self.cursor_track_id() {
                Some(id) if id != MASTER_TRACK_ID && song.track_by_id(id).is_some() => {
                    format!("トラック {}", song.track_display_name(id))
                }
                _ => String::new(),
            };
            match (head.is_empty(), track.is_empty()) {
                (true, _) => track,
                (false, true) => head.to_string(),
                (false, false) => format!("{head} · {track}"),
            }
        };
        if !refs.is_empty() {
            return self.clip_title(&refs, cursor_swatch, &cursor_line);
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
            return InspectorTitle { main, main_swatch, sub: cursor_line(""), sub_swatch: cursor_swatch };
        }
        self.track_title(cursor_swatch)
    }

    fn clip_title(
        &self,
        refs: &[ClipKey],
        cursor_swatch: Option<TitleSwatch>,
        cursor_line: &dyn Fn(&str) -> String,
    ) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let kind = self.inspector_clip_band().map_or(ClipBandKind::Midi, |(k, _)| k);
        let kind_label = vocab::clip_band_title(kind);
        if let [one] = refs
            && let (Some(track), Some(clip)) = (song.track_by_id(one.track_id), song.clip_by_key(*one))
        {
            // 名前の無いクリップは種類を 1 行目に出し、2 行目はトラックだけ (同じ語を 2 行に並べない)。
            let name = clip_name(song, clip);
            let (main, head) = if name.is_empty() { (kind_label.to_string(), "") } else { (name, kind_label) };
            return InspectorTitle {
                main,
                main_swatch: Some(TitleSwatch {
                    rgb: track_color::effective_clip_color(track, clip),
                    target: ColorPickerTarget::Clip(*one),
                }),
                sub: cursor_line(head),
                sub_swatch: cursor_swatch,
            };
        }
        // 複数: 全部が同じトラックのときだけトラック名と色見本を添える。
        let same_track = refs.iter().all(|r| r.track_id == refs[0].track_id);
        InspectorTitle {
            main: vocab::n_clips(refs.len()),
            main_swatch: None,
            sub: if same_track { cursor_line(kind_label) } else { kind_label.to_string() },
            sub_swatch: if same_track { cursor_swatch } else { None },
        }
    }

    fn track_title(&self, cursor_swatch: Option<TitleSwatch>) -> InspectorTitle {
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
        let mut sub = if id == MASTER_TRACK_ID {
            "マスター".to_string()
        } else if song.track_has_children(id) {
            let n = song.tracks.iter().filter(|t| t.parent_group_id == Some(id)).count();
            format!("グループ · 子 {n} 本")
        } else {
            "トラック".to_string()
        };
        if !song.track_effectively_enabled(id) {
            sub.push_str(" · 無効中");
        }
        InspectorTitle { main: song.track_display_name(id).into_owned(), main_swatch: cursor_swatch, sub, sub_swatch: None }
    }

    /// カーソルのトラックの色見本 (マスターは色を持たないので無し)。
    fn cursor_track_swatch(&self) -> Option<TitleSwatch> {
        let id = self.cursor_track_id().filter(|id| *id != MASTER_TRACK_ID)?;
        let track = self.cur.song_doc.song().track_by_id(id)?;
        Some(TitleSwatch { rgb: track_color::effective_track_color(track), target: ColorPickerTarget::Track(id) })
    }
}

/// クリップの表示名 (アレンジのクリップに出る名前と同じ 1 本)。
fn clip_name(song: &common::model::Song, clip: &common::model::Clip) -> String {
    crate::widgets::arrangement::view_build::clip_display_label(clip, song).to_string()
}
