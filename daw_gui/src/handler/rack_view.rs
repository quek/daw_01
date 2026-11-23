//! handler::rack_view — Rack の Par パネルの開閉と、開いている EQ Par のスペクトラム要求
//! (`docs/plan_rack_native_devices.md` §10.5 / §11.2 / Q11 / Q14 / Q18)。
//!
//! 開閉は **見方の都合** (`ProjectView.open_rack_panels`、ViewState に保存、`*` なし)。
//! device ごとに独立して開き、他の Par を閉じない。plugin / 映像 FX / VOICEVOX / 内蔵 / master Limiter
//! が同じ集合を使う。
//!
//! スペクトラムは「いま Rack の見える所に描かれている EQ / Tone EQ の Par」だけを engine に計算させる
//! (描いた Par が集める、`track_inspector::native_panel::EqScopes`)。送るのは差分があったフレームだけ
//! ([`AppData::sync_device_scopes`])。engine は device ごとに slot を据え置くので、スクロールで上の Par が外れても
//! 見え続けている Par のスペクトラムは途切れない (`daw_audio::project_ctl::device_scope_watch`)。

use common::model::RackPanelKey;
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

    /// `key` が「開いている Par」として覚えてよい鍵か: 現存する device で、Par を持つもの (字幕デバイスは
    /// 持たない、`common::plugin_db::is_subtitle`)。master の Limiter は常に居る。ViewState の保存と復元が
    /// 同じこの 1 本を通す (r.md #158 より前のファイルの字幕の鍵を、復元の時点で落とす)。
    pub(crate) fn rack_panel_key_valid(&self, key: RackPanelKey) -> bool {
        match key {
            RackPanelKey::Device(id) => {
                let song = self.cur.song_doc.song();
                song.device_by_id(id).is_some()
                    && song.plugin_by_id(id).is_none_or(|p| !common::plugin_db::is_subtitle(p.format, &p.plugin_id))
            }
            RackPanelKey::MasterLimiter => true,
        }
    }

    /// `ids` (node id) の Par を閉じる。削除 / 移動 (同じトラック内の並べ替えを含む、Q18) の後始末。
    pub(crate) fn close_rack_panels_of(&mut self, ids: &[u64]) {
        for &id in ids {
            let key = RackPanelKey::Device(id);
            self.cur.view.open_rack_panels.remove(&key);
            self.cur.peph.rack_panel_heights.remove(&key);
        }
    }

    /// engine にスペクトラムを計算させたい device: 前フレームにインスペクタが **見える所に** カーブを描いた
    /// EQ / Tone EQ の Par (上から [`MAX_DEVICE_SCOPES`](common::device_scope_bridge::MAX_DEVICE_SCOPES) 個まで、
    /// `ProjectEphemeral::inspector_visible_eq_scopes`)。
    ///
    /// 描いたものから決めるので、行の規則 (折り畳んだ Parallel / chain の中は行が無い)・帯の開閉・インスペクタの
    /// 表示 (r.md #158)・スクロールの位置が全部同じ 1 本で効く — model の木をたどって「開いている Par」から
    /// 決めると、スクロールで外れた Par にも engine が計算し続け、上限ぶんの枠を見えている Par から奪っていた。
    pub fn wanted_device_scopes(&self) -> Vec<u64> {
        self.cur.peph.inspector_visible_eq_scopes.clone()
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

