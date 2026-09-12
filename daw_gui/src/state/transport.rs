//! S3b-1: AppData state group (TransportState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use crate::app::{ExportStage, PendingExport};

/// 再生をどこから・どう始めるか ([`crate::app::AppData::start_transport`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayFrom {
    /// ホーム ([`TransportState::home_beat`]) へ頭出しして `Play`。 Space / Rec。
    Home,
    /// いま見えているプレイヘッド ([`TransportState::playhead_beat`]) から `Play`
    /// (= ランチャーのセルは撃ち直し)。 プラグイン読み込みで一瞬止めた再生の再開。
    /// 停止中にセルを撃ったときは `Home` (Space と同じ頭出し — セルの周回の原点を
    /// 小節線に乗せる)。
    Playhead,
    /// いま見えているプレイヘッドから `PlayContinue` (= 鳴っていたセルも頭出しせず続き)。
    /// Shift+Space (r.md #118)。
    Continue,
}

pub struct TransportState {
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 transport bar の
    /// toggle button で切り替え、 `AppEvent::SetMetronomeEnabled(bool)` で
    /// 更新 → `AudioCommand::SetMetronomeEnabled(bool)` を audio に送信。
    /// audio thread は内蔵 click 音 (sine + linear envelope decay、 accent:
    /// downbeat 880Hz / 他 440Hz) を master mix に重ねる。 起動時 default
    /// false。 session-only (project save には含めない)。
    pub metronome_enabled: bool,

    // -------- Playback / metering --------
    /// r.md #51: audio engine が今 transport を回しているか
    /// (`AudioBridge::playing` の観測ミラー)。
    ///
    /// **writer は `on_tick` ただ 1 箇所**。GUI 側で「Play を送ったから再生中の
    /// はず」という記憶を持つと、engine が自分で止まったとき (曲末 auto-stop /
    /// 書き出し / crash) や、GUI 以外の経路で走り出したときに食い違う。実際、
    /// 旧実装は Rec 単独録音でこれが食い違い、プレイヘッド凍結・オートメーション
    /// 未記録・曲末で止まらない、が一度に起きていた。
    ///
    /// 観測なので Play を押してから true になるまで最大 1 tick (33ms) 遅れる。
    /// プレイヘッド自身が同じ面から来るので、表示上の齟齬は生じない。
    pub is_playing: bool,
    /// r.md #51: count-in の残り samples (`AudioBridge::preroll_remaining_samples`
    /// の観測ミラー)。0 = count-in 中でない。**これ単体で「count-in が終わったか」を
    /// 判定しない** — 0 は「まだ始まっていない」も意味する。録音実体の開始は
    /// `recording.live` を見る。
    pub preroll_remaining: u64,
    /// 再生ループ (ON/OFF + 範囲) の live SSoT。 ループは「作った中身」 ではなく
    /// 「聴き方の都合」 なので `Song` には置かず、 ズーム / スクロールと同じく
    /// ここ (session state) が持ち `ViewState` で永続化する = **変えても dirty
    /// (`*`) にならないが保存される**。 更新は必ず [`AppData::set_loop_region`]
    /// 経由 (state と audio engine への `SetLoop` を 1 か所で揃える)。
    pub loop_region: common::model::LoopRegion,
    /// いま見えているプレイヘッド。 再生中は engine の観測値 (`on_tick`)、 停止中は
    /// **止まった位置** (r.md #121: 停止でホームへ巻き戻さない。 アレンジもピアノロールも
    /// 同じ値を描くので食い違わない) か、 明示 seek で置いた位置。
    pub playhead_beat: Option<f32>,
    /// ホーム = Space (と Rec) が再生を始める位置 (Live の insert marker、 r.md #121)。
    /// 決まるのは **明示 seek** (ruler click / `f` / Home / End) と、 まだ無いときの最初の
    /// 再生開始だけ。 停止では動かない (停止位置は `playhead_beat` に残る)。 `None` = この曲で
    /// まだ一度も置いていない。 アレンジ / ピアノロールのルーラーに ▽ で描く
    /// (`draw_home_marker`)。 Shift+Space ([`crate::app::AppData::play_continue`]) はホームを
    /// 使わず `playhead_beat` から続ける。
    pub home_beat: Option<f32>,
    /// パニックボタンが立てる「遅延 reinit」 の起点時刻。 `Some` の間、
    /// `on_tick` が [`PANIC_REINIT_DELAY`] 経過で `ReinitAllPlugins` を plugin host
    /// に送って `None` に戻す。 master の declick フェードアウト完了後に plugin の
    /// detach を起こすための遅延（段差クリック回避、 [`Self::panic`] 参照）。
    pub panic_reinit_due: Option<std::time::Instant>,
    /// パニックの declick が「ミュート解除待ち」 か。 `panic` で `true`、
    /// `ReinitAllPlugins` の完了通知 `PluginsReinitDone` を受けた時に engine へ
    /// `PanicRelease` を送って `false` に戻す。 ミュート解除を reinit 完了に結び
    /// つけるためのフラグ（[`Self::panic`] 参照）。
    pub panic_release_pending: bool,
    /// r.md #50: マスター出力の全メーター表示状態。テレメトリスレッドの
    /// `MasterAnalyzer` が `MasterMeterTick` で毎ティック更新する。
    ///
    /// 旧 `peak_l_display` / `peak_r_display` / `peak_*_norm` はここに吸収した
    /// (ピークだけ別経路で持つと「同じ音なのに値が違う」を作るため — 計測は
    /// 解析器 1 か所が SSoT)。
    pub master_meter: crate::master_meter::MasterMeterSnapshot,

    // -------- Mixer --------
    /// mixer strip のメーター表示値 `(peak L, peak R, ゲインリダクション dB)`。
    /// GR は **正の減衰量** (0 = 掛かっていない) で持ち、peak と同じ release
    /// 弾道で 0 へ戻る。書き手は `on_track_peaks_tick` の 1 か所。
    pub track_peak_display: Vec<(f32, f32, f32)>,
    /// マスターストリップのゲインリダクション表示値 `(バスコンプ, リミッター)`。
    /// per-track と同じく **正の減衰量 dB** で持ち、同じ release 弾道で 0 へ戻る
    /// (`docs/plan_master_strip.md` §6)。
    pub master_strip_gr: (f32, f32),
    /// docs/plan_modulation.md §4.2 / r.md #89: audio engine が publish した
    /// 変調値面 (**`ModSource::id` キー** — SSoT は `common/src/mod_plane.rs`)。
    /// ~30Hz の `ModScalarsTick` ごとに差し替わり、compose 経路が
    /// `ModPlane::scalar(id)` で引く。
    pub mod_plane: common::mod_plane::ModPlane,
    /// r.md #117: engine が publish した track ごとの鳴っているボイス `(track index, voice)`。
    /// ~30Hz の `TrackVoicesTick` ごとに差し替わり、 変調ラックが `Note` 起点ソースの
    /// カーソルをボイスごとに描くのに使う。
    pub track_voices: Vec<(usize, common::audio_bridge::VoiceSnapshot)>,
    /// `start_transport` が読み込み待ち (`pending_plugin_loads` / asset decode) で queue した
    /// 再生要求。 `Some(どこから)` の間は最後の load 完了で `fire_pending_play` が再発火する。
    pub pending_play: Option<PlayFrom>,
    /// queue された要求が「録音の開始」だったか、だとすれば count-in の長さ
    /// (samples、`0` = count-in 無し)。 録音開始が読み込み待ちで queue された
    /// とき、再発火でも録音と count-in を落とさないために覚えておく (r.md #51)。
    /// `None` = ただの再生。
    pub pending_play_record: Option<u64>,

    /// 進行中 export の現在フェーズ + 進捗 ([`ExportStage`])。音声 freewheel
    /// (標準 WAV export / video 前段) は daw_audio の `ExportWavProgress`、映像
    /// render (video 後段) は daw_gui の `ExportProgress` で更新。`None` = export
    /// 非実行。進捗オーバーレイ表示・入力 gate・再生抑止の単一真実源。
    pub export_stage: Option<ExportStage>,
    /// 音声 freewheel フェーズ (`AudioRender`) の最後に進捗が動いた時刻。export
    /// 開始時と各 `ExportWavProgress` で更新する。`on_tick` の watchdog が、
    /// daw_audio が（crash でなく）hang して完了通知も進捗も来ない状態を検出して
    /// overlay を強制解除するために使う（永久ロック防止）。`None` = 音声 render 非実行。
    pub export_progress_at: Option<std::time::Instant>,
    /// 実行中 export のキャンセルフラグ。UI の Cancel ボタンで `true` にすると
    /// render loop が次フレームで中断し出力を破棄する。`None` = export 非実行。
    pub export_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 1 ステップ video export 中、 音声レンダリング完了待ちの mp4 出力先。
    /// export ダイアログで mp4 を選ぶ → 音声を temp WAV へ自動レンダリング →
    /// `ExportWavComplete` でこの mp4 へ video export（+ WAV mux）を開始する。
    /// `None` = 音声レンダリング待ちでない。
    pub pending_video_export: Option<std::path::PathBuf>,
    /// 自動レンダリングした音声 temp WAV。video export 完了後に削除する。
    pub export_temp_wav: Option<std::path::PathBuf>,
    /// video export 待ちの **拍** レンジ `(start_beat, end_beat)`。
    /// `pending_video_export` と対で立ち、 `ExportWavComplete` で video render を
    /// 始めるときに `RenderConfig::with_range_beats` へ渡す (音声 temp WAV も
    /// 同じ窓に trim 済みなので A/V が揃う)。 `None` = 全曲。
    pub pending_video_export_range: Option<(f64, f64)>,
    /// video export 待ちの出力解像度 `(w, h)` と fps。
    /// `pending_video_export` と対で立ち、 `ExportWavComplete` で video render を
    /// 始めるときに `RenderConfig::with_output_resolution` / `with_output_framerate`
    /// へ渡す per-export override (= export ダイアログで選んだ値。 Song / preview
    /// には永続しない)。 `None` = video export 待ちでない (= プロジェクト値を使用)。
    pub pending_video_export_dims: Option<((u32, u32), f32)>,
    /// a WAV export request held while the plugin host reinitialises
    /// all plugins (deactivate→activate) for a clean offline cold render. Set by
    /// [`Self::begin_wav_export`] (which sends `ReinitAllPlugins`); fired
    /// as `AudioCommand::ExportWav` on `AppEvent::PluginsReinitDone`. Tuple is
    /// `(path, range_frames, write_mod_sidecar)`.
    pub pending_export: Option<PendingExport>,
}

impl TransportState {
    /// 起動時の状態 (停止中、 ループ無し、 メーターは track 数ぶんの無音)。
    /// `track_peak_display` は曲の track 数に揃えた `(peak_l, peak_r, hold)` の列。
    pub fn new(track_peak_display: Vec<(f32, f32, f32)>) -> Self {
        Self {
            metronome_enabled: false,
            is_playing: false,
            preroll_remaining: 0,
            loop_region: common::model::LoopRegion::default(),
            playhead_beat: None,
            home_beat: None,
            panic_reinit_due: None,
            panic_release_pending: false,
            master_meter: crate::master_meter::MasterMeterSnapshot::default(),
            track_peak_display,
            master_strip_gr: (0.0, 0.0),
            mod_plane: common::mod_plane::ModPlane::default(),
            track_voices: Vec::new(),
            pending_play: None,
            pending_play_record: None,
            export_stage: None,
            export_progress_at: None,
            export_cancel: None,
            pending_video_export: None,
            export_temp_wav: None,
            pending_video_export_range: None,
            pending_video_export_dims: None,
            pending_export: None,
        }
    }
}
