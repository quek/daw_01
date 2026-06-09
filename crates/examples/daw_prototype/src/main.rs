//! examples/daw_prototype — M7 visual prototype demo。
//!
//! M7 で実装した全 widget (scroll_area / popup / menu_bar / context_menu / dropdown /
//! tab_view / split_view / time_ruler / bar_beat_grid / level_meter) を 1 window に
//! 統合した「見た目 DAW」サンプル。操作の整合性は問わない (M8 で undo / shortcut /
//! drag&drop が来てから本格化)。
//!
//! UI 構成:
//! - 上端: menu_bar (File / Edit / View / Help)
//! - 左 sidebar (split_view 左側): プリセット dropdown + scroll 可能なファイルリスト
//! - 右メイン (split_view 右側): tab_view (Mixer / Arrangement / Piano Roll / Sample)
//!   - Mixer タブ: 8ch fader + level_meter
//!   - Arrangement タブ: time_ruler + bar_beat_grid + 仮想クリップ (右クリックで context_menu)
//!   - Piano Roll タブ: time_ruler + bar_beat_grid + 仮 note 配置
//!   - Sample タブ: 簡易プレースホルダー (sample_editor は別 example)

use std::sync::Arc;

use daw_ui_core::{
    ArrangementClip, ArrangementEditRequest, ArrangementMasterRow, ArrangementStyle,
    ArrangementTrack, ArrangementView, BarBeatGridStyle, ClipKey, ColorPickerStyle, DialogResult,
    Edit, FaderResponse,
    FileDialogFilter, InputAccumulator, LevelMeterStyle, ListViewStyle, MASTER_TRACK_ID,
    MenuItemSpec, MeterBallistic, MeterScale, ModalStyle, Orientation, ReorderableListEditRequest,
    ReorderableListStyle, SnapConfig, TimeMapping, TimeRulerStyle, UiHost, ViewportState1D,
};
use daw_ui_platform::{AppEvent, AppHost, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

const N_CH: usize = 8;
const N_TRACKS: usize = 12;
const N_BROWSER_ITEMS: usize = 40;

/// M14 Phase 88 (#058): color_picker のスウォッチパレット (track 色用の代表 16 色)。
const TRACK_COLOR_PALETTE: [Color; 16] = [
    Color::rgb(0.90, 0.30, 0.30),
    Color::rgb(0.90, 0.55, 0.20),
    Color::rgb(0.90, 0.80, 0.25),
    Color::rgb(0.55, 0.80, 0.30),
    Color::rgb(0.30, 0.75, 0.45),
    Color::rgb(0.25, 0.75, 0.70),
    Color::rgb(0.30, 0.65, 0.90),
    Color::rgb(0.35, 0.45, 0.90),
    Color::rgb(0.55, 0.40, 0.90),
    Color::rgb(0.75, 0.40, 0.85),
    Color::rgb(0.90, 0.45, 0.70),
    Color::rgb(0.60, 0.45, 0.35),
    Color::rgb(0.45, 0.50, 0.55),
    Color::rgb(0.30, 0.33, 0.38),
    Color::rgb(0.70, 0.72, 0.75),
    Color::rgb(0.95, 0.95, 0.97),
];

struct DawClip {
    id: u32,
    start_beat: f64,
    len_beats: f64,
    name: Arc<str>,
    color: Option<Color>,
    /// M14 Phase 63e (#019): linked clone group id (`None` = 通常 clip)。 同じ `Some(gid)` を
    /// 持つ clip 群は同じ share group (= 同じ hue で描画 + clip name 左に link glyph)。 daw_01
    /// 本体では `content_id` (notes 共有 store の key) に対応する概念。 prototype では「同じ
    /// content を共有しているふり」 として gid だけ track する。
    share_group_id: Option<u32>,
    /// M14 Phase 63k (#025): audio clip 編集 (gain / fade) の caller-side state。 `Some(...)` で
    /// arrangement widget が dB handle line + fade 角 grip + envelope を描画 + drag handler を bind。
    /// `None` で MIDI / Vocal clip の既存挙動 (audio 描画なし、 hit zone 全 disable)。
    audio_edit: Option<DawAudioEdit>,
}

/// M14 Phase 63k (#025): caller-side の audio 編集 state。 `ArrangementClipAudioEdit` の prototype
/// 版 (snap + clamp + 範囲制限を caller が責任を持つ部分含む)。 widget の `audio_edit: Some(...)`
/// に詰めて gain / fade gesture を有効化する。
#[derive(Clone, Copy, Debug)]
struct DawAudioEdit {
    gain_db: f32,
    fade_in_beats: f64,
    fade_out_beats: f64,
    fade_in_curve: daw_ui_core::FadeCurve,
    fade_out_curve: daw_ui_core::FadeCurve,
}

struct DawTrack {
    id: u32,
    name: Arc<str>,
    muted: bool,
    solo: bool,
    /// M14 Phase 68 (#040): Record-arm 状態。 audio engine を持たない prototype では純粋に
    /// `track.armed` の toggle を model 側に反映するだけ (= ArrangementTrack の R button 動作 demo)。
    armed: bool,
    next_clip_id: u32,
    clips: Vec<DawClip>,
    /// M10 Phase 47b: track volume (`0.0..=1.0`、`1.0` で unity)。
    volume: f32,
    /// M14 Phase 63c (#016): 親 track id (`None` で top-level)。 Reaper folder / Live group 互換。
    parent_id: Option<u32>,
    /// M14 Phase 87 (#059): track 表示色 (`None` で色なし)。 header 左端の色ストライプに反映。
    /// Phase 88 (#058) の color_picker で編集される。
    color: Option<Color>,
}

struct DawModel {
    /// mixer faders / pans / mutes
    faders: [f32; N_CH],
    mutes: [bool; N_CH],
    /// preset dropdown 選択中 index
    preset_idx: usize,
    /// arrangement / piano_roll の viewport (X 軸)
    arr_viewport: ViewportState1D,
    /// M9 P0-2: tab_view_with_state で外部制御中のタブ index
    /// (0=Mixer / 1=Arrangement / 2=Piano Roll / 3=Sample)
    current_tab: usize,
    /// simulated peak per channel (sin で時間変化)
    sim_phase: f32,
    last_action: String,
    /// (M9 Phase 45d) `Demo Dialog` ボタン押下から次フレーム frame 開始時の `ui.open_modal`
    /// 発火までを繋ぐ 1 frame レイテンシ用フラグ。`button_at` の click closure からは
    /// `&mut Ui` にアクセスできないので、Edit 経由で立てて次フレームに `ui.open_modal` する。
    open_demo_request: bool,
    // (M9 Phase 45e) arrangement widget 用 state
    arr_tracks: Vec<DawTrack>,
    arr_view: ArrangementView,
    arr_selected_clips: Vec<ClipKey>,
    /// M14 Phase 96 (daw_01 #068): 前フレームの `ArrangementResponse.hovered_clip` を保持し、
    /// 次フレームの連動ハイライト (`in_active_group`) 計算に使う。 daw_01 が「前フレーム hovered_clip
    /// の content_id も active group に含める」 と説明した hover 駆動の最小再現 (hover は当該フレームの
    /// resp で初めて判明するので 1 フレーム遅延で active group に反映する)。
    arr_hovered_clip: Option<ClipKey>,
    /// M14 Phase 63c (#016): multi-select 化 (旧 `arr_selected_track: Option<u32>` から transition)。
    /// 単一選択 = `vec![tid]`、 解除 = `vec![]`、 multi-select は Shift/Ctrl click で widget 側が
    /// modifier-aware に next を生成して送ってくる。
    arr_selected_tracks: Vec<u32>,
    /// M14 Phase 63c (#016): 折り畳み中の group track id 集合。 widget の `track.collapsed` field
    /// を caller 側で computed して渡す source-of-truth。 `ToggleGroupCollapsed(id)` Edit 受信で toggle。
    arr_collapsed_groups: std::collections::HashSet<u32>,
    /// M14 Phase 63n-1 (#028): automation lane を畳んでいる track id 集合。 widget の
    /// `track.automation_lanes_collapsed` field の SSoT。 `ToggleTrackAutomationCollapsed { track }` 受信で toggle。
    arr_track_automation_collapsed: std::collections::HashSet<u32>,
    /// M14 Phase 63n-2 (#028): automation lane の永続 store (`track_id -> Vec<lane>`)。
    /// `DawModel::new()` で初期 sample lane を入れて (track_id == 1 のみ Volume/Pan)、 user の
    /// 編集 (Add / Move / Delete point、 SetLaneEnabled 等) を直接この HashMap に mutate する。
    /// arrangement に渡すときは `m.arr_automation_lanes.get(&t.id).cloned().unwrap_or_default()`。
    arr_automation_lanes:
        std::collections::HashMap<u32, Vec<daw_ui_core::ArrangementAutomationLane>>,
    /// M14 Phase 63n-3 (#028): 選択中の automation clip key 集合 (短 click on clip で SelectAutomationClips
    /// を受信し、 widget へ毎 frame 渡して selected_fill / selected_border 表示を駆動する SSoT)。
    arr_selected_automation_clips: Vec<daw_ui_core::AutomationClipKey>,
    /// M14 Phase 63n-8 (#033): 選択中の automation point key 集合 (空き lane zone 内 lasso + point 短 click
    /// で `SelectAutomationPoints` を受信し、 widget へ毎 frame 渡して selected point の白色 + 大 dot 描画を
    /// 駆動する SSoT)。 `MoveAutomationPoints` の multi-batch も widget が `selected_automation_points`
    /// を読んで自動分岐するため caller 側追加処理は不要。
    arr_selected_automation_points: Vec<daw_ui_core::AutomationPointKey>,
    /// M14 Phase 63n-6 (#031): per-track row 高さ override の永続 store (`track_id -> u16`)。
    /// `SetSingleTrackRowH { track, prev: _, next }` 受信で `insert(track, next)`、 widget へ渡すときは
    /// `t.row_h = m.arr_track_row_h.get(&t.id).copied()` で `Some(h)` / `None` を反映。 None で `view.track_row_h`
    /// global default にフォールバック (= override されていない track は既存 Alt+wheel global zoom に追従)。
    arr_track_row_h: std::collections::HashMap<u32, u16>,
    /// M14 Phase 63e (#019): linked clone で発番する group id の counter。
    /// `CloneClipsLinked` 受信時、 source に group_id がなければ新採番、 source / dst 両方に
    /// 同じ id を assign。 `arr_tracks_for_widget` で `(gid as f32 * 0.618034).rem_euclid(1.0)`
    /// で hue 化して `ArrangementClip.share_group_color` に渡す (golden-ratio で隣接 group が
    /// 色相的に十分離れる、 well-known hash trick)。
    arr_next_share_group_id: u32,
    /// M14 Phase 63n-10 (#034): song-level automation を表す master row (SongTempo / SongTimeSigNumerator
    /// 模擬)。 daw_01 の `Song.song_lanes: Vec<AutomationLane>` の widget 側 representation。
    /// `arrangement()` に `Some(&model.arr_master_row)` で渡すと上端 1 行として描画される。
    /// EditRequest の `track_id == MASTER_TRACK_ID` 経路は master 専用の lane store を mutate する
    /// (= 通常 track の `arr_automation_lanes` HashMap と独立、 SSoT 分離)。
    arr_master_row: ArrangementMasterRow,
    /// `BeginRenameTrack(id)` 受信時にセット。`Some(id)` 中は該当 track header 上に
    /// `text_input_at_focused` を重ね描画 (M11 Phase 52 で `text_input_at` から差し替え、
    /// 「初回 show 自動 focus」が widget 内蔵で boilerplate ゼロ)。Enter / blur / ESC で
    /// `None` に戻す。
    arr_rename_target: Option<u32>,
    /// M14 Phase 88 (#058): track header 右クリック → "Color..." で開く color_picker の対象 track。
    /// `Some(id)` の間 1 フレームごとに `ui.color_picker` を呼んで overlay 描画する。 picker の
    /// `dismissed` で `None` に戻す。
    arr_color_picker_target: Option<u32>,
    /// M14 Phase 99 (daw_01 #071): 空きレーン右クリック (`SecondaryClickEmpty`) で開く
    /// コンテキストメニューの stash。`Some((track, beat, pos))` の間、毎フレーム
    /// `ui.context_menu_at` を呼んで `pos` にメニューを描画する (color_picker overlay と同 idiom)。
    /// on_select (= Text クリップ生成) / 外 click で閉じたら `None` に戻す。
    arr_ctx_menu: Option<(u32, f64, (f32, f32))>,
    /// 上記メニューの 1-shot open trigger。`SecondaryClickEmpty` 受信 Edit で `true` にし、
    /// `draw_arrangement_tab` が `open_at = Some(pos)` を 1 フレームだけ渡したら `false` に戻す
    /// (毎フレーム `Some` を渡すと outside-click で閉じても翌フレーム再 open してしまうため)。
    /// `open_demo_request` と同じ 1 frame レイテンシ flag idiom。
    arr_ctx_menu_open: bool,
    /// (M11 Phase 51) `Ui::reorderable_list` demo 用。Demo Dialog の plugin chain。
    /// drag&drop で並び替え、`ReorderableListEditRequest::Reorder(order)` で新順 index 列を受信。
    demo_chain: Vec<String>,
}

impl DawModel {
    #[allow(clippy::too_many_lines)]
    fn new() -> Self {
        let mut tracks: Vec<DawTrack> = Vec::with_capacity(N_TRACKS);
        for ti in 0..N_TRACKS {
            let mut clips: Vec<DawClip> = Vec::with_capacity(2);
            for ci in 0..2 {
                // M14 Phase 63k (#025) demo: track 3-5 (= "Track 4/5/6") に audio_edit を入れて、
                // arrangement widget の dB handle line + fade 角 grip + envelope の動作確認を可能に。
                // 残り track は MIDI clip 想定で None (既存挙動)。 値は user が「触ったら違いが分かる」
                // ように初期 fade をやや長く + clip 4 (track 4) は gain +3 dB のオフセットで開始。
                let audio_edit = if (3..=5).contains(&ti) {
                    Some(DawAudioEdit {
                        gain_db: if ti == 3 { 3.0 } else { 0.0 },
                        fade_in_beats: if ci == 0 { 0.5 } else { 0.0 },
                        fade_out_beats: if ci == 1 { 0.5 } else { 0.0 },
                        fade_in_curve: daw_ui_core::FadeCurve::Linear,
                        fade_out_curve: daw_ui_core::FadeCurve::Linear,
                    })
                } else {
                    None
                };
                clips.push(DawClip {
                    id: ci as u32,
                    start_beat: ti as f64 * 1.5 + f64::from(ci) * 6.0,
                    len_beats: 4.0,
                    name: Arc::from(format!("clip{}", ci + 1)),
                    color: None,
                    share_group_id: None,
                    audio_edit,
                });
            }
            // M14 Phase 63c (#016) demo: track 0 を group とし、 track 1-2 を子に。
            // 残り (3+) は top-level、 disclosure ▼/▶ + indent + collapsed 動作確認用。
            let parent_id = if ti == 1 || ti == 2 { Some(0_u32) } else { None };
            // M14 Phase 63k (#025) demo: audio clip を持つ track 3-5 は「Audio N」 と名付けて user が識別しやすく。
            // M14 Phase 107 (#079) demo: track 6 は name 領域より明らかに長い名前にして、 ellipsis 省略
            // (末尾 …、 左寄せ、 M/S/R に被らない) の動作を起動直後に確認できるようにする。
            let name = if ti == 0 {
                "Group A".to_string()
            } else if (3..=5).contains(&ti) {
                format!("Audio {}", ti - 2)
            } else if ti == 6 {
                "Lead Synth Bus (sidechained reverb send)".to_string()
            } else {
                format!("Track {}", ti + 1)
            };
            // M14 Phase 87 (#059) demo: 一部 track に初期色を seed して header 左端ストライプの
            // 動作確認 (Phase 88 の color_picker で右クリック → "Color..." で編集可能になる)。
            // M14 Phase 97 (#069) demo: track 1 は Group A の子 (depth 1) なので、 色ストライプが
            // 名前と一緒に右へインデントする (= 親 0 / Audio 3,4 の depth 0 ストライプより右) ことを確認できる。
            let color = match ti {
                0 => Some(Color::rgb(0.90, 0.55, 0.20)), // Group A = orange (depth 0)
                1 => Some(Color::rgb(0.85, 0.40, 0.60)), // 子トラック = pink (depth 1、 #069 indent 確認用)
                3 => Some(Color::rgb(0.20, 0.70, 0.65)), // Audio 1 = teal
                4 => Some(Color::rgb(0.60, 0.40, 0.85)), // Audio 2 = purple
                _ => None,
            };
            tracks.push(DawTrack {
                id: ti as u32,
                name: Arc::from(name),
                muted: false,
                solo: false,
                armed: false,
                next_clip_id: 2,
                clips,
                volume: 0.75,
                parent_id,
                color,
            });
        }
        let arr_view = ArrangementView {
            start_beat: 0.0,
            len_beats: 24.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            // M10 Phase 47b: track header の volume band を表示するため row_h を 36px に (>= 34px が表示閾値)。
            // Phase 48 の Alt+wheel 縦ズームで動的変更可能。
            track_row_h: 36.0,
            header_w: 180.0,
            ruler_h: 24.0,
            playhead_beat: Some(2.0),
            loop_range: Some((4.0, 12.0)),
            data_generation: 0,
            // M13 Phase 55: bpm + time_sig (4/4 で従来挙動維持)
            bpm: 120.0,
            time_sig: (4, 4),
            // M9 Phase 45f (#010 [Replied]): デフォルト Adaptive snap で grid 吸着の動作確認。
            snap: SnapConfig::DEFAULT,
        };
        Self {
            faders: [0.55, 0.70, 0.30, 0.60, 0.40, 0.80, 0.20, 0.55],
            mutes: [false; N_CH],
            preset_idx: 0,
            arr_viewport: ViewportState1D::new(0.0, 48_000.0 * 30.0), // 30 sec @ 48k
            current_tab: 0,
            sim_phase: 0.0,
            last_action: "起動 — メニュー / タブ / dropdown / 右クリック を試して下さい".to_string(),
            open_demo_request: false,
            arr_tracks: tracks,
            arr_view,
            arr_selected_clips: Vec::new(),
            arr_hovered_clip: None,
            arr_selected_tracks: Vec::new(),
            arr_collapsed_groups: std::collections::HashSet::new(),
            arr_track_automation_collapsed: std::collections::HashSet::new(),
            // M14 Phase 63n-2 (#028): track_id == 0 (= "Group A"、 表示 1 番目) のみ sample lane
            // (Volume / Pan) を初期化。 group track にも automation lane を持たせる Bitwig 流。
            // 他 track は空 Vec で「lane なし」 (= 既存挙動互換、 disclosure 描画なし)。
            arr_automation_lanes: {
                let mut h = std::collections::HashMap::new();
                h.insert(0, build_sample_automation_lanes(0));
                h
            },
            arr_selected_automation_clips: Vec::new(),
            arr_selected_automation_points: Vec::new(),
            arr_track_row_h: std::collections::HashMap::new(),
            arr_next_share_group_id: 0,
            arr_rename_target: None,
            arr_color_picker_target: None,
            arr_ctx_menu: None,
            arr_ctx_menu_open: false,
            // M14 Phase 63n-10 (#034): SongTempo 模擬の lane 1 つで起動 (= 上端 master row が "Master" と
            // tempo curve を 1 行で表示)。 daw_01 conversation #034 §C 仕様に従う初期値。 ▶/▼ disclosure
            // で折り畳み確認、 dblclick で point 追加 (既存 EditRequest 流用 + `track == MASTER_TRACK_ID`
            // 経路) を visual verify する。
            arr_master_row: ArrangementMasterRow {
                automation_lanes_collapsed: false,
                automation_lanes: vec![daw_ui_core::ArrangementAutomationLane {
                    id: 0,
                    label: Arc::from("Tempo"),
                    icon_glyph: 'T',
                    color: Color::rgb(0.95, 0.55, 0.20),
                    enabled: true,
                    visible: true,
                    height_px: 60,
                    default_value_norm: 0.5,
                    clips: Vec::new(),
                }],
                height_px_override: None,
            },
            demo_chain: vec![
                "MIDI Quantize".to_string(),
                "Synth".to_string(),
                "Reverb".to_string(),
                "EQ".to_string(),
                "Limiter".to_string(),
            ],
        }
    }

    fn sim_peak(&self, ch: usize) -> f32 {
        let f = (self.sim_phase * 0.4 + ch as f32 * 0.7).sin().abs();
        // mute はゼロ、fader で attenuation、peak は ±1 にクリップ可能
        if self.mutes[ch] { 0.0 } else { (f * self.faders[ch] * 1.2).clamp(0.0, 1.5) }
    }
}

const PRESETS: &[&str] = &["Default", "Mixer-only", "Arrangement", "Piano-roll"];
const HELP_TEXT: &str = "M7 prototype: タブ / split / メニュー / dropdown / 右クリック / scroll";

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<DawModel>,
    model: DawModel,
    scene: Scene,
    input: InputAccumulator,
    last_tick: std::time::Instant,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        let ui = UiHost::<DawModel>::with_window(window.clone());
        let model = DawModel::new();
        let scene = Scene::new();
        let input = InputAccumulator::new();
        window.set_title("daw-ui daw_prototype (M7 visual prototype)");
        Self {
            window,
            renderer,
            ui,
            model,
            scene,
            input,
            last_tick: std::time::Instant::now(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let input = self.input.take_input();
        // sim_phase を時間で進める (level_meter のアニメーション用)
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.model.sim_phase += dt * 4.0;

        let menu_h = 28.0;
        let footer_h = 24.0;
        let menu_rect = Rect { x: 0.0, y: 0.0, w: screen.width as f32, h: menu_h };
        let footer_rect = Rect {
            x: 0.0,
            y: screen.height as f32 - footer_h,
            w: screen.width as f32,
            h: footer_h,
        };
        let body_rect = Rect {
            x: 0.0,
            y: menu_h,
            w: screen.width as f32,
            h: (screen.height as f32 - menu_h - footer_h).max(100.0),
        };

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // sim_phase アニメ継続のため毎フレーム redraw を要求
                // (実運用では audio thread から peak 取得時に request_redraw を呼ぶ)
                ui.request_redraw();

                // M8 Phase 30: shortcut layer。
                // - Ctrl+Z で undo / Ctrl+Shift+Z / Ctrl+Y で redo
                // - Ctrl+O で audio file open dialog
                if ui.take_shortcut("undo") {
                    ui.request_undo();
                }
                if ui.take_shortcut("redo") {
                    ui.request_redo();
                }
                if ui.take_shortcut("open") {
                    ui.request_open_file_dialog(
                        "open_audio",
                        "Open audio file",
                        &[FileDialogFilter {
                            name: "Audio",
                            extensions: &["wav", "mp3", "flac", "ogg"],
                        }],
                    );
                }
                // 前フレームに完了した dialog 結果を取り出して last_action に表示。
                if let Some(result) = ui.take_dialog_result("open_audio") {
                    let action = match result {
                        DialogResult::OpenFile(p) => format!("open: {}", p.display()),
                        DialogResult::Cancelled => "open: cancelled".to_string(),
                        _ => "open: unexpected".to_string(),
                    };
                    ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                        m.last_action = action;
                    }));
                }
                // M8 Phase 32: file drop を window 全体で受ける。drop された path と座標を last_action に表示。
                // daw_01 #023 拡張: 戻り値の DroppedFiles から position も使える (track 解決等)。
                let screen_rect = Rect::new(0.0, 0.0, ui.screen().width as f32, ui.screen().height as f32);
                if let Some(drop) = ui.take_file_drop_in_rect(screen_rect) {
                    let paths_str = drop
                        .paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let action = format!("drop: {} @ ({:.0}, {:.0})", paths_str, drop.position.0, drop.position.1);
                    ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                        m.last_action = action;
                    }));
                }

                // ---- 1. menu_bar ----
                // M9 P1-5: Undo/Redo の dynamic enable + shortcut hint。menu_bar の前に
                // 取得して closure に move (内側 closure から borrow できるように `move` keyword)。
                let edit_can_undo = ui.can_undo();
                let edit_can_redo = ui.can_redo();
                let undo_hint = ui.shortcut_for("undo");
                let redo_hint = ui.shortcut_for("redo");
                ui.menu_bar(menu_rect, move |menu| {
                    menu.menu("File", |sub| {
                        sub.item("New", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → New".to_string();
                            }));
                        });
                        sub.item("Open...", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Open".to_string();
                            }));
                        });
                        // M7+ sub_menu cascade demo: hover で右に出る
                        sub.sub_menu("Recent", |recent| {
                            recent.item("project1.daw", |ui| {
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "File → Recent → project1.daw".to_string();
                                }));
                            });
                            recent.item("session_2026.daw", |ui| {
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "File → Recent → session_2026.daw".to_string();
                                }));
                            });
                            recent.sub_menu("Older", |older| {
                                older.item("draft_a.daw", |ui| {
                                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                        m.last_action = "File → Recent → Older → draft_a".to_string();
                                    }));
                                });
                                older.item("draft_b.daw", |ui| {
                                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                        m.last_action = "File → Recent → Older → draft_b".to_string();
                                    }));
                                });
                            });
                        });
                        sub.item("Save", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Save".to_string();
                            }));
                        });
                        sub.item("Quit", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Quit (no-op in demo)".to_string();
                            }));
                        });
                    });
                    menu.menu("Edit", |sub| {
                        // M9 P1-5 (C 案): on_click closure に &mut Ui を渡せるので、menu の Undo
                        // から ui.request_undo() を直接発火可能。shortcut Ctrl+Z 経路と同等の動作。
                        // enabled は can_undo() / can_redo() に基づいて動的に灰色化、shortcut_hint
                        // で右端に "Ctrl+Z" 等を表示。
                        sub.item_with(MenuItemSpec {
                            label: "Undo",
                            on_click: Box::new(|ui| {
                                ui.request_undo();
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "Edit → Undo (menu)".to_string();
                                }));
                            }),
                            enabled: edit_can_undo,
                            shortcut_hint: undo_hint,
                        });
                        sub.item_with(MenuItemSpec {
                            label: "Redo",
                            on_click: Box::new(|ui| {
                                ui.request_redo();
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "Edit → Redo (menu)".to_string();
                                }));
                            }),
                            enabled: edit_can_redo,
                            shortcut_hint: redo_hint,
                        });
                    });
                    menu.menu("View", |sub| {
                        sub.item("Reset zoom", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.arr_viewport = ViewportState1D::new(0.0, 48_000.0 * 30.0);
                                m.last_action = "View → Reset zoom".to_string();
                            }));
                        });
                    });
                    menu.menu("Help", |sub| {
                        sub.item("About", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = HELP_TEXT.to_string();
                            }));
                        });
                    });
                });

                // ---- 2. split_view (sidebar | main) ----
                ui.split_view("root_split", body_rect, Orientation::Horizontal, 0.22, |ui, sidebar, main| {
                    // ---- sidebar: preset dropdown + scroll list ----
                    let sidebar_pad = 8.0;
                    let dd_rect = Rect {
                        x: sidebar.x + sidebar_pad,
                        y: sidebar.y + sidebar_pad,
                        w: sidebar.w - sidebar_pad * 2.0,
                        h: 26.0,
                    };
                    let preset_idx = m.preset_idx;
                    if let Some(idx) = ui.dropdown("preset", dd_rect, PRESETS, preset_idx) {
                        ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                            m.preset_idx = idx;
                            m.last_action = format!("preset → {}", PRESETS[idx]);
                        }));
                    }

                    // browser scroll list (40 items)
                    let list_rect = Rect {
                        x: sidebar.x + sidebar_pad,
                        y: sidebar.y + sidebar_pad + dd_rect.h + sidebar_pad,
                        w: sidebar.w - sidebar_pad * 2.0,
                        h: (sidebar.h - sidebar_pad * 3.0 - dd_rect.h).max(50.0),
                    };
                    let item_h = 24.0;
                    let total_h = item_h * N_BROWSER_ITEMS as f32;
                    ui.scroll_area("browser", list_rect, (list_rect.w, total_h), |ui, offset| {
                        for i in 0..N_BROWSER_ITEMS {
                            let y = list_rect.y - offset.1 + i as f32 * item_h;
                            let r = Rect { x: list_rect.x, y, w: list_rect.w - 12.0, h: item_h };
                            // 簡易: ラベル付き矩形 (button だと clip 越しに hit が変かも)
                            ui.push_rect(RectCommand::uniform_radius(
                                r,
                                Color::rgb(0.13, 0.14, 0.18),
                                2.0,
                            ));
                            ui.label_at(
                                ("browser_lbl", i),
                                &format!("Sample {i:02}.wav"),
                                r.x + 8.0,
                                r.y + 6.0,
                                12.0,
                                Color::rgb(0.85, 0.88, 0.92),
                            );
                        }
                    });

                    // ---- main: tab_view (M9 P0-2: 外部 state 版で footer button から
                    // タブを切替可能に) ----
                    let mut tab_idx = m.current_tab;
                    ui.tab_view_with_state("main_tabs", main, &mut tab_idx, |tabs| {
                        tabs.tab("Mixer", |ui, pane| {
                            drawmixer_tab(ui, m, pane);
                        });
                        tabs.tab("Arrangement", |ui, pane| {
                            draw_arrangement_tab(ui, m, pane);
                        });
                        tabs.tab("Piano Roll", |ui, pane| {
                            draw_piano_roll_tab(ui, m, pane);
                        });
                        tabs.tab("Sample", |ui, pane| {
                            ui.label_at(
                                "sample_placeholder",
                                "Sample editor は別 example: cargo run --bin sample_editor",
                                pane.x + 16.0,
                                pane.y + 16.0,
                                14.0,
                                Color::rgb(0.85, 0.88, 0.92),
                            );
                        });
                    });
                    // クリックで selected が変化していれば model に書き戻し
                    if tab_idx != m.current_tab {
                        ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                            mm.current_tab = tab_idx;
                        }));
                    }
                });

                // ---- 3. footer (last_action + M9 P0-2: Open Piano Roll button) ----
                // M9 Phase 45a: Ui::panel を背景塗りに使う (heavy+cached+push_rect の 1 行ラッパ)
                ui.panel("footer_bg", footer_rect, Color::rgb(0.08, 0.09, 0.11), 0.0);
                let btn_w = 160.0;
                let btn_pad = 4.0;
                let btn_rect = Rect {
                    x: footer_rect.x + footer_rect.w - btn_w - 8.0,
                    y: footer_rect.y + btn_pad,
                    w: btn_w,
                    h: footer_rect.h - btn_pad * 2.0,
                };
                ui.button_at("open_pr", "Open Piano Roll", btn_rect, || {
                    Edit::mutate(|m: &mut DawModel| {
                        m.current_tab = 2;
                        m.last_action = "footer button → Piano Roll タブへ遷移".to_string();
                    })
                });

                // M9 Phase 45d: Demo Dialog button + Ui::modal + Ui::list_view デモ
                let demo_btn_w = 110.0;
                let demo_btn_rect = Rect {
                    x: btn_rect.x - demo_btn_w - 8.0,
                    y: btn_rect.y,
                    w: demo_btn_w,
                    h: btn_rect.h,
                };
                ui.button_at("open_demo", "Demo Dialog", demo_btn_rect, || {
                    Edit::mutate(|m: &mut DawModel| {
                        m.open_demo_request = true;
                        m.last_action = "Demo Dialog open".to_string();
                    })
                });
                ui.label_at(
                    "footer",
                    &m.last_action,
                    8.0,
                    footer_rect.y + 5.0,
                    12.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );

                // ---- 4. Demo Dialog (M9 Phase 45d): button click → 次フレーム open_modal ----
                if m.open_demo_request {
                    ui.open_modal("demo");
                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                        m.open_demo_request = false;
                    }));
                }
                let modal_style = ModalStyle::default();
                let list_style = ListViewStyle::default();
                let reorder_style = ReorderableListStyle::default();
                let demo_items: [&str; 8] = [
                    "Reverb", "Delay", "Compressor", "EQ",
                    "Limiter", "Chorus", "Distortion", "Gate",
                ];
                let chain_ref: &[String] = &m.demo_chain;
                ui.modal(
                    "demo",
                    (420.0, 540.0),
                    &modal_style,
                    Some(Box::new(|| {
                        Edit::mutate(|m: &mut DawModel| {
                            m.last_action = "Demo Dialog closed".to_string();
                        })
                    })),
                    |ui, panel| {
                        // タイトル
                        ui.label_at(
                            "demo_title",
                            "Demo Dialog — list_view + reorderable_list",
                            panel.x + 16.0,
                            panel.y + 16.0,
                            14.0,
                            Color::rgb(0.95, 0.95, 0.97),
                        );

                        // ---- 上半分: list_view (effects 一覧、行 click で close) ----
                        ui.label_at(
                            "demo_lv_label",
                            "Effects (list_view、行クリックで close)",
                            panel.x + 16.0,
                            panel.y + 44.0,
                            12.0,
                            Color::rgb(0.78, 0.80, 0.85),
                        );
                        let list_rect = Rect {
                            x: panel.x + 16.0,
                            y: panel.y + 64.0,
                            w: panel.w - 32.0,
                            h: 200.0,
                        };
                        let resp = ui.list_view(
                            "demo_list",
                            list_rect,
                            &demo_items,
                            None,
                            &list_style,
                            |ui, name, i, row, _sel| {
                                ui.label_at(
                                    ("demo_label", i),
                                    name,
                                    row.x + 12.0,
                                    row.y + 6.0,
                                    13.0,
                                    Color::rgb(0.92, 0.92, 0.94),
                                );
                            },
                        );
                        if resp.clicked.is_some() {
                            ui.close_modal("demo");
                        }

                        // ---- 下半分: reorderable_list (Plugin Chain、drag で並び替え) ----
                        ui.label_at(
                            "demo_rl_label",
                            "Plugin Chain (reorderable_list、drag で並び替え)",
                            panel.x + 16.0,
                            panel.y + 280.0,
                            12.0,
                            Color::rgb(0.78, 0.80, 0.85),
                        );
                        let reorder_rect = Rect {
                            x: panel.x + 16.0,
                            y: panel.y + 300.0,
                            w: panel.w - 32.0,
                            h: 220.0,
                        };
                        ui.reorderable_list(
                            "demo_chain",
                            reorder_rect,
                            chain_ref,
                            None,
                            &reorder_style,
                            |req: ReorderableListEditRequest| match req {
                                ReorderableListEditRequest::Reorder(order) => {
                                    Edit::mutate(move |m: &mut DawModel| {
                                        let new_chain: Vec<String> = order
                                            .iter()
                                            .filter_map(|&i| m.demo_chain.get(i).cloned())
                                            .collect();
                                        m.demo_chain = new_chain;
                                        m.last_action = "Plugin Chain reordered".to_string();
                                    })
                                }
                            },
                            |ui, name, i, row, _sel, _drag| {
                                ui.label_at(
                                    ("demo_chain_label", i),
                                    name,
                                    row.x + 12.0,
                                    row.y + 6.0,
                                    13.0,
                                    Color::rgb(0.92, 0.92, 0.94),
                                );
                            },
                        );
                    },
                );
            },
        );
    }
}

fn drawmixer_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    // 8 ch を横並び。各 ch = fader + level_meter + label
    let ch_w = pane.w / N_CH as f32;
    let pad_y = 12.0;
    let label_h = 18.0;
    let meter_w = 36.0; // stereo + dB 目盛り + 数値ぶん (#074)
    let fader_w = ch_w - meter_w - 24.0;
    let body_top = pane.y + pad_y + label_h;
    let body_h = (pane.h - pad_y * 2.0 - label_h).max(60.0);

    for ch in 0..N_CH {
        let cx = pane.x + ch_w * ch as f32 + 8.0;
        // ch label
        ui.label_at(
            ("ch_lbl", ch),
            &format!("CH {}", ch + 1),
            cx,
            pane.y + 6.0,
            12.0,
            Color::rgb(0.85, 0.88, 0.92),
        );

        // fader
        let fader_rect = Rect { x: cx, y: body_top, w: fader_w, h: body_h };
        let cur = m.faders[ch];
        let _resp: FaderResponse = ui.fader_at(("ch_fader", ch), fader_rect, cur, 0.7, None, "fader", move |v| {
            Edit::mutate(move |m: &mut DawModel| m.faders[ch] = v)
        });

        // ステレオ level meter: 全 ch に dB 目盛り + 数値ピーク (Ableton Live 風、 #074)。
        // L/R を少し変えてステレオを可視化。
        let meter_rect = Rect { x: cx + fader_w + 4.0, y: body_top, w: meter_w, h: body_h };
        let l = m.sim_peak(ch);
        let r = (m.sim_peak(ch) * 0.78 + 0.05).min(1.3);
        ui.level_meter_stereo(
            ("chmeter", ch),
            meter_rect,
            l,
            r,
            MeterBallistic::Peak,
            LevelMeterStyle {
                scale: Some(MeterScale::default()),
                peak_readout: true,
                ..LevelMeterStyle::default()
            },
        );

        // mute checkbox
        let mute_rect = Rect {
            x: cx,
            y: body_top + body_h + 4.0,
            w: ch_w - 16.0,
            h: 20.0,
        };
        let _ = mute_rect;
    }
}

/// `DawTrack` 列を `ArrangementTrack` に変換 (Arc<str> のみ clone)。
fn arr_track_views(m: &DawModel) -> Vec<ArrangementTrack> {
    // M14 Phase 63c (#016): parent_id chain を辿って depth を計算 + collapsed フラグを caller 側
    // (`m.arr_collapsed_groups`) から各 track に焼き込む。 widget は depth を読むだけ (BFS は caller 責務)。
    let depth_of = |id: u32| -> u8 {
        let mut depth = 0_u8;
        let mut cur = m.arr_tracks.iter().find(|t| t.id == id).and_then(|t| t.parent_id);
        for _ in 0..64 {
            let Some(pid) = cur else {
                break;
            };
            depth = depth.saturating_add(1);
            cur = m.arr_tracks.iter().find(|t| t.id == pid).and_then(|t| t.parent_id);
        }
        depth
    };
    // M14 Phase 96 (daw_01 #068): 連動ハイライト demo — `{選択 clip} ∪ {前フレーム hovered_clip}`
    // の share_group_id 集合を作り、 同グループ member の clip に `in_active_group=true` を立てる
    // (daw_01 が model から計算して per-clip flag で渡す想定の最小再現)。
    let gid_of = |key: ClipKey| -> Option<u32> {
        m.arr_tracks
            .iter()
            .find(|t| t.id == key.track)
            .and_then(|t| t.clips.iter().find(|c| c.id == key.clip))
            .and_then(|c| c.share_group_id)
    };
    let mut active_groups: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for &k in &m.arr_selected_clips {
        if let Some(g) = gid_of(k) {
            active_groups.insert(g);
        }
    }
    if let Some(k) = m.arr_hovered_clip
        && let Some(g) = gid_of(k)
    {
        active_groups.insert(g);
    }
    m.arr_tracks
        .iter()
        .map(|t| ArrangementTrack {
            id: t.id,
            name: Arc::clone(&t.name),
            muted: t.muted,
            solo: t.solo,
            armed: t.armed,
            volume: t.volume,
            clips: t
                .clips
                .iter()
                .map(|c| ArrangementClip {
                    id: c.id,
                    start_beat: c.start_beat,
                    len_beats: c.len_beats,
                    name: Arc::clone(&c.name),
                    color: c.color,
                    // M14 Phase 63e (#019): group_id を golden-ratio hash で hue 化
                    // (隣接 group が色相的に十分離れる well-known trick)。
                    #[allow(clippy::cast_precision_loss)]
                    share_group_color: c.share_group_id.map(|gid| {
                        ((gid as f32) * 0.618_034).rem_euclid(1.0)
                    }),
                    // M14 Phase 63k (#025): caller-side `DawAudioEdit` を widget の
                    // `ArrangementClipAudioEdit` にそのまま転送。 None の clip は MIDI/Vocal 扱いで
                    // arrangement widget が audio gesture を起動しない。
                    audio_edit: c.audio_edit.map(|e| daw_ui_core::ArrangementClipAudioEdit {
                        gain_db: e.gain_db,
                        fade_in_beats: e.fade_in_beats,
                        fade_out_beats: e.fade_out_beats,
                        fade_in_curve: e.fade_in_curve,
                        fade_out_curve: e.fade_out_curve,
                    }),
                    // M14 Phase 72 (#044): daw_prototype は全 clip を audio として扱う (= 既存挙動互換)。
                    // video 編集機能の demo は daw_01 本体側で wire (gui_01 example で video frame
                    // decode しない方針、 KISS)。
                    thumbnail: None,
                    // M14 Phase 96 (daw_01 #068): 共有 clip (share_group_id = Some) のうち、
                    // selection / hover で active になったグループの member を連動強調表示する。
                    in_active_group: c
                        .share_group_id
                        .is_some_and(|g| active_groups.contains(&g)),
                })
                .collect(),
            parent_id: t.parent_id,
            depth: depth_of(t.id),
            collapsed: m.arr_collapsed_groups.contains(&t.id),
            // M14 Phase 72 (#044): daw_prototype は audio track のみ (= 既存挙動互換)。
            kind: daw_ui_core::TrackKind::Audio,
            // M14 Phase 63n-2 (#028): caller-side store (`arr_automation_lanes`) から取得 (clone)。
            // 初回 frame で `DawModel::new()` が track_id == 1 のみ sample lane (Volume / Pan) を
            // seed 済 (= 既存挙動互換、 disclosure 描画なし for 他 track)。 user 編集 (Add /
            // Move / Delete point、 SetLaneEnabled 等) は EditRequest arm が直接 store を mutate。
            automation_lanes_collapsed: m.arr_track_automation_collapsed.contains(&t.id),
            automation_lanes: m
                .arr_automation_lanes
                .get(&t.id)
                .cloned()
                .unwrap_or_default(),
            // M14 Phase 63n-6 (#031): per-track row 高さ override (新 splitter / Alt+drag 経由で
            // `SetSingleTrackRowH` を受信したら `Some(next)` に格納される)。 caller の store から取得。
            row_h: m.arr_track_row_h.get(&t.id).copied(),
            // M14 Phase 87 (#059): track 色を header 左端ストライプに反映。
            color: t.color,
        })
        .collect()
}

/// M14 Phase 63n-1 (#028): daw_prototype 視覚確認用の sample lane 生成。
/// M14 Phase 63n-2 (#028): `track_id == 0` (= "Group A"、 表示 1 番目) で Volume / Pan の 2 lane
/// を返す (group track にも automation lane を持たせる Bitwig 流、 caller の意図で任意 track id
/// に lane を付けられることを示す。 旧 Phase 63n-1 では `track_id == 1` だったが 「最初の track の
/// 下に lane」 が直感的な配置として user feedback で確定)。 各 lane は単純な curve preview と
/// clip rect の見た目確認のため、 短い point 列を持つ 1 clip 構成。
/// M14 Phase 63n-3 (#028) follow-up: src clip と同じ `share_group_color` hue を持つ全 clip に対して
/// `f` を適用する。 None (= 共有グループ未所属) のときは src clip のみに適用。 daw_01 main では
/// `Song.clip_contents` map 経由で content を共有するため content 編集が全 clip に自動波及するが、
/// daw_prototype は points を各 clip に inline で持つ簡易実装なので、 user の point edit (Add / Move /
/// Delete / Curve) で linked clone 兄弟 clip にも同じ編集を波及させる必要がある。
/// M14 Phase 63n-10b (#034 follow-up): `master_lanes` (= `arr_master_row.automation_lanes`) も
/// 走査対象に含める (= master row 内 clip も linked group の hue 一致で波及対象になる)、 src が
/// `MASTER_TRACK_ID` の lookup も同経路で OK。
fn for_each_linked_clip<F>(
    lanes_map: &mut std::collections::HashMap<u32, Vec<daw_ui_core::ArrangementAutomationLane>>,
    master_lanes: &mut Vec<daw_ui_core::ArrangementAutomationLane>,
    src: daw_ui_core::AutomationClipKey,
    mut f: F,
) where
    F: FnMut(&mut daw_ui_core::ArrangementAutomationClip),
{
    let src_lanes_ref = if src.track == MASTER_TRACK_ID {
        Some(&*master_lanes)
    } else {
        lanes_map.get(&src.track)
    };
    let src_hue = src_lanes_ref
        .and_then(|lanes| lanes.iter().find(|l| l.id == src.lane))
        .and_then(|l| l.clips.iter().find(|c| c.id == src.clip))
        .and_then(|c| c.share_group_color);
    if let Some(hue) = src_hue {
        // master + 通常 track 全 lane 群を chain で走査 (= linked group の hue 一致 clip 全部に f 適用)。
        for lanes in lanes_map.values_mut().chain(std::iter::once(&mut *master_lanes)) {
            for l in &mut *lanes {
                for c in &mut l.clips {
                    if let Some(other) = c.share_group_color
                        && (other - hue).abs() < 1e-6
                    {
                        f(c);
                    }
                }
            }
        }
    } else {
        let target_lanes = if src.track == MASTER_TRACK_ID {
            Some(&mut *master_lanes)
        } else {
            lanes_map.get_mut(&src.track)
        };
        if let Some(lanes) = target_lanes
            && let Some(l) = lanes.iter_mut().find(|l| l.id == src.lane)
            && let Some(c) = l.clips.iter_mut().find(|c| c.id == src.clip)
        {
            f(c);
        }
    }
}

/// M14 Phase 63n-10b (#034 follow-up): `track_id` から該当の lane 群への `&mut` を返す。
/// `MASTER_TRACK_ID` (= `u32::MAX`) で `arr_master_row.automation_lanes`、 それ以外で
/// `arr_automation_lanes.get_mut(&track_id)` を返す。 単一 lane 操作 (SetLaneEnabled / Visible /
/// Default / Height / DeleteLane / CreateAutomationClip / ResizeAutomationClips / DeleteAutomationClips
/// 等) の lookup で master / 通常 track の HashMap vs struct field の振り分けを 1 箇所に閉じ込める。
/// linked-group 波及 (= hue 一致で全 clip 編集) は別 helper `for_each_linked_clip` を使う。
fn lanes_mut_for_track(
    mm: &mut DawModel,
    track_id: u32,
) -> Option<&mut Vec<daw_ui_core::ArrangementAutomationLane>> {
    if track_id == MASTER_TRACK_ID {
        Some(&mut mm.arr_master_row.automation_lanes)
    } else {
        mm.arr_automation_lanes.get_mut(&track_id)
    }
}

fn build_sample_automation_lanes(track_id: u32) -> Vec<daw_ui_core::ArrangementAutomationLane> {
    if track_id != 0 {
        return Vec::new();
    }
    let volume_clip = daw_ui_core::ArrangementAutomationClip {
        id: 1,
        start_beat: 0.0,
        len_beats: 16.0,
        name: Arc::from("Volume Auto"),
        points: vec![
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 0.0,
                value_norm: 0.85,
                curve: daw_ui_core::ArrangementCurveKind::Linear,
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 4.0,
                value_norm: 0.30,
                curve: daw_ui_core::ArrangementCurveKind::Linear,
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 8.0,
                value_norm: 0.95,
                // M14 Phase 63n-7 (daw_01 #033): S 字 cubic Bezier (tension=+0.7 で滑らかな S 字)。
                curve: daw_ui_core::ArrangementCurveKind::Bezier { tension: 0.7 },
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 12.0,
                value_norm: 0.50,
                curve: daw_ui_core::ArrangementCurveKind::Hold,
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 16.0,
                value_norm: 0.70,
                // M14 Phase 63n-7 (daw_01 #033): 指数 curve (bend=+0.6 で前半遅・後半速)。
                curve: daw_ui_core::ArrangementCurveKind::Exponential { bend: 0.6 },
            },
        ],
        share_group_color: None,
    };
    let pan_clip = daw_ui_core::ArrangementAutomationClip {
        id: 2,
        start_beat: 4.0,
        len_beats: 8.0,
        name: Arc::from("Pan Auto"),
        points: vec![
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 0.0,
                value_norm: 0.50,
                curve: daw_ui_core::ArrangementCurveKind::Linear,
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 4.0,
                value_norm: 0.20,
                // M14 Phase 63n-7 (daw_01 #033): tension=+0.8 で強い S 字 (端点で水平接線に近い)。
                curve: daw_ui_core::ArrangementCurveKind::Bezier { tension: 0.8 },
            },
            daw_ui_core::ArrangementAutomationPoint {
                time_beat: 8.0,
                value_norm: 0.80,
                // M14 Phase 63n-7 (daw_01 #033): bend=-0.6 で前半速・後半遅 (平方根系)。
                curve: daw_ui_core::ArrangementCurveKind::Exponential { bend: -0.6 },
            },
        ],
        share_group_color: None,
    };
    vec![
        daw_ui_core::ArrangementAutomationLane {
            id: 1,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: daw_ui_renderer::Color::rgb(0.55, 0.85, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.80,
            clips: vec![volume_clip],
        },
        daw_ui_core::ArrangementAutomationLane {
            id: 2,
            label: Arc::from("Pan"),
            icon_glyph: 'P',
            color: daw_ui_renderer::Color::rgb(1.0, 0.75, 0.45),
            enabled: false,
            visible: true,
            height_px: 60,
            default_value_norm: 0.50,
            clips: vec![pan_clip],
        },
    ]
}

#[allow(clippy::too_many_lines)]
fn draw_arrangement_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    // M14 Phase 63n-2 (#028): arrangement の上に **lane visibility inspector** を追加。
    // arrangement widget の `👁` click で lane を hide すると lane 行が完全に消えるため
    // (= daw_01 仕様: prefix sum から外す + 隣 lane が詰まる)、 caller 側で「再表示」 する手段が
    // 必要。 daw_01 本体では `track_inspector.rs` に lane list + visible toggle を出す設計、
    // daw_prototype は最小実装としてタブ上端に 1 行で track 1 の lane visible toggle を並べる。
    // M10 Phase 49 検証用: 下部に mini mixer strip (各 track の vertical fader、DAW 慣習で
    // mixer は arrangement の下)。arrangement の volume band と **同じ `arr_tracks[i].volume`
    // source-of-truth** に bind するので、片方を drag すると drag 中もリアルタイムで他方が
    // 追従することを 1 画面で確認できる。
    let strip_h = 96.0_f32;
    let strip_pad = 8.0_f32;
    let inspector_h = 26.0_f32;
    let inspector_rect = Rect { x: pane.x, y: pane.y, w: pane.w, h: inspector_h };
    let arr_pane = Rect {
        x: pane.x,
        y: pane.y + inspector_h,
        w: pane.w,
        h: (pane.h - inspector_h - strip_h).max(100.0),
    };
    let strip_rect = Rect {
        x: pane.x,
        y: arr_pane.y + arr_pane.h,
        w: pane.w,
        h: strip_h,
    };

    // ---- M14 Phase 63n-2 (#028): lane visibility inspector ----
    // sample lane は track_id == 0 (= "Group A") に紐ついている。 widget の `👁` click で hide した
    // lane をここから再表示できる (= daw_01 仕様の `track_inspector.rs` 相当の最小実装)。
    let inspector_target_track: u32 = 0;
    ui.panel(
        "arr_lane_inspector_bg",
        inspector_rect,
        Color::rgb(0.10, 0.11, 0.13),
        0.0,
    );
    let label_text = m
        .arr_tracks
        .iter()
        .find(|t| t.id == inspector_target_track)
        .map_or_else(
            || "(no track) lanes:".to_string(),
            |t| format!("{} lanes:", t.name),
        );
    ui.push_text(daw_ui_renderer::GlyphArea {
        text: Arc::from(label_text),
        left: inspector_rect.x + 8.0,
        top: inspector_rect.y + 7.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.85, 0.85, 0.88),
        clip_rect: Some(inspector_rect),
        ..daw_ui_renderer::GlyphArea::default()
    });
    let mut bx = inspector_rect.x + 110.0;
    let btn_h = (inspector_h - 4.0).max(16.0);
    let btn_w = 100.0_f32;
    let btn_pad = 4.0_f32;
    let lanes_snapshot: Vec<(u32, bool, Arc<str>)> = m
        .arr_automation_lanes
        .get(&inspector_target_track)
        .map(|v| {
            v.iter()
                .map(|l| (l.id, l.visible, l.label.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (lane_id, visible, label) in lanes_snapshot {
        let glyph = if visible { "👁" } else { "🚫" };
        let label_str = format!("{glyph} {label}");
        let btn_rect = Rect {
            x: bx,
            y: inspector_rect.y + 2.0,
            w: btn_w,
            h: btn_h,
        };
        if bx + btn_w > inspector_rect.x + inspector_rect.w {
            break;
        }
        if ui.button_at_clicked(
            ("arr_lane_vis", inspector_target_track, lane_id),
            &label_str,
            btn_rect,
        ) {
            ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                if let Some(lanes) = mm.arr_automation_lanes.get_mut(&inspector_target_track)
                    && let Some(l) = lanes.iter_mut().find(|l| l.id == lane_id)
                {
                    l.visible = !l.visible;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action =
                    format!("inspector: lane {lane_id} visible toggle (caller-side)");
            }));
        }
        bx += btn_w + btn_pad;
    }

    // ---- M14 Phase 64a (daw_01 #035): scrubable_number widget の visual verify ----
    // inspector 行の右端に「Tempo: [scrubable BPM]」 を配置。 press + 縦 drag (= 0.5 BPM/px) で
    // scrub、 Ctrl + drag で fine (= 0.05 BPM/px)、 dblclick で 120.0 リセット、 single click で
    // text input mode (Enter で commit、 Esc で rollback)。 daw_01 #035 spec Q1=B / Q2=A / Q3=yes /
    // Q4=yes で確定済の動作を全部触れる。
    let scn_w = 70.0_f32;
    let scn_pad_right = 8.0_f32;
    let scn_rect = Rect {
        x: inspector_rect.x + inspector_rect.w - scn_w - scn_pad_right,
        y: inspector_rect.y + 2.0,
        w: scn_w,
        h: btn_h,
    };
    let scn_label_w = 50.0_f32;
    ui.push_text(daw_ui_renderer::GlyphArea {
        text: Arc::from("Tempo:"),
        left: scn_rect.x - scn_label_w - 4.0,
        top: inspector_rect.y + 7.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.85, 0.85, 0.88),
        clip_rect: Some(inspector_rect),
        ..daw_ui_renderer::GlyphArea::default()
    });
    let scn_style = daw_ui_core::ScrubableNumberStyle {
        sensitivity: 0.5, // 1 px = 0.5 BPM (Ableton 風)
        range: Some((20.0, 240.0)),
        font_size: 12.0,
        ..daw_ui_core::ScrubableNumberStyle::default()
    };
    let _ = ui.scrubable_number_at(
        "transport_bpm",
        scn_rect,
        f64::from(m.arr_view.bpm),
        120.0,
        daw_ui_core::ScrubableNumberFormat::Decimal(1),
        &scn_style,
        "scrub bpm",
        |v: f64| {
            Edit::mutate(move |mm: &mut DawModel| {
                #[allow(clippy::cast_possible_truncation)]
                let v_f32 = v as f32;
                mm.arr_view.bpm = v_f32;
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("transport: scrubable BPM → {v_f32:.1}");
            })
        },
    );

    // strip 背景
    ui.panel("arr_minimix_bg", strip_rect, Color::rgb(0.12, 0.13, 0.16), 0.0);
    // 各 track の mini fader (volume のみ)
    let n_t = m.arr_tracks.len();
    let fader_w = 30.0_f32;
    let fader_gap = 4.0_f32;
    let inner_x0 = strip_rect.x + strip_pad;
    let inner_y = strip_rect.y + strip_pad;
    let inner_h = (strip_rect.h - strip_pad * 2.0).max(20.0);
    for (i, t) in m.arr_tracks.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let fx = inner_x0 + i as f32 * (fader_w + fader_gap);
        if fx + fader_w > strip_rect.x + strip_rect.w {
            break;
        }
        let f_rect = Rect { x: fx, y: inner_y, w: fader_w, h: inner_h };
        let tid = t.id;
        let cur_vol = t.volume;
        let _ = ui.fader_at(
            ("arr_minifader", tid),
            f_rect,
            cur_vol,
            1.0,
            None,
            "track volume",
            move |v| {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(tt) = mm.arr_tracks.iter_mut().find(|t| t.id == tid) {
                        tt.volume = v.clamp(0.0, 1.0);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("minimix: track {tid} → {v:.2}");
                })
            },
        );
        // track 番号 label
        let label = format!("{}", i + 1);
        let lab_y = strip_rect.y + strip_rect.h - 14.0;
        let _ = (label, lab_y); // label 描画は省略 (Ui::label_at が必要)
    }
    let _ = n_t;

    let arr_tracks = arr_track_views(m);
    let style = ArrangementStyle::default();
    let resp = ui.arrangement(
        "arr",
        arr_pane,
        &arr_tracks,
        m.arr_view,
        &m.arr_selected_clips,
        &m.arr_selected_tracks,
        &m.arr_selected_automation_clips,
        &m.arr_selected_automation_points,
        &style,
        // M14 Phase 63n-10 (#034): SongTempo 模擬の master row を上端に表示。
        Some(&m.arr_master_row),
        move |req| match req {
            ArrangementEditRequest::SelectClips { next, .. } => {
                let next_v = next;
                Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_selected_clips = next_v;
                    mm.last_action = "arr: SelectClips".to_string();
                })
            }
            ArrangementEditRequest::SelectTrack { next, modifier, .. } => Edit::mutate(move |mm: &mut DawModel| {
                let n = next.len();
                mm.arr_selected_tracks = next;
                mm.last_action = format!("arr: SelectTrack ({n}, {modifier:?})");
            }),
            ArrangementEditRequest::MoveClips(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                for d in deltas {
                    // remove from source track
                    let removed = mm
                        .arr_tracks
                        .iter_mut()
                        .find(|t| t.id == d.from.track)
                        .and_then(|t| {
                            let pos = t.clips.iter().position(|c| c.id == d.from.clip)?;
                            Some(t.clips.remove(pos))
                        });
                    if let Some(mut clip) = removed {
                        clip.start_beat = d.next_start_beat;
                        if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.to_track) {
                            // start_beat 順に挿入
                            let pos = t
                                .clips
                                .iter()
                                .position(|c| c.start_beat > clip.start_beat)
                                .unwrap_or(t.clips.len());
                            t.clips.insert(pos, clip);
                        }
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveClips ({n})");
            }),
            // M14 Phase 63e (#019): Ctrl + drag — 「共有コピー」 意図。 source clip は残し、
            // 同じ share_group_id を持つ新 clip を `to_track` の `next_start_beat` に追加する。
            // source 側に group_id がなければ新採番、 既にあれば既存値を流用 (= 同 group に追加)。
            // daw_01 本体では content_id 共有 + Song.clip_contents map 経由で notes を共有するが、
            // prototype では「同じ group id を持つ clip 群を hue で塗り分ける」 だけで意図を表現。
            ArrangementEditRequest::CloneClipsLinked(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in deltas {
                        // 1) source の現在の group_id を取得
                        let existing_gid = mm
                            .arr_tracks
                            .iter()
                            .find(|t| t.id == d.from.track)
                            .and_then(|t| t.clips.iter().find(|c| c.id == d.from.clip))
                            .and_then(|c| c.share_group_id);
                        let group_id = existing_gid.unwrap_or_else(|| {
                            let id = mm.arr_next_share_group_id;
                            mm.arr_next_share_group_id += 1;
                            id
                        });
                        // 2) source clip に group_id を assign (もし None だったら set)
                        let src_info = mm
                            .arr_tracks
                            .iter_mut()
                            .find(|t| t.id == d.from.track)
                            .and_then(|t| {
                                t.clips.iter_mut().find(|c| c.id == d.from.clip).map(|c| {
                                    c.share_group_id = Some(group_id);
                                    (Arc::clone(&c.name), c.color, c.len_beats, c.audio_edit)
                                })
                            });
                        let Some((name, color, len_beats, src_audio_edit)) = src_info else { continue };
                        // 3) to_track に新 clip を追加 (同 group_id)
                        if let Some(target) =
                            mm.arr_tracks.iter_mut().find(|t| t.id == d.to_track)
                        {
                            let new_id = target.next_clip_id;
                            target.next_clip_id += 1;
                            let new_clip = DawClip {
                                id: new_id,
                                start_beat: d.next_start_beat,
                                len_beats,
                                name,
                                color,
                                share_group_id: Some(group_id),
                                // M14 Phase 63k (#025): linked clone は source の audio_edit も継承する
                                // (= 同 content をシェアする想定なので audio パラメータも一致)。
                                audio_edit: src_audio_edit,
                            };
                            let pos = target
                                .clips
                                .iter()
                                .position(|c| c.start_beat > new_clip.start_beat)
                                .unwrap_or(target.clips.len());
                            target.clips.insert(pos, new_clip);
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CloneClipsLinked ({n})");
                })
            }
            // M14 Phase 63e (#019): Ctrl+Shift + drag — 「独立コピー」 意図。 source clip は残し、
            // 内容を fork した独立 clip を追加する (share group には入れない、 group_id = None)。
            // daw_01 では content を deep clone + 新 ContentId 採番、 prototype では単純コピー。
            ArrangementEditRequest::CloneClipsIndependent(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in deltas {
                        let src_info = mm
                            .arr_tracks
                            .iter()
                            .find(|t| t.id == d.from.track)
                            .and_then(|t| t.clips.iter().find(|c| c.id == d.from.clip))
                            .map(|c| (Arc::clone(&c.name), c.color, c.len_beats, c.audio_edit));
                        let Some((name, color, len_beats, src_audio_edit)) = src_info else { continue };
                        if let Some(target) =
                            mm.arr_tracks.iter_mut().find(|t| t.id == d.to_track)
                        {
                            let new_id = target.next_clip_id;
                            target.next_clip_id += 1;
                            let new_clip = DawClip {
                                id: new_id,
                                start_beat: d.next_start_beat,
                                len_beats,
                                name,
                                color,
                                share_group_id: None,
                                // M14 Phase 63k (#025): independent clone でも audio_edit を継承
                                // (同 content の独立コピー = 同 audio パラメータで開始、 user は
                                // その後個別に編集できる)。
                                audio_edit: src_audio_edit,
                            };
                            let pos = target
                                .clips
                                .iter()
                                .position(|c| c.start_beat > new_clip.start_beat)
                                .unwrap_or(target.clips.len());
                            target.clips.insert(pos, new_clip);
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CloneClipsIndependent ({n})");
                })
            }
            ArrangementEditRequest::ResizeClips(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                for d in deltas {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == d.key.clip)
                    {
                        c.start_beat = d.next_start;
                        c.len_beats = d.next_len;
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ResizeClips ({n})");
            }),
            ArrangementEditRequest::DeleteClips(keys) => Edit::mutate(move |mm: &mut DawModel| {
                let n = keys.len();
                for k in keys {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == k.track) {
                        t.clips.retain(|c| c.id != k.clip);
                    }
                }
                mm.arr_selected_clips.clear();
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: DeleteClips ({n})");
            }),
            ArrangementEditRequest::DoubleClickClip(key) => Edit::mutate(move |mm: &mut DawModel| {
                mm.current_tab = 2;
                mm.last_action =
                    format!("arr: dbl-click clip → Piano Roll (track {} clip {})", key.track, key.clip);
            }),
            ArrangementEditRequest::DoubleClickEmpty { track, beat } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == track) {
                        let new_id = t.next_clip_id;
                        t.next_clip_id += 1;
                        let clip = DawClip {
                            id: new_id,
                            start_beat: beat.max(0.0),
                            len_beats: 2.0,
                            name: Arc::from(format!("new{new_id}")),
                            color: None,
                            share_group_id: None,
                            audio_edit: None,
                        };
                        let pos = t
                            .clips
                            .iter()
                            .position(|c| c.start_beat > clip.start_beat)
                            .unwrap_or(t.clips.len());
                        t.clips.insert(pos, clip);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CreateClip @ track {track} beat {beat:.2}");
                })
            }
            // M14 Phase 99 (daw_01 #071): 空きレーン右クリック → context menu を出すため stash + open
            // trigger を立てる (実メニューは draw_arrangement_tab が `ui.context_menu_at` で描画)。
            ArrangementEditRequest::SecondaryClickEmpty { track, beat, pos } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_ctx_menu = Some((track, beat, pos));
                    mm.arr_ctx_menu_open = true;
                    mm.last_action =
                        format!("arr: SecondaryClickEmpty @ track {track} beat {beat:.2}");
                })
            }
            ArrangementEditRequest::BeginRenameTrack(id) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_rename_target = Some(id);
                mm.last_action = format!("arr: BeginRenameTrack {id}");
            }),
            ArrangementEditRequest::DeleteTrack(id) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_tracks.retain(|t| t.id != id);
                mm.arr_selected_tracks.retain(|t| *t != id);
                mm.arr_collapsed_groups.remove(&id);
                // 子の parent_id が `id` を指していた場合は top-level に持ち上げる (orphan 防止)。
                for t in &mut mm.arr_tracks {
                    if t.parent_id == Some(id) {
                        t.parent_id = None;
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: DeleteTrack {id}");
            }),
            ArrangementEditRequest::MoveTrackUp(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(idx) = mm.arr_tracks.iter().position(|t| t.id == id)
                    && idx > 0
                {
                    mm.arr_tracks.swap(idx, idx - 1);
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveTrackUp {id}");
            }),
            ArrangementEditRequest::MoveTrackDown(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(idx) = mm.arr_tracks.iter().position(|t| t.id == id)
                    && idx + 1 < mm.arr_tracks.len()
                {
                    mm.arr_tracks.swap(idx, idx + 1);
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveTrackDown {id}");
            }),
            ArrangementEditRequest::SetTrackVolume { track, prev: _, next } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == track) {
                        t.volume = next.clamp(0.0, 1.0);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: SetTrackVolume {track} → {:.2}", next.clamp(0.0, 1.0));
                })
            }
            ArrangementEditRequest::ReorderTracks(order) => Edit::mutate(move |mm: &mut DawModel| {
                let n = order.len();
                // id → DawTrack の lookup table を作って、order 順で並べ直す。
                // Vec::swap_remove で順次取り出すと O(n^2) になるが N_TRACKS=12 なので問題なし。
                let mut new_tracks: Vec<DawTrack> = Vec::with_capacity(n);
                for id in &order {
                    if let Some(pos) = mm.arr_tracks.iter().position(|t| t.id == *id) {
                        new_tracks.push(mm.arr_tracks.remove(pos));
                    }
                }
                // order に含まれなかった track は末尾に keep (gui_01 widget が一部 id だけ送る semantics は
                // 無いが、防御的に)。
                new_tracks.append(&mut mm.arr_tracks);
                mm.arr_tracks = new_tracks;
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ReorderTracks ({n})");
            }),
            ArrangementEditRequest::ToggleTrackMute(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == id) {
                    t.muted = !t.muted;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleMute {id}");
            }),
            ArrangementEditRequest::ToggleTrackSolo(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == id) {
                    t.solo = !t.solo;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleSolo {id}");
            }),
            ArrangementEditRequest::ToggleTrackArmed(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == id) {
                    t.armed = !t.armed;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleArmed {id}");
            }),
            ArrangementEditRequest::SetLoopRange { start, end } => Edit::mutate(move |mm: &mut DawModel| {
                let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
                mm.arr_view.loop_range = Some((lo, hi));
                mm.last_action = format!("arr: SetLoopRange [{lo:.2}, {hi:.2}]");
            }),
            // M14 Phase 63j (#024): ruler click / drag による playhead seek。 daw_prototype は audio
            // engine を持たないので `playhead_beat` の更新だけで OK (実 daw_01 は seek IPC も発行)。
            ArrangementEditRequest::SetPlayheadBeat(beat) => Edit::mutate(move |mm: &mut DawModel| {
                let b = beat.max(0.0);
                mm.arr_view.playhead_beat = Some(b);
                mm.last_action = format!("arr: SetPlayheadBeat {b:.3}");
            }),
            ArrangementEditRequest::SetZoomX(zoom) => {
                // M14 Phase 61a (#011): widget は **絶対値 px/beat** を送る (旧 factor 直送り
                // semantic は廃止)。 example の `len_beats` は `lanes_w / zoom` で逆引き保存する。
                // header_w = 180.0 は arr_view 構築時と一致 (L118)。
                let lanes_w = f64::from((arr_pane.w - 180.0).max(1.0));
                Edit::mutate(move |mm: &mut DawModel| {
                    let z = zoom.clamp(2.0, 400.0);
                    let new_len = (lanes_w / f64::from(z)).clamp(1.0, 256.0);
                    mm.arr_view.len_beats = new_len;
                    mm.last_action = format!("arr: SetZoomX → zoom={z:.1} px/beat, len={new_len:.2}");
                })
            },
            ArrangementEditRequest::SetScrollX(start) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_view.start_beat = start.max(0.0);
            }),
            ArrangementEditRequest::SetTrackTop(top) => Edit::mutate(move |mm: &mut DawModel| {
                let max_top = (mm.arr_tracks.len() as f32 - mm.arr_view.tracks_visible)
                    .max(0.0)
                    * mm.arr_view.track_row_h;
                mm.arr_view.track_top = top.clamp(0.0, max_top);
            }),
            ArrangementEditRequest::SetTrackRowH(h) => Edit::mutate(move |mm: &mut DawModel| {
                // M10 Phase 48 / M14 Phase 63n-6 (#031): global row 高さ zoom (Alt+wheel)。
                // floor 16 / cap 1000 は readability + 画面いっぱい拡張 (#031)。 per-track override
                // (`arr_track_row_h: HashMap<u32, u16>`) は別経路 (= `SetSingleTrackRowH`) なのでここでは
                // 触らない (= override 済 track は global zoom に追従しない、 Bitwig per-track と同 idiom)。
                let new_h = h.clamp(16.0, 1000.0);
                mm.arr_view.track_row_h = new_h;
                let max_top = (mm.arr_tracks.len() as f32 - mm.arr_view.tracks_visible)
                    .max(0.0)
                    * new_h;
                mm.arr_view.track_top = mm.arr_view.track_top.clamp(0.0, max_top);
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetTrackRowH → {new_h:.1}");
            }),
            ArrangementEditRequest::SetHeaderW { prev: _, next } => Edit::mutate(move |mm: &mut DawModel| {
                // M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter drag。 widget は raw px を
                // per-frame で渡すので caller 側で実用 range に clamp (daw_01 の `80..=480` と整合)。
                let new_w = next.clamp(80.0, 480.0);
                mm.arr_view.header_w = new_w;
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetHeaderW → {new_w:.1}");
            }),
            ArrangementEditRequest::SetSingleTrackRowH { track, prev: _, next } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    // M14 Phase 63n-6 (#031): per-track row 高さ override。 caller-side clamp は
                    // `[16, 1000]` (global と同 range)、 widget は floor 1 px のみで raw u16 を渡す。
                    let new_h = next.clamp(16, 1000);
                    // M14 Phase 63n-10b (#034 follow-up): MASTER_TRACK_ID は `arr_master_row.height_px_override`
                    // を mutate (= 通常 track の `arr_track_row_h: HashMap<u32, u16>` 経路と独立、 sentinel
                    // 規約で master_row SSoT を flip)。
                    if track == MASTER_TRACK_ID {
                        mm.arr_master_row.height_px_override = Some(new_h);
                    } else {
                        mm.arr_track_row_h.insert(track, new_h);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: SetSingleTrackRowH t{track} → {new_h} px");
                })
            }
            ArrangementEditRequest::ToggleGroupCollapsed(id) => Edit::mutate(move |mm: &mut DawModel| {
                if mm.arr_collapsed_groups.contains(&id) {
                    mm.arr_collapsed_groups.remove(&id);
                } else {
                    mm.arr_collapsed_groups.insert(id);
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleGroupCollapsed {id}");
            }),
            ArrangementEditRequest::SetTrackParent { tracks, parent, anchor_after } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = tracks.len();
                    // (1) source tracks を arr_tracks から remove (順序維持)
                    let mut removed: Vec<DawTrack> = Vec::with_capacity(n);
                    for tid in &tracks {
                        if let Some(pos) = mm.arr_tracks.iter().position(|t| t.id == *tid) {
                            removed.push(mm.arr_tracks.remove(pos));
                        }
                    }
                    // (2) parent_id を更新
                    for t in &mut removed {
                        t.parent_id = parent;
                    }
                    // (3) anchor_after 直後に insert (None で先頭)
                    let insert_at = match anchor_after {
                        Some(aid) => mm
                            .arr_tracks
                            .iter()
                            .position(|t| t.id == aid)
                            .map_or(0, |i| i + 1),
                        None => 0,
                    };
                    for (i, t) in removed.into_iter().enumerate() {
                        mm.arr_tracks.insert(insert_at + i, t);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: SetTrackParent ({n} → {parent:?}, after {anchor_after:?})"
                    );
                })
            }
            // M14 Phase 63k (#025): audio clip の inline 編集 → 該当 clip の `audio_edit.{field}` を更新。
            // daw_prototype は track 3-5 ("Audio 1/2/3") の clip に `Some(DawAudioEdit { ... })` を
            // 持たせており、 widget が dB handle line / fade 角 grip を描画 + drag handler を bind。
            // 実 daw_01 では `AppEvent::SetClipGainDb { target, gain_db }` 等に変換して audio engine に
            // 転送するが、 prototype では caller-side state の直接 mutation で十分。
            ArrangementEditRequest::SetClipGainDb(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                let mut summary = String::new();
                for d in &deltas {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == d.key.clip)
                        && let Some(audio) = c.audio_edit.as_mut()
                    {
                        audio.gain_db = d.next_gain_db;
                        if summary.is_empty() {
                            summary = format!(
                                "{}{:.1} dB (clip {}/{})",
                                if d.next_gain_db >= 0.0 { "+" } else { "" },
                                d.next_gain_db,
                                d.key.track,
                                d.key.clip
                            );
                        }
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetClipGainDb ({n}) → {summary}");
            }),
            ArrangementEditRequest::SetClipFade(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                let mut summary = String::new();
                for d in &deltas {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == d.key.clip)
                        && let Some(audio) = c.audio_edit.as_mut()
                    {
                        match d.edge {
                            daw_ui_core::FadeEdge::In => audio.fade_in_beats = d.next_beats,
                            daw_ui_core::FadeEdge::Out => audio.fade_out_beats = d.next_beats,
                        }
                        if summary.is_empty() {
                            summary = format!(
                                "{:?}={:.2} beats (clip {}/{})",
                                d.edge,
                                d.next_beats,
                                d.key.track,
                                d.key.clip
                            );
                        }
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetClipFade ({n}) → {summary}");
            }),
            ArrangementEditRequest::SetClipFadeCurve(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                let mut summary = String::new();
                for d in &deltas {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == d.key.clip)
                        && let Some(audio) = c.audio_edit.as_mut()
                    {
                        match d.edge {
                            daw_ui_core::FadeEdge::In => audio.fade_in_curve = d.next_curve,
                            daw_ui_core::FadeEdge::Out => audio.fade_out_curve = d.next_curve,
                        }
                        if summary.is_empty() {
                            summary = format!(
                                "{:?}={} (clip {}/{})",
                                d.edge,
                                d.next_curve.name(),
                                d.key.track,
                                d.key.clip
                            );
                        }
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetClipFadeCurve ({n}) → {summary}");
            }),
            ArrangementEditRequest::ToggleTrackAutomationCollapsed { track } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if track == MASTER_TRACK_ID {
                        // M14 Phase 63n-10 (#034): master row 専用 toggle (= 通常 track の HashSet 経路と
                        // 独立、 master_row.automation_lanes_collapsed SSoT を直接 flip)。
                        mm.arr_master_row.automation_lanes_collapsed =
                            !mm.arr_master_row.automation_lanes_collapsed;
                    } else if mm.arr_track_automation_collapsed.contains(&track) {
                        mm.arr_track_automation_collapsed.remove(&track);
                    } else {
                        mm.arr_track_automation_collapsed.insert(track);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: ToggleTrackAutomationCollapsed {track}");
                })
            }
            // M14 Phase 63n-2 (#028): lane header button (★/👁/✕) + default band drag。
            ArrangementEditRequest::SetLaneEnabled { lane, enabled } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, lane.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == lane.lane)
                    {
                        l.enabled = enabled;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: SetLaneEnabled t{} l{} → {}", lane.track, lane.lane, enabled);
                })
            }
            ArrangementEditRequest::SetLaneVisible { lane, visible } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, lane.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == lane.lane)
                    {
                        l.visible = visible;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: SetLaneVisible t{} l{} → {}", lane.track, lane.lane, visible);
                })
            }
            ArrangementEditRequest::SetLaneDefault { lane, prev: _, next } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, lane.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == lane.lane)
                    {
                        l.default_value_norm = next.clamp(0.0, 1.0);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: SetLaneDefault t{} l{} → {:.2}", lane.track, lane.lane, next);
                })
            }
            // M14 Phase 63n-5 (#030): lane 下端 splitter drag による高さ変更。 widget 側で
            // [min, max] = [30, 200] (style 既定) に clamp 済 — caller は別 clamp 不要。
            // drag 中は per-frame 受信 → caller が `lane.height_px = next` で反映 → 次 frame で
            // lane 行高さが伸び縮みする様子が cached 描画にそのまま乗る。
            ArrangementEditRequest::SetLaneHeight { lane, prev: _, next } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, lane.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == lane.lane)
                    {
                        l.height_px = next;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: SetLaneHeight t{} l{} → {} px", lane.track, lane.lane, next);
                })
            }
            ArrangementEditRequest::DeleteLane(lane) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, lane.track) {
                        lanes.retain(|l| l.id != lane.lane);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: DeleteLane t{} l{}", lane.track, lane.lane);
                })
            }
            // M14 Phase 63n-2 (#028): lane body 内 point の add / move / delete / curve 切替。
            // M14 Phase 63n-3 (#028) follow-up: share_group_color 一致 (= linked clone 兄弟) 全 clip に
            // 編集を波及させる (daw_01 main の `Song.clip_contents` 共有と同等の動作を prototype 内で再現)。
            ArrangementEditRequest::AddAutomationPoint {
                clip,
                time_beat,
                value_norm,
            } => Edit::mutate(move |mm: &mut DawModel| {
                for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, clip, |c| {
                    // Linear curve で point を挿入。 time_beat 昇順を保つよう insert 位置を計算。
                    let pos = c
                        .points
                        .iter()
                        .position(|p| p.time_beat > time_beat)
                        .unwrap_or(c.points.len());
                    c.points.insert(
                        pos,
                        daw_ui_core::ArrangementAutomationPoint {
                            time_beat,
                            value_norm,
                            curve: daw_ui_core::ArrangementCurveKind::Linear,
                        },
                    );
                });
                mm.arr_view.data_generation += 1;
                mm.last_action = format!(
                    "arr: AddAutomationPoint t{} l{} c{} @{:.2} v={:.2}",
                    clip.track, clip.lane, clip.clip, time_beat, value_norm
                );
            }),
            // M14 Phase 63n-4 (#029): lane body 内 clip ギャップでの dblclick → 新規 automation clip 作成。
            // widget は snap 適用済 start_beat と style 既定 len_beats を渡す。 prototype は単純に lane に
            // 新 clip を追加 (新 id = 既存 max+1、 share_group なし、 Linear curve の point 1 個 [0.0, default])。
            // 「次 clip 直前まで cap」 等の高度 policy は daw_01 本体実装で扱う想定 (prototype は最小実装)。
            ArrangementEditRequest::CreateAutomationClip {
                lane,
                start_beat,
                len_beats,
            } => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(lanes) = lanes_mut_for_track(mm, lane.track)
                    && let Some(l) = lanes.iter_mut().find(|l| l.id == lane.lane)
                {
                    let new_id =
                        l.clips.iter().map(|c| c.id).max().unwrap_or(0).wrapping_add(1);
                    let default_norm = l.default_value_norm.clamp(0.0, 1.0);
                    let new_clip = daw_ui_core::ArrangementAutomationClip {
                        id: new_id,
                        start_beat: start_beat.max(0.0),
                        len_beats: len_beats.max(0.25),
                        name: Arc::from(format!("auto{new_id}")),
                        // 新 clip は default_value_norm を持つ 1 point から始める (= flat curve)。
                        // user が dblclick で point を追加して curve を肉付けしていく想定。
                        points: vec![daw_ui_core::ArrangementAutomationPoint {
                            time_beat: 0.0,
                            value_norm: default_norm,
                            curve: daw_ui_core::ArrangementCurveKind::Linear,
                        }],
                        share_group_color: None,
                    };
                    let pos = l
                        .clips
                        .iter()
                        .position(|c| c.start_beat > new_clip.start_beat)
                        .unwrap_or(l.clips.len());
                    l.clips.insert(pos, new_clip);
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: CreateAutomationClip t{} l{} @{:.2} len={:.2}",
                        lane.track, lane.lane, start_beat, len_beats
                    );
                }
            }),
            ArrangementEditRequest::MoveAutomationPoints(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in &deltas {
                        let idx = d.point.point_idx as usize;
                        let next_t = d.next_time_beat;
                        let next_v = d.next_value_norm;
                        for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, d.point.clip, |c| {
                            if let Some(p) = c.points.get_mut(idx) {
                                p.time_beat = next_t;
                                p.value_norm = next_v;
                            }
                            // 同 frame 内で sort も実行 (frame 内の point_idx は変動しうる、 caller 仕様 §11.2)。
                            c.points.sort_by(|a, b| {
                                a.time_beat
                                    .partial_cmp(&b.time_beat)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        });
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: MoveAutomationPoints ({n})");
                })
            }
            ArrangementEditRequest::DeleteAutomationPoints(keys) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = keys.len();
                    // 同じ clip 内の複数 point 削除に対応するため、 削除 index を **降順** sort して remove。
                    let mut grouped: std::collections::BTreeMap<(u32, u32, u32), Vec<u32>> =
                        std::collections::BTreeMap::new();
                    for k in &keys {
                        grouped
                            .entry((k.clip.track, k.clip.lane, k.clip.clip))
                            .or_default()
                            .push(k.point_idx);
                    }
                    for ((track, lane_id, clip_id), mut indices) in grouped {
                        indices.sort_by(|a, b| b.cmp(a)); // 降順
                        let src_key = daw_ui_core::AutomationClipKey {
                            track,
                            lane: lane_id,
                            clip: clip_id,
                        };
                        for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, src_key, |c| {
                            for idx in &indices {
                                let i = *idx as usize;
                                if i < c.points.len() {
                                    c.points.remove(i);
                                }
                            }
                        });
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: DeleteAutomationPoints ({n})");
                })
            }
            ArrangementEditRequest::SetAutomationCurveType { point, prev: _, next } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let idx = point.point_idx as usize;
                    for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, point.clip, |c| {
                        if let Some(p) = c.points.get_mut(idx) {
                            p.curve = next;
                        }
                    });
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: SetAutomationCurveType t{} l{} c{} p{} → {:?}",
                        point.clip.track, point.clip.lane, point.clip.clip, point.point_idx, next
                    );
                })
            }
            // M14 Phase 63n-3 (#028): automation clip drag の 5 variant。 source lane から clip を
            // 取り出して `next_start_beat` に更新後 target lane に挿入 (start_beat 昇順)、 lane 跨ぎ
            // 不一致は accept (Bitwig 流、 #028 [Resolved] follow-up 1)。 Linked / Independent clone は
            // source clip 残置 + 同 / 新 share_group hue で複製 (MIDI clip clone 同 idiom)、
            // ただし daw_prototype では points だけ deep clone する (content store は持たない簡易実装)。
            ArrangementEditRequest::MoveAutomationClips(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in &deltas {
                        // (1) source lane から clip を取り出す
                        let removed = lanes_mut_for_track(mm, d.from.track).and_then(|lanes| {
                            lanes.iter_mut().find(|l| l.id == d.from.lane).and_then(|l| {
                                let pos = l.clips.iter().position(|c| c.id == d.from.clip)?;
                                Some(l.clips.remove(pos))
                            })
                        });
                        if let Some(mut clip) = removed {
                            clip.start_beat = d.next_start_beat;
                            // (2) target lane に挿入 (start_beat 昇順)
                            if let Some(lanes) = lanes_mut_for_track(mm, d.to_lane.track)
                                && let Some(l) = lanes.iter_mut().find(|l| l.id == d.to_lane.lane)
                            {
                                let pos = l
                                    .clips
                                    .iter()
                                    .position(|c| c.start_beat > clip.start_beat)
                                    .unwrap_or(l.clips.len());
                                l.clips.insert(pos, clip);
                            }
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: MoveAutomationClips ({n})");
                })
            }
            ArrangementEditRequest::CloneAutomationClipsLinked(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in &deltas {
                        // source の現在の share_group_color を取得 (None なら新採番、 既にあれば流用)。
                        let existing_hue = mm
                            .arr_automation_lanes
                            .get(&d.from.track)
                            .and_then(|lanes| lanes.iter().find(|l| l.id == d.from.lane))
                            .and_then(|l| l.clips.iter().find(|c| c.id == d.from.clip))
                            .and_then(|c| c.share_group_color);
                        let hue = existing_hue.unwrap_or_else(|| {
                            // golden ratio で hue 採番 (= 隣接 group が色相的に十分離れる)
                            let id = mm.arr_next_share_group_id;
                            mm.arr_next_share_group_id += 1;
                            (f32::from(u16::try_from(id).unwrap_or(u16::MAX)) * 0.618_034)
                                .rem_euclid(1.0)
                        });
                        // source clip に hue を assign + name / len / points を取得
                        let src_info = mm
                            .arr_automation_lanes
                            .get_mut(&d.from.track)
                            .and_then(|lanes| lanes.iter_mut().find(|l| l.id == d.from.lane))
                            .and_then(|l| {
                                l.clips.iter_mut().find(|c| c.id == d.from.clip).map(|c| {
                                    c.share_group_color = Some(hue);
                                    (
                                        Arc::clone(&c.name),
                                        c.len_beats,
                                        c.points.clone(),
                                    )
                                })
                            });
                        let Some((name, len_beats, points)) = src_info else { continue };
                        // target lane に新 clip 追加 (新 id 採番、 同 hue を共有)
                        if let Some(lanes) = lanes_mut_for_track(mm, d.to_lane.track)
                            && let Some(l) = lanes.iter_mut().find(|l| l.id == d.to_lane.lane)
                        {
                            // 新 clip id = lane 内 max + 1 (簡易採番、 conflict なし)
                            let new_id =
                                l.clips.iter().map(|c| c.id).max().unwrap_or(0).wrapping_add(1);
                            let new_clip = daw_ui_core::ArrangementAutomationClip {
                                id: new_id,
                                start_beat: d.next_start_beat,
                                len_beats,
                                name,
                                points,
                                share_group_color: Some(hue),
                            };
                            let pos = l
                                .clips
                                .iter()
                                .position(|c| c.start_beat > new_clip.start_beat)
                                .unwrap_or(l.clips.len());
                            l.clips.insert(pos, new_clip);
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CloneAutomationClipsLinked ({n})");
                })
            }
            ArrangementEditRequest::CloneAutomationClipsIndependent(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in &deltas {
                        let src_info = mm
                            .arr_automation_lanes
                            .get(&d.from.track)
                            .and_then(|lanes| lanes.iter().find(|l| l.id == d.from.lane))
                            .and_then(|l| l.clips.iter().find(|c| c.id == d.from.clip))
                            .map(|c| (Arc::clone(&c.name), c.len_beats, c.points.clone()));
                        let Some((name, len_beats, points)) = src_info else { continue };
                        if let Some(lanes) = lanes_mut_for_track(mm, d.to_lane.track)
                            && let Some(l) = lanes.iter_mut().find(|l| l.id == d.to_lane.lane)
                        {
                            let new_id =
                                l.clips.iter().map(|c| c.id).max().unwrap_or(0).wrapping_add(1);
                            let new_clip = daw_ui_core::ArrangementAutomationClip {
                                id: new_id,
                                start_beat: d.next_start_beat,
                                len_beats,
                                name,
                                points,
                                share_group_color: None, // independent clone は share group に入れない
                            };
                            let pos = l
                                .clips
                                .iter()
                                .position(|c| c.start_beat > new_clip.start_beat)
                                .unwrap_or(l.clips.len());
                            l.clips.insert(pos, new_clip);
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CloneAutomationClipsIndependent ({n})");
                })
            }
            ArrangementEditRequest::ResizeAutomationClips(deltas) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = deltas.len();
                    for d in &deltas {
                        if let Some(lanes) = lanes_mut_for_track(mm, d.key.track)
                            && let Some(l) = lanes.iter_mut().find(|l| l.id == d.key.lane)
                            && let Some(c) = l.clips.iter_mut().find(|c| c.id == d.key.clip)
                        {
                            c.start_beat = d.next_start;
                            c.len_beats = d.next_len;
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: ResizeAutomationClips ({n})");
                })
            }
            ArrangementEditRequest::DeleteAutomationClips(keys) => {
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = keys.len();
                    for k in &keys {
                        if let Some(lanes) = lanes_mut_for_track(mm, k.track)
                            && let Some(l) = lanes.iter_mut().find(|l| l.id == k.lane)
                        {
                            l.clips.retain(|c| c.id != k.clip);
                        }
                    }
                    mm.arr_selected_automation_clips.retain(|k| !keys.contains(k));
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: DeleteAutomationClips ({n})");
                })
            }
            ArrangementEditRequest::SelectAutomationClips { next, .. } => {
                let next_v = next;
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = next_v.len();
                    mm.arr_selected_automation_clips = next_v;
                    mm.last_action = format!("arr: SelectAutomationClips ({n})");
                })
            }
            // M14 Phase 63n-8 (#033): lasso + point 短 click による point selection の更新。
            // 単純に上書き — caller 側の追加処理 (例: undo 履歴に push) は別 commit でも対応可。
            ArrangementEditRequest::SelectAutomationPoints { next, .. } => {
                let next_v = next;
                Edit::mutate(move |mm: &mut DawModel| {
                    let n = next_v.len();
                    mm.arr_selected_automation_points = next_v;
                    mm.last_action = format!("arr: SelectAutomationPoints ({n})");
                })
            }
            // M14 Phase 63n-9 (#033): tension/bend handle drag release → curve param 更新。
            // kind で `Bezier { tension }` / `Exponential { bend }` を分岐、 該当 point の curve を新値で
            // 上書き。 linked clips (share_group_color 一致) 全部に伝播 (= for_each_linked_clip)、 既存
            // SetAutomationCurveType と同 idiom。
            ArrangementEditRequest::SetAutomationCurveParam {
                point,
                kind,
                next_value,
                ..
            } => Edit::mutate(move |mm: &mut DawModel| {
                let idx = point.point_idx as usize;
                for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, point.clip, |c| {
                    if let Some(p) = c.points.get_mut(idx) {
                        p.curve = match kind {
                            daw_ui_core::SetAutomationCurveParamKind::BezierTension => {
                                daw_ui_core::ArrangementCurveKind::Bezier { tension: next_value }
                            }
                            daw_ui_core::SetAutomationCurveParamKind::ExponentialBend => {
                                daw_ui_core::ArrangementCurveKind::Exponential { bend: next_value }
                            }
                        };
                    }
                });
                mm.arr_view.data_generation += 1;
                let kind_str = match kind {
                    daw_ui_core::SetAutomationCurveParamKind::BezierTension => "tension",
                    daw_ui_core::SetAutomationCurveParamKind::ExponentialBend => "bend",
                };
                mm.last_action = format!(
                    "arr: SetAutomationCurveParam p{} {} → {:+.2}",
                    point.point_idx, kind_str, next_value
                );
            }),
        },
    );

    // M14 Phase 96 (daw_01 #068): 連動ハイライト用に hovered_clip を 1 フレーム保持する。
    // 変化した時だけ Edit を積む (= 毎フレーム無駄な mutate を避ける、 request_redraw は host が担当)。
    if m.arr_hovered_clip != resp.hovered_clip {
        let hv = resp.hovered_clip;
        ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
            mm.arr_hovered_clip = hv;
        }));
    }

    // ---- track header 右クリック context_menu (Rename / Color / Delete) ----
    for (track_id, header_rect) in &resp.track_header_rects {
        let tid = *track_id;
        ui.context_menu_for(*header_rect, &["Rename", "Color...", "Delete"], move |idx, ui| {
            match idx {
                0 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = Some(tid);
                    mm.last_action = format!("arr: Rename {tid} (context)");
                })),
                // M14 Phase 88 (#058): color_picker を開く (target に track id をセット)。
                1 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_color_picker_target = Some(tid);
                    mm.last_action = format!("arr: Color picker open {tid}");
                })),
                2 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_tracks.retain(|t| t.id != tid);
                    mm.arr_selected_tracks.retain(|t| *t != tid);
                    mm.arr_collapsed_groups.remove(&tid);
                    for t in &mut mm.arr_tracks {
                        if t.parent_id == Some(tid) {
                            t.parent_id = None;
                        }
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: Delete {tid} (context)");
                })),
                _ => {}
            }
        });
    }

    // ---- M14 Phase 88 (#058): color_picker overlay (target track が Some の間 1 frame ごとに描画) ----
    if let Some(tid) = m.arr_color_picker_target {
        let anchor = resp
            .track_header_rects
            .iter()
            .find(|(id, _)| *id == tid)
            .map(|(_, r)| *r);
        if let Some(anchor) = anchor {
            let current = m
                .arr_tracks
                .iter()
                .find(|t| t.id == tid)
                .and_then(|t| t.color)
                .unwrap_or(Color::rgb(0.5, 0.5, 0.5));
            let style = ColorPickerStyle::default();
            let r = ui.color_picker(tid, anchor, current, &TRACK_COLOR_PALETTE, &style);
            if let Some(c) = r.picked {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == tid) {
                        t.color = Some(c);
                    }
                    mm.last_action = format!("arr: Track {tid} 色変更");
                }));
            }
            if r.dismissed {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_color_picker_target = None;
                }));
            }
        } else {
            // target track が消えた (Delete 等) → picker を閉じる。
            ui.push_edit(Edit::mutate(|mm: &mut DawModel| {
                mm.arr_color_picker_target = None;
            }));
        }
    }

    // ---- M14 Phase 99 (daw_01 #071): 空きレーン右クリック → context menu (Text クリップ生成) ----
    // `SecondaryClickEmpty` を受けて stash した `(track, beat, pos)` を使い、毎フレーム
    // `ui.context_menu_at` で `pos` にメニューを描画する (REAPER の右クリック空きエリア → Insert
    // new item idiom)。open_at は 1-shot flag で 1 フレームだけ `Some(pos)` を渡す。on_select で
    // stash した track / beat に Text クリップを生成する (= daw_01 が想定する使い方の最小再現)。
    if let Some((track, beat, pos)) = m.arr_ctx_menu {
        let open_at = if m.arr_ctx_menu_open { Some(pos) } else { None };
        if m.arr_ctx_menu_open {
            ui.push_edit(Edit::mutate(|mm: &mut DawModel| {
                mm.arr_ctx_menu_open = false;
            }));
        }
        ui.context_menu_at("arr_secondary_menu", open_at, &["Text クリップ"], move |idx, ui| {
            if idx == 0 {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == track) {
                        let new_id = t.next_clip_id;
                        t.next_clip_id += 1;
                        let clip = DawClip {
                            id: new_id,
                            start_beat: beat.max(0.0),
                            len_beats: 2.0,
                            name: Arc::from(format!("text{new_id}")),
                            color: None,
                            share_group_id: None,
                            audio_edit: None,
                        };
                        let p = t
                            .clips
                            .iter()
                            .position(|c| c.start_beat > clip.start_beat)
                            .unwrap_or(t.clips.len());
                        t.clips.insert(p, clip);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.arr_ctx_menu = None;
                    mm.last_action = format!(
                        "arr: Text clip @ track {track} beat {beat:.2} (pos {:.0},{:.0})",
                        pos.0, pos.1
                    );
                }));
            }
        });
    }

    // ---- M14 Phase 63n-2 (#028): automation point 右クリック → curve type popup ----
    // `resp.automation_point_rects` を毎 frame loop して context_menu_for を呼ぶ (= clip_rects と同
    // idiom)。 anchor_rect は point dot の周辺 (8x8 px @ default radius=4)、 caller の右クリックを
    // context_menu_for が anchor_rect.contains で判定 → popup を立ち上げる。 prev curve は popup
    // open 時点の `clip.points[point_idx].curve` を retrieve (#028 [Resolved] と同 idiom)。
    // resp は owned value (ArrangementResponse は Clone) で ui への borrow を持たないので、
    // `&resp.automation_point_rects` で iter しつつ closure 内 `ui.push_edit` 可能 (clone 不要)。
    for &(point_key, anchor_rect) in &resp.automation_point_rects {
        let prev_curve = m
            .arr_automation_lanes
            .get(&point_key.clip.track)
            .and_then(|lanes| lanes.iter().find(|l| l.id == point_key.clip.lane))
            .and_then(|l| l.clips.iter().find(|c| c.id == point_key.clip.clip))
            .and_then(|c| c.points.get(point_key.point_idx as usize))
            .map_or(daw_ui_core::ArrangementCurveKind::Linear, |p| p.curve);
        ui.context_menu_for(
            anchor_rect,
            &["Hold", "Linear", "Bezier", "Exponential"],
            move |idx, ui| {
                let next = match idx {
                    0 => daw_ui_core::ArrangementCurveKind::Hold,
                    1 => daw_ui_core::ArrangementCurveKind::Linear,
                    2 => daw_ui_core::ArrangementCurveKind::Bezier { tension: 0.5 },
                    3 => daw_ui_core::ArrangementCurveKind::Exponential { bend: 0.5 },
                    _ => return,
                };
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    let idx = point_key.point_idx as usize;
                    for_each_linked_clip(&mut mm.arr_automation_lanes, &mut mm.arr_master_row.automation_lanes, point_key.clip, |c| {
                        if let Some(p) = c.points.get_mut(idx) {
                            p.curve = next;
                        }
                    });
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: SetAutomationCurveType (popup) p{} → {:?} (prev {:?})",
                        point_key.point_idx, next, prev_curve
                    );
                }));
            },
        );
    }

    // ---- M14 Phase 63n-3 (#028): automation clip 右クリック context_menu (Make Unique / Delete) ----
    // `resp.automation_clip_rects` は visible-tracks / lane 順 / 描画順で並ぶ (collapsed group 内 /
    // collapsed lane / invisible lane / off-screen は除外)。 Make Unique は share_group_color を
    // None にして共有グループから外す (Linked clone から独立させる、 daw_01 #028 仕様縮約版)、
    // Delete は `DeleteAutomationClips` 経由で発火 (widget は trigger を提供しないため caller-driven)。
    for (clip_key, clip_rect) in &resp.automation_clip_rects {
        let key = *clip_key;
        ui.context_menu_for(*clip_rect, &["Make Unique", "Delete"], move |idx, ui| {
            match idx {
                0 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, key.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == key.lane)
                        && let Some(c) = l.clips.iter_mut().find(|c| c.id == key.clip)
                    {
                        c.share_group_color = None;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: Make Unique automation t{} l{} c{} (context)",
                        key.track, key.lane, key.clip
                    );
                })),
                1 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(lanes) = lanes_mut_for_track(mm, key.track)
                        && let Some(l) = lanes.iter_mut().find(|l| l.id == key.lane)
                    {
                        l.clips.retain(|c| c.id != key.clip);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!(
                        "arr: Delete automation t{} l{} c{} (context)",
                        key.track, key.lane, key.clip
                    );
                })),
                _ => {}
            }
        });
    }

    // ---- M14 Phase 63f (#020): clip 右クリック context_menu (Make Unique / Delete) ----
    // `resp.clip_rects` は visible-tracks 順 / draw 順で並ぶ (collapsed 子 / off-screen は除外)。
    // Make Unique は share_group_id を None にして共有グループから外す (daw_01 #020 仕様の縮約版、
    // daw_prototype は Song.clip_contents 相当を持たないため content fork は不要)。
    for (clip_key, clip_rect) in &resp.clip_rects {
        let key = *clip_key;
        ui.context_menu_for(*clip_rect, &["Make Unique", "Delete"], move |idx, ui| {
            match idx {
                0 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == key.clip)
                    {
                        c.share_group_id = None;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: Make Unique track={} clip={} (context)", key.track, key.clip);
                })),
                1 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == key.track) {
                        t.clips.retain(|c| c.id != key.clip);
                    }
                    mm.arr_selected_clips.retain(|k| *k != key);
                    mm.arr_view.data_generation += 1;
                    mm.last_action =
                        format!("arr: Delete track={} clip={} (context)", key.track, key.clip);
                })),
                _ => {}
            }
        });
    }

    // ---- Rename UI overlay (text_input_at を該当 header rect 上に重ねる) ----
    if let Some(rid) = m.arr_rename_target {
        // 該当 track の現名 + header rect を引く
        let cur_name = m
            .arr_tracks
            .iter()
            .find(|t| t.id == rid)
            .map(|t| t.name.to_string())
            .unwrap_or_default();
        let header_rect = resp
            .track_header_rects
            .iter()
            .find(|(id, _)| *id == rid)
            .map(|(_, r)| *r);
        if let Some(rect) = header_rect {
            let pad = 4.0_f32;
            let text_rect = Rect {
                x: rect.x + pad,
                y: rect.y + pad,
                w: (rect.w - pad * 2.0).max(20.0),
                h: (rect.h - pad * 2.0).max(20.0),
            };
            // text_input は背景塗りを持たないため、後ろの track header text が透けて見える。
            // overlay 用に不透明 panel を先に置く (text_input より一段下、glyph より上に来る)。
            ui.panel(
                ("arr_rename_bg", rid),
                text_rect,
                Color::rgb(0.18, 0.20, 0.24),
                3.0,
            );
            // M11 Phase 52 (daw_01 #013): `text_input_at_focused` で「初回 show 自動 focus」を
            // widget に内蔵 — 旧 `arr_rename_just_started` boilerplate (caller 側で
            // `WidgetId::ROOT.child((b"text_input", &id))` を再現して `set_focus` を呼ぶ) を
            // 完全削除。`arr_rename_target = Some(rid)` だけで Logic / Bitwig 慣習の
            // 「Rename → 即タイプ可能」が成立する。
            //
            // on_change では track 名だけ更新 (`arr_rename_target` は触らない、
            // overlay 消去は Enter (resp.committed) / ESC (take_shortcut) で行う)。
            let resp_text = ui.text_input_at_focused(
                ("arr_rename", rid),
                text_rect,
                &cur_name,
                move |new| {
                    Edit::mutate(move |mm: &mut DawModel| {
                        if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == rid) {
                            t.name = Arc::from(new.as_str());
                        }
                        mm.arr_view.data_generation += 1;
                        mm.last_action = format!("arr: rename → {new}");
                    })
                },
            );
            // Enter で確定 → overlay 消去
            if resp_text.committed {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = None;
                    mm.last_action = format!("arr: Rename {rid} committed");
                }));
            }
            // ESC でキャンセル
            if ui.take_shortcut("escape") {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = None;
                    mm.last_action = "arr: Rename cancelled".to_string();
                }));
            }
        } else {
            // header_rect が引けない (track 削除済) → クリア
            ui.push_edit(Edit::mutate(|mm: &mut DawModel| {
                mm.arr_rename_target = None;
            }));
        }
    }
}

fn draw_piano_roll_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    let mapping = TimeMapping::default_4_4_120();
    let viewport = m.arr_viewport;
    let ruler_h = 24.0;
    let key_w = 36.0;
    let ruler_rect = Rect { x: pane.x + key_w, y: pane.y, w: pane.w - key_w, h: ruler_h };
    let grid_rect = Rect {
        x: pane.x + key_w,
        y: pane.y + ruler_h,
        w: pane.w - key_w,
        h: (pane.h - ruler_h).max(50.0),
    };
    ui.time_ruler("pr_ruler", ruler_rect, mapping, viewport, TimeRulerStyle::default());
    ui.push_rect(RectCommand {
        rect: grid_rect,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    ui.bar_beat_grid("pr_grid", grid_rect, mapping, viewport, BarBeatGridStyle::default());

    // keyboard sidebar
    ui.push_rect(RectCommand {
        rect: Rect { x: pane.x, y: pane.y + ruler_h, w: key_w, h: grid_rect.h },
        fill: Color::rgb(0.85, 0.85, 0.88),
        border: Color::rgb(0.30, 0.32, 0.36),
        border_width: 1.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let n_keys = 24; // 2 octaves
    let key_h = (grid_rect.h / n_keys as f32).max(8.0);
    for k in 0..n_keys {
        let pitch = 60 + k; // C4 から
        let is_black = matches!(pitch % 12, 1 | 3 | 6 | 8 | 10);
        if is_black {
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: pane.x,
                    y: grid_rect.y + key_h * k as f32,
                    w: key_w * 0.7,
                    h: key_h - 1.0,
                },
                fill: Color::rgb(0.10, 0.10, 0.12),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
    }

    // 仮 note (5 個) を等間隔配置
    for i in 0..5 {
        let note_start_s = f64::from(i) * 24_000.0 + 12_000.0; // 0.25 sec stagger
        let note_len_s = 18_000.0;
        let pitch_offset = (i as f32) * key_h;
        let nx = grid_rect.x + viewport.unit_to_px(note_start_s, grid_rect.w);
        let nw = viewport.unit_to_px(note_len_s, grid_rect.w);
        if nx + nw < grid_rect.x || nx > grid_rect.x + grid_rect.w {
            continue;
        }
        ui.push_rect(RectCommand {
            rect: Rect { x: nx, y: grid_rect.y + pitch_offset, w: nw, h: key_h - 2.0 },
            fill: Color::rgb(0.42, 0.85, 0.95),
            border: Color::rgb(0.30, 0.55, 0.78),
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: Some(grid_rect),
        });
    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        self.input.ingest(&ev);
        match ev {
            AppEvent::Resized(s) => {
                self.renderer.resize(s);
                self.window.request_redraw();
            }
            // winit ControlFlow::Wait では入力イベントだけでは再描画されないため、
            // 入力が来たら明示的に request_redraw して build_ui を走らせる (mixer 同パターン)。
            AppEvent::PointerMoved(_)
            | AppEvent::PointerInput { .. }
            | AppEvent::Scroll(_)
            | AppEvent::Keyboard(_)
            | AppEvent::ImePreedit { .. }
            | AppEvent::ImeCommit(_) => {
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    fn on_render(&mut self) -> bool {
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e:?}");
        }
        // IME (text_input がない demo だが、念のため empty で disable)
        self.window.set_ime_allowed(false);
        // false: 連続再描画は library 側 (level_meter / tab_view / scroll_area /
        // split_view 等が `Ui::request_redraw()` を呼ぶ) と Edit / focus 変化の
        // auto-redraw に任せる。アイドル時 (sim_phase 動かない / tab 切替なし) は
        // 0fps で電力節約。
        false
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui daw_prototype")
        .with_inner_size(winit::dpi::LogicalSize::new(1280, 800));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("run_app error: {e:?}");
    }
}
