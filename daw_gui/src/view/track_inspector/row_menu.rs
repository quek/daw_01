//! chain list の行の右クリックメニュー。
//!
//! 項目は [`DeviceMenuItem`] の型で持ち、 行の種類ごとの並び ([`menu_items`]) と、 選ばれた項目の
//! 適用 ([`apply`]) を分けて置く。 並びを index の直書きで持つと、 行の種類ごとに配列がずれた
//! とき適用側だけが別の操作を走らせる。 chain list 本体 (`chain_list.rs`) から切り出したもの
//! (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{
    AppData, AppEvent, ChainRow, ChainRowKind, ColorPickerTarget, InsertAt, RelocateDevices,
};
use crate::event_device::DeviceEvent;

use super::chain_list::base_row_h;

/// 右クリックメニューの 1 項目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceMenuItem {
    Bypass,
    Group,
    Ungroup,
    Rename,
    Color,
    Copy,
    Cut,
    Paste,
    Duplicate,
    AddChain,
    Delete,
}

impl DeviceMenuItem {
    /// plugin 行。
    const PLUGIN: &'static [Self] =
        &[Self::Bypass, Self::Group, Self::Copy, Self::Cut, Self::Paste, Self::Duplicate, Self::Delete];
    /// Parallel の開始行。
    const PARALLEL: &'static [Self] = &[
        Self::Bypass,
        Self::Ungroup,
        Self::Rename,
        Self::Color,
        Self::Copy,
        Self::Cut,
        Self::Duplicate,
        Self::Delete,
    ];
    /// Parallel 内の chain 行。
    const CHAIN: &'static [Self] = &[Self::Rename, Self::Color, Self::Duplicate, Self::AddChain, Self::Delete];

    /// 表示名。 `Bypass` だけは対象の今の状態で変わる (`bypassed` = いま無効化されているか)。
    fn label(self, bypassed: bool) -> &'static str {
        match self {
            Self::Bypass => {
                if bypassed {
                    "有効化"
                } else {
                    "無効化"
                }
            }
            Self::Group => "Parallel にまとめる",
            Self::Ungroup => "Parallel を解除",
            Self::Rename => "名前変更",
            Self::Color => "色...",
            Self::Copy => "コピー",
            Self::Cut => "切り取り",
            Self::Paste => "貼り付け",
            Self::Duplicate => "複製",
            Self::AddChain => "chain 追加",
            Self::Delete => "削除",
        }
    }
}

/// メニューを開いた行 (選ばれた項目を適用する `Edit` の closure へ Copy で運ぶ)。
#[derive(Debug, Clone, Copy)]
enum MenuRow {
    Device(u64),
    Parallel(u64),
    Chain { parallel_id: u64, chain_id: u64 },
}

/// 行の種類ごとのメニュー (宛先と項目の並び)。 メニューを持たない行 (操作行 / 終了行) は `None`。
fn menu_items(row: &ChainRow) -> Option<(MenuRow, &'static [DeviceMenuItem])> {
    match &row.kind {
        ChainRowKind::Plugin(e) => Some((MenuRow::Device(e.device_id), DeviceMenuItem::PLUGIN)),
        ChainRowKind::ParallelBegin { parallel_id, .. } => {
            Some((MenuRow::Parallel(*parallel_id), DeviceMenuItem::PARALLEL))
        }
        ChainRowKind::Chain { parallel_id, chain_id, .. } => Some((
            MenuRow::Chain { parallel_id: *parallel_id, chain_id: *chain_id },
            DeviceMenuItem::CHAIN,
        )),
        _ => None,
    }
}

/// 右クリックメニュー (widget の外で重ねる idiom)。 plugin / Parallel / chain 行だけ。
pub(super) fn draw_context_menus(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rows: &[ChainRow],
    row_rects: &[(usize, Rect)],
) {
    for (i, row_rect) in row_rects {
        let Some(r) = rows.get(*i) else { continue };
        let Some((target, items)) = menu_items(r) else { continue };
        let base = Rect { x: row_rect.x, y: row_rect.y, w: row_rect.w, h: base_row_h(&r.kind) };
        let bypassed = match &r.kind {
            ChainRowKind::Plugin(e) => app.all_devices_bypassed(&carried_device_ids(app, rows, e.device_id)),
            ChainRowKind::ParallelBegin { bypassed, .. } => *bypassed,
            _ => false,
        };
        let labels: Vec<&str> = items.iter().map(|item| item.label(bypassed)).collect();
        context_menu(ui, base, &labels, move |app, idx| {
            if let Some(&item) = items.get(idx) {
                apply(app, item, target, base);
            }
        });
    }
}

/// 行 `base` の右クリックメニュー。 選ばれた項目 `idx` を `apply` で model に反映する。
fn context_menu(
    ui: &mut Ui<'_, AppData>,
    base: Rect,
    labels: &[&str],
    apply: impl Fn(&mut AppData, usize) + Copy + Send + 'static,
) {
    ui.context_menu_for(base, labels, move |idx, ui| {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| apply(app, idx)));
    });
}

/// 選ばれた項目 `item` を `row` に適用する。 `anchor` は色 picker を開く位置。
fn apply(app: &mut AppData, item: DeviceMenuItem, row: MenuRow, anchor: Rect) {
    use DeviceMenuItem as I;
    match (row, item) {
        (MenuRow::Parallel(parallel_id), I::Ungroup) => {
            app.handle_event(AppEvent::Device(DeviceEvent::UngroupParallel { parallel_id }));
        }
        (MenuRow::Parallel(parallel_id), I::Rename) => {
            let name = app.cur.song_doc.song().parallel_by_id(parallel_id).map(|r| r.name.clone()).unwrap_or_default();
            app.cur.peph.renaming_chain = Some((parallel_id, name));
        }
        (MenuRow::Parallel(parallel_id), I::Color) => {
            app.open_color_picker(ColorPickerTarget::Parallel(parallel_id), anchor);
        }
        (MenuRow::Device(device_id) | MenuRow::Parallel(device_id), item) => {
            apply_to_carried(app, item, device_id);
        }
        (MenuRow::Chain { chain_id, .. }, I::Rename) => {
            let name = app
                .cur.song_doc
                .song()
                .chain_by_id(chain_id)
                .map(|(_, c)| c.name.clone())
                .unwrap_or_default();
            app.cur.peph.renaming_chain = Some((chain_id, name));
        }
        (MenuRow::Chain { chain_id, .. }, I::Color) => {
            app.open_color_picker(ColorPickerTarget::ParallelChain(chain_id), anchor);
        }
        (MenuRow::Chain { chain_id, .. }, I::Duplicate) => {
            app.handle_event(AppEvent::Device(DeviceEvent::DuplicateParallelChain { chain_id }));
        }
        (MenuRow::Chain { parallel_id, .. }, I::AddChain) => {
            app.handle_event(AppEvent::Device(DeviceEvent::AddParallelChain { parallel_id }));
        }
        (MenuRow::Chain { chain_id, .. }, I::Delete) => {
            app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![chain_id] }));
        }
        // chain 行のメニューに無い項目 (`DeviceMenuItem::CHAIN`)。
        (MenuRow::Chain { .. }, I::Bypass | I::Group | I::Ungroup | I::Copy | I::Cut | I::Paste) => {}
    }
}

/// plugin / Parallel の行に共通の項目。 行が選択に含まれていれば選択全体に掛ける
/// ([`carried_device_ids`])。
fn apply_to_carried(app: &mut AppData, item: DeviceMenuItem, device_id: u64) {
    use DeviceMenuItem as I;
    let rows = app.chain_rows();
    let ids = carried_device_ids(app, &rows, device_id);
    match item {
        I::Bypass => {
            let bypassed = !app.all_devices_bypassed(&ids);
            app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: ids, bypassed }));
        }
        I::Group => app.handle_event(AppEvent::Device(DeviceEvent::GroupDevices { device_ids: ids })),
        I::Copy => app.copy_devices(ids),
        I::Cut => app.cut_devices(ids),
        // 貼り付け位置は「この device の直前」。 選択をこの device 1 本にしてから
        // **Ctrl+V と同じ経路** を起こす。
        I::Paste => {
            app.set_device_selection(vec![device_id]);
            app.ui_ephemeral.pending_shortcut_injections.push("paste");
        }
        I::Duplicate => duplicate_after(app, ids, device_id),
        I::Delete => app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: ids })),
        // 行ごとの項目 (`apply` が先に処理する) と chain 行だけの項目。
        I::Ungroup | I::Rename | I::Color | I::AddChain => {}
    }
}

/// 掴んだ / 右クリックした行が選択に含まれていれば選択全体、 含まれていなければその行だけ
/// (トラックヘッダの右クリックメニューと同じ規則)。 順序は表示順。 chain id は運べないので
/// 除く。
pub(super) fn carried_device_ids(app: &AppData, rows: &[ChainRow], device_id: u64) -> Vec<u64> {
    if app.cur.selection.selected_device_ids.contains(&device_id) {
        rows.iter()
            .filter_map(ChainRow::drag_id)
            .filter(|id| app.cur.selection.selected_device_ids.contains(id))
            .collect()
    } else {
        vec![device_id]
    }
}

/// `device_id` の直後に `ids` のコピーを挿す (メニューの「複製」)。
fn duplicate_after(app: &mut AppData, ids: Vec<u64>, device_id: u64) {
    let Some((dest, index)) = app.cur.song_doc.song().find_device(device_id) else {
        return;
    };
    app.handle_event(AppEvent::Device(DeviceEvent::RelocateDevices(RelocateDevices {
        device_ids: ids,
        dest,
        dest_index: InsertAt::Index(index as u32 + 1),
        copy: true,
    })));
}
