//! Media processing: pixel conversion, H.264 video coding, Opus audio coding and
//! audio playback/mixing.

#[cfg(feature = "codecs")]
pub mod audio;
pub mod clock;
#[cfg(feature = "hw")]
mod ffmpeg;
pub mod video;

/// Sample rate used for all audio inside the app.
pub const SAMPLE_RATE: u32 = 48_000;
/// Number of interleaved channels used for all audio inside the app.
pub const CHANNELS: usize = 2;
/// Samples per channel in one Opus packet (20 ms).
pub const FRAME_SAMPLES: usize = 960;
/// Interleaved samples in one Opus packet.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
