//! PipeWire-Audio-Routing für selektives Capture (GSR-`pipewire_audio.c`-Modell).
//!
//! Problem: der simple Sink-Monitor-Capture (`STREAM_CAPTURE_SINK` auf dem
//! Default-Sink) nimmt ALLES auf — auch Pulses eigene Voice-Wiedergabe (Echo
//! im Stream) und ohne Möglichkeit, eine einzelne App zu wählen.
//!
//! Lösung wie GSR: ein eigener **Null-Sink** (`support.null-audio-sink`) wird
//! erzeugt; die Output-Ports der gewünschten App-Streams (`media.class ==
//! "Stream/Output/Audio"`) werden per `link-factory` ZUSÄTZLICH zu ihren
//! bestehenden Links auf unseren Sink gelinkt (der User hört weiter alles).
//! Der Capture-Stream (in `audio.rs`) hängt am Monitor unseres Sinks
//! (`TARGET_OBJECT` = Sink-Name + `STREAM_CAPTURE_SINK`).
//!
//! Modi:
//! - [`RouteMode::All`]: alle App-Streams AUSSER den excludes (Desktop-Modus;
//!   "Pulse" ist immer dabei — Echo-Schutz).
//! - [`RouteMode::App`]: nur Streams, deren App-Name passt (case-insensitive).
//!
//! Die Registry wird live beobachtet: Apps, die während des Streams starten,
//! werden nachgelinkt; verschwundene Globals werden aufgeräumt. Alles läuft
//! auf dem PipeWire-Audio-Worker-Thread (ein Mainloop, `Rc<RefCell<_>>`).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use anyhow::{Context, Result};
use pipewire as pw;
use pw::core::CoreRc;
use pw::properties::properties;
use pw::registry::{GlobalObject, Listener, RegistryRc};
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;

/// Welche App-Streams auf den Capture-Sink geroutet werden.
pub enum RouteMode {
    /// Alle Apps außer den genannten (case-insensitive; Desktop-Modus).
    All { exclude: Vec<String> },
    /// Nur die eine App (case-insensitive; "App: <name>"-Modus).
    App { name: String },
}

impl RouteMode {
    /// Entscheidet über `application.name` UND `node.name`. Der Echo-Schutz
    /// hängt an `node.name` („Pulse", via PULSE_PROP in Electron-main gesetzt)
    /// — Electron trägt als `application.name` aber „Chromium": ein Match nur
    /// über den App-Namen ließe Pulses eigene Voice-Wiedergabe in den Stream
    /// (genau das Echo, das der Exclude verhindern soll).
    fn matches(&self, app: &str, node: &str) -> bool {
        let hits = |n: &str| n.eq_ignore_ascii_case(app) || n.eq_ignore_ascii_case(node);
        match self {
            RouteMode::All { exclude } => !exclude.iter().any(|e| hits(e)),
            RouteMode::App { name } => hits(name),
        }
    }
}

struct PortInfo {
    global_id: u32,
    channel: String,
}

/// Beide Namens-Properties eines App-Stream-Nodes — der Exclude-/App-Match
/// muss BEIDE sehen (s. `RouteMode::matches`).
struct AppNames {
    app: String,
    node: String,
}

#[derive(Default)]
struct State {
    sink_node_id: Option<u32>,
    /// Input-Ports unseres Null-Sinks: (audio.channel, Port-Global-Id).
    sink_ports: Vec<(String, u32)>,
    /// App-Stream-Nodes: Node-Global-Id → (application.name, node.name).
    app_nodes: HashMap<u32, AppNames>,
    /// Output-Audio-Ports pro App-Node.
    out_ports: HashMap<u32, Vec<PortInfo>>,
    /// Aktive Links (Out-Port, In-Port) → Proxy (Drop = Link weg).
    links: HashMap<(u32, u32), pw::link::Link>,
}

/// Laufender Router. Hält Sink + Registry-Listener + Links am Leben; alles
/// wird beim Drop (= Ende des Audio-Worker-Threads) serverseitig abgeräumt
/// (`object.linger=false`).
pub struct AudioRouter {
    _sink: pw::node::Node,
    _registry: RegistryRc,
    _listener: Listener,
    sink_name: String,
}

impl AudioRouter {
    pub fn start(core: &CoreRc, mode: RouteMode) -> Result<Self> {
        // PID im Namen: zwei Sidecars (Dev + Flatpak) kollidieren sonst.
        let sink_name = format!("pulse-hq-sidecar-capture-{}", std::process::id());
        let sink: pw::node::Node = core
            .create_object(
                "adapter",
                &properties! {
                    "factory.name" => "support.null-audio-sink",
                    *pw::keys::NODE_NAME => sink_name.as_str(),
                    *pw::keys::NODE_DESCRIPTION => "Pulse HQ-Stream Capture",
                    *pw::keys::MEDIA_CLASS => "Audio/Sink",
                    "audio.position" => "[FL FR]",
                    // Monitor liefert die Roh-Samples der Quellen, unabhängig
                    // von Sink-Lautstärke.
                    "monitor.channel-volumes" => "false",
                    "node.virtual" => "true",
                },
            )
            .context("Null-Sink (support.null-audio-sink) erzeugen")?;

        let registry = core.get_registry_rc().context("get_registry")?;
        let state = Rc::new(RefCell::new(State::default()));

        let listener = registry
            .add_listener_local()
            .global({
                let state = state.clone();
                let core = core.clone();
                let sink_name = sink_name.clone();
                move |g| on_global(&core, &state, &mode, &sink_name, g)
            })
            .global_remove({
                let state = state.clone();
                move |id| on_remove(&state, id)
            })
            .register();

        Ok(Self { _sink: sink, _registry: registry, _listener: listener, sink_name })
    }

    /// Node-Name des Capture-Sinks — Ziel (`TARGET_OBJECT`) für den
    /// Monitor-Capture-Stream.
    pub fn sink_name(&self) -> &str {
        &self.sink_name
    }
}

fn on_global(
    core: &CoreRc,
    state: &Rc<RefCell<State>>,
    mode: &RouteMode,
    sink_name: &str,
    g: &GlobalObject<&DictRef>,
) {
    let Some(props) = g.props else { return };
    {
        let mut st = state.borrow_mut();
        match g.type_ {
            ObjectType::Node => {
                if props.get("node.name") == Some(sink_name) {
                    st.sink_node_id = Some(g.id);
                } else if props.get("media.class") == Some("Stream/Output/Audio") {
                    let names = AppNames {
                        app: app_name_of(props).unwrap_or_default(),
                        node: props.get("node.name").unwrap_or_default().to_string(),
                    };
                    tracing::debug!(
                        target: "audio", id = g.id, app = names.app, node = names.node,
                        "App-Audio-Stream erschienen"
                    );
                    st.app_nodes.insert(g.id, names);
                } else {
                    return;
                }
            }
            ObjectType::Port => {
                let Some(node_id) = props.get("node.id").and_then(|s| s.parse::<u32>().ok())
                else {
                    return;
                };
                let direction = props.get("port.direction").unwrap_or("");
                let channel = props.get("audio.channel").unwrap_or("UNK").to_string();
                if st.sink_node_id == Some(node_id) && direction == "in" {
                    st.sink_ports.push((channel, g.id));
                } else if st.app_nodes.contains_key(&node_id) && direction == "out" {
                    st.out_ports
                        .entry(node_id)
                        .or_default()
                        .push(PortInfo { global_id: g.id, channel });
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
    ensure_links(core, &mut state.borrow_mut(), mode);
}

fn on_remove(state: &Rc<RefCell<State>>, id: u32) {
    let mut st = state.borrow_mut();
    st.app_nodes.remove(&id);
    st.out_ports.remove(&id);
    for ports in st.out_ports.values_mut() {
        ports.retain(|p| p.global_id != id);
    }
    st.sink_ports.retain(|&(_, pid)| pid != id);
    if st.sink_node_id == Some(id) {
        st.sink_node_id = None;
    }
    st.links.retain(|&(out_p, in_p), _| out_p != id && in_p != id);
}

/// Fehlende Links zwischen passenden App-Out-Ports und Sink-In-Ports anlegen.
/// Idempotent — läuft nach jedem relevanten Registry-Event.
fn ensure_links(core: &CoreRc, st: &mut State, mode: &RouteMode) {
    let Some(sink_node) = st.sink_node_id else { return };
    if st.sink_ports.is_empty() {
        return;
    }
    let candidates: Vec<(u32, Vec<(u32, String)>)> = st
        .app_nodes
        .iter()
        .filter(|(_, names)| mode.matches(&names.app, &names.node))
        .filter_map(|(&nid, _)| {
            st.out_ports
                .get(&nid)
                .map(|ps| (nid, ps.iter().map(|p| (p.global_id, p.channel.clone())).collect()))
        })
        .collect();

    for (nid, ports) in candidates {
        // SOLL-Linksatz dieses Nodes aus dem AKTUELLEN Port-Stand berechnen und
        // dann abgleichen (anlegen + überzählige entfernen). Wichtig, weil
        // PipeWire die Ports eines Stereo-Streams EINZELN meldet und wir nach
        // jedem Registry-Event laufen: Beim ersten Event sieht der Node wie
        // Mono aus (ein Port) → der Port wurde auf BEIDE Sink-Kanäle gelinkt;
        // der Link blieb nach Eintreffen des zweiten Ports kleben → der zweite
        // Sink-Kanal bekam die SUMME beider Kanäle (rechts doppelt so laut,
        // bei Dual-Mono exakt +6 dB — 2026-07-16 am MoQ-Player gemessen; WHEP
        // hatte es durch seinen Mono-Downmix verdeckt).
        let mono = ports.len() == 1;
        let mut desired: Vec<(u32, u32)> = Vec::new();
        for (port_id, channel) in &ports {
            let dests: Vec<u32> = if mono || channel == "MONO" {
                st.sink_ports.iter().map(|&(_, id)| id).collect()
            } else {
                let matched: Vec<u32> = st
                    .sink_ports
                    .iter()
                    .filter(|(c, _)| c == channel)
                    .map(|&(_, id)| id)
                    .collect();
                if matched.is_empty() {
                    // Exotischer Kanal (AUX…) → wenigstens auf den ersten.
                    st.sink_ports.first().map(|&(_, id)| vec![id]).unwrap_or_default()
                } else {
                    matched
                }
            };
            desired.extend(dests.into_iter().map(|d| (*port_id, d)));
        }

        // Überzählige Links dieses Nodes lösen (Drop des Proxys löst den Link).
        let node_ports: BTreeSet<u32> = ports.iter().map(|(id, _)| *id).collect();
        let stale: Vec<(u32, u32)> = st
            .links
            .keys()
            .filter(|(out_p, _)| node_ports.contains(out_p))
            .filter(|pair| !desired.contains(pair))
            .copied()
            .collect();
        for pair in stale {
            st.links.remove(&pair);
            tracing::info!(target: "audio", node = nid, "veralteten Audio-Link gelöst");
        }

        for (port_id, dest) in desired {
            if st.links.contains_key(&(port_id, dest)) {
                continue;
            }
            match create_link(core, nid, port_id, sink_node, dest) {
                Ok(link) => {
                    tracing::info!(
                        target: "audio",
                        node = nid,
                        "App-Audio → Capture-Sink verbunden"
                    );
                    st.links.insert((port_id, dest), link);
                }
                Err(e) => {
                    tracing::warn!(target: "audio", "Audio-Link erstellen: {e:#}");
                }
            }
        }
    }
}

fn create_link(
    core: &CoreRc,
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
) -> Result<pw::link::Link> {
    // Link stirbt mit unserem Client (linger=false) — kein Aufräum-Leak bei Crash.
    let mut props = properties! { "object.linger" => "false" };
    props.insert("link.output.node", out_node.to_string());
    props.insert("link.output.port", out_port.to_string());
    props.insert("link.input.node", in_node.to_string());
    props.insert("link.input.port", in_port.to_string());
    core.create_object::<pw::link::Link>("link-factory", &props)
        .context("link-factory create_object")
}

/// App-Name eines Stream-Nodes: `application.name` (menschlich, z. B.
/// "Firefox"), Fallback `node.name`.
fn app_name_of(props: &DictRef) -> Option<String> {
    props
        .get("application.name")
        .or_else(|| props.get("node.name"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Apps mit laufendem Audio-Output auflisten (für `list_application_audio`).
/// Kurzer eigener Mainloop: Registry enumerieren, ein sync-Roundtrip, fertig.
pub fn list_applications() -> Result<Vec<String>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw mainloop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw context")?;
    let core = context.connect_rc(None).context("pw connect")?;
    let registry = core.get_registry_rc().context("get_registry")?;

    let names = Rc::new(RefCell::new(BTreeSet::<String>::new()));
    let _rl = registry
        .add_listener_local()
        .global({
            let names = names.clone();
            move |g| {
                if g.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = g.props else { return };
                if props.get("media.class") != Some("Stream/Output/Audio") {
                    return;
                }
                if let Some(n) = app_name_of(props) {
                    names.borrow_mut().insert(n);
                }
            }
        })
        .register();

    // sync(0): der `done` kommt, nachdem der Server alle bestehenden Globals
    // geliefert hat → Mainloop beenden.
    let pending = core.sync(0).context("core sync")?;
    let done = Rc::new(RefCell::new(false));
    let _cl = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let done = done.clone();
            move |_id, seq| {
                if seq == pending {
                    *done.borrow_mut() = true;
                    mainloop.quit();
                }
            }
        })
        .register();
    // Sicherheitsnetz: kommt der `done` nie (Daemon hängt/tot bei offenem
    // Socket), bliebe `mainloop.run()` für immer stehen — und mit ihm die
    // ganze RPC-Schleife des Sidecars.
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_expirations| mainloop.quit()
    });
    timer
        .update_timer(Some(std::time::Duration::from_secs(5)), None)
        .into_result()
        .context("timer armieren")?;
    mainloop.run();

    if !*done.borrow() {
        return Err(anyhow::anyhow!("PipeWire antwortet nicht (Timeout nach 5s)"));
    }
    let out = names.borrow().iter().cloned().collect();
    Ok(out)
}

#[cfg(test)]
mod match_tests {
    use super::RouteMode;

    /// Der Echo-Schutz-Fall: Electron meldet application.name="Chromium",
    /// aber node.name="Pulse" (PULSE_PROP). Der Exclude "Pulse" MUSS greifen.
    #[test]
    fn exclude_matches_node_name_too() {
        let all = RouteMode::All { exclude: vec!["Pulse".into()] };
        assert!(!all.matches("Chromium", "Pulse"), "node.name=Pulse muss excluded sein");
        assert!(!all.matches("Pulse", "whatever"), "application.name=Pulse ebenso");
        assert!(all.matches("Firefox", "firefox-bin"), "fremde Apps bleiben drin");
    }

    #[test]
    fn app_mode_matches_either_name() {
        let app = RouteMode::App { name: "Firefox".into() };
        assert!(app.matches("Firefox", "node-x"));
        assert!(app.matches("something", "firefox"), "case-insensitiv über node.name");
        assert!(!app.matches("Chromium", "chrome"));
    }
}
