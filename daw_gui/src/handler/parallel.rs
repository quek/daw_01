//! handler::parallel — r.md #110 (`docs/plan_parallel.md`): Parallel / chain の編集と、インスペクタの
//! chain list の view-model ([`AppData::chain_rows`])。
//!
//! Song の書き換えは全部 `edit_song` 経由 (不変条件 5)。 chain の gain / pan / mute /
//! solo は `SetChain*` の値のみ IPC で即時に engine へも流す (track の M/S と同じ:
//! LoadSong の再 compile を待たずに効く)。
use crate::app_types::*;
use crate::state::*;
use common::model::{ChainRef, Device, MASTER_TRACK_ID, Parallel, ParallelChain, Split};
use common::protocol::AudioCommand;

impl AppData {
    // -------- 作る / 壊す ---------------------------------------------------

    /// `chain` の `index` に空の Parallel (chain 1 本) を挿す (picker の 「Parallel」)。
    pub(crate) fn add_parallel(&mut self, chain: ChainRef, index: u32) {
        self.ensure_first_track();
        self.edit_song_checked(move |song| {
            if song.chain_devices(chain).is_none() {
                return false;
            }
            let mut parallel = Parallel::new();
            parallel.id = song.alloc_device_id();
            parallel.chains[0].id = song.alloc_device_id();
            color_new_parallel(song, chain, &mut parallel);
            song.insert_device(chain, index as usize, Device::Parallel(parallel));
            true
        });
    }

    /// Group (Live の Ctrl+G): 選んだ device を順に抜き、 先頭の位置に Parallel (chain 1 本に
    /// 格納) を挿す。 選択に Parallel が混ざっていれば中身ごと入れ子になる。
    pub(crate) fn group_devices(&mut self, device_ids: Vec<u64>) {
        if device_ids.is_empty() {
            return;
        }
        // 表示順に並べ替える (chain_rows の順)。
        let order: Vec<u64> = self.chain_rows().iter().filter_map(|r| r.select_id()).collect();
        let mut ids: Vec<u64> = order
            .iter()
            .copied()
            .filter(|id| device_ids.contains(id))
            .collect();
        for id in device_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let result = self.edit_song_checked(move |song| {
            // 先頭 device の位置に Parallel を置く。
            let Some((dest, dest_index)) = song.find_device(ids[0]) else {
                return false;
            };
            // 落とし先が抜く device (Parallel) の中なら不正 (自分の中へは入れられない)。
            let mut taken: Vec<Device> = Vec::new();
            for &id in &ids {
                if let Some(d) = song.remove_device(id) {
                    taken.push(d);
                }
            }
            if taken.is_empty() {
                return false;
            }
            let mut parallel = Parallel::new();
            parallel.id = song.alloc_device_id();
            parallel.chains[0].id = song.alloc_device_id();
            parallel.chains[0].devices = taken;
            color_new_parallel(song, dest, &mut parallel);
            let at = dest_index.min(song.chain_devices(dest).map_or(0, Vec::len));
            song.insert_device(dest, at, Device::Parallel(parallel));
            true
        });
        if result {
            self.flush_song_sync();
        }
    }

    /// Ungroup (Live): Parallel を全 chain の device の直列連結に置換する。
    pub(crate) fn ungroup_parallel(&mut self, parallel_id: u64) {
        let result = self.edit_song_checked(move |song| {
            let Some((dest, index)) = song.find_device(parallel_id) else {
                return false;
            };
            let Some(Device::Parallel(parallel)) = song.remove_device(parallel_id) else {
                return false;
            };
            let flat = parallel.flatten();
            if let Some(chain) = song.chain_devices_mut(dest) {
                let at = index.min(chain.len());
                chain.splice(at..at, flat);
            }
            true
        });
        if result {
            self.prune_device_selection();
            self.flush_song_sync();
        }
    }

    pub(crate) fn add_parallel_chain(&mut self, parallel_id: u64) {
        let added = self.edit_song_checked(move |song| {
            let name = song.parallel_by_id(parallel_id).map(Parallel::next_chain_name);
            let Some(name) = name else { return false };
            let id = song.alloc_device_id();
            let ancestors = ancestor_colors(song, ChainRef::Chain(0), Some(parallel_id));
            let Some(parallel) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            let mut c = ParallelChain::new(name);
            c.id = id;
            c.color = Some(auto_color(&chain_colors(&parallel.chains), &ancestors));
            parallel.chains.push(c);
            true
        });
        let _ = added;
    }

    /// chain を複製 (中身ごと、 id は新採番)。 直後に挿す。
    pub(crate) fn duplicate_parallel_chain(&mut self, chain_id: u64) {
        self.edit_song_checked(move |song| {
            let Some((parallel, chain)) = song.chain_by_id(chain_id) else {
                return false;
            };
            let parallel_id = parallel.id;
            let mut copy = chain.clone();
            copy.name = format!("{} copy", chain.name);
            common::model::for_each_node_id_mut(&mut copy.devices, &mut |id| *id = song.alloc_device_id());
            copy.id = song.alloc_device_id();
            let ancestors = ancestor_colors(song, ChainRef::Chain(0), Some(parallel_id));
            let Some(parallel) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            copy.color = Some(auto_color(&chain_colors(&parallel.chains), &ancestors));
            let at = parallel.chains.iter().position(|c| c.id == chain_id).map_or(parallel.chains.len(), |i| i + 1);
            parallel.chains.insert(at, copy);
            true
        });
        // 複製した plugin を host に実体化する (`paste_devices` と同じ経路)。
        let created: Vec<common::model::PluginInstance> = self
            .song_doc
            .song()
            .all_plugins()
            .filter(|p| !self.ipc.loaded_devices.contains_key(&p.id) && !p.ports.is_video())
            .cloned()
            .collect();
        for inst in &created {
            self.ipc.pending_added_plugin_finalize.insert(inst.id, false);
        }
        for inst in &created {
            self.restore_device(inst);
        }
        self.flush_song_sync();
    }

    // -------- chain の属性 ---------------------------------------------------

    pub(crate) fn rename_parallel_chain(&mut self, chain_id: u64, name: String) {
        self.edit_song_checked(move |song| {
            let Some(c) = song.chain_by_id_mut(chain_id) else {
                return false;
            };
            if c.name == name {
                return false;
            }
            c.name = name;
            true
        });
    }

    pub(crate) fn rename_parallel(&mut self, parallel_id: u64, name: String) {
        self.edit_song_checked(move |song| {
            let Some(r) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            if r.name == name {
                return false;
            }
            r.name = name;
            true
        });
    }

    pub(crate) fn set_parallel_chain_color(&mut self, chain_id: u64, color: Option<[f32; 3]>) {
        self.edit_song_checked(move |song| {
            let Some(c) = song.chain_by_id_mut(chain_id) else {
                return false;
            };
            if c.color == color {
                return false;
            }
            c.color = color;
            true
        });
    }

    pub(crate) fn set_parallel_color(&mut self, parallel_id: u64, color: Option<[f32; 3]>) {
        self.edit_song_checked(move |song| {
            let Some(r) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            if r.color == color {
                return false;
            }
            r.color = color;
            true
        });
    }

    /// chain の gain / pan / mute / solo。 Song を書き換え (undo 対象) つつ、 値のみ IPC で
    /// engine へ即時反映する (再 compile なし)。
    pub(crate) fn set_chain_mixer(&mut self, chain_id: u64, edit: ChainMixerEdit) {
        let Some(track) = self
            .song_doc
            .song()
            .chain_owner_track(ChainRef::Chain(chain_id))
        else {
            return;
        };
        let changed = self.edit_song_checked(move |song| {
            let Some(c) = song.chain_by_id_mut(chain_id) else {
                return false;
            };
            match edit {
                ChainMixerEdit::Gain(g) => {
                    let g = g.clamp(0.0, common::model::MAX_TRACK_GAIN);
                    if c.gain == g {
                        return false;
                    }
                    c.gain = g;
                }
                ChainMixerEdit::Pan(p) => {
                    let p = p.clamp(-1.0, 1.0);
                    if c.pan == p {
                        return false;
                    }
                    c.pan = p;
                }
                ChainMixerEdit::Muted(m) => {
                    if c.muted == m {
                        return false;
                    }
                    c.muted = m;
                }
                ChainMixerEdit::Solo(s) => {
                    if c.solo == s {
                        return false;
                    }
                    c.solo = s;
                }
            }
            true
        });
        if changed {
            let cmd = match edit {
                ChainMixerEdit::Gain(gain) => AudioCommand::SetChainGain { track, chain_id, gain },
                ChainMixerEdit::Pan(pan) => AudioCommand::SetChainPan { track, chain_id, pan },
                ChainMixerEdit::Muted(muted) => AudioCommand::SetChainMuted { track, chain_id, muted },
                ChainMixerEdit::Solo(solo) => AudioCommand::SetChainSolo { track, chain_id, solo },
            };
            self.send_audio(cmd);
        }
    }

    /// Parallel の出力 trim / gain match (Song 書き換え + 値のみ IPC、chain mixer と同じ)。
    pub(crate) fn set_parallel_mixer(&mut self, parallel_id: u64, edit: ParallelMixerEdit) {
        let Some((at, _)) = self.song_doc.song().find_device(parallel_id) else {
            return;
        };
        let Some(track) = self.song_doc.song().chain_owner_track(at) else {
            return;
        };
        let changed = self.edit_song_checked(move |song| {
            let Some(r) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            match edit {
                ParallelMixerEdit::OutGain(g) => {
                    let g = g.clamp(0.0, common::model::MAX_TRACK_GAIN);
                    if r.out_gain == g {
                        return false;
                    }
                    r.out_gain = g;
                }
                ParallelMixerEdit::GainMatch(on) => {
                    if r.gain_match == on {
                        return false;
                    }
                    r.gain_match = on;
                }
                // 順序 (`low <= high`) と値域は model の setter が SSoT (engine 側も同じ関数)。
                ParallelMixerEdit::SplitFreq { edge, hz } => return r.set_split_freq(edge, hz),
                // r.md #114: Selector のアクティブ chain / クロスフェード (同じく model の setter)。
                ParallelMixerEdit::ActiveChain(chain_id) => return r.set_active_chain(chain_id),
                ParallelMixerEdit::SelectorFade(ms) => return r.set_selector_fade(ms),
            }
            true
        });
        if changed {
            let cmd = match edit {
                ParallelMixerEdit::OutGain(gain) => AudioCommand::SetParallelOutGain { track, parallel_id, gain },
                ParallelMixerEdit::GainMatch(on) => AudioCommand::SetParallelGainMatch { track, parallel_id, on },
                ParallelMixerEdit::SplitFreq { edge, hz } => {
                    AudioCommand::SetParallelSplitFreq { track, parallel_id, edge, hz }
                }
                ParallelMixerEdit::ActiveChain(chain_id) => {
                    AudioCommand::SetParallelActiveChain { track, parallel_id, chain_id }
                }
                ParallelMixerEdit::SelectorFade(fade_ms) => {
                    AudioCommand::SetParallelSelectorFade { track, parallel_id, fade_ms }
                }
            };
            self.send_audio(cmd);
        }
    }

    /// r.md #112: 入力の配り方 (`Split`) を切り替える。 構造変更なので `LoadSong` で運ぶ
    /// (値のみ IPC ではない)。 出力数より chain が少なければ空 chain を補い (色は自動)、 既定名
    /// (`Chain N` / 別モードの出力名) の chain は新モードの既定名へ付け替える (Low/Mid/High、
    /// Mid/Side、 off なら `Chain N`)。 ユーザーが付けた名前と中身は据え置き。 出力数を超える
    /// chain は残す (素通し入力)。 off に戻しても chain は消さない。 r.md #114: Selector は 2 本
    /// (A/B) に補い、 アクティブ chain を実在する id (未解決なら先頭) に揃える。
    pub(crate) fn set_parallel_split(&mut self, parallel_id: u64, split: Split) {
        self.edit_song_checked(move |song| {
            let Some(r) = song.parallel_by_id(parallel_id) else {
                return false;
            };
            let mut split = split;
            split.sanitize();
            if r.split == split {
                return false;
            }
            let missing = split.min_chains().saturating_sub(r.chains.len());
            let ids: Vec<u64> = (0..missing).map(|_| song.alloc_device_id()).collect();
            let ancestors = ancestor_colors(song, ChainRef::Chain(0), Some(parallel_id));
            let Some(parallel) = song.parallel_by_id_mut(parallel_id) else {
                return false;
            };
            parallel.split = split;
            for (k, c) in parallel.chains.iter_mut().enumerate() {
                if Split::is_generated_chain_name(&c.name) {
                    c.name = split.default_chain_name(k);
                }
            }
            for id in ids {
                let k = parallel.chains.len();
                let mut c = ParallelChain::new(split.default_chain_name(k));
                c.id = id;
                c.color = Some(auto_color(&chain_colors(&parallel.chains), &ancestors));
                parallel.chains.push(c);
            }
            parallel.normalize_selector();
            true
        });
    }

    /// 見方の都合: Parallel / chain の中身の開閉 (Bitwig の layer の開閉)。 dirty 無し。
    pub(crate) fn toggle_parallel_node_collapsed(&mut self, id: u64) {
        if !self.ui_prefs.collapsed_parallel_nodes.remove(&id) {
            self.ui_prefs.collapsed_parallel_nodes.insert(id);
        }
    }

    /// Parallel / chain の中身を展開しているか (既定 = 展開)。
    pub fn parallel_node_open(&self, id: u64) -> bool {
        !self.ui_prefs.collapsed_parallel_nodes.contains(&id)
    }

    // -------- view-model ----------------------------------------------------

    /// インスペクタの chain list (縦回転 Live 型、`docs/plan_parallel.md` §6.1)。 cursor track の
    /// device ツリーを行に flatten する: Parallel は 開始行 / chain 行 × N (展開中の chain はその直下に
    /// device (再帰) + `+ Plugin`) / `+ chain` / 終了行。 折り畳んだ chain / Parallel は 1 行だけ。
    pub fn chain_rows(&self) -> Vec<ChainRow> {
        let Some(track_id) = self.cursor_track_id() else {
            return Vec::new();
        };
        let song = self.song_doc.song();
        let Some(devices) = song.fx_chain_by_track_id(track_id) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        self.push_chain_rows(devices, ChainRef::Track(track_id), 0, &[], &mut rows);
        rows.push(ChainRow {
            kind: ChainRowKind::AddPlugin { chain: ChainRef::Track(track_id) },
            chain: ChainRef::Track(track_id),
            index: devices.len() as u32,
            depth: 0,
            bars: Vec::new(),
            parallel_band: None,
        });
        rows
    }

    fn push_chain_rows(
        &self,
        devices: &[Device],
        chain: ChainRef,
        depth: u32,
        bars: &[Option<[f32; 3]>],
        rows: &mut Vec<ChainRow>,
    ) {
        for (i, d) in devices.iter().enumerate() {
            match d {
                Device::Plugin(p) => rows.push(ChainRow {
                    kind: ChainRowKind::Plugin(self.chain_entry_for(p)),
                    chain,
                    index: i as u32,
                    depth,
                    bars: bars.to_vec(),
                    parallel_band: None,
                }),
                Device::Parallel(r) => self.push_parallel_rows(r, chain, i as u32, depth, bars, rows),
            }
        }
    }

    /// Parallel 1 つぶんの行: 開始行 / chain 行 × N (展開中の chain はその行の直下に中身 (再帰) +
    /// `+ Plugin`) / `+ chain` / 終了行。 折り畳んだ Parallel は開始行だけ。
    fn push_parallel_rows(
        &self,
        r: &Parallel,
        chain: ChainRef,
        index: u32,
        depth: u32,
        bars: &[Option<[f32; 3]>],
        rows: &mut Vec<ChainRow>,
    ) {
        let row = |kind: ChainRowKind| ChainRow {
            kind,
            chain,
            index,
            depth,
            bars: bars.to_vec(),
            parallel_band: Some(r.color),
        };
        let parallel_open = self.parallel_node_open(r.id);
        rows.push(row(ChainRowKind::ParallelBegin {
            parallel_id: r.id,
            name: r.name.clone(),
            bypassed: r.bypassed,
            color: r.color,
            open: parallel_open,
            out_gain: r.out_gain,
            gain_match: r.gain_match,
            split: r.split,
        }));
        // 折り畳んだ Parallel は開始行 1 本だけ (chain も終了行も出さない)。
        if !parallel_open {
            return;
        }
        // r.md #112: Split の param 行はヘッダ直下 (chain 行の帯域名と並び順で対応する)。
        if r.split.has_params() {
            rows.push(row(ChainRowKind::SplitParams { parallel_id: r.id, split: r.split }));
        }
        // r.md #114: Selector ならアクティブ chain 以外を薄く出す。
        let active = r.active_chain_index();
        for (k, c) in r.chains.iter().enumerate() {
            let open = self.parallel_node_open(c.id);
            rows.push(row(ChainRowKind::Chain {
                parallel_id: r.id,
                chain_id: c.id,
                name: c.name.clone(),
                color: c.color,
                gain: c.gain,
                pan: c.pan,
                muted: c.muted,
                solo: c.solo,
                open,
                n_devices: c.devices.len(),
                inactive: active.is_some_and(|a| a != k),
            }));
            // 展開中 chain の中身は **その chain 行の直下** (他の chain 行はその後ろに続く)。
            if !open {
                continue;
            }
            let mut inner = bars.to_vec();
            inner.push(c.color);
            self.push_chain_rows(&c.devices, ChainRef::Chain(c.id), depth + 1, &inner, rows);
            rows.push(ChainRow {
                kind: ChainRowKind::AddPlugin { chain: ChainRef::Chain(c.id) },
                chain: ChainRef::Chain(c.id),
                index: c.devices.len() as u32,
                depth: depth + 1,
                bars: inner,
                parallel_band: None,
            });
        }
        rows.push(row(ChainRowKind::AddChain { parallel_id: r.id }));
        rows.push(ChainRow {
            kind: ChainRowKind::ParallelEnd { parallel_id: r.id, color: r.color },
            chain,
            index: index + 1,
            depth,
            bars: bars.to_vec(),
            parallel_band: Some(r.color),
        });
    }

    /// `ChainEntry` (plugin 行の表示情報)。 `inspector_chain` と同じ規則。
    pub fn chain_entry_for(&self, p: &common::model::PluginInstance) -> ChainEntry {
        use common::plugin_format::PluginFormat;
        // 埋め込み GUI の有無。 builtin (VOICEVOX / Silence) は規定で持たないので
        // format から即断 (= PluginParamList 到着前でも正しく「Par」routing)。 外部
        // CLAP・VST3 は host の通知 (`slot_has_gui`)、 未受信 (load 直後) は楽観的に
        // true で「GUI」のまま。
        let has_embedded_gui = p.format != PluginFormat::Builtin
            && self.ipc.slot_has_gui.get(&p.id).copied().unwrap_or(true);
        let has_params = self
            .ipc
            .plugin_params
            .get(&p.id)
            .is_some_and(|v| !v.is_empty());
        let is_voicevox =
            p.format == PluginFormat::Builtin && p.plugin_id == common::plugin_db::BUILTIN_ID_VOICEVOX;
        ChainEntry {
            device_id: p.id,
            plugin_name: crate::app_types::resolve_plugin_name(&self.ipc.plugin_db, &p.plugin_id),
            has_embedded_gui,
            is_video: p.ports.is_video(),
            is_voicevox,
            has_params,
            send_all_keys: p.send_all_keys_to_plugin,
            load_error: self.ipc.failed_plugin_loads.get(&p.id).cloned(),
            bypassed: p.bypassed,
            aux_input_count: p.aux_input_count,
            sc_wired: p.aux_inputs.iter().any(Option::is_some),
        }
    }

    /// `device_id` の sidechain (aux 入力) port ごとの配線 (SC パネルの行)。
    /// 行数 = host が報告した port 数 (`aux_input_count`、engine が staging できる
    /// `MAX_AUX_IN` で cap)。
    pub fn sidechain_ports(&self, device_id: u64) -> Vec<SidechainPort> {
        let Some(p) = self.song_doc.song().plugin_by_id(device_id) else {
            return Vec::new();
        };
        let n = (p.aux_input_count as usize).min(common::process_data::MAX_AUX_IN);
        (0..n)
            .map(|port| {
                let route = p.aux_inputs.get(port).and_then(|o| o.as_ref());
                SidechainPort {
                    port: port as u8,
                    source: route.map(|r| r.tap.source),
                    tap_point: route.map(|r| r.tap.tap_point).unwrap_or_default(),
                }
            })
            .collect()
    }

    /// sidechain / follower の source 候補: 「—」 + 他 track + 同 track の Parallel 内 chain
    /// (`Parallel名 / Chain名`)。 自 track の出力は除外 (feedback → `GraphError::Cycle`)。
    pub fn tap_source_choices(&self, include_none: bool) -> Vec<SidechainSourceChoice> {
        let song = self.song_doc.song();
        let cursor_id = self.cursor_track_id();
        let mut choices: Vec<SidechainSourceChoice> = Vec::new();
        if include_none {
            choices.push(SidechainSourceChoice {
                label: "—".into(),
                source: None,
            });
        }
        for t in &song.tracks {
            if Some(t.id) == cursor_id {
                continue;
            }
            choices.push(SidechainSourceChoice {
                label: format!("{} (id {})", t.name, t.id),
                source: Some(common::model::TapSource::Track(t.id)),
            });
        }
        if let Some(devices) = cursor_id.and_then(|id| song.fx_chain_by_track_id(id)) {
            common::model::for_each_chain(devices, &mut |parallel, c| {
                choices.push(SidechainSourceChoice {
                    label: format!("{} / {}", parallel.name, c.name),
                    source: Some(common::model::TapSource::Chain(c.id)),
                });
            });
        }
        choices
    }

    /// パラアウトの宛先候補 (「—」 + 自分以外の track)。
    pub fn paraout_dest_choices(&self) -> Vec<(String, Option<u32>)> {
        let cursor_id = self.cursor_track_id();
        let mut out = vec![("—".to_string(), None)];
        for t in &self.song_doc.song().tracks {
            if Some(t.id) == cursor_id || t.id == MASTER_TRACK_ID {
                continue;
            }
            out.push((format!("{} (id {})", t.name, t.id), Some(t.id)));
        }
        out
    }
}

/// [`AppData::set_chain_mixer`] の編集内容。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChainMixerEdit {
    Gain(f32),
    Pan(f32),
    Muted(bool),
    Solo(bool),
}

/// [`AppData::set_parallel_mixer`] の編集内容。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParallelMixerEdit {
    OutGain(f32),
    GainMatch(bool),
    /// r.md #112: 帯域分割のクロスオーバー周波数 (Hz)。
    SplitFreq { edge: common::model::SplitEdge, hz: f32 },
    /// r.md #114: Selector のアクティブ chain (安定 `ParallelChain::id`)。
    ActiveChain(u64),
    /// r.md #114: Selector のクロスフェード時間 (ms)。
    SelectorFade(f32),
}

/// 自動色 (Bitwig と同じく作った時点で周囲と別の色)。 パレット (色相順) から、 **兄弟にも祖先
/// (外側の chain / Parallel) にも色相が一番遠い** 色を取る。 隣の色相 (赤の次に橙) を順に
/// 振ると見分けがつかないので、 「使用中の色との最小色相差」 が最大の候補を選ぶ。
/// 使用中が無ければ先頭 (赤)。 全候補が使用済みなら差が最大のもの (= 重複を許す)。
pub(crate) fn auto_color(siblings: &[[f32; 3]], ancestors: &[[f32; 3]]) -> [f32; 3] {
    use crate::view::track_color::PALETTE;
    // 無彩色寄りの末尾 2 色 (taupe / slate) は色相で区別できないので候補から外す。
    let candidates = &PALETTE[..PALETTE.len() - 2];
    let used: Vec<f32> = siblings.iter().chain(ancestors).map(|c| hue_deg(*c)).collect();
    if used.is_empty() {
        return candidates[0];
    }
    let score = |c: &[f32; 3]| {
        let h = hue_deg(*c);
        used.iter()
            .map(|u| {
                let d = (h - u).abs() % 360.0;
                d.min(360.0 - d)
            })
            .fold(f32::MAX, f32::min)
    };
    *candidates
        .iter()
        .max_by(|a, b| score(a).partial_cmp(&score(b)).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(&candidates[0])
}

/// RGB (0..1) の色相 (度)。 無彩色は 0。
fn hue_deg(c: [f32; 3]) -> f32 {
    let [r, g, b] = c;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d <= 1e-6 {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d) % 6.0
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let h = h * 60.0;
    if h < 0.0 { h + 360.0 } else { h }
}

fn chain_colors(chains: &[ParallelChain]) -> Vec<[f32; 3]> {
    chains.iter().filter_map(|c| c.color).collect()
}

/// `at` から外側へ辿った祖先の色 (内側から順): chain の色 → その Parallel の色 → …。
/// `from_parallel` を渡すとその Parallel (の色) から辿り始める。 track 直下なら空。
pub(crate) fn ancestor_colors(
    song: &common::model::Song,
    at: ChainRef,
    from_parallel: Option<u64>,
) -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    let mut cur = match from_parallel {
        Some(rid) => {
            let Some(parallel) = song.parallel_by_id(rid) else { return out };
            out.extend(parallel.color);
            song.find_device(rid).map(|(r, _)| r)
        }
        None => Some(at),
    };
    // ネストは有限 (device ツリー) だが、壊れた参照でも必ず止まるよう深さを切る。
    for _ in 0..64 {
        let Some(ChainRef::Chain(cid)) = cur else { break };
        let Some((parallel, chain)) = song.chain_by_id(cid) else { break };
        out.extend(chain.color);
        out.extend(parallel.color);
        cur = song.find_device(parallel.id).map(|(r, _)| r);
    }
    out
}

/// 新しい Parallel (`at` に挿す) とその chain 1 本に自動色を振る: Parallel は同じ列の他の Parallel と
/// 祖先に無い色、chain はその Parallel と祖先に無い色。
fn color_new_parallel(song: &common::model::Song, at: ChainRef, parallel: &mut Parallel) {
    let ancestors = ancestor_colors(song, at, None);
    let sibling_parallels: Vec<[f32; 3]> = song
        .chain_devices(at)
        .map(|devs| devs.iter().filter_map(|d| d.as_parallel().and_then(|r| r.color)).collect())
        .unwrap_or_default();
    let parallel_color = auto_color(&sibling_parallels, &ancestors);
    parallel.color = Some(parallel_color);
    let mut chain_ancestors = vec![parallel_color];
    chain_ancestors.extend(ancestors);
    let mut sibs: Vec<[f32; 3]> = Vec::new();
    for c in &mut parallel.chains {
        let col = auto_color(&sibs, &chain_ancestors);
        c.color = Some(col);
        sibs.push(col);
    }
}
