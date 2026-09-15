//! handler::clips — clip の resize/stretch/duplicate/create/delete (分割は `handler::split`)
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use common::model::{Clip, ClipContent};

impl AppData {
    /// Clip の左右端 trim ハンドラ。 caller (arrangement widget) は
    /// `ResizeClipDelta { prev_start, next_start, prev_len, next_len }`
    /// から `next_start` / `next_len` を直接渡す。
    ///
    /// **r.md #44: trim は clip 側 3 フィールドだけを書き換え、content には一切触れない。**
    /// clip は共有 content への「窓」 (`[content_offset_beats, +length_beats)`) であり、
    /// 再生も描画もこの窓で crop する (`docs/plan_clip_content_window.md`)。 これで
    /// `content_id` を共有する linked clip の開始・終了が完全に独立する。
    ///
    /// - 右端 trim: `length_beats` のみ
    /// - 左端 trim: `start_beat += δ`, `length_beats -= δ`, `content_offset_beats += δ`
    ///   (= 中身は song 上の同じ位置に留まり、左端より前が隠れる)
    /// - 左端を外へ伸ばすと `content_offset_beats` は負にもなる (= 先頭に空白が付く)。
    ///   content は無傷なので、伸ばし直せば隠れていた中身がそのまま復帰する。
    ///
    /// 旧実装は audio だけ `source_start/end_frames` を動かして辻褄を合わせていた
    /// (`trim_audio_event`)。 これは (a) 共有 content を壊すので linked clip を巻き込み、
    /// (b) warp marker / slice onset を持つ event では source 窓を動かすこと自体が
    /// 写像を壊す、 の 2 点で誤りだった。
    ///
    /// `stretch == true` (Shift + 端 drag) は trim ではなく
    /// **time-stretch** (= 内容を新 clip 長に伸縮)。 `stretch_clip_content` 参照。
    pub(crate) fn resize_clip(
        &mut self,
        target: ClipKey,
        new_start_beat: f64,
        new_length_beats: f64,
        stretch: bool,
    ) {
        // r.md #68: 下限は widget の drag preview と共有する (`MIN_CLIP_LEN_BEATS`)。
        // 別リテラルにすると最小長付近で「ゴーストより短く確定する」 ずれが出る。
        let new_length_beats = new_length_beats.max(common::model::MIN_CLIP_LEN_BEATS);
        let new_start_beat = new_start_beat.max(0.0);
        let Some(Some((content_id, prev_start_beat, prev_length_beats, new_offset))) =
            self.edit_song(|song| {
                let track = song.track_by_id_mut(target.track_id)?;
                let clip = track.clip_by_id_mut(target.clip_id)?;
                let prev_start_beat = clip.start_beat;
                let prev_length_beats = clip.length_beats;
                // 左端の移動量ぶんだけ窓を content 上で進める (右端 drag は δ=0)。
                let delta_start = new_start_beat - prev_start_beat;
                clip.start_beat = new_start_beat;
                clip.length_beats = new_length_beats;
                clip.content_offset_beats += delta_start;
                let out = (
                    clip.content_id,
                    prev_start_beat,
                    prev_length_beats,
                    clip.content_offset_beats,
                );
                // 伸ばした先に居た隣のクリップを上書き規則で削る (自分自身は除く)。
                // `Track.clips` の非重なり不変条件は trim でも保たれる。
                track.carve_clip_range(
                    new_start_beat,
                    new_start_beat + new_length_beats,
                    Some(target.clip_id),
                );
                Some(out)
            })
        else {
            return;
        };

        // Shift + 端 drag = time-stretch。 content を新 clip 長に
        // 伸縮し (audio は source 窓固定で event 長変更 + Raw→Stretch 昇格、
        // MIDI は note を比例 scale)、 trim とは別経路で処理する。
        if stretch {
            self.stretch_clip_content(
                target,
                content_id,
                prev_start_beat,
                prev_length_beats,
                new_start_beat,
                new_length_beats,
            );
            return;
        }

        // overlay clip (image / video / text) は「clip 長 = 表示長」が
        // 不変条件。 Audio/Midi では no-op、 overlay の末尾 event だけ窓の末尾
        // (`offset + length`) まで extend する (extend-only / idempotent)。
        // 共有 content を伸ばすが extend-only なので linked clip は自分の窓で
        // clamp され無害 (`ensure_event_covers_clip` の契約)。
        self.edit_song(|song| {
            if let Some(content) = song.clip_contents.get_mut(&content_id) {
                content.ensure_event_covers_clip(new_offset + new_length_beats);
            }
        });
    }

    /// Shift + 端 drag = time-stretch。 clip 内容を新 clip 長に伸縮する。
    /// audio は source 窓 (`source_start/end_frames`) を **固定**して event 長のみ
    /// 変え (engine が `stretch_ratio = native/event 長` で warp 再生)、 Raw は
    /// pitch 保持の `Stretch` (granular) へ昇格 (= ピッチ保持が既定)。 MIDI は
    /// note の `start_beat` / `duration_beats` を比例 scale。 共有 content は fork
    /// してから伸縮し linked siblings (= 別 length) を巻き込まない。 pivot は
    /// 固定端 (右端 drag = 左端固定 / 左端 drag = 右端固定)。
    pub(crate) fn stretch_clip_content(
        &mut self,
        target: ClipKey,
        content_id: common::model::ContentId,
        prev_start: f64,
        prev_len: f64,
        new_start: f64,
        new_len: f64,
    ) {
        if prev_len <= 1e-9 || new_len <= 1e-9 {
            return;
        }
        // r.md #44: content 内の位置は **窓の起点** (`content_offset_beats`) 基準で
        // 出し入れする。 `resize_clip` が先に offset を更新済みなので、ここで読む
        // `new_off` が新しい窓の起点、`prev_off` が伸縮前の起点。
        let new_off = self
            .cur.song_doc
            .song()
            .track_by_id(target.track_id)
            .and_then(|t| t.clip_by_id(target.clip_id))
            .map_or(0.0, |c| c.content_offset_beats);
        let prev_off = new_off - (new_start - prev_start);
        // 共有 content は fork してから伸縮 (siblings の length と無関係)。
        let content_id = if self.cur.song_doc.song().clip_content_refcount(content_id) > 1 {
            self.edit_song(|song| {
                let new_id = song.fork_content(content_id);
                if let Some(clip) = song
                    .track_by_id_mut(target.track_id)
                    .and_then(|t| t.clip_by_id_mut(target.clip_id))
                {
                    clip.content_id = new_id;
                }
                new_id
            })
            .unwrap_or(content_id)
        } else {
            content_id
        };

        self.edit_song(|song| {
            match song.clip_contents.get_mut(&content_id) {
                Some(ClipContent::Audio(audio)) => {
                    for e in &mut audio.events {
                        let (s, l) = stretch_remap(
                            prev_start,
                            prev_len,
                            new_start,
                            new_len,
                            e.event_start_in_clip_beats - prev_off,
                            e.event_length_beats,
                        );
                        e.event_start_in_clip_beats = new_off + s;
                        e.event_length_beats = l;
                        // ピッチ保持を既定: Raw (= 時間操作しない定義) は Stretch
                        // (granular) へ昇格。 既に Repitch/Stretch/Slice なら維持。
                        if e.stretch_mode == common::model::StretchMode::Raw {
                            e.stretch_mode = common::model::StretchMode::Stretch;
                        }
                        // source 窓は固定 = これが stretch の本質。
                    }
                }
                Some(ClipContent::Midi(midi)) => {
                    for n in &mut midi.notes {
                        let (s, l) = stretch_remap(
                            prev_start,
                            prev_len,
                            new_start,
                            new_len,
                            n.start_beat - prev_off,
                            n.duration_beats,
                        );
                        n.start_beat = new_off + s;
                        n.duration_beats = l;
                    }
                }
                other => {
                    // overlay / automation は stretch 概念なし → 長さ追従のみ。
                    if let Some(content) = other {
                        content.ensure_event_covers_clip(new_off + new_len);
                    }
                }
            }
        });
    }

    /// Make Unique (右クリック): 共有 clip → 独立化。 §3.6。
    ///
    /// r.md #14: 右クリックした clip が現在の複数選択に含まれるなら **選択した
    /// 全 clip** を対象にする (含まれないなら右クリックした 1 つだけ — Reverse
    /// 等の単一操作と同じ直感)。 各 clip の content を per-clip で fork するので、
    /// 選択内で互いに linked だった clip も全て独立になる。 1 回の `edit_song` に
    /// まとめて 1 undo step。 既に全て独立なら `edit_song_checked` の no-op 検出で
    /// dirty 化しない。
    pub(crate) fn make_clip_unique(&mut self, target: ClipKey) {
        // 対象集合: 複数選択があれば選択全体を、 無ければ右クリックした clip 単体を
        // 独立化する (Auto-Fade / Auto-Crossfade と同じ「選択集合に効く」idiom)。
        let selected = self.selected_clip_refs();
        let targets: Vec<ClipKey> = if selected.is_empty() { vec![target] } else { selected };
        // status message 用: 「他と共有していて独立化される」clip 数を編集前に数える。
        // (逐次 fork では共有群の最後の 1 つは fork せず独立になるので、 fork 回数だと
        // 1 つ少なく報告してしまう。 元の refcount で「独立化される clip」を数える。)
        let made_unique = targets
            .iter()
            .filter(|t| {
                self.cur.song_doc
                    .song()
                    .track_by_id(t.track_id)
                    .and_then(|tr| tr.clip_by_id(t.clip_id))
                    .is_some_and(|c| self.cur.song_doc.song().clip_content_refcount(c.content_id) >= 2)
            })
            .count();
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in &targets {
                let Some(content_id) = song
                    .track_by_id(t.track_id)
                    .and_then(|tr| tr.clip_by_id(t.clip_id))
                    .map(|c| c.content_id)
                else {
                    continue;
                };
                // 他 clip と共有していなければ既に独立 → fork 不要。 逐次 fork で
                // 参照数が減るので、 選択内で共有していた最後の 1 つは自然に独立扱い。
                if song.clip_content_refcount(content_id) <= 1 {
                    continue;
                }
                let new_content_id = song.fork_content(content_id);
                if let Some(clip) = song
                    .track_by_id_mut(t.track_id)
                    .and_then(|tr| tr.clip_by_id_mut(t.clip_id))
                {
                    clip.content_id = new_content_id;
                    changed = true;
                }
            }
            changed
        });
        self.ui_ephemeral.status_message = match made_unique {
            0 => "すでに独立 clip です".to_string(),
            1 => "Clip を独立化しました".to_string(),
            n => format!("{n} 個のクリップを独立化しました"),
        };
    }

    pub(crate) fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        let Some(Some(r)) = self.edit_song(|song| {
            // Allocate the shared content slot first so the new clip points
            // at a real entry. Orphan content_ids (if track lookup below
            // fails) get reclaimed by `Song::gc_clip_contents` before save.
            let content_id = song.alloc_content_id();
            song.clip_contents.insert(content_id, ClipContent::default());
            let track = song.tracks.get_mut(track_idx as usize)?;
            let track_id = track.id;
            let (speaker_id, singer_name, style_name) = inherited_voice(track);
            let new_clip_id = track.place_clip(Clip {
                id: 0,
                start_beat,
                length_beats: DEFAULT_CLIP_LENGTH,
                content_id,
                // 新規 clip は content 先頭から見せる。
                content_offset_beats: 0.0,
                // 新規クリップにクロスフェードの張り出しは無い。
                xfade_lead_beats: 0.0,
                xfade_tail_beats: 0.0,
                color: None,
                auto_lipsync: false,
                lipsync_gen: 0,
                muted: false,
                speaker_id,
                singer_name,
                style_name,
                // (talk) 新規 clip は読み上げスケール未設定 (= 全既定)。
                talk: None,
            });
            // デフォルトでクリップ名は無し (= content_name 未設定)。 表示名は
            // arrangement_view::clip_display_label が内容 (Text 本文 / ノート歌詞)
            // から導出する。 ユーザーが Rename したときだけ明示名が入る。
            Some(ClipKey { track_id, clip_id: new_clip_id })
        }) else {
            return;
        };
        self.set_single_clip_selection(r);
        self.select_track(r.track_id);
    }

    /// ランチャー (セッション) のセルを削除する (`Delete` / `Ctrl+X`)。
    ///
    /// アレンジのクリップは**範囲操作**で消える (`apply_delete_time_selection`)。
    /// セルはグリッドに時間軸が無く範囲では表せないので、唯一のオブジェクト選択
    /// (`selected_launcher_cells`) をそのまま対象にする。
    ///
    /// 実処理は [`AppData::delete_launcher_cells`] 1 本へ委譲する — トラック行の
    /// セルとレーン行のセルで消し方が割れていると、両方を選んだ `Delete` が
    /// 片方しか消さない (以前はここがトラック行のセルしか見ていなかった)。
    /// 対象面の解決は呼び側が済ませている前提なので、ここでは面タグを見ない
    /// [`AppData::live_launcher_cells`] を使う。
    pub(crate) fn delete_selected_clip(&mut self) {
        let cells = self.live_launcher_cells();
        self.delete_launcher_cells(&cells);
    }
}

/// 新規クリップ / セルが引き継ぐ声 (VOICEVOX)。
///
/// vocal トラックなら同トラックの直前 (= `start_beat` 最大の既存クリップ、
/// **セルも含む**) の声、無ければアプリ既定。非 vocal トラックは未設定 (0)。
/// アレンジの新規クリップとランチャーの空セルで**同じ 1 本**を通す — 別々に
/// 書くと「同じトラックの同じ操作なのに、帯で作ったセルだけ歌わない」になる。
pub(crate) fn inherited_voice(track: &common::model::Track) -> (u32, String, String) {
    if !track.is_voicevox_vocal() {
        return (0, String::new(), String::new());
    }
    track
        .all_clips()
        .filter(|c| c.speaker_id != 0)
        .max_by(|a, b| a.start_beat.total_cmp(&b.start_beat))
        .map(|c| (c.speaker_id, c.singer_name.clone(), c.style_name.clone()))
        .unwrap_or_else(|| {
            (
                common::voicevox::DEFAULT_SINGER_ID,
                common::voicevox::DEFAULT_SINGER_NAME.to_string(),
                common::voicevox::DEFAULT_STYLE_NAME.to_string(),
            )
        })
}
