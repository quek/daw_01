//! handler::rack_view — Rack の Par パネルの開閉と、開いている EQ Par のスペクトラム要求
//! (`docs/plan_rack_native_devices.md` §10.5 / §11.2 / Q11 / Q14 / Q18)。
//!
//! 開閉は **見方の都合** (`ProjectView.open_rack_panels`、ViewState に保存、`*` なし)。
//! device ごとに独立して開き、他の Par を閉じない。plugin / 映像 FX / VOICEVOX / 内蔵 / master Limiter
//! が同じ集合を使う。
//!
//! スペクトラムは「いま Rack に描かれている EQ / Tone EQ の Par」だけを engine に計算させる。
//! 送るのは差分があったフレームだけ ([`AppData::sync_device_scopes`])。

use common::device_scope_bridge::MAX_DEVICE_SCOPES;
use common::model::{NativeKind, RackPanelKey};
use common::protocol::AudioCommand;

use crate::app_types::ChainRowKind;
use crate::state::AppData;

impl AppData {
    /// `DeviceEvent::ToggleRackPanel`。閉じたら実測高も捨てる (次に開いたとき測り直す)。
    pub(crate) fn toggle_rack_panel(&mut self, key: RackPanelKey) {
        if self.cur.view.open_rack_panels.remove(&key) {
            self.cur.peph.rack_panel_heights.remove(&key);
        } else {
            self.cur.view.open_rack_panels.insert(key);
        }
    }

    /// `key` の Par が開いているか。
    pub fn rack_panel_open(&self, key: RackPanelKey) -> bool {
        self.cur.view.open_rack_panels.contains(&key)
    }

    /// `ids` (node id) の Par を閉じる。削除 / 移動 (同じトラック内の並べ替えを含む、Q18) の後始末。
    pub(crate) fn close_rack_panels_of(&mut self, ids: &[u64]) {
        for &id in ids {
            let key = RackPanelKey::Device(id);
            self.cur.view.open_rack_panels.remove(&key);
            self.cur.peph.rack_panel_heights.remove(&key);
        }
    }

    /// engine にスペクトラムを計算させたい device: Rack に行がある (= 折り畳まれた Parallel の中ではない)
    /// EQ / Tone EQ のうち Par が開いているもの。上から [`MAX_DEVICE_SCOPES`] 個まで。
    pub fn wanted_device_scopes(&self) -> Vec<u64> {
        self.chain_rows()
            .iter()
            .filter_map(|r| match &r.kind {
                ChainRowKind::Native(n)
                    if matches!(n.kind, NativeKind::Eq | NativeKind::ToneEq)
                        && self.rack_panel_open(RackPanelKey::Device(n.device_id)) =>
                {
                    Some(n.device_id)
                }
                _ => None,
            })
            .take(MAX_DEVICE_SCOPES)
            .collect()
    }

    /// [`Self::wanted_device_scopes`] が前回送った集合と違えば `SetDeviceScopes` を送って覚える。
    /// runner のフレーム末に 1 回呼ぶ (headless テストは明示的に呼ぶ)。
    pub fn sync_device_scopes(&mut self) {
        let wanted = self.wanted_device_scopes();
        if wanted == self.cur.peph.device_scopes_sent {
            return;
        }
        self.send_audio(AudioCommand::SetDeviceScopes { project: self.pk(), device_ids: wanted.clone() });
        self.cur.peph.device_scopes_sent = wanted;
    }
}

#[cfg(test)]
mod tests {
    use common::model::{ChainRef, MASTER_TRACK_ID, NativeKind, RackPanelKey};
    use common::device_scope_bridge::MAX_DEVICE_SCOPES;

    /// スペクトラムを要求するのは「Rack に行がある EQ / Tone EQ で Par が開いているもの」を上から
    /// 16 個まで: Comp 系の Par は数えない / 折り畳んだ Parallel の中は外れる / 閉じたら実測高も捨てる。
    #[test]
    fn wanted_scopes_follow_open_eq_panels_on_visible_rows() {
        let mut app = crate::test_support::headless_app();
        app.cur.selection.selected_track_ids = vec![MASTER_TRACK_ID];
        let master = ChainRef::Track(MASTER_TRACK_ID);
        let song = app.cur.song_doc.song();
        let bus = song.builtin_native(MASTER_TRACK_ID, NativeKind::BusComp).expect("bus").id;
        let tone = song.builtin_native(MASTER_TRACK_ID, NativeKind::ToneEq).expect("tone").id;
        assert!(app.wanted_device_scopes().is_empty());

        app.add_native(master, NativeKind::Eq, true);
        let eq = *app.cur.selection.selected_device_ids.last().expect("足した EQ が選択される");
        app.toggle_rack_panel(RackPanelKey::Device(bus));
        app.toggle_rack_panel(RackPanelKey::Device(tone));
        assert_eq!(app.wanted_device_scopes(), vec![tone, eq], "行の順 (組み込み → 足した分)、Comp 系は数えない");

        // 閉じたら外れ、実測高も残らない。
        app.cur.peph.rack_panel_heights.insert(RackPanelKey::Device(tone), 99.0);
        app.toggle_rack_panel(RackPanelKey::Device(tone));
        assert_eq!(app.wanted_device_scopes(), vec![eq]);
        assert!(!app.cur.peph.rack_panel_heights.contains_key(&RackPanelKey::Device(tone)));

        // 折り畳んだ Parallel の中の EQ は行が無いので外れる。
        app.group_devices(vec![eq]);
        let parallel = app.cur.song_doc.song().find_device(eq).and_then(|(chain, _)| match chain {
            ChainRef::Chain(c) => app.cur.song_doc.song().chain_by_id(c).map(|(p, _)| p.id),
            ChainRef::Track(_) => None,
        });
        let parallel = parallel.expect("EQ は Parallel の中");
        assert_eq!(app.wanted_device_scopes(), vec![eq], "展開中は数える");
        app.toggle_parallel_node_collapsed(parallel);
        assert!(app.wanted_device_scopes().is_empty());

        // 上限。
        for _ in 0..MAX_DEVICE_SCOPES + 4 {
            app.add_native(master, NativeKind::ToneEq, true);
        }
        assert_eq!(app.wanted_device_scopes().len(), MAX_DEVICE_SCOPES);
    }
}
