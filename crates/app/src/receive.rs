//! One watched remote stream: H.264 decode with audio sync, Opus decode into the mixer.

use crate::video_widget::FrameSlot;
use crossbeam_channel::{RecvTimeoutError, Sender, TrySendError};
use screenshare_media::audio::{AudioPlayer, OpusDecoder, PlaybackHandle};
use screenshare_media::clock::SyncClock;
use screenshare_media::video::H264Decoder;
use screenshare_net::{AudioPacket, PeerId, Session, VideoPacket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest we hold a decoded frame back to line it up with audio.
const MAX_SYNC_DELAY_US: i64 = 150_000;
const KEYFRAME_RETRY: Duration = Duration::from_millis(1000);

pub struct RemoteStream {
    pub stream_id: u64,
    pub slot: Arc<FrameSlot>,
    video_tx: Sender<VideoPacket>,
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
    ) -> Self {
        let clock = SyncClock::new();
        let slot = Arc::new(FrameSlot::default());
        let need_keyframe = Arc::new(AtomicBool::new(true));
        let (video_tx, video_rx) = crossbeam_channel::bounded::<VideoPacket>(90);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<AudioPacket>(100);
        let playback = player.map(|p| Arc::new(p.add_stream(clock.clone())));

        {
            let slot = slot.clone();
            let need_keyframe = need_keyframe.clone();
            let clock = clock.clone();
            std::thread::Builder::new()
                .name("recv-video".into())
                .spawn(move || {
                    let mut decoder = match H264Decoder::new() {
                        Ok(d) => d,
                        Err(err) => {
                            tracing::error!("creating H.264 decoder failed: {err:#}");
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
                        let packet = match video_rx.recv_timeout(Duration::from_millis(250)) {
                            Ok(p) => p,
                            Err(RecvTimeoutError::Timeout) => {
                                if need_keyframe.load(Ordering::Relaxed) {
                                    request(false);
                                }
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        };
                        if need_keyframe.load(Ordering::Relaxed) {
                            if !packet.header.keyframe {
                                request(false);
                                continue;
                            }
                            need_keyframe.store(false, Ordering::Relaxed);
                        }
                        match decoder.decode(&packet.data, packet.header.pts_us) {
                            Ok(Some(frame)) => {
                                if let Some(audio) = clock.audio_pts() {
                                    let ahead = frame.pts_us as i64 - audio;
                                    if ahead > 5_000 {
                                        let wait = ahead.min(MAX_SYNC_DELAY_US) as u64;
                                        std::thread::sleep(Duration::from_micros(wait));
                                    }
                                }
                                slot.publish(frame);
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::debug!("decode error, requesting keyframe: {err:#}");
                                need_keyframe.store(true, Ordering::Relaxed);
                                request(true);
                            }
                        }
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
            video_tx,
            audio_tx,
            need_keyframe,
            playback,
        }
    }

    pub fn push_video(&self, packet: VideoPacket) {
        if let Err(TrySendError::Full(_)) = self.video_tx.try_send(packet) {
            // The decoder fell behind; the next decodable frame is a keyframe.
            self.need_keyframe.store(true, Ordering::Relaxed);
        }
    }

    pub fn push_audio(&self, packet: AudioPacket) {
        let _ = self.audio_tx.try_send(packet);
    }

    pub fn set_gain(&self, gain: f32) {
        if let Some(playback) = &self.playback {
            playback.set_gain(gain);
        }
    }
}
