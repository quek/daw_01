// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `scrubable_number` ウィジェット — drag-to-edit な数値入力。
//!
//! Phase 64a (daw_01 #034): BPM / TimeSig num 等の transport 数値表示で「数値そのものを
//! mouse press + 縦横 drag で連続変化」 + 「single-click で text input mode」 + 「Ctrl で fine drag」
//! + 「dblclick で default reset」 という DAW 慣習を実装する widget。
//!
//! 既存 `knob_at` (= 円形 knob で drag scrub) と `text_input_at` (= keyboard 入力) を組み合わせた
//! 上位 idiom。 `text_input_at_focused` を **内部 delegate** することで IME / clipboard / 選択 /
//! Esc rollback は全部既存実装に乗せ、 scrubable 側は state machine + drag 値計算 + format parse のみ。
//!
//! 操作 binding (Phase 64a confirmed by daw_01 #034):
//! - press + 縦横 drag (合成 >= 4px) → scrub 開始 (`dragging = true`、 per-frame `on_change(new)`)。
//!   右 / 上で増加、 左 / 下で減少 (両軸の符号付き移動量を加算、 daw_01 #108、 画面端でも横で操作可)
//! - Ctrl + drag → sensitivity × 0.1 (fine、 knob/fader と同 idiom)
//! - dblclick (300ms / 5px 以内) → `default_value` リセット + `on_change(default)`
//! - press → 4px 未満で release → text input mode (`editing_text = true`)、 内部 `text_input_at_focused`
//!   が IME / 選択 / Esc rollback / Enter commit を担う
//! - text input mode Enter → committed_text を `format` で parse + range clamp + `on_change(parsed)`
//! - text input mode Esc / focus loss → 静かに rollback (= 元 value 表示に戻る)

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::theme::Palette;
use crate::ui::{Ui, hovered};
use crate::widgets::text_input::TextInputStyle;

/// ダブルクリック判定の時間しきい値 (ms)。 knob/fader と統一。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。 knob/fader と統一。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// drag → text edit 切替の閾値 (px)。 4px 未満の release は短 click 扱いで text input mode に入る。
const DRAG_THRESHOLD_PX: f32 = 4.0;
/// Ctrl + drag の fine sensitivity 倍率。 knob/fader と統一。
const FINE_DRAG_SCALE: f32 = 0.1;

/// 数値の表示書式。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrubableNumberFormat {
    /// 整数表示 (例: BPM の int 部、 TimeSig num)。 `value.round() as i64` で表示 / parse。
    Integer,
    /// 小数 N 桁 (例: `Decimal(1)` で `"120.0"`、 `Decimal(3)` で `"120.345"`)。
    Decimal(u8),
    /// 1-based **小節.拍** 表記。 内部値は 4 分音符 beat、 `beats_per_bar` は 1 小節
    /// の beat 数 (4/4 → 4)。 表示は末尾の不要な 0 / 小数点を落とす (例 `8.0` beat →
    /// `"3.1"`、 `9.5` beat → `"3.2.5"`)。 入力は最初の `.` で小節と拍を分割し、
    /// `"3"` / `"3.1"` / `"3.2.5"` を受ける。 ドメイン非依存にするため拍/小節換算は
    /// `beats_per_bar` 引数で受け取る (UI ライブラリは time signature を知らない)。
    BarBeat { beats_per_bar: f64 },
    /// **零点対称の符号付き値** を「側のラベル + 絶対量」で表記する (負 → `neg`、
    /// 正 → `pos`、 零 → `center`)。 pan の `"L50"` / `"C"` / `"R100"`、 balance、
    /// stereo width、 EQ tilt 等が該当する DAW 慣習表記。
    ///
    /// `scale` は「表示する数字 = |値| × scale」 の倍率で、 数字は四捨五入した整数
    /// (参照 DAW はいずれも整数表示: REAPER `100%L`、 Live `50L`)。 例: pan の内部値が
    /// `-1.0..=1.0` なら `scale = 100.0` で `-0.5` → `"L50"`。
    ///
    /// 入力は表示と同じ土俵で受ける (WYSIWYG): `"L50"` / `"50L"` / `"l 50"` / `"C"` /
    /// `"R30"` / `"30r"`、 およびラベル無しの素の数字 (`"-50"` → 負側 50 = `-0.5`)。
    /// ドメイン非依存にするためラベル文字列と倍率は caller が渡す (`BarBeat` と同じ作法。
    /// UI ライブラリは「左右」 が pan なのか balance なのかを知らない)。
    SignedLabeled {
        /// 負側のラベル (pan なら `"L"`)。
        neg: &'static str,
        /// 正側のラベル (pan なら `"R"`)。
        pos: &'static str,
        /// 零点のラベル (pan なら `"C"`)。
        center: &'static str,
        /// 表示数字 = `|値| × scale` の倍率 (pan の `-1..1` → `0..100` なら `100.0`)。
        scale: f64,
    },
}

impl ScrubableNumberFormat {
    /// 値を書式に従って文字列化する。 **値 ⇄ 文字列 写像の SSoT** で、 widget の表示 /
    /// text input の prefill / アプリ側の read-only readout がすべてこれを共有する。
    #[must_use]
    pub fn format_value(self, value: f64) -> String {
        format_value(value, self)
    }

    /// 文字列を書式に従って値へ (失敗で `None`)。 [`Self::format_value`] の逆写像。
    #[must_use]
    pub fn parse_value(self, text: &str) -> Option<f64> {
        parse_value(text, self)
    }
}

/// `scrubable_number_at` のスタイル + sensitivity + range。
#[derive(Debug, Clone, Copy)]
pub struct ScrubableNumberStyle {
    /// 通常時の rect 塗り色。
    pub bg_color: Color,
    /// hover 時の rect 塗り色 (subtle に切替)。
    pub bg_color_hovered: Color,
    /// drag scrub 中の rect 塗り色 (= scrub 中であることを visual 強調)。
    pub bg_color_dragging: Color,
    /// 数値テキストの色。
    pub text_color: Color,
    /// rect 枠線色。
    pub border: Color,
    /// rect 枠線太さ (px)。
    pub border_width: f32,
    /// rect 角丸 (px)。
    pub radius: f32,
    /// 数値テキストの font size (px)。
    pub font_size: f32,
    /// 欄左端から数値テキストまでの内側余白 (px)。 **表示と、 click で入る text input
    /// モードの両方がこれを使う** ので、 編集に入っても文字が横にずれない。 狭い欄
    /// (mixer strip の pan 等) では詰め、 広い欄では広げる。
    pub pad_x: f32,
    /// scrub sensitivity: **`units_per_pixel`** (daw_01 #035 Q1 = (B) 確定)。
    /// 例: BPM 入力で `sensitivity = 0.5` なら `1 px drag = 0.5 BPM 変化`。 Ctrl 押下時は
    /// この値 × `FINE_DRAG_SCALE` (= 0.1) で 10 倍精細に。 `range` の有無に依存しない absolute scale。
    pub sensitivity: f32,
    /// Optional 値範囲 (clamp 用、 widget が `on_change` 呼び出し前に clamp する)。 `None` で
    /// clamp 無し (= caller 責任で on_change 受信側 / parse 時に clamp)。 daw_01 #035 Q3 = yes 確定。
    pub range: Option<(f64, f64)>,
}

impl ScrubableNumberStyle {
    /// パレット由来の既定スタイル (窪んだ数値欄 = `inset_bg` / hover で `inset_bg_hover` /
    /// scrub 中は `accent`)。
    ///
    /// r.md #48: `Default` を持たせない。 テーマ色を読む `Default::default()` は隠れた
    /// グローバル依存で、 ライトテーマに追従しない (= caller が `..Default::default()` を
    /// 書いた瞬間ダーク固定の色が混ざる)。 呼び出し側は `ui.palette()` を渡す。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg_color: p.inset_bg,
            bg_color_hovered: p.inset_bg_hover,
            bg_color_dragging: p.accent,
            text_color: p.text,
            border: p.border,
            border_width: 1.0,
            radius: 3.0,
            font_size: 14.0,
            pad_x: 4.0,
            sensitivity: 0.5,
            range: None,
        }
    }
}

/// 割り当て済み 1 本の modulation routing の視覚化記述 (Bitwig 流の色帯、 daw_01 #107)。
///
/// `depth` は **widget が描く plain 値単位** (= [`ScrubableNumberStyle::range`] と同じ値ドメイン)。
/// polarity (bipolar の `±` / unipolar の片側) は caller が符号で解決して渡す前提で、 widget は
/// `base` から `base + depth` までを 1 本の帯として描くだけ。
#[derive(Debug, Clone, Copy)]
pub struct ModEntry {
    /// source に割り当てられた色。 複数 entry は strip を縦に等分して各々この色で描く。
    pub color: Color,
    /// base からの到達量 (plain 値単位、 符号付き)。 0 で帯なし。
    pub depth: f64,
}

/// depth ドラッグ編集 (= ある source を arm = 割当モードにしている) の記述 (daw_01 #107)。
///
/// `Modulation::edit` が `Some` の間、 widget の press + 縦横 drag は **base 値でなく depth** を
/// 変化させ (base scrub は抑止 = 非破壊)、 移動した hold frame ごとに `on_mod_change(new_depth)`
/// を発火する。 undo bracket は [`ScrubableNumberResponse::mod_dragging`] の edge を見て daw が
/// 発火する想定 (base scrub と違い widget は Undoable wrap しない)。
pub struct ModEdit<'a, M: ?Sized + 'static> {
    /// 編集中 source の色 (= 枠 / 編集帯の強調に使う)。
    pub source_color: Color,
    /// 現在の depth (plain 値単位、 polarity 解決済)。 drag anchor の初期値。
    pub current_depth: f64,
    /// depth の clamp 範囲 (plain 値単位)。 `None` で clamp 無し。
    pub depth_range: Option<(f64, f64)>,
    /// depth drag の sensitivity (units_per_pixel)。 `None` で [`ScrubableNumberStyle::sensitivity`]
    /// を流用 (depth が base と同じ値スパンなら自然)。 depth_range のスパンが base range と大きく
    /// 異なり「同じ px で同じ割合動かしたい」 ときは `Some` で専用値を渡す (daw_01 #107 で 流用/専用
    /// 両対応の要望)。
    pub depth_sensitivity: Option<f32>,
    /// depth 変化時の Edit を作る closure。 widget が即時 call して `push_edit` するため
    /// `'static` / `Clone` / `Send` は不要 (= borrow で渡せる)。 返す Edit の制約のみ caller 責任。
    pub on_mod_change: &'a dyn Fn(f64) -> Edit<M>,
}

/// `scrubable_number_at` に渡す Bitwig 流 modulation 記述 (optional、 daw_01 #107)。
///
/// `None` で従来描画・従来挙動 (完全回帰)。 `Some` でも `entries` 空 + `live_value` None +
/// `edit` None なら描画差分なし。 帯 / live tick の位置算出には [`ScrubableNumberStyle::range`]
/// が必須 (range 無しのとき帯は描かれず、 depth-edit の枠強調のみ出る)。
pub struct Modulation<'a, M: ?Sized + 'static> {
    /// 割り当て済み routing の視覚化 (色帯で重畳描画)。 空で帯なし。
    pub entries: &'a [ModEntry],
    /// 変調後の現在実値 (= 可動 live tick)。 **plain 値単位** ([`ScrubableNumberStyle::range`] と
    /// 同じドメイン、 正規化値でない)。 `None` で描かない。 ~30Hz 更新前提で overlay 描画
    /// (= cache に載せず毎フレーム描く)。
    pub live_value: Option<f64>,
    /// `Some` で depth ドラッグ編集モード (base scrub 抑止)。 `None` で従来 base scrub。
    pub edit: Option<ModEdit<'a, M>>,
}

/// `scrubable_number_at` の戻り値。
///
/// `bool` field を 3 つ持つが、 各々 (hovered / dragging / editing_text / committed) は
/// **意味的に独立** な observability であり、 state machine 化すると caller の if 文が増えて
/// boilerplate になる (= response struct は「外部から見える観測可能 flag の bag」 という慣習)。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct ScrubableNumberResponse {
    /// 描画された値 (= drag 中の preview、 idle 時は caller value、 reset frame は default_value)。
    pub displayed_value: f64,
    /// rect 上に cursor が乗っているか。
    pub hovered: bool,
    /// drag scrub 中 (= press → 4px 以上動いた状態 → release まで true)。 edge 検出で
    /// caller が `ParamGestureBegin/End` を発火する。
    pub dragging: bool,
    /// modulation depth の drag 編集中 (= `Modulation::edit` Some + press → 4px 超 → release まで)。
    /// edge 検出で caller が `ParamGestureBegin/End` 相当の undo bracket を発火する (daw_01 #107)。
    /// base `dragging` とは排他 (depth-edit 中は base scrub を抑止する)。
    pub mod_dragging: bool,
    /// text input mode に入っているか (= キーボード入力受付中、 cursor 表示)。
    pub editing_text: bool,
    /// 文字入力 commit (Enter or NumpadEnter) の瞬間 true、 1 frame のみ。
    /// `edit_text` を caller が parse する代わりに、 widget が format-aware で parse 済の値を
    /// `on_change(parsed)` で発火する idiom (= daw_01 #035 Q3 確定の「widget は edit_text の parse
    /// をしない」 とは別解釈: widget の `format` を SSoT として parse する方が caller boilerplate ゼロ。
    /// caller が独自 parse したい場合は `edit_text` を読んで自前で push_edit すれば良い)。
    pub committed: bool,
    /// editing_text == true のときの現在のテキストバッファ。 caller の参照用 (= widget は
    /// `format` で parse 済の `on_change(f64)` を発火するため、 通常 caller は読まなくて良い)。
    pub edit_text: Option<String>,
}

/// scrubable_number の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct ScrubableNumberState {
    /// drag anchor (press 時の `(pointer_x, pointer_y, value, ctrl)`)。 `Some` で press 中 (= drag or short-click 判定待ち)。
    drag_anchor: Option<DragAnchor>,
    /// drag 累積距離 (px、 縦横の合成 = `hypot(dx, dy)` の最大値)。 release 時に DRAG_THRESHOLD_PX
    /// 未満なら short-click → editing (横ドラッグでも端で閾値に入る、 daw_01 #108)。
    drag_distance: f32,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
    /// drag 開始時の値 (release frame で undoable Edit の inverse に使う、 knob/fader と同 idiom)。
    drag_initial_value: Option<f64>,
    /// text input mode に入っているか (= editing_text)。 release で `drag_distance < DRAG_THRESHOLD_PX`
    /// のとき true へ遷移、 inner text_input が focus loss / commit で false へ戻る。
    editing: bool,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// press 位置の x。 横ドラッグ (右=増 / 左=減) の起点 (daw_01 #108)。
    pointer_x: f32,
    pointer_y: f32,
    /// 基準値。 base scrub では press 時の base 値、 depth-edit では press 時の depth 値。
    value: f64,
    /// 押下時の Ctrl 状態。 mid-drag で Ctrl toggle 時に再 anchor する判定用 (knob/fader と同 idiom)。
    ctrl: bool,
    /// この gesture が depth-edit (= `Modulation::edit` Some) で開始したか。 true なら drag は
    /// base でなく depth を変化させる。 gesture 途中で固定 (arm 状態が変わっても継続)。
    depth_drag: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
}

/// 値を `format` に従って文字列化する。
fn format_value(value: f64, format: ScrubableNumberFormat) -> String {
    match format {
        ScrubableNumberFormat::Integer => {
            // round to nearest int (= 120.6 → 121)。 i64 cast は range 内前提だが NaN/Inf 防御も。
            if value.is_finite() {
                #[allow(clippy::cast_possible_truncation)]
                let v_i = value.round() as i64;
                v_i.to_string()
            } else {
                "0".to_string()
            }
        }
        ScrubableNumberFormat::Decimal(n) => {
            // `n` は表示桁数 (例: 1 で "120.0")。
            format!("{:.*}", usize::from(n), value)
        }
        ScrubableNumberFormat::BarBeat { beats_per_bar } => format_bar_beat(value, beats_per_bar),
        ScrubableNumberFormat::SignedLabeled { neg, pos, center, scale } => {
            format_signed_labeled(value, neg, pos, center, scale)
        }
    }
}

/// 文字列を `format` に従って parse する (失敗で `None`)。
fn parse_value(text: &str, format: ScrubableNumberFormat) -> Option<f64> {
    let trimmed = text.trim();
    match format {
        ScrubableNumberFormat::Integer => trimmed.parse::<i64>().ok().map(|v| v as f64),
        ScrubableNumberFormat::Decimal(_) => trimmed.parse::<f64>().ok(),
        ScrubableNumberFormat::BarBeat { beats_per_bar } => parse_bar_beat(trimmed, beats_per_bar),
        ScrubableNumberFormat::SignedLabeled { neg, pos, center, scale } => {
            parse_signed_labeled(trimmed, neg, pos, center, scale)
        }
    }
}

/// 零点対称の符号付き値 → `"L50"` / `"C"` / `"R100"` 形式。 数字は `|value| × scale` の
/// 四捨五入整数で、 丸めて 0 になる値 (= 零点の極近傍) は `center` に落とす (= 表示上の
/// `"L0"` / `"R0"` を作らない)。 非有限値は `center`。
fn format_signed_labeled(
    value: f64,
    neg: &str,
    pos: &str,
    center: &str,
    scale: f64,
) -> String {
    if !value.is_finite() {
        return center.to_string();
    }
    let magnitude = (value.abs() * scale).round();
    if magnitude < 1.0 {
        return center.to_string();
    }
    let label = if value < 0.0 { neg } else { pos };
    format!("{label}{magnitude:.0}")
}

/// `"L50"` / `"50L"` / `"l 50"` / `"C"` / `"R30"` / `"30r"` / 素の数字 (`"-50"`) → 値。
/// 表示と同じ土俵 (= `scale` を掛けた数字) で受け、 `value = ±magnitude / scale` を返す。
/// ラベルの照合は ASCII 大文字小文字を無視する。
fn parse_signed_labeled(
    text: &str,
    neg: &str,
    pos: &str,
    center: &str,
    scale: f64,
) -> Option<f64> {
    if scale == 0.0 || !scale.is_finite() {
        return None;
    }
    let t = text.trim();
    if t.eq_ignore_ascii_case(center) {
        return Some(0.0);
    }
    // 先頭 / 末尾のどちらに付いたラベルも受ける (`"L50"` と `"50L"`)。
    let strip = |s: &str, label: &str| -> Option<String> {
        if label.is_empty() {
            return None;
        }
        let lower = s.to_ascii_lowercase();
        let l = label.to_ascii_lowercase();
        lower
            .strip_prefix(&l)
            .or_else(|| lower.strip_suffix(&l))
            .map(|rest| rest.trim().to_string())
    };
    let (sign, number) = if let Some(rest) = strip(t, neg) {
        (-1.0, rest)
    } else if let Some(rest) = strip(t, pos) {
        (1.0, rest)
    } else {
        // ラベル無しの素の数字は符号込みでそのまま (`"-50"` → 負側 50)。
        (1.0, t.to_string())
    };
    let magnitude: f64 = number.parse().ok()?;
    let value = sign * magnitude / scale;
    value.is_finite().then_some(value)
}

/// beat → 1-based "小節.拍" 文字列。 `beat_to_bar_beat` (common::timing) と同じ式
/// (bar = floor(beat/bpb)+1、 beat_in_bar = beat-(bar-1)*bpb+1) なので ruler /
/// transport と表記が一致する。 末尾の不要な 0 / 小数点は落とす。
fn format_bar_beat(beat: f64, beats_per_bar: f64) -> String {
    if beats_per_bar <= 0.0 || !beat.is_finite() {
        return "1.1".to_string();
    }
    let bar_idx = (beat / beats_per_bar).floor().max(0.0);
    let beat_in_bar = beat - bar_idx * beats_per_bar + 1.0;
    #[allow(clippy::cast_possible_truncation)]
    let bar_num = bar_idx as i64 + 1;
    format!("{bar_num}.{}", trim_decimals(beat_in_bar))
}

/// "小節" / "小節.拍" (拍は小数可) → beat。 最初の `.` で小節と拍を分割するので、
/// `"3"` (= 拍 1 既定) / `"3.1"` / `"3.2.5"` を受ける。 不正入力は `None`。
fn parse_bar_beat(text: &str, beats_per_bar: f64) -> Option<f64> {
    if beats_per_bar <= 0.0 {
        return None;
    }
    let (bar_str, beat_str) = text.split_once('.').unwrap_or((text, ""));
    let bar: f64 = bar_str.trim().parse().ok()?;
    let beat_str = beat_str.trim();
    let beat_in_bar: f64 = if beat_str.is_empty() {
        1.0
    } else {
        beat_str.parse().ok()?
    };
    if !bar.is_finite() || !beat_in_bar.is_finite() {
        return None;
    }
    let beat = (bar - 1.0) * beats_per_bar + (beat_in_bar - 1.0);
    beat.is_finite().then_some(beat)
}

/// f64 を末尾 0 / 小数点を落として文字列化 (`1.0`→`"1"`、 `2.5`→`"2.5"`)。
fn trim_decimals(v: f64) -> String {
    let s = format!("{v:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// 矩形指定で drag-to-edit な数値入力を描画 + 処理。
    ///
    /// 値変化時 (= drag scrub / dblclick reset / text commit) に `on_change(new_value)` を 1 度発火する。
    /// drag 中は per-frame 連続発火 (daw_01 #035 Q2 = (A) 確定)、 release で最終値も発火。
    ///
    /// `value`: 表示中の plain 値 (f64 で精度確保)。
    /// `default_value`: dblclick リセット時の値。 `style.range` の clamp は widget 側で実施。
    /// `format`: 表示書式 (Integer / Decimal(N))。 text input mode の parse もこれを SSoT に。
    /// `style`: 色 / sensitivity (units_per_pixel) / 任意 range など。
    /// `on_change`: 値変化時の Edit を作る closure (knob_at と同形)。drag scrub / dblclick reset /
    ///   text commit のいずれも同じ closure で forward mutation を発行する (undo はアプリ層の責務)。
    /// `placeholder`: `Some(s)` かつ **idle** (`!editing_text && drag_anchor 無`) のとき、 数値の
    ///   代わりに `s` を描画する (= 複数選択で値が割れている mixed 項目を `"—"` 表示する用途、 daw_01 #103)。
    ///   drag scrub 中 / 編集中は live 値・編集中テキストを優先 (placeholder 抑制)。 編集開始時の
    ///   text_input seed は placeholder ではなく渡された base `value` を `format` した文字列。
    ///   通常は `None`。
    /// `modulation`: `Some` で Bitwig 流 modulation を表示・編集する (daw_01 #107)。 `None` で
    ///   従来描画・従来挙動 (完全回帰)。 [`Modulation::entries`] を base 値からの色帯で重畳描画、
    ///   [`Modulation::live_value`] を可動 tick で描画、 [`Modulation::edit`] が `Some` のとき
    ///   press + 縦横 drag は base でなく depth を変化させ `on_mod_change` を発火する (base scrub 抑止)。
    ///   帯 / tick の位置算出には `style.range` が必須 (無いと depth-edit 枠強調のみ)。
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn scrubable_number_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f64,
        default_value: f64,
        format: ScrubableNumberFormat,
        style: &ScrubableNumberStyle,
        on_change: F,
        placeholder: Option<&str>,
        modulation: Option<Modulation<'_, M>>,
    ) -> ScrubableNumberResponse
    where
        F: Fn(f64) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"scrubable_number", &id));
        // 内部 `text_input_at_focused` の inner widget id を construct するため、 outer id を
        // 1 度 hash 化して u64 seed として保持。 `id: impl Hash + Clone` の `Clone` 要求を回避
        // (= 既存 widget の `impl Hash` のみと API 統一)、 hash 衝突は WidgetId のドメインで
        // unique 化された outer id に紐つくため実用上 zero。
        let id_seed: u64 = hash_inputs((b"scrubable_number_id_seed", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // ---- modulation 記述の展開 (None = 完全回帰、 borrow のみ取り出す) ----
        let mod_ref = modulation.as_ref();
        let mod_entries: &[ModEntry] = mod_ref.map_or(&[], |m| m.entries);
        let mod_live = mod_ref.and_then(|m| m.live_value);
        let mod_edit = mod_ref.and_then(|m| m.edit.as_ref());
        let depth_mode = mod_edit.is_some();
        let current_depth = mod_edit.map_or(0.0, |e| e.current_depth);
        let depth_range = mod_edit.and_then(|e| e.depth_range);
        // depth drag の sensitivity: ModEdit 指定が無ければ base scrub の sensitivity を流用。
        let depth_sens = mod_edit
            .and_then(|e| e.depth_sensitivity)
            .unwrap_or(style.sensitivity);

        // ---- press / drag / release 処理 (knob と同 pattern + drag distance 計測) ----
        let mut reset_fired = false;
        let mut release_initial_value: Option<f64> = None;
        let mut short_click_release = false;
        // depth gesture の release frame で確定する最終 depth (= pointer の最終位置から再計算)。
        let mut release_depth: Option<f64> = None;
        let (drag_anchor, drag_distance, was_editing) = {
            let state: &mut ScrubableNumberState = self.widget_state(wid);

            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && inside
            {
                let now = Instant::now();
                let is_double = state.last_click.is_some_and(|c| {
                    now.duration_since(c.when).as_millis() < DOUBLE_CLICK_MS
                        && (c.pos.0 - px).hypot(c.pos.1 - py) < DOUBLE_CLICK_PX
                });

                if is_double && !depth_mode {
                    // dblclick → default reset (= editing も解除)。 depth-edit 中は base を
                    // 触らない (非破壊) ので dblclick reset は抑止し下の通常 press 扱いにする。
                    state.last_click = None;
                    state.drag_anchor = None;
                    state.drag_initial_value = None;
                    state.drag_distance = 0.0;
                    state.editing = false;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    // depth-edit 中は anchor の基準値を base でなく現 depth にする。
                    let anchor_value = if depth_mode { current_depth } else { value };
                    state.drag_anchor = Some(DragAnchor {
                        pointer_x: px,
                        pointer_y: py,
                        value: anchor_value,
                        ctrl: pointer.modifiers.ctrl,
                        depth_drag: depth_mode,
                    });
                    state.drag_initial_value = Some(value);
                    state.drag_distance = 0.0;
                    // press 時点で editing なら外す (= 新規 press で text input 終了)。
                    // ただし inner text_input の focus は別経路で残るので、 ここでは editing flag のみ。
                    state.editing = false;
                }
            }

            // mid-drag で Ctrl toggle されたら anchor 再設定 (= 値 jump 回避、 knob/fader と同 idiom)。
            // depth-edit gesture は depth 基準で、 base gesture は base 基準で再 anchor する。
            if let Some(anchor) = state.drag_anchor
                && let Some((px, py)) = pointer.pos
                && pointer.modifiers.ctrl != anchor.ctrl
            {
                let anchor_value = if anchor.depth_drag { current_depth } else { value };
                state.drag_anchor = Some(DragAnchor {
                    pointer_x: px,
                    pointer_y: py,
                    value: anchor_value,
                    ctrl: pointer.modifiers.ctrl,
                    depth_drag: anchor.depth_drag,
                });
            }

            // drag 距離計測 (= drag_anchor が Some の間、 縦横合成 hypot(dx, dy) の最大値を保持)。
            if let (Some(anchor), Some((px, py))) = (state.drag_anchor, pointer.pos) {
                let dist = (px - anchor.pointer_x).hypot(py - anchor.pointer_y);
                if dist > state.drag_distance {
                    state.drag_distance = dist;
                }
            }

            if pointer.primary_just_released {
                let anchor_opt = state.drag_anchor;
                let init = state.drag_initial_value.take();
                let dist = state.drag_distance;
                let was_pressed = anchor_opt.is_some();
                let was_depth = anchor_opt.is_some_and(|a| a.depth_drag);
                state.drag_anchor = None;
                state.drag_distance = 0.0;
                if was_depth {
                    // depth gesture の release: per-frame は anchor が None になる release frame
                    // で fire しないため、 pointer の最終位置から depth を再計算して 1 度確定発火する
                    // (release frame で pointer が動いていた場合の最終値取りこぼし防止、 daw_01 #107
                    // 「release で最終 depth も発火」)。 閾値超 (= 実 drag) のみ。
                    if dist >= DRAG_THRESHOLD_PX
                        && let (Some(anchor), Some((px, py))) = (anchor_opt, pointer.pos)
                    {
                        release_depth =
                            Some(clamp_opt(raw_drag_value(anchor, px, py, depth_sens), depth_range));
                    }
                } else {
                    // base scrub のみ release で undoable wrap するため release_initial_value を残し、
                    // short-click → text input mode も base gesture 限定 (depth は text 編集しない)。
                    release_initial_value = init;
                    if was_pressed && dist < DRAG_THRESHOLD_PX && inside && !reset_fired {
                        state.editing = true;
                        short_click_release = true;
                    }
                }
            }

            (state.drag_anchor, state.drag_distance, state.editing)
        };

        // ---- 表示値の決定 ----
        // base 数値テキスト: reset > base scrub (depth-edit gesture 中は抑止) > value。
        // drag 分岐は `DRAG_THRESHOLD_PX` 超のみ (= doc の「合成 >= 4px → scrub 開始」)。
        // gate しないと click-to-edit の手ぶれ 1-3px でも per-frame on_change が発火し、
        // short-click release は Undoable wrap を skip するため undo 不能な値変化が残る。
        let displayed_value = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((px, py))) = (drag_anchor, pointer.pos)
            && !anchor.depth_drag
            && drag_distance >= DRAG_THRESHOLD_PX
        {
            clamp_opt(raw_drag_value(anchor, px, py, style.sensitivity), style.range)
        } else {
            value
        };

        // depth 値 (= modulation 帯 + on_mod_change): depth-edit gesture drag 中のみ更新。
        // base と同じく閾値 gate (release 側の `dist >= DRAG_THRESHOLD_PX` と対)。
        let displayed_depth = if let (Some(anchor), Some((px, py))) = (drag_anchor, pointer.pos)
            && anchor.depth_drag
            && drag_distance >= DRAG_THRESHOLD_PX
        {
            clamp_opt(raw_drag_value(anchor, px, py, depth_sens), depth_range)
        } else {
            current_depth
        };

        // ---- on_change 発火 (drag 中 = per-frame、 reset 1 回、 release final、 commit 1 回) ----
        let mut committed = false;
        let mut edit_text: Option<String> = None;
        // base scrub と depth-edit は排他 (anchor.depth_drag で判定)。
        let dragging_now =
            drag_anchor.is_some_and(|a| !a.depth_drag) && drag_distance >= DRAG_THRESHOLD_PX;
        let mod_dragging =
            drag_anchor.is_some_and(|a| a.depth_drag) && drag_distance >= DRAG_THRESHOLD_PX;

        // depth (modulation) per-frame 発火: depth-edit gesture が hold 中 (release 後は anchor が
        // None になり displayed_depth == current_depth で skip)。 release は per-frame 済 + daw が
        // mod_dragging falling edge で undo bracket するため widget 側で追加 commit はしない。
        if let Some(edit) = mod_edit
            && drag_anchor.is_some_and(|a| a.depth_drag)
            && (displayed_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(displayed_depth));
        }

        // depth release-frame の最終確定発火 (= 上の per-frame は release frame で skip される)。
        if let Some(edit) = mod_edit
            && let Some(final_depth) = release_depth
            && (final_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(final_depth));
        }

        // reset: dblclick で default にリセット (1 frame、Mutate 1 発)。undo はアプリ層 (S4a)。
        if reset_fired && (default_value - value).abs() > f64::EPSILON {
            self.push_edit(on_change(default_value));
        }

        // drag 中の per-frame 発火 (= short-click release は除外、 reset は別経路で済、
        // release frame も skip — release frame は下で最終値を 1 度 commit する)。
        if !short_click_release
            && !reset_fired
            && release_initial_value.is_none()
            && (displayed_value - value).abs() > f64::EPSILON
        {
            self.push_edit(on_change(displayed_value));
        }

        // S4a: release 時の最終値 (= drag scrub 完了) を 1 度 commit (旧 Undoable の forward 相当)。
        // undo はアプリ層 (daw_gui SongDoc) が担う。
        if let Some(start_value) = release_initial_value
            && (start_value - displayed_value).abs() > f64::EPSILON
            && !short_click_release
        {
            self.push_edit(on_change(displayed_value));
        }

        // ---- text input mode (editing) の内蔵 delegate ----
        // `was_editing` が true なら inner `text_input_at_focused` を描画 (= focus 取得 + 全選択)。
        // daw_01 #112「テキスト入力は focus loss で確定」: commit (Enter) と blur (外 click) の
        // どちらでも `committed_text` を parse + clamp + `on_change` 発火して editing 解除。
        // Esc のみ確定せず rollback (= inner_resp.focused=false かつ committed/blurred でない経路)。
        if was_editing {
            let value_str = format_value(value, format);
            // inner text_input の id は outer id を hash 化した seed で unique 化 (= `Clone` 要求回避)。
            let inner_id = ("scrubable_number_inner", id_seed);
            // 編集モードのタイポグラフィは **表示と同一** にする (`style` の font_size /
            // pad_x)。 旧実装は text_input が font 14 / pad 8 固定だったため、
            // click した瞬間に文字サイズと位置が跳ね、 狭い欄では入力が欄からはみ出していた。
            let inner_style =
                TextInputStyle { font_size: style.font_size, pad_x: style.pad_x };
            let inner_resp = self.text_input_at_focused(
                inner_id,
                rect,
                &value_str,
                &inner_style,
                |_new: String| -> Edit<M> {
                    // typing per-frame では Edit 発火しない (= commit でまとめて発火する設計)。
                    Edit::mutate(|_: &mut M| {})
                },
            );

            // 確定 (Enter / 外 click による blur) の text を parse + clamp + on_change 発火。
            if (inner_resp.committed || inner_resp.blurred)
                && let Some(text) = &inner_resp.committed_text
                && let Some(parsed) = parse_value(text, format)
            {
                // clamp_opt と同じ防御 (反転 / 非有限 range で panic しない)。
                let final_value = clamp_opt(parsed, style.range);
                if (final_value - value).abs() > f64::EPSILON {
                    self.push_edit(on_change(final_value));
                }
                committed = true;
                // editing 終了 (= inner widget は次 frame で見えなくなる、 focus 自動解除は inner 側が
                // 担う想定で、 ここでは scrubable の editing flag だけ false に)。
                let state: &mut ScrubableNumberState = self.widget_state(wid);
                state.editing = false;
            }

            // 残りの focus loss (= Esc) は確定せず editing 終了 (rollback)。 上の commit/blur 分岐で
            // 既に editing=false でも冪等。
            if !inner_resp.focused {
                let state: &mut ScrubableNumberState = self.widget_state(wid);
                state.editing = false;
            }

            edit_text = Some(value_str);
        } else {
            // ---- 通常描画 (= 非 editing): 背景 + 数値テキスト ----
            let bg_fill = if dragging_now {
                style.bg_color_dragging
            } else if hovered(rect, pointer) {
                style.bg_color_hovered
            } else {
                style.bg_color
            };
            // placeholder: Some かつ idle (drag/press 中でない) のとき、 数値の代わりに
            // placeholder を描画 (= mixed 選択の「—」表示、 daw_01 #103)。 drag scrub 中は
            // live 値を出すため抑制 (`drag_anchor.is_none()` で gate)。
            let show_placeholder = placeholder.filter(|_| drag_anchor.is_none());
            let text = match show_placeholder {
                Some(ph) => ph.to_string(),
                None => format_value(displayed_value, format),
            };
            // input_hash で cache: 同じ表示値 / 同じ rect / 同じ bg なら再描画 skip。
            let input_hash = hash_inputs((
                b"scrubable_number",
                rect.x.to_bits(),
                rect.y.to_bits(),
                rect.w.to_bits(),
                rect.h.to_bits(),
                displayed_value.to_bits(),
                dragging_now,
                hovered(rect, pointer),
                style.font_size.to_bits(),
                style.pad_x.to_bits(),
                // **描画する文字列そのもの** を fold する。 値だけを hash すると (a) 同 id で
                // `format` を差し替えたとき cache HIT で旧表記が残り (同値でも `Decimal(2)` の
                // "-0.50" と `SignedLabeled` の "L50" は別物)、 (b) placeholder ⇔ 数値の切替も
                // 取りこぼす。 text は両方を包含するので placeholder 用の fold は不要。
                text.as_str(),
            ));
            let style_copy = *style;
            self.with_widget_node(wid, input_hash, |ui| {
                draw_scrubable_number(ui, rect, &text, bg_fill, &style_copy);
            });

            // ---- modulation overlay (= cache node の外、 毎フレーム描画) ----
            // live_value は ~30Hz 更新、 base/depth は drag 追従なので cache に載せず overlay 化。
            // bg/text の cache node は modulation 非依存のまま据え置き (None で完全回帰)。
            // piano_roll の `draw_lyrics` と同じ「overlay は cache の後に描く」 idiom。
            if mod_ref.is_some() {
                draw_modulation_overlay(
                    self,
                    rect,
                    style,
                    displayed_value,
                    displayed_depth,
                    mod_entries,
                    mod_live,
                    mod_edit.map(|e| e.source_color),
                );
            }
        }

        ScrubableNumberResponse {
            displayed_value,
            hovered: hovered(rect, pointer),
            dragging: dragging_now,
            mod_dragging,
            editing_text: was_editing,
            committed,
            edit_text,
        }
    }
}

fn draw_scrubable_number<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    text: &str,
    bg_fill: Color,
    style: &ScrubableNumberStyle,
) {
    // 背景 (rect 全体)。
    ui.push_rect(RectCommand {
        rect,
        fill: bg_fill,
        border: style.border,
        border_width: style.border_width,
        radius: [style.radius; 4],
        clip_rect: None,
    });

    // 数値テキスト (rect 中央寄せ、 horizontal は left-padded `style.pad_x`)。
    // 文字色は **実際に塗った背景の輝度** から auto-contrast で決める (r.md #48)。
    // drag 中は背景が `bg_color_dragging` に変わり、その色は caller 任意 (daw_gui は
    // `scrub_drag_bg` / `scrub_drag_bg_warm`) なので、`text_color` 固定だとテーマや
    // caller 次第で数値が読めなくなる。通常時 (= `bg_color` 塗り) はテーマの本文色をそのまま
    // 使い、ダークの見た目を変えない。
    let pad_x = style.pad_x;
    let line_h = style.font_size * 1.2;
    let tx = rect.x + pad_x;
    let ty = rect.y + (rect.h - line_h) * 0.5;
    let text_color = if bg_fill == style.bg_color {
        style.text_color
    } else {
        ui.palette().ink_for(bg_fill)
    };
    ui.push_text(GlyphArea {
        text: text.into(),
        left: tx,
        top: ty,
        font_size: style.font_size,
        line_height: line_h,
        color: text_color,
        clip_rect: Some(rect),
        ..GlyphArea::default()
    });
}

/// anchor + 現 pointer から raw drag 値を出す (base / depth 共用)。 **縦横両方向** で値変化させる
/// (daw_01 #108): 各軸が固定の意味を持ち (右=+ / 上=+ / 左=− / 下=−)、 両軸の符号付き移動量を
/// **加算** (`dx + (-dy)`) する。 純縦ドラッグは従来と完全一致 (dx=0) で回帰なし、 画面端でも横で
/// 操作できる。 Ctrl で `FINE_DRAG_SCALE` 倍精細 (sensitivity は units_per_pixel、 両軸共通)。
///
/// per-axis 合成の自然な帰結として、 **正確な右下 / 左上 の対角線では両軸が打ち消し合い値が
/// 変わらない** (右の + と下の − が相殺、 dead zone でなく一貫仕様)。 short-click 閾値は合成距離
/// `hypot(dx, dy)` (daw_01 #108「合成距離で端でも入りやすい」) で判定するので、 対角 drag は
/// dragging 表示に入っても値据え置きになりうる (= 閾値 metric と値 metric が別目的なため意図的)。
fn raw_drag_value(anchor: DragAnchor, px: f32, py: f32, sensitivity: f32) -> f64 {
    let scale = if anchor.ctrl { FINE_DRAG_SCALE } else { 1.0 };
    // 右 (px > anchor_x) で増加、 上 (py < anchor_y) で増加 (= DAW 慣習)。
    let delta_px = (px - anchor.pointer_x) - (py - anchor.pointer_y);
    anchor.value + f64::from(delta_px) * f64::from(sensitivity) * f64::from(scale)
}

/// `Some(range)` かつ `min <= max` のとき clamp、 それ以外 (`None` / 反転 bound / 非有限 bound) は
/// そのまま素通し。 `f64::clamp` は `min > max` や NaN bound で **panic** するため、 caller の range
/// 取り違えで widget を crash させないよう防御する (knob.rs の同名 helper と同仕様)。
fn clamp_opt(v: f64, range: Option<(f64, f64)>) -> f64 {
    match range {
        Some((min, max)) if min <= max => v.clamp(min, max),
        _ => v,
    }
}

/// modulation の色帯 + base マーカー + live tick + depth-edit 枠強調を描く (daw_01 #107)。
///
/// cache node の **後** に毎フレーム呼ばれる overlay (= live_value 30Hz / drag 追従でも
/// bg/text の cache を無効化しない)。 帯 / tick の位置算出には `style.range` が必須で、 無い
/// ときは depth-edit の枠強調だけ出して帯は描かない。
#[allow(clippy::too_many_arguments)]
fn draw_modulation_overlay<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    style: &ScrubableNumberStyle,
    base_value: f64,
    edit_depth: f64,
    entries: &[ModEntry],
    live_value: Option<f64>,
    edit_color: Option<Color>,
) {
    // depth-edit 中の枠強調 (range の有無に関係なく出す)。
    if let Some(c) = edit_color {
        ui.push_rect(RectCommand {
            rect,
            fill: Color::TRANSPARENT,
            border: c,
            border_width: 1.5,
            radius: [style.radius; 4],
            clip_rect: None,
        });
    }

    // 帯 / tick は値→x 写像が要る = range 必須。 無ければ枠強調のみで return。
    let Some((min, max)) = style.range else {
        return;
    };
    if max <= min {
        return;
    }
    let inset = 2.0_f32;
    let track_w = (rect.w - inset * 2.0).max(0.0);
    let value_to_x = |v: f64| -> f32 {
        // 非有限 (NaN/Inf) は track 左端に丸めて renderer に NaN 座標を渡さない
        // (caller bug 防御、 format_value と同じ姿勢)。
        if !v.is_finite() {
            return rect.x + inset;
        }
        #[allow(clippy::cast_possible_truncation)]
        let t = (((v - min) / (max - min)) as f32).clamp(0.0, 1.0);
        rect.x + inset + t * track_w
    };

    // 帯の strip (rect 下端、 数値テキストに被らない位置)。
    let strip_h = (rect.h * 0.16).clamp(2.5, 4.0);
    let strip_y = rect.y + rect.h - strip_h - 1.0;
    let base_x = value_to_x(base_value);

    // 割り当て済み routing を色帯で重畳 (複数は strip を縦に等分し各 row に 1 本ずつ)。
    let n = entries.len().max(1);
    #[allow(clippy::cast_precision_loss)]
    let row_h = (strip_h / n as f32).max(1.0);
    for (i, e) in entries.iter().enumerate() {
        let end_x = value_to_x(base_value + e.depth);
        let (x0, x1) = (base_x.min(end_x), base_x.max(end_x));
        #[allow(clippy::cast_precision_loss)]
        let ry = strip_y + i as f32 * row_h;
        ui.push_rect(RectCommand {
            rect: Rect { x: x0, y: ry, w: (x1 - x0).max(1.0), h: row_h },
            fill: Color { a: 0.9, ..e.color },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    }

    // depth-edit 中: 編集中 depth を source 色で strip 全高に重ね描き (drag の live feedback)。
    if let Some(c) = edit_color {
        let end_x = value_to_x(base_value + edit_depth);
        let (x0, x1) = (base_x.min(end_x), base_x.max(end_x));
        ui.push_rect(RectCommand {
            rect: Rect { x: x0, y: strip_y, w: (x1 - x0).max(1.0), h: strip_h },
            fill: Color { a: 0.85, ..c },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    }

    // base 位置のマーカー (= 帯の起点、 細い縦線)。 帯 / live tick / edit の **いずれも無い**
    // 空 modulation では描かない (= `Some` でも全内容空なら描画差分なしの contract を守る)。
    if !entries.is_empty() || live_value.is_some() || edit_color.is_some() {
        ui.push_rect(RectCommand {
            rect: Rect { x: base_x - 0.5, y: strip_y - 1.0, w: 1.0, h: strip_h + 2.0 },
            // modulation base 位置の中立マーカー線 (r.md #48 で専用トークン化)。
            fill: ui.palette().modulation_base_marker,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    }

    // live 変調値の可動 tick (最前面、 明るい縦線)。
    if let Some(lv) = live_value {
        let lx = value_to_x(lv);
        ui.push_rect(RectCommand {
            rect: Rect { x: lx - 0.75, y: strip_y - 2.0, w: 1.5, h: strip_h + 4.0 },
            // modulation の live 出力値 tick。 r.md #48 で fader / knob と同じ amber
            // トークンに統一 (旧実装はここだけ near-white で、変調系の色の語彙が割れていた)。
            fill: ui.palette().modulation_live,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

    struct BpmModel {
        bpm: f64,
    }

    fn rect_default() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 80.0, h: 28.0 }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_frame(
        host: &mut UiHost<BpmModel>,
        model: &BpmModel,
        rect: Rect,
        value: f64,
        default_value: f64,
        format: ScrubableNumberFormat,
        style: &ScrubableNumberStyle,
        pointer: PointerFrame,
        placeholder: Option<&str>,
    ) -> Vec<Edit<BpmModel>> {
        let mut scene = Scene::new();
        run_frame_scene(host, model, rect, value, default_value, format, style, pointer, placeholder, &mut scene)
    }

    /// `run_frame` と同一だが、 描画後の `scene` を呼び出し側に残す (= 描画テキストの検査用)。
    #[allow(clippy::too_many_arguments)]
    fn run_frame_scene(
        host: &mut UiHost<BpmModel>,
        model: &BpmModel,
        rect: Rect,
        value: f64,
        default_value: f64,
        format: ScrubableNumberFormat,
        style: &ScrubableNumberStyle,
        pointer: PointerFrame,
        placeholder: Option<&str>,
        scene: &mut Scene,
    ) -> Vec<Edit<BpmModel>> {
        let screen = PhysicalSize { width: 200, height: 100 };
        host.frame_to_edits(
            model,
            scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.scrubable_number_at(
                    "test",
                    rect,
                    value,
                    default_value,
                    format,
                    style,
                    |v| Edit::mutate(move |m: &mut BpmModel| m.bpm = v),
                    placeholder,
                    None,
                );
            },
        )
    }

    fn press_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn hold_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn release_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_released: true,
            ..PointerFrame::default()
        }
    }

    #[test]
    fn format_value_integer_rounds() {
        assert_eq!(format_value(120.4, ScrubableNumberFormat::Integer), "120");
        assert_eq!(format_value(120.6, ScrubableNumberFormat::Integer), "121");
    }

    #[test]
    fn format_value_decimal_precision() {
        assert_eq!(format_value(120.456, ScrubableNumberFormat::Decimal(1)), "120.5");
        assert_eq!(format_value(120.456, ScrubableNumberFormat::Decimal(3)), "120.456");
    }

    #[test]
    fn parse_value_handles_int_and_decimal() {
        assert_eq!(parse_value("120", ScrubableNumberFormat::Integer), Some(120.0));
        assert_eq!(parse_value("120.5", ScrubableNumberFormat::Decimal(1)), Some(120.5));
        assert_eq!(parse_value("abc", ScrubableNumberFormat::Integer), None);
        assert_eq!(parse_value("  120  ", ScrubableNumberFormat::Integer), Some(120.0));
    }

    #[test]
    fn format_bar_beat_4_4_trims_zeros() {
        // 4/4 (beats_per_bar = 4): beat 0 = 小節1拍1、 beat 8 = 小節3拍1。
        assert_eq!(format_bar_beat(0.0, 4.0), "1.1");
        assert_eq!(format_bar_beat(4.0, 4.0), "2.1");
        assert_eq!(format_bar_beat(8.0, 4.0), "3.1");
        assert_eq!(format_bar_beat(9.0, 4.0), "3.2");
        // sub-beat は末尾 0 を落として "小節.拍.端数" 風 ("3.2.5")。
        assert_eq!(format_bar_beat(9.5, 4.0), "3.2.5");
        // 退化入力。
        assert_eq!(format_bar_beat(f64::NAN, 4.0), "1.1");
        assert_eq!(format_bar_beat(5.0, 0.0), "1.1");
    }

    #[test]
    fn parse_bar_beat_roundtrips() {
        // "小節.拍" → beat。
        assert_eq!(parse_bar_beat("1.1", 4.0), Some(0.0));
        assert_eq!(parse_bar_beat("3.1", 4.0), Some(8.0));
        assert_eq!(parse_bar_beat("3.2", 4.0), Some(9.0));
        assert_eq!(parse_bar_beat("3.2.5", 4.0), Some(9.5));
        // 小節のみ → 拍1 既定。
        assert_eq!(parse_bar_beat("3", 4.0), Some(8.0));
        assert_eq!(parse_bar_beat("3.", 4.0), Some(8.0));
        // 不正入力。
        assert_eq!(parse_bar_beat("abc", 4.0), None);
        assert_eq!(parse_bar_beat("3.x", 4.0), None);
        assert_eq!(parse_bar_beat("3.1", 0.0), None);
        // round-trip: format → parse で同じ beat。
        for &beat in &[0.0_f64, 8.0, 9.0, 9.5, 13.25] {
            let s = format_bar_beat(beat, 4.0);
            let back = parse_bar_beat(&s, 4.0).unwrap();
            assert!((back - beat).abs() < 1e-6, "roundtrip {beat} -> {s} -> {back}");
        }
    }

    #[test]
    fn parse_value_bar_beat_via_format() {
        let f = ScrubableNumberFormat::BarBeat { beats_per_bar: 4.0 };
        assert_eq!(parse_value("3.1", f), Some(8.0));
        assert_eq!(format_value(8.0, f), "3.1");
    }

    /// `SignedLabeled` (pan の `"L50"` / `"C"` / `"R100"`) の表示。 零点へ丸まる値は
    /// `"L0"` / `"R0"` でなく center ラベルにする。
    #[test]
    fn signed_labeled_formats_side_and_center() {
        let f = pan_format();
        assert_eq!(f.format_value(-1.0), "L100");
        assert_eq!(f.format_value(-0.5), "L50");
        assert_eq!(f.format_value(-0.004), "C"); // |v|*100 = 0.4 → 四捨五入 0 → center
        assert_eq!(f.format_value(0.0), "C");
        assert_eq!(f.format_value(0.3), "R30");
        assert_eq!(f.format_value(1.0), "R100");
        assert_eq!(f.format_value(f64::NAN), "C");
    }

    /// `SignedLabeled` の入力: ラベルは前後どちらでも / 大文字小文字無視 / 素の数字は
    /// **表示と同じ土俵** (scale 側) で解釈。
    #[test]
    fn signed_labeled_parses_both_orders_and_bare_numbers() {
        let f = pan_format();
        let close = |a: Option<f64>, b: f64| (a.unwrap() - b).abs() < 1e-9;
        assert!(close(f.parse_value("L50"), -0.5));
        assert!(close(f.parse_value("50L"), -0.5));
        assert!(close(f.parse_value(" l 50 "), -0.5));
        assert!(close(f.parse_value("C"), 0.0));
        assert!(close(f.parse_value("c"), 0.0));
        assert!(close(f.parse_value("R30"), 0.3));
        assert!(close(f.parse_value("30r"), 0.3));
        assert!(close(f.parse_value("-50"), -0.5));
        assert!(close(f.parse_value("0"), 0.0));
        assert_eq!(f.parse_value("abc"), None);
        assert_eq!(f.parse_value("L"), None);
        assert_eq!(f.parse_value(""), None);
    }

    /// 表示 → 入力の往復で値が戻る (整数表記に丸まる分だけ量子化)。
    #[test]
    fn signed_labeled_round_trips() {
        let f = pan_format();
        for v in [-1.0_f64, -0.75, -0.5, -0.01, 0.0, 0.01, 0.42, 1.0] {
            let text = f.format_value(v);
            let back = f.parse_value(&text).unwrap();
            assert!((back - v).abs() < 0.005, "{v} → {text} → {back}");
        }
    }

    fn pan_format() -> ScrubableNumberFormat {
        ScrubableNumberFormat::SignedLabeled { neg: "L", pos: "R", center: "C", scale: 100.0 }
    }

    /// drag 上方向 (= dy negative) で値が増加、 sensitivity が units_per_pixel として効く。
    #[test]
    fn drag_up_increases_value_by_sensitivity() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        // press
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
            None,
        );
        for e in edits { e.apply(&mut model); }
        // drag up 20px (dy = -20) → expected 120 + (-(-20)) * 0.5 = 120 + 10 = 130
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), false),
            None,
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 130.0).abs() < 1e-5, "drag 上 20px × sensitivity 0.5 = +10 (got {})", model.bpm);
    }

    /// Ctrl + drag で sensitivity が 1/10 (fine) になる。
    #[test]
    fn ctrl_drag_uses_fine_sensitivity() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, true),
            None,
        );
        for e in edits { e.apply(&mut model); }
        // drag up 20px Ctrl → expected 120 + 20 * 0.5 * 0.1 = 120 + 1 = 121
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), true),
            None,
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 121.0).abs() < 1e-5, "Ctrl+drag は 1/10 = +1.0 (got {})", model.bpm);
    }

    /// range が Some なら widget が clamp してから on_change 発火 (= caller boilerplate ゼロ)。
    #[test]
    fn range_clamps_drag_result() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 200.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle {
            sensitivity: 10.0,                  // 1 px = 10 BPM
            range: Some((20.0, 240.0)),         // 上限 240
            ..ScrubableNumberStyle::from_palette(&Palette::dark())
        };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Integer, &style, press_at(center, false),
            None,
        );
        for e in edits { e.apply(&mut model); }
        // drag up 100px → raw = 200 + 100 * 10 = 1200、 clamp で 240。
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Integer, &style,
            hold_at((center.0, center.1 - 100.0), false),
            None,
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 240.0).abs() < 1e-5, "range upper で clamp (got {})", model.bpm);
    }

    /// dblclick で default_value に reset + on_change(default) 発火。
    #[test]
    fn double_click_resets_to_default() {
        use std::thread;
        use std::time::Duration;

        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 200.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle::from_palette(&Palette::dark());
        let center = (40.0_f32, 14.0_f32);

        // 1 回目 click
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
            None,
        );
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, release_at(center),
            None,
        );
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目 click (= dblclick)
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false),
            None,
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 120.0).abs() < 1e-5, "dblclick で default 120.0 にリセット (got {})", model.bpm);
    }

    /// 描画された glyph テキスト一覧を取り出す (placeholder 描画の検査用)。
    fn glyph_texts(scene: &Scene) -> Vec<String> {
        scene.iter_glyphs().map(|g| g.text.as_ref().to_string()).collect()
    }

    /// daw_01 #103: placeholder = Some は **idle 時のみ** 数値の代わりに描画 (mixed「—」)、
    /// drag scrub 中は抑制。 選択切替 Some→None は同 value/rect でも input_hash fold で即反映。
    #[test]
    fn placeholder_shows_when_idle_and_suppressed_during_drag() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);
        let idle = PointerFrame::default();

        // (1) idle + Some("—") → 「—」描画、 数値は出ない。
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, idle, Some("—"), &mut scene,
        );
        let t = glyph_texts(&scene);
        assert!(t.iter().any(|s| s == "—"), "idle placeholder で「—」描画 (got {t:?})");
        assert!(!t.iter().any(|s| s == "120.0"), "placeholder 中は数値を出さない (got {t:?})");

        // (2) 選択切替 Some→None: 同 value/rect でも input_hash fold で即「120.0」に切替
        //     (fold 漏れだと cache HIT で「—」が残る回帰ケース)。
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, idle, None, &mut scene,
        );
        let t = glyph_texts(&scene);
        assert!(t.iter().any(|s| s == "120.0"), "placeholder None で数値に即切替 (got {t:?})");
        assert!(!t.iter().any(|s| s == "—"), "切替後「—」が残らない (got {t:?})");

        // (3) drag 中 (press → hold 20px) + Some("—") → placeholder 抑制、 live 値 130.0 を描画。
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), Some("—"),
            &mut Scene::new(),
        );
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), false), Some("—"), &mut scene,
        );
        let t = glyph_texts(&scene);
        assert!(!t.iter().any(|s| s == "—"), "drag 中は placeholder 抑制 (got {t:?})");
        assert!(t.iter().any(|s| s == "130.0"), "drag 中は live 値 130.0 (120 + 20*0.5) (got {t:?})");
    }

    /// daw_01 #103: 編集開始 (短 click) で内側 text_input は placeholder ではなく base value から
    /// seed され、 編集中は placeholder「—」を抑制する。
    #[test]
    fn placeholder_suppressed_in_edit_mode_seeds_from_value() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle::from_palette(&Palette::dark());
        let center = (40.0_f32, 14.0_f32);

        // press → release (< 4px = 短 click) で editing 突入。 placeholder は Some("—")。
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), Some("—"),
            &mut Scene::new(),
        );
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, release_at(center), Some("—"), &mut scene,
        );
        let t = glyph_texts(&scene);
        assert!(!t.iter().any(|s| s == "—"), "編集中は placeholder「—」を抑制 (got {t:?})");
        assert!(
            t.iter().any(|s| s.contains("120.0")),
            "編集開始で base value 120.0 から seed (got {t:?})"
        );
    }

    /// r.md #62: **編集モードのタイポグラフィが表示と一致する**。 内側 `text_input` は
    /// font 14 / pad 8 固定だったため、 `style.font_size` を下げた欄 (inspector の 11px、
    /// mixer strip の 10px) は click した瞬間に文字サイズと開始 x が跳ね、 狭い欄では
    /// 入力が欄からはみ出していた。 表示 frame と編集 frame の GlyphArea を突き合わせる。
    #[test]
    fn edit_mode_uses_style_font_and_padding() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle {
            font_size: 10.0,
            pad_x: 4.0,
            ..ScrubableNumberStyle::from_palette(&Palette::dark())
        };
        let center = (40.0_f32, 14.0_f32);

        // (1) idle 表示。
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, PointerFrame::default(), None, &mut scene,
        );
        let idle = scene
            .iter_glyphs()
            .find(|g| g.text.as_ref() == "120.0")
            .expect("idle 表示の数値 glyph")
            .clone();

        // (2) 短 click (press → 4px 未満で release) で編集モードへ。
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
            &mut Scene::new(),
        );
        let mut scene = Scene::new();
        run_frame_scene(
            &mut host, &model, rect, 120.0, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, release_at(center), None, &mut scene,
        );
        let editing = scene
            .iter_glyphs()
            .find(|g| g.text.as_ref() == "120.0")
            .expect("編集モードの数値 glyph")
            .clone();

        assert!(
            (idle.font_size - style.font_size).abs() < 1e-6,
            "表示は style.font_size ({}) で描く (got {})",
            style.font_size,
            idle.font_size
        );
        assert!(
            (editing.font_size - style.font_size).abs() < 1e-6,
            "編集モードも style.font_size ({}) で描く (got {})",
            style.font_size,
            editing.font_size
        );
        assert!(
            (editing.left - idle.left).abs() < 1e-6,
            "編集モードで文字の開始 x が動かない (idle {} / editing {})",
            idle.left,
            editing.left
        );
        assert!(
            (idle.left - (rect.x + style.pad_x)).abs() < 1e-6,
            "文字の開始 x は rect.x + style.pad_x (got {})",
            idle.left
        );
    }

    // ---- daw_01 #107: Bitwig 流 modulation ----

    /// base (bpm) と depth を別々に持つ test model。
    struct ModModel {
        bpm: f64,
        depth: f64,
    }

    /// modulation 付き 1 frame を描画 + 処理し、 edits と response を返す。
    #[allow(clippy::too_many_arguments)]
    fn run_mod_frame(
        host: &mut UiHost<ModModel>,
        model: &ModModel,
        rect: Rect,
        style: &ScrubableNumberStyle,
        pointer: PointerFrame,
        edit_mode: bool,
        entries: &[ModEntry],
        live_value: Option<f64>,
        scene: &mut Scene,
    ) -> (Vec<Edit<ModModel>>, ScrubableNumberResponse) {
        let screen = PhysicalSize { width: 200, height: 100 };
        let base = model.bpm;
        let cur_depth = model.depth;
        let resp_cell: std::cell::RefCell<ScrubableNumberResponse> =
            std::cell::RefCell::new(ScrubableNumberResponse::default());
        let edits = host.frame_to_edits(
            model,
            scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let edit_desc = edit_mode.then_some(ModEdit {
                    source_color: Color::rgb(1.0, 0.4, 0.2),
                    current_depth: cur_depth,
                    depth_range: Some((-50.0, 50.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                });
                let modulation = Modulation { entries, live_value, edit: edit_desc };
                let r = ui.scrubable_number_at(
                    "mtest",
                    rect,
                    base,
                    120.0,
                    ScrubableNumberFormat::Decimal(1),
                    style,
                    |v| Edit::mutate(move |m: &mut ModModel| m.bpm = v),
                    None,
                    Some(modulation),
                );
                *resp_cell.borrow_mut() = r;
            },
        );
        (edits, resp_cell.into_inner())
    }

    fn mod_style() -> ScrubableNumberStyle {
        ScrubableNumberStyle {
            sensitivity: 0.5,
            range: Some((100.0, 140.0)),
            ..ScrubableNumberStyle::from_palette(&Palette::dark())
        }
    }

    /// arm 中 (edit_mode) の press + 縦 drag は **depth** を変化させ、 base (bpm) は触らない (非破壊)。
    #[test]
    fn mod_edit_drag_changes_depth_not_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 0.0 };
        let rect = rect_default();
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag up 20px → depth = 0 + 20 * 0.5 = 10。 bpm は不変。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, &style,
            hold_at((center.0, center.1 - 20.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.depth - 10.0).abs() < 1e-5, "depth scrub +10 (got {})", model.depth);
        assert!((model.bpm - 120.0).abs() < 1e-5, "base bpm は depth-edit 中 不変 (got {})", model.bpm);
        assert!(resp.mod_dragging, "depth drag 中は mod_dragging=true");
        assert!(!resp.dragging, "depth drag 中は base dragging=false (排他)");
    }

    /// 非 arm (edit_mode=false) の drag は従来どおり base を scrub し、 depth は触らない。
    #[test]
    fn non_arm_drag_scrubs_base_only() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 7.0 };
        let rect = rect_default();
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), false, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, &style,
            hold_at((center.0, center.1 - 10.0), false), false, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 125.0).abs() < 1e-5, "base scrub +5 (got {})", model.bpm);
        assert!((model.depth - 7.0).abs() < 1e-5, "非 arm では depth 不変 (got {})", model.depth);
        assert!(resp.dragging, "非 arm は base dragging=true");
        assert!(!resp.mod_dragging, "非 arm は mod_dragging=false");
    }

    /// arm 中 dblclick は base default reset を発火しない (非破壊)。
    #[test]
    fn mod_edit_dblclick_does_not_reset_base() {
        use std::thread;
        use std::time::Duration;

        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 200.0, depth: 0.0 };
        let rect = rect_default();
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, release_at(center), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        thread::sleep(Duration::from_millis(50));
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }

        assert!((model.bpm - 200.0).abs() < 1e-5, "arm 中 dblclick で base は reset されない (got {})", model.bpm);
    }

    /// `entries` を渡すと色帯 rect が overlay として追加され、 entry 色で描かれる。 `None` 回帰では出ない。
    #[test]
    fn entries_draw_colored_band_rects() {
        let band = Color::rgb(0.2, 0.8, 1.0);
        // (a) modulation None: overlay 無し (= bg rect のみ)。
        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { bpm: 120.0, depth: 0.0 };
        let style = mod_style();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(
            &model, &mut scene_none, PhysicalSize { width: 200, height: 100 },
            FrameInput::default(),
            |_, ui| {
                ui.scrubable_number_at(
                    "mtest", rect_default(), 120.0, 120.0,
                    ScrubableNumberFormat::Decimal(1), &style,
                    |v| Edit::mutate(move |m: &mut ModModel| m.bpm = v),
                    None, None,
                );
            },
        );
        let count_none = scene_none.rect_count();
        assert!(
            !scene_none.iter_rects().any(|r| (r.fill.b - 1.0).abs() < 1e-3 && r.fill.a < 0.95 && r.fill.g > 0.7),
            "None では band rect は出ない",
        );

        // (b) modulation Some + 1 entry: 帯 rect (entry 色) + base marker が追加。
        let mut host_s: UiHost<ModModel> = UiHost::no_redraw();
        let entries = [ModEntry { color: band, depth: 8.0 }];
        let mut scene_some = Scene::new();
        let (_, _) = run_mod_frame(
            &mut host_s, &model, rect_default(), &style,
            PointerFrame::default(), false, &entries, None, &mut scene_some,
        );
        assert!(
            scene_some.rect_count() > count_none,
            "entries で overlay rect が増える (none={count_none}, some={})",
            scene_some.rect_count(),
        );
        assert!(
            scene_some.iter_rects().any(|r| {
                (r.fill.r - band.r).abs() < 1e-3
                    && (r.fill.g - band.g).abs() < 1e-3
                    && (r.fill.b - band.b).abs() < 1e-3
            }),
            "entry 色の帯 rect が描かれる",
        );
    }

    /// `live_value` を渡すと可動 tick rect が 1 本追加される。
    #[test]
    fn live_value_draws_tick_rect() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { bpm: 120.0, depth: 0.0 };
        let style = mod_style();

        let mut scene_no_tick = Scene::new();
        run_mod_frame(&mut host, &model, rect_default(), &style, PointerFrame::default(), false, &[], None, &mut scene_no_tick);

        let mut host2: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_tick = Scene::new();
        run_mod_frame(&mut host2, &model, rect_default(), &style, PointerFrame::default(), false, &[], Some(130.0), &mut scene_tick);

        assert!(
            scene_tick.rect_count() > scene_no_tick.rect_count(),
            "live_value で tick rect が増える (no_tick={}, tick={})",
            scene_no_tick.rect_count(),
            scene_tick.rect_count(),
        );
    }

    /// depth gesture の release frame で pointer が動いた最終位置の depth が確定発火する。
    #[test]
    fn mod_edit_release_commits_final_depth() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 0.0 };
        let rect = rect_default();
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);

        // press → hold -20px (depth 10 fired & applied)。
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, &style,
            hold_at((center.0, center.1 - 20.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 10.0).abs() < 1e-5, "hold で depth 10 (got {})", model.depth);

        // release は press 位置より更に上 (-30px) で離す → 最終 depth 15 が release frame で確定。
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, &style,
            release_at((center.0, center.1 - 30.0)), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!(
            (model.depth - 15.0).abs() < 1e-5,
            "release frame で pointer 最終位置の depth 15 を確定発火 (got {})",
            model.depth,
        );
    }

    /// `depth_sensitivity: Some` は depth drag で base の `style.sensitivity` を上書きする。
    #[test]
    fn depth_sensitivity_overrides_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 0.0 };
        let rect = rect_default();
        // base sensitivity 0.5 だが depth は 2.0 を使う。
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);
        let screen = PhysicalSize { width: 200, height: 100 };

        let run = |host: &mut UiHost<ModModel>, model: &ModModel, pointer: PointerFrame| -> Vec<Edit<ModModel>> {
            let cur = model.depth;
            host.frame_to_edits(model, &mut Scene::new(), screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let m = Modulation {
                    entries: &[],
                    live_value: None,
                    edit: Some(ModEdit {
                        source_color: Color::WHITE,
                        current_depth: cur,
                        depth_range: Some((-100.0, 100.0)),
                        depth_sensitivity: Some(2.0),
                        on_mod_change: &on_mod,
                    }),
                };
                ui.scrubable_number_at(
                    "mtest", rect, model.bpm, 120.0, ScrubableNumberFormat::Decimal(1),
                    &style, |v| Edit::mutate(move |m: &mut ModModel| m.bpm = v), None, Some(m),
                );
            })
        };

        for e in run(&mut host, &model, press_at(center, false)) { e.apply(&mut model); }
        // drag up 10px × depth_sensitivity 2.0 = 20 (style.sensitivity 0.5 なら 5)。
        for e in run(&mut host, &model, hold_at((center.0, center.1 - 10.0), false)) { e.apply(&mut model); }
        assert!((model.depth - 20.0).abs() < 1e-5, "depth_sensitivity 2.0 で +20 (got {})", model.depth);
    }

    /// `Some` でも entries 空 + live None + edit None なら overlay 描画差分なし (base marker も出ない)。
    #[test]
    fn empty_modulation_draws_no_overlay() {
        let model = ModModel { bpm: 120.0, depth: 0.0 };
        let style = mod_style();
        let screen = PhysicalSize { width: 200, height: 100 };

        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(&model, &mut scene_none, screen, FrameInput::default(), |_, ui| {
            ui.scrubable_number_at(
                "mtest", rect_default(), 120.0, 120.0, ScrubableNumberFormat::Decimal(1),
                &style, |v| Edit::mutate(move |m: &mut ModModel| m.bpm = v), None, None,
            );
        });

        let mut host_e: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_empty = Scene::new();
        run_mod_frame(&mut host_e, &model, rect_default(), &style, PointerFrame::default(), false, &[], None, &mut scene_empty);

        assert_eq!(
            scene_empty.rect_count(),
            scene_none.rect_count(),
            "empty Some は None と同じ rect 数 (= base marker も出ない、 contract)",
        );
    }

    /// 非有限 (NaN) な live_value / depth を渡しても scene の rect 座標に NaN を出さない。
    #[test]
    fn nonfinite_values_produce_no_nan_rects() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { bpm: 120.0, depth: 0.0 };
        let style = mod_style();
        let entries = [ModEntry { color: Color::WHITE, depth: f64::NAN }];
        let mut scene = Scene::new();
        run_mod_frame(
            &mut host, &model, rect_default(), &style,
            PointerFrame::default(), false, &entries, Some(f64::INFINITY), &mut scene,
        );
        for r in scene.iter_rects() {
            assert!(
                r.rect.x.is_finite() && r.rect.y.is_finite() && r.rect.w.is_finite() && r.rect.h.is_finite(),
                "rect 座標に NaN/Inf が出ない (got {:?})",
                r.rect,
            );
        }
    }

    // ---- daw_01 #108: 横ドラッグ ----

    /// 横ドラッグ右で base 値が増加 (縦と同 sensitivity)。
    #[test]
    fn horizontal_drag_right_increases_value() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // drag right 20px (dx=+20, dy=0) → 120 + 20*0.5 = 130
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 + 20.0, center.1), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 130.0).abs() < 1e-5, "横右 20px × 0.5 = +10 (got {})", model.bpm);
    }

    /// 横ドラッグ左で減少 (符号確認)。
    #[test]
    fn horizontal_drag_left_decreases_value() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // drag left 20px (dx=-20) → 120 - 10 = 110
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 - 20.0, center.1), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 110.0).abs() < 1e-5, "横左 20px × 0.5 = -10 (got {})", model.bpm);
    }

    /// 斜めドラッグは両軸を加算 (右 + 上で増加)。
    #[test]
    fn diagonal_drag_sums_both_axes() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // dx=+10, dy=-10 (up) → delta = 10 + 10 = 20 → 120 + 20*0.5 = 130
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 + 10.0, center.1 - 10.0), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 130.0).abs() < 1e-5, "斜め右上 (10,10) 加算 = +10 (got {})", model.bpm);
    }

    /// 純縦ドラッグは横追加後も従来と完全一致 (回帰なし、 dx=0)。
    #[test]
    fn vertical_only_drag_unchanged_after_horizontal_support() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // drag up 20px, x 不変 → 従来どおり +10
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 - 20.0), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 130.0).abs() < 1e-5, "純縦 20px = +10 で回帰なし (got {})", model.bpm);
    }

    /// depth ドラッグ (arm) も横で効く + 合成距離で mod_dragging 閾値に入る。
    #[test]
    fn depth_drag_works_horizontally() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 0.0 };
        let rect = rect_default();
        let style = mod_style();
        let center = (40.0_f32, 14.0_f32);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag right 20px (dx=+20) → depth 0 + 20*0.5 = 10
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, &style,
            hold_at((center.0 + 20.0, center.1), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 10.0).abs() < 1e-5, "横右 depth scrub +10 (got {})", model.depth);
        assert!(resp.mod_dragging, "横 20px の合成距離で mod_dragging=true");
    }

    /// 横ドラッグ下で base 値が減少 (下=−)。
    #[test]
    fn vertical_drag_down_decreases_value() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // drag down 20px (dy=+20) → delta = 0 - 20 = -20 → 120 - 10 = 110
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0, center.1 + 20.0), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 110.0).abs() < 1e-5, "縦下 20px = -10 (got {})", model.bpm);
    }

    /// 左下ドラッグは両軸とも減少方向で加算 (左=− / 下=−)。
    #[test]
    fn diagonal_down_left_decreases_value() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // dx=-10, dy=+10 → delta = -10 - 10 = -20 → 120 - 10 = 110
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 - 10.0, center.1 + 10.0), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 110.0).abs() < 1e-5, "左下 (-10,-10) 加算 = -10 (got {})", model.bpm);
    }

    /// 右下の正確な対角線は両軸が打ち消し合い値が変わらない (per-axis 合成の一貫仕様)。
    #[test]
    fn diagonal_down_right_cancels_to_no_change() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, false), None,
        );
        for e in edits { e.apply(&mut model); }
        // dx=+15, dy=+15 → delta = 15 - 15 = 0 → 値不変 (右の + と下の − が相殺)
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 + 15.0, center.1 + 15.0), false), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 120.0).abs() < 1e-5, "右下対角は相殺で値不変 (got {})", model.bpm);
    }

    /// Ctrl fine sensitivity は横ドラッグにも効く (両軸共通)。
    #[test]
    fn ctrl_fine_applies_to_horizontal() {
        let mut host: UiHost<BpmModel> = UiHost::no_redraw();
        let mut model = BpmModel { bpm: 120.0 };
        let rect = rect_default();
        let style = ScrubableNumberStyle { sensitivity: 0.5, ..ScrubableNumberStyle::from_palette(&Palette::dark()) };
        let center = (40.0_f32, 14.0_f32);

        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style, press_at(center, true), None,
        );
        for e in edits { e.apply(&mut model); }
        // drag right 20px Ctrl → 20 * 0.5 * 0.1 = 1.0 → 121
        let edits = run_frame(
            &mut host, &model, rect, model.bpm, 120.0,
            ScrubableNumberFormat::Decimal(1), &style,
            hold_at((center.0 + 20.0, center.1), true), None,
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.bpm - 121.0).abs() < 1e-5, "Ctrl 横右 = +1.0 (got {})", model.bpm);
    }

    /// Ctrl fine sensitivity は depth の横ドラッグにも効く (depth_sens × FINE)。
    #[test]
    fn ctrl_fine_applies_to_depth_horizontal() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { bpm: 120.0, depth: 0.0 };
        let rect = rect_default();
        let style = mod_style(); // sensitivity 0.5
        let center = (40.0_f32, 14.0_f32);

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, &style, press_at(center, true), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag right 20px Ctrl → depth 0 + 20 * 0.5 * 0.1 = 1.0
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, &style,
            hold_at((center.0 + 20.0, center.1), true), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 1.0).abs() < 1e-5, "Ctrl 横右 depth = +1.0 (got {})", model.depth);
    }
}
