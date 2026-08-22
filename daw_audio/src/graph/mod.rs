// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Routing graph for the audio engine.
//!
//! [`Schedule`] is an immutable, RT-safe execution plan compiled from the
//! current `Song` (`compile`), delivered to the CPAL audio thread inside an
//! `RtBundle` (wait-free SPSC ring) and driven per buffer by `execute`
//! (`render_master_buffer` — live と offline export の共通経路)。
//! plugin の addressing は安定 device id (`PluginInstance::id`) 一本
//! (`docs/plan_arch_refactor.md` §1/§5)。

// bin crate なので「外部 API 面」は無く、 再 export の一部 (GraphError /
// DelayKey / FollowerSlot 等) は module 内テスト経由でしか名指しされない —
// unused-import 警告を抑止して re-export 面を安定に保つ。
#![allow(unused_imports)]

mod compile;
mod delay_line;
pub mod execute;
mod follower;
mod port_buffer;
mod schedule;

pub use compile::{DeviceLatencies, GraphError, compile_schedule};
#[cfg(test)]
pub(crate) use compile::compile_schedule_for_test;
pub use delay_line::DelayLine;
pub use execute::{
    execute_schedule_post_dispatch, process_master_fx_chain, process_track_owned,
    render_master_buffer, set_pd_transport,
};
pub use follower::FollowerSlot;
pub use port_buffer::{PortBuffer, PortBufferPool};
pub use schedule::{BufRef, DelayKey, NodeOp, Schedule};
