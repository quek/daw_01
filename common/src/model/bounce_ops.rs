//! 焼き込み (Bounce In Place / Bounce with FX / `J` Glue) が Song に対して行う操作
//! (`docs/plan_audio_clip.md` §3.8 / `docs/plan_glue_bake.md`)。engine の offline render が **どの段まで描くか**
//! ([`crate::protocol::RenderScope`]) と、描いた WAV を **どこへ置くか** の対応をここが持つ:
//!
//! - Bounce In Place / Glue: `RenderScope::Sources` (素材の音) を元のトラックのクリップへ戻す。再生時に
//!   トラックの device / フェーダー / master をもう一度通る。
//! - Bounce with FX: `RenderScope::PostFx` (device チェーンを通した音) を新しいトラックに置き、焼いた元クリップを
//!   mute する ([`Song::place_bounce_with_fx`])。PostFx 点から後ろ (フェーダーと、そこから先の配線) は
//!   焼かずに元トラックから写す。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use std::collections::{HashMap, HashSet};

use super::*;

/// engine が書いた焼き込み WAV 1 本。
#[derive(Debug, Clone, PartialEq)]
pub struct BakedWav {
    pub path: AudioSourcePath,
    /// render したサンプルレート (= engine のレート)。
    pub sample_rate: u32,
    /// 実際に書かれた長さ (減衰 tail を含む)。
    pub frames: u64,
}

impl Song {
    /// offline render 用に「そのトラックだけ」を描く Song を組む。**決めるのはどのトラックを描くかだけ** —
    /// どの処理段を通すか (master の fx / 音量 / Limiter、トラックの fx / 内蔵 device / Parallel の混ぜ /
    /// フェーダー) は `BounceClipFxOnline::scope` (`RenderScope`) が engine の compile で program の形に焼く。
    /// Song を書き換えて処理を消すと、書き換えで表せない組み合わせ (音源を含む Parallel に音が入る等) が
    /// 正確に焼けない。
    ///
    /// - 他のトラックを落とし、親 group から外す。それらを指していた参照 (send / SC / パラアウト /
    ///   follower / MIDI binding / レーン / 変調) は編集後の不変条件と同じ掃除
    ///   ([`Self::prune_dangling_refs`]) が外す — 自トラックを読む配線 (自トラック Pre-FX の SC 等) は残る。
    ///   **ここは `SongDoc` を通らない** ので明示的に呼ぶ。
    /// - 元トラックの mute / solo を解除する (mute 済みのトラックでも焼く)。
    /// - **ランチャーの主導権は必ずアレンジへ戻す**: 行が [`RowPlayback::Launcher`] /
    ///   `LauncherStopped` のままだと、offline 走査は「今のセッションの状態」を再現する
    ///   (= セルの音が鳴り、アレンジのクリップは鳴らない) ので、焼く対象が丸ごと入れ替わる。
    /// - **移調 0 で焼く** (r.md #130 確定仕様 Q8): 焼いた音は元のトラック / 新しいトラックでもう一度移調に
    ///   追従するので、移調を焼き込むと二重に移調される。「外に出すもの (WAV / SMF) = 鳴る音、プロジェクトの
    ///   中に作るもの = 書いた音」。基準値を 0 にし、移調のレーンと変調も外す (深さの変調は下の掃除が連鎖で外す)。
    #[must_use]
    pub fn isolated_track(&self, track_id: u32) -> Option<Song> {
        let mut kept = self.track_by_id(track_id)?.clone();
        kept.parent_group_id = None;
        kept.muted = false;
        kept.solo = false;
        kept.launcher = RowPlayback::Arranger;
        for lane in &mut kept.automation_lanes {
            lane.launcher = RowPlayback::Arranger;
        }
        // 入力を受けるバスは自分のクリップを鳴らさない。入力を落とすと leaf に変わってクリップが鳴り出すので、
        // 元どおり鳴らさない。
        if !self.track_sounds_own_clips(track_id) {
            kept.clips.clear();
        }
        let mut isolated = self.clone();
        isolated.tracks = vec![kept];
        isolated.transpose = 0;
        isolated.song_lanes.retain(|l| l.target != AutomationTarget::SongTranspose);
        isolated.song_mod_routings.retain(|r| r.target != AutomationTarget::SongTranspose);
        isolated.prune_dangling_refs();
        Some(isolated)
    }

    /// このトラックが自分のクリップを鳴らすか。入力を受けるバス (子を持つ group / send を受ける return /
    /// パラアウトの宛先) は入力の合流だけを描き、自分のクリップを鳴らさない — 楽器兼バスの group
    /// (`Track::paraout_split_device` を持つ) だけは自分の楽器を鳴らす。engine の bus / leaf 分類
    /// (`daw_audio::graph::compile::deps` の `bus_flags` / `gwi_split`) と同じ規則。
    #[must_use]
    pub fn track_sounds_own_clips(&self, track_id: u32) -> bool {
        let is_group = self.tracks.iter().any(|t| t.parent_group_id == Some(track_id));
        let receives_send = self.tracks.iter().any(|t| t.sends.iter().any(|s| s.dest_track_id == track_id));
        let mut receives_paraout = false;
        for devices in self.tracks.iter().map(|t| t.devices.as_slice()).chain(std::iter::once(self.master_fx_chain.as_slice())) {
            for p in plugins(devices) {
                receives_paraout |= p.aux_outputs.iter().flatten().any(|r| r.dest_track == track_id);
            }
        }
        let instrument_bus = is_group && self.track_by_id(track_id).is_some_and(|t| t.paraout_split_device().is_some());
        !(is_group || receives_send || receives_paraout) || instrument_bus
    }

    /// 焼いた WAV を `media.audio_sources` に登録し、`window` を鳴らす **単一 audio event** の content を返す
    /// (bounce / Glue 共通の SSoT)。置き方 (新しい content / 既存 content の置き換え) は呼び出し側が決める。
    ///
    /// - `window` = 焼いた song 絶対拍の範囲 `[start, end)`。
    /// - `event_start_in_clip_beats` = content 内でこの event を置く位置 (clip の窓の起点)。
    ///
    /// **`source_end_frames` は「書き出し窓ちょうど」に切る。** render は減衰 tail の
    /// ぶん窓より長く書く (`RenderWindow::resolve` の `TAIL_MAX_SECONDS`、無音でも
    /// 最低 0.5 秒) ので、ファイル長をそのまま載せると伸縮比
    /// (= source 秒 / event 秒、`stretch_ratio_for`) が 1.0 を超え、**置換後のクリップが
    /// 速く鳴る** (120BPM の 4 拍で実測 +25%)。拍→サンプル換算は engine が窓を
    /// 決めたのと同じ SSoT (`beats_to_samples` = tempo automation の積分) を通す。
    ///
    /// **`stretch_mode` は `Raw`。** 焼いた音は「そのときの tempo で描いた実時間の
    /// 波形」なので、拍に対して線形に読み直す `Stretch` を通すと、テンポカーブのある曲で
    /// 中身が内部でずれる (両端だけ合って中盤が数百 ms 動く)。定テンポでも `Stretch` は
    /// 位相ボコーダを必ず通るのでトランジェントがにじむ。テンポ追従させたければ
    /// 焼いたあとに inspector で切り替えられる (`AudioSource.original_bpm` は入れてある)。
    pub fn add_baked_audio(
        &mut self,
        wav: BakedWav,
        window: (f64, f64),
        event_start_in_clip_beats: f64,
    ) -> (AudioSourceId, AudioContent) {
        let sr = wav.sample_rate;
        let window_frames = crate::automation::beats_to_samples(self, sr, window.1)
            .saturating_sub(crate::automation::beats_to_samples(self, sr, window.0));
        let source_id = self.alloc_audio_source_id();
        let source = AudioSource {
            path: wav.path,
            sample_rate: sr,
            channels: 2,
            frames: wav.frames,
            original_bpm: Some(self.bpm),
            root_key: None,
        };
        self.media.audio_sources.insert(source_id, source);
        let event = AudioEvent {
            // 新規 content の単一 event なので id=1 / allocator は 2 から。
            id: 1,
            source_id,
            event_start_in_clip_beats,
            event_length_beats: window.1 - window.0,
            source_start_frames: 0,
            source_end_frames: window_frames.min(wav.frames).max(1),
            stretch_mode: StretchMode::Raw,
            ..AudioEvent::default()
        };
        (source_id, AudioContent { events: vec![event], next_event_id: 2 })
    }

    /// Bounce with FX の結果 (`clip` = 焼いた content を指すクリップ) を新しいトラックに置き、焼いた元クリップ
    /// (`source`) を mute する。新しいトラックの id を返す (元クリップが無ければ `None`)。
    ///
    /// **元トラックは mute しない** — 他のクリップも、group の子 / send / パラアウトから流れてくる音もそのまま
    /// 鳴り、焼いたクリップの音だけが新しいトラックへ移る (どのトラックでも同じ規則)。
    ///
    /// 焼いた音は元トラックの PostFx 点 (`RenderScope::PostFx`) なので、**PostFx 点から後ろで元トラックの音に
    /// 効いていたものを新しいトラックへ写す** — 元と同じ音量・定位・送り・行き先で鳴り、後からフェーダーを
    /// 動かせる:
    ///
    /// - フェーダー: volume / pan / mute / solo と、それを指すレーン (Volume / Pan / Mute / SendGain) と変調を
    ///   複製する。レーンの中身は独立に複製し (`fork_content`)、変調は新しい id で同じ変調ソースを指す
    ///   (トラック複製と同じ規則)。複製した変調の深さを指すレーン / 変調も連れていく。
    /// - send: 同じ id のまま複製する。pre-fader の send は PostFx 点 = 焼いた音を読むので同じ量が出る。
    /// - 行き先: 同じ親 group の、元トラックの subtree の直後に置く。
    /// - 元トラックを読む配線 (SC / follower) は付け替えない — 元トラックは鳴り続けるので、付け替えると残りの音を
    ///   失う。
    pub fn place_bounce_with_fx(&mut self, source: ClipKey, name: String, clip: Clip) -> Option<u32> {
        let src_idx = self.track_index_by_id(source.track_id)?;
        self.tracks[src_idx].clip_by_id_mut(source.clip_id)?.muted = true;
        let insert_at = self.subtree_end(source.track_id).unwrap_or(src_idx) + 1;
        let id = self.alloc_track_id();
        let (automation_lanes, mod_routings) = self.copy_fader_stage(src_idx);
        let src = &self.tracks[src_idx];
        let mut track = Track {
            id,
            name,
            volume: src.volume,
            pan: src.pan,
            muted: src.muted,
            solo: src.solo,
            parent_group_id: src.parent_group_id,
            sends: src.sends.clone(),
            next_send_id: src.next_send_id,
            automation_lanes,
            next_lane_id: src.next_lane_id,
            mod_routings,
            ..Track::default()
        };
        track.place_clip(clip);
        self.tracks.insert(insert_at.min(self.tracks.len()), track);
        Some(id)
    }

    /// 元トラックのフェーダーの段 (Volume / Pan / Mute / SendGain) を指すレーンと変調の複製
    /// ([`Self::place_bounce_with_fx`])。変調は、フェーダーを指すものと、複製する変調の深さを指すもの (固定点まで)。
    fn copy_fader_stage(&mut self, src_idx: usize) -> (Vec<AutomationLane>, Vec<ModRouting>) {
        use TrackBuiltinParam as B;
        let follows = |target: &AutomationTarget, picked: &HashSet<u32>| match target {
            AutomationTarget::TrackBuiltin(B::Volume | B::Pan | B::Mute | B::SendGain { .. }) => true,
            AutomationTarget::ModRoutingDepth { routing_id } => picked.contains(routing_id),
            _ => false,
        };
        let src = &self.tracks[src_idx];
        let mut picked: HashSet<u32> = HashSet::new();
        loop {
            let more: Vec<u32> = src
                .mod_routings
                .iter()
                .filter(|r| !picked.contains(&r.id) && follows(&r.target, &picked))
                .map(|r| r.id)
                .collect();
            if more.is_empty() {
                break;
            }
            picked.extend(more);
        }
        let mut routings: Vec<ModRouting> = src.mod_routings.iter().filter(|r| picked.contains(&r.id)).cloned().collect();
        let mut lanes: Vec<AutomationLane> =
            src.automation_lanes.iter().filter(|l| follows(&l.target, &picked)).cloned().collect();
        let remap: HashMap<u32, u32> = routings.iter().map(|r| (r.id, self.alloc_mod_routing_id())).collect();
        let rewire = |target: &mut AutomationTarget| {
            if let AutomationTarget::ModRoutingDepth { routing_id } = target
                && let Some(&new) = remap.get(routing_id)
            {
                *routing_id = new;
            }
        };
        for r in &mut routings {
            r.id = remap[&r.id];
            rewire(&mut r.target);
        }
        // 同じ content を共有するクリップ同士 (リンク) は複製先でもリンクのまま。
        let mut forked: HashMap<ContentId, ContentId> = HashMap::new();
        let mut fork = |song: &mut Song, id: &mut ContentId| {
            *id = *forked.entry(*id).or_insert_with(|| song.fork_content(*id));
        };
        for lane in &mut lanes {
            rewire(&mut lane.target);
            for c in &mut lane.clips {
                fork(self, &mut c.content_id);
            }
            for cell in &mut lane.session_clips {
                fork(self, &mut cell.clip.content_id);
            }
        }
        (lanes, routings)
    }

    /// `root` とその子孫のうち、トラック列で最後に居るものの index。
    fn subtree_end(&self, root: u32) -> Option<usize> {
        let within = |id: u32| {
            let mut cur = Some(id);
            for _ in 0..=self.tracks.len() {
                match cur {
                    Some(c) if c == root => return true,
                    Some(c) => cur = self.track_by_id(c).and_then(|t| t.parent_group_id),
                    None => return false,
                }
            }
            false
        };
        self.tracks.iter().rposition(|t| within(t.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u32) -> Track {
        Track { id, ..Track::default() }
    }

    /// `isolated_track` が決めるのは「どのトラックを描くか」だけ: 他トラックと、それを指す参照 (SC / send /
    /// follower) は外れ、自トラックの device (内蔵を含む) / 自トラックを読む SC / master の段はそのまま残る
    /// (どの段を通すかは `RenderScope` が engine の compile で決める)。
    #[test]
    fn isolated_track_keeps_only_the_track_and_leaves_processing_to_the_scope() {
        let mut song = Song::default();
        let (tid, side) = (song.alloc_track_id(), song.alloc_track_id());
        song.tracks = vec![track(tid), track(side)];
        song.normalize_native_devices();
        let reads = |song: &mut Song, source: TapSource, point: TapPoint| {
            let id = song.alloc_device_id();
            let mut dev = NativeDevice::new_added(NativeKind::Comp, id, 2);
            dev.aux_input = Some(AuxInputRoute { tap: AudioTap::new(source, point) });
            song.insert_device(ChainRef::Track(tid), 0, Device::Native(dev));
            id
        };
        let side_sc = reads(&mut song, TapSource::Track(side), TapPoint::PostFader);
        let own_sc = reads(&mut song, TapSource::Track(tid), TapPoint::PreFx);
        let kind = ModSourceKind::EnvelopeFollower { tap: Some(AudioTap::post_fader(side)), follower: Default::default() };
        song.mod_sources.push(ModSource { id: 1, owner_track_id: tid, color: [1.0; 3], kind, enabled: true });
        let t = song.track_by_id_mut(tid).expect("track");
        t.sends.push(Send { id: 1, dest_track_id: side, gain: 1.0, mode: SendMode::PostFader, enabled: true });
        t.muted = true;
        t.launcher = RowPlayback::LauncherStopped;
        song.master_limiter.on = true;
        song.song_lanes.push(AutomationLane::new(AutomationTarget::MasterLimiter(MasterLimiterParam::On), 1.0));

        let isolated = song.isolated_track(tid).expect("isolated");
        assert_eq!(isolated.tracks.len(), 1, "描くのはそのトラックだけ");
        let kept = &isolated.tracks[0];
        let sc = |id| isolated.native_by_id(id).expect("内蔵 device は残る").aux_input.map(|r| r.tap.source);
        assert_eq!(sc(side_sc), None, "他トラックを読む SC は外れる");
        assert_eq!(sc(own_sc), Some(TapSource::Track(tid)), "自トラックを読む SC は残る");
        let natives = |t: &Track| {
            let mut v = Vec::new();
            for_each_native(&t.devices, &mut |n| v.push(n.id));
            v
        };
        assert_eq!(natives(kept), natives(song.track_by_id(tid).expect("track")), "内蔵 device は Song から消さない");
        assert!(kept.sends.is_empty(), "他トラック宛ての send は外れる");
        assert_eq!(isolated.mod_sources[0].follower().and_then(|(tap, _)| tap), None, "他トラックを聴く follower は入力なし");
        assert!(!kept.muted && kept.launcher == RowPlayback::Arranger, "mute を解き、アレンジを描く");
        assert!(isolated.master_limiter.on, "master の段は Song に残し、scope が通さない");
        assert_eq!(isolated.song_lanes.len(), song.song_lanes.len());
        assert_eq!(isolated.master_fx_chain, song.master_fx_chain);
    }

    /// r.md #130 Q8: 焼き込みは書いた音 (移調 0) で描く。基準値・移調のレーン・移調の変調が外れ、
    /// 他の song 側のレーン / 変調とトラックの追従設定は残る (焼いた音はもう一度移調に追従する)。
    #[test]
    fn isolated_track_renders_without_transpose() {
        let mut song = Song { transpose: 5, ..Song::default() };
        let tid = song.alloc_track_id();
        song.tracks = vec![Track { follow_transpose: false, ..track(tid) }];
        song.mod_sources.push(ModSource { id: 1, owner_track_id: MASTER_TRACK_ID, color: [1.0; 3], kind: ModSourceKind::default(), enabled: true });
        let routing = |id, target| ModRouting { id, target, source_id: 1, depth: 0.5, polarity: Polarity::Unipolar, enabled: true };
        song.song_lanes = vec![
            AutomationLane { id: 1, ..AutomationLane::new(AutomationTarget::SongTranspose, 3.0) },
            AutomationLane { id: 2, ..AutomationLane::new(AutomationTarget::SongTempo, 120.0) },
        ];
        song.song_mod_routings = vec![routing(1, AutomationTarget::SongTranspose), routing(2, AutomationTarget::SongTempo)];

        let isolated = song.isolated_track(tid).expect("isolated");
        assert_eq!(isolated.transpose, 0);
        assert!(!isolated.transpose_can_be_nonzero(), "移調が 0 以外になる経路が残らない");
        assert_eq!(isolated.song_lanes.iter().map(|l| l.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(isolated.song_mod_routings.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
        assert!(!isolated.tracks[0].follow_transpose, "追従設定は写したまま");
    }

    /// Bounce with FX の置き方 = PostFx 点から後ろを写す規則: フェーダー (値 / レーン / 変調とその深さ) / send /
    /// 親 group が新しいトラックへ写り、元トラックは焼いたクリップの mute だけ (他のクリップ・中身・読む配線は
    /// そのまま)。写したものは編集後の不変条件でも消えない (= dangling を作らない)。
    #[test]
    fn bounce_with_fx_moves_what_follows_the_post_fx_point_to_the_new_track() {
        use TrackBuiltinParam as B;
        let mut song = Song::default();
        let [group, src, sibling, ret, reader] = [(); 5].map(|()| song.alloc_track_id());
        song.tracks = vec![track(group), track(src), track(sibling), track(ret), track(reader)];
        for id in [src, sibling] {
            song.track_by_id_mut(id).expect("child").parent_group_id = Some(group);
        }
        song.normalize_native_devices();
        let reads = |song: &mut Song, point: TapPoint| {
            let id = song.alloc_device_id();
            let mut dev = NativeDevice::new_added(NativeKind::Comp, id, 2);
            dev.aux_input = Some(AuxInputRoute { tap: AudioTap::new(TapSource::Track(src), point) });
            song.insert_device(ChainRef::Track(reader), 0, Device::Native(dev));
            id
        };
        let (post_fader_sc, post_fx_sc) = (reads(&mut song, TapPoint::PostFader), reads(&mut song, TapPoint::PostFx));
        let lfo = song.alloc_mod_source_id();
        song.mod_sources.push(ModSource {
            id: lfo,
            owner_track_id: src,
            color: [1.0; 3],
            kind: ModSourceKind::EnvelopeFollower { tap: Some(AudioTap::post_fader(src)), follower: Default::default() },
            enabled: true,
        });
        let volume_content = song.alloc_content(ClipContent::Automation(AutomationContent::default()), "vol".into());
        let (pan_routing, depth_routing, device_routing) =
            (song.alloc_mod_routing_id(), song.alloc_mod_routing_id(), song.alloc_mod_routing_id());
        let routing = |id, target| ModRouting { id, target, source_id: lfo, depth: 0.5, polarity: Polarity::default(), enabled: true };
        let source_content = song.alloc_content(ClipContent::Audio(AudioContent::default()), "src".into());
        let t = song.track_by_id_mut(src).expect("src");
        let source_clip = t.place_clip(Clip { start_beat: 1.0, length_beats: 2.0, content_id: source_content, ..Clip::default() });
        let other_clip = t.place_clip(Clip { start_beat: 4.0, length_beats: 2.0, content_id: source_content, ..Clip::default() });
        (t.volume, t.pan, t.solo) = (0.5, -0.4, true);
        t.sends.push(Send { id: 3, dest_track_id: ret, gain: 0.7, mode: SendMode::PreFader, enabled: true });
        t.next_send_id = 4;
        let mut volume_lane = AutomationLane::new(AutomationTarget::TrackBuiltin(B::Volume), 0.5);
        volume_lane.id = 1;
        volume_lane.clips.push(AutomationClip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: volume_content, ..AutomationClip::default() });
        let mut send_lane = AutomationLane::new(AutomationTarget::TrackBuiltin(B::SendGain { send_id: 3, legacy_send_idx: None }), 0.7);
        send_lane.id = 2;
        t.automation_lanes = vec![volume_lane, send_lane];
        t.next_lane_id = 3;
        t.mod_routings = vec![
            routing(pan_routing, AutomationTarget::TrackBuiltin(B::Pan)),
            routing(depth_routing, AutomationTarget::ModRoutingDepth { routing_id: pan_routing }),
            routing(device_routing, AutomationTarget::ModSourceParam { source_id: lfo, param: ModParam::Rate }),
        ];
        song.enforce_edit_invariants();
        let before = song.track_by_id(src).expect("src").clone();

        let content_id = song.alloc_content(ClipContent::Audio(AudioContent::default()), "baked".into());
        let clip = Clip { start_beat: 1.0, length_beats: 2.0, content_id, ..Clip::default() };
        let id = song.place_bounce_with_fx(ClipKey { track_id: src, clip_id: source_clip }, "FX".into(), clip).expect("placed");
        song.enforce_edit_invariants();

        let order: Vec<u32> = song.tracks.iter().map(|t| t.id).collect();
        assert_eq!(order, [group, src, id, sibling, ret, reader], "元トラックの直後 (同じ group の中)");
        let new = song.track_by_id(id).expect("new track");
        assert_eq!((new.volume, new.pan, new.muted, new.solo), (0.5, -0.4, false, true), "フェーダーは写す");
        assert_eq!(new.parent_group_id, Some(group), "行き先は同じ親 group");
        assert_eq!(new.sends, before.sends, "send は同じ id のまま");
        assert_eq!(new.clips.len(), 1);
        let lane_targets: Vec<_> = new.automation_lanes.iter().map(|l| (l.id, l.target.clone())).collect();
        assert_eq!(lane_targets, before.automation_lanes.iter().map(|l| (l.id, l.target.clone())).collect::<Vec<_>>(), "フェーダーのレーンは不変条件の後も残る");
        let copied = new.automation_lanes[0].clips[0].content_id;
        assert_ne!(copied, volume_content, "レーンの中身は独立に複製する");
        assert_eq!(song.clip_contents.get(&copied), song.clip_contents.get(&volume_content));
        let [pan, depth] = new.mod_routings.as_slice() else { panic!("フェーダーの変調とその深さだけを写す: {:?}", new.mod_routings) };
        assert_eq!(pan.target, AutomationTarget::TrackBuiltin(B::Pan));
        assert!(pan.id != pan_routing && pan.source_id == lfo, "変調は新しい id で同じソースを指す");
        assert_eq!(depth.target, AutomationTarget::ModRoutingDepth { routing_id: pan.id }, "深さは複製した変調を指し直す");

        let old = song.track_by_id(src).expect("src");
        let muted = |clip| old.clip_by_id(clip).expect("clip").muted;
        assert!(!old.muted && muted(source_clip) && !muted(other_clip), "元トラックは鳴り続け、焼いたクリップだけを mute する");
        assert_eq!((old.automation_lanes.clone(), old.mod_routings.clone(), old.sends.clone(), old.solo), (before.automation_lanes, before.mod_routings, before.sends, before.solo), "元トラックの中身は触らない");
        let tap = |dev| song.native_by_id(dev).and_then(|n| n.aux_input).map(|r| r.tap);
        assert_eq!(tap(post_fader_sc), Some(AudioTap::post_fader(src)), "元トラックは鳴り続けるので SC は付け替えない");
        assert_eq!(tap(post_fx_sc), Some(AudioTap::new(TapSource::Track(src), TapPoint::PostFx)));
        assert_eq!(song.mod_sources[0].follower().and_then(|(t, _)| t.copied()), Some(AudioTap::post_fader(src)));
    }
}
