//! Application audio capture on PipeWire.
//!
//! We create one unconnected capture stream and link the output ports of every
//! matching application playback stream into it; PipeWire mixes the links.
//! Our own playback is never linked, so remote streams are not re-broadcast.

use crate::mixer::{SourceMixer, SourceWriter};
use crate::{AudioApp, AudioCapture, AudioTarget};
use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

const PLAYBACK_CLASS: &str = "Stream/Output/Audio";

fn app_from_props(props: &spa::utils::dict::DictRef) -> AudioApp {
    let pid = props
        .get("application.process.id")
        .and_then(|p| p.parse().ok());
    let binary = props.get("application.process.binary").map(str::to_string);
    let name = props
        .get("application.name")
        .or_else(|| props.get("node.description"))
        .or_else(|| props.get("node.name"))
        .map(str::to_string)
        .or_else(|| binary.clone())
        .unwrap_or_else(|| "Unknown".to_string());
    AudioApp { name, pid, binary }
}

type BoundNode = (pw::node::Node, pw::node::NodeListener);

/// Binds a playback node to receive its full info properties; registry
/// globals omit the process id and binary.
fn bind_playback(
    registry: &pw::registry::Registry,
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    on_info: impl Fn(AudioApp) + 'static,
) -> Option<BoundNode> {
    let node: pw::node::Node = registry.bind(obj).ok()?;
    let listener = node
        .add_listener_local()
        .info(move |info| {
            if let Some(props) = info.props() {
                on_info(app_from_props(props));
            }
        })
        .register();
    Some((node, listener))
}

/// Lists applications with a playback stream.
pub(crate) fn list_apps() -> Result<Vec<AudioApp>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let registry = core.get_registry_rc()?;

    let apps: Rc<RefCell<HashMap<u32, AudioApp>>> = Rc::default();
    let bound: Rc<RefCell<Vec<BoundNode>>> = Rc::default();

    // Two roundtrips: the first delivers the globals, the second the info of
    // the nodes bound in response.
    let pending = Rc::new(RefCell::new(Some(core.sync(0)?)));
    let ml = mainloop.clone();
    let core_weak = core.downgrade();
    let first_done = Rc::new(std::cell::Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE || *pending.borrow() != Some(seq) {
                return;
            }
            if first_done.replace(true) {
                ml.quit();
            } else if let Some(core) = core_weak.upgrade() {
                *pending.borrow_mut() = core.sync(0).ok();
            }
        })
        .register();

    let reg_weak = registry.downgrade();
    let found = apps.clone();
    let keep = bound.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some(PLAYBACK_CLASS) {
                return;
            }
            let Some(registry) = reg_weak.upgrade() else {
                return;
            };
            let id = obj.id;
            found.borrow_mut().insert(id, app_from_props(props));
            let found = found.clone();
            if let Some(b) = bind_playback(&registry, obj, move |app| {
                found.borrow_mut().insert(id, app);
            }) {
                keep.borrow_mut().push(b);
            }
        })
        .register();

    let ml = mainloop.clone();
    let timeout = mainloop.loop_().add_timer(move |_| ml.quit());
    timeout.update_timer(Some(Duration::from_secs(2)), None);
    mainloop.run();
    drop(bound);
    Ok(apps.take().into_values().collect())
}

struct NodeInfo {
    app: AudioApp,
    is_playback: bool,
    /// Full info props received; playback nodes are only linked once resolved.
    resolved: bool,
}

struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
}

struct Graph {
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,
    our_node_name: String,
    our_node: Option<u32>,
    links: HashMap<(u32, u32), pw::link::Link>,
    bound: HashMap<u32, BoundNode>,
    filter: Box<dyn Fn(&AudioApp) -> bool>,
}

impl Graph {
    fn reconcile(&mut self, core: &pw::core::Core) {
        tracing::trace!(
            our = ?self.our_node,
            nodes = self.nodes.len(),
            ports = self.ports.len(),
            "reconciling audio links"
        );
        let Some(our) = self.our_node else { return };
        let inputs: HashMap<&str, u32> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == our && !p.output)
            .map(|(id, p)| (p.channel.as_str(), *id))
            .collect();
        let (Some(&fl), Some(&fr)) = (inputs.get("FL"), inputs.get("FR")) else {
            return;
        };
        let mut wanted = Vec::new();
        for (port_id, port) in &self.ports {
            if !port.output {
                continue;
            }
            let Some(node) = self.nodes.get(&port.node) else {
                continue;
            };
            if !node.is_playback
                || !node.resolved
                || crate::is_self(&node.app)
                || !(self.filter)(&node.app)
            {
                continue;
            }
            let ch = port.channel.as_str();
            let targets: &[u32] = if ch == "FL" || (ch.ends_with('L') && ch != "MONO") {
                &[fl]
            } else if ch == "FR" || ch.ends_with('R') {
                &[fr]
            } else {
                &[fl, fr]
            };
            for &input in targets {
                wanted.push((port.node, *port_id, input));
            }
        }
        for (out_node, out_port, in_port) in wanted {
            if self.links.contains_key(&(out_port, in_port)) {
                continue;
            }
            let props = properties! {
                "link.output.node" => out_node.to_string(),
                "link.output.port" => out_port.to_string(),
                "link.input.node" => our.to_string(),
                "link.input.port" => in_port.to_string(),
                "object.linger" => "false",
            };
            match core.create_object::<pw::link::Link>("link-factory", &props) {
                Ok(link) => {
                    tracing::debug!("linked port {out_port} -> {in_port}");
                    self.links.insert((out_port, in_port), link);
                }
                Err(err) => tracing::warn!("creating audio link failed: {err}"),
            }
        }
    }

    fn remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.bound.remove(&id);
        if self.ports.remove(&id).is_some() {
            self.links.retain(|(o, i), _| *o != id && *i != id);
        }
    }
}

/// Starts capturing the target's audio.
pub(crate) fn start(target: AudioTarget) -> Result<AudioCapture> {
    let filter: Box<dyn Fn(&AudioApp) -> bool + Send> = match target {
        AudioTarget::Desktop { exclude } => {
            Box::new(move |app| !exclude.iter().any(|e| app.matches(e)))
        }
        AudioTarget::App(wanted) => Box::new(move |app| {
            (wanted.pid.is_some() && app.pid == wanted.pid)
                || (wanted.binary.is_some() && app.binary == wanted.binary)
                || app.name == wanted.name
        }),
    };

    let (mixer, frames) = SourceMixer::start();
    let writer = mixer.add_source();
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

    std::thread::Builder::new()
        .name("pipewire-audio".into())
        .spawn(move || {
            let result = run(filter, writer, quit_rx, &ready_tx);
            if let Err(err) = &result {
                tracing::warn!("audio capture failed: {err:#}");
            }
            let _ = ready_tx.send(result);
        })?;

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("PipeWire audio setup timed out"))??;

    Ok(AudioCapture::new(
        frames,
        Box::new(move || {
            let _ = quit_tx.send(());
            mixer.stop();
        }),
    ))
}

fn run(
    filter: Box<dyn Fn(&AudioApp) -> bool + Send>,
    writer: SourceWriter,
    quit_rx: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let registry = core.get_registry_rc()?;

    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

    let node_name = format!("screenshareapp-capture-{}", std::process::id());
    let stream = pw::stream::StreamBox::new(
        &core,
        "screenshare-audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => node_name.as_str(),
            "node.autoconnect" => "false",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(writer)
        .process(|stream, writer| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
            let Some(bytes) = d.data() else { return };
            let end = (offset + size).min(bytes.len());
            let samples: Vec<f32> = bytes[offset.min(end)..end]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            writer.push(&samples);
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(screenshare_media::SAMPLE_RATE);
    info.set_channels(2);
    let mut position = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing audio format: {e:?}"))?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values).context("audio format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let graph = Rc::new(RefCell::new(Graph {
        nodes: HashMap::new(),
        ports: HashMap::new(),
        our_node_name: node_name,
        our_node: None,
        links: HashMap::new(),
        bound: HashMap::new(),
        filter: Box::new(filter),
    }));

    let g = graph.clone();
    let core_for_links = core.clone();
    let g_remove = graph.clone();
    let reg_weak = registry.downgrade();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            let Some(props) = obj.props else { return };
            let mut graph = g.borrow_mut();
            match obj.type_ {
                ObjectType::Node => {
                    if props.get("node.name") == Some(graph.our_node_name.as_str()) {
                        graph.our_node = Some(obj.id);
                    }
                    let is_playback = props.get("media.class") == Some(PLAYBACK_CLASS);
                    graph.nodes.insert(
                        obj.id,
                        NodeInfo {
                            app: app_from_props(props),
                            is_playback,
                            resolved: !is_playback,
                        },
                    );
                    if is_playback && let Some(registry) = reg_weak.upgrade() {
                        let id = obj.id;
                        let g = g.clone();
                        let core = core_for_links.clone();
                        let bound = bind_playback(&registry, obj, move |app| {
                            // Info events are dispatched outside the global callback.
                            let Ok(mut graph) = g.try_borrow_mut() else {
                                return;
                            };
                            if let Some(node) = graph.nodes.get_mut(&id) {
                                node.app = app;
                                node.resolved = true;
                            }
                            graph.reconcile(&core);
                        });
                        if let Some(b) = bound {
                            graph.bound.insert(id, b);
                        }
                    }
                }
                ObjectType::Port => {
                    let Some(node) = props.get("node.id").and_then(|n| n.parse().ok()) else {
                        return;
                    };
                    graph.ports.insert(
                        obj.id,
                        PortInfo {
                            node,
                            output: props.get("port.direction") == Some("out"),
                            channel: props.get("audio.channel").unwrap_or("MONO").to_string(),
                        },
                    );
                }
                _ => return,
            }
            graph.reconcile(&core_for_links);
        })
        .global_remove(move |id| g_remove.borrow_mut().remove(id))
        .register();

    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    graph.borrow_mut().links.clear();
    Ok(())
}
