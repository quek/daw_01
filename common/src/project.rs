use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{AuxInputRoute, CURRENT_VERSION, LoopRegion, ProjectFile, Song, ViewState};

/// Result of `load_project`: the normalized song plus the optional GUI view
/// state. `view` is `None` for legacy files / files saved without
/// view state — callers fall back to their default (fit-to-content) behavior.
#[derive(Debug, Clone)]
pub struct LoadedProject {
    pub song: Song,
    pub view: Option<ViewState>,
    /// 再生ループ (ON/OFF + 範囲) の**解決済み**値。 `view` があればその
    /// `loop_region`、 無ければ v30 以前の `Song.loop_start_beat` /
    /// `loop_end_beat` からの移行値 (ON/OFF は当時セッション限りだったので `false`)。
    ///
    /// `view` に畳み込まず独立して返すのは、 ViewState 導入 (v28) 以前のファイルに
    /// ループ範囲だけのために `ViewState::default()` を合成すると、 ズーム / 行高 /
    /// ヘッダ幅まで既定値へ潰れてしまうため (= `view: None` の「globals は現状維持」
    /// 挙動が壊れる)。
    pub loop_region: LoopRegion,
}

/// Oldest project-file version `load` will accept. Versions below this
/// (currently `1` = the retired row-based format) are rejected with a
/// "re-create the project" error. Versions in `[MIN_LOADABLE_VERSION,
/// CURRENT_VERSION)` are accepted and forward-migrated via
/// `#[serde(default)]` on any new fields — fine because every field
/// added since v2 has a sensible default and no reinterpretation of
/// existing data.
const MIN_LOADABLE_VERSION: u32 = 2;

/// v26 で `builtin.video.subtitle` device が text overlay の表示ゲートになった
/// (`docs/plan_voicevox_talk.md` §6)。`migrate_text_overlay_to_subtitle_device` は
/// このバージョン未満の保存ファイルにだけ適用する。
const SUBTITLE_DEVICE_VERSION: u32 = 26;

/// v27 で `Clip.muted` / `Note.muted` が mute の SSoT になった。
/// `migrate_per_event_mute_to_clip_mute` はこのバージョン未満の保存ファイルにだけ適用する。
const CLIP_MUTE_VERSION: u32 = 27;

/// v31 で `Song.loop_start_beat` / `loop_end_beat` を撤去し、再生ループ (ON/OFF + 範囲) を
/// session state + [`ViewState::loop_region`] へ移した (「聴き方の都合」 は dirty を立てない
/// が保存される、[`LoopRegion`] 参照)。この版未満のファイルは Song 直下にループ範囲を持つので
/// [`legacy_song_loop_region`] が deserialize 前に拾い上げる。
const LOOP_IN_VIEW_STATE_VERSION: u32 = 31;

/// v30 (§10) で `ClipContent` を `#[serde(untagged)]` から tagged (`type` field) 化した。
/// この版未満のファイルは content を untagged (flat `{"notes":[...]}` 等) で保存しているので、
/// `migrate_clip_content_add_tag` で `type` を注入してから deserialize する。
const CLIP_CONTENT_TAG_VERSION: u32 = 30;

/// Save without GUI view state (legacy callers / tests / headless `--script`).
/// Delegates to `save_project` with `view = None`.
pub fn save(path: impl AsRef<Path>, song: &Song) -> Result<()> {
    save_project(path, song, None)
}

/// Save a project, optionally embedding GUI view state. `view` is
/// written as `ProjectFile.view` (a sibling of `song`), so the Song / IPC
/// layout is untouched. Atomic write via tmp → rename.
pub fn save_project(
    path: impl AsRef<Path>,
    song: &Song,
    view: Option<&ViewState>,
) -> Result<()> {
    let path = path.as_ref();
    let tmp = tmp_path(path);

    // Normalize for save (GC orphan content / audio / video / image
    // source-pool entries) so disk files stay tidy. Working on a clone —
    // caller's in-memory Song is not mutated.
    let mut song = song.clone();
    song.normalize_for_save();
    let project = ProjectFile {
        version: CURRENT_VERSION,
        song,
        view: view.cloned(),
    };
    let json = serde_json::to_string_pretty(&project)
        .context("failed to serialize project to JSON")?;

    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(json.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// 旧 `InstrumentSource::Vocal { speaker_id, style_name }`
/// (= JSON object `{"Vocal": {...}}`) を unit `Vocal` (= JSON string
/// `"Vocal"`) へ移行し、 旧トラック声をそのトラックの全 clip に焼き込む。
/// 声は per-clip (`Clip::speaker_id` 等) が SSoT になったため。
///
/// - 既に `speaker_id` を持つ clip (= 新形式) は尊重して上書きしない。
/// - `singer_name` は旧データに無いので空のまま (= `/singers` 取得後に
///   app 側が speaker_id から逆引きして埋める)。
/// - 新形式ファイル (source が既に string `"Vocal"` 等) は no-op。
fn migrate_vocal_source_to_clips(value: &mut serde_json::Value) {
    let Some(tracks) = value
        .get_mut("song")
        .and_then(|s| s.get_mut("tracks"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for track in tracks {
        // 旧形式判定: source == { "Vocal": { speaker_id, style_name } }。
        let Some(vocal) = track
            .get("source")
            .and_then(|s| s.get("Vocal"))
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let old_speaker = vocal
            .get("speaker_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let old_style = vocal
            .get("style_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        // source を unit "Vocal" に置換。
        track["source"] = serde_json::Value::String("Vocal".to_string());
        // 全 clip へ焼き込み (新形式 clip は触らない)。
        let Some(clips) = track
            .get_mut("clips")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for clip in clips {
            let Some(obj) = clip.as_object_mut() else {
                continue;
            };
            if obj.contains_key("speaker_id") {
                continue;
            }
            if old_speaker != 0 {
                obj.insert("speaker_id".to_string(), serde_json::json!(old_speaker));
            }
            if !old_style.is_empty() {
                obj.insert("style_name".to_string(), serde_json::json!(old_style));
            }
        }
    }
}

/// deserialize 前に旧 `sidechain_sources: Vec<Option<u32>>` (PluginInstance) を現行
/// `aux_inputs: Vec<Option<AuxInputRoute>>` (PostFader タップ) へ lift する。PluginInstance は
/// `tracks[].devices` / 旧 3-split chain / `master_fx_chain` に散在するので、`sidechain_sources`
/// キーを持つオブジェクトを再帰的に見つけて変換する (flatten の前後どちらでも効く location 非依存)。
/// 型安全のため旧値を `Vec<Option<u32>>` に deserialize → `AuxInputRoute::post_fader` で lift →
/// 再 serialize する (JSON を hard-code しない)。idempotent: `aux_inputs` が既にある object は
/// 旧キーを drain するだけ、`sidechain_sources` が無い object は無変更。旧 in-memory 移行
/// (`PluginInstance::migrate_legacy_aux`、§10 で撤去) の JSON 前処理版。
fn migrate_legacy_sidechain_to_aux(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            if let Some(sc) = map.remove("sidechain_sources")
                && !map.contains_key("aux_inputs")
                && let Ok(sources) = serde_json::from_value::<Vec<Option<u32>>>(sc)
            {
                let aux: Vec<Option<AuxInputRoute>> = sources
                    .into_iter()
                    .map(|opt| opt.map(AuxInputRoute::post_fader))
                    .collect();
                if !aux.is_empty()
                    && let Ok(v) = serde_json::to_value(aux)
                {
                    map.insert("aux_inputs".to_string(), v);
                }
            }
            for v in map.values_mut() {
                migrate_legacy_sidechain_to_aux(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                migrate_legacy_sidechain_to_aux(v);
            }
        }
        _ => {}
    }
}

/// 旧 per-section plugin slot 表現 (MIDI FX chain / 単 Instrument / audio FX chain)。
/// single-chain 再設計 (`docs/plan_linear_chain.md`) 後は旧 project の load 移行専用となり、
/// wire protocol から migration 層 (ここ) へ移設した。`migrate_legacy_device_chains` が
/// `slot → device_index` 解決にのみ使う (deserialize 専用)。
#[derive(Debug, Clone, Copy, serde::Deserialize)]
enum PluginSlot {
    MidiFx(u32),
    Instrument,
    Fx(u32),
}

/// deserialize 前に旧 3-split デバイスチェーン (`midi_fx_chain` / `instrument` / `fx_chain`) を
/// 現行の単一 `devices` へ平坦化し、automation lane / midi_binding の旧 `slot: PluginSlot` を
/// positional `device_index` へ解決する。`device_index` 自体は残し、ensure_ids の後段 remap が
/// `device_index → 安定 device_id` に写像する分業。旧 in-memory 移行 (§10 で撤去した
/// `Track::flatten_legacy_devices` と `ensure_ids` の binding-slot loop) の JSON 前処理版。
/// flatten 順は `midi_fx ++ instrument? ++ fx`、slot→index は
/// `MidiFx(i)→i / Instrument→n_midi / Fx(i)→n_midi+has_inst+i`。
fn migrate_legacy_device_chains(value: &mut serde_json::Value) {
    use serde_json::Value;
    use std::collections::HashMap;

    fn slot_to_index(slot: PluginSlot, n_midi: usize, has_inst: bool) -> u32 {
        match slot {
            PluginSlot::MidiFx(i) => i,
            PluginSlot::Instrument => n_midi as u32,
            PluginSlot::Fx(i) => (n_midi + has_inst as usize) as u32 + i,
        }
    }

    // PluginParam target の旧 `slot` を `device_index` へ解決 (device_index 既存なら旧キー掃除のみ)。
    fn resolve_slot(pp: &mut serde_json::Map<String, Value>, n_midi: usize, has_inst: bool) {
        if pp.contains_key("device_index") {
            pp.remove("slot");
            return;
        }
        if let Some(slot_val) = pp.remove("slot")
            && let Ok(slot) = serde_json::from_value::<PluginSlot>(slot_val)
        {
            pp.insert(
                "device_index".to_string(),
                Value::from(slot_to_index(slot, n_midi, has_inst)),
            );
        }
    }

    let Some(song) = value.as_object_mut() else {
        return;
    };

    // pass 1: track id → (n_midi, has_inst)。midi_binding が参照 track の chain 長を要るため
    // flatten 前に採取する。
    let mut chain_lens: HashMap<u64, (usize, bool)> = HashMap::new();
    if let Some(tracks) = song.get("tracks").and_then(Value::as_array) {
        for track in tracks {
            let n_midi = track
                .get("midi_fx_chain")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let has_inst = track.get("instrument").is_some_and(|v| !v.is_null());
            if let Some(id) = track.get("id").and_then(Value::as_u64) {
                chain_lens.insert(id, (n_midi, has_inst));
            }
        }
    }

    // pass 2: 各 track の automation lane slot を解決 + 3-split を devices へ平坦化。
    if let Some(tracks) = song.get_mut("tracks").and_then(Value::as_array_mut) {
        for track in tracks.iter_mut() {
            let Some(obj) = track.as_object_mut() else {
                continue;
            };
            let has_devices = obj
                .get("devices")
                .and_then(Value::as_array)
                .is_some_and(|d| !d.is_empty());
            let n_midi = obj
                .get("midi_fx_chain")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let has_inst = obj.get("instrument").is_some_and(|v| !v.is_null());
            let has_legacy = n_midi > 0
                || has_inst
                || obj
                    .get("fx_chain")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty());
            // guard (Track::flatten_legacy_devices と同): 新形式 (devices 既存) or legacy 無しは
            // 平坦化しない。旧キーだけは掃除する。
            if has_devices || !has_legacy {
                obj.remove("midi_fx_chain");
                obj.remove("instrument");
                obj.remove("fx_chain");
                continue;
            }
            if let Some(lanes) = obj.get_mut("automation_lanes").and_then(Value::as_array_mut) {
                for lane in lanes.iter_mut() {
                    if let Some(pp) = lane
                        .get_mut("target")
                        .and_then(|t| t.get_mut("PluginParam"))
                        .and_then(Value::as_object_mut)
                    {
                        resolve_slot(pp, n_midi, has_inst);
                    }
                }
            }
            let mut devices = Vec::new();
            if let Some(Value::Array(a)) = obj.remove("midi_fx_chain") {
                devices.extend(a);
            }
            if let Some(inst) = obj.remove("instrument")
                && !inst.is_null()
            {
                devices.push(inst);
            }
            if let Some(Value::Array(a)) = obj.remove("fx_chain") {
                devices.extend(a);
            }
            obj.insert("devices".to_string(), Value::Array(devices));
        }
    }

    // pass 3: midi_binding の slot を参照 track の chain 長で解決 (r.md #8 M7)。
    if let Some(bindings) = song.get_mut("midi_bindings").and_then(Value::as_array_mut) {
        for binding in bindings.iter_mut() {
            let Some(pp) = binding
                .get_mut("target")
                .and_then(|t| t.get_mut("PluginParam"))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if let Some(&(n_midi, has_inst)) = pp
                .get("track")
                .and_then(Value::as_u64)
                .and_then(|id| chain_lens.get(&id))
            {
                resolve_slot(pp, n_midi, has_inst);
            }
        }
    }
}

/// deserialize 前に旧 per-clip インライン content を content store へ移す。
/// v5 の `Clip.notes` (インライン MIDI) → `clip_contents[cid]`(未 tag Midi)、v19 の `Clip.name`
/// (per-clip 名) → `clip_content_names[cid]` (共有名 map、同一 content_id は先勝ち)。content_id が
/// 未採番 (0/欠落) の legacy clip には fresh id を採番し `next_content_id` を進める (runtime clip の
/// 採番は `Song::ensure_clip_contents` が引き継ぐ)。automation lane / song lane の clip は name のみ
/// (payload はインラインに持たない)。作った Midi は未 tag なので、後続の `migrate_clip_content_add_tag`
/// (`notes` → Midi) が tag する。旧 in-memory 移行 (`ensure_clip_contents` の name/notes ドレイン、
/// §10 で撤去) の JSON 前処理版。
fn migrate_legacy_clip_content(value: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(song) = value.as_object_mut() else {
        return;
    };

    // pre-scan: 全 clip の content_id 最大値 (採番カウンタを既存 id より上へ上げる)。
    fn scan_max(clips: Option<&Value>, max: &mut u64) {
        let Some(arr) = clips.and_then(Value::as_array) else {
            return;
        };
        for c in arr {
            if let Some(cid) = c.get("content_id").and_then(Value::as_u64) {
                *max = (*max).max(cid);
            }
        }
    }
    let mut max_cid = 0u64;
    if let Some(tracks) = song.get("tracks").and_then(Value::as_array) {
        for t in tracks {
            scan_max(t.get("clips"), &mut max_cid);
            if let Some(lanes) = t.get("automation_lanes").and_then(Value::as_array) {
                for l in lanes {
                    scan_max(l.get("clips"), &mut max_cid);
                }
            }
        }
    }
    if let Some(lanes) = song.get("song_lanes").and_then(Value::as_array) {
        for l in lanes {
            scan_max(l.get("clips"), &mut max_cid);
        }
    }
    let mut counter = song
        .get("next_content_id")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if counter <= max_cid {
        counter = max_cid + 1;
    }
    if counter == 0 {
        counter = 1;
    }

    // content store を song から取り出す (clip walk との借用衝突を避ける)。
    let mut contents = match song.remove("clip_contents") {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    let mut names = match song.remove("clip_content_names") {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };

    // clip 1 個を処理: name → names、notes(許可時) → contents(Midi)、未採番なら content_id 採番。
    let mut process = |clip: &mut Value, allow_notes: bool| {
        let Some(obj) = clip.as_object_mut() else {
            return;
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let notes = if allow_notes {
            obj.get("notes")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .cloned()
        } else {
            None
        };
        obj.remove("name");
        obj.remove("notes");
        if name.is_none() && notes.is_none() {
            return;
        }
        let mut cid = obj.get("content_id").and_then(Value::as_u64).unwrap_or(0);
        if cid == 0 {
            cid = counter;
            counter += 1;
            obj.insert("content_id".to_string(), Value::from(cid));
        }
        let key = cid.to_string();
        if let Some(name) = name {
            names.entry(key.clone()).or_insert_with(|| Value::from(name));
        }
        if let Some(notes) = notes {
            contents
                .entry(key)
                .or_insert_with(|| serde_json::json!({ "notes": notes }));
        }
    };

    if let Some(tracks) = song.get_mut("tracks").and_then(Value::as_array_mut) {
        for t in tracks.iter_mut() {
            if let Some(clips) = t.get_mut("clips").and_then(Value::as_array_mut) {
                for c in clips.iter_mut() {
                    process(c, true);
                }
            }
            if let Some(lanes) = t
                .get_mut("automation_lanes")
                .and_then(Value::as_array_mut)
            {
                for l in lanes.iter_mut() {
                    if let Some(clips) = l.get_mut("clips").and_then(Value::as_array_mut) {
                        for c in clips.iter_mut() {
                            process(c, false);
                        }
                    }
                }
            }
        }
    }
    if let Some(lanes) = song.get_mut("song_lanes").and_then(Value::as_array_mut) {
        for l in lanes.iter_mut() {
            if let Some(clips) = l.get_mut("clips").and_then(Value::as_array_mut) {
                for c in clips.iter_mut() {
                    process(c, false);
                }
            }
        }
    }

    if !contents.is_empty() {
        song.insert("clip_contents".to_string(), Value::Object(contents));
    }
    if !names.is_empty() {
        song.insert("clip_content_names".to_string(), Value::Object(names));
    }
    song.insert("next_content_id".to_string(), Value::from(counter));
}

/// 全 song-load 経路 (`load_project` のファイル load、script の `appLoadSongJson` /
/// `loadSongFromObject`) が deserialize 前に通す legacy song migration の束。song value を受け、
/// 旧 sidechain / 3-split device chain / per-clip インライン content (v5 notes / v19 name) を
/// 現行構造へ移す。1 箇所に集約することで load 経路ごとの migration 漏れ (= 旧データ欠落) を防ぐ。
/// tagged ClipContent 化 (`tag_clip_contents_in_song`) は呼び元が続けて適用する
/// (作った Midi content が tag される順序)。
pub fn migrate_legacy_song(song: &mut serde_json::Value) {
    migrate_legacy_sidechain_to_aux(song);
    migrate_legacy_device_chains(song);
    migrate_legacy_clip_content(song);
    migrate_flat_media_to_pools(song);
    migrate_flat_ids_to_allocators(song);
}

/// deserialize 前に旧 .daw のフラットな media source マップ (`audio_sources` / `video_sources` /
/// `image_sources` を Song 直下) を nested `"media"` へ移す (§10 bullet 4 の MediaPools 化)。
/// 既に `"media"` があれば新形式なので no-op。serde `flatten` が `HashMap<u32, _>` の整数キーを
/// content-buffer 経由で復元できないため nested を採用したことに伴う後方互換移行。
fn migrate_flat_media_to_pools(song: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(obj) = song.as_object_mut() else {
        return;
    };
    if obj.contains_key("media") {
        return;
    }
    let mut media = serde_json::Map::new();
    for key in ["audio_sources", "video_sources", "image_sources"] {
        if let Some(v) = obj.remove(key) {
            media.insert(key.to_string(), v);
        }
    }
    if !media.is_empty() {
        obj.insert("media".to_string(), Value::Object(media));
    }
}

/// deserialize 前に旧 .daw のフラットな `next_*_id` カウンタ (Song 直下) を nested `"ids"` へ移す
/// (§10 bullet 4 の IdAllocators 化)。既に `"ids"` があれば新形式なので no-op。
/// `migrate_flat_media_to_pools` と同じ後方互換移行 (serde flatten は Song の HashMap キーを壊すため
/// nested を採用)。`migrate_legacy_clip_content` が設定するフラット `next_content_id` の後に走る。
fn migrate_flat_ids_to_allocators(song: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(obj) = song.as_object_mut() else {
        return;
    };
    if obj.contains_key("ids") {
        return;
    }
    let mut ids = serde_json::Map::new();
    for key in [
        "next_track_id",
        "next_device_id",
        "next_content_id",
        "next_audio_source_id",
        "next_video_source_id",
        "next_image_source_id",
        "next_song_lane_id",
        "next_section_id",
        "next_mod_source_id",
    ] {
        if let Some(v) = obj.remove(key) {
            ids.insert(key.to_string(), v);
        }
    }
    if !ids.is_empty() {
        obj.insert("ids".to_string(), Value::Object(ids));
    }
}

/// (v30) `ClipContent` を untagged → tagged (`type` field) 化した際の後方互換移行。
/// v<30 のファイルは content を untagged (flat) で保存しているので、`#[serde(tag = "type")]`
/// deserialize の前に旧 untagged 判別規則で `type` を注入する。判別順は旧
/// `#[serde(untagged)]` + `deny_unknown_fields` と同じ:
/// `notes` → Midi / `points` → Automation / `events[0]` の
/// `source_start_micros` → Video / `text` → Text / `opacity` → Image / それ以外 → Audio
/// (TextEvent も `opacity` を持つので text を opacity より先に判定する)。
/// 空 content (`{}`、旧 untagged では先頭 variant に落ちていた) は Midi。`type` を既に持つ
/// content は no-op (idempotent)。content は `song.clip_contents` (content_id → content の map)。
pub fn tag_clip_contents_in_song(song: &mut serde_json::Value) {
    let Some(contents) = song
        .get_mut("clip_contents")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for content in contents.values_mut() {
        let Some(obj) = content.as_object_mut() else {
            continue;
        };
        if obj.contains_key("type") {
            continue; // 既に tagged
        }
        let ty = if obj.contains_key("notes") {
            "Midi"
        } else if obj.contains_key("points") {
            "Automation"
        } else if let Some(ev0) = obj
            .get("events")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| a.first())
            .and_then(serde_json::Value::as_object)
        {
            // 判定順が重要: TextEvent は `opacity` を **持つ** (テキストオーバーレイの不透明度)
            // ので、`text` (TextEvent 固有) を `opacity` (Image/Text 共有) より先に見る。
            // 旧 untagged + deny_unknown_fields の exact-match と同じ結果になるよう:
            // source_start_micros(Video 固有) → text(Text 固有) → opacity(Text 除外後は Image) → Audio。
            if ev0.contains_key("source_start_micros") {
                "Video"
            } else if ev0.contains_key("text") {
                "Text"
            } else if ev0.contains_key("opacity") {
                "Image"
            } else {
                "Audio"
            }
        } else if obj.contains_key("next_event_id") {
            // events を全削除して空になった Audio content。 events は
            // `skip_serializing_if = Vec::is_empty` で省略されるが、 v29 の
            // `next_event_id` (`skip_serializing_if = is_zero_u32`) は残るため
            // `{"next_event_id": N}` として serialize される。 旧 untagged +
            // deny_unknown_fields はこれを (Midi が unknown field で失格し) Audio と
            // 解決していた — その等価性を保つ。
            "Audio"
        } else if obj.contains_key("next_point_id") {
            // 同上: points を全削除して空になった Automation content
            // (`{"next_point_id": N}`)。 旧 untagged では Automation。
            "Automation"
        } else {
            // 真に空 (`{}` or `{"next_note_id": N}`) → 旧 untagged でも先頭 variant
            // (Midi) に落ちた。
            "Midi"
        };
        obj.insert("type".to_string(), serde_json::Value::String(ty.to_string()));
    }
}

/// project value (`{version, song, ...}`) の `song.clip_contents` に v30 `type` タグを注入する。
/// `load_project` が version-gate 越しに呼ぶ。Song 単体 JSON は [`tag_clip_contents_in_song`]。
fn migrate_clip_content_add_tag(value: &mut serde_json::Value) {
    if let Some(song) = value.get_mut("song") {
        tag_clip_contents_in_song(song);
    }
}

/// (v26) device-gated text overlay 移行 (`docs/plan_voicevox_talk.md` §6)。
/// v25 以前は `ClipContent::Text` がトラック非依存で常時 overlay 表示されていた。
/// v26 で `builtin.video.subtitle` device が表示ゲートになったため、旧プロジェクトの
/// 「Text clip を 1 つ以上持つトラック」へ字幕デバイスを auto-insert して見た目を保つ。
/// idempotent (既に字幕デバイスを持つトラックは no-op)。**version-gate 前提** — v26 以降の
/// プロジェクトには適用しない (= ユーザーが字幕デバイスを抜いた「喋るが映さない」
/// トラックを誤って表示化しない)。caller が `project.version < SUBTITLE_DEVICE_VERSION` で gate。
fn migrate_text_overlay_to_subtitle_device(song: &mut Song) {
    use crate::model::ClipContent;
    // clip_contents は tracks と別 borrow になるので、Text な content_id を先に集める。
    let text_content_ids: std::collections::HashSet<crate::model::ContentId> = song
        .clip_contents
        .iter()
        .filter(|(_, c)| matches!(c, ClipContent::Text(_)))
        .map(|(id, _)| *id)
        .collect();
    for track in &mut song.tracks {
        let has_text_clip = track
            .clips
            .iter()
            .any(|c| text_content_ids.contains(&c.content_id));
        if has_text_clip && !track.has_subtitle_device() {
            track.devices.push(crate::model::PluginInstance::with_ports(
                crate::plugin_db::SUBTITLE_ID.to_string(),
                crate::plugin_format::PluginFormat::Builtin,
                crate::port_config::PortConfig {
                    has_video_input: true,
                    has_video_output: true,
                    ..Default::default()
                },
            ));
        }
    }
}

/// (v27) per-event mute → clip-level mute 統合。v26 以前は clip の mute を
/// audio / image / video / text event の `muted` フラグ (inspector の "Mute" トグルが clip 内
/// 全 event を mute) で表現していた。v27 で `Clip.muted` を SSoT に一本化したので、旧プロジェクトの
/// 「event が muted な content」を `Clip.muted = true` へ畳み込み、event 側の `muted` は false に戻す。
/// 共有 content (linked clip) は 1 度 false 化すれば全 clip に効く。idempotent。**version-gate 前提** —
/// v27 以降のプロジェクトの event.muted は触らない (将来 per-event mute UI 用に温存)。caller が
/// `project.version < CLIP_MUTE_VERSION` で gate。
fn migrate_per_event_mute_to_clip_mute(song: &mut Song) {
    use crate::model::{ClipContent, ContentId};
    // content_id 単位で「event が 1 つでも muted だったか」を判定しつつ event.muted を false に戻す。
    let mut muted_contents: std::collections::HashSet<ContentId> = std::collections::HashSet::new();
    for (id, content) in song.clip_contents.iter_mut() {
        let any_muted = match content {
            ClipContent::Audio(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Image(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Video(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Text(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Midi(_) | ClipContent::Automation(_) => false,
        };
        if any_muted {
            muted_contents.insert(*id);
        }
    }
    if muted_contents.is_empty() {
        return;
    }
    for track in &mut song.tracks {
        for clip in &mut track.clips {
            if muted_contents.contains(&clip.content_id) {
                clip.muted = true;
            }
        }
    }
}

/// v30 以前の `.daw` が Song 直下に持っていたループ範囲を読み出す
/// ([`LOOP_IN_VIEW_STATE_VERSION`])。`Song` からフィールドが消えた今、
/// deserialize すると黙って捨てられるので、`from_value` の**前**に生 JSON から拾う。
/// ON/OFF は当時セッション限り (保存されていなかった) なので `enabled: false` —
/// 「起動直後はループ OFF」 という旧挙動をそのまま保つ。
/// 範囲が未定義 (`end <= start`、既定の 0/0 を含む) なら `None` = 移行するものなし。
fn legacy_song_loop_region(value: &serde_json::Value) -> Option<LoopRegion> {
    let song = value.get("song")?;
    let beat = |key: &str| song.get(key).and_then(serde_json::Value::as_f64);
    let mut region = LoopRegion {
        enabled: false,
        start_beat: beat("loop_start_beat")?,
        end_beat: beat("loop_end_beat")?,
    };
    region.sanitize();
    region.has_range().then_some(region)
}

/// Load just the song (legacy callers / tests / headless `--script`).
/// Delegates to `load_project` and drops the view state.
pub fn load(path: impl AsRef<Path>) -> Result<Song> {
    Ok(load_project(path)?.song)
}

/// version-gated migration の 1 entry: `(introduced_in, apply)`。ファイルの `version` が
/// `introduced_in` 未満のときだけ `apply` を走らせる。`T` は適用対象 (生 JSON `Value` か `Song`)。
type Migration<T> = (u32, fn(&mut T));

/// deserialize が成立する前に生の JSON `Value` へ当てる version-gated migration の表。
/// tagged ClipContent の `type` 注入等、型 deserialize の前提を作るものを置く。
/// version 非依存 (idempotent) な前処理 (`migrate_vocal_source_to_clips`) は
/// gate せず `load_project` 冒頭で無条件に呼ぶ。
const VALUE_MIGRATIONS: &[Migration<serde_json::Value>] =
    &[(CLIP_CONTENT_TAG_VERSION, migrate_clip_content_add_tag)];

/// deserialize 後の `Song` へ当てる version-gated migration の表。`< CURRENT_VERSION` で
/// gate してはならない (version bump のたびに一つ前のバージョンのファイルへ誤再適用される)。
const SONG_MIGRATIONS: &[Migration<Song>] = &[
    // (v26) device-gated text overlay 以前の Text 持ちトラックへ字幕デバイスを補い表示を保つ。
    // normalize の前に走るので、補った device も他 device と同じ正規化 (aux migration 等) を通る。
    (SUBTITLE_DEVICE_VERSION, migrate_text_overlay_to_subtitle_device),
    // (v27) 旧 per-event mute を `Clip.muted` へ畳み込む (v27+ の `event.muted` は将来 UI 用に温存)。
    (CLIP_MUTE_VERSION, migrate_per_event_mute_to_clip_mute),
];

/// Load a project including optional GUI view state. The returned
/// `song` is fully normalized (same as `load`); `view` is `None` for legacy
/// files / files saved without view state.
pub fn load_project(path: impl AsRef<Path>) -> Result<LoadedProject> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    // per-clip 声移行: 旧 `InstrumentSource::Vocal { speaker_id,
    // style_name }` (JSON object) を unit `Vocal` (JSON string) に変換し、
    // 旧トラック声をそのトラックの全 clip へ焼き込んでから deserialize する
    // (= 本 deserialize は unit `Vocal` だけ見れば良い、 声は per-clip が SSoT)。
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse project JSON from {}", path.display()))?;
    migrate_vocal_source_to_clips(&mut value);
    // 旧 sidechain / 3-split device chain / per-clip インライン content (v5 notes / v19 name) を
    // deserialize 前に現行構造へ移す (全 load 経路共通の SSoT、clip_content_add_tag より前 =
    // 作った Midi content が tag される)。
    if let Some(song) = value.get_mut("song") {
        migrate_legacy_song(song);
    }
    // deserialize が成立する前に生の JSON を整える version-gated migration を単一 dispatch
    // table (VALUE_MIGRATIONS) から適用する (例: v30 の tagged ClipContent `type` 注入 —
    // tagged deserialize は `type` が無いと失敗するため from_value の前に当てる)。
    let file_version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    // v31 で `Song` から消えたループ範囲を、deserialize (= 未知フィールドの黙殺) の
    // **前**に生 JSON から救い出す。他の migration と違い `Song` にも `ViewState` にも
    // 書き戻せない (前者はフィールドを撤去済、後者を合成すると view: None の
    // 「globals は現状維持」 が壊れる) ので、値のまま持ち回って下で解決する。
    let legacy_loop = (file_version < u64::from(LOOP_IN_VIEW_STATE_VERSION))
        .then(|| legacy_song_loop_region(&value))
        .flatten();
    for &(introduced_in, migrate) in VALUE_MIGRATIONS {
        if file_version < u64::from(introduced_in) {
            migrate(&mut value);
        }
    }
    let project: ProjectFile = serde_json::from_value(value)
        .with_context(|| format!("failed to deserialize project from {}", path.display()))?;
    if project.version > CURRENT_VERSION {
        anyhow::bail!(
            "project file {} has version {} newer than supported {}",
            path.display(),
            project.version,
            CURRENT_VERSION
        );
    }
    if project.version < MIN_LOADABLE_VERSION {
        anyhow::bail!(
            "project file {} uses retired version {} (the row-based \
             format predating version 2); re-create the project in the \
             current free-time-note format.",
            path.display(),
            project.version,
        );
    }
    if project.version < CURRENT_VERSION {
        tracing::info!(
            path = %path.display(),
            from_version = project.version,
            current_version = CURRENT_VERSION,
            "loaded legacy project file; missing fields filled with serde defaults"
        );
    }
    // 再生ループの解決: v31+ は `ViewState::loop_region` が真実源。範囲が未設定なら
    // v30 以前の `Song.loop_*_beat` から移行する (v28..v30 のファイルは view を持つが
    // loop_region は既定値なので、この順序でないと旧範囲を取りこぼす)。解決値は
    // `view` にも書き戻して「同じ値が 2 か所で食い違う」状態を作らない。
    let mut view = project.view;
    let mut loop_region = view.as_ref().map(|v| v.loop_region).unwrap_or_default();
    if !loop_region.has_range() && let Some(legacy) = legacy_loop {
        loop_region = legacy;
    }
    loop_region.sanitize();
    if let Some(v) = view.as_mut() {
        v.loop_region = loop_region;
    }
    let mut song = project.song;
    // deserialize 後の Song へ当てる version-gated migration を単一 dispatch table
    // (SONG_MIGRATIONS) から適用する。各 entry は「その挙動が導入されたバージョン」未満の
    // ファイルにだけ走る (gate 規約の詳細は SONG_MIGRATIONS の doc-comment)。
    for &(introduced_in, migrate) in SONG_MIGRATIONS {
        if project.version < introduced_in {
            migrate(&mut song);
        }
    }
    // Re-establish every invariant the codebase assumes about a loaded
    // song in one SSoT call: value-range sanity (bpm/time_sig/length/loop
    // — defends downstream divisors against 0/NaN from corrupt files),
    // v5→v6/v6→v7 content & source-id migration, stable id assignment
    // (track/clip/parent_group_id consistency), and the scale-change /
    // automation-point sort invariants. Idempotent — safe if a caller
    // (e.g. `daw_gui::app::open_project`) re-runs it.
    song.normalize_after_load();
    Ok(LoadedProject { song, view, loop_region })
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Clip, ClipContent, InstrumentSource, MidiContent, Note, TextContent, TextEvent, Track,
    };
    use crate::model::{
        AudioEditorViewState, ClipKey, EditorWindowGeometry, FollowMode, PianoRollViewState,
        ViewState,
    };
    use tempfile::tempdir;

    /// per-clip view + globals を含む代表的な `ViewState`。
    fn sample_view_state() -> ViewState {
        ViewState {
            arrange_zoom_x: 37.5,
            arrange_scroll_beat: 12.0,
            arrange_track_top: 48.0,
            arrange_track_row_h: 72.0,
            arrange_header_w: 200.0,
            track_row_overrides: [(1u32, 64u16), (3, 120)].into_iter().collect(),
            expanded_automation_tracks: vec![2, 5],
            master_row_automation_expanded: true,
            arrange_follow: FollowMode::Page,
            loop_region: LoopRegion { enabled: true, start_beat: 8.0, end_beat: 24.0 },
            arrange_snap_enabled: false,
            arrange_snap_choice: 4,
            pianoroll_snap_enabled: true,
            pianoroll_snap_choice: 2,
            piano_roll_fold: true,
            snap_on_draw: true,
            snap_live_input: false,
            bottom_panel: 1,
            selected_clip: Some(ClipKey { track_id: 2, clip_id: 1 }),
            selected_clips: vec![
                ClipKey { track_id: 1, clip_id: 1 },
                ClipKey { track_id: 2, clip_id: 1 },
            ],
            piano_roll_views: vec![
                (
                    ClipKey { track_id: 1, clip_id: 1 },
                    PianoRollViewState { zoom_x: 120.0, zoom_y: 22.0, top_pitch: 72, scroll_beat: 3.0 },
                ),
                (
                    ClipKey { track_id: 2, clip_id: 1 },
                    PianoRollViewState { zoom_x: 16.0, zoom_y: 8.0, top_pitch: 96, scroll_beat: 0.0 },
                ),
            ],
            audio_editor_views: vec![(
                ClipKey { track_id: 3, clip_id: 7 },
                AudioEditorViewState { start_beat: 1.5, len_beats: 8.0 },
            )],
            plugin_editor_windows: vec![
                (7, EditorWindowGeometry { x: 240, y: 160, width: 880, height: 162 }),
                (11, EditorWindowGeometry { x: -40, y: 900, width: 1105, height: 687 }),
            ],
            launcher_layout: crate::model::LauncherLayout::LauncherOnly,
            launcher_width: 420.0,
            launcher_scene_col_w: 96.0,
            launcher_scroll_scene: 2.5,
        }
    }

    /// ViewState は save_project → load_project で完全に往復する。
    #[test]
    fn save_project_roundtrips_view_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("with_view.daw");
        let view = sample_view_state();
        save_project(&path, &Song::default(), Some(&view)).unwrap();
        let loaded = load_project(&path).unwrap();
        assert_eq!(
            loaded.view.as_ref(),
            Some(&view),
            "view state が save/load で完全往復する"
        );
    }

    /// `view` キーを持たない旧ファイルは `view == None` で読め、
    /// 従来挙動 (fit-to-content) にフォールバックできる。
    #[test]
    fn legacy_file_loads_with_none_view() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.daw");
        write_project_with_version(&path, &Song::default(), CURRENT_VERSION);
        let loaded = load_project(&path).unwrap();
        assert_eq!(loaded.view, None);
    }

    /// (b) ループ (ON/OFF + 範囲) は `ViewState` 経由で save → load を往復する。
    /// `Song` には載らない (= 変えても dirty にならない) が失われもしない、が要求。
    #[test]
    fn loop_region_roundtrips_through_view_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("loop.daw");
        let view = ViewState {
            loop_region: LoopRegion { enabled: true, start_beat: 8.0, end_beat: 24.0 },
            ..ViewState::default()
        };
        save_project(&path, &Song::default(), Some(&view)).unwrap();
        // 保存された JSON の `song` 側にループが漏れていないこと (= Song は無関係)。
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["song"].get("loop_start_beat").is_none());
        assert!(raw["song"].get("loop_end_beat").is_none());

        let loaded = load_project(&path).unwrap();
        assert_eq!(loaded.loop_region, view.loop_region);
        assert_eq!(loaded.view.unwrap().loop_region, view.loop_region);
    }

    /// (c) v30 以前の `.daw` (= ループ範囲が `Song` 直下) から移行される。
    /// ViewState を持たない古い版でも範囲を失わず、かつ `view` は `None` のまま
    /// (= 合成して globals を既定値へ潰さない)。ON/OFF は当時保存されていないので `false`。
    #[test]
    fn legacy_song_loop_range_migrates_to_loaded_loop_region() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("v30_loop.daw");
        let mut song_value = serde_json::to_value(Song::default()).unwrap();
        song_value["loop_start_beat"] = serde_json::json!(4.0);
        song_value["loop_end_beat"] = serde_json::json!(12.0);
        let project = serde_json::json!({ "version": 30, "song": song_value });
        std::fs::write(&path, serde_json::to_string(&project).unwrap()).unwrap();

        let loaded = load_project(&path).unwrap();
        assert_eq!(
            loaded.loop_region,
            LoopRegion { enabled: false, start_beat: 4.0, end_beat: 12.0 },
            "旧 Song のループ範囲を拾う (ON/OFF は当時セッション限りなので false)"
        );
        assert_eq!(loaded.view, None, "ViewState を合成しない (globals は現状維持)");
    }

    /// v28..v30 の `.daw` は `view` を持つが `loop_region` は未保存 (= 既定)。
    /// この場合も `Song` 直下の旧ループ範囲を拾い、`view` にも書き戻して食い違わせない。
    #[test]
    fn legacy_song_loop_range_wins_over_defaulted_view_loop_region() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("v30_loop_with_view.daw");
        let mut song_value = serde_json::to_value(Song::default()).unwrap();
        song_value["loop_start_beat"] = serde_json::json!(2.0);
        song_value["loop_end_beat"] = serde_json::json!(6.0);
        let view_value = serde_json::to_value(sample_view_state()).unwrap();
        // v30 の view には loop_region が無い。
        let mut view_value = view_value;
        view_value.as_object_mut().unwrap().remove("loop_region");
        let project =
            serde_json::json!({ "version": 30, "song": song_value, "view": view_value });
        std::fs::write(&path, serde_json::to_string(&project).unwrap()).unwrap();

        let loaded = load_project(&path).unwrap();
        assert_eq!(
            loaded.loop_region,
            LoopRegion { enabled: false, start_beat: 2.0, end_beat: 6.0 }
        );
        assert_eq!(loaded.view.unwrap().loop_region, loaded.loop_region);
        // 旧 view の他のフィールドは失われない。
        assert_eq!(load_project(&path).unwrap().view.unwrap().arrange_zoom_x, 37.5);
    }

    /// 旧 `save` (= `save_project(.., None)` への委譲) は view を書かない。
    #[test]
    fn plain_save_writes_no_view() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("no_view.daw");
        save(&path, &Song::default()).unwrap();
        assert_eq!(load_project(&path).unwrap().view, None);
    }

    /// version `v` の project file を `path` に書く (song は Rust で組んで serialize)。
    /// device-gated text overlay 移行 (v26) の version-gate を試すための helper。
    fn write_project_with_version(path: &Path, song: &Song, v: u32) {
        let song_value = serde_json::to_value(song).unwrap();
        let project = serde_json::json!({ "version": v, "song": song_value });
        std::fs::write(path, serde_json::to_string(&project).unwrap()).unwrap();
    }

    /// Text clip 1 つを持つ非 vocal トラック 1 本だけの song (字幕デバイス無し)。
    fn song_with_one_text_clip() -> Song {
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: "こんにちは".into(),
                    event_length_beats: 8.0,
                    ..TextEvent::default()
                }],
            }),
        );
        let mut track = Track {
            id: 1,
            name: "Subs".into(),
            ..Track::default()
        };
        track.clips.push(Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            ..Clip::default()
        });
        song.tracks.push(track);
        song
    }

    /// (v27): event 単位 mute だった clip を `Clip.muted` へ畳み込み、
    /// event 側の `muted` を false に戻す。
    #[test]
    fn migrate_per_event_mute_folds_into_clip_and_clears_event() {
        let mut song = song_with_one_text_clip();
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Text(t)) = song.clip_contents.get_mut(&cid) {
            t.events[0].muted = true;
        }
        migrate_per_event_mute_to_clip_mute(&mut song);
        assert!(
            song.tracks[0].clips[0].muted,
            "event が muted な clip は Clip.muted = true に畳み込まれる"
        );
        let Some(ClipContent::Text(t)) = song.clip_contents.get(&cid) else {
            panic!("expected Text content");
        };
        assert!(!t.events[0].muted, "畳み込み後は event.muted = false に戻る");
    }

    /// 元々 mute されていない clip は migration で変化しない。
    #[test]
    fn migrate_per_event_mute_leaves_unmuted_clip_untouched() {
        let mut song = song_with_one_text_clip();
        migrate_per_event_mute_to_clip_mute(&mut song);
        assert!(!song.tracks[0].clips[0].muted);
    }

    /// (§10) 旧 `sidechain_sources` を deserialize 前に `aux_inputs` (PostFader タップ) へ lift。
    #[test]
    fn migrate_legacy_sidechain_lifts_to_post_fader_aux() {
        let mut value = serde_json::json!({
            "tracks": [{
                "devices": [{
                    "plugin_id": "test.compressor",
                    "format": "Vst3",
                    "sidechain_sources": [1, null]
                }]
            }]
        });
        migrate_legacy_sidechain_to_aux(&mut value);
        let dev = &value["tracks"][0]["devices"][0];
        assert!(
            dev.get("sidechain_sources").is_none(),
            "legacy キーは drain される"
        );
        let aux: Vec<Option<AuxInputRoute>> =
            serde_json::from_value(dev["aux_inputs"].clone()).unwrap();
        assert_eq!(aux, vec![Some(AuxInputRoute::post_fader(1)), None]);
    }

    /// idempotent: `aux_inputs` が既にある object は lift せず旧キーを drain するだけ。
    #[test]
    fn migrate_legacy_sidechain_noop_when_aux_present() {
        let mut value = serde_json::json!({
            "devices": [{ "sidechain_sources": [5], "aux_inputs": [null] }]
        });
        migrate_legacy_sidechain_to_aux(&mut value);
        let dev = &value["devices"][0];
        assert!(dev.get("sidechain_sources").is_none());
        let aux: Vec<Option<AuxInputRoute>> =
            serde_json::from_value(dev["aux_inputs"].clone()).unwrap();
        assert_eq!(aux, vec![None], "既存 aux_inputs は不変");
    }

    /// (§10) 旧 3-split chain の `devices` への平坦化 + automation/binding の slot → device_index 解決。
    #[test]
    fn migrate_legacy_device_chains_flattens_and_resolves_slots() {
        let mut value = serde_json::json!({
            "song": {
                "tracks": [{
                    "id": 1,
                    "midi_fx_chain": [
                        { "plugin_id": "arp", "format": "Clap" },
                        { "plugin_id": "quant", "format": "Clap" }
                    ],
                    "instrument": { "plugin_id": "synth", "format": "Clap" },
                    "fx_chain": [
                        { "plugin_id": "comp", "format": "Clap" },
                        { "plugin_id": "reverb", "format": "Clap" }
                    ],
                    "automation_lanes": [
                        { "target": { "PluginParam": { "track": 1, "param_id": 5, "slot": "Instrument" } } },
                        { "target": { "PluginParam": { "track": 1, "param_id": 9, "slot": { "Fx": 1 } } } },
                        { "target": { "PluginParam": { "track": 1, "param_id": 2, "slot": { "MidiFx": 1 } } } }
                    ]
                }],
                "midi_bindings": [
                    { "target": { "PluginParam": { "track": 1, "param_id": 7, "slot": { "Fx": 0 } } } }
                ]
            }
        });
        migrate_legacy_device_chains(&mut value["song"]);
        let track = &value["song"]["tracks"][0];
        // flatten 順: midi_fx ++ instrument ++ fx。
        let ids: Vec<&str> = track["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["plugin_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["arp", "quant", "synth", "comp", "reverb"]);
        assert!(track.get("midi_fx_chain").is_none(), "旧 chain キーは drain");
        assert!(track.get("instrument").is_none());
        assert!(track.get("fx_chain").is_none());
        // lane slot → device_index (Instrument=n_midi=2 / Fx(1)=n_midi+has_inst+1=4 / MidiFx(1)=1)。
        let lanes = track["automation_lanes"].as_array().unwrap();
        let lane_idx = |i: usize| {
            lanes[i]["target"]["PluginParam"]["device_index"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(lane_idx(0), 2, "Instrument → 2");
        assert_eq!(lane_idx(1), 4, "Fx(1) → 4");
        assert_eq!(lane_idx(2), 1, "MidiFx(1) → 1");
        assert!(
            lanes[0]["target"]["PluginParam"].get("slot").is_none(),
            "slot キーは drain"
        );
        // binding slot → device_index (Fx(0) = n_midi+has_inst+0 = 3)。
        let binding = &value["song"]["midi_bindings"][0]["target"]["PluginParam"];
        assert_eq!(binding["device_index"].as_u64().unwrap(), 3, "Fx(0) → 3");
        assert!(binding.get("slot").is_none());
    }

    /// 新形式 (devices 既存) は平坦化しない (guard)。
    #[test]
    fn migrate_legacy_device_chains_noop_for_new_format() {
        let mut value = serde_json::json!({
            "song": { "tracks": [{
                "id": 1,
                "devices": [{ "plugin_id": "synth", "format": "Clap" }]
            }] }
        });
        migrate_legacy_device_chains(&mut value["song"]);
        let devices = value["song"]["tracks"][0]["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["plugin_id"].as_str().unwrap(), "synth");
    }

    /// (§10 bullet 4) 旧 .daw のフラットな media source マップ (Song 直下) を nested `media` へ移行。
    #[test]
    fn migrate_flat_media_to_pools_moves_top_level_maps() {
        let mut song = serde_json::json!({
            "audio_sources": { "1": {} },
            "video_sources": { "2": {} },
            "tracks": []
        });
        migrate_flat_media_to_pools(&mut song);
        assert!(song.get("audio_sources").is_none(), "フラットキーは media へ移動");
        assert!(song.get("video_sources").is_none());
        assert!(song["media"]["audio_sources"].get("1").is_some());
        assert!(song["media"]["video_sources"].get("2").is_some());
        // idempotent: 既に media があれば (新形式) no-op。
        let before = song.clone();
        migrate_flat_media_to_pools(&mut song);
        assert_eq!(song, before, "media 済みは no-op");
    }

    /// (§10 bullet 4) 旧 .daw のフラットな `next_*_id` カウンタ (Song 直下) を nested `ids` へ移行。
    #[test]
    fn migrate_flat_ids_to_allocators_moves_top_level_counters() {
        let mut song = serde_json::json!({
            "next_track_id": 5,
            "next_content_id": 9,
            "tracks": []
        });
        migrate_flat_ids_to_allocators(&mut song);
        assert!(song.get("next_track_id").is_none(), "フラットカウンタは ids へ移動");
        assert_eq!(song["ids"]["next_track_id"], 5);
        assert_eq!(song["ids"]["next_content_id"], 9);
        // idempotent: 既に ids があれば (新形式) no-op。
        let before = song.clone();
        migrate_flat_ids_to_allocators(&mut song);
        assert_eq!(song, before, "ids 済みは no-op");
    }

    /// (v30 §10) untagged content JSON へ `migrate_clip_content_add_tag` が正しい `type` を
    /// 注入することを全 variant + 空 content で検証する (旧 untagged 判別規則の再現)。
    #[test]
    fn migrate_clip_content_add_tag_disambiguates_all_variants() {
        let cases: &[(&str, serde_json::Value)] = &[
            ("Midi", serde_json::json!({ "notes": [] })),
            ("Automation", serde_json::json!({ "points": [] })),
            ("Audio", serde_json::json!({ "events": [{ "source_start_frames": 0 }] })),
            ("Video", serde_json::json!({ "events": [{ "source_start_micros": 0 }] })),
            ("Image", serde_json::json!({ "events": [{ "opacity": 1.0 }] })),
            ("Text", serde_json::json!({ "events": [{ "text": "hi" }] })),
            // 回帰 (実機 v28 project): TextEvent は `opacity` を持つ (テキストオーバーレイの
            // 不透明度)。text を opacity より先に判定しないと Image 誤判定 → ImageEvent の
            // source_id 欠落で load 失敗 (`missing field source_id`)。
            ("Text", serde_json::json!({ "events": [{ "text": "hi", "opacity": 0.8 }] })),
            // 空 content は旧 untagged で先頭 variant (Midi) に落ちていた。
            ("Midi", serde_json::json!({})),
            // 回帰 (sibling of opacity→Image bug): events を全削除した Audio content は
            // `events` が省略され `next_event_id` だけ残る (`{"next_event_id": N}`)。
            // 旧 untagged は Audio と解決していた。final else に落として Midi と誤判定すると
            // deny_unknown_fields で v29 project の load が失敗する。
            ("Audio", serde_json::json!({ "next_event_id": 3 })),
            // 同型: points を全削除した Automation content。
            ("Automation", serde_json::json!({ "next_point_id": 2 })),
        ];
        for (expected, content) in cases {
            let mut proj = serde_json::json!({
                "version": 29,
                "song": { "clip_contents": { "1": content } }
            });
            migrate_clip_content_add_tag(&mut proj);
            assert_eq!(
                proj["song"]["clip_contents"]["1"]["type"].as_str(),
                Some(*expected),
                "content {content:?} は type={expected} に判別される"
            );
        }
    }

    /// (v30 §10) idempotent: 既に `type` を持つ tagged content は上書きしない。
    #[test]
    fn migrate_clip_content_add_tag_is_idempotent() {
        let mut proj = serde_json::json!({
            "version": 30,
            "song": { "clip_contents": { "1": { "type": "Audio", "events": [] } } }
        });
        migrate_clip_content_add_tag(&mut proj);
        assert_eq!(proj["song"]["clip_contents"]["1"]["type"].as_str(), Some("Audio"));
    }

    // 注: untagged → tagged の実 load 経路 (migration → from_value) は既存の
    // `load_v6_clip_content_struct_form_deserializes_as_midi_variant` が既にカバーする
    // (v6 < CLIP_CONTENT_TAG_VERSION なので同 migration を通り、flat `{notes}` が Midi に載る)。

    #[test]
    fn migrate_v25_text_overlay_adds_subtitle_device() {
        // v25 (= device-gated text 以前) は Text overlay がトラック非依存で常時表示。
        // load 時に Text 持ちトラックへ字幕デバイスが補われ、表示が保たれる。
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.daw");
        write_project_with_version(&path, &song_with_one_text_clip(), 25);
        let song = load(&path).unwrap();
        assert!(
            song.tracks[0].has_subtitle_device(),
            "v25 の Text 持ちトラックへ字幕デバイスが auto-insert される"
        );
    }

    #[test]
    fn migrate_skips_when_subtitle_device_already_present() {
        // 既に字幕デバイスを持つ v25 トラックは二重挿入しない (idempotent)。
        let dir = tempdir().unwrap();
        let path = dir.path().join("old_with_dev.daw");
        let mut song = song_with_one_text_clip();
        song.tracks[0]
            .devices
            .push(crate::model::PluginInstance::with_ports(
                crate::plugin_db::SUBTITLE_ID.to_string(),
                crate::plugin_format::PluginFormat::Builtin,
                crate::port_config::PortConfig {
                    has_video_input: true,
                    has_video_output: true,
                    ..Default::default()
                },
            ));
        write_project_with_version(&path, &song, 25);
        let loaded = load(&path).unwrap();
        let n = loaded.tracks[0]
            .devices
            .iter()
            .filter(|d| d.plugin_id == crate::plugin_db::SUBTITLE_ID)
            .count();
        assert_eq!(n, 1, "字幕デバイスは二重挿入されない");
    }

    /// (talk) 実機 end-to-end 検証用の fixture 生成 (`docs/plan_voicevox_talk.md` §7)。
    /// VOICEVOX device 付きトラック + 読み上げテキストの Text clip を 1 本持つ .daw を
    /// `target/talk_fixture.daw` に書く。`daw_gui --script` がこれを load → exportWav し、
    /// 出力 WAV の非無音判定で「full pipeline で実際に喋る」を headless 検証する。
    /// 通常 test では不要なので `#[ignore]` (= 明示実行で再生成)。
    #[test]
    #[ignore = "fixture generator: writes target/talk_fixture.daw"]
    fn gen_talk_fixture() {
        use crate::model::{PluginInstance, TextContent, TextEvent};
        use crate::port_config::PortConfig;
        let mut song = Song { bpm: 120.0, length_beats: 16.0, ..Default::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: "こんにちは。これは読み上げのテストです。".into(),
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    ..TextEvent::default()
                }],
            }),
        );
        let mut track = Track {
            id: 1,
            name: "Talk".into(),
            ..Track::default()
        };
        // VOICEVOX builtin (instrument: note_in → audio_out)。= is_voicevox_vocal。
        track.devices.push(PluginInstance::with_ports(
            crate::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
            crate::plugin_format::PluginFormat::Builtin,
            PortConfig {
                has_note_input: true,
                has_audio_output: true,
                ..Default::default()
            },
        ));
        track.clips.push(Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            speaker_id: crate::voicevox::DEFAULT_TALK_SPEAKER_ID,
            ..Clip::default()
        });
        song.tracks.push(track);
        // workspace の target/ (= `common/target` ではない)。test の cwd は package root
        // なので相対 "target/..." だと存在しないディレクトリを指す。r.md #60 で
        // マシン固有の絶対パスを外したときにここを踏んだ。
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/talk_fixture.daw");
        save(&path, &song).unwrap();
        eprintln!("wrote talk fixture: {}", path.display());
    }

    #[test]
    fn no_migration_for_current_version_text_clip() {
        // v26 (= CURRENT) の Text 持ちトラックは migration 対象外。字幕デバイスを
        // 抜いた「喋るが映さない」トラックを誤って表示化しない。
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.daw");
        write_project_with_version(&path, &song_with_one_text_clip(), CURRENT_VERSION);
        let song = load(&path).unwrap();
        assert!(
            !song.tracks[0].has_subtitle_device(),
            "新規 (v26) の Text 持ちトラックには字幕デバイスを補わない"
        );
    }

    #[test]
    fn save_and_load_default_song() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        let song = Song::default();
        save(&path, &song).unwrap();
        let mut loaded = load(&path).unwrap();
        // v24: default の project_id は 0 (未採番)。save は 0 のまま書き、
        // load が `ensure_project_id` で採番するので loaded.project_id != 0。それ以外は
        // 一致するはず。project_id を 0 に戻してから残りを比較する。
        assert_ne!(loaded.project_id, 0, "load で project_id が採番される");
        loaded.project_id = 0;
        assert_eq!(loaded, song);
    }

    #[test]
    fn save_and_load_vocal_clip_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        // v6 形式で構築 (notes は clip_contents へ、 clip.notes は空)。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                next_note_id: 0,
                notes: vec![
                    Note {
                        id: 0,
                        start_beat: 0.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: Some("こ".into()),
                        muted: false,
                    },
                    Note {
                        id: 0,
                        start_beat: 1.0,
                        duration_beats: 0.5,
                        pitch: 62,
                        velocity: 100,
                        lyric: Some("ん".into()),
                        muted: false,
                    },
                ],
            }),
        );
        song.tracks.push(Track {
            id: 1,
            name: "Vocal".into(),
            source: InstrumentSource::Vocal,
            clips: vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 16.0,
                content_id: cid,
                content_offset_beats: 0.0,
                color: None,
                auto_lipsync: false,
                lipsync_gen: 0,
                muted: false,
                speaker_id: 3061,
                singer_name: "中国うさぎ".into(),
                style_name: "ノーマル".into(),
                talk: None,
            }],
            ..Track::default()
        });
        // load() runs `normalize_after_load` (ensure_ids + ensure_clip_
        // contents + sanitize + sorts). Apply the same normalization to
        // the original so the round-trip assert compares like-with-like
        // (idempotent — load runs it once more on read).
        song.normalize_after_load();
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap(), song);
    }

    #[test]
    fn migrate_old_vocal_source_bakes_voice_into_clips() {
        // 旧形式: track.source = {"Vocal": {speaker_id, style_name}}、
        // clip は声フィールド無し。 migration は source を unit "Vocal" に変換し、
        // 旧トラック声を全 clip へ焼き込む (新形式 clip = 既に speaker_id 持ちは尊重)。
        let mut value = serde_json::json!({
            "version": 2,
            "song": {
                "tracks": [{
                    "source": { "Vocal": { "speaker_id": 3061, "style_name": "へろへろ" } },
                    "clips": [
                        { "id": 1 },
                        { "id": 2, "speaker_id": 7, "style_name": "あまあま" }
                    ]
                }]
            }
        });
        migrate_vocal_source_to_clips(&mut value);
        let track = &value["song"]["tracks"][0];
        // source は unit "Vocal" (JSON string) に。
        assert_eq!(track["source"], serde_json::json!("Vocal"));
        // clip 1 (声無し): 旧トラック声を焼き込み。
        assert_eq!(track["clips"][0]["speaker_id"], 3061);
        assert_eq!(track["clips"][0]["style_name"], "へろへろ");
        // clip 2 (既に speaker_id 持ち): 尊重して上書きしない。
        assert_eq!(track["clips"][1]["speaker_id"], 7);
        assert_eq!(track["clips"][1]["style_name"], "あまあま");
    }

    #[test]
    fn migrate_is_noop_for_new_unit_vocal_source() {
        // 新形式 (source が既に string "Vocal") は no-op。
        let mut value = serde_json::json!({
            "version": 2,
            "song": { "tracks": [{ "source": "Vocal", "clips": [{ "id": 1 }] }] }
        });
        let before = value.clone();
        migrate_vocal_source_to_clips(&mut value);
        assert_eq!(value, before);
    }

    #[test]
    fn load_v6_clip_content_struct_form_deserializes_as_midi_variant() {
        // v6 saves stored `ClipContent` as a flat struct
        // `{ "notes": [...] }`. v7 promotes `ClipContent` to an enum
        // `Midi(MidiContent) | Audio(AudioContent)` with
        // `#[serde(untagged)]` so the legacy struct form deserialises
        // straight into `Midi(MidiContent { notes })`.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v6.daw");
        let v6_json = r#"{
            "version": 6,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "next_track_id": 2,
                "next_content_id": 2,
                "clip_contents": {
                    "1": {
                        "notes": [
                            {"start_beat": 0.0, "duration_beats": 1.0, "pitch": 60, "velocity": 100}
                        ]
                    }
                },
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 2,
                        "clips": [
                            {
                                "id": 1,
                                "name": "C",
                                "start_beat": 0.0,
                                "length_beats": 4.0,
                                "content_id": 1
                            }
                        ]
                    }
                ]
            }
        }"#;
        fs::write(&path, v6_json).unwrap();
        let song = load(&path).expect("v6 must forward-migrate to v7");
        let content = song
            .clip_contents
            .get(&1)
            .expect("v6 content_id 1 must round-trip");
        let notes = content
            .notes()
            .expect("legacy struct form must deserialise as Midi variant");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 60);
        // audio_sources defaults to empty for v6 files.
        assert!(song.media.audio_sources.is_empty());
        assert!(song.ids.next_audio_source_id >= 1);
    }

    #[test]
    fn load_v5_migrates_clip_notes_to_clip_contents() {
        // v5 saves stored notes per-`Clip` directly. After load, the
        // legacy `notes` vector must be drained into `clip_contents` and
        // a fresh `content_id` allocated.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v5.daw");
        let v5_json = r#"{
            "version": 5,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 2,
                        "clips": [
                            {
                                "id": 1,
                                "name": "C",
                                "start_beat": 0.0,
                                "length_beats": 4.0,
                                "notes": [
                                    {"start_beat": 0.0, "duration_beats": 1.0, "pitch": 60, "velocity": 100}
                                ]
                            }
                        ]
                    }
                ]
            }
        }"#;
        fs::write(&path, v5_json).unwrap();
        let song = load(&path).expect("v5 must forward-migrate");
        let clip = &song.tracks[0].clips[0];
        assert_ne!(clip.content_id, 0, "ensure_clip_contents must allocate");
        // 旧 inline `Clip.notes` は §10 で撤去され、前処理が clip_contents(Midi) へ移す。
        let content = song
            .clip_contents
            .get(&clip.content_id)
            .expect("content_id must have an entry after migration");
        let notes = content.notes().expect("legacy Clip.notes must migrate to Midi variant");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 60);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        save(&path, &Song::default()).unwrap();
        assert!(path.exists());
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn save_overwrites_existing_file_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        let mut song = Song::default();
        save(&path, &song).unwrap();

        song.bpm = 140.0;
        save(&path, &song).unwrap();

        assert_eq!(load(&path).unwrap().bpm, 140.0);
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn load_rejects_newer_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("future.daw");
        let future = ProjectFile {
            version: CURRENT_VERSION + 1,
            song: Song::default(),
            view: None,
        };
        fs::write(&path, serde_json::to_string(&future).unwrap()).unwrap();

        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("newer"), "unexpected error: {err}");
    }

    #[test]
    fn load_accepts_v4_with_default_routing_fields() {
        // v4 saves had no `kind` / `parent_group_id` keys on each `Track`.
        // Loading must succeed and fill those fields with their serde
        // defaults (Audio / None).
        let dir = tempdir().unwrap();
        let path = dir.path().join("v4.daw");
        let v4_json = r#"{
            "version": 4,
            "song": {
                "bpm": 140.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 0.85,
                        "pan": 0.0,
                        "next_clip_id": 1
                    }
                ]
            }
        }"#;
        fs::write(&path, v4_json).unwrap();
        let song = load(&path).expect("v4 must forward-migrate");
        assert_eq!(song.bpm, 140.0);
        assert_eq!(song.tracks.len(), 1);
        let t = &song.tracks[0];
        assert_eq!(t.parent_group_id, None);
    }

    #[test]
    fn load_accepts_v18_with_default_group_transform() {
        // v18 saves had no `group_transform` key on each `Track`. Loading
        // must succeed and fill it with the serde default (`None`), proving
        // the v19 field is forward-compatible (enum 末尾追加 = forward-migrate
        // のみ)。See `docs/plan_tachie_group_transform.md` §4.5.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v18.daw");
        let v18_json = r#"{
            "version": 18,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Char A",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 1,
                        "color": [0.5, 0.5, 0.5]
                    }
                ]
            }
        }"#;
        fs::write(&path, v18_json).unwrap();
        let song = load(&path).expect("v18 must forward-migrate");
        assert_eq!(song.tracks.len(), 1);
        assert_eq!(song.tracks[0].group_transform, None);
    }

    #[test]
    fn load_v23_assigns_project_id_and_persists() {
        // v23 saves had no `project_id`. Loading must succeed and assign a fresh
        // non-zero id (`ensure_project_id`), and re-saving must persist it so the
        // next load returns the SAME id (`docs/plan_fixme_33_clipboard.md`).
        let dir = tempdir().unwrap();
        let path = dir.path().join("v23.daw");
        let v23_json = r#"{
            "version": 23,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": []
            }
        }"#;
        fs::write(&path, v23_json).unwrap();
        let song = load(&path).expect("v23 must forward-migrate");
        assert_ne!(song.project_id, 0, "load で project_id が採番される");
        let id = song.project_id;
        // re-save → re-load で同一 id が保たれる (ensure_project_id は非 0 を上書きしない)。
        save(&path, &song).unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.project_id, id, "save/load で project_id が保たれる");
    }

    #[test]
    fn load_rejects_legacy_row_based_version() {
        // Version 1 was the row-based format; we no longer support it.
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.daw");
        fs::write(
            &path,
            r#"{"version":1,"song":{"bpm":120.0,"time_sig":[4,4],"length_beats":64.0}}"#,
        )
        .unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("retired"), "unexpected error: {err}");
    }

    /// v33: マスター音量が保存され、開き直すと復元されること。
    /// 旧 `.daw` (フィールド無し) は unity で読めること。
    #[test]
    fn master_gain_round_trips_and_defaults_to_unity_for_old_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gain.daw");
        let song = Song {
            master_gain: 0.5,
            ..Default::default()
        };
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap().master_gain, 0.5);

        // v32 以前は master_gain キーを持たない。
        let old = dir.path().join("v32.daw");
        fs::write(
            &old,
            r#"{"version":32,"song":{"bpm":120.0,"time_sig":[4,4],"length_beats":64.0}}"#,
        )
        .unwrap();
        assert_eq!(
            load(&old).unwrap().master_gain,
            1.0,
            "旧ファイルの聞こえ方が変わってはいけない"
        );
    }

    #[test]
    fn load_rejects_invalid_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid.daw");
        fs::write(&path, "not valid json {").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn load_fails_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.daw");
        assert!(load(&path).is_err());
    }

    #[test]
    fn tmp_path_appends_tmp_suffix() {
        assert_eq!(
            tmp_path(Path::new("project.daw")),
            PathBuf::from("project.daw.tmp")
        );
        assert_eq!(tmp_path(Path::new("noext")), PathBuf::from("noext.tmp"));
    }
}
