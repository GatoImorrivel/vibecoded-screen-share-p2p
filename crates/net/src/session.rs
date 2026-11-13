use crate::media::{MediaProtocol, ShareHandle, ShareRegistry, Viewer};
use crate::protocol::{
    AudioPacket, ControlMsg, GossipMsg, MEDIA_ALPN, Presence, ShareDetails, ShareInfo, VideoPacket,
    sign_gossip, verify_gossip,
};
use crate::ticket::Ticket;
use anyhow::{Context, Result};
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId as PeerId, SecretKey};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::{GOSSIP_ALPN, Gossip};
use iroh_gossip::proto::TopicId;
use n0_future::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const HEARTBEAT: Duration = Duration::from_secs(3);
const PEER_TIMEOUT: Duration = Duration::from_secs(12);
const REJOIN_INTERVAL: Duration = Duration::from_secs(10);

/// Events emitted by a [`Session`].
#[derive(Debug)]
pub enum NetEvent {
    /// We have at least one direct gossip neighbor.
    Connected,
    /// We lost all gossip neighbors and are trying to reconnect.
    Disconnected,
    PeerUpdated {
        peer: PeerId,
        nickname: String,
        share: Option<ShareInfo>,
    },
    PeerLeft {
        peer: PeerId,
    },
    Video {
        peer: PeerId,
        packet: VideoPacket,
    },
    Audio {
        peer: PeerId,
        packet: AudioPacket,
    },
    /// The media connection to a sharer we were watching closed.
    ViewerClosed {
        peer: PeerId,
    },
}

struct PeerState {
    nickname: String,
    share: Option<ShareInfo>,
    addr: EndpointAddr,
    last_seen: Instant,
}

struct State {
    nickname: String,
    share: Option<ShareInfo>,
    peers: HashMap<PeerId, PeerState>,
    neighbors: usize,
    bootstrap: Vec<PeerId>,
    last_rejoin: Instant,
}

struct Inner {
    endpoint: Endpoint,
    router: Router,
    memory: MemoryLookup,
    topic: TopicId,
    sender: GossipSender,
    secret: SecretKey,
    state: Mutex<State>,
    registry: Arc<ShareRegistry>,
    viewers: Mutex<HashMap<PeerId, Viewer>>,
    events: mpsc::UnboundedSender<NetEvent>,
    runtime: tokio::runtime::Handle,
    tasks: Mutex<JoinSet<()>>,
}

/// A joined screen-sharing session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Creates a brand new session that others can join with [`Session::ticket`].
    pub async fn create(nickname: String) -> Result<(Self, mpsc::UnboundedReceiver<NetEvent>)> {
        Self::start(nickname, Ticket::new_topic(), Vec::new()).await
    }

    /// Joins the session described by `ticket`.
    pub async fn join(
        ticket: &Ticket,
        nickname: String,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NetEvent>)> {
        Self::start(nickname, ticket.topic, ticket.peers.clone()).await
    }

    async fn start(
        nickname: String,
        topic: TopicId,
        bootstrap: Vec<EndpointAddr>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NetEvent>)> {
        let secret = SecretKey::generate();
        let memory = MemoryLookup::new();
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret.clone())
            .address_lookup(memory.clone())
            .bind()
            .await
            .context("binding network endpoint")?;
        // Wait briefly for a home relay so the ticket carries a relay address;
        // on a LAN without internet this times out and direct addresses are used.
        let _ = tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await;

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let registry = Arc::new(ShareRegistry::default());
        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(
                MEDIA_ALPN,
                MediaProtocol {
                    registry: registry.clone(),
                },
            )
            .spawn();

        let own_id = endpoint.id();
        let bootstrap_ids: Vec<PeerId> = bootstrap
            .iter()
            .map(|a| a.id)
            .filter(|id| *id != own_id)
            .collect();
        for addr in bootstrap {
            memory.add_endpoint_info(addr);
        }
        let (sender, receiver) = gossip
            .subscribe(topic, bootstrap_ids.clone())
            .await
            .context("joining gossip topic")?
            .split();

        let (events, events_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            endpoint,
            router,
            memory,
            topic,
            sender,
            secret,
            state: Mutex::new(State {
                nickname,
                share: None,
                peers: HashMap::new(),
                neighbors: 0,
                bootstrap: bootstrap_ids,
                last_rejoin: Instant::now(),
            }),
            registry,
            viewers: Mutex::new(HashMap::new()),
            events,
            runtime: tokio::runtime::Handle::current(),
            tasks: Mutex::new(JoinSet::new()),
        });

        {
            let mut tasks = inner.tasks.lock().unwrap();
            tasks.spawn(gossip_loop(Arc::downgrade(&inner), receiver));
            tasks.spawn(heartbeat_loop(Arc::downgrade(&inner)));
        }
        let session = Self { inner };
        session.announce();
        Ok((session, events_rx))
    }

    pub fn id(&self) -> PeerId {
        self.inner.endpoint.id()
    }

    /// Invite code containing the topic, our address and a few known members.
    pub fn ticket(&self) -> String {
        let mut peers = vec![self.inner.endpoint.addr()];
        let state = self.inner.state.lock().unwrap();
        peers.extend(state.peers.values().take(3).map(|p| p.addr.clone()));
        Ticket {
            topic: self.inner.topic,
            peers,
        }
        .to_string()
    }

    pub fn set_nickname(&self, nickname: String) {
        self.inner.state.lock().unwrap().nickname = nickname;
        self.announce();
    }

    /// Starts sharing. The share stays announced until the handle is dropped.
    ///
    /// `bitrate` is `(initial, min, max)` in bits per second.
    pub fn start_share(&self, details: ShareDetails, bitrate: (u32, u32, u32)) -> ShareHandle {
        let stream_id: u64 = rand::random();
        let ShareDetails {
            kind,
            title,
            width,
            height,
            has_audio,
            codec,
        } = details;
        self.inner.state.lock().unwrap().share = Some(ShareInfo {
            stream_id,
            kind,
            title,
            width,
            height,
            has_audio,
            codec,
        });
        let weak = Arc::downgrade(&self.inner);
        let handle = ShareHandle::new(
            self.inner.registry.clone(),
            stream_id,
            bitrate,
            Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let mut state = inner.state.lock().unwrap();
                    if state
                        .share
                        .as_ref()
                        .is_some_and(|s| s.stream_id == stream_id)
                    {
                        state.share = None;
                    }
                    drop(state);
                    Session { inner }.announce();
                }
            }),
        );
        self.announce();
        handle
    }

    /// Starts receiving `stream_id` from `peer`, dialing it if needed.
    pub fn subscribe(&self, peer: PeerId, stream_id: u64) {
        let mut viewers = self.inner.viewers.lock().unwrap();
        let viewer = viewers.entry(peer).or_insert_with(|| {
            let _guard = self.inner.runtime.enter();
            Viewer::spawn(self.inner.endpoint.clone(), peer, self.inner.events.clone())
        });
        if viewer.subscribed.insert(stream_id) {
            viewer.send(ControlMsg::Subscribe { stream_id });
        }
    }

    pub fn unsubscribe(&self, peer: PeerId, stream_id: u64) {
        let mut viewers = self.inner.viewers.lock().unwrap();
        if let Some(viewer) = viewers.get_mut(&peer) {
            viewer.subscribed.remove(&stream_id);
            viewer.send(ControlMsg::Unsubscribe { stream_id });
            if viewer.subscribed.is_empty() {
                viewers.remove(&peer);
            }
        }
    }

    /// Forgets the connection to `peer` after it closed, so the next
    /// [`Session::subscribe`] dials again.
    pub fn drop_viewer(&self, peer: PeerId) {
        self.inner.viewers.lock().unwrap().remove(&peer);
    }

    pub fn request_keyframe(&self, peer: PeerId, stream_id: u64) {
        if let Some(viewer) = self.inner.viewers.lock().unwrap().get(&peer) {
            viewer.send(ControlMsg::RequestKeyframe { stream_id });
        }
    }

    /// Broadcasts our presence now.
    fn announce(&self) {
        let inner = self.inner.clone();
        self.inner.runtime.spawn(async move {
            broadcast_presence(&inner).await;
        });
    }

    /// Announces departure and shuts the endpoint down.
    pub async fn leave(self) {
        let inner = self.inner;
        if let Ok(msg) = sign_gossip(&inner.secret, GossipMsg::Leave) {
            let _ =
                tokio::time::timeout(Duration::from_millis(500), inner.sender.broadcast(msg)).await;
        }
        inner.viewers.lock().unwrap().clear();
        inner.tasks.lock().unwrap().abort_all();
        let _ = tokio::time::timeout(Duration::from_secs(2), inner.router.shutdown()).await;
    }
}

async fn broadcast_presence(inner: &Inner) {
    let presence = {
        let state = inner.state.lock().unwrap();
        Presence {
            nickname: state.nickname.clone(),
            share: state.share.clone(),
            addr: inner.endpoint.addr(),
        }
    };
    match sign_gossip(&inner.secret, GossipMsg::Presence(presence)) {
        Ok(msg) => {
            if let Err(err) = inner.sender.broadcast(msg).await {
                tracing::debug!("presence broadcast failed: {err}");
            }
        }
        Err(err) => tracing::warn!("failed to sign presence: {err:#}"),
    }
}

async fn gossip_loop(inner: Weak<Inner>, mut receiver: GossipReceiver) {
    while let Some(event) = receiver.next().await {
        let Some(inner) = inner.upgrade() else { return };
        let event = match event {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("gossip receive error: {err}");
                continue;
            }
        };
        match event {
            Event::NeighborUp(_) => {
                let first = {
                    let mut state = inner.state.lock().unwrap();
                    state.neighbors += 1;
                    state.neighbors == 1
                };
                if first {
                    let _ = inner.events.send(NetEvent::Connected);
                }
                broadcast_presence(&inner).await;
            }
            Event::NeighborDown(_) => {
                let none_left = {
                    let mut state = inner.state.lock().unwrap();
                    state.neighbors = state.neighbors.saturating_sub(1);
                    state.neighbors == 0
                };
                if none_left {
                    let _ = inner.events.send(NetEvent::Disconnected);
                }
            }
            Event::Received(msg) => {
                let (from, msg) = match verify_gossip(&msg.content) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!("ignoring invalid gossip message: {err:#}");
                        continue;
                    }
                };
                if from == inner.endpoint.id() {
                    continue;
                }
                handle_gossip(&inner, from, msg).await;
            }
            Event::Lagged => tracing::warn!("gossip receiver lagged"),
            _ => {}
        }
    }
}

async fn handle_gossip(inner: &Arc<Inner>, from: PeerId, msg: GossipMsg) {
    match msg {
        GossipMsg::Presence(presence) => {
            if presence.addr.id != from {
                return;
            }
            inner.memory.add_endpoint_info(presence.addr.clone());
            let nickname = sanitize_nickname(&presence.nickname);
            let (is_new, changed) = {
                let mut state = inner.state.lock().unwrap();
                let entry = state.peers.get_mut(&from);
                match entry {
                    Some(peer) => {
                        let changed = peer.nickname != nickname || peer.share != presence.share;
                        peer.nickname = nickname.clone();
                        peer.share = presence.share.clone();
                        peer.addr = presence.addr;
                        peer.last_seen = Instant::now();
                        (false, changed)
                    }
                    None => {
                        state.peers.insert(
                            from,
                            PeerState {
                                nickname: nickname.clone(),
                                share: presence.share.clone(),
                                addr: presence.addr,
                                last_seen: Instant::now(),
                            },
                        );
                        (true, true)
                    }
                }
            };
            if changed {
                let _ = inner.events.send(NetEvent::PeerUpdated {
                    peer: from,
                    nickname,
                    share: presence.share,
                });
            }
            // A newcomer missed our earlier announcements.
            if is_new {
                broadcast_presence(inner).await;
            }
        }
        GossipMsg::Leave => {
            remove_peer(inner, from);
        }
    }
}

fn remove_peer(inner: &Inner, peer: PeerId) {
    let removed = inner.state.lock().unwrap().peers.remove(&peer).is_some();
    if removed {
        inner.viewers.lock().unwrap().remove(&peer);
        let _ = inner.events.send(NetEvent::PeerLeft { peer });
    }
}

async fn heartbeat_loop(inner: Weak<Inner>) {
    let mut interval = tokio::time::interval(HEARTBEAT);
    loop {
        interval.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        broadcast_presence(&inner).await;

        let now = Instant::now();
        let (stale, rejoin) = {
            let mut state = inner.state.lock().unwrap();
            let stale: Vec<PeerId> = state
                .peers
                .iter()
                .filter(|(_, p)| now.duration_since(p.last_seen) > PEER_TIMEOUT)
                .map(|(id, _)| *id)
                .collect();
            let rejoin = if state.neighbors == 0
                && now.duration_since(state.last_rejoin) > REJOIN_INTERVAL
            {
                state.last_rejoin = now;
                let mut ids: Vec<PeerId> = state.peers.keys().copied().collect();
                ids.extend(state.bootstrap.iter().copied());
                ids.sort();
                ids.dedup();
                ids
            } else {
                Vec::new()
            };
            (stale, rejoin)
        };
        for peer in stale {
            tracing::info!("peer {} timed out", peer.fmt_short());
            remove_peer(&inner, peer);
        }
        if !rejoin.is_empty() {
            tracing::info!("no gossip neighbors, rejoining via {} peers", rejoin.len());
            let _ = inner.sender.join_peers(rejoin).await;
        }
    }
}

fn sanitize_nickname(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "anonymous".to_string()
    } else {
        trimmed.to_string()
    }
}
