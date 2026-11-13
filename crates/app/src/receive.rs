//! One watched remote stream: decode with optional audio sync, Opus into the mixer.

use crate::video_widget::FrameSlot;
use crossbeam_channel::Sender;
use parking_lot::{Condvar, Mutex};
use screenshare_media::audio::{AudioPlayer, OpusDecoder, PlaybackHandle};
use screenshare_media::clock::SyncClock;
use screenshare_media::video::{Codec, open_decoder};
use screenshare_net::{AudioPacket, PeerId, Session, VideoPacket};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Cap on how long we hold a frame for A/V sync. Screen sharing prefers low
/// latency over perfect lip sync.
const MAX_SYNC_DELAY_US: i64 = 40_000;
/// Only a few frames may wait for the decoder. A deep FIFO was the main cause
/// of compounding display delay: old frames played out while new ones were dropped.
const VIDEO_QUEUE: usize = 4;
const KEYFRAME_RETRY: Duration = Duration::from_millis(1000);

/// Latest-biased inbox: overflow clears the backlog and demands a keyframe
/// instead of keeping stale pictures and discarding fresh ones.
struct VideoInbox {
    queue: Mutex<VecDeque<VideoPacket>>,
    ready: Condvar,
    closed: AtomicBool,
}

impl VideoInbox {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::with_capacity(VIDEO_QUEUE)),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        })
    }

    fn push(&self, packet: VideoPacket, need_keyframe: &AtomicBool) {
        let mut q = self.queue.lock();
        if packet.header.keyframe {
            // A keyframe restarts the GOP; anything before it is obsolete.
            q.clear();
            need_keyframe.store(false, Ordering::Relaxed);
        } else if need_keyframe.load(Ordering::Relaxed) {
            return;
        } else if q.len() >= VIDEO_QUEUE {
            // Decoder fell behind: shed the backlog and wait for a keyframe so
            // we do not keep displaying seconds-old video.
            q.clear();
            need_keyframe.store(true, Ordering::Relaxed);
            // Wake the decoder so it can request a keyframe immediately.
            self.ready.notify_one();
            return;
        }
        q.push_back(packet);
        self.ready.notify_one();
    }

    /// Blocks until a packet is available or `timeout` elapses.
    fn recv_timeout(&self, timeout: Duration) -> Option<VideoPacket> {
        let mut q = self.queue.lock();
        if q.is_empty() {
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            self.ready.wait_for(&mut q, timeout);
        }
        q.pop_front()
    }

    /// Drains waiting packets, jumping to the latest keyframe when present so a
    /// backlog does not get decoded frame-by-frame.
    fn coalesce(first: VideoPacket, inbox: &VideoInbox) -> (VideoPacket, Vec<VideoPacket>, bool) {
        let mut q = inbox.queue.lock();
        if q.is_empty() {
            return (first, Vec::new(), false);
        }
        let mut batch = Vec::with_capacity(q.len() + 1);
        batch.push(first);
        while let Some(p) = q.pop_front() {
            batch.push(p);
        }
        let more_waiting = false;
        if let Some(i) = batch.iter().rposition(|p| p.header.keyframe) {
            let first = batch[i].clone();
            let rest = batch[i + 1..].to_vec();
            (first, rest, more_waiting)
        } else {
            // No keyframe in the backlog: decode only the oldest so the
            // decoder state stays valid; leave the rest for the next loop.
            let first = batch.remove(0);
            for p in batch.into_iter().rev() {
                q.push_front(p);
            }
            (first, Vec::new(), !q.is_empty())
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }
}

pub struct RemoteStream {
    pub stream_id: u64,
    pub slot: Arc<FrameSlot>,
    video: Arc<VideoInbox>,
    audio_tx: Sender<AudioPacket>,
    need_keyframe: Arc<AtomicBool>,
    playback: Option<Arc<PlaybackHandle>>,
}

impl RemoteStream {
    pub fn start(
        session: Session,
        peer: PeerId,
        stream_id: u64,
        player: Option<&AudioPlayer>,
        hardware: bool,
        codec: Codec,
    ) -> Self {
        let clock = SyncClock::new();
        let slot = Arc::new(FrameSlot::default());
        let need_keyframe = Arc::new(AtomicBool::new(true));
        let video = VideoInbox::new();
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<AudioPacket>(100);
        let playback = player.map(|p| Arc::new(p.add_stream(clock.clone())));

        {
            let slot = slot.clone();
            let need_keyframe = need_keyframe.clone();
            let clock = clock.clone();
            let video = video.clone();
            std::thread::Builder::new()
                .name("recv-video".into())
                .spawn(move || {
                    let mut decoder = match open_decoder(hardware, codec) {
                        Ok(d) => d,
                        Err(err) => {
                            tracing::error!("creating {} decoder failed: {err:#}", codec.name());
                            return;
                        }
                    };
                    let mut last_request: Option<Instant> = None;
                    let mut request = |force: bool| {
                        if force || last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
                            last_request = Some(Instant::now());
                            session.request_keyframe(peer, stream_id);
                        }
                    };
                    loop {
                        let Some(packet) = video.recv_timeout(Duration::from_millis(250)) else {
                            if video.closed.load(Ordering::Relaxed) {
                                break;
                            }
                            if need_keyframe.load(Ordering::Relaxed) {
                                request(false);
                            }
                            continue;
                        };
                        let (packet, rest, more_waiting) = VideoInbox::coalesce(packet, &video);
                        if need_keyframe.load(Ordering::Relaxed) {
                            if !packet.header.keyframe {
                                request(false);
                                continue;
                            }
                            need_keyframe.store(false, Ordering::Relaxed);
                        }

                        let mut latest = None;
                        let catch_up = !rest.is_empty() || more_waiting;
                        for packet in std::iter::once(packet).chain(rest) {
                            match decoder.decode(&packet.data, packet.header.pts_us) {
                                Ok(Some(frame)) => latest = Some(frame),
                                Ok(None) => {}
                                Err(err) => {
                                    tracing::debug!("decode error, requesting keyframe: {err:#}");
                                    need_keyframe.store(true, Ordering::Relaxed);
                                    request(true);
                                    latest = None;
                                    break;
                                }
                            }
                        }
                        let Some(frame) = latest else { continue };
                        slot.set_decoder(decoder.name());
                        // Hold for audio only when we are caught up. Sleeping
                        // while a backlog waits is what made delay compound.
                        if !catch_up && let Some(audio) = clock.audio_pts() {
                            let ahead = frame.pts_us() as i64 - audio;
                            if ahead > 5_000 {
                                let wait = ahead.min(MAX_SYNC_DELAY_US) as u64;
                                std::thread::sleep(Duration::from_micros(wait));
                            }
                        }
                        slot.publish(frame);
                    }
                })
                .expect("spawning thread");
        }

        if let Some(playback) = playback.clone() {
            std::thread::Builder::new()
                .name("recv-audio".into())
                .spawn(move || {
                    let mut decoder = match OpusDecoder::new() {
                        Ok(d) => d,
                        Err(err) => {
                            tracing::error!("creating Opus decoder failed: {err:#}");
                            return;
                        }
                    };
                    while let Ok(packet) = audio_rx.recv() {
                        match decoder.decode(packet.seq, &packet.data) {
                            Ok(pcm) if !pcm.is_empty() => playback.push(packet.pts_us, &pcm),
                            Ok(_) => {}
                            Err(err) => tracing::debug!("opus decode failed: {err:#}"),
                        }
                    }
                })
                .expect("spawning thread");
        }

        Self {
            stream_id,
            slot,
            video,
            audio_tx,
            need_keyframe,
            playback,
        }
    }

    pub fn push_video(&self, packet: VideoPacket) {
        self.video.push(packet, &self.need_keyframe);
    }

    pub fn push_audio(&self, packet: AudioPacket) {
        // Datagram semantics: drop when the decoder is behind.
        let _ = self.audio_tx.try_send(packet);
    }

    pub fn set_gain(&self, gain: f32) {
        if let Some(playback) = &self.playback {
            playback.set_gain(gain);
        }
    }
}

impl Drop for RemoteStream {
    fn drop(&mut self) {
        self.video.close();
    }
}
