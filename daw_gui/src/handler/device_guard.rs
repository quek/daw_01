//! handler::device_guard — 組み込み内蔵 device を消せない / 包めない / 普通のドラッグで運べない
//! (`docs/plan_rack_native_devices.md` Q5 / §10.11)。
//!
//! 規則そのもの (`Song::is_builtin_native` / `Song::can_relocate`) は model が持ち、ここは「編集の口に
//! 渡す id 列をどう絞るか」だけを持つ。Rack の drop slot の有効判定 (`valid_drop`) と handler が
//! **同じ関数** を通るので、見た目で落とせる slot と実際に動く device が食い違わない。
//!
//! 正規化 (`Song::normalize_native_devices`) はガードの代わりにしない — 漏れた組み込みは補充されるが、
//! id もレーンも変わるのでバグの症状になる。

use common::model::{ChainRef, Song};

use crate::state::AppData;

/// 絞り込みの対象になる編集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceOp {
    /// × / メニューの削除 / Delete キー。
    Remove,
    /// Ctrl+X / メニューの切り取り (クリップボードにも載せない)。
    Cut,
    /// Ctrl+G / メニューの「Parallel にまとめる」。
    Group,
    /// 行 D&D / ヘッダへの drop / 複製。`copy` = Ctrl (コピーは組み込みでもどこへでも置ける)。
    Relocate { dest: ChainRef, copy: bool },
}

/// `id` にこの編集を掛けてよいか。
fn permitted(song: &Song, id: u64, op: DeviceOp) -> bool {
    match op {
        DeviceOp::Remove | DeviceOp::Cut | DeviceOp::Group => !song.is_builtin_native(id),
        DeviceOp::Relocate { dest, copy } => song.can_relocate(id, dest, copy),
    }
}

/// `ids` のうちこの編集を掛けてよいもの (順序は保つ)。
#[must_use]
pub(crate) fn permitted_ids(song: &Song, ids: &[u64], op: DeviceOp) -> Vec<u64> {
    ids.iter().copied().filter(|&id| permitted(song, id, op)).collect()
}

/// `ids` に 1 つでも掛けられるものがあるか (確保なし。drop slot の判定が毎フレーム呼ぶ)。
#[must_use]
pub(crate) fn any_permitted(song: &Song, ids: &[u64], op: DeviceOp) -> bool {
    ids.iter().any(|&id| permitted(song, id, op))
}

/// `ids` のうち **組み込みだから** 落とされる数 (status に出す理由)。Parallel を自分の中へ落とす
/// 循環のように組み込みと無関係に拒まれるものは数えない。
#[must_use]
pub(crate) fn rejected_builtin_count(song: &Song, ids: &[u64], op: DeviceOp) -> usize {
    ids.iter().filter(|&&id| song.is_builtin_native(id) && !permitted(song, id, op)).count()
}

impl AppData {
    /// いまの Song で `ids` を `op` に掛けてよいものへ絞り、組み込みを落としたら status で理由を出す
    /// (編集の入口用。deferred 実行の本体は実行時の Song で [`permitted_ids`] を掛け直す)。
    pub(crate) fn permit_or_explain(&mut self, ids: &[u64], op: DeviceOp) -> Vec<u64> {
        let song = self.cur.song_doc.song();
        if rejected_builtin_count(song, ids, op) > 0 {
            self.ui_ephemeral.status_message = rejected_builtin_message(op).to_string();
        }
        permitted_ids(self.cur.song_doc.song(), ids, op)
    }
}

/// 組み込みを落としたときの status の文言。
#[must_use]
pub(crate) fn rejected_builtin_message(op: DeviceOp) -> &'static str {
    match op {
        DeviceOp::Remove => "組み込みの Comp / EQ は削除できません",
        DeviceOp::Cut => "組み込みの Comp / EQ は切り取れません (コピーはできます)",
        DeviceOp::Group => "組み込みの Comp / EQ は Parallel にまとめられません",
        DeviceOp::Relocate { .. } => "組み込みの Comp / EQ は他の場所へ移動できません (Ctrl でコピー)",
    }
}
