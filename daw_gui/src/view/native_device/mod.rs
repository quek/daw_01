//! 内蔵 device (`Device::Native`) の共有描画部品 (r.md #129、`docs/plan_rack_native_devices.md` §10.4)。
//!
//! Rack (行ミニ / Par) / Mixer 帯 / マスターパネルが同じ関数を呼ぶ。DAW 固有なので daw-ui core ではなく
//! view/ に置く (不変条件 8)。
//!
//! | 部品 | 中身 |
//! |---|---|
//! | [`knob`] | つまみ (+ 数値欄)。値の住所・live 値・変調・ジェスチャーは既存の口を通す |
//! | [`curve`] | EQ / Tone EQ の合成カーブ (+ スペクトラム) と、点操作が共有する座標写像 |
//! | [`gr`] | GR メーター (縦 / 横 / セグメント) と数値表記 |
//!
//! widget id は必ず [`wid`] で作る — 描画面 ([`ParamSurface`]) と device ([`RackPanelKey`]) を含めるので、
//! 同じ種類の Par を 2 枚開いても、Mixer 帯と Rack に同じ param を描いても入力状態が混ざらない (§18-AA)。

pub mod curve;
pub mod gr;
pub mod knob;

use std::hash::Hash;

use common::model::{AutomationLane, ModRouting, RackPanelKey, Song, Track};

use crate::app::ParamSurface;

pub use curve::{CurveAxes, CurveBand, CurveHandle, CurveLook, EqCurveSource, HandleAxes, curve_handles, draw_eq_curve};
pub use gr::{LIMITER_GR_SEGMENTS, draw_gr_horizontal, draw_gr_segments, draw_gr_vertical, gr_text};
pub use knob::{NativeKnobResponse, NativeKnobSpec, limiter_knob, native_knob, native_knob_with_value};

/// OFF (bypass) の device の表示 (EQ カーブの線とスペクトラム / GR メーターの溝と塗り) の不透明度の倍率。
/// 形は変えずに薄くする (Q9)。部品ごとに持つと OFF の見え方が面ごとに割れるので 1 か所に置く。
const INACTIVE_ALPHA: f32 = 0.45;

/// 共有部品の widget id: `(描画面, 持ち主の device, 部品名, 部品内の鍵)`。
///
/// 位置 (行 index / トラック index) を鍵にしない (不変条件 1) — 並べ替えても入力状態が付いて回らない。
#[must_use]
pub fn wid<K: Hash>(
    s: ParamSurface,
    o: RackPanelKey,
    part: &'static str,
    key: K,
) -> (ParamSurface, RackPanelKey, &'static str, K) {
    (s, o, part, key)
}

/// パラメーターの持ち主 (track id か `MASTER_TRACK_ID`)。lane / routing の store を解決済みで
/// 持つ (ツマミごとに track を線形探索しない)。置き場の規則は `Song::param_stores` 1 か所。
#[derive(Clone, Copy)]
pub struct ParamOwner<'a> {
    pub id: u32,
    pub lanes: &'a [AutomationLane],
    pub routings: &'a [ModRouting],
}

impl<'a> ParamOwner<'a> {
    /// `owner_id` の store (`None` = そのトラックが無い)。
    #[must_use]
    pub fn resolve(song: &'a Song, owner_id: u32) -> Option<Self> {
        let (lanes, routings) = song.param_stores(owner_id)?;
        Some(Self { id: owner_id, lanes, routings })
    }

    /// トラック `t` の store。
    #[must_use]
    pub fn of_track(t: &'a Track) -> Self {
        Self { id: t.id, lanes: &t.automation_lanes, routings: &t.mod_routings }
    }

    /// master (`song_lanes` / `song_mod_routings`)。
    #[must_use]
    pub fn master(song: &'a Song) -> Self {
        Self {
            id: common::model::MASTER_TRACK_ID,
            lanes: &song.song_lanes,
            routings: &song.song_mod_routings,
        }
    }
}
