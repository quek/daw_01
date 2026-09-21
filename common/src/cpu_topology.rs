//! CPU の物理コア構成 (どの論理 CPU が同じ物理コアの SMT sibling か)。
//!
//! # なぜ要るか
//!
//! RT worker (plugin_host の `process()` を回すスレッド) が SMT の sibling 同士に乗り合わせると、
//! 同じ `process()` の CPU 時間が伸びて xrun になる。「CPU 使用率 50% なのに xrun」の正体がこれ —
//! 論理 32 本の 50% は物理 16 コアがほぼ埋まった状態で、sibling に 2 本目を乗せても速くならず、
//! 1 本目まで遅くなる。
//!
//! 実測 (2026-09-21、Ryzen 9 5950X = 物理 16 / 論理 32、Analog Lab V × 40、480 frame、
//! GUI あり・File→Open → Space で再生・master ピーク −5〜−7 dBFS を毎窓で確認、
//! 他プロセス ≤ 1.2 コア。1 回 = 5 秒窓 × 6 ≈ 3,000 buffer):
//!
//! ```text
//!                                   xrun/秒 (各回)                 process() p50
//! 制限なし                          2.35  3.79  3.24               3,300〜3,460 µs
//! 各コアの 1 本目の集合に制限       0.49  1.41  1.83  1.24  1.63  0.52   1,600〜2,130 µs
//! 1 本ずつ特定の論理 CPU に固定     3.04                           1,883 µs
//! ```
//!
//! 1 本ずつの固定が負けるのは、worker がコアより多いと同じ CPU に固定された 2 本目が、
//! 空いている別のコアへ移れず 1 本目の `process()` が終わるまで待たされるため。**集合に制限し、
//! 集合のどこで走るかはスケジューラに任せる** ([`one_logical_per_core_mask`] +
//! [`restrict_current_thread_to`])。
//!
//! 制限するのは RT worker だけ。daw_audio の runner を反対側 (各コアの 2 本目) に寄せる案も
//! 測ったが良くならなかった (1.96 / 5.36 回/秒)。

/// 物理コアごとの論理 CPU の bit mask (group 0 のみ = 論理 64 本まで)。
///
/// Windows: `GetLogicalProcessorInformation` の `RelationProcessorCore` を集める。
/// 取得失敗 / 非 Windows は空。
#[must_use]
pub fn physical_core_masks() -> Vec<u64> {
    #[cfg(windows)]
    {
        windows_impl::core_masks()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 物理コアごとに代表 1 本ずつ (各コアの mask の最下位 bit) の論理 CPU を集めた mask。
/// 取れなければ `0` (= 呼び側は制限しない)。
#[must_use]
pub fn one_logical_per_core_mask() -> u64 {
    physical_core_masks().iter().fold(0u64, |acc, m| acc | (1u64 << m.trailing_zeros()))
}

/// 今のスレッドを論理 CPU の集合 `mask` に制限する。成功で `true`。`mask == 0` は何もしない。
pub fn restrict_current_thread_to(mask: u64) -> bool {
    if mask == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        windows_impl::restrict_current(mask)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
mod windows_impl {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformation, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
    };
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

    pub(super) fn core_masks() -> Vec<u64> {
        // 1 回目は必要バイト数を聞くだけ (ERROR_INSUFFICIENT_BUFFER で返る)。
        let mut len: u32 = 0;
        let _ = unsafe { GetLogicalProcessorInformation(None, &mut len) };
        let n = len as usize / std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
        if n == 0 {
            return Vec::new();
        }
        let mut buf: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> =
            vec![unsafe { std::mem::zeroed() }; n];
        if unsafe { GetLogicalProcessorInformation(Some(buf.as_mut_ptr()), &mut len) }.is_err() {
            return Vec::new();
        }
        let got = len as usize / std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
        let mut masks: Vec<u64> = buf
            .iter()
            .take(got)
            .filter(|e| e.Relationship == RelationProcessorCore)
            .map(|e| e.ProcessorMask as u64)
            .filter(|m| *m != 0)
            .collect();
        masks.sort_unstable_by_key(|m| m.trailing_zeros());
        masks
    }

    pub(super) fn restrict_current(mask: u64) -> bool {
        let Ok(mask) = usize::try_from(mask) else { return false };
        // 0 が返ったら失敗 (成功時は前の mask が返る)。
        unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_masks_are_disjoint_and_the_set_takes_one_per_core() {
        let masks = physical_core_masks();
        // 取れない環境 (非 Windows / 64 論理超) は空でよい。
        let mut seen = 0u64;
        for m in &masks {
            assert_ne!(*m, 0);
            assert_eq!(seen & m, 0, "同じ論理 CPU が 2 つのコアに出た: {m:#b}");
            seen |= m;
        }
        let set = one_logical_per_core_mask();
        assert_eq!(set.count_ones() as usize, masks.len(), "集合はコアごとにちょうど 1 本");
        for m in &masks {
            assert_eq!((set & m).count_ones(), 1, "コア {m:#b} から 1 本だけ選ぶ");
        }
    }
}
