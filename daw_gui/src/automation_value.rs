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
    /// 表示単位での小数桁数 (drag readout / prefill の文字列整形用)。
    #[must_use]
    fn decimals(&self) -> usize {
        match self.format {
            ScrubableNumberFormat::Integer => 0,
            ScrubableNumberFormat::Decimal(n) => n as usize,
            // automation 値で BarBeat は使わないが、 fallback で 2 桁。
            ScrubableNumberFormat::BarBeat { .. } => 2,
        }
    }

    /// plain 値を **数字のみ** の文字列に (inline 入力欄の初期値 = 単位なし)。
    #[must_use]
    pub fn format_number(&self, plain: f64) -> String {
        let d = (self.to_display)(plain);
        format!("{:.*}", self.decimals(), d)
    }

    /// plain 値を **単位つき** の文字列に (drag 中の現値表示用)。
    #[must_use]
    pub fn format_with_unit(&self, plain: f64) -> String {
        let num = self.format_number(plain);
        if self.unit.is_empty() {
            num
        } else {
            format!("{num} {}", self.unit)
        }
    }

    /// ユーザー入力文字列を plain 値へ。先頭の数値部分を抽出 → 表示レンジで
    /// clamp → `from_display`。数値が読めなければ `None` (= 入力を破棄して
    /// 元値を維持)。
    #[must_use]
    pub fn parse_to_plain(&self, s: &str) -> Option<f64> {
        let display = parse_leading_f64(s)?;
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

/// 文字列先頭の浮動小数を読む (単位 suffix "dB" 等を許容)。
fn parse_leading_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    let mut end = 0;
    for (i, c) in t.char_indices() {
        if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    t.get(..end)?.parse().ok()
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
#[must_use]
pub fn automation_value_display(
    target: &AutomationTarget,
    plugin_range: Option<(f64, f64)>,
) -> AutomationValueDisplay {
    use AutomationTarget as T;
    // 既定 (0..=1 恒等、小数 3 桁、単位なし) — image/text/group の位置・色など。
    let unit01 = AutomationValueDisplay {
        unit: "",
        format: ScrubableNumberFormat::Decimal(3),
        range: (0.0, 1.0),
        to_display: id,
        from_display: id,
    };
    // 回転 (ラジアン↔度、-180..180)。
    let rotation = AutomationValueDisplay {
        unit: "\u{00b0}",
        format: ScrubableNumberFormat::Decimal(1),
        range: (-180.0, 180.0),
        to_display: rad_to_deg,
        from_display: deg_to_rad,
    };
    // dB ゲイン (線形 0..=2 ↔ -60..+6 dB)。
    let gain_db = AutomationValueDisplay {
        unit: "dB",
        format: ScrubableNumberFormat::Decimal(1),
        range: (-60.0, 6.0),
        to_display: lin_to_db,
        from_display: db_to_lin,
    };
    match target {
        T::TrackBuiltin(TrackBuiltinParam::Volume | TrackBuiltinParam::SendGain { .. }) => gain_db,
        T::TrackBuiltin(TrackBuiltinParam::Pan) => AutomationValueDisplay {
            unit: "",
            format: ScrubableNumberFormat::Decimal(2),
            range: (-1.0, 1.0),
            to_display: id,
            from_display: id,
        },
        T::TrackBuiltin(TrackBuiltinParam::Mute) => AutomationValueDisplay {
            unit: "",
            format: ScrubableNumberFormat::Integer,
            range: (0.0, 1.0),
            to_display: id,
            from_display: id,
        },
        // PluginParam は plain = native。実 min/max があればそれを表示レンジに。
        T::PluginParam { .. } => AutomationValueDisplay {
            unit: "",
            format: ScrubableNumberFormat::Decimal(3),
            range: plugin_range.unwrap_or((0.0, 1.0)),
            to_display: id,
            from_display: id,
        },
        T::SongTempo => AutomationValueDisplay {
            unit: "BPM",
            format: ScrubableNumberFormat::Decimal(1),
            range: (1.0, 400.0),
            to_display: id,
            from_display: id,
        },
        T::SongTimeSigNumerator => AutomationValueDisplay {
            unit: "",
            format: ScrubableNumberFormat::Integer,
            range: (1.0, 32.0),
            to_display: id,
            from_display: id,
        },
        T::ImageBuiltin(ImageBuiltinParam::Rotation)
        | T::TextBuiltin(TextBuiltinParam::Rotation)
        | T::GroupTransform(GroupTransformParam::Rotation) => rotation,
        // Group Scale は線形 0.1..10 表示 (log space は norm 変換側で吸収)。
        T::GroupTransform(GroupTransformParam::ScaleX | GroupTransformParam::ScaleY) => {
            AutomationValueDisplay {
                unit: "\u{00d7}",
                format: ScrubableNumberFormat::Decimal(3),
                range: (0.1, 10.0),
                to_display: id,
                from_display: id,
            }
        }
        // Text の px 系: FontSize / OutlineWidth / Shadow offset・blur。
        T::TextBuiltin(TextBuiltinParam::FontSize) => AutomationValueDisplay {
            unit: "px",
            format: ScrubableNumberFormat::Decimal(1),
            range: (1.0, 4096.0),
            to_display: id,
            from_display: id,
        },
        T::TextBuiltin(TextBuiltinParam::OutlineWidth | TextBuiltinParam::ShadowBlur) => {
            AutomationValueDisplay {
                unit: "px",
                format: ScrubableNumberFormat::Decimal(1),
                range: (0.0, 100.0),
                to_display: id,
                from_display: id,
            }
        }
        T::TextBuiltin(TextBuiltinParam::ShadowOffsetX | TextBuiltinParam::ShadowOffsetY) => {
            AutomationValueDisplay {
                unit: "px",
                format: ScrubableNumberFormat::Decimal(1),
                range: (-200.0, 200.0),
                to_display: id,
                from_display: id,
            }
        }
        // 残り (image/text/group の位置・サイズ・不透明度・色 channel) は 0..=1 恒等。
        T::ImageBuiltin(_) | T::TextBuiltin(_) | T::GroupTransform(_) => unit01,
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
        // +24 dB は表示レンジ (−60..+6) で +6 にクランプ → 線形 \u{2248} 1.995。
        let plain = d.parse_to_plain("24").unwrap();
        assert!((plain - (d.from_display)(6.0)).abs() < 1e-9);
        // 単位 suffix つきでも数値先頭を読む。
        assert!(d.parse_to_plain("-6.0 dB").is_some());
        // 数値でなければ None。
        assert!(d.parse_to_plain("abc").is_none());
    }

    #[test]
    fn pan_is_identity_two_decimals() {
        let d = automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Pan), None);
        assert_eq!(d.range, (-1.0, 1.0));
        assert_eq!(d.format_number(-0.5), "-0.50");
        assert_eq!((d.from_display)(0.5), 0.5);
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

    #[test]
    fn format_with_unit_appends_unit() {
        assert_eq!(vol().format_with_unit(1.0), "0.0 dB");
        let pan = automation_value_display(&T::TrackBuiltin(TrackBuiltinParam::Pan), None);
        assert_eq!(pan.format_with_unit(0.25), "0.25");
    }
}
