//! r.md #87 クリップランチャー — GUI 配線 (束 D) の統合テスト。
//!
//! 1 テスト = 1 つのユーザーシナリオ。widget を出さずに `AppData::handle_event`
//! 経由で「撃つ / 止める / 戻す / 作る / 消す / 貼る / パッドで撃つ」を通す。

use std::sync::Arc;

use common::model::{Clip, ClipContent, LaunchMode, LaunchQuantize, RowPlayback, Track};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event_launcher::{
    LaunchEdit, LauncherCellKey, LauncherEvent, LauncherRow,
};
use daw_gui::state::LauncherFocus;
use daw_gui::widgets::select_modifier::SelectModifier;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    (app, audio_rx, plugin_rx)
}

/// トラック `n` 本 (id = 1..=n) + 列 `scenes` 本の曲を作る (セルはまだ 1 つも無い)。
/// `AppData::new` は既定トラックを持つので **先に空にする** — 残したまま
/// id 1 の track を push すると id が重複し、`track_by_id` が別のトラックを引く。
fn seed(app: &mut AppData, tracks: u32, scenes: usize) {
    app.edit_song(|song| {
        song.tracks.clear();
        song.scenes.clear();
        for i in 0..tracks {
            song.tracks.push(Track { id: i + 1, next_clip_id: 1, ..Track::default() });
        }
        song.ids.next_track_id = tracks + 1;
        for _ in 0..scenes {
            song.push_scene();
        }
    });
}

/// 行 `track_id` の列 `scene_index` にセルを 1 つ置いて、その key を返す。
fn put_cell(app: &mut AppData, track_id: u32, scene_index: usize) -> LauncherCellKey {
    app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell {
        row: LauncherRow::Track(track_id),
        scene_index,
    }));
    let scene_id = app.song_doc.song().scenes[scene_index].id;
    app.cell_in_row_at_scene(LauncherRow::Track(track_id), scene_id)
        .expect("セルが作られている")
}

fn launcher_of(app: &AppData, track_id: u32) -> RowPlayback {
    app.song_doc.song().track_by_id(track_id).expect("track").launcher
}

/// セルを撃つとその行だけがランチャー主導になり、「アレンジに戻す (全行)」で戻る。
#[test]
fn セルを撃つと行の主導権がランチャーへ移り全行戻すで戻る() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 2);
    let cell = put_cell(&mut app, 1, 0);

    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));

    assert_eq!(
        launcher_of(&app, 1),
        RowPlayback::Launcher { clip_id: cell.clip_id() },
        "撃った行はそのセルを鳴らす状態になる"
    );
    assert_eq!(
        launcher_of(&app, 2),
        RowPlayback::Arranger,
        "撃っていない行はアレンジ主導のまま (主導権は行単位)"
    );

    app.handle_event(AppEvent::Launcher(LauncherEvent::AllToArranger));
    assert_eq!(launcher_of(&app, 1), RowPlayback::Arranger);
}

/// シーンを撃つと **全行がランチャーへ移る**。その列にセルがある行は鳴り、
/// 無い行は停止する (計画書 Q11 「空セル = 停止」 + §0「シーンを撃つと主導権を奪う」/
/// Bitwig: "triggering a scene shifts all tracks to Launcher control")。
///
/// アレンジ主導のまま残すと、シーンを撃った直後にアレンジの音とセルの音が混ざる。
#[test]
fn シーン発火は全行をランチャーへ移し空セルの行は停止する() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 3, 2);
    let t1_s0 = put_cell(&mut app, 1, 0);
    let t1_s1 = put_cell(&mut app, 1, 1);
    let t2_s0 = put_cell(&mut app, 2, 0);
    // トラック 3 にはセルを置かない (= アレンジ主導のまま)。

    // 列 0 を撃つ → t1 / t2 が鳴る。
    let scene0 = app.song_doc.song().scenes[0].id;
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchScene {
        scene_id: scene0,
        pressed: true,
    }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::Launcher { clip_id: t1_s0.clip_id() });
    assert_eq!(launcher_of(&app, 2), RowPlayback::Launcher { clip_id: t2_s0.clip_id() });
    assert_eq!(
        launcher_of(&app, 3),
        RowPlayback::LauncherStopped,
        "セルを持たない行もランチャーへ移って停止する (アレンジの音が残らない)"
    );

    // 列 1 を撃つ → t1 は次のセルへ、t2 は空セルなので停止、t3 は据え置き。
    let scene1 = app.song_doc.song().scenes[1].id;
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchScene {
        scene_id: scene1,
        pressed: true,
    }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::Launcher { clip_id: t1_s1.clip_id() });
    assert_eq!(
        launcher_of(&app, 2),
        RowPlayback::LauncherStopped,
        "空セルの列を撃たれた行は停止 (アレンジへは戻さない)"
    );
    assert_eq!(
        launcher_of(&app, 3),
        RowPlayback::LauncherStopped,
        "2 度目のシーン発火でも停止のまま (アレンジへは戻さない)"
    );
}

/// 空きプレースホルダ列にセルを置くと、そこまでの列が実体化する。
/// **開いただけでは列が増えない**という規約 (r.md #9) の裏返しで、
/// 「置いた瞬間に実体化」がここで初めて起きる。
#[test]
fn プレースホルダ列にセルを置くと列が実体化する() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 0);
    assert!(app.song_doc.song().scenes.is_empty(), "最初は列ゼロ");

    // 表示上の 3 列目 (index 2) に置く。
    let cell = put_cell(&mut app, 1, 2);

    assert_eq!(app.song_doc.song().scenes.len(), 3, "途中の列もまとめて実体化する");
    let scene2 = app.song_doc.song().scenes[2].id;
    assert_eq!(app.scene_of_cell(cell), Some(scene2), "置いたセルは 3 列目に乗る");
}

/// `Delete` (= `DeleteSelectedClip`) はセルだけを消し、同じトラックの
/// アレンジのクリップは残す。セルとクリップは同じ `ClipKey` 空間を共有するので、
/// 「どちらの入れ物に居るか」で行き先が決まる。
#[test]
fn セルの削除はアレンジのクリップを巻き込まない() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    // アレンジ側にクリップを 1 本置く。
    app.edit_song(|song| {
        let content = song.alloc_content(ClipContent::default(), String::new());
        let track = &mut song.tracks[0];
        let id = track.alloc_clip_id();
        track.clips.push(Clip { id, start_beat: 0.0, length_beats: 4.0, content_id: content, ..Clip::default() });
    });
    let cell = put_cell(&mut app, 1, 0);
    let arrangement_clip_id = app.song_doc.song().tracks[0].clips[0].id;

    // セルを選んで削除 (`create_launcher_cell` が選択済みだが明示する)。
    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectCell {
        cell,
        modifier: daw_gui::widgets::select_modifier::SelectModifier::Single,
    }));
    app.handle_event(AppEvent::DeleteSelectedClip);

    let track = &app.song_doc.song().tracks[0];
    assert!(track.session_clips.is_empty(), "セルは消える");
    assert_eq!(track.clips.len(), 1, "アレンジのクリップは残る");
    assert_eq!(track.clips[0].id, arrangement_clip_id);
}

/// セルを copy して別の行へ paste すると、同一プロジェクトなので **content を共有** する
/// (= 片方を編集すると両方に効くリンクセル)。
#[test]
fn セルをコピーして別の行に貼るとリンクになる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 1);
    let src = put_cell(&mut app, 1, 0);
    let src_content = app.song_doc.song().tracks[0].session_clips[0].clip.content_id;

    let (json, count) = app.copy_launcher_cells_clip().expect("セルを選択済み");
    assert_eq!(count, 1);
    let env = daw_gui::clipboard::ClipboardEnvelope::from_json(&json).expect("自前 envelope");
    let daw_gui::clipboard::ClipboardPayload::LauncherCells(cells) = env.payload else {
        panic!("セルの payload で運ぶ");
    };

    let pasted = app.paste_launcher_cells(
        cells,
        env.source_project_id,
        LauncherFocus { row: LauncherRow::Track(2), scene_index: 0 },
    );

    assert_eq!(pasted, 1);
    let dst = &app.song_doc.song().tracks[1].session_clips;
    assert_eq!(dst.len(), 1, "トラック 2 にセルが 1 つ増える");
    assert_eq!(dst[0].clip.content_id, src_content, "同一プロジェクトの貼り付けは content 共有");
    assert_eq!(
        app.song_doc.song().tracks[0].session_clips.len(),
        1,
        "コピー元のセルはそのまま残る"
    );
    // id は行ごとの空間なので、貼り先で採り直した値が偶然コピー元と同じでも構わない
    // (`ClipKey` は `track_id` とセットで初めて一意)。
    let _ = src;
}

/// パッドのノートでセルを撃つ: Learn → ノート受信で bind → 以後そのノートで発火する。
/// **bind に当たったノートは音源へ流さない** (パッドで撃つたびに楽器が鳴らない)。
#[test]
fn midi_learn_したノートでセルを撃てる() {
    let (mut app, mut audio_rx, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    let scene_id = app.song_doc.song().scenes[0].id;
    // 録音待機にして「bind に当たらないノートは音源へ流れる」ことも見る。
    app.edit_song(|song| song.tracks[0].armed = true);
    while audio_rx.try_recv().is_ok() {}

    app.handle_event(AppEvent::Launcher(LauncherEvent::StartLearn(
        common::model::BindingTarget::LaunchCell { track_id: 1, scene_id },
    )));
    app.handle_event(AppEvent::MidiNoteOn { channel: 9, pitch: 36, velocity: 100 });

    assert_eq!(app.launcher_bindings().len(), 1, "Learn 中の 1 打で bind される");
    assert_eq!(launcher_of(&app, 1), RowPlayback::Arranger, "bind した打鍵では撃たない");
    assert!(
        !matches!(audio_rx.try_recv(), Ok(AudioCommand::PreviewNoteOn { .. })),
        "bind に消費されたノートは音源へ流さない"
    );

    // 2 打目で発火する。
    app.handle_event(AppEvent::MidiNoteOn { channel: 9, pitch: 36, velocity: 100 });
    assert_eq!(launcher_of(&app, 1), RowPlayback::Launcher { clip_id: cell.clip_id() });

    // 別チャンネルの同じノートは鍵盤の音として素通りする (channel を落とさない)。
    // 直前の発火で `AudioCommand::LaunchCell` がキューに載っているので、
    // 「次に来るのが PreviewNoteOn か」を見る前に捌いておく。
    while audio_rx.try_recv().is_ok() {}
    app.handle_event(AppEvent::MidiNoteOn { channel: 0, pitch: 36, velocity: 100 });
    assert!(
        matches!(audio_rx.try_recv(), Ok(AudioCommand::PreviewNoteOn { .. })),
        "ch 違いは binding に当たらず通常の入力として鳴る"
    );
}

/// インスペクタの一括変更: 選択している全セルに同じローンチ設定が入る。
#[test]
fn ローンチ設定は複数選択へ一括で効く() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 1);
    let a = put_cell(&mut app, 1, 0);
    let b = put_cell(&mut app, 2, 0);

    app.handle_event(AppEvent::Launcher(LauncherEvent::SetLaunchSettings {
        cells: vec![a, b],
        edit: LaunchEdit::Quantize(LaunchQuantize::Bars(4)),
    }));
    app.handle_event(AppEvent::Launcher(LauncherEvent::SetLaunchSettings {
        cells: vec![a, b],
        edit: LaunchEdit::Mode(LaunchMode::Gate),
    }));

    for cell in [a, b] {
        let s = app.launch_settings_of(cell).expect("セルは実在する");
        assert_eq!(s.quantize, LaunchQuantize::Bars(4));
        assert_eq!(s.mode, LaunchMode::Gate);
    }
    assert_eq!(
        app.launch_fold(&[a, b], |s| s.quantize),
        Some(LaunchQuantize::Bars(4)),
        "全部同じなのでインスペクタは値を表示できる"
    );

    // 片方だけ変えると畳めなくなる (= インスペクタは `—` を出す)。
    app.handle_event(AppEvent::Launcher(LauncherEvent::SetLaunchSettings {
        cells: vec![b],
        edit: LaunchEdit::Quantize(LaunchQuantize::Off),
    }));
    assert_eq!(app.launch_fold(&[a, b], |s| s.quantize), None);
}

/// `Gate` のセルは離すと止まり、`Toggle` のセルはもう一度押すと止まる。
/// GUI 側が `Song` に書く「ユーザーが最後に撃った状態」がモードで変わる。
#[test]
fn gate_は離すと止まり_toggle_は再押下で止まる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 2);
    let gate = put_cell(&mut app, 1, 0);
    app.handle_event(AppEvent::Launcher(LauncherEvent::SetLaunchSettings {
        cells: vec![gate],
        edit: LaunchEdit::Mode(LaunchMode::Gate),
    }));

    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: gate, pressed: true }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::Launcher { clip_id: gate.clip_id() });
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: gate, pressed: false }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::LauncherStopped, "Gate は離すと止まる");

    let toggle = put_cell(&mut app, 1, 1);
    app.handle_event(AppEvent::Launcher(LauncherEvent::SetLaunchSettings {
        cells: vec![toggle],
        edit: LaunchEdit::Mode(LaunchMode::Toggle),
    }));
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: toggle, pressed: true }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::Launcher { clip_id: toggle.clip_id() });
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: toggle, pressed: false }));
    assert_eq!(
        launcher_of(&app, 1),
        RowPlayback::Launcher { clip_id: toggle.clip_id() },
        "Toggle は離しても鳴り続ける"
    );
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: toggle, pressed: true }));
    assert_eq!(launcher_of(&app, 1), RowPlayback::LauncherStopped, "Toggle は再押下で止まる");
}

/// Capture: いま鳴っているセルを新しい列として取り込む。**再生は止めない**
/// (押した瞬間に音が変わらないことを優先する)。
#[test]
fn capture_は鳴っているセルを新しい列に取り込む() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 1);
    let t1 = put_cell(&mut app, 1, 0);
    let t2 = put_cell(&mut app, 2, 0);
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell: t1, pressed: true }));
    // トラック 2 は撃たない (= 取り込まれない)。
    let _ = t2;

    app.handle_event(AppEvent::Launcher(LauncherEvent::CaptureScene));

    assert_eq!(app.song_doc.song().scenes.len(), 2, "列が 1 本増える");
    let new_scene = app.song_doc.song().scenes[1].id;
    let captured = app
        .cell_in_row_at_scene(LauncherRow::Track(1), new_scene)
        .expect("鳴っていた行のセルが取り込まれる");
    assert!(
        app.cell_in_row_at_scene(LauncherRow::Track(2), new_scene).is_none(),
        "鳴っていない行は取り込まない"
    );
    // 中身はリンク (同じ content を共有)。
    let src_content = app.song_doc.song().tracks[0].session_clips[0].clip.content_id;
    let new_content = app.song_doc.song().tracks[0]
        .session_clips
        .iter()
        .find(|c| c.clip.id == captured.clip_id())
        .expect("取り込んだセル")
        .clip
        .content_id;
    assert_eq!(new_content, src_content);
    assert_eq!(
        launcher_of(&app, 1),
        RowPlayback::Launcher { clip_id: t1.clip_id() },
        "Capture は再生中のセルを差し替えない"
    );
}

/// 矢印キーのフォーカス移動は、行はグリッドの端で止まり、列は **末尾の
/// 空きプレースホルダ列まで**歩ける (そこにセルを置けるようにするため)。
#[test]
fn フォーカス移動は末尾のプレースホルダ列まで届く() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 2);
    app.handle_event(AppEvent::Launcher(LauncherEvent::FocusCell {
        row: LauncherRow::Track(1),
        scene_index: 0,
    }));

    app.handle_event(AppEvent::Launcher(LauncherEvent::MoveFocus { dx: 5, dy: 0 }));
    assert_eq!(
        app.launcher.focus,
        Some(LauncherFocus { row: LauncherRow::Track(1), scene_index: 2 }),
        "実シーン 2 本 + 末尾のプレースホルダ 1 本まで"
    );

    app.handle_event(AppEvent::Launcher(LauncherEvent::MoveFocus { dx: 0, dy: 5 }));
    assert_eq!(
        app.launcher.focus,
        Some(LauncherFocus { row: LauncherRow::Track(2), scene_index: 2 }),
        "行は最下段で止まる"
    );

    app.handle_event(AppEvent::Launcher(LauncherEvent::MoveFocus { dx: -9, dy: -9 }));
    assert_eq!(
        app.launcher.focus,
        Some(LauncherFocus { row: LauncherRow::Track(1), scene_index: 0 }),
        "左上で止まる"
    );
}

/// セルを持たない列を消すと、鳴っていた行は停止に落ちる (アレンジへは戻さない)。
#[test]
fn 列を消すと鳴っていた行は停止に落ちる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));

    let scene_id = app.song_doc.song().scenes[0].id;
    app.handle_event(AppEvent::Launcher(LauncherEvent::DeleteScenes(vec![scene_id])));

    assert!(app.song_doc.song().scenes.is_empty());
    assert!(app.song_doc.song().tracks[0].session_clips.is_empty(), "列と一緒にセルも消える");
    assert_eq!(
        launcher_of(&app, 1),
        RowPlayback::LauncherStopped,
        "鳴らすものが無くなってもアレンジのクリップが黙って鳴り出さない"
    );
}

/// 複製は「同じ行の右隣で最初に空いている列」へ置く。埋まっていれば列を 1 本足す。
#[test]
fn セルの複製は右隣の空き列へ置く() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 2);
    let a = put_cell(&mut app, 1, 0);
    // 列 1 は空いているのでそこへ。
    app.handle_event(AppEvent::Launcher(LauncherEvent::DuplicateCells {
        cells: vec![a],
        unique: false,
    }));
    let scene1 = app.song_doc.song().scenes[1].id;
    let dup = app
        .cell_in_row_at_scene(LauncherRow::Track(1), scene1)
        .expect("右隣の空き列へ置かれる");
    assert_eq!(app.song_doc.song().scenes.len(), 2, "空きがあるので列は増えない");

    // もう一度複製すると空きが無いので列が 1 本増える。
    app.handle_event(AppEvent::Launcher(LauncherEvent::DuplicateCells {
        cells: vec![dup],
        unique: false,
    }));
    assert_eq!(app.song_doc.song().scenes.len(), 3, "空きが無ければ列を足す");
}

/// 独立複製 (`unique = true`) は content を採り直すので、元と連動しない。
#[test]
fn 独立複製したセルは元と_content_を共有しない() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 2);
    let a = put_cell(&mut app, 1, 0);
    let src_content = app.song_doc.song().tracks[0].session_clips[0].clip.content_id;

    app.handle_event(AppEvent::Launcher(LauncherEvent::DuplicateCells {
        cells: vec![a],
        unique: true,
    }));

    let scene1 = app.song_doc.song().scenes[1].id;
    let dup = app.cell_in_row_at_scene(LauncherRow::Track(1), scene1).expect("複製されている");
    let dup_content = app.song_doc.song().tracks[0]
        .session_clips
        .iter()
        .find(|c| c.clip.id == dup.clip_id())
        .expect("複製セル")
        .clip
        .content_id;
    assert_ne!(dup_content, src_content);
}

/// 撃った状態は「ユーザーが最後に撃ったもの」なので `Song` に入り、
/// **保存対象 (= `*` が立つ)**。フォローアクションの遷移先は入らない
/// (それは engine の走行状態) — ここでは前者だけを固定する。
#[test]
fn セル発火は未保存マークを立てる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    // seed / put_cell で立った dirty をここで一度落とす。
    app.song_doc.mark_saved();
    assert!(!app.song_doc.is_dirty());

    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));
    assert!(app.song_doc.is_dirty(), "撃った状態は曲の一部 (Q10)");

    // 同じセルをもう一度撃っても状態は変わらないので、余計な undo step は積まない。
    let epoch = app.song_doc.edit_epoch();
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));
    assert_eq!(app.song_doc.edit_epoch(), epoch, "同じ状態への再発火で履歴を伸ばさない");
}

/// 空セルの上で `Enter` を押すとその行が止まる (= 空セルは停止)。
#[test]
fn 空セルの上で_enter_を押すと行が止まる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 2);
    let cell = put_cell(&mut app, 1, 0);
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));

    app.handle_event(AppEvent::Launcher(LauncherEvent::FocusCell {
        row: LauncherRow::Track(1),
        scene_index: 1,
    }));
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchFocused));

    assert_eq!(launcher_of(&app, 1), RowPlayback::LauncherStopped);
}

/// セルを別の行へドラッグすると、元の行から消えて先の行に現れる (id は貼り先で採り直す)。
#[test]
fn セルを別の行へ移すと元の行から消える() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 1);
    let src = put_cell(&mut app, 1, 0);

    app.handle_event(AppEvent::Launcher(LauncherEvent::MoveCells {
        moves: vec![daw_gui::event_launcher::LauncherCellMove {
            from: src,
            to_row: LauncherRow::Track(2),
            to_scene_index: 0,
        }],
        mode: daw_gui::event_launcher::LauncherDropMode::Move,
    }));

    assert!(app.song_doc.song().tracks[0].session_clips.is_empty(), "元の行からは消える");
    assert_eq!(app.song_doc.song().tracks[1].session_clips.len(), 1, "先の行に現れる");
}

/// 既にセルがある場所をもう一度叩いても何も起きない。
/// **列だけが増えて undo できない**という故障を防ぐ (`ensure_scene_at` を
/// 「置けると分かってから」呼ぶ契約)。
#[test]
fn 置けないセル作成は列も未保存マークも増やさない() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    put_cell(&mut app, 1, 0);
    app.song_doc.mark_saved();

    // 同じ場所へもう一度 (= 既にセルがある)。
    app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell {
        row: LauncherRow::Track(1),
        scene_index: 0,
    }));
    assert_eq!(app.song_doc.song().scenes.len(), 1, "列は増えない");
    assert!(!app.song_doc.is_dirty(), "何も起きていないので `*` は付かない");

    // 存在しない行へ (= 置けない)。プレースホルダ列を指しても列は作らない。
    app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell {
        row: LauncherRow::Track(99),
        scene_index: 5,
    }));
    assert_eq!(app.song_doc.song().scenes.len(), 1, "置けないなら列も作らない");
    assert!(!app.song_doc.is_dirty());
}

/// 掴んだセルをそのままの位置へ落としても、曲は変わらない。
#[test]
fn 同じ場所へのドロップは未保存マークを立てない() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::Launcher(LauncherEvent::MoveCells {
        moves: vec![daw_gui::event_launcher::LauncherCellMove {
            from: cell,
            to_row: LauncherRow::Track(1),
            to_scene_index: 0,
        }],
        mode: daw_gui::event_launcher::LauncherDropMode::Move,
    }));

    assert!(!app.song_doc.is_dirty());
    assert_eq!(
        app.song_doc.song().tracks[0].session_clips[0].clip.id,
        cell.clip_id(),
        "id も採り直さない"
    );
}

/// セルのダブルクリック (= `OpenCellEditor`) で **ピアノロールがそのセルを開く**。
/// セルのクリップは `Track.session_clips` に居るので、住所が index だった頃は
/// 編集面から**そもそも指せなかった** (開いても対象が解決できず何も出ない)。
#[test]
fn セルを開くとピアノロールの編集対象になる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    app.ui_prefs.bottom_panel = 0;

    app.handle_event(AppEvent::Launcher(LauncherEvent::OpenCellEditor(cell)));

    assert_eq!(app.ui_prefs.bottom_panel, 1, "ピアノロールのタブが開く");
    assert_eq!(
        app.pianoroll_target_clip(),
        Some(common::model::ClipKey { track_id: 1, clip_id: cell.clip_id() }),
        "編集対象がそのセルのクリップ"
    );
}

/// 停止中にセルを撃つと、その操作自体が再生の開始になる (Live / Bitwig と同じ)。
/// ランチャーは transport の拍で走るので、Play を送らないと**音が出ない**。
#[test]
fn 停止中にセルを撃つと再生が始まる() {
    let (mut app, mut audio_rx, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    while audio_rx.try_recv().is_ok() {}

    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));

    let mut sent = Vec::new();
    while let Ok(c) = audio_rx.try_recv() {
        sent.push(c);
    }
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::Play)),
        "撃った瞬間に Play が出る: {sent:?}"
    );
    assert!(
        sent.iter().any(|c| matches!(c, AudioCommand::LaunchCell { pressed: true, .. })),
        "発火自体も送る: {sent:?}"
    );
}

/// 再生中に撃ったときは Play を重ねて送らない (二重に送ると開始位置が戻る)。
#[test]
fn 再生中に撃っても_play_を重ねない() {
    let (mut app, mut audio_rx, _p) = build_app();
    seed(&mut app, 1, 1);
    let cell = put_cell(&mut app, 1, 0);
    app.transport.is_playing = true;
    while audio_rx.try_recv().is_ok() {}

    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchCell { cell, pressed: true }));

    let mut sent = Vec::new();
    while let Ok(c) = audio_rx.try_recv() {
        sent.push(c);
    }
    assert!(!sent.iter().any(|c| matches!(c, AudioCommand::Play)), "Play は出ない: {sent:?}");
}

/// 別トラックのセルを選ぶと、インスペクタが見ているトラック (= カーソル) も
/// そのトラックへ動く。
///
/// 追従が無いと、インスペクタの上半分 (トラック名 / 色 / デバイスチェーン) が
/// **前のトラックのまま**で、下半分 (クリップ / ローンチ設定) だけ新しいセルに
/// なる = 2 つのトラックの情報が 1 画面に混ざる。アレンジのクリップ選択
/// (`select_clip`) は最初から追従していたので、ランチャーだけが非対称だった。
#[test]
fn 別トラックのセルを選ぶとカーソルトラックも追従する() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 3, 2);
    let first = put_cell(&mut app, 1, 0);
    let third = put_cell(&mut app, 3, 1);

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectCell {
        cell: first,
        modifier: SelectModifier::Single,
    }));
    assert_eq!(app.cursor_track_id(), Some(1), "選んだセルのトラックがカーソルになる");

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectCell {
        cell: third,
        modifier: SelectModifier::Single,
    }));
    assert_eq!(
        app.cursor_track_id(),
        Some(3),
        "別トラックのセルへ移ったらカーソルも移る (前のトラックが残らない)"
    );
}

/// シーン見出しをクリックすると列が選択され、**セルを 1 つも持たない列でも**
/// インスペクタがその列のフォローアクションを出せる。
///
/// 列の選択が無かった頃は、列のフォローアクションへ届く唯一の経路が
/// 「その列にセルを持つ行を選ぶ」だった = 空の列は設定不能だった。
#[test]
fn シーンを選ぶと列のフォローアクションが編集対象になる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 3);
    let empty_scene = app.song_doc.song().scenes[2].id;

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectScene {
        scene_id: empty_scene,
        modifier: SelectModifier::Single,
    }));

    assert_eq!(app.selection.selected_scene_ids, vec![empty_scene]);
    app.handle_event(AppEvent::Launcher(LauncherEvent::SetSceneFollow {
        scene_ids: app.selection.selected_scene_ids.clone(),
        edit: LaunchEdit::FollowEnabled(true),
    }));
    let scene = app
        .song_doc
        .song()
        .scenes
        .iter()
        .find(|s| s.id == empty_scene)
        .expect("列は残っている");
    assert!(scene.follow.enabled, "セルの無い列にもフォローアクションを設定できる");
}

/// 列とセルの選択は排他。同じインスペクタ面 (ローンチ) を使うので、両方が
/// 非空だと「セルの設定」と「列の設定」が同時に出てどちらを触っているか
/// 分からなくなる。
#[test]
fn 列を選ぶとセルの選択は落ちその逆も同じ() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 2, 2);
    let cell = put_cell(&mut app, 1, 0);
    let scene = app.song_doc.song().scenes[1].id;

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectCell {
        cell,
        modifier: SelectModifier::Single,
    }));
    assert!(!app.selection.selected_launcher_cells.is_empty(), "セルが選ばれている");

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectScene {
        scene_id: scene,
        modifier: SelectModifier::Single,
    }));
    assert_eq!(app.selection.selected_scene_ids, vec![scene]);
    assert!(app.selection.selected_launcher_cells.is_empty(), "列を選んだらセルの選択は落ちる");

    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectCell {
        cell,
        modifier: SelectModifier::Single,
    }));
    assert!(app.selection.selected_scene_ids.is_empty(), "セルを選んだら列の選択は落ちる");
}

/// 列を消したら、その列を指していた選択も一緒に落ちる
/// (残すとインスペクタが存在しない列の設定を出す)。
#[test]
fn 消えた列は選択からも落ちる() {
    let (mut app, _a, _p) = build_app();
    seed(&mut app, 1, 2);
    let scene = app.song_doc.song().scenes[1].id;
    app.handle_event(AppEvent::Launcher(LauncherEvent::SelectScene {
        scene_id: scene,
        modifier: SelectModifier::Single,
    }));

    app.handle_event(AppEvent::Launcher(LauncherEvent::DeleteScenes(vec![scene])));

    assert!(app.selection.selected_scene_ids.is_empty(), "消えた列は選択に残らない");
}

/// 実体の無い列 (右側のプレースホルダ) を撃つのは「全行停止」なので、
/// **それで再生を始めない**。止めるつもりの操作が鳴り出すのは操作の意味が逆。
#[test]
fn 実体の無い列を撃っても再生は始まらない() {
    let (mut app, mut audio_rx, _p) = build_app();
    seed(&mut app, 2, 1);
    put_cell(&mut app, 1, 0);
    while audio_rx.try_recv().is_ok() {}
    assert!(!app.transport.is_playing, "前提: 停止している");

    // `scene_id = 0` = まだ `Song.scenes` に無い列 (widget の placeholder)。
    app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchScene {
        scene_id: 0,
        pressed: true,
    }));

    let mut sent = Vec::new();
    while let Ok(c) = audio_rx.try_recv() {
        sent.push(c);
    }
    assert!(
        !sent.iter().any(|c| matches!(c, AudioCommand::Play)),
        "全停止で Play を送らない: {sent:?}"
    );
    assert!(!app.transport.is_playing, "全停止で再生が始まらない");
    assert_eq!(launcher_of(&app, 1), RowPlayback::LauncherStopped, "行は停止に落ちる");
}
