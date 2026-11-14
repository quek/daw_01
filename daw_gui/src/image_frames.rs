//! 画像のフレーム列 — 静止画は 1 枚、アニメーション GIF / APNG / WebP は全フレーム。
//!
//! decode はここが唯一の口 (取り込み / 開き直し / 書き出しが同じ絵を見る)。
//! 表示側は [`FrameTiming::frame_at`] に「イベント開始からの経過秒」を渡して
//! フレームを選ぶ。経過秒は playhead から決まるので、停止中のスクラブでも
//! 書き出しでも同じフレームになる (時計で回さない)。
//!
//! GPU には 1 フレーム 1 texture で載せる ([`ImageTextures`])。同じ画像を別の時刻で
//! 2 か所に置いても、それぞれが自分のフレームを引ける。

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use daw_ui_renderer::TextureHandle;
use image::{AnimationDecoder, ImageFormat, ImageReader};

/// decode 済みのフレーム列が使ってよい CPU メモリの上限 (BGRA8 の合計)。
/// 超えた分のフレームは読まず、そこまでをループする (長大な GIF で GUI を
/// メモリ不足で落とさない)。
const MAX_DECODED_BYTES: usize = 512 * 1024 * 1024;

/// ブラウザ (Chrome / Firefox) と同じく、10 ms 以下の delay は 100 ms とみなす。
/// delay 0 の GIF は「最速」ではなく「既定の速さ」として作られていることが多い。
const MIN_DELAY_MS: f64 = 10.0;
const FALLBACK_DELAY_MS: f64 = 100.0;

/// フレームの切り替わり時刻とループ回数。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameTiming {
    /// 各フレームの終わりの時刻 (先頭からの累積秒、単調増加)。静止画は空。
    ends_secs: Vec<f64>,
    /// `None` = 無限ループ、`Some(n)` = n 周したら最後のフレームで止まる。
    loops: Option<u32>,
}

impl FrameTiming {
    /// 各フレームの表示時間 (秒) から作る。
    pub fn from_durations(durations_secs: &[f64], loops: Option<u32>) -> Self {
        let mut acc = 0.0;
        let ends_secs = durations_secs
            .iter()
            .map(|d| {
                acc += d;
                acc
            })
            .collect();
        Self { ends_secs, loops }
    }

    /// イベント開始から `secs` 秒の時点で見せるフレームの番号。
    pub fn frame_at(&self, secs: f64) -> usize {
        let n = self.ends_secs.len();
        let Some(&total) = self.ends_secs.last() else {
            return 0;
        };
        // NaN は `is_finite` で落とす (以降の剰余が NaN になる)。
        if n <= 1 || !secs.is_finite() || secs <= 0.0 || !total.is_finite() || total <= 0.0 {
            return 0;
        }
        if let Some(loops) = self.loops
            && secs >= total * f64::from(loops)
        {
            return n - 1;
        }
        let t = secs % total;
        self.ends_secs.partition_point(|&end| end <= t).min(n - 1)
    }
}

/// decode 済みの画像 (BGRA8)。
#[derive(Debug)]
pub struct ImagePixels {
    pub width: u32,
    pub height: u32,
    /// 各フレームの BGRA8 (長さ `width * height * 4`)。必ず 1 枚以上。
    pub frames: Vec<Vec<u8>>,
    pub timing: FrameTiming,
}

/// 画像ファイルを decode する。形式は中身から判定する (拡張子を信じない)。
pub fn decode_image_pixels(path: &Path) -> Result<ImagePixels, String> {
    // 既定の上限 (確保 512 MB) つきの reader。静止画はこれでそのまま読む。
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    // アニメーションとして読めなければ静止画 (先頭フレーム) として読み直す。
    // 16-bit の APNG のように、静止画なら読めるものを落とさない。
    let animated = decode_animation(path, reader.format()).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), error = %e, "animation decode failed; reading as a still image");
        None
    });
    let pixels = match animated {
        Some(p) => p,
        None => {
            let rgba = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
            let (width, height) = rgba.dimensions();
            ImagePixels {
                width,
                height,
                frames: vec![rgba_to_bgra(rgba.into_raw())],
                timing: FrameTiming::default(),
            }
        }
    };
    if pixels.width == 0 || pixels.height == 0 {
        return Err(format!("zero-sized image: {}x{}", pixels.width, pixels.height));
    }
    Ok(pixels)
}

/// アニメーションなら全フレームを読む。静止画 (アニメーションでない PNG / WebP、
/// その他の形式) は `Ok(None)`。
fn decode_animation(
    path: &Path,
    format: Option<ImageFormat>,
) -> Result<Option<ImagePixels>, String> {
    let err = |e: image::ImageError| e.to_string();
    match format {
        Some(ImageFormat::Gif) => {
            let dec = image::codecs::gif::GifDecoder::new(open_buffered(path)?).map_err(err)?;
            collect_frames(limited(dec)?).map(Some)
        }
        Some(ImageFormat::Png) => {
            // `new` + `set_limits` では png crate の内部まで上限が届かない。
            let dec =
                image::codecs::png::PngDecoder::with_limits(open_buffered(path)?, alloc_limits())
                    .map_err(err)?;
            if !dec.is_apng().map_err(err)? {
                return Ok(None);
            }
            collect_frames(limited(dec)?.apng().map_err(err)?).map(Some)
        }
        Some(ImageFormat::WebP) => {
            let dec = image::codecs::webp::WebPDecoder::new(open_buffered(path)?).map_err(err)?;
            if !dec.has_animation() {
                return Ok(None);
            }
            collect_frames(limited(dec)?).map(Some)
        }
        _ => Ok(None),
    }
}

fn alloc_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES as u64);
    limits
}

fn open_buffered(path: &Path) -> Result<BufReader<File>, String> {
    File::open(path).map(BufReader::new).map_err(|e| e.to_string())
}

/// decoder を直に作ると確保量の上限が無い (`ImageReader` は既定の上限つき)。
/// 巨大なキャンバスを名乗る壊れた / 悪意のあるファイルで確保させないよう、
/// 1 フレームの大きさを decode の前に見て、decoder にも同じ上限を渡す。
///
/// `set_limits` が確保量まで見るのは GIF と PNG だけ。WebP の decoder は寸法しか
/// 見ないので、WebP で効くのはここの寸法検査と [`collect_frames`] の累計の上限。
fn limited<D: image::ImageDecoder>(mut dec: D) -> Result<D, String> {
    let (width, height) = dec.dimensions();
    let frame_bytes = u64::from(width) * u64::from(height) * 4;
    if frame_bytes > MAX_DECODED_BYTES as u64 {
        return Err(format!("image too large: {width}x{height}"));
    }
    dec.set_limits(alloc_limits()).map_err(|e| e.to_string())?;
    Ok(dec)
}

fn collect_frames<'a>(dec: impl AnimationDecoder<'a>) -> Result<ImagePixels, String> {
    let loops = match dec.loop_count() {
        image::metadata::LoopCount::Infinite => None,
        image::metadata::LoopCount::Finite(n) => Some(n.get()),
    };
    let mut frames = Vec::new();
    let mut durations = Vec::new();
    let mut size = (0, 0);
    let mut bytes = 0usize;
    for frame in dec.into_frames() {
        let frame = match frame {
            Ok(f) => f,
            // 末尾が切れたファイル等。読めたところまでを使う (0 枚なら呼び出し側が
            // 静止画として読み直す)。
            Err(e) if !frames.is_empty() => {
                tracing::warn!(frames = frames.len(), error = %e, "animation truncated");
                break;
            }
            Err(e) => return Err(e.to_string()),
        };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = f64::from(numer) / f64::from(denom.max(1));
        let ms = if ms <= MIN_DELAY_MS { FALLBACK_DELAY_MS } else { ms };
        // decoder はフレームを画面全体に合成済みで返す (GIF の disposal も済み)。
        let buf = frame.into_buffer();
        if frames.is_empty() {
            size = buf.dimensions();
        } else if buf.dimensions() != size {
            return Err(format!(
                "animation frame size changed: {:?} -> {:?}",
                size,
                buf.dimensions()
            ));
        }
        bytes = bytes.saturating_add(buf.as_raw().len());
        // 1 フレーム目は上の寸法検査で上限内と分かっている。
        if bytes > MAX_DECODED_BYTES {
            tracing::warn!(
                frames = frames.len(),
                "animated image exceeds {MAX_DECODED_BYTES} bytes; remaining frames dropped"
            );
            break;
        }
        frames.push(rgba_to_bgra(buf.into_raw()));
        durations.push(ms / 1000.0);
    }
    if frames.is_empty() {
        return Err("animation has no frames".to_string());
    }
    let timing = if frames.len() > 1 {
        FrameTiming::from_durations(&durations, loops)
    } else {
        FrameTiming::default()
    };
    Ok(ImagePixels { width: size.0, height: size.1, frames, timing })
}

fn rgba_to_bgra(mut bytes: Vec<u8>) -> Vec<u8> {
    for px in bytes.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    bytes
}

/// 1 画像ぶんの GPU texture (1 フレーム 1 枚)。preview と書き出しが共用する。
#[derive(Debug, Clone)]
pub struct ImageTextures {
    /// 必ず 1 枚以上。
    pub frames: Vec<TextureHandle>,
    pub timing: FrameTiming,
}

impl ImageTextures {
    /// イベント開始から `secs` 秒の時点で引く texture (フレームが無ければ `None`)。
    pub fn handle_at(&self, secs: f64) -> Option<TextureHandle> {
        self.frames.get(self.timing.frame_at(secs)).or(self.frames.last()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_at_はフレームの切り替わりとループ回数に従う() {
        // 0.125s, 0.25s, 0.125s の 3 フレーム (1 周 0.5s)。境界を厳密に突くため
        // 2 進で正確に表せる値にしている。
        let inf = FrameTiming::from_durations(&[0.125, 0.25, 0.125], None);
        let twice = FrameTiming::from_durations(&[0.125, 0.25, 0.125], Some(2));
        let cases: &[(&FrameTiming, f64, usize)] = &[
            (&inf, 0.0, 0),
            (&inf, -1.0, 0),
            (&inf, f64::NAN, 0),
            (&inf, 0.0625, 0),
            (&inf, 0.125, 1), // 境界は次のフレーム
            (&inf, 0.25, 1),
            (&inf, 0.375, 2),
            (&inf, 0.5625, 0), // 2 周目
            (&inf, 10.25, 1),
            (&twice, 0.9375, 2),
            (&twice, 1.0, 2), // 2 周で最後のフレームに止まる
            (&twice, 100.0, 2),
        ];
        for &(timing, secs, want) in cases {
            assert_eq!(timing.frame_at(secs), want, "secs={secs} loops={:?}", timing.loops);
        }
        assert_eq!(FrameTiming::default().frame_at(5.0), 0);
    }

    #[test]
    fn アニメーション_gif_を全フレーム_bgra_で読む() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, Rgba, RgbaImage};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("anim.gif");
        {
            let file = File::create(&path).expect("create");
            let mut enc = GifEncoder::new(file);
            enc.set_repeat(Repeat::Infinite).expect("repeat");
            let colors = [[255, 0, 0, 255], [0, 0, 255, 255]];
            let delays = [50, 200];
            for (c, ms) in colors.iter().zip(delays) {
                let img = RgbaImage::from_pixel(4, 2, Rgba(*c));
                let delay = Delay::from_numer_denom_ms(ms, 1);
                enc.encode_frame(Frame::from_parts(img, 0, 0, delay)).expect("frame");
            }
        }
        let px = decode_image_pixels(&path).expect("decode");
        assert_eq!((px.width, px.height), (4, 2));
        assert_eq!(px.frames.len(), 2);
        // BGRA: 赤 → (0,0,255,255)、青 → (255,0,0,255)
        assert_eq!(&px.frames[0][0..4], &[0, 0, 255, 255]);
        assert_eq!(&px.frames[1][0..4], &[255, 0, 0, 255]);
        assert_eq!(px.timing.frame_at(0.04), 0);
        assert_eq!(px.timing.frame_at(0.06), 1);
        assert_eq!(px.timing.frame_at(0.26), 0); // 0.25s で 1 周
    }

    #[test]
    fn 静止画は_1_フレーム() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("still.gif");
        image::RgbaImage::from_pixel(3, 3, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .expect("save");
        let px = decode_image_pixels(&path).expect("decode");
        assert_eq!(px.frames.len(), 1);
        assert_eq!(px.timing, FrameTiming::default());
    }
}
