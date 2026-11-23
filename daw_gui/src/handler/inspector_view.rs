//! handler::inspector_view — r.md #158 のトラックインスペクタの見せ方 (帯の開閉 / 表示 / 幅)。
//!
//! 帯と詳細の開閉は **プロジェクトの見方** (`ProjectView.inspector_folds`、`ViewState` に保存、`*` なし)、
//! 表示 / 幅は **この人の画面の使い方** (`UiPrefs`、app_config に保存)。どちらも `edit_song` を
//! 通さない (undo / dirty / epoch が動かない — `rack_view::toggle_rack_panel` と同じ扱い)。
//!
//! 帯を畳む直前に入力欄の文字を確定する処理は置かない: 見出しを押した press のフレームで、
//! 入力欄は「欄の外の press」で blur して確定する (`text_input` の `blurred`、daw_01 #112)。
//! 見出しの開閉は release のフレームなので、畳む時点で確定は済んでいる。`I` は素の文字キーなので
//! 入力中は入力欄へ譲られ、隠す操作で入力中の欄が消えることも無い。

use crate::event_inspector::InspectorEvent;
use crate::state::inspector_view::InspectorBand;
use crate::state::*;

/// インスペクタの既定幅 / 下限 / 上限 [px]。下限は内蔵 device の Par の格子 (6 列 × 数値欄 52px) が
/// Parallel の深さ 2 でも重ならない幅 (`docs/plan_rmd_158_inspector.md` §6-10)、上限はアレンジを
/// 潰しすぎない範囲 (マスターパネルと同じ)。
pub const INSPECTOR_DEFAULT_W: f32 = 360.0;
pub const INSPECTOR_MIN_W: f32 = 360.0;
pub const INSPECTOR_MAX_W: f32 = 640.0;

/// 幅を有効範囲へ畳む (NaN は既定幅)。読み出し・保存・ドラッグが同じ 1 本を通る。
#[must_use]
pub fn clamp_inspector_w(w: f32) -> f32 {
    if w.is_finite() { w.clamp(INSPECTOR_MIN_W, INSPECTOR_MAX_W) } else { INSPECTOR_DEFAULT_W }
}

impl AppData {
    pub fn handle_inspector_event(&mut self, ev: InspectorEvent) {
        match ev {
            InspectorEvent::ToggleBand(band) => {
                self.cur.view.inspector_folds.toggle_band(band);
            }
            InspectorEvent::ToggleDetail(detail) => {
                self.cur.view.inspector_folds.toggle_detail(detail);
            }
            InspectorEvent::ToggleVisible => {
                self.ui_prefs.inspector_open = !self.ui_prefs.inspector_open;
                self.persist_app_config();
            }
            // ドラッグ中は状態だけ更新し、release でまとめてディスクへ書く
            // (`SetMasterPanelWidth` と同じ commit-on-release)。
            InspectorEvent::SetWidth { w, commit } => {
                let next = clamp_inspector_w(w);
                if (next - self.ui_prefs.inspector_w).abs() >= 0.5 {
                    self.ui_prefs.inspector_w = next;
                }
                if commit {
                    self.persist_app_config();
                }
            }
        }
    }

    /// 帯が開いているか (種類ごとに共通)。
    #[must_use]
    pub fn inspector_band_open(&self, band: InspectorBand) -> bool {
        self.cur.view.inspector_folds.band_open(band)
    }

    /// Rack の行が画面に描かれているか (インスペクタが出ていて、Rack の帯が開いている)。
    /// 見えない Par に engine の仕事 (EQ のスペクトラム) をさせないための判定。
    #[must_use]
    pub fn inspector_rack_visible(&self) -> bool {
        self.ui_prefs.inspector_open && self.inspector_band_open(InspectorBand::Rack)
    }
}
