//! ランチャーの行の器の成長便 (`docs/plan_unbounded_tracks.md` §2.5)。
//!
//! 行数に上限を置かないので、器は **off-thread で曲から数えて確保し、song と同じ便で届ける**
//! ([`LauncherGrowth`])。RT はそれを差し込むだけ ([`LauncherRuntime::install_growth`])。

use super::*;

/// ランチャーの行の器 (走行状態の行 + 供給元テーブル)。RT で伸ばすと再確保になるので、
/// [`Self::for_song`] で off-thread に確保する。
#[derive(Debug, Default)]
pub struct LauncherGrowth {
    pub(super) rows: Vec<RowRuntime>,
    pub(super) table: RowSourceTable,
}

impl LauncherGrowth {
    /// `song` の行を全部持てる器。
    #[must_use]
    pub fn for_song(song: &Song) -> Self {
        let (rows, groups) = row_capacity(song);
        Self::with_capacity(rows, groups)
    }

    #[must_use]
    pub fn with_capacity(rows: usize, groups: usize) -> Self {
        Self { rows: Vec::with_capacity(rows), table: RowSourceTable::with_capacity(rows, groups) }
    }
}

impl LauncherRuntime {
    /// 大きい器を差し込む (audio thread、`refresh_bundle`)。走行状態の行は要素ごと move で移し、
    /// 供給元テーブルは毎 buffer 作り直すので空の器と入れ替えるだけ。戻り値 = 押し出した旧器
    /// (recycle で off-thread に落とす)。今より小さい器は差し込まずそのまま返す。
    ///
    /// RT 安全: move と容量内の push だけ (確保・解放なし)。
    pub fn install_growth(&mut self, mut growth: LauncherGrowth) -> LauncherGrowth {
        if growth.rows.capacity() > self.rows.capacity() {
            growth.rows.append(&mut self.rows);
            std::mem::swap(&mut self.rows, &mut growth.rows);
        }
        let (rows, groups) = self.table.capacity();
        let (new_rows, new_groups) = growth.table.capacity();
        if new_rows > rows || new_groups > groups {
            std::mem::swap(&mut self.table, &mut growth.table);
        }
        growth
    }
}
