//! Our outgoing share: capture → I420 → H.264 for video, capture → Opus for audio.

use bytes::Bytes;
use crossbeam_channel::RecvTimeoutError;
use parking_lot::Mutex;
use screenshare_capture::{AudioTarget, VideoCapture};
use screenshare_media::audio::OpusEncoder;
use screenshare_media::clock::now_us;
use screenshare_media::video::{H264Encoder, RawFrame, convert_to_i420, fit_dimensions};
use screenshare_net::protocol::VideoHeader;
use screenshare_net::{EncoderControl, ShareHandle, VideoPacket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const AUDIO_BITRATE: i32 = 128_000;
const MIN_BITRATE: u32 = 300_000;
/// Re-send the last frame this often when the screen is static, so new
/// subscribers and keyframe requests are served promptly.
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub struct ShareConfig {
    pub max_fps: u32,
    pub max_size: (u32, u32),
    pub max_bitrate: u32,
}

impl ShareConfig {
    pub fn bitrates(&self) -> (u32, u32, u32) {
        let max = self.max_bitrate.max(MIN_BITRATE * 2);
        (max * 3 / 4, MIN_BITRATE, max)
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || f(flag))
            .expect("spawning thread");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct ActiveShare {
    handle: Arc<ShareHandle>,
    _video: Worker,
    audio: Mutex<Option<Worker>>,
}

impl ActiveShare {
    /// Starts encoding `capture`. `on_end` runs if the captured source goes away.
    pub fn start(
        capture: VideoCapture,
        handle: ShareHandle,
        config: ShareConfig,
        on_end: impl FnOnce() + Send + 'static,
    ) -> Self {
        let handle = Arc::new(handle);
        let video_handle = handle.clone();
        let video = Worker::spawn("share-video", move |stop| {
            if video_loop(&capture, &video_handle, config, &stop) {
                on_end();
            }
        });
        Self {
            handle,
            _video: video,
            audio: Mutex::new(None),
        }
    }

    /// Starts (or replaces) the audio that accompanies this share.
    pub fn set_audio(&self, target: Option<AudioTarget>) -> anyhow::Result<()> {
        let mut audio = self.audio.lock();
        *audio = None;
        let Some(target) = target else { return Ok(()) };
        let capture = screenshare_capture::start_audio(target)?;
        let mut encoder = OpusEncoder::new(AUDIO_BITRATE)?;
        let handle = self.handle.clone();
        *audio = Some(Worker::spawn("share-audio", move |stop| {
            while !stop.load(Ordering::Relaxed) {
                match capture.frames.recv_timeout(Duration::from_millis(100)) {
                    Ok((pts, pcm)) => {
                        if handle.subscriber_count() == 0 {
                            continue;
                        }
                        match encoder.encode(&pcm) {
                            Ok(data) => handle.send_audio(pts, data),
                            Err(err) => tracing::warn!("opus encode failed: {err:#}"),
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }));
        Ok(())
    }
}

/// Returns true when the capture source ended on its own.
fn video_loop(
    capture: &VideoCapture,
    handle: &ShareHandle,
    config: ShareConfig,
    stop: &AtomicBool,
) -> bool {
    let fps = config.max_fps.clamp(1, 120);
    let (mut bitrate, _, max_bitrate) = config.bitrates();
    let mut encoder: Option<(H264Encoder, (u32, u32))> = None;
    let mut last: Option<RawFrame> = None;
    let mut last_sent = Instant::now() - REFRESH;
    let mut want_keyframe = false;

    while !stop.load(Ordering::Relaxed) {
        while let Some(control) = handle.try_control() {
            match control {
                EncoderControl::Keyframe => want_keyframe = true,
                EncoderControl::Bitrate(b) => {
                    bitrate = b;
                    // Changing the bitrate rebuilds the encoder and costs a
                    // keyframe: apply decreases at once, increases only when large.
                    if let Some((enc, _)) = &mut encoder {
                        let current = enc.bitrate();
                        let worthwhile =
                            b < current || b >= current + current * 3 / 10 || b == max_bitrate;
                        if worthwhile && let Err(err) = enc.set_bitrate(b) {
                            tracing::warn!("changing bitrate failed: {err:#}");
                            encoder = None;
                        }
                    }
                }
            }
        }

        let mut fresh = false;
        match capture.frames.recv_timeout(Duration::from_millis(50)) {
            Ok(mut frame) => {
                while let Ok(newer) = capture.frames.try_recv() {
                    frame = newer;
                }
                last = Some(frame);
                fresh = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
        }

        if handle.subscriber_count() == 0 {
            continue;
        }
        let Some(raw) = &last else { continue };
        if !(fresh || want_keyframe || last_sent.elapsed() >= REFRESH) {
            continue;
        }

        let (w, h) = fit_dimensions(raw.width, raw.height, config.max_size.0, config.max_size.1);
        if w < 16 || h < 16 {
            continue;
        }
        if encoder.as_ref().is_none_or(|(_, size)| *size != (w, h)) {
            match H264Encoder::new(bitrate, fps as f32) {
                Ok(enc) => encoder = Some((enc, (w, h))),
                Err(err) => {
                    tracing::error!("creating H.264 encoder failed: {err:#}");
                    return true;
                }
            }
        }
        let (enc, _) = encoder.as_mut().expect("created above");
        if want_keyframe {
            enc.request_keyframe();
            want_keyframe = false;
        }
        let mut i420 = convert_to_i420(raw, w, h);
        if !fresh {
            i420.pts_us = now_us();
        }
        match enc.encode(&i420) {
            Ok(Some(frame)) => {
                handle.send_video(VideoPacket {
                    header: VideoHeader {
                        stream_id: handle.stream_id(),
                        pts_us: frame.pts_us,
                        keyframe: frame.keyframe,
                        width: frame.width,
                        height: frame.height,
                    },
                    data: Bytes::from(frame.data),
                });
                last_sent = Instant::now();
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!("H.264 encode failed: {err:#}");
                encoder = None;
            }
        }
    }
    false
}
