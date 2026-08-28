//! app.rs から分割した補助 type / free fn / const 群 (view-model / inspector
//! summary / IPC bookkeeping / pure helper)。 挙動は元と同一、 可視性のみ
//! cross-module 用に private -> pub(crate) へ引き上げてある。
use std::collections::{HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc};
use common::model::{ClipContent, MidiContent, Note, Song, Track};
use common::plugin_db::PluginDatabase;

/// `plan_track_removal_ipc` の出力。 順序が deadlock 防止に必須なので
/// テスト可能な enum で表現する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackRemovalIpc {
    /// daw_audio engine に `AudioCommand::ClosePluginShmem { device_id }`
    /// を送る (use-after-free deadlock 防止のため teardown より先)。
    CloseAudioShmem { device_id: u64 },
    /// daw_plugin_host に `PluginCommand::RemoveSlotPlugin { device_id }` を送る
    /// (plugin instance の proper teardown)。 r.md #71 (プラグインのコピー / 移動):
    /// **track という単位は host 側に無い** — 帰属を二重所有すると device 移動で
    /// stale になるので、列挙は Song を持つ daw_gui 側の責務。
    RemoveHostDevice { device_id: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackMixEntry {
    /// Vec position in `song.tracks`。 widget layout (= sequential x 位置 +
    /// peak display lookup) 用。 IPC / AppEvent では track_id を使うこと
    /// (= Phase 6 review で SSOT 違反を解消、 reorder race 防止)。
    pub index: u32,
    /// stable な Track::id。 mixer strip → AppEvent → AudioCommand まで
    /// 一貫してこれで track を識別する。
    pub track_id: u32,
    pub name: String,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    /// raw linear amplitude (`-1.0..=1.0` のうち post-smoothing peak)。
    /// `Ui::level_meter` は内部で dB 変換するので、view 側ではこのまま渡す。
    pub peak_l_raw: f32,
    pub peak_r_raw: f32,
    /// `kind == Group` のとき mixer strip / arrangement で別色表示し、
    /// 子トラックを束ねる sub-mix bus として識別する。
    pub is_group: bool,
    /// このトラックが「リターン」 (= 他トラックの send 宛先) かどうか。
    /// `is_group` と同じく派生値で、`Track::kind` のような field は無い。
    /// mixer がリターンストリップを通常 strip と分けて描画するために使う。
    pub is_return: bool,
    /// このトラックの depth (parent_group_id を辿った段数)。 0 = master 直下、
    /// 1 = 1 段ネスト、… mixer strip / arrangement view が階層インデント描画に使う。
    pub depth: u8,
    /// このトラックの effective 色 (明示上書き or id 由来の導出パレット色、
    /// `track_color::effective_track_color`)。 mixer strip が左端の縦カラー
    /// ストライプに使う (arrangement header の色ストライプと同じ idiom)。
    /// master strip は色を持たず neutral 背景なので使わない。
    pub color: [f32; 3],
}

impl Default for TrackMixEntry {
    fn default() -> Self {
        Self {
            index: u32::MAX,
            track_id: u32::MAX,
            name: String::new(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            peak_l_raw: 0.0,
            peak_r_raw: 0.0,
            is_group: false,
            is_return: false,
            depth: 0,
            color: [0.5, 0.5, 0.5],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPickEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub features: Vec<String>,
    pub format_label: String,
    /// `features` から導出した行き先カテゴリ (種別タグ表示 + 自動振り分け用)。
    pub category: PluginCategory,
}

/// 統合プラグインピッカー (`docs/plan_unified_plugin_picker.md`): プラグインの
/// `features` から導出する「行き先カテゴリ」。優先順 **note-effect > instrument >
/// audio-effect**、 どの主カテゴリも名乗らなければ `Fx` に倒す (= 未分類は FX
/// チェーンへ)。 VST3 の note-effect は Phase B (scan 時 bus 探り) で `features` に
/// `"note-effect"` が乗るまで判定不可なので、 現状 VST3 は instrument / audio-effect
/// の二択。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginCategory {
    Instrument,
    Fx,
    MidiFx,
    /// 内蔵 GPU 映像効果 (`builtin.video.*`、feature `video-effect`)。
    /// GUI 描画パスで処理する device。チェーンに刺さるが audio バスは素通り。
    Video,
}

impl PluginCategory {
    pub fn from_features(features: &[String]) -> Self {
        let has = |k: &str| features.iter().any(|f| f == k);
        if has("video-effect") {
            // 映像効果は audio/note の前に判定 (排他)。
            Self::Video
        } else if has("note-effect") {
            Self::MidiFx
        } else if has("instrument") {
            Self::Instrument
        } else {
            // "audio-effect" も、 どの主カテゴリも持たない未分類も FX チェーンへ。
            Self::Fx
        }
    }

    /// ピッカー行に出す種別タグ。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Instrument => "楽器",
            Self::Fx => "FX",
            Self::MidiFx => "MIDI",
            Self::Video => "映像",
        }
    }
}

/// 単一デバイスチェーン (`docs/plan_linear_chain.md`): プラグインの役割は判定
/// せず engine が port を順に直結するだけ。plugin を追加するときは追加先の
/// `ports` だけが必要 — DB entry の 4 bool (probe で確定する SSoT) から
/// [`PortConfig`] を組む小ヘルパ。
pub(crate) fn port_config_of(e: &common::plugin_db::PluginEntry) -> common::port_config::PortConfig {
    common::port_config::PortConfig {
        has_note_input: e.has_note_input,
        has_note_output: e.has_note_output,
        has_audio_output: e.has_audio_output,
        has_audio_input: e.has_audio_input,
        // 内蔵映像効果のみ video ports を持つ。
        has_video_input: e.has_video_input,
        has_video_output: e.has_video_output,
    }
}

/// `Track::default()` を生成し、`f` で一部 field だけ上書きして返す。
///
/// 単一デバイスチェーン migration で `Track` の legacy field
/// (`legacy_midi_fx_chain` / `legacy_instrument` / `legacy_fx_chain`) が
/// `pub(crate)` (= common 内の functional-update 専用) に降格したため、daw_gui
/// から `Track { name, ..Track::default() }` の functional-update 構文を使うと
/// E0451 (private fields) になる。daw_gui 側の全 Track 構築をこのヘルパ経由に
/// 統一して、 public field だけを上書きする (legacy field は default のまま空)。
pub fn track_with(f: impl FnOnce(&mut Track)) -> Track {
    let mut t = Track::default();
    f(&mut t);
    t
}

impl PluginPickEntry {
    pub(crate) fn from_db_entry(e: &common::plugin_db::PluginEntry) -> Self {
        Self {
            id: e.id.clone(),
            name: if e.name.is_empty() {
                e.id.clone()
            } else {
                e.name.clone()
            },
            vendor: e.vendor.clone(),
            features: e.features.clone(),
            format_label: e.format.as_str().to_string(),
            category: PluginCategory::from_features(&e.features),
        }
    }
}

/// 単一デバイスチェーン上の 1 行 (`docs/plan_linear_chain.md` §5)。役割は持たず
/// (判定もしない)、安定 `device_id` (`PluginInstance::id`) でアドレスする。
/// 表示順は Vec の位置が持つ (r.md #71 プラグインのコピー / 移動:
/// positional index はイベントにも帳簿にも出さない)。表示は plugin 名のみ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEntry {
    pub device_id: u64,
    pub plugin_name: String,
    /// チェーン行ボタンの分岐用。 埋め込み GUI (editor window) を持つ
    /// plugin か (`PluginParamList` の `has_embedded_gui`、 未受信は楽観的に true)。
    pub has_embedded_gui: bool,
    /// この device が内蔵映像 FX (= `ports.is_video()`) か。 映像 FX は専用の
    /// インライン param パネル (`open_video_fx_params`) を持つ。
    pub is_video: bool,
    /// この device が VOICEVOX builtin か (= 声選択パネルを出す対象)。
    pub is_voicevox: bool,
    /// host から param 一覧が届いていて 1 つ以上 param があるか (= 汎用 param
    /// パネルに出す中身がある)。
    pub has_params: bool,
    /// r.md #36: 「キーを全部プラグインに送る」 (= ホストがキーを一切横取りしない)。
    pub send_all_keys: bool,
    /// plugin_host での load が失敗した device の理由 (`SlotPluginLoadFailed`)。
    /// `Some` = song には居るが host には instance が無い = **無音**。
    /// チェーン行に警告色 + ⚠ を出し、 「読み込み失敗」 セクションで理由と
    /// 「再読込」 ボタンを出す (自動リトライはしない)。
    pub load_error: Option<String>,
}

impl ChainEntry {
    /// チェーン行ボタンが「埋め込み GUI window を開く」 のではなく
    /// 「インライン param パネルをトグルする」 種類か。 映像 FX / VOICEVOX /
    /// 埋め込み GUI を持たないが param がある plugin が該当。
    pub fn shows_param_panel(&self) -> bool {
        self.is_video || self.is_voicevox || (!self.has_embedded_gui && self.has_params)
    }

    /// チェーン行にボタンを出すか。 GUI も param パネルも無い device
    /// (= Silence 等の no-op builtin) はボタンを出さない。
    pub fn shows_button(&self) -> bool {
        (self.has_embedded_gui && !self.is_video) || self.shows_param_panel()
    }
}

/// Audio event 単位 field の inspector 表示用 read snapshot (Phase 2 PR1)。
/// `selected_clip` が `ClipContent::Audio` の clip を指していて、 中に
/// 少なくとも 1 event があれば `inspector_audio_event_summary()` が
/// `Some` を返す。 view (`track_inspector::draw`) はこれを使って
/// "Audio Event" section を出し、 toggle / dropdown 操作を `target`
/// に向けて発火する。 Phase 1 で 1 clip = 1 event 前提なので first event
/// を代表値として表示する。
#[derive(Debug, Clone, Copy)]
pub struct InspectorAudioEventSummary {
    /// 編集 AppEvent (`SetClipReversed` 等) の宛先 clip。
    pub target: ClipRef,
    pub reversed: bool,
    pub muted: bool,
    pub stretch_mode: common::model::StretchMode,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
    // scrubable_number に表示する現値 (= first event 代表値)。
    pub gain_db: f32,
    pub pan: f32,
    pub pitch_semitones: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    /// r.md #38: fade scrub の range 上限 = **この event の長さ** (拍)。
    /// fade は clip 長ではなく event 長に対して掛かる (音 / 映像 / 画像 / 字幕の
    /// 適用側が全部 event 長基準)。 handler 側の clamp (`e.event_length_beats`) と
    /// ここが食い違うと、 入力した値が clamp で切られて表示が巻き戻る。
    pub fade_max_beats: f64,
}

/// Image event 単位 field の inspector 表示用 read snapshot
/// (`docs/plan_image_overlay.md` §4 P4)。 `selected_clip` が
/// `ClipContent::Image` の clip を指していて、 中に少なくとも 1 event
/// があれば `inspector_image_event_summary()` が `Some` を返す。
/// view は "Image Event" section を出し、 数値入力 (x/y/w/h/opacity) と
/// fade / mute toggle を first event 代表値として表示する。
///
/// `{x,y,w,h,opacity}_automated` は対応する `ImageBuiltin` lane が
/// 存在する (`Track.automation_lanes` 上の visible 不問) か。 lane が
/// あれば automate toggle は ON 表示で「もう一度押すと削除」、 無ければ
/// OFF 表示で「押すと lane を作る」 動作になる
/// (`docs/plan_image_automation.md` §4.3 / §5)。
#[derive(Debug, Clone, Copy)]
pub struct InspectorImageEventSummary {
    pub target: ClipRef,
    pub muted: bool,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
    pub x_automated: bool,
    pub y_automated: bool,
    pub w_automated: bool,
    pub h_automated: bool,
    pub opacity_automated: bool,
    pub rotation_automated: bool,
    // scrubable_number に表示する現値 (= first event 代表値)。
    // rotation は radians 保持 (view が degree 表示に変換)。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    pub rotation_radians: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    /// r.md #38: fade scrub の range 上限 = **この event の長さ** (拍)。
    /// fade は clip 長ではなく event 長に対して掛かる (音 / 映像 / 画像 / 字幕の
    /// 適用側が全部 event 長基準)。 handler 側の clamp (`e.event_length_beats`) と
    /// ここが食い違うと、 入力した値が clamp で切られて表示が巻き戻る。
    pub fade_max_beats: f64,
}

/// docs/plan_text_overlay.md §4 P5: text inspector の編集対象 numeric
/// field 列挙。 image inspector が field 毎に個別 `SetClipImage*` event を
/// 持つのに対し、 text は 23 field と多いため `SetClipTextNumField` 1 event と
/// `TextNumField` discriminator で集約する。 inspector の数値
/// 入力は scrubable_number 化され、 on_change が直接 `SetClipTextNumField` を
/// 発火する (= 旧 `ClipTextNumEditChanged` / `CommitClipTextNumEdit` の
/// buffer 経路は撤去)。
///
/// FadeInBeats / FadeOutBeats は automation lane 対象外 (= `_automated`
/// は常に false)、 残 21 field は対応する `TextBuiltinParam` 経由で
/// lane override 可能。 `text_num_to_builtin` で `TextBuiltinParam` への
/// 対応 mapping を提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextNumField {
    X,
    Y,
    W,
    H,
    Rotation,
    FontSize,
    Opacity,
    FillR,
    FillG,
    FillB,
    FillA,
    OutlineR,
    OutlineG,
    OutlineB,
    OutlineA,
    OutlineWidth,
    ShadowR,
    ShadowG,
    ShadowB,
    ShadowA,
    ShadowOffsetX,
    ShadowOffsetY,
    ShadowBlur,
    FadeInBeats,
    FadeOutBeats,
}

/// `TextNumField` → `TextBuiltinParam` mapping (lane-eligible のみ)。
/// FadeInBeats / FadeOutBeats は lane 対象外なので `None` を返す。
pub fn text_num_to_builtin(field: TextNumField) -> Option<common::model::TextBuiltinParam> {
    use TextNumField as F;
    use common::model::TextBuiltinParam as B;
    Some(match field {
        F::X => B::X,
        F::Y => B::Y,
        F::W => B::W,
        F::H => B::H,
        F::Rotation => B::Rotation,
        F::FontSize => B::FontSize,
        F::Opacity => B::Opacity,
        F::FillR => B::FillR,
        F::FillG => B::FillG,
        F::FillB => B::FillB,
        F::FillA => B::FillA,
        F::OutlineR => B::OutlineR,
        F::OutlineG => B::OutlineG,
        F::OutlineB => B::OutlineB,
        F::OutlineA => B::OutlineA,
        F::OutlineWidth => B::OutlineWidth,
        F::ShadowR => B::ShadowR,
        F::ShadowG => B::ShadowG,
        F::ShadowB => B::ShadowB,
        F::ShadowA => B::ShadowA,
        F::ShadowOffsetX => B::ShadowOffsetX,
        F::ShadowOffsetY => B::ShadowOffsetY,
        F::ShadowBlur => B::ShadowBlur,
        F::FadeInBeats | F::FadeOutBeats => return None,
    })
}

/// inspector の scrubable_number
/// で drag / text 編集中の field を識別する key。 group transform の
/// `group_scrub_active: Option<GroupTransformParam>` と同 idiom で、 各
/// scrubable の active edge を検知して `BeginInspectorScrub` /
/// `EndInspectorScrub` を 1 undo step に bracket する。 audio / image は
/// fixed な variant、 text は 25 numeric field を `TextNumField` で内包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorScrubField {
    Gain,
    Pan,
    Pitch,
    Formant,
    FadeIn,
    FadeOut,
    ImageX,
    ImageY,
    ImageW,
    ImageH,
    ImageOpacity,
    ImageRotation,
    ImageFadeIn,
    ImageFadeOut,
    Text(TextNumField),
    /// 内蔵映像 FX param scrub（device_id, param_id 単位で
    /// drag stroke を undo 1 step に bracket する）。
    VideoFx { device_id: u64, param_id: u32 },
    /// 汎用 plugin param scrub (「⚙」パネル、device_id, param_id 単位)。
    PluginParam { device_id: u64, param_id: u32 },
    /// talk 読み上げスケール (話速/音高/抑揚/音量) の scrub。
    Talk(TalkParamKind),
}

/// docs/plan_modulation.md §9: one row of the inspector
/// modulation-source rack. `kind` を clone 保持し、 UI が種別別 (follower / LFO /
/// Random / MSEG / Steps) のエディタを出す。
#[derive(Debug, Clone)]
pub struct ModSourceRow {
    pub id: u32,
    pub color: [f32; 3],
    /// Live scalar (`0..=1`) from the polled `mod_scalars` plane — follower env
    /// または generator 値 (engine が全種別を publish)。
    pub scalar: f32,
    /// 変調器種別 + 設定。follower の tap_point (PreFx/PostFx/PostFader、
    /// docs/plan_modulation_followups.md §1) は `EnvelopeFollower { tap }` 内に内包。
    pub kind: common::model::ModSourceKind,
}

/// r.md #78: 変調ラックの「このソースが駆動している接続」1 行。
///
/// **ソース側から見た 1 本の routing** で、 対象がどのトラックにあっても並ぶ。
/// かつては「カーソルトラックの routing」×「カーソルトラック所有のソース」の積
/// でしか描いておらず、 ソース所有トラックと対象トラックが違う routing は
/// どちらのインスペクタにも出ず**削除できなかった** (孤児)。
#[derive(Debug, Clone)]
pub struct ModRoutingRow {
    /// この routing を保持しているトラック (`MASTER_TRACK_ID` → `song_mod_routings`)。
    /// depth / 極性 / 削除の宛先。
    pub track_id: u32,
    pub target: common::model::AutomationTarget,
    /// 表示ラベル。 対象がソース所有トラック以外なら `"<トラック名> \u{25b8} "` 前置き。
    pub label: String,
    pub depth: f32,
    pub bipolar: bool,
}

/// `AddModSource` で作る変調器の種別タグ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModSourceKindTag {
    Follower,
    Lfo,
    Random,
    Mseg,
    Steps,
}

/// generator (LFO/Random/MSEG/Steps) 設定の編集 (consolidated event)。
/// follower の track/attack/release/tap は既存の専用 event を使う。
#[derive(Debug, Clone, PartialEq)]
pub enum ModSourceEdit {
    /// 全 generator 共通の rate。
    Rate(common::model::ModRate),
    /// 全 generator 共通の retrigger。
    Retrigger(common::model::RetriggerMode),
    LfoShape(common::model::LfoShape),
    LfoPhase(f32),
    /// Bitwig 流 Stepped↔Smoothed 連続モーフ (0..=1)。
    RandomSmooth(f32),
    /// 乱数列を引き直す (seed を派生更新)。
    RerollSeed,
    MsegPlayMode(common::model::MsegPlayMode),
    MsegAddPoint {
        time: f32,
        value: f32,
    },
    MsegMovePoint {
        index: usize,
        time: f32,
        value: f32,
    },
    MsegSetCurve {
        segment: usize,
        curve: f32,
    },
    MsegRemovePoint(usize),
    StepsCount(usize),
    StepValue {
        index: usize,
        value: f32,
    },
    StepsDirection(common::model::StepsDirection),
    StepsSlew(f32),
}

/// per-control modulation で、コントロールの *表示* 値ドメインと target の *model*
/// 値ドメイン (= `automation::plain_to_norm` の入力単位) の橋渡し方法。
/// scrubable_number は plain ドメイン (fn ptr 変換)、knob / fader は正規化 0..=1
/// ドメイン (target の `norm_to_plain`/`plain_to_norm` を使う)。`Copy` なので
/// `build_mod` の on_mod_change closure に capture できる。
#[derive(Clone, Copy)]
pub enum ModControlDomain {
    /// 表示値 == target の model plain 値。`to_model`/`to_display` で変換
    /// (恒等 = image/group pos、回転 = deg↔rad)。
    Plain { to_model: fn(f64) -> f64, to_display: fn(f64) -> f64 },
    /// 表示値 == target の正規化 0..=1 (knob のトラック位置 = `plain_to_norm`)。
    Norm,
    /// 表示値 == フェーダーの正規化トラック位置 0..=1 (dB taper、gui_01 #110)。
    /// volume(amp) ↔ frac を `scale` (= フェーダーに渡すのと同じ `MeterScale`) の
    /// dB↔frac 写像で橋渡しする。model depth は従来どおり線形 (volume/2) 空間に
    /// 留まる (engine の `fill_track_param_ramps` がその空間で消費するため)。
    FaderDb(daw_ui_core::MeterScale),
    /// 映像 FX param。表示は実レンジ (`min`..`max`、`log` なら対数)、
    /// model は正規化 0..=1 (`PluginParam` は `plain==norm` なので model==norm)。
    /// `to_model` = 実値→norm、`to_display` = norm→実値 ([`common::video_fx::ParamKind`]
    /// と同写像)。インスペクタの video FX param スクラブ + per-control 変調で使う。
    Ranged { min: f64, max: f64, log: bool },
}

impl ModControlDomain {
    /// コントロール表示値 → target の model plain 値。
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_model(self, target: &common::model::AutomationTarget, display: f64) -> f64 {
        match self {
            ModControlDomain::Plain { to_model, .. } => to_model(display),
            ModControlDomain::Norm => common::automation::norm_to_plain(target, display as f32),
            // frac → volume amp。 `MeterScale::frac_to_amp` に一本化 (r.md #11):
            // 直接ドラッグ (arrangement band / mixer) と同じ変換を使い、 最下端
            // (frac 0) で確実に無音 (amp 0) にする。 旧インライン
            // `10^(frac_to_db/20)` は下端で −60dB の残留 gain を出していた。
            ModControlDomain::FaderDb(scale) => f64::from(scale.frac_to_amp(display as f32)),
            // 実値 → norm (PluginParam は model==norm)。
            ModControlDomain::Ranged { min, max, log } => {
                if log && min > 0.0 && max > 0.0 && display > 0.0 {
                    ((display / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
                } else if (max - min).abs() < f64::EPSILON {
                    0.0
                } else {
                    ((display - min) / (max - min)).clamp(0.0, 1.0)
                }
            }
        }
    }
    /// target の model plain 値 → コントロール表示値。
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_display(self, target: &common::model::AutomationTarget, model: f64) -> f64 {
        match self {
            ModControlDomain::Plain { to_display, .. } => to_display(model),
            ModControlDomain::Norm => f64::from(common::automation::plain_to_norm(target, model)),
            // volume amp → frac。 `MeterScale::amp_to_frac` に一本化 (r.md #11、
            // volume<=0 は −∞dB = frac 下端)。
            ModControlDomain::FaderDb(scale) => f64::from(scale.amp_to_frac(model as f32)),
            // norm (model) → 実値表示。
            ModControlDomain::Ranged { min, max, log } => {
                let n = model.clamp(0.0, 1.0);
                if log && min > 0.0 && max > 0.0 {
                    min * (max / min).powf(n)
                } else {
                    min + n * (max - min)
                }
            }
        }
    }
}

/// docs/plan_modulation_routing_redesign.md §6: per-control modulation data the
/// inspector turns into a gui_01 `Modulation` widget arg. entries / live / armed
/// depth はすべてコントロールの *表示* ドメイン ([`ModControlDomain`]) で持つ
/// (see [`AppData::inspector_mod_data`]).
#[derive(Debug, Clone, Default)]
pub struct InspectorModData {
    /// Assigned routings for this target: `(source color, reachable depth in the
    /// control's *display* domain)`. Computed from the actual reachable value
    /// `to_display(norm_to_plain(base_norm + depth))` so it is exact for affine
    /// (image/group pos), unit-shifted (rotation deg↔rad) and log (scale) targets.
    pub entries: Vec<([f32; 3], f64)>,
    /// The current modulated value (base ⊕ live offset), in the control's
    /// *display* units. `None` when this target has no routings (no live tick).
    pub live_display: Option<f64>,
    /// `Some((source color, current display depth, source_id))` when a source is
    /// armed → the control enters depth-drag edit mode.
    pub armed: Option<([f32; 3], f64, u32)>,
    /// Track owning the routing (`MASTER_TRACK_ID` → song-level).
    pub track_id: u32,
    /// `plain_to_norm(target, to_model(display_base))` — the base in normalized
    /// space, so a dragged display depth maps back to model normalized depth via
    /// `plain_to_norm(to_model(display_base + d)) − base_norm`.
    pub base_norm: f64,
}

/// docs/plan_text_overlay.md §4 P5: text inspector の read snapshot
/// (= image idiom)。 `selected_clip` が `ClipContent::Text` を指していて
/// 中に 1 event 以上あれば `inspector_text_event_summary()` が `Some` を
/// 返す。 `automated` は lane を持つ全 `TextBuiltinParam` の集合 (=
/// inspector 各 「A」 toggle が ON / OFF 判定に使う)。 align / fade curve
/// は dropdown 直値、 `muted` は toggle 直値。
#[derive(Debug, Clone)]
pub struct InspectorTextEventSummary {
    pub target: ClipRef,
    pub muted: bool,
    pub align: common::model::TextAlign,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
    pub automated: std::collections::HashSet<common::model::TextBuiltinParam>,
    /// scrubable_number に表示する現値の供給源 (= first event
    /// snapshot)。 `text_num_field_value` で field 毎の f64 を取り出す
    /// (Rotation は degree に変換)。
    pub event: common::model::TextEvent,
    /// r.md #38: fade scrub の range 上限 = **この event の長さ** (拍)。
    /// fade は clip 長ではなく event 長に対して掛かる (音 / 映像 / 画像 / 字幕の
    /// 適用側が全部 event 長基準)。 handler 側の clamp (`e.event_length_beats`) と
    /// ここが食い違うと、 入力した値が clamp で切られて表示が巻き戻る。
    pub fade_max_beats: f64,
}

impl InspectorTextEventSummary {
    /// scrubable_number の `value` 引数に渡す現値を field 毎に
    /// 取り出す。 Rotation は内部 radians を degree に変換して返す
    /// (= 旧 text_input が degree 表示だったのと整合、 on_change で
    /// radians に戻す)。
    pub fn text_num_field_value(&self, field: TextNumField) -> f64 {
        text_event_num_value(&self.event, field)
    }
}

/// `TextEvent` 1 つから `TextNumField` の現値 (f64) を取り出す。
/// `InspectorTextEventSummary::text_num_field_value` (= アンカー代表値) と
/// inspector の mixed 畳み込み (`inspector_text_num_folded`、 = 他クリップの event)
/// の両方から使う single source。 Rotation は内部 radians を degree に変換。
pub fn text_event_num_value(ev: &common::model::TextEvent, field: TextNumField) -> f64 {
    use TextNumField as F;
    match field {
        F::X => ev.x.into(),
        F::Y => ev.y.into(),
        F::W => ev.w.into(),
        F::H => ev.h.into(),
        F::Rotation => ev.rotation_radians.to_degrees().into(),
        F::FontSize => ev.font_size_px.into(),
        F::Opacity => ev.opacity.into(),
        F::FillR => ev.fill_color[0].into(),
        F::FillG => ev.fill_color[1].into(),
        F::FillB => ev.fill_color[2].into(),
        F::FillA => ev.fill_color[3].into(),
        F::OutlineR => ev.outline_color[0].into(),
        F::OutlineG => ev.outline_color[1].into(),
        F::OutlineB => ev.outline_color[2].into(),
        F::OutlineA => ev.outline_color[3].into(),
        F::OutlineWidth => ev.outline_width_px.into(),
        F::ShadowR => ev.shadow_color[0].into(),
        F::ShadowG => ev.shadow_color[1].into(),
        F::ShadowB => ev.shadow_color[2].into(),
        F::ShadowA => ev.shadow_color[3].into(),
        F::ShadowOffsetX => ev.shadow_offset_px.0.into(),
        F::ShadowOffsetY => ev.shadow_offset_px.1.into(),
        F::ShadowBlur => ev.shadow_blur_px.into(),
        F::FadeInBeats => ev.fade_in_beats,
        F::FadeOutBeats => ev.fade_out_beats,
    }
}

/// Per-plugin sidechain wiring entry shown in the inspector. One row per
/// chain device (addressed by stable `device_id`); the `current_source` field
/// is the value of `PluginInstance::aux_inputs[0]` tap source (port 0; the
/// inspector only exposes the first aux input port for now). `track_id` は
/// source picker が自トラックを除外するのに要るので残す (アドレスには使わない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainEntry {
    pub track_id: u32,
    pub device_id: u64,
    pub plugin_name: String,
    pub current_source: Option<u32>,
    /// aux_inputs[0] の現 tap point (B8 / r.md #8: inspector で編集可能化)。
    /// route 未設定は `PostFader` 既定。
    pub current_tap_point: common::model::TapPoint,
}

/// Sidechain source picker choice: `None` = "—" (disconnected),
/// `Some(track_id)` = a specific track. Self-track is filtered out by
/// the picker because feeding a track its own output into a sidechain
/// creates a feedback loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainSourceChoice {
    pub label: String,
    pub track_id: Option<u32>,
}

/// パラアウト (docs/plan_paraout.md): one inspector row group per chain device
/// that declares `is_main=false` audio outputs (`aux_output_count > 0`). The
/// inspector shows an "explode" button (auto-create child tracks) plus a
/// per-port destination dropdown. `routes[port]` = the current
/// `PluginInstance::aux_outputs[port]` destination track id (`None` = unrouted
/// = silent, the industry-standard default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelOutputEntry {
    pub track_id: u32,
    pub device_id: u64,
    pub plugin_name: String,
    pub aux_output_count: u8,
    pub routes: Vec<Option<u32>>,
    /// True once at least one aux output is routed (so the inspector shows the
    /// per-port dropdowns instead of just the explode button).
    pub exploded: bool,
}

/// 「＋ Send」 ボタンで開く宛先トラックピッカーの状態。 plugin_picker の
/// `is_plugin_picker_open` と同 idiom で、 開いている間 `Some(..)` を保持し、
/// `src_track_id` (= send 元) を覚えておく。 track_picker.rs がこれを見て
/// modal を開閉する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPickerState {
    /// この track に新しい send を追加する (= send 元)。
    pub src_track_id: u32,
}

/// どの種類の export がレンジピッカーを開いたか。 ピッカー確定後に
/// 元の export action へ戻るための分岐に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportRangeKind {
    /// File → Export WAV...
    Wav,
    /// File → Export Video... (mp4)。 Windows 専用。
    Mp4,
    /// r.md #54: 解析 → ラウドネス解析... (ファイルは書かず値だけ出す)。
    Loudness,
}

impl ExportRangeKind {
    /// ピッカーのタイトル。
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Wav => "WAV 書き出し範囲",
            Self::Mp4 => "Video 書き出し設定",
            Self::Loudness => "ラウドネス解析の範囲",
        }
    }

    /// 確定ボタンのラベル。
    #[must_use]
    pub fn confirm_label(self) -> &'static str {
        match self {
            Self::Wav | Self::Mp4 => "書き出す...",
            Self::Loudness => "解析",
        }
    }
}

/// レンジピッカーのワンクリック範囲プリセット (r.md #54)。
/// 「今の関心領域」を拍で言い直すだけの純粋な写像で、どれも同じ
/// `start_beat` / `end_beat` を書き換える (= ピッカーの値は 1 つ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportRangeSource {
    /// transport のループ帯。
    Loop,
    /// いま選択しているクリップ / ノートを包む範囲。
    Selection,
    /// プレイヘッドが乗っているセクション。
    Section,
    /// 曲全体 (0 .. `length_beats`)。
    Whole,
}

impl ExportRangeSource {
    /// ボタンに出すラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Loop => "ループ範囲",
            Self::Selection => "選択範囲",
            Self::Section => "セクション",
            Self::Whole => "曲全体",
        }
    }

    /// ピッカーに並べる順 (左から)。
    pub const ALL: [Self; 4] = [Self::Loop, Self::Selection, Self::Section, Self::Whole];
}

/// Export WAV / Video の前に出すレンジピッカーモーダルの状態。
/// `Some` の間だけ `export_range_modal` が描画され、 下の UI 操作をブロック
/// する。 Ardour / REAPER の time-selection export に倣い、 ユーザーが書き出す
/// 時間範囲を **拍 (beat)** で選ぶ。 拍は song の native 単位なので audio
/// (beat→sample) と video (frame→秒→拍) の両 export が同じ窓で揃い、 A/V sync
/// が崩れない。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportRangePicker {
    /// 開始拍 (0 以上、 `end_beat` 未満)。
    pub start_beat: f64,
    /// 終了拍 (`start_beat` より大、 song 長以下)。
    pub end_beat: f64,
    /// 確定後に戻る export 種別。
    pub kind: ExportRangeKind,
    /// video export の出力解像度 `(width, height)`。 picker を開いた
    /// 時点で `Song.video_resolution` を seed し、 dropdown で変更する。 確定時に
    /// `RenderConfig.output_resolution` へ渡る per-export override (Song /
    /// preview には永続しない)。 `Wav` では未使用。
    pub resolution: (u32, u32),
    /// video export の出力フレームレート。 picker を開いた時点で
    /// `Song.video_framerate` を seed し、 dropdown で変更する。 `Wav` では未使用。
    pub framerate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct ClipRef {
    pub track: u32,
    pub clip: u32,
}

/// r.md #38: clip 内の 1 event を指す参照。 `ClipRef` (track index / clip index) の延長で、
/// `event` は clip の `ClipContent` 内 event index。
///
/// fade はこの粒度で編集する。 clip 単位だと複数 event を持つ clip で
/// 「アレンジ画面で掴んだ event」 と 「実際に書き換わる event」 がずれる
/// (旧実装は clip 内全 event に broadcast していた)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct ClipEventRef {
    pub clip: ClipRef,
    pub event: u32,
}

/// v18 (`docs/plan_track_clip_color.md`): color_picker overlay (gui_01 #058)
/// の編集対象。`Some` の間 arrangement_view が 1 フレームごとに
/// `ui.color_picker` を呼んで overlay を描画する。`Track` は track id、
/// `Clip` は index ベース `ClipRef`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerTarget {
    Track(u32),
    Clip(ClipRef),
    /// Arranger セクション帯の色。
    Section(u32),
}

/// gui_01 #028: 1 point の addressing。daw_01 側は `(track_id, lane_id,
/// clip_id, point_idx)` 4-tuple で持つ (gui_01 の `AutomationPointKey`
/// と 1:1 対応)。`AppEvent::DeleteAutomationPoints` などの batch event
/// で複数受ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AutomationPointKeyRef {
    pub track_id: u32,
    pub lane_id: u32,
    pub clip_id: u32,
    pub point_idx: u32,
}

/// 編集面 (copy / cut / delete / duplicate / zoom の対象)。
///
/// 直交して共存できる選択集合 (audio event / note / automation point /
/// automation clip / clip / track / section) は同時に非空になり得るので、
/// 「最後に選んだ面」 (last-wins) を `SelectionState::last_edit_select` が保持し、
/// [`AppData::edit_surface`](crate::app::AppData::edit_surface) がポインタ面 →
/// last-wins → 非空優先順の順で対象面を 1 つに解決する。 各選択 setter が
/// 「選択が非空になったとき」 にタグを更新する (空クリアでは面は移らない)。
/// 固定 type 優先順位 tier だと「クリップを選択して Del したのに残存 automation
/// 点が消える」 (#071) が面跨ぎで再発するため、 選択の時系列を単一 SSoT で追う。
///
/// タグ (「最後に選んだ面」) と arbiter の解決結果は **同じ値空間** なので
/// 単一 enum で表す (旧実装は `EditSelectSurface` と view 私有の `EditSurface`
/// に分裂し、 `edit_surface` が 1:1 で翻訳する mirror 型になっていた)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSurface {
    AudioEvents,
    Notes,
    AutomationPoints,
    AutomationClips,
    Clips,
    /// トラック面 (ヘッダ / ミキサーストリップ)。 **明示的に選んだときだけ**
    /// 立つ — `selected_track_ids` はクリップ選択の追従 (`select_track`) や
    /// 削除後の自動再選択でも非空になるので、 非空を「削除意図」 の代理にできない
    /// (`edit_surface` の非空優先順 fallback から外してある)。
    Tracks,
    /// Arranger セクション帯 (選択中なら Delete で帯削除)。
    Sections,
    /// r.md #71 (プラグインのコピー / 移動): インスペクタの Chain 行
    /// (選択中のプラグイン)。 **明示的に行を click したときだけ**立つので、
    /// `edit_surface` の非空優先順 fallback には入れない (タグ経由の
    /// last-wins だけで足りる)。
    Devices,
}

/// gui_01 #028: `MoveAutomationPoints` 用の 1 point delta。`value_norm`
/// は normalized 0..1 (widget が cursor 座標から計算した値)、handler が
/// `lane.target` を引いて plain 単位に逆変換する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveAutomationPointEntry {
    pub key: AutomationPointKeyRef,
    pub prev_time_beat: f64,
    pub prev_value_norm: f32,
    pub next_time_beat: f64,
    pub next_value_norm: f32,
}

/// gui_01 #028 (Phase 63n-3): `MoveAutomationClips` /
/// `CloneAutomationClipsLinked` / `CloneAutomationClipsIndependent` 用
/// の 1 clip delta。`from` source clip → `to_lane` の `next_start_beat`
/// 位置へ移動 / 共有コピー / 独立コピー。lane 跨ぎは target 不一致でも
/// 全 accept (Bitwig 流、`docs/plan_automation.md` §5.4)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveAutomationClipEntry {
    pub from: common::model::AutomationClipKey,
    pub to_lane: common::model::AutomationLaneKey,
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

/// gui_01 #028 (Phase 63n-3): `ResizeAutomationClips` 用の 1 clip delta。
/// 左 edge drag は `next_start` + `next_len` 両方変動、右 edge drag は
/// `next_len` のみ変動 (`prev_start == next_start`)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeAutomationClipEntry {
    pub key: common::model::AutomationClipKey,
    pub prev_start: f64,
    pub prev_len: f64,
    pub next_start: f64,
    pub next_len: f64,
}

/// 立ち絵 group transform inspector 用 snapshot（automate トグルの点灯状態）。
/// `docs/plan_tachie_group_transform.md` §5.5。
pub struct GroupTransformInspectorSummary {
    pub track_id: u32,
    /// `group_param_index` 順に、各 param の `GroupTransform` lane の有無。
    pub automated: [bool; 8],
    /// 編集対象の base transform（`group_transform` 無しなら default）。
    /// inspector の scrubable_number が毎フレーム現値として表示する。
    pub transform: common::model::GroupTransform,
}

/// 開いている内蔵映像 FX param パネルのデータ（inspector が
/// scrubable_number 行に展開する）。`def` はカタログ定義、`values` は `def.params`
/// 同順の現在実値（lane default_value or manifest default を実レンジへ展開）。
#[derive(Clone)]
pub struct VideoFxParamsInspector {
    pub track_id: u32,
    pub device_id: u64,
    pub def: &'static common::video_fx::VideoFxDef,
    pub values: Vec<f32>,
}

/// 埋め込み GUI を持たない plugin の「⚙」インライン param パネルの
/// read snapshot。 `open_plugin_params` が cursor track の device を指すとき
/// `inspector_plugin_params()` が返す。 VOICEVOX builtin は `voice` に device
/// 既定の声を、 汎用 plugin は `params` に編集可能な param 行を載せる。
pub struct PluginParamsInspector {
    pub track_id: u32,
    pub device_id: u64,
    pub plugin_name: String,
    /// 汎用 plugin param 行 (lane default_value を実レンジ化した現値つき)。
    pub params: Vec<PluginParamRow>,
}

/// `PluginParamsInspector` の 1 param 行 (実レンジ表示 + 編集レンジ情報)。
pub struct PluginParamRow {
    pub id: u32,
    pub name: String,
    pub value_real: f64,
    /// plugin の default value (実レンジ)。 scrubable のダブルクリックリセット用。
    pub default_real: f64,
    pub min: f64,
    pub max: f64,
    /// `STEPPED` フラグ (= 整数ステップ)。 表示フォーマットの分岐に使う。
    pub stepped: bool,
    /// `READONLY` フラグ。 編集不可なので scrubable でなくラベル表示にする。
    pub readonly: bool,
}

/// inspector / resync が走査する `GroupTransformParam` の固定順
/// （`group_param_index` の index と一致）。
pub const GROUP_PARAMS: [common::model::GroupTransformParam; 8] = {
    use common::model::GroupTransformParam as G;
    [
        G::X,
        G::Y,
        G::Rotation,
        G::ScaleX,
        G::ScaleY,
        G::AnchorX,
        G::AnchorY,
        G::Opacity,
    ]
};

/// `GroupTransformParam` の edit buffer / `automated` 配列 index（固定順）。
pub fn group_param_index(p: common::model::GroupTransformParam) -> usize {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => 0,
        G::Y => 1,
        G::Rotation => 2,
        G::ScaleX => 3,
        G::ScaleY => 4,
        G::AnchorX => 5,
        G::AnchorY => 6,
        G::Opacity => 7,
    }
}

/// inspector ラベル / status 用の短い param 名。
pub fn group_param_label(p: common::model::GroupTransformParam) -> &'static str {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => "X",
        G::Y => "Y",
        G::Rotation => "Rotation",
        G::ScaleX => "ScaleX",
        G::ScaleY => "ScaleY",
        G::AnchorX => "AnchorX",
        G::AnchorY => "AnchorY",
        G::Opacity => "Opacity",
    }
}

/// `GroupTransform` の指定 param の plain 値を取り出す。
pub(crate) fn group_transform_field(
    gt: &common::model::GroupTransform,
    p: common::model::GroupTransformParam,
) -> f32 {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => gt.x,
        G::Y => gt.y,
        G::Rotation => gt.rotation_radians,
        G::ScaleX => gt.scale_x,
        G::ScaleY => gt.scale_y,
        G::AnchorX => gt.anchor_x,
        G::AnchorY => gt.anchor_y,
        G::Opacity => gt.opacity,
    }
}

/// gui_01 #028 §7.3: `AutomationTarget` に対する人間可読 display name。
/// Inspector の knob hint や status_message で使う。`Plugin Param N` は
/// Phase 2 で IPC 経由で実 plugin の param name に置換する。
pub fn automation_target_display_name(
    target: &common::model::AutomationTarget,
) -> String {
    use common::model::{AutomationTarget, ImageBuiltinParam, TrackBuiltinParam};
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => "Volume".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => "Pan".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => "Mute".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, .. }) => {
            // v29: 安定 send id (1 始まり)。 位置ベースの連番表示は S3b で
            // 「track の sends 内位置」 を引く形に戻す予定 (ここは song 非依存
            // の pure label なので id をそのまま出す)。
            format!("Send {send_id}")
        }
        AutomationTarget::PluginParam { param_id, .. } => format!("Param {param_id}"),
        AutomationTarget::SongTempo => "Tempo".into(),
        AutomationTarget::SongTimeSigNumerator => "Time Sig".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::X) => "Image X".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y) => "Image Y".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::W) => "Image W".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::H) => "Image H".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity) => "Image Opacity".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation) => "Image Rotation".into(),
        AutomationTarget::TextBuiltin(p) => {
            use common::model::TextBuiltinParam as T;
            match p {
                T::X => "Text X".into(),
                T::Y => "Text Y".into(),
                T::W => "Text W".into(),
                T::H => "Text H".into(),
                T::Opacity => "Text Opacity".into(),
                T::Rotation => "Text Rotation".into(),
                T::FontSize => "Text FontSize".into(),
                T::FillR => "Text Fill R".into(),
                T::FillG => "Text Fill G".into(),
                T::FillB => "Text Fill B".into(),
                T::FillA => "Text Fill A".into(),
                T::OutlineR => "Text Outline R".into(),
                T::OutlineG => "Text Outline G".into(),
                T::OutlineB => "Text Outline B".into(),
                T::OutlineA => "Text Outline A".into(),
                T::OutlineWidth => "Text Outline Width".into(),
                T::ShadowR => "Text Shadow R".into(),
                T::ShadowG => "Text Shadow G".into(),
                T::ShadowB => "Text Shadow B".into(),
                T::ShadowA => "Text Shadow A".into(),
                T::ShadowOffsetX => "Text Shadow OffsetX".into(),
                T::ShadowOffsetY => "Text Shadow OffsetY".into(),
                T::ShadowBlur => "Text Shadow Blur".into(),
            }
        }
        AutomationTarget::GroupTransform(p) => {
            use common::model::GroupTransformParam as G;
            match p {
                G::X => "Group X".into(),
                G::Y => "Group Y".into(),
                G::Rotation => "Group Rotation".into(),
                G::ScaleX => "Group ScaleX".into(),
                G::ScaleY => "Group ScaleY".into(),
                G::AnchorX => "Group AnchorX".into(),
                G::AnchorY => "Group AnchorY".into(),
                G::Opacity => "Group Opacity".into(),
            }
        }
    }
}

/// gui_01 #028 §7.3: 最後にユーザーが触った parameter。`A` キー
/// shortcut で「現在 selected_track の lane に追加」 する際の source。
/// session-only (起動時 None、Undo / save 対象外)。
///
/// `track_id` は Bitwig 流に「touched parameter が **属する track**」
/// (selected_track ではなく)。これにより別 track のプラグインを
/// inspector で触った後 `A` を押すと、その plugin が乗る track 上に
/// lane ができる (= 期待動作)。
#[derive(Debug, Clone)]
pub struct TouchedParam {
    pub track_id: u32,
    pub target: common::model::AutomationTarget,
    /// inspector の hint 表示や status_message で使う名前
    /// ("Volume" / "Pan" / "Cutoff (Serum)" 等)。
    pub display_name: String,
    /// 設定時刻。stale 判定 (= track / plugin が削除されたあとの自動
    /// クリア) 用。
    pub touched_at: std::time::Instant,
}

pub const ARRANGE_PX_PER_BEAT: f32 = 24.0;

pub const ARRANGE_TRACK_HEIGHT: f32 = 88.0;

/// arrangement の 1 行 (track 行 / automation lane 行) の最小高さ (px)。
/// Alt+wheel の縦ズームも `X`/`Z` の自動フィットもここで下げ止まる。
/// **上限は設けない** — 「全体表示」 は viewport を埋めるのが定義で、 上限があると
/// 行数が少ないとき下半分が空く (Ardour `Editor::fit_tracks` も下限のみ)。
pub const MIN_ARRANGE_ROW_H: f32 = 16.0;

/// arrangement の 1 行の最大高さ (px)。 UI 操作 (`SetArrangeTrackRowH`) の暴走ガードで、
/// 自動フィットは viewport 高さそのものが上限になるので使わない。
pub const MAX_ARRANGE_ROW_H: f32 = 2000.0;

pub const DEFAULT_NOTE_DURATION: f64 = 0.25;

pub const DEFAULT_CLIP_LENGTH: f64 = 4.0;

/// export レンジの最小幅 (拍)。 start == end の縮退で 0 フレームの
/// 出力を作らないよう、 end は常に start + これ以上に保つ。
pub const MIN_EXPORT_RANGE_BEATS: f64 = 0.25;

/// 鍵盤レーン click のプレビュー発音 velocity (MIDI 0..=127、 固定値)。
/// gui_01 #055: widget は押下 pitch のみ返すので velocity は daw_01 側で固定。
pub(crate) const PREVIEW_VELOCITY: u8 = 100;

/// パニックボタンで `AudioCommand::Panic`（master declick）を送ってから
/// `ReinitAllPlugins` を送るまでの遅延。 audio engine が次の buffer で declick を
/// 開始し fade-out（5ms）し切るまで（= buffer 1 個分 + 5ms ≒ 最大数十 ms）master
/// が 0 になるのを待ってから plugin を mix から外す。 80ms あれば大きめの buffer
/// でも余裕。 `on_tick`（30Hz ≒ 33ms 間隔）が経過判定する。
pub(crate) const PANIC_REINIT_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

/// Audio Editor zoom の最小 view span (beats)。 1/64 拍 = 約 0.015 beats。
/// これ未満は描画上意味がなく `view_len` を 0 に近づけると `beats_per_px`
/// が発散するので clamp。
pub const MIN_AUDIO_EDITOR_VIEW_LEN_BEATS: f64 = 1.0 / 64.0;

/// Phase 2 PR-C: 進行中の plugin-FX bounce の追跡 entry。
/// `AudioCommand::BounceClipFxOnline` 発火時に `AppData::pending_clip_fx_bounce
/// = Some(...)` でセット、 `AudioEvent::BounceClipFxComplete` 受信で
/// 完了処理 (= 新 audio source + 新 track + 新 Clip 配置) → `None` に戻す。
/// `path` / `source_track` / `source_clip` は IPC echo back と pending entry
/// の identifier 照合に使う。 `clip_name` / `clip_length_beats` /
/// `start_beat` は完了時の新 track / 新 Clip の名前 / 配置に使う。
/// `source_path` は完了時に AudioSource として登録するときの
/// `AudioSourcePath` (= ProjectRelative or Absolute、 outpath が
/// `<project_dir>/bounce/...` か `bounce_cache/...` かで決まる)。
/// bounce の 2 モード。 完了 handler はこれで分岐する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BounceMode {
    /// 音源/synth の素の音 (= insert FX 抜き) を **同じクリップに置換**。
    InPlace,
    /// 音源/synth + そのトラックの insert FX を **新トラックに複製**、
    /// 元トラックは自動ミュート (非破壊・二重再生回避)。
    WithFx,
}

#[derive(Debug, Clone)]
pub struct PendingClipFxBounce {
    /// In Place (同位置置換) か With FX (新トラック) か。
    pub mode: BounceMode,
    pub source_track: u32,
    pub source_clip: u32,
    /// 完了時に song を書き換える対象の stable id。 `source_track` / `source_clip`
    /// は bounce 開始時の index で IPC echo の照合専用 — bounce 中の編集 (トラック
    /// 削除 / 並べ替え / クリップ移動) で stale になり得るため、 song の書き換え
    /// (WithFx の元トラック mute / InPlace の content 置換) はこちらで解決する。
    pub source_track_id: u32,
    pub source_content_id: common::model::ContentId,
    pub out_path: PathBuf,
    pub source_path: common::model::AudioSourcePath,
    pub clip_name: String,
    pub clip_length_beats: f64,
    pub start_beat: f64,
    /// r.md #44: bounce 元 clip の内容窓 offset。 bounce 結果 (single event) を
    /// この位置に置くことで、窓 `[offset, offset + length)` と event が一致する。
    pub content_offset_beats: f64,
}

/// 歌唱 bounce の合成待ち (`PrepareVocalSynth` → `VocalSynthReady`) の退避 entry。
/// 待ち中の編集で clip index が動いても正しい clip へ bounce できるよう stable id
/// で保持し、 `VocalSynthReady` 受信時に現在の `ClipRef` へ解決してから
/// `start_clip_bounce` する。
#[derive(Debug, Clone, Copy)]
pub struct PendingVocalSynthBounce {
    pub track_id: u32,
    pub clip_id: u32,
    pub mode: BounceMode,
}

/// `Z` キーの段階ズームが復元用に積む arrangement の view 状態スナップショット。
/// `X` (= `ArrangeZoomBack`) が pop して 1 段ずつ巻き戻す。 縦ズームは
/// `track_row_overrides` / `automation_lane_row_overrides` を書き換えるので、
/// per-track / per-lane override も一緒に捕まえる。 `PartialEq` は「ユーザーが
/// Z の後で手動ズーム / スクロールしたか」 (= 段階ズームの仕切り直し判定) に使う。
#[derive(Clone, PartialEq)]
pub(crate) struct ArrangeViewSnapshot {
    pub(crate) zoom_x: f32,
    pub(crate) scroll_beat: f32,
    pub(crate) row_h: f32,
    pub(crate) track_top: f32,
    pub(crate) row_overrides: std::collections::HashMap<u32, u16>,
    pub(crate) lane_row_overrides: std::collections::HashMap<common::model::AutomationLaneKey, u16>,
}

/// `Z` 段階ズームが「同じ対象に対する連続押下か」 を判定するための選択シグネチャ。
/// 選択面 (通常 clip / primary clip / automation clip) のいずれかが変わったら別対象
/// とみなして段階を 0 (横ズーム) に戻す。
#[derive(Clone, PartialEq)]
pub(crate) struct ZoomSelectionSig {
    pub(crate) clips: Vec<common::model::ClipKey>,
    pub(crate) clip: Option<common::model::ClipKey>,
    pub(crate) automation: Vec<common::model::AutomationClipKey>,
    /// 対象面 (通常 clip / automation clip)。 選択集合が同じでも対象面が変われば
    /// (= ポインタが clip レーン ⇄ automation レーンへ移動) 仕切り直す。
    pub(crate) target_automation: bool,
}

/// `Z` 段階ズームの現在のアンカー。 直近の Z が「どの選択」を「どの view 状態」に
/// 適用したか + 何段進んだか (`stage`: 1=横適用済 / 2=横+縦適用済) を保持する。
/// 次の Z で選択シグネチャが変わる or `applied_view` と現在 view が食い違う
/// (= ユーザーが手動で動かした) なら段階 0 から仕切り直す。
#[derive(Clone)]
pub(crate) struct ArrangeZoomAnchor {
    pub(crate) sig: ZoomSelectionSig,
    pub(crate) applied_view: ArrangeViewSnapshot,
    pub(crate) stage: u8,
}

/// 進行中 export の現在フェーズ + 進捗。`AppData::export_stage` が単一の真実源で、
/// 進捗オーバーレイ表示・入力 gate (`handle_event` 冒頭)・再生抑止 (`play`) の判定に
/// 使う。`None` = export 非実行。
///
/// 標準 WAV export と video export の前段は `AudioRender` (daw_audio が freewheel で
/// 音声を書き出す)、video export の後段は `VideoRender` (daw_gui がフレームを render)。
/// 標準 WAV か video かの区別 (= overlay タイトル) は `pending_video_export` の有無と
/// `VideoRender` フェーズで判定する (`export_overlay::draw`)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExportStage {
    /// daw_audio が freewheel で音声をレンダリング中。`(done, total)` は **sample 数**
    /// (song body)。標準 WAV export では唯一フェーズ、video export では前段。
    AudioRender { done: u64, total: u64 },
    /// daw_gui が映像フレームをレンダリング中 (video export 後段)。`(done, total)` は
    /// **frame 数**。
    VideoRender { done: u64, total: u64 },
}

/// a WAV export held while plugins reinitialise — `(path,
/// range_beats, write_mod_sidecar)`. See [`AppData::pending_export`].
/// 範囲は **拍** (r.md #54 で拍→サンプル換算を daw_audio 側へ一本化した)。
pub type PendingExport = (std::path::PathBuf, Option<(f64, f64)>, bool);

/// D3/D4: arrangement build のラベルキャッシュ。 `arrangement_view` の build は
/// 毎フレーム全 track×clip ぶん track 名 `Arc::from` と `clip_display_label`
/// (= `Arc::from` + 歌詞連結) を呼び再確保していた。 名前は編集時しか変わらない
/// ので、 `song_epoch` が進んだとき (= undo 境界をまたぐ編集) だけ作り直し、
/// 通常フレームは `Arc` の clone (refcount bump) で済ませる。 `clip_display_label`
/// は `clip.content_id` のみに依存するので content 単位で 1 回だけ算出する。
#[derive(Default)]
pub(crate) struct ArrLabelCache {
    /// このキャッシュ内容が対応する `AppData::song_epoch`。 一致する間は再計算しない。
    pub(crate) epoch: u64,
    pub(crate) track_names: std::collections::HashMap<u32, std::sync::Arc<str>>,
    pub(crate) content_labels:
        std::collections::HashMap<common::model::ContentId, std::sync::Arc<str>>,
    /// D4 同件: section ruler / automation clip も同じ per-frame `Arc::from(&str)`
    /// だった。 user 編集可能 (= intern 不可・無制限成長する) なので track/clip 名と
    /// 同じ `song_epoch` 世代キャッシュで持つ。
    pub(crate) section_names: std::collections::HashMap<u32, std::sync::Arc<str>>,
    pub(crate) content_names:
        std::collections::HashMap<common::model::ContentId, std::sync::Arc<str>>,
}

/// r.md #56: 再生位置の秒表示用 [`common::tempo_map::TempoMap`] の `song_epoch`
/// 世代キャッシュ。
///
/// `song_beat_to_seconds` は SongTempo automation lane がある曲で毎回
/// `TempoMap::from_song` を走らせる (1/16 拍刻みで積分。 5 分の曲で ~9,600
/// breakpoint = 約 77KB の `Vec` 確保)。 transport バーは常時・毎フレーム描画
/// されるので、 そのまま呼ぶと曲長に比例して悪化する。 `TempoMap` は
/// 「生成 O(曲長) / 引き O(log n)」 という設計 (tempo_map.rs 冒頭 doc
/// 「Build off the audio thread (on song change)」) なので、 世代キャッシュに
/// 載せるのが本来の使い方。 lane が無い曲は `map = None` を持ち、 定数 BPM の
/// 高速経路 (table を張らない) にそのまま落ちる。
#[derive(Default)]
pub(crate) struct TempoMapCache {
    /// このキャッシュ内容が対応する `AppData::song_epoch`。
    pub(crate) epoch: u64,
    /// 一度でも構築したか。 `epoch` の初期値 0 と実際の epoch 0 を区別する。
    pub(crate) built: bool,
    /// テンポカーブがある曲だけ `Some`。
    pub(crate) map: Option<common::tempo_map::TempoMap>,
}

/// Windows native file dialog (rfd) を background thread で **owner-modal** に
/// 開くための parent window ラッパー。 `rfd::FileDialog::set_parent` は
/// `HasWindowHandle + HasDisplayHandle` を要求するが、 GUI スレッドの winit
/// `Window` は `AppData` が保持していないので、 runner が渡した main window の
/// `HWND` (isize) からこの場で Win32 raw handle を再構築する。 rfd は set_parent
/// で raw handle を吸い出して `Send` な `FileDialog` に格納するだけなので、 この
/// ラッパは dialog 構築時 (GUI スレッド) にしか参照されない。
#[cfg(windows)]
pub(crate) struct Win32Parent {
    pub(crate) hwnd: isize,
}

#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for Win32Parent {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let hwnd = std::num::NonZeroIsize::new(self.hwnd)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window_handle::Win32WindowHandle::new(hwnd);
        // SAFETY: hwnd は main window の HWND。 dialog は main window の子操作
        // (owner-modal) で main window より先に閉じるため、 handle 参照が使われる
        // 間 window は生存している。
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(
                raw_window_handle::RawWindowHandle::Win32(handle),
            )
        })
    }
}

#[cfg(windows)]
impl raw_window_handle::HasDisplayHandle for Win32Parent {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        // Windows backend は parent HWND のみ参照し parent_display は使わないが、
        // set_parent の trait bound を満たすため空の Windows display handle を返す。
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(
                raw_window_handle::RawDisplayHandle::Windows(
                    raw_window_handle::WindowsDisplayHandle::new(),
                ),
            )
        })
    }
}

/// native file dialog の種別 + 結果消費に必要な context。 dialog は別スレッドで
/// 開き、 選択された path 群を `AppEvent::FileDialogResult` で GUI スレッドへ返した
/// あと、 `handle_file_dialog_result` がこの kind で振り分ける。 dialog を GUI
/// スレッドで同期に開くと、 preview window 等の 2 枚目 top-level window の再描画
/// flood で dialog の modal pump が枯れて数分フリーズするため、 全 native file
/// dialog をこの経路に統一している (commit 8b9f9d0 の export 修正を全 dialog へ展開)。
#[derive(Debug, Clone, PartialEq)]
pub enum FileDialogKind {
    /// プロジェクト (.daw) を開く。
    OpenProject,
    /// video export の mp4 出力先 (Windows のみ到達)。 レンジ
    /// ピッカーで選んだ書き出し窓 (拍)。 `None` = 全曲。 出力解像度
    /// `(w, h)` と fps (= picker で選んだ per-export override)。
    ExportMp4 {
        range_beats: Option<(f64, f64)>,
        resolution: (u32, u32),
        framerate: f32,
    },
    /// WAV 書き出し。 レンジピッカーで選んだ書き出し窓 (**拍**)。 `None` = 全曲。
    /// 拍→サンプル換算は daw_audio 側 SSoT が行う (r.md #54)。
    ExportWav {
        range: Option<(f64, f64)>,
    },
    /// MIDI (SMF) 書き出し。
    ExportMidi,
    /// オーディオ取り込み (複数可)。
    ImportAudio,
    /// 動画取り込み (複数可)。
    ImportVideo,
    /// 画像取り込み (複数可)。
    ImportImage,
    /// MIDI (SMF) 取り込み (複数可)。
    ImportMidi,
    /// Audio Editor の "Add From Source..."。 取り込み先 clip と挿入位置を保持。
    AddAudioEvent {
        clip: ClipRef,
        position_in_clip_beats: f64,
    },
}

/// `spawn_file_dialog` が走らせる rfd dialog の呼び出し種別。
pub(crate) enum FileDialogMode {
    Save,
    PickFile,
    PickFiles,
}

/// 安定 `device_id` (`PluginInstance::id`) から **いまの** 所属 track と
/// chain 内位置を引き直す。 track 内 device は `(Track::id, Vec index)`、
/// master bus の device は `(MASTER_TRACK_ID, master_fx_chain の Vec index)`。
/// 見つからなければ `None` (= 削除済み device への stale event 等は
/// 呼び出し側で無視する)。
///
/// **返り値は保持しないこと。** これは「Song から毎回引き直す一時的な解決」で
/// あって参照ではない (不変条件 1 が禁じているのは *保持される* positional
/// 参照)。 automation lane / recording gesture が track 所有である以上、
/// 「この device はいまどの track の持ち物か」 を知る口は 1 本要る。
pub fn find_device_by_id(
    song: &common::model::Song,
    device_id: u64,
) -> Option<(u32, u32)> {
    if device_id == 0 {
        return None;
    }
    for t in &song.tracks {
        if let Some(i) = t.devices.iter().position(|d| d.id == device_id) {
            return Some((t.id, i as u32));
        }
    }
    if let Some(i) = song
        .master_fx_chain
        .iter()
        .position(|d| d.id == device_id)
    {
        return Some((common::model::MASTER_TRACK_ID, i as u32));
    }
    None
}

/// r.md #36: `(track_id, device_index)` 座標から `PluginInstance` 本体を引く。
/// `track_id == MASTER_TRACK_ID` は `master_fx_chain` を見る。
/// device が存在しなければ `None` (id 未採番かどうかは見ない — それは
/// [`device_id_at`] の責務)。
#[must_use]
pub fn device_at(
    song: &common::model::Song,
    track_id: u32,
    device_index: u32,
) -> Option<&common::model::PluginInstance> {
    let devices: &[common::model::PluginInstance] =
        if track_id == common::model::MASTER_TRACK_ID {
            &song.master_fx_chain
        } else {
            song.tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| t.devices.as_slice())?
        };
    devices.get(device_index as usize)
}

/// 安定 `device_id` から `PluginInstance` 本体を可変で引く
/// (`find_device_by_id` + `fx_chain_by_track_id_mut` の合成)。 device の属性を
/// 書き換える handler (sidechain / パラアウト / キー送出) が共通で使う。
#[must_use]
pub fn device_mut_by_id(
    song: &mut common::model::Song,
    device_id: u64,
) -> Option<&mut common::model::PluginInstance> {
    let (track_id, index) = find_device_by_id(song, device_id)?;
    song.fx_chain_by_track_id_mut(track_id)?.get_mut(index as usize)
}

/// 逆方向: 旧 `(track_id, device_index)` 座標から安定 `device_id` を引く。
/// IPC 送信サイト (SetSlotPlugin / RemoveSlotPlugin / GUI open 等) が
/// positional な GUI 内部状態から protocol の id addressing へ変換するのに
/// 使う。 `track_id == MASTER_TRACK_ID` は `master_fx_chain` を見る。
/// device が存在しない / id 未採番 (0) なら `None`。
#[must_use]
pub fn device_id_at(
    song: &common::model::Song,
    track_id: u32,
    device_index: u32,
) -> Option<u64> {
    // 座標解決は `device_at` 1 本に集約する (同じ走査を 2 度書かない)。
    device_at(song, track_id, device_index)
        .map(|d| d.id)
        .filter(|&id| id != 0)
}

/// v29 機械適応 helper: `(track_idx, clip_idx)` の MIDI content を引く
/// (`Song::notes_in_clip_mut` の content 版)。 note 追加サイトが
/// `MidiContent::alloc_note_id()` で安定 id を採番できるように allocator
/// ごと返す。
pub(crate) fn midi_content_in_clip_mut(
    song: &mut Song,
    track_idx: usize,
    clip_idx: usize,
) -> Option<&mut MidiContent> {
    let content_id = song.tracks.get(track_idx)?.clips.get(clip_idx)?.content_id;
    match song.clip_contents.get_mut(&content_id) {
        Some(ClipContent::Midi(m)) => Some(m),
        _ => None,
    }
}

/// r.md #71 (プラグインのコピー / 移動): device の運搬要求 1 件分。 表示順は
/// `device_ids` の並びが決める (呼び出し側がチェーン表示順に整えて渡す)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocateDevices {
    pub device_ids: Vec<u64>,
    /// 落とし先チェーンの所有者。`MASTER_TRACK_ID` なら `Song.master_fx_chain`。
    pub dest_track: u32,
    /// 落とし先チェーン内の挿入位置 (`0..=chain.len()`)。
    pub dest_index: u32,
    /// `true` = コピー (新 device id を採番)、`false` = 移動 (id 据え置き = 音を切らない)。
    pub copy: bool,
}

/// r.md #71 (プラグインのコピー / 移動): チェーンから掴んだプラグインの
/// 運搬中データ (daw-ui の drag payload に載る)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDragPayload {
    /// 運ぶ device (チェーン表示順)。
    pub device_ids: Vec<u64>,
    /// 掴んだときのチェーン所有者 (ドラッグ中の表示切り替えで cursor が動くので、
    /// 「どこから来たか」 は payload 側が覚えておく)。
    pub source_track: u32,
}

/// r.md #71 (プラグインのコピー / 移動): チェーン行の drag payload に付ける札。
/// drop 側 (インスペクタのチェーン / アレンジのトラックヘッダ) はこの札とだけ
/// 照合する (daw-ui core はペイロードの中身を知らない)。
pub const DEVICE_DRAG_KIND: &str = "daw_01.device_chain";

/// `loaded_devices` の値: 1 つの device (`PluginInstance::id`) に対する load 情報。
/// r.md #71 (プラグインのコピー / 移動): キーが安定 `device_id` になったので、
/// 値に id を複製して持たない (SSoT)。
#[derive(Debug, Clone)]
pub struct LoadedDeviceInfo {
    /// stable string id (= `PluginInstance::plugin_id` と同じ値)。
    /// reconcile の device-level diff で「Song と host で同じ plugin が
    /// 居るか」 を判定するキー。
    pub plugin_id_str: String,
}

/// `reconcile_plugins_with_song` の Phase B が計算する action。
/// IPC dispatch から独立した純粋データ型にすることで unit test しやすく
/// する (4dc982c で導入した device-level diff の regression 防止)。
///
/// v34 (r.md #71 プラグインのコピー / 移動): アドレスは安定 `device_id` 一本。
/// track / chain 内 index は出てこない (host は帰属も順序も持たない)。
#[derive(Debug, Clone, PartialEq)]
pub enum SlotReconcileAction {
    /// host にあるが Song に無い device を host から消す
    /// (= `PluginCommand::RemoveSlotPlugin` 相当)。
    RemoveDevice { device_id: u64 },
    /// Song にあるが host に無い、 もしくは plugin_id_str が違う device を
    /// (再) load する (= `PluginCommand::SetSlotPlugin` 相当)。 caller が
    /// `plugin_db` から format / path を解決して IPC を組み立てる。
    LoadDevice {
        device_id: u64,
        plugin_id_str: String,
        initial_state: Option<Vec<u8>>,
    },
}

/// Phase B 純粋関数化。 song と現在の `loaded_devices` cache を見て、 host
/// と Song を揃えるための action 列を返す。 副作用なし (IPC は呼ばない、
/// AppData にも触らない)。
///
/// 走査順は Song 順 (track → master_fx_chain の Vec 順 = 音の処理順) なので
/// `LoadDevice` の並びは決定的。 `RemoveDevice` は host 側 map の iteration
/// 順に依存しないよう id 昇順に sort する。
pub fn compute_slot_reconcile_actions(
    song: &common::model::Song,
    loaded_devices: &HashMap<u64, LoadedDeviceInfo>,
) -> Vec<SlotReconcileAction> {
    // Song 側で host slot を持つ device (= 映像でない device) の id 集合。
    // 内蔵映像効果は plugin_host に載らない device なので、 ここに混ぜると
    // 毎回 `LoadDevice` が出て「load 応答が来ない device」 が永久に溜まる。
    let mut song_host_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut actions = Vec::new();

    let visit = |devices: &[common::model::PluginInstance],
                 song_host_ids: &mut std::collections::HashSet<u64>,
                 actions: &mut Vec<SlotReconcileAction>| {
        for inst in devices {
            if inst.ports.is_video() {
                continue;
            }
            song_host_ids.insert(inst.id);
            let need_load = match loaded_devices.get(&inst.id) {
                None => true,
                Some(info) => info.plugin_id_str != inst.plugin_id,
            };
            if !need_load {
                continue;
            }
            actions.push(SlotReconcileAction::LoadDevice {
                device_id: inst.id,
                plugin_id_str: inst.plugin_id.clone(),
                initial_state: inst.state.as_deref().map(<[u8]>::to_vec),
            });
        }
    };

    for track in &song.tracks {
        visit(&track.devices, &mut song_host_ids, &mut actions);
    }
    // master bus fx chain (= 音源境界なしの全 audio FX)。
    visit(&song.master_fx_chain, &mut song_host_ids, &mut actions);

    // (1) host にあるが Song に無い device → RemoveDevice。 **余剰を落として
    //     から load する** 順序は現行仕様なので、 先頭へ差し込む。
    let mut host_extra: Vec<u64> = loaded_devices
        .keys()
        .copied()
        .filter(|id| !song_host_ids.contains(id))
        .collect();
    host_extra.sort_unstable();
    let removals: Vec<SlotReconcileAction> = host_extra
        .into_iter()
        .map(|device_id| SlotReconcileAction::RemoveDevice { device_id })
        .collect();
    let mut out = removals;
    out.append(&mut actions);
    out
}

/// `RequestAllStates` の発行理由。 plugin_host から `AllPluginStates`
/// が返ってくるまで [`AppData::pending_state_queue`] に保持し、 応答時に
/// 対応する完了処理 (save または deferred edit) を実行する。
///
/// 連続した編集 (例: 2 連続 delete_track) は queue に積まれ、 各々が
/// 自前の `RequestAllStates` 応答を待ってから順次実行される。 これで
/// 「先行 edit の応答待ち中に来た 2 番目の edit が state 同期なしで
/// 走り、 Undo で knob 値が復元されない」 race を回避する。
#[derive(Debug, Clone)]
pub enum PendingStateRequest {
    /// project save。 ファイル書き出し完了で消費される。 `snapshot` は **この Save の
    /// `RequestAllStates` を発行する瞬間** の song を凍結したもの。 enqueue 時点では
    /// `None` で積み、 `dispatch_front_state_request` が state 収集を始めるその瞬間に
    /// 充填する。 こうすると snapshot の plugin layout と、 host が返す plugin state の
    /// layout が同時刻サンプリングになり (FIFO IPC)、 待機中の slot 削除 / 並べ替えが
    /// 保存ファイルへ誤適用される窓が出ない。 充填後に走った編集はこの snapshot に
    /// 入らず live song に留まり、 次の save に回る
    /// (grill-me 2026-06-10, `docs/plan_progress_streaming.md`)。
    Save {
        path: PathBuf,
        snapshot: Option<Box<Song>>,
        /// snapshot を凍結した瞬間の `edit_epoch` (dirty 判定: finish_save が
        /// 「round-trip 中に live へ編集が入ったか」 を epoch 差で判定する)。
        snap_epoch: u64,
    },
    /// plugin が **削除される** 編集操作の Undo snapshot 作成。
    /// state を Song に書き込んでから [`AppData::push_undo_snapshot`]
    /// を呼ぶことで、 削除直前の knob 値等を Undo で復元できる。
    Deferred(DeferredEdit),
    /// copy (Ctrl+C)。state 書き戻し後の live song から対象を最新 plugin state
    /// 込みで serialize して `pending_clipboard_write` に積むだけ (Song 不変)。
    /// **undo snapshot は積まない** (copy は履歴を汚さない) ので `Deferred` とは
    /// 別 variant にする。
    CopyToClipboard(ClipboardCopyRequest),
}

/// [`PendingStateRequest::CopyToClipboard`] の対象。 round-trip 待ちに積む
/// 「何をコピーするか」 は面ごとに違うが、 待ち行列の仕組みは 1 本でよい。
#[derive(Debug, Clone)]
pub enum ClipboardCopyRequest {
    Tracks(Vec<u32>),
    /// r.md #71 (プラグインのコピー / 移動): チェーンで選んだ device。
    Devices(Vec<u64>),
}

/// state 取得が完了したあとに plugin-main thread へ実行させる編集。
/// 対象は **すべて安定 id** (`track_id` / `device_id`) で持つので、 pending 中に
/// 他の編集が track / device の Vec position をずらしても整合性が保たれる
/// (r.md #71 で device 側も positional index から id へ移行)。
#[derive(Debug, Clone)]
pub enum DeferredEdit {
    /// トラック削除 (r.md #43)。 選択集合を **1 件にまとめて** 持つ — id ごとに
    /// enqueue すると round-trip が分かれて undo が N ステップに割れる。
    DeleteTracks { track_ids: Vec<u32> },
    UngroupTracks { track_ids: Vec<u32> },
    /// 単一デバイスチェーン: 安定 `device_id` で指した device を chain から外す。
    /// 複数選択を **1 件にまとめて** 持つ (id ごとに enqueue すると undo が
    /// N ステップに割れる)。
    RemoveDevices { device_ids: Vec<u64> },
    /// r.md #71 (プラグインのコピー / 移動): device を別のチェーンへ運ぶ
    /// (移動 / コピー)。 最新の knob 値を Song へ書き戻してから実行したいので
    /// round-trip 待ちに積む。
    RelocateDevices(RelocateDevices),
    /// r.md #71: device の cut (Ctrl+X)。最新 plugin state 込みで serialize して
    /// `pending_clipboard_write` に積んでから削除する (1 undo step)。
    CutDevices { device_ids: Vec<u64> },
    /// トラック cut (Ctrl+X)。最新 plugin state 込みで serialize して
    /// `pending_clipboard_write` に積んでから各トラックを削除する。`Deferred` 経由なので
    /// 削除前に undo snapshot が積まれ、Ctrl+Z 1 回で復元できる。
    CutTracks { track_ids: Vec<u32> },
    /// トラック複製 (r.md #30)。plugin state を Song に書き戻してから serialize +
    /// 独立/リンク複製するので、 複製先 device の初期 state が最新 knob を反映する
    /// (copy → paste と同じ理由で deferred)。`linked=true` はクリップ中身を元と
    /// content_id 共有、 `false` は独立コピー。
    DuplicateTracks { track_ids: Vec<u32>, linked: bool },
}

/// builtin VOICEVOX (歌唱/読み上げ) 合成の 1 plugin 分の状態。
/// `AppData.voicevox_synth_status` に key=plugin_id で保持する。
#[derive(Debug, Clone, PartialEq)]
pub struct VocalSynthStatus {
    /// いま合成中か (= plugin host の `queued_gen > done_gen`)。
    pub busy: bool,
    /// engine に**到達できず** (未接続/起動途中) 失敗してからの起点。到達成功で `None`。
    /// `Some` のまま `VOICEVOX_ENGINE_WARNING` 経過 = engine 未接続として警告に切替。
    pub failing_since: Option<std::time::Instant>,
    /// engine には**到達できたが入力を拒否**された内容エラーの理由 (例: `lyricが不正です: ー`)。
    /// `Some` の間は「合成できない歌詞」警告を即時表示 (閾値なし)。到達成功/新規合成で `None`。
    pub rejected: Option<String>,
}

/// 合成が `failing` のまま継続したとき「engine に接続できません」へ切り替える
/// までの猶予。engine 起動 (boot) に数秒かかるので、その間は「合成中」に見せる。
pub const VOICEVOX_ENGINE_WARNING: std::time::Duration = std::time::Duration::from_secs(5);

/// 口パク (lip-sync) 背景ジョブの 1 vocal clip 分の結果。`query_phonemes` の
/// 出力と、生成先 clip の配置情報 (start / length / earliest note) をまとめて
/// main thread へ渡す (`AppEvent::LipsyncGenerated`)。docs/plan_pakupaku.md §7。
#[derive(Debug, Clone, PartialEq)]
pub struct LipsyncClipResult {
    /// 生成先 clip の start_beat (= 元 vocal clip と揃える)。
    pub clip_start_beat: f64,
    /// 生成先 clip の length_beats (= 元 vocal clip と揃える)。
    pub clip_len_beats: f64,
    /// phoneme 列の **frame 0** が来る clip-local beat (= 合成 wav の先頭が来る位置)。
    /// 歌なら `voicevox::sing_head_beat(基準ノート)`、talk なら「発話開始 − pre-silence」。
    /// `common::lipsync::build_mouth_events` にそのまま渡す (r.md #39)。
    pub first_phoneme_local_beat: f64,
    /// (talk) この結果の元ソーストラックの並び順 index (= 口パク優先度)。複数の
    /// ソーストラック (歌唱 Vox / 読み上げ Talk 等) が同じ口 track を出力先に
    /// 指定したとき、時間が重なる部分は **index が小さい (= 上の) トラック**が
    /// 優先される (`docs/plan_voicevox_talk.md`、apply 側で区間マージ)。
    pub priority: u32,
    /// VOICEVOX phoneme 列 (先頭/末尾 pau 込み、frame 0 起点)。
    pub phonemes: Vec<common::voicevox::Phoneme>,
}

/// (talk) 複数ソーストラックの口パク mouth event 区間を、上位トラック優先で重なり
/// なく統合する (`docs/plan_voicevox_talk.md`)。入力は `(song-absolute start, end,
/// image_id, priority)` — priority はソーストラックの並び順 index (小 = 上 = 優先)。
/// 戻り値は start 昇順・非重複の `(start, end, image_id)`。上位が claim した時間帯は
/// 下位が埋めず、隙間 (どのソースも clip を持たない時間) は event 無し。これで Vox
/// (歌唱) と Talk (読み上げ) が同じ口 track を共有でき、重なりは上のトラックが勝つ。
pub(crate) fn merge_lipsync_events_by_priority(mut events: Vec<(f64, f64, u32, u32)>) -> Vec<(f64, f64, u32)> {
    // 上 (priority 小) → 下、同 priority 内は start 昇順。上位から claim させる。
    events.sort_by(|a, b| a.3.cmp(&b.3).then(a.0.total_cmp(&b.0)));
    let mut claimed: Vec<(f64, f64)> = Vec::new(); // sorted, non-overlapping
    let mut out: Vec<(f64, f64, u32)> = Vec::new();
    for (s, e, img, _prio) in events {
        if e - s <= 1e-9 {
            continue;
        }
        // [s, e] から既 claim を引いた未 claim 部分だけ emit。
        let mut cursor = s;
        for &(cs, ce) in &claimed {
            if ce <= cursor {
                continue;
            }
            if cs >= e {
                break;
            }
            if cs > cursor {
                out.push((cursor, cs.min(e), img));
            }
            cursor = cursor.max(ce);
            if cursor >= e {
                break;
            }
        }
        if cursor < e {
            out.push((cursor, e, img));
        }
        // [s, e] を claimed に挿入して coalesce (= 以後この区間は claim 済み)。
        claimed.push((s, e));
        claimed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut coalesced: Vec<(f64, f64)> = Vec::with_capacity(claimed.len());
        for &(cs, ce) in &claimed {
            if let Some(last) = coalesced.last_mut()
                && cs <= last.1 + 1e-9
            {
                last.1 = last.1.max(ce);
                continue;
            }
            coalesced.push((cs, ce));
        }
        claimed = coalesced;
    }
    // start 昇順に並べ、隣接同 image をマージ。
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64, u32)> = Vec::with_capacity(out.len());
    for (s, e, img) in out {
        if let Some(last) = merged.last_mut()
            && last.2 == img
            && (s - last.1).abs() <= 1e-6
        {
            last.1 = e;
            continue;
        }
        merged.push((s, e, img));
    }
    merged
}

/// 未保存変更がある状態で「現在のプロジェクトを破棄する操作」 を
/// 行おうとしたとき、 ガードモーダル (`dirty_guard_modal`) で保存確認を挟んでから
/// 実行する操作の種類。 終了 (`Quit`、 旧 close 確認) と、 New / Open /
/// Open Recent を一本化する (= 同じ「破棄する前に確認」 セマンティクス)。
#[derive(Debug, Clone, PartialEq)]
pub enum DirtyGuardAction {
    /// アプリを終了する (✕ / Alt+F4 / File > 終了 / Ctrl+Q / OS のセッション終了)。
    /// 終了コードは `QuitRequest` から運ばれる (smoke test が判定結果を載せる)。
    Quit(crate::shutdown::QuitRequest),
    /// 新規プロジェクト (`action_new`)。
    New,
    /// プロジェクトを開く (ファイル選択 dialog、 `action_open`)。
    Open,
    /// 指定パスのプロジェクトを開く (Open Recent、 `action_open_path`)。
    OpenPath(PathBuf),
}

/// (talk) Text clip の読み上げスケール 1 項目 (`AppEvent::SetClipTalkParam`)。
/// VOICEVOX `audio_query` の話速/音高/抑揚/音量に対応 (`docs/plan_voicevox_talk.md` §4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkParamKind {
    Speed,
    Pitch,
    Intonation,
    Volume,
}

pub(crate) fn clamp_snap_choice(c: u8) -> u8 {
    let max = (crate::view::snap::SNAP_LABELS.len() - 1) as u8;
    c.min(max)
}

/// frames @ source_sr → beats @ project bpm. Used to size newly
/// imported audio clips so the visual length matches the file
/// duration at the project's current tempo.
/// 画像を `preview_resolution` 内に **アスペクト比維持で中央 fit** する
/// PiP rect を計算する。 `(x, y, w, h)` は normalized 0..=1。 画像が
/// preview より横長なら横一杯 + 上下に余白、 縦長なら縦一杯 + 左右に
/// 余白。 image / preview 寸法が 0 のときは安全側で全画面 `(0,0,1,1)`。
pub(crate) fn aspect_fit_pip_rect(
    preview_resolution: (u32, u32),
    image_size: (u32, u32),
) -> (f32, f32, f32, f32) {
    let (pw, ph) = preview_resolution;
    let (iw, ih) = image_size;
    if pw == 0 || ph == 0 || iw == 0 || ih == 0 {
        return (0.0, 0.0, 1.0, 1.0);
    }
    let preview_aspect = pw as f32 / ph as f32;
    let image_aspect = iw as f32 / ih as f32;
    if image_aspect >= preview_aspect {
        // 画像が横長 → 横幅一杯、 縦に余白。
        let w = 1.0_f32;
        let h = preview_aspect / image_aspect;
        let x = 0.0_f32;
        let y = (1.0 - h) * 0.5;
        (x, y, w, h)
    } else {
        // 画像が縦長 → 縦一杯、 横に余白。
        let h = 1.0_f32;
        let w = image_aspect / preview_aspect;
        let y = 0.0_f32;
        let x = (1.0 - w) * 0.5;
        (x, y, w, h)
    }
}

/// media import (audio / image) 時にクリップを置く track の決定方法。
/// drag&drop / dialog の「起点」を型で表す (`Option<u32>` の `None` が
/// 「空きスペース drop」と「dialog (位置情報なし)」を区別できず両者を
/// 一番上への新規 track にまとめていた曖昧さを解消。r.md #31)。
///
/// arrangement の drop 位置 → track の解決 (`track_index_at_y`) と、File
/// メニュー / dialog 経由 (位置情報なし) の 3 通りを、それぞれ別 variant で運ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportTrackTarget {
    /// drop が既存 track に当たった (arrangement view が y 座標から解決した
    /// `song.tracks` index)。その track にクリップを貼る。
    Track(u32),
    /// track の無い下の余白に drop された → 一番下 (`song.tracks` 末尾) に
    /// 新規 track を作ってクリップを貼る (r.md #31)。song が空でもここで
    /// 最初の track を作れる。
    NewTrackBottom,
    /// 位置情報なし (File メニュー / dialog 経由)。handler ごとの既定に従う
    /// (audio = cursor track fallback、image = 一番下に新規 track)。
    NoHint,
}

/// メディアドロップの配置先を解決する (`action_import_image` / `action_import_midi`
/// の core 判定)。
/// - `Some(idx)` (= `Track(idx)` で既存 track を指す): その track に clip を貼る。
/// - `None`: 一番下に新規 track を作って貼る。track の無い下の領域への drop
///   (`NewTrackBottom`)、範囲外 index の `Track`、dialog 経由 (`NoHint`) は
///   どれもこちら (= r.md #31 で「一番上への insert」を廃し全て末尾 push へ)。
pub(crate) fn resolve_media_drop_target(target: ImportTrackTarget, n_tracks: usize) -> Option<usize> {
    match target {
        ImportTrackTarget::Track(i) => {
            let i = i as usize;
            (i < n_tracks).then_some(i)
        }
        ImportTrackTarget::NewTrackBottom | ImportTrackTarget::NoHint => None,
    }
}

/// time-stretch で clip-local の (start, len) を新 clip 長へ写像する
/// ピュア関数。 固定端 pivot (右端 drag = 左端固定 / 左端 drag = 右端固定) で
/// 絶対 beat 上を `factor = new_len/prev_len` で scale し、 新 clip-local へ戻す。
/// `prev_len <= 0` は identity (退化保護)。 audio event / MIDI note 共通。
pub(crate) fn stretch_remap(
    prev_start: f64,
    prev_len: f64,
    new_start: f64,
    new_len: f64,
    local_start: f64,
    local_len: f64,
) -> (f64, f64) {
    if prev_len <= 1e-9 {
        return (local_start, local_len);
    }
    let factor = new_len / prev_len;
    // start が動いた = 左端 drag (右端固定)、 不動 = 右端 drag (左端固定)。
    let pivot_abs = if (new_start - prev_start).abs() > 1e-9 {
        prev_start + prev_len
    } else {
        prev_start
    };
    let old_abs = prev_start + local_start;
    let new_abs = pivot_abs + (old_abs - pivot_abs) * factor;
    ((new_abs - new_start).max(0.0), (local_len * factor).max(0.0))
}

/// 新規 track の挿入 index を決めるピュアロジック。 選択中の track が
/// 1 つ以上あれば「最上段 (= `tracks` 内 index 最小) の選択の **直上**」、無ければ末尾。
/// 複数選択でも一番上を基準にすることで選択のかたまりを割らない。
/// `selected` 内の stale id (= `tracks` に存在しない) は `position()` が `None` を
/// 返すので自然に無視され、 全部 stale なら末尾に fallback する。
/// (の「最下段の直後」から、ユーザー指定で「最上段の直上」へ変更。)
pub(crate) fn add_track_insert_index(tracks: &[Track], selected: &[u32]) -> usize {
    selected
        .iter()
        .filter_map(|sid| tracks.iter().position(|t| t.id == *sid))
        .min()
        .unwrap_or(tracks.len())
}

/// 新規 Arranger セクションの既定名。Intro/Aメロ/サビ… を巡回し、
/// それを超えたら `Part N` に連番フォールバック。
pub(crate) fn section_default_name(index: usize) -> String {
    const NAMES: [&str; 7] = ["Intro", "Aメロ", "Bメロ", "サビ", "間奏", "Cメロ", "アウトロ"];
    NAMES
        .get(index)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| format!("Part {}", index + 1))
}

/// 新規 Arranger セクションの既定色 (パレットを巡回)。
pub(crate) fn section_default_color(index: usize) -> [f32; 3] {
    const PALETTE: [[f32; 3]; 7] = [
        [0.35, 0.55, 0.85],
        [0.45, 0.75, 0.55],
        [0.85, 0.65, 0.35],
        [0.85, 0.45, 0.55],
        [0.60, 0.50, 0.80],
        [0.40, 0.75, 0.80],
        [0.80, 0.55, 0.75],
    ];
    PALETTE[index % PALETTE.len()]
}

pub(crate) fn frames_to_beats(frames: u64, sample_rate: u32, bpm: f32) -> f64 {
    if sample_rate == 0 || bpm <= 0.0 {
        return 0.0;
    }
    let secs = frames as f64 / sample_rate as f64;
    secs * (bpm as f64) / 60.0
}

/// `docs/plan_video.md`: μs duration → project beats. Used by
/// `action_import_video` to set the visual length of the auto-created
/// video clip to its native duration at the project's current tempo.
pub(crate) fn micros_to_beats(micros: u64, bpm: f32) -> f64 {
    if bpm <= 0.0 {
        return 0.0;
    }
    let secs = micros as f64 / 1_000_000.0;
    secs * (bpm as f64) / 60.0
}

/// `app_dirs` から解決した path で recent list を復元。 path が `None`
/// (= 永続化先なし) や読み込み失敗時は空 list を返す (起動を妨げない)。
pub(crate) fn load_recent_list(path: Option<PathBuf>) -> crate::recent::RecentFiles {
    let Some(path) = path else {
        return Default::default();
    };
    match crate::recent::load(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = ?e, path = %path.display(), "recent list load failed");
            Default::default()
        }
    }
}

/// 選択ノートを複製して `notes` 末尾に追加するピュアロジック (D キー複製の core)。
/// `selected` の各 index のノートを clone し、選択範囲の beat span
/// (`max(start+dur) - min(start)`、最低 1/16 拍) ぶん後ろへずらして append する。
/// 元ノートは不変。戻り値は複製ノートの新 index (= 複製後の選択)。
/// 選択が空 / 該当 index 無しなら `notes` 不変で空 Vec を返す。
pub(crate) fn duplicate_notes_into(content: &mut MidiContent, selected: &[u32]) -> Vec<u32> {
    let mut clones: Vec<Note> = selected
        .iter()
        .filter_map(|&idx| content.notes.get(idx as usize).cloned())
        .collect();
    if clones.is_empty() {
        return Vec::new();
    }
    let min_start = clones
        .iter()
        .map(|n| n.start_beat)
        .fold(f64::INFINITY, f64::min);
    let max_end = clones
        .iter()
        .map(|n| n.start_beat + n.duration_beats)
        .fold(f64::NEG_INFINITY, f64::max);
    let offset = (max_end - min_start).max(0.0625);
    for n in &mut clones {
        n.start_beat += offset;
        // clone は元 note の `id` も複製する。 per-content 一意 note id 不変条件
        // (invariant #1、 piano_roll が selection/hit-test/drag/edit に note.id を使う)
        // を守るため新規採番する。 さもないと複製ノートを選択/ドラッグすると元と
        // 両方に作用する (M4 = duplicate_audio_editor_event の sibling)。
        n.id = content.alloc_note_id();
    }
    let base = content.notes.len() as u32;
    let count = clones.len() as u32;
    content.notes.append(&mut clones);
    (base..base + count).collect()
}

/// gui_01 #054 (Ctrl+drag コピー) の core。`entries` = [(source note index,
/// new_start_beat, new_pitch)]。各 source を clone し start_beat/pitch を指定値にして
/// `notes` 末尾へ追加。元は不変。戻り値は複製の新 index。該当 index 無しなら不変で空 Vec。
pub(crate) fn copy_notes_into(content: &mut MidiContent, entries: &[(u32, f64, u8)]) -> Vec<u32> {
    let mut clones: Vec<Note> = Vec::new();
    for &(idx, new_beat, new_pitch) in entries {
        if let Some(src) = content.notes.get(idx as usize) {
            let mut c = src.clone();
            c.start_beat = new_beat.max(0.0);
            c.pitch = new_pitch;
            clones.push(c);
        }
    }
    if clones.is_empty() {
        return Vec::new();
    }
    // clone は元 note の `id` を複製するので新規採番する (duplicate_notes_into と
    // 同じ per-content 一意 id 不変条件、 invariant #1)。
    for c in &mut clones {
        c.id = content.alloc_note_id();
    }
    let base = content.notes.len() as u32;
    let count = clones.len() as u32;
    content.notes.append(&mut clones);
    (base..base + count).collect()
}

/// 同一ピッチの MIDI ノートが時間的に重ならない不変条件を強制する
/// (Bitwig / Ableton 流、`docs/plan_fixme_83_note_overlap.md`)。
///
/// `winners` = 直前に追加 / 移動 / サイズ変更 / コピーされたノートの index (= 衝突時に
/// 勝つ側)。同一ピッチで winner と重なる loser を **last-note-wins** で解消する:
/// - 完全被覆 → loser 削除
/// - loser が winner より前に始まる → loser 末尾を winner 開始でトリム
///   (末尾重なり + 中央挿入 = truncate-not-split、自動分割しない)
/// - loser 先頭が winner に覆われ後半が残る → loser 開始を winner 終端へ前送りし後半を残す
///   (REAPER 流の非破壊的挙動、user 採用)
///
/// winner 同士の重なり (時間 / ピッチ量子化・glue で発生し得る) は pitch ごとに start 昇順で
/// 「後から始まる方が勝ち、前のノート末尾をトリム」で解消する (move / copy 等の並進操作では
/// winner 群は重ならないので no-op)。**異なるピッチは一切触らない** (= 和音は自由)。
///
/// 削除で index がずれるため、戻り値は古い index → 新 index の remap 表 (削除は `None`)。
/// 削除は降順 `Vec::remove` で行う (`delete_selected_notes` と同 idiom)。caller は
/// [`remap_indices`] で `selected_notes` / 新規 winner id を写し替える。
pub(crate) fn resolve_note_overlaps(notes: &mut Vec<Note>, winners: &[u32]) -> Vec<Option<u32>> {
    const EPS: f64 = 1e-9;
    let n = notes.len();
    // winner index を範囲内・重複除去して正規化 (入力順は保持)。
    let mut is_winner = vec![false; n];
    let mut winner_order: Vec<usize> = Vec::new();
    for &w in winners {
        let w = w as usize;
        if w < n && !is_winner[w] {
            is_winner[w] = true;
            winner_order.push(w);
        }
    }
    let mut deleted = vec![false; n];

    // ---- Phase B: winner 同士の重なり解消 (pitch ごとに start 昇順、後勝ち) ----
    {
        let mut by_pitch: std::collections::HashMap<u8, Vec<usize>> =
            std::collections::HashMap::new();
        for &w in &winner_order {
            by_pitch.entry(notes[w].pitch).or_default().push(w);
        }
        for group in by_pitch.values_mut() {
            if group.len() < 2 {
                continue;
            }
            group.sort_by(|&a, &b| {
                notes[a]
                    .start_beat
                    .total_cmp(&notes[b].start_beat)
                    .then(a.cmp(&b))
            });
            for i in 0..group.len() - 1 {
                let a = group[i];
                let c = group[i + 1];
                if deleted[a] {
                    continue;
                }
                let a_end = notes[a].start_beat + notes[a].duration_beats;
                let c_start = notes[c].start_beat;
                if a_end > c_start + EPS {
                    let new_dur = c_start - notes[a].start_beat;
                    if new_dur <= EPS {
                        deleted[a] = true;
                    } else {
                        notes[a].duration_beats = new_dur;
                    }
                }
            }
        }
    }

    // ---- Phase A: 各 winner が同一ピッチの loser をトリム / 削除 ----
    for &w in &winner_order {
        if deleted[w] {
            continue;
        }
        let ws = notes[w].start_beat;
        let we = ws + notes[w].duration_beats;
        if we <= ws + EPS {
            continue;
        }
        let p = notes[w].pitch;
        for b in 0..n {
            if b == w || is_winner[b] || deleted[b] || notes[b].pitch != p {
                continue;
            }
            let bs = notes[b].start_beat;
            let be = bs + notes[b].duration_beats;
            // 重なり無し (隣接 be == ws / bs == we は許容)。
            if be <= ws + EPS || bs >= we - EPS {
                continue;
            }
            if ws <= bs + EPS && be <= we + EPS {
                // 完全被覆 → 削除。
                deleted[b] = true;
            } else if bs < ws - EPS {
                // loser が winner より前に始まる → 末尾を winner 開始でトリム
                // (末尾重なり + 中央挿入 = truncate-not-split)。
                let new_dur = ws - bs;
                if new_dur <= EPS {
                    deleted[b] = true;
                } else {
                    notes[b].duration_beats = new_dur;
                }
            } else {
                // loser 先頭が winner に覆われ後半が we を超える → 開始を we へ前送り
                // して後半を残す (REAPER 流、user 採用)。
                let new_dur = be - we;
                if new_dur <= EPS {
                    deleted[b] = true;
                } else {
                    notes[b].start_beat = we;
                    notes[b].duration_beats = new_dur;
                }
            }
        }
    }

    // ---- 削除を適用 + remap 表を構築 ----
    let mut remap: Vec<Option<u32>> = vec![None; n];
    let mut deleted_before = 0u32;
    for i in 0..n {
        if deleted[i] {
            deleted_before += 1;
        } else {
            remap[i] = Some(i as u32 - deleted_before);
        }
    }
    for i in (0..n).rev() {
        if deleted[i] {
            notes.remove(i);
        }
    }
    remap
}

/// [`resolve_note_overlaps`] の remap 表で古い index 列を新 index 列へ写す
/// (削除されたものは除外)。selected_notes / 新規 winner id の付け替えに使う。
pub(crate) fn remap_indices(remap: &[Option<u32>], idxs: &[u32]) -> Vec<u32> {
    idxs.iter()
        .filter_map(|&i| remap.get(i as usize).copied().flatten())
        .collect()
}

/// `SetClipColor` の core: target clip の `content_id` を共有する全 track の全 clip へ
/// `color` を伝播する (= 共有クリップの色を変えれば共有先全部が同色、 cross-track 含む)。
/// `content_id == 0` (未採番 sentinel) のときは伝播せず target clip のみ塗る (defensive、
/// 別々の未採番 clip を巻き込まない)。target が範囲外なら何もしない。
///
/// 「クリップ色をトラックに揃える」(`ResetTrackClipColors`) は逆に **track-scoped** で
/// 他 track の共有 clip を変えない — 色は per-clip 所有 (`Clip.color`) なので、 SET 伝播 /
/// RESET track-local の両立ができる (`docs/plan_track_clip_color.md` 追加要件)。
pub(crate) fn propagate_clip_color(tracks: &mut [Track], target: ClipRef, color: Option<[f32; 3]>) {
    let content_id = tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .map(|c| c.content_id);
    match content_id {
        Some(cid) if cid != 0 => {
            for t in tracks.iter_mut() {
                for clip in t.clips.iter_mut().filter(|c| c.content_id == cid) {
                    clip.color = color;
                }
            }
        }
        _ => {
            if let Some(clip) = tracks
                .get_mut(target.track as usize)
                .and_then(|t| t.clips.get_mut(target.clip as usize))
            {
                clip.color = color;
            }
        }
    }
}

/// [`diff_preview`] が返す 1 アクション (鍵盤プレビューの note-on/off)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewAction {
    NoteOff { track_id: u32, pitch: u8 },
    NoteOn { track_id: u32, pitch: u8 },
}

/// 鍵盤プレビューの状態遷移 (gui_01 #055)。 現在発音中の note `prev` と今フレーム
/// の目標 `next` (どちらも `(track_id, pitch)`) を比較し、 発行すべき note-off /
/// note-on を順に返す (held-value + caller diff)。
/// - `None → Some`: `[NoteOn]`
/// - `Some(a) → Some(b)` (a≠b): `[NoteOff(a), NoteOn(b)]`
/// - `Some → None`: `[NoteOff]`
/// - 変化なし: 空 (= 同 pitch 押下継続中は再送しない)
pub(crate) fn diff_preview(prev: Option<(u32, u8)>, next: Option<(u32, u8)>) -> Vec<PreviewAction> {
    if prev == next {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if let Some((track_id, pitch)) = prev {
        actions.push(PreviewAction::NoteOff { track_id, pitch });
    }
    if let Some((track_id, pitch)) = next {
        actions.push(PreviewAction::NoteOn { track_id, pitch });
    }
    actions
}

pub(crate) fn resolve_plugin_name(plugin_db: &Option<Arc<PluginDatabase>>, plugin_id: &str) -> String {
    plugin_db
        .as_deref()
        .and_then(|db| db.find_by_id(plugin_id))
        .map(|e| {
            if e.name.is_empty() {
                plugin_id.to_string()
            } else {
                e.name.clone()
            }
        })
        .unwrap_or_else(|| plugin_id.to_string())
}

/// decode 済み audio staging entry (source_id, buffer)。
pub(crate) type DecodedAudio = (
    common::model::AudioSourceId,
    std::sync::Arc<crate::audio_source_cache::AudioSourceBuffer>,
);

/// decode 済み image staging entry (source_id, (w, h, bgra))。
pub(crate) type DecodedImage = (
    common::model::ImageSourceId,
    (u32, u32, std::sync::Arc<Vec<u8>>),
);

/// r.md #42: 動画クリップのサムネイル 1 件 (`(id, (width, height, rgba))`)。
/// 画像と同じく **ディスク上の動画ファイルが唯一の正**で、 GPU テクスチャはそこから
/// 何度でも作り直せる派生データ。
pub(crate) type DecodedVideoThumbnail = (
    common::model::VideoSourceId,
    (u32, u32, std::sync::Arc<Vec<u8>>),
);

/// background asset decode の中間バッファ。 decode スレッドが結果を
/// push + `done` を進め、 GUI スレッドの `on_asset_decode_tick` が caches へ排出
/// する。
#[derive(Default)]
pub struct AssetDecodeStaging {
    /// decode 済みで未取り込みの audio。
    pub audio: Vec<DecodedAudio>,
    /// 同 image。
    pub image: Vec<DecodedImage>,
    /// 同 video サムネイル (r.md #42)。 `(id, (w, h, rgba))`。
    pub video_thumbnail: Vec<DecodedVideoThumbnail>,
    /// 処理済み件数 (成功 + 失敗、 進捗表示用)。
    pub done: usize,
    /// 総件数。
    pub total: usize,
    /// **未処理の audio 件数**。 再生 gate はこれだけを見る (r.md #42)。
    ///
    /// 「decode 中は再生できない」 制約の根拠は **音が揃っていないこと** であって、
    /// サムネイルや立ち絵の画像とは無関係。 `asset_decode.is_some()` 全体で gate すると、
    /// GPU 復旧後の画像再読込 (音は cache 済) や、 音源の無いプロジェクトを開いた直後まで
    /// 再生が保留されてしまう。
    pub audio_remaining: usize,
}

/// PNG/JPEG/… を decode して BGRA8 + 寸法を返す (background thread から呼ぶ自由
/// 関数)。 失敗 / 0 サイズは `None`。 旧 `decode_image_sources_into_cache` の
/// image::open → RGBA → BGRA part を抽出したもの。
pub(crate) fn decode_image_to_bgra(abs: &Path) -> Option<(u32, u32, std::sync::Arc<Vec<u8>>)> {
    match image::open(abs) {
        Ok(dynamic) => {
            let rgba = dynamic.into_rgba8();
            let (w, h) = rgba.dimensions();
            if w == 0 || h == 0 {
                tracing::warn!(path = %abs.display(), "asset decode (image): zero-sized");
                return None;
            }
            let mut bytes = rgba.into_raw();
            for px in bytes.chunks_exact_mut(4) {
                px.swap(0, 2); // RGBA → BGRA
            }
            Some((w, h, std::sync::Arc::new(bytes)))
        }
        Err(e) => {
            tracing::warn!(path = %abs.display(), error = %e, "asset decode (image) failed");
            None
        }
    }
}

/// r.md #42: 動画 1 本から 1 フレーム目のサムネイル RGBA8 を取り出す
/// (background thread から呼ぶ自由関数、 `decode_image_to_bgra` と同 idiom)。
///
/// import 時と同じ [`crate::import_video::extract_thumbnail`] を通すので、
/// 「取り込んだ直後」 と「開き直した後」 と「GPU 再初期化後」 で同じ絵になる。
#[cfg(windows)]
pub(crate) fn decode_video_thumbnail(abs: &Path) -> Option<(u32, u32, std::sync::Arc<Vec<u8>>)> {
    match crate::import_video::extract_thumbnail(abs) {
        Ok(t) => Some((t.width, t.height, std::sync::Arc::new(t.rgba))),
        Err(e) => {
            tracing::warn!(
                path = %abs.display(),
                error = %e,
                "asset decode (video thumbnail) failed"
            );
            None
        }
    }
}

/// 動画 decode は libav (`import_video` / `libav_decoder`) 依存で Windows 限定なので、
/// 他プラットフォームではサムネイル機能ごと無効 (job も積まれない)。
#[cfg(not(windows))]
pub(crate) fn decode_video_thumbnail(_abs: &Path) -> Option<(u32, u32, std::sync::Arc<Vec<u8>>)> {
    None
}

