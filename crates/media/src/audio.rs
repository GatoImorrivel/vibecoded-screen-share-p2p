use crate::clock::SyncClock;
use crate::{CHANNELS, FRAME_LEN, FRAME_SAMPLES, SAMPLE_RATE};
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opus encoder for 48 kHz stereo, 20 ms frames.
pub struct OpusEncoder {
    inner: opus::Encoder,
    out: Vec<u8>,
}

impl OpusEncoder {
    pub fn new(bitrate_bps: i32) -> Result<Self> {
        let mut inner = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .context("creating opus encoder")?;
        inner.set_bitrate(opus::Bitrate::Bits(bitrate_bps))?;
        Ok(Self {
            inner,
            out: vec![0u8; 4000],
        })
    }

    /// Encodes exactly [`FRAME_LEN`] interleaved samples.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>> {
        debug_assert_eq!(pcm.len(), FRAME_LEN);
        let n = self.inner.encode_float(pcm, &mut self.out)?;
        Ok(self.out[..n].to_vec())
    }
}

/// Opus decoder with packet-loss concealment for sequence gaps.
pub struct OpusDecoder {
    inner: opus::Decoder,
    next_seq: Option<u32>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                .context("creating opus decoder")?,
            next_seq: None,
        })
    }

    /// Decodes one packet, concealing up to three missing packets before it.
    /// Returns an empty vec for late or duplicate packets.
    pub fn decode(&mut self, seq: u32, data: &[u8]) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        if let Some(expected) = self.next_seq {
            let gap = seq.wrapping_sub(expected);
            if gap > u32::MAX / 2 {
                return Ok(out);
            }
            for _ in 0..gap.min(3) {
                let mut buf = vec![0f32; FRAME_LEN];
                let n = self.inner.decode_float(&[], &mut buf, false)?;
                buf.truncate(n * CHANNELS);
                out.extend_from_slice(&buf);
            }
        }
        let mut buf = vec![0f32; FRAME_LEN * 3];
        let n = self.inner.decode_float(data, &mut buf, false)?;
        buf.truncate(n * CHANNELS);
        out.extend_from_slice(&buf);
        self.next_seq = Some(seq.wrapping_add(1));
        Ok(out)
    }
}

const TARGET_BUFFER: usize = 3 * FRAME_SAMPLES; // 60 ms
const PREBUFFER: usize = 2 * FRAME_SAMPLES; // 40 ms
const MAX_BUFFER: usize = 10 * FRAME_SAMPLES; // 200 ms

struct JitterBuffer {
    samples: VecDeque<f32>,
    head_pts_us: i64,
    gain: f32,
    playing: bool,
    clock: Arc<SyncClock>,
}

impl JitterBuffer {
    fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }

    fn push(&mut self, pts_us: u64, pcm: &[f32]) {
        if self.samples.is_empty() {
            self.head_pts_us = pts_us as i64;
        }
        self.samples.extend(pcm.iter().copied());
        if self.frames() > MAX_BUFFER {
            let drop_frames = self.frames() - TARGET_BUFFER;
            self.samples.drain(..drop_frames * CHANNELS);
            self.head_pts_us += frames_to_us(drop_frames);
        }
    }

    fn pop_frame(&mut self) -> Option<(f32, f32)> {
        if !self.playing {
            if self.frames() < PREBUFFER {
                return None;
            }
            self.playing = true;
        }
        match (self.samples.pop_front(), self.samples.pop_front()) {
            (Some(l), Some(r)) => {
                self.head_pts_us += frames_to_us(1);
                Some((l * self.gain, r * self.gain))
            }
            _ => {
                self.playing = false;
                None
            }
        }
    }
}

fn frames_to_us(frames: usize) -> i64 {
    (frames as i64 * 1_000_000) / SAMPLE_RATE as i64
}

type Streams = Arc<Mutex<HashMap<u64, JitterBuffer>>>;

/// Plays and mixes any number of remote audio streams on the default output device.
pub struct AudioPlayer {
    streams: Streams,
    next_id: AtomicU64,
    _stream: Option<cpal::Stream>,
}

// cpal::Stream is !Send on some platforms; the player lives on the UI-owned
// engine and is only touched through the shared `streams` map.
unsafe impl Send for AudioPlayer {}
unsafe impl Sync for AudioPlayer {}

impl AudioPlayer {
    pub fn new() -> Self {
        let streams: Streams = Arc::default();
        let stream = match open_output(streams.clone()) {
            Ok(s) => Some(s),
            Err(err) => {
                tracing::warn!("audio output unavailable: {err:#}");
                None
            }
        };
        Self {
            streams,
            next_id: AtomicU64::new(1),
            _stream: stream,
        }
    }

    /// Registers a new remote audio stream. Removing the handle stops playback.
    pub fn add_stream(&self, clock: Arc<SyncClock>) -> PlaybackHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().insert(
            id,
            JitterBuffer {
                samples: VecDeque::with_capacity(MAX_BUFFER * CHANNELS * 2),
                head_pts_us: 0,
                gain: 1.0,
                playing: false,
                clock,
            },
        );
        PlaybackHandle {
            id,
            streams: self.streams.clone(),
        }
    }
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for feeding one remote stream into the mixer.
pub struct PlaybackHandle {
    id: u64,
    streams: Streams,
}

impl PlaybackHandle {
    pub fn push(&self, pts_us: u64, pcm: &[f32]) {
        if let Some(buf) = self.streams.lock().get_mut(&self.id) {
            buf.push(pts_us, pcm);
        }
    }

    pub fn set_gain(&self, gain: f32) {
        if let Some(buf) = self.streams.lock().get_mut(&self.id) {
            buf.gain = gain;
        }
    }
}

impl Drop for PlaybackHandle {
    fn drop(&mut self) {
        self.streams.lock().remove(&self.id);
    }
}

fn open_output(streams: Streams) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let default = device.default_output_config()?;
    let preferred = cpal::StreamConfig {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };

    let err_fn = |err| tracing::warn!("audio output error: {err}");
    let build = |config: cpal::StreamConfig| {
        let streams = streams.clone();
        let channels = config.channels as usize;
        let ratio = SAMPLE_RATE as f64 / config.sample_rate as f64;
        let mut resampler = Resampler::new(ratio);
        device.build_output_stream(
            config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut guard = streams.lock();
                for frame in out.chunks_mut(channels) {
                    let (l, r) = resampler.next(|| mix_one(&mut guard));
                    for (i, s) in frame.iter_mut().enumerate() {
                        *s = match i {
                            0 => l,
                            1 => r,
                            _ => 0.0,
                        };
                    }
                }
                for buf in guard.values() {
                    if buf.playing {
                        buf.clock.set_audio_pts(buf.head_pts_us);
                    }
                }
            },
            err_fn,
            None,
        )
    };

    let stream = match build(preferred) {
        Ok(s) => s,
        Err(_) if default.sample_format() == cpal::SampleFormat::F32 => {
            build(default.config()).context("building output stream")?
        }
        Err(e) => return Err(e).context("building 48 kHz f32 output stream"),
    };
    stream.play()?;
    Ok(stream)
}

fn mix_one(streams: &mut HashMap<u64, JitterBuffer>) -> (f32, f32) {
    let (mut l, mut r) = (0.0, 0.0);
    for buf in streams.values_mut() {
        if let Some((sl, sr)) = buf.pop_frame() {
            l += sl;
            r += sr;
        }
    }
    (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0))
}

/// Linear resampler from 48 kHz to the device rate.
struct Resampler {
    ratio: f64,
    pos: f64,
    prev: (f32, f32),
    cur: (f32, f32),
}

impl Resampler {
    fn new(ratio: f64) -> Self {
        Self {
            ratio,
            pos: 1.0,
            prev: (0.0, 0.0),
            cur: (0.0, 0.0),
        }
    }

    fn next(&mut self, mut pull: impl FnMut() -> (f32, f32)) -> (f32, f32) {
        if (self.ratio - 1.0).abs() < f64::EPSILON {
            return pull();
        }
        while self.pos >= 1.0 {
            self.prev = self.cur;
            self.cur = pull();
            self.pos -= 1.0;
        }
        let t = self.pos as f32;
        self.pos += self.ratio;
        (
            self.prev.0 + (self.cur.0 - self.prev.0) * t,
            self.prev.1 + (self.cur.1 - self.prev.1) * t,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_roundtrip_with_gap() {
        let mut enc = OpusEncoder::new(128_000).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let pcm: Vec<f32> = (0..FRAME_LEN)
            .map(|i| (i as f32 * 0.01).sin() * 0.3)
            .collect();
        let a = enc.encode(&pcm).unwrap();
        let b = enc.encode(&pcm).unwrap();
        assert_eq!(dec.decode(0, &a).unwrap().len(), FRAME_LEN);
        // Packet 1 lost: one concealed frame plus the decoded one.
        assert_eq!(dec.decode(2, &b).unwrap().len(), FRAME_LEN * 2);
        // Late packet is ignored.
        assert!(dec.decode(1, &a).unwrap().is_empty());
    }
}
