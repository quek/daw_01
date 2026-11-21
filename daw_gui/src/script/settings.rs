//! 設定 window の script API (r.md #156): 開閉 (`daw.appSetSettingsOpen`) / 左のカテゴリ
//! (`daw.appSetSettingsCategory`) / ページの項目 (`daw.appSetTheme` / `daw.appSetVoicevoxChunkSecs`) と、
//! 今の状態 (`daw.settingsJson`)。どれも人の操作と同じ `AppEvent` を通す。
//!
//! 先読みは `daw.appSetRenderAhead`、オーディオデバイスの項目はデバイスの script API が持つ。

use boa_engine::{Context, JsArgs, JsResult, JsString, JsValue};

use super::{js_native, with_app};
use crate::app::AppEvent;
use crate::state::SettingsCategory;

/// `daw.appSetSettingsOpen(open)` — 設定 window を開く / 閉じる (メニューの「設定...」と同じ
/// `AppEvent::ToggleSettings`。もう望みの状態なら何もしない)。
pub(super) fn daw_app_set_settings_open(_this: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let open = args.get_or_undefined(0).to_boolean();
    with_app(move |app, _side, _io| {
        if app.ui_prefs.settings_open != open {
            app.handle_event(AppEvent::ToggleSettings);
        }
    });
    Ok(JsValue::undefined())
}

/// `daw.appSetSettingsCategory(id)` — 左のカテゴリを選ぶ (`"audio"` / `"voicevox"` / `"theme"`)。
/// 一覧の行の click と同じ `AppEvent::SetSettingsCategory`。
pub(super) fn daw_app_set_settings_category(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let id = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let Some(category) = SettingsCategory::from_id(&id) else {
        return Err(js_native(format!("appSetSettingsCategory: unknown category {id:?}")));
    };
    with_app(move |app, _side, _io| app.handle_event(AppEvent::SetSettingsCategory(category)));
    Ok(JsValue::undefined())
}

/// `daw.appSetTheme(id)` — 「テーマ」ページの行の click と同じ `AppEvent::SetTheme`。
pub(super) fn daw_app_set_theme(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    with_app(move |app, _side, _io| app.handle_event(AppEvent::SetTheme(id)));
    Ok(JsValue::undefined())
}

/// `daw.appSetVoicevoxChunkSecs(secs)` — 「VOICEVOX」ページの数値欄を確定したときと同じ
/// `AppEvent::SetVoicevoxChunkSecs { commit: true }`。
pub(super) fn daw_app_set_voicevox_chunk_secs(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let secs = args.get_or_undefined(0).to_number(ctx)? as f32;
    with_app(move |app, _side, _io| {
        app.handle_event(AppEvent::SetVoicevoxChunkSecs { secs, commit: true });
    });
    Ok(JsValue::undefined())
}

/// `daw.settingsJson()` — 設定 window と各ページの項目の今の状態と、`app_config.json` に実際に
/// 保存されている内容 (`saved`。保存先が無い起動では `null`)。
///
/// `{open, category, renderAhead, voicevoxChunkSecs, theme, themes: [id],
///   audioDevice: {config, listing, listed, connected, problem}, saved}`
///
/// `audioDevice` は「オーディオ」ページが読む state のうち、設定 (`config`) と、一覧・接続・失敗の
/// 有無だけ。一覧と開いているデバイスの中身は、デバイスの script API で読む。
pub(super) fn daw_settings_json(_this: &JsValue, _args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    let json = with_app(move |app, _side, _io| {
        let dev = &app.audio_device;
        let saved = app.ui_prefs.app_dirs.as_ref().and_then(|dirs| {
            let text = std::fs::read_to_string(dirs.app_config()).ok()?;
            serde_json::from_str::<serde_json::Value>(&text).ok()
        });
        serde_json::json!({
            "open": app.ui_prefs.settings_open,
            "category": app.ui_prefs.settings_category.id(),
            "renderAhead": app.ui_prefs.render_ahead,
            "voicevoxChunkSecs": app.ui_prefs.voicevox_chunk_secs,
            "theme": app.theme.id,
            "themes": app.ui_ephemeral.available_themes.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            "audioDevice": {
                "config": dev.config,
                "listing": dev.listing,
                "listed": dev.devices.is_some(),
                "connected": dev.info.is_some(),
                "problem": dev.problem(),
            },
            "saved": saved,
        })
        .to_string()
    });
    Ok(JsString::from(json).into())
}
