//! 「オーディオ」ページの dropdown の選択肢 (r.md #156): 一覧に並べる文字、選ぶと何になるか、今どれか。
//!
//! 描画 ([`super::audio`]) から分けてあるのは、ここが「state → 選択肢」の対応だけを持つから
//! (一覧に無いデバイス・使っていない入力・デバイスの既定、の扱いはここで決まる)。
//!
//! 今どれかは **実際に開いているデバイス** ([`AudioDeviceInfo`]) から取る。デバイスそのものの選択
//! (どれを開きたいか) だけは設定 ([`common::audio_device::AudioDeviceConfig`]) から取る —
//! 開けずに OS 標準へ戻っていても、選んだデバイスは選ばれたまま見える。

use common::audio_device::{
    AudioChannelInfo, AudioDeviceEntry, AudioDeviceInfo, AudioDeviceList, AudioDeviceRef, AudioDriverKind,
    AudioInputDevice, ChannelRange,
};

/// dropdown 1 つぶんの選択肢。`labels[i]` を選ぶと `values[i]` になる。
pub(super) struct Choices<T> {
    pub labels: Vec<String>,
    pub values: Vec<T>,
    /// 今の値の位置。どれでもなければ [`NONE_SELECTED`] (dropdown の本体は空)。
    pub selected: usize,
}

/// どの項目も選ばれていない。
pub(super) const NONE_SELECTED: usize = usize::MAX;

impl<T> Choices<T> {
    fn new() -> Self {
        Self { labels: Vec::new(), values: Vec::new(), selected: NONE_SELECTED }
    }

    fn push(&mut self, label: impl Into<String>, value: T) -> usize {
        self.labels.push(label.into());
        self.values.push(value);
        self.labels.len() - 1
    }
}

/// OS 標準のドライバーの呼び名。
pub(super) fn system_driver_label() -> &'static str {
    if cfg!(windows) { "Windows 標準" } else { "OS 標準" }
}

/// ドライバーの種類。合成のデバイス (検査用) は、今それで動いているときだけ並べる。
pub(super) fn driver_kinds(current: AudioDriverKind) -> Choices<AudioDriverKind> {
    let mut c = Choices::new();
    c.push(system_driver_label(), AudioDriverKind::System);
    if cfg!(windows) || current == AudioDriverKind::Asio {
        c.push("ASIO", AudioDriverKind::Asio);
    }
    if current == AudioDriverKind::Synthetic {
        c.push("合成 (検査用)", AudioDriverKind::Synthetic);
    }
    c.selected = c.values.iter().position(|k| *k == current).unwrap_or(NONE_SELECTED);
    c
}

/// 「OS の既定のデバイス」の項目名。一覧が取れていれば、今どれが既定かを添える。
fn default_device_label(entries: &[AudioDeviceEntry]) -> String {
    match entries.iter().find(|e| e.is_default) {
        Some(e) => format!("既定のデバイス ({})", e.device.name),
        None => "既定のデバイス".to_string(),
    }
}

/// 選んであるデバイスが一覧に無いときに、選択肢の末尾へ足す項目 (選んだままにできるように)。
/// 一覧を取り終えているのに無ければ「見つかりません」と添える。
fn push_unlisted<T>(c: &mut Choices<T>, device: &AudioDeviceRef, listed: bool, value: T) -> usize {
    let label =
        if listed { format!("{} (見つかりません)", device.name) } else { device.name.clone() };
    c.push(label, value)
}

/// OS 標準の出力デバイス。先頭は「既定のデバイス」(= `None`)。
pub(super) fn system_outputs(
    list: Option<&AudioDeviceList>,
    current: Option<&AudioDeviceRef>,
) -> Choices<Option<AudioDeviceRef>> {
    let entries = list.map_or(&[][..], |l| &l.system_outputs[..]);
    let mut c = Choices::new();
    c.selected = c.push(default_device_label(entries), None);
    for e in entries {
        c.push(e.device.name.clone(), Some(e.device.clone()));
    }
    if let Some(cur) = current {
        c.selected = match entries.iter().position(|e| e.device.id == cur.id) {
            Some(i) => i + 1,
            None => push_unlisted(&mut c, cur, list.is_some(), Some(cur.clone())),
        };
    }
    c
}

/// OS 標準の入力デバイス。先頭 2 つは「なし」と「既定のデバイス」。
pub(super) fn system_inputs(
    list: Option<&AudioDeviceList>,
    current: &AudioInputDevice,
) -> Choices<AudioInputDevice> {
    let entries = list.map_or(&[][..], |l| &l.system_inputs[..]);
    let mut c = Choices::new();
    c.push("なし", AudioInputDevice::None);
    c.push(default_device_label(entries), AudioInputDevice::Default);
    for e in entries {
        c.push(e.device.name.clone(), AudioInputDevice::Device(e.device.clone()));
    }
    c.selected = match current {
        AudioInputDevice::None => 0,
        AudioInputDevice::Default => 1,
        AudioInputDevice::Device(cur) => match entries.iter().position(|e| e.device.id == cur.id) {
            Some(i) => i + 2,
            None => push_unlisted(&mut c, cur, list.is_some(), AudioInputDevice::Device(cur.clone())),
        },
    };
    c
}

/// ASIO のドライバー。まだ選んでいなければ、どれも選ばれていない。
pub(super) fn asio_drivers(
    list: Option<&AudioDeviceList>,
    current: Option<&AudioDeviceRef>,
) -> Choices<AudioDeviceRef> {
    let entries = list.map_or(&[][..], |l| &l.asio_drivers[..]);
    let mut c = Choices::new();
    for e in entries {
        c.push(e.device.name.clone(), e.device.clone());
    }
    if let Some(cur) = current {
        c.selected = match entries.iter().position(|e| e.device.id == cur.id) {
            Some(i) => i,
            None => push_unlisted(&mut c, cur, list.is_some(), cur.clone()),
        };
    }
    c
}

/// ch 1 本の表示名: 人が数える番号 (1 始まり) と、ドライバーが付けた名前。
fn channel_label(ch: &AudioChannelInfo) -> String {
    let number = u32::from(ch.index) + 1;
    if ch.name.is_empty() { number.to_string() } else { format!("{number}: {}", ch.name) }
}

/// 出力するステレオ組 (デバイスの ch を先頭から 2 本ずつ)。値は組の先頭の ch 番号。
pub(super) fn output_pairs(info: &AudioDeviceInfo) -> Choices<u16> {
    let mut c = Choices::new();
    for [a, b] in info.output_channels.as_chunks::<2>().0 {
        let numbers = format!("{} + {}", u32::from(a.index) + 1, u32::from(b.index) + 1);
        let label = if a.name.is_empty() || b.name.is_empty() {
            numbers
        } else {
            format!("{numbers}: {} / {}", a.name, b.name)
        };
        c.push(label, a.index);
    }
    c.selected =
        c.values.iter().position(|first| *first == info.output_first_channel).unwrap_or(NONE_SELECTED);
    c
}

/// 使う入力 ch の範囲の、始まりと終わりの選択肢。値は「それを選んだときの範囲」。
pub(super) struct InputRangeChoices {
    /// 始まり。先頭の項目は「使わない」。
    pub first: Choices<ChannelRange>,
    /// 終わり (始まり以降の ch)。入力を使っていなければ `None` (選ぶものが無い)。
    pub last: Option<Choices<ChannelRange>>,
}

/// 使う入力 ch の範囲。デバイスに入力 ch が無ければ `None`。
pub(super) fn input_range(info: &AudioDeviceInfo) -> Option<InputRangeChoices> {
    let channels = &info.input_channels;
    let device_last = channels.last()?.index;
    let active = info.active_inputs;
    let in_use = active.count > 0;
    // 今の終わり。使っていないところから始まりを選んだら、そこから最後の ch までを使う
    // (「未指定 = 全部」と同じ向き)。
    let active_last = if in_use { active.first.saturating_add(active.count - 1) } else { device_last };

    let mut first = Choices::new();
    first.selected = first.push("使わない", ChannelRange { first: 0, count: 0 });
    for ch in channels {
        // 始まりを今の終わりより後ろへ動かしたら、終わりも一緒に動かす (範囲が裏返らない)。
        let last = active_last.max(ch.index);
        let i = first.push(channel_label(ch), range_between(ch.index, last));
        if in_use && ch.index == active.first {
            first.selected = i;
        }
    }
    if in_use && first.selected == 0 {
        first.selected = NONE_SELECTED;
    }

    let last = in_use.then(|| {
        let mut last = Choices::new();
        for ch in channels.iter().filter(|ch| ch.index >= active.first) {
            let i = last.push(channel_label(ch), range_between(active.first, ch.index));
            if ch.index == active_last {
                last.selected = i;
            }
        }
        last
    });
    Some(InputRangeChoices { first, last })
}

/// ch 番号 `first` から `last` まで (両端を含む)。
fn range_between(first: u16, last: u16) -> ChannelRange {
    ChannelRange { first, count: (last - first).saturating_add(1) }
}

/// サンプルレート / バッファサイズのように「デバイスの既定」か、候補のどれかを選ぶ項目。
/// 値は設定に入れるもの (`None` = デバイスの既定)。
///
/// 本体に出るのはいつも **実際の値** `actual`: 未指定なら「既定 (実際の値)」、指定していれば実際の値の
/// 項目 (指定どおりに開けなかったときも、実際の値が見える)。
pub(super) fn device_default_or(
    candidates: &[u32],
    actual: u32,
    wanted: Option<u32>,
    label: impl Fn(u32) -> String,
) -> Choices<Option<u32>> {
    let mut c = Choices::new();
    let default_label = if wanted.is_none() { format!("既定 ({})", label(actual)) } else { "既定".to_string() };
    c.selected = c.push(default_label, None);
    let mut numbers = candidates.to_vec();
    numbers.push(actual);
    numbers.sort_unstable();
    numbers.dedup();
    for n in numbers {
        let i = c.push(label(n), Some(n));
        if wanted.is_some() && n == actual {
            c.selected = i;
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, is_default: bool) -> AudioDeviceEntry {
        AudioDeviceEntry { device: AudioDeviceRef { id: id.into(), name: name.into() }, is_default }
    }

    fn channels(count: u16) -> Vec<AudioChannelInfo> {
        (0..count).map(|index| AudioChannelInfo { index, name: String::new() }).collect()
    }

    /// 選んであるデバイスを外しても、選択は消えずに「見つかりません」付きで残る
    /// (一覧をまだ取っていない間は、名前だけで出す)。
    #[test]
    fn a_selected_device_that_is_not_listed_stays_selectable() {
        let gone = AudioDeviceRef { id: "wasapi:gone".into(), name: "USB DAC".into() };
        let list = AudioDeviceList {
            system_outputs: vec![entry("wasapi:a", "Speakers", true)],
            ..Default::default()
        };

        let listed = system_outputs(Some(&list), Some(&gone));
        assert_eq!(listed.labels, ["既定のデバイス (Speakers)", "Speakers", "USB DAC (見つかりません)"]);
        assert_eq!(listed.selected, 2);
        assert_eq!(listed.values[2].as_ref(), Some(&gone));

        let not_listed_yet = system_outputs(None, Some(&gone));
        assert_eq!(not_listed_yet.labels, ["既定のデバイス", "USB DAC"]);
        assert_eq!(not_listed_yet.selected, 1);

        // 照合は id。名前が変わっていても同じデバイスとして選ばれる。
        let renamed = AudioDeviceRef { id: "wasapi:a".into(), name: "旧い名前".into() };
        assert_eq!(system_outputs(Some(&list), Some(&renamed)).selected, 1);
    }

    #[test]
    fn asio_has_no_selection_until_a_driver_is_chosen() {
        let list = AudioDeviceList {
            asio_drivers: vec![entry("{A}", "Driver A", false), entry("{B}", "Driver B", false)],
            ..Default::default()
        };
        assert_eq!(asio_drivers(Some(&list), None).selected, NONE_SELECTED);
        let b = AudioDeviceRef { id: "{B}".into(), name: "Driver B".into() };
        assert_eq!(asio_drivers(Some(&list), Some(&b)).selected, 1);
    }

    /// 入力の範囲: 始まりを終わりより後ろへ動かすと終わりも付いてくる。終わりは始まり以降だけを並べる。
    #[test]
    fn the_input_range_never_turns_inside_out() {
        let info = AudioDeviceInfo {
            input_channels: channels(8),
            active_inputs: ChannelRange { first: 2, count: 2 },
            ..Default::default()
        };
        let c = input_range(&info).unwrap();
        assert_eq!(c.first.selected, 3, "「使わない」の次から ch 1, 2, 3 …");
        assert_eq!(c.first.values[0], ChannelRange { first: 0, count: 0 });
        assert_eq!(c.first.values[1], ChannelRange { first: 0, count: 4 }, "始まりを前へ: 終わりはそのまま");
        assert_eq!(c.first.values[6], ChannelRange { first: 5, count: 1 }, "始まりを終わりの後ろへ");

        let last = c.last.unwrap();
        assert_eq!(last.labels, ["3", "4", "5", "6", "7", "8"]);
        assert_eq!(last.selected, 1);
        assert_eq!(last.values[5], ChannelRange { first: 2, count: 6 });
    }

    /// 入力を使っていないとき: 終わりは選べず、始まりを選ぶとそこから最後の ch までを使う。
    #[test]
    fn choosing_a_start_from_unused_takes_the_rest_of_the_channels() {
        let info = AudioDeviceInfo {
            input_channels: channels(4),
            active_inputs: ChannelRange { first: 0, count: 0 },
            ..Default::default()
        };
        let c = input_range(&info).unwrap();
        assert_eq!(c.first.selected, 0);
        assert!(c.last.is_none());
        assert_eq!(c.first.values[2], ChannelRange { first: 1, count: 3 });

        assert!(input_range(&AudioDeviceInfo::default()).is_none(), "入力 ch の無いデバイス");
    }

    /// 本体に出るのはいつも実際の値: 未指定なら「既定 (値)」、指定どおりに開けなくても実際の値の項目。
    #[test]
    fn a_device_default_shows_the_actual_value() {
        let hz = |n: u32| format!("{n} Hz");
        let unset = device_default_or(&[44_100, 48_000, 96_000], 48_000, None, hz);
        assert_eq!(unset.labels, ["既定 (48000 Hz)", "44100 Hz", "48000 Hz", "96000 Hz"]);
        assert_eq!(unset.selected, 0);

        // 96 kHz を望んだが 48 kHz で開いた。
        let refused = device_default_or(&[44_100, 48_000, 96_000], 48_000, Some(96_000), hz);
        assert_eq!(refused.labels[0], "既定");
        assert_eq!(refused.labels[refused.selected], "48000 Hz");

        // 候補に無い値で動いていても、その値が出る。
        let odd = device_default_or(&[64, 128], 480, Some(480), |n| n.to_string());
        assert_eq!(odd.labels, ["既定", "64", "128", "480"]);
        assert_eq!(odd.selected, 3);
    }

    #[test]
    fn output_pairs_are_addressed_by_the_first_channel_number() {
        let mut info = AudioDeviceInfo { output_channels: channels(5), output_first_channel: 2, ..Default::default() };
        let c = output_pairs(&info);
        assert_eq!(c.labels, ["1 + 2", "3 + 4"], "余った 1 本は組にならない");
        assert_eq!(c.values, [0, 2]);
        assert_eq!(c.selected, 1);

        info.output_channels[0].name = "Main L".into();
        info.output_channels[1].name = "Main R".into();
        assert_eq!(output_pairs(&info).labels[0], "1 + 2: Main L / Main R");
    }
}
