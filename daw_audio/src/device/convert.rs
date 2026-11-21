//! デバイスの sample 形式 ⇔ engine の f32 (`-1.0..=1.0`) の変換。RT で呼ぶ (確保しない)。
//!
//! ASIO の buffer は ch ごとの byte 列で、形式は ch ごとに決まる (`ASIOChannelInfo::type`)。扱うのは
//! little-endian の PCM 5 種: Int16 / Int24 (3 byte 詰め) / Int32 / Float32 / Float64。Windows のドライバーが
//! 実際に使うのはこの範囲 (big-endian と、32 bit の器に 16〜24 bit を右詰めする形式は開くときに断る)。
//!
//! 整数への変換は **2^(bit-1) 倍して飽和** (`+1.0` は最大値に張り付く)。整数からの変換は `2^(bit-1)` で割る
//! (`-1.0` ちょうど 〜 `+1.0` の 1 LSB 手前)。往復で 1 LSB を超えてずれない。

/// 扱える sample 形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    I16,
    /// 3 byte 詰め (little-endian、符号は最上位 byte の bit 7)。
    I24,
    I32,
    F32,
    F64,
}

impl SampleFormat {
    #[cfg(test)]
    const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// ASIO の sample type から。扱えない型は `None`。
    #[cfg(all(windows, target_pointer_width = "64"))]
    pub fn from_asio(sample_type: crate::asio::sys::AsioSampleType) -> Option<Self> {
        use crate::asio::sys;
        match sample_type {
            sys::ASIOST_INT16_LSB => Some(Self::I16),
            sys::ASIOST_INT24_LSB => Some(Self::I24),
            sys::ASIOST_INT32_LSB => Some(Self::I32),
            sys::ASIOST_FLOAT32_LSB => Some(Self::F32),
            sys::ASIOST_FLOAT64_LSB => Some(Self::F64),
            _ => None,
        }
    }
}

const I16_SCALE: f32 = 32_768.0;
const I24_SCALE: f32 = 8_388_608.0;
const I32_SCALE: f64 = 2_147_483_648.0;

/// f32 → 24 bit の整数 (飽和)。
fn to_i24(s: f32) -> i32 {
    // `as` は飽和し、NaN は 0 になる。
    ((s * I24_SCALE) as i32).clamp(-8_388_608, 8_388_607)
}

/// デバイスの byte 列 (`src`、1 ch ぶん) を f32 へ。`dst.len()` sample ぶん読む (`src` が足りない分は触らない)。
pub fn decode(format: SampleFormat, src: &[u8], dst: &mut [f32]) {
    match format {
        SampleFormat::I16 => {
            for (d, b) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                *d = f32::from(i16::from_le_bytes(*b)) / I16_SCALE;
            }
        }
        SampleFormat::I24 => {
            for (d, b) in dst.iter_mut().zip(src.as_chunks::<3>().0) {
                // 上位 3 byte に置いて算術右シフト = 符号拡張。
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                *d = v as f32 / I24_SCALE;
            }
        }
        SampleFormat::I32 => {
            for (d, b) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                *d = (f64::from(i32::from_le_bytes(*b)) / I32_SCALE) as f32;
            }
        }
        SampleFormat::F32 => {
            for (d, b) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                *d = f32::from_le_bytes(*b);
            }
        }
        SampleFormat::F64 => {
            for (d, b) in dst.iter_mut().zip(src.as_chunks::<8>().0) {
                *d = f64::from_le_bytes(*b) as f32;
            }
        }
    }
}

/// f32 をデバイスの byte 列 (`dst`、1 ch ぶん) へ。`src` は `step` 個おきに読む (interleave の 1 ch を取り出す)。
pub fn encode(format: SampleFormat, src: &[f32], step: usize, dst: &mut [u8]) {
    let samples = src.iter().step_by(step.max(1)).copied();
    match format {
        SampleFormat::I16 => {
            for (b, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(samples) {
                *b = ((s * I16_SCALE) as i16).to_le_bytes();
            }
        }
        SampleFormat::I24 => {
            for (b, s) in dst.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                b.copy_from_slice(&to_i24(s).to_le_bytes()[..3]);
            }
        }
        SampleFormat::I32 => {
            for (b, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
                *b = ((f64::from(s) * I32_SCALE) as i32).to_le_bytes();
            }
        }
        SampleFormat::F32 => {
            for (b, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
                *b = s.to_le_bytes();
            }
        }
        SampleFormat::F64 => {
            for (b, s) in dst.as_chunks_mut::<8>().0.iter_mut().zip(samples) {
                *b = f64::from(s).to_le_bytes();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_one(format: SampleFormat, s: f32) -> Vec<u8> {
        let mut out = vec![0xAA; format.bytes()];
        encode(format, &[s], 1, &mut out);
        out
    }

    fn decode_one(format: SampleFormat, bytes: &[u8]) -> f32 {
        let mut out = [f32::NAN];
        decode(format, bytes, &mut out);
        out[0]
    }

    /// Int24 (3 byte 詰め): 符号と両端。負の値は最上位 byte の bit 7 から符号拡張される。
    #[test]
    fn int24_packed_sign_and_edges() {
        let f = SampleFormat::I24;
        // +1.0 は最大値 0x7FFFFF に張り付く (0x800000 = -1.0 に回り込まない)。
        assert_eq!(encode_one(f, 1.0), [0xFF, 0xFF, 0x7F]);
        assert_eq!(encode_one(f, 2.0), [0xFF, 0xFF, 0x7F]);
        assert_eq!(encode_one(f, -1.0), [0x00, 0x00, 0x80]);
        assert_eq!(encode_one(f, -2.0), [0x00, 0x00, 0x80]);
        assert_eq!(encode_one(f, 0.0), [0x00, 0x00, 0x00]);
        assert_eq!(encode_one(f, 0.5), [0x00, 0x00, 0x40]);
        // -1 LSB = 0xFFFFFF。
        assert_eq!(encode_one(f, -1.0 / I24_SCALE), [0xFF, 0xFF, 0xFF]);
        assert_eq!(encode_one(f, f32::NAN), [0x00, 0x00, 0x00]);

        assert_eq!(decode_one(f, &[0x00, 0x00, 0x80]), -1.0);
        assert_eq!(decode_one(f, &[0xFF, 0xFF, 0xFF]), -1.0 / I24_SCALE);
        assert_eq!(decode_one(f, &[0x01, 0x00, 0x00]), 1.0 / I24_SCALE);
        assert_eq!(decode_one(f, &[0xFF, 0xFF, 0x7F]), 8_388_607.0 / I24_SCALE);
        assert_eq!(decode_one(f, &[0x00, 0x00, 0x40]), 0.5);
    }

    #[test]
    fn int16_and_int32_edges() {
        assert_eq!(encode_one(SampleFormat::I16, 1.0), i16::MAX.to_le_bytes());
        assert_eq!(encode_one(SampleFormat::I16, -1.0), i16::MIN.to_le_bytes());
        assert_eq!(encode_one(SampleFormat::I16, -0.5), (-16_384i16).to_le_bytes());
        assert_eq!(decode_one(SampleFormat::I16, &i16::MIN.to_le_bytes()), -1.0);
        assert_eq!(decode_one(SampleFormat::I16, &(-1i16).to_le_bytes()), -1.0 / I16_SCALE);

        assert_eq!(encode_one(SampleFormat::I32, 1.0), i32::MAX.to_le_bytes());
        assert_eq!(encode_one(SampleFormat::I32, -1.0), i32::MIN.to_le_bytes());
        assert_eq!(encode_one(SampleFormat::I32, 0.25), (1i32 << 29).to_le_bytes());
        assert_eq!(decode_one(SampleFormat::I32, &i32::MIN.to_le_bytes()), -1.0);
        assert_eq!(decode_one(SampleFormat::I32, &(1i32 << 30).to_le_bytes()), 0.5);
    }

    #[test]
    fn floats_pass_through() {
        assert_eq!(encode_one(SampleFormat::F32, 0.123), 0.123f32.to_le_bytes());
        assert_eq!(decode_one(SampleFormat::F32, &(-1.5f32).to_le_bytes()), -1.5);
        assert_eq!(encode_one(SampleFormat::F64, -0.75), (-0.75f64).to_le_bytes());
        assert_eq!(decode_one(SampleFormat::F64, &0.25f64.to_le_bytes()), 0.25);
    }

    /// 整数形式は往復で 1 LSB を超えてずれない。
    #[test]
    fn integer_round_trip_stays_within_one_lsb() {
        for (format, lsb) in [
            (SampleFormat::I16, 1.0 / I16_SCALE),
            (SampleFormat::I24, 1.0 / I24_SCALE),
            (SampleFormat::I32, 1.0 / I16_SCALE / I16_SCALE),
        ] {
            for i in -20..=20 {
                let s = i as f32 * 0.049;
                let back = decode_one(format, &encode_one(format, s));
                assert!((back - s).abs() <= lsb, "{format:?} {s} -> {back}");
            }
        }
    }

    /// interleave の 1 ch だけを取り出して書く / `dst` より長い `src` は余りを無視する。
    #[test]
    fn encode_picks_one_channel_of_interleaved_input() {
        let interleaved = [0.5, -0.5, 0.25, -0.25];
        let mut left = [0u8; 4];
        let mut right = [0u8; 4];
        encode(SampleFormat::I16, &interleaved, 2, &mut left);
        encode(SampleFormat::I16, &interleaved[1..], 2, &mut right);
        assert_eq!(left, [0x00, 0x40, 0x00, 0x20]);
        assert_eq!(right, [0x00, 0xC0, 0x00, 0xE0]);
    }
}
