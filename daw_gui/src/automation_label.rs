//! `AutomationTarget` の人間可読ラベル。
//!
//! `app_types.rs` が実コード 1,000 行 budget (不変条件 9) を超えているので、
//! r.md #89 で arm を足すのに合わせて切り出した。**song 非依存の pure label** で、
//! ソース名やプラグイン param 名まで入った表示は song を引ける
//! `AppData::automation_target_label` が担う。

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
        // r.md #89: モジュレーター自身のツマミ。**この関数は song 非依存の pure label** なので
        // 種別名 (LFO / Rand / …) を出せない。ソース名まで入った表示は song を引ける
        // `AppData::automation_target_label` が担う (SSoT はそちら)。
        AutomationTarget::ModSourceParam { source_id, param } => {
            format!("変調 {source_id} \u{25b8} {}", param.label())
        }
        AutomationTarget::ModRoutingDepth { routing_id } => {
            format!("変調 #{routing_id} の深さ")
        }
    }
}
