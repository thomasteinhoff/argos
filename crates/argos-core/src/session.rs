use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
use crate::signal;

pub const VIDEO_PT: u8 = 96;
pub const AUDIO_PT: u8 = 111;
const VIDEO_SSRC: u32 = 0x5a5a_77e1;
const AUDIO_SSRC: u32 = 0x5a5a_77e2;
const MTU: usize = 1200;
const GATHER_TIMEOUT: Duration = Duration::from_secs(10);

pub fn runtime() -> Arc<dyn Runtime> {
    static RUNTIME: OnceLock<Arc<dyn Runtime>> = OnceLock::new();
    Arc::clone(RUNTIME.get_or_init(|| default_runtime().expect("no runtime feature enabled")))
}

fn tokio() -> &'static tokio::runtime::Runtime {
    static TOKIO: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
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
    on_packet: Option<Arc<dyn Fn(&Packet) + Send + Sync>>,
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
        if state == RTCPeerConnectionState::Connected {
            if let Ok(mut current) = self.state.lock() {
                current.connected = true;
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
    on_packet: Option<Arc<dyn Fn(&Packet) + Send + Sync>>,
    udp_addrs: Vec<String>,
) -> Result<Arc<dyn PeerConnection>, String> {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: h264_codec(),
                payload_type: VIDEO_PT,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )
        .map_err(|error| error.to_string())?;
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: opus_codec(),
                payload_type: AUDIO_PT,
                ..Default::default()
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

pub struct Sharer {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticRTP>,
    audio_track: Arc<TrackLocalStaticRTP>,
    packetizer: Mutex<Packetizer>,
    audio_packetizer: Mutex<Packetizer>,
    state: Arc<Mutex<HandlerState>>,
}

impl Sharer {
    pub async fn new(udp_addrs: Vec<String>) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(HandlerState::default()));
        let pc = build_pc(state.clone(), None, udp_addrs).await?;
        let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
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
        )));
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| error.to_string())?;
        let audio_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
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
        )));
        pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            pc,
            track,
            audio_track,
            packetizer: Mutex::new(Packetizer::new(VIDEO_SSRC, VIDEO_PT, MTU)),
            audio_packetizer: Mutex::new(Packetizer::new(AUDIO_SSRC, AUDIO_PT, MTU)),
            state,
        })
    }

    pub async fn create_offer(&self) -> Result<String, String> {
        let offer = self
            .pc
            .create_offer(None)
            .await
            .map_err(|error| error.to_string())?;
        self.pc
            .set_local_description(offer)
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

    pub async fn set_answer(&self, code: &str) -> Result<(), String> {
        let answer =
            signal::decode(code).map_err(|error| format!("invalid answer code: {error}"))?;
        self.pc
            .set_remote_description(answer)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn send_frame(&self, bitstream: &[u8], timestamp: u32) -> Result<(), String> {
        let nalus: Vec<&[u8]> = h264::AnnexBIter::new(bitstream).collect();
        let packets = {
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
        for packet in packets {
            self.track
                .write_rtp(packet)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub async fn send_audio(&self, opus: &[u8], timestamp: u32) -> Result<(), String> {
        let packets = {
            let mut packetizer = self
                .audio_packetizer
                .lock()
                .map_err(|_| "audio packetizer lock poisoned".to_string())?;
            packetizer.packetize(opus, timestamp, true)
        };
        for packet in packets {
            self.audio_track
                .write_rtp(packet)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
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

pub struct Viewer {
    pc: Arc<dyn PeerConnection>,
    state: Arc<Mutex<HandlerState>>,
}

impl Viewer {
    pub async fn new(
        udp_addrs: Vec<String>,
        on_packet: Arc<dyn Fn(&Packet) + Send + Sync>,
    ) -> Result<Self, String> {
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
