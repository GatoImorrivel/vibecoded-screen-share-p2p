use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Microseconds since process start, used as the presentation timestamp base
/// for every captured audio and video frame of a share.
pub fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Tracks the sender-side timestamp of the audio currently being played for
/// one remote stream, so its video can be held back to stay in sync.
#[derive(Debug, Default)]
pub struct SyncClock {
    audio_pts_us: AtomicI64,
    updated_local_us: AtomicU64,
}

impl SyncClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            audio_pts_us: AtomicI64::new(-1),
            updated_local_us: AtomicU64::new(0),
        })
    }

    pub fn set_audio_pts(&self, pts_us: i64) {
        self.audio_pts_us.store(pts_us, Ordering::Relaxed);
        self.updated_local_us.store(now_us(), Ordering::Relaxed);
    }

    /// Returns the remote pts currently audible, if audio has been playing recently.
    pub fn audio_pts(&self) -> Option<i64> {
        let pts = self.audio_pts_us.load(Ordering::Relaxed);
        let updated = self.updated_local_us.load(Ordering::Relaxed);
        if pts < 0 || now_us().saturating_sub(updated) > 500_000 {
            return None;
        }
        // Extrapolate since the last mixer callback.
        Some(pts + now_us().saturating_sub(updated) as i64)
    }
}
