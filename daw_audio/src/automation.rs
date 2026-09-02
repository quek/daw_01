//! Audio-side automation playback. Walks the song's automation lanes,
//! evaluates curves at sample resolution, and fills the per-track
//! `volume_per_sample` / `pan_per_sample` ramps for the buffer.
//!
//! Phase 1: track-builtin Volume / Pan only. Plugin parameter
//! automation generates `clap_event_param_value` events through the
//! plugin host and is wired in Phase 2 (`docs/plan_automation.md`).
//!
//! RT-safe: no allocation, no I/O, no locking. Reads `Song` through a
//! shared reference (`ArcSwap<Song>` already loaded by the caller).

#![allow(dead_code)]

use common::automation::apply_modulation_with;
use common::mod_plane::ModTickPlaneRef;
use common::model::{
    AutomationTarget, ChannelStrip, MasterStrip, Song, Track, TrackBuiltinParam,
};
use common::process_data::ProcessData;

use crate::launcher::TrackRows;
use crate::launcher::render::{lane_value, phase_at_frame};

/// Fill `volume_per_sample` / `pan_per_sample` (each at least `frames`
/// long, but typically `MAX_FRAMES`) for the given track and buffer.
///
/// Default fill: each sample gets `track.volume` / `track.pan` as a
/// constant. Each enabled `Volume` / `Pan` lane then overwrites its
/// target buffer with the curve value sampled at every sample position.
///
/// `bpm == 0` or `sample_rate == 0` short-circuits to the constant
/// fallback (defensive — the engine starts in this state during init).
///
/// Lanes targeting plugin parameters are silently skipped: those are
/// converted into `TimedParamEvent`s elsewhere (Phase 2).
///
/// Phase 4 Step C-2: `recording_lanes` に `(track_id, lane.target)` が含まれて
/// いる lane は curve eval を **skip** し、 track.volume / track.pan の constant
/// fallback がそのまま buffer に残る。 これで GUI 側の knob 操作 (=
/// SetTrackVolume / SetTrackPan IPC) が即時に audio に反映される (Live /
/// Bitwig の Touch / Latch / Write の "you hear what you do" UX)。 set は
/// audio thread が buffer 頭で `load()` する snapshot (ArcSwap、 lock-free)。
#[allow(clippy::too_many_arguments)]
pub fn fill_track_param_ramps(
    song: Option<&Song>,
    track_idx: u32,
    // r.md #87: この track の行 (トラック行 + レーン行) の供給元。レーン行が
    // ランチャー主導ならそのセルのカーブを、停止していれば `lane.default_value` を
    // 出す (Q11)。空 (`TrackRows::default()`) なら全部アレンジ = 従来の挙動。
    rows: TrackRows<'_>,
    sample_rate: u32,
    // M5 (r.md #8 再監査): automation lookup beat は transport の積分済
    // `playhead_beats` を anchor に、 buffer 内は `current_bpm` で advance する
    // (SongTempo automation を尊重、 A10 と同経路)。 定 tempo では従来の
    // `(playhead + i)/samples_per_beat` と bit 同一。
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    volume_per_sample: &mut [f32],
    pan_per_sample: &mut [f32],
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
    // docs/plan_modulation.md §5 / r.md #89: **刻みごとの**変調値面
    // (`ModSource::id` キー)。volume/pan lane の `mod_routings` がこれを引く。
    // 刻みの間は線形補間する — 64 サンプルの段 (48kHz で 750Hz) をそのまま
    // 音量に当てると段差が音として出る。空 = 変調なし。
    mod_plane: ModTickPlaneRef<'_>,
) {
    let frames = (frames as usize).min(volume_per_sample.len()).min(pan_per_sample.len());
    if frames == 0 {
        return;
    }
    let (track_volume, track_pan) = song
        .and_then(|s| s.tracks.get(track_idx as usize))
        .map(|t| (t.volume, t.pan))
        .unwrap_or((1.0, 0.0));
    for slot in volume_per_sample.iter_mut().take(frames) {
        *slot = track_volume;
    }
    for slot in pan_per_sample.iter_mut().take(frames) {
        *slot = track_pan;
    }

    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else {
        return;
    };
    let track_id = track.id;
    if current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let beats_per_frame = current_bpm / (60.0 * f64::from(sample_rate));
    if beats_per_frame <= 0.0 {
        return;
    }

    // docs/plan_modulation_routing_redesign.md §3.1: Volume / Pan は lane の有無に
    // 関わらず変調する。base = 「enabled かつ非 recording な lane があればその curve
    // 値、無ければ track の constant 値」、そこに `Track.mod_routings` の当該 target
    // 変調を正規化領域で乗せる。lane も mod_routing も無い target は constant fill の
    // ままで正しいので per-sample ループを丸ごと skip (= 無回帰)。
    let fill_builtin = |target: AutomationTarget, buf: &mut [f32], track_const: f32| {
        // 当該 target を駆動する lane (enabled + 非 recording)。
        let lane = track.automation_lanes.iter().enumerate().find(|(_, l)| {
            l.enabled
                && l.target == target
                && !recording_lanes
                    .iter()
                    .any(|(t, tg)| *t == track_id && *tg == l.target)
        });
        let has_mod = track.mod_routings.iter().any(|r| r.target == target);
        if lane.is_none() && !has_mod {
            // constant fill (上で書いた track.volume / track.pan) がそのまま正しい。
            return;
        }
        // r.md #87: このレーン行の供給元。`switch_frame` を跨ぐと途中で変わる。
        let src = lane.map(|(li, _)| rows.lane(li)).unwrap_or_default();
        for (i, slot) in buf.iter_mut().enumerate().take(frames) {
            let beat = playhead_beats + i as f64 * beats_per_frame;
            let base = match lane {
                #[allow(clippy::cast_possible_truncation)]
                Some((_, l)) => {
                    let phase = phase_at_frame(src, i as u32);
                    lane_value(l, &song.clip_contents, phase, beat)
                }
                None => f64::from(track_const),
            };
            #[allow(clippy::cast_possible_truncation)]
            let f = i as u32;
            // r.md #89 Q9: 深さ自体が動く変調も刻みごとに解決する。
            *slot = apply_modulation_with(
                &target,
                base,
                &track.mod_routings,
                |id| mod_plane.scalar_at_frame(id, f),
                |r| mod_plane.depth_at_frame(r.id, f).unwrap_or(r.depth),
            ) as f32;
        }
    };
    fill_builtin(
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
        volume_per_sample,
        track_volume,
    );
    fill_builtin(
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan),
        pan_per_sample,
        track_pan,
    );
}

/// この buffer で実際に効く **チャンネルストリップ設定**を解決する
/// (`docs/plan_channel_strip.md` §7)。
///
/// 出発点は `track.strip` の静的値。そこへ (1) 有効なオートメーションレーンの
/// カーブ値、(2) `mod_routings` の変調 を順に重ねる。target ↔ フィールドの対応は
/// `ChannelStrip::{target_value, set_target_value}` が SSoT なので、ここは
/// 「どの target を、どの順で解決するか」だけを持つ。
///
/// **block-rate (buffer 先頭で 1 回)**。EQ 係数とコンプの時定数は buffer ごとに
/// 組み直すので、サンプル単位のランプは要らない (音量 / パンと違い、係数を
/// サンプルごとに引き直す意味が無い)。
///
/// RT 安全: 確保・ロックなし。`ChannelStrip` は `Copy` の値型。
pub fn resolve_track_strip(
    song: &Song,
    track: &Track,
    rows: TrackRows<'_>,
    playhead_beats: f64,
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
    mod_plane: ModTickPlaneRef<'_>,
) -> ChannelStrip {
    let mut strip = track.strip;
    if track.automation_lanes.is_empty() && track.mod_routings.is_empty() {
        return strip;
    }

    // (1) レーン: 有効かつ録音中でないものだけがカーブ値で上書きする
    // (録音中は GUI のノブ操作を素通しさせる = fill_track_param_ramps と同じ規則)。
    for (li, lane) in track.automation_lanes.iter().enumerate() {
        let AutomationTarget::TrackBuiltin(param) = &lane.target else {
            continue;
        };
        if !lane.enabled || strip.target_value(param).is_none() {
            continue;
        }
        if recording_lanes.iter().any(|(t, tg)| *t == track.id && *tg == lane.target) {
            continue;
        }
        let phase = phase_at_frame(rows.lane(li), 0);
        let v = lane_value(lane, &song.clip_contents, phase, playhead_beats);
        #[allow(clippy::cast_possible_truncation)]
        strip.set_target_value(param, v as f32);
    }

    // (2) 変調: base は (1) まで解決済みの現在値。`apply_modulation_with` は
    // 同じ target の routing を内部で全部畳むので、target ごとに 1 度だけ呼ぶ
    // (同じ target の 2 本目以降は skip)。
    for (i, routing) in track.mod_routings.iter().enumerate() {
        let AutomationTarget::TrackBuiltin(param) = &routing.target else {
            continue;
        };
        let Some(base) = strip.target_value(param) else {
            continue;
        };
        if track.mod_routings[..i].iter().any(|q| q.target == routing.target) {
            continue;
        }
        let v = apply_modulation_with(
            &routing.target,
            f64::from(base),
            &track.mod_routings,
            |id| mod_plane.scalar_at_frame(id, 0),
            |r| mod_plane.depth_at_frame(r.id, 0).unwrap_or(r.depth),
        );
        #[allow(clippy::cast_possible_truncation)]
        strip.set_target_value(param, v as f32);
    }
    strip
}

/// この buffer で実際に効く **マスターストリップ設定**を解決する
/// (`docs/plan_master_strip.md` §5)。
///
/// master には `Track` が無いので、レーンは `song.song_lanes`、変調は
/// `song.song_mod_routings` から引く (master fx の param automation と同じ store)。
/// 構造は [`resolve_track_strip`] と同じ block-rate 解決で、段階式パラメータの
/// 丸めは `MasterStrip::set_param` が担う。
///
/// RT 安全: 確保・ロックなし。`MasterStrip` は `Copy` の値型。
pub fn resolve_master_strip(
    song: &Song,
    rows: TrackRows<'_>,
    playhead_beats: f64,
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
    mod_plane: ModTickPlaneRef<'_>,
) -> MasterStrip {
    let mut strip = song.master_strip;
    if song.song_lanes.is_empty() && song.song_mod_routings.is_empty() {
        return strip;
    }
    let master = common::model::MASTER_TRACK_ID;

    for (li, lane) in song.song_lanes.iter().enumerate() {
        let AutomationTarget::MasterStrip(param) = &lane.target else {
            continue;
        };
        if !lane.enabled {
            continue;
        }
        if recording_lanes.iter().any(|(t, tg)| *t == master && *tg == lane.target) {
            continue;
        }
        let phase = phase_at_frame(rows.lane(li), 0);
        let v = lane_value(lane, &song.clip_contents, phase, playhead_beats);
        #[allow(clippy::cast_possible_truncation)]
        strip.set_param(*param, v as f32);
    }

    for (i, routing) in song.song_mod_routings.iter().enumerate() {
        let AutomationTarget::MasterStrip(param) = &routing.target else {
            continue;
        };
        if song.song_mod_routings[..i].iter().any(|q| q.target == routing.target) {
            continue;
        }
        let base = strip.param(*param);
        let v = apply_modulation_with(
            &routing.target,
            f64::from(base),
            &song.song_mod_routings,
            |id| mod_plane.scalar_at_frame(id, 0),
            |r| mod_plane.depth_at_frame(r.id, 0).unwrap_or(r.depth),
        );
        #[allow(clippy::cast_possible_truncation)]
        strip.set_param(*param, v as f32);
    }
    strip
}

/// Phase 2b (`docs/plan_automation.md` §8.3): push automation events for
/// the specified device (v29: 安定 device id `PluginInstance::id` で指定)
/// into `pd.events_in` as `EventKind::ParamValue` entries.
/// plugin_host's `process_server` decodes them into
/// `TimedParamEvent` and forwards to `LoadedPlugin::process(..,
/// param_events, ..)` which converts them to CLAP `clap_event_param_value`
/// / VST3 `IParameterChanges`.
///
/// Phase 2 では「1 buffer = 1 update」 (frame 0 でのみ curve 値を 1 度
/// push) として簡素実装。 frame 単位 sample (= 64 frame 刻みで複数
/// push) は Phase 3+ でカーブの滑らかさが必要になったときに拡張。
///
/// RT 安全性: `push_param` は固定 capacity の `events_in` 配列に書く
/// だけ、 allocation なし。 `lane_value_at` は curve evaluator のみ
/// (allocation なし、 浮動小数演算のみ)。
#[allow(clippy::too_many_arguments)]
pub fn fill_pd_param_events(
    pd: &mut ProcessData,
    song: &Song,
    track_id: u32,
    // r.md #87: この track の行の供給元 (master fx は行を持たないので
    // `TrackRows::default()` = 全部アレンジ)。
    rows: TrackRows<'_>,
    device_id: u64,
    sample_rate: u32,
    // M5 (r.md #8 再監査): transport の積分済 `playhead_beats` を anchor に
    // buffer 内は `current_bpm` で advance (SongTempo automation 尊重、 A10 同経路)。
    // track/group/master 全経路で同じ引数を渡すので、 旧実装の「master=current_bpm /
    // track・group=song.bpm」 の不一致も解消。 定 tempo では従来式と bit 同一。
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
    // docs/plan_modulation.md §5 / r.md #89: **刻みごとの**変調値面 (id キー)。
    // PluginParam lane の `mod_routings` がこれを引く。空 = 変調なし。
    mod_plane: ModTickPlaneRef<'_>,
) {
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    // master fx (`MASTER_TRACK_ID`) は Track ではないので automation は `song_lanes`、
    // 変調は `song_mod_routings` (Song 直下の song/master-level store) から引く。 それ
    // 以外は通常 track。 `song_lanes` に混在する SongTempo/TimeSig lane は下の
    // PluginParam フィルタで自然に skip される。 (r.md #8 再監査: master fx 自動化/変調)
    let (lanes, mod_routings): (
        &[common::model::AutomationLane],
        &[common::model::ModRouting],
    ) = if track_id == common::model::MASTER_TRACK_ID {
        (&song.song_lanes, &song.song_mod_routings)
    } else {
        let Some(track) = song.tracks.iter().find(|t| t.id == track_id) else {
            return;
        };
        (&track.automation_lanes, &track.mod_routings)
    };
    let beats_per_frame = current_bpm / (60.0 * f64::from(sample_rate));
    if beats_per_frame <= 0.0 {
        return;
    }
    for (lane_idx, lane) in lanes.iter().enumerate() {
        if !lane.enabled {
            continue;
        }
        // Phase 4 Step C-2 (plugin param 版): recording 中 (Touch/Latch/Write)
        // の lane は curve eval を skip する。 これで plugin が自身の GUI で
        // 持っている値 (= ユーザのノブ操作) を host が curve で毎バッファ
        // 上書きするのを止め、「you hear what you do」 を成立させる。 track
        // builtin Volume/Pan の `fill_track_param_ramps` と同じ仕組みだが、
        // 旧実装は plugin param 側にこの skip が無く、 write が read のまま /
        // touch が半分しか効かないバグだった。
        if recording_lanes
            .iter()
            .any(|(t, tg)| *t == track_id && *tg == lane.target)
        {
            continue;
        }
        let param_id = match &lane.target {
            AutomationTarget::PluginParam { device_id: d, param_id, .. }
                if *d == device_id =>
            {
                *param_id
            }
            _ => continue,
        };
        // automation curve 値 (絶対値)。モジュレーションは下で正規化オフセットを
        // ParamMod として別送する (`docs/plan_modulation_routing_redesign.md` §3.2)
        // ので、CLAP modulatable param では automation を破壊せず非破壊に乗る。
        //
        // B4 (r.md #8): sub-buffer (64 frame 刻み) で curve をサンプルし、 値が変わる
        // たびに frame offset 付きで push_param する (= sample-accurate)。 旧実装は
        // frame 0 の 1 回のみで、 速い automation が階段状 (zipper) になっていた。
        // 静的セグメントは値不変なので 1 event に縮退 (events_in=256 を無駄に食わない)。
        // push_param は満杯時 drop のみ (panic なし) なので RT 安全。
        //
        // r.md #89: 刻み幅の SSoT は `crate::mod_tick::MOD_TICK_FRAMES`。automation の
        // サブバッファ刻みと変調の制御グリッドは **同じ格子でなければならない**
        // (設計正本 §2.2) — 別々の 64 を持つと、片方を変えたときに黙って食い違う。
        const SUB_FRAMES: u32 = crate::mod_tick::MOD_TICK_FRAMES;
        // r.md #87: このレーン行の供給元 (ランチャー主導ならセルのカーブ)。
        let src = rows.lane(lane_idx);
        let mut f = 0u32;
        let mut last_v = f64::NAN;
        loop {
            let beat_at_f = playhead_beats + f64::from(f) * beats_per_frame;
            let v = lane_value(lane, &song.clip_contents, phase_at_frame(src, f), beat_at_f);
            if last_v.is_nan() || (v - last_v).abs() > 1e-6 {
                pd.push_param(f, param_id, v);
                last_v = v;
            }
            if f + SUB_FRAMES >= frames {
                break;
            }
            f += SUB_FRAMES;
        }
    }

    // docs/plan_modulation_routing_redesign.md §3.2: この device の plugin param を
    // 変調する routing があれば、target ごとに正規化オフセット 1 個を `ParamMod` で
    // 送る。**lane の有無に関わらず** (= lane-free モジュレーション)。plugin_host が
    // per-format に CLAP `param_mod` / 合成へ変換する。follower が 0 に戻った時も
    // offset 0 を送って mod を解除するため、毎バッファ無条件に emit する。
    // r.md #89: **溢れさせない**。`param_mods` はリングなので、満杯だと古い側が
    // 落ちる。積む順は param ごと (外) × 刻み (内) なので、落ちるのは「先頭 param の
    // 全刻み」= その param に 1 件も届かず、前 buffer の offset が解除されないまま
    // 居座る (毎 buffer 同じ順で溢れるので永久に戻らない)。件数が枠を超えるときは
    // **刻みを間引いて解像度を落とす** — どの param にも必ず最後の刻みが届く。
    let n_ticks = mod_plane.starts(frames).count().max(1);
    let n_params = mod_routings
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            matches!(&r.target, AutomationTarget::PluginParam { device_id: d, .. } if *d == device_id)
                && !mod_routings[..*i].iter().any(|p| p.target == r.target)
        })
        .count()
        .max(1);
    let budget = (common::process_data::MAX_PARAM_MODS / n_params).max(1);
    let stride = n_ticks.div_ceil(budget).max(1);
    for (i, r) in mod_routings.iter().enumerate() {
        let AutomationTarget::PluginParam { device_id: d, param_id, .. } = &r.target else {
            continue;
        };
        if *d != device_id {
            continue;
        }
        // 同一 target は 1 度だけ (先行する同 target routing があれば skip)。
        if mod_routings[..i].iter().any(|p| p.target == r.target) {
            continue;
        }
        // r.md #89: **刻みごとに** frame offset 付きで送る。1 buffer 1 発だと
        // 変調の解像度が buffer 長 (≒46Hz) に落ち、Hz 指定の LFO が原理的に
        // 鳴らないうえ live (device buffer 長) と書き出し (1024 固定) で段差の
        // 位置が違う。値が変わらない刻みは縮退させる (automation 側の
        // `push_param` と同じ idiom — `param_mods` の枠を無駄に食わない)。
        let mut last = f64::NAN;
        for (t, f) in mod_plane.starts(frames).enumerate() {
            // 間引くのは中間の刻みだけ。**最後の刻みは必ず送る** (解除されない
            // offset が居座らないように)。
            if t % stride != 0 && t + 1 != n_ticks {
                continue;
            }
            // r.md #89 Q9: 深さ自体が動く変調は面が持つ実効値を使う。
            let offset = f64::from(common::automation::modulation_offset_norm_with(
                &r.target,
                mod_routings,
                |id| mod_plane.scalar_at_frame(id, f),
                |rr| mod_plane.depth_at_frame(rr.id, f).unwrap_or(rr.depth),
            ));
            if last.is_nan() || (offset - last).abs() > 1e-6 {
                pd.push_param_mod(f, *param_id, offset);
                last = offset;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の薄いラッパ: 全行アレンジ (= ランチャー無し) で従来の経路を通す。
    /// ランチャー行の評価は `crate::launcher::render` 側でテストする。
    #[allow(clippy::too_many_arguments)]
    fn ramps(
        song: Option<&Song>,
        track_idx: u32,
        sample_rate: u32,
        current_bpm: f64,
        playhead_beats: f64,
        frames: u32,
        volume_per_sample: &mut [f32],
        pan_per_sample: &mut [f32],
        recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
        mod_scalars: &[f32],
    ) {
        let ids: Vec<u32> = song
            .map(|s| s.mod_sources.iter().map(|m| m.id).collect())
            .unwrap_or_default();
        let mod_plane = ModTickPlaneRef::new(&ids, mod_scalars, 64);
        fill_track_param_ramps(
            song,
            track_idx,
            TrackRows::default(),
            sample_rate,
            current_bpm,
            playhead_beats,
            frames,
            volume_per_sample,
            pan_per_sample,
            recording_lanes,
            mod_plane,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn pd_params(
        pd: &mut ProcessData,
        song: &Song,
        track_id: u32,
        device_id: u64,
        sample_rate: u32,
        current_bpm: f64,
        playhead_beats: f64,
        frames: u32,
        recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
        mod_scalars: &[f32],
    ) {
        let ids: Vec<u32> = song.mod_sources.iter().map(|m| m.id).collect();
        let mod_plane = ModTickPlaneRef::new(&ids, mod_scalars, 64);
        fill_pd_param_events(
            pd,
            song,
            track_id,
            TrackRows::default(),
            device_id,
            sample_rate,
            current_bpm,
            playhead_beats,
            frames,
            recording_lanes,
            mod_plane,
        );
    }
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane,
        AutomationPoint, ClipContent, Song, Track,
    };

    /// Helper: build a song with one track owning a single automation
    /// lane that ramps `Volume` from 0.0 → 1.0 across beats 0..4.
    fn one_volume_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 1.0,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "vol".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.5,
            )
        };
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.volume = 0.5;
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));
        song
    }

    /// 120 BPM at 48 kHz → 24000 samples/beat.
    const SR: u32 = 48000;

    /// Phase 4 Step C-2: 既存テストは recording 中ではないので、 共通 helper
    /// で empty set を borrow する。
    fn empty_recording_lanes()
    -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        std::collections::HashSet::new()
    }

    /// v23 single-chain: `Track` の `legacy_*` migration fields は `common`
    /// crate に `pub(crate)` で閉じているため、 downstream crate (daw_audio)
    /// の test では `Track { .., ..Track::default() }` の functional-update が
    /// E0451 になる。 `Track::default()` から組んで mutator で埋める helper で
    /// 回避する (private field に触れずに済む)。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    #[test]
    fn no_song_falls_back_to_unity_volume_zero_pan() {
        let mut vol = vec![0.0_f32; 8];
        let mut pan = vec![0.5_f32; 8];
        let empty = empty_recording_lanes();
        ramps(None, 0, SR, 120.0, 0.0, 8, &mut vol, &mut pan, &empty, &[]);
        assert!(vol.iter().all(|&v| (v - 1.0).abs() < 1e-6));
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn no_lanes_fills_with_track_strip_constants() {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.volume = 0.7;
            t.pan = -0.25;
        }));
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            16,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        assert!(vol.iter().all(|&v| (v - 0.7).abs() < 1e-6));
        assert!(pan.iter().all(|&p| (p - -0.25).abs() < 1e-6));
    }

    #[test]
    fn volume_lane_ramps_across_buffer() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        let empty = empty_recording_lanes();
        // Buffer of 16 samples starting at playhead 0 → first 16 samples
        // out of 24000 samples-per-beat × 4 = 96000 total. Volume should
        // ramp from 0.0 toward ~16/96000 ≈ 0.000167.
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            16,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        assert!(vol[0].abs() < 1e-6);
        assert!(vol[15] > 0.0 && vol[15] < 0.001, "vol[15]={}", vol[15]);
        // Pan untouched (no Pan lane) → falls back to track.pan = 0.
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn buffer_at_clip_midpoint_returns_half() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        // Beat 2.0 = 48000 samples in. Curve linear 0→1 over beats 0..4
        // → value 0.5 at beat 2.
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected ~0.5, got {}", v);
        }
    }

    #[test]
    fn disabled_lane_uses_default_value() {
        let mut song = one_volume_lane_song();
        song.tracks[0].automation_lanes[0].enabled = false;
        // default_value is 0.5 from one_volume_lane_song.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        // Bypass returns the lane's default — but `fill_track_param_ramps`
        // only writes the lane buffer when `enabled = true`. With the
        // lane disabled the constant fallback (track.volume = 0.5)
        // remains, which happens to equal the default in this test.
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn outside_clip_range_uses_default_value() {
        let song = one_volume_lane_song();
        // Buffer at beat 10 (sample 240_000) — clip ends at beat 4.
        // lane_value_at returns lane.default_value = 0.5.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            10.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6, "got {}", v);
        }
    }

    /// Phase 4 Step C-2: `recording_lanes` に含まれる lane は curve eval を
    /// skip し、 track.volume の constant が残るべき (= live knob 値で audio
    /// が鳴る挙動の基盤)。 track.volume を 0.9、 curve は beat 2.0 で 0.5 に
    /// なる構成にし、 bypass で vol[i] が 0.9 になることを確認する。
    #[test]
    fn recording_lane_bypasses_curve_eval() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9; // 区別のため curve mid (0.5) と違う値に
        let track_id = song.tracks[0].id;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let mut recording = std::collections::HashSet::new();
        recording.insert((
            track_id,
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ),
        ));
        // Beat 2.0 の curve eval は 0.5 だが、 recording bypass で track.volume
        // (= 0.9) がそのまま残る。
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &recording,
            &[],
        );
        for &v in vol.iter() {
            assert!(
                (v - 0.9).abs() < 1e-6,
                "expected track.volume bypass (0.9), got {}",
                v
            );
        }
    }

    /// Phase 4 Step C-2: recording set にない lane は通常通り curve eval する。
    /// recording bypass test と pair の sanity check (= bypass が必要なときだけ
    /// 効いて、 不要なときは無回帰)。
    #[test]
    fn non_recording_lane_still_uses_curve() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        // Beat 2.0 の curve eval は 0.5、 bypass されないので vol[i] = 0.5。
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected curve eval (0.5), got {}", v);
        }
    }

    /// One track (id 7) with a single `PluginParam` (device_id 40, param 5)
    /// automation lane ramping 0.25 → 0.75 over beats 0..4.
    const DEVICE_ID: u64 = 40;

    /// レーンとノブ (静的値) の優先順位: 有効なレーンがあればカーブ値が勝ち、
    /// 録音中はノブが素通しになる (= `fill_track_param_ramps` と同じ規則)。
    #[test]
    fn ストリップのレーンはカーブ値で上書きし録音中は素通しする() {
        use common::model::{CompParam, EqBand, EqParam};

        let target =
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::StripComp { param: CompParam::Threshold });
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![AutomationPoint {
                    id: 1,
                    time_beat: 0.0,
                    // 正規化 0.5 = -30dB (Threshold は -60..0 の線形)。
                    value: -30.0,
                    curve: AutomationCurve::Linear,
                }],
                next_point_id: 2,
            }),
        );
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "thr".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target.clone(), -30.0)
        };
        song.tracks.push(track(|t| {
            t.id = 1;
            t.strip.comp.on = true;
            t.strip.comp.threshold_db = -6.0;
            // EQ の値はレーンが無いので静的値のまま残ること (巻き添え確認)。
            t.strip.eq.hmf.gain_db = 4.0;
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));

        let empty = empty_recording_lanes();
        let resolved = resolve_track_strip(
            &song,
            &song.tracks[0],
            TrackRows::default(),
            0.0,
            &empty,
            ModTickPlaneRef::default(),
        );
        assert!((resolved.comp.threshold_db - -30.0).abs() < 1e-4, "{}", resolved.comp.threshold_db);
        assert!(
            (resolved.eq.param(EqBand::Hmf, EqParam::Gain) - 4.0).abs() < 1e-6,
            "レーンの無い param まで書き換わっている"
        );

        // 録音中 (= ノブを掴んでいる) はカーブを評価せず静的値が残る。
        let mut recording = std::collections::HashSet::new();
        recording.insert((1_u32, target));
        let touched = resolve_track_strip(
            &song,
            &song.tracks[0],
            TrackRows::default(),
            0.0,
            &recording,
            ModTickPlaneRef::default(),
        );
        assert!((touched.comp.threshold_db - -6.0).abs() < 1e-6, "{}", touched.comp.threshold_db);
    }

    fn one_plugin_param_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.25,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 0.75,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "p".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target, 0.5)
        };
        song.tracks.push(track(|t| {
            t.id = 7;
            t.name = "T".into();
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));
        song
    }

    /// B4 (r.md #8): ramping plugin-param lane を 512-frame buffer で fill すると、
    /// frame 0 の 1 event でなく sub-buffer (64 刻み) の複数 event が出る
    /// (= sample-accurate、 速い automation の zipper 解消)。
    #[test]
    fn fill_pd_param_events_sub_samples_changing_curve() {
        let song = one_plugin_param_lane_song();
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(&mut pd, &song, 7, DEVICE_ID, SR, 120.0, 0.0, 512, &empty, &[]);
        assert!(
            pd.n_events_in > 1,
            "ramp は sub-buffer で複数 event を出すべき, got {}",
            pd.n_events_in
        );
        // 全 event が param_id 5、 frame offset 単調増加、 値は ramp に沿って増加。
        let mut last_time = 0u32;
        for i in 0..pd.n_events_in as usize {
            let e = &pd.events_in[i];
            assert_eq!(e.param_id, 5);
            if i > 0 {
                assert!(e.time > last_time, "frame offset 単調増加");
            }
            last_time = e.time;
        }
        assert!(pd.events_in[0].value >= 0.25 - 1e-6);
        let last = pd.events_in[(pd.n_events_in - 1) as usize].value;
        assert!(last > pd.events_in[0].value, "ramp で値が増加");
    }

    /// Phase 4 Step C-2 (plugin param 版) 回帰: recording 中 (Touch/Latch/Write)
    /// の plugin-param lane は curve eval を skip し、 plugin が GUI で持つ値を
    /// host が上書きしない。 旧実装は skip が無く write が read のままだった。
    #[test]
    fn fill_pd_param_events_skips_recording_lanes() {
        let song = one_plugin_param_lane_song();
        let track_id = 7;
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };

        // Not recording: the curve value is pushed as a ParamValue event (read).
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(&mut pd, &song, track_id, DEVICE_ID, SR, 120.0, 0.0, 64, &empty, &[]);
        assert_eq!(pd.n_events_in, 1, "read mode must push the curve value");

        // Recording: the lane is skipped, so no curve event overwrites the
        // plugin's live GUI value.
        let mut pd2 = ProcessData::empty();
        let mut rec = std::collections::HashSet::new();
        rec.insert((track_id, target));
        pd_params(&mut pd2, &song, track_id, DEVICE_ID, SR, 120.0, 0.0, 64, &rec, &[]);
        assert_eq!(pd2.n_events_in, 0, "recording lane curve must be suppressed");
    }

    /// r.md #8 再監査: master fx (`MASTER_TRACK_ID`) の PluginParam automation は
    /// track ではなく `song_lanes` から引く。 track を 1 つも持たない song の
    /// song_lanes に置いた PluginParam lane が `pd_params(MASTER_TRACK_ID,
    /// device_id)` で適用されること (= master fx 自動化) を検証。
    #[test]
    fn fill_pd_param_events_master_fx_reads_song_lanes() {
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.25,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 0.75,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "m".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target, 0.5)
        };
        song.song_lanes = vec![lane];
        // MASTER_TRACK_ID の track は存在しない → song_lanes 経由で解決するはず。
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(
            &mut pd,
            &song,
            common::model::MASTER_TRACK_ID,
            DEVICE_ID,
            SR,
            120.0,
            0.0,
            64,
            &empty,
            &[],
        );
        assert_eq!(pd.n_events_in, 1, "master fx PluginParam lane (song_lanes) must apply");
        assert_eq!(pd.events_in[0].param_id, 5);
        assert!(
            (pd.events_in[0].value - 0.25).abs() < 1e-6,
            "curve value at beat 0 should be 0.25, got {}",
            pd.events_in[0].value
        );
        // 別 device_id (別 master fx) には適用されない。
        let mut pd2 = ProcessData::empty();
        pd_params(
            &mut pd2,
            &song,
            common::model::MASTER_TRACK_ID,
            DEVICE_ID + 1,
            SR,
            120.0,
            0.0,
            64,
            &empty,
            &[],
        );
        assert_eq!(pd2.n_events_in, 0, "別 device_id の master fx には lane が無い");
    }
}
