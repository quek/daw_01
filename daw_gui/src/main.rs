mod app;
mod job;
mod subprocess;
mod view;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use common::audio_bridge::{
    AudioBridgeHandle, CHANNELS, MAX_FRAMES, SAMPLE_RATE, ready_sem_id, request_sem_id, shmem_id,
};
use common::pipe::pipe_path;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use common::win_sem::Semaphore;
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use vizia::prelude::*;

use crate::app::{AppData, AppEvent};
use crate::job::JobHandle;
use crate::view::{
    ArrangementView, BottomPanelView, PluginPickerView, StatusBarView, TrackInspectorView,
    TransportView,
};

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let job = JobHandle::new()?;
    let rt = Runtime::new().context("failed to create tokio runtime")?;

    let pid = std::process::id();
    let session = AudioSession {
        shmem_id: shmem_id(pid),
        request_sem_id: request_sem_id(pid),
        ready_sem_id: ready_sem_id(pid),
        sample_rate: SAMPLE_RATE,
        max_frames: MAX_FRAMES,
        channels: CHANNELS as u16,
    };
    let bridge = Arc::new(
        AudioBridgeHandle::create(&session.shmem_id)
            .context("failed to create audio shmem")?,
    );
    let _request_sem = Semaphore::create(&session.request_sem_id, 0, 2)
        .context("failed to create request semaphore")?;
    let _ready_sem = Semaphore::create(&session.ready_sem_id, 0, 2)
        .context("failed to create ready semaphore")?;
    tracing::info!(?session, "created audio session handles");

    let (audio_child, plugin_child, mut audio_server, mut plugin_server) =
        rt.block_on(spawn_and_handshake(&job))?;

    rt.block_on(async {
        write_msg(&mut audio_server, &MainToChild::Session(session.clone())).await?;
        write_msg(&mut plugin_server, &MainToChild::Session(session.clone())).await?;
        anyhow::Ok(())
    })
    .context("failed to send audio session")?;

    let _children = (audio_child, plugin_child);

    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    let (plugin_tx, plugin_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel::<ChildToMain>();
    rt.spawn(send_loop(audio_server, audio_rx));
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx));

    let plugin_db = load_or_build_plugin_db();

    let clap_plugin_path: Option<PathBuf> = None;

    tracing::info!("opening main window");
    run_gui(
        audio_tx,
        plugin_tx,
        clap_plugin_path,
        plugin_db,
        Arc::clone(&bridge),
        incoming_rx,
    )?;
    tracing::info!("daw_gui exiting");
    drop(bridge);
    Ok(())
}

async fn spawn_and_handshake(
    job: &JobHandle,
) -> Result<(Child, Child, NamedPipeServer, NamedPipeServer)> {
    let pid = std::process::id();
    let audio_pipe = pipe_path(pid, ChildKind::Audio);
    let plugin_pipe = pipe_path(pid, ChildKind::PluginHost);

    let audio_server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&audio_pipe)
        .with_context(|| format!("failed to create pipe {audio_pipe}"))?;
    let plugin_server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&plugin_pipe)
        .with_context(|| format!("failed to create pipe {plugin_pipe}"))?;

    let audio_child = subprocess::spawn_sibling("daw_audio", [&audio_pipe])?;
    job.assign(&audio_child)?;
    let plugin_child = subprocess::spawn_sibling("daw_plugin_host", [&plugin_pipe])?;
    job.assign(&plugin_child)?;

    let (audio_result, plugin_result) = tokio::try_join!(
        handshake(audio_server, ChildKind::Audio),
        handshake(plugin_server, ChildKind::PluginHost),
    )?;
    let (audio_hello, audio_server) = audio_result;
    let (plugin_hello, plugin_server) = plugin_result;
    tracing::info!(?audio_hello, "audio handshake complete");
    tracing::info!(?plugin_hello, "plugin_host handshake complete");

    Ok((audio_child, plugin_child, audio_server, plugin_server))
}

async fn handshake(
    mut server: NamedPipeServer,
    expected: ChildKind,
) -> Result<(ChildToMain, NamedPipeServer)> {
    server.connect().await.context("failed to accept client")?;
    let hello: ChildToMain = read_msg(&mut server).await?;
    let kind = match &hello {
        ChildToMain::Hello { kind, .. } => *kind,
        other => anyhow::bail!("expected Hello from child, got {:?}", other),
    };
    anyhow::ensure!(
        kind == expected,
        "child kind mismatch: expected {:?}, got {:?}",
        expected,
        kind
    );
    write_msg(&mut server, &MainToChild::Ack).await?;
    Ok((hello, server))
}

async fn send_loop(mut pipe: NamedPipeServer, mut rx: UnboundedReceiver<MainToChild>) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_msg(&mut pipe, &msg).await {
            tracing::error!(error = ?e, ?msg, "failed to send message to child");
            break;
        }
    }
    tracing::info!("send loop ended");
}

async fn plugin_pipe_loop(
    mut pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: tokio::sync::mpsc::UnboundedSender<ChildToMain>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, ChildToMain>(&mut pipe) => {
                match msg {
                    Ok(m) => {
                        if incoming_tx.send(m).is_err() {
                            tracing::info!("incoming receiver dropped; pipe loop exiting");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = ?e, "plugin_host pipe closed");
                        break;
                    }
                }
            }
            Some(msg) = rx.recv() => {
                if let Err(e) = write_msg(&mut pipe, &msg).await {
                    tracing::error!(error = ?e, ?msg, "failed to send message to plugin_host");
                    break;
                }
            }
            else => break,
        }
    }
    tracing::info!("plugin pipe loop ended");
}

fn run_gui(
    audio_tx: UnboundedSender<MainToChild>,
    plugin_tx: UnboundedSender<MainToChild>,
    clap_plugin_path: Option<PathBuf>,
    plugin_db: Option<Arc<common::plugin_db::PluginDatabase>>,
    bridge: Arc<AudioBridgeHandle>,
    incoming_rx: UnboundedReceiver<ChildToMain>,
) -> Result<()> {
    let mut incoming_rx_slot = Some(incoming_rx);
    Application::new(move |cx| {
        cx.set_default_font(&["HackGen Console NF"]);
        // Vizia 0.3's default `list list-item { height: 30px }` clips
        // tall rows (e.g. mixer strips) and triggers a Skia matrix-invert
        // panic on auto-sized list items. Override the lists we use.
        let _ = cx.add_stylesheet(LIST_CSS);
        AppData::new(
            audio_tx.clone(),
            plugin_tx.clone(),
            clap_plugin_path.clone(),
            plugin_db.clone(),
        )
        .build(cx);
        register_shortcuts(cx);
        spawn_playhead_poller(cx, Arc::clone(&bridge));
        spawn_autosave_timer(cx);
        if let Some(rx) = incoming_rx_slot.take() {
            spawn_incoming_bridge(cx, rx);
        }

        // Top-level layout:
        //   transport (top)
        //   ┌── inspector (left, fixed) ── arrangement (stretch) ──┐
        //   bottom panel (mixer or piano roll)
        //   status bar
        VStack::new(cx, |cx| {
            build_menu_bar(cx);
            TransportView::new(cx).height(Pixels(44.0));

            HStack::new(cx, |cx| {
                TrackInspectorView::new(cx).width(Pixels(280.0));
                ArrangementView::new(cx)
                    .width(Stretch(1.0))
                    .height(Stretch(1.0));
            })
            .height(Stretch(1.0));

            BottomPanelView::new(cx);

            StatusBarView::new(cx).height(Pixels(26.0));
        });

        // Modal plugin-picker overlay.
        Binding::new(cx, AppData::is_plugin_picker_open, |cx, open| {
            if open.get(cx) {
                PluginPickerView::new(cx);
            }
        });

        // Help / keybindings cheat-sheet.
        Binding::new(cx, AppData::is_help_open, |cx, open| {
            if open.get(cx) {
                build_help_overlay(cx);
            }
        });
    })
    .title("daw_01")
    .inner_size((1280, 800))
    .run()
    .map_err(|e| anyhow::anyhow!("Vizia application error: {e:?}"))
}

/// Bridges the tokio `ChildToMain` receiver to Vizia AppEvents. Runs on a
/// Vizia-spawned background thread (`cx.spawn`), consuming one message at a
/// time via `blocking_recv` and posting the matching `AppEvent`.
fn spawn_incoming_bridge(cx: &mut Context, mut rx: UnboundedReceiver<ChildToMain>) {
    cx.spawn(move |proxy| {
        while let Some(msg) = rx.blocking_recv() {
            let event = match msg {
                ChildToMain::SlotGuiOpened { track, slot, width, height } => {
                    Some(AppEvent::GuiOpenedFromChild { track, slot, width, height })
                }
                ChildToMain::SlotGuiRequestResize { track, slot, width, height } => {
                    Some(AppEvent::GuiRequestResizeFromChild { track, slot, width, height })
                }
                ChildToMain::SlotGuiClosed { track, slot } => {
                    Some(AppEvent::GuiClosedFromChild { track, slot })
                }
                ChildToMain::ExportWavComplete { error } => {
                    Some(AppEvent::ExportWavComplete { error })
                }
                ChildToMain::SlotPluginLoaded { track, slot, id, name } => {
                    Some(AppEvent::SlotPluginLoadedFromChild { track, slot, id, name })
                }
                ChildToMain::SlotPluginState { .. } => None,
                ChildToMain::AllPluginStates { entries } => {
                    Some(AppEvent::AllStatesReceived(entries))
                }
                ChildToMain::Hello { .. } => None,
            };
            if let Some(event) = event
                && proxy.emit(event).is_err()
            {
                break;
            }
        }
        tracing::info!("incoming bridge exited");
    });
}

fn load_or_build_plugin_db() -> Option<Arc<common::plugin_db::PluginDatabase>> {
    use common::plugin_db::{default_cache_path, scan_system, PluginDatabase};
    if let Some(cache) = default_cache_path() {
        match PluginDatabase::load_from_file(&cache) {
            Ok(Some(db)) => {
                tracing::info!(
                    count = db.entries.len(),
                    path = %cache.display(),
                    "loaded cached plugin database"
                );
                return Some(Arc::new(db));
            }
            Ok(None) => {
                tracing::info!(path = %cache.display(), "no cache, scanning…");
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to load cache, scanning");
            }
        }
        match scan_system() {
            Ok(db) => {
                if let Err(e) = db.save_to_file(&cache) {
                    tracing::warn!(error = ?e, "failed to write plugin cache");
                } else {
                    tracing::info!(
                        count = db.entries.len(),
                        path = %cache.display(),
                        "wrote plugin database cache"
                    );
                }
                return Some(Arc::new(db));
            }
            Err(e) => {
                tracing::error!(error = ?e, "plugin scan failed");
            }
        }
    }
    None
}

/// Fires `AppEvent::AutosaveTick` every 30 seconds. The handler decides
/// whether the song is dirty enough to actually write to disk; this loop
/// just provides the cadence.
fn spawn_autosave_timer(cx: &mut Context) {
    cx.spawn(move |proxy| {
        loop {
            std::thread::sleep(Duration::from_secs(30));
            if proxy.emit(AppEvent::AutosaveTick).is_err() {
                break;
            }
        }
    });
}

fn spawn_playhead_poller(cx: &mut Context, bridge: Arc<AudioBridgeHandle>) {
    cx.spawn(move |proxy| {
        let mut peaks_buf: Vec<(f32, f32)> = Vec::with_capacity(common::audio_bridge::MAX_TRACKS);
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let samples = bridge.playhead_samples();
            let (peak_l, peak_r) = bridge.peaks();
            if proxy
                .emit(AppEvent::Tick(samples, peak_l.to_bits(), peak_r.to_bits()))
                .is_err()
            {
                break;
            }
            bridge.track_peaks(&mut peaks_buf);
            let track_peaks: Vec<(u32, u32)> = peaks_buf
                .iter()
                .map(|&(l, r)| (l.to_bits(), r.to_bits()))
                .collect();
            if proxy
                .emit(AppEvent::TrackPeaksTick(track_peaks))
                .is_err()
            {
                break;
            }
        }
    });
}

/// Bitwig-style accelerators. Mouse drives clip / note editing; the
/// keyboard handles transport, save, and `Delete` for the active selection.
fn register_shortcuts(cx: &mut Context) {
    Keymap::from(vec![
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyN),
            KeymapEntry::new(AppEvent::New, |cx| cx.emit(AppEvent::New)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyO),
            KeymapEntry::new(AppEvent::Open, |cx| cx.emit(AppEvent::Open)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyS),
            KeymapEntry::new(AppEvent::Save, |cx| cx.emit(AppEvent::Save)),
        ),
        (
            KeyChord::new(Modifiers::CTRL | Modifiers::SHIFT, Code::KeyS),
            KeymapEntry::new(AppEvent::SaveAs, |cx| cx.emit(AppEvent::SaveAs)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyE),
            KeymapEntry::new(AppEvent::ExportWav, |cx| cx.emit(AppEvent::ExportWav)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::Space),
            KeymapEntry::new(AppEvent::PlayToggle, |cx| cx.emit(AppEvent::PlayToggle)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyZ),
            KeymapEntry::new(AppEvent::Undo, |cx| cx.emit(AppEvent::Undo)),
        ),
        (
            KeyChord::new(Modifiers::CTRL | Modifiers::SHIFT, Code::KeyZ),
            KeymapEntry::new(AppEvent::Redo, |cx| cx.emit(AppEvent::Redo)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyY),
            KeymapEntry::new(AppEvent::Redo, |cx| cx.emit(AppEvent::Redo)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyC),
            KeymapEntry::new(AppEvent::CopySelectedNotes, |cx| {
                cx.emit(AppEvent::CopySelectedNotes)
            }),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyV),
            KeymapEntry::new(AppEvent::PasteNotes, |cx| cx.emit(AppEvent::PasteNotes)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyP),
            KeymapEntry::new(AppEvent::ToggleLoop, |cx| cx.emit(AppEvent::ToggleLoop)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyV),
            KeymapEntry::new(AppEvent::SynthesizeVocal, |cx| {
                cx.emit(AppEvent::SynthesizeVocal)
            }),
        ),
        // Delete: prioritise note selection (piano roll context); fall back
        // to clip selection (arrangement context). Both events are no-ops
        // when their target is empty so the order is safe.
        (
            KeyChord::new(Modifiers::empty(), Code::Delete),
            KeymapEntry::new(AppEvent::DeleteSelectedNotes, |cx| {
                cx.emit(AppEvent::DeleteSelectedNotes);
                cx.emit(AppEvent::DeleteSelectedClip);
            }),
        ),
        // Help / cheat-sheet: F1 toggles, `?` (Shift+/) toggles, Esc closes.
        (
            KeyChord::new(Modifiers::empty(), Code::F1),
            KeymapEntry::new(AppEvent::ToggleHelp, |cx| {
                cx.emit(AppEvent::ToggleHelp)
            }),
        ),
        (
            KeyChord::new(Modifiers::SHIFT, Code::Slash),
            KeymapEntry::new(AppEvent::ToggleHelp, |cx| {
                cx.emit(AppEvent::ToggleHelp)
            }),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::Escape),
            KeymapEntry::new(AppEvent::CloseHelp, |cx| cx.emit(AppEvent::CloseHelp)),
        ),
    ])
    .build(cx);
}

/// Floating overlay that lists every keyboard shortcut. Two columns
/// (`shortcut` / `description`); each row is a single Label so layout
/// stays simple and the overlay never panics on empty content.
const HELP_SHORTCUTS: &[(&str, &str)] = &[
    ("Space", "再生 / 停止"),
    ("P", "ループ ON/OFF"),
    ("V", "VOICEVOX 合成"),
    ("Ctrl+Z", "Undo"),
    ("Ctrl+Shift+Z / Ctrl+Y", "Redo"),
    ("Ctrl+C", "選択ノートをコピー"),
    ("Ctrl+V", "コピーしたノートを貼り付け"),
    ("Delete", "選択ノート / クリップを削除"),
    ("Ctrl+N", "新規プロジェクト"),
    ("Ctrl+O", "プロジェクトを開く"),
    ("Ctrl+S", "保存"),
    ("Ctrl+Shift+S", "名前を付けて保存"),
    ("Ctrl+E", "WAV 書き出し"),
    ("F1 / ?", "このヘルプを開閉"),
    ("Esc", "ヘルプを閉じる / 編集中をキャンセル"),
    ("クリップをダブルクリック", "ピアノロールに開く"),
    ("空白をダブルクリック", "新規クリップ / ノート"),
    ("Shift+クリック", "選択に追加"),
    ("空白でドラッグ", "矩形範囲選択"),
    ("クリップ右端ドラッグ", "リサイズ"),
    ("ホイール", "横スクロール (アレンジ) / 縦スクロール (ピアノロール)"),
    ("Ctrl+ホイール", "ズーム"),
    ("Shift+ホイール", "横スクロール (ピアノロール)"),
    ("ルーラを横ドラッグ", "ループ範囲を設定"),
    ("ルーラをダブルクリック", "ループ範囲を解除"),
];

fn build_help_overlay(cx: &mut Context) {
    VStack::new(cx, |cx| {
        VStack::new(cx, |cx| {
            Label::new(cx, "Keyboard Shortcuts")
                .font_size(16.0)
                .color(Color::rgb(230, 230, 230))
                .padding_bottom(Pixels(8.0));
            for (chord, desc) in HELP_SHORTCUTS {
                HStack::new(cx, |cx| {
                    Label::new(cx, *chord)
                        .font_size(12.0)
                        .color(Color::rgb(180, 200, 240))
                        .width(Pixels(220.0));
                    Label::new(cx, *desc)
                        .font_size(12.0)
                        .color(Color::rgb(220, 220, 220))
                        .width(Stretch(1.0));
                })
                .height(Pixels(20.0));
            }
            HStack::new(cx, |cx| {
                Element::new(cx).width(Stretch(1.0));
                Button::new(cx, |cx| Label::new(cx, "Close (Esc)").font_size(11.0))
                    .on_press(|ex| ex.emit(AppEvent::CloseHelp));
            })
            .padding_top(Pixels(12.0));
        })
        .padding(Pixels(20.0))
        .gap(Pixels(2.0))
        .background_color(Color::rgb(36, 36, 40))
        .width(Pixels(560.0));
    })
    .position_type(PositionType::Absolute)
    .alignment(Alignment::Center)
    .width(Stretch(1.0))
    .height(Stretch(1.0))
    .background_color(Color::rgba(0, 0, 0, 140));
}

/// CSS overrides for the Lists we ship. The defaults' `height: 30px` on
/// `list-item` (and absence of `layout-type: row` on the mixer strip list)
/// cause clipped rows and Skia matrix-invert panics when the inner
/// content's preferred size doesn't match.
const LIST_CSS: &str = r#"
list.track-headers-list list-item {
    height: 56px;
    width: 1s;
}
list.chain-list list-item {
    height: 28px;
}
list.plugin-picker-list list-item {
    height: 30px;
    background-color: rgb(30, 30, 34);
}
list.plugin-picker-list list-item:hover {
    background-color: rgb(70, 70, 78);
}
"#;

fn build_menu_bar(cx: &mut Context) {
    MenuBar::new(cx, |cx| {
        Submenu::new(
            cx,
            |cx| Label::new(cx, "File"),
            |cx| {
                menu_item(cx, "New", "Ctrl+N", AppEvent::New);
                menu_item(cx, "Open...", "Ctrl+O", AppEvent::Open);
                Submenu::new(
                    cx,
                    |cx| Label::new(cx, "Open Recent"),
                    |cx| {
                        // Each entry rebuilds when AppData::recent_paths_display
                        // changes (Save / Open mutates the list).
                        List::new(
                            cx,
                            AppData::recent_paths_display,
                            |cx, _idx, item| {
                                MenuButton::new(
                                    cx,
                                    move |ex| {
                                        let s = item.get(ex);
                                        ex.emit(AppEvent::OpenRecent(s.into()));
                                    },
                                    move |cx| {
                                        Label::new(
                                            cx,
                                            item.map(|s: &String| s.clone()),
                                        )
                                    },
                                );
                            },
                        );
                    },
                );
                Divider::new(cx);
                menu_item(cx, "Save", "Ctrl+S", AppEvent::Save);
                menu_item(cx, "Save As...", "Ctrl+Shift+S", AppEvent::SaveAs);
                Divider::new(cx);
                menu_item(cx, "Export WAV...", "Ctrl+E", AppEvent::ExportWav);
            },
        );
        Submenu::new(
            cx,
            |cx| Label::new(cx, "Edit"),
            |cx| {
                menu_item(cx, "Undo", "Ctrl+Z", AppEvent::Undo);
                menu_item(cx, "Redo", "Ctrl+Shift+Z", AppEvent::Redo);
                Divider::new(cx);
                menu_item(cx, "Copy Notes", "Ctrl+C", AppEvent::CopySelectedNotes);
                menu_item(cx, "Paste Notes", "Ctrl+V", AppEvent::PasteNotes);
                Divider::new(cx);
                menu_item(
                    cx,
                    "Quantize 1/4",
                    "",
                    AppEvent::QuantizeSelectedNotes(1),
                );
                menu_item(
                    cx,
                    "Quantize 1/8",
                    "",
                    AppEvent::QuantizeSelectedNotes(2),
                );
                menu_item(
                    cx,
                    "Quantize 1/16",
                    "",
                    AppEvent::QuantizeSelectedNotes(4),
                );
                menu_item(
                    cx,
                    "Quantize 1/32",
                    "",
                    AppEvent::QuantizeSelectedNotes(8),
                );
            },
        );
        Submenu::new(
            cx,
            |cx| Label::new(cx, "Track"),
            |cx| {
                menu_item(cx, "Add Vocal Track", "", AppEvent::AddVocalTrack);
                menu_item(cx, "Add Instrument Track", "", AppEvent::AddInstrumentTrack);
                menu_item(cx, "Remove Last Track", "", AppEvent::RemoveLastTrack);
            },
        );
    });
}

fn menu_item(cx: &mut Context, label: &'static str, shortcut: &'static str, event: AppEvent) {
    MenuButton::new(
        cx,
        move |cx| cx.emit(event.clone()),
        move |cx| {
            HStack::new(cx, |cx| {
                Label::new(cx, label);
                Label::new(cx, shortcut).class("shortcut");
            })
            .gap(Stretch(1.0))
        },
    );
}
