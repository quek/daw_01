//! 列 (シーン) のフォローアクションの走行状態と、その張り / 張り直し / 発火。
//!
//! 行 (`RowRuntime`) のフォローと対になる部分だけをここへ分ける。**クリップのフォローより
//! 優先する** (Live 12 の規則) ので、`LauncherRuntime::update` は行を解く前に
//! [`LauncherRuntime::tick_scene_follow`] を呼ぶ。

use super::*;

/// 列 (シーン) のフォローアクションの走行状態。
#[derive(Debug, Clone)]
pub(super) struct SceneRun {
    pub(super) scene_id: u32,
    pub(super) at: f64,
    /// その列でいちばん長いセルの長さ (拍)。**Linked (既定) の再武装に要る** —
    /// 0 のまま張り直すと `due_beat` が `None` を返し、以後そのシーンの
    /// フォローアクションが二度と発火しない。
    pub(super) longest: f64,
    /// `at` の周期の起点 (撃った拍 / 直前の発火拍)。設定が変わったときの張り直しに要る。
    pub(super) base: f64,
    /// `at` を導いたときの列の設定。`Song` 側で変わったら走行中でも張り直す
    /// ([`LauncherRuntime::resync_scene_follow`]、行の `armed_follow` と対)。
    pub(super) armed: FollowAction,
}

impl SceneRun {
    pub(super) const NONE: Self = Self {
        scene_id: 0,
        at: f64::INFINITY,
        longest: 0.0,
        base: 0.0,
        armed: FollowAction {
            enabled: false,
            a: FollowActionKind::NoAction,
            b: FollowActionKind::NoAction,
            chance_a: 100,
            linked: true,
            time_beats: 4.0,
            multiplier: 1,
        },
    };
}

impl LauncherRuntime {
    /// 列のフォローアクションの走行状態を捨てる。
    ///
    /// **「全行の走行状態をリセットする操作」と同じ寿命**にする — プロジェクト
    /// 切替 / 再生開始 (reseed) / 全停止 / 全行アレンジ復帰。1 行だけの停止・
    /// アレンジ復帰では捨てない (他の行はまだその列を鳴らしているので、列の
    /// 連鎖はまだ生きている)。残したまま全停止すると、止めたはずの全行が
    /// `scene.at` に到達した瞬間に勝手に鳴り出す。
    pub(super) fn disarm_scene(&mut self) {
        self.scene = SceneRun::NONE;
    }

    /// 列のフォローアクションの起点を張る。Linked は「その列で最も長いセル」を
    /// 1 周とみなす — 列そのものは長さを持たないので、鳴っている中身から導く。
    pub(super) fn arm_scene_follow(
        &mut self,
        song: SongRef<'_>,
        scene_id: u32,
        fire: f64,
        longest: f64,
        now: f64,
    ) {
        let follow = song
            .index
            .scene_pos(scene_id)
            .and_then(|pos| song.scenes.get(pos))
            .map(|s| s.follow.clone())
            .unwrap_or_default();
        let base = if fire.is_finite() { fire } else { now };
        self.scene = SceneRun {
            scene_id,
            at: follow::due_beat(&follow, base, longest).unwrap_or(f64::INFINITY),
            longest,
            base,
            armed: follow,
        };
    }

    /// 走行中に列のフォローアクションの設定が変わったら張り直す (行の
    /// [`Self::resync_cells`] と同じ規則、SSoT は `Song`)。起点 `base` から周期を刻んで
    /// `now` 以降で最初の発火へ。
    pub(super) fn resync_scene_follow(&mut self, song: SongRef<'_>, now: f64) {
        if self.scene.scene_id == 0 {
            return;
        }
        let Some(scene) = song.index.scene_pos(self.scene.scene_id).and_then(|pos| song.scenes.get(pos)) else {
            return;
        };
        if scene.follow == self.scene.armed {
            return;
        }
        self.scene.armed = scene.follow.clone();
        self.scene.at =
            follow::next_due_beat(&scene.follow, self.scene.base, self.scene.longest, now)
                .unwrap_or(f64::INFINITY);
    }

    /// 列のフォローアクション。**クリップのそれより優先**するので、行を解く前に
    /// ここで予約を置く (行の予約は「新しい発火が前を置き換える」)。
    ///
    /// **グローバル量子化は受け取らない** — 列のフォローアクションはそれを
    /// 迂回する (計画書 §2.3) ので、発火拍の解決に使う値が無い。
    pub(super) fn tick_scene_follow(&mut self, song: SongRef<'_>, span: BufferSpan) {
        if !self.scene.at.is_finite() || self.scene.at >= span.end_beat() {
            return;
        }
        let fire = self.scene.at.max(span.start_beat);
        let Some((pos, scene)) =
            song.index.scene_pos(self.scene.scene_id).and_then(|pos| Some((pos, song.scenes.get(pos)?)))
        else {
            self.disarm_scene();
            return;
        };
        let follow = scene.follow.clone();
        let n = self.fill_scene_occupancy(song);
        let seed = follow::row_seed(SCENE_SEED_SALT, self.scene.scene_id);
        let scene_pos = |id| song.index.scene_pos(id);
        let outcome = follow::resolve(&follow, &self.occupied[..n], pos, scene_pos, seed, fire);
        match outcome {
            FollowOutcome::Keep => {
                // 再武装は **初回と同じ長さ** (その列の最長セル) で張る。
                let longest = self.scene.longest;
                self.scene.base = fire;
                self.scene.at =
                    follow::due_beat(&follow, fire, longest).unwrap_or(f64::INFINITY);
            }
            // `queue_all` が列の連鎖も解除する。
            FollowOutcome::Stop => self.queue_all(QueueTarget::Stop, fire),
            FollowOutcome::Go(idx) => match song.scenes.get(idx).map(|s| s.id) {
                Some(id) => self.launch_scene(song, FireAt::Chain { fire }, id, span.start_beat),
                None => self.disarm_scene(),
            },
        }
    }

    /// 「その列にどれかの行のセルがあるか」を作業領域へ埋め、有効長を返す。
    ///
    /// 数える行はランチャーの行の集合そのもの ([`SongIndex::scene_longest`]) — マスター行
    /// (`Song.song_lanes`) を落とすと、そこにしかセルの無い列が「空列」と判定され、
    /// Q13 の「空セルに区切られた塊」が誤って途切れる (`Next` がその列を飛ばす /
    /// その列自身のフォローアクションが一度も発火しない)。同じ id の列が複数あれば先頭だけが埋まる。
    fn fill_scene_occupancy(&mut self, song: SongRef<'_>) -> usize {
        let n = song.scenes.len().min(MAX_SCENES);
        for (i, (slot, scene)) in self.occupied[..n].iter_mut().zip(&song.scenes).enumerate() {
            *slot = song.index.scene_pos(scene.id) == Some(i) && song.index.scene_longest(scene.id).is_some();
        }
        n
    }
}

/// その列で最も長いセルの長さ (拍、セルが無ければ 0)。
///
/// 列そのものは長さを持たないので、Linked の列のフォローアクションは鳴っている
/// 中身から 1 周を導く。`launch_scene` が発火時に数えるのと**同じ規則**を
/// 再生開始 (reseed) でも使うために切り出してある。
pub(super) fn scene_longest(song: SongRef<'_>, scene_id: u32) -> f64 {
    song.index.scene_longest(scene_id).unwrap_or(0.0)
}
