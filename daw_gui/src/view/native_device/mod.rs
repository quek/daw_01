//! 内蔵 device (`Device::Native`) の共有描画部品 (r.md #129、`docs/plan_rack_native_devices.md` §10.4)。
//!
//! Rack / Mixer 帯 / マスターパネルが同じ関数を呼ぶ。DAW 固有なので daw-ui core ではなく
//! view/ に置く (不変条件 8)。

use common::model::{AutomationLane, ModRouting, Song, Track};

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
