//! r.md #138: VST3 の `IUnitInfo` (`ivstunits.h`) から鍵盤ごとの名前 (PitchName) を読む。
//! 変更通知 (`IUnitHandler::notifyProgramListChange`) の受け口は `vst3_host::Vst3ComponentHandler`。

use vst3::ComPtr;
use vst3::Steinberg::Vst::{
    IEditController, IEditControllerTrait, IUnitInfo, IUnitInfoTrait, ParameterInfo,
    ParameterInfo_::ParameterFlags_, ProgramListInfo, String128, UnitInfo, kNoProgramListId,
    kRootUnitId,
};
use vst3::Steinberg::{kResultOk, kResultTrue};

use crate::vst3_scan::utf16_buf_to_string;

/// 鍵盤ごとの名前 (`ivstunits.h` の呼び出しはすべて [UI-thread & (Initialized | Connected)]
/// = plugin-main)。
///
/// 名前は「program list の 1 program」に付くので、root unit (`kRootUnitId`) の program list と、
/// その unit の program-change param (`kIsProgramChange`) が指す今の program を引く。
/// root unit が list を持たなければ最初の list、program-change param が無ければ program 0。
pub fn key_names(controller: &ComPtr<IEditController>) -> Vec<common::protocol::KeyName> {
    let Some(units) = controller.cast::<IUnitInfo>() else {
        return Vec::new();
    };
    let Some(list_id) = program_list_id(&units) else {
        return Vec::new();
    };
    let program = current_program_index(controller);
    if unsafe { units.hasProgramPitchNames(list_id, program) } != kResultTrue {
        return Vec::new();
    }
    let raw = (0..128_i16).filter_map(|pitch| {
        let mut name: String128 = [0; 128];
        (unsafe { units.getProgramPitchName(list_id, program, pitch, &raw mut name) } == kResultOk)
            .then(|| crate::key_names::RawKeyName {
                port: 0,
                key: pitch,
                channel: -1,
                name: utf16_buf_to_string(&name),
            })
    });
    crate::key_names::normalize(raw)
}

/// root unit の program list。無ければ最初の list。list が 1 つも無ければ `None`。
fn program_list_id(units: &ComPtr<IUnitInfo>) -> Option<i32> {
    let root_list = (0..unsafe { units.getUnitCount() }.clamp(0, 4096)).find_map(|i| {
        // SAFETY: UnitInfo is plain data; zeroed start is legal.
        let mut info: UnitInfo = unsafe { std::mem::zeroed() };
        (unsafe { units.getUnitInfo(i, &raw mut info) } == kResultOk
            && info.id == kRootUnitId
            && info.programListId != kNoProgramListId)
            .then_some(info.programListId)
    });
    if root_list.is_some() {
        return root_list;
    }
    if unsafe { units.getProgramListCount() } <= 0 {
        return None;
    }
    // SAFETY: ProgramListInfo is plain data; zeroed start is legal.
    let mut info: ProgramListInfo = unsafe { std::mem::zeroed() };
    (unsafe { units.getProgramListInfo(0, &raw mut info) } == kResultOk).then_some(info.id)
}

/// root unit の program-change param (`kIsProgramChange`) が指す program index。
/// 無ければ 0。normalized 値 → index は `stepCount` (= program 数 - 1) 倍して丸める。
fn current_program_index(controller: &ComPtr<IEditController>) -> i32 {
    let count = unsafe { controller.getParameterCount() };
    for i in 0..count.max(0) {
        // SAFETY: ParameterInfo is plain data; zeroed start is legal.
        let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
        if unsafe { controller.getParameterInfo(i, &raw mut info) } != kResultOk {
            continue;
        }
        if info.flags & ParameterFlags_::kIsProgramChange == 0 || info.unitId != kRootUnitId {
            continue;
        }
        let value = unsafe { controller.getParamNormalized(info.id) }.clamp(0.0, 1.0);
        #[allow(clippy::cast_possible_truncation)]
        return (value * f64::from(info.stepCount.max(0))).round() as i32;
    }
    0
}
