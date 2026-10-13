//! CPAL クロージャ専有の状態 [`DeviceRt`] — 開いている [`ProjectRt`] の列とデバイス最終ミックス
//! (`docs/plan_project_tabs.md` §3.1)。
//!
//! **stall の窓** (`crate::audio_worker` の module doc): project の描画が `PoolRun::InFlight` で戻ったら、その project
//! は窓が閉じるまで**凍結**する — RT はその project に一切触らない (便の取り込み・transport の要求・コマンド・描画・
//! bus の合算・閉じたときの返却)。生き残りが文脈を通して読み書きする物 (曲・schedule・scratch・bus・plugin の表 …)
//! は全部 project の持ち物なので、project ごと触らなければ漏れが無い (文脈の届く先を数え上げる口を作らない)。
//! 生き残りが借りている rig は project が `Arc` で持ち、窓が閉じたら off-thread へ返す (RT で最後の `Arc` を
//! 落とさない — plugin host を立て直した後はこれが最後の参照)。

use super::*;

/// CPAL クロージャ専有の状態 (`docs/plan_project_tabs.md` §3.1)。開いている
/// [`ProjectRt`] の列とデバイス最終ミックス。project の追加 / 撤去は
/// [`ProjectDelivery`] で off-thread から届く (RT で確保しない)。
pub struct DeviceRt {
    /// デバイス最終ミックス (全 project の bus を加算したもの)。
    pub master_l: Vec<f32>,
    pub master_r: Vec<f32>,
    /// 開いている project (容量 `MAX_PROJECTS` を事前確保、`push` / `swap_remove`
    /// は再確保しない)。並びに意味は無い (ミックスは加算)。
    ///
    /// `Box` なのは意図: `ProjectRt` は scratch 込みで数 MB あり、`Vec<ProjectRt>` だと
    /// push / swap_remove / ring の move が RT で MB 級の memcpy になる。Box なら
    /// ポインタ 1 本の move。
    #[allow(clippy::vec_box)]
    pub projects: Vec<Box<ProjectRt>>,
    /// 窓の間に閉じた project (容量 `MAX_PROJECTS`)。もう `projects` には居ない (key で引けない・描かない) が、
    /// 生き残りが文脈を通して触っているので、窓が閉じるまで off-thread へ返さない (返すと recv loop が解放する)。
    #[allow(clippy::vec_box)]
    pub closing: Vec<Box<ProjectRt>>,
    /// project slot の生成 / 撤去便 (recv loop → RT)。
    pub project_rx: rtrb::Consumer<ProjectDelivery>,
    /// 撤去した `ProjectRt` を off-thread で drop させる口 (RT で free しない)。
    pub project_recycle_tx: rtrb::Producer<Box<ProjectRt>>,
    /// Pending preview / launcher commands from the receive loop ([`EngineCommandSender`]).
    /// Drained at the top of every `process_buffer`.
    pub cmd_rx: rtrb::Consumer<EngineCommand>,
    /// Resources shared with the export / notify threads.
    pub shared: Arc<EngineShared>,
    /// デバイス全体 snapshot (worker rig / sampler ring) の配送 ring 対。
    pub device_rx: rtrb::Consumer<DeviceBundle>,
    pub device_recycle_tx: rtrb::Producer<DeviceBundle>,
    /// RT が使う worker rig (bundle 由来 Arc clone)。
    pub worker: Option<Arc<WorkerRig>>,
    /// Global Sampler のリング (bundle 由来 Arc clone)。
    pub sampler: Option<Arc<SamplerRig>>,
    /// Global Sampler の RT 走行状態 (セグメント / 試聴)。
    pub sampler_rt: SamplerRt,
    /// stream 開始からの累積 render フレーム数 (MIDI 試聴シーケンスの時計)。
    pub frames_rendered: u64,
    /// r.md #129 §11.2: device scope の slot ごとに、最後に書いた見出し `(project, device id)`。
    /// 表と違う slot だけ見出しを書き換える (毎 buffer 16 slot の atomic を読み書きしない)。
    pub scope_headers: [(ProjectKey, u64); MAX_DEVICE_SCOPES],
    /// 描き手が描画スレッド (先読み) か。描き手を替える `crate::output_render` だけが書く。
    /// transport の publish を聞こえている回まで遅らせるかを決める ([`super::TransportOut`])。
    pub render_ahead: bool,
}

impl DeviceRt {
    pub fn new(
        max_frames: usize,
        cmd_rx: rtrb::Consumer<EngineCommand>,
        shared: Arc<EngineShared>,
        project_rx: rtrb::Consumer<ProjectDelivery>,
        project_recycle_tx: rtrb::Producer<Box<ProjectRt>>,
        device_rx: rtrb::Consumer<DeviceBundle>,
        device_recycle_tx: rtrb::Producer<DeviceBundle>,
    ) -> Self {
        Self {
            master_l: vec![0.0; max_frames],
            master_r: vec![0.0; max_frames],
            projects: Vec::with_capacity(MAX_PROJECTS),
            closing: Vec::with_capacity(MAX_PROJECTS),
            project_rx,
            project_recycle_tx,
            cmd_rx,
            shared,
            device_rx,
            device_recycle_tx,
            worker: None,
            sampler: None,
            sampler_rt: SamplerRt::new(),
            frames_rendered: 0,
            scope_headers: [(ProjectKey::NONE, 0); MAX_DEVICE_SCOPES],
            render_ahead: false,
        }
    }

    /// 「どれか 1 つでも走っている (count-in 込み)」 — idle park 判定用。
    #[must_use]
    pub fn any_rolling(&self) -> bool {
        self.projects.iter().any(|p| p.is_rolling())
    }

    /// 「どれか 1 つでも再生中」 — xrun 計数用 (停止中の cold start を除外する)。
    #[must_use]
    pub fn any_playing(&self) -> bool {
        self.projects.iter().any(|p| p.playing)
    }

    /// `key` の project。窓の間 (凍結中) の project は引けない — RT はそれに触らない (module doc)。
    fn project_mut(&mut self, key: ProjectKey) -> Option<&mut ProjectRt> {
        self.projects.iter_mut().find(|p| p.key == key && !p.in_window()).map(|b| &mut **b)
    }

    /// project slot の生成 / 撤去便を取り込む。`Open` は容量内 push (再確保なし)、
    /// `Close` は `swap_remove` して recycle ring へ (drop は off-thread)。
    fn refresh_projects(&mut self) {
        while let Ok(delivery) = self.project_rx.pop() {
            match delivery {
                ProjectDelivery::Open(rt) => {
                    if self.projects.len() < self.projects.capacity() {
                        self.projects.push(rt);
                    } else {
                        // 容量超過 (recv loop が cap を守るので届かない)。捨てるのも
                        // off-thread — recycle へ返す。
                        let _ = self.project_recycle_tx.push(rt);
                    }
                }
                ProjectDelivery::Close(key) => {
                    // 閉じたタブが握っていた試聴の走行状態を降ろす
                    // (`docs/plan_project_tabs.md` §5.5)。
                    self.sampler_rt.forget_project(key);
                    if let Some(i) = self.projects.iter().position(|p| p.key == key) {
                        let rt = self.projects.swap_remove(i);
                        self.retire(rt);
                    }
                }
            }
        }
    }

    /// RT から外した project を off-thread へ返す。窓の間なら閉じるまで `closing` で預かる。
    fn retire(&mut self, rt: Box<ProjectRt>) {
        if !rt.in_window() {
            let _ = self.project_recycle_tx.push(rt);
        } else if self.closing.len() < self.closing.capacity() {
            self.closing.push(rt);
        } else {
            // 預かり切れない (窓の間に閉じた project が `MAX_PROJECTS` を超えた)。返すと解放され、生き残りが
            // 解放済みに触るので leak する (RT で free もしない)。
            std::mem::forget(rt);
        }
    }

    /// 窓が閉じた project を再開させる (module doc)。窓が借りていた rig は off-thread へ返し、窓の間に閉じた
    /// project はここで初めて off-thread へ返す。ring が満杯なら次の buffer で。
    fn settle_windows(&mut self) {
        for p in &mut self.projects {
            p.settle_window(&mut self.device_recycle_tx);
        }
        let mut i = 0;
        while i < self.closing.len() {
            if !self.project_recycle_tx.is_full() && self.closing[i].settle_window(&mut self.device_recycle_tx) {
                let rt = self.closing.swap_remove(i);
                let _ = self.project_recycle_tx.push(rt);
            } else {
                i += 1;
            }
        }
    }

    /// デバイス全体 snapshot (worker / sampler) の install。最新だけ残し、
    /// 旧値は recycle ring で off-thread drop。
    fn refresh_device_bundle(&mut self) {
        let mut newest: Option<DeviceBundle> = None;
        while let Ok(bundle) = self.device_rx.pop() {
            if let Some(skipped) = newest.take() {
                let _ = self.device_recycle_tx.push(skipped);
            }
            newest = Some(bundle);
        }
        let Some(new) = newest else { return };
        let old_worker = std::mem::replace(&mut self.worker, new.worker);
        // Global Sampler: 世代が変わった (別 Arc) ら走行状態を捨てる。旧リングは
        // recycle bundle で off-thread unmap。
        let sampler_changed = !match (&self.sampler, &new.sampler) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let old_sampler = std::mem::replace(&mut self.sampler, new.sampler);
        if sampler_changed {
            self.sampler_rt.reset();
        }
        let _ = self.device_recycle_tx.push(DeviceBundle {
            worker: old_worker,
            sampler: old_sampler,
        });
    }

    /// Drain pending commands. Called at the top of `process_buffer`.
    fn pump_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.pop() {
            self.apply_command(cmd);
        }
    }

    /// コマンドを 1 つ配る。窓の間 (凍結中) の project 宛ては捨てる ([`Self::project_mut`]) — 鍵盤プレビューの対の
    /// 片割れは、窓が閉じるときに積まれたまま残った方も捨てる (`ProjectRt::settle_window`)。
    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            // r.md #87: ランチャーの操作。発火拍の解決は buffer 頭の
            // `launcher.update` (= song snapshot が入ってから) が行う。
            EngineCommand::Launch { project, req } => {
                if let Some(p) = self.project_mut(project) {
                    p.launcher.push_request(req);
                }
            }
            EngineCommand::SamplerPreview { start, end } => self.sampler_rt.set_preview(start, end),
            EngineCommand::SamplerPreviewStop => self.sampler_rt.stop_preview(),
            // 鍵盤プレビュー: 該当 track の pending_preview に積む。
            // process_track_owned が次の dispatch で frame 0 に注入する。
            EngineCommand::PreviewNoteOn { project, track, pitch, velocity } => {
                let on = NoteTransition::On { note_id: PREVIEW_NOTE_ID, key: pitch, velocity };
                if let Some(p) = self.project_mut(project) {
                    p.push_preview(track, on);
                }
            }
            EngineCommand::PreviewNoteOff { project, track, pitch } => {
                let off = NoteTransition::Off { note_id: PREVIEW_NOTE_ID, key: pitch };
                if let Some(p) = self.project_mut(project) {
                    p.push_preview(track, off);
                }
            }
        }
    }

    /// Render `frames` of device output into `master_l/r` = 全 project の bus の加算
    /// (`docs/plan_project_tabs.md` §3.2)。
    ///
    /// buffer の間は `EngineShared::live_rendering` を立てておく (書き出しとの Dekker handshake。
    /// 予約 `export_running` は立てた **後** に読む)。
    pub fn process_buffer(
        &mut self,
        shared: &SharedState,
        bridge: &AudioBridgeHandle,
        scope: &common::scope_bridge::ScopeBridgeHandle,
        device_scope: &DeviceScopeBridgeHandle,
        sample_rate: u32,
        frames: usize,
    ) {
        let _ = shared;
        self.shared.live_rendering.store(true, Ordering::SeqCst);
        self.process_buffer_body(bridge, scope, device_scope, sample_rate, frames);
        self.shared.live_rendering.store(false, Ordering::SeqCst);
    }

    fn process_buffer_body(
        &mut self,
        bridge: &AudioBridgeHandle,
        scope: &common::scope_bridge::ScopeBridgeHandle,
        device_scope: &DeviceScopeBridgeHandle,
        sample_rate: u32,
        frames: usize,
    ) {
        self.settle_windows();
        self.refresh_projects();
        self.refresh_device_bundle();
        self.pump_commands();

        // recv loop が schedule compile の buffer_frames (leaf sidechain の
        // 1-buffer 補償量) に使う実測値。変化時のみ store (steady state では
        // load 1 回で済む)。
        if self.shared.last_buffer_frames.load(Ordering::Relaxed) != frames as u32 {
            self.shared
                .last_buffer_frames
                .store(frames as u32, Ordering::Release);
        }

        let n = frames;
        self.master_l[..n].fill(0.0);
        self.master_r[..n].fill(0.0);

        let export_running = self.shared.export_running.load(Ordering::SeqCst);
        let scope_project = self.shared.scope_project();
        // Global Sampler の録音源が縛られている project: `Master` はアクティブなタブ
        // (= scope と同じ)、`Track` はその tap の project。
        let sampler_project = match self.sampler.as_deref().map(|r| r.source) {
            Some(SamplerSource::Track { project, .. }) => project,
            Some(SamplerSource::Master) | None => scope_project,
        };

        // r.md #51: transport の要求 (Play / Stop) と seek の消費、および
        // 「今どういう状態か」の publish は **どの早期 return よりも先に、全 project
        // について**行う。
        //
        // 旧実装は count-in と書き出しの早期 return より後ろに置いていたため、
        // その間に届いた Stop が永久に消費されず、
        // - count-in を取り消したのに preroll が 0 になった瞬間に曲が鳴り出す
        // - 書き出しが終わった瞬間に (誰も押していないのに) 再生が再開する
        // という 2 つの取りこぼしが起きていた。要求は届いた順に必ず消費する。
        // 窓の間 (凍結中) の project の要求は atomic に残り、窓が閉じた buffer で消費される。
        for p in self.projects.iter_mut().filter(|p| !p.in_window()) {
            p.begin_buffer();
        }
        let ahead = self.render_ahead;

        // freewheel export: while the export thread holds the audio
        // resources, write silence and skip dispatch so the worker pool
        // and plugin instances are exclusively driven by the export
        // render loop. **全 project が止まる**。
        if export_running {
            for p in self.projects.iter_mut().filter(|p| !p.in_window()) {
                // r.md #87: 書き出し中に届いたローンチ操作は捨てる。溜めておくと
                // 書き出し明けに **全部同時に**発火し、64 件で溢れた分は無言で消える。
                p.launcher.clear_requests();
                // 停止中も現在の playhead をそのまま publish する。
                p.transport_out.now.playhead = p.shared.playhead.load(Ordering::Acquire);
                p.transport_out.publish(bridge.project(p.telemetry_slot), ahead);
            }
            return;
        }

        let frames_rendered = self.frames_rendered;
        for p in self.projects.iter_mut().filter(|p| !p.in_window()) {
            let is_scope = p.key == scope_project;
            let is_sampler = p.key == sampler_project;
            let ctx = DeviceCtx {
                worker: self.worker.as_ref(),
                sampler: if is_sampler { self.sampler.as_deref() } else { None },
                sampler_rt: &mut self.sampler_rt,
                scope: is_scope.then_some(scope),
                device_scope: is_scope
                    .then_some(DeviceScopeCtx { bridge: device_scope, headers: &mut self.scope_headers }),
                frames_rendered,
            };
            let slot = bridge.project(p.telemetry_slot);
            p.render_buffer(ctx, slot, sample_rate, n);
            // A2: publish the engine's playhead to shmem so the GUI
            // can draw the cursor. 停止中も現在の playhead をそのまま
            // publish する (= ruler click で動かした位置や、 Stop 直前
            // の位置を GUI に反映、 業界標準の挙動)。先読みのときは聞こえている回の値 (`TransportOut`)。
            p.transport_out.now.playhead = p.shared.playhead.load(Ordering::Acquire);
            p.transport_out.publish(slot, ahead);
            // この描画が窓を開けた (生き残りが文脈を持っている): bus はまだ生き残りの物なので混ぜない。
            if p.in_window() {
                continue;
            }
            // デバイス最終ミックスへ加算 (1 project なら加算 1 回 = 従来と同じコスト)。
            for i in 0..n {
                self.master_l[i] += p.bus_l[i];
                self.master_r[i] += p.bus_r[i];
            }
        }

        self.frames_rendered = self.frames_rendered.wrapping_add(n as u64);
    }
}

impl Drop for DeviceRt {
    /// 窓がまだ開いている project は解放しない (leak) — 生き残りが文脈を通してまだ触りうる (module doc)。
    /// stream を落とすのはアプリの終了時 (off-RT)。
    fn drop(&mut self) {
        for p in self.projects.drain(..).chain(self.closing.drain(..)) {
            if p.window.as_ref().is_some_and(|rig| rig.pool.as_ref().is_some_and(|pool| pool.window_open())) {
                std::mem::forget(p);
            }
        }
    }
}

impl ProjectRt {
    /// 窓の間 (この project の描画の文脈を、生き残りがまだ持っているかもしれない) か。RT はこの間 project に
    /// 触らない (module doc)。
    #[must_use]
    pub fn in_window(&self) -> bool {
        self.window.is_some()
    }

    /// 窓が閉じていれば、借りていた rig を `recycle` へ返して true (窓が無ければそのまま true)。まだ開いている
    /// (か、ring が満杯) なら false。
    fn settle_window(&mut self, recycle: &mut rtrb::Producer<DeviceBundle>) -> bool {
        let Some(rig) = &self.window else { return true };
        if rig.pool.as_ref().is_some_and(|pool| pool.window_open()) {
            return false;
        }
        if let Err(rtrb::PushError::Full(back)) = recycle.push(DeviceBundle { worker: self.window.take(), sampler: None })
        {
            self.window = back.worker;
            return false;
        }
        // 窓の間に捨てた鍵盤プレビュー (`DeviceRt::apply_command`) の対の片割れが積まれたまま残っていると、再開後に
        // On だけ鳴って Off が来ない — 残りも捨てる。
        for s in &mut self.scratch {
            s.state.pending_preview.clear();
        }
        true
    }
}
