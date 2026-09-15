//! handler::section_event — Arranger セクション帯の編集の入口 (`AppEvent::Section`)。
//! 編集の本体は `handler::automation` の `apply_*_section`。

use crate::event_section::SectionEvent;
use crate::state::AppData;

impl AppData {
    /// [`SectionEvent`] の処理 (`AppEvent::Section` の 1 arm)。
    pub(crate) fn handle_section_event(&mut self, ev: SectionEvent) {
        match ev {
            SectionEvent::Create { start, len } => self.apply_create_section(start, len),
            SectionEvent::Move { id, start } => self.apply_move_section(id, start),
            SectionEvent::Resize { id, start, len } => self.apply_resize_section(id, start, len),
            SectionEvent::Duplicate { id, dest_start } => self.apply_duplicate_section(id, dest_start),
            SectionEvent::DeleteBand(id) => self.apply_delete_section_band(id),
            SectionEvent::DeleteRange(id) => self.apply_delete_section_range(id),
            SectionEvent::DeleteSelected => self.apply_delete_selected_sections(),
        }
    }
}
