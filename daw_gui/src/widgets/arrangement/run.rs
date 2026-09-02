//! arrangement widget の 1 フレームのパイプライン。 **フェーズを順に呼ぶだけ**で、
//! 個々のフェーズの中身は同ディレクトリの兄弟モジュールが持つ。
//!
//! **この並び順は不変条件**: `Scene.primitives` は push 順 = z 順なので
//! (`daw_gui/tests/arr_widget.rs` の描画順テスト)、 `render::dispatch` →
//! `release::commit_releases` → `header::draw_rows` の順を入れ替えると視覚 regression に
//! なる (build / test / clippy をすり抜ける種類の壊れ方)。 `arr_widget.rs` の
//! `heavy_lanes_bg_is_drawn_before_header_rows` が機械的に止める。

use super::*;

pub fn arrangement(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> ArrangementResponse {
    // 入力ビューは `arrangement()` のスタックに置いたまま、 `frame` がそこから借りる。
    let built = view_build::build(app, area);
    let f = frame::build(&built, area, ui);
    let mut response = ArrangementResponse { ruler_rect: f.ruler, ..Default::default() };

    // 1. レイアウトを app にミラー (auto-fit / 縦ズーム用)。
    frame::mirror_layout(app, ui, &f, &mut response);
    // 2. press 振り分け (splitter → clip → arranger → ruler → header → automation)。
    press::dispatch(ui, &f);
    // 3. drag 継続 + 端オートスクロール + per-frame live 発火。
    drag::advance(ui, &f);
    launcher::drag::advance(ui, &f);
    // 4. session の overlay 用スナップショットと release take。
    let (live, released) = sessions::take(ui, &f, &mut response);
    let launcher_sessions = launcher::drag::take(ui, &f);
    let overlays = sessions::overlays(&f, &live, &released);
    // 5. hover 判定 → cursor 決定 (cursor は hover が書いた response を読む)。
    cursor::hover(&f, &live, &mut response);
    // 端オートスクロールの対象ドラッグが生きているか。判定は `arrangement_edge_scroll_axes`
    // 1 本 (端スクロール本体と同じ SSoT) — caller はこの間だけ再生追従を止める。
    response.edge_scroll_drag = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        arrangement_edge_scroll_axes(state).is_some()
    };
    cursor::apply(ui, &f, &live, &response);
    launcher::press::cursor(ui, &f);
    // 6. heavy 描画 → release commit → track header 描画 (この 3 つの順が z 順)。
    render::dispatch(ui, app, &f, &overlays, &response);
    release::commit_releases(ui, &f, &mut response, released);
    let clicks = header::draw_rows(ui, &f, &live, &mut response);
    header::commit_clicks(ui, &f, clicks, &mut response);
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
    launcher::release::commit(ui, &f, launcher_sessions, &mut response);
    // 7. caller 向け rect 群の収集。
    rects::collect(&f, &live, &mut response);

    response
}
