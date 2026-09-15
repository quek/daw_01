//! automation 値の **人間可読単位** 表示/入力の SSoT。
//!
//! オートメーション点の値・レーンのデフォルト値・ドラッグ中の現値表示は
//! すべて「インスペクタと同じ人間可読単位」 (Volume = dB, Pan = -1..1,
//! 回転 = 度, FontSize = px, Tempo = BPM, PluginParam = native) で扱う。
//! target ごとの **単位ラベル・フォーマット・表示レンジ・plain↔表示変換**
//! をこの 1 関数 (`automation_value_display`) に集約する。
//!
//! - **plain** = model が持つ native 値 (`AutomationPoint::value` /
//!   `AutomationLane::default_value` の単位)。Volume は線形 `0..=2`、回転は
//!   ラジアン、等。
//! - **display** = ユーザーが読み書きする人間可読値。Volume は dB、回転は度。
//!
//! `common::automation::{plain_to_norm, norm_to_plain}` が plain↔正規化 (0..1)
//! の SSoT であるのと対称に、ここは plain↔display の SSoT。

use common::model::{
    AutomationTarget, GroupTransformParam, ImageBuiltinParam, TextBuiltinParam, TrackBuiltinParam,
};
use daw_ui_core::ScrubableNumberFormat;

/// **pan / balance の数値表記** (`"L50"` / `"C"` / `"R100"`)。 内部値は `-1.0..=1.0`、
/// 表示は左右それぞれ 0..100 の整数 (参照 DAW の慣習: REAPER `100%L..100%R`、Live `50L`)。
///
/// pan の数値が出る経路 (mixer strip の readout / track inspector の Pan 行 / automation
/// lane header の default 値 / automation point drag の readout) は **すべてこれを共有する**。
/// 表記を変えるならここ 1 箇所 (SSoT)。
pub const PAN_FORMAT: ScrubableNumberFormat = ScrubableNumberFormat::SignedLabeled {
    neg: "L",
    pos: "R",
    center: "C",
    scale: 100.0,
};

/// **移調の数値表記** (`"+2"` / `"0"` / `"-5"`、半音、r.md #130)。transport の Transpose 欄と移調レーンの
/// 既定値 / 点のドラッグ readout が共有する (表記を変えるならここ 1 箇所)。入力は `"+2"` / `"-2"` / `"2"`。
pub const TRANSPOSE_FORMAT: ScrubableNumberFormat = ScrubableNumberFormat::SignedLabeled {
    neg: "-",
    pos: "+",
    center: "0",
    scale: 1.0,
};

/// `AutomationTarget` 1 つ分の値の人間可読表示記述子。
#[derive(Clone, Copy, Debug)]
pub struct AutomationValueDisplay {
    /// 単位ラベル ("dB" / "\u{00b0}" / "px" / "BPM" / "\u{00d7}" / "")。
    pub unit: &'static str,
    /// `scrubable_number_at` に渡すフォーマット (Integer / Decimal(n))。
    pub format: ScrubableNumberFormat,
    /// 表示単位での `(min, max)` (clamp 用)。
    pub range: (f64, f64),
    /// plain (model) → display (人間可読)。
    pub to_display: fn(f64) -> f64,
    /// display → plain (`to_display` の逆)。
    pub from_display: fn(f64) -> f64,
}

impl AutomationValueDisplay {
    /// plain 値を **単位なし** の文字列に (inline 入力欄の初期値 / readout の数値部)。
    ///
    /// 文字列化は [`ScrubableNumberFormat::format_value`] に委譲する = `scrubable_number`
    /// widget が入力欄に描く文字列と **同一の写像** (SSoT)。 Pan のように `SignedLabeled`
    /// を使う target では `"L50"` / `"C"` のような側ラベル付き表記になる。
    #[must_use]
    pub fn format_number(&self, plain: f64) -> String {
        self.format.format_value((self.to_display)(plain))
    }

    /// plain 値を **単位つき** の文字列に (drag 中の現値表示用)。
    #[must_use]
    pub fn format_with_unit(&self, plain: f64) -> String {
        let num = self.format_number(plain);
        // ラベルを出す値 ("OFF") には単位を付けない (widget の描画と同じ判定)。
        if self.unit.is_empty() || !self.format.unit_applies((self.to_display)(plain)) {
            num
        } else {
            format!("{num} {}", self.unit)
        }
    }

    /// ユーザー入力文字列を plain 値へ。単位 suffix (`"-6.0 dB"`) を剥がしてから
    /// **書式自身の parser** ([`ScrubableNumberFormat::parse_value`] = 入力欄と同じ解釈、
    /// Pan の `"L50"` 等) に渡し、表示レンジで clamp → `from_display`。数値が読めなければ
    /// `None` (= 入力を破棄して元値を維持)。
    ///
    /// parser を 1 本に保つのが要点。「先頭の数値だけ読む」 fallback を併用すると、`scale` を
    /// 持つ書式 (Pan の `SignedLabeled`) で **土俵が 100 倍ずれる**: `"50%"` は書式 parser が
    /// 拒否 → fallback が `50` を display 値と解釈 → `clamp(-1, 1)` で `1.0` = R100 に化ける。
    #[must_use]
    pub fn parse_to_plain(&self, s: &str) -> Option<f64> {
        let display = self.format.parse_value(&strip_unit_suffix(s, self.unit))?;
        let clamped = display.clamp(self.range.0, self.range.1);
        Some((self.from_display)(clamped))
    }

    /// plain 値を有効レンジへ clamp (数値打ち以外の経路でも plain 単位で安全化)。
    #[must_use]
    pub fn clamp_plain(&self, plain: f64) -> f64 {
        let display = (self.to_display)(plain).clamp(self.range.0, self.range.1);
        (self.from_display)(display)
    }
}

/// 末尾に付いた記述子自身の単位ラベル (`"dB"` / `"°"` / `"BPM"`) を剥がす (ASCII 大文字小文字は
/// 無視)。 `unit` が空、または付いていなければ trim だけして返す。 これで「単位つきで打ち直す」
/// を許容しつつ、 parse 本体は書式 1 本に保てる。
fn strip_unit_suffix<'a>(s: &'a str, unit: &str) -> std::borrow::Cow<'a, str> {
    let t = s.trim();
    if unit.is_empty() || t.len() < unit.len() {
        return std::borrow::Cow::Borrowed(t);
    }
    let cut = t.len() - unit.len();
    // `unit` が非 ASCII ("°") のとき、 byte 差分が char 境界に落ちない入力があり得る
    // (`split_at` は境界外で panic する)。
    if !t.is_char_boundary(cut) {
        return std::borrow::Cow::Borrowed(t);
    }
    let (head, tail) = t.split_at(cut);
    if tail.eq_ignore_ascii_case(unit) {
        std::borrow::Cow::Owned(head.trim_end().to_string())
    } else {
        std::borrow::Cow::Borrowed(t)
    }
}

// ---- 変換関数 (fn pointer 用) ----

fn id(v: f64) -> f64 {
    v
}

/// 線形ゲイン (`0..=2`) → dB。`-60 dB` を floor (= 0 / 負値で `-inf` を避ける)。
fn lin_to_db(v: f64) -> f64 {
    if v <= 1e-4 {
        -60.0
    } else {
        (20.0 * v.log10()).max(-60.0)
    }
}

/// dB → 線形ゲイン (`lin_to_db` の逆)。
fn db_to_lin(v: f64) -> f64 {
    10f64.powf(v / 20.0)
}

fn rad_to_deg(v: f64) -> f64 {
    v.to_degrees()
}

fn deg_to_rad(v: f64) -> f64 {
    v.to_radians()
}

/// `target` の値を人間可読単位で表示/入力するための記述子を返す。
/// `plugin_range` は `PluginParam` の実 min/max (daw_gui の `plugin_params`
/// cache 由来、無ければ `None`)。
///
/// **表示レンジはここで数値を書かない。** 正規化の値域 ([`common::automation::target_range`]) を
/// `to_display` で表示単位へ写したものが表示レンジ — 2 か所に数値を持つと、表示・入力できる値が
/// 変調で潰れる / レーンの上端に張り付く形で食い違う (設計書 §18.2-3、旧 Text の px 系)。
/// ここが決めるのは単位ラベル・書式・plain↔表示の変換だけ。
#[must_use]
pub fn automation_value_display(
    target: &AutomationTarget,
    plugin_range: Option<(f64, f64)>,
) -> AutomationValueDisplay {
    let (unit, format, to_display, from_display) = display_units(target);
    let (lo, hi) = common::automation::target_range(target, plugin_range).display_range();
    AutomationValueDisplay { unit, format, range: (to_display(lo), to_display(hi)), to_display, from_display }
}

/// `target` の (単位ラベル, 書式, plain → 表示, 表示 → plain)。`_` を書かない網羅 match。
#[allow(clippy::type_complexity)]
fn display_units(target: &AutomationTarget) -> (&'static str, ScrubableNumberFormat, fn(f64) -> f64, fn(f64) -> f64) {
    use AutomationTarget as T;
    use ScrubableNumberFormat as F;
    match target {
        // dB ゲイン (線形 0..=2 ↔ -60..+6 dB)。
        T::TrackBuiltin(
            TrackBuiltinParam::Volume
            | TrackBuiltinParam::SendGain { .. }
            | TrackBuiltinParam::ChainGain { .. }
            | TrackBuiltinParam::ParallelOutGain { .. },
        ) => ("dB", F::Decimal(1), lin_to_db, db_to_lin),
        // 単位は表記自身が持つ (`"L50"`) ので unit ラベルは空。
        T::TrackBuiltin(TrackBuiltinParam::Pan | TrackBuiltinParam::ChainPan { .. }) => ("", PAN_FORMAT, id, id),
        T::TrackBuiltin(TrackBuiltinParam::Mute) => ("", F::Integer, id, id),
        // r.md #112: クロスオーバー周波数 (Hz、 対数)。
        T::TrackBuiltin(TrackBuiltinParam::ParallelSplitFreq { .. }) => ("Hz", F::Significant { digits: 3 }, id, id),
        // r.md #114: Selector の位置 (0..=1 を全 chain で等分。 chain 数は lane から見えないので
        // 位置そのまま。 どの chain かはインスペクタの `Active` 欄が示す)。
        T::TrackBuiltin(TrackBuiltinParam::ParallelSelect { .. }) => ("", F::Decimal(2), id, id),
        // r.md #129: 内蔵 device (§7.4)。plain = 表示単位そのもの (Hz / dB / ms / 比) なので変換は恒等。
        T::NativeParam { param, .. } => {
            let (unit, format) = native_unit_format(*param);
            (unit, format, id, id)
        }
        T::MasterLimiter(common::model::MasterLimiterParam::On) => ("", F::Integer, id, id),
        T::MasterLimiter(common::model::MasterLimiterParam::Ceiling) => ("dB", F::Decimal(1), id, id),
        // PluginParam は plain = native。
        T::PluginParam { .. } => ("", F::Decimal(3), id, id),
        // r.md #89: モジュレーターのツマミ。
        T::ModSourceParam { param, .. } => {
            use common::model::ModParam;
            match param {
                ModParam::Rate => ("Hz", F::Decimal(3), id, id),
                ModParam::FollowerAttack | ModParam::FollowerRelease => ("ms", F::Decimal(1), id, id),
                ModParam::FollowerHpHz | ModParam::FollowerLpHz => ("Hz", F::Decimal(0), id, id),
                _ => ("", F::Decimal(2), id, id),
            }
        }
        T::ModRoutingDepth { .. } => ("", F::Decimal(2), id, id),
        T::SongTempo => ("BPM", F::Decimal(1), id, id),
        T::SongTimeSigNumerator => ("", F::Integer, id, id),
        // 半音 (符号が単位の役を持つので単位ラベルは空)。
        T::SongTranspose => ("", TRANSPOSE_FORMAT, id, id),
        // 回転 (ラジアン↔度)。
        T::ImageBuiltin(ImageBuiltinParam::Rotation)
        | T::TextBuiltin(TextBuiltinParam::Rotation)
        | T::GroupTransform(GroupTransformParam::Rotation) => ("\u{00b0}", F::Decimal(1), rad_to_deg, deg_to_rad),
        // Group Scale は線形表示 (log space は norm 変換側で吸収)。
        T::GroupTransform(GroupTransformParam::ScaleX | GroupTransformParam::ScaleY) => {
            ("\u{00d7}", F::Decimal(3), id, id)
        }
        // Text の px 系: FontSize / OutlineWidth / Shadow offset・blur (plain = px)。
        T::TextBuiltin(
            TextBuiltinParam::FontSize
            | TextBuiltinParam::OutlineWidth
            | TextBuiltinParam::ShadowBlur
            | TextBuiltinParam::ShadowOffsetX
            | TextBuiltinParam::ShadowOffsetY,
        ) => ("px", F::Decimal(1), id, id),
        // 残り (image/text/group の位置・サイズ・不透明度・色 channel) は 0..=1 恒等。
        T::ImageBuiltin(_) | T::TextBuiltin(_) | T::GroupTransform(_) => ("", F::Decimal(3), id, id),
    }
}

/// 内蔵 device の住所ごとの単位と書式 (§7.4 の表)。
fn native_unit_format(param: common::model::NativeParamId) -> (&'static str, ScrubableNumberFormat) {
    use common::model::{BusCompParam, CompParam, EqParam, NativeParamId as P};
    match param {
        P::On(_) => ("", ScrubableNumberFormat::Integer),
        P::Comp(CompParam::Threshold | CompParam::Makeup) => ("dB", ScrubableNumberFormat::Decimal(1)),
        P::Comp(CompParam::Ratio) => (":1", ScrubableNumberFormat::Decimal(1)),
        // Hz は下端 20 と上端 20k が 3 桁離れるので有効数字表記
        // (固定小数だと下端が潰れるか上端が欄に入らない)。
        P::Comp(CompParam::Attack | CompParam::Release) => ("ms", ScrubableNumberFormat::Significant { digits: 3 }),
        // 検出フィルタは左端 (plain 0) が OFF (`ParamRange::LogWithOff`)。"0 Hz" と出すと
        // 「0 Hz で効いている」と読めてしまうので、0 だけ "OFF" と書く。
        P::Comp(CompParam::ScFreq) => ("Hz", ScrubableNumberFormat::SignificantZeroLabeled { digits: 3, zero: "OFF" }),
        P::Eq { param: EqParam::Freq, .. } => ("Hz", ScrubableNumberFormat::Significant { digits: 3 }),
        P::Eq { param: EqParam::Gain, .. } => ("dB", ScrubableNumberFormat::Decimal(1)),
        P::Eq { param: EqParam::Q, .. } => ("", ScrubableNumberFormat::Decimal(2)),
        P::BusComp(BusCompParam::Threshold | BusCompParam::Makeup) => ("dB", ScrubableNumberFormat::Decimal(1)),
        // 段階式は段の index が plain。表記は段のラベル (`NativeParamId::step_labels` が SSoT) で、
        // レーン見出し・点の数値・マスターパネルが同じ "4:1" を出す。
        P::BusComp(BusCompParam::Ratio | BusCompParam::Attack | BusCompParam::Release) => (
            "",
            param.step_labels().map_or(ScrubableNumberFormat::Integer, |labels| ScrubableNumberFormat::Choices { labels }),
        ),
        P::ToneEq(_) => ("dB", ScrubableNumberFormat::Decimal(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::AutomationTarget as T;

    fn vol() -> AutomationValueDisplay {
        automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Volume), None)
    }

    #[test]
    fn volume_is_db_with_correct_endpoints() {
        let d = vol();
        assert_eq!(d.unit, "dB");
        // 線形 1.0 = 0 dB、 2.0 \u{2248} +6.02 dB。
        assert!((d.to_display)(1.0).abs() < 1e-6);
        assert!(((d.to_display)(2.0) - 6.0206).abs() < 1e-3);
        // 0 / 負は floor -60 dB。
        assert!(((d.to_display)(0.0) - (-60.0)).abs() < 1e-9);
    }

    #[test]
    fn volume_db_round_trips() {
        let d = vol();
        for db in [-60.0, -24.0, -6.0, 0.0, 3.0, 6.0] {
            let lin = (d.from_display)(db);
            let back = (d.to_display)(lin);
            assert!((back - db).abs() < 1e-6, "db {db} -> {lin} -> {back}");
        }
    }

    #[test]
    fn rotation_is_degrees_round_trip() {
        let d = automation_value_display(&T::GroupTransform(GroupTransformParam::Rotation), None);
        assert_eq!(d.unit, "\u{00b0}");
        // 90 deg = \u{03c0}/2 rad。
        assert!(((d.from_display)(90.0) - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
        assert!(((d.to_display)(std::f64::consts::PI) - 180.0).abs() < 1e-9);
    }

    #[test]
    fn parse_clamps_to_display_range() {
        let d = vol();
        // +24 dB は表示レンジ (−60 dB..正規化の上端 = 線形 `MAX_TRACK_GAIN` の +6.02 dB) でクランプ → 線形の上端。
        let plain = d.parse_to_plain("24").unwrap();
        assert!((plain - f64::from(common::model::MAX_TRACK_GAIN)).abs() < 1e-9);
        // 単位 suffix は剥がしてから書式 parser に渡す (大文字小文字は無視)。
        assert!(d.parse_to_plain("-6.0 dB").is_some());
        assert!(d.parse_to_plain("-6.0 db").is_some());
        // 数値でなければ None。
        assert!(d.parse_to_plain("abc").is_none());
    }

    /// parser は書式 1 本 (先頭数値だけ読む fallback を併用しない)。 併用すると `scale` を持つ
    /// 書式で土俵が 100 倍ずれ、 `"50%"` が R100 に化ける。 読めない入力は値を変えない契約。
    #[test]
    fn parse_does_not_fall_back_to_bare_leading_number() {
        let pan = automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Pan), None);
        assert_eq!(pan.parse_to_plain("50%"), None, "書式が拒否する入力は None (R100 に化けない)");
        assert_eq!(pan.parse_to_plain("R50%"), None);
        // 一方、 素の数字は書式自身が表示土俵 (0..100) で受ける。
        assert!((pan.parse_to_plain("50").unwrap() - 0.5).abs() < 1e-9);
    }

    /// pan は plain 値 (-1..1) 恒等 + **L/C/R 表記** (r.md #47)。表示・入力の両方向を固定する。
    #[test]
    fn pan_is_identity_with_lr_notation() {
        let d = automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Pan), None);
        assert_eq!(d.range, (-1.0, 1.0));
        assert_eq!((d.from_display)(0.5), 0.5);
        assert_eq!(d.format_number(-0.5), "L50");
        assert_eq!(d.format_number(0.0), "C");
        assert_eq!(d.format_number(1.0), "R100");
        // 入力は表示と同じ土俵 (WYSIWYG)。ラベルは前後どちらでも、大文字小文字も無視。
        assert!((d.parse_to_plain("L50").unwrap() + 0.5).abs() < 1e-9);
        assert!((d.parse_to_plain("50l").unwrap() + 0.5).abs() < 1e-9);
        assert!(d.parse_to_plain("C").unwrap().abs() < 1e-9);
        assert!((d.parse_to_plain("r30").unwrap() - 0.3).abs() < 1e-9);
        // 素の数字も表示土俵 (0..100) で解釈し、表示レンジで clamp。
        assert!((d.parse_to_plain("-50").unwrap() + 0.5).abs() < 1e-9);
        assert!((d.parse_to_plain("R500").unwrap() - 1.0).abs() < 1e-9);
        assert!(d.parse_to_plain("abc").is_none());
    }

    #[test]
    fn tempo_is_bpm_identity() {
        let d = automation_value_display(&T::SongTempo, None);
        assert_eq!(d.unit, "BPM");
        assert_eq!(d.range, (1.0, 400.0));
        assert_eq!(d.format_number(120.0), "120.0");
    }

    #[test]
    fn plugin_param_uses_supplied_range() {
        let target = T::PluginParam {
            device_id: 1,
            param_id: 3,
            legacy_device_index: None,
        };
        let d = automation_value_display(&target, Some((20.0, 20_000.0)));
        assert_eq!(d.range, (20.0, 20_000.0));
        // range 無しは 0..1 既定。
        let d2 = automation_value_display(&target, None);
        assert_eq!(d2.range, (0.0, 1.0));
    }

    /// A-6 (§7.4 / §18-W): 段階式は段の index ではなくラベルで出し、ラベルで入力を受ける。
    #[test]
    fn bus_comp_steps_display_and_parse_as_labels() {
        use common::model::{BusCompParam, CompParam, NativeParamId};
        let ratio = automation_value_display(
            &T::NativeParam { device_id: 1, param: NativeParamId::BusComp(BusCompParam::Ratio) },
            None,
        );
        assert_eq!(ratio.format_with_unit(1.0), "4:1");
        assert_eq!(ratio.parse_to_plain("10:1"), Some(2.0));
        assert_eq!(ratio.parse_to_plain("abc"), None);

        let sc = automation_value_display(
            &T::NativeParam { device_id: 1, param: NativeParamId::Comp(CompParam::ScFreq) },
            None,
        );
        assert_eq!(sc.format_with_unit(0.0), "OFF", "検出フィルタの左端は OFF (単位を付けない)");
        assert_eq!(sc.format_with_unit(150.0), "150 Hz");
        assert_eq!(sc.parse_to_plain("off"), Some(0.0));
    }

    #[test]
    fn format_with_unit_appends_unit() {
        assert_eq!(vol().format_with_unit(1.0), "0.0 dB");
        let pan = automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Pan), None);
        // pan は表記自身が側を示すので unit suffix は付かない。
        assert_eq!(pan.format_with_unit(0.25), "R25");
    }
}
