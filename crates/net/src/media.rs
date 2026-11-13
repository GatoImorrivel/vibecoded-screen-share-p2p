//! The media protocol: sharers accept viewer connections and push frames to each
//! subscriber; viewers dial sharers and receive frames.

use crate::protocol::{
    AudioPacket, ControlMsg, MEDIA_ALPN, VideoPacket, read_framed, read_video, write_framed,
    write_video,
};
use crate::session::NetEvent;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointId as PeerId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak, mpsc as std_mpsc};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

/// Requests from the network layer to the sharer's encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderControl {
    Keyframe,
    Bitrate(u32),
}

/// Frames queued per subscriber before the sharer starts dropping for that subscriber.
const SUBSCRIBER_QUEUE: usize = 4;
/// How long a single frame may take to send before we abandon it. Kept short so
/// a slow network does not stall every newer frame behind one stuck write.
const FRAME_SEND_TIMEOUT: Duration = Duration::from_millis(400);
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Bitrate cuts are expensive (some encoders reopen); space them out.
const BITRATE_CUT_INTERVAL: Duration = Duration::from_secs(3);
const BITRATE_RAISE_INTERVAL: Duration = Duration::from_secs(5);

struct Subscriber {
    tx: mpsc::Sender<Arc<VideoPacket>>,
    conn: Connection,
    waiting_keyframe: bool,
}

/// Additive-increase / multiplicative-decrease bitrate control driven by
/// real send failures, not by local queue pressure.
struct RateController {
    target: u32,
    min: u32,
    max: u32,
    last_change: Instant,
    last_congestion: Instant,
}

impl RateController {
    fn on_congestion(&mut self, now: Instant) -> Option<u32> {
        self.last_congestion = now;
        if now.duration_since(self.last_change) < BITRATE_CUT_INTERVAL {
            return None;
        }
        let next = ((self.target as f64 * 0.7) as u32).max(self.min);
        self.change(next, now)
    }

    fn on_tick(&mut self, now: Instant) -> Option<u32> {
        if now.duration_since(self.last_congestion) < BITRATE_RAISE_INTERVAL
            || now.duration_since(self.last_change) < BITRATE_RAISE_INTERVAL
        {
            return None;
        }
        let next = ((self.target as f64 * 1.15) as u32).min(self.max);
        self.change(next, now)
    }

    fn change(&mut self, next: u32, now: Instant) -> Option<u32> {
        if next == self.target {
            return None;
        }
        self.target = next;
        self.last_change = now;
        Some(next)
    }
}

struct ActiveShare {
    stream_id: u64,
    subscribers: HashMap<PeerId, Subscriber>,
    control: std_mpsc::Sender<EncoderControl>,
    rate: RateController,
    last_keyframe_request: Option<Instant>,
}

impl ActiveShare {
    fn request_keyframe(&mut self, now: Instant) {
        if self
            .last_keyframe_request
            .is_some_and(|t| now.duration_since(t) < KEYFRAME_MIN_INTERVAL)
        {
            return;
        }
        self.last_keyframe_request = Some(now);
        let _ = self.control.send(EncoderControl::Keyframe);
    }

    /// A frame send timed out or failed: the peer cannot keep up. Cut bitrate
    /// and make them wait for a keyframe. Local queue Full is *not* this —
    /// that only drops the newest frame for latency.
    fn congested(&mut self, peer: PeerId, now: Instant) {
        if let Some(sub) = self.subscribers.get_mut(&peer) {
            sub.waiting_keyframe = true;
        }
        if let Some(bitrate) = self.rate.on_congestion(now) {
            tracing::debug!("congestion: lowering bitrate to {bitrate}");
            let _ = self.control.send(EncoderControl::Bitrate(bitrate));
        }
        self.request_keyframe(now);
    }
}

/// Our own active share (at most one) and its subscribers.
#[derive(Default)]
pub(crate) struct ShareRegistry {
    active: Mutex<Option<ActiveShare>>,
}

impl ShareRegistry {
    fn subscribe(
        self: &Arc<Self>,
        peer: PeerId,
        stream_id: u64,
        conn: &Connection,
    ) -> Option<mpsc::Receiver<Arc<VideoPacket>>> {
        let mut guard = self.active.lock().unwrap();
        let share = guard.as_mut().filter(|s| s.stream_id == stream_id)?;
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        share.subscribers.insert(
            peer,
            Subscriber {
                tx,
                conn: conn.clone(),
                waiting_keyframe: true,
            },
        );
        share.last_keyframe_request = None;
        share.request_keyframe(Instant::now());
        Some(rx)
    }

    fn unsubscribe(&self, peer: PeerId) {
        if let Some(share) = self.active.lock().unwrap().as_mut() {
            share.subscribers.remove(&peer);
        }
    }

    fn request_keyframe(&self, stream_id: u64) {
        if let Some(share) = self
            .active
            .lock()
            .unwrap()
            .as_mut()
            .filter(|s| s.stream_id == stream_id)
        {
            share.request_keyframe(Instant::now());
        }
    }

    fn congested(&self, peer: PeerId, stream_id: u64) {
        if let Some(share) = self
            .active
            .lock()
            .unwrap()
            .as_mut()
            .filter(|s| s.stream_id == stream_id)
        {
            share.congested(peer, Instant::now());
        }
    }
}

/// Handle to our active share. Dropping it ends the share.
pub struct ShareHandle {
    registry: Arc<ShareRegistry>,
    stream_id: u64,
    control_rx: Mutex<std_mpsc::Receiver<EncoderControl>>,
    audio_seq: std::sync::atomic::AtomicU32,
    on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl ShareHandle {
    pub(crate) fn new(
        registry: Arc<ShareRegistry>,
        stream_id: u64,
        bitrate: (u32, u32, u32),
        on_drop: Box<dyn FnOnce() + Send + Sync>,
    ) -> Self {
        let (control, control_rx) = std_mpsc::channel();
        let (initial, min, max) = bitrate;
        let now = Instant::now();
        *registry.active.lock().unwrap() = Some(ActiveShare {
            stream_id,
            subscribers: HashMap::new(),
            control,
            rate: RateController {
                target: initial,
                min,
                max,
                last_change: now,
                last_congestion: now,
            },
            last_keyframe_request: None,
        });
        Self {
            registry,
            stream_id,
            control_rx: Mutex::new(control_rx),
            audio_seq: Default::default(),
            on_drop: Some(on_drop),
        }
    }

    pub fn stream_id(&self) -> u64 {
        self.stream_id
    }

    pub fn subscriber_count(&self) -> usize {
        self.registry
            .active
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |s| s.subscribers.len())
    }

    /// Next pending encoder request, if any.
    pub fn try_control(&self) -> Option<EncoderControl> {
        self.control_rx.lock().unwrap().try_recv().ok()
    }

    /// Queues an encoded frame for every subscriber that can take it.
    ///
    /// A full local queue only drops this frame for that subscriber. Treating
    /// Full as congestion used to cut bitrate and force keyframes, which on
    /// Linux (VAAPI/software encoder reopen) permanently collapsed FPS.
    pub fn send_video(&self, packet: VideoPacket) {
        let packet = Arc::new(packet);
        let now = Instant::now();
        let mut guard = self.registry.active.lock().unwrap();
        let Some(share) = guard.as_mut() else { return };
        for sub in share.subscribers.values_mut() {
            if sub.waiting_keyframe && !packet.header.keyframe {
                continue;
            }
            match sub.tx.try_send(packet.clone()) {
                Ok(()) => sub.waiting_keyframe = false,
                // Latency shed: the sender will pick up a later frame.
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        if let Some(bitrate) = share.rate.on_tick(now) {
            tracing::debug!("raising bitrate to {bitrate}");
            let _ = share.control.send(EncoderControl::Bitrate(bitrate));
        }
    }

    /// Sends one Opus packet to every subscriber as an unreliable datagram.
    pub fn send_audio(&self, pts_us: u64, data: Vec<u8>) {
        let seq = self
            .audio_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let packet = AudioPacket {
            stream_id: self.stream_id,
            seq,
            pts_us,
            data,
        };
        let Ok(bytes) = postcard::to_stdvec(&packet) else {
            return;
        };
        let bytes = bytes::Bytes::from(bytes);
        let guard = self.registry.active.lock().unwrap();
        let Some(share) = guard.as_ref() else { return };
        for sub in share.subscribers.values() {
            if let Err(err) = sub.conn.send_datagram(bytes.clone()) {
                tracing::trace!("audio datagram dropped: {err}");
            }
        }
    }
}

impl Drop for ShareHandle {
    fn drop(&mut self) {
        let mut guard = self.registry.active.lock().unwrap();
        if guard
            .as_ref()
            .is_some_and(|s| s.stream_id == self.stream_id)
        {
            *guard = None;
        }
        drop(guard);
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// Accept side of [`MEDIA_ALPN`].
#[derive(Clone)]
pub(crate) struct MediaProtocol {
    pub(crate) registry: Arc<ShareRegistry>,
}

impl std::fmt::Debug for MediaProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaProtocol")
    }
}

impl ProtocolHandler for MediaProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let (_send, mut recv) = conn.accept_bi().await.map_err(AcceptError::from_err)?;
        let mut senders = JoinSet::new();
        loop {
            match read_framed::<ControlMsg>(&mut recv).await {
                Ok(Some(ControlMsg::Subscribe { stream_id })) => {
                    if let Some(rx) = self.registry.subscribe(peer, stream_id, &conn) {
                        tracing::info!("{} subscribed to stream {stream_id}", peer.fmt_short());
                        senders.spawn(video_sender(
                            conn.clone(),
                            rx,
                            Arc::downgrade(&self.registry),
                            peer,
                            stream_id,
                        ));
                    }
                }
                Ok(Some(ControlMsg::Unsubscribe { .. })) => self.registry.unsubscribe(peer),
                Ok(Some(ControlMsg::RequestKeyframe { stream_id })) => {
                    self.registry.request_keyframe(stream_id)
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!("control stream from {} ended: {err:#}", peer.fmt_short());
                    break;
                }
            }
        }
        self.registry.unsubscribe(peer);
        senders.abort_all();
        conn.close(0u32.into(), b"bye");
        Ok(())
    }
}

async fn video_sender(
    conn: Connection,
    mut rx: mpsc::Receiver<Arc<VideoPacket>>,
    registry: Weak<ShareRegistry>,
    peer: PeerId,
    stream_id: u64,
) {
    while let Some(mut packet) = rx.recv().await {
        // Prefer the newest waiting frame so a slow send does not deliver a
        // backlog of already-stale pictures.
        while let Ok(newer) = rx.try_recv() {
            packet = newer;
        }

        let mut send = match tokio::time::timeout(FRAME_SEND_TIMEOUT, conn.open_uni()).await {
            Ok(Ok(send)) => send,
            Ok(Err(err)) => {
                if conn.close_reason().is_some() {
                    break;
                }
                tracing::debug!("opening video stream failed: {err:#}");
                if let Some(reg) = registry.upgrade() {
                    reg.congested(peer, stream_id);
                }
                continue;
            }
            Err(_) => {
                tracing::debug!("opening video stream timed out for {}", peer.fmt_short());
                if let Some(reg) = registry.upgrade() {
                    reg.congested(peer, stream_id);
                }
                continue;
            }
        };
        // Newer frames take priority over a stalled older one on the same connection.
        let _ = send.set_priority((packet.header.pts_us / 1000) as i32);
        match tokio::time::timeout(FRAME_SEND_TIMEOUT, write_video(&mut send, &packet)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = send.reset(0u32.into());
                if conn.close_reason().is_some() {
                    break;
                }
                tracing::debug!("frame send failed: {err:#}");
                if let Some(reg) = registry.upgrade() {
                    reg.congested(peer, stream_id);
                }
            }
            Err(_) => {
                // Abort the stuck stream so it stops competing for bandwidth.
                let _ = send.reset(0u32.into());
                tracing::debug!("frame send timed out for {}", peer.fmt_short());
                if let Some(reg) = registry.upgrade() {
                    reg.congested(peer, stream_id);
                }
            }
        }
    }
}

/// Our connection to one sharer we are watching.
pub(crate) struct Viewer {
    cmd: mpsc::UnboundedSender<ControlMsg>,
    pub(crate) subscribed: HashSet<u64>,
    tasks: JoinSet<()>,
}

impl Viewer {
    pub(crate) fn spawn(
        endpoint: Endpoint,
        peer: PeerId,
        events: mpsc::UnboundedSender<NetEvent>,
    ) -> Self {
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(run_viewer(endpoint, peer, cmd_rx, events));
        Self {
            cmd,
            subscribed: HashSet::new(),
            tasks,
        }
    }

    pub(crate) fn send(&self, msg: ControlMsg) {
        let _ = self.cmd.send(msg);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.tasks.abort_all();
    }
}

async fn run_viewer(
    endpoint: Endpoint,
    peer: PeerId,
    mut cmd_rx: mpsc::UnboundedReceiver<ControlMsg>,
    events: mpsc::UnboundedSender<NetEvent>,
) {
    let result: anyhow::Result<()> = async {
        let conn =
            tokio::time::timeout(Duration::from_secs(20), endpoint.connect(peer, MEDIA_ALPN))
                .await
                .map_err(|_| anyhow::anyhow!("timed out connecting"))??;
        tracing::info!("media connection to {} established", peer.fmt_short());
        let (mut send, _recv) = conn.open_bi().await?;

        let video_conn = conn.clone();
        let video_events = events.clone();
        let video = async move {
            loop {
                let recv = video_conn.accept_uni().await?;
                match read_video(recv).await {
                    Ok(packet) => {
                        let _ = video_events.send(NetEvent::Video { peer, packet });
                    }
                    Err(err) => tracing::debug!("dropped video frame: {err:#}"),
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        };

        let audio_conn = conn.clone();
        let audio_events = events.clone();
        let audio = async move {
            loop {
                let datagram = audio_conn.read_datagram().await?;
                if let Ok(packet) = postcard::from_bytes::<AudioPacket>(&datagram) {
                    let _ = audio_events.send(NetEvent::Audio { peer, packet });
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        };

        let control = async move {
            while let Some(msg) = cmd_rx.recv().await {
                write_framed(&mut send, &msg).await?;
            }
            let _ = send.finish();
            anyhow::Ok(())
        };

        tokio::select! {
            r = video => r?,
            r = audio => r?,
            r = control => r?,
        }
        conn.close(0u32.into(), b"bye");
        Ok(())
    }
    .await;
    if let Err(err) = result {
        tracing::info!("media connection to {} closed: {err:#}", peer.fmt_short());
    }
    let _ = events.send(NetEvent::ViewerClosed { peer });
}
