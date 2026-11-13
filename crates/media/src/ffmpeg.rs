//! GPU H.264 encode/decode through libavcodec (NVENC, AMF, Quick Sync, Media
//! Foundation and VAAPI encoders; D3D11VA, NVDEC and VAAPI decoders), with the
//! libavcodec software decoder as the last resort.

use crate::video::{
    Codec, EncodedFrame, EncoderPrefs, I420Frame, Nv12Frame, VideoDecoder, VideoEncoder, YuvFrame,
    YuvLayout,
};
use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use std::ffi::{CStr, CString, c_int};
use std::ptr;
use std::sync::{Arc, Once, OnceLock};

const TIME_BASE: ff::AVRational = ff::AVRational {
    num: 1,
    den: 1_000_000,
};

fn init_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Failed probes and dropped packets are expected and reported through
        // tracing; FFmpeg's own stderr output is opt-in.
        let level = if std::env::var_os("SCREENSHARE_FFMPEG_LOG").is_some() {
            ff::AV_LOG_INFO
        } else {
            ff::AV_LOG_QUIET
        };
        unsafe { ff::av_log_set_level(level) };
    });
}

/// Backends listed in `SCREENSHARE_SKIP_BACKENDS` (comma-separated labels such
/// as `NVENC,NVDEC`) are never tried; useful to diagnose a misbehaving driver.
fn skipped(label: &str) -> bool {
    std::env::var("SCREENSHARE_SKIP_BACKENDS").is_ok_and(|list| {
        list.split(',')
            .any(|s| s.trim().eq_ignore_ascii_case(label))
    })
}

fn av_error(code: c_int) -> anyhow::Error {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    anyhow!("{msg} ({code})")
}

fn check(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 {
        Err(av_error(code).context(what.to_string()))
    } else {
        Ok(code)
    }
}

const EAGAIN: c_int = ff::AVERROR(ff::EAGAIN);

/// Sets a private codec option, ignoring options this build doesn't know.
unsafe fn set_opt(ctx: *mut ff::AVCodecContext, key: &str, value: &str) {
    let (k, v) = (CString::new(key).unwrap(), CString::new(value).unwrap());
    let ret = unsafe { ff::av_opt_set((*ctx).priv_data, k.as_ptr(), v.as_ptr(), 0) };
    if ret < 0 {
        tracing::debug!("encoder option {key}={value} rejected: {}", av_error(ret));
    }
}

unsafe fn create_device(kind: ff::AVHWDeviceType) -> Result<*mut ff::AVBufferRef> {
    let mut device = ptr::null_mut();
    check(
        unsafe { ff::av_hwdevice_ctx_create(&mut device, kind, ptr::null(), ptr::null_mut(), 0) },
        "creating hardware device",
    )?;
    Ok(device)
}

// --- encoder ---------------------------------------------------------------

struct EncoderBackend {
    codec: &'static CStr,
    /// The codec this backend encodes.
    codec_id: Codec,
    label: &'static str,
    /// Device for encoders that only take GPU frames; others accept NV12 in
    /// system memory and upload internally.
    device: Option<ff::AVHWDeviceType>,
    options: &'static [(&'static str, &'static str)],
}

fn nvenc(codec: &'static CStr, codec_id: Codec, label: &'static str) -> EncoderBackend {
    EncoderBackend {
        codec,
        codec_id,
        label,
        device: None,
        options: &[
            ("preset", "p3"),
            ("tune", "ull"),
            ("rc", "cbr"),
            ("zerolatency", "1"),
            ("delay", "0"),
            ("forced-idr", "1"),
        ],
    }
}

fn vaapi(codec: &'static CStr, codec_id: Codec, label: &'static str) -> EncoderBackend {
    EncoderBackend {
        codec,
        codec_id,
        label,
        device: Some(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI),
        options: &[("rc_mode", "CBR")],
    }
}

#[cfg(windows)]
fn encoders() -> Vec<EncoderBackend> {
    vec![
        nvenc(c"h264_nvenc", Codec::H264, "NVENC"),
        EncoderBackend {
            codec: c"h264_amf",
            codec_id: Codec::H264,
            label: "AMF",
            device: None,
            options: &[
                ("usage", "ultralowlatency"),
                ("quality", "speed"),
                ("rc", "cbr"),
                ("forced_idr", "1"),
                ("header_spacing", "1"),
            ],
        },
        EncoderBackend {
            codec: c"h264_qsv",
            codec_id: Codec::H264,
            label: "Quick Sync",
            device: None,
            options: &[
                ("preset", "veryfast"),
                ("look_ahead", "0"),
                ("async_depth", "1"),
                ("low_delay_brc", "1"),
                ("forced_idr", "1"),
            ],
        },
        EncoderBackend {
            codec: c"h264_mf",
            codec_id: Codec::H264,
            label: "Media Foundation",
            device: None,
            options: &[
                ("hw_encoding", "1"),
                ("scenario", "display_remoting"),
                ("rate_control", "cbr"),
            ],
        },
    ]
}

#[cfg(target_os = "linux")]
fn encoders() -> Vec<EncoderBackend> {
    vec![
        nvenc(c"h264_nvenc", Codec::H264, "NVENC"),
        vaapi(c"h264_vaapi", Codec::H264, "VAAPI"),
    ]
}

#[cfg(not(any(windows, target_os = "linux")))]
fn encoders() -> Vec<EncoderBackend> {
    Vec::new()
}

/// The first backend for `codec` that works on this machine, probed once per codec.
fn probed_encoder(codec: Codec) -> Option<&'static EncoderBackend> {
    static PROBED: OnceLock<[Option<usize>; 2]> = OnceLock::new();
    let found = PROBED.get_or_init(|| {
        init_logging();
        [Codec::Av1, Codec::H264].map(|c| {
            let mut found = None;
            for (index, backend) in encoders()
                .into_iter()
                .enumerate()
                .filter(|(_, b)| b.codec_id == c)
            {
                if skipped(backend.label) {
                    continue;
                }
                let label = backend.label;
                // Leak the probe candidate so errors don't borrow it.
                match probe(Box::leak(Box::new(backend))) {
                    Ok(()) => {
                        tracing::info!("using {label} for {} encoding", c.name());
                        found = Some(index);
                        break;
                    }
                    Err(e) => tracing::debug!("{label} ({}): unavailable: {e:#}", c.name()),
                }
            }
            if found.is_none() {
                tracing::debug!("no GPU {} encoder available", c.name());
            }
            found
        })
    });
    let index = found[match codec {
        Codec::Av1 => 0,
        Codec::H264 => 1,
    }]?;
    let backend = encoders()
        .into_iter()
        .filter(|b| b.codec_id == codec)
        .nth(index)?;
    // The encoder structs keep a `&'static` reference; leak the small backend.
    Some(Box::leak(Box::new(backend)))
}

/// Opens the encoder at a small size and checks it actually emits a keyframe,
/// since some backends open fine and only fail once the GPU is touched.
fn probe(backend: &'static EncoderBackend) -> Result<()> {
    let (w, h) = (640, 360);
    let mut enc = FfEncoder::open(
        backend,
        EncoderPrefs {
            hardware: true,
            codec: backend.codec_id,
            width: w,
            height: h,
            fps: 30,
            bitrate: 1_000_000,
        },
    )?;
    let frame = YuvFrame::Nv12(Nv12Frame {
        width: w,
        height: h,
        y: vec![16; (w * h) as usize],
        uv: vec![128; (w * h / 2) as usize],
        pts_us: 0,
    });
    for i in 0..4u64 {
        let mut frame = frame.clone();
        frame.set_pts_us(i * 33_333);
        if let Some(out) = enc.encode(&frame)? {
            if out.keyframe {
                return Ok(());
            }
            bail!("first output was not a keyframe");
        }
    }
    bail!("encoder produced no output")
}

pub fn hw_encoder_name(codec: Codec) -> Option<&'static str> {
    probed_encoder(codec).map(|b| b.label)
}

pub fn open_hw_encoder(prefs: EncoderPrefs) -> Option<Box<dyn VideoEncoder>> {
    let backend = probed_encoder(prefs.codec)?;
    match FfEncoder::open(backend, prefs) {
        Ok(enc) => Some(Box::new(enc)),
        Err(e) => {
            tracing::warn!(
                "{} failed at {}x{}, falling back to software: {e:#}",
                backend.label,
                prefs.width,
                prefs.height
            );
            None
        }
    }
}

struct FfEncoder {
    backend: &'static EncoderBackend,
    prefs: EncoderPrefs,
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    hw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    device: *mut ff::AVBufferRef,
    frames: *mut ff::AVBufferRef,
    force_keyframe: bool,
    last_pts: i64,
    params: ParamSets,
}

// The FFmpeg objects are owned exclusively by this struct and only used from
// whichever thread currently holds it.
unsafe impl Send for FfEncoder {}

impl FfEncoder {
    fn open(backend: &'static EncoderBackend, prefs: EncoderPrefs) -> Result<Self> {
        init_logging();
        let mut enc = Self {
            backend,
            prefs,
            ctx: ptr::null_mut(),
            frame: ptr::null_mut(),
            hw_frame: ptr::null_mut(),
            packet: ptr::null_mut(),
            device: ptr::null_mut(),
            frames: ptr::null_mut(),
            force_keyframe: true,
            last_pts: -1,
            params: ParamSets::default(),
        };
        // On error, Drop releases whatever was allocated so far.
        unsafe { enc.init()? };
        Ok(enc)
    }

    unsafe fn init(&mut self) -> Result<()> {
        let EncoderPrefs {
            width,
            height,
            fps,
            bitrate,
            ..
        } = self.prefs;
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(self.backend.codec.as_ptr());
            if codec.is_null() {
                bail!("{} is not in this FFmpeg build", self.backend.label);
            }
            self.ctx = ff::avcodec_alloc_context3(codec);
            if self.ctx.is_null() {
                bail!("allocating encoder context");
            }
            let ctx = &mut *self.ctx;
            ctx.width = width as c_int;
            ctx.height = height as c_int;
            ctx.time_base = TIME_BASE;
            ctx.framerate = ff::AVRational {
                num: fps.max(1) as c_int,
                den: 1,
            };
            ctx.bit_rate = bitrate as i64;
            ctx.rc_max_rate = bitrate as i64;
            // Half a second of buffer: tight enough for low latency, loose
            // enough that keyframes don't get crushed.
            ctx.rc_buffer_size = (bitrate / 2) as c_int;
            ctx.max_b_frames = 0;
            // Keyframes are mostly on demand; a periodic one bounds how long a
            // corrupted picture can persist.
            ctx.gop_size = (fps.max(1) * 10) as c_int;
            ctx.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            ctx.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            ctx.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            ctx.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            ctx.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            ctx.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;

            if let Some(kind) = self.backend.device {
                self.device = create_device(kind)?;
                self.frames = ff::av_hwframe_ctx_alloc(self.device);
                if self.frames.is_null() {
                    bail!("allocating hardware frame pool");
                }
                let frames = &mut *((*self.frames).data as *mut ff::AVHWFramesContext);
                frames.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
                frames.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                frames.width = width as c_int;
                frames.height = height as c_int;
                frames.initial_pool_size = 8;
                check(
                    ff::av_hwframe_ctx_init(self.frames),
                    "initializing hardware frame pool",
                )?;
                ctx.pix_fmt = frames.format;
                ctx.hw_frames_ctx = ff::av_buffer_ref(self.frames);
                self.hw_frame = ff::av_frame_alloc();
            }

            if self.backend.codec_id == Codec::H264 {
                set_opt(self.ctx, "profile", "main");
            }
            for (key, value) in self.backend.options {
                set_opt(self.ctx, key, value);
            }

            check(
                ff::avcodec_open2(self.ctx, codec, ptr::null_mut()),
                "opening encoder",
            )?;
            if !ctx.extradata.is_null() && ctx.extradata_size > 0 {
                let extra = std::slice::from_raw_parts(ctx.extradata, ctx.extradata_size as usize);
                self.params.observe(extra);
            }

            self.frame = ff::av_frame_alloc();
            self.packet = ff::av_packet_alloc();
            if self.frame.is_null()
                || self.packet.is_null()
                || (self.device.is_null() != self.hw_frame.is_null())
            {
                bail!("allocating frames");
            }
            let frame = &mut *self.frame;
            frame.format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            frame.width = width as c_int;
            frame.height = height as c_int;
            check(ff::av_frame_get_buffer(self.frame, 0), "allocating frame")?;
        }
        Ok(())
    }

    unsafe fn release(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.hw_frame);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }

    unsafe fn upload(&mut self, src: &Nv12Frame) -> Result<*mut ff::AVFrame> {
        unsafe {
            check(
                ff::av_frame_make_writable(self.frame),
                "making frame writable",
            )?;
            let frame = &mut *self.frame;
            let (w, h) = (src.width as usize, src.height as usize);
            copy_plane_in(&src.y, w, h, frame.data[0], frame.linesize[0]);
            let cw = src.chroma_width() as usize * 2;
            let ch = src.chroma_height() as usize;
            copy_plane_in(&src.uv, cw, ch, frame.data[1], frame.linesize[1]);

            let mut pts = src.pts_us as i64;
            if pts <= self.last_pts {
                pts = self.last_pts + 1;
            }
            self.last_pts = pts;
            frame.pts = pts;

            let target = if self.hw_frame.is_null() {
                self.frame
            } else {
                ff::av_frame_unref(self.hw_frame);
                check(
                    ff::av_hwframe_get_buffer(self.frames, self.hw_frame, 0),
                    "allocating GPU frame",
                )?;
                check(
                    ff::av_hwframe_transfer_data(self.hw_frame, self.frame, 0),
                    "uploading frame",
                )?;
                (*self.hw_frame).pts = pts;
                self.hw_frame
            };
            let t = &mut *target;
            if std::mem::take(&mut self.force_keyframe) {
                t.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                t.flags |= ff::AV_FRAME_FLAG_KEY;
            } else {
                t.pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
                t.flags &= !ff::AV_FRAME_FLAG_KEY;
            }
            Ok(target)
        }
    }
}

impl Drop for FfEncoder {
    fn drop(&mut self) {
        unsafe { self.release() };
    }
}

fn copy_plane_in(src: &[u8], width: usize, rows: usize, dst: *mut u8, stride: c_int) {
    for row in 0..rows {
        let line = &src[row * width..(row + 1) * width];
        unsafe {
            ptr::copy_nonoverlapping(line.as_ptr(), dst.add(row * stride as usize), width);
        }
    }
}

impl VideoEncoder for FfEncoder {
    fn name(&self) -> &str {
        self.backend.label
    }

    fn codec(&self) -> Codec {
        self.backend.codec_id
    }

    fn input_layout(&self) -> YuvLayout {
        YuvLayout::Nv12
    }

    fn bitrate(&self) -> u32 {
        self.prefs.bitrate
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if bitrate_bps == self.prefs.bitrate {
            return Ok(());
        }
        self.prefs.bitrate = bitrate_bps;
        if self.backend.label == "NVENC" {
            // NVENC reconfigures its rate control in place on the next frame.
            unsafe {
                (*self.ctx).bit_rate = bitrate_bps as i64;
                (*self.ctx).rc_max_rate = bitrate_bps as i64;
                (*self.ctx).rc_buffer_size = (bitrate_bps / 2) as c_int;
            }
            return Ok(());
        }
        unsafe {
            self.release();
            self.last_pts = -1;
            self.init()?;
        }
        self.force_keyframe = true;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn encode(&mut self, frame: &YuvFrame) -> Result<Option<EncodedFrame>> {
        let YuvFrame::Nv12(src) = frame else {
            bail!("{} takes NV12 frames", self.backend.label);
        };
        if (src.width, src.height) != (self.prefs.width, self.prefs.height) {
            bail!("frame size changed without reopening the encoder");
        }
        let mut data = Vec::new();
        let mut keyframe = false;
        let mut pts = src.pts_us as i64;
        unsafe {
            let input = self.upload(src)?;
            check(ff::avcodec_send_frame(self.ctx, input), "sending frame")?;
            loop {
                let ret = ff::avcodec_receive_packet(self.ctx, self.packet);
                if ret == EAGAIN || ret == ff::AVERROR_EOF {
                    break;
                }
                check(ret, "receiving packet")?;
                let pkt = &*self.packet;
                let bytes = std::slice::from_raw_parts(pkt.data, pkt.size as usize);
                keyframe |= pkt.flags & ff::AV_PKT_FLAG_KEY != 0;
                pts = pkt.pts;
                data.extend_from_slice(bytes);
                ff::av_packet_unref(self.packet);
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        // H.264: prepend cached SPS/PPS to a bare IDR so a mid-stream viewer can
        // decode it. AV1's decoder doesn't need this.
        let data = match self.backend.codec_id {
            Codec::H264 => {
                keyframe |= contains_idr(&data);
                self.params.complete(data)
            }
            Codec::Av1 => data,
        };
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            pts_us: pts.max(0) as u64,
            width: self.prefs.width,
            height: self.prefs.height,
        }))
    }
}

/// Iterates the NAL units of an Annex B byte stream as `(type, unit with start code)`.
fn nal_units(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|s| s.0)
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|((_, payload), end)| payload < end)
        .map(move |((begin, payload), end)| (data[payload] & 0x1f, &data[begin..end]))
}

fn contains_idr(data: &[u8]) -> bool {
    nal_units(data).any(|(kind, _)| kind == 5)
}

/// Remembers the latest SPS/PPS so every IDR picture can be decoded on its
/// own, even from encoders that only emit them once at startup. Viewers join
/// mid-stream and start from the next keyframe.
#[derive(Default)]
struct ParamSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl ParamSets {
    fn observe(&mut self, data: &[u8]) {
        for (kind, unit) in nal_units(data) {
            match kind {
                7 => self.sps = unit.to_vec(),
                8 => self.pps = unit.to_vec(),
                _ => {}
            }
        }
    }

    fn complete(&mut self, data: Vec<u8>) -> Vec<u8> {
        let (mut has_sps, mut has_pps, mut has_idr) = (false, false, false);
        for (kind, _) in nal_units(&data) {
            has_sps |= kind == 7;
            has_pps |= kind == 8;
            has_idr |= kind == 5;
        }
        if has_sps || has_pps {
            self.observe(&data);
        }
        if !has_idr || (has_sps && has_pps) || self.sps.is_empty() || self.pps.is_empty() {
            return data;
        }
        let mut out = Vec::with_capacity(self.sps.len() + self.pps.len() + data.len());
        out.extend_from_slice(&self.sps);
        out.extend_from_slice(&self.pps);
        out.extend_from_slice(&data);
        out
    }
}

// --- decoder ---------------------------------------------------------------

#[cfg(windows)]
const DECODERS: &[(ff::AVHWDeviceType, &str)] =
    &[(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, "D3D11VA")];

#[cfg(target_os = "linux")]
const DECODERS: &[(ff::AVHWDeviceType, &str)] = &[
    (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, "NVDEC"),
    (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, "VAAPI"),
];

#[cfg(not(any(windows, target_os = "linux")))]
const DECODERS: &[(ff::AVHWDeviceType, &str)] = &[];

const SOFTWARE: &str = "software";

/// Consecutive hardware decode errors after which the decoder gives up on the GPU.
const MAX_HW_ERRORS: u32 = 8;

pub fn open_decoder(hardware: bool, codec: Codec) -> Result<Box<dyn VideoDecoder>> {
    init_logging();
    if hardware {
        for &(kind, label) in DECODERS.iter().filter(|(_, label)| !skipped(label)) {
            match FfDecoder::open(Some((kind, label)), codec) {
                Ok(dec) => {
                    tracing::info!("using {label} for {} decoding", codec.name());
                    return Ok(Box::new(dec));
                }
                Err(e) => tracing::debug!("{label} {} decoding unavailable: {e:#}", codec.name()),
            }
        }
    }
    Ok(Box::new(FfDecoder::open(None, codec)?))
}

struct FfDecoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    device: *mut ff::AVBufferRef,
    codec: Codec,
    /// Requested GPU backend, if any, and its surface format.
    hw: Option<(&'static str, ff::AVPixelFormat)>,
    /// Backend that produced the most recent frame.
    current: &'static str,
    decoded_any: bool,
    hw_errors: u32,
}

unsafe impl Send for FfDecoder {}

unsafe extern "C" fn pick_format(
    ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        let wanted = (*ctx).opaque as usize;
        let mut p = formats;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as usize == wanted {
                return *p;
            }
            p = p.add(1);
        }
        // The GPU can't decode this stream (e.g. an unsupported profile or
        // size); let libavcodec decode it in software.
        ff::avcodec_default_get_format(ctx, formats)
    }
}

impl FfDecoder {
    fn open(hw: Option<(ff::AVHWDeviceType, &'static str)>, codec: Codec) -> Result<Self> {
        let mut dec = Self {
            ctx: ptr::null_mut(),
            frame: ptr::null_mut(),
            sw_frame: ptr::null_mut(),
            packet: ptr::null_mut(),
            device: ptr::null_mut(),
            codec,
            hw: None,
            current: SOFTWARE,
            decoded_any: false,
            hw_errors: 0,
        };
        unsafe { dec.init(hw)? };
        Ok(dec)
    }

    unsafe fn init(&mut self, hw: Option<(ff::AVHWDeviceType, &'static str)>) -> Result<()> {
        unsafe {
            let id = match self.codec {
                Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
                Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
            };
            let codec = ff::avcodec_find_decoder(id);
            if codec.is_null() {
                bail!("this FFmpeg build has no {} decoder", self.codec.name());
            }
            self.ctx = ff::avcodec_alloc_context3(codec);
            if self.ctx.is_null() {
                bail!("allocating decoder context");
            }
            let ctx = &mut *self.ctx;
            ctx.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            ctx.flags2 |= ff::AV_CODEC_FLAG2_FAST;
            ctx.pkt_timebase = TIME_BASE;
            // Frame threading would add a frame of latency per thread.
            ctx.thread_type = ff::FF_THREAD_SLICE;
            ctx.thread_count = 0;

            if let Some((kind, label)) = hw {
                let mut surface = None;
                for i in 0.. {
                    let config = ff::avcodec_get_hw_config(codec, i);
                    if config.is_null() {
                        break;
                    }
                    let config = &*config;
                    if config.device_type == kind
                        && config.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int
                            != 0
                    {
                        surface = Some(config.pix_fmt);
                        break;
                    }
                }
                let Some(surface) = surface else {
                    bail!(
                        "the {} decoder has no {label} support in this build",
                        self.codec.name()
                    );
                };
                self.device = create_device(kind)?;
                ctx.hw_device_ctx = ff::av_buffer_ref(self.device);
                ctx.opaque = surface as usize as *mut _;
                ctx.get_format = Some(pick_format);
                ctx.thread_count = 1;
                self.hw = Some((label, surface));
            }

            check(
                ff::avcodec_open2(self.ctx, codec, ptr::null_mut()),
                "opening decoder",
            )?;
            self.frame = ff::av_frame_alloc();
            self.sw_frame = ff::av_frame_alloc();
            self.packet = ff::av_packet_alloc();
            if self.frame.is_null() || self.sw_frame.is_null() || self.packet.is_null() {
                bail!("allocating frames");
            }
        }
        Ok(())
    }

    unsafe fn release(&mut self) {
        unsafe {
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_packet_free(&mut self.packet);
            ff::av_buffer_unref(&mut self.device);
        }
    }

    fn fall_back_to_software(&mut self) -> Result<()> {
        if let Some((label, _)) = self.hw {
            tracing::warn!("{label} decoding failed, switching to software decoding");
        }
        unsafe {
            self.release();
            self.hw = None;
            self.init(None)?;
        }
        self.current = SOFTWARE;
        self.decoded_any = false;
        self.hw_errors = 0;
        Ok(())
    }

    unsafe fn decode_packet(&mut self, data: &[u8], pts_us: u64) -> Result<Option<YuvFrame>> {
        unsafe {
            check(
                ff::av_new_packet(self.packet, data.len() as c_int),
                "allocating packet",
            )?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.packet).data, data.len());
            (*self.packet).pts = pts_us as i64;
            let sent = ff::avcodec_send_packet(self.ctx, self.packet);
            ff::av_packet_unref(self.packet);
            check(sent, "decoding packet")?;

            let mut latest = None;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if ret == EAGAIN || ret == ff::AVERROR_EOF {
                    break;
                }
                check(ret, "decoding frame")?;
                let mut src = self.frame;
                self.current = SOFTWARE;
                if let Some((label, surface)) = self.hw
                    && (*self.frame).format == surface as c_int
                {
                    ff::av_frame_unref(self.sw_frame);
                    check(
                        ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0),
                        "downloading frame",
                    )?;
                    src = self.sw_frame;
                    self.current = label;
                }
                let pts = match (*self.frame).pts {
                    p if p >= 0 => p as u64,
                    _ => pts_us,
                };
                latest = Some(frame_to_yuv(&*src, pts)?);
                ff::av_frame_unref(self.frame);
            }
            Ok(latest)
        }
    }
}

impl Drop for FfDecoder {
    fn drop(&mut self) {
        unsafe { self.release() };
    }
}

fn copy_plane_out(src: *const u8, stride: c_int, width: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * rows);
    for row in 0..rows {
        let line = unsafe { std::slice::from_raw_parts(src.add(row * stride as usize), width) };
        out.extend_from_slice(line);
    }
    out
}

unsafe fn frame_to_yuv(frame: &ff::AVFrame, pts_us: u64) -> Result<YuvFrame> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    const NV12: c_int = ff::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
    const YUV420P: c_int = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
    const YUVJ420P: c_int = ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as c_int;
    Ok(match frame.format {
        NV12 => YuvFrame::Nv12(Nv12Frame {
            width: w as u32,
            height: h as u32,
            y: copy_plane_out(frame.data[0], frame.linesize[0], w, h),
            uv: copy_plane_out(frame.data[1], frame.linesize[1], cw * 2, ch),
            pts_us,
        }),
        YUV420P | YUVJ420P => YuvFrame::I420(I420Frame {
            width: w as u32,
            height: h as u32,
            y: copy_plane_out(frame.data[0], frame.linesize[0], w, h),
            u: copy_plane_out(frame.data[1], frame.linesize[1], cw, ch),
            v: copy_plane_out(frame.data[2], frame.linesize[2], cw, ch),
            pts_us,
        }),
        other => bail!("unsupported decoded pixel format {other}"),
    })
}

impl VideoDecoder for FfDecoder {
    fn name(&self) -> &str {
        self.current
    }

    fn decode(&mut self, data: &[u8], pts_us: u64) -> Result<Option<Arc<YuvFrame>>> {
        match unsafe { self.decode_packet(data, pts_us) } {
            Ok(frame) => {
                if frame.is_some() {
                    self.decoded_any = true;
                    self.hw_errors = 0;
                }
                Ok(frame.map(Arc::new))
            }
            Err(e) if self.hw.is_some() => {
                self.hw_errors += 1;
                // A GPU that never produced a picture, or keeps failing, is
                // replaced by software decoding; the caller requests a keyframe.
                if !self.decoded_any || self.hw_errors >= MAX_HW_ERRORS {
                    self.fall_back_to_software()?;
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_sets_are_prepended_to_bare_idr() {
        let mut sets = ParamSets::default();
        let first = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9,
        ];
        assert_eq!(sets.complete(first.to_vec()), first);
        let bare_idr = vec![0, 0, 0, 1, 0x65, 7, 7];
        let fixed = sets.complete(bare_idr.clone());
        let kinds: Vec<u8> = nal_units(&fixed).map(|(k, _)| k).collect();
        assert_eq!(kinds, [7, 8, 5]);
        let p_frame = vec![0, 0, 0, 1, 0x41, 5];
        assert_eq!(sets.complete(p_frame.clone()), p_frame);
    }

    /// Exercises the FFmpeg encoder path (NV12 upload, forced keyframes,
    /// in-place reopen on bitrate change) with x264 standing in for a GPU.
    #[test]
    fn ffmpeg_encoder_roundtrip() {
        static X264: EncoderBackend = EncoderBackend {
            codec: c"libx264",
            codec_id: Codec::H264,
            label: "x264",
            device: None,
            options: &[("preset", "ultrafast"), ("tune", "zerolatency")],
        };
        let (w, h) = (320, 240);
        let prefs = EncoderPrefs {
            hardware: true,
            codec: Codec::H264,
            width: w,
            height: h,
            fps: 30,
            bitrate: 800_000,
        };
        let Ok(mut enc) = FfEncoder::open(&X264, prefs) else {
            eprintln!("libx264 not in this FFmpeg build, skipping");
            return;
        };
        let mut dec = FfDecoder::open(None, Codec::H264).unwrap();
        let mut decoded = 0;
        for i in 0..20u64 {
            if i == 10 {
                enc.set_bitrate(400_000).unwrap();
            }
            let frame = YuvFrame::Nv12(Nv12Frame {
                width: w,
                height: h,
                y: (0..w * h).map(|p| (p % 200 + i as u32) as u8).collect(),
                uv: vec![128; (w * h / 2) as usize],
                pts_us: i * 33_333,
            });
            if let Some(out) = enc.encode(&frame).unwrap() {
                if i == 0 || i == 10 {
                    assert!(out.keyframe, "frame {i} should be a keyframe");
                    assert!(nal_units(&out.data).any(|(k, _)| k == 7));
                }
                if let Some(f) = dec.decode(&out.data, out.pts_us).unwrap() {
                    assert_eq!((f.width(), f.height()), (w, h));
                    decoded += 1;
                }
            }
        }
        assert!(decoded >= 15, "decoded {decoded} frames");
    }
}
