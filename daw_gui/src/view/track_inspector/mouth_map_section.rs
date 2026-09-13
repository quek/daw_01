//! インスペクタの「口パク (口形状 → 画像)」 セクション。
//!
//! contract は `chain_sections.rs` / `launch_section.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。 `track_inspector/mod.rs::draw` から
//! 切り出したもの (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};

/// import 済み image source の表示名 (ファイル名)。口パク mapping dropdown 用。
fn image_source_label(src: &common::model::ImageSource) -> String {
    // import 時に保持した元ファイル名を優先 (on-disk path は content addressing
    // で sanitize / hash 済みなので日本語名が潰れて区別できない)。 v21 以前の
    // project は `name` 未保持 (空文字) なので path の file_name に fallback。
    if !src.name.is_empty() {
        return src.name.clone();
    }
    let path = match &src.path {
        common::model::ImageSourcePath::ProjectRelative(p)
        | common::model::ImageSourcePath::Absolute(p) => p,
    };
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// この track を口パク出力先に指定している vocal track があるとき、7 形状
/// (a/i/u/e/o/N/閉口) の画像割当を表示する。各 slot は import 済み image を選ぶ。
pub(super) fn draw_mouth_map_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let cursor_idx = app.cursor_track_index();
    let Some(track) = cursor_idx.and_then(|i| app.cur.song_doc.song().tracks.get(i)) else {
        return y;
    };
    let this_id = track.id;
    let is_target = app
        .cur.song_doc.song()
        .tracks
        .iter()
        .any(|t| t.lipsync_target_track == Some(this_id));
    if !is_target {
        return y;
    }
    ui.label_at(
        "inspector_mouthmap_label",
        "口パク (口形状 → 画像)",
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += 18.0;
    // import 済み image source の (id, ファイル名) 一覧 (id 昇順)。
    // image_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
    // ラベル文字列を別 Vec へ再 clone せず、 ソート後そのまま labels へ move する。
    let mut images: Vec<(common::model::ImageSourceId, String)> = app
        .cur.song_doc.song()
        .media.image_sources
        .iter()
        .map(|(id, src)| (*id, image_source_label(src)))
        .collect();
    images.sort_by_key(|(id, _)| *id);
    let map = track.mouth_map.as_ref();
    const SHAPES: [(common::model::MouthShape, &str); 7] = [
        (common::model::MouthShape::A, "あ"),
        (common::model::MouthShape::I, "い"),
        (common::model::MouthShape::U, "う"),
        (common::model::MouthShape::E, "え"),
        (common::model::MouthShape::O, "お"),
        (common::model::MouthShape::N, "ん"),
        (common::model::MouthShape::Closed, "閉"),
    ];
    let mut image_ids: Vec<common::model::ImageSourceId> =
        Vec::with_capacity(images.len());
    let mut labels: Vec<String> = Vec::with_capacity(images.len() + 1);
    labels.push("(なし)".into());
    for (id, name) in images {
        image_ids.push(id);
        labels.push(name);
    }
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    for (slot_i, (shape, shape_label)) in SHAPES.iter().enumerate() {
        ui.label_at(
            format!("inspector_mouthmap_slot_label_{slot_i}"),
            shape_label,
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let dropdown_rect = Rect {
            x: area.x + pad + 40.0,
            y,
            w: area.w - pad * 2.0 - 40.0,
            h: 22.0,
        };
        let cur = map.map_or(0, |m| m.get(*shape));
        let selected_idx = if cur == 0 {
            0
        } else {
            image_ids
                .iter()
                .position(|id| *id == cur)
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        if let Some(picked) = ui.dropdown(
            format!("inspector_mouthmap_dropdown_{slot_i}"),
            dropdown_rect,
            &label_refs,
            selected_idx,
        ) {
            let source_id = if picked == 0 {
                0
            } else {
                image_ids.get(picked - 1).copied().unwrap_or(0)
            };
            let shape = *shape;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetMouthMapSlot {
                    track: this_id,
                    shape,
                    source_id,
                });
            }));
        }
        y += 26.0;
    }
    y + 4.0
}
