//! 設定 window の「オーディオ」ページ (r.md #156): デバイスの選択と、先読み。
//!
//! - 並びは上から ドライバーの種類 → デバイス → 出力 ch → 入力 ch → サンプルレート → バッファサイズ →
//!   レイテンシ → 先読み。
//! - **選んだ瞬間に切り替わる** (`AudioDeviceEvent::SetConfig`。「適用」ボタンは無い)。
//! - 一覧に出すのは engine が報告したもの (`app.audio_device.devices` / `info`)。値は実際に開いている
//!   デバイスのもの。選択肢の作り方は [`super::audio_choices`]。
//! - デバイスの行の数は、種類を替えても・開くのに失敗しても変わらない (下の項目が動かない)。

use common::audio_device::{AudioDeviceConfig, AudioDriverKind, synthetic};
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use super::audio_choices::{self as choices, NONE_SELECTED};
use super::page::{DropdownId, Page};
use crate::app::{AppData, AppEvent};
use crate::event_audio_device::AudioDeviceEvent;

/// 「コントロールパネル」ボタンの幅 (px)。
const PANEL_BUTTON_W: f32 = 160.0;
/// 入力 ch の範囲の、始まりと終わりの間 (「〜」を置く幅、px)。
const RANGE_GAP: f32 = 24.0;
/// 「〜」の字の幅 (全角 1 文字、px)。
const TILDE_W: f32 = 12.0;

pub(super) fn draw(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    pg.heading(ui, "device", "デバイス", true);
    draw_driver_kind(ui, pg);
    match pg.app.audio_device.config.driver {
        AudioDriverKind::System => {
            draw_system_output(ui, pg);
            draw_system_input(ui, pg);
        }
        AudioDriverKind::Asio => {
            draw_asio_driver(ui, pg);
            draw_control_panel(ui, pg);
        }
        AudioDriverKind::Synthetic => draw_synthetic_device(ui, pg),
    }
    draw_notice(ui, pg);
    draw_output_pair(ui, pg);
    draw_input_range(ui, pg);

    pg.heading(ui, "format", "サンプルレートとバッファ", false);
    draw_sample_rate(ui, pg);
    draw_buffer_size(ui, pg);
    draw_latency(ui, pg);

    draw_render_ahead(ui, pg);
}

/// 設定を 1 項目だけ変えたものへ切り替える (今と同じなら handler が何もしない)。
fn switch_to(ui: &mut Ui<'_, AppData>, pg: &Page<'_>, change: impl FnOnce(&mut AudioDeviceConfig)) {
    let mut config = pg.app.audio_device.config.clone();
    change(&mut config);
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::AudioDevice(AudioDeviceEvent::SetConfig(config)));
    }));
}

fn draw_driver_kind(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let rect = pg.row(ui, "driver_kind", "ドライバーの種類");
    let c = choices::driver_kinds(pg.app.audio_device.config.driver);
    if let Some(i) = pg.dropdown(ui, DropdownId::DriverKind, rect, &c.labels, c.selected) {
        let kind = c.values[i];
        switch_to(ui, pg, |config| config.set_driver(kind));
    }
}

fn draw_system_output(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let rect = pg.row(ui, "system_output", "出力デバイス");
    let mut c = choices::system_outputs(dev.devices.as_ref(), dev.config.system_output.as_ref());
    if let Some(i) = pg.dropdown(ui, DropdownId::SystemOutput, rect, &c.labels, c.selected) {
        let device = c.values.swap_remove(i);
        switch_to(ui, pg, |config| config.set_system_output(device));
    }
}

fn draw_system_input(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let rect = pg.row(ui, "system_input", "入力デバイス");
    let mut c = choices::system_inputs(dev.devices.as_ref(), &dev.config.system_input);
    if let Some(i) = pg.dropdown(ui, DropdownId::SystemInput, rect, &c.labels, c.selected) {
        let device = c.values.swap_remove(i);
        switch_to(ui, pg, |config| config.set_system_input(device));
    }
}

fn draw_asio_driver(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let rect = pg.row(ui, "asio_driver", "ドライバー");
    let mut c = choices::asio_drivers(dev.devices.as_ref(), dev.config.asio_driver.as_ref());
    if c.labels.is_empty() {
        // 開いても空の一覧しか出ないので、dropdown は置かない。
        let text = if dev.devices.is_some() { "ASIO ドライバーがありません" } else { "—" };
        pg.value(ui, "asio_driver", rect, text, true);
        return;
    }
    if let Some(i) = pg.dropdown(ui, DropdownId::AsioDriver, rect, &c.labels, c.selected) {
        let driver = c.values.swap_remove(i);
        switch_to(ui, pg, |config| config.set_asio_driver(Some(driver)));
    }
    if c.selected == NONE_SELECTED {
        // まだ選んでいない: 空の本体に、何をする欄かを薄く出す。
        let inner = Rect { x: rect.x + 8.0, w: (rect.w - 28.0).max(0.0), ..rect };
        pg.value(ui, "asio_driver_placeholder", inner, "選んでください", true);
    }
}

/// ドライバーの設定画面を開くボタン。ドライバーを開けていない間は行だけ取って何も置かない
/// (行の数を変えない)。
fn draw_control_panel(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let rect = pg.row(ui, "asio_panel", "");
    if !pg.app.audio_device.info.as_ref().is_some_and(|info| info.has_control_panel) {
        return;
    }
    let button = Rect { w: rect.w.min(PANEL_BUTTON_W), ..rect };
    ui.button_at("settings_asio_panel", "コントロールパネル", button, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AudioDevice(AudioDeviceEvent::OpenControlPanel));
        })
    });
}

/// 合成のデバイス (検査用。script から選ばれたときだけ出る)。
fn draw_synthetic_device(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let rect = pg.row(ui, "synthetic", "デバイス");
    let id = pg.app.audio_device.config.synthetic_device.as_deref().unwrap_or(synthetic::RAMP);
    pg.value(ui, "synthetic", rect, id, false);
    // OS 標準 / ASIO と行の数を揃える。
    pg.row(ui, "synthetic_spacer", "");
}

/// うまく行っていないときの知らせ (開けずに OS 標準の既定デバイスへ戻した、など)。
/// デバイスの行のすぐ下 — 選んだ結果が、選んだ場所の近くに出る。場所はいつも取ってあるので、
/// 出たり消えたりしても下の項目は動かない。
fn draw_notice(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let p = &pg.app.theme.core;
    let (text, color) = match (&dev.error, &dev.info) {
        (Some(error), _) => (format!("オーディオデバイスを開けませんでした: {error}"), p.text_error),
        (None, None) => ("オーディオエンジンと繋がっていません".to_string(), p.text_error),
        (None, Some(info)) => match &info.fallback_reason {
            // 理由と、**実際に開いているデバイス** (`info`)。設定どおりに開けなかったときに何で動くか
            // (OS 標準の既定デバイスへ戻した / 入力だけ無し / 音の出ないデバイス) は engine が決めるので、
            // ここでは決め付けず、開いているものの名前をそのまま出す。
            Some(reason) if info.output_name.is_empty() => (reason.clone(), p.text_error),
            // 選んでいないのに合成のデバイス (実機を開かない) で動いている = どの実機も開けなかった。
            Some(reason)
                if info.driver == AudioDriverKind::Synthetic
                    && dev.config.driver != AudioDriverKind::Synthetic =>
            {
                (format!("{reason} — 音は出ません"), p.text_error)
            }
            Some(reason) => (format!("{reason} — 「{}」で動いています", info.output_name), p.text_error),
            None if dev.listing && dev.devices.is_none() => {
                ("デバイスの一覧を取得しています…".to_string(), p.text_faint)
            }
            None => (String::new(), p.text_faint),
        },
    };
    pg.notice(ui, "audio_problem", &text, color);
}

fn draw_output_pair(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let rect = pg.row(ui, "output_pair", "出力チャンネル");
    let c = pg.app.audio_device.info.as_ref().map(choices::output_pairs);
    let Some(c) = c.filter(|c| !c.labels.is_empty()) else {
        pg.value(ui, "output_pair", rect, "—", true);
        return;
    };
    if let Some(i) = pg.dropdown(ui, DropdownId::OutputPair, rect, &c.labels, c.selected) {
        let first = c.values[i];
        switch_to(ui, pg, |config| config.output_first_channel = Some(first));
    }
}

/// 使う入力 ch の範囲: 「始まり 〜 終わり」。
fn draw_input_range(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let rect = pg.row(ui, "input_range", "入力チャンネル");
    let Some(c) = pg.app.audio_device.info.as_ref().and_then(choices::input_range) else {
        pg.value(ui, "input_range", rect, "なし", true);
        return;
    };
    let w = ((rect.w - RANGE_GAP) * 0.5).max(0.0);
    let first_rect = Rect { w, ..rect };
    if let Some(i) = pg.dropdown(ui, DropdownId::InputFirst, first_rect, &c.first.labels, c.first.selected) {
        let range = c.first.values[i];
        switch_to(ui, pg, |config| config.input_channels = Some(range));
    }
    let Some(last) = c.last else {
        return;
    };
    // 「〜」は全角 1 文字ぶんの幅で、間の中央に置く。
    let tilde = Rect { x: rect.x + w + (RANGE_GAP - TILDE_W) * 0.5, w: RANGE_GAP, ..rect };
    pg.value(ui, "input_range_tilde", tilde, "〜", true);
    let last_rect = Rect { x: rect.x + w + RANGE_GAP, w, ..rect };
    if let Some(i) = pg.dropdown(ui, DropdownId::InputLast, last_rect, &last.labels, last.selected) {
        let range = last.values[i];
        switch_to(ui, pg, |config| config.input_channels = Some(range));
    }
}

fn draw_sample_rate(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let rect = pg.row(ui, "sample_rate", "サンプルレート");
    let Some(info) = &dev.info else {
        pg.value(ui, "sample_rate", rect, "—", true);
        return;
    };
    if info.sample_rates.is_empty() {
        // ホストからは選べない。OS 標準のレートは OS のサウンド設定が決める。
        let text = if info.driver == AudioDriverKind::System {
            let os = if cfg!(windows) { "Windows" } else { "OS" };
            format!("{} ({os} の設定)", hz(info.sample_rate))
        } else {
            hz(info.sample_rate)
        };
        pg.value(ui, "sample_rate", rect, &text, false);
        return;
    }
    let c = choices::device_default_or(&info.sample_rates, info.sample_rate, dev.config.sample_rate, hz);
    if let Some(i) = pg.dropdown(ui, DropdownId::SampleRate, rect, &c.labels, c.selected) {
        let rate = c.values[i];
        switch_to(ui, pg, |config| config.sample_rate = rate);
    }
}

fn draw_buffer_size(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let dev = &pg.app.audio_device;
    let rect = pg.row(ui, "buffer_size", "バッファサイズ");
    let Some(info) = &dev.info else {
        pg.value(ui, "buffer_size", rect, "—", true);
        return;
    };
    if info.buffer_sizes.is_empty() {
        // ホストからは選べない。ASIO はドライバーのコントロールパネルで変える。
        let text = if info.has_control_panel {
            format!("{} (パネルで変更)", samples(info.buffer_frames))
        } else {
            samples(info.buffer_frames)
        };
        pg.value(ui, "buffer_size", rect, &text, false);
        return;
    }
    let c = choices::device_default_or(&info.buffer_sizes, info.buffer_frames, dev.config.buffer_frames, samples);
    if let Some(i) = pg.dropdown(ui, DropdownId::BufferSize, rect, &c.labels, c.selected) {
        let frames = c.values[i];
        switch_to(ui, pg, |config| config.buffer_frames = frames);
    }
}

/// 入力 / 出力のレイテンシ (読むだけ)。デバイスの申告値で、先読みの遅れは含まない。
fn draw_latency(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let info = pg.app.audio_device.info.as_ref();
    let input = info.filter(|info| info.active_inputs.count > 0 && !info.input_channels.is_empty());
    let rect = pg.row(ui, "latency_in", "入力レイテンシ");
    match input {
        Some(info) => {
            let text = latency(info.input_latency_frames, info.sample_rate);
            pg.value(ui, "latency_in", rect, &text, false);
        }
        None => pg.value(ui, "latency_in", rect, "—", true),
    }
    let rect = pg.row(ui, "latency_out", "出力レイテンシ");
    match info {
        Some(info) => {
            let text = latency(info.output_latency_frames, info.sample_rate);
            pg.value(ui, "latency_out", rect, &text, false);
        }
        None => pg.value(ui, "latency_out", rect, "—", true),
    }
}

fn hz(rate: u32) -> String {
    format!("{rate} Hz")
}

fn samples(frames: u32) -> String {
    format!("{frames} サンプル")
}

/// 「5.3 ms (256 サンプル)」。
fn latency(frames: u32, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return samples(frames);
    }
    let ms = f64::from(frames) * 1000.0 / f64::from(sample_rate);
    format!("{ms:.1} ms ({})", samples(frames))
}

/// 出力の先読みの on / off (`UiPrefs::render_ahead`)。
fn draw_render_ahead(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let app = pg.app;
    pg.heading(ui, "render_ahead", "先読み", false);
    let rect = pg.full_row();
    ui.checkbox_at(
        "settings_render_ahead",
        rect,
        app.ui_prefs.render_ahead,
        "先読みで音切れを防ぐ",
        |on| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetRenderAhead(on))),
    );
    pg.hint(
        ui,
        "render_ahead",
        "2 周期分を先に描いておきます。操作への反応が 2 周期分遅れます。切り替えは停止中に効きます。",
    );
}
