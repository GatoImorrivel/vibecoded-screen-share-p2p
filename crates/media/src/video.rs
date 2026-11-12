#[cfg(feature = "codecs")]
pub use codec::{H264Decoder, H264Encoder};

/// Byte order of a packed 32-bit capture frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
}

/// A raw captured frame in a packed 32-bit format.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub pts_us: u64,
}

/// A planar YUV 4:2:0 frame with tightly packed planes.
#[derive(Debug, Clone)]
pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub pts_us: u64,
}

impl I420Frame {
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }

    /// Converts to packed RGBA; used for thumbnails, not the hot render path.
    pub fn to_rgba(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let cw = self.chroma_width() as usize;
        let mut out = vec![0u8; w * h * 4];
        for row in 0..h {
            for col in 0..w {
                let y = self.y[row * w + col] as f32;
                let u = self.u[(row / 2) * cw + col / 2] as f32;
                let v = self.v[(row / 2) * cw + col / 2] as f32;
                let (r, g, b) = yuv_to_rgb(y, u, v);
                let o = (row * w + col) * 4;
                out[o] = r;
                out[o + 1] = g;
                out[o + 2] = b;
                out[o + 3] = 255;
            }
        }
        out
    }
}

// BT.709 limited range, matching the WGSL shader in the app crate.
fn yuv_to_rgb(y: f32, u: f32, v: f32) -> (u8, u8, u8) {
    let y = (y - 16.0) * 1.164;
    let u = u - 128.0;
    let v = v - 128.0;
    let r = y + 1.793 * v;
    let g = y - 0.213 * u - 0.533 * v;
    let b = y + 2.112 * u;
    (
        r.clamp(0.0, 255.0) as u8,
        g.clamp(0.0, 255.0) as u8,
        b.clamp(0.0, 255.0) as u8,
    )
}

/// Fits `(w, h)` inside `(max_w, max_h)` keeping aspect ratio, with even dimensions.
pub fn fit_dimensions(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    let ow = ((w as f64 * scale) as u32).max(2) & !1;
    let oh = ((h as f64 * scale) as u32).max(2) & !1;
    (ow, oh)
}

/// Converts a packed frame to I420 at `(out_w, out_h)` using nearest-neighbour
/// sampling for luma and 2x2 averaging for chroma.
pub fn convert_to_i420(src: &RawFrame, out_w: u32, out_h: u32) -> I420Frame {
    let (ow, oh) = (out_w as usize, out_h as usize);
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (ri, bi) = match src.format {
        PixelFormat::Bgra => (2, 0),
        PixelFormat::Rgba => (0, 2),
    };
    let x_map: Vec<usize> = (0..ow).map(|x| (x * sw / ow).min(sw - 1) * 4).collect();
    let y_map: Vec<usize> = (0..oh)
        .map(|y| (y * sh / oh).min(sh - 1) * src.stride)
        .collect();

    let cw = ow.div_ceil(2);
    let ch = oh.div_ceil(2);
    let mut y_plane = vec![0u8; ow * oh];
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];

    let px = |row: usize, col: usize| -> (i32, i32, i32) {
        let o = y_map[row] + x_map[col];
        let d = &src.data[o..o + 4];
        (d[ri] as i32, d[1] as i32, d[bi] as i32)
    };

    for cy in 0..ch {
        for cx in 0..cw {
            let (mut rs, mut gs, mut bs, mut n) = (0, 0, 0, 0);
            for dy in 0..2 {
                let row = cy * 2 + dy;
                if row >= oh {
                    continue;
                }
                for dx in 0..2 {
                    let col = cx * 2 + dx;
                    if col >= ow {
                        continue;
                    }
                    let (r, g, b) = px(row, col);
                    // BT.709 limited range, fixed point (8 fractional bits).
                    let yv = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                    y_plane[row * ow + col] = yv.clamp(0, 255) as u8;
                    rs += r;
                    gs += g;
                    bs += b;
                    n += 1;
                }
            }
            let (r, g, b) = (rs / n, gs / n, bs / n);
            let u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
            u_plane[cy * cw + cx] = u.clamp(0, 255) as u8;
            v_plane[cy * cw + cx] = v.clamp(0, 255) as u8;
        }
    }

    I420Frame {
        width: out_w,
        height: out_h,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        pts_us: src.pts_us,
    }
}

/// An encoded H.264 access unit.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "codecs")]
mod codec {
    use super::{EncodedFrame, I420Frame};
    use anyhow::{Context, Result};
    use openh264::OpenH264API;
    use openh264::decoder::{Decoder, DecoderConfig};
    use openh264::encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        RateControlMode, UsageType,
    };
    use openh264::formats::YUVSource;
    use std::sync::Arc;

    impl YUVSource for I420Frame {
        fn dimensions(&self) -> (usize, usize) {
            (self.width as usize, self.height as usize)
        }

        fn strides(&self) -> (usize, usize, usize) {
            let c = self.chroma_width() as usize;
            (self.width as usize, c, c)
        }

        fn y(&self) -> &[u8] {
            &self.y
        }

        fn u(&self) -> &[u8] {
            &self.u
        }

        fn v(&self) -> &[u8] {
            &self.v
        }
    }

    /// Software H.264 encoder tuned for screen content.
    pub struct H264Encoder {
        encoder: Encoder,
        bitrate_bps: u32,
        fps: f32,
        force_keyframe: bool,
    }

    impl H264Encoder {
        pub fn new(bitrate_bps: u32, fps: f32) -> Result<Self> {
            Ok(Self {
                encoder: Self::build(bitrate_bps, fps)?,
                bitrate_bps,
                fps,
                force_keyframe: true,
            })
        }

        fn build(bitrate_bps: u32, fps: f32) -> Result<Encoder> {
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(bitrate_bps))
                .max_frame_rate(FrameRate::from_hz(fps))
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .complexity(Complexity::Low)
                .skip_frames(true)
                // A periodic keyframe bounds how long a corrupted picture can persist.
                .intra_frame_period(IntraFramePeriod::from_num_frames((fps * 10.0) as u32));
            Encoder::with_api_config(OpenH264API::from_source(), config)
                .context("creating H.264 encoder")
        }

        pub fn bitrate(&self) -> u32 {
            self.bitrate_bps
        }

        /// Rebuilds the encoder with a new target bitrate; the next frame becomes a keyframe.
        pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
            if bitrate_bps == self.bitrate_bps {
                return Ok(());
            }
            self.encoder = Self::build(bitrate_bps, self.fps)?;
            self.bitrate_bps = bitrate_bps;
            self.force_keyframe = true;
            Ok(())
        }

        pub fn request_keyframe(&mut self) {
            self.force_keyframe = true;
        }

        /// Encodes one frame. Returns `None` if the rate controller skipped it.
        pub fn encode(&mut self, frame: &I420Frame) -> Result<Option<EncodedFrame>> {
            if self.force_keyframe {
                self.encoder.force_intra_frame();
                self.force_keyframe = false;
            }
            let bitstream = self.encoder.encode(frame).context("encoding frame")?;
            let frame_type = bitstream.frame_type();
            if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
                return Ok(None);
            }
            let data = bitstream.to_vec();
            if data.is_empty() {
                return Ok(None);
            }
            Ok(Some(EncodedFrame {
                data,
                keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
                pts_us: frame.pts_us,
                width: frame.width,
                height: frame.height,
            }))
        }
    }

    /// Software H.264 decoder producing tightly packed I420 frames.
    pub struct H264Decoder {
        decoder: Decoder,
    }

    impl H264Decoder {
        pub fn new() -> Result<Self> {
            let decoder =
                Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
                    .context("creating H.264 decoder")?;
            Ok(Self { decoder })
        }

        pub fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<Arc<I420Frame>>> {
            let Some(yuv) = self.decoder.decode(data).context("decoding frame")? else {
                return Ok(None);
            };
            let (w, h) = yuv.dimensions();
            let (ys, us, vs) = yuv.strides();
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let copy_plane = |src: &[u8], stride: usize, pw: usize, ph: usize| {
                let mut out = Vec::with_capacity(pw * ph);
                for row in 0..ph {
                    out.extend_from_slice(&src[row * stride..row * stride + pw]);
                }
                out
            };
            Ok(Some(Arc::new(I420Frame {
                width: w as u32,
                height: h as u32,
                y: copy_plane(yuv.y(), ys, w, h),
                u: copy_plane(yuv.u(), us, cw, ch),
                v: copy_plane(yuv.v(), vs, cw, ch),
                pts_us,
            })))
        }
    }
}

#[cfg(all(test, feature = "codecs"))]
mod tests {
    use super::*;

    fn test_frame(w: u32, h: u32) -> RawFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                data[o] = (x % 256) as u8;
                data[o + 1] = (y % 256) as u8;
                data[o + 2] = 128;
                data[o + 3] = 255;
            }
        }
        RawFrame {
            width: w,
            height: h,
            stride: (w * 4) as usize,
            format: PixelFormat::Bgra,
            data,
            pts_us: 0,
        }
    }

    #[test]
    fn fit_keeps_even_dimensions() {
        assert_eq!(fit_dimensions(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_dimensions(1001, 501, 1920, 1080), (1000, 500));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let raw = test_frame(320, 240);
        let i420 = convert_to_i420(&raw, 320, 240);
        let mut enc = H264Encoder::new(1_000_000, 30.0).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let encoded = enc
            .encode(&i420)
            .unwrap()
            .expect("first frame must be encoded");
        assert!(encoded.keyframe);
        let decoded = dec
            .decode(&encoded.data, 0)
            .unwrap()
            .expect("decoded frame");
        assert_eq!((decoded.width, decoded.height), (320, 240));
    }
}
