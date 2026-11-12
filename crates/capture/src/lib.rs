//! Platform screen and audio capture behind one API.
//!
//! - Windows: Windows.Graphics.Capture for video, WASAPI process loopback for audio.
//! - Linux (Wayland, or X11 with a portal): xdg-desktop-portal ScreenCast + PipeWire
//!   for video, PipeWire links from application streams for audio.
//! - Linux X11 without a portal: polling capture through `xcap`.

mod mixer;
mod sources;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use anyhow::Result;
use crossbeam_channel::Receiver;
pub use screenshare_media::video::{PixelFormat, RawFrame};

pub use sources::{Thumbnail, list_sources, thumbnail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Monitor,
    Window,
}

/// A capturable monitor or window, as listed by the in-app picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Platform handle: xcap monitor/window id (HMONITOR/HWND on Windows, XID on X11).
    pub id: u32,
    pub kind: SourceKind,
    pub title: String,
    pub app_name: Option<String>,
    pub pid: Option<u32>,
    pub width: u32,
    pub height: u32,
}

/// How the user chooses what to share on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// We list sources ourselves (Windows, X11).
    InApp,
    /// The xdg-desktop-portal shows its own picker (Wayland).
    Portal,
}

pub fn picker_kind() -> PickerKind {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return PickerKind::Portal;
        }
    }
    PickerKind::InApp
}

/// What to capture.
#[derive(Debug, Clone)]
pub enum VideoRequest {
    Source(Source),
    Portal,
}

/// A running video capture. Frames stop (the channel disconnects) when the
/// source goes away; dropping the capture stops it.
pub struct VideoCapture {
    pub frames: Receiver<RawFrame>,
    pub kind: SourceKind,
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Process owning the captured window, when known.
    pub pid: Option<u32>,
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl VideoCapture {
    pub(crate) fn new(
        frames: Receiver<RawFrame>,
        kind: SourceKind,
        title: String,
        size: (u32, u32),
        pid: Option<u32>,
        stop: Box<dyn FnOnce() + Send>,
    ) -> Self {
        Self {
            frames,
            kind,
            title,
            width: size.0,
            height: size.1,
            pid,
            stop: Some(stop),
        }
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// Starts capturing. May block while a system picker dialog is open, so call
/// it off the UI thread.
pub fn start_video(request: VideoRequest, max_fps: u32) -> Result<VideoCapture> {
    match request {
        VideoRequest::Source(source) => {
            #[cfg(windows)]
            {
                windows::video::start(source, max_fps)
            }
            #[cfg(not(windows))]
            {
                sources::start_polling(source, max_fps)
            }
        }
        VideoRequest::Portal => {
            #[cfg(target_os = "linux")]
            {
                linux::portal::start(max_fps)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = max_fps;
                anyhow::bail!("the screen cast portal is only available on Linux")
            }
        }
    }
}

/// An application currently producing audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    pub name: String,
    pub pid: Option<u32>,
    pub binary: Option<String>,
}

impl AudioApp {
    /// Case-insensitive substring match against name or binary.
    pub fn matches(&self, pattern: &str) -> bool {
        let p = pattern.trim().to_lowercase();
        if p.is_empty() {
            return false;
        }
        self.name.to_lowercase().contains(&p)
            || self
                .binary
                .as_ref()
                .is_some_and(|b| b.to_lowercase().contains(&p))
    }
}

/// Whether `app` is this process. Matches the binary name too, because inside
/// a Flatpak sandbox our pid differs from the host pid PipeWire reports.
pub(crate) fn is_self(app: &AudioApp) -> bool {
    use std::sync::OnceLock;
    static OWN_BINARY: OnceLock<Option<String>> = OnceLock::new();
    if app.pid == Some(std::process::id()) {
        return true;
    }
    let own = OWN_BINARY.get_or_init(|| {
        std::env::current_exe()
            .ok()?
            .file_name()?
            .to_str()
            .map(|s| s.trim_end_matches(".exe").to_string())
    });
    match (own, &app.binary) {
        (Some(own), Some(binary)) => binary.trim_end_matches(".exe") == own,
        _ => false,
    }
}

/// Lists applications that currently have an audio playback stream.
pub fn list_audio_apps() -> Vec<AudioApp> {
    let result = {
        #[cfg(target_os = "linux")]
        {
            linux::audio::list_apps()
        }
        #[cfg(windows)]
        {
            windows::audio::list_apps()
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            Ok(Vec::new())
        }
    };
    let mut apps = result.unwrap_or_else(|err| {
        tracing::warn!("listing audio apps failed: {err:#}");
        Vec::new()
    });
    apps.retain(|a| !is_self(a));
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.pid.is_some() && a.pid == b.pid);
    apps
}

/// Which audio to capture alongside the video.
#[derive(Debug, Clone)]
pub enum AudioTarget {
    /// Everything playing on the system except this app and the listed apps.
    Desktop { exclude: Vec<String> },
    /// Only the given application (and its child processes).
    App(AudioApp),
}

/// A running audio capture producing 20 ms frames of 48 kHz stereo f32 with
/// their capture timestamps. Dropping it stops capture.
pub struct AudioCapture {
    pub frames: Receiver<(u64, Vec<f32>)>,
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl AudioCapture {
    pub(crate) fn new(frames: Receiver<(u64, Vec<f32>)>, stop: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            frames,
            stop: Some(stop),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

pub fn start_audio(target: AudioTarget) -> Result<AudioCapture> {
    #[cfg(target_os = "linux")]
    {
        linux::audio::start(target)
    }
    #[cfg(windows)]
    {
        windows::audio::start(target)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = target;
        anyhow::bail!("audio capture is not supported on this platform")
    }
}
