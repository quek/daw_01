//! arrangement widget の 1 フレームのパイプライン。 **フェーズを順に呼ぶだけ**で、
//! 個々のフェーズの中身は同ディレクトリの兄弟モジュールが持つ。
//!
//! **この並び順は不変条件**: `Scene.primitives` は push 順 = z 順なので
//! (`daw_gui/tests/arr_widget.rs` の描画順テスト)、 `render::dispatch` →
//! `release::commit_releases` → `header::draw_rows` の順を入れ替えると視覚 regression に
//! なる (build / test / clippy をすり抜ける種類の壊れ方)。 `arr_widget.rs` の
//! `heavy_lanes_bg_is_drawn_before_header_rows` が機械的に止める。

use super::*;

/// r.md #156: 録音中の区間の帯 (この file の描画の 1 段)。
mod rec_region;
/// r.md #157: コードトラックの帯 (`mod.rs` はサイズ budget の天井なので、ここに宣言する)。
pub(super) mod chord_lane;

pub fn arrangement(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> ArrangementResponse {
    // 入力ビューは `arrangement()` のスタックに置いたまま、 `frame` がそこから借りる。
    let built = view_build::build(app, area);
    let f = frame::build(&built, area, ui);
    let mut response = ArrangementResponse {
        ruler_rect: f.ruler,
        arranger_rect: f.arranger_rect,
        ..Default::default()
    };

    // 1. レイアウトを app にミラー (auto-fit / 縦ズーム用)。
    frame::mirror_layout(app, ui, &f, &mut response);
    // 2. press 振り分け (splitter → clip → arranger → ruler → header → automation)。
    press::dispatch(ui, &f);
    // 2b. ボタンを押したまま Esc → 生きている drag session を全部捨てる (r.md #127)。
    cancel::on_escape(ui, &f);
    // 2c. `docs/plan_project_tabs.md` §5.6: タブ帯の上に留まる / Ctrl+Tab → 運んでいる
    //     クリップ / トラックを別タブへの持ち込み (drag payload) に昇格して切り替える。
    xfer::promote(app, ui, &f);
    // 2d. r.md #141: クリップ / セッションビューのセルを daw_01 の窓の外 (プラグインのエディタ窓 / Explorer) へ運んだ →
    //     移動を取り消して OS のファイルドラッグに切り替える (素材ファイルを渡す)。
    xfer::promote_to_os_file_drag(app, ui, &f);
    // 3. drag 継続 + 端オートスクロール + per-frame live 発火。
    drag::advance(ui, &f);
    launcher::drag::advance(ui, &f);
    // 4. session の overlay 用スナップショットと release take。
    let (live, mut released) = sessions::take(ui, app, &f, &mut response);
    let launcher_sessions = launcher::drag::take(ui, &f);
    let overlays = sessions::overlays(app, &f, &live, &released);
    // 5. hover 判定 → cursor 決定 (cursor は hover が書いた response を読む)。
    cursor::hover(&f, &live, &mut response);
    // 端オートスクロールの対象ドラッグが生きているか。判定は `arrangement_edge_scroll_axes`
    // 1 本 (端スクロール本体と同じ SSoT) — caller はこの間だけ再生追従を止める。
    response.edge_scroll_drag = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        arrangement_edge_scroll_axes(state).is_some()
    };
    response.xfer_drag_active = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.clip_drag.as_ref().is_some_and(|d| d.kind == ClipDragKind::Move)
            || state.track_reorder.is_some()
            || state.launcher.cell_drag.is_some()
    };
    cursor::apply(ui, &f, &live, &response);
    launcher::press::cursor(ui, &f);
    // 5b. r.md #157: コードトラックの帯 (入力 → 描画 → 確定 → カーソル)。帯は他のゾーンと y が排他。
    //     描画はプレイヘッド / ループ帯 (`render::dispatch`) より奥、カーソルは上の 2 つより後勝ち。
    chord_lane::run(ui, app, &f, &mut released);
    // 6. heavy 描画 → release commit → track header 描画 (この 3 つの順が z 順)。
    render::dispatch(ui, app, &f, &overlays, &response);
    release::commit_releases(ui, &f, &mut response, released);
    let clicks = header::draw_rows(ui, &f, &live, &mut response);
    header::commit_clicks(ui, &f, clicks, &mut response);
    // 6a. r.md #157: コードの帯の見出しのボタン・Arranger 帯の見出しの表示トグル・箱をレーンへ
    //     運んでいる間の落とし先 (Arranger 帯の見出しとレーンは `render::dispatch` が塗るので、その後)。
    chord_lane::after_render(ui, app, &f);
    // 6b. ランチャー帯 (r.md #87)。 帯はヘッダ / レーンと x が排他なので z の競合は無いが、
    //     ランチャー主導の行の減光だけはアレンジのクリップの上に乗るので **この位置**。
    launcher::draw::dispatch(
        ui,
        app,
        &f,
        &launcher_sessions,
        live.clip_drag.as_ref(),
        live.automation_clip_drag.as_ref(),
        &mut response,
    );
    // 6c. r.md #131: 無効トラックの行を沈める (レーンのクリップとランチャーのセルの上に乗るので帯の後)。
    disabled_rows::draw(ui, &f);
    // 6d. r.md #156: オーディオ入力を録音中の区間 (レーンのクリップの上に乗る)。
    rec_region::draw(ui, app, &f);
    launcher::release::commit(ui, &f, launcher_sessions, &mut response);
    // 7. caller 向け rect 群の収集。
    rects::collect(&f, &live, &mut response);

    response
}
