//! スクラブ / ドラッグの **undo bracket の唯一の出入口** ([`ScrubGesture`])。
//!
//! インスペクタの数値欄・レーン見出しの既定値欄・ツマミの変調深さドラッグ・
//! 変調ラック・グループ変換は、どれも `Song` を毎フレーム書く。束ねないと
//! 1 ドラッグで数十 undo step が積まれ `UNDO_LIMIT` (200) を溢れさせ、
//! **それ以前の実編集履歴を捨てる**。
//!
//! ## 寿命は「開始した欄が今フレームも描かれている間」
//!
//! 各欄が `Begin` / `End` を自分で撃つ「対」の形は、**片方が来なくなったときに
//! 静かに壊れる** — 欄が画面から消える (選択が変わる / パネルを閉じる /
//! トラックを消す / レーンを別トラックへ運ぶ) と終了側が二度と呼ばれず、
//! 以降の編集が全部 1 undo step へ束ねられ続ける。undo を 1 回押すと関係ない
//! ところまで戻るので、気付いたときには原因が追えない。
//!
//! そこで寿命を**描画の存在**に縛る:
//!
//! 1. 欄は毎フレーム [`push`] を呼ぶ (active / 非 active を問わず)。
//! 2. [`push`] は「自分が今の所有者なら」在席印
//!    ([`UiEphemeral::scrub_gesture_seen`](crate::state::UiEphemeral::scrub_gesture_seen))
//!    を立てる。
//! 3. フレーム末に [`sweep`] が印を見て、立っていなければ gesture を閉じる。
//!
//! **所有者は 1 度に 1 つ**。`SongDoc` の bracket
//! ([`SongDoc::begin_gesture`](crate::state::SongDoc::begin_gesture)) が 1 本しか
//! 無いので、追跡側を面ごとに分けると「A が開けたまま B が閉じる」が黙って作れる。
//!
//! ## 開くのは同じフレームの値より先 (prelude キュー)
//!
//! 欄は `active` (drag が閾値を越えた / 変調深さのドラッグが始まった) を見てから申告するので、欄が
//! 同じフレームに既に積んだ最初の値の Edit より **後ろ** に並ぶ。そのまま適用すると最初の値だけが
//! bracket の外で 1 undo step 積まれる。そこで **開く Edit だけ** を daw-ui core の prelude キュー
//! (`Ui::push_prelude_edit`) に積んで値より先に効かせる。在席印と閉じる Edit は通常キューのまま
//! (離したフレームの最後の値を、閉じる前に適用するため)。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent, ScrubGesture};

/// 欄 1 つぶんの毎フレーム申告。`active` は「いま drag / text 編集中か」。
///
/// **active でないフレームも呼ぶこと** — 呼ばないと [`sweep`] が「欄が消えた」と
/// 判断して gesture を閉じる。呼び忘れは「ドラッグ中に undo bracket が
/// 1 フレームで切れる」形で出る。
pub(crate) fn push(ui: &mut Ui<'_, AppData>, app: &AppData, owner: ScrubGesture, active: bool) {
    let (holds, transition) = push_action(app, &owner, active);
    if holds {
        // 在席印。閉じる側 (`sweep`) はこれが立っていないことだけを根拠にする。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.cur.peph.scrub_gesture_seen = true;
        }));
    }
    let Some(open_it) = transition else {
        return;
    };
    if open_it {
        // 同じフレームの値より先に開く (モジュール doc)。
        ui.push_prelude_edit(Edit::mutate(move |app: &mut AppData| open(app, owner.clone())));
    } else {
        // **閉じるのは自分が所有者のままのときだけ。** 開く Edit は prelude で先に
        // 適用されるので、同じフレームで A が離され B が掴まれると必ず
        // 「B を開く」→「A を閉じる」の順になる。所有者を照合しないと
        // 掴んだばかりの B の bracket をその場で畳んでしまう。
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if app.cur.peph.scrub_gesture.as_ref() == Some(&owner) {
                close(app);
            }
        }));
    }
}

/// [`push`] の判定 (Ui を持たない純関数): `(所有者が自分か, 遷移)`。遷移は `Some(true)` = 開く、
/// `Some(false)` = 閉じる、`None` = 何もしない。所有者の照合は **面を含む** `ScrubGesture` の等値で
/// 行うので、別の面の非アクティブな申告は別の面の bracket に触れない。
fn push_action(app: &AppData, owner: &ScrubGesture, active: bool) -> (bool, Option<bool>) {
    let holds = app.cur.peph.scrub_gesture.as_ref() == Some(owner);
    (holds, (active != holds).then_some(active))
}

/// gesture を開く。既に別の所有者が握っていれば先に閉じる (1 度に 1 本)。
fn open(app: &mut AppData, owner: ScrubGesture) {
    close(app);
    match &owner {
        // グループ変換だけ専用の Begin/End を持つ (`docs/plan_tachie_group_transform.md`)。
        ScrubGesture::GroupTransform { .. } => app.handle_event(AppEvent::BeginGroupTransformDrag),
        ScrubGesture::ModRack => {
            // 係数の再コンパイルを drag 中は伏せる (§3)。
            app.handle_event(AppEvent::SetModFollowerScrubbing(true));
            app.handle_event(AppEvent::BeginInspectorScrub);
        }
        _ => app.handle_event(AppEvent::BeginInspectorScrub),
    }
    app.cur.peph.scrub_gesture = Some(owner);
    app.cur.peph.scrub_gesture_seen = true;
}

/// gesture を閉じる。**立ち下がりでも消滅 ([`sweep`]) でもここ 1 本を通る** —
/// 閉じ方が 2 通りあると、片方だけが副作用 (host 再同期 / 変調の割り当て確定) を
/// 持つ形に育つ。
pub(crate) fn close(app: &mut AppData) {
    let Some(owner) = app.cur.peph.scrub_gesture.take() else {
        return;
    };
    app.cur.peph.scrub_gesture_seen = false;
    match owner {
        ScrubGesture::GroupTransform { .. } => app.handle_event(AppEvent::EndGroupTransformDrag),
        ScrubGesture::ModRack => {
            app.handle_event(AppEvent::SetModFollowerScrubbing(false));
            app.handle_event(AppEvent::EndInspectorScrub);
        }
        ScrubGesture::ModDepth { track_id, target, .. } => {
            app.handle_event(AppEvent::EndInspectorScrub);
            // 立ち下がり = このツマミへの割り当てが完了した瞬間。routing 自体は
            // drag 中に作られているので、ここは解除と通知だけ (`view::modulation`)。
            app.connect_armed_mod_source_to(track_id, target);
        }
        ScrubGesture::Inspector(_) | ScrubGesture::LaneDefault(_) => {
            app.handle_event(AppEvent::EndInspectorScrub);
        }
    }
}

/// フレーム末に 1 回だけ呼ぶ。所有者が今フレーム描かれていなければ閉じる。
/// パラメーターのジェスチャー (面つき所有者) の寿命回収も同じ点で行う (r.md #129 §7.6)。
/// runner が毎フレーム `build_root` の直後に呼ぶ。integration test も同じ順で呼ぶ。
pub fn sweep(app: &mut AppData) {
    if app.cur.peph.scrub_gesture.is_some() && !app.cur.peph.scrub_gesture_seen {
        close(app);
    }
    app.cur.peph.scrub_gesture_seen = false;
    crate::view::param_gesture::sweep_param_gestures(app);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use common::protocol::{AudioCommand, PluginCommand};
    use tokio::sync::mpsc;

    use super::{ScrubGesture, close, open, sweep};
    use crate::app::AppData;
    use crate::app_types::InspectorScrubField;
    use crate::dispatcher::{
        BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
    };

    fn build_app() -> AppData {
        let (audio_tx, _a) = mpsc::unbounded_channel::<AudioCommand>();
        let (plugin_tx, _p) = mpsc::unbounded_channel::<PluginCommand>();
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

    /// **欄が画面から消えても bracket は必ず閉じる。**
    ///
    /// 「消えた」= そのフレームで所有者が [`super::push`] を呼ばなかったこと。
    /// 閉じないと以降の編集が全部 1 undo step へ束ねられ続け、undo 1 回で
    /// 関係ないところまで戻る (しかも画面には何も出ないので気付けない)。
    #[test]
    fn 欄が描かれなくなったフレームで_bracket_が閉じる() {
        let mut app = build_app();
        open(&mut app, ScrubGesture::Inspector(InspectorScrubField::Gain));
        assert!(app.cur.song_doc.gesture_active(), "前提: bracket が開いている");

        // 描かれ続けているフレーム: `push` が在席印を立てた状態で sweep。
        app.cur.peph.scrub_gesture_seen = true;
        sweep(&mut app);
        assert!(app.cur.song_doc.gesture_active(), "描かれている間は開いたまま");
        assert!(app.cur.peph.scrub_gesture.is_some());

        // 欄が消えたフレーム: 在席印が立たない。
        sweep(&mut app);
        assert!(!app.cur.song_doc.gesture_active(), "欄が消えたら閉じる");
        assert!(app.cur.peph.scrub_gesture.is_none());
    }

    /// 所有者は 1 度に 1 つ。別の欄が掴んだら前の bracket は畳まれる
    /// (`SongDoc` の bracket が 1 本しか無いので、2 つ開いた気になれない)。
    #[test]
    fn 別の欄が掴むと前の所有者は降りる() {
        let mut app = build_app();
        open(&mut app, ScrubGesture::Inspector(InspectorScrubField::Gain));
        let group = ScrubGesture::GroupTransform { device_id: 9, param: common::model::GroupTransformParam::X };
        open(&mut app, group.clone());
        assert_eq!(app.cur.peph.scrub_gesture, Some(group));
        assert!(app.cur.song_doc.gesture_active());

        close(&mut app);
        assert!(app.cur.peph.scrub_gesture.is_none());
        assert!(!app.cur.song_doc.gesture_active());
    }

    /// F-G4 (r.md #129): 変調深さの所有者は面つき。Rack で深さをドラッグしている間、同じ
    /// `(track, target)` を描く MixerStrip の非アクティブな申告は bracket を閉じず、◉ も残る。
    #[test]
    fn 別の面の非アクティブな深さ申告は所有者を閉じない() {
        use crate::state::ParamSurface;
        let mut app = build_app();
        let target = common::model::AutomationTarget::TrackBuiltin(common::model::TrackBuiltinParam::Volume);
        let rack = ScrubGesture::ModDepth { surface: ParamSurface::Rack, track_id: 1, target: target.clone() };
        let mixer = ScrubGesture::ModDepth { surface: ParamSurface::MixerStrip, track_id: 1, target };
        app.cur.peph.armed_mod_source = Some(3);
        open(&mut app, rack.clone());
        assert_eq!(super::push_action(&app, &mixer, false), (false, None), "別の面は何もしない");
        assert_eq!(super::push_action(&app, &rack, true), (true, None), "所有者は在席印だけ");
        assert_eq!(app.cur.peph.scrub_gesture, Some(rack));
        assert_eq!(app.cur.peph.armed_mod_source, Some(3), "◉ が残る");
        assert!(app.cur.song_doc.gesture_active());
    }
}
