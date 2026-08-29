//! S4b: arrangement widget の pure-fn 単体テスト (geometry / hit-test / draw primitive helpers)。
//! private fn に触るため親モジュールの submodule ファイルとして分離 (`use super::*`)。

    use super::*;
    // S4b: reorder ヘルパは daw-ui-core の reorderable_list へ移設したので import 経由で参照。
    use daw_ui_core::widgets::reorderable_list::{apply_reorder, compute_reorder_target_index};

    /// r.md #48: テストの基準テーマ。組込みダークから組むので、`Palette` / `DawColors` の
    /// ダーク値を変えたら期待値も自動で追従する (色を literal でベタ書きしない)。
    /// `Theme::builtin` はプロセスグローバルを触らないので並列テストでも安全。
    fn test_theme() -> crate::theme::Theme {
        crate::theme::Theme::builtin("dark").expect("組込みダークテーマは常に存在する")
    }

    /// 旧 `ArrangementStyle::default()` の置換 — 組込みダークテーマから組んだ style。
    fn test_style() -> ArrangementStyle {
        ArrangementStyle::from_theme(&test_theme())
    }

    /// r.md #73: lane の既定 target (plain 0..=2 ↔ norm 0..=1 の affine、 表示窓の内側)。
    /// 行レイアウトのテストは曲線の形を見ないので、 どの affine target でも等価。
    fn volume_target() -> common::model::AutomationTarget {
        common::model::AutomationTarget::TrackBuiltin(common::model::TrackBuiltinParam::Volume)
    }

    fn clip(id: u32, start: f64, len: f64, name: &str) -> ClipView {
        ClipView {
            id,
            start_beat: start,
            len_beats: len,
            content_offset_beats: 0.0,
            name: Arc::from(name),
            color: None,
            share_group_color: None,
            fades: Vec::new(),
            thumbnail: None,
            in_active_group: false,
            muted: false,
        }
    }

    /// r.md #68: 「clip 矩形いっぱいに `clip_len` 拍が載る」 写像 (= 既存の fade 幾何
    /// テストが前提にしていた縮尺)。 本番は `content_map` が `lanes.w / view.len_beats`
    /// から同じ値を作る。
    fn test_content_map(clip_rect: Rect, clip_len: f64) -> ContentMap {
        ContentMap { origin_x: clip_rect.x, px_per_beat: f64::from(clip_rect.w) / clip_len }
    }

    /// r.md #68: 16:9 の test thumbnail (`start_in_content_beats` だけ可変)。
    fn test_thumbnail(start_in_content_beats: f64) -> ClipThumbnail {
        use std::num::NonZeroU32;
        ClipThumbnail {
            texture: TextureHandle::from_raw(NonZeroU32::new(7).unwrap()),
            width: 1920,
            height: 1080,
            start_in_content_beats,
        }
    }

    /// r.md #38: fade を 1 event ぶん持つ test clip helper (content 種別は問わない)。
    fn fade_clip(id: u32, start: f64, len: f64, name: &str, fade: common::model::EventFade) -> ClipView {
        let mut c = clip(id, start, len, name);
        c.fades = vec![ClipEventFade { event_index: 0, fade }];
        c
    }

    /// r.md #38: `EventFade` の組み立てショートハンド (event が clip 全体を覆う場合)。
    fn ev_fade(len: f64, fi: f64, fo: f64, ci: FadeCurve, co: FadeCurve) -> common::model::EventFade {
        common::model::EventFade {
            start_in_clip_beats: 0.0,
            len_beats: len,
            fade_in_beats: fi,
            fade_out_beats: fo,
            fade_in_curve: ci,
            fade_out_curve: co,
        }
    }

    /// M14 Phase 63k (#025): fade を 1 event ぶん持つ test clip helper。
    fn audio_clip(id: u32, start: f64, len: f64, name: &str) -> ClipView {
        ClipView {
            id,
            start_beat: start,
            len_beats: len,
            content_offset_beats: 0.0,
            name: Arc::from(name),
            color: None,
            share_group_color: None,
            // r.md #38: clip 全体を覆う 1 event、 fade 0 (= handle は clip の角に一致)。
            fades: vec![ClipEventFade {
                event_index: 0,
                fade: ev_fade(len, 0.0, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }],
            thumbnail: None,
            in_active_group: false,
            muted: false,
        }
    }

    fn track(id: u32, name: &str, clips: Vec<ClipView>) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            armed: false,
            clips,
            volume: 1.0,
            parent_id: None,
            depth: 0,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            collapsed: false,
            row_h: None,
            kind: TrackKind::Audio,
            color: None,
        }
    }

    fn test_view() -> ArrangementView {
        ArrangementView {
            start_beat: 0.0,
            scroll_beat_raw: 0.0,
            len_beats: 16.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 0.0,
            ruler_h: 0.0,
            playhead_beat: None,
            loop_range: None,
            data_generation: 0,
            bpm: 120.0,
            time_sig: (4, 4),
            // 数値検証 test は raw beat 値を期待するので明示 OFF。
            snap: SnapConfig::OFF,
            arranger_lane_h: 0.0,
        }
    }

    fn test_lanes() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 640.0, h: 256.0 }
    }

    /// M14 Phase 63n-1 (#028): test 用 prefix-sum tops 生成 helper。
    /// lane を持たない tracks (= 既存挙動) では `tops[i] = lanes_y - track_top + i * track_row_h` と等価。
    fn make_tops(tracks: &[ArrangementTrack], lanes: Rect, view: ArrangementView) -> Vec<f32> {
        visible_track_row_tops(tracks, lanes.y, view.track_top, view.track_row_h)
    }

    /// 簡易 tops (test_view + test_lanes と同条件で N track ぶん、 lane なし)。
    /// `lanes_y=0, track_top=0, row_h=32` → `tops = [0.0, 32.0, 64.0, 96.0, ...]`。
    fn legacy_tops(n: usize) -> Vec<f32> {
        #[allow(clippy::cast_precision_loss)]
        (0..=n).map(|i| i as f32 * 32.0).collect()
    }

    #[test]
    fn clip_to_rect_basic_position() {
        let view = test_view();
        let lanes = test_lanes();
        let c = clip(0, 4.0, 4.0, "x");
        // visible_idx 2 → row_top = lanes.y - track_top + 2 * track_row_h = 0 - 0 + 64 = 64
        let r = clip_to_rect(64.0, view.track_row_h, &c, view, lanes);
        // beat_to_px = 640/16 = 40
        // x = 0 + 4*40 = 160, w = 4*40 = 160
        // row_top = 64, y = 64+2 = 66, h = 32-4 = 28
        assert!((r.x - 160.0).abs() < 1e-3);
        assert!((r.w - 160.0).abs() < 1e-3);
        assert!((r.y - 66.0).abs() < 1e-3);
        assert!((r.h - 28.0).abs() < 1e-3);
    }

    #[test]
    fn track_index_from_y_basic() {
        // lanes_y=0, row_h=32 → y=0 → idx 0, y=32 → idx 1, y=64 → idx 2
        let tops = legacy_tops(3);
        assert_eq!(track_index_from_y(0.0, 0.0, &tops), Some(0));
        assert_eq!(track_index_from_y(32.0, 0.0, &tops), Some(1));
        assert_eq!(track_index_from_y(64.0, 0.0, &tops), Some(2));
        // y < tops[0] = 範囲外
        assert_eq!(track_index_from_y(-5.0, 0.0, &tops), None);
        // y > tops[3] = 範囲外
        assert_eq!(track_index_from_y(200.0, 0.0, &tops), None);
    }

    #[test]
    fn track_index_from_y_with_scroll() {
        // track_top=16 を反映した tops: y=lanes_y-16+i*32 → tops = [-16, 16, 48, 80, 112]
        let view = ArrangementView { track_top: 16.0, ..test_view() };
        let lanes = test_lanes();
        let tracks: Vec<ArrangementTrack> = (0..4).map(|i| track(i, "t", vec![])).collect();
        let tops = make_tops(&tracks, lanes, view);
        // y=10 → -16 <= 10 < 16 → idx 0、 y=26 → 16 <= 26 < 48 → idx 1
        assert_eq!(track_index_from_y(10.0, 0.0, &tops), Some(0));
        assert_eq!(track_index_from_y(26.0, 0.0, &tops), Some(1));
    }

    #[test]
    fn visible_track_row_tops_with_no_lanes_matches_legacy_layout() {
        // M14 Phase 63n-1 (#028) regression: lane 0 個では legacy 式 `tops[i] = lanes_y - track_top
        // + i * track_row_h` と完全一致 (= 既存挙動完全互換)。
        let view = test_view();
        let lanes = test_lanes();
        let tracks: Vec<ArrangementTrack> = (0..4).map(|i| track(i, "t", vec![])).collect();
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0, 64.0, 96.0, 128.0]);
    }

    #[test]
    fn visible_track_row_tops_with_expanded_lane_grows_track_height() {
        // M14 Phase 63n-1 (#028): expanded lane (visible) を持つ track 以降は次 track の row_top が
        // 下にずれる。 collapsed もしくは invisible lane は加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = false;
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            // r.md #73: この 4 本は行レイアウトのテストで curve を見ないので、
            // affine な既定 target (Volume) で足りる。
            target: volume_target(),
            plugin_range: None,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let t2 = track(2, "t2", vec![]);
        let tracks = vec![t1, t2];
        let tops = make_tops(&tracks, lanes, view);
        // tops[0] = 0、 tops[1] = 0 + (32 + 60) = 92 (t1 expanded)、 tops[2] = 92 + 32 = 124 (t2 collapsed)
        assert_eq!(tops, vec![0.0, 92.0, 124.0]);
    }

    #[test]
    fn visible_track_row_tops_collapsed_lane_does_not_extend_height() {
        // M14 Phase 63n-1 (#028): `automation_lanes_collapsed = true` で lane を持っていても加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = true; // 既存挙動
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            // r.md #73: この 4 本は行レイアウトのテストで curve を見ないので、
            // affine な既定 target (Volume) で足りる。
            target: volume_target(),
            plugin_range: None,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let tracks = vec![t1];
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0]); // collapsed = legacy と同じ
    }

    #[test]
    fn visible_track_row_tops_invisible_lane_does_not_extend_height() {
        // M14 Phase 63n-1 (#028): `lane.visible = false` の lane は expanded でも加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = false;
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            // r.md #73: この 4 本は行レイアウトのテストで curve を見ないので、
            // affine な既定 target (Volume) で足りる。
            target: volume_target(),
            plugin_range: None,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: false, // hidden
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let tracks = vec![t1];
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0]); // invisible lane = legacy と同じ
    }

    /// r.md #63: 行レイアウト (`X` の全体表示 / `Z` の縦ズームが唯一の根拠にする) は
    /// track 行と展開 lane 行を描画順に並べ、 `content_top` は prefix sum。
    /// lane の可視条件は `visible_track_row_tops` (高さ合計) と同一でなければならない
    /// — ここがズレると「行数は数えたが実際には描かれない」 で fit が外れる。
    #[test]
    fn arrangement_row_layout_lists_track_and_expanded_lane_rows() {
        let view = test_view();
        let lane = |id: u32, visible: bool, height_px: u16| ArrangementAutomationLane {
            id,
            target: volume_target(),
            plugin_range: None,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible,
            height_px,
            default_value_norm: 0.5,
            clips: Vec::new(),
        };
        // t1: 展開 (可視 lane 60 + 不可視 lane は行にならない)、 t2: 畳んでいるので lane 無し、
        // t3: per-track 行高 override 48。
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = false;
        t1.automation_lanes = vec![lane(11, true, 60), lane(12, false, 40)];
        let mut t2 = track(2, "t2", vec![]);
        t2.automation_lanes_collapsed = true;
        t2.automation_lanes = vec![lane(21, true, 70)];
        let mut t3 = track(3, "t3", vec![]);
        t3.row_h = Some(48);
        let tracks = vec![t1, t2, t3];

        let rows = arrangement_row_layout(&tracks, view.track_row_h);
        let expect = |key, content_top: f32, height: f32| ArrangementRow { key, content_top, height };
        assert_eq!(
            rows,
            vec![
                expect(ArrangementRowKey::Track(1), 0.0, 32.0),
                expect(
                    ArrangementRowKey::Lane(common::model::AutomationLaneKey { track: 1, lane: 11 }),
                    32.0,
                    60.0,
                ),
                expect(ArrangementRowKey::Track(2), 92.0, 32.0),
                expect(ArrangementRowKey::Track(3), 124.0, 48.0),
            ],
        );

        // 行の高さ合計 == track 単位の prefix sum (= 描画 / hit-test が使う tops) と一致する。
        let tops = make_tops(&tracks, test_lanes(), view);
        assert_eq!(
            rows.last().map(|r| r.content_top + r.height),
            tops.last().copied(),
        );
    }

    #[test]
    fn clip_hit_returns_move_in_center() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        // clip rect at (0, 2, 160, 28), center = (80, 16)
        let hit = clip_hit(&tracks, &tops, view, lanes, 80.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::Move))
        );
    }

    #[test]
    fn clip_hit_returns_resize_left_at_left_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 1.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_returns_resize_right_at_right_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 159.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
    }

    #[test]
    fn clip_hit_returns_none_outside_lanes() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, -10.0, -10.0, 4.0);
        assert_eq!(hit, None);
    }

    // -------- Hit-test extension tests (clip rect 外側 ±resize_handle_px) --------
    // clip (id 100) start=4, len=4 → rect x∈[160,320] y∈[66,94] in track 2
    // (test_lanes (0,0,640,256), test_view 16 beats / 8 tracks / row_h=32)。
    // ただし以下のテストは start=0 len=4 → x∈[0,160] y∈[2,30] in track 0 を使う。

    #[test]
    fn clip_hit_returns_resize_left_at_outer_left_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]、edge=4 で拡張範囲 x∈[-4,164)。lanes の左端 0 で外側左を表現できないので
        // clip start=2 (x=80) の clip を使い、cx=77 で外側左 (x=80-3) を確認。
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 77.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_returns_resize_right_at_outer_right_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]、cx=162 = rect 右端(160) + 2 → 外側右
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 162.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
    }

    #[test]
    fn clip_hit_returns_none_just_past_outer_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]。cx=165 = rect 右端 + 5 → 拡張範囲 [-4,164) の外
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 165.0, 16.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn clip_hit_short_clip_inside_returns_move() {
        let view = test_view();
        let lanes = test_lanes();
        // 短 clip (len=0.1 → w=4px、edge*2=8px 以下) の rect 内中央は Move 強制
        // start=2, len=0.1 → x=80, w=4
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 0.1, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 81.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::Move))
        );
    }

    #[test]
    fn clip_hit_short_clip_outer_left_returns_resize_left() {
        let view = test_view();
        let lanes = test_lanes();
        // 短 clip でも rect 外側左は ResizeLeft
        // start=2, len=0.1 → x=80。cx=78 = x - 2 → 外側左
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 0.1, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 78.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_adjacent_clips_inside_clip_owns_shared_handle() {
        // clip A (id 100, start=0, len=4) → rect x∈[0,160]、右端拡張 [156,164)
        // clip B (id 101, start=4, len=4) → rect x∈[160,320]、左端拡張 [156,164)
        // 共有境界 boundary=160。各 clip は自分の rect 内側のハンドル px を所有する
        // (in-rect は outer-extension に無条件で勝つ / #101、piano_roll note_hit_in と対)。
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "a"), clip(101, 4.0, 4.0, "b")],
        )];
        let tops = make_tops(&tracks, lanes, view);
        // cx=159: A の rect 内側 (in-rect ResizeRight) が B の外側ハンドル (outer ResizeLeft)
        // に勝つ。旧 last-wins では B ResizeLeft だった回帰ケース。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 159.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
        // cx=161: B の rect 内側 (in-rect ResizeLeft) が A の外側ハンドル (outer) に勝つ。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 161.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 101 }, ClipDragKind::ResizeLeft))
        );
        // cx=160: 共有境界。半開区間で B の rect 内側 → B の左端 resize。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 160.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 101 }, ClipDragKind::ResizeLeft))
        );
    }

    // `loop_band_hit_kind_*` の test は M14 Phase 69 (#041) で
    // `crate::widgets::ruler_ops::tests` に extract (piano_roll と共有)。

    #[test]
    fn rects_intersect_basic() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, w: 10.0, h: 10.0 };
        let c = Rect { x: 20.0, y: 0.0, w: 10.0, h: 10.0 };
        assert!(rects_intersect(a, b));
        assert!(!rects_intersect(a, c));
    }

    #[test]
    fn arrangement_view_default_sane() {
        let v = ArrangementView::default();
        assert!(v.len_beats > 0.0);
        assert!(v.track_row_h > 0.0);
        assert!(v.tracks_visible > 0.0);
        assert!(v.header_w > 0.0);
        assert!(v.ruler_h > 0.0);
    }

    #[test]
    fn arrangement_style_default_sane() {
        let s = test_style();
        assert!(s.resize_handle_px > 0.0);
        assert!(s.playhead_width_px > 0.0);
        assert!(s.clip_radius >= 0.0);
    }

    // M14 Phase 63f (#020): clip_rects API
    #[test]
    fn arrangement_response_default_has_empty_clip_rects() {
        let r = ArrangementResponse::default();
        assert!(r.clip_rects.is_empty());
        assert!(r.track_header_rects.is_empty());
    }

    // ============================================================
    // M14 Phase 72 (daw_01 #044): video track / thumbnail
    // ============================================================

    #[test]
    fn track_kind_default_is_audio() {
        assert_eq!(TrackKind::default(), TrackKind::Audio);
    }

    // ============================================================
    // r.md #68: 中身の時間写像 (ContentMap) — クリップ幅を分母に入れない
    // ============================================================

    /// **これが #68 の本体**。 clip の長さを変えても、 中身の同じ拍は画面の同じ x に
    /// 落ちる。 旧実装は `clip_rect.w / clip_len_beats` を縮尺にしていたので、 ドラッグ
    /// ゴーストが `len_new / len_old` 倍の time-stretch した絵を描いていた。
    #[test]
    fn content_map_is_independent_of_clip_length() {
        let view = test_view();
        let lanes = test_lanes();
        let short = clip(1, 8.0, 4.0, "c");
        let long = ClipView { len_beats: 32.0, ..clip(1, 8.0, 4.0, "c") };
        assert_eq!(
            content_map(&short, view, lanes),
            content_map(&long, view, lanes),
            "clip 長は中身の縮尺にも原点にも影響してはいけない"
        );
    }

    /// 左端トリム = `start_beat` と `content_offset_beats` が同量動く (`resize_clip`)。
    /// このとき content 原点は不動なので、 中身は掴んだ端に付いてこない
    /// (旧実装は原点が窓の左端だったので波形が端に張り付いて滑っていた)。
    #[test]
    fn content_map_origin_is_invariant_under_left_trim() {
        let view = test_view();
        let lanes = test_lanes();
        let before = clip(1, 8.0, 4.0, "c");
        let after = ClipView {
            start_beat: 9.5,
            len_beats: 2.5,
            content_offset_beats: 1.5,
            ..clip(1, 8.0, 4.0, "c")
        };
        assert_eq!(content_map(&before, view, lanes), content_map(&after, view, lanes));
    }

    /// 中身の縮尺は `clip_to_rect` と同じ `beat_to_px`。 これが揃っていないと
    /// 波形 / ノートがルーラーのグリッドに載らない (旧実装は 2px ずれていた)。
    #[test]
    fn content_map_scale_matches_clip_rect_scale() {
        let view = test_view();
        let lanes = test_lanes();
        let c = clip(1, 0.0, 4.0, "c");
        let map = content_map(&c, view, lanes);
        let r = clip_to_rect(0.0, view.track_row_h, &c, view, lanes);
        assert!((map.x(0.0) - r.x).abs() < 1e-3, "content 拍 0 = clip 左端");
        assert!((map.w(c.len_beats) - r.w).abs() < 1e-3, "clip 長ぶんの幅 = clip 矩形の幅");
    }

    /// r.md #68: タイル 1 枚は **行高から native aspect で決まる固定サイズ**。
    /// clip 矩形の幅には一切依存しない (= 端 drag しても大きさが変わらない)。
    #[test]
    fn thumbnail_tile_size_follows_row_height_only() {
        // 16:9 (1920x1080)、 行高 40px → 1 枚 = 40 * 16/9 = 71.111... px
        let map = ContentMap { origin_x: 100.0, px_per_beat: 40.0 };
        let narrow =
            thumbnail_tiling(Rect::new(100.0, 10.0, 20.0, 40.0), 100.0, 120.0, map, test_thumbnail(0.0))
                .expect("visible");
        let wide =
            thumbnail_tiling(Rect::new(100.0, 10.0, 600.0, 40.0), 100.0, 700.0, map, test_thumbnail(0.0))
                .expect("visible");
        assert!((narrow.tile_w - wide.tile_w).abs() < 1e-4, "clip 幅で大きさが変わってはいけない");
        assert!((narrow.tile_h - wide.tile_h).abs() < 1e-4);
        assert!((narrow.tile_w - 40.0 * 16.0 / 9.0).abs() < 1e-3, "got {}", narrow.tile_w);
    }

    /// r.md #68 の核心: 敷き詰めの **位相は content 原点**。 左端をトリムしても
    /// (= `start_beat` と `content_offset_beats` が同量動く) タイルの絶対位置は 1px も
    /// 動かない。 「クリップ左端から px 単位で繰り返す」 実装だとここが滑る。
    #[test]
    fn thumbnail_tiles_keep_phase_when_clip_is_trimmed() {
        let view = test_view();
        let lanes = test_lanes();
        let before = ClipView { thumbnail: Some(test_thumbnail(0.0)), ..clip(1, 2.0, 8.0, "v") };
        // 左端を 1 拍ぶんトリム (start 3.0 / offset 1.0)。 content 原点は 2.0 のまま。
        let after = ClipView {
            start_beat: 3.0,
            len_beats: 7.0,
            content_offset_beats: 1.0,
            ..before.clone()
        };
        let tiling = |c: &ClipView| {
            let r = clip_to_rect(0.0, view.track_row_h, c, view, lanes);
            let vis = r.intersect(lanes);
            thumbnail_tiling(r, vis.x, vis.x + vis.w, content_map(c, view, lanes), c.thumbnail.unwrap())
                .expect("visible")
        };
        let (a, b) = (tiling(&before), tiling(&after));
        assert!((a.tile_w - b.tile_w).abs() < 1e-4, "1 枚の幅は不変");
        // 可視域の始まりが変わるので「1 枚目」 の index はずれるが、 位相
        // (= タイル境界の絶対位置) は保たれる = 差はタイル幅の整数倍。
        let shift = (b.first_x - a.first_x) / a.tile_w;
        assert!(
            (shift - shift.round()).abs() < 1e-3,
            "タイル境界がタイル幅の整数倍でしかずれない: shift={shift}"
        );
    }

    /// 敷き詰めは可視域を覆う (= 長い clip を横スクロールして先頭が画面外に出ても
    /// タイルは見え続ける。 旧実装は 1 枚しか置かなかったので消えていた)。
    #[test]
    fn thumbnail_tiles_cover_the_visible_range() {
        let map = ContentMap { origin_x: 0.0, px_per_beat: 40.0 };
        // 可視域 [500, 900)、 1 枚 = 40 * 16/9 ≈ 71.1px。
        let t = thumbnail_tiling(Rect::new(0.0, 0.0, 4000.0, 40.0), 500.0, 900.0, map, test_thumbnail(0.0))
            .expect("visible");
        assert!(t.first_x <= 500.0 && t.first_x + t.tile_w > 500.0, "1 枚目が可視域左端を跨ぐ");
        let covered = t.first_x + t.tile_w * f32::from(u16::try_from(t.count).unwrap());
        assert!(covered >= 900.0, "可視域右端まで届く: covered={covered}");
        assert!(!t.truncated);
    }

    /// texture サイズ 0 で div-by-zero / panic しない (1:1 aspect に degrade)。
    #[test]
    fn thumbnail_tiling_zero_texture_clamped_to_one() {
        let map = ContentMap { origin_x: 0.0, px_per_beat: 40.0 };
        let thumb = ClipThumbnail { width: 0, height: 0, ..test_thumbnail(0.0) };
        let t = thumbnail_tiling(Rect::new(0.0, 0.0, 100.0, 30.0), 0.0, 100.0, map, thumb)
            .expect("visible");
        assert!((t.tile_w - 30.0).abs() < 1e-3);
        assert!((t.tile_h - 30.0).abs() < 1e-3);
    }

    /// 極端に縦長のソース × 高ズームでもタイル数は上限で頭打ち (= 無限に quad を
    /// 積まない)。 打ち切ったことは `truncated` で表に出す。
    #[test]
    fn thumbnail_tiling_caps_tile_count() {
        let map = ContentMap { origin_x: 0.0, px_per_beat: 40.0 };
        // 1 x 4000 の極端なソース → 1 枚 = 40 * (1/4000) = 0.01px。
        let thumb = ClipThumbnail { width: 1, height: 4000, ..test_thumbnail(0.0) };
        let t = thumbnail_tiling(Rect::new(0.0, 0.0, 2000.0, 40.0), 0.0, 2000.0, map, thumb)
            .expect("visible");
        assert_eq!(t.count, MAX_THUMBNAIL_TILES);
        assert!(t.truncated, "上限に当たったことを隠さない");
    }

    #[test]
    fn arrangement_track_kind_field_round_trip() {
        let t = ArrangementTrack {
            id: 99,
            name: Arc::from("video1"),
            muted: false,
            solo: false,
            armed: false,
            clips: Vec::new(),
            volume: 1.0,
            parent_id: None,
            depth: 0,
            collapsed: false,
            kind: TrackKind::Video,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            row_h: None,
            color: None,
        };
        assert_eq!(t.kind, TrackKind::Video);
    }

    #[test]
    fn arrangement_clip_thumbnail_field_round_trip() {
        let c = ClipView { thumbnail: Some(test_thumbnail(0.0)), ..clip(1, 0.0, 4.0, "v_clip") };
        let t = c.thumbnail.unwrap();
        assert_eq!(t.texture.raw(), 7);
        assert_eq!(t.width, 1920);
        assert_eq!(t.height, 1080);
    }

    #[test]
    fn drag_preview_geometry_move_clamps_track() {
        let anchor = ClipDragAnchor {
            key: ClipKey { track: 0, clip: 0 },
            start_beat: 4.0,
            len_beats: 2.0,
            track_index: 0,
        };
        let (s, l, idx) = drag_preview_geometry(anchor, ClipDragKind::Move, 1.5, 5, 0, 3, 0.05);
        assert!((s - 5.5).abs() < 1e-9);
        assert!((l - 2.0).abs() < 1e-9);
        // 0 + 5 = 5 → clamped to 2 (tracks=3 → max idx = 2)
        assert_eq!(idx, 2);
    }

    // M10 Phase 46: track reorder
    #[test]
    fn compute_reorder_target_above_first_row() {
        // 上端外 → 0
        assert_eq!(compute_reorder_target_index(2, -10.0, 0.0, 0.0, 32.0, 5), 0);
        assert_eq!(compute_reorder_target_index(2, 0.0, 0.0, 0.0, 32.0, 5), 0);
    }

    #[test]
    fn compute_reorder_target_below_last_row() {
        // 下端外 → n_tracks (clamp)、anchor=0 で n=5 → target_u=5、anchor 後なので 5-1=4
        assert_eq!(compute_reorder_target_index(0, 1000.0, 0.0, 0.0, 32.0, 5), 4);
    }

    #[test]
    fn compute_reorder_target_self_or_next_returns_anchor() {
        // anchor=2, mouse on row 2 → no-op = 2
        // row 2 中央 = 32*2 + 16 = 80
        assert_eq!(compute_reorder_target_index(2, 80.0, 0.0, 0.0, 32.0, 5), 2);
        // anchor=2, mouse on row 2 中央より下 = 90 → target=3, anchor+1 → no-op = 2
        assert_eq!(compute_reorder_target_index(2, 90.0, 0.0, 0.0, 32.0, 5), 2);
    }

    #[test]
    fn compute_reorder_target_above_anchor_keeps_target() {
        // anchor=4, row 1 中央 (40) → target_u=1 → anchor より前なので 1
        assert_eq!(compute_reorder_target_index(4, 40.0, 0.0, 0.0, 32.0, 5), 1);
        // anchor=4, row 0 上半分 (10) → target_u=0
        assert_eq!(compute_reorder_target_index(4, 10.0, 0.0, 0.0, 32.0, 5), 0);
    }

    #[test]
    fn compute_reorder_target_below_anchor_offsets_by_one() {
        // anchor=0, row 3 中央 (32*3+16=112) → frac=0.5 → target_unbounded=4 → anchor 抜き後 [r1, r2, r3, r4] の
        // 「row 3 と row 4 の間」= new index 3。target_u=4 > anchor+1=1 で 4-1=3。
        assert_eq!(compute_reorder_target_index(0, 112.0, 0.0, 0.0, 32.0, 5), 3);
        // anchor=1, mouse=144 → row 4.5 → target_unbounded=5 → clamp to 5 → 5-1=4
        assert_eq!(compute_reorder_target_index(1, 144.0, 0.0, 0.0, 32.0, 5), 4);
    }

    #[test]
    fn compute_reorder_target_with_track_top_scroll() {
        // header_top=10 + track_top=16 (1/2 row 上にスクロール) + mouse_y=18 → local=24 → row 0.75 → frac>=0.5 → row 1
        // anchor=3, target_u=1 → anchor より前 → 1
        assert_eq!(compute_reorder_target_index(3, 18.0, 10.0, 16.0, 32.0, 5), 1);
    }

    #[test]
    fn compute_reorder_target_zero_row_h_safe() {
        assert_eq!(compute_reorder_target_index(0, 100.0, 0.0, 0.0, 0.0, 5), 0);
        assert_eq!(compute_reorder_target_index(0, 100.0, 0.0, 0.0, 32.0, 0), 0);
    }

    // ============================================================
    // M14 Phase 101 (daw_01 #072): resolve_track_drop / gap_from_y
    // ============================================================

    /// hierarchy 付き track 生成 helper (depth / parent_id を明示)。
    fn htrack(id: u32, depth: u8, parent: Option<u32>) -> ArrangementTrack {
        let mut t = track(id, "t", vec![]);
        t.depth = depth;
        t.parent_id = parent;
        t
    }

    fn group_set(tracks: &[ArrangementTrack]) -> HashSet<u32> {
        tracks.iter().filter_map(|t| t.parent_id).collect()
    }

    /// resolve_track_drop の薄い wrapper (test 用 default 引数)。 visible = full、 lane なし tops。
    /// mouse_x / anchor_mouse_x は `col` (= 右へ動かした indent 列数) を 16px/列で与える。
    #[allow(clippy::cast_precision_loss)]
    fn resolve(
        tracks: &[ArrangementTrack],
        source: &[u32],
        mouse_y: f32,
        col: f32,
    ) -> ReorderDrop {
        let visible: Vec<ArrangementTrack> =
            tracks.iter().filter(|t| is_visible_track(t, tracks)).cloned().collect();
        let tops = visible_track_row_tops(&visible, 0.0, 0.0, 32.0);
        let is_group = group_set(tracks);
        let anchor_x = 100.0_f32;
        resolve_track_drop(
            tracks,
            &visible,
            &tops,
            &is_group,
            source,
            16.0,
            mouse_y,
            anchor_x + col * 16.0,
            anchor_x,
        )
    }

    #[test]
    fn gap_from_y_maps_rows_and_edges() {
        let tops = vec![0.0, 32.0, 64.0, 96.0]; // 3 行
        assert_eq!(gap_from_y(&tops, -5.0), 0); // 上端より上
        assert_eq!(gap_from_y(&tops, 10.0), 0); // 行0 上半分 (<16)
        assert_eq!(gap_from_y(&tops, 20.0), 1); // 行0 下半分 (>16)
        assert_eq!(gap_from_y(&tops, 50.0), 2); // 行1 下半分 (mid=48)
        assert_eq!(gap_from_y(&tops, 95.0), 3); // 行2 下半分
        assert_eq!(gap_from_y(&tops, 200.0), 3); // 最下端以下 = 末尾 gap
        // 退化
        assert_eq!(gap_from_y(&[], 10.0), 0);
        assert_eq!(gap_from_y(&[0.0], 10.0), 0);
    }

    #[test]
    fn drop_below_bottom_group_lands_top_level() {
        // 再現: 最下段 group G + 子 c1/c2、 上にある通常 track t0 を「一番下へ」 drop。
        // 期待: t0 が group block 全体の後ろに top-level で着地 (= parent None, anchor_after = c2)。
        let tracks = vec![
            htrack(0, 0, None),         // t0 (drag source)
            htrack(1, 0, None),         // G (group: c1/c2 の親)
            htrack(2, 1, Some(1)),      // c1
            htrack(3, 1, Some(1)),      // c2 (最終子)
        ];
        // 一番下 (mouse_y=200) へ、 X 動かさず (col=0)。
        let d = resolve(&tracks, &[0], 200.0, 0.0);
        assert_eq!(d.gap, 4, "末尾 gap");
        assert_eq!(d.depth, 0, "X 不動 → 境界 default = 最浅 (top-level)");
        assert_eq!(d.parent, None, "top-level に着地 (group の内側ではない)");
        assert_eq!(d.anchor_after, Some(3), "最終子 c2 の後ろ (= block 全体の後ろ)");
    }

    #[test]
    fn drop_below_bottom_group_with_indent_nests_into_group() {
        // 同じ末尾 drop でも X を 1 段 indent すれば末尾 group へ nest。
        let tracks = vec![
            htrack(0, 0, None),
            htrack(1, 0, None),
            htrack(2, 1, Some(1)),
            htrack(3, 1, Some(1)),
        ];
        let d = resolve(&tracks, &[0], 200.0, 1.0);
        assert_eq!(d.depth, 1);
        assert_eq!(d.parent, Some(1), "末尾 group G の子になる");
        assert_eq!(d.anchor_after, Some(3), "c2 の後ろ (= group の最終子)");
    }

    #[test]
    fn drop_between_members_stays_inside_group() {
        // メンバー間 (c1 と c2 の間) は境界 default で内側 (= 同 group の子)。
        let tracks = vec![
            htrack(1, 0, None),         // G
            htrack(2, 1, Some(1)),      // c1
            htrack(3, 1, Some(1)),      // c2
            htrack(9, 0, None),         // t9 (drag source)
        ];
        // gap between c1(visible idx1) と c2(idx2) = gap2 → mouse_y ~ 48..64 の下半分。
        let d = resolve(&tracks, &[9], 55.0, 0.0);
        assert_eq!(d.gap, 2);
        assert_eq!(d.depth, 1, "メンバー間 = 内側 (深さ 1)");
        assert_eq!(d.parent, Some(1));
        assert_eq!(d.anchor_after, Some(2), "c1 の後ろ");
    }

    #[test]
    fn drop_at_gap_x_controls_pop_out_depth() {
        // [s, A(group), B(group,A の子), x(B の子), T(top)]。 x と T の間 (gap4) で X により
        // 深さ 0/1/2 を選べる (= 何段 group を抜けるか / 末尾 group に nest)。
        let tracks = vec![
            htrack(99, 0, None),        // s (drag source、 先頭)
            htrack(1, 0, None),         // A
            htrack(2, 1, Some(1)),      // B
            htrack(3, 2, Some(2)),      // x (最深 leaf)
            htrack(4, 0, None),         // T
        ];
        // visible=[s,A,B,x,T] tops=[0,32,64,96,128,160]。 x(idx3)とT(idx4)の間=gap4 →
        // T (行4、 128..160) の上半分 (mid=144) で gap4 を選ぶ → mouse_y=130。
        let d0 = resolve(&tracks, &[99], 130.0, 0.0);
        assert_eq!(d0.gap, 4);
        assert_eq!((d0.depth, d0.parent), (0, None), "X 不動 → top-level (x の block 後ろ、 T の前)");
        assert_eq!(d0.anchor_after, Some(3));

        let d1 = resolve(&tracks, &[99], 130.0, 1.0);
        assert_eq!((d1.depth, d1.parent), (1, Some(1)), "1 段 indent → A の子 (B subtree の後ろ)");
        assert_eq!(d1.anchor_after, Some(3));

        let d2 = resolve(&tracks, &[99], 130.0, 2.0);
        assert_eq!((d2.depth, d2.parent), (2, Some(2)), "2 段 indent → B の子 (x の sibling)");
        assert_eq!(d2.anchor_after, Some(3));

        // 区間 clamp: 過剰 indent (col=5) でも max_d=2 で止まる。
        let d_clamp = resolve(&tracks, &[99], 130.0, 5.0);
        assert_eq!(d_clamp.depth, 2);
    }

    #[test]
    fn drop_after_collapsed_group_anchors_past_hidden_children() {
        // collapsed group G (子 c1/c2 が hidden) の直後 (visible 上は G と T の間) へ drop。
        // anchor_after は **hidden な最終子 c2** を指す (= Vec 上 group block の連続性を保つ)。
        // header (G) を指すと expand 時に block 内へ source が紛れ込むため不可。
        let tracks = vec![
            htrack(99, 0, None),                       // s (source、 先頭)
            {
                let mut g = htrack(1, 0, None);
                g.collapsed = true;
                g
            }, // G (collapsed group)
            htrack(2, 1, Some(1)),                     // c1 (hidden)
            htrack(3, 1, Some(1)),                     // c2 (hidden, 最終子)
            htrack(4, 0, None),                        // T
        ];
        // visible=[s, G, T] tops=[0,32,64,96]。 G(idx1)とT(idx2)の間=gap2 → mouse_y ~ 55。
        let d = resolve(&tracks, &[99], 55.0, 0.0);
        assert_eq!(d.gap, 2);
        assert_eq!((d.depth, d.parent), (0, None), "X 不動 → top-level");
        assert_eq!(
            d.anchor_after,
            Some(3),
            "hidden 最終子 c2 の後ろ (header G ではない、 block 連続性維持)"
        );

        // 1 段 indent すれば collapsed group の子として末尾に nest。
        let dn = resolve(&tracks, &[99], 55.0, 1.0);
        assert_eq!((dn.depth, dn.parent), (1, Some(1)));
        assert_eq!(dn.anchor_after, Some(3));
    }

    #[test]
    fn anchor_after_skips_source_tracks() {
        // 直前 track が source 自身のとき anchor_after は **その手前の非 source** を指す
        // (caller が source を remove してから anchor を探す → 見つからず末尾 append する罠を回避)。
        let tracks = vec![htrack(7, 0, None), htrack(8, 0, None)]; // [x, s]
        // s(id=8) を一番下へ。 above=s, below=None, ins=2 → tracks[..2]=[x,s] の非 source = x。
        let d = resolve(&tracks, &[8], 200.0, 0.0);
        assert_eq!(d.parent, None);
        assert_eq!(d.anchor_after, Some(7), "source s ではなく x を anchor にする");
    }

    #[test]
    fn drop_group_into_own_header_gap_does_not_self_parent() {
        // expanded group G を G ヘッダ直下 (G と c1 の間) へ drag。 唯一の合法深さ depth(G)+1=1 では
        // parent=G=source になり self-cycle。 source を親にしない不変で parent は G の親 (None) へ繰り上がる。
        let tracks = vec![
            htrack(1, 0, None),    // G (drag source)
            htrack(2, 1, Some(1)), // c1
            htrack(3, 1, Some(1)), // c2
        ];
        // gap1 = G(row0) と c1(row1) の間 → c1 上半分 (32..48) → mouse_y=40。
        let d = resolve(&tracks, &[1], 40.0, 0.0);
        assert_eq!(d.gap, 1);
        assert_ne!(d.parent, Some(1), "source G を自分の親にしない (self-cycle 回避)");
        assert_eq!(d.parent, None, "非 source 祖先が無い → top-level へ繰り上げ");
        assert_eq!(d.anchor_after, None, "G より前に非 source 無し → 先頭");
    }

    #[test]
    fn drop_multiselect_ancestor_descendant_never_parents_to_source() {
        // multi-select で moving 中の祖先 (A) / 子 (B) を親にしない。 [A, B(A の子), x(B の子), T]。
        // {A,B} を drag して x..T の gap に深く落としても parent は source(A/B) を避け None へ繰り上がる。
        let tracks = vec![
            htrack(1, 0, None),    // A (group, source)
            htrack(2, 1, Some(1)), // B (group, A の子, source)
            htrack(3, 2, Some(2)), // x (B の子)
            htrack(4, 0, None),    // T
        ];
        // x(row2)とT(row3)の間 = gap3 → T 上半分 (96..112) → mouse_y=100。 深く indent (col=5)。
        let d = resolve(&tracks, &[1, 2], 100.0, 5.0);
        assert!(d.parent != Some(1) && d.parent != Some(2), "source A/B を親にしない");
        assert_eq!(d.parent, None, "全 source 祖先を抜けて top-level");
    }

    #[test]
    fn apply_reorder_basic() {
        // [10, 20, 30, 40, 50] anchor=0 → target=2: [20, 30, 10, 40, 50]
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 0, 2), vec![20, 30, 10, 40, 50]);
        // anchor=4 → target=0: [50, 10, 20, 30, 40]
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 4, 0), vec![50, 10, 20, 30, 40]);
        // anchor=2 → target=2 (compute_reorder_target_index が anchor 自身を返した no-op semantics):
        // remove(2)=30 → [10, 20, 40, 50]、insert(2, 30) → 元 array に戻る
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 2, 2), vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn apply_reorder_safe_on_oob() {
        assert_eq!(apply_reorder(&[1, 2, 3], 5, 0), vec![1, 2, 3]); // anchor OOB
        assert_eq!(apply_reorder::<u32>(&[], 0, 0), Vec::<u32>::new()); // empty
    }

    // M10 Phase 47b: track header volume
    #[test]
    fn volume_from_mouse_x_basic() {
        // band_x=100, band_w=200 → mouse=100 → 0.0、200 → 0.5、300 → 1.0
        assert!((volume_from_mouse_x(100.0, 100.0, 200.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(200.0, 100.0, 200.0) - 0.5).abs() < 1e-6);
        assert!((volume_from_mouse_x(300.0, 100.0, 200.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn volume_from_mouse_x_clamps_outside() {
        assert!((volume_from_mouse_x(50.0, 100.0, 200.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(500.0, 100.0, 200.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn volume_from_mouse_x_zero_width_safe() {
        assert!((volume_from_mouse_x(100.0, 100.0, 0.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(100.0, 100.0, -10.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn header_row_layout_hides_band_at_default_row_h() {
        // default row_h=32 → inner_h=24、btn=20 + gap=2 + band=4 = 26 > 24 → 非表示
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 32.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_none(), "default 32px row では band 非表示 (progressive disclosure)");
    }

    #[test]
    fn header_row_layout_shows_band_when_large_enough() {
        // row_h=34 → inner_h=26 = 20+2+4 → ぎりぎり表示
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 34.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_some(), "row_h=34 で band 表示開始");

        // row_h=48 で十分余裕あり
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 48.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_some(), "row_h=48 で band 表示");
        let band = layout.volume_band.unwrap();
        assert!((band.h - 4.0).abs() < 1e-6, "band の高さ = volume_band_h");
        assert!(band.y > layout.buttons[0].y, "band は buttons の下に来る");
    }

    #[test]
    fn header_row_layout_hides_band_when_volume_band_h_zero() {
        // band_h=0 → 常に非表示 (disable)
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 };
        let layout = header_row_layout(row, 0.0);
        assert!(layout.volume_band.is_none(), "band_h=0 で disable");
    }

    // -------- M13 Phase 55: ruler / time_sig 対応 grid の確認 --------


    // ============================================================
    // M14 Phase 96 (daw_01 #068): 共有グループ連動ハイライト
    // ============================================================


    // ===== M14 Phase 127 (daw_01 #105): Arranger レーン (section) tests =====

    fn section(id: u32, start: f64, len: f64, name: &str) -> SectionView {
        SectionView {
            id,
            name: Arc::from(name),
            color: [0.30, 0.45, 0.65],
            start_beat: start,
            len_beats: len,
            selected: false,
        }
    }

    fn snap_quarter() -> SnapConfig {
        SnapConfig {
            mode: common::snap::SnapMode::Straight { div: 4 },
            enabled: true,
            min_beat_unit: 1.0 / 128.0,
            time_sig: (4, 4),
        }
    }

    fn section_drag(kind: SectionGesture, start: f64, len: f64, last: (f32, f32), ctrl: bool) -> SectionDragSession {
        SectionDragSession {
            kind,
            section_id: 7,
            anchor_start: start,
            anchor_len: len,
            anchor_press_beat: start,
            anchor_mouse: (0.0, 0.0),
            last_mouse: last,
            last_alt: false,
            last_ctrl: ctrl,
            last_shift: false,
        }
    }


    /// `section_rect_from`: beat → arranger レーン内 px (高さは lane 全高)。
    #[test]
    fn section_rect_from_basic_position() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        // 1 beat = 50 px。 start=2.0 → x=100、 len=4.0 → w=200、 高さは lane 全高。
        let r = section_rect_from(2.0, 4.0, view, arranger);
        assert!((r.x - 100.0).abs() < 1e-3, "x=100: got {}", r.x);
        assert!((r.w - 200.0).abs() < 1e-3, "w=200: got {}", r.w);
        assert!((r.y - 0.0).abs() < 1e-3 && (r.h - 20.0).abs() < 1e-3, "lane 全高");
    }

    /// `section_hit`: 帯中央 = Move、 左端 = ResizeLeft、 右端 = ResizeRight、 lane 外 (y) = None。
    #[test]
    fn section_hit_move_resize_zones() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        let secs = vec![section(7, 2.0, 4.0, "A")]; // x 100..300
        assert_eq!(section_hit(&secs, arranger, view, 200.0, 10.0, 4.0), Some((7, ClipDragKind::Move)));
        assert_eq!(section_hit(&secs, arranger, view, 100.0, 10.0, 4.0), Some((7, ClipDragKind::ResizeLeft)));
        assert_eq!(section_hit(&secs, arranger, view, 300.0, 10.0, 4.0), Some((7, ClipDragKind::ResizeRight)));
        assert_eq!(section_hit(&secs, arranger, view, 200.0, 25.0, 4.0), None, "lane の y 外は None");
    }

    /// `section_hit`: 隣接 section の共有境界 (A.right == B.left) では、 cursor が A の rect 内
    /// (A 右端ハンドル) なら、 B の左端外側拡張ハンドルより A を優先 (#101 / piano_roll #053 と同 2-tier)。
    #[test]
    fn section_hit_adjacent_in_rect_priority() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        // A 0..4 (x 0..200)、 B 4..8 (x 200..400)。 共有境界 x=200。
        let secs = vec![section(1, 0.0, 4.0, "A"), section(2, 4.0, 4.0, "B")];
        // x=199: A の rect 内 (右端 -1px) なので A の ResizeRight が、 B の左端外側ハンドルより勝つ。
        assert_eq!(
            section_hit(&secs, arranger, view, 199.0, 10.0, 4.0),
            Some((1, ClipDragKind::ResizeRight)),
            "A の右端 (in-rect) が B の左端 outer より優先"
        );
        // x=201: B の rect 内 (左端 +1px) なので B の ResizeLeft。
        assert_eq!(
            section_hit(&secs, arranger, view, 201.0, 10.0, 4.0),
            Some((2, ClipDragKind::ResizeLeft)),
            "B の左端 (in-rect) が A の右端 outer より優先"
        );
    }

    /// `section_at_inrect` は帯の **内側のみ** を返し、 resize handle の外側拡張
    /// (`±resize_handle_px`) を含めない。 帯のすぐ隣の空白の dblclick / 右クリックを隣 section の
    /// rename / メニューに化けさせない (= `section_hit` との決定的な差)。
    #[test]
    fn section_at_inrect_excludes_resize_handle_extension() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        let secs = vec![section(7, 2.0, 4.0, "Aメロ")]; // x 100..300
        // 帯の内側はヒットする (中央 / 端の内側 1px)。
        assert_eq!(section_at_inrect(&secs, arranger, view, 200.0, 10.0), Some(7), "帯中央");
        assert_eq!(section_at_inrect(&secs, arranger, view, 100.0, 10.0), Some(7), "左端 (in-rect)");
        assert_eq!(section_at_inrect(&secs, arranger, view, 299.0, 10.0), Some(7), "右端 -1px (in-rect)");
        // 帯の **すぐ隣の空白** (resize handle 拡張部 ±4px の内側) は None = リネームしない。
        assert_eq!(section_at_inrect(&secs, arranger, view, 98.0, 10.0), None, "左端の外 2px は空白");
        assert_eq!(section_at_inrect(&secs, arranger, view, 300.0, 10.0), None, "右端ちょうど (= rect 外) は空白");
        assert_eq!(section_at_inrect(&secs, arranger, view, 302.0, 10.0), None, "右端の外 2px は空白");
        // 同じ外側 2px で `section_hit` は拡張ハンドルにヒットする (= bug の発生源、 drag では正当)。
        assert!(
            section_hit(&secs, arranger, view, 302.0, 10.0, 4.0).is_some(),
            "section_hit は外側拡張を含む (drag 用) — point gesture では section_at_inrect を使う"
        );
        // lane の y 外 (帯外の縦領域) も None。
        assert_eq!(section_at_inrect(&secs, arranger, view, 200.0, 25.0), None, "lane の y 外");
    }

    /// `compute_section_drag_beat_delta`: snap OFF は pivot+raw を素通し (= 各 gesture で delta = raw)。
    #[test]
    fn section_drag_delta_raw_passthrough_off() {
        let off = &SnapConfig::OFF;
        // Move: pivot = anchor_start。
        let sd = section_drag(SectionGesture::Move, 4.0, 4.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 1.5, off, 50.0) - 1.5).abs() < 1e-6);
        // ResizeRight: pivot = anchor_start + anchor_len。
        let sd = section_drag(SectionGesture::ResizeRight, 4.0, 4.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 0.7, off, 50.0) - 0.7).abs() < 1e-6);
        // Create: pivot = anchor_press_beat (= anchor_start in helper)。
        let sd = section_drag(SectionGesture::Create, 2.0, 0.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 3.0, off, 50.0) - 3.0).abs() < 1e-6);
    }

    /// `compute_section_drag_beat_delta`: quarter snap で pivot+raw を grid に丸めた差分を返す
    /// (絶対位置 snap)。 Move pivot=4.0 + raw 1.1 = 5.1 → snap 5.0 → delta 1.0。
    #[test]
    fn section_drag_delta_snaps_pivot() {
        let snap = snap_quarter();
        let sd = section_drag(SectionGesture::Move, 4.0, 4.0, (0.0, 0.0), false);
        let d = compute_section_drag_beat_delta(&sd, 1.1, &snap, 50.0);
        assert!((d - 1.0).abs() < 1e-6, "5.1 → snap 5.0 → delta 1.0: got {d}");
    }

    /// `beats_per_bar`: 4/4=4、 3/4=3、 6/8=3、 7/8=3.5、 異常 0 は 1 以上に floor。
    #[test]
    fn beats_per_bar_time_sigs() {
        assert!((beats_per_bar((4, 4)) - 4.0).abs() < 1e-6);
        assert!((beats_per_bar((3, 4)) - 3.0).abs() < 1e-6);
        assert!((beats_per_bar((6, 8)) - 3.0).abs() < 1e-6);
        assert!((beats_per_bar((7, 8)) - 3.5).abs() < 1e-6);
        assert!(beats_per_bar((0, 0)) >= 1.0, "0/0 は 1 以上に floor");
    }



    /// id=10, beat 2..6 の clip に share_group hue / in_active_group を載せた test clip。
    fn shared_clip(in_active: bool, hue: Option<f32>) -> ClipView {
        let mut c = clip(10, 2.0, 4.0, "shared");
        c.share_group_color = hue;
        c.in_active_group = in_active;
        c
    }





    /// `in_active_group` は viewport_key (heavy cache key) に含まれない: flip しても
    /// `fold_arrangement_clip_hash` は不変 (= hover 由来の active group 変化で heavy cache を
    /// 無効化せず、 強調は cached 外 overlay で毎フレーム描く)。
    #[test]
    fn fold_arrangement_clip_hash_ignores_in_active_group() {
        // clone で同一 Arc<str> name を共有させ、 in_active_group だけが異なる 2 clip を作る
        // (clip() を 2 回呼ぶと name.as_ptr() が変わり hash の name 成分で差が出てしまうため)。
        let c_off = shared_clip(false, Some(0.33));
        let mut c_on = c_off.clone();
        c_on.in_active_group = true;
        let before = vec![track(0, "t0", vec![c_off])];
        let after = vec![track(0, "t0", vec![c_on])];
        assert_eq!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "in_active_group 変化は cache を無効化しない"
        );
    }

    /// r.md #58 同件: 上の契約は **caller 側でも** 守られていなければ意味がない。
    /// `data_generation` は `viewport_key` の 1 成分なので、ここに `in_active_group` が
    /// 混ざっていると widget 側でいくら除外しても hover でアレンジ全体が再構築される
    /// (実際に混ざっていた)。widget 側テストは caller を守らないので両側で固定する。
    #[test]
    fn caller_data_generation_ignores_in_active_group() {
        let c_off = shared_clip(false, Some(0.33));
        let mut c_on = c_off.clone();
        c_on.in_active_group = true;
        let before = vec![track(0, "t0", vec![c_off])];
        let after = vec![track(0, "t0", vec![c_on])];
        assert_eq!(
            crate::widgets::arrangement::view_build::data_generation(&before),
            crate::widgets::arrangement::view_build::data_generation(&after),
            "in_active_group 変化で heavy cache を捨てない"
        );
    }

    // ============================================================
    // M14 Phase 108 (daw_01 #080): share マークを Video-kind track の clip にも描く
    // ============================================================



    /// M14 Phase 89 (daw_01 #060): arrangement の代表 clip fill が共有 `daw_ui_core::color` の閾値の
    /// 期待側に乗る (黄 selected fill = 明るい側 / 暗青 default fill = 暗い側)。 luminance 関数自体の
    /// 単調性 / 極値は `daw_ui_core::color` 側で検証済。
    #[test]
    fn clip_fills_land_on_expected_contrast_side() {
        use daw_ui_core::color::{CONTRAST_LUMINANCE_THRESHOLD, relative_luminance};
        use daw_ui_core::theme::srgb;
        // パレット値は linear。 **画面上の見え方** で書かないと直感とずれる:
        // linear (0.18,0.40,0.65) は画面では sRGB (118,169,212) の明るいスチールブルーで、
        // 「暗い側」 ではない (2026-08-15 まで relative_luminance の二重デコードで暗いと
        // 誤判定されていた)。 既定クリップ色はこの明るい側に乗る。
        let yellow = relative_luminance(1.0, 0.85, 0.30); // clip_selected_fill
        let steel_blue = relative_luminance(0.18, 0.40, 0.65); // clip_default_fill
        let true_dark = {
            let c = srgb(0.10, 0.14, 0.22); // 画面上も暗い紺
            relative_luminance(c.r, c.g, c.b)
        };
        assert!(yellow > CONTRAST_LUMINANCE_THRESHOLD, "黄 fill は明るい側: {yellow}");
        assert!(
            steel_blue > CONTRAST_LUMINANCE_THRESHOLD,
            "既定 fill は画面上明るいので明るい側: {steel_blue}"
        );
        assert!(true_dark < CONTRAST_LUMINANCE_THRESHOLD, "本当に暗い紺は暗い側: {true_dark}");
        assert!(yellow > steel_blue && steel_blue > true_dark, "黄 > スチール青 > 紺 の単調性");
    }

    /// M14 Phase 89 (daw_01 #060): `clip_text_color_for` が fill 輝度で暗/明文字を選び、
    /// 半透明 fill は lane bg と合成した実効色で判定し、 opt-out 時は固定色を返す。
    ///
    /// r.md #48: clip はユーザー着色の可変背景なので、選ばれるのは **極性固定インク**
    /// (`ink_on_bright` / `ink_on_dark`)。テーマ従属の `text` ではないことを直接 assert する
    /// (ここが `text` に化けるとライトテーマでクリップ名が消える)。
    #[test]
    fn clip_text_color_for_picks_contrast() {
        let theme = test_theme();
        let p = &theme.core;
        let style = ArrangementStyle::from_theme(&theme);

        // 明るい fill (黄 selected) → 暗インク。
        assert_eq!(
            clip_text_color_for(p, &style, style.clip_selected_fill, style.bg),
            p.ink_on_bright,
            "明るい黄 fill には暗インク"
        );
        // 既定 fill (スチールブルー) は画面上明るい (sRGB 127,157,190) → 暗インク。
        // かつて明インクが選ばれていたが実効 2.8:1 で AA を大きく割っていた。
        assert_eq!(
            clip_text_color_for(p, &style, style.clip_default_fill, style.bg),
            p.ink_on_bright,
            "既定 fill は画面上明るいので暗インク"
        );
        // 画面上も暗い fill → 明インク。
        assert_eq!(
            clip_text_color_for(p, &style, daw_ui_core::theme::srgb(0.10, 0.14, 0.22), style.bg),
            p.ink_on_dark,
            "暗い紺 fill には明インク"
        );
        // 半透明の薄緑 share fill: 不透明なら明るく暗インク寄りだが、 暗い lane bg と薄い alpha で
        // 合成すると実効輝度が下がり明インクが選ばれる (合成判定が効いている証拠)。
        // alpha は 0.10。 輝度は linear で線形合成されるので、 薄緑を lane bg に 30% 重ねた
        // 実効輝度は 0.28 = 画面上は中間色で、暗インクが正しい (旧 0.30 は二重デコードで
        // 「暗い」と誤判定されていた)。
        let pale_green = Color::rgba(0.55, 0.85, 0.55, 0.10);
        let opaque = clip_text_color_for(
            p,
            &style,
            Color::rgb(pale_green.r, pale_green.g, pale_green.b),
            style.bg,
        );
        let composited = clip_text_color_for(p, &style, pale_green, style.bg);
        assert_eq!(opaque, p.ink_on_bright, "不透明な薄緑は暗インク");
        assert_eq!(
            composited, p.ink_on_dark,
            "暗 lane bg に薄く重ねた薄緑 (alpha 0.1) は明インク"
        );

        // opt-out: auto を切ると fill に依らず clip_text_color 固定。
        let mut off = style;
        off.clip_auto_contrast_text = false;
        assert_eq!(
            clip_text_color_for(p, &off, off.clip_selected_fill, off.bg),
            off.clip_text_color,
            "opt-out 時は明るい fill でも clip_text_color 固定"
        );
    }

    /// r.md #48: 同じ判定がライトテーマでも成立する (= 極性は **テーマで反転しない**)。
    /// 明るいクリップには暗インク / 暗いクリップには明インクが、両テーマで同じ色になる。
    #[test]
    fn clip_text_color_polarity_is_theme_independent() {
        let bright_clip = Color::rgb(0.95, 0.80, 0.35);
        let dark_clip = Color::rgb(0.10, 0.12, 0.18);
        for id in ["dark", "light"] {
            let theme = crate::theme::Theme::builtin(id).unwrap();
            let p = &theme.core;
            let style = ArrangementStyle::from_theme(&theme);
            assert_eq!(
                clip_text_color_for(p, &style, bright_clip, style.bg),
                p.ink_on_bright,
                "theme={id}: 明るいクリップには暗インク"
            );
            assert_eq!(
                clip_text_color_for(p, &style, dark_clip, style.bg),
                p.ink_on_dark,
                "theme={id}: 暗いクリップには明インク"
            );
        }
    }


    // M14 Phase 61b (#011): fold_arrangement_clip_hash の cache invalidation 性質を verify。
    // (1) 同一データなら 2 回 fold して同値、 (2) clip.start_beat 変化で hash 変わる、
    // (3) clip.len_beats 変化で hash 変わる、 (4) clip.id 入替で hash 変わる。

    #[test]
    fn fold_arrangement_clip_hash_stable_for_unchanged_data() {
        let tracks = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "c0"), clip(101, 8.0, 2.0, "c1")],
        )];
        let h1 = fold_arrangement_clip_hash(&tracks);
        let h2 = fold_arrangement_clip_hash(&tracks);
        assert_eq!(h1, h2, "同じ tracks slice の fold は冪等");
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_move() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let after = vec![track(10, "t0", vec![clip(100, 4.0, 4.0, "c")])]; // start_beat 0 → 4
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.start_beat 変化で hash が変わる (#011 残像 fix)"
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_resize() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let after = vec![track(10, "t0", vec![clip(100, 0.0, 6.0, "c")])]; // len_beats 4 → 6
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.len_beats 変化で hash が変わる (#011 残像 fix)"
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_id_swap() {
        let before = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "c"), clip(101, 8.0, 2.0, "d")],
        )];
        let after = vec![track(
            10,
            "t0",
            vec![clip(101, 0.0, 4.0, "c"), clip(100, 8.0, 2.0, "d")],
        )]; // id 入替 (位置同じでも identity 違う)
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.id 入替で hash が変わる (FNV identity 確認)"
        );
    }

    /// MIDI clip arm の hash gap fix (share_group_color / color / name) regression test。
    /// caller が share_group / clip color を変更した frame で viewport_key も更新されることを保証。
    #[test]
    fn fold_arrangement_clip_hash_changes_on_share_group_color() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let mut c_after = clip(100, 0.0, 4.0, "c");
        c_after.share_group_color = Some(0.5);
        let after = vec![track(10, "t0", vec![c_after])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.share_group_color None→Some で hash が変わる",
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_color() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let mut c_after = clip(100, 0.0, 4.0, "c");
        c_after.color = Some(daw_ui_renderer::Color::rgb(0.8, 0.2, 0.2));
        let after = vec![track(10, "t0", vec![c_after])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.color 変化で hash が変わる",
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_rename() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "old name")])];
        let after = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "new name")])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.name (Arc<str>) ptr 変化で hash が変わる",
        );
    }

    // ============================================================
    // M14 Phase 63c (#016): group hierarchy + multi-select + reparent
    // ============================================================

    /// `parent_id` を持つ track 1 つ作る helper (test 専用)。
    fn track_with_parent(
        id: u32,
        name: &str,
        parent_id: Option<u32>,
        depth: u8,
        collapsed: bool,
    ) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            armed: false,
            clips: Vec::new(),
            volume: 1.0,
            parent_id,
            depth,
            collapsed,
            kind: TrackKind::Audio,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            row_h: None,
            color: None,
        }
    }

    #[test]
    fn is_group_track_returns_true_when_child_exists() {
        // `1` (parent) → `2`, `3` (children); `1` is group, `2`/`3` are leaves
        let tracks = vec![
            track_with_parent(1, "g", None, 0, false),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(1), 1, false),
        ];
        assert!(is_group_track(1, &tracks), "1 has children → is_group");
        assert!(!is_group_track(2, &tracks), "2 is leaf → not is_group");
        assert!(!is_group_track(3, &tracks), "3 is leaf → not is_group");
    }

    #[test]
    fn is_visible_track_returns_false_when_ancestor_collapsed() {
        // `1` collapsed → `2` (child), `3` (grandchild) hidden; `4` (sibling) visible
        let tracks = vec![
            track_with_parent(1, "g", None, 0, true),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(2), 2, false),
            track_with_parent(4, "leaf", None, 0, false),
        ];
        assert!(is_visible_track(&tracks[0], &tracks), "root 自身は visible (collapsed 適用は子のみ)");
        assert!(!is_visible_track(&tracks[1], &tracks), "親 1 が collapsed → 子 2 は不可視");
        assert!(!is_visible_track(&tracks[2], &tracks), "祖父 1 が collapsed → 孫 3 は不可視");
        assert!(is_visible_track(&tracks[3], &tracks), "別 chain の 4 は visible");
    }

    #[test]
    fn compute_visible_indices_skips_collapsed_subtree() {
        let tracks = vec![
            track_with_parent(1, "g", None, 0, true),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(2), 2, false),
            track_with_parent(4, "leaf", None, 0, false),
        ];
        let visible = compute_visible_indices(&tracks);
        assert_eq!(
            visible,
            vec![0, 3],
            "collapsed 親 1 の subtree (2, 3) は skip、 visible は [0, 3]"
        );
    }

    #[test]
    fn disclosure_rect_within_name_rect_left_edge() {
        // disclosure rect は name_rect の左端から indent_px 幅で切り出し
        let style = test_style();
        let name_rect = Rect { x: 100.0, y: 50.0, w: 120.0, h: 24.0 };
        let r = disclosure_rect_for(name_rect, &style, 0);
        assert!((r.x - 100.0).abs() < 1e-6, "disclosure x は name_rect 左端");
        assert!(r.w >= 8.0, "disclosure 幅は 8px 以上");
        assert!(r.w <= style.indent_px, "disclosure 幅は indent_px (= 16) 以下");
        assert!(r.y >= name_rect.y && r.y + r.h <= name_rect.y + name_rect.h, "y range は name_rect 内");
    }

    // ---- r.md #35: clip の Shift+click 範囲選択 (可視 track 行 × 時間の長方形ブロック) ----
    // 選択遷移そのものの網羅は `widgets::select_modifier` の unit test 側。 ここでは
    // **arrangement の実データ (`ArrangementTrack` / `ClipView`) から範囲表を組む配線**
    // (`clip_range_items`) が行 index と時間範囲を正しく載せているかを検証する。

    /// 3 track × 4 拍グリッド。 各 track に 1 拍幅の clip が 4 個 (id は track ごとに 1..=4)。
    fn range_test_tracks() -> Vec<ArrangementTrack> {
        (0..3)
            .map(|t| {
                let clips = (0..4)
                    .map(|c| clip(c + 1, f64::from(c), 1.0, "c"))
                    .collect();
                track(t + 1, "T", clips)
            })
            .collect()
    }

    #[test]
    fn clip_range_items_は可視行indexと時間範囲を載せる() {
        let tracks = range_test_tracks();
        let items = clip_range_items(&tracks);
        assert_eq!(items.len(), 12, "3 track × 4 clip");
        // 先頭 track の 2 番目の clip: row=0 / beat 1..2。
        let it = items.iter().find(|i| i.key == ClipKey { track: 1, clip: 2 }).unwrap();
        assert_eq!(it.row, 0);
        assert!((it.start - 1.0).abs() < 1e-9 && (it.end - 2.0).abs() < 1e-9);
        // 3 番目の track は row=2。
        let it3 = items.iter().find(|i| i.key == ClipKey { track: 3, clip: 1 }).unwrap();
        assert_eq!(it3.row, 2);
    }

    #[test]
    fn shift_click_は_track_をまたぐ長方形ブロックを選ぶ() {
        // アンカー = track1 の clip2 (beat 1..2)、 clicked = track3 の clip3 (beat 2..3)。
        // → track1..3 × beat 1..3 の 6 個 (各 track の clip2, clip3)。
        let tracks = range_test_tracks();
        let items = clip_range_items(&tracks);
        let anchor = ClipKey { track: 1, clip: 2 };
        let clicked = ClipKey { track: 3, clip: 3 };
        let next = SelectModifier::RangeFromAnchor
            .resolve(&[anchor], clicked, || range_block(&items, anchor, clicked));
        let expect: Vec<ClipKey> = [1_u32, 2, 3]
            .iter()
            .flat_map(|t| [2_u32, 3].iter().map(move |c| ClipKey { track: *t, clip: *c }))
            .collect();
        assert_eq!(next, expect);
    }

    #[test]
    fn shift_click_は同じtrack内ならその行だけ選ぶ() {
        let tracks = range_test_tracks();
        let items = clip_range_items(&tracks);
        let anchor = ClipKey { track: 2, clip: 2 };
        let clicked = ClipKey { track: 2, clip: 4 };
        let next = SelectModifier::RangeFromAnchor
            .resolve(&[anchor], clicked, || range_block(&items, anchor, clicked));
        assert_eq!(
            next,
            vec![
                ClipKey { track: 2, clip: 2 },
                ClipKey { track: 2, clip: 3 },
                ClipKey { track: 2, clip: 4 },
            ],
            "隣接 clip は端点を共有するが、 範囲外の clip1 は含めない"
        );
    }

    #[test]
    fn shift_click_はアンカーが無ければ単一選択に倒れる() {
        let tracks = range_test_tracks();
        let items = clip_range_items(&tracks);
        let clicked = ClipKey { track: 2, clip: 3 };
        // アンカー未設定 (None) を模す → range が None → Single 相当。
        let next = SelectModifier::RangeFromAnchor.resolve(&[], clicked, || {
            let anchor: Option<ClipKey> = None;
            range_block(&items, anchor?, clicked)
        });
        assert_eq!(next, vec![clicked]);
    }

    #[test]
    fn arrangement_style_has_indent_and_disclosure_defaults() {
        let s = test_style();
        assert!(s.indent_px > 0.0, "indent_px は 0 以上 (default 16)");
        assert!(s.indent_px <= 32.0, "indent_px は実用範囲 (~16-32) 内");
        // M14 Phase 113 (daw_01 #085): group track 専用背景 (旧 track_group_bg) は撤去。 group の
        // 構造手掛かりは disclosure ▶▼ + indent のみなので、 disclosure_color が可視色であることを確認。
        assert!(
            s.disclosure_color.r > 0.0 || s.disclosure_color.g > 0.0 || s.disclosure_color.b > 0.0,
            "disclosure_color は黒以外の色"
        );
    }

    // ============================================================
    // M14 Phase 63j (#024): ruler click / drag による playhead seek
    // ============================================================

    // ============================================================
    // M14 Phase 63j (#024): loop range edit に snap 適用
    // ============================================================
    //
    // `compute_loop_drag_endpoints` の unit test 7 件 (Start/End/Middle/NewRange の
    // snap 適用 / alt bypass / snap OFF) は M14 Phase 69 (#041) で
    // `crate::widgets::ruler_ops::tests` に extract (piano_roll と共有)。

    // -------- M14 Phase 63k (#025): audio_edit grip hit-test + drag commit -----------------------

    /// audio_edit が None の clip では audio_grip_hit が常に None を返す (= MIDI / Vocal clip は
    /// 既存挙動、 audio gesture は完全 disable)。
    #[test]
    fn audio_grip_hit_returns_none_when_audio_edit_is_none() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        // fade を持たない clip は中央 click でも fade 角 click でも None
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        // Get hit at clip middle
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 80.0, 16.0, &style),
            None,
            "fade を持たない clip の中央は何も返さない"
        );
        // Get hit at top-left corner
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 6.0, 6.0, &style),
            None,
            "fade を持たない clip は FadeCornerIn を返さない"
        );
    }

    /// r.md #73: clip 中央には **もう掴み所が無い** (gain の上下ドラッグを撤去した)。
    /// fade の角だけが残る。
    #[test]
    fn audio_grip_hit_returns_nothing_at_clip_middle() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        // clip rect = (0, 2, 160, 28)、中央 (80, 16) は旧 gain handle band の内側。
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c")])];
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 80.0, 16.0, &style),
            None,
            "clip 中央の gain 帯は撤去済み"
        );
    }

    /// fade in 角 (r.md #46: clip 名帯の **下**、中身領域の上端左 12×12) は FadeCornerIn。
    #[test]
    fn audio_grip_hit_returns_fade_corner_in_at_top_left() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c")])];
        // clip rect = (0, 2, 160, 28)、名前帯 = 14px → 中身領域は y ∈ [16, 30)。
        // 掴む正方形は (0..12, 16..28) → cx=6, cy=20。
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 6.0, 20.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerIn { event_index: 0 }))
        );
        // r.md #46: 名前帯の中 (cy=6) はもう掴む場所ではない (クリップ名と重ならない)。
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 6.0, 6.0, &style),
            None,
            "クリップ名の帯に fade の四角が食い込んではいけない"
        );
    }

    /// fade out 角 (中身領域の上端右 12×12) は FadeCornerOut を返す。
    #[test]
    fn audio_grip_hit_returns_fade_corner_out_at_top_right() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c")])];
        // clip rect = (0, 2, 160, 28) → 掴む正方形は (148..160, 16..28) → cx=155, cy=20。
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 155.0, 20.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerOut { event_index: 0 }))
        );
    }

    /// 短 clip (`r.w < audio_min_clip_w_for_handles_px`) は audio grip 全 disable。
    #[test]
    fn audio_grip_hit_returns_none_for_short_clip() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        // len_beats=0.5 → w = 20px、 default min = 32 → grip disable
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 0.5, "c")])];
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 10.0, 16.0, &style),
            None
        );
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 2.0, 6.0, &style),
            None
        );
    }

    /// FadeCurve.next() は Linear → Exp → SCurve → Linear の cycle。
    #[test]
    fn fade_curve_next_cycles() {
        assert_eq!(FadeCurve::Linear.next(), FadeCurve::Exponential);
        assert_eq!(FadeCurve::Exponential.next(), FadeCurve::SCurve);
        assert_eq!(FadeCurve::SCurve.next(), FadeCurve::Linear);
    }

    // r.md #73: `compute_audio_drag_outcome` の Gain 分岐 (dy → dB、 ±range clamp) の
    // 2 件は、クリップ中央のゲインドラッグごと撤去したので削除した。

    /// compute_audio_drag_outcome: FadeIn + horizontal lock は dx 正で fade_in_beats 増加。
    /// beat_per_px = 0.025 (= 40 px/beat), dx = +40 px → +1 beat.
    #[test]
    fn compute_audio_drag_outcome_fade_in_horizontal_changes_length() {
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(4.0, 0.5, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (0.0, 0.0),
            last_mouse: (40.0, 0.0),
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025) {
            Some(AudioDragOutcome::FadeLength { edge, next_beats }) => {
                assert_eq!(edge, FadeEdge::In);
                // anchor 0.5 + delta 1.0 = 1.5
                assert!((next_beats - 1.5).abs() < 1e-6, "got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// FadeOut + horizontal lock は dx **負** で fade_out_beats 増加 (右側から内側に伸びる)。
    #[test]
    fn compute_audio_drag_outcome_fade_out_horizontal_uses_negative_dx() {
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeOut,
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(4.0, 0.0, 0.5, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (160.0, 0.0),
            last_mouse: (120.0, 0.0), // dx = -40
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025) {
            Some(AudioDragOutcome::FadeLength { edge, next_beats }) => {
                assert_eq!(edge, FadeEdge::Out);
                // dx=-40 → -40 * 0.025 = -1.0、 FadeOut signed = -(-1) = +1
                // anchor 0.5 + 1.0 = 1.5
                assert!((next_beats - 1.5).abs() < 1e-6, "got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// fade length は `0..=clip_len_beats` に clamp される。
    #[test]
    fn compute_audio_drag_outcome_fade_length_clamps_to_clip_len() {
        // dx = +400 px @ 0.025 beat/px = +10 beat → 0.5 + 10 = 10.5、 clamp to clip_len 4.0
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(4.0, 0.5, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (0.0, 0.0),
            last_mouse: (400.0, 0.0),
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025) {
            Some(AudioDragOutcome::FadeLength { next_beats, .. }) => {
                assert!((next_beats - 4.0).abs() < 1e-6, "clamped to 4.0: got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// r.md #38: fade length の上限は **clip 長ではなく event 長**。 音 / 映像 / 画像 /
    /// 字幕はどれも event 長基準で fade を適用するので、 clip より短い event
    /// (trim / split 後) では event 長で頭打ちにならないと絵と実挙動がずれる。
    #[test]
    fn compute_audio_drag_outcome_fade_length_clamps_to_event_len_not_clip_len() {
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            // clip は 4 拍だが event は先頭 1 拍だけ。
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(1.0, 0.0, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (0.0, 0.0),
            last_mouse: (400.0, 0.0), // +10 beat 相当
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025) {
            Some(AudioDragOutcome::FadeLength { next_beats, .. }) => {
                assert!(
                    (next_beats - 1.0).abs() < 1e-6,
                    "event 長 1.0 で頭打ち (clip 長 4.0 ではない): got {next_beats}"
                );
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    // -------- r.md #45 / #46: 可変背景 (clip 色) 上のコントラスト ----------------

    /// 波形色は clip の **実塗り色** から選ぶ。明るい clip では暗い波形、
    /// 暗い clip では明るい波形になる (固定ブルーだと明 clip 上で沈む = r.md #45)。
    /// クリップピーク (赤) も同じ規律で明暗を切り替える。
    ///
    /// r.md #48: 波形インクは極性固定なので、この関係は **両テーマで同じ** でなければ
    /// ならない (ライトで反転すると明るいクリップ上の波形が消える)。
    #[test]
    fn waveform_colors_follow_clip_fill_luminance() {
        let dark_clip = Color::rgb(0.10, 0.12, 0.18);
        let bright_clip = Color::rgb(0.95, 0.80, 0.35); // 既定パレットのアンバー相当
        let lane_bg = Color::rgb(0.06, 0.07, 0.09);
        for id in ["dark", "light"] {
            let theme = crate::theme::Theme::builtin(id).unwrap();
            let p = &theme.core;
            for selected in [false, true] {
                let (fg_dark_bg, peak_dark_bg) =
                    waveform_colors_for(p, dark_clip, lane_bg, selected);
                let (fg_bright_bg, peak_bright_bg) =
                    waveform_colors_for(p, bright_clip, lane_bg, selected);
                let lum = |c: Color| daw_ui_core::color::relative_luminance(c.r, c.g, c.b);
                assert!(
                    lum(fg_dark_bg) > lum(fg_bright_bg),
                    "暗 clip では明るい波形 / 明 clip では暗い波形 (theme={id} selected={selected})"
                );
                assert!(
                    lum(peak_dark_bg) > lum(peak_bright_bg),
                    "クリップピークも背景輝度で切り替わる (theme={id} selected={selected})"
                );
            }
        }
    }

    /// fade の前景と裏打ちは **逆極性**。どちらの下地 (clip 色 / 波形) でも縁が立つ。
    /// r.md #48: 極性の判定は `Palette::ink_for` なので両テーマで同じ向きになる。
    #[test]
    fn fade_colors_are_opposite_polarity_and_follow_clip_fill() {
        let lum = |c: Color| daw_ui_core::color::relative_luminance(c.r, c.g, c.b);
        for id in ["dark", "light"] {
            let theme = crate::theme::Theme::builtin(id).unwrap();
            let p = &theme.core;
            let style = ArrangementStyle::from_theme(&theme);
            let lane_bg = style.bg;

            let (fg, backing) =
                fade_colors_for(p, &style, Color::rgb(0.10, 0.12, 0.18), lane_bg);
            assert!(lum(fg) > lum(backing), "theme={id}: 暗 clip 上は明色の前景 + 暗い裏打ち");

            let (fg2, backing2) =
                fade_colors_for(p, &style, Color::rgb(0.95, 0.80, 0.35), lane_bg);
            assert!(lum(fg2) < lum(backing2), "theme={id}: 明 clip 上は暗色の前景 + 明るい裏打ち");
        }
    }

    /// muted clip は fill が減光されるので、中身の色もその **減光後** の色を基準に選ぶ
    /// (`clip_effective_fill` が draw と共通の 1 本)。
    #[test]
    fn clip_effective_fill_applies_muted_dim() {
        let style = test_style();
        let base = Color::rgb(0.95, 0.80, 0.35);
        let mut c = fade_clip(1, 0.0, 4.0, "c", ev_fade(4.0, 0.0, 0.0, FadeCurve::Linear, FadeCurve::Linear));
        c.color = Some(base);
        c.muted = false;
        let normal = clip_effective_fill(&c, TrackKind::Audio, &style);
        c.muted = true;
        let dimmed = clip_effective_fill(&c, TrackKind::Audio, &style);
        assert!(dimmed.a < normal.a, "muted は alpha を落として lane 背景を透かす");
        // 暗い lane 背景に合成した実効色は暗くなる = 中身の色選択もそれに追従する。
        let eff = |x: Color| {
            let o = daw_ui_core::color::composite_over(x, style.bg);
            daw_ui_core::color::relative_luminance(o.r, o.g, o.b)
        };
        assert!(eff(dimmed) < eff(normal), "合成後の実効輝度が下がる");
    }

    // -------- r.md #38: fade 幾何 (描画と hit-test の SSoT) ----------------------

    /// fade in の線は「event 左端の **下端** (無音、 固定) → fade 末尾の **上端** (フル)」。
    /// r.md #38 以前は上下が逆で、 固定端が上・可動端が下だった (= fade out の絵)。
    #[test]
    fn fade_geometry_in_anchors_at_bottom_and_handle_at_top() {
        let style = test_style();
        let r = Rect { x: 0.0, y: 10.0, w: 160.0, h: 28.0 };
        let f = ClipEventFade {
            event_index: 0,
            fade: ev_fade(4.0, 1.0, 0.0, FadeCurve::Linear, FadeCurve::Linear),
        };
        let g = fade_geometry(r, test_content_map(r, 4.0), &f, FadeEdge::In, &style);
        // r.md #46: fade は clip 名の帯を避けて中身領域 (上インセット 14px) に描く。
        let content_top = r.y + clip_content_inset_top(&style);
        assert!((g.anchor[0] - r.x).abs() < 1e-4, "無音端は event 左端");
        assert!((g.anchor[1] - (r.y + r.h)).abs() < 1e-4, "無音端は下端");
        assert!((g.handle[1] - content_top).abs() < 1e-4, "フル端は中身領域の上端");
        // 1 拍 / 4 拍 * 160px = 40px
        assert!((g.width_px - 40.0).abs() < 1e-3, "got {}", g.width_px);
        assert!((g.handle[0] - 40.0).abs() < 1e-3, "掴む点は fade 末尾へ動く");
    }

    /// fade out は「fade 開始の上端 → event 右端の下端」。
    #[test]
    fn fade_geometry_out_anchors_at_bottom_right() {
        let style = test_style();
        let r = Rect { x: 0.0, y: 10.0, w: 160.0, h: 28.0 };
        let f = ClipEventFade {
            event_index: 0,
            fade: ev_fade(4.0, 0.0, 1.0, FadeCurve::Linear, FadeCurve::Linear),
        };
        let g = fade_geometry(r, test_content_map(r, 4.0), &f, FadeEdge::Out, &style);
        let content_top = r.y + clip_content_inset_top(&style);
        assert!((g.anchor[0] - (r.x + r.w)).abs() < 1e-4, "無音端は event 右端");
        assert!((g.anchor[1] - (r.y + r.h)).abs() < 1e-4, "無音端は下端");
        assert!((g.handle[0] - 120.0).abs() < 1e-3, "フル端は右端から 40px 内側");
        assert!((g.handle[1] - content_top).abs() < 1e-4, "フル端は中身領域の上端");
    }

    /// fade = 0 のとき掴む正方形は event の角に一致する (従来の操作感を保つ)。
    #[test]
    fn fade_geometry_zero_fade_keeps_handle_at_corner() {
        let style = test_style();
        let r = Rect { x: 5.0, y: 10.0, w: 160.0, h: 28.0 };
        let f = ClipEventFade {
            event_index: 0,
            fade: ev_fade(4.0, 0.0, 0.0, FadeCurve::Linear, FadeCurve::Linear),
        };
        let corner = style.audio_fade_corner_size_px;
        let g_in = fade_geometry(r, test_content_map(r, 4.0), &f, FadeEdge::In, &style);
        assert!((g_in.handle_rect.x - r.x).abs() < 1e-4);
        let g_out = fade_geometry(r, test_content_map(r, 4.0), &f, FadeEdge::Out, &style);
        assert!((g_out.handle_rect.x - (r.x + r.w - corner)).abs() < 1e-4);
    }

    /// event が clip の一部しか占めない場合、 fade は **その event の矩形** を基準に描かれる。
    #[test]
    fn fade_geometry_uses_event_rect_not_clip_rect() {
        let style = test_style();
        // clip = 4 拍 / 160px、 event は 1 拍目から 2 拍ぶん (40px 〜 120px)。
        let r = Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 };
        let f = ClipEventFade {
            event_index: 0,
            fade: common::model::EventFade {
                start_in_clip_beats: 1.0,
                len_beats: 2.0,
                fade_in_beats: 0.0,
                fade_out_beats: 0.0,
                fade_in_curve: FadeCurve::Linear,
                fade_out_curve: FadeCurve::Linear,
            },
        };
        let g = fade_geometry(r, test_content_map(r, 4.0), &f, FadeEdge::In, &style);
        assert!((g.event_rect.x - 40.0).abs() < 1e-3, "got {}", g.event_rect.x);
        assert!((g.event_rect.w - 80.0).abs() < 1e-3, "got {}", g.event_rect.w);
        assert!((g.anchor[0] - 40.0).abs() < 1e-3, "無音端は event 左端 (clip 左端ではない)");
    }

    /// fade_in が event 全長 / fade_out が 0 のとき、 両方の掴む正方形が
    /// **同じ位置に重なる**。 単純な後勝ちだと退化した Out (幅 0) が常に勝ち、
    /// 実際に伸びている In を掴めなくなるので、 幅の大きい方が勝つこと。
    #[test]
    fn audio_grip_hit_prefers_wider_fade_when_handles_coincide() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        // fade_in = 4 拍 (= event 全長)、 fade_out = 0 → どちらの handle も event 右上。
        let c = fade_clip(100, 0.0, 4.0, "c", ev_fade(4.0, 4.0, 0.0, FadeCurve::Linear, FadeCurve::Linear));
        let tracks = vec![track(10, "t0", vec![c])];
        let tops = make_tops(&tracks, lanes, view);
        // clip rect = (0, 2, 160, 28) → 重なった handle_rect は x ∈ [148, 160)
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &tops, view, lanes, 152.0, 20.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerIn { event_index: 0 })),
            "幅のある fade in 側が掴める (幅 0 の fade out に奪われない)"
        );
    }

    /// 掴む正方形は fade 長に追従する: fade を伸ばすと元の角では掴めず、 fade 末尾で掴める。
    #[test]
    fn audio_grip_hit_follows_fade_end() {
        let view = test_view();
        let lanes = test_lanes();
        let style = test_style();
        // clip = 4 拍 → w = 160px。 fade_in = 2 拍 → 80px。
        let c = fade_clip(100, 0.0, 4.0, "c", ev_fade(4.0, 2.0, 0.0, FadeCurve::Linear, FadeCurve::Linear));
        let tracks = vec![track(10, "t0", vec![c])];
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &tops, view, lanes, 84.0, 20.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerIn { event_index: 0 })),
            "fade 末尾 (80px) の正方形で掴める"
        );
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &tops, view, lanes, 6.0, 20.0, &style),
            None,
            "clip 左端はもう掴む場所ではない (fade 末尾へ移動した)"
        );
    }

    /// FadeIn + vertical lock は curve 切替を返す (Linear → Exponential)。
    #[test]
    fn compute_audio_drag_outcome_fade_in_vertical_toggles_curve() {
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(4.0, 0.5, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (0.0, 0.0),
            last_mouse: (0.0, -20.0),
            locked_horizontal: Some(false),
        };
        match compute_audio_drag_outcome(&ad, 0.025) {
            Some(AudioDragOutcome::FadeCurve { edge, next_curve }) => {
                assert_eq!(edge, FadeEdge::In);
                assert_eq!(next_curve, FadeCurve::Exponential);
            }
            other => panic!("expected FadeCurve, got {other:?}"),
        }
    }

    /// sticky direction 未確定 (locked_horizontal = None) は no-op (None) を返す。
    #[test]
    fn compute_audio_drag_outcome_unlocked_returns_none() {
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor_fade: Some(ClipEventFade {
                event_index: 0,
                fade: ev_fade(4.0, 0.5, 0.0, FadeCurve::Linear, FadeCurve::Linear),
            }),
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            content_map_anchor: ContentMap { origin_x: 0.0, px_per_beat: 40.0 },
            clip_bg_anchor: daw_ui_renderer::Color::rgb(0.1, 0.1, 0.1),
            anchor_mouse: (0.0, 0.0),
            last_mouse: (3.0, 4.0), // < threshold 10 px
            locked_horizontal: None,
        };
        assert_eq!(compute_audio_drag_outcome(&ad, 0.025), None);
    }

    // r.md #73: `fold_arrangement_clip_hash_changes_on_gain_db` は削除した。 gain_db は
    // dB ハンドル線を描くためだけに widget モデル (`ClipView::audio_edit`) へ渡っていた値で、
    // 線ごと撤去したので widget はもう gain を知らない = hash に混ぜる対象でもない。

    #[test]
    fn fold_arrangement_clip_hash_changes_on_fade_curve() {
        // r.md #38: fade は `ClipView.fades` (per-event) に移ったので、 hash も
        // そちら経由で反応しなければならない (混ぜ忘れると fade を編集しても
        // cached が再構築されず線が更新されない)。
        let before = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c")])];
        let mut after = before.clone();
        after[0].clips[0].fades[0].fade.fade_in_curve = FadeCurve::Exponential;
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "fade_in_curve 変化で hash が変わる"
        );
        let mut after_len = before.clone();
        after_len[0].clips[0].fades[0].fade.fade_in_beats = 1.0;
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after_len),
            "fade_in_beats 変化で hash が変わる"
        );
    }

    // r.md #73: ここにあった flatten 検証 7 本と `sample_polyline_y` は
    // `tests_curve.rs` へ移した (god file budget + 曲線テストを 1 か所に集める)。
    // 移設時に「上り区間 / 下り区間の両方で確かめる」形へ書き直してある — 旧テストは
    // screen y で 1 方向しか見ておらず、 #73 の不具合 (上り区間で符号が逆) を通していた。

    // ============================================================
    // M14 Phase 77 (daw_01 #048): 縦 scroll 時の scissor 動作 unit test
    // ============================================================

    // ============================================================
    // M14 Phase 117 (daw_01 #091): header 幅 drag splitter
    // ============================================================

    /// `header_resize_splitter_at` が境界 `rect.x + header_w` 中心 ±handle/2 の縦帯 × 全高で hit、
    /// 帯の外 / header_w=0 / handle=0 で miss する。
    #[test]
    fn header_resize_splitter_at_hits_centered_full_height_band() {
        let style = test_style(); // header_resize_handle_px = 8 → ±4
        let rect = Rect { x: 100.0, y: 50.0, w: 800.0, h: 400.0 };
        let header_w = 160.0;
        let boundary = 100.0 + 160.0; // = 260
        // 境界中心: hit。
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 200.0));
        // 全高で hit (上端 / 下端近く)。
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 50.0));
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 449.0));
        // 帯端 (±4px) 内側: hit (256) / 外側: miss (255.9 は < 256 で外、 264 は半開で外)。
        assert!(header_resize_splitter_at(rect, header_w, &style, 256.0, 200.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, 255.0, 200.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, 264.0, 200.0));
        // rect の外 (上 / 下): miss。
        assert!(!header_resize_splitter_at(rect, header_w, &style, boundary, 49.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, boundary, 450.0));
        // header_w = 0 (header 無し): 常に miss。
        assert!(!header_resize_splitter_at(rect, 0.0, &style, 100.0, 200.0));
        // handle = 0: 無効化。
        let no_handle = ArrangementStyle { header_resize_handle_px: 0.0, ..test_style() };
        assert!(!header_resize_splitter_at(rect, header_w, &no_handle, boundary, 200.0));
    }

    // ============================================================
    // M14 Phase 118 (daw_01 #092): group track 名 double-click rename の信頼性
    // ============================================================

    // ============================================================
    // M14 Phase 125 (#102): plain-drag marquee select (REPLACE / UNION / XOR)
    // ============================================================




    // ============================================================
    // r.md #53: 追従スクロールは整数ピクセルの剛体平行移動でなければならない
    // ============================================================

    /// 追従スクロールを連続値で少しずつ進めても、各クリップの screen x は
    /// **整数ピクセル単位でしか動かない**。
    ///
    /// これが崩れると 1px 幅の枠線が毎フレーム別のサブピクセル位相で描かれ、
    /// 「くっきり ↔ にじみ」を往復してチラつく (= ユーザー報告の症状)。
    /// スナップは `pixel_snapped_scroll_beat` **1 箇所だけ**で行い、各アイテムの端は
    /// 丸めない (端ごとに丸めると今度は幅が 1px 揺れる) ので、この test は同時に
    /// 「幅がスクロールで変わらない」ことも押さえる。
    #[test]
    fn follow_scroll_moves_clips_by_whole_pixels() {
        let lanes = test_lanes();
        let zoom = 40.0_f32; // px/beat
        // 端数だらけの開始位置を並べて、位相がクリップごとに違う状況を作る。
        let clips = [
            clip(1, 0.0, 4.0, "a"),
            clip(2, 4.371, 2.5, "b"),
            clip(3, 9.013, 1.25, "c"),
        ];

        let mut prev: Option<Vec<Rect>> = None;
        // 1 フレームあたり 0.017 拍 (= 0.68px) ずつ進む = 整数 px に揃っていない刻み。
        for step in 0..60 {
            let scroll = f64::from(step) * 0.017;
            let view = ArrangementView {
                start_beat: pixel_snapped_scroll_beat(scroll, lanes.w, zoom),
                len_beats: view_len_beats(lanes.w, zoom),
                ..test_view()
            };
            let rects: Vec<Rect> = clips
                .iter()
                .map(|c| clip_to_rect(0.0, view.track_row_h, c, view, lanes))
                .collect();

            if let Some(prev) = &prev {
                for (i, (a, b)) in prev.iter().zip(&rects).enumerate() {
                    let dx = b.x - a.x;
                    assert!(
                        (dx - dx.round()).abs() < 1e-3,
                        "step {step} clip {i}: x が {dx} px 動いた (整数でない) \
                         — スクロール原点がピクセル境界に載っていない",
                    );
                    assert!(
                        (b.w - a.w).abs() < 1e-3,
                        "step {step} clip {i}: 幅が {} → {} と変わった",
                        a.w,
                        b.w,
                    );
                }
                // 全クリップが同じ量だけ動く (= 剛体平行移動)。
                let d0 = rects[0].x - prev[0].x;
                for (i, (a, b)) in prev.iter().zip(&rects).enumerate() {
                    assert!(
                        ((b.x - a.x) - d0).abs() < 1e-3,
                        "step {step} clip {i} だけ {} px 動いた (他は {d0} px)",
                        b.x - a.x,
                    );
                }
            }
            prev = Some(rects);
        }
    }

    /// スナップ後の原点で描いた x と、同じ原点を使う逆変換 (`px_to_beat`) が往復する。
    /// = 「見えている位置がそのまま掴める」不変条件。
    #[test]
    fn snapped_view_round_trips_through_px_to_beat() {
        let lanes = test_lanes();
        let zoom = 40.0_f32;
        let view = ArrangementView {
            start_beat: pixel_snapped_scroll_beat(3.271, lanes.w, zoom),
            len_beats: view_len_beats(lanes.w, zoom),
            ..test_view()
        };
        let c = clip(1, 7.5, 2.0, "a");
        let r = clip_to_rect(0.0, view.track_row_h, &c, view, lanes);
        let back = px_to_beat(r.x, lanes.x, lanes.w, view);
        assert!(
            (back - c.start_beat).abs() < 0.01,
            "px_to_beat({}) = {back}, expected {}",
            r.x,
            c.start_beat
        );
    }

    // ============================================================
    // daw_01 #071: automation clip 複数選択 (box-drag / shift-click / multi-move)
    // ============================================================



