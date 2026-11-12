//! Screen/window capture through the xdg-desktop-portal ScreenCast API.

use crate::{PixelFormat, RawFrame, SourceKind, VideoCapture};
use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use crossbeam_channel::{Sender, bounded};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use screenshare_media::clock::now_us;
use std::os::fd::OwnedFd;

struct PortalStream {
    fd: OwnedFd,
    node_id: u32,
    kind: SourceKind,
    size: (u32, u32),
}

/// Opens the portal picker and starts streaming the chosen monitor or window.
pub(crate) fn start(max_fps: u32) -> Result<VideoCapture> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(SourceKind, (u32, u32))>>();
    let (frame_tx, frame_rx) = bounded(2);
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();

    std::thread::Builder::new()
        .name("portal-capture".into())
        .spawn(move || {
            let (proxy, session, stream) = match pollster::block_on(open_portal()) {
                Ok(v) => v,
                Err(err) => {
                    let _ = setup_tx.send(Err(err));
                    return;
                }
            };
            let _ = setup_tx.send(Ok((stream.kind, stream.size)));
            if let Err(err) = run_stream(stream, max_fps, frame_tx, quit_rx) {
                tracing::warn!("screen cast stream ended with error: {err:#}");
            }
            let _ = pollster::block_on(session.close());
            drop(proxy);
        })?;

    let (kind, size) = setup_rx
        .recv()
        .map_err(|_| anyhow!("portal thread exited"))??;
    let title = match kind {
        SourceKind::Monitor => "Screen",
        SourceKind::Window => "Window",
    };
    Ok(VideoCapture::new(
        frame_rx,
        kind,
        title.to_string(),
        size,
        None,
        Box::new(move || {
            let _ = quit_tx.send(());
        }),
    ))
}

async fn open_portal() -> Result<(
    Screencast,
    ashpd::desktop::Session<Screencast>,
    PortalStream,
)> {
    let proxy = Screencast::new()
        .await
        .context("screen cast portal unavailable")?;
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::EMBEDDED) => CursorMode::EMBEDDED,
        _ => CursorMode::HIDDEN,
    };
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .context("screen sharing was cancelled")?;
    let Some(stream) = response.streams().first() else {
        bail!("no stream was selected");
    };
    let kind = match stream.source_type() {
        Some(t) if t.contains(SourceType::WINDOW) => SourceKind::Window,
        _ => SourceKind::Monitor,
    };
    let size = stream
        .size()
        .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
        .unwrap_or((1920, 1080));
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("opening PipeWire remote")?;
    Ok((
        proxy,
        session,
        PortalStream {
            fd,
            node_id,
            kind,
            size,
        },
    ))
}

struct StreamData {
    format: spa::param::video::VideoInfoRaw,
    pixel: Option<PixelFormat>,
    tx: Sender<RawFrame>,
    min_interval_us: u64,
    last_us: u64,
}

fn run_stream(
    portal: PortalStream,
    max_fps: u32,
    tx: Sender<RawFrame>,
    quit_rx: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(portal.fd, None)?;

    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

    let stream = pw::stream::StreamBox::new(
        &core,
        "screenshare-video",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let data = StreamData {
        format: Default::default(),
        pixel: None,
        tx,
        min_interval_us: 1_000_000 / max_fps.max(1) as u64,
        last_us: 0,
    };

    let ml = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            tracing::debug!("screen cast stream: {old:?} -> {new:?}");
            if let pw::stream::StreamState::Error(err) = &new {
                tracing::warn!("screen cast stream error: {err}");
                ml.quit();
            }
            if matches!(new, pw::stream::StreamState::Unconnected)
                && !matches!(old, pw::stream::StreamState::Connecting)
            {
                ml.quit();
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if data.format.parse(param).is_err() {
                return;
            }
            use spa::param::video::VideoFormat as F;
            data.pixel = match data.format.format() {
                F::BGRx | F::BGRA => Some(PixelFormat::Bgra),
                F::RGBx | F::RGBA => Some(PixelFormat::Rgba),
                other => {
                    tracing::warn!("unsupported screen cast format {other:?}");
                    None
                }
            };
            let size = data.format.size();
            tracing::info!(
                "screen cast format {:?} {}x{}",
                data.format.format(),
                size.width,
                size.height
            );
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(pixel) = data.pixel else { return };
            let now = now_us();
            if now.saturating_sub(data.last_us) < data.min_interval_us {
                return;
            }
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk = d.chunk();
            let (size, offset, stride) = (
                chunk.size() as usize,
                chunk.offset() as usize,
                chunk.stride(),
            );
            if size == 0 {
                // Cursor-only or damage-free update.
                return;
            }
            let (w, h) = (data.format.size().width, data.format.size().height);
            let stride = if stride > 0 {
                stride as usize
            } else {
                w as usize * 4
            };
            let needed = stride * h as usize;
            let Some(bytes) = d.data() else { return };
            if bytes.len() < offset + needed || w == 0 || h == 0 {
                return;
            }
            data.last_us = now;
            let frame = RawFrame {
                width: w,
                height: h,
                stride,
                format: pixel,
                data: bytes[offset..offset + needed].to_vec(),
                pts_us: now,
            };
            if let Err(crossbeam_channel::TrySendError::Disconnected(_)) = data.tx.try_send(frame) {
                tracing::debug!("frame receiver gone");
            }
        })
        .register()?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: portal.size.0.max(1),
                height: portal.size.1.max(1)
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction {
                num: max_fps.max(1),
                denom: 1
            },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing format: {e:?}"))?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values).context("format pod")?];

    stream.connect(
        spa::utils::Direction::Input,
        Some(portal.node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}
