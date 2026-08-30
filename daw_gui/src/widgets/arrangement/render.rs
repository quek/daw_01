//! S4b Phase D: arrangement widget の heavy (cached + overlay) 描画パス。
//! `arrangement()` の `ui.heavy(...)` closure body と、 その手前の cache key 構築 /
//! per-frame capture 整備。
//!
//! 旧実装は 36 個の per-frame capture を明示引数で受けていた (37 引数)。 現在は
//! フレーム不変の地形を `&ArrangementFrame`、 overlay 群を `&Overlays`、 このフェーズ
//! だけが必要とする owned 値を `&HeavyInput` にまとめて **4 引数**。

use super::*;

/// heavy 描画フェーズ。 cache key の構築 → `HeavyInput` の組み立て → `ui.heavy` の dispatch。
///
/// **`ui.heavy` / `hctx.cached` のクロージャに `'static` 境界は無い**
/// (`ui/crates/ui/src/widgets/heavy.rs` の `heavy` / `cached` の署名)。 `&ArrangementFrame` は
/// `ui` から一切借りていないのでそのまま持ち込める。 旧実装が `Arc::from(visible_tracks.clone())`
/// の毎フレーム deep clone と prefix-sum の 3 重計算をしていたのは、 この境界を誤読していたため。
///
/// **outer `app` を必要とする唯一のフェーズ** (`TempoMap::from_song` と `content_build` 系)。
///
/// `LiveSessions` は受け取らない。 このフェーズが session から必要とするものは
/// **すべて `Overlays` に入っている** (旧実装の heavy 引数も overlay 由来の 10 個だけで、
/// live session を直接読む箇所は 1 つも無かった)。 受け取っても使わない引数を置くと
/// 「ここから session を読んでよい」 という誤ったシグナルになる。
pub(super) fn dispatch(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    f: &ArrangementFrame<'_>,
    overlays: &Overlays,
    response: &ArrangementResponse,
) {
    // ---- 描画 (heavy + cached + 動的 overlay) ----
    // M10 Phase 50: pending_reorder_hash を viewport_key に入れて、release frame の optimistic
    // preview で cache miss を強制 (新順序での再描画を 1 frame 遅延なく行う)。
    // tuple Hash 実装は 12 要素まで → nested tuple で 13 要素分を表現。
    // M13 Phase 55: bpm / time_sig を 3 つ目の nested tuple で追加し v2 に bump。
    // M14 Phase 61b (#011): clip 個別の (id, start_beat, len_beats) 変化を widget 側で hash
    // して 4 つ目の outer 要素 internal_clip_hash として viewport_key に追加 + v3 に bump。
    // M14 Phase 63c (#016): selected_tracks を fold して selection 変化での cache miss を保証
    // (旧 `selected_track.unwrap_or(u32::MAX)` の単一 u32 に対し、 multi-select は集合 hash)。
    // 加えて parent_id / depth / collapsed の構成変化は data_generation で caller 責務 (group
    // 構成変化は track 構成変化と同義、 caller が data_generation を bump する前提)。
    // (review) fold は caller slice でなく **visible_tracks** (= collapsed subtree
    // 除外 + synthetic master prepend 済み) を対象にする。 旧実装は master row
    // (song_lanes) が hash 対象外で、 テンポ等の master automation 編集・折り畳み・
    // 高さ変更が cached 層で stale になっていた (cached は master 行も描く)。
    let internal_clip_hash = fold_arrangement_clip_hash(&f.visible_tracks);
    let selected_tracks_hash: u64 =
        f.selected_tracks.iter().fold(0xCBF2_9CE4_8422_2325_u64, |a, &x| {
            a.wrapping_mul(0x100_0000_01B3).wrapping_add(u64::from(x))
        });
    // M14 Phase 63n-3 (#028): selected_automation_clips を fold して cache 再構築を保証 (= 選択
    // 変化時に lane の clip rect 描画が selected_fill / selected_border に切り替わる)。
    let selected_automation_clips_hash: u64 =
        f.selected_automation_clips.iter().fold(0xCBF2_9CE4_8422_2325_u64, |a, k| {
            a.wrapping_mul(0x100_0000_01B3)
                .wrapping_add(u64::from(k.track))
                .wrapping_mul(0x100_0000_01B3)
                .wrapping_add(u64::from(k.lane))
                .wrapping_mul(0x100_0000_01B3)
                .wrapping_add(u64::from(k.clip))
        });
    // r.md #73: 曲げている最中の区間は cached 層が base curve を描かない (2 重線の防止)。
    // **cached の中身が変わるので key に入れる**。 hover (`hovered_segment`) を意図的に外して
    // あるのと対照的だが、こちらは値が変わるのが drag の開始と終了の 2 回だけなので、
    // 「マウスを動かすたびに全体再構築」 にはならない。
    let bend_skip = overlays.segment_bend.as_ref().map(|b| b.point);
    let bend_skip_hash: u64 = bend_skip.map_or(0, |k| {
        [k.clip.track, k.clip.lane, k.clip.clip, k.point_id].iter().fold(
            0xCBF2_9CE4_8422_2325_u64,
            |a, &x| a.wrapping_mul(0x100_0000_01B3).wrapping_add(u64::from(x)),
        )
    });
    let viewport_key = (
        (
            b"arrangement_widget_v8" as &[u8],
            f.rect.w.to_bits(),
            f.rect.h.to_bits(),
            f.view.start_beat.to_bits(),
            f.view.len_beats.to_bits(),
            f.view.track_top.to_bits(),
            f.view.track_row_h.to_bits(),
            f.view.tracks_visible.to_bits(),
            f.view.header_w.to_bits(),
            f.view.ruler_h.to_bits(),
            f.view.data_generation,
            selected_tracks_hash,
        ),
        overlays.reorder_hash,
        (f.view.bpm.to_bits(), u32::from(f.view.time_sig.0), u32::from(f.view.time_sig.1)),
        internal_clip_hash,
        selected_automation_clips_hash,
        // (review) cached primitives は絶対座標で再生されるため、 widget の
        // 位置 (rect.x/y) と arranger lane 高さ (lanes.y のオフセット成分) も
        // key に含める — 「サイズ不変で位置 / lane 高さだけ変わる」 layout
        // 変化で旧座標に描かれるのを correct-by-construction で防ぐ。
        (f.rect.x.to_bits(), f.rect.y.to_bits(), f.view.arranger_lane_h.to_bits()),
        bend_skip_hash,
    );

    // M14 Phase 63n-3 (#028) / 63c (#016) / 63n-8 (#033): heavy 用 selection 集合。
    // 旧実装はこれを 4 本 + `selected_tracks_for_heavy` の計 5 本 clone していたが、
    // borrow で足りるもの (`selected_tracks`) は `f` から直接読む。
    let selected_clip_set: HashSet<ClipKey> = f.selected_clips.iter().copied().collect();
    let selected_automation_clip_set: HashSet<AutomationClipKey> =
        f.selected_automation_clips.iter().copied().collect();
    let selected_automation_point_set: HashSet<AutomationPointKey> =
        f.selected_automation_points.iter().copied().collect();

    // M13 Phase 55: ruler / lanes grid を library `time_ruler` / `bar_beat_grid` に統合。
    // beat 単位の view を sample 単位の `ViewportState1D` に変換 (sample_rate = 48k で
    // 比例定数は打ち消されるので BarBeat 表示には影響しない)。
    let mapping = TimeMapping {
        sample_rate: 48_000.0,
        tempo_bpm: f64::from(f.view.bpm.max(1.0)),
        time_sig: (f.view.time_sig.0.max(1), f.view.time_sig.1.max(1)),
        display: TimeDisplay::BarBeat,
    };
    let spb = mapping.samples_per_beat();
    let sample_viewport =
        ViewportState1D::new(f.view.start_beat * spb, f.view.len_beats.max(1e-6) * spb);
    // r.md #48: 汎用 widget の style はパレットから組む (`Default` は「いまどのテーマで
    // 描いているか」 を知れないので廃止された)。色は arrangement の style が SSoT なので
    // 上書きし、間引き閾値だけパレット既定を引き継ぐ。
    let palette = ui.palette();
    let grid_style = BarBeatGridStyle {
        bar_color: f.style.bar_line,
        beat_color: f.style.beat_line,
        bar_line_width: f.style.bar_line_width_px,
        beat_line_width: f.style.beat_line_width_px,
        // M14 Phase 63m (daw_01 #027): zoom 連動の beat 線間引き (default 4px)。
        ..BarBeatGridStyle::from_palette(palette)
    };
    let ruler_style = TimeRulerStyle {
        bg: f.style.ruler_bg,
        tick_color: f.style.bar_line,
        label_color: f.style.ruler_label_color,
        bar_tick_height: 12.0,
        beat_tick_height: 5.0,
        // M14 Phase 63m (daw_01 #027): zoom 連動の label / beat tick 間引き (default 60 / 4 px)。
        ..TimeRulerStyle::from_palette(palette)
    };

    // S4b Phase C / r.md #68: 波形 / MIDI プレビューの中身を model + audio cache から
    // 1 フレーム分だけ集める (`Arc<AudioSourceBuffer>` は refcount clone で安価)。
    // visible clip のみ。 座標は **content-local 拍** で、 画面 x への
    // 換算は widget 側の `content_map` (content 原点 + ビューのズーム) が 1 本で行う。
    // SongTempo automation を持つ曲だけ曲線評価になる (無ければ定数 = 従来と同コスト)。
    // base とゴーストで **同じ写像** を使う (engine と同じ `event_wave_spans` の入力)。
    let tempo_map = common::audio_render::TempoMap::from_song(app.song_doc.song());
    let clip_content = content_build::build_clip_content(app, &tempo_map, &f.visible_tracks);
    // r.md #68: Shift + 端 drag (= time-stretch) のときだけ、 ゴーストの中身を
    // commit と同じ `stretch_remap` + `event_wave_spans` で組み直す (Slice 配置 /
    // Raw→Stretch 昇格まで含めてプレビュー = 確定結果)。 トリム / 移動では確定後の
    // 中身が base と同一なので空 = 描画側が `clip_content` をそのまま使う。
    let stretch_ghost_content = content_build::build_stretch_ghost_content(
        app,
        &tempo_map,
        &f.visible_tracks,
        overlays.clip.as_ref(),
        overlays.clip_min_len,
    );

    let heavy = HeavyInput {
        viewport_key_hash: hash_inputs(viewport_key),
        // 旧 `id_for_inner`。 heavy closure が `'static` を要求すると誤読して id を hash 化
        // していた名残だが、 hash 値そのものは `bar_beat_grid` / `time_ruler` の widget id に
        // 使われているので値は 1 bit も変えない。
        id_hash: hash_inputs(f.id),
        // r.md #58: フェードの掴む正方形を出す clip。 `response.hovered_clip` は
        // `cursor::hover` で **このフレーム中に** 確定済みなので、 caller 側ミラー
        // (`app.ui_ephemeral.arrangement_hover_clip`、 1 フレーム遅れ) ではなくこちらを使う。
        // **`viewport_key` にも `fold_arrangement_clip_hash` にも入れないこと。**
        hovered_clip: response.hovered_clip,
        // r.md #73: Alt hover 中の「曲げられる区間」。 `hovered_clip` と同じく
        // `cursor::hover` がこのフレーム中に確定させた値を heavy へ渡す
        // (`render_arrangement_heavy` は response を受け取らないので、
        // **response に足しただけでは強調が描けない**)。 cache キーには入れない。
        hovered_segment: response.hovered_automation_segment,
        // r.md #73: 曲げている最中の区間 (cached 層で base curve を描かない対象)。
        // `hovered_segment` と違い `viewport_key` に入っている (上の `bend_skip_hash`)。
        bend_skip,
        clip_content,
        stretch_ghost_content,
        selected_clip_set,
        selected_automation_clip_set,
        selected_automation_point_set,
        mapping,
        sample_viewport,
        grid_style,
        ruler_style,
    };

    ui.heavy(("arrangement_inner", &f.id), |hctx| {
        render_arrangement_heavy(hctx, f, &heavy, overlays);
    });
}

/// heavy クロージャに渡す「このフレームだけの owned 値」。 `f` から借りられるもの
/// (`visible_tracks` / `tops` / `sections` / `selected_*` のスライス) は**入れない**。
pub(super) struct HeavyInput {
    pub viewport_key_hash: u64,
    pub id_hash: u64,
    /// r.md #58: フェードの掴む正方形を出す clip。 `response.hovered_clip` の写し。
    /// **`viewport_key_hash` の材料にしてはいけない** (hover でアレンジ全体が再構築される)。
    pub hovered_clip: Option<ClipKey>,
    /// r.md #73: Alt hover 中に「曲げられる区間」があるならその入射側 point。
    /// Alt 強調 (overlay 層) を出す対象を決めるためだけに使う。
    /// **`viewport_key_hash` の材料にしてはいけない** (`hovered_clip` と同じ罠)。
    pub hovered_segment: Option<AutomationPointIdKey>,
    /// r.md #73: Alt+ドラッグで曲げている最中の区間。cached 層はこの区間の base curve を
    /// **描かない** (`draw_automation_lane` の `bend_skip`)。 preview だけが線になるので
    /// 「元の形の上に preview が重なって 2 重に見える」 が原理的に起きない。
    ///
    /// これは `hovered_segment` と違って **`viewport_key_hash` の材料にする** (下の
    /// `bend_skip_hash`)。 cached の中身が変わるので入れないと反映されないし、値が変わるのは
    /// ドラッグの開始と終了の 2 回だけなので hover のような毎フレーム再構築にはならない。
    pub bend_skip: Option<AutomationPointIdKey>,
    pub clip_content: HashMap<ClipKey, ClipContentDraw>,
    /// r.md #68: Shift + 端 drag のときだけ入る「伸縮済みの中身」。 空のときは
    /// ゴーストも `clip_content` をそのまま描く (トリム / 移動では確定後の中身が
    /// base と同一 = 中身は 1px も動かない)。
    pub stretch_ghost_content: HashMap<ClipKey, ClipContentDraw>,
    pub selected_clip_set: HashSet<ClipKey>,
    pub selected_automation_clip_set: HashSet<AutomationClipKey>,
    pub selected_automation_point_set: HashSet<AutomationPointKey>,
    pub mapping: TimeMapping,
    pub sample_viewport: ViewportState1D,
    pub grid_style: BarBeatGridStyle,
    pub ruler_style: TimeRulerStyle,
}

fn render_arrangement_heavy(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    heavy: &HeavyInput,
    overlays: &Overlays,
) {
    let lanes = f.lanes;
    let ruler = f.ruler;
    let header_pane = f.header_pane;
    // r.md #48: このフレームの色パレット。`&'a Palette` (host 寿命) なので、以降の
    // `hctx.cached(..)` / `hctx.with_clip_rect(..)` クロージャにそのまま持ち込める。
    let p = hctx.palette();
    // M14 Phase 77 (daw_01 #048): track_top に依存する draw を scope 単位で scissor。
    // `below_ruler` は ruler 下の領域 (= header_pane ∪ lanes)、 automation lane / reorder
    // overlay 等 header と lanes をまたぐ draw 用。 ruler / loop_band / playhead は
    // track_top に依存しない static draw なので scope 外に置いて既存挙動維持。
    // r.md #87: ランチャー帯が header と lanes の間に挟まるので、幅は `f` が持つ
    // 実測値を使う (`header_pane.w + lanes.w` で再導出すると帯のぶん右端が欠ける)。
    let below_ruler = f.content_below_ruler;
    // === cached: viewport_key 一致時 skip ===
    hctx.cached(heavy.viewport_key_hash, |hctx| {
        push_filled_rect(hctx, header_pane, f.style.header_bg);
        // M14 Phase 77 (daw_01 #048): lanes scope (track row 系の y 依存 draw)。
        hctx.with_clip_rect(lanes, |hctx| {
            draw_lanes_bg(
                hctx,
                lanes,
                &f.visible_tracks,
                &f.tops,
                f.view,
                f.selected_tracks,
                f.style,
            );
            hctx.ui_mut().bar_beat_grid(
                ("arr_grid", heavy.id_hash),
                lanes,
                heavy.mapping,
                heavy.sample_viewport,
                heavy.grid_style,
                // M14 Phase 124 (#100): subdivision はピアノロール限定なので arrangement は None。
                None,
            );
            draw_clips(hctx, &f.visible_tracks, &f.tops, f.view, lanes, f.style);
            // M14 Phase 63k (#025): fade を持つ clip に envelope を重ねる。
            // 描画は draw_clips 後 (clip rect の上に重なる)、 selection overlay より前 (selection の
            // 黄色 fill が上書きしない、 selection 中も fade が見える)。
            let view_end_for_audio = f.view.start_beat + f.view.len_beats;
            for (i, t) in f.visible_tracks.iter().enumerate() {
                let row_top = f.tops[i];
                // draw_clips と同じ per-track 実効行高 (culling / rect の両方)。
                let row_h = effective_track_row_h(t, f.view.track_row_h);
                if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                    continue;
                }
                for c in &t.clips {
                    if c.fades.is_empty() {
                        continue;
                    }
                    let end = c.start_beat + c.len_beats;
                    if end < f.view.start_beat || c.start_beat > view_end_for_audio {
                        continue;
                    }
                    let r = clip_to_rect(row_top, row_h, c, f.view, lanes);
                    if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                        continue;
                    }
                    if r.w < f.style.audio_min_clip_w_for_handles_px {
                        continue;
                    }
                    // r.md #38: fade は content 種別に依らず全 clip 種別に描く
                    // (音声だけでなく映像 / 画像 / 字幕も同じ見た目)。
                    // r.md #46: fade の色は clip の実塗り色から auto-contrast。
                    // r.md #58: cached 層に残るのは**カーブだけ**。 掴む正方形は
                    // hover 中の clip にだけ出すので cached 外の overlay で描く。
                    draw_clip_fade_curves(
                        hctx,
                        r,
                        content_map(c, f.view, lanes),
                        &c.fades,
                        clip_effective_fill(c, t.kind, f.style),
                        f.style,
                    );
                }
            }
        });
        // M14 Phase 77 (daw_01 #048): ruler scope (static、 track_top に依存しない static
        // primitive だが defensive で wrap)。
        if f.view.ruler_h > 0.0 {
            hctx.with_clip_rect(ruler, |hctx| {
                hctx.ui_mut().time_ruler(
                    ("arr_ruler", heavy.id_hash),
                    ruler,
                    heavy.mapping,
                    heavy.sample_viewport,
                    heavy.ruler_style,
                );
            });
        }

        // M14 Phase 63n-1 (#028): automation lane 行群の描画 (track 行の下、 expand されたもののみ)。
        // 各 visible track の `automation_lanes_collapsed = false` のとき、 visible lane を上から
        // 順に積む (header = lane 左端 / body = lane 右端 = clip 描画域と同 x)。 lane の y 範囲は
        // `tops[i] + track_row_h` から `tops[i+1]` (= 次 track 上端) の間。
        // 描画は cached 内: viewport_key に lane 関連 hash が入る前提 (fold_arrangement_clip_hash
        // を後ほど lane も含むように拡張する)。 現状は clip hash で大方の変化を検出可能。
        //
        // M14 Phase 77 (daw_01 #048): below_ruler scope (header_pane + lanes を跨ぐ draw 用)。
        // automation lane の背景 fill は header_rect.x から body_rect 終端まで
        // span するので、 単独 lanes / header_pane scope では片側が切られる。
        hctx.with_clip_rect(below_ruler, |hctx| {
            for (i, t) in f.visible_tracks.iter().enumerate() {
                if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                    continue;
                }
                let track_row_top = f.tops[i];
                // viewport culling: track 領域全体 (track + lanes) が viewport 外なら skip
                let track_total_bottom = f.tops[i + 1];
                if track_total_bottom < lanes.y || track_row_top > lanes.y + lanes.h {
                    continue;
                }
                let mut lane_y = track_row_top + effective_track_row_h(t, f.view.track_row_h);
                // M14 Phase 63n-1 (#028) follow-up: lane 行 header は親 track と同じ indent に揃える
                // (= 親 track の depth * indent_px)。 group 配下の track の lane が「どの track の
                // lane か」 を視覚的に追えるようにするため (#028 user 指摘 1)。
                let header_indent = f32::from(t.depth) * f.style.indent_px;
                for lane in &t.automation_lanes {
                    if !lane.visible {
                        continue;
                    }
                    let lh = f32::from(lane.height_px);
                    // lane 行 viewport culling
                    if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                        lane_y += lh;
                        continue;
                    }
                    let header_rect = Rect {
                        x: header_pane.x + header_indent,
                        y: lane_y,
                        w: (header_pane.w - header_indent).max(2.0),
                        h: lh,
                    };
                    let body_rect = Rect { x: lanes.x, y: lane_y, w: lanes.w, h: lh };
                    draw_automation_lane(
                        hctx,
                        t.id,
                        lane,
                        header_rect,
                        body_rect,
                        f.view,
                        f.style,
                        lanes,
                        &heavy.selected_automation_clip_set,
                        heavy.bend_skip,
                    );
                    lane_y += lh;
                }
            }
        });
    });

    // === cached 外: clip content (波形 / MIDI) → selection / drag preview / playhead / loop band ===
    // S4b Phase C: clip の中身 (audio 波形 / MIDI ノート) を clip クロームの直上・selection
    // overlay の直下に、 name 帯と共有する `clip_content_inset_top` で描く (旧 app 側 rect +
    // hardcode inset 重ね描きを撤去、 inset SSoT 化)。 decode 完了で毎フレーム反映されるよう
    // cached の外で描く。
    hctx.with_clip_rect(lanes, |hctx| {
        let inset = clip_content_inset_top(f.style);
        let view_end = f.view.start_beat + f.view.len_beats;
        for (i, t) in f.visible_tracks.iter().enumerate() {
            let row_top = f.tops[i];
            let row_h = effective_track_row_h(t, f.view.track_row_h);
            if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                continue;
            }
            for c in &t.clips {
                let key = ClipKey { track_id: t.id, clip_id: c.id };
                let Some(content) = heavy.clip_content.get(&key) else {
                    continue;
                };
                let end = c.start_beat + c.len_beats;
                if end < f.view.start_beat || c.start_beat > view_end {
                    continue;
                }
                let r = clip_to_rect(row_top, row_h, c, f.view, lanes);
                if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                    continue;
                }
                let is_selected = heavy.selected_clip_set.contains(&key);
                // r.md #68: 中身の x 写像は content 原点 + ビューのズームだけ
                // (clip の表示幅は分母に入らない = 端 drag しても中身は動かない)。
                let map = content_map(c, f.view, lanes);
                match content {
                    ClipContentDraw::Audio { events } => {
                        draw_clip_waveform_inner(
                            hctx,
                            key,
                            r,
                            map,
                            events,
                            is_selected,
                            lanes,
                            inset,
                            // r.md #45: 波形色は実塗り色から auto-contrast。
                            clip_effective_fill(c, t.kind, f.style),
                            f.style,
                            "audio_clip_wf",
                        );
                    }
                    ClipContentDraw::Midi { notes } => {
                        // r.md #20: ノート色のコントラストは **実際に塗られる背景色** で計算する。
                        // 選択でも fill は clip 本来の色のまま (draw_selection_overlay はリングのみ)
                        // なので、 旧来の is_selected → clip_selected_fill(黄) 前提は誤り
                        // (既定色=暗青 clip を選択するとノートが暗背景に暗色で描かれ消えていた)。
                        let clip_bg = c.color.unwrap_or(f.style.clip_default_fill);
                        draw_clip_midi_inner(hctx, r, notes, map, clip_bg, f.style, lanes.x, inset);
                    }
                }
            }
        }
    });

    // M14 Phase 77 (daw_01 #048): track_top に依存する overlay 群を below_ruler scope で
    // wrap。 loop_band / playhead は static (ruler / spans ruler+lanes) なので scope 外。
    hctx.with_clip_rect(below_ruler, |hctx| {
        // M14 Phase 96 (daw_01 #068): 連動ハイライトは selection overlay の **前** に描画
        // (選択中 member は黄塗りが上書き優先、 非選択の同グループ member が hue 強調の主役)。
        draw_active_group_overlay(hctx, &f.visible_tracks, &f.tops, f.view, lanes, f.style);
        draw_selection_overlay(
            hctx,
            &f.visible_tracks,
            &f.tops,
            &heavy.selected_clip_set,
            f.view,
            lanes,
            f.style,
        );
        // 時間範囲の帯は**選択リングより後 = 手前**に描く (クリップを部分的に
        // 覆っているときに境目が見える、`docs/plan_range_selection.md` §5)。
        // ドラッグ中はプレビュー (release commit と同じ値) を、それ以外は確定した範囲を出す。
        if let Some(sel) = overlays.range_preview.as_ref().or(f.time_selection) {
            draw_time_range_overlay(hctx, &f.rows, sel, f.view, lanes, f.style);
        }
        // r.md #58: フェードの掴む正方形は hover 中の clip (+ フェードをドラッグ中の
        // clip) にだけ出す。 選択リングより後 = 手前に描き、 ドラッグ ghost より前に
        // 置く (掴む対象が最前面、 ドラッグ中のプレビューはさらにその上)。
        //
        // `with_clip_rect(lanes, ..)` の入れ子は必須。 `push_filled_rect` は
        // `clip_rect: None` で外側スコープの scissor に依存しており、 `below_ruler` は
        // トラックヘッダ帯を含むため、 これが無いと左へスクロールアウトした clip の
        // 正方形がヘッダの上に漏れる。
        hctx.with_clip_rect(lanes, |hctx| {
            draw_fade_handle_overlay(
                hctx,
                &f.visible_tracks,
                &f.tops,
                f.view,
                lanes,
                heavy.hovered_clip,
                overlays.audio.as_ref().map(|ad| ad.key),
                f.style,
            );
        });
        // r.md #70: **静的 overlay 群 → 全ドラッグ ghost → lasso** の順に揃える。
        // レンダラは call order = z-order なので、 automation 面の静的 overlay (選択済 point /
        // curve handle) を ghost より後に描いていると、 掴んでいる point が静止した dot / handle に
        // 隠れる。 clip / track 並べ替え / fade と同じ「base を全部描いてから ghost」 規律へ。
        //
        // ※ 2 ブロック目 (r.md #73 の `draw_bend_overlays`) は純粋な静的 overlay では**なく**
        //    区間 bend の live preview を内包する。 point drag と bend drag は排他ジェスチャなので
        //    前出ししても実害は無いが、 「静的だから前」 という理由では**ない**。
        // M14 Phase 63n-8 (#033): selected automation points overlay (cached 外、 selection 変化のみで
        // 全 lane 再キャッシュは走らない設計)。 base draw (cached 内) は selection 不問の通常 dot を
        // 描く、 ここで selected な点だけを白色 + 大 dot で上書き (= base dot を完全に覆って差し替え)。
        // 描画式は `draw_automation_lane` の point dot と同 SSoT (`body_origin_x + abs_beat * beat_to_px`、
        // `clip_y + (1 - value_norm) * clip_h`)、 collapsed track / invisible lane は skip。
        if !heavy.selected_automation_point_set.is_empty() {
            let beat_to_px = f64::from(lanes.w) / f.view.len_beats.max(1e-6);
            let r_sel = f.style.automation_point_radius_selected_px;
            // r.md #73: dot は automation clip の面 (= lane 色 / 選択の黄 / ライトテーマの
            // 明るいレーン) の上に乗る。 fill / border を **逆極性のペア**にして、
            // どちらの極性の背景でも必ず一方が読めるようにする (非選択 dot と同 idiom)。
            let (sel_dot_fill, sel_dot_border) = automation_point_selected_colors(p, f.style);
            let pad = f.style.automation_clip_v_pad_px;
            for (i, t) in f.visible_tracks.iter().enumerate() {
                if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                    continue;
                }
                let row_top = f.tops[i];
                let row_total_bottom = f.tops[i + 1];
                if row_total_bottom < lanes.y || row_top > lanes.y + lanes.h {
                    continue;
                }
                let mut lane_y = row_top + effective_track_row_h(t, f.view.track_row_h);
                for lane in &t.automation_lanes {
                    if !lane.visible {
                        continue;
                    }
                    let lh = f32::from(lane.height_px);
                    if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                        lane_y += lh;
                        continue;
                    }
                    let clip_y = lane_y + pad;
                    let clip_h = (lh - pad * 2.0).max(2.0);
                    for c in &lane.clips {
                        for (p_idx, pt) in c.points.iter().enumerate() {
                            #[allow(clippy::cast_possible_truncation)]
                            let key = AutomationPointKey {
                                clip: AutomationClipKey {
                                    track: t.id,
                                    lane: lane.id,
                                    clip: c.id,
                                },
                                point_idx: p_idx as u32,
                            };
                            if !heavy.selected_automation_point_set.contains(&key) {
                                continue;
                            }
                            let abs_beat = c.start_beat + pt.time_beat;
                            #[allow(clippy::cast_possible_truncation)]
                            let px =
                                lanes.x + ((abs_beat - f.view.start_beat) * beat_to_px) as f32;
                            let py = clip_y + (1.0 - pt.value_norm.clamp(0.0, 1.0)) * clip_h;
                            hctx.push_rect(RectCommand {
                                rect: Rect {
                                    x: px - r_sel,
                                    y: py - r_sel,
                                    w: r_sel * 2.0,
                                    h: r_sel * 2.0,
                                },
                                fill: sel_dot_fill,
                                border: sel_dot_border,
                                border_width: 1.5,
                                radius: [r_sel; 4],
                                clip_rect: Some(Rect {
                                    x: lanes.x,
                                    y: lane_y,
                                    w: lanes.w,
                                    h: lh,
                                }),
                            });
                        }
                    }
                    lane_y += lh;
                }
            }
        }
        // r.md #73: 中央ハンドルの描画は撤去した (ハンドルは原理的に動かない —
        // Bezier の t=0.5 は tension に依らず常に中点)。 代わりに
        //   (1) Alt hover 中の「曲げられる区間」の強調 (どこを掴むと何が起きるかの可視化)
        //   (2) bend drag 中の preview 曲線
        // を描く。 どちらも `cached` の外 — hover / preview の形は毎フレーム変わるので
        // cache キーに混ぜると全再構築になる。
        // ただし **「いまどの区間を曲げているか」 だけは cache キーに入っている**
        // (`bend_skip`)。 cached 側がその区間の base curve を描かないようにするためで、
        // 値が変わるのは drag の開始と終了の 2 回だけ (毎フレームではない)。
        draw_bend_overlays(
            hctx,
            f,
            heavy.hovered_segment,
            overlays.segment_bend.as_ref(),
            &heavy.selected_automation_clip_set,
        );
        if let Some((nd, bd, td)) = overlays.clip.as_ref() {
            draw_drag_preview(
                hctx,
                nd,
                &f.visible_tracks,
                &f.tops,
                f.view,
                lanes,
                f.style,
                f.visible_tracks.len(),
                *bd,
                *td,
                overlays.clip_min_len,
                &heavy.clip_content,
                &heavy.stretch_ghost_content,
            );
        }
        // M14 Phase 63k (#025): audio_drag ghost overlay (drag 中の dB / fade preview + label)。
        // commit-by-release のため clip_rect_anchor + 計算済 outcome から preview rect / line を
        // 描き直す。 cached 外なので 1 frame 1 描画 (drag 中のみ)、 release frame で session が
        // take されてから次 frame は ghost 消滅。 base 描画 (cached 内) も同 frame 表示されるが、
        // ghost が上に重なって最新値を user に見せる。
        if let Some(ad) = overlays.audio {
            draw_audio_drag_ghost(hctx, &ad, f.beat_per_px, f.style);
        }
        // M14 Phase 63n-2 (#028): automation_point_drag ghost (新位置の point dot を半透明で重ねる)。
        // anchor 固定の `body_rect_anchor` / `clip_rect_anchor` で beat_to_px / y 軸を計算 (drag
        // 中の view scroll 耐性)。 release commit と同じ式で next position を出すため SSoT を
        // 共有 (commit と overlay が同一値で確定)。 alt は session の `last_alt` を真値とする。
        if let Some(pd) = overlays.point {
            let dx = pd.last_mouse.0 - pd.anchor_mouse.0;
            let dy = pd.last_mouse.1 - pd.anchor_mouse.1;
            let beat_to_px = f64::from(pd.body_rect_anchor.w) / f.view.len_beats.max(1e-6);
            let raw_dt = f64::from(dx) / beat_to_px;
            let raw_abs = pd.clip_start_beat + pd.anchor_time_beat + raw_dt;
            let snapped_abs =
                f.view.snap.snap_beat(raw_abs, pd.last_alt, f.zoom_x_px_per_beat);
            let next_local =
                (snapped_abs - pd.clip_start_beat).clamp(0.0, pd.clip_len_beats.max(0.0));
            let next_value =
                (pd.anchor_value_norm - dy / pd.clip_rect_anchor.h.max(1.0)).clamp(0.0, 1.0);
            let abs_beat = pd.clip_start_beat + next_local;
            #[allow(clippy::cast_possible_truncation)]
            let px = pd.body_rect_anchor.x + ((abs_beat - f.view.start_beat) * beat_to_px) as f32;
            let py = pd.clip_rect_anchor.y + (1.0 - next_value) * pd.clip_rect_anchor.h;
            let r = f.style.automation_point_radius_px;
            hctx.push_rect(RectCommand {
                rect: Rect { x: px - r, y: py - r, w: r * 2.0, h: r * 2.0 },
                fill: f.style.clip_selected_fill,
                // 枠が乗るのは automation lane 面 (パレット自身のクローム面) と
                // `clip_selected_fill` なので、極性固定インクではなくテーマ従属の `text`
                // (ライトでは暗くなり、明るい lane / 黄 dot の双方で縁が立つ)。
                border: p.text,
                border_width: 1.5,
                radius: [r; 4],
                clip_rect: Some(pd.body_rect_anchor),
            });
        }
        // M14 Phase 63n-3 (#028): automation_clip_drag ghost (drag 中の preview rect、 cross-lane
        // drop 解決込み)。 fill / border / badge は MIDI clip drag preview と完全対称。
        if let Some(acd) = overlays.automation_clip.as_ref() {
            let is_move_clone = matches!(acd.kind, ClipDragKind::Move) && acd.last_ctrl;
            let (fill, border, badge_glyph) = if is_move_clone {
                if acd.last_shift {
                    (
                        f.style.clip_clone_indep_fill,
                        f.style.clip_clone_indep_border,
                        Some('+'),
                    )
                } else {
                    (
                        f.style.clip_clone_linked_fill,
                        f.style.clip_clone_linked_border,
                        Some('⇌'),
                    )
                }
            } else {
                (f.style.clip_selected_fill, f.style.clip_selected_border, None)
            };
            // beat_to_px は現在フレームの lanes.w から算出 (全 lane body は幅 lanes.w で同一、
            // for_each_visible_lane 参照)。 press 時の anchor 幅でなく現幅を使うことで drag 中の
            // window / header resize に追従する。
            let beat_to_px = f64::from(lanes.w) / f.view.len_beats.max(1e-6);
            let raw_beat_delta = if beat_to_px > 1e-9 {
                f64::from(acd.last_mouse.0 - acd.anchor_mouse.0) / beat_to_px
            } else {
                0.0
            };
            // snap pivot = anchors[0] (= 掴んだ clip)、 release commit と同 SSoT。
            let beat_delta = compute_automation_clip_drag_beat_delta(
                acd,
                raw_beat_delta,
                &f.view.snap,
                f.zoom_x_px_per_beat,
            );
            let min_len = if f.view.snap.is_active(acd.last_alt) {
                f.view.snap.beat_unit(f.zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };
            // #071: 単一選択は cursor で cross-lane drop を preview、 複数選択は各 anchor の自 lane に
            // 留め horizontal time-shift を preview (release commit の cross-lane policy と一致)。
            let single = acd.anchors.len() == 1;
            let pad = f.style.automation_clip_v_pad_px;
            for a in &acd.anchors {
                let (g_start, g_len) = match acd.kind {
                    ClipDragKind::Move => ((a.start_beat + beat_delta).max(0.0), a.len_beats),
                    ClipDragKind::ResizeRight => {
                        (a.start_beat, (a.len_beats + beat_delta).max(min_len))
                    }
                    ClipDragKind::ResizeLeft => {
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual = new_start - a.start_beat;
                        (new_start, (a.len_beats - actual).max(min_len))
                    }
                };
                let target_body = if single && matches!(acd.kind, ClipDragKind::Move) {
                    automation_lane_key_at_y(
                        &f.visible_tracks,
                        &f.tops,
                        f.view.track_row_h,
                        header_pane.x,
                        header_pane.w,
                        lanes.x,
                        lanes.w,
                        f.style,
                        acd.last_mouse.1,
                    )
                    .map_or(a.body_rect, |(_, body)| body)
                } else {
                    a.body_rect
                };
                let g_clip_y = target_body.y + pad;
                let g_clip_h = (target_body.h - pad * 2.0).max(2.0);
                #[allow(clippy::cast_possible_truncation)]
                let g_x = target_body.x + ((g_start - f.view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let g_w = ((g_len * beat_to_px) as f32).max(2.0);
                let ghost_rect = Rect { x: g_x, y: g_clip_y, w: g_w, h: g_clip_h };
                if ghost_rect.x + ghost_rect.w >= lanes.x && ghost_rect.x <= lanes.x + lanes.w {
                    hctx.push_rect(RectCommand {
                        rect: ghost_rect,
                        fill,
                        border,
                        border_width: f.style.clip_selected_border_w,
                        radius: [f.style.clip_radius; 4],
                        clip_rect: Some(lanes),
                    });
                    if let Some(g) = badge_glyph
                        && ghost_rect.w > f.style.clip_clone_badge_size + 4.0
                        && ghost_rect.h > f.style.clip_clone_badge_size + 2.0
                    {
                        // r.md #73: 下地は ghost の塗り (可変) なので極性を実効背景から決める。
                        let badge_ink = clip_ink_for(hctx.palette(), fill, f.style.bg);
                        hctx.push_text(GlyphArea {
                            text: Arc::from(g.to_string()),
                            left: ghost_rect.x + 4.0,
                            top: ghost_rect.y + 2.0,
                            font_size: f.style.clip_clone_badge_size,
                            line_height: f.style.clip_clone_badge_size * 1.2,
                            color: badge_ink,
                            clip_rect: Some(ghost_rect),
                            ..GlyphArea::default()
                        });
                    }
                }
            }
        }
        // M14 Phase 63n-8 (#033): lasso 矩形 overlay (drag 中のみ、 cached 外で半透明 cyan 系を描画)。
        // anchor から last_mouse の bounding rect を style.automation_lasso_fill / border で 1 度描画。
        // press と release が同 frame で起きる超短 click の場合、 session は release frame で take 済
        // = `overlays.lasso = None` で overlay 不描画 (= 即時消失、 user 視点で「click だけ」 と認識される)。
        if let Some(ls) = overlays.lasso {
            let rect = Rect {
                x: ls.anchor.0.min(ls.last_mouse.0),
                y: ls.anchor.1.min(ls.last_mouse.1),
                w: (ls.anchor.0 - ls.last_mouse.0).abs(),
                h: (ls.anchor.1 - ls.last_mouse.1).abs(),
            };
            hctx.push_rect(RectCommand {
                rect,
                fill: f.style.automation_lasso_fill,
                border: f.style.automation_lasso_border,
                border_width: 1.0,
                radius: [0.0; 4],
                clip_rect: Some(lanes),
            });
        }
    }); // end with_clip_rect(below_ruler)  for selection / drag / lasso overlays
    // loop band: drag preview がある場合は preview を描く、無ければ view.loop_range
    // M14 Phase 77: loop_band は ruler 領域、 track_top 不変なので scope 外で defensive wrap。
    if let Some(range) = overlays.loop_preview.or(f.view.loop_range) {
        draw_loop_band(
            hctx,
            range,
            f.view.start_beat,
            f.view.len_beats,
            ruler,
            f.style.loop_band,
            f.style.loop_handle,
            f.style.loop_handle_w,
        );
    }
    // M14 Phase 127 (daw_01 #105): Arranger レーン (背景 + section 帯 + drag preview)。 loop band と
    // 同じく cached 外・track scroll 非依存なので below_ruler scope の外で描画 (ruler と lanes の間)。
    if f.arranger_lane_h > 0.0 {
        draw_sections_lane(
            hctx,
            f.sections,
            overlays.section,
            f.view,
            f.arranger_rect,
            f.arranger_header_rect,
            &f.view.snap,
            f.zoom_x_px_per_beat,
            f.style,
        );
    }
    if let Some(b) = f.view.playhead_beat
        && b >= f.view.start_beat
        && b <= f.view.start_beat + f.view.len_beats
    {
        let beat_to_px = f64::from(lanes.w) / f.view.len_beats.max(1e-6);
        #[allow(clippy::cast_possible_truncation)]
        let x = lanes.x + ((b - f.view.start_beat) * beat_to_px) as f32;
        draw_playhead_line(
            hctx,
            x,
            ruler.y,
            lanes.y + lanes.h,
            f.style.playhead_color,
            f.style.playhead_width_px,
        );
    }

    // === M10 Phase 46 → 101 (daw_01 #072): track reorder drop indicator + preview ===
    // M14 Phase 77 (daw_01 #048): reorder overlay は header_pane + lanes を跨ぐ (横 1 行帯)
    // ので below_ruler scope で wrap (= ruler / toolbar への leak 防止)。
    //
    // M14 Phase 101 (daw_01 #072): 深さを可視化する。 (1) 着地先 group があれば header 行を
    // hilight (Cubase の緑矢印に相当)、 (2) indicator 横線の **左端を解決済み深さの indent 列に
    // 合わせる** (flush-left = top-level / 1 段右 = group の子)。 これらは `resolve_track_drop` の
    // 結果から事前計算済 (= commit と同じ着地位置を描く)。
    if let Some(ov) = overlays.reorder.as_ref() {
        hctx.with_clip_rect(below_ruler, |hctx| {
            // (1) group-header hilight (nest 先の肯定フィードバック)。
            if let Some(hl) = ov.highlight_row {
                push_filled_rect(hctx, hl, f.style.reorder_group_highlight);
            }
            // (2) 深さ連動 drop indicator 横線。 左端 = indent 列、 右端 = header + lanes。
            let line_right = f.content_below_ruler.x + f.content_below_ruler.w;
            let line_x = ov.indent_x.min(line_right - 1.0);
            push_filled_rect(
                hctx,
                Rect {
                    x: line_x,
                    y: ov.indicator_y - f.style.reorder_drop_indicator_h * 0.5,
                    w: (line_right - line_x).max(1.0),
                    h: f.style.reorder_drop_indicator_h,
                },
                f.style.reorder_drop_indicator,
            );
            // (3) dragging row 半透明複製 (header_pane 領域、last_mouse_y 中心)。
            let row_h = f.view.track_row_h;
            let drag_y = (ov.drag_center_y - row_h * 0.5)
                .clamp(header_pane.y, header_pane.y + header_pane.h - row_h);
            let alpha = f.style.reorder_drag_alpha.clamp(0.0, 1.0);
            let base_rgb = f.style.track_selected_bg;
            push_filled_rect(
                hctx,
                Rect { x: header_pane.x, y: drag_y, w: header_pane.w, h: row_h },
                Color::rgba(base_rgb.r, base_rgb.g, base_rgb.b, alpha),
            );
        });
    }
}

/// r.md #73: 曲線編集の overlay 2 種を `cached` の外に描く。
///
/// - **(1) hover 強調** … Alt 押下中にポインタが乗っている区間を
///   `automation_curve_bend_hover_color` でなぞる。 入力は `cursor::hover` が確定させた
///   `AutomationPointIdKey` 1 つだけで、 **hover のたびに `automation_segment_at` を
///   再実行しない** (cursor 層で 1 度出した結果を key で運び、 ここでは幾何を引き直す)。
/// - **(2) bend preview** … drag 中の `preview_curve` を `line_width × 1.5` の
///   `automation_curve_bend_preview_color` で描く。 **その区間の base curve は cached 側が
///   描かない** (`HeavyInput::bend_skip`) ので、 これがその区間の唯一の線になる。
///   旧実装は base に「重ねて覆う」 つもりだったが、 曲げれば形が食い違うのだから
///   原理的に覆いきれず、 **2 重線**として見えていた (r.md #73 の不具合報告)。
///
/// 形の評価は cached 側と同じ `curve::flatten_segment` を通す
/// (= 「プレビューだけ別式」を作らない。 それが #73 の不具合の原因だった)。
fn draw_bend_overlays(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    hovered: Option<AutomationPointIdKey>,
    bend: Option<&AutomationSegmentBendSession>,
    selected_clips: &HashSet<AutomationClipKey>,
) {
    // 強調 / preview の色は「その区間が乗っている面の実効背景」から導くので、
    // 幾何を引く段階でパレットが要る (`hctx.palette()` は host 寿命の借用なので
    // 以降 `hctx` を可変で使っても問題ない)。
    let p = hctx.palette();
    // (1) hover 強調。 drag 中は preview が同じ区間を上塗りするので出さない。
    if let Some(key) = hovered
        && bend.is_none()
        && let Some(seg) = resolve_bend_segment(p, f, key, selected_clips)
    {
        draw_segment_polyline(
            hctx,
            seg,
            seg.curve,
            f.style.automation_curve_bend_hover_color,
            f.style.automation_curve_line_width_px * 1.5,
        );
    }
    // (2) drag 中の preview。 `preview_curve` は毎フレーム逆算済 (drag.rs)。
    if let Some(bd) = bend
        && let Some(mut seg) = resolve_bend_segment(p, f, bd.point, selected_clips)
    {
        // 値と clip 描画域は press 時の anchor を使う (drag 中 model は不変なので同値だが、
        // 縦スクロール / lane 高さ変更が同時に起きても preview が飛ばない)。
        seg.clip_rect = bd.clip_rect_anchor;
        seg.a_plain = bd.a_plain;
        seg.b_plain = bd.b_plain;
        draw_segment_polyline(
            hctx,
            seg,
            bd.preview_curve,
            f.style.automation_curve_bend_preview_color,
            f.style.automation_curve_line_width_px * 1.5,
        );
    }
}

/// r.md #73: 区間 key → 描画に必要な幾何 (lane 参照 + clip 描画域 + 端点)。
/// `automation_segment_at` と **同じ式**で x / clip_rect を出す (point dot と curve の x が
/// ずれる既知バグ #028 user 指摘 2 の再発防止)。
fn resolve_bend_segment<'a>(
    p: &daw_ui_core::theme::Palette,
    f: &'a ArrangementFrame<'a>,
    key: AutomationPointIdKey,
    selected_clips: &HashSet<AutomationClipKey>,
) -> Option<ResolvedSegment<'a>> {
    let (lane, clip) = find_lane_clip(&f.visible_tracks, key.clip)?;
    let (p_prev, p_next) = curve::find_automation_segment_by_id(&f.visible_tracks, key)?;
    let body_rect = automation_lane_body_rect(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        key.clip.lane_key(),
    )?;
    let pad = f.style.automation_clip_v_pad_px;
    let clip_rect = Rect {
        x: body_rect.x,
        y: body_rect.y + pad,
        w: body_rect.w,
        h: (body_rect.h - pad * 2.0).max(2.0),
    };
    let beat_to_px = f64::from(body_rect.w) / f.view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let x_prev = body_rect.x
        + ((clip.start_beat + p_prev.time_beat - f.view.start_beat) * beat_to_px) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let x_next = body_rect.x
        + ((clip.start_beat + p_next.time_beat - f.view.start_beat) * beat_to_px) as f32;
    // r.md #73: 強調 / preview の色は **その clip が実際に塗られている色**から導く。
    // 塗りの決め方 (selected > disabled > lane 識別色) と、それを lane 面に合成する式は
    // cached 側の curve と同じ `draw.rs` のヘルパを通す (2 か所で書くと極性がずれる)。
    let is_selected = selected_clips.contains(&key.clip);
    let (fill, _) = automation_clip_colors(p, lane, is_selected, f.style);
    let eff_bg = automation_clip_eff_bg(fill, f.style);
    Some(ResolvedSegment {
        lane,
        // scissor は cached 側の base curve と同じ **automation clip の矩形**
        // (`draw_automation_lane` と同じ `automation_clip_rect`)。 lane body 全体で切ると
        // 「base は clip で切られるのに強調だけクリップ外へはみ出す」 食い違いが出る。
        scissor: automation_clip_rect(body_rect, f.view, clip.start_beat, clip.len_beats, f.style),
        clip_rect,
        eff_bg,
        x_prev,
        x_next,
        a_plain: p_prev.value_plain,
        b_plain: p_next.value_plain,
        curve: p_next.curve,
    })
}

/// `resolve_bend_segment` の戻り値。
#[derive(Clone, Copy)]
struct ResolvedSegment<'a> {
    lane: &'a ArrangementAutomationLane,
    /// overlay の scissor (= base curve と同じ automation clip の矩形)。
    scissor: Rect,
    /// 値 ↔ y の anchor (lane body に縦 padding を適用したもの)。
    clip_rect: Rect,
    /// この区間が乗っている面の **実効背景**。強調 / preview の色をここから導く。
    eff_bg: Color,
    x_prev: f32,
    x_next: f32,
    a_plain: f64,
    b_plain: f64,
    curve: common::model::AutomationCurve,
}

/// 1 区間を `curve::flatten_segment` で polyline 化して押す。
///
/// r.md #73: なぞる相手は **可変背景** である。レーンの塗りは lane 識別色 / テーマ /
/// **選択状態** で変わり、選択中の automation clip の塗りは
/// `clip_selected_fill = p.selection_warm` — 強調 / preview のトークン
/// `automation_curve_bend_*_color` と **同じ値**である。
///
/// 解き方は **芯の色を面から導く** の 1 本だけ (`automation_accent_on` =
/// `Palette::adapt_on`)。色相・彩度を保ったまま図形コントラスト 3:1 を満たす明度まで
/// 寄せるので、`accent` と面の色が同一トークンでも芯は必ず読める。足りていれば恒等なので、
/// ダークの非選択レーンでは従来と 1 bit も変わらない。
///
/// **縁取りは持たない。** 旧実装は固定色の芯が塗りに沈むのを逆極性の縁で凌ごうとしたが、
/// それは「消える」を「芯が抜けて縁が 2 本残る」= **二重線**に変換しただけだった
/// (r.md #73「曲げている最中に線が 2 重に見える」の実体。実ピクセルで確認)。
/// 芯が必ず読めるようになった今、縁は可視性に寄与しないうえ **害がある** — hover 強調は
/// base curve を上から覆う作りなので、面と同系色になった縁が base を消して縁の外側に
/// 断片を残す (実測: 選択中クリップで芯の 1px 下に背景が覗く)。
/// (memory `feedback_ui_indicator_contrast_on_variable_bg`)
fn draw_segment_polyline(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    seg: ResolvedSegment<'_>,
    curve: common::model::AutomationCurve,
    accent: Color,
    line_width_px: f32,
) {
    let map = curve::LaneValueMap::from_lane(seg.lane, seg.clip_rect);
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(64);
    pts.push((seg.x_prev, map.plain_to_y(seg.a_plain)));
    curve::flatten_segment(
        map,
        (seg.x_prev, seg.a_plain),
        (seg.x_next, seg.b_plain),
        curve,
        2.0,
        &mut pts,
    );
    if pts.len() < 2 {
        return;
    }
    // 芯は「その面の上で必ず読める暖色」。面と同一トークンでも沈まない。
    let color = automation_accent_on(hctx.palette(), seg.eff_bg, accent);
    let segments: Vec<daw_ui_renderer::LineSegment> = pts
        .windows(2)
        .map(|p| daw_ui_renderer::LineSegment {
            a: [p[0].0, p[0].1],
            b: [p[1].0, p[1].1],
            color,
        })
        .collect();
    hctx.push_lines(daw_ui_renderer::LineBatch {
        segments: segments.into(),
        line_width_px,
        clip_rect: Some(seg.scissor),
    });
}
