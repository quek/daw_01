//! handler::activity — r.md #49 のアイドル省電力判定。
//!
//! daw_gui の責務は **「アプリの窓がアクティブか」という事実の SSoT** であって、
//! オーディオを止めてよいかの判断ではない。後者は daw_audio が持つ (再生中 /
//! count-in / 書き出し中 / 出力が無音か、を engine だけが知っているため)。

use common::protocol::AudioCommand;

use crate::state::*;

/// メーターの量子化ステップ。バー高さは 100px 前後なので 1/1024 は確実に
/// サブピクセル = 「見た目が変わらない差」。
const METER_STEPS: f32 = 1024.0;

/// `f32` を `steps` 分解能の整数へ落として指紋に混ぜられる形にする。
/// NaN / 非有限は 1 つの固定値に畳む (指紋の目的は等値比較だけ)。
fn quantize(v: f32, steps: f32) -> u64 {
    if !v.is_finite() {
        return u64::MAX;
    }
    // i64 経由で負値 (pan / 変調スカラー) も潰さずに表現する。
    ((v * steps).round() as i64) as u64
}

/// linear amplitude → メーター表示上の正規化値。-60dB 未満は 0.0 に張り付くので、
/// 指数減衰する `track_peak_display` が有限ステップで必ず収束する。
fn meter_norm(linear: f32) -> f32 {
    common::meter::db_to_norm(common::meter::linear_to_db(linear))
}

impl AppData {
    /// 進捗を見せている最中か。裏に回しても画面を止めない条件。
    ///
    /// 「時間のかかる処理の進捗は動き続ける」という決定 (docs/plan_idle_power.md §0)
    /// に対応する。ここに挙げるのは **画面に進捗が出るもの**だけ — 出ないものを
    /// 足すと、見えない何かのために永久に省電力へ入らなくなる。
    #[must_use]
    pub fn app_busy(&self, now: std::time::Instant) -> bool {
        self.transport.export_stage.is_some()
            || self.transport.pending_video_export.is_some()
            // r.md #54: 解析中はレポート窓に進捗バーと伸びるグラフが出ている。
            || self.loudness.phase.is_busy()
            || self.ipc.is_rescanning
            || self.voicevox_animating(now)
            // r.md #61: 終了処理中は「プラグインを解放しています… (N 秒)」の
            // 経過表示が動いている。窓が非アクティブなまま終了する経路
            // (OS のサインアウト / プラグインエディタにフォーカスがある状態の
            // Ctrl+Q) でも表示が凍らないよう、他の進捗表示と同じ扱いにする。
            || self.shutdown.is_draining()
    }

    /// transport が走っているか (再生 or 録音)。
    ///
    /// r.md #51 以降、`transport.is_playing` は engine の観測値なので、録音を
    /// 別に見る必要は無い (録音は必ず transport の上で走る)。count-in 中も
    /// engine は playing なのでここに含まれる。
    #[must_use]
    pub fn transport_rolling(&self) -> bool {
        self.transport.is_playing
    }

    /// 画面を描き続けるべきか。
    ///
    /// **再生 / 録音中は、非アクティブでも描き続ける**。r.md #49 の条件は
    /// 「再生停止中 **かつ** ウインドウがアクティブでない」であって、アクティブ判定
    /// だけで止めてよいとは書かれていない。裏で再生しながら別ウィンドウで作業する
    /// (歌詞を見る / 譜面を追う) のは普通の使い方で、そこで画面が凍ると壊れて見える。
    ///
    /// 実際、これを落として「再生中なのに 27 秒間 1 フレームも描かれない」状態を
    /// 作ってしまった (2026-08-15 実機検証)。
    ///
    /// transport が走っている間は `on_tick` に同居する曲末の自動停止判定・
    /// オートメーション録音の点打ち (1/64 拍の間引き)・再生追従スクロールも
    /// 動き続ける必要があるので、描画条件と tick レートの条件はここで一致する。
    #[must_use]
    pub fn should_keep_rendering(&self, now: std::time::Instant) -> bool {
        crate::state::activity::should_keep_rendering(
            self.activity.app_windows_active(),
            self.app_busy(now),
            self.transport_rolling(),
        )
    }

    /// r.md #49: tick 系イベント (`Tick` / `ModScalarsTick` / `TrackPeaksTick` /
    /// `MetricsTick` / `SystemMetricsTick`) が **画面に出る値**を変えたかを判定する
    /// ための指紋。`handle_event` の前後で比較し、変わったときだけ再描画する。
    ///
    /// 再描画判定を「イベントの種類」ではなく「値の変化」で行うのは、tick が
    /// 毎秒 30 回届くのに中身が同じ (停止中は playhead も peak も動かない) ためで、
    /// 従来はこれで無条件に 30fps 描き続けていた。
    ///
    /// **表示解像度で量子化する**のが要点。生の f32 を比べると:
    /// - メーターの release は `prev * 0.85` の指数減衰で **厳密には 0 にならない**
    ///   (`common::meter::update_peak`) ため、永久に「変化あり」になる。dB 経由で
    ///   正規化すると -60dB 未満が 0.0 に張り付いて必ず収束する
    /// - DSP load の EMA は毎 tick 揺れるが、ステータスバーの表示は整数パーセント
    ///   (`view::status_bar` の `{:>3.0}%`) なので、その粒度より細かい差は
    ///   「画面が変わっていない」
    ///
    /// tick 以外の event はここを通さず**従来どおり無条件で再描画**する。判定対象を
    /// 5 つに閉じ込めることで、残り数百の variant に「立て忘れ = 画面が固まる」
    /// という新しい失敗モードを持ち込まない。
    #[must_use]
    pub fn tick_visual_fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        };
        // Song が変わったら (= automation 録音が点を打った等) 無条件に再描画。
        mix(self.song_doc.edit_epoch());
        // playhead は拍の 1/1000 まで見る (ズーム最大でも 1px 未満)。
        mix(quantize(self.transport.playhead_beat.unwrap_or(f32::NAN), 1000.0));
        mix(quantize(self.ui_prefs.arrange_scroll_beat, 1000.0));
        // r.md #50: マスターパネルの全メーター (ピーク / VU / ラウドネス /
        // スペクトラム / オシロ / ゴニオ) は解析器が 1 つのダイジェストに畳んで
        // よこす。解析器側で表示解像度に量子化してあるので、無音になれば必ず
        // 収束する = ここで再描画が止まる。
        mix(self.transport.master_meter.visual_digest);
        // トラックメーターは linear なので dB 経由で正規化してから量子化する
        // (そのまま量子化すると指数減衰が 0 に収束せず永久に描き続ける)。
        for (l, r, gr) in &self.transport.track_peak_display {
            mix(quantize(meter_norm(*l), METER_STEPS));
            mix(quantize(meter_norm(*r), METER_STEPS));
            // GR も動く表示なので digest に混ぜる (混ぜないとコンプだけが
            // 動いている間に再描画が止まり、メーターが凍る)。0 に収束するよう
            // 表示レンジで正規化してから量子化する。
            mix(quantize(
                (*gr / common::model::GR_METER_RANGE_DB).clamp(0.0, 1.0),
                METER_STEPS,
            ));
        }
        // マスターストリップの GR (コンプ / リミッター)。同じ理由で digest に混ぜる。
        for gr in [self.transport.master_strip_gr.0, self.transport.master_strip_gr.1] {
            mix(quantize(
                (gr / common::model::MASTER_GR_METER_RANGE_DB).clamp(0.0, 1.0),
                METER_STEPS,
            ));
        }
        // 変調スカラーは画像 / グループ / 映像効果の見た目を直接動かすので細かく見る。
        for v in self.transport.mod_plane.values() {
            mix(quantize(*v, 4096.0));
        }
        // リソースモニターの表示粒度 = 整数パーセント。
        let m = &self.ipc.metrics;
        mix(quantize(m.dsp_load_peak * 100.0, 1.0));
        mix(quantize(m.system_cpu, 1.0));
        mix(quantize(m.fps, 1.0));
        mix(m.xrun_count);
        mix(u64::from(m.buffer_frames));
        mix(u64::from(m.sample_rate));
        // watchdog が発火すると status_message / export 表示 / 録音状態が変わる。
        mix(self.ui_ephemeral.status_message.len() as u64);
        mix(u64::from(self.transport.export_stage.is_some()));
        // r.md #54: 解析の進捗バーと曲線は 250ms ごとに更新される。走査済み
        // フレーム数を混ぜて、進んだフレームだけ描き直す。
        mix(u64::from(self.loudness.phase.is_busy()));
        mix(
            self.loudness
                .report
                .as_ref()
                .map_or(0, |r| r.done_frames ^ u64::from(r.complete)),
        );
        // Rec ボタンの表示は「要求」「実際に録っているか」「count-in 中か」で変わる。
        // count-in の残量は **有無だけ**混ぜる — 生の残量を混ぜると、画面に出ない
        // 数値が毎 tick 変わるだけで count-in 中ずっと 30fps 描き直すことになる
        // (表示解像度で量子化する、というこの関数の原則どおり)。
        mix(u64::from(self.recording.requested));
        mix(u64::from(self.recording.live));
        mix(u64::from(self.transport.preroll_remaining > 0));
        mix(u64::from(self.transport.is_playing));
        h
    }

    /// アクティブ状態が変わっていたら daw_audio へ報告する。
    ///
    /// 変化時のみ送る (毎フレーム送ると focus が動かない限り無意味な IPC が流れる)。
    /// **`app_busy` は含めない** — 書き出し中は engine 側が `export_running` で
    /// park しないし、VOICEVOX 合成やプラグイン検索は音を必要としないので、
    /// 「窓がアクティブか」という事実だけを渡すのが正しい分界。
    pub fn sync_app_active_with_audio(&mut self) {
        let active = self.activity.app_windows_active();
        if self.activity.last_sent_app_active == Some(active) {
            return;
        }
        self.activity.last_sent_app_active = Some(active);
        self.send_audio(AudioCommand::SetAppActive(active));
    }
}
