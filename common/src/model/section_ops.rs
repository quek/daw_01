//! Arranger セクション帯の破壊的編集 (移動 / 複製 / 削除) と、その土台になるタイムライン
//! ripple ([`Ripple`] / [`Song::ripple_timeline`]) と境界での clip 分割 ([`Song::split_clips_at`])。
//! 時間範囲操作 (`time_ops.rs`) も同じ ripple / 分割を通る。
//!
//! 型 ([`Section`]) は wire を渡るので `sections.rs` に置き、ここは wire に載らないロジック
//! だけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use super::*;

/// タイムライン ripple 1 回分 — 「`from_beat` 以降の全ての時間位置を `delta` ずらす」。
///
/// セクションの移動 / 複製 / 範囲削除は Song 内の時間位置をこの規則でずらす
/// ([`Song::ripple_timeline`])。 ループ範囲のように **`Song` の外に住む時間位置**
/// を同じ規則で追従させるため、 ripple を行う `Song` メソッドは適用した ripple 列を
/// 返す (呼び出し側が幾何を再計算する「補償コード」 を書かせない)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ripple {
    pub from_beat: f64,
    pub delta: f64,
}

impl Ripple {
    /// 1 つの拍位置に適用する。 結果は `0.0` 以上に clamp。
    pub fn shift(&self, beat: &mut f64) {
        if *beat >= self.from_beat {
            *beat = (*beat + self.delta).max(0.0);
        }
    }
}

/// r.md #71: セクション帯を `desired_start` (= **移動後の座標系** = ドラッグ中に画面で
/// 見えている開始拍) へ落とすときに、**実際に着地する開始拍**を返す。
///
/// [`Song::move_section`] は帯の範囲 `[a,b)` を ripple-close で詰め、落とし先に
/// ripple-open で空けて置き直す。 open は「落とし先以降」 を右へ逃がすので、
/// 帯と重なりうるのは **落とし先より前から始まる帯** だけ。 そこへ食い込む位置を
/// 指していたら、近い方の境界へ寄せる (Studio One の insert-before / replace 相当)。
/// 重ならなければ `desired_start` をそのまま返す = 通常のドラッグの感触は変わらない。
///
/// **preview (widget の ghost) と commit がこの 1 本を共有する**のが要件。 片方だけ
/// 解決すると「見えていた位置と違う所に落ちる」 という別のバグになる。 また合法な位置は
/// 素通しなので **冪等** で、 preview 側で解決済みの値を `move_section` に渡しても
/// 二重補正にならない。
///
/// `others` は **移動する帯を除いた** 現在の帯の `(start_beat, len_beats)` 列
/// (現在の座標系のまま渡す。 close 後の位置はこの関数が内部で導出する)。
/// 帯は非重複なので食い込む相手は高々 1 つ。
///
/// 参考: Studio One の Arranger Track はタイムライン上の位置へドラッグして落とし、
/// ripple が隙間を詰める。落とし先の帯を「置き換える / 前後に挿入する」 のどれになるかは
/// ポインタ位置で決まり、タグで予告される
/// (<https://www.soundonsound.com/techniques/studio-one-making-arrangements>)。
#[must_use]
pub fn resolve_section_move_dest<I>(
    others: I,
    moved_start: f64,
    moved_len: f64,
    desired_start: f64,
) -> f64
where
    I: IntoIterator<Item = (f64, f64)>,
{
    if moved_len <= 0.0 {
        return desired_start.max(0.0);
    }
    // close で帯を抜いたぶん、`b` 以降の帯は左へ詰まる。 それが drop 時点の配置。
    let b = moved_start + moved_len;
    resolve_section_drop_start(
        others.into_iter().map(|(start, len)| {
            (if start >= b { start - moved_len } else { start }, len)
        }),
        desired_start,
    )
}

/// r.md #71: 帯を `desired_start` へ落とすときに、**実際に着地する開始拍**を返す core。
///
/// `existing` は **drop 時点で存在する帯**の `(start_beat, len_beats)` 列。
/// ripple-open は「落とし先以降」 を右へ逃がすので、置いた帯と重なりうるのは
/// **落とし先より前から始まる帯**だけ。 そこへ食い込む位置を指していたら近い方の
/// 境界へ寄せる。 重ならなければ `desired_start` をそのまま返す (= 素通し・冪等)。
///
/// 移動 ([`resolve_section_move_dest`]) と複製 ([`Song::duplicate_section`]) の違いは
/// **`existing` の中身だけ**: 移動は「自分を除き、close で詰まった位置」、
/// 複製は「全帯を現在位置のまま」 (close しないので元帯も障害物になる)。
#[must_use]
pub fn resolve_section_drop_start<I>(existing: I, desired_start: f64) -> f64
where
    I: IntoIterator<Item = (f64, f64)>,
{
    let dest = desired_start.max(0.0);
    for (start, len) in existing {
        if start < dest && start + len > dest {
            // dest がこの帯の内側 = そのままでは重なる。近い方の端へ寄せる。
            let (lo, hi) = (start, start + len);
            return if dest - lo <= hi - dest { lo } else { hi };
        }
    }
    dest
}

/// r.md #71: 「帯が動いた / 動かなかった」 を判定する拍スケールの許容差。
/// 落とし先は算術で導かれるので、元位置へ寄せ戻された場合でも bit 一致しない。
const SECTION_MOVE_EPS_BEATS: f64 = 1e-9;

impl Song {
    /// `sections` の invariant を回復する: `start_beat` 昇順、互いに非交差
    /// (重複なし、隙間は許容)、`len_beats > 0`。セクションを追加 / 移動 / リサイズした
    /// あとに呼ぶ。重複は「先に始まる方を優先」 (= 後発の `start_beat` を直前 section の
    /// `end_beat` までクランプして隙間化) して解消し、長さが `0` 以下になった section は
    /// 破棄する。idempotent。
    pub fn normalize_sections(&mut self) {
        self.sections.sort_by(|a, b| {
            a.start_beat
                .partial_cmp(&b.start_beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut prev_end = f64::NEG_INFINITY;
        for s in &mut self.sections {
            if s.start_beat < prev_end {
                let end = s.end_beat();
                s.start_beat = prev_end;
                s.len_beats = (end - prev_end).max(0.0);
            }
            prev_end = s.end_beat();
        }
        self.sections.retain(|s| s.len_beats > f64::EPSILON);
    }

    /// タイムライン全体の ripple シフト。
    /// `from_beat` 以降の全ての時間位置を `delta` だけずらす (結果は `0.0` 以上に clamp)。
    /// 破壊的セクション移動の close (`delta < 0` = 範囲を詰める) / open (`delta > 0` =
    /// 範囲を空ける) プリミティブ。 対象は全トラックの clip 位置、各トラックと `song_lanes`
    /// の automation clip 位置、`scale_changes`、`sections`、`length_beats`。
    /// clip 内の note / event / point は clip-local なので動かさない (clip 位置だけずらせば
    /// 中身は付いてくる = 歌声キャッシュ key 不変、再合成不要)。 シフト後に scale / sections
    /// の invariant を復元する。
    ///
    /// 戻り値は適用した [`Ripple`]。 ループ範囲のように **`Song` の外に住む時間位置**
    /// (session state + `ViewState`) を同じ規則で追従させるために返す。
    pub fn ripple_timeline(&mut self, from_beat: f64, delta: f64) -> Ripple {
        self.ripple_timeline_with(from_beat, delta, true)
    }

    /// [`Song::ripple_timeline`] の本体。`shift_sections = false` で **セクション帯だけ
    /// 動かさない** — 範囲削除 ([`Song::delete_time_range`]) は帯を「重なりぶん縮める」
    /// 規則で先に計算し終えているので、その上から始点だけずらすと二重に動く。
    pub(crate) fn ripple_timeline_with(
        &mut self,
        from_beat: f64,
        delta: f64,
        shift_sections: bool,
    ) -> Ripple {
        let r = Ripple { from_beat, delta };
        for t in &mut self.tracks {
            for c in &mut t.clips {
                r.shift(&mut c.start_beat);
            }
            for lane in &mut t.automation_lanes {
                for c in &mut lane.clips {
                    r.shift(&mut c.start_beat);
                }
            }
        }
        for lane in &mut self.song_lanes {
            for c in &mut lane.clips {
                r.shift(&mut c.start_beat);
            }
        }
        for sc in &mut self.scale_changes {
            r.shift(&mut sc.beat);
        }
        if shift_sections {
            for s in &mut self.sections {
                r.shift(&mut s.start_beat);
            }
        }
        if self.length_beats >= from_beat {
            self.length_beats = (self.length_beats + delta).max(0.0);
        }
        self.ensure_scale_changes_sorted();
        self.normalize_sections();
        r
    }

    /// セクション帯を `dest_start` へ
    /// 破壊的に移動し、 曲構成を組み替える (Studio One 流の能動アレンジャー)。 帯の範囲
    /// `[a, b)` 内の全トラック clip + automation + `song_lanes` automation + `scale_changes`
    /// を帯と一緒に取り出し、 `[a,b)` を ripple-close で詰め、 落とし先に ripple-open で
    /// 空けて落とし直す。 他セクション / 他 clip は ripple で前後に流れる。
    ///
    /// # `dest_start` の意味 (r.md #71 で契約を変更)
    ///
    /// **`dest_start` は「移動後の帯の開始拍」** = ドラッグ中に画面で見えている位置。
    /// 帯が置けない位置 (他帯に食い込む) を指していたら
    /// [`resolve_section_move_dest`] が近い方の境界へ寄せるので、
    /// **実際の着地位置は `resolve_section_move_dest(..)` の戻り値** になる。
    /// preview 側も同じ関数を通すことで overlay == commit が構造的に保たれる。
    ///
    /// 旧契約は「`[a,b)` を close した **中間座標系** の絶対拍」 で、 前へ動かすときだけ
    /// `dest_start - len` の逆算を呼び出し側に強いていた。 その結果
    /// 「1 つ先の帯まで引っ張らないと届かない / 隣へ落とすと元に戻る」 という
    /// **1 セクションぶんのズレ**になっていた (r.md #71 のユーザー報告そのもの)。
    /// 中間座標系はこの関数の内部事情であって、 ユーザーが指しているものではない。
    ///
    /// 戻り値は適用した [`Ripple`] 列 (**空 = 移動しなかった**)。 呼び出し側は Song の外に
    /// 住む時間位置 (session state のループ範囲) をこれで追従させる。
    ///
    /// 境界をまたぐ clip は移動前に `split_clips_at(a)` / `split_clips_at(b)` で分割するので、
    /// 帯範囲ぴったりの content だけが追従する (Studio One の split-at-boundary)。 残りの
    /// content / 他セクションは ripple で前後に流れる。
    pub fn move_section(&mut self, section_id: u32, dest_start: f64) -> Vec<Ripple> {
        let Some(sec) = self.sections.iter().find(|s| s.id == section_id).cloned() else {
            return Vec::new();
        };
        let (a, len) = (sec.start_beat, sec.len_beats);
        let b = a + len;
        // r.md #71: 落とし先は「移動後の帯の開始拍」。 置けない位置は境界へ寄せる。
        // preview (widget) も同じ関数を通すので、 見えていた位置に落ちる。
        // 既に解決済みの値を渡されても冪等 (合法な位置は素通し) なので二重補正にならない。
        let dest_start = resolve_section_move_dest(
            self.sections.iter().filter(|s| s.id != section_id).map(|s| (s.start_beat, s.len_beats)),
            a,
            len,
            dest_start,
        );
        // 「動かなかった」 判定は **拍スケールの許容差**で見る。 解決後の落とし先は
        // `start - moved_len` 等の算術で導くので、 元位置へ寄せ戻された場合でも `a` と
        // bit 一致するとは限らない (小数拍の帯だと 1e-15 ずれる)。 `f64::EPSILON` だと
        // それをすり抜け、 見た目 no-op の drag で clip 分割 + undo + dirty が走る。
        if len <= 0.0 || (dest_start - a).abs() < SECTION_MOVE_EPS_BEATS {
            return Vec::new();
        }
        let in_range = |start: f64| start >= a && start < b;

        // 0. 境界をまたぐ clip を a / b で分割し、 以降の membership 抽出を正確にする。
        self.split_clips_at(a);
        self.split_clips_at(b);

        // 1. 範囲内の content を取り出し、 帯先頭 (a) 基準のローカル位置に正規化。
        let mut taken_clips: Vec<(u32, Clip)> = Vec::new();
        for t in &mut self.tracks {
            let mut i = 0;
            while i < t.clips.len() {
                if in_range(t.clips[i].start_beat) {
                    let mut c = t.clips.remove(i);
                    c.start_beat -= a;
                    taken_clips.push((t.id, c));
                } else {
                    i += 1;
                }
            }
        }
        let mut taken_auto: Vec<(u32, u32, AutomationClip)> = Vec::new();
        for t in &mut self.tracks {
            let tid = t.id;
            for lane in &mut t.automation_lanes {
                let lid = lane.id;
                let mut i = 0;
                while i < lane.clips.len() {
                    if in_range(lane.clips[i].start_beat) {
                        let mut c = lane.clips.remove(i);
                        c.start_beat -= a;
                        taken_auto.push((tid, lid, c));
                    } else {
                        i += 1;
                    }
                }
            }
        }
        let mut taken_song_auto: Vec<(u32, AutomationClip)> = Vec::new();
        for lane in &mut self.song_lanes {
            let lid = lane.id;
            let mut i = 0;
            while i < lane.clips.len() {
                if in_range(lane.clips[i].start_beat) {
                    let mut c = lane.clips.remove(i);
                    c.start_beat -= a;
                    taken_song_auto.push((lid, c));
                } else {
                    i += 1;
                }
            }
        }
        let mut taken_scales: Vec<ScaleChange> = Vec::new();
        self.scale_changes.retain(|sc| {
            if in_range(sc.beat) {
                let mut s = *sc;
                s.beat -= a;
                taken_scales.push(s);
                false
            } else {
                true
            }
        });
        // 帯自身も取り出す (ripple では動かさず、 後で dest に置き直す)。
        self.sections.retain(|s| s.id != section_id);

        // 2. `[a,b)` を詰める (close)。
        let close = self.ripple_timeline(b, -len);
        // 3. 落とし先。 r.md #71: **そのまま使う**。 close は「帯を抜いた」 だけで、
        //    残りの帯の並びは変わらない。 続く open が `dest_start` 以降を右へ逃がすので、
        //    帯を `dest_start` に置けば最終的な開始拍はちょうど `dest_start` になる
        //    (= ドラッグ中に見えていた位置)。
        //
        //    旧実装はここで `dest_start - len` / `a` の逆算をしていた。 それは
        //    「close 後の中間座標系での位置」 を呼び出し側に指定させる契約であり、
        //    前へ動かすときに 1 セクションぶんズレる原因だった (r.md #71)。
        let dest2 = dest_start;
        // 4. 落とし先に `len` ぶん空ける (open)。
        let open = self.ripple_timeline(dest2, len);

        // 5. 取り出した content を `dest2` 基準で戻す。
        for (tid, mut c) in taken_clips {
            c.start_beat += dest2;
            if let Some(t) = self.tracks.iter_mut().find(|t| t.id == tid) {
                // 非重なり不変条件はここも通す (帯は ripple で空けた所へ戻すので
                // 実際には削られないが、規則の適用点を 1 つに保つ)。
                t.place_clip(c);
            }
        }
        for (tid, lid, mut c) in taken_auto {
            c.start_beat += dest2;
            if let Some(l) = self
                .tracks
                .iter_mut()
                .find(|t| t.id == tid)
                .and_then(|t| t.automation_lanes.iter_mut().find(|l| l.id == lid))
            {
                l.clips.push(c);
            }
        }
        for (lid, mut c) in taken_song_auto {
            c.start_beat += dest2;
            if let Some(l) = self.song_lanes.iter_mut().find(|l| l.id == lid) {
                l.clips.push(c);
            }
        }
        for mut s in taken_scales {
            s.beat += dest2;
            self.scale_changes.push(s);
        }
        // 6. 帯を dest2 に置き直す。
        self.sections.push(Section {
            id: sec.id,
            name: sec.name,
            color: sec.color,
            start_beat: dest2,
            len_beats: len,
        });

        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        self.normalize_sections();
        vec![close, open]
    }

    /// セクション帯を `dest_start` に複製
    /// 挿入する (Ctrl+drag、 ripple-insert)。 範囲 `[a,b)` 内の clip / automation を **linked**
    /// (= `content_id` 共有、 REAPER pooled idiom) で複製し、 clip id だけ新規採番。 `dest_start`
    /// 以降を `len` ぶん右へ ripple して空けてから複製を落とす。 元の content は残す。 新しい
    /// セクション id と適用した [`Ripple`] を返す (`None` = 複製しなかった)。 ripple は
    /// `move_section` と同じく Song の外に住む時間位置 (ループ範囲) の追従用。
    /// `move_section` / `delete_section_range`
    /// と同じく境界 `a` / `b` で `split_clips_at` してから `start_beat ∈ [a,b)` membership で複製する
    /// ので、 境界をまたぐ clip も範囲内ぶんだけ正しく複製される。
    pub fn duplicate_section(
        &mut self,
        section_id: u32,
        dest_start: f64,
    ) -> Option<(u32, Ripple)> {
        let sec = self.sections.iter().find(|s| s.id == section_id).cloned()?;
        let (a, len) = (sec.start_beat, sec.len_beats);
        let b = a + len;
        if len <= 0.0 {
            return None;
        }
        // r.md #71 同件: 複製も「置けない位置」 (他帯に食い込む) を指されたら境界へ寄せる。
        // 寄せないと `normalize_sections` が重なりを潰し、**複製だけ短くなる**
        // (ゴーストは満寸で見えているのに、落とすと切り詰められる)。
        // 移動と違って close しないので、障害物は **全帯を現在位置のまま** (元帯も含む)。
        let dest_start = resolve_section_drop_start(
            self.sections.iter().map(|s| (s.start_beat, s.len_beats)),
            dest_start,
        );
        // 中身の写しと時間ごとの貼り付けは時間範囲操作と同じ 1 本
        // ([`Song::copy_time_range`] / [`Song::paste_time_range`])。 帯そのもの (`[a,b)` に
        // 完全に入る唯一のセクション) も写しに含まれ、 貼り先で新 id を得る。
        let copy = self.copy_time_range(a, b)?;
        let pasted = self.paste_time_range(dest_start, &copy, true)?;
        let new_id = pasted.section_ids.first().copied()?;
        Some((new_id, pasted.ripple))
    }

    /// セクション帯だけ削除する (内容は温存、 Studio One の Backspace 相当)。
    /// 削除できたら `true`。
    pub fn delete_section(&mut self, section_id: u32) -> bool {
        let before = self.sections.len();
        self.sections.retain(|s| s.id != section_id);
        self.sections.len() != before
    }

    /// セクションの**時間範囲ごと**削除して
    /// 詰める (Studio One の "Delete Range" 相当、 破壊的)。 境界を分割してから範囲内の全
    /// content を消し、 `[a,b)` を ripple-close で詰める。 削除できたら適用した
    /// [`Ripple`] を返す (`None` = 何もしなかった)。 ripple は Song の外に住む時間位置
    /// (ループ範囲) の追従用。
    pub fn delete_section_range(&mut self, section_id: u32) -> Option<Ripple> {
        let sec = self.sections.iter().find(|s| s.id == section_id).cloned()?;
        // 時間を消す規則は [`Song::delete_time_range`] 1 本 (帯は範囲に完全に入るので消える)。
        self.delete_time_range(sec.start_beat, sec.start_beat + sec.len_beats)
    }

    /// 全トラック clip / track automation clip /
    /// `song_lanes` clip のうち `beat` を**厳密にまたぐ** (`start < beat < start+len`) ものを
    /// 2 つに分割する。 **content は一切触らず、窓 (`start_beat` / `length_beats` /
    /// `content_offset_beats`) を 2 つに割るだけ** — 左断片は長さを `beat` まで詰め、
    /// 右断片は `content_offset_beats` を `cut = beat - start` ぶん進める。 両断片は同じ
    /// `content_id` を共有した 2 つの窓になるので、 linked clip の関係も、 窓の外に隠れて
    /// いた素材も壊れない (窓モデル、 `docs/plan_clip_content_window.md`)。 セクション移動の
    /// 前にこれを境界 `a` / `b` で呼ぶと、 以降の「`start_beat ∈ [a,b)`」 membership 抽出が
    /// 境界跨ぎ clip でも正確になる。 歌声 clip も MIDI として分割され、 右断片は note 集合が
    /// 変わるのでキャッシュ key が変化し自動で再合成される。
    pub fn split_clips_at(&mut self, beat: f64) {
        for ti in 0..self.tracks.len() {
            let mut i = 0;
            while i < self.tracks[ti].clips.len() {
                let (start, len, cid, off) = {
                    let c = &self.tracks[ti].clips[i];
                    (c.start_beat, c.length_beats, c.content_id, c.content_offset_beats)
                };
                if start < beat && beat < start + len {
                    let cut = beat - start;
                    // 跨ぐ note / event は content 側で切る (共有されていれば CoW)。
                    // その上で窓を 2 つに割る — 両断片は同じ content を別の窓で見る。
                    let cid = self.split_content_at(cid, off + cut);
                    let right_id = self.tracks[ti].alloc_clip_id();
                    let mut right = self.tracks[ti].clips[i].clone();
                    right.id = right_id;
                    right.content_id = cid;
                    right.start_beat = beat;
                    right.length_beats = len - cut;
                    right.content_offset_beats = off + cut;
                    // 切り口の両側は新しい端 (張り出しを継がない、`ClipWindow::clear_overhang`)。
                    right.clear_overhang(true, false);
                    self.tracks[ti].clips[i].content_id = cid;
                    self.tracks[ti].clips[i].length_beats = cut;
                    self.tracks[ti].clips[i].clear_overhang(false, true);
                    self.tracks[ti].clips.insert(i + 1, right);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            for li in 0..self.tracks[ti].automation_lanes.len() {
                let mut j = 0;
                while j < self.tracks[ti].automation_lanes[li].clips.len() {
                    let (start, len, off) = {
                        let c = &self.tracks[ti].automation_lanes[li].clips[j];
                        (c.start_beat, c.length_beats, c.content_offset_beats)
                    };
                    if start < beat && beat < start + len {
                        let cut = beat - start;
                        let lane = &mut self.tracks[ti].automation_lanes[li];
                        let right_id = lane.alloc_clip_id();
                        let mut right = lane.clips[j].clone();
                        right.id = right_id;
                        right.start_beat = beat;
                        right.length_beats = len - cut;
                        right.content_offset_beats = off + cut;
                        lane.clips[j].length_beats = cut;
                        lane.clips.insert(j + 1, right);
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
            }
        }
        for li in 0..self.song_lanes.len() {
            let mut j = 0;
            while j < self.song_lanes[li].clips.len() {
                let (start, len, off) = {
                    let c = &self.song_lanes[li].clips[j];
                    (c.start_beat, c.length_beats, c.content_offset_beats)
                };
                if start < beat && beat < start + len {
                    let cut = beat - start;
                    let lane = &mut self.song_lanes[li];
                    let right_id = lane.alloc_clip_id();
                    let mut right = lane.clips[j].clone();
                    right.id = right_id;
                    right.start_beat = beat;
                    right.length_beats = len - cut;
                    right.content_offset_beats = off + cut;
                    lane.clips[j].length_beats = cut;
                    lane.clips.insert(j + 1, right);
                    j += 2;
                } else {
                    j += 1;
                }
            }
        }
    }
}
