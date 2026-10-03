use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::rtp::packet::Packet;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{default_runtime, Runtime};

use crate::h264::{self, Packetizer};
use crate::metrics::{Ema, SenderMetrics, StageTimer};
use crate::signal;

pub const VIDEO_PT: u8 = 96;
pub const AUDIO_PT: u8 = 111;
const VIDEO_SSRC: u32 = 0x5a5a_77e1;
const AUDIO_SSRC: u32 = 0x5a5a_77e2;
const MTU: usize = 1200;
const GATHER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an offer may go unanswered before the slot can be handed out again.
///
/// Gathering itself is bounded by `GATHER_TIMEOUT`, so anything past that plus a
/// margin is an offer the viewer never acted on — its datagram was lost, or the
/// person closed the window. Either way, re-offering beats leaving them on a
/// connecting screen forever.
const UNANSWERED_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a connection must sit down before another request earns a fresh
/// offer for it.
///
/// A viewer whose link dropped re-requests every few seconds and gives up after
/// five tries, so a re-offer has to land inside that window or the reconnect is
/// lost. The grace period is what keeps it from firing on the first duplicate
/// click or on a link that is mid-handshake: only a connection that has already
/// been seen down for this long is one that is not coming back on its own.
const REOFFER_DELAY: Duration = Duration::from_secs(5);

/// How long a connection that was up may sit down before its slot is freed.
///
/// Long enough to ride out a link that dips and recovers on its own, since
/// tearing the connection down on the first `Disconnected` would turn a blip
/// into a reconnect.
const DOWN_TIMEOUT: Duration = Duration::from_secs(20);

pub type PacketCallback = Arc<dyn Fn(&Packet) + Send + Sync>;

pub fn runtime() -> Arc<dyn Runtime> {
    static RUNTIME: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    Arc::clone(RUNTIME.get_or_init(|| default_runtime().expect("no runtime feature enabled")))
}

fn tokio() -> &'static tokio::runtime::Runtime {
    static TOKIO: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    })
}

pub fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    let mut out: Option<T> = None;
    let slot = &mut out;
    tokio().block_on(Box::pin(async move {
        *slot = Some(future.await);
    }));
    out.expect("block_on future did not complete")
}

#[derive(Default)]
struct HandlerState {
    gathering_complete: bool,
    connected: bool,
}

struct SessionHandler {
    state: Arc<Mutex<HandlerState>>,
    on_packet: Option<PacketCallback>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for SessionHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            if let Ok(mut current) = self.state.lock() {
                current.gathering_complete = true;
            }
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if let Ok(mut current) = self.state.lock() {
            match state {
                RTCPeerConnectionState::Connected => current.connected = true,
                RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed => current.connected = false,
                _ => {}
            }
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(callback) = &self.on_packet {
            let callback = Arc::clone(callback);
            runtime().spawn(Box::pin(async move {
                while let Some(event) = track.poll().await {
                    if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                        callback(&packet);
                    }
                }
            }));
        }
    }
}

fn h264_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_owned(),
        rtcp_feedback: vec![],
    }
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn setting_engine() -> rtc::peer_connection::configuration::setting_engine::SettingEngine {
    SettingEngineBuilder::new()
        .with_include_loopback_candidate(true)
        .build()
}

async fn build_pc(
    state: Arc<Mutex<HandlerState>>,
    on_packet: Option<PacketCallback>,
    udp_addrs: Vec<String>,
) -> Result<Arc<dyn PeerConnection>, String> {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: h264_codec(),
                payload_type: VIDEO_PT,
            },
            RtpCodecKind::Video,
        )
        .map_err(|error| error.to_string())?;
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: opus_codec(),
                payload_type: AUDIO_PT,
            },
            RtpCodecKind::Audio,
        )
        .map_err(|error| error.to_string())?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)
        .map_err(|error| error.to_string())?;
    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine())
            .with_handler(Arc::new(SessionHandler { state, on_packet }))
            .with_runtime(runtime())
            .with_udp_addrs(udp_addrs)
            .build()
            .await
            .map_err(|error| error.to_string())?,
    );
    Ok(pc)
}

async fn wait_for_gathering(state: &Mutex<HandlerState>) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        if state
            .lock()
            .map(|current| current.gathering_complete)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if started.elapsed() >= GATHER_TIMEOUT {
            return Err("timed out waiting for ICE gathering".to_string());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Creates an offer and returns it as a signal code, candidates included.
async fn create_offer(
    pc: &Arc<dyn PeerConnection>,
    state: &Mutex<HandlerState>,
) -> Result<String, String> {
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|error| error.to_string())?;
    pc.set_local_description(offer)
        .await
        .map_err(|error| error.to_string())?;
    wait_for_gathering(state).await?;
    let description = pc
        .local_description()
        .await
        .ok_or_else(|| "no local description".to_string())?;
    Ok(signal::encode(&description))
}

/// One viewer's connection to a sharer.
///
/// Every viewer gets its own peer connection and its own pair of tracks. The
/// encoded frame is shared, the transports are not: an SDP is a single-negotiated
/// description of one connection, so a second viewer on the first connection's
/// answer would be negotiating against a description that already says `recvonly`
/// and has no second media section to put them in.
struct ViewerConn {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticRTP>,
    audio_track: Arc<TrackLocalStaticRTP>,
    state: Arc<Mutex<HandlerState>>,
    /// Whether this connection ever reached `Connected`.
    ///
    /// Distinguishes "the viewer closed their app" from "the viewer never got
    /// my offer", which the connection state reports identically and which want
    /// opposite treatment: one frees the slot at once, the other holds it until
    /// the answer window closes.
    ever_connected: bool,
    /// When the connection was first seen without a live peer.
    down_since: Option<Instant>,
    /// Time spent writing this viewer's share of a frame, over the connection
    /// rather than the whole fan-out.
    ///
    /// Its own handle rather than a field so the write path can record into it
    /// after releasing the map lock, which it must not hold across a socket write.
    write: Arc<Ema>,
}

fn video_track() -> TrackLocalStaticRTP {
    TrackLocalStaticRTP::new(MediaStreamTrack::new(
        "argos-stream".to_string(),
        "argos-video".to_string(),
        "argos-video".to_string(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(VIDEO_SSRC),
                ..Default::default()
            },
            codec: h264_codec(),
            ..Default::default()
        }],
    ))
}

fn audio_track() -> TrackLocalStaticRTP {
    TrackLocalStaticRTP::new(MediaStreamTrack::new(
        "argos-stream".to_string(),
        "argos-audio".to_string(),
        "argos-audio".to_string(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(AUDIO_SSRC),
                ..Default::default()
            },
            codec: opus_codec(),
            ..Default::default()
        }],
    ))
}

/// One viewer's destination for a packetized frame.
///
/// The timing handle comes along with the track so the write path can record
/// into it without holding the map lock, and so each viewer's cost is separable:
/// one aggregate figure across several viewers cannot say whose socket is slow,
/// and that is exactly the question when one person's stream stutters and
/// everyone else's does not.
struct WriteTarget {
    id: String,
    track: Arc<TrackLocalStaticRTP>,
    write: Arc<Ema>,
}

pub struct Sharer {
    /// Live connections, keyed by the viewer's id.
    ///
    /// Ordering is irrelevant here: a frame goes to all of them, and the UI keeps
    /// its own order for display.
    viewers: Mutex<HashMap<String, ViewerConn>>,
    udp_addrs: Vec<String>,
    /// Shared across viewers so every connection sees one monotonic sequence
    /// space for a given SSRC. Per-viewer packetizers would also be correct —
    /// each peer connection is its own RTP session — but sharing keeps the
    /// packetize step off the per-viewer path, and a `Bytes` payload clone is a
    /// refcount bump rather than a copy.
    packetizer: Mutex<Packetizer>,
    audio_packetizer: Mutex<Packetizer>,
}

impl Sharer {
    pub fn new(udp_addrs: Vec<String>) -> Self {
        Self {
            viewers: Mutex::new(HashMap::new()),
            udp_addrs,
            packetizer: Mutex::new(Packetizer::new(VIDEO_SSRC, VIDEO_PT, MTU)),
            audio_packetizer: Mutex::new(Packetizer::new(AUDIO_SSRC, AUDIO_PT, MTU)),
        }
    }

    /// Opens a connection for one viewer and returns the offer it must answer.
    ///
    /// Any previous connection for the same id is closed first, so a peer that
    /// asks again — a re-request after a dropped link, or a click that arrived
    /// twice over UDP — gets one connection rather than two tracks fighting
    /// over the same viewer's packets.
    pub async fn add_viewer(&self, id: &str) -> Result<String, String> {
        self.remove_viewer(id).await;
        let state = Arc::new(Mutex::new(HandlerState::default()));
        let pc = build_pc(state.clone(), None, self.udp_addrs.clone()).await?;
        let track = Arc::new(video_track());
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| error.to_string())?;
        let audio_track = Arc::new(audio_track());
        pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| error.to_string())?;
        let offer = create_offer(&pc, &state).await?;
        let Ok(mut viewers) = self.viewers.lock() else {
            let _ = pc.close().await;
            return Err("sharer viewer lock poisoned".to_string());
        };
        viewers.insert(
            id.to_string(),
            ViewerConn {
                pc,
                track,
                audio_track,
                state,
                ever_connected: false,
                down_since: None,
                write: Arc::new(Ema::default()),
            },
        );
        Ok(offer)
    }

    /// Answers the offer belonging to one viewer.
    pub async fn set_answer(&self, id: &str, code: &str) -> Result<(), String> {
        let pc = self
            .viewers
            .lock()
            .ok()
            .and_then(|viewers| viewers.get(id).map(|conn| Arc::clone(&conn.pc)))
            .ok_or_else(|| "that viewer is no longer waiting for an offer".to_string())?;
        let answer =
            signal::decode(code).map_err(|error| format!("invalid answer code: {error}"))?;
        pc.set_remote_description(answer)
            .await
            .map_err(|error| error.to_string())
    }

    /// Closes one viewer's connection and forgets it.
    pub async fn remove_viewer(&self, id: &str) {
        let conn = self
            .viewers
            .lock()
            .ok()
            .and_then(|mut viewers| viewers.remove(id));
        if let Some(conn) = conn {
            let _ = conn.pc.close().await;
        }
    }

    /// Folds a connection's state into its slot, returning whether it is up.
    ///
    /// Owns the `down_since` clock, which is the only thing that knows how long
    /// a connection has been dead — and therefore the only thing that can tell a
    /// handshake still in progress from a connection that is never going to
    /// arrive.
    fn observe(conn: &mut ViewerConn, connected: bool, now: Instant) -> bool {
        if connected {
            conn.ever_connected = true;
            conn.down_since = None;
            return true;
        }
        conn.down_since.get_or_insert(now);
        false
    }

    /// Whether a fresh offer is worth making for this viewer.
    ///
    /// A request means one of three things: a first click, the same click twice
    /// (UDP does not dedupe), or a reconnect after the link dropped. The first
    /// two want the offer already in flight — re-offering would throw away a
    /// negotiation that is about to succeed. The third wants a new connection,
    /// and without this it would be answered with the dead one and sit on a
    /// connecting screen until the viewer gave up.
    pub fn needs_reoffer(&self, id: &str) -> bool {
        let now = Instant::now();
        let Ok(mut viewers) = self.viewers.lock() else {
            return false;
        };
        let Some(conn) = viewers.get_mut(id) else {
            return false;
        };
        let connected = conn
            .state
            .lock()
            .map(|state| state.connected)
            .unwrap_or(false);
        if Self::observe(conn, connected, now) {
            return false;
        }
        // Still gathering, and never connected: an answer is not possible yet,
        // so this is a handshake in progress rather than a failed one. Without
        // this the second of two clicks inside the gather window would tear down
        // a negotiation that is about to succeed.
        if !conn.ever_connected
            && !conn
                .state
                .lock()
                .map(|state| state.gathering_complete)
                .unwrap_or(false)
        {
            return false;
        }
        now.duration_since(conn.down_since.unwrap_or(now)) >= REOFFER_DELAY
    }

    /// Drops connections that are finished, returning the ids it freed.
    ///
    /// Called by the UI rather than from the send path, so the `await` on the
    /// closes is off every frame. The map lock is released first: a close can
    /// take a moment, and blocking a sender on it would stall the encode thread.
    pub async fn prune(&self) -> Vec<String> {
        let now = Instant::now();
        let mut dead: Vec<(String, Arc<dyn PeerConnection>)> = Vec::new();
        {
            let Ok(mut viewers) = self.viewers.lock() else {
                return Vec::new();
            };
            for (id, conn) in viewers.iter_mut() {
                let connected = conn
                    .state
                    .lock()
                    .map(|state| state.connected)
                    .unwrap_or(false);
                if Self::observe(conn, connected, now) {
                    continue;
                }
                let limit = if conn.ever_connected {
                    DOWN_TIMEOUT
                } else {
                    UNANSWERED_TIMEOUT
                };
                if now.duration_since(conn.down_since.unwrap_or(now)) >= limit {
                    dead.push((id.clone(), Arc::clone(&conn.pc)));
                }
            }
            for (id, _) in &dead {
                viewers.remove(id);
            }
        }
        for (_, pc) in &dead {
            let _ = pc.close().await;
        }
        dead.into_iter().map(|(id, _)| id).collect()
    }

    /// Tracks a frame has to reach, with the id of the viewer behind each.
    ///
    /// Snapshot so the encode thread never holds the map lock across a socket
    /// write: adding a viewer then never waits on a congested send, and a stuck
    /// viewer cannot hold up everyone else's picture.
    ///
    /// Only connections that are up. A viewer still negotiating has no RTP sender
    /// to write to, and treating its failure as a dead connection would drop
    /// someone who is on their way in — they get their first frame from the
    /// keyframe interval instead, which is the same wait anyone joining a stream
    /// already has.
    fn video_targets(&self) -> Vec<WriteTarget> {
        self.connected_tracks(true)
    }

    fn audio_targets(&self) -> Vec<WriteTarget> {
        self.connected_tracks(false)
    }

    fn connected_tracks(&self, video: bool) -> Vec<WriteTarget> {
        self.viewers
            .lock()
            .map(|viewers| {
                viewers
                    .iter()
                    .filter(|(_, conn)| {
                        conn.state
                            .lock()
                            .map(|state| state.connected)
                            .unwrap_or(false)
                    })
                    .map(|(id, conn)| {
                        let track = if video {
                            &conn.track
                        } else {
                            &conn.audio_track
                        };
                        WriteTarget {
                            id: id.clone(),
                            track: Arc::clone(track),
                            write: Arc::clone(&conn.write),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn send_frame(
        &self,
        bitstream: &[u8],
        timestamp: u32,
        metrics: &SenderMetrics,
    ) -> Result<(), String> {
        let packets = {
            let _packetize = StageTimer::new(&metrics.packetize);
            let nalus: Vec<&[u8]> = h264::AnnexBIter::new(bitstream).collect();
            let mut packetizer = self
                .packetizer
                .lock()
                .map_err(|_| "packetizer lock poisoned".to_string())?;
            nalus
                .iter()
                .enumerate()
                .flat_map(|(index, nalu)| {
                    let marker = index + 1 == nalus.len();
                    packetizer.packetize(nalu, timestamp, marker)
                })
                .collect::<Vec<Packet>>()
        };
        // No viewer to send to is the normal state between going live and the first
        // request, not an error worth writing to the stats panel.
        let targets = self.video_targets();
        if targets.is_empty() {
            return Ok(());
        }
        let _write = StageTimer::new(&metrics.write);
        let mut failed = Vec::new();
        for target in targets {
            // Timed per viewer as well as in aggregate. The aggregate says the
            // send path is costing something; this says whose socket it is
            // costing it to, which is the difference between "the stream is
            // heavy" and "this one viewer's link is not keeping up".
            let _viewer_write = StageTimer::new(&target.write);
            for packet in &packets {
                if let Err(_error) = target.track.write_rtp(packet.clone()).await {
                    metrics.write_errors.incr();
                    if !failed.contains(&target.id) {
                        failed.push(target.id);
                    }
                    break;
                }
            }
        }
        // One dead viewer must not cost everyone else their picture, so the
        // error is contained to its own connection and the frame still counts as
        // sent. The slot goes with it, which is what lets the same person be
        // served again when they ask.
        self.drop_failed(&failed).await;
        Ok(())
    }

    pub async fn send_audio(
        &self,
        opus: &[u8],
        timestamp: u32,
        metrics: &SenderMetrics,
    ) -> Result<(), String> {
        metrics.audio_frames.incr();
        metrics.audio_bytes.add(opus.len() as u64);
        let packets = {
            let mut packetizer = self
                .audio_packetizer
                .lock()
                .map_err(|_| "audio packetizer lock poisoned".to_string())?;
            packetizer.packetize(opus, timestamp, true)
        };
        let targets = self.audio_targets();
        if targets.is_empty() {
            return Ok(());
        }
        let mut failed = Vec::new();
        for target in targets {
            let _viewer_write = StageTimer::new(&target.write);
            for packet in &packets {
                if let Err(_error) = target.track.write_rtp(packet.clone()).await {
                    metrics.audio_errors.incr();
                    if !failed.contains(&target.id) {
                        failed.push(target.id);
                    }
                    break;
                }
            }
        }
        self.drop_failed(&failed).await;
        Ok(())
    }

    /// Closes the connections whose writes failed.
    async fn drop_failed(&self, ids: &[String]) {
        for id in ids {
            self.remove_viewer(id).await;
        }
    }

    /// How long writing this viewer's share of a frame has been taking, as
    /// `(mean, peak)` in milliseconds. `(0.0, 0.0)` when there is no such viewer
    /// or nothing has been written yet.
    ///
    /// Per viewer rather than in aggregate because the aggregate cannot answer
    /// the question that matters when one person's stream stutters and nobody
    /// else's does: whether their writes are slow, or the loss is happening after
    /// the packets leave.
    pub fn viewer_write_ms(&self, id: &str) -> (f32, f32) {
        self.viewers
            .lock()
            .ok()
            .and_then(|viewers| viewers.get(id).map(|conn| conn.write.clone()))
            .map(|write| (write.mean_ms(), write.peak_ms()))
            .unwrap_or((0.0, 0.0))
    }

    /// Clears every viewer's write timing, so a measurement window reset covers
    /// the whole pipeline rather than leaving the per-viewer figures describing
    /// some earlier window.
    pub fn reset_write_timings(&self) {
        if let Ok(viewers) = self.viewers.lock() {
            for conn in viewers.values() {
                conn.write.reset();
            }
        }
    }

    /// Whether one viewer's connection is up.
    pub fn viewer_connected(&self, id: &str) -> bool {
        let Ok(viewers) = self.viewers.lock() else {
            return false;
        };
        viewers
            .get(id)
            .and_then(|conn| conn.state.lock().ok().map(|state| state.connected))
            .unwrap_or(false)
    }

    /// Viewers with a live connection, and the number of slots in total.
    ///
    /// Two counts because they answer different questions: the second says
    /// whether anyone is being served at all, the first says how many people are
    /// actually watching.
    pub fn viewer_counts(&self) -> (usize, usize) {
        self.viewers
            .lock()
            .map(|viewers| {
                let connected = viewers
                    .values()
                    .filter(|conn| {
                        conn.state
                            .lock()
                            .map(|state| state.connected)
                            .unwrap_or(false)
                    })
                    .count();
                (connected, viewers.len())
            })
            .unwrap_or((0, 0))
    }

    pub fn status(&self) -> &'static str {
        let (connected, slots) = self.viewer_counts();
        if slots == 0 {
            return "Waiting for someone to join";
        }
        if connected > 0 {
            return "Connected";
        }
        // Every slot is still negotiating. Whether they have finished gathering
        // is the only thing left to say about it.
        let gathering = self
            .viewers
            .lock()
            .map(|viewers| {
                viewers.values().any(|conn| {
                    !conn
                        .state
                        .lock()
                        .map(|s| s.gathering_complete)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(true);
        if gathering {
            "Gathering"
        } else {
            "Waiting for peer"
        }
    }

    /// True while at least one viewer has a live connection.
    pub fn is_connected(&self) -> bool {
        self.viewer_counts().0 > 0
    }

    pub async fn close(&self) {
        let conns: Vec<Arc<dyn PeerConnection>> = self
            .viewers
            .lock()
            .map(|mut viewers| viewers.drain().map(|(_, conn)| conn.pc).collect())
            .unwrap_or_default();
        for pc in conns {
            let _ = pc.close().await;
        }
    }
}

pub struct Viewer {
    pc: Arc<dyn PeerConnection>,
    state: Arc<Mutex<HandlerState>>,
}

impl Viewer {
    pub async fn new(udp_addrs: Vec<String>, on_packet: PacketCallback) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(HandlerState::default()));
        let pc = build_pc(state.clone(), Some(on_packet), udp_addrs).await?;
        pc.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
        pc.add_transceiver_from_kind(
            RtpCodecKind::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(Self { pc, state })
    }

    pub async fn answer_offer(&self, code: &str) -> Result<String, String> {
        let offer = signal::decode(code).map_err(|error| format!("invalid offer code: {error}"))?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|error| error.to_string())?;
        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|error| error.to_string())?;
        self.pc
            .set_local_description(answer)
            .await
            .map_err(|error| error.to_string())?;
        wait_for_gathering(&self.state).await?;
        let description = self
            .pc
            .local_description()
            .await
            .ok_or_else(|| "no local description".to_string())?;
        Ok(signal::encode(&description))
    }

    pub fn status(&self) -> &'static str {
        let current = self.state.lock().ok();
        match current.as_deref() {
            Some(state) if state.connected => "Connected",
            Some(state) if state.gathering_complete => "Waiting for peer",
            _ => "Gathering",
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().map(|s| s.connected).unwrap_or(false)
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}
