use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

#[cfg(test)]
mod tests;

pub const DISCOVERY_PORT: u16 = 45892;
const BEACON_INTERVAL: Duration = Duration::from_secs(2);
const PEER_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub struct LanPeer {
    pub id: String,
    pub name: String,
    pub addr: SocketAddr,
    pub sharing: bool,
    pub last_seen: Instant,
}

#[derive(Debug)]
pub enum LanEvent {
    Request {
        id: String,
        name: String,
    },
    Offer {
        id: String,
        sdp: String,
    },
    Answer {
        id: String,
        sdp: String,
    },
    /// The viewer lost packets and needs a fresh intra frame.
    ///
    /// The rtc transport offers no way to ask for this: both codecs negotiate
    /// an empty `rtcp_feedback`, the event handler has no RTCP callback, and
    /// the RTP sender has no retransmission buffer. The app's own LAN channel
    /// is the only path that works, and on a mostly-LAN session it is the path
    /// with the least latency anyway.
    Keyframe {
        id: String,
    },
    /// The viewer's measurement of the link, so the sharer can adjust quality.
    /// It cannot see its own link; this is the only evidence it gets.
    Report {
        id: String,
        loss: f32,
        fps: f32,
        /// Everything else the viewer measured about its own end. Diagnosis
        /// only — nothing in the quality controller reads it.
        diag: Diagnostics,
    },
    /// Who else is watching the same stream, and what shape the stream is.
    ///
    /// Sent by the sharer because it is the only party that knows: a viewer can
    /// see the other machines on the network, but not which of them are watching
    /// *this* stream. Carries the source frame rate too, so a viewer can judge
    /// its own decode time against a real frame budget rather than guessing one.
    Roster {
        id: String,
        viewers: Vec<RosterEntry>,
        source_fps: u32,
        /// The stream's target height. `None` is the sharer's native resolution,
        /// which is a real answer rather than a missing one.
        source_height: Option<u32>,
    },
}

/// Receive-side measurements that explain *why* a viewer's picture stutters, as
/// distinct from how bad its link is.
///
/// Kept apart from `loss` and `fps` because those two are load-bearing for the
/// adaptive controller and have been on the wire since the first version, while
/// these answer a question nobody has asked the controller yet. Every field
/// defaults, so a build from before this existed still parses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Diagnostics {
    /// Mean time inside the H.264 decoder, in milliseconds.
    ///
    /// Against the stream's frame budget this is what separates "this machine
    /// cannot decode fast enough" from "the link is dropping packets" — two
    /// problems with the same visible symptom and opposite fixes.
    #[serde(default)]
    pub decode_ms: f32,
    /// Mean time spent expanding a decoded frame to RGBA for display.
    #[serde(default)]
    pub present_ms: f32,
    /// Share of decoded frames that were replaced before being displayed,
    /// `0..100`.
    ///
    /// Counts frames the decode thread produced and the UI never showed, which
    /// is a stall no amount of keyframe recovery can fix.
    #[serde(default)]
    pub render_drops: f32,
    /// Decode errors per second.
    #[serde(default)]
    pub decode_errors: f32,
    /// Packets are arriving but nothing is decoding: the decoder is waiting for
    /// an intra frame it has not been given.
    #[serde(default)]
    pub waiting_keyframe: bool,
}

impl Diagnostics {
    /// Whether every measurement is a real number.
    pub fn is_finite(&self) -> bool {
        self.decode_ms.is_finite()
            && self.present_ms.is_finite()
            && self.render_drops.is_finite()
            && self.decode_errors.is_finite()
    }

    /// The measurements, brought into the ranges they claim to be in.
    ///
    /// One peer must not be able to hand the sharer a negative drop rate or a
    /// 400% one and have either of them show up as fact.
    pub fn clamped(&self) -> Self {
        Self {
            decode_ms: self.decode_ms.max(0.0),
            present_ms: self.present_ms.max(0.0),
            render_drops: self.render_drops.clamp(0.0, 100.0),
            decode_errors: self.decode_errors.max(0.0),
            waiting_keyframe: self.waiting_keyframe,
        }
    }
}

/// One viewer as the sharer sees it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RosterEntry {
    pub id: String,
    pub name: String,
    /// Whether their connection is up, as opposed to still negotiating.
    #[serde(default)]
    pub connected: bool,
}

impl RosterEntry {
    /// The name to show, never blank.
    pub fn label(&self) -> &str {
        if self.name.trim().is_empty() {
            "Anonymous"
        } else {
            self.name.as_str()
        }
    }
}

/// What a viewer tells the sharer about its end of the stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkReport {
    pub loss: f32,
    pub fps: f32,
    pub diag: Diagnostics,
}

#[derive(Serialize, Deserialize)]
struct Message {
    kind: String,
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    sharing: bool,
    #[serde(default)]
    sdp: String,
    /// Link loss as a percentage, in the `report` message.
    #[serde(default)]
    loss: f32,
    /// Decoded frames per second, in the `report` message.
    #[serde(default)]
    fps: f32,
    /// Diagnosis-only measurements, in the `report` message.
    #[serde(default)]
    diag: Diagnostics,
    /// Everyone the sharer is serving, in the `roster` message.
    #[serde(default)]
    roster: Vec<RosterEntry>,
    /// The stream's frame rate, in the `roster` message.
    #[serde(default)]
    source_fps: u32,
    /// The stream's target height in the `roster` message; absent means the
    /// sharer's native resolution.
    #[serde(default)]
    source_height: Option<u32>,
}

pub struct Lan {
    id: String,
    signal: Arc<UdpSocket>,
    signal_port: u16,
    peers: Arc<Mutex<HashMap<String, LanPeer>>>,
    events: Receiver<LanEvent>,
    name: Arc<Mutex<String>>,
    sharing: Arc<AtomicBool>,
    local: Arc<Mutex<Option<Ipv4Addr>>>,
    stop: Arc<AtomicBool>,
    joins: Vec<JoinHandle<()>>,
}

impl Lan {
    pub fn start(name: String) -> Result<Self, String> {
        let discovery = Arc::new(bind_discovery(DISCOVERY_PORT)?);
        let signal = Arc::new(bind_signal()?);
        let signal_port = signal.local_addr().map(|a| a.port()).unwrap_or(0);
        let id = random_id();
        let peers = Arc::new(Mutex::new(HashMap::new()));
        let name = Arc::new(Mutex::new(name));
        let sharing = Arc::new(AtomicBool::new(false));
        let local = Arc::new(Mutex::new(find_vpn_address()));
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = channel();

        let mut joins = Vec::new();

        let discovery_thread = Arc::clone(&discovery);
        let discovery_peers = Arc::clone(&peers);
        let discovery_name = Arc::clone(&name);
        let discovery_sharing = Arc::clone(&sharing);
        let discovery_local = Arc::clone(&local);
        let discovery_stop = Arc::clone(&stop);
        let discovery_id = id.clone();
        joins.push(
            thread::Builder::new()
                .name("argos-lan-discovery".to_string())
                .spawn(move || {
                    discovery_loop(
                        discovery_thread,
                        discovery_peers,
                        discovery_name,
                        discovery_sharing,
                        discovery_local,
                        discovery_stop,
                        discovery_id,
                        signal_port,
                    )
                })
                .map_err(|error| error.to_string())?,
        );

        let signal_thread = Arc::clone(&signal);
        let signal_stop = Arc::clone(&stop);
        let signal_id = id.clone();
        joins.push(
            thread::Builder::new()
                .name("argos-lan-signal".to_string())
                .spawn(move || signal_loop(signal_thread, signal_stop, signal_id, events_tx))
                .map_err(|error| error.to_string())?,
        );

        Ok(Self {
            id,
            signal,
            signal_port,
            peers,
            events,
            name,
            sharing,
            local,
            stop,
            joins,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn local_address(&self) -> Option<Ipv4Addr> {
        self.local.lock().ok().and_then(|local| *local)
    }

    pub fn set_name(&self, name: String) {
        if let Ok(mut current) = self.name.lock() {
            *current = name;
        }
    }

    pub fn set_sharing(&self, sharing: bool) {
        self.sharing.store(sharing, Ordering::Relaxed);
    }

    pub fn peers(&self) -> Vec<LanPeer> {
        let Ok(peers) = self.peers.lock() else {
            return Vec::new();
        };
        let mut list: Vec<LanPeer> = peers
            .values()
            .filter(|peer| peer.last_seen.elapsed() < PEER_TIMEOUT)
            .cloned()
            .collect();
        list.sort_by_cached_key(|peer| peer.name.to_lowercase());
        list
    }

    pub fn try_event(&self) -> Option<LanEvent> {
        self.events.try_recv().ok()
    }

    fn send_to_peer(&self, id: &str, message: Message) {
        let addr = self
            .peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(id).map(|peer| peer.addr));
        let Some(addr) = addr else {
            return;
        };
        if let Ok(json) = serde_json::to_vec(&message) {
            let _ = self.signal.send_to(&json, addr);
        }
    }

    fn message(&self, kind: &str, to: &str) -> Message {
        Message {
            kind: kind.to_string(),
            id: self.id.clone(),
            name: String::new(),
            to: to.to_string(),
            port: self.signal_port,
            sharing: false,
            sdp: String::new(),
            loss: 0.0,
            fps: 0.0,
            diag: Diagnostics::default(),
            roster: Vec::new(),
            source_fps: 0,
            source_height: None,
        }
    }

    pub fn send_request(&self, id: &str, name: &str) {
        let mut message = self.message("request", id);
        message.name = name.to_string();
        self.send_to_peer(id, message);
    }

    pub fn send_offer(&self, id: &str, sdp: String) {
        let mut message = self.message("offer", id);
        message.sdp = sdp;
        self.send_to_peer(id, message);
    }

    pub fn send_answer(&self, id: &str, sdp: String) {
        let mut message = self.message("answer", id);
        message.sdp = sdp;
        self.send_to_peer(id, message);
    }

    /// Asks a sharer for an immediate intra frame.
    ///
    /// Best-effort. UDP, and the request is only useful while the sharer is
    /// already connected to the same peer, so a lost message costs at most one
    /// `KEYFRAME_INTERVAL` of waiting.
    pub fn send_keyframe(&self, id: &str) {
        let message = self.message("keyframe", id);
        self.send_to_peer(id, message);
    }

    /// Reports this end's view of the link to a sharer.
    ///
    /// Goes out whether or not anything is wrong: the loss and frame rate are
    /// what let the sharer's controller step back up when a link recovers, which
    /// it could never infer on its own. The diagnostics ride along in the same
    /// datagram because they are the same measurement of the same window, and a
    /// second message would be a second thing to lose.
    pub fn send_report(&self, id: &str, report: &LinkReport) {
        let mut message = self.message("report", id);
        message.loss = report.loss;
        message.fps = report.fps;
        message.diag = report.diag;
        self.send_to_peer(id, message);
    }

    /// Tells one viewer who else is watching, and what the stream looks like.
    ///
    /// Best-effort, and meant to be repeated: it is sent whenever the roster
    /// changes and on a slow keepalive, because UDP loses datagrams and a
    /// viewer that missed the only roster it would ever get would show an empty
    /// list forever with nothing to say so.
    pub fn send_roster(
        &self,
        id: &str,
        viewers: &[RosterEntry],
        source_fps: u32,
        source_height: Option<u32>,
    ) {
        let mut message = self.message("roster", id);
        message.roster = viewers.to_vec();
        message.source_fps = source_fps;
        message.source_height = source_height;
        self.send_to_peer(id, message);
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

fn bind_discovery(port: u16) -> Result<UdpSocket, String> {
    let socket =
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    socket.set_reuse_address(true).map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
    socket.bind(&addr.into()).map_err(|e| e.to_string())?;
    Ok(socket.into())
}

fn bind_signal() -> Result<UdpSocket, String> {
    let socket =
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    socket.bind(&addr.into()).map_err(|e| e.to_string())?;
    Ok(socket.into())
}

fn random_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    hasher.write_u64(nanos);
    hasher.write_u64(std::process::id() as u64);
    format!("{:016x}", hasher.finish())
}

fn find_vpn_address() -> Option<Ipv4Addr> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    for interface in &interfaces {
        if let if_addrs::IfAddr::V4(v4) = &interface.addr {
            if interface.name.to_lowercase().contains("radmin") {
                return Some(v4.ip);
            }
        }
    }
    for interface in &interfaces {
        if let if_addrs::IfAddr::V4(v4) = &interface.addr {
            if v4.ip.octets()[0] == 26 {
                return Some(v4.ip);
            }
        }
    }
    None
}

fn broadcast_targets() -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT))];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let if_addrs::IfAddr::V4(v4) = interface.addr {
                if let Some(broadcast) = v4.broadcast {
                    let addr = SocketAddr::from((broadcast, DISCOVERY_PORT));
                    if !targets.contains(&addr) {
                        targets.push(addr);
                    }
                }
            }
        }
    }
    targets
}

#[allow(clippy::too_many_arguments)]
fn discovery_loop(
    socket: Arc<UdpSocket>,
    peers: Arc<Mutex<HashMap<String, LanPeer>>>,
    name: Arc<Mutex<String>>,
    sharing: Arc<AtomicBool>,
    local: Arc<Mutex<Option<Ipv4Addr>>>,
    stop: Arc<AtomicBool>,
    id: String,
    signal_port: u16,
) {
    let targets = broadcast_targets();
    let mut buffer = [0u8; 8192];
    let mut last_beacon = Instant::now() - BEACON_INTERVAL;

    while !stop.load(Ordering::Relaxed) {
        if last_beacon.elapsed() >= BEACON_INTERVAL {
            let detected = find_vpn_address();
            if let Ok(mut current) = local.lock() {
                *current = detected;
            }
            let current_name = name.lock().map(|n| n.clone()).unwrap_or_default();
            let message = Message {
                kind: "hello".to_string(),
                id: id.clone(),
                name: current_name,
                to: String::new(),
                port: signal_port,
                sharing: sharing.load(Ordering::Relaxed),
                sdp: String::new(),
                loss: 0.0,
                fps: 0.0,
                diag: Diagnostics::default(),
                roster: Vec::new(),
                source_fps: 0,
                source_height: None,
            };
            if let Ok(json) = serde_json::to_vec(&message) {
                for target in &targets {
                    let _ = socket.send_to(&json, target);
                }
            }
            if let Ok(mut peers) = peers.lock() {
                peers.retain(|_, peer| peer.last_seen.elapsed() < PEER_TIMEOUT);
            }
            last_beacon = Instant::now();
        }

        match socket.recv_from(&mut buffer) {
            Ok((length, addr)) => {
                if length == 0 {
                    continue;
                }
                let Ok(message) = serde_json::from_slice::<Message>(&buffer[..length]) else {
                    continue;
                };
                if message.id == id || message.kind != "hello" {
                    continue;
                }
                if let Ok(mut peers) = peers.lock() {
                    peers.insert(
                        message.id.clone(),
                        LanPeer {
                            id: message.id,
                            name: if message.name.trim().is_empty() {
                                "Unnamed".to_string()
                            } else {
                                message.name
                            },
                            addr: SocketAddr::new(addr.ip(), message.port),
                            sharing: message.sharing,
                            last_seen: Instant::now(),
                        },
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

/// Maps a decoded datagram to the event the app reacts to.
///
/// Split out from [`signal_loop`] because the string-to-event mapping is the
/// whole contract between two machines and nothing else in the type system
/// holds it together: rename a kind on the sending side and the sharer goes
/// quiet with no error anywhere. The `report` arm carries a validation rule
/// that only exists because of that same distance — it is not obviously
/// necessary at the call site, so it needs a test to stay.
fn dispatch(message: Message) -> Option<LanEvent> {
    match message.kind.as_str() {
        "request" => Some(LanEvent::Request {
            id: message.id,
            name: message.name,
        }),
        "offer" => Some(LanEvent::Offer {
            id: message.id,
            sdp: message.sdp,
        }),
        "answer" => Some(LanEvent::Answer {
            id: message.id,
            sdp: message.sdp,
        }),
        "keyframe" => Some(LanEvent::Keyframe { id: message.id }),
        "report" => {
            // A datagram whose measurements are absent rather than zero
            // deserialises to the defaults, giving "no loss, no frames" — a
            // fabricated healthy link that would walk the sharer's ladder up on
            // evidence nobody sent. Non-finite values never get this far: they
            // serialise as `null` and fail to parse.
            if !message.loss.is_finite() || !message.fps.is_finite() || !message.diag.is_finite() {
                return None;
            }
            Some(LanEvent::Report {
                id: message.id,
                loss: message.loss.clamp(0.0, 100.0),
                fps: message.fps.clamp(0.0, 1000.0),
                diag: message.diag.clamped(),
            })
        }
        "roster" => Some(LanEvent::Roster {
            id: message.id,
            viewers: message.roster,
            source_fps: message.source_fps,
            source_height: message.source_height,
        }),
        _ => None,
    }
}

fn signal_loop(
    socket: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
    id: String,
    events: Sender<LanEvent>,
) {
    let mut buffer = [0u8; 65536];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buffer) {
            Ok((length, _addr)) => {
                if length == 0 {
                    continue;
                }
                let Ok(message) = serde_json::from_slice::<Message>(&buffer[..length]) else {
                    continue;
                };
                if message.id == id || message.to != id {
                    continue;
                }
                if let Some(event) = dispatch(message) {
                    let _ = events.send(event);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}
