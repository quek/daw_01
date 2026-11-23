//! 読み込みで安定 id の空間に余裕を取り戻す ([`Song::ensure_id_headroom`]、`normalize_after_load` の id 処理の先頭)。
//!
//! 採番 (`alloc_*_id`) は「counter を返して 1 進める」で、counter が型の上限に張り付くと毎回同じ id を返す
//! (`saturating_add`)。このアプリは 1 から 1 つずつ数えるので上限には届かないが、壊れた / 手で書き換えたファイルは
//! counter や使用中の id を上限の近くに置ける。ここで id 空間ごとに:
//!
//! 1. counter を「使用中の最大 + 1」以上へ上げる (遅れた counter は次の採番で使用中の id を返す)。
//! 2. 上げた counter が上限の半分を超えたら、その id を指すものが空間の中だけ (選択などの session 状態だけが
//!    指す) なら `1..=n` へ詰め直し、外 (別の id 空間・`ViewState`・プラグインのアーカイブ) から指されていれば
//!    読み込みを拒む ([`IdSpaceExhausted`])。外の参照は付け替える口が 1 つに集まっていないので、詰め直すと
//!    付け替え漏れが無言で別の物を指す。
//! 3. ただし空間に要素が 1 つも無ければ (使用中の id が無い)、counter が上限に近くても拒まず `1` へ戻す
//!    ([`EmptySpace`])。戻すのは dangling な参照の掃除 (`prune_dangling_refs` / `normalize_session`) の後の 2 回目
//!    (掃除の後も空なら、Song の中に指すものは残っていない)。プラグインのアーカイブ (ARA) は中を見られないので、
//!    アーカイブを持つ曲ではそれが指しうる空間 (クリップの中身・音声素材・音声の event) を戻さない。`ViewState`
//!    の表示の記録 (行の高さなど) は見方の都合なので、古い記録が新しい要素に付いても中身は壊れない。
//!
//! 後段の `ensure_*` (未採番 / 重複の採り直し) は counter が上限から遠いことを前提に数えるので、1 回目を先に通す。
//! 冪等 (2 回目は何も変えない — 開いただけで `*` を立てない)。コードの箱 (`chords`) は別の口
//! (`Song::compact_chord_ids_if_saturated`、`chord_ops.rs`) が詰め直す — 箱は系統とクリップの記録が一緒に
//! 付け替わるので、ここの「拒む / 詰め直す」の 2 択に乗らない。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use std::fmt;

use super::*;

/// 安定 id の整数型。
trait IdNum: Copy + Ord {
    /// counter がこれを超えた空間は上限に近すぎる (1 から 1 つずつ数えて届く数ではない)。
    const LIMIT: Self;
    /// 空の空間の counter を戻す先 (`0` は未採番 sentinel)。
    const FIRST: Self;
    fn succ(self) -> Self;
}

impl IdNum for u32 {
    const LIMIT: Self = u32::MAX / 2;
    const FIRST: Self = 1;
    fn succ(self) -> Self {
        self.saturating_add(1)
    }
}

impl IdNum for u64 {
    const LIMIT: Self = u64::MAX / 2;
    const FIRST: Self = 1;
    fn succ(self) -> Self {
        self.saturating_add(1)
    }
}

/// 要素が 1 つも無い空間の counter が上限に近いときの扱い (module doc の 3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptySpace {
    /// 拒まず、counter もそのまま (dangling な参照の掃除の前の 1 回目)。
    Defer,
    /// counter を `1` へ戻す (掃除の後の 2 回目)。
    Reset,
}

/// `counter` を `max_used + 1` 以上へ上げる (下げない)。上げた counter が上限を超えたら `true`。
fn raise<T: IdNum>(counter: &mut T, max_used: Option<T>) -> bool {
    if let Some(max) = max_used {
        *counter = (*counter).max(max.succ());
    }
    *counter > T::LIMIT
}

/// [`raise`] の、空の空間を拒まない版 (module doc の 3)。`archived` = 中を見られないプラグインのアーカイブが
/// この空間を指しうる (空でも戻さない)。拒むべきなら `true`。
fn settle<T: IdNum>(counter: &mut T, max_used: Option<T>, empty: EmptySpace, archived: bool) -> bool {
    if max_used.is_none() && *counter > T::LIMIT && !archived {
        if empty == EmptySpace::Reset {
            *counter = T::FIRST;
        }
        return false;
    }
    raise(counter, max_used)
}

/// id を並び順に `1..=n` へ振り直し、次の counter (`n + 1`) を返す。
fn renumber<'a>(ids: impl Iterator<Item = &'a mut u32>) -> u32 {
    let mut next = 1u32;
    for id in ids {
        *id = next;
        next = next.saturating_add(1);
    }
    next
}

/// 読み込みを拒む理由: 外から指される id 空間の番号が上限に近すぎる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdSpaceExhausted {
    /// 見せる名前 (「トラック」「トラック「Bass」のクリップ」…)。
    pub space: String,
}

impl fmt::Display for IdSpaceExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}の番号が上限に近すぎるため開けません (このアプリは番号を 1 から順に振るので、壊れたか手で書き換えたファイルです)",
            self.space
        )
    }
}

impl std::error::Error for IdSpaceExhausted {}

/// `hit` なら `space` の読み込みを拒む。
fn refuse_if(hit: bool, space: impl FnOnce() -> String) -> Result<(), IdSpaceExhausted> {
    if hit { Err(IdSpaceExhausted { space: space() }) } else { Ok(()) }
}

impl Song {
    /// 全 id 空間の counter を使用中の最大 + 1 以上へ上げ、上限に近い空間を詰め直す / 拒む / 空なら `empty` の
    /// とおりに扱う (module doc)。
    pub(crate) fn ensure_id_headroom(&mut self, empty: EmptySpace) -> Result<(), IdSpaceExhausted> {
        // ARA のアーカイブはクリップの中身・音声素材・音声の event の id を中に持つ (見られない)。
        let ara = self.all_plugins().any(|p| p.ara_archive.is_some());
        self.ensure_song_id_headroom(empty, ara)?;
        self.ensure_section_ids();
        for track in &mut self.tracks {
            track.ensure_id_headroom(empty)?;
        }
        for lane in &mut self.song_lanes {
            lane.ensure_clip_id_headroom("曲", empty)?;
        }
        for content in self.clip_contents.values_mut() {
            content.ensure_id_headroom(empty, ara)?;
        }
        Ok(())
    }

    /// `Song::ids` の空間 (セクションとコードの箱を除く)。どれも外から指される:
    /// トラック = 親 / センド先 / サイドチェイン / パラアウト / `ViewState`、デバイス = オートメーション /
    /// 変調 / MIDI 割り当ての宛先 / `ViewState`、クリップの中身と音声素材 = ARA のアーカイブの persistent id、
    /// 動画 / 画像素材 = event と口パク、レーン = `ViewState`、モジュレーターと変調 = 宛先、シーン = セル / 発火の記録 /
    /// follow action / MIDI 割り当て。
    fn ensure_song_id_headroom(&mut self, empty: EmptySpace, ara: bool) -> Result<(), IdSpaceExhausted> {
        let max_track = self.tracks.iter().map(|t| t.id).max();
        let mut max_device: Option<u64> = None;
        let mut see = |id: u64| max_device = max_device.max(Some(id));
        for track in &self.tracks {
            for_each_node_id(&track.devices, &mut see);
        }
        for_each_node_id(&self.master_fx_chain, &mut see);
        // 中身は clip が指すものに加えて、どの clip も指さない store / 名前の key も使用中 (再利用すると古い名前が付く)。
        let max_content = self
            .live_content_ids()
            .into_iter()
            .chain(self.clip_contents.keys().copied())
            .chain(self.clip_content_names.keys().copied())
            .max();
        // 素材は pool の key に加えて、pool に無い素材を指す event / 口パクも使用中 (再利用すると「素材なし」が別の素材で鳴る)。
        let live = self.live_source_ids();
        let max_audio = live.audio.iter().chain(self.media.audio_sources.keys()).copied().max();
        let max_video = live.video.iter().chain(self.media.video_sources.keys()).copied().max();
        let max_image = live.image.iter().chain(self.media.image_sources.keys()).copied().max();
        let max_song_lane = self.song_lanes.iter().map(|l| l.id).max();
        let max_mod_source = self.mod_sources.iter().map(|m| m.id).max();
        let max_mod_routing = self.all_mod_routings().map(|r| r.id).max();
        let max_scene = self.scenes.iter().map(|s| s.id).max();

        let ids = &mut self.ids;
        let e = empty;
        refuse_if(settle(&mut ids.next_track_id, max_track, e, false), || "トラック".into())?;
        refuse_if(settle(&mut ids.next_device_id, max_device, e, false), || "デバイス".into())?;
        refuse_if(settle(&mut ids.next_content_id, max_content, e, ara), || "クリップの中身".into())?;
        refuse_if(settle(&mut ids.next_audio_source_id, max_audio, e, ara), || "音声素材".into())?;
        refuse_if(settle(&mut ids.next_video_source_id, max_video, e, false), || "動画素材".into())?;
        refuse_if(settle(&mut ids.next_image_source_id, max_image, e, false), || "画像素材".into())?;
        refuse_if(settle(&mut ids.next_song_lane_id, max_song_lane, e, false), || "曲のオートメーションレーン".into())?;
        refuse_if(settle(&mut ids.next_mod_source_id, max_mod_source, e, false), || "モジュレーター".into())?;
        refuse_if(settle(&mut ids.next_mod_routing_id, max_mod_routing, e, false), || "変調の割り当て".into())?;
        refuse_if(settle(&mut ids.next_scene_id, max_scene, e, false), || "シーン".into())?;
        Ok(())
    }

    /// セクションの id: 指すものは選択などの session 状態だけなので、上限に近ければ並び順に詰め直す。そうでなければ
    /// 未採番 (`0`) と重複を採り直す (読み込みでセクションの id を正す口はここだけ)。
    fn ensure_section_ids(&mut self) {
        if raise(&mut self.ids.next_section_id, self.sections.iter().map(|s| s.id).max()) {
            self.ids.next_section_id = renumber(self.sections.iter_mut().map(|s| &mut s.id));
            return;
        }
        let mut seen = std::collections::HashSet::new();
        for s in &mut self.sections {
            if s.id == 0 || !seen.insert(s.id) {
                s.id = self.ids.next_section_id.max(1);
                self.ids.next_section_id = s.id + 1;
                seen.insert(s.id);
            }
        }
    }
}

impl Track {
    /// トラック内の空間。どれも外から指される: クリップ = `ViewState` の clip ごとの表示 / 行の主導権、
    /// レーン = `ViewState` の隠し / 行の高さ、センド = センド量のオートメーション / 変調の宛先。
    fn ensure_id_headroom(&mut self, empty: EmptySpace) -> Result<(), IdSpaceExhausted> {
        let max_clip = self.all_clips().map(|c| c.id).max();
        refuse_if(settle(&mut self.next_clip_id, max_clip, empty, false), || {
            format!("トラック「{}」のクリップ", self.name)
        })?;
        let max_lane = self.automation_lanes.iter().map(|l| l.id).max();
        refuse_if(settle(&mut self.next_lane_id, max_lane, empty, false), || {
            format!("トラック「{}」のオートメーションレーン", self.name)
        })?;
        let max_send = self.sends.iter().map(|s| s.id).max();
        refuse_if(settle(&mut self.next_send_id, max_send, empty, false), || {
            format!("トラック「{}」のセンド", self.name)
        })?;
        let owner = format!("トラック「{}」", self.name);
        for lane in &mut self.automation_lanes {
            lane.ensure_clip_id_headroom(&owner, empty)?;
        }
        Ok(())
    }
}

impl AutomationLane {
    /// レーン内のクリップ (`ViewState` のオートメーション編集の表示 / 行の主導権が指す)。
    fn ensure_clip_id_headroom(&mut self, owner: &str, empty: EmptySpace) -> Result<(), IdSpaceExhausted> {
        let max_clip = self.all_clips().map(|c| c.id).max();
        refuse_if(settle(&mut self.next_clip_id, max_clip, empty, false), || {
            format!("{owner}のオートメーションレーンのクリップ")
        })
    }
}

impl ClipContent {
    /// content 内の要素。ノートとオートメーションの点は選択などの session 状態だけが指すので詰め直す。音声の event は
    /// take ([`AudioEvent::take_id`] = 分割の片が継いだ元の event の id、元が消えても使用中) と ARA の audio modification
    /// の persistent id (プラグインのアーカイブの中) から指されるので拒む。
    fn ensure_id_headroom(&mut self, empty: EmptySpace, ara: bool) -> Result<(), IdSpaceExhausted> {
        match self {
            ClipContent::Midi(m) => {
                if raise(&mut m.next_note_id, m.notes.iter().map(|n| n.id).max()) {
                    m.next_note_id = renumber(m.notes.iter_mut().map(|n| &mut n.id));
                }
            }
            ClipContent::Automation(a) => {
                if raise(&mut a.next_point_id, a.points.iter().map(|p| p.id).max()) {
                    a.next_point_id = renumber(a.points.iter_mut().map(|p| &mut p.id));
                }
            }
            ClipContent::Audio(a) => {
                let max_used = a.events.iter().flat_map(|e| [e.id, e.take_id]).max();
                refuse_if(settle(&mut a.next_event_id, max_used, empty, ara), || "音声クリップのイベント".into())?;
            }
            // event に安定 id の allocator を持たない種類。
            ClipContent::Video(_) | ClipContent::Image(_) | ClipContent::Text(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn midi(ids: &[u32], next: u32) -> ClipContent {
        let notes = ids.iter().map(|&id| Note { id, duration_beats: 1.0, pitch: 60, ..Note::default() }).collect();
        ClipContent::Midi(MidiContent { notes, next_note_id: next })
    }

    fn song_with_track() -> Song {
        let mut song = Song::default();
        let id = song.alloc_track_id();
        song.tracks.push(Track { id, name: "Bass".into(), ..Track::default() });
        song
    }

    /// counter が使用中の id より遅れていたら上げる: 次の採番が使用中の id を返さない。
    #[test]
    fn load_raises_lagging_counters_above_ids_in_use() {
        let mut song = song_with_track();
        // どの clip も指さない中身 (名前だけ残る) と、pool に無い素材を指す event と、元の event が消えた take と、セクション。
        song.clip_content_names.insert(7, "古い名前".into());
        let take = AudioEvent { id: 1, take_id: 9, source_id: 5, ..AudioEvent::default() };
        song.clip_contents.insert(3, ClipContent::Audio(AudioContent { events: vec![take], next_event_id: 2 }));
        let clip = Clip { content_id: 3, length_beats: 4.0, ..Clip::default() };
        song.tracks[0].clips.push(clip);
        song.sections.push(Section { id: 4, name: "A".into(), color: [0.0; 3], start_beat: 0.0, len_beats: 4.0 });
        song.ids.next_content_id = 1;
        song.ids.next_audio_source_id = 1;
        song.ids.next_section_id = 1;

        assert_eq!(song.normalize_after_load(), Ok(false));
        assert_eq!(song.alloc_content_id(), 8, "名前だけ残る中身の id を再利用しない");
        assert_eq!(song.alloc_audio_source_id(), 6, "event が指す (pool に無い) 素材の id を再利用しない");
        assert_eq!(song.alloc_section_id(), 5);
        let Some(ClipContent::Audio(a)) = song.clip_contents.get_mut(&3) else { panic!("audio content") };
        assert_eq!(a.alloc_event_id(), 10, "take が指す (消えた) event の id を再利用しない");

        let once = song.clone();
        assert_eq!(song.normalize_after_load(), Ok(false));
        assert_eq!(song, once, "2 回目の正規化で Song が変わると『開いただけで *』になる");
    }

    /// 外から指されない空間 (ノート / オートメーションの点 / セクション) は、上限に近ければ詰め直す。
    #[test]
    fn load_renumbers_saturated_spaces_nothing_outside_points_at() {
        let mut song = song_with_track();
        song.clip_contents.insert(1, midi(&[u32::MAX - 1, u32::MAX], u32::MAX));
        let points = [u32::MAX, 3]
            .iter()
            .map(|&id| AutomationPoint { id, ..AutomationPoint::default() })
            .collect();
        song.clip_contents.insert(2, ClipContent::Automation(AutomationContent { points, next_point_id: 0 }));
        for (id, start) in [(u32::MAX, 0.0), (u32::MAX - 5, 4.0)] {
            song.sections.push(Section { id, name: "S".into(), color: [0.0; 3], start_beat: start, len_beats: 4.0 });
        }
        for (content_id, start) in [(1, 0.0), (2, 4.0)] {
            song.tracks[0].clips.push(Clip { content_id, start_beat: start, length_beats: 4.0, ..Clip::default() });
        }

        assert_eq!(song.normalize_after_load(), Ok(false));
        let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&1) else { panic!("midi content") };
        assert_eq!(m.notes.iter().map(|n| n.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!((m.alloc_note_id(), m.alloc_note_id()), (3, 4), "詰め直した後は重ならない id を出し続ける");
        let Some(ClipContent::Automation(a)) = song.clip_contents.get(&2) else { panic!("automation content") };
        let mut point_ids: Vec<u32> = a.points.iter().map(|p| p.id).collect();
        point_ids.sort_unstable();
        assert_eq!((point_ids, a.next_point_id), (vec![1, 2], 3));
        let mut section_ids: Vec<u32> = song.sections.iter().map(|s| s.id).collect();
        section_ids.sort_unstable();
        assert_eq!(section_ids, [1, 2]);
        assert_eq!((song.alloc_section_id(), song.alloc_section_id()), (3, 4));

        let once = song.clone();
        assert_eq!(song.normalize_after_load(), Ok(false));
        assert_eq!(song, once, "詰め直しは 1 度だけ (2 回目は何も変えない)");
    }

    /// 外から指される空間が上限に近ければ読み込みを拒む。ファイルの読み込み (`project::load`) で、
    /// 理由が日本語で出るところまで確かめる。
    #[test]
    fn load_refuses_saturated_spaces_pointed_at_from_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("saturated.daw");
        let mut song = song_with_track();
        song.tracks[0].id = u32::MAX - 1;
        crate::project::save(&path, &song).expect("save");
        let err = crate::project::load(&path).expect_err("トラックの番号が上限に近いファイルは開かない");
        assert_eq!(
            err.to_string(),
            "トラックの番号が上限に近すぎるため開けません (このアプリは番号を 1 から順に振るので、壊れたか手で書き換えたファイルです)"
        );

        let mut song = song_with_track();
        let plugin = PluginInstance::new("x".into(), crate::plugin_format::PluginFormat::Clap);
        song.master_fx_chain.push(Device::Plugin(PluginInstance { id: u64::MAX, ..plugin }));
        assert_eq!(song.normalize_after_load(), Err(IdSpaceExhausted { space: "デバイス".into() }));

        let mut song = song_with_track();
        let dest = song.tracks[0].id;
        song.tracks[0].sends.push(Send { id: 1, dest_track_id: dest, gain: 1.0, mode: SendMode::default(), enabled: true });
        song.tracks[0].next_send_id = u32::MAX;
        assert_eq!(
            song.normalize_after_load(),
            Err(IdSpaceExhausted { space: "トラック「Bass」のセンド".into() }),
            "使用中の id が小さくても、counter が上限に近ければ同じ"
        );
    }

    /// 要素が 1 つも無い空間は、counter が上限に近くても拒まず `1` へ戻す (指すものが無いので詰め直す物も無い)。
    /// 消えたトラックを指していた dangling な参照 (MIDI 割り当て) は掃除されてから戻す。ARA のアーカイブを持つ曲では、
    /// アーカイブが中で指しうる空間 (クリップの中身) は空でも拒む。
    #[test]
    fn load_resets_saturated_counters_of_empty_spaces() {
        let mut song = song_with_track();
        song.tracks[0].next_send_id = u32::MAX;
        song.tracks[0].next_clip_id = u32::MAX;
        song.ids.next_scene_id = u32::MAX;
        song.ids.next_mod_routing_id = u32::MAX;
        assert_eq!(song.normalize_after_load(), Ok(false));
        let t = &song.tracks[0];
        assert_eq!((t.next_send_id, t.next_clip_id, song.ids.next_scene_id, song.ids.next_mod_routing_id), (1, 1, 1, 1));
        let once = song.clone();
        assert_eq!(song.normalize_after_load(), Ok(false));
        assert_eq!(song, once, "冪等");

        let mut song = Song::default();
        song.tracks.clear();
        song.ids.next_track_id = u32::MAX;
        let target = BindingTarget::TrackVolume(u32::MAX - 1);
        song.midi_bindings.push(MidiBinding { channel: 0, input: MidiBindInput::ControlChange(1), legacy_controller: None, target });
        assert_eq!(song.normalize_after_load(), Ok(false), "トラックが 1 本も無ければトラックの番号も戻す");
        assert_eq!(song.ids.next_track_id, 1);
        assert!(song.midi_bindings.is_empty(), "消えたトラックを指す割り当ては、番号を戻す前に掃除される");

        let mut song = song_with_track();
        song.master_fx_chain.push(Device::Plugin(PluginInstance {
            id: 1,
            ara_archive: Some(std::sync::Arc::from(&b"ara"[..])),
            ..PluginInstance::new("x".into(), crate::plugin_format::PluginFormat::Clap)
        }));
        song.ids.next_content_id = u32::MAX;
        assert_eq!(
            song.normalize_after_load(),
            Err(IdSpaceExhausted { space: "クリップの中身".into() }),
            "ARA のアーカイブが中で指しうる空間は空でも戻さない"
        );
    }
}
