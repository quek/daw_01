//! ランチャー帯の widget が返した「意図」を `AppEvent::Launcher` へ橋渡しする。
//!
//! **widget は `Song` も engine も触らない** ([`LauncherIntent`] の doc) ので、
//! 押した結果を実際に効かせるのはここが唯一の口。この橋が無いと、セルを押しても
//! 何も起きない (widget は intent を積むだけ、handler は `AppEvent` を待つだけ、で
//! 両方が「自分は正しい」まま噛み合わない — 実際に r.md #87 の統合でそうなった)。
//!
//! 変換は 1 対 1 が原則で、ここで意味を足さない。例外は 2 つだけで、どちらも
//! **widget 側の語彙に無い区別**をここで解くもの:
//!
//! - 空セル (`clip_id == 0`) の ▶ は「その行を止める」 (計画書 Q11)。
//! - 空セルの選択は掴む実体が無いので、選択ではなくキーボード焦点の移動にする。

use daw_ui_core::{Edit, TextInputStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ImportTrackTarget};
use crate::state::LauncherFocus;
use crate::event_launcher::{
    ArrangementClipRef as EvClipRef, CellToArrangerDrop as EvCellToArranger,
    ClipToCellDrop as EvClipToCell, LauncherCellKey as EvCellKey, LauncherCellMove as EvCellMove,
    LauncherDropMode, LauncherEvent, LauncherRow,
};
use crate::widgets::arrangement::{
    ArrangementClipRef as WidgetClipRef, ArrangementResponse, ArrangementRowKey, ClipCopyMode,
    LauncherCellKey, LauncherIntent,
};
use crate::widgets::select_modifier::SelectModifier;

/// widget の行キー → handler の行キー。
fn row_of(row: ArrangementRowKey) -> LauncherRow {
    match row {
        ArrangementRowKey::Track(id) => LauncherRow::Track(id),
        ArrangementRowKey::Lane(l) => LauncherRow::Lane(common::model::AutomationLaneKey {
            track: l.track,
            lane: l.lane,
        }),
    }
}

/// widget のセルキー → handler のセルキー。**空セルは `None`**
/// (handler 側の鍵は「実在するクリップ」を指す形なので、空セルは表現できない)。
fn cell_of(cell: LauncherCellKey) -> Option<EvCellKey> {
    // `ClipKey` は widget と model で **同じ型** (r.md #87 で mirror を畳んだ)。
    // `AutomationClipKey` だけまだ widget 側の同名型が居るので詰め替える。
    if let Some(k) = cell.clip_key() {
        return Some(EvCellKey::Track(k));
    }
    cell.automation_clip_key().map(|k| {
        EvCellKey::Lane(common::model::AutomationClipKey {
            track: k.track,
            lane: k.lane,
            clip: k.clip,
        })
    })
}

/// widget が掴んだアレンジのクリップ → handler の参照 (レーンの key は widget の mirror 型
/// から common の型へ詰め替える、`cell_of` と同じ理由)。
fn clip_ref_of(r: WidgetClipRef) -> EvClipRef {
    match r {
        WidgetClipRef::Track(k) => EvClipRef::Track(k),
        WidgetClipRef::Lane(k) => EvClipRef::Lane(common::model::AutomationClipKey {
            track: k.track,
            lane: k.lane,
            clip: k.clip,
        }),
    }
}

fn drop_mode_of(mode: ClipCopyMode) -> LauncherDropMode {
    match mode {
        ClipCopyMode::Move => LauncherDropMode::Move,
        ClipCopyMode::CloneLinked => LauncherDropMode::CopyLinked,
        ClipCopyMode::CloneIndependent => LauncherDropMode::CopyIndependent,
    }
}

/// このフレームの intent を全部 `AppEvent` へ流す。**発生順**を保つ
/// (選択 → 発火 の順序が入れ替わると、撃った先が 1 フレーム古い選択になる)。
pub(super) fn dispatch(app: &AppData, ui: &mut Ui<'_, AppData>, resp: &ArrangementResponse) {
    // ポインタが乗っているセルを毎フレーム `AppData` へ移す。
    // **貼り付け先の解決 (`Ctrl+V`) とキーボード操作の対象判定がこれを読む**ので、
    // 配線を落とすと「セルをコピーできるのに貼れない」「セルを選ぶまで矢印も
    // Enter も効かない」になる (widget は `hovered_cell` を計算していたのに、
    // それを受け取る口が誰も呼ばれていなかった)。
    // 変化したフレームだけ Edit を積む (毎フレーム mutate しない、hovered clip と同じ作法)。
    let hover = resp
        .launcher
        .hovered_cell
        .map(|k| LauncherFocus { row: row_of(k.row), scene_index: k.scene_index as usize });
    if hover != app.launcher.hover {
        let at = hover.map(|f| (f.row, f.scene_index));
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::SetHover(at)));
        }));
    }
    for intent in &resp.launcher.intents {
        let Some(ev) = convert(intent) else {
            continue;
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(ev.clone()));
        }));
    }
}

/// intent 1 件を `LauncherEvent` へ。`None` = このフレームでは何もしない。
fn convert(intent: &LauncherIntent) -> Option<LauncherEvent> {
    Some(match intent {
        LauncherIntent::Launch { cell, pressed } => match cell_of(*cell) {
            Some(c) => LauncherEvent::LaunchCell { cell: c, pressed: *pressed },
            // 空セルの ▶ は「その行を止める」 (計画書 Q11)。離しでは何もしない。
            None if *pressed => LauncherEvent::StopRow { row: row_of(cell.row) },
            None => return None,
        },
        LauncherIntent::LaunchScene { scene_id, pressed } => {
            LauncherEvent::LaunchScene { scene_id: *scene_id, pressed: *pressed }
        }
        LauncherIntent::StopRow(row) => LauncherEvent::StopRow { row: row_of(*row) },
        LauncherIntent::StopAllRows => LauncherEvent::StopAllRows,
        LauncherIntent::SwitchRowToArranger(row) => {
            LauncherEvent::RowToArranger { row: row_of(*row) }
        }
        LauncherIntent::SwitchAllToArranger => LauncherEvent::AllToArranger,
        LauncherIntent::SelectCell { cell, additive, range } => match cell_of(*cell) {
            Some(c) => LauncherEvent::SelectCell {
                cell: c,
                modifier: SelectModifier::from_modifiers(*range, *additive),
            },
            // 空セルは掴む実体が無いので、選択ではなく焦点だけ動かす
            // (矢印キーの起点になり、そのまま `Enter` で撃てる)。
            None => LauncherEvent::FocusCell {
                row: row_of(cell.row),
                scene_index: cell.scene_index as usize,
            },
        },
        LauncherIntent::SelectScene { scene_id, additive, range } => LauncherEvent::SelectScene {
            scene_id: *scene_id,
            modifier: SelectModifier::from_modifiers(*range, *additive),
        },
        LauncherIntent::CreateCell { row, scene_index } => LauncherEvent::CreateCell {
            row: row_of(*row),
            scene_index: *scene_index as usize,
        },
        // 開くのは既存のクリップ編集面 (ピアノロール / オーディオエディタ)。
        // どちらを開くかは content 種別で決まるので、判定は handler 側 1 箇所
        // (アレンジのクリップのダブルクリックと同じ式) に置く。
        LauncherIntent::OpenCellEditor(cell) => LauncherEvent::OpenCellEditor(cell_of(*cell)?),
        LauncherIntent::MoveCells { moves, mode } => {
            let moves: Vec<EvCellMove> = moves
                .iter()
                .filter_map(|m| {
                    Some(EvCellMove {
                        from: cell_of(m.from)?,
                        to_row: row_of(m.to_row),
                        to_scene_index: m.to_scene_index as usize,
                    })
                })
                .collect();
            if moves.is_empty() {
                return None;
            }
            LauncherEvent::MoveCells { moves, mode: drop_mode_of(*mode) }
        }
        LauncherIntent::DropClipsToCells { drops, mode } => {
            let drops: Vec<EvClipToCell> = drops
                .iter()
                .map(|d| EvClipToCell {
                    from: clip_ref_of(d.from),
                    to_row: row_of(d.to_row),
                    to_scene_index: d.to_scene_index as usize,
                })
                .collect();
            if drops.is_empty() {
                return None;
            }
            LauncherEvent::DropClipsToCells { drops, mode: drop_mode_of(*mode) }
        }
        LauncherIntent::DropCellsToArranger { drops, mode } => {
            let drops: Vec<EvCellToArranger> = drops
                .iter()
                .filter_map(|d| {
                    Some(EvCellToArranger {
                        from: cell_of(d.from)?,
                        to_row: row_of(d.to_row),
                        to_start_beat: d.to_start_beat,
                    })
                })
                .collect();
            if drops.is_empty() {
                return None;
            }
            LauncherEvent::DropCellsToArranger { drops, mode: drop_mode_of(*mode) }
        }
        LauncherIntent::ReorderScene { scene_id, to_index } => LauncherEvent::MoveScene {
            scene_id: *scene_id,
            to_index: *to_index as usize,
        },
    })
}

/// ファイルを落とした座標がランチャーのセルに当たっていれば、その取り込み先。
///
/// **行は帯が返した行の y 帯 (`row_bands`) で解く** — X だけで解くと停止列 /
/// 返す列に落としたときまでセル扱いになるし、`cell_rects` (= セルを置ける行しか
/// 載らない) で解くと **グループ行 / マスター行 / テンポ・拍子レーン行に落とした
/// ファイルが「一番下に新トラックを作る」に化ける** (セルの上下インセットで空く
/// 行間 4px の隙間でも同じ)。
///
/// 「その行にセルを置けるか」は [`row_accepts_cells`](crate::handler::launcher_cells::row_accepts_cells)
/// **1 本だけ**を引く (作成 / 移動 / drop / 貼り付けと同じ判定)。置けない行は
/// `None` = 何もしない。オートメーションレーン行はメディア (オーディオ / 画像 /
/// MIDI) を置けないのでこれも `None`。
pub(crate) fn cell_drop_target(
    app: &AppData,
    resp: &ArrangementResponse,
    pos: (f32, f32),
) -> Option<ImportTrackTarget> {
    let grid = resp.launcher.grid_rect;
    if grid.w <= 0.0 || !grid.contains(pos.0, pos.1) {
        // 停止列 / 返す列 / 見出し行 / つかみ代の上に落ちた。格子ではないので
        // **セルにもアレンジにも置かない** (帯の上に落としたものがアレンジの
        // 拍 0 へ飛ぶ方が事故が大きい)。
        return None;
    }
    // 列は**セルの rect に当たらなくても**解く (列と列の 2px の隙間、行が 1 つも
    // 無い下の余白、行と行の隙間 — どこに落ちても列は決まる)。
    let col_w = resp.launcher.col_w;
    if col_w <= 0.0 {
        return None;
    }
    let rel = (pos.0 - grid.x) / col_w + resp.launcher.scroll_scene;
    if rel < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scene_index = rel.floor() as u32;
    // 行は「その y を含む行の帯」から引く。**行が 1 つも無い下の余白**のときだけ
    // 一番下に新しいトラックを作って、その行のセルにする (アレンジ側の
    // `NewTrackBottom` と同じ約束を、ランチャーの語彙で持つ)。
    let row = resp
        .launcher
        .row_bands
        .iter()
        .find(|(_, r)| pos.1 >= r.y && pos.1 < r.y + r.h)
        .map(|(k, _)| *k);
    match row {
        Some(ArrangementRowKey::Track(track_id)) => {
            // グループ行 / マスター行はセルを持てない (置いても鳴らない)。
            let row = LauncherRow::Track(track_id);
            crate::handler::launcher_cells::row_accepts_cells(app.song_doc.song(), row)
                .then_some(ImportTrackTarget::LauncherCell { track_id, scene_index })
        }
        // オートメーションレーン行にはオーディオ / 画像 / MIDI を置けない。
        Some(ArrangementRowKey::Lane(_)) => None,
        None => Some(ImportTrackTarget::LauncherNewTrack { scene_index }),
    }
}

/// セル (クリップ有り) の右クリックメニュー。
const CELL_MENU: &[&str] = &["ピアノロール / エディタで開く", "色...", "独立化", "削除"];

/// 空セルの右クリックメニュー。
const EMPTY_CELL_MENU: &[&str] = &["空のクリップを作る"];

/// セルの右クリックメニューを重ねる (widget は rect を返すだけ)。
///
/// これが無いと、セルに対してできるのは「撃つ / 選ぶ / ダブルクリック」だけで、
/// **削除・色・独立化をポインタから実行する手段が無い** (アレンジのクリップには
/// 全部あるので、同じ操作が帯だけできない非対称になっていた)。
pub(crate) fn cell_overlays(ui: &mut Ui<'_, AppData>, resp: &ArrangementResponse) {
    for (key, rect) in &resp.launcher.cell_rects {
        let rect = *rect;
        let Some(cell) = cell_of(*key) else {
            // 空セル: 中身を作るだけ。列がプレースホルダなら handler が実体化する。
            let (row, scene_index) = (row_of(key.row), key.scene_index as usize);
            ui.context_menu_for(rect, EMPTY_CELL_MENU, move |idx, ui| {
                ui.push_edit(empty_cell_menu_edit(idx, row, scene_index));
            });
            continue;
        };
        ui.context_menu_for(rect, CELL_MENU, move |idx, ui| {
            ui.push_edit(cell_menu_edit(idx, cell, rect));
        });
    }
}

/// [`EMPTY_CELL_MENU`] の選択 → 適用する編集。
fn empty_cell_menu_edit(idx: usize, row: LauncherRow, scene_index: usize) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        if idx == 0 {
            app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell { row, scene_index }));
        }
    })
}

/// [`CELL_MENU`] の選択 → 適用する編集。
fn cell_menu_edit(idx: usize, cell: EvCellKey, rect: Rect) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| match idx {
        0 => app.handle_event(AppEvent::Launcher(LauncherEvent::OpenCellEditor(cell))),
        // 色は既存のカラーピッカーへ (クリップと同じ対象種別)。
        1 => match cell {
            EvCellKey::Track(k) => {
                app.open_color_picker(crate::app::ColorPickerTarget::Clip(k), rect);
            }
            EvCellKey::Lane(k) => {
                app.open_color_picker(crate::app::ColorPickerTarget::AutomationClip(k), rect);
            }
        },
        2 => match cell {
            EvCellKey::Track(k) => app.handle_event(AppEvent::MakeClipUnique(k)),
            EvCellKey::Lane(k) => app.handle_event(AppEvent::MakeAutomationClipUnique(k)),
        },
        3 => app.handle_event(AppEvent::Launcher(LauncherEvent::DeleteCells(vec![cell]))),
        _ => {}
    })
}

/// シーン見出しの右クリックメニューの項目 (表示順)。
const SCENE_MENU: &[&str] = &["名前を変更", "色...", "鳴っているセルを取り込む", "削除"];

/// [`SCENE_MENU`] の選択 → 適用する編集。メニューのコールバックから **本体を
/// 引き剥がす**ためのヘルパ (インデント段数を budget 内に収める)。
fn scene_menu_edit(idx: usize, scene_id: u32, rect: Rect) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| match idx {
        0 => app.handle_event(AppEvent::Launcher(LauncherEvent::BeginRenameScene(scene_id))),
        1 => app.open_color_picker(crate::app::ColorPickerTarget::Scene(scene_id), rect),
        2 => app.handle_event(AppEvent::Launcher(LauncherEvent::CaptureScene)),
        3 => app.handle_event(AppEvent::Launcher(LauncherEvent::DeleteScenes(vec![scene_id]))),
        _ => {}
    })
}

/// シーン見出しの右クリックメニューと、rename 中の inline text input。
///
/// **widget は rect を返すだけ**なので、メニューも入力欄も caller が重ねる
/// (track / clip の rename と同 idiom)。これが無いと「列を右クリックしても何も出ない /
/// 名前を変えられない」になる。
pub(crate) fn scene_overlays(app: &AppData, ui: &mut Ui<'_, AppData>, resp: &ArrangementResponse) {
    let renaming = app.launcher.scene_rename_id;
    for (scene_id, index, rect) in &resp.launcher.scene_rects {
        let scene_id = *scene_id;
        let index = *index as usize;
        let rect = *rect;
        // まだ実体化していない列 (プレースホルダ) は名前も色も持てないので、
        // メニューは「ここに列を作る」意味を持つ Insert だけにする。
        if scene_id == 0 {
            ui.context_menu_for(rect, PLACEHOLDER_MENU, move |idx, ui| {
                if idx == 0 {
                    // **押した列に**生やす (末尾追加だと「列 5 を右クリックしたのに
                    // 列 1 が生える」)。
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::Launcher(LauncherEvent::AddSceneAt(index)));
                    }));
                }
            });
            continue;
        }
        ui.context_menu_for(rect, SCENE_MENU, move |idx, ui| {
            ui.push_edit(scene_menu_edit(idx, scene_id, rect));
        });
        if Some(scene_id) == renaming {
            scene_rename_input(app, ui, scene_id, rect);
        }
    }
}

/// プレースホルダ列の右クリックメニュー。
const PLACEHOLDER_MENU: &[&str] = &["シーンを追加"];

/// 列名の inline 編集 (見出しの上に重ねる text input)。
/// track / clip の rename と同じく **Enter でも外クリックでも確定**する。
fn scene_rename_input(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    scene_id: u32,
    rect: Rect,
) {
    let input_rect = Rect { x: rect.x + 2.0, y: rect.y + 1.0, w: rect.w - 4.0, h: 20.0 };
    let r = ui.text_input_at_focused(
        ("launcher_scene_rename", scene_id),
        input_rect,
        &app.launcher.scene_rename_text,
        &TextInputStyle::default(),
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Launcher(LauncherEvent::RenameSceneChanged(
                    new.clone(),
                )));
            })
        },
    );
    if r.committed || r.blurred {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::CommitRenameScene));
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use common::protocol::{AudioCommand, PluginCommand};
    use tokio::sync::mpsc;

    use crate::dispatcher::{
        BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
    };
    use crate::widgets::arrangement::LauncherResponse;

    fn build_app() -> AppData {
        let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
        let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        AppData::new(
            audio_tx,
            plugin_tx,
            None,
            None,
            event_dispatcher,
            job_dispatcher,
            None,
            None,
            common::audio_bridge::DEFAULT_SAMPLE_RATE,
        )
    }

    /// 行 `10` = グループ (子 `11` を持つ) / 行 `11` = 通常トラック。
    /// 帯は 1 列ぶんだけ描かれていて、行の帯は y = 0..20 / 20..40。
    fn app_and_resp() -> (AppData, ArrangementResponse) {
        let mut app = build_app();
        app.edit_song(|song| {
            song.tracks.clear();
            song.tracks.push(common::model::Track {
                id: 10,
                name: "group".into(),
                ..common::model::Track::default()
            });
            song.tracks.push(common::model::Track {
                id: 11,
                name: "child".into(),
                parent_group_id: Some(10),
                ..common::model::Track::default()
            });
        });
        let launcher = LauncherResponse {
            grid_rect: Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 },
            col_w: 100.0,
            scroll_scene: 0.0,
            row_bands: vec![
                (
                    ArrangementRowKey::Track(10),
                    Rect { x: 0.0, y: 0.0, w: 100.0, h: 20.0 },
                ),
                (
                    ArrangementRowKey::Track(11),
                    Rect { x: 0.0, y: 20.0, w: 100.0, h: 20.0 },
                ),
            ],
            ..LauncherResponse::default()
        };
        let resp = ArrangementResponse { launcher, ..ArrangementResponse::default() };
        (app, resp)
    }

    /// **グループ行へ落としたファイルが「一番下に新トラックを作る」に化けない。**
    ///
    /// 行の解決を `cell_rects` (= セルを置ける行しか載らない) で行うと、グループ行も
    /// 「行が 1 つも無い余白」も同じ「当たらなかった」に潰れ、グループ行への drop が
    /// 黙って新トラックの作成になる。 画面上は「落とした場所と関係ないところに
    /// トラックが生える」形でしか出ないので、気付くのは実機だけ。
    #[test]
    fn グループ行へのファイル_dropは新トラックを作らない() {
        let (app, resp) = app_and_resp();
        assert_eq!(cell_drop_target(&app, &resp, (10.0, 10.0)), None);
    }

    /// 通常トラック行はそのままセルの取り込み先になる。
    #[test]
    fn 通常トラック行へのファイル_dropはその行のセルになる() {
        let (app, resp) = app_and_resp();
        assert_eq!(
            cell_drop_target(&app, &resp, (10.0, 30.0)),
            Some(ImportTrackTarget::LauncherCell { track_id: 11, scene_index: 0 }),
        );
    }

    /// **行と行の隙間**でも行は決まる (セルの上下インセット 4px に落ちても、
    /// 新トラックの作成に化けない)。
    #[test]
    fn 行の境目に落としてもその行のセルになる() {
        let (app, resp) = app_and_resp();
        assert_eq!(
            cell_drop_target(&app, &resp, (10.0, 21.0)),
            Some(ImportTrackTarget::LauncherCell { track_id: 11, scene_index: 0 }),
        );
    }

    /// 行が 1 つも無い下の余白だけが「一番下に新トラックを作る」。
    #[test]
    fn 行の無い余白へのファイル_dropは新トラックになる() {
        let (app, resp) = app_and_resp();
        assert_eq!(
            cell_drop_target(&app, &resp, (10.0, 80.0)),
            Some(ImportTrackTarget::LauncherNewTrack { scene_index: 0 }),
        );
    }
}
