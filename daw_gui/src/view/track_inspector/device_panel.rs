//! インスペクタのチェーン行を **展開したとき** に出る param パネル本体。
//!
//! `mod.rs` の `reorderable_list_expandable` に渡す expansion クロージャが
//! ここ 1 本を呼ぶ。 分けてあるのは god file budget (不変条件 9、3,000 行) の
//! ため — 元は `draw` の中に約 1,300 行のクロージャとして埋まっていた。
//! contract は `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点) -> 次の y`」。
use super::*;

/// 開いたデバイスの param パネルを描き、消費後の `y` を返す。
/// 各セクションは自分の gate (`voicevox_param_panel_open()` 等) で選別する。
pub(super) fn draw_device_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    exp_rect: Rect,
) -> f32 {
    let p = &app.theme.core;
    #[allow(unused_mut)]
    let mut y = exp_rect.y;
                // ====== 開いたデバイスの param パネル本体 (各 section gate が選別) ======

    // ---- Group Transform section (`docs/plan_tachie_group_transform.md` §5.5) --
    // cursor track が visual group のとき、立ち絵全体の 2D affine + opacity を
    // 数値編集 + per-param「A」automate トグルで expose。image inspector と同
    // idiom。group は表示 clip を持たないので clip 選択ではなく track 選択が
    // トリガ（§5.5）。純 audio バスには出ない（§5.6 group_has_visual_content）。
    if let Some(summary) = app.inspector_group_transform_summary() {
        ui.label_at(
            "inspector_group_transform_label",
            "Group Transform",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 64.0;
        let auto_btn_w = 22.0;
        let auto_btn_gap = 4.0;
        let input_x = area.x + pad + label_w;
        let input_w = row_w - label_w - auto_btn_w - auto_btn_gap;
        let auto_btn_x = input_x + input_w + auto_btn_gap;
        let track_id = summary.track_id;
        let gt = summary.transform;

        for param in crate::app::GROUP_PARAMS {
            use common::model::GroupTransformParam as G;
            let idx = crate::app::group_param_index(param);
            // param 別の (現値, default, 書式, range, sensitivity[units/px], label)。
            // Rotation は degree 表示 / 入力（model は radians）。
            #[allow(clippy::type_complexity)]
            let (value, default, fmt, range, sens, label): (
                f64,
                f64,
                ScrubableNumberFormat,
                Option<(f64, f64)>,
                f32,
                &str,
            ) = match param {
                G::X => (gt.x.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, "X"),
                G::Y => (gt.y.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, "Y"),
                G::Rotation => (
                    gt.rotation_radians.to_degrees().into(),
                    0.0,
                    ScrubableNumberFormat::Decimal(1),
                    // 度域 range。modulation の色帯/live tick は range が要る (gui_01 overlay
                    // は range なしだと枠強調のみ)。handler は従来どおり -π..π wrap。
                    Some((-180.0, 180.0)),
                    1.0,
                    "Rot (°)",
                ),
                G::ScaleX => (
                    gt.scale_x.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    "ScaleX",
                ),
                G::ScaleY => (
                    gt.scale_y.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    "ScaleY",
                ),
                G::AnchorX => (
                    gt.anchor_x.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "AnchorX",
                ),
                G::AnchorY => (
                    gt.anchor_y.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "AnchorY",
                ),
                G::Opacity => (
                    gt.opacity.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "Opacity",
                ),
            };
            ui.label_at((param, "group_label"), label, area.x + pad, y + 5.0, 11.0, p.text);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
            // per-control modulation (docs/plan_modulation_routing_redesign.md §6):
            // 立ち絵を音でドラッグ変調する Bitwig 流。全 8 param を対象 (Rotation は
            // deg↔rad、ScaleX/Y は log space を `build_mod` が到達値ベースで吸収)。
            let g_target = AutomationTarget::GroupTransform(param);
            let g_domain = if matches!(param, G::Rotation) {
                mod_widget::PLAIN_ROTATION
            } else {
                mod_widget::PLAIN_IDENT
            };
            let g_mod_build = build_mod(app, g_target.clone(), value, g_domain, track_id);
            let g_modulation = Some(g_mod_build.modulation());
            let resp = ui.scrubable_number_at(
                (param, "group_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                value,
                default,
                fmt,
                &style,
                move |v| {
                    // Rotation は degree 入力 → radians に変換して設定。
                    let value = if matches!(param, G::Rotation) {
                        (v as f32).to_radians()
                    } else {
                        v as f32
                    };
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetGroupTransformField {
                            track_id,
                            param,
                            value,
                        })
                    })
                },
                None,
                g_modulation,
            );
            // modulation depth ドラッグの falling edge で host 再同期 (audio target の
            // depth 反映用。visual group transform は compose が即読みするので視覚は即時)。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &g_target, resp.mod_dragging);
            // drag / text 編集の開始・終了 edge で undo を 1 step に bracket。
            let active = resp.dragging || resp.editing_text;
            let was_active = app.ui_ephemeral.group_scrub_active == Some(param);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.group_scrub_active = Some(param);
                    app.handle_event(AppEvent::BeginGroupTransformDrag);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.group_scrub_active = None;
                    app.handle_event(AppEvent::EndGroupTransformDrag);
                }));
            }
            let auto_on = summary.automated[idx];
            ui.toggle_button_at(
                (param, "group_auto"),
                "A",
                Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
                auto_on,
                &toggle_automate_style(&app.theme),
                move |_| {
                    Edit::mutate(move |app: &mut AppData| {
                        let ev = if auto_on {
                            AppEvent::RemoveGroupAutomationLane { param }
                        } else {
                            AppEvent::AddGroupAutomationLane { param }
                        };
                        app.handle_event(ev);
                    })
                },
            );
            y += input_h + 4.0;
        }
        y += 8.0;
    }

    // 内蔵映像 FX のパラメータ調整パネル（チェーン行の "GUI" ボタンで開閉）。
    // 各 param を scrubable_number で実レンジ表示 + per-control 変調（Ranged domain で kick→効果）。
    // 値の SSoT は PluginParam lane の default_value（`SetVideoFxParam` が格納）。
    if let Some(view) = app.inspector_video_fx_params() {
        ui.label_at("inspector_vfx_label", view.def.name, area.x + pad, y, 12.0, p.text);
        y += 18.0;
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 88.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        let track_id = view.track_id;
        let device_id = view.device_id;
        for (i, param) in view.def.params.iter().enumerate() {
            let value = f64::from(view.values[i]);
            let (min, max) = param.kind.range();
            let (min, max) = (f64::from(min), f64::from(max));
            let default_real = f64::from(param.kind.norm_to_real(param.kind.default_norm()));
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            ui.label_at_clipped(
                (i, "vfx_label"),
                param.name,
                Rect {
                    x: area.x + pad,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: param.id,
                legacy_device_index: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: param.kind.is_log() };
            let mod_build = build_mod(app, target.clone(), value, domain, track_id);
            let modulation = Some(mod_build.modulation());
            let param_id = param.id;
            let resp = ui.scrubable_number_at(
                (i, "vfx_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                value,
                default_real,
                ScrubableNumberFormat::Decimal(3),
                &style,
                move |v| {
                    #[allow(clippy::cast_possible_truncation)]
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetVideoFxParam {
                            device_id,
                            param_id,
                            value_real: v as f32,
                        });
                    })
                },
                None,
                modulation,
            );
            // drag / text 編集の開始・終了 edge で undo を 1 step に bracket（毎フレームの
            // SetVideoFxParam は非 undoable、stroke 先頭で 1 snapshot）。
            let active = resp.dragging || resp.editing_text;
            let scrub_key = crate::app::InspectorScrubField::VideoFx { device_id, param_id };
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                }));
            }
            // 変調 depth ドラッグの falling edge で host 再同期（音声 target の depth 反映用）。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &target, resp.mod_dragging);
            y += input_h + 4.0;
        }
        y += 8.0;
    }

    // ---- Plugin param panel -----------------------------------
    // 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し CLAP・VST3) の
    // チェーン行「Par」ボタンで開閉。 VOICEVOX は device 既定の声 (キャラ→スタイル)
    // を、 汎用 plugin は param を scrubable_number で実レンジ編集する。 値の SSoT は
    // PluginParam lane の default_value (= `set_plugin_param`、 映像 FX と同 idiom)。
    if let Some(view) = app.inspector_plugin_params() {
        let device_id = view.device_id;
        let track_id = view.track_id;
        ui.label_at_clipped(
            "inspector_pp_label",
            &view.plugin_name,
            Rect { x: area.x + pad, y, w: (area.w - pad * 2.0).max(1.0), h: 12.0 * 1.2 },
            12.0,
            p.text,
        );
        y += 18.0;

        // 汎用 param 行 (scrubable_number で実レンジ + per-control 変調)。
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 96.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        for (i, row) in view.params.iter().enumerate() {
            // param 名はプラグイン由来で長さ上限が無い。 label_w(96) を超えると
            // 後続の値ボックス (不透明 bg) に覆われて途中で消えるので rect で切る。
            ui.label_at_clipped(
                (i, "pp_name"),
                &row.name,
                Rect {
                    x: area.x + pad,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            if row.readonly {
                // 編集不可 param は現値をラベル表示するだけ。
                let txt = format!("{:.3}", row.value_real);
                ui.label_at_clipped(
                    (i, "pp_ro"),
                    &txt,
                    Rect { x: input_x, y: y + 5.0, w: input_w, h: 11.0 * 1.2 },
                    11.0,
                    p.text,
                );
                y += input_h + 4.0;
                continue;
            }
            let (min, max) = (row.min, row.max);
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: row.id,
                legacy_device_index: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: false };
            let mod_build = build_mod(app, target.clone(), row.value_real, domain, track_id);
            let modulation = Some(mod_build.modulation());
            let param_id = row.id;
            let fmt = if row.stepped {
                ScrubableNumberFormat::Decimal(0)
            } else {
                ScrubableNumberFormat::Decimal(3)
            };
            let resp = ui.scrubable_number_at(
                (i, "pp_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                row.value_real,
                row.default_real,
                fmt,
                &style,
                move |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPluginParam {
                            device_id,
                            param_id,
                            value_real: v,
                        });
                    })
                },
                None,
                modulation,
            );
            // drag / text 編集 stroke を undo 1 step に bracket (`SetPluginParam` は
            // per-frame 非 undoable)。 終端で host へ 1 回 resync して音に反映する
            // (映像 FX と違い audio plugin は daw_audio が lane を読むため要 push)。
            let active = resp.dragging || resp.editing_text;
            let scrub_key = crate::app::InspectorScrubField::PluginParam { device_id, param_id };
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                    // 確定値の host push は不要: scrub 中の edit_song が bump した epoch を
                    // runner の frame flush が per-frame で LoadSong する (sync 一本化)。
                }));
            }
            // 変調 depth ドラッグの falling edge で host 再同期。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &target, resp.mod_dragging);
            y += input_h + 4.0;
        }
        y += 8.0;
    }

    // ---- Text Event section (`docs/plan_text_overlay.md` §4 P5 + P5.B) --
    // selected_clip が `ClipContent::Text` のとき、 first event の全 field
    // (text / font / align / 23 numeric + 2 fade beats / fade curves / mute)
    // を expose。 numeric field は scrubable_number 化され、
    // on_change が `TextNumField` discriminator 付き `SetClipTextNumField` を
    // 直接 dispatch する (drag / type 両対応、 undo は Begin/EndInspectorScrub)。
    //
    // (talk/v26) これは「字幕の見た目」編集 UI。字幕 (`builtin.video.subtitle`) device が
    // 挿さっているトラック (= 画面表示が有効) のときだけ出す。device 無しで字幕パラメータが
    // 出るのは無意味なので gate する (`docs/plan_voicevox_talk.md`)。本文 (セリフ) 自体は
    // talk 節 (VOICEVOX device 時) でも編集できるので、ここで隠しても talk-only トラックの
    // テキスト編集は失われない。
    let text_track_has_subtitle = app
        .selected_clip_ref()
        .and_then(|r| app.song_doc.song().tracks.get(r.track as usize))
        .is_some_and(common::model::Track::has_subtitle_device);
    // 字幕 device の「Par」を押したときだけ Text Event 欄を出す
    // (= 専用欄を常時表示せず Par パネルに集約)。
    if text_track_has_subtitle
        && app.subtitle_param_panel_open()
        && let Some(summary) = app.inspector_text_event_summary()
    {
        if app.ui_ephemeral.clip_edit_buffer_target != Some(summary.target) {
            let target = summary.target;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ResyncClipTextEditBuffers(target));
            }));
        }

        ui.label_at(
            "inspector_text_event_label",
            "Text Event",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 60.0;
        let auto_btn_w = 22.0;
        let auto_btn_gap = 4.0;
        let input_x = area.x + pad + label_w;
        let numeric_input_w = row_w - label_w - auto_btn_w - auto_btn_gap;
        let string_input_w = row_w - label_w;
        let auto_btn_x = input_x + numeric_input_w + auto_btn_gap;

        // Mute toggle
        let toggle_h = 24.0;
        let new_mute = !summary.muted;
        ui.toggle_button_at(
            "inspector_text_mute",
            "Mute",
            Rect { x: area.x + pad, y, w: row_w, h: toggle_h },
            summary.muted,
            &toggle_audio_style(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                        targets: app.inspector_target_refs(),
                        edit: DiscreteClipEdit::TextMuted(new_mute),
                    })
                })
            },
        );
        y += toggle_h + 8.0;

        // Text content (single-line, Enter で commit)
        ui.label_at(
            "inspector_text_content_label",
            "Text",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let text_resp = ui.text_input_at(
            "inspector_text_content_input",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            &app.ui_ephemeral.clip_text_content_edit_text,
            &TextInputStyle::default(),
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                })
            },
        );
        // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
        if text_resp.committed || text_resp.blurred {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipTextContentEdit)
            }));
        }
        y += input_h + 4.0;

        // Font family: ボタンで font picker を開く。ラベルは現在の
        // フォント (空 = デフォルト)。検索付きモーダルで選び、 ↑↓ / ホバーで
        // キャンバスにライブプレビューされる。
        ui.label_at(
            "inspector_text_font_label",
            "Font",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let font_btn_label = if app.ui_ephemeral.clip_text_font_family_edit_text.is_empty() {
            "(default)".to_string()
        } else {
            app.ui_ephemeral.clip_text_font_family_edit_text.clone()
        };
        if ui.button_at_clicked(
            "inspector_text_font_button",
            &font_btn_label,
            Rect { x: input_x, y, w: string_input_w, h: input_h },
        ) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenFontPicker)
            }));
        }
        y += input_h + 4.0;

        // Align dropdown (Left / Center / Right)
        ui.label_at(
            "inspector_text_align_label",
            "Align",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        const ALIGN_LABELS: &[&str] = &["Left", "Center", "Right"];
        let align_idx = match summary.align {
            TextAlign::Left => 0,
            TextAlign::Center => 1,
            TextAlign::Right => 2,
        };
        if let Some(picked) = ui.dropdown(
            "inspector_text_align_dropdown",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            ALIGN_LABELS,
            align_idx,
        ) {
            let new_align = match picked {
                0 => TextAlign::Left,
                2 => TextAlign::Right,
                _ => TextAlign::Center,
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextAlign(new_align),
                })
            }));
        }
        y += input_h + 8.0;

        // 23 numeric rows + 2 fade beats を 1 closure 化した helper。
        // 各行を scrubable_number 化し (= 1 箇所変更で 25 行全部が drag 対応)、
        // `scrub_field` で drag / text 編集を undo 1 step に bracket する。
        // automate 「A」 トグルは従来どおり共存。 値源は summary の first event
        // snapshot、 on_change は `SetClipTextNumField` (Rotation は deg→rad)。
        let text_fade_max = summary.fade_max_beats.max(0.0);
        let emit_num_row = |ui: &mut Ui<'_, AppData>,
                            app: &AppData,
                            label: &str,
                            field: TextNumField,
                            row_y: &mut f32| {
            // ラベル欄は input_x までの label_w(60) しかない。 "Sh Blur (px)" は
            // 実 advance 69.6px でここを溢れ、 後から描かれる数値ボックスの
            // 不透明背景に末尾が食われていた。
            ui.label_at_clipped(
                (field, "label"),
                label,
                Rect {
                    x: area.x + pad,
                    y: *row_y + 5.0,
                    w: (label_w - 2.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            // field 別の (書式, range, sensitivity[units/px])。 clamp は handler
            // (`set_clip_text_num_field`) と一致させる。
            let (fmt, range, sens): (ScrubableNumberFormat, Option<(f64, f64)>, f32) = match field {
                // PiP rect (0..1 normalized)。
                TextNumField::X | TextNumField::Y | TextNumField::W | TextNumField::H => {
                    (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004)
                }
                // Rotation: degree 表示、 handler が -π..π wrap (range なし)。
                // 度域 range で modulation overlay を描けるように (handler は -π..π wrap)。
                TextNumField::Rotation => {
                    (ScrubableNumberFormat::Decimal(1), Some((-180.0, 180.0)), 1.0)
                }
                // Font size (px, >= 1.0)。
                TextNumField::FontSize => {
                    (ScrubableNumberFormat::Decimal(1), Some((1.0, 4096.0)), 0.5)
                }
                // 0..1 の opacity / RGBA。
                TextNumField::Opacity
                | TextNumField::FillR
                | TextNumField::FillG
                | TextNumField::FillB
                | TextNumField::FillA
                | TextNumField::OutlineR
                | TextNumField::OutlineG
                | TextNumField::OutlineB
                | TextNumField::OutlineA
                | TextNumField::ShadowR
                | TextNumField::ShadowG
                | TextNumField::ShadowB
                | TextNumField::ShadowA => {
                    (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004)
                }
                // px 値 (>= 0)。 outline width / shadow blur。
                TextNumField::OutlineWidth | TextNumField::ShadowBlur => {
                    (ScrubableNumberFormat::Decimal(1), Some((0.0, 1024.0)), 0.5)
                }
                // shadow offset px (handler 側 clamp 無し、 上下とも自由)。
                TextNumField::ShadowOffsetX | TextNumField::ShadowOffsetY => {
                    (ScrubableNumberFormat::Decimal(1), None, 0.5)
                }
                // fade beats (0..clip 長)。
                TextNumField::FadeInBeats | TextNumField::FadeOutBeats => {
                    (ScrubableNumberFormat::Decimal(3), Some((0.0, text_fade_max)), 0.01)
                }
            };
            let default = match field {
                // dblclick reset の妥当値。
                TextNumField::Opacity
                | TextNumField::FillA
                | TextNumField::W
                | TextNumField::H => summary.text_num_field_value(field),
                TextNumField::FontSize => 48.0,
                _ => 0.0,
            };
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
            scrub_field(
                ui,
                app,
                (field, "input"),
                Rect { x: input_x, y: *row_y, w: numeric_input_w, h: input_h },
                app.inspector_text_num_folded(field),
                default,
                fmt,
                &style,
                InspectorScrubField::Text(field),
                move |t, v| {
                    // Rotation のみ degree 入力 → radians に変換 (handler が wrap)。
                    let value = if matches!(field, TextNumField::Rotation) {
                        (v as f32).to_radians()
                    } else {
                        v as f32
                    };
                    AppEvent::SetClipTextNumField { target: t, field, value }
                },
            );
            if let Some(builtin) = text_num_to_builtin(field) {
                let auto_on = summary.automated.contains(&builtin);
                ui.toggle_button_at(
                    (field, "auto"),
                    "A",
                    Rect { x: auto_btn_x, y: *row_y, w: auto_btn_w, h: input_h },
                    auto_on,
                    &toggle_automate_style(&app.theme),
                    move |_| {
                        Edit::mutate(move |app: &mut AppData| {
                            let ev = if auto_on {
                                AppEvent::RemoveTextAutomationLane { field: builtin }
                            } else {
                                AppEvent::AddTextAutomationLane { field: builtin }
                            };
                            app.handle_event(ev);
                        })
                    },
                );
            }
            *row_y += input_h + 4.0;
        };

        emit_num_row(ui, app, "X", TextNumField::X, &mut y);
        emit_num_row(ui, app, "Y", TextNumField::Y, &mut y);
        emit_num_row(ui, app, "W", TextNumField::W, &mut y);
        emit_num_row(ui, app, "H", TextNumField::H, &mut y);
        emit_num_row(ui, app, "Rot (°)", TextNumField::Rotation, &mut y);
        emit_num_row(ui, app, "Size (px)", TextNumField::FontSize, &mut y);
        emit_num_row(ui, app, "Opacity", TextNumField::Opacity, &mut y);
        emit_num_row(ui, app, "Fill R", TextNumField::FillR, &mut y);
        emit_num_row(ui, app, "Fill G", TextNumField::FillG, &mut y);
        emit_num_row(ui, app, "Fill B", TextNumField::FillB, &mut y);
        emit_num_row(ui, app, "Fill A", TextNumField::FillA, &mut y);
        emit_num_row(ui, app, "Out R", TextNumField::OutlineR, &mut y);
        emit_num_row(ui, app, "Out G", TextNumField::OutlineG, &mut y);
        emit_num_row(ui, app, "Out B", TextNumField::OutlineB, &mut y);
        emit_num_row(ui, app, "Out A", TextNumField::OutlineA, &mut y);
        emit_num_row(ui, app, "Out W (px)", TextNumField::OutlineWidth, &mut y);
        emit_num_row(ui, app, "Sh R", TextNumField::ShadowR, &mut y);
        emit_num_row(ui, app, "Sh G", TextNumField::ShadowG, &mut y);
        emit_num_row(ui, app, "Sh B", TextNumField::ShadowB, &mut y);
        emit_num_row(ui, app, "Sh A", TextNumField::ShadowA, &mut y);
        emit_num_row(ui, app, "Sh X (px)", TextNumField::ShadowOffsetX, &mut y);
        emit_num_row(ui, app, "Sh Y (px)", TextNumField::ShadowOffsetY, &mut y);
        emit_num_row(ui, app, "Sh Blur (px)", TextNumField::ShadowBlur, &mut y);
        emit_num_row(ui, app, "Fade In", TextNumField::FadeInBeats, &mut y);
        emit_num_row(ui, app, "Fade Out", TextNumField::FadeOutBeats, &mut y);

        // Fade curve dropdowns (2 個)。 FADE_CURVE_LABELS / curve <-> index
        // は image fade と同 idiom。
        const FADE_CURVE_LABELS: &[&str] = &["Linear", "Exp", "S-Curve"];
        let curve_to_idx = |c: FadeCurve| match c {
            FadeCurve::Linear => 0,
            FadeCurve::Exponential => 1,
            FadeCurve::SCurve => 2,
        };
        let idx_to_curve = |i: usize| match i {
            1 => FadeCurve::Exponential,
            2 => FadeCurve::SCurve,
            _ => FadeCurve::Linear,
        };
        ui.label_at(
            "inspector_text_fade_in_curve_label",
            "In Curve",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_in_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_in_curve),
        ) {
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::In, new_curve),
                })
            }));
        }
        y += input_h + 4.0;
        ui.label_at(
            "inspector_text_fade_out_curve_label",
            "Out Curve",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_out_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_out_curve),
        ) {
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::Out, new_curve),
                })
            }));
        }
        y += input_h + 12.0;
        // suppress unused warning when section emits nothing further
        let _ = (label_w, summary);
    }

    // カーソルトラックの index。None のとき以降の per-track セクションは
    // 描画しない (track 0 を誤対象にしない)。
    let cursor_idx = app.cursor_track_index();

    // Clip Voice 編集: 選択中の clip が vocal track 上の MIDI clip の
    // とき、 キャラ ▼ → スタイル ▼ の 2 段 dropdown で per-clip 声を選ぶ。
    // 声は per-clip (`Clip::speaker_id`) が SSoT、 SetClipVoice で焼き込む。
    if app.voicevox_param_panel_open()
        && let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song_doc.song().tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song_doc.song()
            .clip_contents
            .get(&clip.content_id)
            .is_none_or(|c| matches!(c, common::model::ClipContent::Midi(_)))
    {
        let clip_key = common::model::ClipKey {
            track_id: track.id,
            clip_id: clip.id,
        };
        let cur_speaker = clip.speaker_id;
        // 現在の声の表示名: clip 焼き込み名 → speaker_id 逆引き → アプリ既定。
        let (cur_singer, cur_style) = if !clip.singer_name.is_empty() {
            (clip.singer_name.clone(), clip.style_name.clone())
        } else if let Some(found) = app.voicevox.singers.iter().find_map(|s| {
            s.styles
                .iter()
                .find(|st| st.id == cur_speaker)
                .map(|st| (s.name.clone(), st.name.clone()))
        }) {
            found
        } else {
            (
                common::voicevox::DEFAULT_SINGER_NAME.to_string(),
                common::voicevox::DEFAULT_STYLE_NAME.to_string(),
            )
        };

        ui.label_at(
            "inspector_clip_voice_label",
            "Clip Voice",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        if app.voicevox.singers.is_empty() {
            // engine 未起動 / 一覧未取得: 焼き込み声名 + 取得中。 声名は常に出せる。
            let txt = format!("{cur_singer} - {cur_style}  (一覧取得中…)");
            ui.label_at(
                "inspector_clip_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                p.text,
            );
            y += 26.0;
        } else {
            // 上段: キャラ dropdown。
            let char_labels: Vec<&str> =
                app.voicevox.singers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .voicevox.singers
                .iter()
                .position(|s| s.name == cur_singer)
                .unwrap_or(0);
            let char_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_char = ui.dropdown(
                "inspector_clip_voice_char",
                char_rect,
                &char_labels,
                cur_char_idx,
            );
            y += 28.0;

            // 下段: スタイル dropdown (= 上段で選んだ or 現在のキャラの styles)。
            let char_idx = picked_char.unwrap_or(cur_char_idx).min(app.voicevox.singers.len() - 1);
            let singer = &app.voicevox.singers[char_idx];
            let style_labels: Vec<&str> =
                singer.styles.iter().map(|st| st.name.as_str()).collect();
            let cur_style_idx = singer
                .styles
                .iter()
                .position(|st| st.id == cur_speaker)
                .unwrap_or(0);
            let style_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_style = ui.dropdown(
                "inspector_clip_voice_style",
                style_rect,
                &style_labels,
                cur_style_idx,
            );
            y += 28.0;

            // 確定値: キャラを変えたらそのキャラの先頭 style、 style を変えたら
            // その style を採用 (= (speaker_id, singer_name, style_name))。
            let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
                app.voicevox.singers.get(pc).and_then(|s| {
                    s.styles
                        .first()
                        .map(|st| (st.id, s.name.clone(), st.name.clone()))
                })
            } else if let Some(ps) = picked_style {
                singer
                    .styles
                    .get(ps)
                    .map(|st| (st.id, singer.name.clone(), st.name.clone()))
            } else {
                None
            };
            if let Some((sid, sn, stn)) = chosen {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipVoice {
                        clip: clip_key,
                        speaker_id: sid,
                        singer_name: sn.clone(),
                        style_name: stn.clone(),
                    });
                }));
            }

            // 再取得ボタン (新規キャラ導入時に押す)。
            let refetch_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_clip_voice_refetch",
                "声一覧を再取得",
                refetch_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RefetchSingers);
                }));
            }
            y += 28.0;
        }
    }

    // (talk) Text Clip 読み上げ編集 (`docs/plan_voicevox_talk.md` §4)。選択中 clip が
    // VOICEVOX デバイス付きトラック上の Text clip のとき、talk 話者 (キャラ→talk style)
    // + 読み上げスケール 4 つ (話速/音高/抑揚/音量) を編集する。声は `Clip::speaker_id`
    // を talk style として流用 (SetClipVoice で焼き込み)。スケールは `Clip::talk`。
    if app.voicevox_param_panel_open()
        && let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song_doc.song().tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song_doc.song()
            .clip_contents
            .get(&clip.content_id)
            .is_some_and(|c| matches!(c, common::model::ClipContent::Text(_)))
    {
        let clip_key = common::model::ClipKey {
            track_id: track.id,
            clip_id: clip.id,
        };
        let cur_speaker = clip.speaker_id;
        let has_subtitle = track.has_subtitle_device();
        let talk = clip.talk.unwrap_or_default();
        // 現在の talk 声名: clip 焼き込み名 → speaker_id 逆引き → 空 (取得中表示)。
        let (cur_char, cur_style) = if !clip.singer_name.is_empty() {
            (clip.singer_name.clone(), clip.style_name.clone())
        } else {
            app.voicevox.talk_speakers
                .iter()
                .find_map(|s| {
                    s.styles
                        .iter()
                        .find(|st| st.id == cur_speaker)
                        .map(|st| (s.name.clone(), st.name.clone()))
                })
                .unwrap_or_default()
        };

        ui.label_at(
            "inspector_talk_label",
            "読み上げ (Talk)",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        // 字幕デバイス未挿入 = 画面非表示。ワンクリック追加ヘルパ (Q10)。
        if !has_subtitle {
            let warn_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_talk_add_subtitle",
                "+ 字幕デバイス (画面に表示)",
                warn_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::SelectPluginFromDb {
                        id: common::plugin_db::SUBTITLE_ID.to_string(),
                        keep_open: false,
                        open_gui: false,
                    });
                }));
            }
            y += 26.0;
        }

        // (talk) 本文 (セリフ) 入力。字幕 device 時は overlay「Text Event」節が本文入力を
        // 持つので、ここは字幕 device 無し (= 喋るが映さない talk-only) のときだけ出し、
        // 二重入力を避ける。編集 buffer / events は overlay と共用 (同時表示しないので競合せず)。
        if !has_subtitle {
            if app.ui_ephemeral.clip_edit_buffer_target != Some(r) {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ResyncClipTextEditBuffers(r));
                }));
            }
            ui.label_at(
                "inspector_talk_text_label",
                "セリフ",
                area.x + pad,
                y + 5.0,
                11.0,
                p.text,
            );
            let resp = ui.text_input_at(
                "inspector_talk_text_input",
                Rect {
                    x: area.x + pad + 48.0,
                    y,
                    w: area.w - pad * 2.0 - 48.0,
                    h: 22.0,
                },
                &app.ui_ephemeral.clip_text_content_edit_text,
                &TextInputStyle::default(),
                |s| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                    })
                },
            );
            // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
            if resp.committed || resp.blurred {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitClipTextContentEdit)
                }));
            }
            y += 26.0;
        }

        // talk 話者 picker (キャラ → talk style)。
        if app.voicevox.talk_speakers.is_empty() {
            let txt = if cur_char.is_empty() {
                "(talk 声一覧 取得中…)".to_string()
            } else {
                format!("{cur_char} - {cur_style}  (一覧取得中…)")
            };
            ui.label_at(
                "inspector_talk_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                p.text,
            );
            y += 26.0;
        } else {
            let char_labels: Vec<&str> =
                app.voicevox.talk_speakers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .voicevox.talk_speakers
                .iter()
                .position(|s| s.name == cur_char)
                .or_else(|| {
                    app.voicevox.talk_speakers
                        .iter()
                        .position(|s| s.styles.iter().any(|st| st.id == cur_speaker))
                })
                .unwrap_or(0);
            let char_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_char =
                ui.dropdown("inspector_talk_char", char_rect, &char_labels, cur_char_idx);
            y += 28.0;

            let char_idx = picked_char
                .unwrap_or(cur_char_idx)
                .min(app.voicevox.talk_speakers.len() - 1);
            let speaker = &app.voicevox.talk_speakers[char_idx];
            let style_labels: Vec<&str> =
                speaker.styles.iter().map(|st| st.name.as_str()).collect();
            let cur_style_idx = speaker
                .styles
                .iter()
                .position(|st| st.id == cur_speaker)
                .unwrap_or(0);
            let style_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_style = ui.dropdown(
                "inspector_talk_style",
                style_rect,
                &style_labels,
                cur_style_idx,
            );
            y += 28.0;

            let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
                app.voicevox.talk_speakers.get(pc).and_then(|s| {
                    s.styles
                        .first()
                        .map(|st| (st.id, s.name.clone(), st.name.clone()))
                })
            } else if let Some(ps) = picked_style {
                speaker
                    .styles
                    .get(ps)
                    .map(|st| (st.id, speaker.name.clone(), st.name.clone()))
            } else {
                None
            };
            if let Some((sid, sn, stn)) = chosen {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipVoice {
                        clip: clip_key,
                        speaker_id: sid,
                        singer_name: sn.clone(),
                        style_name: stn.clone(),
                    });
                }));
            }

            let refetch_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_talk_refetch",
                "talk 声一覧を再取得",
                refetch_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RefetchSpeakers);
                }));
            }
            y += 28.0;
        }

        // 読み上げスケール 4 つ。VOICEVOX talk の話速/音高/抑揚/音量。
        let scales = [
            ("話速", TalkParamKind::Speed, f64::from(talk.speed_scale), 1.0),
            ("音高", TalkParamKind::Pitch, f64::from(talk.pitch_scale), 0.0),
            (
                "抑揚",
                TalkParamKind::Intonation,
                f64::from(talk.intonation_scale),
                1.0,
            ),
            ("音量", TalkParamKind::Volume, f64::from(talk.volume_scale), 1.0),
        ];
        for (label, kind, val, default) in scales {
            ui.label_at(
                ("inspector_talk_scale_label", label),
                label,
                area.x + pad,
                y + 4.0,
                11.0,
                p.text,
            );
            let input_rect = Rect {
                x: area.x + pad + 48.0,
                y,
                w: area.w - pad * 2.0 - 48.0,
                h: 20.0,
            };
            let resp = ui.scrubable_number_at(
                ("inspector_talk_scale", label),
                input_rect,
                val,
                default,
                ScrubableNumberFormat::Decimal(2),
                &scrub_style(&app.theme),
                move |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetClipTalkParam {
                            clip: clip_key,
                            param: kind,
                            value: v as f32,
                        });
                    })
                },
                None,
                None,
            );
            // 他の inspector 数値 field (`scrub_field`) と同じ Begin/End bracket
            // で 1 drag = 1 undo step にする — これが無いと talk 4 項目だけ
            // Ctrl+Z で戻せない (review)。
            let scrub_key = crate::app::InspectorScrubField::Talk(kind);
            let active = resp.dragging || resp.editing_text;
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                }));
            }
            y += 24.0;
        }
    }

    // ---- 親グループ (Parent) の編集 UI はインスペクタから撤去した ----
    // 親子 (グループ階層) の編集はアレンジビューでのトラックドラッグ
    // (drag reparent SetTrackParent) 一本に統一する。階層は
    // アレンジの入れ子インデントで可視化されるので、同じ概念をインスペクタの
    // ドロップダウンでも編集できると Single Source of Truth が崩れる。
    // `AppEvent::SetTrackParent` / `action_set_track_parent` 自体はアレンジ
    // ドラッグが使うため残す。

    // ---- 口パク (lip-sync) 出力先 binding ----------------------------
    // Vocal track のみ。生成した口画像 ImageEvent を焼き込む先の口 track
    // (立ち絵 group の子 image track) を選ぶ。設定で再生成が走る。
    // VOICEVOX device の「Par」を押したときだけ出す (= 専用欄を常時
    // 表示せず Par パネルに集約。声 / 話速 / 口パク先をまとめて 1 箇所で編集)。
    if app.voicevox_param_panel_open()
        && let Some(track) = cursor_idx.and_then(|i| app.song_doc.song().tracks.get(i))
        && track.is_voicevox_vocal()
    {
        let self_id = track.id;
        // 候補: 自分以外の全 track (= 口 track はどれでも選べる)。
        // candidate_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
        // 表示名はここで 1 度だけ format/clone する (別 Vec への再 clone を避ける)。
        let mut candidate_ids: Vec<u32> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        labels.push("(なし)".into());
        for t in app.song_doc.song().tracks.iter().filter(|t| t.id != self_id) {
            candidate_ids.push(t.id);
            labels.push(if t.name.is_empty() {
                format!("Track {}", t.id)
            } else {
                t.name.clone()
            });
        }
        ui.label_at(
            "inspector_lipsync_target_label",
            "口パク出力先",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;
        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected_idx = match track.lipsync_target_track {
            None => 0,
            Some(tid) => candidate_ids
                .iter()
                .position(|id| *id == tid)
                .map(|i| i + 1)
                .unwrap_or(0),
        };
        if let Some(picked) = ui.dropdown(
            "inspector_lipsync_target_dropdown",
            dropdown_rect,
            &label_refs,
            selected_idx,
        ) {
            let target = if picked == 0 {
                None
            } else {
                candidate_ids.get(picked - 1).copied()
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLipsyncTarget {
                    track: self_id,
                    target,
                });
            }));
        }
        y += 30.0;
    }
                // ====== /device param panel ======
    y
}
