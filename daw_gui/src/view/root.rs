//! ルート view: 画面全体を Transport / Inspector / Arrangement / BottomPanel /
//! StatusBar に分割し、各 sub view を呼ぶ。Plugin picker / help は modal overlay。
//!
//! build_root の末尾で `Ui::take_shortcut` を順に消費し、AppEvent (or
//! `Ui::request_undo` 等) に変換する。global shortcut の dispatch はここに集約。

use daw_ui_core::{Edit, Orientation, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, EditSurface};
use crate::event::NudgeStep;
use crate::event_launcher::{LauncherCellKey, LauncherEvent};
use crate::view::{
    about, arrangement_view, bottom_panel, clipboard_ops, dirty_guard_modal, export_overlay,
    export_range_modal,
    shutdown_overlay,
    font_picker, load_overlay, loudness_report, master_panel, menu_bar, mixer_strips, plugin_picker,
    recovery_modal,
    resource_monitor,
    settings, shortcuts_help, snap, status_bar, track_inspector, track_picker, transport,
    undo_history, voicevox_overlay,
};

pub const MENU_H: f32 = 24.0;
pub const TRANSPORT_H: f32 = 44.0;
pub const STATUS_H: f32 = 24.0;
pub const INSPECTOR_W: f32 = 280.0;

/// arrangement (top) と bottom_panel (= piano_roll / mixer / audio_editor) の
/// 初期分割比率。 上が `default_ratio`、 下が `1.0 - default_ratio`。 0.65 で
/// 旧 BOTTOM_H = 240 / 典型 sh = 720 とおおよそ等価。 ユーザーが境界 handle
/// を drag すると gui_01 `split_view` widget が state に新比率を持って frame
/// 越しに保持する (= session 内のみ persist、 project save 不対応は別 phase)。
const ARRANGEMENT_SPLIT_DEFAULT_RATIO: f32 = 0.65;

pub fn build_root<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, screen: PhysicalSize) {
    let sw = screen.width as f32;
    let sh = screen.height as f32;

    // 全画面背景 = アプリの床。 全 panel がこの上に浮いて見える (`Palette::is_dark`
    // の判定基準でもあるので、 テーマ切替はまずここが変わる)。
    ui.panel(
        "root_bg",
        Rect { x: 0.0, y: 0.0, w: sw, h: sh },
        app.theme.core.window_bg,
        0.0,
    );

    // r.md #29: 編集履歴 window が開いていれば、 その rect 分だけ背後の pointer を
    // 占有する予約を **背景 widget 描画より前** に行う (= window の上の click が
    // アレンジ等に漏れず、 window の外は通常操作できる true floating)。 本体描画は
    // build_root 末尾で `undo_history::draw`。
    // r.md #54: 解析の走査中は他の floating window を出さない。これらは
    // `with_floating_region` で raw pointer に戻すので、暗転の下でも押せてしまう
    // (編集履歴の行を click すると走査中に Song が飛ぶ)。
    let floating_ok = !app.loudness.phase.is_busy();
    if floating_ok {
        undo_history::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
        // r.md #48: 設定 window も同じ true-floating 機構 (背景を暗転しないので、
        // テーマを選んだ瞬間に背後の全画面が切り替わるのを見ながら選べる)。
        settings::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
    }
    // r.md #54: ラウドネスレポート window。走査中は **画面全体** を予約して
    // 背景を丸ごと inert にする (暗転と入力遮断が同じ 1 つの根拠から出る)。
    loudness_report::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

    // ----- レイアウト計算 -----
    let menu_rect = Rect { x: 0.0, y: 0.0, w: sw, h: MENU_H };
    let transport_rect = Rect { x: 0.0, y: MENU_H, w: sw, h: TRANSPORT_H };
    let header_h = MENU_H + TRANSPORT_H;
    let center_bottom_rect = Rect {
        x: 0.0,
        y: header_h,
        w: sw,
        h: (sh - header_h - STATUS_H).max(0.0),
    };
    let status_rect = Rect {
        x: 0.0,
        y: sh - STATUS_H,
        w: sw,
        h: STATUS_H,
    };

    menu_bar::draw(app, ui, menu_rect);
    transport::draw(app, ui, transport_rect);

    // inspector を左カラムにフル高さで配置し、 その右で arrangement
    // (上) と bottom_panel (= mixer / piano_roll / audio editor、 下) を縦分割する。
    // 旧レイアウト (inspector は上ペイン内の左帯、 bottom panel は全幅) から
    // 「左=inspector フル高 / 右上=arrangement / 右下=bottom panel」 へ再編。
    // gui_01 `split_view` が 6px の handle を描画して上下 drag を扱う。
    // r.md #50: 右端にマスターパネルを切り出してから、残りを inspector +
    // split(arrangement / bottom) に配る。**アレンジと Mixer/MIDI エディタの
    // 両方の右**に常駐させるので、切り出すのは center_bottom 帯の全高。
    let master_w = master_panel::panel_width(app).min((center_bottom_rect.w - INSPECTOR_W).max(0.0));
    let master_rect = Rect {
        x: center_bottom_rect.x + center_bottom_rect.w - master_w,
        y: center_bottom_rect.y,
        w: master_w,
        h: center_bottom_rect.h,
    };
    let inspector_rect = Rect {
        x: center_bottom_rect.x,
        y: center_bottom_rect.y,
        w: INSPECTOR_W,
        h: center_bottom_rect.h,
    };
    let right_rect = Rect {
        x: center_bottom_rect.x + INSPECTOR_W,
        y: center_bottom_rect.y,
        w: (center_bottom_rect.w - INSPECTOR_W - master_w).max(0.0),
        h: center_bottom_rect.h,
    };

    // inspector はフル高の左カラム。 global shortcut を消費する widget では
    // ないので split (= arrangement / bottom panel widget) より前に描いてよい。
    track_inspector::draw(app, ui, inspector_rect);

    // 比率は **アプリが所有する** (widget は覚えない)。`ui_prefs` に置くことで
    // `ViewState` 経由でプロジェクトに保存され、開き直しても境界が戻らない。
    // `0.0` = 未設定 (新規 / 旧ファイル) なので既定比率へ倒す。
    let split_ratio = if app.ui_prefs.arrangement_split_ratio > 0.0 {
        app.ui_prefs.arrangement_split_ratio
    } else {
        ARRANGEMENT_SPLIT_DEFAULT_RATIO
    };
    // 内蔵チャンネルストリップ帯 (docs/plan_channel_strip.md §4): EQ / Comp を
    // 開いたら、**開いた帯の高さぶんだけ**下ペインを広げる。こうするとフェーダー /
    // メーター / Sends は開閉で 1px も動かない (strip 全体が同じ量だけ伸びるため)。
    //
    // **保存された比率は書き換えず**、描画時に差し引くだけにする — 閉じた瞬間に
    // ユーザーの比率へ自動で戻り、復元用の状態を別に持たずに済む。アレンジ側が
    // 潰れきらないよう下限だけ置く (画面が足りないときはそこで頭打ちになり、
    // 以降はフェーダーが縮む)。
    //
    // r.md #99: split の drag は **表示比率** (= 差し引いた後) で来るので、保存する
    // ときは差し引いた分を足し戻す。足し戻さないとリリースの次フレームでもう一度
    // 差し引かれ、EQ / Comp を開いている間だけ「離した瞬間に帯の高さぶん縮む」。
    let extra_frac = if app.ui_prefs.bottom_panel == Some(0) && right_rect.h > 0.0 {
        mixer_strips::extra_head_height(app) / right_rect.h
    } else {
        0.0
    };
    let split_ratio = if extra_frac > 0.0 {
        const MIN_ARRANGEMENT_FRAC: f32 = 0.15;
        (split_ratio - extra_frac).max(MIN_ARRANGEMENT_FRAC)
    } else {
        split_ratio
    };
    // r.md #96: 下部パネルが閉じている (`B` で Mixer を閉じた) ときは split を
    // 出さず、アレンジが右カラムの全高を使う。保存された比率は触らないので、
    // 次に開いたときは元の高さに戻る。
    match app.ui_prefs.bottom_panel {
        Some(tab) => ui.split_view(
            "root_arrange_bottom",
            right_rect,
            Orientation::Vertical,
            split_ratio,
            |next| {
                // 「見方の都合」なので `*` は立てない (ズーム / スクロールと同じ扱い、
                // `project_dirty_flag_rule`)。
                Edit::mutate(move |app: &mut AppData| {
                    app.ui_prefs.arrangement_split_ratio = next + extra_frac;
                })
            },
            |ui, arrangement_rect, bottom_rect| {
                draw_arrangement_column(app, ui, arrangement_rect, bottom_rect);
                bottom_panel::draw(app, ui, bottom_rect, tab);
            },
        ),
        None => {
            // 高さ 0 の bottom_rect: `dispatch_shortcuts` の「pointer が下部パネル内か」
            // 判定が必ず偽になり、Mixer / Piano Roll 文脈のショートカットが誤発火しない。
            let closed_bottom = Rect { x: right_rect.x, y: right_rect.y + right_rect.h, w: right_rect.w, h: 0.0 };
            draw_arrangement_column(app, ui, right_rect, closed_bottom);
        }
    }

    // r.md #50: マスターパネル (右端フル高)。split の後に描くのは、
    // 分割 handle の hit 判定より手前で pointer を受けたいから。
    master_panel::draw(app, ui, master_rect);

    status_bar::draw(app, ui, status_rect);

    // resource monitor (r.md #3): 詳細パネル (non-modal overlay)。 開いている時だけ
    // 描画する。 modal より前に呼ぶので、 modal が出れば自然に隠れる (意図どおり)。
    resource_monitor::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

    // Undo 履歴 window (r.md #29): inline な true-floating window の本体描画
    // (背景描画の後 = z-order 最前面)。 pointer 占有予約は build_root 冒頭の
    // `undo_history::reserve`。
    // r.md #54: 走査中は描かない (reserve と対) — 暗転の下に操作可能な窓を残さない。
    if !app.loudness.phase.is_busy() {
        undo_history::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

        // 設定 window (r.md #48): 同上、 背景描画の後 = z-order 最前面。
        settings::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
    }

    // ラウドネスレポート window (r.md #54): 同上。走査中はここで暗転も描く。
    loudness_report::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

    // Modal: plugin picker。draw 関数内で modal の open/close を app.ui_ephemeral.is_plugin_picker_open
    // と同期させる (常時呼び、内部で is_modal_open / open_modal を管理)。
    plugin_picker::draw(app, ui, screen);

    // Modal: font picker (Text クリップのフォント選択)。is_font_picker_open と同期。
    font_picker::draw(app, ui, screen);

    // 非ブロック overlay: プロジェクトロードの進捗。
    load_overlay::draw(app, ui, screen);

    // 非ブロック overlay: VOICEVOX wav 合成 / 口パク生成の進行状態。
    voicevox_overlay::draw(app, ui, screen);

    // Modal: send 宛先トラックピッカー。app.ui_ephemeral.send_picker == Some(..) のとき開く。
    track_picker::draw(app, ui, screen);

    // Modal: recovery (起動時 or Open 時に検出された autosave 候補)。
    // app.ui_ephemeral.show_recovery_modal を internal で監視するため常時呼び。
    recovery_modal::draw(app, ui, screen);

    // Modal: 未保存変更ありで「プロジェクトを破棄する操作」 (終了 / New /
    // Open / Open Recent) をしようとしたときの「保存して続行 / 保存せず続行 /
    // キャンセル」 確認。 app.ui_ephemeral.dirty_guard を監視。
    dirty_guard_modal::draw(app, ui, screen);

    // Modal: 書き出し範囲ピッカー。app.ui_ephemeral.export_range_picker == Some の
    // とき開く。 export 実行前なので export_overlay より前に描いてよい。
    export_range_modal::draw(app, ui, screen);

    // Overlay: WAV / Video export 中の進捗 + Cancel。app.transport.export_stage を監視。
    export_overlay::draw(app, ui, screen);

    // Overlay: F1 ショートカット / マウス操作一覧。app.ui_prefs.is_help_open と
    // 同期。最前面に出すため他の modal / overlay より後に描く。
    shortcuts_help::draw(app, ui, screen);

    // Overlay: ヘルプ > バージョン情報 (r.md #60)。app.ui_prefs.is_about_open と同期。
    about::draw(app, ui, screen);

    // r.md #61: Overlay:「終了処理中…」。子プロセスがプラグインを畳んで exit
    // するのを待っている間だけ出す。**最後に描く** — 終了に入ったら他の
    // modal / overlay より前面で、下の操作を全部塞ぐ。
    // (バージョン情報を開いたまま終了しても、終了オーバーレイが手前に来る。)
    shutdown_overlay::draw(app, ui, screen);

    // r.md #71 (プラグインのコピー / 移動): 運んでいる最中に「何を掴んでいるか」を
    // 見せる。view の最後に描くので常に最前面。運搬そのものは daw-ui の drag payload が
    // 持っているので、ここは表示だけ (状態を持たない)。
    draw_device_drag_preview(app, ui);
}

/// 運搬中の device ラベル (D-6)。 波形やクリップ色の上に出るので、 背景に依存しない
/// 暗いチップ + 明るい文字でコントラストを保証する
/// (`[[feedback_ui_indicator_contrast_on_variable_bg]]`)。
/// 右カラムのアレンジ部分: root レベルの shortcut dispatch → arrangement 描画。
///
/// gui_01 widget (piano_roll 等) は `take_shortcut` を消費する側面があるため、先に
/// root レベルで shortcut を捌いて広域の挙動を確定させる。widget 描画時には消費済みに
/// なり、widget 内蔵の同名 shortcut handler は no-op に縮退する。`bottom_rect` は
/// 下部パネルの領域 (閉じているときは高さ 0) で、piano_roll / mixer active 判定に使う。
fn draw_arrangement_column(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    arrangement_rect: Rect,
    bottom_rect: Rect,
) {
    dispatch_shortcuts(app, ui, bottom_rect);
    arrangement_view::draw(app, ui, arrangement_rect);
}

fn draw_device_drag_preview(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let Some(p) = ui.drag_payload::<crate::app::DeviceDragPayload>(crate::app::DEVICE_DRAG_KIND)
    else {
        return;
    };
    let Some((px, py)) = ui.pointer().pos else {
        return;
    };
    let label = format!("プラグイン {}", p.device_ids.len());
    let core = &app.theme.core;
    let chip = Rect { x: px + 12.0, y: py + 12.0, w: 120.0, h: 22.0 };
    ui.panel_with_border("device_drag_chip", chip, core.panel_raised, core.accent, 1.0, 3.0);
    ui.label_at("device_drag_label", &label, chip.x + 8.0, chip.y + 5.0, 11.0, core.text);
}

/// r.md #67: カーソルキーで選択ノートを動かす / 伸縮する / 音程を変える。
///
/// **対象面が `Notes` のときだけ消費する**。 面の解決は copy / cut / delete と同じ
/// [`AppData::edit_surface`] (last-selection-wins) なので、「アレンジでクリップを選び直した
/// 直後に矢印を押してもノートは動かない」 が既存規則どおりに保証され、 マウス位置にも依存しない。
/// ノートを 1 つも選んでいなければ `edit_surface` が `Notes` を返さないので何も起きない。
///
/// 各キーは [`Ui::take_shortcut_count`] で **届いた回数ぶん** 取り出す。 矢印は repeatable
/// 宣言なので押しっぱなしで 1 フレームに複数回積まれることがあり、 1 回しか消費しないと
/// 移動量がフレームレート次第で目減りする。
///
/// 修飾キーの規約 (daw_01 全体と共通): 無修飾 = 位置 / Ctrl = そのノート自身の量 (長さ) /
/// Shift = 大きいステップ / Alt = スナップ一時無効。
/// 範囲がアクティブなときの矢印キー (`docs/plan_range_selection.md` §3.2)。
///
/// - 素の ←→ … **範囲内の素材**をグリッド 1 つ分ナッジ (Live §6.9)
/// - `Alt`+←→ … 同じくナッジ、ただしスナップ無効 (微小量)
/// - `Shift`+←→ … 範囲の右端を伸縮
///
/// ノート面と同じ 12 本のバインドを**面で振り分ける**だけなので、キー割り当ては増えない。
fn dispatch_range_nudge(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    if !matches!(surface, Some(EditSurface::TimeRange)) {
        return;
    }
    // グリッド 1 つ分 (スナップ OFF なら 1 拍)。 Alt 版は微小量。
    let snap = crate::view::snap::arrange_snap_config(app);
    let grid = snap.beat_unit(app.ui_prefs.arrange_zoom_x).unwrap_or(1.0);
    const FINE: f64 = 1.0 / 64.0;
    let takes: [(&'static str, f64, bool); 6] = [
        ("daw.nudge_note_left", -grid, false),
        ("daw.nudge_note_right", grid, false),
        ("daw.nudge_note_left_fine", -FINE, false),
        ("daw.nudge_note_right_fine", FINE, false),
        ("daw.nudge_note_left_bar", -grid, true),
        ("daw.nudge_note_right_bar", grid, true),
    ];
    // ↑↓ = 範囲をレーン方向へ伸ばす。
    for (name, dir) in [("daw.nudge_note_up", -1_i32), ("daw.nudge_note_down", 1_i32)] {
        let n = ui.take_shortcut_count(name).min(64);
        for _ in 0..n {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.extend_time_selection_lanes(dir);
            }));
        }
    }
    for (name, delta, is_resize) in takes {
        let n = ui.take_shortcut_count(name).min(64);
        if n == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let total = delta * n as f64;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_resize {
                app.resize_time_selection(total);
            } else {
                app.nudge_time_selection(total);
            }
        }));
    }
}

fn dispatch_note_nudge(ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    if !matches!(surface, Some(EditSurface::Notes)) {
        return;
    }
    // 1 フレームに積める repeat 数の上限。 OS のリピート速度 (最速でも ~30/s) に対して
    // 十分過大なので実用上ここに当たることは無いが、 `dir * n` の符号反転で i32 が
    // オーバーフローしない (= debug で panic しない) ことを構造的に保証する。
    const MAX_REPEATS_PER_FRAME: usize = 64;
    let take = |ui: &mut Ui<'_, AppData>, name: &'static str| -> i32 {
        i32::try_from(ui.take_shortcut_count(name).min(MAX_REPEATS_PER_FRAME)).unwrap_or(0)
    };
    // (shortcut 名, 発行する event を作る関数)。`steps` の符号が方向。
    let time: [(&'static str, NudgeStep, i32); 6] = [
        ("daw.nudge_note_left", NudgeStep::Grid, -1),
        ("daw.nudge_note_right", NudgeStep::Grid, 1),
        ("daw.nudge_note_left_bar", NudgeStep::Bar, -1),
        ("daw.nudge_note_right_bar", NudgeStep::Bar, 1),
        ("daw.nudge_note_left_fine", NudgeStep::Fine, -1),
        ("daw.nudge_note_right_fine", NudgeStep::Fine, 1),
    ];
    for (name, step, dir) in time {
        let n = take(ui, name);
        if n > 0 {
            let steps = dir * n;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::NudgeSelectedNoteTime { step, steps });
            }));
        }
    }
    for (name, dir) in [("daw.nudge_note_shorter", -1), ("daw.nudge_note_longer", 1)] {
        let n = take(ui, name);
        if n > 0 {
            let steps = dir * n;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::NudgeSelectedNoteLength {
                    step: NudgeStep::Grid,
                    steps,
                });
            }));
        }
    }
    let pitch: [(&'static str, bool, i32); 4] = [
        ("daw.nudge_note_up", false, 1),
        ("daw.nudge_note_down", false, -1),
        ("daw.nudge_note_octave_up", true, 1),
        ("daw.nudge_note_octave_down", true, -1),
    ];
    for (name, octave, dir) in pitch {
        let n = take(ui, name);
        if n > 0 {
            let steps = dir * n;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::NudgeSelectedNotePitch { octave, steps });
            }));
        }
    }
}

/// `ShortcutMap` ルックアップで判定済みの shortcut name を pull して AppEvent / undo
/// 要求に変換する。`Ui::take_shortcut` は 1 度だけ消費するので、各 name について
/// この関数で一括処理する。`app` は immut で受けて、コピーや状態判定のみで使う
/// (mutation は `Ui::push_edit(Edit::mutate(...))` 経由)。
///
/// Q (= 「カーソル直下のものを無効化 / 有効化」) を内蔵チャンネルストリップに
/// 割り当てる。カーソルが Comp / EQ のセクション本体か常設帯の上にあれば、
/// そのセクションのバイパスを切り替えて `true`。対象が無ければ `false` で、
/// 呼び出し側は従来どおり note / clip の mute へ進む。
///
/// 対象面の算出は `view::strip_sections` (`mixer_hovered_strip_section`) が SSoT。
/// r.md #105: `Q` が bypass 切替する device = **カーソル直下のチェーン行だけ** (S キーの
/// ソロと同じ「カーソルがある行」規則)。 device の選択集合は使わない — チェーン行の
/// 選択は画面上で見分けにくく、 選択優先にすると「別の行を指して押したのに前に click
/// した行が切り替わる」 (実機 2026-09-05)。 空 = device は対象外 (clip / note へ落とす)。
fn q_device_targets(app: &AppData) -> Vec<u64> {
    app.ui_ephemeral.inspector_hovered_device.into_iter().collect()
}

fn toggle_hovered_strip_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    mixer_active: bool,
) -> bool {
    use crate::event::{MasterSection, StripEdit, StripSection};
    // マスターパネルは常時描かれるので hover が古くなることはない。ミキサーより
    // 先に見る (パネルは mixer / arrangement のどちらの上にも無く、排他)。
    if let Some(section) = app.ui_ephemeral.master_hovered_section {
        let param = match section {
            MasterSection::Comp => common::model::MasterStripParam::CompOn,
            MasterSection::Eq => common::model::MasterStripParam::EqOn,
            MasterSection::Limiter => common::model::MasterStripParam::LimiterOn,
        };
        let on = app.song_doc.song().master_strip.param(param) >= 0.5;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::MasterStripEdit {
                param,
                value: f32::from(u8::from(!on)),
            });
        }));
        return true;
    }
    // hover 値は strip を描いた frame にしか更新されないので、Mixer タブから
    // 離れた後も最後の値が残る。**タブと pointer 位置で毎回ゲートする**
    // (`mixer_hovered_track` を使う S キーと同じ作法) — 無いと Piano Roll に
    // 切り替えた後の Q がノート mute ではなくストリップ切替になる。
    if !mixer_active {
        return false;
    }
    let Some((track_id, section)) = app.ui_ephemeral.mixer_hovered_strip_section else {
        return false;
    };
    let param = match section {
        StripSection::Comp => common::model::TrackBuiltinParam::StripCompOn,
        StripSection::Eq => common::model::TrackBuiltinParam::StripEqOn,
    };
    let on = app
        .song_doc
        .song()
        .track_by_id(track_id)
        .and_then(|t| t.strip.target_value(&param))
        .unwrap_or(0.0)
        >= 0.5;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::StripEdit {
            track: track_id,
            edit: StripEdit::Param { param, value: f32::from(u8::from(!on)) },
        });
    }));
    true
}

/// `f` キーが発火するイベント。カーソル直下の拍を現在の snap 設定で吸着し
/// (`alt` = 一時 snap 解除)、どこで押したかで 2 通りに解決する:
///
/// - **ピアノロールで開いているのがランチャーのセル** — 全体を「カーソルの拍から」
///   ([`LauncherEvent::PlayFromCellBeat`]: そのセルをその拍から撃ち、鳴っている他の
///   全行も同じ拍へ揃え、song も seek)。song の seek だけでは足りない — セルは
///   `start_beat = 0` の自分の時間軸で走り、seek は engine が位相を平行移動して吸収する
///   (`on_transport_jump`) ので、再生位置が動かない。ピアノロールの拍はセルの
///   `start_beat` 原点 (= `editor_playhead_beat` と同じ空間) なので、そのままセル内の
///   位相になる。
/// - **それ以外** (アレンジのクリップ / アレンジ面) — song-absolute 拍へ seek して再生
///   ([`AppEvent::PlayFromCursor`])。停止中 play() / 再生中 seek 継続は handler 側。
///
/// grid 外 (hover が `None`) なら何もしない。
fn play_from_cursor_event(app: &AppData, alt: bool, is_pianoroll_active: bool) -> Option<AppEvent> {
    if !is_pianoroll_active {
        let raw = app.ui_ephemeral.arrangement_hover_beat_raw?;
        let beat = snap::arrange_snap_config(app).snap_beat(
            raw,
            alt,
            app.ui_prefs.arrange_zoom_x.max(1.0),
        );
        return Some(AppEvent::PlayFromCursor { beat });
    }
    let raw = app.ui_ephemeral.pianoroll_hover_beat_song_raw?;
    let beat = snap::piano_roll_snap_config(app).snap_beat(raw, alt, app.pianoroll_zoom_x());
    let cell = app.pianoroll_target_clip().filter(|k| {
        app.song_doc
            .song()
            .track_by_id(k.track_id)
            .is_some_and(|t| t.session_clip_by_id(k.clip_id).is_some())
    });
    Some(match cell {
        Some(key) => AppEvent::Launcher(LauncherEvent::PlayFromCellBeat {
            cell: LauncherCellKey::Track(key),
            phase_beats: beat - app.clip_start_beat_of(key),
        }),
        None => AppEvent::PlayFromCursor { beat },
    })
}

/// `bottom_rect` は piano_roll active 判定用。マウスが bottom_panel 領域内 + Piano Roll
/// タブが選択中なら G/X/1/2/3 を piano_roll 系に流す。それ以外は arrange 系。
fn dispatch_shortcuts(app: &AppData, ui: &mut Ui<'_, AppData>, bottom_rect: Rect) {
    // r.md #54: 範囲ラウドネス解析の走査中は **Esc (= 中止) 以外の shortcut を
    // 一切通さない**。
    //
    // レポート窓の全画面 `reserve_floating_region` が落とすのは pointer だけで、
    // `take_shortcut` は素通りする。そのままだと Ctrl+Z / Ctrl+Y / 履歴ジャンプが
    // freewheel 走査の最中に Song を差し替え、次フレームの `flush_song_sync` が
    // 走査中の daw_audio へ `LoadSong` を、plugin_host へ `SetupAraDocument`
    // (= プラグイン再初期化) を送ってしまう (undo / redo は `edit_song` を通らない
    // ので編集ロックでも止まらない)。Ctrl+E は走査中に `ReinitAllPlugins` を撃つ。
    // 書き出しは `export_overlay` が真のモーダル (capture_keyboard) なので同じ
    // 事故が起きない — 解析だけがこの保護を欠いていた。
    if app.loudness.phase.is_busy() {
        if ui.take_shortcut("escape") {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelLoudnessAnalysis)
            }));
        }
        return;
    }
    // 編集面 arbiter (clipboard / delete / `Z` zoom / `R` loop が共有)。 関数冒頭で
    // 1 度算出し、 先頭の `R` ブロックから末尾の全選択ブロックまで全シーケンスで使う。
    // `is_pianoroll_active`: マウスが bottom_panel 内 + Piano Roll タブ選択中か。
    let pointer_in_bottom = ui
        .pointer()
        .pos
        .is_some_and(|(px, py)| bottom_rect.contains(px, py));
    let is_pianoroll_active = app.ui_prefs.bottom_panel == Some(1) && pointer_in_bottom;
    let surface = app.edit_surface(is_pianoroll_active);
    // `Z` 段階ズーム / `R` loop の対象面 (通常 clip / automation clip) は
    // copy / cut / delete と同じ `edit_surface` arbiter で解決する (last-selection-wins)。
    // これで「MIDI clip を選んでも残存 automation 選択へズームしてしまう」 を防ぐ。
    let zoom_automation = matches!(surface, Some(EditSurface::AutomationClips));

    // ----- Transport -----
    if ui.take_shortcut("daw.play_toggle") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::PlayToggle)
        }));
    }
    if ui.take_shortcut("daw.toggle_loop") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleLoop)
        }));
    }
    if ui.take_shortcut("daw.loop_selected_clip") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::LoopSelectedClipToggle {
                automation: zoom_automation,
            })
        }));
    }
    // Phase 7 B5 (`docs/plan_scale.html` §5.3): Shift+P で選択 clip の
    // note pitch を最寄り in-scale に一括補正。 selected_notes が空なら
    // clip 全 note、 そうでなければ選択 note のみ。
    if ui.take_shortcut("daw.quantize_pitches_to_scale") {
        let target = if app.selected_note_ids().is_empty() {
            crate::app::QuantizePitchTarget::SelectedClipAllNotes
        } else {
            crate::app::QuantizePitchTarget::SelectedNotes
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::QuantizePitchesToScale(target))
        }));
    }
    // Ableton Live's Cmd/Ctrl+G — group the selected tracks. gui_01
    // #016 で arrangement widget が track header の Shift/Ctrl クリック
    // 多重選択を実装したので、 selection は `selected_track_ids` から
    // 直接取れる。 空なら no-op。
    if ui.take_shortcut("daw.group_tracks") {
        let track_ids = app.selection.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::GroupSelectedTracks { track_ids });
            }));
        }
    }
    // Alt+G — ungroup the selected group tracks (Ableton Live の
    // Cmd/Ctrl+Shift+G に相当、 本 DAW はユーザー指定で Alt+G)。
    if ui.take_shortcut("daw.ungroup_tracks") {
        let track_ids = app.selection.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::UngroupTracks { track_ids });
            }));
        }
    }
    // Ctrl+T: 新規トラックを末尾に追加 (vocal は instrument に VOICEVOX を挿して作る)。
    if ui.take_shortcut("daw.add_track") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AddInstrumentTrack);
        }));
    }
    // PR-V4: daw.synthesize_vocal shortcut は無効化 (builtin VOICEVOX
    // plugin が自動 synth するため explicit trigger 不要)。 user が
    // shortcut を押しても sync_vocal_metadata で再 flush が走るので、
    // 「再 synth したい」 場合は notes 編集すれば trigger される。

    // ----- File -----
    if ui.take_shortcut("new") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::New)
        }));
    }
    if ui.take_shortcut("open") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Open)
        }));
    }
    if ui.take_shortcut("save") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Save)
        }));
    }
    if ui.take_shortcut("save_as") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SaveAs)
        }));
    }
    if ui.take_shortcut("daw.export_wav") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ExportWav)
        }));
    }
    // r.md #61: Ctrl+Q。File > 終了 / ✕ / Alt+F4 と同じ `AppEvent::Quit` に合流する。
    if ui.take_shortcut("quit") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Quit(crate::shutdown::QuitRequest::USER))
        }));
    }
    // ----- Edit -----
    // undo/redo は daw_gui の SongDoc snapshot が SSoT (AppEvent::Undo/Redo → song_doc.undo())。
    // lib 側 undo は S4a で撤去したので、ここで shortcut を拾って自前 undo に流すのが最終形。
    if ui.take_shortcut("undo") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Undo)
        }));
    }
    if ui.take_shortcut("redo") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Redo)
        }));
    }
    // r.md #29: Ctrl+Alt+Z で編集履歴パネルを開閉。
    if ui.take_shortcut("daw.toggle_undo_history") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleUndoHistory)
        }));
    }
    // r.md #50: Ctrl+Alt+M でマスターパネルを開閉 (REAPER の Master Track トグルと同キー)。
    if ui.take_shortcut("daw.toggle_master_panel") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleMasterPanel)
        }));
    }
    // r.md #96: B で下部パネルの Mixer を開閉 (Bitwig の B と同キー)。
    if ui.take_shortcut("daw.toggle_mixer_panel") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleMixerPanel)
        }));
    }
    // r.md #55: Ctrl+Shift+W で開いているプラグインエディタ窓を全部閉じる。
    // 転送対象なので、エディタ窓にフォーカスがある状態で押しても同じ経路に合流する。
    if ui.take_shortcut("daw.close_all_plugin_editors") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CloseAllPluginEditors)
        }));
    }
    // r.md #48: Ctrl+, で設定を開閉 (Edit メニュー「設定...」 と同じイベント)。
    if ui.take_shortcut("daw.toggle_settings") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleSettings)
        }));
    }
    // r.md #54: Ctrl+L で範囲ラウドネス解析 (解析メニューと同じイベント)。
    if ui.take_shortcut("daw.analyze_loudness") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AnalyzeLoudness)
        }));
    }
    // ----- Clipboard / Delete (統一 arbiter) -----
    // ポインタが乗っている編集面 → なければ選択優先順、で対象面を一意に決める
    // (grill-me 2026-06-11)。copy / cut / paste / delete が同じ arbiter を共有。
    // text_input focus 中は gui_01 が cut/copy/paste/delete を自動 suppress するので
    // typing guard は不要。
    // (`is_pianoroll_active` / `surface` / `zoom_automation` は関数冒頭で算出済 —
    // `R` loop が先頭ブロックで使うため。)

    // f キー: カーソル位置から再生 (アレンジ / アレンジのクリップは seek、ランチャーの
    // セルはそのセルを途中から撃つ)。解決は `play_from_cursor_event`。
    if ui.take_shortcut("daw.play_from_cursor")
        && let Some(ev) = play_from_cursor_event(app, ui.pointer().modifiers.alt, is_pianoroll_active)
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.handle_event(ev)));
    }

    // Home / End: プレイヘッドをタイムラインの端へ移動 (r.md #10)。 typing 中は
    // `typing_only` 宣言で抑止され text_input のカーソル移動になるので、
    // ここに来るのは非 typing 時のみ。 seek は handler が停止/再生とも面倒を見る。
    if ui.take_shortcut("daw.goto_timeline_home") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::GotoTimelineHome)
        }));
    }
    if ui.take_shortcut("daw.goto_timeline_end") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::GotoTimelineEnd)
        }));
    }

    // Alt+F: 再生追従スクロールの方式を循環 (OFF → 連続 → ページ)。
    if ui.take_shortcut("daw.cycle_arrange_follow") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CycleArrangeFollow);
        }));
    }

    // トラック copy/cut の非同期結果 (plugin state 収集後) を OS clipboard へ flush。
    if let Some(text) = app.ui_ephemeral.pending_clipboard_write.clone() {
        ui.set_clipboard_text(text);
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.pending_clipboard_write = None;
        }));
    }

    if ui.take_shortcut("copy") {
        clipboard_ops::copy_for_surface(app, ui, surface);
    }
    if ui.take_shortcut("cut") {
        clipboard_ops::cut_for_surface(app, ui, surface);
    }
    if ui.take_shortcut("paste")
        && let Some(text) = ui.take_clipboard_paste()
    {
        clipboard_ops::paste_from_clipboard(app, ui, &text, is_pianoroll_active);
    }
    if ui.take_shortcut("delete") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.delete_current_surface(is_pianoroll_active);
        }));
    }

    // ----- Grid snap / fit -----
    // active view 判定は上で算出した `is_pianoroll_active` を共有する
    // (マウスが bottom_panel 領域内 AND Piano Roll タブ選択中)。
    if ui.take_shortcut("daw.toggle_snap") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::TogglePianoRollSnap);
            } else {
                app.handle_event(AppEvent::ToggleArrangeSnap);
            }
        }));
    }
    if ui.take_shortcut("daw.fit_view") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::FitPianoRollToClip);
            } else {
                // arrangement: 直前のズームに戻る (履歴が空なら全体フィット)。
                app.handle_event(AppEvent::ArrangeZoomBack);
            }
        }));
    }
    // Z: arrangement の段階ズーム (1 回目=横、 2 回目=縦)。 piano roll が active
    // (= pointer が piano roll 上) のときは clip ズーム概念が無いので発火しない。
    // text_input focus 中は gui_01 が単キーを抑制する。
    if ui.take_shortcut("daw.zoom_selected_clip") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if !is_pianoroll_active {
                app.handle_event(AppEvent::ZoomArrangeToSelectedClip {
                    automation: zoom_automation,
                });
            }
        }));
    }
    if ui.take_shortcut("daw.narrow_grid") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::NarrowPianoRollGrid);
            } else {
                app.handle_event(AppEvent::NarrowArrangeGrid);
            }
        }));
    }
    if ui.take_shortcut("daw.widen_grid") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::WidenPianoRollGrid);
            } else {
                app.handle_event(AppEvent::WidenArrangeGrid);
            }
        }));
    }
    if ui.take_shortcut("daw.toggle_triplet") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::TogglePianoRollTriplet);
            } else {
                app.handle_event(AppEvent::ToggleArrangeTriplet);
            }
        }));
    }

    // ----- Track solo (S): piano roll + arrangement / mixer -----
    // S キーで「マウス直下のトラック」を solo toggle する (mixer / arrangement の
    // S ボタンと同じ ToggleTrackSolo を発火)。 対象 track は pointer の位置で決まる:
    // - piano roll active (= pointer が bottom panel 内 + Piano Roll タブ。 audio
    //   editor は MIDI 編集文脈ではないので除外): **編集対象 clip**
    //   (`pianoroll_target_clip` = 新規ノートの所属先・凡例の強調行) の所属 track。
    //   時間範囲選択から引くと、 ノートを選んだ瞬間 (レーンが KeyTrack に変わる) や
    //   ランチャーセル編集中に None になって効かない。
    // - mixer (= pointer が bottom panel 内 + Mixer タブ): マウス直下のストリップ
    //   (`mixer_hovered_track`)。 master strip / strip 外は None で no-op。
    // - それ以外 (= pointer がアレンジ上): マウス直下のトラック
    //   (`arrange_hovered_track`)。 ヘッダ列でもクリップレーン上でも
    //   同じトラック行を返し、 ruler / master 行 / トラック外は None で no-op。
    //   いずれも選択トラックではなく「カーソルがあるトラック」を solo する。
    // text_input focus 中は gui_01 が単キーを抑制するので rename / 歌詞編集中は発火しない。
    if ui.take_shortcut("daw.toggle_track_solo") {
        let target_track_id = if is_pianoroll_active {
            if app.ui_ephemeral.audio_editor_clip.is_some() {
                None
            } else {
                app.pianoroll_target_clip()
                    .and_then(|c| app.song_doc.song().track_by_id(c.track_id))
                    .map(|t| t.id)
            }
        } else if app.ui_prefs.bottom_panel == Some(0) && pointer_in_bottom {
            app.ui_ephemeral.mixer_hovered_track
        } else {
            app.ui_ephemeral.arrange_hovered_track
        };
        if let Some(track_id) = target_track_id {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackSolo(track_id));
            }));
        }
    }

    // ----- Mute (Q) -----
    // 「選択中のものがあればそれらを、 無ければマウスカーソル直下のものを」 mute toggle。
    // 対象は文脈で決まる:
    // - piano roll active (= bottom panel が piano roll タブ + pointer が bottom 内、 ただし
    //   audio editor を開いていない MIDI 編集文脈): note を対象。 選択 note (`selected_notes`)
    //   があればそれら、 無ければカーソル直下 note (`pianoroll_hover_note`)。
    // - それ以外 (アレンジ / audio editor): clip を対象。 audio editor 中はその clip、
    //   そうでなければ選択 clip (`selected_clips`)、 無ければカーソル直下 clip
    //   (`arrangement_hover_clip`)。
    // toggle 方向は「対象が全部 muted なら unmute、 1 つでも非 muted なら全 mute」。
    // text_input フォーカス中は gui_01 が単キーを抑制する。
    // 内蔵チャンネルストリップ (docs/plan_channel_strip.md) が Q を先取りする:
    // カーソルが Comp / EQ の上にあればそのセクションのバイパスを切り替え、
    // 下の clip / note の mute へは落とさない。
    // r.md #105: 次にインスペクタのプラグインチェーン。 対象の決め方は clip と同型 —
    // カーソルがチェーン行の上なら「選択 device があればそれら、無ければその行」、
    // チェーン外でも **最後に選んだ面が device** (last-wins、 `edit_surface` と同じ
    // タイブレーカ) なら選択 device。 どちらでもなければ clip / note へ落とす。
    let mixer_active = app.ui_prefs.bottom_panel == Some(0) && pointer_in_bottom;
    if ui.take_shortcut("daw.toggle_mute") && !toggle_hovered_strip_section(app, ui, mixer_active) {
        let device_targets = q_device_targets(app);
        if !device_targets.is_empty() {
            let bypassed = !app.all_devices_bypassed(&device_targets);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetDevicesBypassed {
                    device_ids: device_targets,
                    bypassed,
                });
            }));
        } else if is_pianoroll_active && app.ui_ephemeral.audio_editor_clip.is_none() {
            // note 群は packed note id (`selected_notes` / `pianoroll_hover_note` は
            // 表示中全クリップに跨る packed id)。所属クリップは handler が decode するので、
            // ここで単一 anchor clip に縛らない (複数クリップ同時 mute を保つ)。
            let notes: Vec<u32> = if !app.selected_note_ids().is_empty() {
                app.selected_note_ids()
            } else {
                app.ui_ephemeral.pianoroll_hover_note.into_iter().collect()
            };
            if !notes.is_empty() {
                let new_muted = !app.all_notes_muted(&notes);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNotesMuted {
                        notes,
                        muted: new_muted,
                    });
                }));
            }
        } else {
            // 範囲が立っていれば **範囲操作** — 境界で分割して範囲部分だけをミュートする
            // (Live §6.9 "deactivates a selection of material"、
            // `docs/plan_range_selection.md` §8)。
            if !is_pianoroll_active && app.selection.time.is_some() {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.apply_mute_time_selection();
                }));
            } else {
                let targets: Vec<crate::app::ClipKey> = if is_pianoroll_active {
                    // audio waveform editor を開いている: その clip を mute。
                    app.ui_ephemeral.audio_editor_clip.into_iter().collect()
                } else {
                    app.ui_ephemeral.arrangement_hover_clip.into_iter().collect()
                };
                if !targets.is_empty() {
                    let new_muted = !app.all_clips_muted(&targets);
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetClipsMuted {
                            targets,
                            muted: new_muted,
                        });
                    }));
                }
            }
        }
    }

    // ----- r.md #87: ランチャーのキー操作 (Tab / 矢印 / Enter) -----
    // **note nudge より先に**呼ぶ (矢印の取り合いをここで決める)。 対象面が
    // `Notes` のときはランチャーが矢印を取らないので、両者は排他になる。
    crate::view::launcher_keys::dispatch_launcher_keys(app, ui, surface);
    // 範囲がアクティブなら矢印は範囲操作 (ノート nudge と面で排他)。
    dispatch_range_nudge(app, ui, surface);

    // ----- r.md #67: カーソルキーでノートを移動 / 伸縮 / 音程変更 -----
    dispatch_note_nudge(ui, surface);

    // ----- Ctrl+A: 文脈別全選択 (grill-me 2026-06-09) -----
    // マウス位置で対象を判定する (選択前なので Delete の「非空セット」判定は
    // 使えず pointer 位置で振り分け)。 下部パネル + audio editor 開: 全 event、
    // 下部パネル + piano roll: 全ノート、 それ以外 (アレンジ): 全クリップ。
    // (automation lane 上の「全ポイント → 全クリップ」段階拡大は後続で追加。)
    if ui.take_shortcut("select_all") {
        // 帯が今の操作対象なら **ランチャーのセルを全選択**する。落とすと
        // `SelectAllClips` に流れて曲全体の範囲が張られ、面が黙って範囲へ移る
        // (画面は変わらないのに、次の Delete がアレンジの全クリップを消す)。
        if crate::view::launcher_keys::select_all_cells_if_launcher(app, ui, surface) {
            // 帯が取った (選択は helper が積む)。
        } else if is_pianoroll_active && app.ui_ephemeral.audio_editor_clip.is_some() {
            let indices = app.all_audio_event_indices();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAudioEditorEventSelection(indices.clone()));
            }));
        } else if is_pianoroll_active {
            // 表示中クリップの全ノートを覆う範囲にする (`docs/plan_range_selection.md` §3.2)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.select_all_shown_notes();
            }));
        } else if let Some(lane) = app.ui_ephemeral.arrange_hovered_automation_lane {
            // automation lane 上: 段階拡大 (#071 で clip 段を追加)。
            //   1 回目 = lane の全ポイント
            //   2 回目 (全ポイント選択済 or ポイント無し) = lane の全 automation clip
            //   3 回目 (全 clip 選択済 or clip 無し)     = 曲全体の全 (通常) クリップ
            // tier2 で点とクリップが両方選択された状態になるが、 直近選択 (= clip) が
            // last-wins で copy/cut/delete の対象になる (edit_surface 参照)。
            let all_points = app.all_automation_points_in_lane(lane);
            let points_done = all_points.is_empty()
                || (app.selection.selected_automation_points.len() == all_points.len() && {
                    let cur: std::collections::HashSet<_> =
                        app.selection.selected_automation_points.iter().collect();
                    all_points.iter().all(|p| cur.contains(p))
                });
            if !points_done {
                let prev = app.selection.selected_automation_points.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAutomationPoints {
                        prev: prev.clone(),
                        next: all_points.clone(),
                    });
                }));
            } else {
                let all_clips = app.all_automation_clips_in_lane(lane);
                let clips_done = all_clips.is_empty()
                    || (app.selection.selected_automation_clips.len() == all_clips.len() && {
                        let cur: std::collections::HashSet<_> =
                            app.selection.selected_automation_clips.iter().collect();
                        all_clips.iter().all(|c| cur.contains(c))
                    });
                if !clips_done {
                    let prev = app.selection.selected_automation_clips.clone();
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SelectAutomationClips {
                            prev: prev.clone(),
                            next: all_clips.clone(),
                        });
                    }));
                } else {
                    ui.push_edit(Edit::mutate(|app: &mut AppData| {
                        app.handle_event(AppEvent::SelectAllClips);
                    }));
                }
            }
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::SelectAllClips);
            }));
        }
    }

    // ----- D / Alt+D: 編集面ごとの複製 (規則は `clipboard_ops` が持つ) -------
    clipboard_ops::dispatch_duplicate(app, ui, is_pianoroll_active);

    // ----- Clip rename (F2) -------------------------------------------------
    // 選択中 clip を inline rename (右クリックメニュー "Rename" と同経路)。
    // rename は単一対象なので selected_clip (= 末尾カーソル clip) を使う。
    // 選択 clip が無ければ no-op。 text_input focus 中は gui_01 が shortcut を
    // 抑制するので rename 編集中の F2 は発火しない。
    // clip が選択されていれば clip rename、 そうでなければ
    // (track header のみ選択 / フォーカス時) cursor track の名前を rename。
    // どちらも単一対象 (selected_clip = 末尾カーソル clip、 track は
    // cursor_track_index)。 track header の double-click が効かない場面でも
    // F2 で確実に rename を開始できる。
    if ui.take_shortcut("daw.rename_clip") {
        if let Some(target) = app.selected_clip_ref() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BeginRenameClip(target));
            }));
        } else if let Some(track_id) = app.cursor_track_id() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BeginRenameTrack(track_id));
            }));
        }
    }

    // ----- 共有を一括選択 (Shift+L) ------------------------------------------
    // ----- Automation: A キー (gui_01 #028 §7.3) ----------------------------
    // last-touched parameter (volume / pan / lane default knob 操作で更新) の
    // lane を所有 track に追加。 既存の lane は visible / enabled = true で
    // 復活、 該当 track の automation lane 群を即時展開。 `last_touched_param`
    // が None / stale な場合は handler 内で status_message を出して no-op。
    // gui_01 が text_input focus 中は自動 skip するので、 編集中に `a` を
    // 打っても発火しない。
    if ui.take_shortcut("daw.add_automation_from_last_touched") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AddAutomationFromLastTouched);
        }));
    }

    // ----- Split (E) / Glue (J) — Phase 1 PR7 -------------------------------
    // MIDI / Audio / Vocal すべての clip kind に対して動作する統合操作。
    // 詳細は `docs/plan_audio_clip.md` §3.3。
    if ui.take_shortcut("daw.split_clip_at_cursor") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SplitClipAtPlayhead { snap: true });
        }));
    }
    if ui.take_shortcut("daw.split_clip_at_cursor_no_snap") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SplitClipAtPlayhead { snap: false });
        }));
    }
    // `j` はビューで意味が分かれる — アレンジャー = 範囲を 1 クリップへ焼き込む /
    // ピアノロール = **Join Notes** (同じ音のノートを 1 本に結合)。
    // Live も `Ctrl+J` を Consolidate / Join Notes に振り分けている
    // (`docs/plan_range_selection.md` §7.4)。
    if ui.take_shortcut("daw.glue_selected_clips") {
        if is_pianoroll_active && app.ui_ephemeral.audio_editor_clip.is_none() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.action_join_selected_notes();
            }));
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::GlueSelectedClips);
            }));
        }
    }

    // ----- Help -----
    if ui.take_shortcut("daw.toggle_help") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleHelp)
        }));
    }
    // ----- ビデオプレビューウィンドウ (F12) -----
    if ui.take_shortcut("daw.toggle_preview_window") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::TogglePreviewWindow)
        }));
    }
    // Phase 2 PR-D 段階 1: Audio Editor 開いているとき Ctrl+D で
    // 選択中 event を Duplicate。 audio_editor_clip is None のときは
    // 消費しない (= 既存 D / Alt+D の clip duplicate と紛らわしくない
    // よう、 Audio Editor 内限定の shortcut として gate する)。
    //
    // r.md #87: ランチャーのセルを選んでいるときは **セルの複製が勝つ**
    // (計画書 §3.5 の「Ctrl+D で複製」)。 同じキーに 2 つ目の binding を
    // 宣言できない (`ShortcutMap::matches` は先勝ち) ので、take は 1 度だけ行い
    // 行き先をここで振り分ける。
    let dup_pressed = ui.take_shortcut("daw.duplicate_audio_event");
    if !crate::view::launcher_keys::duplicate_cells_if_launcher(app, ui, surface, dup_pressed)
        && dup_pressed
        && app.ui_ephemeral.audio_editor_clip.is_some()
    {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateAudioEditorEvent)
        }));
    }
    // PR-D 段階 2: Audio Editor 内 event 選択 navigation (Ctrl+] / Ctrl+[)。
    // 現選択 idx を ±1 wrap-around で移動。 audio_editor が開いてないと
    // 無効、 events が空なら no-op。
    if app.ui_ephemeral.audio_editor_clip.is_some() && ui.take_shortcut("daw.next_audio_event") {
        let next = app.next_audio_editor_event_idx(1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(next))
        }));
    }
    if app.ui_ephemeral.audio_editor_clip.is_some() && ui.take_shortcut("daw.prev_audio_event") {
        let prev = app.next_audio_editor_event_idx(-1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(prev))
        }));
    }
    // B12 (r.md #8): 選択オーディオクリップを auto-warp (transient を拍グリッドに整列)。
    // arrangement / audio editor どちらの選択でも可 (handler が audio 以外は no-op)。
    if ui.take_shortcut("daw.auto_warp_clip") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AutoWarpSelectedClip)
        }));
    }
    // modal が開いている間は escape を消費しない (modal 側で close する)。
    // 優先度: track rename → clip rename → Audio Editor close → CloseHelp。
    // rename (track header / clip rect の inline text_input) と Audio Editor
    // (bottom panel) は同時には開かないので順番でも実用 OK。
    // plugin picker / send picker が開いている間は escape を消費しない
    // (= track_picker / plugin_picker の modal が close_on_escape で閉じる)。
    //
    // piano_roll の歌詞 inline 編集中も escape を消費しない。 この
    // `dispatch_shortcuts` は `bottom_panel::draw` (= piano_roll widget) より前に走るため、
    // ここで `take_shortcut("escape")` を消費すると widget の歌詞キャンセルハンドラ
    // (piano_roll.rs) に escape が届かず、 代わりに下の選択解除 branch が走って編集中
    // clip が deselect → MIDI エディタが空表示になってしまう。 編集中はここで消費せず
    // widget に委ねる (widget が `take_shortcut("escape")` で歌詞編集を cancel する)。
    // 条件は piano_roll widget が実際に走る状況 (Piano Roll タブ + Audio Editor 非表示) に
    // 一致させる (`app.ui_ephemeral.piano_roll_lyric_editing` 単独だと stale-true で誤委譲しうる)。
    let pianoroll_lyric_editing =
        app.ui_prefs.bottom_panel == Some(1) && app.ui_ephemeral.audio_editor_clip.is_none() && app.ui_ephemeral.piano_roll_lyric_editing;
    if !app.ui_ephemeral.is_plugin_picker_open
        && !app.ui_ephemeral.is_font_picker_open
        && app.ui_ephemeral.send_picker.is_none()
        && !pianoroll_lyric_editing
        && ui.take_shortcut("escape")
    {
        if app.ui_ephemeral.track_rename_id.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameTrack)
            }));
        } else if app.ui_ephemeral.section_rename_id.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameSection)
            }));
        } else if app.ui_ephemeral.clip_rename.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameClip)
            }));
        } else if app.ui_ephemeral.audio_editor_clip.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseAudioEditor)
            }));
        } else if app.ui_ephemeral.armed_mod_source.is_some() {
            // r.md #78: 変調ソースの待受 (◉) を Esc で取り消す。 待受中は
            // 「次に触ったツマミ」に繋がるモードなので、 window を閉じるより先に
            // モードを抜ける。 ラックの ◉ ボタンはカーソルトラック所有のソース
            // しか出ず、 トラックを移ると解除する手段が無くなるため必須。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::SetArmedModSource(None));
            }));
        } else if app.ui_ephemeral.resource_panel_open {
            // resource monitor (r.md #3): 詳細パネルが開いていれば Esc で閉じる
            // (rename / audio editor の後、 選択解除より優先)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleResourcePanel)
            }));
        } else if app.ui_prefs.loudness_report_open {
            // r.md #54: レポート window が開いていれば Esc で閉じる (同順)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleLoudnessReport)
            }));
        } else if app.ui_prefs.undo_history_open {
            // r.md #29: 編集履歴 window が開いていれば Esc で閉じる
            // (resource panel と同順、 選択解除より優先)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleUndoHistory)
            }));
        } else if app.ui_prefs.settings_open {
            // r.md #48: 設定 window が開いていれば Esc で閉じる (同順)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSettings)
            }));
        } else if app.selection.time.is_some()
            || !app.selection.selected_launcher_cells.is_empty()
            || !app.selected_note_ids().is_empty()
            || !app.selection.selected_automation_points.is_empty()
            || !app.selection.selected_automation_clips.is_empty()
        {
            // Escape で選択解除 (clip / note / automation point / clip)。
            // 死蔵だった ClearSelection / ClearNoteSelection を生かす。
            // audio editor は上の分岐で先に閉じるので、 ここに来る時点で
            // audio event 選択は対象外 (close 時に clear 済)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ClearSelection);
                app.handle_event(AppEvent::ClearNoteSelection);
                app.selection.selected_automation_points.clear();
                app.selection.selected_automation_clips.clear();
            }));
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseHelp)
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use common::protocol::{AudioCommand, PluginCommand};
    use daw_ui_core::{FrameInput, UiHost};
    use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey};
    use daw_ui_renderer::Scene;
    use tokio::sync::mpsc;

    use crate::app::ClipKey;
    use crate::dispatcher::{
        BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
    };

    fn build_app() -> AppData {
        let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
        let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        AppData::new(
            audio_tx,
            plugin_tx,
            None,
            None,
            event_dispatcher,
            job_dispatcher,
            None,
            None,
            common::audio_bridge::DEFAULT_SAMPLE_RATE,
        )
    }

    /// Esc 押下 1 フレームを `dispatch_shortcuts` に通し、 push された Edit を app に適用する。
    /// `UiHost::no_redraw()` の default binding は "escape" = Escape を含むので、
    /// Escape KeyEvent を渡すと frame 頭で "escape" shortcut が pending に積まれる。
    fn dispatch_escape(app: &mut AppData) {
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1280, height: 720 };
        let bottom_rect = Rect { x: 0.0, y: 400.0, w: 1280.0, h: 320.0 };
        let input = FrameInput {
            keyboard: vec![KeyEvent {
                state: ElementState::Pressed,
                text: None,
                physical_key: PhysicalKey::Escape, repeat: false
            }],
            ..FrameInput::default()
        };
        let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
            dispatch_shortcuts(app, ui, bottom_rect);
        });
        for e in edits {
            e.apply(app);
        }
    }

    /// r.md #67: 矢印キー 1 押しを 1 フレーム流し、`dispatch_shortcuts` が push した
    /// Edit を app に適用する。`daw_shortcut_map` を使うので **本番と同じキー定義**
    /// (typing_only / repeatable 含む) を通る。`repeat` 件数で auto-repeat を模す。
    fn dispatch_arrow(app: &mut AppData, physical_key: PhysicalKey, mods: Modifiers, count: usize) {
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        *host.shortcut_map_mut() = crate::view::shortcuts::daw_shortcut_map();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1280, height: 720 };
        let bottom_rect = Rect { x: 0.0, y: 400.0, w: 1280.0, h: 320.0 };
        let keyboard: Vec<KeyEvent> = (0..count)
            .map(|i| KeyEvent {
                state: ElementState::Pressed,
                text: None,
                physical_key,
                // 2 件目以降を OS auto-repeat とみなす (repeatable 宣言の検証)。
                repeat: i > 0,
            })
            .collect();
        let input = FrameInput {
            keyboard,
            pointer: daw_ui_core::PointerFrame { modifiers: mods, ..Default::default() },
            ..FrameInput::default()
        };
        let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
            dispatch_shortcuts(app, ui, bottom_rect);
        });
        for e in edits {
            e.apply(app);
        }
    }

    /// 素キー 1 押しを 1 フレーム流し、`dispatch_shortcuts` が push した Edit を app に
    /// 適用する。`daw_shortcut_map` (本番と同じ定義) を通る。typing 中の素キー抑止
    /// (`bare_char_key`) は daw-ui core 側のテストが担う。
    fn dispatch_char_key(app: &mut AppData, ch: char) {
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        *host.shortcut_map_mut() = crate::view::shortcuts::daw_shortcut_map();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1280, height: 720 };
        let bottom_rect = Rect { x: 0.0, y: 400.0, w: 1280.0, h: 320.0 };
        let input = FrameInput {
            keyboard: vec![KeyEvent {
                state: ElementState::Pressed,
                text: Some(ch.to_string()),
                physical_key: PhysicalKey::Char(ch.to_ascii_uppercase()),
                repeat: false,
            }],
            ..FrameInput::default()
        };
        let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
            dispatch_shortcuts(app, ui, bottom_rect);
        });
        for e in edits {
            e.apply(app);
        }
    }

    /// r.md #96: `B` は Mixer のトグル。閉 → Mixer → 閉、Piano Roll タブ → Mixer。
    #[test]
    fn b_toggles_mixer_panel() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = None;
        dispatch_char_key(&mut app, 'b');
        assert_eq!(app.ui_prefs.bottom_panel, Some(0), "閉じていれば Mixer で開く");
        dispatch_char_key(&mut app, 'b');
        assert_eq!(app.ui_prefs.bottom_panel, None, "Mixer が見えていれば閉じる");
        app.ui_prefs.bottom_panel = Some(1);
        dispatch_char_key(&mut app, 'b');
        assert_eq!(app.ui_prefs.bottom_panel, Some(0), "Piano Roll タブなら Mixer へ切替 (閉じない)");
    }

    /// note を 1 つ持つ MIDI クリップを 1 本置き、その note を選択した状態にする。
    fn app_with_selected_note(start_beat: f64) -> AppData {
        let mut app = build_app();
        app.edit_song(move |song| {
            song.tracks.clear();
            let cid = song.alloc_content_id();
            song.clip_contents.insert(
                cid,
                common::model::ClipContent::Midi(common::model::MidiContent {
                    notes: vec![common::model::Note {
                        pitch: 60,
                        start_beat,
                        duration_beats: 1.0,
                        velocity: 100,
                        ..common::model::Note::default()
                    }],
                    ..common::model::MidiContent::default()
                }),
            );
            song.tracks.push(crate::app::track_with(|t| {
                t.id = 1;
                t.clips = vec![common::model::Clip {
                    id: 10,
                    content_id: cid,
                    start_beat: 0.0,
                    length_beats: 32.0,
                    ..common::model::Clip::default()
                }];
            }));
        });
        let key = common::model::ClipKey { track_id: 1, clip_id: 10 };
        app.set_clip_selection(vec![key]);
        app.ui_prefs.pianoroll_snap_enabled = true;
        app.ui_prefs.pianoroll_snap_choice = crate::view::snap::CHOICE_PIANOROLL_DEFAULT; // 1/16
        app.handle_event(AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 0)]));
        app
    }

    fn note_start(app: &AppData) -> f64 {
        let song = app.song_doc.song();
        song.clip_notes(&song.tracks[0].clips[0])[0].start_beat
    }

    /// キー → shortcut → AppEvent の配線 (矢印が本番のキー定義で届き、ノートが動く)。
    #[test]
    fn arrow_key_nudges_the_selected_note() {
        let mut app = app_with_selected_note(4.0);
        dispatch_arrow(&mut app, PhysicalKey::ArrowRight, Modifiers::empty(), 1);
        assert!((note_start(&app) - 4.25).abs() < 1e-9, "→ で 1/16 拍 右へ: got {}", note_start(&app));
        let shift = Modifiers { shift: true, ..Modifiers::empty() };
        dispatch_arrow(&mut app, PhysicalKey::ArrowLeft, shift, 1);
        assert!((note_start(&app) - 0.25).abs() < 1e-9, "Shift+← で 1 小節 左へ: got {}", note_start(&app));
    }

    /// 押しっぱなし (auto-repeat) が **回数ぶん** 適用される。1 回しか消費しないと
    /// 移動量がフレームレート次第で目減りする (`take_shortcut_count` の検証)。
    #[test]
    fn held_arrow_applies_every_repeat_in_the_frame() {
        let mut app = app_with_selected_note(4.0);
        dispatch_arrow(&mut app, PhysicalKey::ArrowRight, Modifiers::empty(), 3);
        assert!(
            (note_start(&app) - 4.75).abs() < 1e-9,
            "1 フレームに 3 回届いたら 3 ステップ動く: got {}",
            note_start(&app)
        );
    }

    /// ノートを選んでいなければ矢印は何もしない (ユーザー決定: 再生位置移動等に割り当てない)。
    #[test]
    fn arrow_does_nothing_without_a_note_selection() {
        let mut app = app_with_selected_note(4.0);
        app.handle_event(AppEvent::ClearNoteSelection);
        app.selection.last_edit_select = None;
        dispatch_arrow(&mut app, PhysicalKey::ArrowRight, Modifiers::empty(), 1);
        assert!((note_start(&app) - 4.0).abs() < 1e-9, "選択が無ければ動かない");
    }

    /// piano_roll の歌詞 inline 編集中の Esc は global の `dispatch_shortcuts`
    /// で消費されず piano_roll widget に委ねられる。 ここで消費 (選択解除) されると
    /// 編集中 clip が deselect → MIDI エディタが空表示になる回帰を防ぐ。
    #[test]
    fn escape_during_lyric_edit_is_not_consumed_by_global_dispatch() {
        // 選択は範囲からの導出なので、ダミー id ではなく **実在するノート** を選ぶ。
        let mut app = app_with_selected_note(4.0);
        app.ui_prefs.bottom_panel = Some(1); // Piano Roll タブ
        app.ui_ephemeral.piano_roll_lyric_editing = true; // 歌詞編集中
        dispatch_escape(&mut app);
        assert_eq!(
            app.selected_note_ids(),
            vec![AppData::pack_note_id(0, 0)],
            "歌詞編集中の Esc は global dispatch で消費されず note 選択は維持される",
        );
    }

    /// 対の保証: 歌詞編集中でなければ Esc は従来どおり global dispatch が消費して
    /// note 選択を解除する (既存挙動を壊していない)。
    #[test]
    fn escape_clears_note_selection_when_not_lyric_editing() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = Some(1);
        app.ui_ephemeral.piano_roll_lyric_editing = false; // 非編集
        app.handle_event(AppEvent::SetNoteSelection(vec![1]));
        dispatch_escape(&mut app);
        assert!(
            app.selected_note_ids().is_empty(),
            "非編集時の Esc は従来どおり note 選択を解除する",
        );
    }

    /// Audio Editor が開いている間は piano_roll widget が走らないので、 歌詞フラグが
    /// stale-true でも委譲してはならない (= Esc が宙に浮く)。 ガードの
    /// `audio_editor_clip.is_none()` 項が効いて、 Esc は従来どおり Audio Editor を閉じる。
    #[test]
    fn escape_closes_audio_editor_even_if_lyric_flag_is_stale() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = Some(1);
        app.ui_ephemeral.piano_roll_lyric_editing = true; // stale-true を想定
        app.ui_ephemeral.audio_editor_clip = Some(ClipKey { track_id: 0, clip_id: 0 });
        dispatch_escape(&mut app);
        assert!(
            app.ui_ephemeral.audio_editor_clip.is_none(),
            "Audio Editor 表示中の Esc は歌詞フラグに関わらず Audio Editor を閉じる",
        );
    }

    /// resource monitor (r.md #3) 詳細パネルが開いている間の Esc はパネルを
    /// 閉じ、 選択は維持する (audio editor の後、 選択解除より優先 = 2 段階で
    /// 次の Esc が選択解除に回る)。
    #[test]
    fn escape_closes_resource_panel_before_clearing_selection() {
        let mut app = app_with_selected_note(4.0);
        app.ui_ephemeral.resource_panel_open = true;
        dispatch_escape(&mut app);
        assert!(!app.ui_ephemeral.resource_panel_open, "Esc は開いている詳細パネルを閉じる");
        assert_eq!(
            app.selected_note_ids(),
            vec![AppData::pack_note_id(0, 0)],
            "パネルを閉じる Esc は選択を解除しない",
        );
    }

    /// r.md #48: 設定 window が開いている間の Esc は window を閉じ、選択は維持する
    /// (編集履歴 window / resource panel と同順)。 ここが抜けると Esc で閉じられず、
    /// 代わりに選択が消える。
    #[test]
    fn escape_closes_settings_window_before_clearing_selection() {
        let mut app = app_with_selected_note(4.0);
        app.ui_prefs.settings_open = true;
        dispatch_escape(&mut app);
        assert!(!app.ui_prefs.settings_open, "Esc は開いている設定 window を閉じる");
        assert_eq!(
            app.selected_note_ids(),
            vec![AppData::pack_note_id(0, 0)],
            "設定 window を閉じる Esc は選択を解除しない",
        );
    }
}
