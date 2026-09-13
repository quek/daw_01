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
use common::model::{Device, NativeKind, RackPanelKey};
use common::protocol::AudioCommand;

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

    /// engine にスペクトラムを計算させたい device: Rack に行がある (= 折り畳まれた Parallel / chain の中では
    /// ない) EQ / Tone EQ のうち Par が開いているもの。上から [`MAX_DEVICE_SCOPES`] 個まで。
    ///
    /// runner のフレーム末に毎フレーム呼ばれるので、行 (`chain_rows` = plugin 名の解決や帯の確保を伴う) は
    /// 組まずに **カーソルトラックの木だけ** を行と同じ順・同じ規則 (`push_chain_rows`: 折り畳んだ Parallel は
    /// 開始行だけ、折り畳んだ chain は chain 行だけ) でたどる。何も開いていなければ確保もしない。
    pub fn wanted_device_scopes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        if let Some(devices) = self.cursor_track_id().and_then(|t| self.cur.song_doc.song().fx_chain_by_track_id(t)) {
            collect_open_eq_scopes(self, devices, &mut out);
        }
        out
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

/// [`AppData::wanted_device_scopes`] の木のたどり: 行と同じ順で、折り畳んだ Parallel / chain の中には入らない。
fn collect_open_eq_scopes(app: &AppData, devices: &[Device], out: &mut Vec<u64>) {
    for d in devices {
        if out.len() == MAX_DEVICE_SCOPES {
            return;
        }
        match d {
            Device::Native(n)
                if matches!(n.kind(), NativeKind::Eq | NativeKind::ToneEq) && app.rack_panel_open(RackPanelKey::Device(n.id)) =>
            {
                out.push(n.id);
            }
            Device::Parallel(p) if app.parallel_node_open(p.id) => {
                for c in p.chains.iter().filter(|c| app.parallel_node_open(c.id)) {
                    collect_open_eq_scopes(app, &c.devices, out);
                }
            }
            Device::Native(_) | Device::Parallel(_) | Device::Plugin(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use common::model::{ChainRef, MASTER_TRACK_ID, NativeKind, RackPanelKey};
    use common::device_scope_bridge::MAX_DEVICE_SCOPES;

    /// スペクトラムを要求するのは「Rack に行がある EQ / Tone EQ で Par が開いているもの」を上から
    /// 16 個まで: Comp 系の Par は数えない / 折り畳んだ Parallel / chain の中は外れる / 閉じたら実測高も捨てる。
    /// 木を直接たどる結果は、Rack の行 (`chain_rows`) から数えた結果と常に一致する。
    #[test]
    fn wanted_scopes_follow_open_eq_panels_on_visible_rows() {
        // Rack の行から数える (描画と同じ行の規則。`wanted_device_scopes` はこれを組まずに同じ答えを出す)。
        let from_rows = |app: &crate::state::AppData| -> Vec<u64> {
            app.chain_rows()
                .iter()
                .filter_map(|r| match &r.kind {
                    crate::app_types::ChainRowKind::Native(n)
                        if matches!(n.kind, NativeKind::Eq | NativeKind::ToneEq)
                            && app.rack_panel_open(RackPanelKey::Device(n.device_id)) =>
                    {
                        Some(n.device_id)
                    }
                    _ => None,
                })
                .take(MAX_DEVICE_SCOPES)
                .collect()
        };
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
        let (parallel, chain) = app
            .cur
            .song_doc
            .song()
            .find_device(eq)
            .and_then(|(chain, _)| match chain {
                ChainRef::Chain(c) => app.cur.song_doc.song().chain_by_id(c).map(|(p, _)| (p.id, c)),
                ChainRef::Track(_) => None,
            })
            .expect("EQ は Parallel の中");
        assert_eq!(app.wanted_device_scopes(), vec![eq], "展開中は数える");
        assert_eq!(app.wanted_device_scopes(), from_rows(&app));
        app.toggle_parallel_node_collapsed(parallel);
        assert!(app.wanted_device_scopes().is_empty());
        assert_eq!(app.wanted_device_scopes(), from_rows(&app));
        // Parallel を開いても chain を折り畳めば行が無い。
        app.toggle_parallel_node_collapsed(parallel);
        app.toggle_parallel_node_collapsed(chain);
        assert!(app.wanted_device_scopes().is_empty(), "折り畳んだ chain の中も外れる");
        assert_eq!(app.wanted_device_scopes(), from_rows(&app));
        app.toggle_parallel_node_collapsed(chain);

        // 上限。
        for _ in 0..MAX_DEVICE_SCOPES + 4 {
            app.add_native(master, NativeKind::ToneEq, true);
        }
        assert_eq!(app.wanted_device_scopes().len(), MAX_DEVICE_SCOPES);
        assert_eq!(app.wanted_device_scopes(), from_rows(&app), "上限で切る位置も行の順");
    }
}
