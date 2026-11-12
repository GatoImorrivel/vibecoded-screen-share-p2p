//! Monitor and window capture with Windows.Graphics.Capture.

use crate::{PixelFormat, RawFrame, Source, SourceKind, VideoCapture};
use anyhow::{Result, anyhow};
use crossbeam_channel::{Sender, TrySendError, bounded};
use screenshare_media::clock::now_us;
use std::time::Duration;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

struct Handler {
    tx: Sender<RawFrame>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Sender<RawFrame>;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { tx: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let mut buffer = frame.buffer()?;
        let (width, height) = (buffer.width(), buffer.height());
        let stride = buffer.row_pitch() as usize;
        let raw = buffer.as_raw_buffer();
        let len = (stride * height as usize).min(raw.len());
        let frame = RawFrame {
            width,
            height,
            stride,
            format: PixelFormat::Bgra,
            data: raw[..len].to_vec(),
            pts_us: now_us(),
        };
        if let Err(TrySendError::Disconnected(_)) = self.tx.try_send(frame) {
            control.stop();
        }
        Ok(())
    }
}

fn settings<T>(item: T, tx: Sender<RawFrame>, max_fps: u32) -> Settings<Sender<RawFrame>, T>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType>,
{
    Settings::new(
        item,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_micros(
            1_000_000 / max_fps.max(1) as u64,
        )),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        tx,
    )
}

pub(crate) fn start(source: Source, max_fps: u32) -> Result<VideoCapture> {
    let (tx, rx) = bounded(2);
    let handle = source.id as usize as *mut std::ffi::c_void;
    let control = match source.kind {
        SourceKind::Monitor => {
            Handler::start_free_threaded(settings(Monitor::from_raw_hmonitor(handle), tx, max_fps))
        }
        SourceKind::Window => {
            Handler::start_free_threaded(settings(Window::from_raw_hwnd(handle), tx, max_fps))
        }
    }
    .map_err(|e| anyhow!("starting capture: {e}"))?;

    Ok(VideoCapture::new(
        rx,
        source.kind,
        source.title,
        (source.width, source.height),
        source.pid,
        Box::new(move || {
            if let Err(err) = control.stop() {
                tracing::debug!("stopping capture: {err}");
            }
        }),
    ))
}
