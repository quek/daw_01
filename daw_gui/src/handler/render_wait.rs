//! handler::render_wait — オフライン描画 (WAV / Video 書き出し・ラウドネス解析・Bounce・Glue) を **plugin の読み込みが
//! 全部確定してから** 始める門と、読み込み待ちで預かった要求 (再生の A7 を含む) を出す唯一の口。
//!
//! 読み込み中の plugin を鳴らすトラックは engine のグラフに入らない (r.md #131、`Song::executable_mask`) ので、
//! そのまま焼くとそのトラックは無音になる。しかも書き出し / 解析の間は読み込み応答を捨てる (`AppData::handle_event`
//! の block-list)。ユーザーの意図は「焼きたい」なので、断らずに **確定 (成功 / 失敗) を待ってから始める** — 再生の
//! A7 が読み込みを待ってから走り出すのと同じ規則。待っている間はまだ何も始めていない (engine も host も占有しない)
//! ので、読み込み応答はそのまま受け、Song も凍らせない。

use crate::state::*;

impl AppData {
    /// plugin の読み込みが 1 つでも応答待ちか (オフライン描画の入口が [`Self::defer_render`] するか決める)。
    #[must_use]
    pub(crate) fn plugin_loads_pending(&self) -> bool {
        !self.cur.pipc.pending_plugin_loads.is_empty()
    }

    /// 読み込みが確定するまで `render` の開始を預かる。入口は自分の検査を済ませてから、[`Self::plugin_loads_pending`]
    /// のときだけこれを呼んで return する。
    ///
    /// 書き出し / 解析 ([`PendingRender::blocks_screen`]) は押した時点で始まっている扱い — 走り出すときと同じく再生を
    /// 止め、止めた再生を読み込み後に戻す予約 (A7) も捨てる (`stop` が捨てる。戻した直後に描画の開始が止めることに
    /// なる)。待っている間の見え方は、書き出しは進捗オーバーレイ、解析はレポート窓、Bounce / Glue はステータスバーが
    /// 同じ状態から出す。
    pub(crate) fn defer_render(&mut self, render: PendingRender) {
        if render.blocks_screen() {
            self.stop();
        }
        tracing::info!(
            render = render.name(),
            remaining = self.cur.pipc.pending_plugin_loads.len(),
            "offline render waits for plugin loads"
        );
        self.cur.transport.pending_render = Some(render);
    }

    /// 読み込み待ちで預かった要求を、読み込みが全部確定していれば出す。**`handle_event` の一番外側の終わりから呼ぶ
    /// 唯一の口** — 読み込みが確定する経路 (応答 / 失敗 / 読み込み中の device を消す編集・undo・トラック削除) のどれで
    /// 空になっても、その操作の副作用が全部済んだ後で出す (経路ごとに発火を撒くと、撒き忘れた経路で要求が永久に残る)。
    ///
    /// 再生 (A7) は asset decode も揃ってから。預かった順 (再生 → 描画) に出す — 画面を塞ぐ描画を預かったときは
    /// [`Self::defer_render`] が再生の予約を捨てているので、ここで再生と描画が衝突するのは Bounce / Glue だけで、
    /// それは「再生中に Bounce を押した」のと同じ。
    pub(crate) fn resume_after_plugin_loads(&mut self) {
        if self.shutdown.is_shutting_down() || self.plugin_loads_pending() {
            return;
        }
        if self.cur.transport.pending_play.is_some() && !self.audio_decode_pending() {
            self.fire_pending_play();
        }
        let Some(render) = self.cur.transport.pending_render.take() else {
            return;
        };
        tracing::info!(render = render.name(), "plugin loads settled; starting the offline render");
        match render {
            PendingRender::Wav { path, range } => self.export_wav_to(path, range),
            PendingRender::Video { output_path, range_beats, resolution, framerate } => {
                self.action_begin_export_mp4(output_path, range_beats, resolution, framerate);
            }
            PendingRender::Loudness { range } => self.begin_loudness_analysis(range),
            PendingRender::Bounce { target, mode, label } => match self.live_clip_key(target) {
                Some(target) => self.request_bounce(target, mode, label),
                None => self.ui_ephemeral.status_message = "Bounce: 対象クリップが消えたため中止しました".into(),
            },
            PendingRender::Glue { sel, label } => self.glue_selection(sel, label),
        }
    }

    /// 読み込み待ちの描画のうち `which` に当たるものを取り消す (まだ何も始めていないので捨てるだけ)。取り消したら `true`。
    pub(crate) fn cancel_pending_render(&mut self, which: impl FnOnce(&PendingRender) -> bool) -> bool {
        let Some(render) = self.cur.transport.pending_render.take_if(|r| which(r)) else {
            return false;
        };
        self.ui_ephemeral.status_message = format!("{}をキャンセルしました", render.name());
        true
    }

    /// オフライン描画 `what` を新しく始められないなら、理由を status に出して `true`。engine の offline render は
    /// 同時に 1 本なので、走っている描画か、読み込み待ちで開始を預かっている描画があれば始めない。全入口 (書き出し /
    /// 解析 / Bounce / Glue) がこれ 1 つを見る — 入口ごとに「自分と同じ種類だけ」を見ていると、別の種類が走っている
    /// 最中に始めて engine に弾かれる (書き出し中の Bounce、Bounce 中の書き出し)。
    pub(crate) fn refuse_render_while_another(&mut self, what: &str) -> bool {
        let busy = if let Some(render) = &self.cur.transport.pending_render {
            format!("{}の開始待ち (プラグイン読み込み中)", render.name())
        } else if self.cur.loudness.phase.is_busy() {
            "ラウドネス解析の実行中".to_string()
        } else if self.export_or_analysis_busy() {
            "書き出しの実行中".to_string()
        } else if self.cur.pipc.pending_clip_fx_bounce.is_some() || self.cur.pipc.pending_vocal_synth_bounce.is_some() {
            "Bounce の実行中".to_string()
        } else if self.cur.pipc.pending_glue_bake.is_some() {
            "Glue の実行中".to_string()
        } else {
            return false;
        };
        self.ui_ephemeral.status_message = format!("{what}: {busy}のため開始できません");
        true
    }

    /// 再生を始められない理由。オフライン描画が engine を占有している間と、画面を塞ぐ描画 (書き出し / 解析) が読み込み
    /// 待ちで開始を待っている間は再生しない。
    #[must_use]
    pub(crate) fn offline_render_refuses_play(&self) -> Option<&'static str> {
        let waiting_export = self.cur.transport.pending_render.as_ref().is_some_and(PendingRender::is_export);
        if self.loudness_in_progress() {
            Some("ラウドネス解析中は再生できません")
        } else if self.export_or_analysis_busy() || waiting_export {
            Some("書き出し中は再生できません")
        } else if self.offline_render_busy() {
            // bounce / Glue の焼き込みも同じ freewheel を占有する。
            Some("焼き込み中は再生できません")
        } else {
            None
        }
    }

    /// ラウドネス解析が進行中か (読み込み待ちで開始を待っている間を含む)。レポート窓の暗転・入力遮断・中止ボタンは
    /// これを見る (待っている間も解析は始まっている扱い)。
    #[must_use]
    pub fn loudness_in_progress(&self) -> bool {
        self.cur.loudness.phase.is_busy()
            || matches!(self.cur.transport.pending_render, Some(PendingRender::Loudness { .. }))
    }
}
