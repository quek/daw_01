//! r.md #132 残件: **分割は切れ目を入れるだけ** — 分割前の 1 event を鳴らした出力と、分割後の片を並べて
//! 鳴らした出力が一致する。
//!
//! Song を組んで `compile_audio_schedule` → `render_audio_events` (live と書き出しが共有する経路) で
//! 鳴らし、分割は model の SSoT (`Song::split_content_at_points` = オーディオエディタの `E` / 範囲操作、
//! `Song::split_clips_at` = クリップの分割) で入れる。 どの mode でも切り口に掛かる写像 (伸縮・warp・
//! slice・テンポ変化・移調・逆再生・fade・フォルマント・グローバルトランスポーズ) を 1 つずつ含める。
//!
//! 許容の根拠:
//! - **tape / slice 経路** (エンジンを通らない): **1 sample も違わない**。 読み位置は take の頭からの
//!   sample 数で決まり、Raw / Repitch の読み位置の積分器 (`TapeCursor`) も後ろの片が前の片の積分を
//!   そのまま引き継ぐ (`acquire_tape_cursor` の続き) ので、切り口で丸め直さない。
//! - **スペクトル経路** (Stretch / フォルマント / 移調): 片は前の片のストリームを **引き継ぐ**
//!   (`acquire_engine` の続き) ので、切り口で解析の履歴は切れない。 残る違いはエンジンへの 1 回の呼び出しが
//!   切り口で 2 回に分かれること — Signalsmith Stretch は 1 回の呼び出しの中で入出力を線形に対応させるので、
//!   呼び出しの切り方で出力が僅かに変わる。 これは分割と無関係に host の buffer 長でも同じだけ起きる
//!   (= エンジンが元々その精度でしか定義されない) ので、**同じ event を別の buffer 長で鳴らした差** を
//!   対照に測り、分割の差がその桁に収まることを見る。 prime し直した (= 引き継げなかった) 場合は切り口に
//!   解析窓ぶんの欠けが出て、対照の何十倍にもなる。

use super::*;
use common::model::{AudioContent, AudioEvent, AudioSource, BeatMarker, Track};

const ENGINE_SR: u32 = 48_000;
const BPM: f32 = 120.0;
const ORIGIN: &str = "C:/split-fidelity/source.wav";

/// 位置で音色と振幅が変わるステレオ素材 (読み位置が 1 frame ずれても値が変わる)。
fn source(sample_rate: u32, frames: u64) -> Arc<AudioSourceBuffer> {
    let plane = |detune: f64| -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(sample_rate);
                let env = 0.2 + 0.7 * (i as f64 / frames as f64);
                let tone = (std::f64::consts::TAU * (220.0 + 180.0 * t) * detune * t).sin()
                    + 0.3 * (std::f64::consts::TAU * 1_330.0 * t).sin();
                (0.6 * env * tone) as f32
            })
            .collect()
    };
    Arc::new(AudioSourceBuffer {
        origin: std::path::PathBuf::from(ORIGIN),
        sample_rate,
        channels: 2,
        frames,
        samples: vec![plane(1.0), plane(1.01)],
    })
}

/// 1 つの audio event を置いた曲の作り方。
struct Scene {
    event: AudioEvent,
    source: Arc<AudioSourceBuffer>,
    /// テンポの直線変化 `(頭の BPM, 尻の BPM)`。
    tempo: Option<(f64, f64)>,
    /// グローバルトランスポーズ (半音)。
    transpose: i32,
}

impl Scene {
    fn new(event: AudioEvent, source: Arc<AudioSourceBuffer>) -> Self {
        Self { event, source, tempo: None, transpose: 0 }
    }

    /// clip 1 つ (拍 0 から event 全体) に event 1 つの曲。
    fn song(&self) -> Song {
        let transpose = i8::try_from(self.transpose).expect("移調は i8 に収まる");
        let mut song = Song { bpm: BPM, transpose, ..Song::default() };
        song.media.audio_sources.insert(
            1,
            AudioSource {
                path: AudioSourcePath::Absolute(std::path::PathBuf::from(ORIGIN)),
                sample_rate: self.source.sample_rate,
                channels: self.source.channels,
                frames: self.source.frames,
                original_bpm: None,
                root_key: None,
            },
        );
        let event = AudioEvent { id: 1, source_id: 1, ..self.event.clone() };
        let len = event.event_start_in_clip_beats + event.event_length_beats;
        let content_id = song.alloc_content_id();
        song.clip_contents
            .insert(content_id, ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 }));
        let mut track = Track { id: 1, ..Track::default() };
        track.clips.push(Clip { id: 1, start_beat: 0.0, length_beats: len, content_id, ..Clip::default() });
        track.next_clip_id = 2;
        song.tracks.push(track);
        if let Some((from, to)) = self.tempo {
            add_tempo_ramp(&mut song, from, to, len);
        }
        song
    }
}

/// SongTempo の直線 `from → to` (拍 0 から `len` まで)。
fn add_tempo_ramp(song: &mut Song, from: f64, to: f64, len: f64) {
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint, AutomationTarget,
    };
    let points = vec![
        AutomationPoint { id: 1, time_beat: 0.0, value: from, curve: AutomationCurve::Linear },
        AutomationPoint { id: 2, time_beat: len, value: to, curve: AutomationCurve::Linear },
    ];
    let content_id = song.alloc_content_id();
    song.clip_contents
        .insert(content_id, ClipContent::Automation(AutomationContent { points, next_point_id: 3 }));
    let mut lane = AutomationLane::new(AutomationTarget::SongTempo, f64::from(BPM));
    lane.id = song.alloc_song_lane_id();
    lane.clips.push(AutomationClip {
        id: 1,
        name: "Tempo".into(),
        start_beat: 0.0,
        length_beats: len,
        content_id,
        content_offset_beats: 0.0,
        color: None,
    });
    lane.next_clip_id = 2;
    song.song_lanes.push(lane);
}

/// 1 トラックぶんの描画の器 (live の `TrackScratch` と同じく、エンジン pool と累積器はトラックごと)。
struct TrackState {
    engines: Vec<StretchEngine>,
    accum: Vec<TapeCursor>,
    render_seq: u64,
}

/// 曲の全トラックを `beats` 拍ぶん `block` frame ずつ鳴らして足した L / R (engine と同じく buffer ごとに
/// 現在の BPM で拍を進める)。
fn render(song: &Song, source: &Arc<AudioSourceBuffer>, beats: f64, block: usize, transpose: i32) -> Vec<f32> {
    let mut cached = HashMap::new();
    cached.insert(1u32, Arc::clone(source));
    let prev = AudioClipRenderer::new(Vec::new(), cached);
    let renderer = compile_audio_schedule(song, Some(&prev), None, ENGINE_SR, false);
    assert!(renderer.sources.contains_key(&1), "素材は decode せずに引き継ぐ");
    let mut tracks: Vec<TrackState> = (0..song.tracks.len())
        .map(|t| TrackState {
            engines: (0..renderer.engines_per_track.get(t).copied().unwrap_or(0))
                .map(|_| StretchEngine::new(ENGINE_SR).expect("stretch engine"))
                .collect(),
            accum: vec![TapeCursor::IDLE; MAX_TAPE_STREAMS_PER_TRACK],
            render_seq: 0,
        })
        .collect();
    let mut event_l = vec![0.0f32; common::process_data::MAX_FRAMES];
    let mut event_r = vec![0.0f32; common::process_data::MAX_FRAMES];
    let tempo = common::automation::SongTempoCurve::of(song);
    let (mut out, mut playhead) = (Vec::new(), 0.0_f64);
    while playhead < beats {
        let bpm = tempo.at(playhead);
        let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
        for (track_idx, state) in tracks.iter_mut().enumerate() {
            render_audio_events(
                &renderer,
                track_idx,
                0,
                &mut l,
                &mut r,
                playhead,
                bpm,
                ENGINE_SR,
                block as u32,
                transpose,
                &mut ClipRenderState {
                    repitch_accum: &mut state.accum,
                    engines: &mut state.engines,
                    event_l: &mut event_l,
                    event_r: &mut event_r,
                    render_seq: &mut state.render_seq,
                },
            );
        }
        out.extend(l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]));
        playhead += block as f64 * f64::from(bpm) / (60.0 * f64::from(ENGINE_SR));
    }
    out
}

/// 最大の差と、差のエネルギー / 信号のエネルギー (buffer 長の違う対照は、両方が鳴らした長さまで比べる)。
fn difference(a: &[f32], b: &[f32]) -> (f32, f64) {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let max = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max);
    let err: f64 = a.iter().zip(b).map(|(x, y)| f64::from(x - y).powi(2)).sum();
    let sig: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum();
    assert!(sig > 1.0, "鳴っていない: {sig}");
    (max, err / sig)
}

/// 分割の入れ方。
#[derive(Clone, Copy, Debug)]
enum Split {
    /// 1 つの clip の中で content を切る (オーディオエディタの `E` / `Shift+E`、範囲操作)。
    Content,
    /// クリップごと割る (片は別の clip の窓)。
    Clips,
}

/// `cuts` のうち長さ `len` の event の内側に入るものだけで割る。
fn split(song: &mut Song, how: Split, cuts: &[f64], len: f64) {
    let cuts: Vec<f64> = cuts.iter().copied().filter(|&c| c > 0.0 && c < len).collect();
    match how {
        Split::Content => {
            let content_id = song.tracks[0].clips[0].content_id;
            let after = song.split_content_at_points(content_id, &cuts);
            assert_eq!(after, content_id, "共有していない content は fork しない");
            let ClipContent::Audio(audio) = &song.clip_contents[&content_id] else { panic!("audio") };
            assert_eq!(audio.events.len(), cuts.len() + 1, "切り口ごとに片ができる");
        }
        Split::Clips => {
            for &at in &cuts {
                song.split_clips_at(at);
            }
            assert_eq!(song.tracks[0].clips.len(), cuts.len() + 1, "切り口ごとに clip が割れる");
        }
    }
}

/// tape / slice 経路: 分割前後の出力が 1 sample も違わない (module doc)。
fn assert_tape_fidelity(label: &str, scene: &Scene, cuts: &[f64], how: Split) {
    let song = scene.song();
    let beats = scene.event.event_length_beats + 0.5;
    let whole = render(&song, &scene.source, beats, 512, scene.transpose);
    let mut pieces = song.clone();
    split(&mut pieces, how, cuts, scene.event.event_length_beats);
    let split_out = render(&pieces, &scene.source, beats, 512, scene.transpose);
    let (max, _) = difference(&whole, &split_out);
    assert!(max == 0.0, "{label} ({how:?}): 分割前後で最大 {max:.3e} 違う");
}

/// スペクトル経路: 分割の差が「同じ event を別の buffer 長で鳴らした差」の桁に収まる (module doc)。
fn assert_spectral_fidelity(label: &str, scene: &Scene, cuts: &[f64], how: Split) {
    let song = scene.song();
    let beats = scene.event.event_length_beats + 0.5;
    let whole = render(&song, &scene.source, beats, 512, scene.transpose);
    let control = render(&song, &scene.source, beats, 480, scene.transpose);
    let (_, partition) = difference(&whole, &control);
    let mut pieces = song.clone();
    split(&mut pieces, how, cuts, scene.event.event_length_beats);
    let split_out = render(&pieces, &scene.source, beats, 512, scene.transpose);
    let (max, rel) = difference(&whole, &split_out);
    assert!(
        rel <= partition * 4.0 + 1e-9,
        "{label} ({how:?}): 分割の差 {rel:.3e} (最大 {max:.3e}) が buffer 長の差 {partition:.3e} を大きく超える \
         = 切り口でストリームが途切れている"
    );
}

/// 48 kHz で 2 秒 (= 120 BPM で 4 拍) の素材を `len` 拍に置いた event。
fn event(mode: StretchMode, len: f64) -> AudioEvent {
    AudioEvent { source_end_frames: 96_000, event_length_beats: len, stretch_mode: mode, ..AudioEvent::default() }
}

/// グリッド線 (0.75 拍ごと) と、グリッドに乗らない位置の両方で切る。
const CUTS: [f64; 4] = [0.75, 1.5, 2.3125, 3.0];

#[test]
fn raw_と_repitch_は分割前と同じ音を読む() {
    let src = source(48_000, 96_000);
    for how in [Split::Content, Split::Clips] {
        assert_tape_fidelity("raw", &Scene::new(event(StretchMode::Raw, 4.0), Arc::clone(&src)), &CUTS, how);
        let raw_pitched = AudioEvent { pitch_semitones: 3.0, ..event(StretchMode::Raw, 4.0) };
        assert_tape_fidelity("raw +3", &Scene::new(raw_pitched, Arc::clone(&src)), &CUTS, how);
        // 伸ばした Repitch (4 拍の素材を 5 拍に) を下げて移調、逆再生も。
        let repitch = AudioEvent { pitch_semitones: -2.0, ..event(StretchMode::Repitch, 5.0) };
        assert_tape_fidelity("repitch stretched -2", &Scene::new(repitch.clone(), Arc::clone(&src)), &CUTS, how);
        let reversed = AudioEvent { reversed: true, ..repitch };
        assert_tape_fidelity("repitch reversed", &Scene::new(reversed, Arc::clone(&src)), &CUTS, how);
    }
    // source SR ≠ engine SR (読み位置が毎 sample 小数になる)。
    let src44 = source(44_100, 88_200);
    let raw44 = AudioEvent { source_end_frames: 88_200, ..event(StretchMode::Raw, 4.0) };
    assert_tape_fidelity("raw 44.1k", &Scene::new(raw44, src44), &CUTS, Split::Content);
}

#[test]
fn slice_は切り口を跨ぐ_slice_もそのまま鳴らす() {
    let src = source(48_000, 96_000);
    let onsets = vec![0, 15_000, 33_000, 51_000, 70_000];
    for how in [Split::Content, Split::Clips] {
        // 伸ばす (gap) / 詰める (cut) / 移調 — どれも slice の途中を切る位置がある。
        for (len, semis) in [(6.0, 0.0), (3.0, 0.0), (4.0, 5.0)] {
            let ev = AudioEvent { onsets: onsets.clone(), pitch_semitones: semis, ..event(StretchMode::Slice, len) };
            assert_tape_fidelity(&format!("slice len {len} pitch {semis}"), &Scene::new(ev, Arc::clone(&src)), &CUTS, how);
        }
    }
}

#[test]
fn テンポが変化していても分割前と同じ音を読む() {
    let src = source(48_000, 96_000);
    for how in [Split::Content, Split::Clips] {
        // native rate の Raw と slice 本体は拍あたりの読み量がテンポで変わる (= take の頭からの拍で決まる)。
        let raw = Scene { tempo: Some((90.0, 150.0)), ..Scene::new(event(StretchMode::Raw, 4.0), Arc::clone(&src)) };
        assert_tape_fidelity("raw tempo ramp", &raw, &CUTS, how);
        let slice = AudioEvent { onsets: vec![0, 20_000, 45_000, 71_000], ..event(StretchMode::Slice, 5.0) };
        let slice = Scene { tempo: Some((140.0, 80.0)), ..Scene::new(slice, Arc::clone(&src)) };
        assert_tape_fidelity("slice tempo ramp", &slice, &CUTS, how);
    }
    // テンポ変化中の Repitch は読み位置を積分するので、細かく割って (1/64 拍 = 1 buffer に何片も、トラックに
    // 数百の event) も片が積分を引き継ぐ。
    let repitch = AudioEvent { pitch_semitones: 2.0, ..event(StretchMode::Repitch, 5.0) };
    let repitch = Scene { tempo: Some((100.0, 160.0)), ..Scene::new(repitch, Arc::clone(&src)) };
    let fine: Vec<f64> = (1..320).map(|i| f64::from(i) / 64.0).collect();
    assert_tape_fidelity("repitch tempo ramp 320 pieces", &repitch, &fine, Split::Content);
}

#[test]
fn 切り口を跨ぐ_fade_のランプは片に続く() {
    let src = source(48_000, 96_000);
    let ev = AudioEvent {
        fade_in_beats: 1.9,
        fade_in_curve: FadeCurve::SCurve,
        fade_out_beats: 2.2,
        fade_out_curve: FadeCurve::Exponential,
        ..event(StretchMode::Raw, 4.0)
    };
    for how in [Split::Content, Split::Clips] {
        // 0.75 / 1.5 は fade-in の途中、2.3125 / 3.0 は fade-out の途中。
        assert_tape_fidelity("fades", &Scene::new(ev.clone(), Arc::clone(&src)), &CUTS, how);
    }
}

#[test]
fn stretch_と_warp_はストリームを引き継いで分割前と同じ音になる() {
    let src = source(48_000, 96_000);
    for how in [Split::Content, Split::Clips] {
        let uniform = AudioEvent { pitch_semitones: 4.0, ..event(StretchMode::Stretch, 5.5) };
        assert_spectral_fidelity("stretch uniform", &Scene::new(uniform, Arc::clone(&src)), &CUTS, how);
        let warped = AudioEvent {
            beat_markers: vec![
                BeatMarker { source_frame: 0, locked_beat: 0.0 },
                BeatMarker { source_frame: 30_000, locked_beat: 1.8 },
                BeatMarker { source_frame: 60_000, locked_beat: 2.6 },
                BeatMarker { source_frame: 96_000, locked_beat: 4.0 },
            ],
            ..event(StretchMode::Stretch, 4.0)
        };
        assert_spectral_fidelity("stretch warp", &Scene::new(warped, Arc::clone(&src)), &CUTS, how);
    }
}

/// 測定器の対照: 同じ分割でも後ろの片を **別のトラック** (= 別のエンジン pool で、ストリームを引き継げない)
/// に置くと、切り口で prime し直した差が出て、スペクトル経路の許容を大きく超える。 上のテストが
/// 「引き継ぎが切れたら落ちる」ことの確認。
#[test]
fn 対照_ストリームを引き継げないと切り口で許容を超える差が出る() {
    let src = source(48_000, 96_000);
    let scenes = [
        ("stretch uniform", Scene::new(AudioEvent { pitch_semitones: 4.0, ..event(StretchMode::Stretch, 5.5) }, Arc::clone(&src))),
        ("raw formant", Scene::new(AudioEvent { formant_semitones: 3.0, ..event(StretchMode::Raw, 4.0) }, Arc::clone(&src))),
    ];
    for (label, scene) in scenes {
        let song = scene.song();
        let beats = scene.event.event_length_beats + 0.5;
        let whole = render(&song, &src, beats, 512, 0);
        let (_, partition) = difference(&whole, &render(&song, &src, beats, 480, 0));
        let mut pieces = song.clone();
        split(&mut pieces, Split::Clips, &[2.0], scene.event.event_length_beats);
        let moved = pieces.tracks[0].clips.split_off(1);
        pieces.tracks.push(Track { id: 2, clips: moved, next_clip_id: 10, ..Track::default() });
        let (_, rel) = difference(&whole, &render(&pieces, &src, beats, 512, 0));
        assert!(
            rel > partition * 4.0 + 1e-9,
            "{label}: 引き継げない片の差 {rel:.3e} が許容 (buffer 長の差 {partition:.3e} の 4 倍) に収まってしまう"
        );
    }
}

/// 片のストリームの引き継ぎ (buffer の途中で前の片が鳴り終え、同じエンジンを後ろの片が続ける) と、take の
/// 座標の fade ランプが、audio thread で確保しない (Rust / vendored C++ の両方)。
#[cfg(feature = "rt-assert")]
#[test]
fn 片の引き継ぎと_fade_ランプは_audio_thread_で確保しない() {
    let src = source(48_000, 96_000);
    let ev = AudioEvent { pitch_semitones: 4.0, fade_in_beats: 2.0, fade_out_beats: 2.0, ..event(StretchMode::Stretch, 5.5) };
    let mut song = Scene::new(ev, Arc::clone(&src)).song();
    split(&mut song, Split::Clips, &CUTS, 5.5);
    let mut cached = HashMap::new();
    cached.insert(1u32, Arc::clone(&src));
    let renderer = compile_audio_schedule(&song, Some(&AudioClipRenderer::new(Vec::new(), cached)), None, ENGINE_SR, false);
    assert_eq!(renderer.engines_per_track.first().copied(), Some(1), "片は重ならないのでエンジンは 1 基");
    let mut engines = vec![StretchEngine::new(ENGINE_SR).expect("engine")];
    let mut accum = vec![TapeCursor::IDLE; MAX_TAPE_STREAMS_PER_TRACK];
    let (mut event_l, mut event_r) = (vec![0.0f32; common::process_data::MAX_FRAMES], vec![0.0f32; common::process_data::MAX_FRAMES]);
    let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
    let samples_per_beat = f64::from(ENGINE_SR) * 60.0 / f64::from(BPM);
    let mut render_seq = 0u64;
    // SAFETY: 引数なしのカウンタ読み出し。
    let cxx_before = unsafe { signalsmith_sys::sms_alloc_count() };
    assert_ne!(cxx_before, u64::MAX, "signalsmith-sys/alloc-count が有効になっていない");
    let buffers = (6.0 * samples_per_beat / 512.0) as u64;
    assert_no_alloc::assert_no_alloc(|| {
        for buf in 0..buffers {
            render_audio_events(
                &renderer,
                0,
                0,
                &mut l,
                &mut r,
                (buf * 512) as f64 / samples_per_beat,
                BPM,
                ENGINE_SR,
                512,
                0,
                &mut ClipRenderState {
                    repitch_accum: &mut accum,
                    engines: &mut engines,
                    event_l: &mut event_l,
                    event_r: &mut event_r,
                    render_seq: &mut render_seq,
                },
            );
        }
    });
    // SAFETY: 同上。
    let cxx_after = unsafe { signalsmith_sys::sms_alloc_count() };
    assert_eq!(cxx_after, cxx_before, "vendored C++ エンジンが RT で確保した");
    assert_eq!(engines[0].stream_key(), Some(1), "エンジンで描いた buffer がある");
}

#[test]
fn フォルマントとグローバルトランスポーズもストリームを引き継ぐ() {
    let src = source(48_000, 96_000);
    for how in [Split::Content, Split::Clips] {
        let formant = AudioEvent { formant_semitones: 3.0, ..event(StretchMode::Raw, 4.0) };
        assert_spectral_fidelity("raw formant", &Scene::new(formant, Arc::clone(&src)), &CUTS, how);
        let transposed = Scene { transpose: 5, ..Scene::new(event(StretchMode::Repitch, 4.0), Arc::clone(&src)) };
        assert_spectral_fidelity("repitch transpose", &transposed, &CUTS, how);
    }
}
