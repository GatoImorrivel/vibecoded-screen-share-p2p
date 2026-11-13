#[cfg(feature = "codecs")]
pub use codec::{
    Codec, EncoderPrefs, SoftwareEncoder, VideoDecoder, VideoEncoder, encoder_backend,
    open_decoder, open_encoder,
};

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

/// A YUV 4:2:0 frame with a luma plane and one interleaved U/V plane, the
/// layout GPU encoders and decoders use.
#[derive(Debug, Clone)]
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// `chroma_width * chroma_height` U/V byte pairs.
    pub uv: Vec<u8>,
    pub pts_us: u64,
}

impl Nv12Frame {
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }
}

/// Plane layout of a [`YuvFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvLayout {
    I420,
    Nv12,
}

/// A decoded or to-be-encoded picture in either 4:2:0 layout.
#[derive(Debug, Clone)]
pub enum YuvFrame {
    I420(I420Frame),
    Nv12(Nv12Frame),
}

impl YuvFrame {
    pub fn width(&self) -> u32 {
        match self {
            Self::I420(f) => f.width,
            Self::Nv12(f) => f.width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::I420(f) => f.height,
            Self::Nv12(f) => f.height,
        }
    }

    pub fn pts_us(&self) -> u64 {
        match self {
            Self::I420(f) => f.pts_us,
            Self::Nv12(f) => f.pts_us,
        }
    }

    pub fn set_pts_us(&mut self, pts_us: u64) {
        match self {
            Self::I420(f) => f.pts_us = pts_us,
            Self::Nv12(f) => f.pts_us = pts_us,
        }
    }

    pub fn layout(&self) -> YuvLayout {
        match self {
            Self::I420(_) => YuvLayout::I420,
            Self::Nv12(_) => YuvLayout::Nv12,
        }
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

/// Scales and converts a packed frame to planar Y, U and V at `(out_w, out_h)`
/// using nearest-neighbour sampling for luma and 2x2 averaging for chroma.
fn convert_planes(src: &RawFrame, ow: usize, oh: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (sw, sh) = (src.width as usize, src.height as usize);
    let x_map: Vec<usize> = (0..ow).map(|x| (x * sw / ow).min(sw - 1) * 4).collect();
    let y_map: Vec<usize> = (0..oh)
        .map(|y| (y * sh / oh).min(sh - 1) * src.stride)
        .collect();

    let cw = ow.div_ceil(2);
    let ch = oh.div_ceil(2);
    let mut y_plane = vec![0u8; ow * oh];
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];

    // Split into bands of chroma rows; small frames aren't worth the threads.
    let threads = if ow * oh >= 640 * 360 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
    } else {
        1
    };
    let band_rows = ch.div_ceil(threads);
    let band = |first_row: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]| {
        let rows = &y_map[first_row * 2..((first_row + band_rows) * 2).min(oh)];
        match src.format {
            PixelFormat::Bgra => convert_band::<2, 0>(&src.data, &x_map, rows, ow, y, u, v),
            PixelFormat::Rgba => convert_band::<0, 2>(&src.data, &x_map, rows, ow, y, u, v),
        }
    };
    std::thread::scope(|s| {
        let bands = y_plane
            .chunks_mut(band_rows * 2 * ow)
            .zip(u_plane.chunks_mut(band_rows * cw))
            .zip(v_plane.chunks_mut(band_rows * cw))
            .enumerate();
        for (i, ((y, u), v)) in bands {
            if i + 1 == threads {
                band(i * band_rows, y, u, v);
            } else {
                s.spawn(move || band(i * band_rows, y, u, v));
            }
        }
    });
    (y_plane, u_plane, v_plane)
}

/// Converts the source rows at byte offsets `rows` (two output rows per
/// chroma row) into the matching slices of the Y, U and V planes.
fn convert_band<const R: usize, const B: usize>(
    data: &[u8],
    x_map: &[usize],
    rows: &[usize],
    ow: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    let cw = ow.div_ceil(2);
    let rgb = |o: usize| -> (i32, i32, i32) {
        let d = &data[o..o + 4];
        (d[R] as i32, d[1] as i32, d[B] as i32)
    };
    // BT.709 limited range, fixed point (8 fractional bits).
    let luma = |(r, g, b): (i32, i32, i32)| {
        (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(0, 255) as u8
    };
    for (cy, pair) in rows.chunks(2).enumerate() {
        let top = pair[0];
        let bottom = pair.get(1).copied();
        for cx in 0..cw {
            let c0 = cx * 2;
            let c1 = (c0 + 1 < ow).then_some(c0 + 1);
            let (mut rs, mut gs, mut bs, mut n) = (0, 0, 0, 0);
            for (dy, row) in [Some(top), bottom].into_iter().enumerate() {
                let Some(row) = row else { continue };
                for (dx, col) in [Some(c0), c1].into_iter().enumerate() {
                    let Some(col) = col else { continue };
                    let px = rgb(row + x_map[col]);
                    y_plane[(cy * 2 + dy) * ow + c0 + dx] = luma(px);
                    rs += px.0;
                    gs += px.1;
                    bs += px.2;
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
}

/// Converts a packed frame to I420 at `(out_w, out_h)`.
pub fn convert_to_i420(src: &RawFrame, out_w: u32, out_h: u32) -> I420Frame {
    let (y, u, v) = convert_planes(src, out_w as usize, out_h as usize);
    I420Frame {
        width: out_w,
        height: out_h,
        y,
        u,
        v,
        pts_us: src.pts_us,
    }
}

/// Converts a packed frame to NV12 at `(out_w, out_h)`.
pub fn convert_to_nv12(src: &RawFrame, out_w: u32, out_h: u32) -> Nv12Frame {
    let (y, u, v) = convert_planes(src, out_w as usize, out_h as usize);
    let mut uv = Vec::with_capacity(u.len() * 2);
    for (u, v) in u.iter().zip(&v) {
        uv.push(*u);
        uv.push(*v);
    }
    Nv12Frame {
        width: out_w,
        height: out_h,
        y,
        uv,
        pts_us: src.pts_us,
    }
}

/// Converts a packed frame to whichever layout an encoder wants.
pub fn convert_frame(src: &RawFrame, out_w: u32, out_h: u32, layout: YuvLayout) -> YuvFrame {
    match layout {
        YuvLayout::I420 => YuvFrame::I420(convert_to_i420(src, out_w, out_h)),
        YuvLayout::Nv12 => YuvFrame::Nv12(convert_to_nv12(src, out_w, out_h)),
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
    use super::{EncodedFrame, I420Frame, YuvFrame, YuvLayout};
    use anyhow::{Context, Result, bail};
    use openh264::OpenH264API;
    use openh264::encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        RateControlMode, UsageType,
    };
    use openh264::formats::YUVSource;
    use std::sync::Arc;

    /// A video codec.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Codec {
        /// Universally supported; the fallback when a peer can't do AV1.
        #[default]
        H264,
        /// Better compression on newer GPUs and modern CPUs.
        Av1,
    }

    impl Codec {
        /// All codecs, best first.
        pub const ALL: &[Codec] = &[Codec::Av1, Codec::H264];

        /// Short wire name, also used in `ShareInfo.codec`.
        pub fn name(self) -> &'static str {
            match self {
                Codec::H264 => "h264",
                Codec::Av1 => "av1",
            }
        }

        /// Parses a wire name back; unknown values fall back to H.264.
        pub fn from_name(name: &str) -> Codec {
            match name {
                "av1" => Codec::Av1,
                _ => Codec::H264,
            }
        }
    }

    /// An H.264 encoder: GPU through FFmpeg, or openh264 on the CPU.
    pub trait VideoEncoder: Send {
        /// Short human-readable backend name, e.g. "NVENC" or "software".
        fn name(&self) -> &str;
        /// The codec this encoder produces.
        fn codec(&self) -> Codec;
        /// The plane layout [`VideoEncoder::encode`] expects.
        fn input_layout(&self) -> YuvLayout;
        fn bitrate(&self) -> u32;
        /// Changes the target bitrate. Some backends rebuild themselves and
        /// emit a keyframe; others adjust in place.
        fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()>;
        fn request_keyframe(&mut self);
        /// Encodes one frame. Returns `None` when the encoder produced no
        /// output for it (rate-control skip or pipeline delay).
        fn encode(&mut self, frame: &YuvFrame) -> Result<Option<EncodedFrame>>;
    }

    /// A decoder producing frames for the YUV display shader.
    pub trait VideoDecoder: Send {
        /// Short human-readable backend name, e.g. "D3D11VA" or "software".
        fn name(&self) -> &str;
        fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<Arc<YuvFrame>>>;
    }

    #[derive(Debug, Clone, Copy)]
    pub struct EncoderPrefs {
        /// Try GPU encoders before falling back to software.
        pub hardware: bool,
        /// The codec to encode with.
        pub codec: Codec,
        pub width: u32,
        pub height: u32,
        pub fps: u32,
        pub bitrate: u32,
    }

    /// Opens the best available encoder for `prefs`.
    pub fn open_encoder(prefs: EncoderPrefs) -> Result<Box<dyn VideoEncoder>> {
        #[cfg(feature = "hw")]
        if prefs.hardware
            && let Some(encoder) = crate::ffmpeg::open_hw_encoder(prefs)
        {
            return Ok(encoder);
        }
        match prefs.codec {
            Codec::H264 => Ok(Box::new(SoftwareEncoder::new(
                prefs.bitrate,
                prefs.fps as f32,
            )?)),
            // Filled in by the rav1e-backed software encoder.
            Codec::Av1 => bail!("no AV1 encoder available on this machine"),
        }
    }

    /// Name of the encoder [`open_encoder`] would pick for `codec`, probing once per process.
    pub fn encoder_backend(hardware: bool, codec: Codec) -> &'static str {
        #[cfg(feature = "hw")]
        if hardware && let Some(name) = crate::ffmpeg::hw_encoder_name(codec) {
            return name;
        }
        let _ = (hardware, codec);
        SoftwareEncoder::NAME
    }

    /// Opens the best available decoder for `codec`; GPU first when `hardware` is set.
    pub fn open_decoder(hardware: bool, codec: Codec) -> Result<Box<dyn VideoDecoder>> {
        #[cfg(feature = "hw")]
        {
            crate::ffmpeg::open_decoder(hardware, codec)
        }
        #[cfg(not(feature = "hw"))]
        {
            let _ = hardware;
            match codec {
                Codec::H264 => Ok(Box::new(SoftwareDecoder::new()?)),
                Codec::Av1 => {
                    anyhow::bail!("AV1 needs the `hw` feature (FFmpeg) in this build")
                }
            }
        }
    }

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

    /// Software H.264 encoder (openh264) tuned for screen content.
    pub struct SoftwareEncoder {
        encoder: Encoder,
        bitrate_bps: u32,
        fps: f32,
        force_keyframe: bool,
    }

    impl SoftwareEncoder {
        pub const NAME: &'static str = "software";

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
    }

    impl VideoEncoder for SoftwareEncoder {
        fn name(&self) -> &str {
            Self::NAME
        }

        fn codec(&self) -> Codec {
            Codec::H264
        }

        fn input_layout(&self) -> YuvLayout {
            YuvLayout::I420
        }

        fn bitrate(&self) -> u32 {
            self.bitrate_bps
        }

        /// Rebuilds the encoder with a new target bitrate; the next frame becomes a keyframe.
        fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
            if bitrate_bps == self.bitrate_bps {
                return Ok(());
            }
            self.encoder = Self::build(bitrate_bps, self.fps)?;
            self.bitrate_bps = bitrate_bps;
            self.force_keyframe = true;
            Ok(())
        }

        fn request_keyframe(&mut self) {
            self.force_keyframe = true;
        }

        fn encode(&mut self, frame: &YuvFrame) -> Result<Option<EncodedFrame>> {
            let YuvFrame::I420(frame) = frame else {
                bail!("the software encoder takes I420 frames");
            };
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

    /// Software H.264 decoder (openh264), used only in builds without FFmpeg.
    /// It decodes Constrained Baseline streams, i.e. other software encoders.
    #[cfg(not(feature = "hw"))]
    pub struct SoftwareDecoder {
        decoder: openh264::decoder::Decoder,
    }

    #[cfg(not(feature = "hw"))]
    impl SoftwareDecoder {
        pub fn new() -> Result<Self> {
            use openh264::decoder::{Decoder, DecoderConfig};
            let decoder =
                Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
                    .context("creating H.264 decoder")?;
            Ok(Self { decoder })
        }
    }

    #[cfg(not(feature = "hw"))]
    impl VideoDecoder for SoftwareDecoder {
        fn name(&self) -> &str {
            "software"
        }

        fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<Arc<YuvFrame>>> {
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
            Ok(Some(Arc::new(YuvFrame::I420(I420Frame {
                width: w as u32,
                height: h as u32,
                y: copy_plane(yuv.y(), ys, w, h),
                u: copy_plane(yuv.u(), us, cw, ch),
                v: copy_plane(yuv.v(), vs, cw, ch),
                pts_us,
            }))))
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

    /// Straightforward single-threaded conversion to check the banded one against.
    fn reference_i420(src: &RawFrame, ow: usize, oh: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (sw, sh) = (src.width as usize, src.height as usize);
        let (ri, bi) = match src.format {
            PixelFormat::Bgra => (2, 0),
            PixelFormat::Rgba => (0, 2),
        };
        let (cw, ch) = (ow.div_ceil(2), oh.div_ceil(2));
        let (mut y, mut u, mut v) = (vec![0; ow * oh], vec![0; cw * ch], vec![0; cw * ch]);
        let px = |row: usize, col: usize| {
            let o = (row * sh / oh).min(sh - 1) * src.stride + (col * sw / ow).min(sw - 1) * 4;
            let d = &src.data[o..o + 4];
            (d[ri] as i32, d[1] as i32, d[bi] as i32)
        };
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut rs, mut gs, mut bs, mut n) = (0, 0, 0, 0);
                for row in (cy * 2..cy * 2 + 2).filter(|&r| r < oh) {
                    for col in (cx * 2..cx * 2 + 2).filter(|&c| c < ow) {
                        let (r, g, b) = px(row, col);
                        let yv = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                        y[row * ow + col] = yv.clamp(0, 255) as u8;
                        (rs, gs, bs, n) = (rs + r, gs + g, bs + b, n + 1);
                    }
                }
                let (r, g, b) = (rs / n, gs / n, bs / n);
                u[cy * cw + cx] =
                    (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                v[cy * cw + cx] =
                    (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        }
        (y, u, v)
    }

    #[test]
    fn banded_conversion_matches_reference() {
        for (sw, sh, ow, oh, format) in [
            (1001, 733, 1001, 733, PixelFormat::Bgra),
            (1920, 1080, 1279, 719, PixelFormat::Rgba),
            (37, 21, 37, 21, PixelFormat::Bgra),
        ] {
            let mut raw = test_frame(sw, sh);
            raw.format = format;
            for (i, b) in raw.data.iter_mut().enumerate() {
                *b = b.wrapping_add((i * 31 % 251) as u8);
            }
            let ours = convert_to_i420(&raw, ow, oh);
            let (y, u, v) = reference_i420(&raw, ow as usize, oh as usize);
            assert!(
                ours.y == y && ours.u == u && ours.v == v,
                "{sw}x{sh} -> {ow}x{oh}"
            );
        }
    }

    #[test]
    fn nv12_matches_i420() {
        let raw = test_frame(64, 48);
        let i420 = convert_to_i420(&raw, 64, 48);
        let nv12 = convert_to_nv12(&raw, 64, 48);
        assert_eq!(i420.y, nv12.y);
        for (i, (u, v)) in i420.u.iter().zip(&i420.v).enumerate() {
            assert_eq!(nv12.uv[i * 2], *u);
            assert_eq!(nv12.uv[i * 2 + 1], *v);
        }
    }

    fn roundtrip(hardware: bool) -> (String, String) {
        let (w, h) = (320, 240);
        let mut enc = open_encoder(EncoderPrefs {
            hardware,
            codec: Codec::H264,
            width: w,
            height: h,
            fps: 30,
            bitrate: 1_000_000,
        })
        .unwrap();
        let mut dec = open_decoder(hardware, Codec::H264).unwrap();
        let mut decoded = None;
        for i in 0..10u64 {
            let mut raw = test_frame(w, h);
            raw.pts_us = i * 33_333;
            let frame = convert_frame(&raw, w, h, enc.input_layout());
            if let Some(encoded) = enc.encode(&frame).unwrap() {
                if i == 0 {
                    assert!(encoded.keyframe, "first frame must be a keyframe");
                }
                if let Some(f) = dec.decode(&encoded.data, encoded.pts_us).unwrap() {
                    decoded = Some(f);
                }
            }
        }
        let decoded = decoded.expect("decoded at least one frame");
        assert_eq!((decoded.width(), decoded.height()), (w, h));
        (enc.name().to_string(), dec.name().to_string())
    }

    #[test]
    fn software_roundtrip() {
        let (enc, _) = roundtrip(false);
        assert_eq!(enc, "software");
    }

    #[test]
    fn best_available_roundtrip() {
        let (enc, dec) = roundtrip(true);
        eprintln!("encoder: {enc}, decoder: {dec}");
    }
}
