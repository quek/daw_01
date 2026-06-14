//! `channel_fader_meter` widget — fader + ステレオ level meter を **単一の dB→ピクセル y 写像**
//! で統合した mixer 用複合 widget (M14 Phase 111 / daw_01 #083)。
//!
//! ## なぜ統合するか
//!
//! `fader_at` と `level_meter_stereo` を別々に並べると、 同じ `MeterScale` カーブを渡しても
//! **fraction→ピクセル y の内部 inset が widget ごとに違う** ため、 fader ハンドルと meter 目盛りが
//! 縦にズレる (fader = `[rect.y+8, rect.y+h-8]` / meter = `[rect.y+22, rect.y+h-6]`)。 カーブを
//! 共有しても「画素写像が 1 箇所所有」 でない限りズレは再発する。
//!
//! 本 widget は **group rect から導出した 1 つの `region` (= [`meter_content_region`])** を
//! fader 列の track 領域と meter 列の縦 content の **両方** に渡すので、 どの dB でもハンドル中心と
//! メーターのバー上端 / tick / 0dB 線が画素単位で一致する (SSoT)。 `fader_at` / `level_meter_stereo`
//! は汎用部品として不変のまま、 内部の `fader_core` / `meter_body` を再利用する。
//!
//! ## レイアウト
//!
//! group rect を横に `[fader_w | METER_GAP | meter (tick|L|R|数字)]` に分割する。 fader 列は thumb +
//! track、 meter 列は `level_meter_stereo` と同一の `[tick | L | R | 数字]`。 peak readout チップと
//! dB 目盛りは meter 列のみ (fader 列にグリッドは引かない)。 縦の dB→y 領域だけが共有される。
//!
//! ## 操作 (DAW 標準、 `fader_at` 再利用)
//!
//! - fader thumb を drag で音量編集 (下端 `−∞` / 上端 `+6dB`、 `style.scale` のカーブで dB↔位置)
//! - thumb をダブルクリックで `default_db` (= 0dB unity) にリセット
//! - Ctrl + drag で感度 1/10
//! - meter 列の click は peak readout の reset (widget 内部で消費)
//!
//! x 位置で hit-test を分岐するので fader drag と meter reset は空間的に重ならない。

use std::hash::Hash;

use daw_ui_renderer::Rect;

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::fader::FaderResponse;
use crate::widgets::level_meter::{LevelMeterStyle, MeterBallistic, meter_content_region};
use crate::widgets::scrubable_number::Modulation;

/// fader 列と meter 列の間の隙間 (px)。 daw_01 の従来 `METER_GAP` と一致 (group_w 55 =
/// fader 18 + gap 2 + meter 35 を内部分割で踏襲)。
const METER_GAP: f32 = 2.0;

/// [`Ui::channel_fader_meter`] の戻り値。
pub struct ChannelFaderMeterResponse {
    /// fader 部分のレスポンス。 `displayed_value` は dB (無音 = `f32::NEG_INFINITY`)。
    /// gesture edge 検出には `dragging` を使う。
    pub fader: FaderResponse,
    /// modulation depth の drag 編集中 (= `fader.mod_dragging` の便宜 re-export、 daw_01 #110)。
    /// edge 検出で caller が depth の undo bracket を発火する。 base `fader.dragging` とは排他。
    pub mod_dragging: bool,
    // meter の peak-reset click は widget 内部で消費済み。
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// fader + ステレオ level meter を単一の dB→y 写像で描く複合 widget (daw_01 #083)。
    ///
    /// `rect` が group 全体、 `fader_w` が左の fader 列幅 (残りが meter 列)。 `volume_db` /
    /// `default_db` は dB (無音 = `f32::NEG_INFINITY`)、 `l` / `r` は L/R peak linear (毎フレーム)。
    /// `style.scale` の [`MeterScale`](crate::MeterScale) を fader ハンドルと meter バーの **両方** に
    /// 適用するのでカーブ一致がコードで保証される (`scale: Some(_)` 前提)。 `style.peak_readout = true`
    /// で上端に peak 帯を確保し、 それを除いた領域が共有 region になる。
    ///
    /// `on_change(new_db)` が値変化時に `Edit<M>` を発行する (undo/redo は dB 空間、 `fader_at` と同じ
    /// inverse 機構)。 frac0 (下端) は `f32::NEG_INFINITY` で渡る。
    ///
    /// `modulation`: `Some` で Bitwig 流 modulation を表示・編集する (daw_01 #110、 #109 knob / #107
    ///   scrubable の fader 版)。 `None` で従来描画・従来挙動 (完全回帰)。 値ドメインは dB でなく
    ///   **フェーダーの正規化トラック位置 0..=1** (= つまみの 0=最下端〜1=最上端): 絶対
    ///   [`Modulation::live_value`](crate::Modulation) は 0..=1、 符号付き [`ModEntry::depth`](crate::ModEntry)
    ///   / `ModEdit::current_depth` は base 位置からの増減量 (dB/log 写像でなく位置の frac、 polarity と
    ///   volume↔位置 解決は caller)。 縦トラックに沿った色帯 ([`Modulation::entries`](crate::Modulation)) +
    ///   可動の水平 live マーク + arm 中の source 色枠/帯を fader 列に cache 外 overlay 描画 (dB 目盛り /
    ///   メーター / peak と共存、 meter 列へははみ出さない)。 arm 中 (`edit` Some) は thumb の press + 縦
    ///   drag が base(音量) でなく depth を変化させ `on_mod_change` を発火する (base 移動抑止 = 非破壊)。
    #[allow(clippy::too_many_arguments)]
    pub fn channel_fader_meter<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        fader_w: f32,
        volume_db: f32,
        default_db: f32,
        l: f32,
        r: f32,
        ballistic: MeterBallistic,
        style: LevelMeterStyle,
        label: &'static str,
        on_change: F,
        modulation: Option<Modulation<'_, M>>,
    ) -> ChannelFaderMeterResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        // 共有 dB→y 領域: group rect から 1 度だけ導出する。 fader と meter は同じ rect.y / rect.h を
        // 見るので、 この region.y / region.h を両方に渡せば画素整合する (SSoT、 #083 の本質)。
        let region = meter_content_region(rect, style.scale.is_some(), style.peak_readout);

        // 横分割: [fader_w | METER_GAP | meter]。fader_w / meter_x を rect 内に clamp。
        let fader_w = fader_w.clamp(0.0, rect.w);
        let fader_col = Rect { x: rect.x, y: rect.y, w: fader_w, h: rect.h };
        let meter_x = (rect.x + fader_w + METER_GAP).min(rect.x + rect.w);
        let meter_w = (rect.x + rect.w - meter_x).max(0.0);
        let meter_col = Rect { x: meter_x, y: rect.y, w: meter_w, h: rect.h };

        // fader (左列): 共有 region を track 領域 (thumb 中心の可動域) として fader_core に渡す。
        // fader_core は self.pointer を raw で読み thumb 内 press のみ反応する (press を消費しない)。
        let fader_wid = WidgetId::ROOT.child((b"cfm_fader", &id));
        let fader = self.fader_core(
            fader_wid,
            fader_col,
            region.y,
            region.h,
            volume_db,
            default_db,
            style.scale,
            label,
            on_change,
            modulation,
        );

        // hit-test を真に「列で分離」 にする: thumb は THUMB_W=28 で fader_w (=18 等) より広く、
        // 右へ ~3px はみ出して meter 列に食い込む。 fader が base drag を掴んだら その press を消費し、
        // 続く meter_body の peak-reset が同 press を二重処理しないようにする (= 重なり領域では fader が
        // 優先。 fader を掴まない純粋な meter 列 press は従来どおり reset)。 depth gesture (#110) は
        // fader_core が内部で既に consume 済なので、 ここは base drag だけを見れば足りる。
        if fader.dragging && self.pointer.primary_just_pressed {
            self.consume_pointer_click();
        }

        // meter (右列): 同じ region.y / region.h を縦 content として meter_body に渡す。
        // meter_body は meter_col 内の primary press を peak reset として消費する (上の consume +
        // depth の fader_core consume で fader が掴んだ press は既に除かれているので衝突しない)。
        let meter_wid = WidgetId::ROOT.child((b"cfm_meter", &id));
        let content = Rect { x: meter_col.x, y: region.y, w: meter_col.w, h: region.h };
        self.meter_body(meter_wid, meter_col, content, l, r, ballistic, &style);

        ChannelFaderMeterResponse { mod_dragging: fader.mod_dragging, fader }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    //! channel_fader_meter の検証:
    //! - **整合 (本 widget の存在理由)**: volume=0dB で fader thumb 中心 y == meter 0dB 線 y
    //! - hit-test 分岐: fader thumb press → drag、 meter 列 press → fader を動かさない
    //! - 値変化が dB で on_change に届く (fader_core の dB↔fraction 配線)

    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;
    use crate::widgets::level_meter::{MeterScale, meter_content_region};
    use crate::{FrameInput, LevelMeterStyle};

    struct Vol {
        db: f32,
    }

    fn group_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 55.0, h: 280.0 }
    }

    const FADER_W: f32 = 18.0;

    fn style() -> LevelMeterStyle {
        LevelMeterStyle {
            scale: Some(MeterScale::default()),
            peak_readout: true,
            ..LevelMeterStyle::default()
        }
    }

    /// 1 フレーム描画して (scene, edits) を返す。 `l`/`r` は meter レベル。
    fn run(
        host: &mut UiHost<Vol>,
        model: &Vol,
        pointer: PointerFrame,
        l: f32,
        r: f32,
    ) -> (Scene, Vec<Edit<Vol>>) {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 320 };
        let edits = host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |m: &Vol, ui| {
                ui.channel_fader_meter(
                    "cfm",
                    group_rect(),
                    FADER_W,
                    m.db,
                    0.0,
                    l,
                    r,
                    MeterBallistic::Peak,
                    style(),
                    "Volume",
                    |new_db| Edit::mutate(move |m: &mut Vol| m.db = new_db),
                    None,
                );
            },
        );
        (scene, edits)
    }

    fn press_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            modifiers: Modifiers::default(),
            ..PointerFrame::default()
        }
    }

    fn hold_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame { pos: Some(pos), primary_pressed: true, ..PointerFrame::default() }
    }

    /// **本 widget の存在理由**: 0dB で fader thumb 中心と meter 0dB 線が画素整合する。
    /// `fader_at` + `level_meter_stereo` を別々に並べた旧構成では ~13px ズレていた症状の回帰防止。
    #[test]
    fn fader_thumb_aligns_with_meter_zero_line_at_0db() {
        let mut host: UiHost<Vol> = UiHost::no_redraw();
        let model = Vol { db: 0.0 };
        // l=r=0 で meter バー / peak hold 線を出さない (h でバーと thumb/0dB線を判別するため)。
        let (scene, _) = run(&mut host, &model, PointerFrame::default(), 0.0, 0.0);

        let rect = group_rect();
        let scale = MeterScale::default();
        let region = meter_content_region(rect, true, true);
        // 共有写像 y(frac) = region.y + region.h*(1-frac)、 frac = db_to_frac(0)。
        let expected_y = region.y + region.h * (1.0 - scale.db_to_frac(0.0));

        // thumb: 高さ 10px の rect は thumb だけ (l=r=0 なのでメーターバーは無い)。
        let thumb = scene
            .iter_rects()
            .map(|r| r.rect)
            .find(|r| (r.h - 10.0).abs() < 0.5)
            .expect("thumb rect (h≈10) が存在する");
        let thumb_cy = thumb.y + thumb.h * 0.5;

        // 0dB 線: 高さ 3px の rect は emphasize_zero の 0dB 横線だけ (tick は h=2)。
        let zero_line = scene
            .iter_rects()
            .map(|r| r.rect)
            .find(|r| (r.h - 3.0).abs() < 0.4)
            .expect("0dB 横線 (h=3) が存在する");
        let zero_cy = zero_line.y + zero_line.h * 0.5;

        assert!(
            (thumb_cy - expected_y).abs() < 0.6,
            "thumb 中心 {thumb_cy} が共有写像 y {expected_y} に一致"
        );
        assert!(
            (zero_cy - expected_y).abs() < 1.0,
            "0dB 線 {zero_cy} が共有写像 y {expected_y} に一致 (round 誤差込み)"
        );
        // 本丸: thumb と 0dB 線が画素整合 (旧構成の ~13px ズレが無い)。
        assert!(
            (thumb_cy - zero_cy).abs() < 1.0,
            "fader thumb 中心 {thumb_cy} と meter 0dB 線 {zero_cy} が画素整合する"
        );
    }

    /// fader thumb を press → 上方向に drag で音量が上がり on_change に dB が届く。
    #[test]
    fn fader_thumb_drag_raises_volume_db() {
        let mut host: UiHost<Vol> = UiHost::no_redraw();
        let mut model = Vol { db: 0.0 };
        let rect = group_rect();
        let region = meter_content_region(rect, true, true);
        let scale = MeterScale::default();
        // 0dB の thumb 中心 (fader 列中央 x = col.x + col.w/2)。
        let thumb_x = rect.x + FADER_W * 0.5;
        let thumb_y = region.y + region.h * (1.0 - scale.db_to_frac(0.0));

        // press → 大きく上へ移動 (= 音量増)。
        let (_, edits) = run(&mut host, &model, press_at((thumb_x, thumb_y)), 0.0, 0.0);
        for e in edits {
            e.apply(&mut model);
        }
        let (_, edits) = run(&mut host, &model, hold_at((thumb_x, region.y)), 0.0, 0.0);
        let had_edit = !edits.is_empty();
        for e in edits {
            e.apply(&mut model);
        }

        assert!(had_edit, "drag フレームで on_change Edit が発行される");
        assert!(model.db > 0.0, "上方向 drag で音量 dB が 0 より上がる (got {})", model.db);
    }

    /// thumb 右はみ出し (THUMB_W=28 &gt; fader_w=18) が meter 列に食い込む重なり領域 (x≈21) の press は、
    /// fader drag を掴み + meter peak reset は起きない (consume で二重処理を防止、 #083 「列で分離」)。
    #[test]
    fn overlap_region_press_grabs_fader_and_suppresses_meter_reset() {
        let mut host: UiHost<Vol> = UiHost::no_redraw();
        let mut model = Vol { db: 0.0 };
        let rect = group_rect();
        let region = meter_content_region(rect, true, true);
        let scale = MeterScale::default();
        let thumb_y = region.y + region.h * (1.0 - scale.db_to_frac(0.0));
        // 重なり領域 x: meter_x = 0 + 18 + 2 = 20、 thumb 右端 ≈ 23 → x=21 は両方に属する。
        let overlap_x = rect.x + FADER_W + METER_GAP + 1.0;
        let thumb_right = rect.x + (FADER_W + 28.0) * 0.5; // THUMB_W = 28
        assert!(
            overlap_x >= rect.x + FADER_W + METER_GAP && overlap_x < thumb_right,
            "x が meter 列内かつ thumb 内 (重なり領域)"
        );

        // frame 1: 高レベルで long_peak を立てる (reset 検出用の基準)。
        let (_, _) = run(&mut host, &model, PointerFrame::default(), 0.9, 0.9);
        // frame 2: 重なり領域で press → fader が掴み consume、 meter は reset されない。
        let (scene, edits) = run(&mut host, &model, press_at((overlap_x, thumb_y)), 0.0, 0.0);
        for e in edits {
            e.apply(&mut model);
        }
        // meter reset が起きていれば long_peak=0 → readout "-inf"。 起きていなければ finite (~-0.9)。
        assert!(
            !scene.iter_glyphs().any(|g| g.text.as_ref() == "-inf"),
            "重なり press で meter peak が reset されない (long_peak 維持)"
        );
        // frame 3: drag up → fader が値を上げる (= 掴んでいた)。
        let (_, edits) = run(&mut host, &model, hold_at((overlap_x, region.y)), 0.0, 0.0);
        for e in edits {
            e.apply(&mut model);
        }
        assert!(model.db > 0.0, "重なり press は fader drag を掴む (got {})", model.db);
    }

    /// meter 列の press は fader を掴まない (x で分岐、 fader 値は不変)。
    #[test]
    fn meter_column_press_does_not_drag_fader() {
        let mut host: UiHost<Vol> = UiHost::no_redraw();
        let mut model = Vol { db: 0.0 };
        let rect = group_rect();
        // meter 列中央の x (fader thumb の x 範囲外)。
        let meter_x = rect.x + FADER_W + METER_GAP;
        let meter_w = rect.x + rect.w - meter_x;
        let press_x = meter_x + meter_w * 0.5;

        // meter 列で press → hold で上へ移動。fader が掴まれていれば値が動くはず。
        let (_, edits) = run(&mut host, &model, press_at((press_x, 140.0)), 0.0, 0.0);
        for e in edits {
            e.apply(&mut model);
        }
        let (_, edits) = run(&mut host, &model, hold_at((press_x, 40.0)), 0.0, 0.0);
        for e in edits {
            e.apply(&mut model);
        }

        assert_eq!(model.db, 0.0, "meter 列の press/drag は fader 値を変えない");
    }
}
