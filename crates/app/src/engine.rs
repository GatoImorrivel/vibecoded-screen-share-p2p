//! Owns the session, our share and the watched streams. Runs networking on its
//! own tokio runtime and reports to the UI through [`events`].

use crate::receive::RemoteStream;
use crate::share::{ActiveShare, ShareConfig};
use crate::video_widget::FrameSlot;
use iced::futures::Stream;
use parking_lot::Mutex;
use screenshare_capture::{AudioApp, AudioTarget, SourceKind, VideoRequest};
use screenshare_media::audio::AudioPlayer;
use screenshare_net::{NetEvent, PeerId, Session, ShareInfo, ShareKind, Ticket};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub enum UiEvent {
    Connected,
    Disconnected,
    PeerUpdated {
        peer: PeerId,
        nickname: String,
        share: Option<ShareInfo>,
    },
    PeerLeft {
        peer: PeerId,
    },
    /// Our capture source went away (window closed, portal stopped).
    ShareEnded,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: PeerId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    /// Whole desktop → all audio (minus exclusions); window → that app.
    Auto,
    Off,
}

#[derive(Debug, Clone)]
pub struct ShareStarted {
    pub kind: ShareKind,
    pub title: String,
    /// The window's owning app is unknown (portal); the UI should ask which app to capture.
    pub needs_audio_app: bool,
    pub audio_error: Option<String>,
}

#[derive(Default)]
struct State {
    session: Option<Session>,
    generation: u64,
    share: Option<ActiveShare>,
    remote: HashMap<PeerId, RemoteStream>,
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    state: Arc<Mutex<State>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    player: Arc<AudioPlayer>,
}

static UI_EVENTS: OnceLock<Mutex<Option<mpsc::UnboundedReceiver<UiEvent>>>> = OnceLock::new();

/// Stream of engine events for an iced subscription. Only the first caller gets events.
pub fn events() -> impl Stream<Item = UiEvent> {
    let rx = UI_EVENTS.get().and_then(|m| m.lock().take());
    iced::futures::stream::unfold(rx, |rx| async move {
        let mut rx = rx?;
        let event = rx.recv().await?;
        Some((event, Some(rx)))
    })
}

fn spawn_result<T, F>(
    rt: &tokio::runtime::Runtime,
    fut: F,
) -> impl Future<Output = Result<T, String>> + use<T, F>
where
    T: Send + 'static,
    F: Future<Output = Result<T, String>> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    async move { rx.await.unwrap_or_else(|_| Err("cancelled".into())) }
}

impl Engine {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .enable_all()
            .build()
            .expect("building tokio runtime");
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let _ = UI_EVENTS.set(Mutex::new(Some(ui_rx)));
        Self {
            rt,
            state: Arc::default(),
            ui_tx,
            player: Arc::new(AudioPlayer::new()),
        }
    }

    /// Creates a new session, or joins one when `ticket` is given.
    pub fn connect(
        &self,
        ticket: Option<String>,
        nickname: String,
    ) -> impl Future<Output = Result<SessionInfo, String>> + use<> {
        self.leave();
        let state = self.state.clone();
        let ui_tx = self.ui_tx.clone();
        spawn_result(&self.rt, async move {
            let (session, events) = match ticket {
                Some(t) => {
                    let ticket: Ticket = t.parse().map_err(|_| "That invite code is not valid.")?;
                    Session::join(&ticket, nickname).await
                }
                None => Session::create(nickname).await,
            }
            .map_err(|err| format!("{err:#}"))?;
            let info = SessionInfo { id: session.id() };
            let generation = {
                let mut s = state.lock();
                s.generation += 1;
                s.session = Some(session);
                s.generation
            };
            tokio::spawn(pump(events, state, ui_tx, generation));
            Ok(info)
        })
    }

    pub fn leave(&self) {
        let (session, share, remote) = {
            let mut s = self.state.lock();
            s.generation += 1;
            (
                s.session.take(),
                s.share.take(),
                std::mem::take(&mut s.remote),
            )
        };
        drop(remote);
        drop(share);
        if let Some(session) = session {
            self.rt.spawn(session.leave());
        }
    }

    pub fn ticket(&self) -> Option<String> {
        self.state.lock().session.as_ref().map(|s| s.ticket())
    }

    /// Starts receiving `peer`'s stream; returns where its frames appear.
    pub fn watch(&self, peer: PeerId, stream_id: u64) -> Option<Arc<FrameSlot>> {
        let mut s = self.state.lock();
        let session = s.session.clone()?;
        if let Some(existing) = s.remote.get(&peer) {
            if existing.stream_id == stream_id {
                return Some(existing.slot.clone());
            }
            session.unsubscribe(peer, existing.stream_id);
        }
        let stream = RemoteStream::start(session.clone(), peer, stream_id, Some(&self.player));
        let slot = stream.slot.clone();
        s.remote.insert(peer, stream);
        let _guard = self.rt.enter();
        session.subscribe(peer, stream_id);
        Some(slot)
    }

    pub fn unwatch(&self, peer: PeerId) {
        let mut s = self.state.lock();
        if let Some(stream) = s.remote.remove(&peer)
            && let Some(session) = &s.session
        {
            session.unsubscribe(peer, stream.stream_id);
        }
    }

    pub fn set_gain(&self, peer: PeerId, gain: f32) {
        if let Some(stream) = self.state.lock().remote.get(&peer) {
            stream.set_gain(gain);
        }
    }

    /// Starts capturing and sharing. Blocks (off the UI thread) while a
    /// system picker is open.
    pub fn start_share(
        &self,
        request: VideoRequest,
        audio: AudioMode,
        exclude: Vec<String>,
        config: ShareConfig,
    ) -> impl Future<Output = Result<ShareStarted, String>> + use<> {
        let state = self.state.clone();
        let ui_tx = self.ui_tx.clone();
        let rt = self.rt.handle().clone();
        spawn_result(&self.rt, async move {
            tokio::task::spawn_blocking(move || {
                let session = state.lock().session.clone().ok_or("Not in a session.")?;
                state.lock().share = None;
                let capture = screenshare_capture::start_video(request, config.max_fps)
                    .map_err(|err| format!("{err:#}"))?;
                let kind = match capture.kind {
                    SourceKind::Monitor => ShareKind::Monitor,
                    SourceKind::Window => ShareKind::Window,
                };
                let title = capture.title.clone();
                let target = match (audio, capture.kind) {
                    (AudioMode::Off, _) => None,
                    (AudioMode::Auto, SourceKind::Monitor) => {
                        Some(AudioTarget::Desktop { exclude })
                    }
                    (AudioMode::Auto, SourceKind::Window) => capture.pid.map(|pid| {
                        AudioTarget::App(AudioApp {
                            name: title.clone(),
                            pid: Some(pid),
                            binary: None,
                        })
                    }),
                };
                let needs_audio_app = audio == AudioMode::Auto && target.is_none();
                let has_audio = audio == AudioMode::Auto;
                let handle = {
                    let _guard = rt.enter();
                    session.start_share(
                        kind,
                        title.clone(),
                        capture.width,
                        capture.height,
                        has_audio,
                        config.bitrates(),
                    )
                };
                let share = ActiveShare::start(capture, handle, config, move || {
                    let _ = ui_tx.send(UiEvent::ShareEnded);
                });
                let audio_error = share
                    .set_audio(target)
                    .err()
                    .map(|err| format!("Audio capture failed: {err:#}"));
                state.lock().share = Some(share);
                Ok(ShareStarted {
                    kind,
                    title,
                    needs_audio_app,
                    audio_error,
                })
            })
            .await
            .map_err(|err| err.to_string())?
        })
    }

    /// Picks the app whose audio accompanies a window share (portal flow).
    pub fn set_share_audio(
        &self,
        app: Option<AudioApp>,
    ) -> impl Future<Output = Result<(), String>> + use<> {
        let state = self.state.clone();
        spawn_result(&self.rt, async move {
            tokio::task::spawn_blocking(move || {
                let s = state.lock();
                match &s.share {
                    Some(share) => share
                        .set_audio(app.map(AudioTarget::App))
                        .map_err(|err| format!("Audio capture failed: {err:#}")),
                    None => Ok(()),
                }
            })
            .await
            .map_err(|err| err.to_string())?
        })
    }

    pub fn stop_share(&self) {
        let share = self.state.lock().share.take();
        drop(share);
    }

    /// Runs blocking work (source enumeration, thumbnails) off the UI thread.
    pub fn blocking<T, F>(&self, f: F) -> impl Future<Output = Option<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let task = self.rt.spawn_blocking(f);
        async move { task.await.ok() }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let session = {
            let mut s = self.state.lock();
            s.share = None;
            s.remote.clear();
            s.session.take()
        };
        if let Some(session) = session {
            let _ = self.rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(1), session.leave()).await
            });
        }
    }
}

async fn pump(
    mut events: mpsc::UnboundedReceiver<NetEvent>,
    state: Arc<Mutex<State>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    generation: u64,
) {
    while let Some(event) = events.recv().await {
        let ui = {
            let s = state.lock();
            if s.generation != generation {
                return;
            }
            match event {
                NetEvent::Video { peer, packet } => {
                    if let Some(stream) = s.remote.get(&peer)
                        && stream.stream_id == packet.header.stream_id
                    {
                        stream.push_video(packet);
                    }
                    None
                }
                NetEvent::Audio { peer, packet } => {
                    if let Some(stream) = s.remote.get(&peer)
                        && stream.stream_id == packet.stream_id
                    {
                        stream.push_audio(packet);
                    }
                    None
                }
                NetEvent::Connected => Some(UiEvent::Connected),
                NetEvent::Disconnected => Some(UiEvent::Disconnected),
                NetEvent::PeerUpdated {
                    peer,
                    nickname,
                    share,
                } => Some(UiEvent::PeerUpdated {
                    peer,
                    nickname,
                    share,
                }),
                NetEvent::PeerLeft { peer } => Some(UiEvent::PeerLeft { peer }),
                NetEvent::ViewerClosed { peer } => {
                    if let Some(session) = &s.session {
                        session.drop_viewer(peer);
                    }
                    if let Some(stream) = s.remote.get(&peer) {
                        tracing::info!("media connection to {peer} closed, retrying");
                        tokio::spawn(resubscribe(
                            state.clone(),
                            peer,
                            stream.stream_id,
                            generation,
                        ));
                    }
                    None
                }
            }
        };
        if let Some(ui) = ui {
            let _ = ui_tx.send(ui);
        }
    }
}

async fn resubscribe(state: Arc<Mutex<State>>, peer: PeerId, stream_id: u64, generation: u64) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    let s = state.lock();
    if s.generation == generation
        && s.remote
            .get(&peer)
            .is_some_and(|r| r.stream_id == stream_id)
        && let Some(session) = &s.session
    {
        session.subscribe(peer, stream_id);
    }
}
