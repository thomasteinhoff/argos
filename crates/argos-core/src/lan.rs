use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

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

pub enum LanEvent {
    Request { id: String, name: String },
    Offer { id: String, sdp: String },
    Answer { id: String, sdp: String },
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
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
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
                match message.kind.as_str() {
                    "request" => {
                        let _ = events.send(LanEvent::Request {
                            id: message.id,
                            name: message.name,
                        });
                    }
                    "offer" => {
                        let _ = events.send(LanEvent::Offer {
                            id: message.id,
                            sdp: message.sdp,
                        });
                    }
                    "answer" => {
                        let _ = events.send(LanEvent::Answer {
                            id: message.id,
                            sdp: message.sdp,
                        });
                    }
                    _ => {}
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}
