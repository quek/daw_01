//! track header 列の immediate-mode 描画と、 そこで検出した click の確定発行。
//!
//! **描画は heavy と `commit_releases` の後**に走る (= `Scene` の最前面)。

use super::*;

use crate::view::disclosure::{RevealAxis, disclosure_glyph};

/// この 1 フレームで検出した header の click (loop 内で `push_edit` すると複数発行に
/// なるため、 loop 後に 1 度だけ発行する — 旧 `clicked_track_for_select` /
/// `disclosure_clicked`)。
#[derive(Default)]
pub(super) struct HeaderClicks {
    pub clicked_track: Option<u32>,
    pub disclosure: Option<u32>,
}

/// header 行の描画 + click 検出。 `response.track_header_rects` を積む。
///
/// M10 Phase 50: `f.visible_tracks` を使う (release frame の optimistic preview と同順序)。
/// M14 Phase 63c (#016): collapsed 親配下は `frame::build` の時点で除外済なので、
/// ここは `enumerate()` で回すだけでよい (旧 `visible_idx_for_headers =
/// compute_visible_indices(&tracks_for_draw)` は filter 済リストに対する **恒等**だった)。
/// 各 track header に depth * indent_px の左 indent + group track には disclosure ▼/▶ アイコン。
/// selection は `f.selected_tracks` で判定。 修飾 (Shift / Ctrl) で Single /
/// RangeFromAnchor / Toggle を decode して渡す。 1 frame 内で最初に click された track id を
/// `clicked_track` に蓄え、 loop 後に `apply_select_tracks` を 1 度呼ぶ。
pub(super) fn draw_rows(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &mut ArrangementResponse,
) -> HeaderClicks {
    let mut clicks = HeaderClicks::default();
    if f.header_w <= 0.0 {
        // `header_w == 0` で header 行を 1 本も描かないのが現行挙動。
        // (`with_clip_rect` の push/pop も走らないが、 中身が空なので観測差は無い。)
        return clicks;
    }
    // M14 Phase 77 (daw_01 #048): header row 描画 push_* 群を `header_pane` で auto-scissor する。
    // 旧実装は「closure 化すると `ui.xxx` の大量 rename を要する」 という理由で
    // `current_clip` の push/pop を open-code していたが、 header ループが独立 fn になった時点で
    // その理由は消えた。 `Ui::with_clip_rect` の中身は open-code と完全に同一。
    ui.with_clip_rect(f.header_pane, |ui| {
        draw_rows_inner(ui, f, live, response, &mut clicks);
    });
    clicks
}

#[allow(clippy::too_many_lines)]
fn draw_rows_inner(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &mut ArrangementResponse,
    clicks: &mut HeaderClicks,
) {
    let style = f.style;
    let view = f.view;
    let header_pane = f.header_pane;
    let pointer = f.pointer;
    for (visible_i, t) in f.visible_tracks.iter().enumerate() {
        // M14 Phase 63n-1 (#028): row top は `frame::build` の prefix sum tops をそのまま使う
        // (`header_pane.y == lanes.y` は rect 分割の y 原点共通から自明)。
        let row_y = f.tops[visible_i];
        let row = Rect { x: header_pane.x, y: row_y, w: header_pane.w, h: view.track_row_h };
        if row.y + row.h < header_pane.y || row.y > header_pane.y + header_pane.h {
            continue;
        }

        // M14 Phase 63n-10 (#034): master row 専用 header 描画。 mute/solo button / volume band /
        // group disclosure / row click → トラック選択の全 path を skip し、 neutral gray 背景 +
        // "Master" label + lane disclosure (`+`/`-`) のみを描画する (daw_01 #034 §B 仕様)。
        // 通常 track 経路の `selected_tracks` / `is_group_set` 判定とは独立 (master は selection
        // 対象外、 group でもない、 = 「特殊な行」 として描画分岐)。
        if t.id == MASTER_TRACK_ID {
            // M14 Phase 90 (daw_01 #061): master 行も選択可能。 selected なら通常 track と同じ
            // `track_selected_bg`、 非選択は従来の `master_row_color`。 "Master" label / lane
            // disclosure はこの背景の上に重畳描画 (色は据え置き)。
            let master_bg = if f.selected_tracks.contains(&t.id) {
                style.track_selected_bg
            } else {
                style.master_row_color
            };
            ui.panel(("arr_master_thbg", 0_u32), row, master_bg, 0.0);
            let indent = f32::from(t.depth) * style.indent_px; // 0 固定だが既存 idiom 維持
            let row_for_layout =
                Rect { x: row.x + indent, y: row.y, w: (row.w - indent).max(2.0), h: row.h };
            let layout = header_row_layout(row_for_layout, 0.0); // volume band 無し
            // "Master" label を name_rect に push_text (button にはしない = click は selection 経路に
            // 流さない)。 font_size は style.master_row_label_size、 色は master_row_label_color。
            let label_rect = layout.name_rect;
            ui.push_text(GlyphArea {
                text: Arc::from("Master"),
                left: label_rect.x + 4.0,
                top: label_rect.y + (label_rect.h - style.master_row_label_size * 1.2) * 0.5,
                font_size: style.master_row_label_size,
                line_height: style.master_row_label_size * 1.2,
                color: style.master_row_label_color,
                clip_rect: Some(label_rect),
                ..GlyphArea::default()
            });
            // M14 Phase 63n-10 (#034): lane disclosure (`+` / `-`) を master row でも描画 (= 通常
            // track と同 idiom)。 click 検出は press block 経由で `actions.lane_toggle = Some(t.id)`
            // (= `MASTER_TRACK_ID`) が立ち、 `ToggleTrackAutomationCollapsed { track:
            // MASTER_TRACK_ID }` が発火する SSoT。
            if !t.automation_lanes.is_empty() {
                let lane_disc = layout.lane_disc_rect;
                let label = if t.automation_lanes_collapsed { "+" } else { "-" };
                ui.push_text(GlyphArea {
                    text: label.into(),
                    left: lane_disc.x,
                    top: lane_disc.y
                        + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                    font_size: style.automation_disclosure_size,
                    line_height: style.automation_disclosure_size * 1.2,
                    color: style.disclosure_color,
                    clip_rect: Some(lane_disc),
                    ..GlyphArea::default()
                });
            }
            // Response.track_header_rects に積む (caller が master row の rect 領域を識別可能に)。
            response.track_header_rects.push((t.id, row));
            // M14 Phase 90 (daw_01 #061): master 行の header click もトラック選択。 通常 track と
            // 同じ `clicked_track` 経路を再利用し、 loop 後の modifier-aware 発行に乗せる
            // (Single なら next=[MASTER_TRACK_ID])。 lane disclosure (`+`/`-`) rect 内 release は
            // automation collapse トグルが priority なので除外する (disclosure > row-select)。
            // master には mute/solo/volume band が無いので row 全体 (disclosure 除く) が対象。
            if pointer.primary_just_released
                && let Some((rx, ry)) = pointer.pos
                && row.contains(rx, ry)
                && (t.automation_lanes.is_empty() || !layout.lane_disc_rect.contains(rx, ry))
                && !ui.has_open_popups()
            {
                clicks.clicked_track = Some(t.id);
            }
            continue;
        }

        // 背景 (selection > 通常)。 M14 Phase 113 (daw_01 #085): group track 専用の
        // 背景 tint は撤去 (group は indent / disclosure ▶▼ で識別、 背景は他 track と同じ
        // neutral header_bg)。 `is_group_set` は依然 disclosure 描画 / hit-test で使う。
        if f.selected_tracks.contains(&t.id) {
            ui.panel(("arr_thsel", t.id), row, style.track_selected_bg, 0.0);
        } else {
            ui.panel(("arr_thbg", t.id), row, style.header_bg, 0.0);
        }

        // M14 Phase 63c (#016): depth * indent_px の左 indent。 layout 計算は indent 反映後の
        // row_inner で実行する (= row.x + indent、 row.w - indent)。 #069 で color strip も
        // この indent に追従させるため、 strip 描画の前に indent を確定する。
        let indent = f32::from(t.depth) * style.indent_px;

        // M14 Phase 87 (daw_01 #059): track color strip。 Some(c) のとき header の (indent 後の)
        // 左端に縦ストライプを背景の上から描く (selected/group/video 背景と色衝突しない)。
        // None は strip 非描画 = 既存挙動完全互換。
        // M14 Phase 97 (daw_01 #069): x を row.x + indent にして名前と同じだけ右にインデント
        // (子トラックで色ストライプが名前と一緒にネスト)。 depth==0 は indent=0 で従来と pixel 一致。
        if let Some(c) = t.color
            && style.track_color_strip_w > 0.0
        {
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: row.x + indent,
                    y: row.y,
                    w: style.track_color_strip_w,
                    h: row.h,
                },
                fill: c,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(row),
            });
        }

        let row_for_layout =
            Rect { x: row.x + indent, y: row.y, w: (row.w - indent).max(2.0), h: row.h };
        let band_h = if matches!(t.kind, TrackKind::Video) {
            // M14 Phase 72 (#044): video track は volume band を非描画。
            0.0
        } else {
            style.track_volume_band_h
        };
        let layout = header_row_layout(row_for_layout, band_h);
        let name_rect = layout.name_rect;
        let [m_rect, s_rect, r_rect] = layout.buttons;

        // M10 Phase 47b: track volume band 描画。
        // drag 中の track はその drag session の last_mouse_x で preview volume を計算 (リアルタイム feedback)。
        if let Some(band) = layout.volume_band {
            let dragging_this = live.track_volume.as_ref().filter(|tv| tv.track_id == t.id);
            let display_v = if let Some(tv) = dragging_this {
                volume_from_mouse_x(tv.last_mouse_x, tv.band_rect.x, tv.band_rect.w)
            } else {
                // stored amp → frac (band と同じ MeterScale 空間)。 +6dB 側も
                // 正しい fill 位置で描く (r.md #11)。
                MeterScale::default().amp_to_frac(t.volume)
            };
            ui.panel(("arr_tvol_track", t.id), band, style.track_volume_band_track, 0.0);
            let fill_w = band.w * display_v;
            if fill_w > 0.0 {
                ui.panel(
                    ("arr_tvol_fill", t.id),
                    Rect { x: band.x, y: band.y, w: fill_w, h: band.h },
                    style.track_volume_band_fill,
                    0.0,
                );
            }
        }

        // M14 Phase 63c (#016): group disclosure — group track のみ描画 + click で
        // `AppEvent::ToggleGroupCollapsed` を発行 (loop 後に発火、 トラック選択より
        // priority 高)。 arrangement は track が **縦** に並び group の子は下に
        // 現れるので開示軸は Block (r.md #74: 折り畳み中 ▶ / 展開中 ▼)。
        let is_group = f.is_group_set.contains(&t.id);
        let disclosure_rect = disclosure_rect_for(name_rect, style, t.depth);
        if is_group {
            let label = disclosure_glyph(t.collapsed, RevealAxis::Block);
            ui.push_text(GlyphArea {
                text: label.into(),
                left: disclosure_rect.x + disclosure_rect.w * 0.2,
                top: disclosure_rect.y + (disclosure_rect.h - style.track_text_size * 1.2) * 0.5,
                font_size: style.track_text_size,
                line_height: style.track_text_size * 1.2,
                color: style.disclosure_color,
                clip_rect: Some(disclosure_rect),
                ..GlyphArea::default()
            });
            if pointer.primary_just_released
                && let Some((rx, ry)) = pointer.pos
                && disclosure_rect.contains(rx, ry)
            {
                clicks.disclosure = Some(t.id);
            }
        }
        // M14 Phase 63n-1 (#028) + 63n-2 修正: track 行 header の lane disclosure (S button の
        // 右、 `layout.lane_disc_rect`) を **`+` / `-`** で描画。 旧 `▽`/`▷` (U+25BD/U+25B7) は
        // font 不在で不可視 click target になる、 旧 `▼`/`▶` は group disclosure と同 glyph で
        // user が混同する両方の問題を解消した最終形 (#028 follow-up user feedback で確定)。
        // `automation_lanes.is_empty()` の track は描画しない (= layout 上は rect 確保するが
        // visual には何も出ない、 click もメッセージにならない)。 click 検出は press block で
        // 同じ `layout.lane_disc_rect` を使うので描画と hit-test の SSoT が完全一致。
        if !t.automation_lanes.is_empty() {
            let lane_disc = layout.lane_disc_rect;
            let label = if t.automation_lanes_collapsed { "+" } else { "-" };
            ui.push_text(GlyphArea {
                text: label.into(),
                left: lane_disc.x,
                top: lane_disc.y + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                font_size: style.automation_disclosure_size,
                line_height: style.automation_disclosure_size * 1.2,
                color: style.disclosure_color,
                clip_rect: Some(lane_disc),
                ..GlyphArea::default()
            });
        }
        // disclosure を除いた name 領域 (group の場合は disclosure 分削る)
        let name_rect_visible = if is_group {
            Rect {
                x: disclosure_rect.x + disclosure_rect.w,
                y: name_rect.y,
                w: (name_rect.w - disclosure_rect.w).max(2.0),
                h: name_rect.h,
            }
        } else {
            name_rect
        };
        let button_zones: [Rect; 4] = [name_rect_visible, m_rect, s_rect, r_rect];

        let id_name = ("arr_tname", t.id);
        let id_mute = ("arr_tmute", t.id);
        let id_solo = ("arr_tsolo", t.id);
        let id_armed = ("arr_tarmed", t.id);

        let track_id = t.id;
        let muted = t.muted;
        let solo = t.solo;
        let armed = t.armed;

        let name_text = t.name.clone();
        // M14 Phase 63c (#016): name 領域 click は modifier-aware なトラック選択を loop 後に
        // 発行する形に変更。 button_at_clicked で click 検知のみ行い、 内部で Edit は emit
        // しない (旧設計は button_at の closure 内で選択更新を emit していた)。
        // M14 Phase 105 (#076): track 名 font は `style.track_text_size` に追従させる
        // (汎用 button の 16px 固定では daw_01 が名前サイズを下げられないため)。
        // M14 Phase 107 (#079): track 名は常に左寄せ (Reaper / Cubase / Live と同じ。
        // 先頭が識別に最重要、 省略時の左寄せとも一致)。 M/S/R toggle は中央寄せのまま。
        if ui.button_at_clicked_sized_aligned(
            id_name,
            &name_text,
            name_rect_visible,
            style.track_text_size,
            daw_ui_core::widgets::button::ButtonTextAlign::Left,
        ) && !ui.has_open_popups()
        {
            // 名前ボタン経路も popup ガード対象 (release catch-all / master 行と同件)。
            // daw-ui の button 自体も popup mask を見るようにしたが、 ここは
            // 「どの行が click されたか」 を蓄える daw_gui 側の状態遷移なので明示する。
            clicks.clicked_track = Some(t.id);
        }
        // M14 Phase 118 (daw_01 #092): group track 名 double-click rename の信頼性。 深くネストした
        // group track は indent で name_rect が 20px floor まで潰れ、 さらに disclosure 分を引くと
        // `name_rect_visible` が 2〜4px になり double-click が当たらなかった。 group track のみ
        // **header row 全体** を rename hit zone にし、 single-click で別意味を持つ sub-zone
        // (M·S·R / lane disclosure / volume band drag / header splitter) を除外する。 これで
        // indent 空白 + 名前帯のどこを double-click しても rename が始まる (REAPER の TCP 名 dblclick
        // 流)。 通常 track は `name_rect_visible` のまま (名前帯が潰れないので挙動完全不変、 sub-zone
        // 除外も常に no-op)。
        //
        // M14 Phase 119 (daw_01 #092 follow-up): **group disclosure (`▶`/`▼`) も rename zone に含める**。
        // depth-0 (top-level) group は disclosure が name 帯の左端 (= indent 空白が無く x∈[pad, pad+
        // indent_px]) に張り付くため、 旧実装は disclosure を sub-zone 除外していた結果「最上段 / 子持ち
        // group の名前左側を double-click しても rename されない」 症状になっていた (master row の有無は
        // 無関係 = 最上段が top-level group になりがちなだけの相関、 と pixel/hit-test 検証で確定)。 disclosure
        // の **single-click** 折り畳みは別経路 (`clicks.disclosure`) で従来どおり (回帰なし)。 **double-click**
        // は明確に rename 意図なので disclosure 上でも rename を起こす。 double-click が disclosure を踏むと
        // 2 release で折り畳みが 2 回 toggle するが、 `AppEvent::ToggleGroupCollapsed` は daw_01 の
        // `collapsed_groups` (HashSet) を反転するだけの非 undoable な view-state edit なので net-zero
        // (= fold 状態保存、 undo 履歴も汚さない、 r.md #74)。 M·S·R /
        // lane disclosure は name 帯の **右**で名前と無関係なので除外を維持 (button の double-toggle を rename に
        // 化けさせない)、 volume band も名前帯の下の独立 drag 控除なので維持。
        let rename_hit = if is_group { row } else { name_rect_visible };
        if let Some((dcx, dcy)) = ui.take_double_click_in_rect(rename_hit) {
            let in_subzone = m_rect.contains(dcx, dcy)
                || s_rect.contains(dcx, dcy)
                || r_rect.contains(dcx, dcy)
                || (!t.automation_lanes.is_empty() && layout.lane_disc_rect.contains(dcx, dcy))
                || layout.volume_band.is_some_and(|b| b.contains(dcx, dcy))
                // M14 Phase 118 follow-up: group の broad zone は header / lanes 境界まで届くので、
                // header 幅 splitter (#091) の hot zone も除外して rename と resize を分離する。
                || header_resize_splitter_at(f.rect, f.header_w, style, dcx, dcy);
            if !in_subzone {
                ui.push_edit({
                    let v_id = track_id;
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::BeginRenameTrack(v_id));
                    })
                });
            }
        }
        ui.toggle_button_at(id_mute, "M", m_rect, muted, &style.mute_button, |_| {
            {
                let v_id = track_id;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackMute(v_id));
                })
            }
        });
        ui.toggle_button_at(id_solo, "S", s_rect, solo, &style.solo_button, |_| {
            {
                let v_id = track_id;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackSolo(v_id));
                })
            }
        });
        // M14 Phase 68 (#040): R button (Record-arm)。 mute / solo と完全同 idiom、
        // armed track のみが audio engine の録音入力対象 (caller 仕様)。
        ui.toggle_button_at(id_armed, "R", r_rect, armed, &style.armed_button, |_| {
            {
                let v_id = track_id;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackArmed(v_id));
                })
            }
        });
        // Phase 47c: ↑/↓/× button は削除 (drag&drop reorder + Delete shortcut で代替)。
        // `MoveTrackUp/Down` は context menu / keyboard 用に残す (削除は root の arbiter)。

        // Response.track_header_rects に積む
        response.track_header_rects.push((t.id, row));

        // トラック選択トリガ: row 内 release + button_zones / disclosure いずれにも非 hit。
        // catch-all は modifier-aware な選択更新の元データを蓄えるだけ (発行は loop 後)。
        // lane disclosure (`+`/`-`) と volume band も除外する — 除外しないと
        // toggle / volume drag の release が選択更新を併発し、 multi-select が
        // 単一選択に潰れる (master 分岐は除外済みで通常 track だけ漏れていた、 review)。
        // popup (右クリックメニュー) が開いている frame も除外する: context menu は
        // `capture_input == false` で背景 pointer を mask しないので、 menu item への
        // click がこの row にも届き multi-select が単一に潰れる → 「選択トラックを
        // まとめて Delete / 複製」 が右クリック 1 本にしか効かなくなる
        // (clip 短 click の r.md #14 ガードと同 class、 r.md #43 で同件対処)。
        if pointer.primary_just_released
            && let Some((rx, ry)) = pointer.pos
            && row.contains(rx, ry)
            && !button_zones.iter().any(|b| b.contains(rx, ry))
            && !(is_group && disclosure_rect.contains(rx, ry))
            && (t.automation_lanes.is_empty() || !layout.lane_disc_rect.contains(rx, ry))
            && !layout.volume_band.is_some_and(|b| b.contains(rx, ry))
            && !ui.has_open_popups()
        {
            clicks.clicked_track = Some(t.id);
        }
    }
}

/// disclosure toggle → `AppEvent::ToggleGroupCollapsed`、
/// それ以外は `app.apply_select_tracks(tid, modifier, &visible_ids)`。
/// modifier は **press 時 snapshot** (`state.press_modifiers`) を真値にする。
///
/// **disclosure は `AppEvent::ToggleGroupCollapsed` を経由する** (r.md #74 で新設。
/// mixer の group disclosure も同じ event に合流し、 `ui_prefs.collapsed_groups` の
/// 反転経路は 1 つだけになった)。 automation lane の開閉
/// (`AppEvent::ToggleTrackAutomationCollapsed`) とは別物なので混同しないこと。
/// 発行順は disclosure が先で、 立ったら `clicked_track` を `None` に
/// 落とすという priority も現行のまま。
pub(super) fn commit_clicks(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    clicks: HeaderClicks,
    response: &mut ArrangementResponse,
) {
    let HeaderClicks { mut clicked_track, disclosure } = clicks;
    // M14 Phase 63c (#016): disclosure click → `AppEvent::ToggleGroupCollapsed`
    // (priority 高、 トラック選択はこの frame では skip = group の collapsed toggle
    // 動作のみで selection は変えない、 Reaper / Live と同じ UX)。 mixer の
    // disclosure も同じ event に合流する (r.md #74)。
    if let Some(tid) = disclosure {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ToggleGroupCollapsed { track_id: tid });
        }));
        clicked_track = None;
    }

    // r.md #71 (プラグインのコピー / 移動): 外部 drag (device の運搬) を落とした
    // frame は、 この release を「ヘッダの click」 として扱わない。 扱うと
    // Ctrl+drop が Toggle 選択として解決され、 落とし先トラックの選択が反転する /
    // last-wins タグが Tracks に倒れて次の Delete がトラックを消しに行く。
    // drag の commit 自体は caller (`view/arrangement_view.rs`) がこの後で行う。
    // `dragging_kind()` は daw-ui core の汎用 API (札の中身を知らない) なので、
    // 不変条件 8 (core にドメイン知識を持ち込まない) にも触れない。
    // 上の disclosure と同じ「priority が高い操作の frame は選択を走らせない」形。
    if ui.dragging_kind().is_some() {
        clicked_track = None;
    }

    // M14 Phase 63c (#016): clicked_track があれば modifier-aware なトラック選択を
    // 1 度だけ発行する。 Single → next = [tid]、 anchor 更新。 RangeFromAnchor (Shift)
    // → anchor から visible 列の連続範囲 (anchor が None なら Single 同等)。
    // Toggle (Ctrl) → tid を selected に対して toggle。
    if let Some(tid) = clicked_track {
        // press 時 snapshot を真値にする (release frame の生読みは
        // ModifiersChanged 先行 race で Ctrl/Shift+click が Single に化ける、
        // clip 短クリックの `last_ctrl`/`last_shift` と同 class)。
        let (shift, ctrl) = {
            let state: &ArrangementState = ui.widget_state(f.wid);
            (state.press_modifiers.shift, state.press_modifiers.ctrl)
        };
        let modifier = SelectModifier::from_modifiers(shift, ctrl);
        // r.md #35: アンカーは `SelectionState.track_anchor` が所有する (旧: widget state の
        // `ArrangementState.selection_anchor`)。 全選択面で同じ場所・同じ更新規則にするため
        // (`docs/plan_selection_modifiers.md` §4.3)。 更新規則も **Single / Toggle で更新、
        // Range は据え置き** に直した (旧実装は Single / Range で更新していたので、 Shift+click
        // を繰り返すと基点が歩いて範囲を伸縮できなかった。 Explorer / Finder / REAPER は据え置き)。
        // r.md #43: 選択遷移の解決自体は `AppData::apply_select_tracks` に集約した
        // (mixer strip の click と同じ 1 実装 + last-wins タグを Tracks に倒す口)。
        // view が渡すのは「この view の可視順」 だけ。
        let visible_ids: Vec<u32> = f.visible_tracks.iter().map(|t| t.id).collect();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.apply_select_tracks(tid, modifier, &visible_ids);
        }));
        response.selection_changed = true;
    }
}
