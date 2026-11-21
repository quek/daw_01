//! 設定 window のページが共有する並べ方 (r.md #156)。
//!
//! ページは [`Page`] の `y` カーソル 1 本で上から積む (並び順がそのまま画面の上下順)。
//! 行の形は「左に項目名、右に操作部品」で、どのページも同じ列幅を使う。

use daw_ui_core::{Ui, WidgetId, hash_inputs};
use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;

/// ページの内側の余白 (px)。
pub(super) const PAD: f32 = 14.0;
/// 項目 1 行の高さ (px)。
pub(super) const ROW_H: f32 = 24.0;
/// 行と行の間 (px)。
const ROW_GAP: f32 = 6.0;
/// 項目名の列の幅 (px)。いちばん長い項目名 (「合成の塊の長さ (秒)」) が省略されずに入る幅。
const LABEL_W: f32 = 148.0;
/// 操作部品の幅の上限 (px)。window を広げても部品が間延びしないように。
const CONTROL_MAX_W: f32 = 320.0;
/// 見出しの下の余白 (px)。
const HEADING_GAP: f32 = 4.0;
/// 説明文の文字サイズと行送り (px)。
const HINT_FONT: f32 = 10.0;
const HINT_LINE_H: f32 = 14.0;

/// ページを上から積むカーソル。`x` / `w` は中身の左端と幅 (余白と scrollbar を引いた後)。
pub(super) struct Page<'a> {
    pub app: &'a AppData,
    pub x: f32,
    pub w: f32,
    /// 次に置く行の上端 (スクロール済みの画面座標)。
    pub y: f32,
}

impl Page<'_> {
    /// 塊の見出し (dim の小さい文字)。先頭以外では上に間を空ける。
    pub(super) fn heading(&mut self, ui: &mut Ui<'_, AppData>, id: &'static str, text: &str, first: bool) {
        if !first {
            self.y += 10.0;
        }
        let p = &self.app.theme.core;
        ui.label_at(("settings_heading", id), text, self.x, self.y + 3.0, 12.0, p.text_dim);
        self.y += 18.0 + HEADING_GAP;
    }

    /// 「項目名 + 操作部品」の行を 1 つ取り、項目名を描いて、操作部品の矩形を返す。
    pub(super) fn row(&mut self, ui: &mut Ui<'_, AppData>, id: &'static str, label: &str) -> Rect {
        let p = &self.app.theme.core;
        ui.label_at_clipped(
            ("settings_row_label", id),
            label,
            Rect { x: self.x, y: self.y + (ROW_H - 12.0 * 1.2) * 0.5, w: LABEL_W - 8.0, h: ROW_H },
            12.0,
            p.text,
        );
        let control = Rect {
            x: self.x + LABEL_W,
            y: self.y,
            w: (self.w - LABEL_W).clamp(0.0, CONTROL_MAX_W),
            h: ROW_H,
        };
        self.y += ROW_H + ROW_GAP;
        control
    }

    /// 項目名の無い、幅いっぱいの行 (チェックボックスなど)。
    pub(super) fn full_row(&mut self) -> Rect {
        let rect = Rect { x: self.x, y: self.y, w: self.w, h: ROW_H };
        self.y += ROW_H + ROW_GAP;
        rect
    }

    /// 説明文 (faint の小さい文字)。幅に合わせて折り返す。
    pub(super) fn hint(&mut self, ui: &mut Ui<'_, AppData>, id: &'static str, text: &str) {
        let color = self.app.theme.core.text_faint;
        self.wrapped(ui, id, text, self.x, self.w, color);
        self.y += ROW_GAP;
    }

    /// `text` を幅 `w` で折り返して描き、`y` を描いた行数ぶん進める。
    fn wrapped(&mut self, ui: &mut Ui<'_, AppData>, id: &'static str, text: &str, x: f32, w: f32, color: Color) {
        let lines = wrapped_lines(ui, id, text, HINT_FONT, w);
        for (i, line) in lines.iter().enumerate() {
            ui.label_at(("settings_wrapped", id, i), line, x, self.y, HINT_FONT, color);
            self.y += HINT_LINE_H;
        }
        store_wrapped_lines(ui, id, lines);
    }
}

/// 折り返し結果のキャッシュ (文字幅の実測は shape を伴うので、毎フレームはやり直さない)。
#[derive(Default)]
struct WrapCache {
    key: u64,
    lines: Vec<String>,
}

fn wrap_cache_id(id: &'static str) -> WidgetId {
    WidgetId::ROOT.child((b"settings_wrap", id))
}

/// `text` を幅 `max_w` に収まる行へ割る。文字・幅・サイズが前フレームと同じならキャッシュを返す
/// (返した `Vec` は描き終えたら [`store_wrapped_lines`] で戻す)。
fn wrapped_lines(ui: &mut Ui<'_, AppData>, id: &'static str, text: &str, font: f32, max_w: f32) -> Vec<String> {
    let key = hash_inputs((text, font.to_bits(), max_w.to_bits()));
    let cache: &mut WrapCache = ui.widget_state(wrap_cache_id(id));
    if cache.key == key && !cache.lines.is_empty() {
        return std::mem::take(&mut cache.lines);
    }
    cache.key = key;
    wrap(ui, text, font, max_w)
}

fn store_wrapped_lines(ui: &mut Ui<'_, AppData>, id: &'static str, lines: Vec<String>) {
    let cache: &mut WrapCache = ui.widget_state(wrap_cache_id(id));
    cache.lines = lines;
}

/// `text` を、実測幅が `max_w` を超えない最長の接頭辞で順に切る (日本語の説明文なので文字単位)。
/// 1 文字も入らない幅でも 1 行に 1 文字は置く (無限ループにしない)。
fn wrap(ui: &mut Ui<'_, AppData>, text: &str, font: f32, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if ui.measure_text(rest, font) <= max_w {
            lines.push(rest.to_string());
            break;
        }
        // 各文字の終端の byte 位置。`ends[k]` までが k+1 文字。
        let ends: Vec<usize> = rest.char_indices().map(|(i, c)| i + c.len_utf8()).collect();
        // 収まる最大の文字数を二分探索 (lo 文字は収まる / hi 文字は収まらない)。
        let (mut lo, mut hi) = (1, ends.len());
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if ui.measure_text(&rest[..ends[mid - 1]], font) <= max_w {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let cut = ends[lo - 1];
        lines.push(rest[..cut].to_string());
        rest = rest[cut..].trim_start();
    }
    lines
}
