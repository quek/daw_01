//! Video Preview 窓 (`docs/plan_video.md` P4) に届いた `WindowEvent` の処理。
//! `Runner` の責務分けの一部として `runner.rs` から切り出した (キー入力は main 窓の
//! shortcut 経路へ、pointer は PiP / Transform box の drag 編集へ、lifecycle は
//! `preview_window_visible` へ)。`Runner` / `RunnerState` の private field を触るので
//! 子モジュール。

use super::*;

impl Runner {
    /// docs/plan_video.md P4: handle a WindowEvent dispatched against
    /// the preview window. Limited to lifecycle / display events —
    /// CloseRequested flips `AppData.preview_window_visible` to false
    /// so the next frame's lifecycle pass destroys the OS window;
    /// Resized synchronises the wgpu surface; RedrawRequested re-runs
    /// the placeholder (or, post P5/P7, the composited frame).
    pub(super) fn handle_preview_window_event(&mut self, event: WindowEvent) {
        // r.md #107: preview 窓にフォーカスがある間のキーは **main 窓と同じ shortcut
        // 経路** に流す (F12 で閉じる / Space で再生停止 / その他全部)。 preview には
        // text 入力欄が無いので、 main の `ShortcutMap` (`root.rs::dispatch_shortcuts`)
        // がそのまま唯一の判定になる。 旧実装は Space だけを proxy で特別扱いしていた。
        match &event {
            // 合成 press (フォーカス移動時に winit が作る) は流さない — runner.rs の
            // main 窓側と同じ理由 (F12 で開いた瞬間に閉じる)。
            WindowEvent::KeyboardInput { is_synthetic: true, .. } => return,
            WindowEvent::KeyboardInput { event: key, .. } => {
                let key = KeyEvent {
                    state: map_state(key.state),
                    text: key.text.as_ref().map(|s| s.to_string()),
                    physical_key: map_phys_key(key.physical_key),
                    repeat: key.repeat,
                };
                self.dispatch_platform_event(PlatformEvent::Keyboard(key));
                return;
            }
            WindowEvent::ModifiersChanged(mods) => {
                let st = mods.state();
                self.dispatch_platform_event(PlatformEvent::ModifiersChanged(Modifiers {
                    ctrl: st.control_key(),
                    shift: st.shift_key(),
                    alt: st.alt_key(),
                    logo: st.super_key(),
                }));
                return;
            }
            _ => {}
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                state.app.ui_prefs.preview_window_visible = false;
                // Lifecycle pass on the next render_frame drops the
                // preview state; nothing else to do here.
            }
            WindowEvent::Resized(size) => {
                preview.resize(daw_ui_platform::PhysicalSize {
                    width: size.width,
                    height: size.height,
                });
            }
            // r.md #49: preview 窓も daw_01 の窓なので、ここを触っている間は
            // アプリはアクティブ。これを拾わないと preview をクリックした瞬間に
            // main が `Focused(false)` を受けて「非アクティブ」と誤判定する
            // (preview 側の `Focused(true)` はどこにも届かない)。
            WindowEvent::Focused(focused) => {
                state.app.activity.preview_focused = focused;
                state.app.sync_app_active_with_audio();
                if focused {
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // device lost はメインループ側の復旧シーケンスが拾う (ここで再帰的に
                // request_redraw しないことで、 消失中の preview スピンも止まる)。
                match preview.render(&state.app.theme) {
                    Ok(()) => state.preview_error_log.reset(),
                    // device lost はメインループ側の復旧シーケンスが拾う。
                    Err(e) if e.is_device_lost() => {}
                    Err(e) => {
                        state.preview_error_log.record(
                            Instant::now(),
                            "preview render error",
                            &e,
                        );
                    }
                }
            }
            // `docs/plan_image_overlay.md` §4 P5: PiP rect の drag 編集。
            // CursorMoved / MouseInput を捕捉して、 hit-test → drag
            // state 開始 → MouseMoved delta から normalized rect 更新
            // → AppEvent::SetClipImage{X,Y,W,H} 発火。
            WindowEvent::CursorMoved { position, .. } => {
                let cursor = (position.x as f32, position.y as f32);
                state.preview_cursor = Some(cursor);
                if let Some(drag) = state.preview_drag {
                    let size = preview.renderer.size();
                    let project_resolution = state.app.song_doc.song().video_resolution;
                    let project_box = preview_project_box(
                        (size.width as f32, size.height as f32),
                        project_resolution,
                    );
                    // 描画 (`draw_selection_overlay`) と同じ **ライブの**
                    // `selection_group_transform` で逆写像する。こうすると描画・
                    // hit-test・drag が常に同一の group affine を共有し、再生中に
                    // automation が group を動かしてもハンドルが cursor とズレない
                    // （凍結値だと描画はライブ・drag は古い値で不一致になる）。
                    // 通常 image は None = 恒等写像。
                    let map = match preview.selection_group_transform {
                        Some(t) => crate::group_compose::CanvasMap::group(&t, project_box),
                        None => crate::group_compose::CanvasMap::project(project_box),
                    };
                    handle_preview_drag(&state.app, &self.proxy, &drag, cursor, &map);
                }
                if let Some(gdrag) = state.preview_group_drag {
                    let size = preview.renderer.size();
                    let project_resolution = state.app.song_doc.song().video_resolution;
                    handle_group_drag(
                        &self.proxy,
                        &gdrag,
                        cursor,
                        (size.width as f32, size.height as f32),
                        project_resolution,
                    );
                }
            }
            WindowEvent::CursorLeft { .. } => {
                state.preview_cursor = None;
            }
            WindowEvent::MouseInput {
                state: button_state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let pressed = matches!(button_state, winit::event::ElementState::Pressed);
                if pressed {
                    if let Some(cursor) = state.preview_cursor
                        && let Some(overlay) = preview.selection_overlay
                        && let Some(target) = state.app.selected_clip_ref()
                    {
                        let size = preview.renderer.size();
                        let screen = (size.width as f32, size.height as f32);
                        let rotation = preview.selection_rotation_radians;
                        let project_resolution = state.app.song_doc.song().video_resolution;
                        let project_box = preview_project_box(screen, project_resolution);
                        // 選択中 clip が active visual group の子なら親 group の
                        // affine を合成（= ハンドルが立ち絵に重なる）。drag 中は
                        // 毎 frame ライブの `selection_group_transform` を読み直す
                        // ので、ここでは凍結しない（描画と完全一致）。
                        let map = match preview.selection_group_transform {
                            Some(t) => crate::group_compose::CanvasMap::group(&t, project_box),
                            None => crate::group_compose::CanvasMap::project(project_box),
                        };
                        let mode = hit_test_handles(overlay, rotation, &map, cursor);
                        if let Some(mode) = mode {
                            // rect 中心 (canvas→screen 写像後) と cursor の角度を
                            // 保存 (Rotate mode の delta 計算で使う)。
                            let (nx, ny, nw, nh) = overlay;
                            let (cx0, cy0) = map.to_screen(nx + nw * 0.5, ny + nh * 0.5);
                            let start_cursor_angle =
                                (cursor.1 - cy0).atan2(cursor.0 - cx0);
                            state.preview_drag = Some(PreviewDragState {
                                mode,
                                start_cursor: cursor,
                                start_rect: overlay,
                                start_rotation_radians: rotation,
                                start_cursor_angle,
                                target,
                            });
                            // drag begin: snapshot 1 個 + lane recording
                            // seed。 image / text で別 marker event を
                            // 撃つ (= 後者は `docs/plan_text_overlay.md`
                            // §4 P6)。 target の clip kind で振り分ける。
                            let drag_target_kind =
                                preview_drag_target_kind(&state.app, target);
                            let begin_ev = match drag_target_kind {
                                PreviewDragTargetKind::Text => AppEvent::BeginTextPiPDrag,
                                _ => AppEvent::BeginImagePiPDrag,
                            };
                            let _ = self.proxy.send_event(begin_ev);
                        }
                    }
                    // Transform box drag begin（clip drag が始まらなかったときのみ）。
                    // 選択中トラックに Transform 配置 device が刺さって
                    // いれば対象（立ち絵 group も通常トラックも）。base group_transform は
                    // device 追加時に materialize 済なので overlay と同じ effective transform
                    // で hit-test する（枠が出れば必ず掴める）。
                    // r.md #87: 行ごとの実効拍。`if let` の鎖に埋めるとネストが 1 段
                    // 深くなるので手前で組む (engine が publish していない行は
                    // `RowTimeline` が `Song.launcher` へ倒す)。
                    let running = state.app.launcher_running_rows();
                    let beat = state.app.transport.playhead_beat.map(f64::from).unwrap_or(0.0);
                    let rows = RowTimeline::with_running(0.0, beat, &running);
                    if state.preview_drag.is_none()
                        && let Some(cursor) = state.preview_cursor
                        && let Some(track_id) = state.app.cursor_track_id()
                        && let Some(track) = state.app.song_doc.song().track_by_id(track_id)
                        && let Some(transform) = crate::video_fx::resolve_track_transform(
                            state.app.song_doc.song(),
                            track,
                            &rows,
                            state.app.transport.mod_plane.as_ref(),
                        )
                    {
                        let size = preview.renderer.size();
                        let screen = (size.width as f32, size.height as f32);
                        let project_resolution = state.app.song_doc.song().video_resolution;
                        let project_box = preview_project_box(screen, project_resolution);
                        if let Some(mode) = group_hit_test(&transform, project_box, cursor) {
                            let (rx, ry, _rw, _rh, _rot, px, py, _) =
                                crate::group_compose::group_quad_params(
                                    &transform,
                                    project_box,
                                );
                            let pivx = rx + px;
                            let pivy = ry + py;
                            state.preview_group_drag = Some(GroupDragState {
                                mode,
                                start_cursor: cursor,
                                start_transform: transform,
                                target_track_id: track_id,
                                pivot_screen: (pivx, pivy),
                                start_cursor_angle: (cursor.1 - pivy)
                                    .atan2(cursor.0 - pivx),
                                start_pivot_dist: (cursor.0 - pivx)
                                    .hypot(cursor.1 - pivy),
                            });
                            let _ =
                                self.proxy.send_event(AppEvent::BeginGroupTransformDrag);
                        }
                    }
                } else {
                    if let Some(drag) = state.preview_drag.take() {
                        // drag end: lane recording seed のクリア。 begin と同
                        // kind の End event を送る。
                        let end_ev =
                            match preview_drag_target_kind(&state.app, drag.target) {
                                PreviewDragTargetKind::Text => AppEvent::EndTextPiPDrag,
                                _ => AppEvent::EndImagePiPDrag,
                            };
                        let _ = self.proxy.send_event(end_ev);
                    }
                    if state.preview_group_drag.take().is_some() {
                        let _ = self.proxy.send_event(AppEvent::EndGroupTransformDrag);
                    }
                }
            }
            _ => {}
        }
    }
}
