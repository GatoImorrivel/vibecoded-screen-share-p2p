//! Monitor and window capture with Windows.Graphics.Capture.

use crate::{PixelFormat, RawFrame, Source, SourceKind, VideoCapture};
use anyhow::{Result, anyhow};
use crossbeam_channel::{Sender, TrySendError, bounded};
use screenshare_media::clock::now_us;
use std::time::Duration;
use windows_capture::capture::{Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{self, InternalCaptureControl};
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

fn settings<T>(
    item: T,
    tx: Sender<RawFrame>,
    cursor: CursorCaptureSettings,
    border: DrawBorderSettings,
    interval: MinimumUpdateIntervalSettings,
) -> Settings<Sender<RawFrame>, T>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType>,
{
    Settings::new(
        item,
        cursor,
        border,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        tx,
    )
}

pub(crate) fn start(source: Source, max_fps: u32) -> Result<VideoCapture> {
    let (tx, rx) = bounded(2);
    let handle = source.id as usize as *mut std::ffi::c_void;
    // Prefer no yellow border, the cursor, and a frame-rate cap. Each of those
    // is a newer Graphics Capture option; older Windows rejects the session
    // entirely if we ask for one it lacks, so drop that option and try again.
    let mut cursor = CursorCaptureSettings::WithCursor;
    let mut border = DrawBorderSettings::WithoutBorder;
    let mut interval = MinimumUpdateIntervalSettings::Custom(Duration::from_micros(
        1_000_000 / max_fps.max(1) as u64,
    ));
    let control = loop {
        let started = match source.kind {
            SourceKind::Monitor => Handler::start_free_threaded(settings(
                Monitor::from_raw_hmonitor(handle),
                tx.clone(),
                cursor,
                border,
                interval,
            )),
            SourceKind::Window => Handler::start_free_threaded(settings(
                Window::from_raw_hwnd(handle),
                tx.clone(),
                cursor,
                border,
                interval,
            )),
        };
        match started {
            Ok(control) => break control,
            Err(GraphicsCaptureApiError::GraphicsCaptureApiError(reason)) => match reason {
                graphics_capture_api::Error::BorderConfigUnsupported
                    if border != DrawBorderSettings::Default =>
                {
                    tracing::info!(
                        "this Windows version cannot hide the capture border; sharing with the system border"
                    );
                    border = DrawBorderSettings::Default;
                }
                graphics_capture_api::Error::MinimumUpdateIntervalUnsupported
                    if interval != MinimumUpdateIntervalSettings::Default =>
                {
                    tracing::info!(
                        "this Windows version cannot cap the capture rate; sharing at the system rate"
                    );
                    interval = MinimumUpdateIntervalSettings::Default;
                }
                graphics_capture_api::Error::CursorConfigUnsupported
                    if cursor != CursorCaptureSettings::Default =>
                {
                    tracing::info!(
                        "this Windows version cannot force cursor capture; using the system default"
                    );
                    cursor = CursorCaptureSettings::Default;
                }
                other => return Err(anyhow!("starting capture: {other}")),
            },
            Err(other) => return Err(anyhow!("starting capture: {other}")),
        }
    };

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
