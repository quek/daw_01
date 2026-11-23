//! handler::inspector_title — r.md #158 のインスペクタの「何を見ているか」(`docs/plan_rmd_158_inspector.md`
//! 仕様 8 / 13)。
//!
//! - タイトル: 選んでいるもの (クリップ → 列 → トラックの優先順)。クリップならクリップ名とクリップ色、
//!   クリップの居るトラックの名前と色見本は 2 行目に小さく添える (カーソルのトラックではない — クリップを
//!   選んだまま別のトラックのヘッダを押しても、クリップの居場所は変わらない)。名前の無いクリップは
//!   「(名前なし)」で、種類は 2 行目に残す。複数なら「N クリップ」/「N トラック」。種類の違うクリップを
//!   一緒に選んでいれば、種類の語は「種類の違うクリップ」(代表の種類を名乗らない)。トラック色を変える
//!   色見本はいつもタイトルの中にある (スクロールしない場所)。
//! - 帯の対象: 帯の見出しの右に出す「この帯が効く相手」。入力は書き込み先のトラック全部、Rack / 口パク /
//!   モジュレーションはカーソルのトラック 1 本だけ (複数選択なら「… のみ」)。タイトルの 1 行目と同じ語は
//!   帯の側で省く (`track_inspector::draw_bands`)。
//!
//! どれも描き始めに 1 回だけ作る選択 ([`InspectorSelection`]) から導く (選択を引き直さない)。
//! 文言は `view::track_inspector::vocab` (見出し・欄名の語の SSoT)。handler から view の定数を読むのは
//! `view::track_color` / `view::native_device` と同じ向き。

use std::borrow::Cow;

use common::model::{Clip, ClipKey, MASTER_TRACK_ID};

use crate::color_target::ColorPickerTarget;
use crate::handler::audio_rec::InputTarget;
use crate::handler::inspector_selection::{ClipBand, InspectorSelection};
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

/// 複数のトラックを選んだときの 2 行目 (名前のカンマ区切り) を打ち切る字数。インスペクタの最大幅 (640px) を
/// 11px の半角で並べても必ず超える長さ — 見える幅を超えた残りは組み立てない (省略記号は描く側の
/// `label_at_clipped` が付ける)。並びは選んだ順のまま。
const TRACK_NAMES_CAP: usize = 160;

impl AppData {
    /// 選択の帯の対象: クリップ 1 つならその名前、複数なら「N クリップ」、種類が混ざっていれば種類ごとの内訳
    /// (タイトルの 1 行目と違う語なので帯に残る)。オートメーションレーンのセルだけなら空。
    #[must_use]
    pub fn inspector_clip_band_target(&self, sel: &InspectorSelection<'_>) -> String {
        if let Some(band) = sel.band.filter(|b| b.mixed()) {
            return vocab::clip_kind_counts(&band.counts);
        }
        match sel.items.as_slice() {
            [one] => self.clip_name(one.key, one.clip),
            [] => String::new(),
            many => vocab::n_clips(many.len()),
        }
    }

    /// 選んでいる列の表示名 (複数ならカンマ区切り、無名なら並び順の番号)。列を選んでいなければ `None`。
    /// タイトルと列の帯が同じ値を使う ([`InspectorSelection::scene_names`] に 1 回だけ作る)。
    #[must_use]
    pub(crate) fn selected_scene_names(&self) -> Option<String> {
        let ids = &self.cur.selection.selected_scene_ids;
        let scenes = &self.cur.song_doc.song().scenes;
        match ids.as_slice() {
            [] => None,
            [one] => scenes.iter().position(|s| s.id == *one).map(|i| scenes[i].display_name(i)),
            _ => Some(
                scenes
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| ids.contains(&s.id))
                    .map(|(i, s)| s.display_name(i))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }

    /// 入力の帯の対象: 書き込み先 (`targets` = [`Self::input_targets`]) の名前、複数なら「選択中の N トラック」
    /// (複数選択のうち録れるのが 1 本なら、カーソルではなくその 1 本)。
    #[must_use]
    pub fn input_band_target(&self, targets: &[InputTarget<'_>]) -> Cow<'_, str> {
        match targets {
            [] => Cow::Borrowed(""),
            [one] => self.cur.song_doc.song().track_display_name(one.track.id),
            many => Cow::Owned(vocab::target_selected_tracks(many.len())),
        }
    }

    /// Rack / 口パク / モジュレーションの帯の対象: カーソルのトラック (複数選択なら「… のみ」)。
    #[must_use]
    pub fn cursor_band_target(&self) -> Cow<'_, str> {
        let Some(cursor) = self.cursor_track_id() else {
            return Cow::Borrowed("");
        };
        let name = self.cur.song_doc.song().track_display_name(cursor);
        if self.cur.selection.selected_track_ids.len() > 1 { Cow::Owned(vocab::target_cursor_only(&name)) } else { name }
    }

    /// インスペクタのタイトル (仕様 8)。
    #[must_use]
    pub fn inspector_title(&self, sel: &InspectorSelection<'_>) -> InspectorTitle {
        if let Some(band) = sel.band.filter(|_| !sel.items.is_empty()) {
            return self.clip_title(sel, band);
        }
        let song = self.cur.song_doc.song();
        let scenes = &self.cur.selection.selected_scene_ids;
        if !scenes.is_empty() {
            let main_swatch = match scenes.as_slice() {
                [one] => song.scenes.iter().find(|s| s.id == *one).map(|s| TitleSwatch {
                    rgb: track_color::effective_scene_color(s),
                    target: ColorPickerTarget::Scene(*one),
                }),
                _ => None,
            };
            // 1 行目で「列」と分かるようにする (無名の列の表示名は番号だけなので、名前だけでは何か読めない)。
            let main = if scenes.len() > 1 {
                vocab::n_scenes(scenes.len())
            } else {
                vocab::scene_title(sel.scene_names.as_deref().unwrap_or_default())
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
    fn clip_title(&self, sel: &InspectorSelection<'_>, band: ClipBand) -> InspectorTitle {
        let kind_label = vocab::clip_kind_label(band.kind, band.mixed());
        // ランチャーのセルか (アレンジのクリップと見分ける。どちらも同じクリップの帯に出る)。
        let all_heads = [kind_label, vocab::PLACE_CELL];
        let heads = if sel.from_cells { &all_heads[..] } else { &all_heads[..1] };
        if let [one] = sel.items.as_slice() {
            // 名前の無いクリップは「(名前なし)」。種類は 2 行目に残す (1 行目と帯の見出しに同じ語を並べない)。
            let name = self.clip_name(one.key, one.clip);
            let main = if name.is_empty() { vocab::UNNAMED_CLIP.to_string() } else { name };
            return InspectorTitle {
                main,
                main_swatch: Some(TitleSwatch {
                    rgb: track_color::effective_clip_color(one.track, one.clip),
                    target: ColorPickerTarget::Clip(one.key),
                }),
                sub: self.track_line(Some(one.key.track_id), heads),
                sub_swatch: self.track_swatch(one.key.track_id),
            };
        }
        // 複数: 全部が同じトラックのときだけ、そのトラックの名前と色見本を添える。
        let first = sel.items[0].key.track_id;
        let track = sel.items.iter().all(|c| c.key.track_id == first).then_some(first);
        InspectorTitle {
            main: vocab::n_clips(sel.items.len()),
            main_swatch: None,
            sub: self.track_line(track, heads),
            sub_swatch: track.and_then(|id| self.track_swatch(id)),
        }
    }

    /// 2 行目: `heads` (種類 / 置き場) と「トラック 名前」を「 · 」で繋ぐ。`track` がマスター / 無いなら省く。
    fn track_line(&self, track: Option<u32>, heads: &[&str]) -> String {
        let song = self.cur.song_doc.song();
        let track = track
            .filter(|id| *id != MASTER_TRACK_ID && song.track_by_id(*id).is_some())
            .map(|id| vocab::track_label(&song.track_display_name(id)));
        let mut out = String::new();
        for part in heads.iter().copied().chain(track.as_deref()) {
            if !out.is_empty() {
                out.push_str(vocab::SEP);
            }
            out.push_str(part);
        }
        out
    }

    fn track_title(&self) -> InspectorTitle {
        let song = self.cur.song_doc.song();
        let selected = &self.cur.selection.selected_track_ids;
        if selected.len() > 1 {
            // 名前を選んだ順に並べ、見える幅を超えたら打ち切る (何百本選んでも組み立てるのは数十本ぶん)。
            let mut sub = String::new();
            let mut chars = 0;
            for id in selected {
                if chars >= TRACK_NAMES_CAP {
                    break;
                }
                if !sub.is_empty() {
                    sub.push_str(", ");
                    chars += 2;
                }
                let name = song.track_display_name(*id);
                chars += name.chars().count();
                sub.push_str(&name);
            }
            return InspectorTitle { main: vocab::n_tracks(selected.len()), main_swatch: None, sub, sub_swatch: None };
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

    /// クリップの表示名 (アレンジのクリップに出る名前と同じ 1 本)。アレンジと同じラベルキャッシュ
    /// (`ArrLabelCache`、`edit_epoch` 世代) から引く — 歌詞の連結や字幕の片の並べ替えを毎フレームしない。
    /// キャッシュに無い (作り直す前の 1 フレーム) ときだけ直接作る (アレンジの build と同じ fallback)。
    pub(crate) fn clip_name(&self, key: ClipKey, clip: &Clip) -> String {
        let cached = self.arrangement_labels().clip_labels.get(&key).map(|l| l.to_string());
        cached.unwrap_or_else(|| {
            crate::widgets::arrangement::view_build::clip_display_label(clip, self.cur.song_doc.song()).to_string()
        })
    }
}
