#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
mod receive;
mod settings;
mod share;
mod theme;
mod video_widget;
mod view;

use engine::{AudioMode, Engine, SessionInfo, ShareStarted, UiEvent};
use iced::widget::image;
use iced::{Subscription, Task, keyboard, window};
use screenshare_capture::{AudioApp, PickerKind, Source, VideoRequest};
use screenshare_net::{PeerId, ShareInfo, ShareKind};
use settings::{Settings, ThemeChoice};
use share::ShareConfig;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use video_widget::FrameSlot;

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn,iroh_gossip=warn,wgpu=warn,naga=warn".into()),
        )
        .init();

    iced::application(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .window(window_settings())
        .run()
}

fn window_settings() -> window::Settings {
    window::Settings {
        size: iced::Size::new(1100.0, 720.0),
        min_size: Some(iced::Size::new(760.0, 480.0)),
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "io.github.screenshareapp.ScreenShare".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub struct Peer {
    pub nickname: String,
    pub share: Option<ShareInfo>,
}

pub struct Watch {
    pub peer: PeerId,
    pub stream_id: u64,
    pub slot: Arc<FrameSlot>,
    pub volume: f32,
    pub muted: bool,
}

pub enum MyShare {
    Idle,
    Starting,
    Live { kind: ShareKind, title: String },
}

pub struct PickerSource {
    pub source: Source,
    pub thumbnail: Option<image::Handle>,
}

pub enum Dialog {
    /// In-app source picker (Windows, X11). `None` while loading.
    Sources {
        sources: Option<Vec<PickerSource>>,
        selected: Option<usize>,
    },
    /// Which app's audio goes with a portal-picked window. `None` while loading.
    AudioApp {
        apps: Option<Vec<AudioApp>>,
        selected: Option<usize>,
    },
    Settings,
}

pub struct SessionView {
    pub me: PeerId,
    pub connected: bool,
    pub peers: BTreeMap<PeerId, Peer>,
    pub watching: Vec<Watch>,
    /// Streams the user closed; not reopened automatically.
    pub declined: HashSet<u64>,
    pub focus: Option<PeerId>,
    pub my_share: MyShare,
    pub share_audio: bool,
    pub copied: bool,
}

pub struct App {
    engine: Engine,
    pub settings: Settings,
    pub exclude_input: String,
    pub ticket_input: String,
    pub busy: bool,
    pub error: Option<String>,
    pub dialog: Option<Dialog>,
    pub session: Option<SessionView>,
    pub fullscreen: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    NicknameChanged(String),
    TicketChanged(String),
    Create,
    Join,
    Connected(Result<SessionInfo, String>),
    ToggleTheme,
    SetTheme(ThemeChoice),
    OpenSettings,
    CloseDialog,
    ExcludeChanged(String),
    SetFps(u32),
    SetMaxHeight(u32),
    SetBitrate(u32),
    Engine(UiEvent),
    CopyInvite,
    ClearCopied,
    Leave,
    Watch(PeerId),
    Unwatch(PeerId),
    Volume(PeerId, f32),
    ToggleMute(PeerId),
    Focus(Option<PeerId>),
    ToggleFullscreen,
    Escape,
    Tick,
    ShareAudioToggled(bool),
    ShareClicked,
    SourcesLoaded(Vec<PickerSource>),
    PickSource(usize),
    ConfirmSource,
    ShareResult(Result<ShareStarted, String>),
    StopShare,
    AudioAppsLoaded(Vec<AudioApp>),
    PickAudioApp(Option<usize>),
    ConfirmAudioApp,
    AudioSet(Result<(), String>),
    DismissError,
}

impl std::fmt::Debug for PickerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PickerSource")
            .field("source", &self.source)
            .finish()
    }
}

impl Clone for PickerSource {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            thumbnail: self.thumbnail.clone(),
        }
    }
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let settings = Settings::load();
        let app = Self {
            engine: Engine::new(),
            exclude_input: settings.exclude_apps.join(", "),
            settings,
            ticket_input: String::new(),
            busy: false,
            error: None,
            dialog: None,
            session: None,
            fullscreen: false,
        };
        (app, Task::none())
    }

    fn title(&self) -> String {
        match &self.session {
            Some(s) => format!("screenshare · {} in session", s.peers.len() + 1),
            None => "screenshare".into(),
        }
    }

    fn theme(&self) -> Option<iced::Theme> {
        Some(theme::theme(self.settings.theme))
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run(engine::events).map(Message::Engine),
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                } => Some(Message::Escape),
                _ => None,
            }),
        ];
        if self
            .session
            .as_ref()
            .is_some_and(|s| !s.watching.is_empty())
        {
            subs.push(iced::time::every(Duration::from_millis(16)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn share_config(&self) -> ShareConfig {
        ShareConfig {
            max_fps: self.settings.max_fps,
            max_size: self.settings.max_size(),
            max_bitrate: self.settings.max_bitrate_kbps * 1000,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NicknameChanged(name) => {
                self.settings.nickname = name.chars().take(32).collect();
            }
            Message::TicketChanged(ticket) => self.ticket_input = ticket,
            Message::Create | Message::Join => {
                let nickname = self.settings.nickname.trim().to_string();
                if nickname.is_empty() {
                    self.error = Some("Pick a nickname first.".into());
                    return Task::none();
                }
                let ticket =
                    matches!(message, Message::Join).then(|| self.ticket_input.trim().to_string());
                if ticket.as_ref().is_some_and(|t| t.is_empty()) {
                    self.error = Some("Paste an invite code to join.".into());
                    return Task::none();
                }
                self.settings.save();
                self.busy = true;
                self.error = None;
                return Task::perform(self.engine.connect(ticket, nickname), Message::Connected);
            }
            Message::Connected(result) => {
                self.busy = false;
                match result {
                    Ok(info) => {
                        self.ticket_input.clear();
                        self.session = Some(SessionView {
                            me: info.id,
                            connected: false,
                            peers: BTreeMap::new(),
                            watching: Vec::new(),
                            declined: HashSet::new(),
                            focus: None,
                            my_share: MyShare::Idle,
                            share_audio: true,
                            copied: false,
                        });
                    }
                    Err(err) => self.error = Some(err),
                }
            }
            Message::ToggleTheme => {
                self.settings.theme = match self.settings.theme {
                    ThemeChoice::Light => ThemeChoice::Dark,
                    ThemeChoice::Dark => ThemeChoice::Light,
                };
                self.settings.save();
            }
            Message::SetTheme(choice) => {
                self.settings.theme = choice;
                self.settings.save();
            }
            Message::OpenSettings => {
                self.exclude_input = self.settings.exclude_apps.join(", ");
                self.dialog = Some(Dialog::Settings);
            }
            Message::CloseDialog => {
                if matches!(self.dialog, Some(Dialog::Settings)) {
                    self.settings.save();
                }
                self.dialog = None;
            }
            Message::ExcludeChanged(text) => {
                self.settings.exclude_apps = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                self.exclude_input = text;
            }
            Message::SetFps(fps) => self.settings.max_fps = fps,
            Message::SetMaxHeight(h) => self.settings.max_height = h,
            Message::SetBitrate(kbps) => self.settings.max_bitrate_kbps = kbps,
            Message::Engine(event) => return self.on_engine(event),
            Message::CopyInvite => {
                if let (Some(ticket), Some(session)) = (self.engine.ticket(), &mut self.session) {
                    session.copied = true;
                    return Task::batch([
                        iced::clipboard::write(ticket),
                        Task::perform(tokio::time::sleep(Duration::from_secs(2)), |_| {
                            Message::ClearCopied
                        }),
                    ]);
                }
            }
            Message::ClearCopied => {
                if let Some(session) = &mut self.session {
                    session.copied = false;
                }
            }
            Message::Leave => {
                self.engine.leave();
                self.session = None;
                self.dialog = None;
                if self.fullscreen {
                    return self.set_fullscreen(false);
                }
            }
            Message::Watch(peer) => self.watch(peer),
            Message::Unwatch(peer) => {
                if let Some(session) = &mut self.session {
                    if let Some(w) = session.watching.iter().find(|w| w.peer == peer) {
                        session.declined.insert(w.stream_id);
                    }
                    session.watching.retain(|w| w.peer != peer);
                    if session.focus == Some(peer) {
                        session.focus = None;
                    }
                }
                self.engine.unwatch(peer);
            }
            Message::Volume(peer, volume) => {
                if let Some(w) = self.watch_mut(peer) {
                    w.volume = volume;
                    w.muted = false;
                    self.engine.set_gain(peer, volume);
                }
            }
            Message::ToggleMute(peer) => {
                if let Some(w) = self.watch_mut(peer) {
                    w.muted = !w.muted;
                    let gain = if w.muted { 0.0 } else { w.volume };
                    self.engine.set_gain(peer, gain);
                }
            }
            Message::Focus(peer) => {
                if let Some(session) = &mut self.session {
                    session.focus = peer;
                }
                if peer.is_none() && self.fullscreen {
                    return self.set_fullscreen(false);
                }
            }
            Message::ToggleFullscreen => return self.set_fullscreen(!self.fullscreen),
            Message::Escape => {
                if self.dialog.is_some() {
                    return self.update(Message::CloseDialog);
                }
                if self.fullscreen {
                    return self.set_fullscreen(false);
                }
                if let Some(session) = &mut self.session {
                    session.focus = None;
                }
            }
            Message::Tick => {}
            Message::ShareAudioToggled(on) => {
                if let Some(session) = &mut self.session {
                    session.share_audio = on;
                }
            }
            Message::ShareClicked => return self.share_clicked(),
            Message::SourcesLoaded(sources) => {
                if let Some(Dialog::Sources {
                    sources: slot,
                    selected,
                }) = &mut self.dialog
                {
                    *selected = (sources.len() == 1).then_some(0);
                    *slot = Some(sources);
                }
            }
            Message::PickSource(i) => {
                if let Some(Dialog::Sources { selected, .. }) = &mut self.dialog {
                    *selected = Some(i);
                }
            }
            Message::ConfirmSource => {
                let source = match &self.dialog {
                    Some(Dialog::Sources {
                        sources: Some(sources),
                        selected: Some(i),
                    }) => sources.get(*i).map(|s| s.source.clone()),
                    _ => None,
                };
                if let Some(source) = source {
                    self.dialog = None;
                    return self.start_share(VideoRequest::Source(source));
                }
            }
            Message::ShareResult(result) => {
                let Some(session) = &mut self.session else {
                    self.engine.stop_share();
                    return Task::none();
                };
                match result {
                    Ok(started) => {
                        session.my_share = MyShare::Live {
                            kind: started.kind,
                            title: started.title,
                        };
                        if let Some(err) = started.audio_error {
                            self.error = Some(err);
                        }
                        if started.needs_audio_app {
                            self.dialog = Some(Dialog::AudioApp {
                                apps: None,
                                selected: None,
                            });
                            return Task::perform(
                                self.engine.blocking(screenshare_capture::list_audio_apps),
                                |apps| Message::AudioAppsLoaded(apps.unwrap_or_default()),
                            );
                        }
                    }
                    Err(err) => {
                        session.my_share = MyShare::Idle;
                        self.error = Some(format!("Could not start sharing: {err}"));
                    }
                }
            }
            Message::StopShare => {
                self.engine.stop_share();
                if let Some(session) = &mut self.session {
                    session.my_share = MyShare::Idle;
                }
                if matches!(self.dialog, Some(Dialog::AudioApp { .. })) {
                    self.dialog = None;
                }
            }
            Message::AudioAppsLoaded(apps) => {
                if let Some(Dialog::AudioApp {
                    apps: slot,
                    selected,
                }) = &mut self.dialog
                {
                    *selected = (apps.len() == 1).then_some(0);
                    *slot = Some(apps);
                }
            }
            Message::PickAudioApp(i) => {
                if let Some(Dialog::AudioApp { selected, .. }) = &mut self.dialog {
                    *selected = i;
                }
            }
            Message::ConfirmAudioApp => {
                let app = match self.dialog.take() {
                    Some(Dialog::AudioApp {
                        apps: Some(apps),
                        selected: Some(i),
                    }) => apps.get(i).cloned(),
                    _ => None,
                };
                if app.is_some() {
                    return Task::perform(self.engine.set_share_audio(app), Message::AudioSet);
                }
            }
            Message::AudioSet(result) => {
                if let Err(err) = result {
                    self.error = Some(err);
                }
            }
            Message::DismissError => self.error = None,
        }
        Task::none()
    }

    fn on_engine(&mut self, event: UiEvent) -> Task<Message> {
        if let UiEvent::ShareEnded = event {
            return self.update(Message::StopShare);
        }
        let Some(session) = &mut self.session else {
            return Task::none();
        };
        match event {
            UiEvent::Connected => session.connected = true,
            UiEvent::Disconnected => session.connected = false,
            UiEvent::PeerUpdated {
                peer,
                nickname,
                share,
            } => {
                let new_stream = share.as_ref().map(|s| s.stream_id);
                session.peers.insert(peer, Peer { nickname, share });
                let watched = session
                    .watching
                    .iter()
                    .find(|w| w.peer == peer)
                    .map(|w| w.stream_id);
                match (watched, new_stream) {
                    (Some(old), Some(new)) if old != new => self.watch(peer),
                    (Some(_), None) => {
                        session.watching.retain(|w| w.peer != peer);
                        if session.focus == Some(peer) {
                            session.focus = None;
                        }
                        self.engine.unwatch(peer);
                    }
                    (None, Some(new)) if !session.declined.contains(&new) => self.watch(peer),
                    _ => {}
                }
            }
            UiEvent::PeerLeft { peer } => {
                session.peers.remove(&peer);
                session.watching.retain(|w| w.peer != peer);
                if session.focus == Some(peer) {
                    session.focus = None;
                }
                self.engine.unwatch(peer);
            }
            UiEvent::ShareEnded => {}
        }
        Task::none()
    }

    fn watch(&mut self, peer: PeerId) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(stream_id) = session
            .peers
            .get(&peer)
            .and_then(|p| p.share.as_ref())
            .map(|s| s.stream_id)
        else {
            return;
        };
        session.declined.remove(&stream_id);
        let Some(slot) = self.engine.watch(peer, stream_id) else {
            return;
        };
        if let Some(w) = session.watching.iter_mut().find(|w| w.peer == peer) {
            w.stream_id = stream_id;
            w.slot = slot;
            self.engine
                .set_gain(peer, if w.muted { 0.0 } else { w.volume });
        } else {
            session.watching.push(Watch {
                peer,
                stream_id,
                slot,
                volume: 1.0,
                muted: false,
            });
        }
    }

    fn watch_mut(&mut self, peer: PeerId) -> Option<&mut Watch> {
        self.session
            .as_mut()?
            .watching
            .iter_mut()
            .find(|w| w.peer == peer)
    }

    fn set_fullscreen(&mut self, on: bool) -> Task<Message> {
        self.fullscreen = on;
        let mode = if on {
            window::Mode::Fullscreen
        } else {
            window::Mode::Windowed
        };
        window::latest().and_then(move |id| window::set_mode(id, mode))
    }

    fn share_clicked(&mut self) -> Task<Message> {
        if self
            .session
            .as_ref()
            .is_none_or(|s| !matches!(s.my_share, MyShare::Idle))
        {
            return Task::none();
        }
        match screenshare_capture::picker_kind() {
            PickerKind::Portal => self.start_share(VideoRequest::Portal),
            PickerKind::InApp => {
                self.dialog = Some(Dialog::Sources {
                    sources: None,
                    selected: None,
                });
                Task::perform(self.engine.blocking(load_sources), |sources| {
                    Message::SourcesLoaded(sources.unwrap_or_default())
                })
            }
        }
    }

    fn start_share(&mut self, request: VideoRequest) -> Task<Message> {
        let Some(session) = &mut self.session else {
            return Task::none();
        };
        session.my_share = MyShare::Starting;
        let audio = if session.share_audio {
            AudioMode::Auto
        } else {
            AudioMode::Off
        };
        Task::perform(
            self.engine.start_share(
                request,
                audio,
                self.settings.exclude_apps.clone(),
                self.share_config(),
            ),
            Message::ShareResult,
        )
    }
}

fn load_sources() -> Vec<PickerSource> {
    let sources = match screenshare_capture::list_sources() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("listing sources failed: {err:#}");
            return Vec::new();
        }
    };
    sources
        .into_iter()
        .map(|source| {
            let thumbnail = screenshare_capture::thumbnail(&source, 320)
                .map(|t| image::Handle::from_rgba(t.width, t.height, t.rgba));
            PickerSource { source, thumbnail }
        })
        .collect()
}
