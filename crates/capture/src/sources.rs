//! Source enumeration, thumbnails and the polling fallback capture, via `xcap`.

use crate::{PixelFormat, RawFrame, Source, SourceKind, VideoCapture};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::bounded;
use screenshare_media::clock::now_us;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A small RGBA preview image.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Lists monitors and visible top-level windows (in-app picker platforms).
pub fn list_sources() -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for (i, m) in xcap::Monitor::all()
        .context("listing monitors")?
        .into_iter()
        .enumerate()
    {
        let name = m
            .friendly_name()
            .or_else(|_| m.name())
            .unwrap_or_else(|_| format!("Screen {}", i + 1));
        out.push(Source {
            id: m.id()?,
            kind: SourceKind::Monitor,
            title: name,
            app_name: None,
            pid: None,
            width: m.width()?,
            height: m.height()?,
        });
    }
    let own = std::process::id();
    for w in xcap::Window::all().context("listing windows")? {
        let (Ok(title), Ok(width), Ok(height)) = (w.title(), w.width(), w.height()) else {
            continue;
        };
        let pid = w.pid().ok();
        if title.trim().is_empty()
            || width < 64
            || height < 64
            || w.is_minimized().unwrap_or(false)
            || pid == Some(own)
        {
            continue;
        }
        out.push(Source {
            id: w.id()?,
            kind: SourceKind::Window,
            title,
            app_name: w.app_name().ok().filter(|s| !s.is_empty()),
            pid,
            width,
            height,
        });
    }
    Ok(out)
}

fn grab(source: &Source) -> Result<xcap::image::RgbaImage> {
    match source.kind {
        SourceKind::Monitor => xcap::Monitor::all()?
            .into_iter()
            .find(|m| m.id().ok() == Some(source.id))
            .ok_or_else(|| anyhow!("monitor is gone"))?
            .capture_image()
            .map_err(Into::into),
        SourceKind::Window => xcap::Window::all()?
            .into_iter()
            .find(|w| w.id().ok() == Some(source.id))
            .ok_or_else(|| anyhow!("window is gone"))?
            .capture_image()
            .map_err(Into::into),
    }
}

/// Captures a single downscaled frame of `source` for the picker.
pub fn thumbnail(source: &Source, max_width: u32) -> Option<Thumbnail> {
    let img = grab(source).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let tw = max_width.min(w).max(1);
    let th = ((h as u64 * tw as u64) / w as u64).max(1) as u32;
    let src = img.as_raw();
    let mut rgba = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        let sy = (y as u64 * h as u64 / th as u64) as usize;
        for x in 0..tw {
            let sx = (x as u64 * w as u64 / tw as u64) as usize;
            let o = (sy * w as usize + sx) * 4;
            rgba.extend_from_slice(&src[o..o + 4]);
        }
    }
    Some(Thumbnail {
        width: tw,
        height: th,
        rgba,
    })
}

/// Fallback capture by repeatedly grabbing screenshots (X11 without a portal).
#[allow(dead_code)]
pub(crate) fn start_polling(source: Source, max_fps: u32) -> Result<VideoCapture> {
    let first = grab(&source).context("capturing first frame")?;
    let size = (first.width(), first.height());
    let (tx, rx) = bounded(2);
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let src = source.clone();
    let interval = Duration::from_secs_f64(1.0 / max_fps.max(1) as f64);
    std::thread::Builder::new()
        .name("xcap-capture".into())
        .spawn(move || {
            let mut next = Some(first);
            while flag.load(Ordering::Relaxed) {
                let started = Instant::now();
                let img = match next.take().map(Ok).unwrap_or_else(|| grab(&src)) {
                    Ok(img) => img,
                    Err(err) => {
                        tracing::info!("capture source ended: {err:#}");
                        break;
                    }
                };
                let frame = RawFrame {
                    width: img.width(),
                    height: img.height(),
                    stride: img.width() as usize * 4,
                    format: PixelFormat::Rgba,
                    data: img.into_raw(),
                    pts_us: now_us(),
                };
                if let Err(crossbeam_channel::TrySendError::Disconnected(_)) = tx.try_send(frame) {
                    break;
                }
                if let Some(rest) = interval.checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        })?;
    Ok(VideoCapture::new(
        rx,
        source.kind,
        source.title,
        size,
        source.pid,
        Box::new(move || running.store(false, Ordering::Relaxed)),
    ))
}
