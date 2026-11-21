//! RT でデバイスの入力 ch の音を読む口 (r.md #156、`docs/plan_audio_io.md`「入力の渡し方」)。
//!
//! 入力は 2 か所で読める。どちらも **同じ時計** (stream が始まってからコールバックに届いたフレームの通し番号) で
//! 位置が分かる:
//!
//! - **録音 (生の入力)**: [`InputTap::capture`]。デバイスのコールバックのスレッドで、届いたままの入力で毎回
//!   呼ばれる (先読みに遅らされない)。
//! - **モニター (描画の中)**: [`DeviceIo::input`]。描く 1 回 (`ProjectRt::render_buffer`) ごとに、そのフレーム数
//!   ぶんの入力。先読みのときは描画スレッドが先に描くので、コールバックが積んだ入力を wait-free の ring 越しに
//!   受け取る (描き溜めのぶん遅れる。`crate::output_render`)。
//!
//! 時刻の対応: 出力のフレーム `F` は端子から `F + 出力レイテンシ` に鳴り、入力のフレーム `D` は端子に
//! `D - 入力レイテンシ` に来た音。 だから入力のフレーム `D` は、出力のフレーム `D - 入力レイテンシ - 出力レイテンシ`
//! と同時に鳴っていた音 (先読みは [`DeviceIo::out_frame`] に織り込み済み)。
//!
//! 書き出しはこの口を通らない (`NativeIo::default()` = 入力なしで描く) — 入力音は書き出しに入らない。

use common::audio_device::ChannelRange;

/// デバイスの入力 1 ブロック。ch は planar の f32 で、長さはどれも [`Self::frames`]。入力の無いデバイスでは空。
#[derive(Clone, Copy)]
pub struct InputBlock<'a> {
    /// 使っている入力 ch の k 番目 (= デバイスの ch `channels.first + k`) は `data[k * stride + offset..][..frames]`。
    data: &'a [f32],
    stride: usize,
    offset: usize,
    frames: usize,
    channels: ChannelRange,
    device_frame: u64,
}

impl<'a> InputBlock<'a> {
    /// 入力なし (検査用)。
    #[cfg(test)]
    pub const EMPTY: InputBlock<'static> = InputBlock {
        data: &[],
        stride: 0,
        offset: 0,
        frames: 0,
        channels: ChannelRange { first: 0, count: 0 },
        device_frame: 0,
    };

    /// `data` は planar (ch ごとに `stride` 個ずつ並び、先頭の `frames` 個が有効。`frames <= stride`)。
    /// `channels` はその並びがデバイスのどの ch 番号に当たるか。`data` が足りない ch は [`Self::channel`] が
    /// `None` を返す。
    #[must_use]
    pub fn new(data: &'a [f32], stride: usize, frames: usize, channels: ChannelRange, device_frame: u64) -> Self {
        debug_assert!(frames <= stride || channels.count == 0);
        Self { data, stride, offset: 0, frames, channels, device_frame }
    }

    /// このブロックの `offset` フレーム目から `frames` フレームぶん (範囲は呼び側が保証する)。
    #[must_use]
    pub fn part(&self, offset: usize, frames: usize) -> Self {
        debug_assert!(offset + frames <= self.frames);
        Self {
            offset: self.offset + offset,
            frames,
            device_frame: self.device_frame + offset as u64,
            ..*self
        }
    }

    /// このブロックのフレーム数。
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// **デバイスの入力 ch 番号** `device_ch` (0 始まり。一覧の並び順ではない) の音。その ch を使っていない /
    /// デバイスに無いなら `None`。
    #[must_use]
    pub fn channel(&self, device_ch: u16) -> Option<&'a [f32]> {
        if !self.channels.contains(device_ch) {
            return None;
        }
        let start = usize::from(device_ch - self.channels.first) * self.stride + self.offset;
        self.data.get(start..start + self.frames)
    }

    /// このブロックの先頭のフレームの通し番号 (module doc の時計)。[`InputTap::capture`] では連続する
    /// (前のブロックの `device_frame + frames` が次の `device_frame`)。
    #[must_use]
    pub fn device_frame(&self) -> u64 {
        self.device_frame
    }
}

/// 録音用の取り出し口。**デバイスのコールバックのスレッド**で、届いたままの入力で毎回呼ばれる
/// (入力を開いていないデバイスでも、ch の無いブロックで時刻だけ進む)。
///
/// 呼ぶ順番の保証: `capture` (`device_frame` = D) は、`out_frame <= D` の描画 ([`DeviceIo::out_frame`]) が全部
/// 済んでから呼ばれる (先読みしないときは同じコールバックの描画の後。先読みのときは描画が先行している)。
/// stream を組み直しても同じ tap が引き継がれる (描き手と同じ扱い)。
///
/// RT: 確保・ロック・I/O をしないこと。
pub trait InputTap: Send + 'static {
    fn capture(&mut self, input: &InputBlock<'_>);
}

/// 描く 1 回に渡すデバイスの入出力 (`engine::DeviceCtx::io`)。
#[derive(Clone, Copy)]
pub struct DeviceIo<'a> {
    /// この描画ブロックと同じ長さの入力 (モニター用)。先読みのときは描き溜めのぶん古い。足りなかった分は無音。
    pub input: InputBlock<'a>,
    /// この描画ブロックの先頭のフレームがデバイスへ渡る時点の通し番号 (module doc の時計)。
    pub out_frame: u64,
    /// 開いているデバイスの入力 / 出力レイテンシ (frames、`AudioDeviceInfo` と同じ値)。
    pub input_latency_frames: u32,
    pub output_latency_frames: u32,
}

#[cfg(test)]
impl DeviceIo<'static> {
    /// 入力もデバイスも無い描画 (検査用)。
    pub const NONE: DeviceIo<'static> =
        DeviceIo { input: InputBlock::EMPTY, out_frame: 0, input_latency_frames: 0, output_latency_frames: 0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_is_addressed_by_device_channel_number() {
        // 使っている入力 = デバイスの ch 2, 3。stride 4 のうち 3 フレームが有効。
        let data = [1.0, 2.0, 3.0, 0.0, 10.0, 20.0, 30.0, 0.0];
        let block = InputBlock::new(&data, 4, 3, ChannelRange { first: 2, count: 2 }, 100);
        assert_eq!(block.channel(2), Some(&[1.0, 2.0, 3.0][..]));
        assert_eq!(block.channel(3), Some(&[10.0, 20.0, 30.0][..]));
        assert_eq!(block.channel(0), None, "使っていない ch");
        assert_eq!(block.channel(4), None, "範囲の外");
        assert_eq!(InputBlock::EMPTY.channel(0), None);
        assert_eq!(InputBlock::EMPTY.frames(), 0);
        // 途中から切り出しても ch 番号と時刻が合う。
        let tail = block.part(1, 2);
        assert_eq!(tail.channel(3), Some(&[20.0, 30.0][..]));
        assert_eq!(tail.device_frame(), 101);
        // 入力 ch の無いブロックは時刻だけ持つ。
        let timed = InputBlock::new(&[], 0, 256, ChannelRange::default(), 512);
        assert_eq!((timed.frames(), timed.device_frame(), timed.channel(0)), (256, 512, None));
    }
}
