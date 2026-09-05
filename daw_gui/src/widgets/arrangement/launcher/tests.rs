//! ランチャー帯の検査。
//!
//! **狙いは「一度踏むと直すまで使えない」種類の欠陥**:
//! 帯を畳んだら戻せない / 行が 1 段ズレる / 標識が背景に沈む / まとめセルが
//! 子を撃たない。どれも build も clippy もすり抜け、実機で初めて分かる。

use super::*;

use daw_ui_core::theme::{contrast_ratio, Palette};

fn theme() -> crate::theme::Theme {
    crate::theme::Theme::builtin("dark").expect("組込みダークテーマは常に存在する")
}

fn view_with_scenes(n: usize) -> LauncherView {
    LauncherView {
        scenes: (0..n)
            .map(|i| LauncherSceneView {
                #[allow(clippy::cast_possible_truncation)]
                id: i as u32 + 1,
                name: Arc::from(format!("Scene {}", i + 1)),
                color: Color::rgb(0.5, 0.5, 0.5),
                follow: false,
                selected: false,
            })
            .collect(),
        ..LauncherView::default()
    }
}

// ============================================================
// 帯幅 — 「畳んだら戻せない」を止める
// ============================================================

/// 帯を左端 (アレンジのみ) / 右端 (ランチャーのみ) まで寄せても、
/// **帯の右端スプリッタは画面内に残り、ヘッダ境界スプリッタとは別の場所で掴める**。
///
/// ここが崩れると「一度アレンジのみにしたらもう帯を出せない」= 機能が消える。
#[test]
fn 帯を端まで寄せてもスプリッタが両方掴める() {
    let rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    let header_w = 160.0;
    // ヘッダ境界のスプリッタは中心対称なので、右へ `handle/2` だけ帯を食う。
    let header_bite = ArrangementStyle::from_theme(&theme()).header_resize_handle_px * 0.5;
    let avail = rect.w - header_w;
    /// 計画書 Q5 が要求する「どちらの端でも残るつかみ代」。
    const REQUIRED_GRAB_PX: f32 = 6.0;

    for (name, layout, expect_pane_w) in [
        ("アレンジのみ", LauncherLayout::ArrangerOnly, GRAB_W),
        ("ランチャーのみ", LauncherLayout::LauncherOnly, avail - GRAB_W),
    ] {
        let view = LauncherView { layout, ..LauncherView::default() };
        let pane_w = layout::resolve_pane_w(&view, avail);
        assert!(
            (pane_w - expect_pane_w).abs() < 1e-3,
            "{name}: 帯幅 {pane_w} (期待 {expect_pane_w})"
        );
        let boundary = header_w + pane_w;
        assert!(
            boundary <= rect.x + rect.w,
            "{name}: スプリッタが widget の右外へ出ている (boundary {boundary})"
        );
        // ヘッダ境界に食われた残りが、要求のつかみ代を満たすこと。
        let exclusive_left = (header_w + header_bite).max(boundary - PANE_SPLITTER_HANDLE);
        assert!(
            boundary - exclusive_left >= REQUIRED_GRAB_PX,
            "{name}: 帯スプリッタの掴み代が {}px しか無い (最低 {REQUIRED_GRAB_PX}px)",
            boundary - exclusive_left
        );
        assert!(
            layout::pane_splitter_hit(rect, boundary, exclusive_left, 300.0),
            "{name}: x={exclusive_left} で帯スプリッタが掴めない"
        );
        // **アレンジのレーンの側は食わない** (拍 0 のクリップ / 点が掴めなくなる)。
        assert!(
            !layout::pane_splitter_hit(rect, boundary, boundary, 300.0),
            "{name}: 境界より右 (= レーン側) までホットゾーンが伸びている"
        );
    }
}

/// 帯 = 停止列 + セル格子 + 返す列 で、**列がはみ出さない**。
/// 分割を間違えるとセルが返す列の下へ潜り、押しても発火しないボタンができる。
#[test]
fn 帯は停止列とセル格子と返す列にちょうど分かれる() {
    let rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    let view = view_with_scenes(3);
    let r = layout::split(rect, 160.0, 300.0, 38.0, 38.0, 562.0, &view);
    assert!((r.pane.w - 300.0).abs() < 1e-3);
    // 帯 = 停止列 + 格子 + 返す列 + **スプリッタ専用のつかみ代** (右端)。
    // つかみ代を列に食わせると「幅を変えるドラッグ」と「アレンジへ返す」ボタンが
    // 同じ場所に居て、ボタンが押せなくなる。
    let cols = r.stop_col.w + r.grid.w + r.return_col.w;
    assert!((cols + PANE_SPLITTER_HANDLE - r.pane.w).abs() < 1e-3, "cols={cols}");
    assert!(
        (r.return_col.x + r.return_col.w + PANE_SPLITTER_HANDLE - (r.pane.x + r.pane.w)).abs()
            < 1e-3,
        "返す列の右にちょうどつかみ代が残る"
    );
    assert!((r.grid.x - (r.stop_col.x + r.stop_col.w)).abs() < 1e-3);
    assert!((r.return_col.x - (r.grid.x + r.grid.w)).abs() < 1e-3);
    assert!(!r.collapsed, "300px あれば格子は描ける");
    // 見出し行とセル格子は y が排他 (見出しの下端 = 格子の上端)。
    assert!((r.head.y + r.head.h - r.grid.y).abs() < 1e-3);
    // つかみ代だけまで畳んだら「格子は描けない」と判定される。
    let collapsed = layout::split(rect, 160.0, GRAB_W, 38.0, 38.0, 562.0, &view);
    assert!(collapsed.collapsed, "つかみ代だけの幅では格子を描けない");
}

/// **「両方」レイアウトは必ず格子を描ける** (計画書 Q5-b の「比率を覚えている」が
/// 成り立つ前提)。
///
/// スプリッタを端まで引くと、吸着で `ArrangerOnly` / `LauncherOnly` に切り替わる直前まで
/// 「両方」のまま `ui_prefs.launcher_width` が更新される。閾値と復元の clamp が別々の値を
/// 見ていると、記憶に残るのが「格子が 1 列も入らない幅」になり、`Tab` で戻したとき
/// 掴み代だけの帯が出る — 一度踏むと手でスプリッタを掴み直すまで使えない。
/// **壊れても build も clippy も通る**ので機械で止める。
#[test]
fn 両方レイアウトの帯幅は必ず格子を描ける() {
    let rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    let header_w = 160.0;
    let avail = rect.w - header_w;
    let view = view_with_scenes(2);
    // 記憶が壊れた値 (負 / 0 / 吸着直前の幅 / 画面より広い) も含める。
    for w in [-10.0_f32, 0.0, 1.0, GRAB_W, GRAB_W + 1.0, 40.0, 300.0, 5_000.0] {
        let pane_w = layout::resolve_pane_w_raw(LauncherLayout::Both, w, avail);
        let r = layout::split(rect, header_w, pane_w, 38.0, 38.0, 562.0, &view);
        assert!(!r.collapsed, "覚えた幅 {w} で「両方」が畳まれている (pane_w={pane_w})");
        assert!(
            r.grid.w >= MIN_COL_W,
            "覚えた幅 {w} で列が 1 本も入らない (grid.w={})",
            r.grid.w
        );
        // アレンジ側も潰れない (右端へ引き切った記憶で戻ったときの対称形)。
        assert!(
            avail - pane_w >= MIN_BOTH_PANE_W,
            "覚えた幅 {w} でアレンジ側が {}px しか残らない",
            avail - pane_w
        );
    }
}

// ============================================================
// 行 — 「1 段ズレる」を止める
// ============================================================

/// 帯が使う行一覧 ([`arrangement_row_layout`]) と、ヘッダ / アレンジのレーンが使う
/// prefix sum ([`visible_track_row_tops`]) が、**トラック行で必ず同じ y を指す**。
///
/// 帯だけ別の式で行を積むと、per-track 行高 override とレーン展開が混ざった構成で
/// 静かに 1 段ズレる (見た目にはセルが隣のトラックのものに見える)。
#[test]
fn 帯の行とアレンジの行は同じ縦位置に並ぶ() {
    let lane = ArrangementAutomationLane {
        id: 7,
        target: common::model::AutomationTarget::TrackBuiltin(
            common::model::TrackBuiltinParam::Volume,
        ),
        plugin_range: None,
        label: Arc::from("Volume"),
        icon_glyph: 'V',
        color: Color::rgb(0.5, 0.5, 0.5),
        enabled: true,
        visible: true,
        height_px: 60,
        default_value_norm: 0.5,
        clips: Vec::new(),
    };
    let base = ArrangementTrack {
        id: 0,
        name: Arc::from("t"),
        muted: false,
        solo: false,
        armed: false,
        clips: Vec::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        automation_lanes_collapsed: true,
        automation_lanes: Vec::new(),
        row_h: None,
        kind: TrackKind::Audio,
        color: None,
    };
    let tracks = vec![
        // 行高 override 付き
        ArrangementTrack { id: 1, row_h: Some(48), ..base.clone() },
        // レーンを展開している行 (次の行が 60px ぶん下へずれる)
        ArrangementTrack {
            id: 2,
            automation_lanes_collapsed: false,
            automation_lanes: vec![lane],
            ..base.clone()
        },
        ArrangementTrack { id: 3, ..base },
    ];
    let row_h = 32.0;
    let lanes_y = 100.0;
    let track_top = 12.0;
    let rows = arrangement_row_layout(&tracks, row_h);
    let tops = visible_track_row_tops(&tracks, lanes_y, track_top, row_h);
    let origin = tops[0];

    let track_rows: Vec<&ArrangementRow> = rows
        .iter()
        .filter(|r| matches!(r.key, ArrangementRowKey::Track(_)))
        .collect();
    assert_eq!(track_rows.len(), tracks.len(), "トラック行が欠けている");
    for (i, r) in track_rows.iter().enumerate() {
        let from_rows = origin + r.content_top;
        assert!(
            (from_rows - tops[i]).abs() < 1e-3,
            "行 {i}: 帯 {from_rows} vs ヘッダ/レーン {}",
            tops[i]
        );
    }
    // レーン行はトラック行 2 の直下 (32px 行の下端) から始まる。
    let lane_row = rows
        .iter()
        .find(|r| matches!(r.key, ArrangementRowKey::Lane(_)))
        .expect("展開したレーン行が並ぶ");
    assert!((origin + lane_row.content_top - (tops[1] + 32.0)).abs() < 1e-3);
    assert!((lane_row.height - 60.0).abs() < 1e-3);
}

// ============================================================
// 列 — プレースホルダ
// ============================================================

/// 実シーンが 1 つでも、格子は表示幅ぶんの **空きプレースホルダ列**を出す。
/// そこをダブルクリックした時だけ `Song::ensure_scene_at` で列が実体化するので、
/// **開いただけでは `*` が立たない** (r.md #9)。
#[test]
fn 実シーンの右にプレースホルダ列が並ぶ() {
    let rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };
    let view = view_with_scenes(1);
    // 格子幅 = 300 - 12 (つかみ代) - 16 - 16 = 256px、列幅 96px → 3 列ぶん見える。
    let r = layout::split(rect, 160.0, 300.0, 38.0, 38.0, 562.0, &view);
    let (first, last) = r.visible_cols();
    assert_eq!(first, 0);
    assert!(last > view.scenes.len(), "プレースホルダ列が出ていない (last={last})");

    let row = ArrangementRowKey::Track(1);
    let real = layout::cell_key(&view, row, 0);
    assert_eq!(real.scene_id, 1, "0 列目は実シーン");
    let placeholder = layout::cell_key(&view, row, 2);
    assert_eq!(placeholder.scene_id, 0, "実シーンの外はプレースホルダ (id 0)");
    assert_eq!(placeholder.scene_index, 2, "実体化するときの列位置");
    assert!(placeholder.is_empty());
}

// ============================================================
// グループ行のまとめセル
// ============================================================

/// グループ行のまとめセルを撃つと、**子行それぞれのセル**へ展開される。
/// 子が空セルでも 1 件出す (空セル = その行を止める、計画書 Q11)。
#[test]
fn グループ行のまとめセルは子行へ展開される() {
    let base = ArrangementTrack {
        id: 0,
        name: Arc::from("t"),
        muted: false,
        solo: false,
        armed: false,
        clips: Vec::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        automation_lanes_collapsed: true,
        automation_lanes: Vec::new(),
        row_h: None,
        kind: TrackKind::Audio,
        color: None,
    };
    let tracks = vec![
        ArrangementTrack { id: 10, ..base.clone() },              // グループ
        ArrangementTrack { id: 11, parent_id: Some(10), depth: 1, ..base.clone() },
        ArrangementTrack { id: 12, parent_id: Some(11), depth: 2, ..base.clone() }, // 孫
        ArrangementTrack { id: 13, ..base },                      // 無関係
    ];
    let mut view = view_with_scenes(2);
    let group_row = LauncherRowView {
        playback: RowPlayback::Arranger,
        armed: false,
        group: true,
        launchable: true,
        takes_cells: false,
        cells: HashMap::new(),
    };
    view.rows.insert(ArrangementRowKey::Track(10), group_row);
    // 子 11 は 2 列目 (scene id 2) にセルを持ち、孫 12 は空。
    let mut child = HashMap::new();
    child.insert(
        2,
        LauncherCellView {
            clip_id: 99,
            name: Arc::from("Kick"),
            color: Color::rgb(0.3, 0.4, 0.6),
            muted: false,
            linked: false,
            in_active_group: false,
            follow: false,
            content_offset_beats: 0.0,
            len_beats: 4.0,
            looping: true,
            curve: Vec::new(),
        },
    );
    view.rows.insert(
        ArrangementRowKey::Track(11),
        LauncherRowView {
            playback: RowPlayback::Arranger,
            armed: false,
            group: false,
            launchable: true,
            takes_cells: true,
            cells: child,
        },
    );
    view.rows.insert(
        ArrangementRowKey::Track(12),
        LauncherRowView {
            playback: RowPlayback::Arranger,
            armed: false,
            group: false,
            launchable: true,
            takes_cells: true,
            cells: HashMap::new(),
        },
    );

    let cell = layout::cell_key(&view, ArrangementRowKey::Track(10), 1);
    let expanded = press::expand_group_cells(&tracks, &view, cell);
    let rows: Vec<ArrangementRowKey> = expanded.iter().map(|c| c.row).collect();
    assert_eq!(
        rows,
        vec![ArrangementRowKey::Track(11), ArrangementRowKey::Track(12)],
        "子と孫の 2 行 (無関係な 13 は入らない)"
    );
    assert_eq!(expanded[0].clip_id, 99, "子はセルを持つので発火");
    assert_eq!(expanded[1].clip_id, 0, "孫は空セル = その行を止める");

    // グループでない行はそのまま 1 件 (展開しない)。
    let plain = layout::cell_key(&view, ArrangementRowKey::Track(11), 1);
    assert_eq!(press::expand_group_cells(&tracks, &view, plain), vec![plain]);
}

// ============================================================
// 標識のコントラスト
// ============================================================

/// 標識が乗りうる背景の spectrum。**沈む側を必ず含む**
/// (`tests_contrast.rs` と同じ方針 — fixture に沈む側が無いと欠陥を検出できない)。
fn cell_backgrounds() -> Vec<(&'static str, Color)> {
    vec![
        ("暗レーン (ダークの window_bg)", Palette::dark().window_bg),
        ("明レーン (ライトの window_bg)", Palette::light().window_bg),
        ("選択の黄 (selection_warm)", Palette::dark().selection_warm),
        ("ユーザー着色 (ほぼ黒の青)", Color::rgb(0.02, 0.03, 0.06)),
        ("ユーザー着色 (明るいクリーム)", Color::rgb(0.95, 0.93, 0.78)),
        ("ユーザー着色 (蛍光の黄緑)", Color::rgb(0.80, 1.00, 0.20)),
    ]
}

/// ▶ / 停止 / 録音 の記号は、**どんなセルの塗りの上でも読める**。
///
/// セルの塗りはユーザーが自由に着色する可変背景なので、極性固定インクを直接置くと
/// 必ずどちらかの極性で消える。チップを 1 枚敷いて、その合成結果からインクを
/// 選ぶのが唯一の解 (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
#[test]
fn セルの標識はどの塗りの上でも読める() {
    const MIN: f32 = 3.0;
    let p = Palette::dark();
    for (name, bg) in cell_backgrounds() {
        let ind = draw::indicator_on(&p, bg);
        let ratio = contrast_ratio(ind.ink, ind.eff_bg);
        assert!(
            ratio >= MIN,
            "▶ / 停止 / 録音 の記号が「{name}」の上で読めない: {ratio:.2}:1 (最低 {MIN}:1)"
        );
    }
}

/// 走行中の進捗バーは、**どんなセルの塗りの上でも溝と区別できる**。
/// 「鳴っているのに進捗が見えない」はランチャーでは致命的 (どのセルが走っているかが
/// 唯一そこにしか出ない)。
#[test]
fn 進捗バーはどの塗りの上でも溝と区別できる() {
    const MIN: f32 = 3.0;
    let p = Palette::dark();
    for (name, bg) in cell_backgrounds() {
        let ind = draw::indicator_on(&p, bg);
        let bar = p.adapt_on(ind.eff_bg, p.meter_green);
        let ratio = contrast_ratio(bar, ind.eff_bg);
        assert!(
            ratio >= MIN,
            "進捗バーが「{name}」の上で溝に沈む: {ratio:.2}:1 (最低 {MIN}:1)"
        );
    }
}
