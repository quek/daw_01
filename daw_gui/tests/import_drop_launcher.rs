//! r.md #93: セッションビュー (ランチャー帯) へのファイル drop の**着地先**。
//!
//! 帯の「行が 1 つも無い下の余白」に落としたファイルは、**一番下に新トラックを作って
//! その行のセル**に入る (`ImportTrackTarget::LauncherNewTrack`)。アレンジのレーンへ
//! 化けてはいけない。停止列 / 返す列 / シーン見出しの上に落としたものは、セルにも
//! アレンジにも置かない。
//!
//! headless: 実 D&D の OS イベントは駆動できないので、`UiHost::frame` に
//! `FrameInput::file_drop` を載せて **本物の `arrangement_view::draw`** を回す。
//! 帯の矩形は widget が同じ `ui_prefs` から解いた実レイアウト
//! (`ArrangementResponse::launcher`) を読むので、fixture の幾何が実機とズレない。
//! import は同期 decode なので assert は frame 直後に確定する。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::model::LauncherLayout;
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::AppData;
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::view::arrangement_view;
use daw_gui::widgets::arrangement::{arrangement, ArrangementResponse};
use daw_ui_core::{DroppedFiles, FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

use hound::{SampleFormat, WavSpec, WavWriter};
use tempfile::TempDir;

/// `arrangement_view::draw` に渡す領域 (Snap toolbar 込み)。
const AREA: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
/// `arrangement_view::draw` が上端に取る Snap toolbar の高さ (view 側の定数と同値)。
/// 帯の幾何を widget から直接読むときに、同じ本体矩形を渡すために要る。
const TOOLBAR_H: f32 = 24.0;
const HEADER_W: f32 = 160.0;
const PANE_W: f32 = 300.0;

struct Fixture {
    app: AppData,
    host: UiHost<AppData>,
    wav: PathBuf,
    _proj: TempDir,
    _audio_rx: UnboundedReceiver<AudioCommand>,
    _plugin_rx: UnboundedReceiver<PluginCommand>,
}

/// トラック 2 本 (id 1 / 2、クリップ無し) + 帯を `layout` で出した headless app。
/// 窓は 600px あるのに行は master + 2 本しか無いので、帯の下側は広い余白になる
/// (= 実機で起きた形)。
fn fixture(layout: LauncherLayout) -> Fixture {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    let proj = TempDir::new().unwrap();
    app.song_doc.file_path = Some(proj.path().join("proj.daw"));
    app.edit_song(|song| {
        song.tracks.clear();
        for id in 1..=2u32 {
            song.tracks.push(common::model::Track {
                id,
                name: format!("T{id}"),
                ..common::model::Track::default()
            });
        }
        song.ids.next_track_id = 3;
    });
    app.ui_prefs.arrange_header_w = HEADER_W;
    app.ui_prefs.arrange_scroll_beat = 0.0;
    app.ui_prefs.arrange_track_top = 0.0;
    app.ui_prefs.launcher_layout = layout;
    app.ui_prefs.launcher_width = PANE_W;
    let wav = write_wav(proj.path(), "loop.wav", 48_000);
    Fixture {
        app,
        host: UiHost::no_redraw(),
        wav,
        _proj: proj,
        _audio_rx: audio_rx,
        _plugin_rx: plugin_rx,
    }
}

fn write_wav(dir: &Path, name: &str, frames: usize) -> PathBuf {
    let path = dir.join(name);
    let spec = WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut w = WavWriter::create(&path, spec).unwrap();
    for i in 0..frames {
        w.write_sample((i % 100) as i16).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn screen() -> PhysicalSize {
    PhysicalSize { width: AREA.w as u32, height: AREA.h as u32 }
}

impl Fixture {
    /// widget が今の `ui_prefs` から解く実レイアウト (帯の矩形 / 行の y 帯)。
    /// `arrangement_view::draw` が widget に渡すのと同じ本体矩形 (toolbar を除く) で回す。
    fn geometry(&mut self) -> ArrangementResponse {
        let body = Rect { x: AREA.x, y: AREA.y + TOOLBAR_H, w: AREA.w, h: AREA.h - TOOLBAR_H };
        let mut scene = Scene::new();
        let mut out = None;
        self.host.frame(&mut self.app, &mut scene, screen(), FrameInput::default(), |app, ui| {
            out = Some(arrangement(app, ui, body));
        });
        out.expect("frame が closure を 1 度呼ぶ")
    }

    /// `pos` に WAV を 1 つ落として、view が積んだ Edit を app に適用する。
    fn drop_wav_at(&mut self, pos: (f32, f32)) {
        let input = FrameInput {
            file_drop: Some(DroppedFiles { paths: vec![self.wav.clone()], position: pos }),
            ..FrameInput::default()
        };
        let mut scene = Scene::new();
        self.host.frame(&mut self.app, &mut scene, screen(), input, |app, ui| {
            arrangement_view::draw(app, ui, AREA);
        });
    }

    /// (トラック数, アレンジのクリップ総数, セルの総数)。
    fn placement(&self) -> (usize, usize, usize) {
        let song = self.app.song_doc.song();
        (
            song.tracks.len(),
            song.tracks.iter().map(|t| t.clips.len()).sum(),
            song.tracks.iter().map(|t| t.session_clips.len()).sum(),
        )
    }
}

/// 帯の格子の中で、**どの行の帯にも当たらない**下の余白の y。
fn empty_y_below_rows(resp: &ArrangementResponse) -> f32 {
    let grid = resp.launcher.grid_rect;
    let y = grid.y + grid.h - 20.0;
    let rows_bottom = resp
        .launcher
        .row_bands
        .iter()
        .map(|(_, r)| r.y + r.h)
        .fold(grid.y, f32::max);
    assert!(
        y > rows_bottom,
        "fixture が実機の形になっていない: 行の下端 {rows_bottom} が余白 y={y} より下"
    );
    y
}

// ============================================================
// 余白 → 一番下に新トラック + そのセル
// ============================================================

/// **r.md #93 の本件。** 帯の行より下の余白に落としたら、新トラックの**セル**に入る。
/// アレンジのレーンには 1 つも置かれない。
#[test]
fn 帯の行より下の余白へのdropは新トラックのセルになる() {
    let mut fx = fixture(LauncherLayout::Both);
    let resp = fx.geometry();
    let grid = resp.launcher.grid_rect;
    let y = empty_y_below_rows(&resp);
    let x = grid.x + resp.launcher.col_w * 0.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (3, 0, 1), "(トラック数, アレンジのクリップ数, セル数)");
    let new_track = fx.app.song_doc.song().tracks.last().unwrap();
    assert_eq!(new_track.session_clips.len(), 1);
    assert_eq!(new_track.clips.len(), 0);
}

/// 「ランチャーのみ」レイアウト (= セッションビューを全幅で出した形) でも同じ。
#[test]
fn ランチャーのみレイアウトでも余白へのdropは新トラックのセルになる() {
    let mut fx = fixture(LauncherLayout::LauncherOnly);
    let resp = fx.geometry();
    let grid = resp.launcher.grid_rect;
    let y = empty_y_below_rows(&resp);
    // 2 列目に落とす (列も drop 位置から決まる)。
    let x = grid.x + resp.launcher.col_w * 1.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (3, 0, 1), "(トラック数, アレンジのクリップ数, セル数)");
    let song = fx.app.song_doc.song();
    let cell = &song.tracks.last().unwrap().session_clips[0];
    let scene_index = song.scenes.iter().position(|s| s.id == cell.scene_id);
    assert_eq!(scene_index, Some(1), "落とした列 (2 列目) のセルに入る");
}

// ============================================================
// 格子ではない場所 → 何もしない (アレンジへ流れない)
// ============================================================

/// 停止列 (帯の左端) の余白に落としても、アレンジに新トラックは生えない。
#[test]
fn 停止列の余白へのdropは何も置かない() {
    let mut fx = fixture(LauncherLayout::Both);
    let resp = fx.geometry();
    let y = empty_y_below_rows(&resp);
    let x = (resp.launcher.pane_rect.x + resp.launcher.grid_rect.x) * 0.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (2, 0, 0), "(トラック数, アレンジのクリップ数, セル数)");
}

/// 「アレンジへ返す」列 (帯の右端) の余白に落としても同じ。
#[test]
fn 返す列の余白へのdropは何も置かない() {
    let mut fx = fixture(LauncherLayout::Both);
    let resp = fx.geometry();
    let y = empty_y_below_rows(&resp);
    let grid = resp.launcher.grid_rect;
    let x = grid.x + grid.w + 4.0;
    assert!(
        x < resp.launcher.pane_rect.x + resp.launcher.pane_rect.w,
        "返す列の x が帯の中に無い"
    );
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (2, 0, 0), "(トラック数, アレンジのクリップ数, セル数)");
}

/// シーン見出し (帯の上端) に落としても同じ。
#[test]
fn シーン見出しへのdropは何も置かない() {
    let mut fx = fixture(LauncherLayout::Both);
    let resp = fx.geometry();
    let grid = resp.launcher.grid_rect;
    let x = grid.x + resp.launcher.col_w * 0.5;
    let y = (AREA.y + TOOLBAR_H + grid.y) * 0.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (2, 0, 0), "(トラック数, アレンジのクリップ数, セル数)");
}

/// グループ行 (セルを持てない行) の格子に落としても、そのグループにアレンジの
/// クリップが貼られたり、一番下に新トラックが生えたりしない。
#[test]
fn グループ行へのdropは何も置かない() {
    let mut fx = fixture(LauncherLayout::Both);
    fx.app.edit_song(|song| {
        // id 1 をグループにして id 2 をその子にする (行は master / 1 / 2 の順)。
        song.tracks[1].parent_group_id = Some(1);
    });
    let resp = fx.geometry();
    let (_, group_band) = resp
        .launcher
        .row_bands
        .iter()
        .find(|(k, _)| *k == daw_gui::widgets::arrangement::ArrangementRowKey::Track(1))
        .copied()
        .expect("グループ行の帯が返る");
    let x = resp.launcher.grid_rect.x + resp.launcher.col_w * 0.5;
    let y = group_band.y + group_band.h * 0.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (2, 0, 0), "(トラック数, アレンジのクリップ数, セル数)");
}

// ============================================================
// アレンジ側は従来どおり
// ============================================================

/// アレンジのレーンの余白に落としたら、従来どおり一番下に新トラック + アレンジのクリップ。
#[test]
fn アレンジのレーンの余白へのdropは新トラックのアレンジクリップになる() {
    let mut fx = fixture(LauncherLayout::Both);
    let resp = fx.geometry();
    let lanes = resp.lanes_rect;
    let y = empty_y_below_rows(&resp);
    let x = lanes.x + lanes.w * 0.5;
    fx.drop_wav_at((x, y));
    assert_eq!(fx.placement(), (3, 1, 0), "(トラック数, アレンジのクリップ数, セル数)");
}
