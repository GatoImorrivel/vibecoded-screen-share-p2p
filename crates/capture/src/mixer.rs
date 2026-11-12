//! Turns one or more irregular PCM sources into a steady stream of 20 ms frames.

use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use screenshare_media::clock::now_us;
use screenshare_media::{CHANNELS, FRAME_LEN, FRAME_SAMPLES, SAMPLE_RATE};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Cap per source so a stalled consumer can't grow memory without bound (200 ms).
const MAX_BUFFERED: usize = 10 * FRAME_LEN;

#[derive(Default)]
struct Shared {
    sources: Vec<Arc<Mutex<VecDeque<f32>>>>,
}

/// Collects interleaved 48 kHz stereo samples from several writers and emits
/// summed 20 ms frames on a channel.
#[derive(Clone)]
pub(crate) struct SourceMixer {
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
}

/// One input of a [`SourceMixer`]. Dropping it removes the input.
pub(crate) struct SourceWriter {
    buf: Arc<Mutex<VecDeque<f32>>>,
    shared: Arc<Mutex<Shared>>,
}

impl SourceWriter {
    pub(crate) fn push(&self, samples: &[f32]) {
        let mut buf = self.buf.lock();
        buf.extend(samples.iter().copied());
        if buf.len() > MAX_BUFFERED {
            let excess = buf.len() - MAX_BUFFERED;
            buf.drain(..excess - excess % CHANNELS);
        }
    }
}

impl Drop for SourceWriter {
    fn drop(&mut self) {
        self.shared
            .lock()
            .sources
            .retain(|s| !Arc::ptr_eq(s, &self.buf));
    }
}

impl SourceMixer {
    pub(crate) fn start() -> (Self, Receiver<(u64, Vec<f32>)>) {
        let (tx, rx) = bounded(50);
        let mixer = Self {
            shared: Arc::default(),
            running: Arc::new(AtomicBool::new(true)),
        };
        let m = mixer.clone();
        std::thread::Builder::new()
            .name("audio-mixer".into())
            .spawn(move || m.run(tx))
            .expect("spawning mixer thread");
        (mixer, rx)
    }

    pub(crate) fn add_source(&self) -> SourceWriter {
        let buf = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_BUFFERED)));
        self.shared.lock().sources.push(buf.clone());
        SourceWriter {
            buf,
            shared: self.shared.clone(),
        }
    }

    pub(crate) fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn run(self, tx: Sender<(u64, Vec<f32>)>) {
        let frame_us = (FRAME_SAMPLES as u64 * 1_000_000) / SAMPLE_RATE as u64;
        while self.running.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let sources = self.shared.lock().sources.clone();
                let ready = sources.iter().any(|s| s.lock().len() >= FRAME_LEN);
                if !ready {
                    break;
                }
                let mut frame = vec![0f32; FRAME_LEN];
                for source in &sources {
                    let mut buf = source.lock();
                    let n = buf.len().min(FRAME_LEN);
                    for (out, s) in frame.iter_mut().zip(buf.drain(..n)) {
                        *out += s;
                    }
                }
                for s in frame.iter_mut() {
                    *s = s.clamp(-1.0, 1.0);
                }
                let pts = now_us().saturating_sub(frame_us);
                if let Err(crossbeam_channel::TrySendError::Disconnected(_)) =
                    tx.try_send((pts, frame))
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixes_two_sources_into_frames() {
        let (mixer, rx) = SourceMixer::start();
        let a = mixer.add_source();
        let b = mixer.add_source();
        a.push(&vec![0.25; FRAME_LEN]);
        b.push(&vec![0.5; FRAME_LEN]);
        let (_, frame) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(frame.len(), FRAME_LEN);
        assert!(frame.iter().all(|s| (*s - 0.75).abs() < 1e-6));
        drop(b);
        a.push(&vec![0.1; FRAME_LEN]);
        let (_, frame) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!((frame[0] - 0.1).abs() < 1e-6);
        mixer.stop();
    }
}
