use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use argos_core::diagnose::{self, diagnose, Bottleneck};
use argos_core::metrics::{AudioMetrics, Ema, ReceiverMetrics, SenderMetrics, StageTimer};
use argos_core::quality::{Controller as QualityController, Decision, Report};
use argos_core::{h264, lan, session, Packet};
use argos_media::audio::{
    AudioCapture, AudioPlayback, OpusAudioDecoder, OpusAudioEncoder, FRAME_SAMPLES,
};
use argos_media::capture::{self, CaptureSession, MonitorInfo};
use argos_media::decode::{DecodedFrame, H264Decoder};
use argos_media::encode::H264Encoder;

use crate::config::{self, AppConfig};

/// Fallback interval for periodic intra frames.
///
/// This used to be 2 s and was the sharer's *only* recovery mechanism, because
/// the rtc transport offers no RTCP feedback path (see `session.rs`). It is now
/// a backstop behind on-demand requests over the LAN channel: every keyframe is
/// a burst on a Radmin tunnel, and a burst is what causes loss in the first
/// place. `KEYFRAME_REQUEST_INTERVAL` is the same quantity seen from the other
/// side.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(4);

/// Floor between keyframes forced on a viewer's request.
///
/// A keyframe costs a full-frame burst. Honouring every request from a viewer
/// on a bad link would turn a recovery mechanism into a denial of service, so
/// this rate-limits a peer regardless of how often it asks.
const KEYFRAME_REQUEST_FLOOR: Duration = Duration::from_millis(500);

/// How often the viewer may ask for a keyframe.
///
/// Two seconds is comfortably inside the `KEYFRAME_INTERVAL` fallback, so a
/// viewer waiting on an intra frame gets one sooner rather than at the interval
/// anyway — which would make the whole mechanism pointless.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(2);

/// How often the viewer reports its view of the link to the sharer. Matches the
/// resolution of the receiver's own fps/loss measurement.
const REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Loss above this, in a single measurement window, makes the viewer ask for a
/// keyframe.
///
/// Deliberately well below the adaptive controller's 3% down-threshold: this is
/// a different job. A keyframe is cheap insurance and recovers in one frame,
/// whereas stepping resolution down costs quality and takes seconds to undo.
const LOSS_BURST: f32 = 2.0;

/// How stale a viewer's link report may be before it is ignored.
///
/// The sharer sends one stream to everyone and adapts its single resolution to
/// the worst of them, so a viewer's report keeps steering the ladder for as long
/// as it is fresh. Past this the viewer has almost certainly gone, and acting on
/// its last-known loss would hold the resolution down for nobody.
const VIEWER_REPORT_TTL: Duration = Duration::from_secs(3);

/// How often the sharer sweeps its viewer slots for connections that are done.
///
/// Throttled because the sweep closes peer connections, and that is not work
/// worth doing sixty times a second. The grace periods that decide whether a
/// connection counts as done are in seconds, so a half-second sweep cannot miss
/// anything.
const VIEWER_SWEEP_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, PartialEq, Eq)]
enum Screen {
    Home,
    Share,
    Peer(String),
    View,
}

struct Preview {
    texture: egui::TextureHandle,
    width: u32,
    height: u32,
}

enum EncodeMsg {
    Frame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
}

/// Packs a resolution choice into the atomic the encode worker polls.
///
/// A negative value means "native" (no scaling); anything else is a target
/// height in pixels. The target travels as an atomic rather than as an
/// [`EncodeMsg`] because it must not share the frame queue: that queue is full
/// exactly when this matters (the encoder is behind), and a control message
/// behind a backlog of stale frames would be dropped or applied minutes late.
fn encode_target(height: Option<u32>) -> i32 {
    height.map_or(-1, |height| height as i32)
}

fn decode_target(value: i32) -> Option<u32> {
    (value >= 0).then_some(value as u32)
}

#[derive(Default)]
struct EncodeStats {
    frames_sent: u64,
    encode_ms: f32,
    encode_errors: u64,
    error: Option<String>,
}

fn encode_worker(
    rx: Receiver<EncodeMsg>,
    sharer: Arc<session::Sharer>,
    mut encoder: H264Encoder,
    stats: Arc<Mutex<EncodeStats>>,
    metrics: Arc<SenderMetrics>,
    force_keyframe: Arc<AtomicBool>,
    target_height: Arc<AtomicI32>,
) {
    // RTP timestamps come from elapsed wall time, not from the frame rate.
    // The counter form (`timestamp += 90_000 / fps`) claims a fixed interval
    // per frame, which is false for any frame that is late, coalesced or
    // dropped — so the sender's clock outruns real time and the receiver has to
    // discard good frames to stay in sync. The frame rate now lives only in the
    // encoder's own configuration, set before this thread starts.
    let clock = h264::Clock::new();
    let mut last_keyframe = Instant::now();
    // The target the encoder is currently configured for. Compared against the
    // shared atomic on every frame, so a resolution change is picked up on the
    // next frame regardless of how backed up the frame queue is.
    let mut applied_target = target_height.load(Ordering::Relaxed);
    // Set when an intra frame has been asked for but not yet encoded. The flag
    // outlives the request, because the frame that carries the intra arrives
    // later — and that frame's size is the number worth measuring, not the
    // request's.
    let mut pending_keyframe = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            EncodeMsg::Frame {
                rgba,
                width,
                height,
            } => {
                metrics.captured_width.set(width as u64);
                metrics.captured_height.set(height as u64);
                // Pick up an adaptive or user resolution change. This is polled
                // rather than queued: the frame queue fills up precisely when
                // the encoder is behind, which is exactly when the controller
                // is trying to lower the resolution, so a queued control
                // message would be dropped by the very congestion it exists to
                // relieve.
                let target = target_height.load(Ordering::Relaxed);
                if target != applied_target {
                    applied_target = target;
                    encoder.set_target_height(decode_target(target));
                    // A resolution change is unviewable until the next intra
                    // frame, so this one is never optional.
                    encoder.force_keyframe();
                    last_keyframe = Instant::now();
                    pending_keyframe = true;
                }
                // A viewer asking for recovery. An atomic flag rather than a
                // message: the frame queue below is small and fills up exactly
                // when the machine is struggling, and a dropped request would
                // strand a viewer waiting for an intra frame it was promised.
                // Requests that arrive faster than the floor are coalesced — one
                // intra frame serves every request in the interval.
                let requested = force_keyframe.swap(false, Ordering::Relaxed);
                // Backstop for a request that never arrived: the LAN channel is
                // UDP, and a manual-code session has no channel at all.
                let overdue = last_keyframe.elapsed() >= KEYFRAME_INTERVAL;
                if (requested && last_keyframe.elapsed() >= KEYFRAME_REQUEST_FLOOR) || overdue {
                    encoder.force_keyframe();
                    last_keyframe = Instant::now();
                    pending_keyframe = true;
                }
                // Conversion and encoding are timed separately: they have very
                // different costs and very different fixes. The conversion is
                // our own scalar code and, when it scales, does five integer
                // divisions per output pixel; the encode is openh264.
                let converted = {
                    let _convert = StageTimer::new(&metrics.convert);
                    encoder.convert(&rgba, width, height)
                };
                let bitstream = match converted {
                    Ok(dims) => {
                        metrics.encoded_width.set(dims.0 as u64);
                        metrics.encoded_height.set(dims.1 as u64);
                        let _encode = StageTimer::new(&metrics.encode);
                        encoder.encode_planes(dims.0, dims.1)
                    }
                    Err(error) => Err(error),
                };
                match bitstream {
                    Ok(bitstream) => {
                        // Sampled at send time, not at receipt: the clock is
                        // measuring how long this frame waited, which is exactly
                        // the latency a receiver has to absorb.
                        let ts = clock.ticks(Instant::now());
                        let size = bitstream.len() as u64;
                        metrics.encoded_bytes.add(size);
                        if pending_keyframe {
                            // The frame following a forced intra is an IDR, so
                            // this is the size of the burst that goes out on the
                            // wire. If this number is large, it is a plausible
                            // cause of the loss it is meant to help recover from.
                            metrics.keyframes.incr();
                            metrics.last_keyframe_bytes.set(size);
                            pending_keyframe = false;
                        }
                        match session::block_on(sharer.send_frame(&bitstream, ts, &metrics)) {
                            Ok(()) => {
                                metrics.encoded.record();
                                if let Ok(mut stats) = stats.lock() {
                                    stats.frames_sent += 1;
                                    stats.encode_ms = metrics.encode.mean_ms();
                                }
                            }
                            Err(error) => {
                                if let Ok(mut stats) = stats.lock() {
                                    if stats.error.is_none() {
                                        stats.error = Some(error);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        metrics.encode_errors.incr();
                        if let Ok(mut stats) = stats.lock() {
                            stats.encode_errors += 1;
                            if stats.error.is_none() {
                                stats.error = Some(error);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One viewer's slot in a share.
///
/// A share is a single encode fanned out over one peer connection per viewer, so
/// the UI tracks viewers as peers rather than as a connection. Everything keyed
/// off "who is this message from" — keyframe requests, link reports, answers —
/// resolves through this list.
struct ViewerSlot {
    /// Peer id over the LAN channel, or a generated id for a manual code.
    id: String,
    /// Who they are, for the panel. Empty when the peer sent none.
    name: String,
    /// Whether their connection is up. Refreshed by the sweep; the sharer is the
    /// only thing that actually knows, and this only has to be close enough to
    /// label the panel.
    connected: bool,
    /// When the offer went out, which is the start of both the answer window and
    /// the "connecting…" timer shown beside a viewer.
    offered: Instant,
    /// Their last link measurement. The sharer adapts to the worst report it has
    /// from any viewer, so these are kept per viewer rather than blended.
    loss: f32,
    fps: f32,
    /// Everything else their end measured: decode time, render drops, whether
    /// they are waiting on a keyframe.
    ///
    /// Read-only, and deliberately so. This is the evidence for deciding whether
    /// the ladder should move; it is not itself a reason to move it, because
    /// "this machine is slow" and "this link is bad" want opposite responses
    /// from a setting that everybody shares.
    diag: lan::Diagnostics,
    /// When that measurement arrived, so a viewer's silence ages out instead of
    /// steering the ladder forever.
    reported: Option<Instant>,
    /// When this viewer last asked for an intra frame, for the per-viewer floor.
    /// Without it, several viewers losing packets at once would each be inside
    /// their own window and the request would multiply.
    last_keyframe_request: Instant,
}

impl ViewerSlot {
    fn new(id: &str, name: &str, offered: Instant) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            connected: false,
            offered,
            loss: 0.0,
            fps: 0.0,
            diag: lan::Diagnostics::default(),
            reported: None,
            // Start well outside the floor so the first request a viewer makes is
            // honoured rather than eaten by the limiter. There is no earlier
            // request to be close to — this is the start of the session.
            last_keyframe_request: offered - KEYFRAME_REQUEST_FLOOR - Duration::from_millis(1),
        }
    }

    /// True while this slot is still worth an answer or already serving.
    fn is_live(&self, now: Instant) -> bool {
        self.connected || now.duration_since(self.offered) < VIEWER_ANSWER_TIMEOUT
    }

    fn label(&self) -> &str {
        if self.name.trim().is_empty() {
            "Anonymous"
        } else {
            self.name.as_str()
        }
    }

    /// One line about how this viewer's end of the stream is doing.
    ///
    /// Three states, because they want different words. A viewer still
    /// negotiating has said nothing and has nothing to say. A viewer whose
    /// reports have stopped being fresh has gone quiet, which is not the same as
    /// being healthy — the whole point of the report TTL is that silence stops
    /// counting as evidence. A viewer reporting gets its numbers and a verdict on
    /// them.
    ///
    /// `source_fps` is the stream's own rate, because a decode time cannot be
    /// called too slow without knowing how often a frame is due.
    fn summary(&self, now: Instant, source_fps: u32) -> RichText {
        let fresh = self
            .reported
            .is_some_and(|at| now.duration_since(at) <= VIEWER_REPORT_TTL);
        if !self.connected && !fresh {
            return RichText::new(format!(
                "{}: connecting… {}s",
                self.label(),
                now.duration_since(self.offered).as_secs()
            ))
            .color(Color32::from_rgb(220, 200, 120));
        }
        if !fresh {
            return RichText::new(format!("{}: not reporting", self.label()))
                .color(Color32::from_rgb(220, 200, 120));
        }
        let verdict = diagnose(&self.diag, self.loss, source_fps);
        let numbers = format!("{:.0} fps, {:.1}% loss", self.fps, self.loss);
        let text = format!(
            "{}: {numbers} — {}",
            self.label(),
            verdict.describe(source_fps)
        );
        if verdict == Bottleneck::Ok {
            RichText::new(text).weak()
        } else {
            // Anything that is not "ok" is worth interrupting a list for. The
            // whole reason these numbers cross the network is that one person
            // said their stream was stuttering and the sharer could not see it.
            RichText::new(text).color(Color32::from_rgb(220, 200, 120))
        }
    }
}

/// Recent decode samples a stage mean covers.
///
/// A second's worth at 60 fps. Long enough that a stall which has already ended
/// is still visible, short enough that the mean still describes the machine as it
/// is now rather than as it was when the session started.
const DIAG_DECODE_SAMPLES: usize = 60;

/// Largest the video is ever shown in the windowed UI.
///
/// A 1080p source used to land exactly here, so any lower resolution now scales
/// up to match. Fullscreen passes an unbounded maximum instead: the cap exists to
/// keep a large stream from becoming a postage stamp in a sidebar, and a
/// fullscreen window is not a sidebar.
const VIEWER_IMAGE_CAP: egui::Vec2 = egui::vec2(1024.0, 576.0);

/// How long the fullscreen controls stay on screen after the mouse last moved.
///
/// Long enough to reach for a button without a panic, short enough that the
/// picture ends up alone. A HUD that never went away would sit permanently on
/// top of the thing the user went fullscreen to look at, which is the one thing
/// fullscreen is for.
const HUD_LINGER: Duration = Duration::from_secs(3);

/// How long the controls take to fade out once the linger is up.
///
/// Long enough that a pointer which strays across the bottom of the screen for
/// half a second does not make the controls blink, short enough that a
/// deliberate hand is already moving by the time they are gone.
const HUD_FADE: Duration = Duration::from_secs(1);

/// Whether this frame's input asks to toggle fullscreen.
///
/// Three keys for one action, because none of them is sufficient alone. F11 is
/// what every other program uses and what a person will try first. Plain F is
/// what works when a laptop's function keys need a modifier to reach. Escape is
/// the way out from anywhere, and for exactly that reason it only counts while
/// fullscreen is already on: swallowing Escape in a windowed dialog would leave
/// it with no way to dismiss itself.
///
/// The modifiers on `F` are checked because a bare letter shortcut that fires
/// through Ctrl and Alt is a well-worn way to make a program's own shortcuts
/// stop working, and Ctrl here is already the pipeline readout. Alt and Shift are
/// excluded even though nothing else claims them, because the day something does
/// it will be too late to work out which of the two should have won.
///
/// `fullscreen` is the current state rather than whether a session exists. The
/// caller only reaches this with a view in hand, and F11 on a share screen with
/// nothing to enlarge should do nothing at all rather than emptying the window.
fn fullscreen_key(input: &egui::InputState, fullscreen: bool) -> bool {
    if input.key_pressed(egui::Key::F11) {
        return true;
    }
    if input.key_pressed(egui::Key::Escape) && fullscreen {
        return true;
    }
    input.key_pressed(egui::Key::F)
        && !input.modifiers.ctrl
        && !input.modifiers.alt
        && !input.modifiers.command
        && !input.modifiers.shift
}

/// The measurement the adaptive controller should act on, as `(loss, fps)`.
///
/// One encode feeds everyone and the ladder has one rung to move, so it has to
/// answer for the worst link rather than an average of them: averaging would let
/// a viewer on a bad link be carried by someone on a good one, which is the
/// failure this is meant to prevent. `None` when nobody has reported recently
/// enough to be evidence of anything — a viewer that has gone quiet must stop
/// steering the ladder, or the resolution stays pinned down for nobody.
fn worst_link(slots: &[ViewerSlot], now: Instant) -> Option<(f32, f32)> {
    slots
        .iter()
        .filter(|slot| {
            slot.reported
                .is_some_and(|at| now.duration_since(at) <= VIEWER_REPORT_TTL)
        })
        .map(|slot| (slot.loss, slot.fps))
        .reduce(|(loss, fps), (loss_b, fps_b)| (loss.max(loss_b), fps.min(fps_b)))
}

/// How long an unanswered offer is worth waiting on before the slot is reused.
///
/// Comfortably past ICE gathering, since the sharer only returns from the offer
/// once candidates are in: a viewer who answers slower than that was never going
/// to, and holding the slot for them means holding it against their next request.
const VIEWER_ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

struct ShareSession {
    sharer: Arc<session::Sharer>,
    /// Viewers being served, in the order they arrived.
    viewers: Vec<ViewerSlot>,
    /// The manual-code offer currently on screen, and the slot it belongs to.
    ///
    /// Manual codes are the same thing a LAN request produces, minus the peer
    /// that asked, so they take a slot with a generated id. More than one can
    /// exist; only the newest is shown, since the panel has one paste box and a
    /// code that has been answered is dead.
    manual_offer: Option<(String, String)>,
    /// Counter behind the generated manual ids, so a second manual code gets its
    /// own connection instead of replacing the first.
    manual_seq: u32,
    answer_input: String,
    error: Option<String>,
    tx: SyncSender<EncodeMsg>,
    join: Option<JoinHandle<()>>,
    stats: Arc<Mutex<EncodeStats>>,
    /// Stage timings and drop counts shared with the capture thread.
    metrics: Arc<SenderMetrics>,
    last_encode: Instant,
    audio_capture: Option<AudioCapture>,
    audio_encoder: Option<OpusAudioEncoder>,
    audio_error: Option<String>,
    audio_timestamp: u32,
    audio_frames_sent: u64,
    /// Adaptive resolution, driven by the viewer's reports over the LAN channel.
    /// Pure state machine: see `argos_core::quality`.
    quality: QualityController,
    /// Frame rate the encoder was built with.
    ///
    /// Held here rather than read off the app because the picker that sets it is
    /// disabled while streaming, and this is what the viewers are told: a decode
    /// time means nothing without knowing how often a frame is due, and
    /// advertising a rate the encoder is not running at would make every
    /// viewer's verdict wrong in a way that looks like a bug in their machine.
    frame_rate: u32,
    /// Target height the encode worker polls. An atomic, not a message, so a
    /// resolution change is never lost behind a full frame queue.
    target_height: Arc<AtomicI32>,
    /// Why the height last changed, for the UI.
    quality_note: Option<String>,
    /// Encoder-handoff counters as of the last report, so the drop *rate* over
    /// the interval — not the lifetime total — can be fed to the controller.
    load_offered: u64,
    load_dropped: u64,
    /// Set by a viewer's keyframe request, consumed by the encode worker.
    ///
    /// One flag for the whole share rather than one per viewer, and that is the
    /// point: there is one encode, so one intra frame serves everyone who is
    /// waiting on one.
    force_keyframe: Arc<AtomicBool>,
    /// When the audience list last went out, and what it said.
    ///
    /// Both halves are needed. The list changes rarely and the channel loses
    /// datagrams, so sending only on change would leave a viewer who missed one
    /// datagram with a permanently wrong answer and no way to tell. The
    /// keepalive covers the loss; the signature keeps a session with nobody
    /// changing from spending a datagram per viewer per frame on it.
    roster_sent: Option<Instant>,
    roster_signature: Vec<String>,
}

impl ShareSession {
    /// The audience as the sharer sees it.
    ///
    /// Slots rather than the sharer's connection map, because the UI already
    /// keeps them in arrival order and knows which ones are still negotiating.
    /// A viewer is listed from the moment they ask, not from the moment their
    /// connection comes up: someone who has typed the code and is waiting is
    /// watching as far as anybody in the room is concerned.
    fn audience(&self) -> Vec<lan::RosterEntry> {
        self.viewers
            .iter()
            .map(|slot| lan::RosterEntry {
                id: slot.id.clone(),
                name: slot.name.clone(),
                connected: slot.connected,
            })
            .collect()
    }

    /// Pushes the audience list to every viewer, on change or on the keepalive.
    ///
    /// Includes the stream's frame rate and height because a viewer cannot work
    /// either out: it knows what it decoded and not what it was sent. The frame
    /// rate is what makes its own decode time readable as fast or slow, so
    /// without it the diagnosis would have no budget to compare against.
    ///
    /// Rate-limited rather than sent on every change because a join and a leave
    /// can happen back to back, and each of those is a datagram per viewer.
    fn broadcast_roster(&mut self, height: Option<u32>, lan: &lan::Lan) {
        let now = Instant::now();
        let audience = self.audience();
        let signature: Vec<String> = audience
            .iter()
            .map(|entry| format!("{}|{}|{}", entry.id, entry.name, entry.connected))
            .collect();
        let due = self
            .roster_sent
            .is_none_or(|at| now.duration_since(at) >= ROSTER_KEEPALIVE);
        if !due && signature == self.roster_signature {
            return;
        }
        self.roster_sent = Some(now);
        self.roster_signature = signature;
        for slot in &self.viewers {
            // Unconnected viewers get it too. Their LAN channel is already
            // working — they sent a request and answered an offer over it — and
            // this way the list is correct before their first frame arrives
            // rather than appearing a keyframe interval later.
            lan.send_roster(&slot.id, &audience, self.frame_rate, height);
        }
    }

    /// Frames the encode handoff discarded since the last call, as a percentage
    /// of those it was offered.
    ///
    /// The lifetime counters only report the average since the stream started,
    /// which decays as clean periods accumulate and eventually stops seeing a
    /// fresh overload. The controller wants the rate over the report interval.
    fn encoder_drops(&mut self) -> f32 {
        let offered = self.metrics.queued.frames();
        let dropped = self.metrics.queued.dropped();
        let offered_delta = offered.saturating_sub(self.load_offered);
        let dropped_delta = dropped.saturating_sub(self.load_dropped);
        self.load_offered = offered;
        self.load_dropped = dropped;
        if offered_delta == 0 {
            // Nothing was offered, so nothing was refused. Absence of demand is
            // not evidence of pressure.
            return 0.0;
        }
        dropped_delta as f32 / offered_delta as f32 * 100.0
    }
}

#[derive(Default)]
struct ViewStats {
    packets: u64,
    bytes: u64,
    aus: u64,
    decoded: u64,
    decode_errors: u64,
    decode_none: u64,
    first_width: u32,
    first_height: u32,
    first_error: Option<String>,
    audio_packets: u64,
    audio_decoded: u64,
    audio_errors: u64,
    audio_error: Option<String>,
    base_seq: Option<u16>,
    highest_seq: Option<u16>,
    lost: u64,
}

struct Quality {
    last: Instant,
    last_decoded: u64,
    last_bytes: u64,
    last_lost: u64,
    last_received: u64,
    fps: f32,
    mbps: f32,
    loss: f32,
    /// When the last report went to the sharer, rate-limited so a healthy link
    /// costs nothing and a broken one cannot flood the channel.
    last_report: Instant,
    /// When this viewer last asked the sharer for a keyframe. Rate limiting
    /// belongs on the requester: the sharer cannot tell a stream of genuine
    /// requests from one viewer stuck in a retry loop.
    last_keyframe_request: Instant,
    /// Loss measured in the window that triggered the last request, for display.
    requested_at_loss: f32,
    /// How many keyframes this session has asked for. Non-zero is what makes the
    /// "recovered" line meaningful; without it, a session that never lost
    /// anything would claim to have recovered from a request it never sent.
    recoveries: u64,
    /// Packets arrived in the last window but none of them decoded.
    ///
    /// Distinct from loss, which is measured by sequence gaps and so reports a
    /// perfectly intact stream here. This is the decoder waiting on an intra
    /// frame it will not be given, which is the one failure loss cannot see.
    starved: bool,
    /// Decode errors per second over the last window.
    ///
    /// A rate rather than a total because a viewer that made ten errors an hour
    /// ago and none since is not currently in trouble, and the sharer needs to
    /// be able to tell that from a viewer who is failing right now.
    decode_errors: f32,
    /// Share of decoded frames that were replaced before display, `0..100`.
    ///
    /// The one failure no amount of keyframe recovery fixes: the frame decoded
    /// fine and the machine could not show it.
    render_drops: f32,
    /// Worst `decode_errors` seen since the counters were last cleared.
    ///
    /// Carried across windows because a burst that recovers between two reports
    /// is still a burst, and a per-window rate would quietly drop it.
    decode_errors_peak: f32,
    /// Worst `render_drops` seen since the counters were last cleared.
    render_drops_peak: f32,
    /// Counter baselines for the two rates above, read once when the window
    /// closed so a rate is a difference between two samples rather than a
    /// division of two separately-taken reads of the same counter.
    last_errors: u64,
    last_dropped: u64,
    last_shown: u64,
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            last_decoded: 0,
            last_bytes: 0,
            last_lost: 0,
            last_received: 0,
            fps: 0.0,
            mbps: 0.0,
            loss: 0.0,
            // Both start "long ago" so the first measurement is not rate limited.
            last_report: Instant::now() - REPORT_INTERVAL - Duration::from_millis(1),
            last_keyframe_request: Instant::now()
                - KEYFRAME_REQUEST_INTERVAL
                - Duration::from_millis(1),
            requested_at_loss: 0.0,
            recoveries: 0,
            starved: false,
            decode_errors: 0.0,
            render_drops: 0.0,
            decode_errors_peak: 0.0,
            render_drops_peak: 0.0,
            last_errors: 0,
            last_dropped: 0,
            last_shown: 0,
        }
    }
}

impl Quality {
    /// This viewer's measurements, as sent to the sharer.
    ///
    /// Peaks rather than window rates for the two failure counts, because what a
    /// sharer needs to know is whether this viewer has *ever* been in trouble
    /// recently, not whether it happened to be clean in the half-second window
    /// that happened to contain a report.
    ///
    /// The stage means cover recent frames rather than the whole session. This
    /// number exists to be compared against a frame budget, and a session mean
    /// cannot answer that: a decoder that misses its budget on every frame and one
    /// that sits comfortably inside it average to something that is below both.
    /// What matters is how the frames being decoded now went.
    fn link_report(&self, metrics: &ReceiverMetrics) -> lan::LinkReport {
        lan::LinkReport {
            loss: self.loss,
            fps: self.fps,
            diag: lan::Diagnostics {
                decode_ms: metrics
                    .decode
                    .recent_mean_ms(DIAG_DECODE_SAMPLES)
                    .unwrap_or(0.0),
                present_ms: metrics
                    .present
                    .recent_mean_ms(DIAG_DECODE_SAMPLES)
                    .unwrap_or(0.0),
                render_drops: self.render_drops_peak,
                decode_errors: self.decode_errors_peak,
                waiting_keyframe: self.starved,
            },
        }
    }
}

struct Reconnect {
    attempts: u32,
    last_request: Instant,
}

struct ViewSession {
    viewer: Arc<session::Viewer>,
    answer_code: Option<String>,
    error: Option<String>,
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    stats: Arc<Mutex<ViewStats>>,
    /// Lock-free counters and stage timings for this receive session, shared
    /// with the transport's packet callback.
    metrics: Arc<ReceiverMetrics>,
    texture: Option<Preview>,
    /// Width-to-height ratio of the first decoded frame.
    ///
    /// Remembered for the whole session so the on-screen box is sized from the
    /// source shape, not the current decoded resolution. When the sharer drops
    /// the resolution the picture is stretched into the same rectangle rather
    /// than shrinking, so the window is stable while watching.
    display_aspect: Option<f32>,
    audio_playback: Option<Arc<AudioPlayback>>,
    audio_error: Option<String>,
    quality: Quality,
    was_connected: bool,
    reconnect: Option<Reconnect>,
    lan_peer: Option<String>,
    /// Peer this viewer watches. Keyframe requests and link reports go only to
    /// the sharer actually being watched, never to whoever else is on the
    /// network. `None` for a manual-code session, which is why those sessions
    /// get no recovery: there is no LAN channel to ask over.
    sharer_id: Option<String>,
    /// Everyone the sharer says is watching, and the shape of the stream.
    ///
    /// Only the sharer knows this. A viewer can see the other machines on the
    /// network, but not which of them are watching *this* stream, so it is
    /// pushed to us rather than discovered.
    roster: Vec<lan::RosterEntry>,
    /// When that list arrived. UDP loses datagrams and a list that quietly stops
    /// being updated would be indistinguishable from everyone leaving.
    roster_received: Option<Instant>,
    /// The stream's frame rate, which is what makes a decode time readable as
    /// fast or slow. Zero until the sharer says.
    source_fps: u32,
    /// The stream's target height; `None` means native.
    source_height: Option<u32>,
    /// This viewer's own peer id, so it can mark itself in the audience list.
    ///
    /// The sharer's roster is keyed by peer id, and a viewer has its own id from
    /// the start, so the two can be matched exactly. Matching on the displayed
    /// name instead would be one more thing that can be wrong — two people on the
    /// network can both be called "desk" — and a mismatched viewer would show up
    /// as a stranger in its own list.
    self_id: Option<String>,
    /// This viewer's own name, for the fallback list a manual-code session has.
    self_name: String,
    /// Whether the window is showing this stream alone.
    ///
    /// Kept on the session rather than the app so that it cannot outlive the
    /// thing it is a property of: a fullscreen flag that survived `stop_view`
    /// would leave the user staring at a black fullscreen window with no session
    /// behind it and no obvious way back.
    fullscreen: bool,
    /// When the mouse last moved, so the fullscreen controls can fade out.
    ///
    /// The HUD has to be reachable while the pointer is being moved towards it,
    /// so the fade is measured from the last movement rather than from the last
    /// frame. An idle timer would make the controls vanish under the pointer
    /// exactly when they are being aimed at.
    hud_idle_since: Instant,
    /// Where the pointer was last frame, or `None` if it has never been over the
    /// window.
    ///
    /// Compared rather than read as "did it move", because the comparison is the
    /// only thing that notices a pointer which has come to rest — a delta reports
    /// motion once and then nothing at all.
    last_pointer: Option<egui::Pos2>,
}

impl ViewSession {
    /// Everyone watching, including this viewer.
    ///
    /// From the sharer's list when there is one, because it is the only account
    /// that is complete.
    fn audience(&self) -> Vec<lan::RosterEntry> {
        audience_from(&self.roster, self.self_id.as_deref(), &self.self_name)
    }

    /// How visible the fullscreen controls should be right now, `0.0` to `1.0`.
    ///
    /// Fully opaque while the pointer is over them, whatever the idle clock says.
    /// A control that fades out while the pointer is on it is a control that
    /// cannot be pressed, and the pointer arriving is the strongest possible
    /// signal that the controls are wanted.
    ///
    /// `idle` is passed in rather than read from the clock so the arithmetic can
    /// be stated once and tested at the boundaries, which is where a fade of this
    /// shape goes wrong: a frame or a millisecond either side of a threshold is
    /// the difference between controls that come back and controls that stay
    /// invisible.
    fn hud_alpha(&self, idle: Duration) -> f32 {
        let fade_start = HUD_LINGER.saturating_sub(HUD_FADE);
        if idle <= fade_start {
            return 1.0;
        }
        if idle >= HUD_LINGER {
            return 0.0;
        }
        (HUD_LINGER.saturating_sub(idle)).as_secs_f32() / HUD_FADE.as_secs_f32()
    }

    /// How long the pointer has been still.
    fn hud_idle(&self, now: Instant) -> Duration {
        now.duration_since(self.hud_idle_since)
    }
}

/// The audience list, or the only viewer who can be known about it.
///
/// A manual-code session has no channel to be told over, so it falls back to a
/// list of one — itself. That is the honest answer: showing an empty room would
/// say nobody is watching when the person reading the screen is.
fn audience_from(
    roster: &[lan::RosterEntry],
    self_id: Option<&str>,
    self_name: &str,
) -> Vec<lan::RosterEntry> {
    if roster.is_empty() {
        return vec![lan::RosterEntry {
            id: self_id.unwrap_or_default().to_string(),
            name: self_name.to_string(),
            connected: true,
        }];
    }
    roster.to_vec()
}

/// Whether a list last received at `received` can still be believed.
///
/// `None` is not stale: nobody has claimed to know anything yet, which is a
/// different thing from having been told something and then not being told
/// again. Only the second one deserves a warning.
fn roster_is_stale(received: Option<Instant>, now: Instant) -> bool {
    received.is_some_and(|at| now.duration_since(at) > ROSTER_STALE)
}

/// How long an audience list may go without an update before it is shown as out
/// of date.
///
/// Three times the sharer's keepalive, so a single lost datagram is invisible
/// while a sharer that has actually gone quiet is not. Generous on purpose: the
/// failure this guards against is a list that stops updating and looks like a
/// room everybody left, which is worse than a slightly stale headcount.
const ROSTER_STALE: Duration = Duration::from_secs(9);

/// How often the sharer repeats the audience list whether or not it changed.
///
/// This channel is UDP and a datagram can simply not arrive, so a list sent only
/// on change would leave a viewer who missed one datagram permanently wrong with
/// no way to tell that anything is missing. Repeating it every few seconds costs
/// a few bytes and makes the wrong answer self-correcting.
const ROSTER_KEEPALIVE: Duration = Duration::from_secs(3);

/// Scales a colour's alpha, so a whole widget can be faded by hand.
///
/// egui has no per-widget opacity: a colour is either opaque or it is not, and
/// there is no way to say "draw this at 40%" and have it apply to the label, the
/// border and the fill together. Anything that fades therefore has to be handed
/// the fade as a number and apply it itself, which is why the audience list takes
/// one instead of being drawn with colours that happen to be pale.
fn with_alpha(colour: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        colour.r(),
        colour.g(),
        colour.b(),
        (colour.a() as f32 * alpha.clamp(0.0, 1.0)) as u8,
    )
}

/// The current style with every colour in it faded.
///
/// Covers the parts of the HUD that egui draws rather than us, which is mainly
/// the buttons. Applied to a `Ui` rather than to the whole context, so the
/// restore is automatic at the end of the scope and the rest of the window is
/// unaffected.
///
/// Deliberately does not include `override_text_color`, which would flatten the
/// green and amber the audience list uses into one grey — a faded control is
/// allowed to lose its colour, a faded *list* is not, because the colour there is
/// carrying meaning.
fn faded_style(base: &egui::Style, alpha: f32) -> egui::Style {
    let mut style = base.clone();
    let visuals = &mut style.visuals;
    // Both text colours are options in egui, and `None` means "inherit". Reading
    // them through their accessors resolves the inheritance against the style
    // this is a copy of, which is what makes the result a drop-in replacement
    // rather than a style whose labels have quietly stopped being bold.
    visuals.weak_text_color = Some(with_alpha(visuals.weak_text_color(), alpha));
    visuals.override_text_color = Some(with_alpha(visuals.strong_text_color(), alpha));
    visuals.hyperlink_color = with_alpha(visuals.hyperlink_color, alpha);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke.color = with_alpha(widget.fg_stroke.color, alpha);
        widget.bg_fill = with_alpha(widget.bg_fill, alpha);
        widget.weak_bg_fill = with_alpha(widget.weak_bg_fill, alpha);
        widget.bg_stroke.color = with_alpha(widget.bg_stroke.color, alpha);
    }
    style
}

/// The audience list, rendered the same way on both sides.
///
/// One function for both because the two sides already know the same things about
/// each other, and a viewer who is told "nobody else is watching" while the sharer
/// shows three rows would have no way to tell which of the two is true.
///
/// `me` is the local peer id, empty on the sharer's side where every row is
/// somebody else. `stale` is only ever true for a viewer: the sharer can read the
/// list straight out of its own slot list and has no reason to doubt it.
///
/// `alpha` is `1.0` everywhere except inside the fullscreen HUD. The colours here
/// say something — green means connected, amber means waiting — and scaling them
/// keeps them saying it at any opacity, where simply drawing them at reduced
/// opacity over black would shift what they mean.
fn viewer_list(
    ui: &mut egui::Ui,
    audience: &[lan::RosterEntry],
    me: &str,
    stale: bool,
    alpha: f32,
) {
    let connected = audience.iter().filter(|entry| entry.connected).count();
    if audience.is_empty() {
        ui.label(RichText::new("Nobody is watching yet.").weak());
        return;
    }
    let summary = if connected == audience.len() {
        format!("{connected} watching")
    } else {
        format!("{connected} of {} watching", audience.len())
    };
    if stale {
        // Said out loud, because the alternative is a headcount that quietly
        // stops being true and reads as fact.
        ui.label(
            RichText::new(format!("{summary} · list may be out of date"))
                .color(with_alpha(Color32::from_rgb(220, 200, 120), alpha)),
        );
    } else {
        ui.label(RichText::new(summary).weak());
    }
    for entry in audience {
        let is_me = !me.is_empty() && entry.id == me;
        let name = if is_me {
            format!("{} (you)", entry.label())
        } else {
            entry.label().to_string()
        };
        let state = if entry.connected {
            RichText::new("watching").color(with_alpha(Color32::from_rgb(150, 220, 150), alpha))
        } else {
            RichText::new("connecting…").color(with_alpha(Color32::from_rgb(220, 200, 120), alpha))
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong());
            ui.label(state);
        });
    }
}

/// The audience as a viewer sees it, under the picture.
///
/// A viewer's own copy of the sharer's list, plus the staleness warning. Kept as
/// its own wrapper because it has to answer two questions the sharer's call does
/// not: is this list current, and which of these people am I.
fn viewer_audience(ui: &mut egui::Ui, view: &ViewSession, alpha: f32) {
    let audience = view.audience();
    let stale = roster_is_stale(view.roster_received, Instant::now());
    let me = view.self_id.as_deref().unwrap_or_default();
    viewer_list(ui, &audience, me, stale, alpha);
}

pub struct ArgosApp {
    config: AppConfig,
    screen: Screen,
    show_settings: bool,
    /// Always-on-top pipeline readout, toggled with Ctrl+D. Exists because the
    /// interesting numbers (stage means, peak stalls, drop rates) are the ones
    /// that explain a freeze, and they have to be readable *while* it freezes.
    show_metrics: bool,
    /// Start of the current measurement window. Reset with the overlay's
    /// "Reset" button so a mean covers a known span rather than the whole
    /// session, which is what makes an intermittent stall visible.
    metrics_since: Instant,
    name_input: String,
    code_input: String,
    monitors: Vec<MonitorInfo>,
    selected_monitor: usize,
    capture: Option<CaptureSession>,
    preview: Option<Preview>,
    preview_active: bool,
    fps_frames: u64,
    fps_last: Instant,
    fps: f32,
    preview_error: Option<String>,
    share: Option<ShareSession>,
    share_error: Option<String>,
    share_height: Option<u32>,
    frame_rate: u32,
    live: bool,
    view: Option<ViewSession>,
    view_error: Option<String>,
    /// Adaptive resolution toggle. Off means the sharer ignores viewer reports
    /// and only the user's choice applies, which is the escape hatch if the
    /// controller ever fights a link it should not be judging.
    auto_quality: bool,
    lan: Option<lan::Lan>,
    lan_error: Option<String>,
    pending_view: Option<String>,
    /// When the sharer last swept its viewer slots for finished connections.
    last_viewer_sweep: Instant,
    radmin_exe: Option<std::path::PathBuf>,
    radmin_error: Option<String>,
}

impl ArgosApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = config::load();
        let (lan, lan_error) = match lan::Lan::start(config.name.clone()) {
            Ok(lan) => (Some(lan), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            name_input: config.name.clone(),
            code_input: String::new(),
            monitors: Vec::new(),
            selected_monitor: 0,
            capture: None,
            preview: None,
            preview_active: false,
            fps_frames: 0,
            fps_last: Instant::now(),
            fps: 0.0,
            preview_error: None,
            share: None,
            share_error: None,
            // The adaptive controller starts at the ceiling and walks down, so
            // the sensible default is the best we can send, not a compromise
            // picked before the link was known.
            share_height: None,
            frame_rate: 60,
            live: false,
            view: None,
            view_error: None,
            auto_quality: true,
            lan,
            lan_error,
            pending_view: None,
            last_viewer_sweep: Instant::now(),
            radmin_exe: crate::radmin::find_exe(),
            radmin_error: None,
            config,
            screen: Screen::Home,
            show_settings: false,
            show_metrics: false,
            metrics_since: Instant::now(),
        }
    }

    fn profile_label(&self) -> String {
        if self.config.name.is_empty() {
            "Set your name".to_owned()
        } else {
            self.config.name.clone()
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("ARGOS").strong().size(18.0))
                    .on_hover_text("Home")
                    .clicked()
                {
                    self.screen = Screen::Home;
                }
                ui.separator();
                if ui
                    .button(
                        RichText::new(self.profile_label()).color(Color32::from_rgb(160, 190, 255)),
                    )
                    .on_hover_text("Your name, as friends will see it")
                    .clicked()
                {
                    self.name_input.clone_from(&self.config.name);
                    self.show_settings = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(self.status_text()).color(Color32::GRAY));
                });
            });
        });
    }

    fn side_bar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("side_panel")
            .resizable(false)
            .default_width(240.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let selected = self.screen == Screen::Share;
                if ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::Button::selectable(selected, RichText::new("Share").strong()),
                    )
                    .clicked()
                {
                    self.screen = Screen::Share;
                }
                ui.separator();
                self.sidebar(ui);
            });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Profile")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(300.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("How should friends see you?");
                ui.add(
                    egui::TextEdit::singleline(&mut self.name_input)
                        .hint_text("Your name")
                        .desired_width(260.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let name = self.name_input.trim().to_string();
                        self.config.name = name.clone();
                        config::save(&self.config);
                        if let Some(lan) = &self.lan {
                            lan.set_name(name);
                        }
                        self.show_settings = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_settings = false;
                    }
                });
            });
        self.show_settings = open;
    }

    fn refresh_monitors(&mut self) {
        self.monitors = capture::list_monitors();
        if !self.monitors.is_empty() {
            self.selected_monitor = self.selected_monitor.min(self.monitors.len() - 1);
        }
    }

    fn monitor_label(info: &MonitorInfo) -> String {
        let primary = if info.is_primary { " [primary]" } else { "" };
        format!("{} ({}x{}){}", info.name, info.width, info.height, primary)
    }

    fn quality_label(height: Option<u32>) -> &'static str {
        match height {
            Some(720) => "720p",
            Some(540) => "540p",
            Some(360) => "360p",
            _ => "Native",
        }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.frame_rate.max(1) as u64)
    }

    fn start_capture(&mut self) -> Result<Option<MonitorInfo>, String> {
        let Some(source) = self.monitors.get(self.selected_monitor) else {
            return Ok(None);
        };
        let mut session = CaptureSession::new();
        session.set_interval(self.frame_interval());
        session.start(source)?;
        self.fps_last = Instant::now();
        self.fps_frames = 0;
        self.fps = 0.0;
        self.capture = Some(session);
        Ok(Some(source.clone()))
    }

    fn toggle_preview(&mut self) {
        self.preview_error = None;
        if self.preview_active {
            self.preview_active = false;
            self.preview = None;
            return;
        }
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.preview_error = Some(error);
                return;
            }
        }
        self.preview_active = true;
    }

    fn code_widget(ui: &mut egui::Ui, code: &str, rows: usize) {
        let mut display = code.to_owned();
        ui.add(
            egui::TextEdit::multiline(&mut display)
                .code_editor()
                .desired_width(380.0)
                .desired_rows(rows),
        );
    }

    /// Creates the share if it does not exist yet.
    ///
    /// One encode, one audio capture, one set of worker threads — all of it
    /// independent of how many people are watching. Viewers attach to it
    /// afterwards, which is what lets the second one join without disturbing the
    /// first.
    fn ensure_share(&mut self) -> Result<(), String> {
        self.share_error = None;
        if self.share.is_some() {
            return Ok(());
        }
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.share_error = Some(error.clone());
                return Err(error);
            }
        }
        let udp = vec!["0.0.0.0:0".to_string()];
        let sharer = Arc::new(session::Sharer::new(udp));
        let encoder_result = H264Encoder::new_at(self.frame_rate as f32).map(|mut encoder| {
            encoder.set_target_height(self.share_height);
            encoder
        });
        let encoder = match encoder_result {
            Ok(encoder) => encoder,
            Err(error) => {
                session::block_on(sharer.close());
                self.share_error = Some(error.clone());
                return Err(error);
            }
        };
        let (tx, rx) = sync_channel::<EncodeMsg>(4);
        let stats = Arc::new(Mutex::new(EncodeStats::default()));
        let worker_stats = Arc::clone(&stats);
        let worker_sharer = Arc::clone(&sharer);
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let worker_force_keyframe = Arc::clone(&force_keyframe);
        let target_height = Arc::new(AtomicI32::new(encode_target(self.share_height)));
        let worker_target_height = Arc::clone(&target_height);
        let worker_metrics = self
            .capture
            .as_ref()
            .map(|capture| Arc::clone(capture.metrics()))
            .unwrap_or_default();
        let join = match thread::Builder::new()
            .name("argos-encode".to_string())
            .spawn(move || {
                encode_worker(
                    rx,
                    worker_sharer,
                    encoder,
                    worker_stats,
                    worker_metrics,
                    worker_force_keyframe,
                    worker_target_height,
                )
            }) {
            Ok(join) => join,
            Err(error) => {
                session::block_on(sharer.close());
                let message = error.to_string();
                self.share_error = Some(message.clone());
                return Err(message);
            }
        };
        let (audio_capture, audio_encoder, audio_error) =
            match (AudioCapture::start(), OpusAudioEncoder::new()) {
                (Ok(capture), Ok(encoder)) => (Some(capture), Some(encoder), None),
                (capture, encoder) => {
                    let error = capture.err().or_else(|| encoder.err());
                    (None, None, error)
                }
            };
        if let Some(lan) = &self.lan {
            lan.set_sharing(true);
        }
        self.share = Some(ShareSession {
            sharer,
            viewers: Vec::new(),
            manual_offer: None,
            manual_seq: 0,
            answer_input: String::new(),
            error: None,
            tx,
            join: Some(join),
            stats,
            metrics: self
                .capture
                .as_ref()
                .map(|capture| Arc::clone(capture.metrics()))
                .unwrap_or_default(),
            last_encode: Instant::now(),
            audio_capture,
            audio_encoder,
            audio_error,
            audio_timestamp: 0,
            audio_frames_sent: 0,
            quality: QualityController::at_height(self.share_height),
            frame_rate: self.frame_rate,
            target_height,
            quality_note: None,
            load_offered: 0,
            load_dropped: 0,
            force_keyframe,
            roster_sent: None,
            roster_signature: Vec::new(),
        });
        Ok(())
    }

    /// Opens a connection for one viewer and returns the offer they must answer.
    ///
    /// Reuses the running share, and replaces whatever this viewer had before:
    /// a request that arrives twice over UDP, or from someone whose connection
    /// has already been swept, gets a fresh offer instead of silently failing to
    /// negotiate against a description that is already answered.
    fn add_share_viewer(&mut self, id: &str, name: &str) -> Result<String, String> {
        self.ensure_share()?;
        let now = Instant::now();
        let Some(share) = self.share.as_mut() else {
            return Err("no share to attach a viewer to".to_string());
        };
        // The sharer has just replaced this viewer's connection, so the old slot
        // describes nothing. Leaving it would double-count them in the panel and
        // let a stale loss reading steer the ladder.
        share.viewers.retain(|slot| slot.id != id);
        match session::block_on(share.sharer.add_viewer(id)) {
            Ok(offer) => {
                share.viewers.push(ViewerSlot::new(id, name, now));
                Ok(offer)
            }
            Err(error) => {
                share.error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Serves a peer who asked to watch.
    fn on_view_request(&mut self, id: &str, name: &str) {
        if !self.live {
            return;
        }
        let now = Instant::now();
        if let Some(share) = self.share.as_ref() {
            // Already serving them, or still waiting on an answer, and the
            // connection is not one that has settled down. A request that lands
            // here is a retransmit or a second click, and the offer in flight is
            // the right answer to both — re-offering would throw away a
            // negotiation that is about to succeed. The exception is a slot the
            // sharer knows is dead: that viewer needs a new connection, which is
            // what keeps a dropped link from reconnecting to nothing.
            if share
                .viewers
                .iter()
                .any(|slot| slot.id == id && slot.is_live(now))
                && !share.sharer.needs_reoffer(id)
            {
                return;
            }
        }
        let offer = match self.add_share_viewer(id, name) {
            Ok(offer) => offer,
            Err(_) => return,
        };
        if let Some(lan) = &self.lan {
            lan.send_offer(id, offer);
        }
    }

    /// Brings the viewer list in line with the connections that actually exist.
    ///
    /// The sharer is the only thing that knows which connections are finished,
    /// and it applies the grace periods: a viewer who closes the app must be
    /// given time to be noticed, or every sweep would tear down a healthy link
    /// the moment it blipped.
    fn sweep_viewers(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_viewer_sweep) < VIEWER_SWEEP_INTERVAL {
            return;
        }
        self.last_viewer_sweep = now;
        let Some(share) = self.share.as_mut() else {
            return;
        };
        let freed = session::block_on(share.sharer.prune());
        if !freed.is_empty() {
            share.viewers.retain(|slot| !freed.contains(&slot.id));
        }
        for slot in &mut share.viewers {
            slot.connected = share.sharer.viewer_connected(&slot.id);
        }
    }

    fn go_live(&mut self) {
        self.share_error = None;
        if self.capture.is_none() {
            if let Err(error) = self.start_capture() {
                self.share_error = Some(error);
                return;
            }
        }
        self.live = true;
        if let Some(lan) = &self.lan {
            lan.set_sharing(true);
        }
    }

    fn stop_live(&mut self) {
        if let Some(mut share) = self.share.take() {
            let sharer = Arc::clone(&share.sharer);
            let join = share.join.take();
            drop(share);
            if let Some(join) = join {
                let _ = join.join();
            }
            session::block_on(sharer.close());
        }
        self.live = false;
        if let Some(lan) = &self.lan {
            lan.set_sharing(false);
        }
        self.share_error = None;
    }

    fn create_manual_offer(&mut self) {
        self.share_error = None;
        if self.ensure_share().is_err() {
            return;
        }
        // A generated id, because a manual code has no peer to key on. Numbered
        // so asking for a second code opens a second connection instead of
        // replacing the first and hanging whoever is holding it.
        let id = {
            let share = self.share.as_mut().expect("share exists");
            share.manual_seq += 1;
            format!("manual-{}", share.manual_seq)
        };
        let Ok(offer) = self.add_share_viewer(&id, "Manual viewer") else {
            return;
        };
        if let Some(share) = self.share.as_mut() {
            share.manual_offer = Some((id, offer));
        }
    }

    fn update_capture_active(&mut self) -> bool {
        let active = self.preview_active
            || self
                .share
                .as_ref()
                .is_some_and(|share| share.sharer.is_connected());
        if let Some(capture) = &self.capture {
            capture.set_active(active);
        }
        if !active {
            self.preview = None;
            self.fps = 0.0;
        }
        active
    }

    fn accept_answer(share: &mut ShareSession) {
        let code = share.answer_input.trim().to_string();
        if code.is_empty() {
            share.error = Some("paste the viewer's answer code first".to_string());
            return;
        }
        // The answer belongs to the code on screen. A share can have several
        // connections, but only one is offered by hand at a time, so this is
        // unambiguous — and routing it to the wrong one would fail as an
        // invalid description rather than as "that viewer is gone".
        let Some((id, _)) = share.manual_offer.clone() else {
            share.error = Some("create a connection code first".to_string());
            return;
        };
        match session::block_on(share.sharer.set_answer(&id, &code)) {
            Ok(()) => {
                share.error = None;
                // The code has served its purpose: keeping it would invite a
                // second paste against a description that is already answered.
                share.manual_offer = None;
            }
            Err(error) => share.error = Some(error),
        }
    }

    fn start_view(&mut self, offer: String, lan_peer: Option<String>) {
        self.view_error = None;
        if offer.is_empty() {
            self.view_error = Some("paste a connection code from the sharer first".to_string());
            return;
        }
        if let Some(existing) = self.view.take() {
            session::block_on(existing.viewer.close());
        }
        let latest = Arc::new(Mutex::new(None));
        let stats = Arc::new(Mutex::new(ViewStats::default()));
        let metrics = Arc::new(ReceiverMetrics::default());
        let vol = self.config.volume_percent as f32 / 100.0;
        let (audio_playback, audio_error) = match AudioPlayback::start(vol) {
            Ok(playback) => (Some(Arc::new(playback)), None),
            Err(error) => (None, Some(error)),
        };
        if let Some(ref pb) = audio_playback {
            pb.set_volume_percent(self.config.volume_percent);
        }
        let callback: Arc<dyn Fn(&Packet) + Send + Sync> = Arc::new(Self::receive_callback(
            Arc::clone(&latest),
            Arc::clone(&stats),
            Arc::clone(&metrics),
            audio_playback.clone(),
        ));
        let udp = vec!["0.0.0.0:0".to_string()];
        let viewer = match session::block_on(session::Viewer::new(udp, callback)) {
            Ok(viewer) => Arc::new(viewer),
            Err(error) => {
                self.view_error = Some(error);
                return;
            }
        };
        let answer = match session::block_on(viewer.answer_offer(&offer)) {
            Ok(code) => code,
            Err(error) => {
                self.view_error = Some(error);
                return;
            }
        };
        let answer_code = match &lan_peer {
            Some(peer) => {
                if let Some(lan) = &self.lan {
                    lan.send_answer(peer, answer);
                }
                None
            }
            None => Some(answer),
        };
        self.view = Some(ViewSession {
            viewer,
            answer_code,
            error: None,
            latest,
            stats,
            metrics,
            texture: None,
            display_aspect: None,
            audio_playback,
            audio_error,
            quality: Quality::default(),
            was_connected: false,
            reconnect: None,
            // `lan_peer` is the sharer's id, and is the only handle a
            // viewer has for asking that sharer for a keyframe or telling it
            // what the link looks like from here. `lan_peer` stays as the
            // reconnect target; the two differ only in intent, not in value.
            sharer_id: lan_peer.clone(),
            lan_peer,
            roster: Vec::new(),
            roster_received: None,
            source_fps: 0,
            source_height: None,
            self_id: self.lan.as_ref().map(|lan| lan.id().to_string()),
            self_name: self.config.name.clone(),
            fullscreen: false,
            // Started idle rather than fresh, so the controls are not sitting on
            // top of the picture for the first three seconds of a session.
            hud_idle_since: Instant::now() - HUD_LINGER,
            last_pointer: None,
        });
    }

    fn request_view(&mut self, id: &str) {
        self.view_error = None;
        if let Some(lan) = &self.lan {
            lan.send_request(id, &self.config.name);
        }
        self.pending_view = Some(id.to_string());
    }

    fn stop_view(&mut self, ctx: &egui::Context) {
        // Before the session is dropped, while there is still something to ask.
        // A window left fullscreen after the stream stopped is a black screen
        // with no session behind it and Escape handled by a view that no longer
        // exists.
        if self.view.as_ref().is_some_and(|view| view.fullscreen) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        if let Some(view) = self.view.take() {
            session::block_on(view.viewer.close());
        }
        self.pending_view = None;
        self.view_error = None;
        self.screen = Screen::Home;
    }

    /// Puts the window into or out of fullscreen, if there is anything to show.
    ///
    /// Both directions are worth doing unconditionally rather than only on
    /// failure. A viewer can press F11, change their mind, and press it again
    /// faster than the window manager applies the first request, in which case
    /// the two commands race and whichever the OS handled last wins. Sending the
    /// state we want every time is the only version of this that cannot end up
    /// disagreeing with the app.
    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view.fullscreen = on;
        // Reset on the way in so the controls are there for the moment the
        // window changes, and on the way out so a pointer that has not moved
        // does not come back to a faded HUD.
        view.hud_idle_since = Instant::now();
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    fn view_visible(&self) -> bool {
        // Fullscreen means the stream is the entire window. Whatever screen the
        // user navigated to underneath — and whatever they will navigate to when
        // they come back out — the decoder must keep being fed, because this
        // function is what tells `poll_view` whether to consume frames.
        if self.view.as_ref().is_some_and(|view| view.fullscreen) {
            return true;
        }
        match &self.screen {
            Screen::Peer(id) => {
                self.view
                    .as_ref()
                    .and_then(|view| view.sharer_id.as_deref())
                    == Some(id.as_str())
            }
            Screen::View => self.view.is_some(),
            _ => false,
        }
    }

    fn poll_lan_events(&mut self) {
        // Before the events, so a request is answered against a slot list that
        // has already had its dead connections cleared out, and so the roster
        // that follows reflects those clearings.
        self.sweep_viewers();
        let Some(lan) = self.lan.as_ref() else {
            return;
        };
        let mut events = Vec::new();
        while let Some(event) = lan.try_event() {
            events.push(event);
        }
        for event in events {
            match event {
                lan::LanEvent::Request { id, name } => self.on_view_request(&id, &name),
                lan::LanEvent::Offer { id, sdp } => {
                    if self.pending_view.as_deref() == Some(id.as_str()) {
                        self.pending_view = None;
                        self.start_view(sdp, Some(id));
                    }
                }
                lan::LanEvent::Answer { id, sdp } => {
                    if let Some(share) = self.share.as_mut() {
                        // Only for a viewer whose offer is actually out, and only
                        // for the connection that offer came from — a share may
                        // have several, and an answer against the wrong one is a
                        // description mismatch with no useful error.
                        if share.viewers.iter().any(|slot| slot.id == id) {
                            match session::block_on(share.sharer.set_answer(&id, &sdp)) {
                                Ok(()) => share.error = None,
                                Err(error) => share.error = Some(error),
                            }
                        }
                    }
                }
                lan::LanEvent::Keyframe { id } => {
                    let now = Instant::now();
                    let Some(share) = self.share.as_mut() else {
                        continue;
                    };
                    let Some(slot) = share.viewers.iter_mut().find(|slot| slot.id == id) else {
                        continue;
                    };
                    // A peer that is not being served gets no burst on this
                    // sharer's bandwidth, and one viewer asking every few
                    // hundred milliseconds does not get to multiply them either.
                    if now.duration_since(slot.last_keyframe_request) < KEYFRAME_REQUEST_FLOOR {
                        continue;
                    }
                    slot.last_keyframe_request = now;
                    // One flag for the share: there is one encode, so the intra
                    // frame that unblocks this viewer unblocks everyone waiting
                    // on one.
                    share.force_keyframe.store(true, Ordering::Relaxed);
                }
                lan::LanEvent::Report {
                    id,
                    loss,
                    fps,
                    diag,
                } => {
                    self.apply_report(&id, &lan::LinkReport { loss, fps, diag });
                }
                lan::LanEvent::Roster {
                    id,
                    viewers,
                    source_fps,
                    source_height,
                } => {
                    // Only from the sharer being watched. Anyone else on the
                    // network could send one of these, and acting on it would
                    // display someone else's audience as ours.
                    let watching = self
                        .view
                        .as_ref()
                        .and_then(|view| view.sharer_id.as_deref())
                        == Some(id.as_str());
                    if !watching {
                        continue;
                    }
                    if let Some(view) = self.view.as_mut() {
                        view.roster = viewers;
                        view.roster_received = Some(Instant::now());
                        view.source_fps = source_fps;
                        view.source_height = source_height;
                    }
                }
            }
        }
        // After the events, so a viewer who has just joined is already in the
        // list the moment the others are told about them, rather than being
        // absent from it for one keepalive interval. Costs one comparison per
        // frame in the common case and a datagram per viewer when it matters.
        if let Some(lan) = self.lan.as_ref() {
            let height = self.share_height;
            if let Some(share) = self.share.as_mut() {
                share.broadcast_roster(height, lan);
            }
        }
    }

    /// Feeds a viewer's link measurement into the adaptive quality controller.
    ///
    /// Runs on the UI thread, so it must not block. It publishes the chosen
    /// height to the lock-free target the encode worker polls; unlike the frame
    /// queue it cannot be dropped, which is what makes adaptive resolution
    /// actually reach the encoder while the encoder is the bottleneck.
    ///
    /// The diagnostics are stored and nothing more happens with them here. They
    /// exist to be read: what to do about a viewer whose machine cannot keep up
    /// is a decision about the ladder, and making that call automatically would
    /// mean one person's slow decoder silently lowers everyone else's quality
    /// before anyone has looked at why.
    fn apply_report(&mut self, id: &str, report: &lan::LinkReport) {
        let now = Instant::now();
        let Some(share) = self.share.as_mut() else {
            return;
        };
        let Some(slot) = share.viewers.iter_mut().find(|slot| slot.id == id) else {
            return;
        };
        slot.loss = report.loss;
        slot.fps = report.fps;
        slot.diag = report.diag;
        slot.reported = Some(now);
        if !self.auto_quality {
            return;
        }
        // One encode feeds every viewer, and the ladder has one setting to move,
        // so it has to answer for the worst link rather than an average of them.
        let Some((loss, fps)) = worst_link(&share.viewers, now) else {
            return;
        };
        // The viewer can measure the link but not the sender's own handoff, so
        // the encoder half of the report is measured here. It is the only
        // evidence available when the link is lossless and the machine is the
        // limit.
        let drops = share.encoder_drops();
        let Decision::Step(_) = share.quality.update(Report { loss, fps, drops }, now) else {
            return;
        };
        // The controller now carries native as a real ladder rung, so its own
        // height already includes the user's ceiling and needs no remapping.
        let height = share.quality.height();
        share.quality_note = share.quality.last_reason().map(str::to_string);
        share
            .target_height
            .store(encode_target(height), Ordering::Relaxed);
    }

    fn receive_callback(
        latest: Arc<Mutex<Option<DecodedFrame>>>,
        stats: Arc<Mutex<ViewStats>>,
        metrics: Arc<ReceiverMetrics>,
        audio_playback: Option<Arc<AudioPlayback>>,
    ) -> impl Fn(&Packet) + Send + Sync {
        struct Pipeline {
            depacketizer: h264::Depacketizer,
            decoder: Option<H264Decoder>,
            audio_decoder: Option<OpusAudioDecoder>,
        }
        let pipeline = Arc::new(Mutex::new(Pipeline {
            depacketizer: h264::Depacketizer::new(),
            decoder: H264Decoder::new().ok(),
            audio_decoder: OpusAudioDecoder::new().ok(),
        }));
        move |packet: &Packet| {
            // Total time inside the callback. If this mean approaches the packet
            // arrival interval, the callback is the bottleneck and the receiver
            // cannot keep up no matter how much spare CPU the decoder has.
            let _receive = StageTimer::new(&metrics.receive);
            // Counters are atomics, not the `stats` mutex. At 500+ video packets
            // a second this callback was taking that lock three or four times
            // per packet, and the UI thread reads it every frame; the contention
            // was itself a source of jitter. `ViewStats` still records the
            // session-long numbers the existing panels show.
            metrics.packets.incr();
            metrics.bytes.add(packet.payload.len() as u64);
            if packet.header.payload_type == session::AUDIO_PT {
                metrics.audio_packets.incr();
                metrics.audio_bytes.add(packet.payload.len() as u64);
                if let Ok(mut stats) = stats.lock() {
                    stats.audio_packets += 1;
                }
                let Some(playback) = &audio_playback else {
                    return;
                };
                let Ok(mut pipeline) = pipeline.lock() else {
                    return;
                };
                let Some(decoder) = pipeline.audio_decoder.as_mut() else {
                    return;
                };
                match decoder.decode(&packet.payload) {
                    Ok(samples) => {
                        playback.push(samples);
                        metrics.audio_decoded.record();
                        if let Ok(mut stats) = stats.lock() {
                            stats.audio_decoded += 1;
                        }
                    }
                    Err(error) => {
                        metrics.audio_errors.incr();
                        if let Ok(mut stats) = stats.lock() {
                            if stats.audio_error.is_none() {
                                stats.audio_error = Some(error);
                            }
                            stats.audio_errors += 1;
                        }
                    }
                }
                return;
            }
            {
                let Ok(mut stats) = stats.lock() else {
                    return;
                };
                stats.packets += 1;
                stats.bytes += packet.payload.len() as u64;
                let seq = packet.header.sequence_number;
                match stats.highest_seq {
                    None => {
                        stats.base_seq = Some(seq);
                        stats.highest_seq = Some(seq);
                    }
                    Some(highest) => {
                        if seq != highest && seq.wrapping_sub(highest) < 0x8000 {
                            let gap = seq.wrapping_sub(highest) as u64 - 1;
                            stats.lost += gap;
                            // A gap here means the packet never arrived, and with
                            // no retransmission buffer in the transport the frame
                            // it belonged to is unrecoverable. This is the number
                            // that explains a stalled viewer.
                            metrics.sequence_losses.add(gap);
                            stats.highest_seq = Some(seq);
                        }
                    }
                }
            }
            let Ok(mut pipeline) = pipeline.lock() else {
                return;
            };
            let nalus = {
                let _depacketize = StageTimer::new(&metrics.depacketize);
                pipeline.depacketizer.push(packet)
            };
            let Some(nalus) = nalus else {
                return;
            };
            metrics.access_units.incr();
            if let Ok(mut stats) = stats.lock() {
                stats.aus += 1;
            }
            let Some(decoder) = pipeline.decoder.as_mut() else {
                return;
            };
            let access_unit = h264::access_unit_to_annexb(&nalus);
            match decoder.decode(&access_unit, &metrics) {
                Ok(Some(frame)) => {
                    if let Ok(mut stats) = stats.lock() {
                        if stats.first_width == 0 {
                            stats.first_width = frame.width;
                            stats.first_height = frame.height;
                        }
                        stats.decoded += 1;
                    }
                    // Only the newest frame matters. If the UI has not consumed
                    // the previous one, it is stale by definition, so replacing
                    // it is the correct behaviour and the drop is counted so the
                    // present rate can be compared against the decode rate.
                    if let Ok(mut slot) = latest.lock() {
                        if slot.is_some() {
                            metrics.presented.drop_frame();
                        } else {
                            metrics.presented.record();
                        }
                        *slot = Some(frame);
                    }
                }
                Ok(None) => {
                    metrics.no_picture.incr();
                    if let Ok(mut stats) = stats.lock() {
                        stats.decode_none += 1;
                    }
                }
                Err(error) => {
                    // The decoder lost sync (e.g. a keyframe was lost): drop
                    // everything until the next keyframe rather than feeding
                    // it error-prone frames; the depacketizer's sync gate
                    // handles that once reset.
                    pipeline.depacketizer.reset();
                    metrics.decode_errors.incr();
                    if let Ok(mut stats) = stats.lock() {
                        if stats.first_error.is_none() {
                            stats.first_error = Some(error);
                        }
                        stats.decode_errors += 1;
                    }
                }
            }
        }
    }

    fn poll_capture(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let interval = self.frame_interval();
        let encode_due = self.share.as_ref().is_some_and(|share| {
            share.sharer.is_connected() && now.duration_since(share.last_encode) >= interval
        });
        if !self.preview_active && !encode_due {
            return;
        }
        // Surface capture-side failures (recoverable retries are kept internal
        // to the capture thread) even when no frame is pending right now.
        let capture_error = self.capture.as_ref().and_then(CaptureSession::error);
        let frame = self.capture.as_ref().and_then(CaptureSession::latest);
        if let Some(error) = capture_error {
            if self.preview_active && self.preview_error.is_none() {
                self.preview_error = Some(error.clone());
            }
            if let Some(share) = self.share.as_mut() {
                if share.error.is_none() {
                    share.error = Some(error);
                }
            }
        }
        let Some(frame) = frame else {
            return;
        };
        if self.preview_active {
            self.fps_frames += 1;
            let elapsed = now.duration_since(self.fps_last);
            if elapsed >= Duration::from_secs(1) {
                self.fps = self.fps_frames as f32 / elapsed.as_secs_f32();
                self.fps_frames = 0;
                self.fps_last = now;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            match self.preview.as_mut() {
                Some(preview) if preview.width == frame.width && preview.height == frame.height => {
                    preview.texture.set(image, egui::TextureOptions::LINEAR);
                }
                _ => {
                    self.preview = Some(Preview {
                        texture: ctx.load_texture("preview", image, egui::TextureOptions::LINEAR),
                        width: frame.width,
                        height: frame.height,
                    });
                }
            }
        }
        if let Some(share) = self.share.as_mut() {
            if share.sharer.is_connected() && now.duration_since(share.last_encode) >= interval {
                share.last_encode = now;
                if let Ok(mut stats) = share.stats.lock() {
                    if let Some(error) = stats.error.take() {
                        share.error = Some(error);
                    }
                }
                let msg = EncodeMsg::Frame {
                    rgba: frame.rgba,
                    width: frame.width,
                    height: frame.height,
                };
                match share.tx.try_send(msg) {
                    Ok(()) => share.metrics.queued.record(),
                    // The encoder is not keeping up, so the handoff queue is
                    // full and this frame is discarded. `encoded` cannot show
                    // this: the frame never reached the encoder to be counted.
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        share.metrics.queued.drop_frame()
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                }
            }
        }
    }

    fn poll_share_audio(&mut self) {
        let Some(share) = self.share.as_mut() else {
            return;
        };
        // The capture worker retries a dead device on its own and only reports the
        // failures it wants surfaced, so taking them here cannot hide a later
        // one behind an earlier.
        if let Some(notice) = share
            .audio_capture
            .as_ref()
            .and_then(AudioCapture::try_error)
        {
            share.audio_error = Some(notice);
        }
        // A worker that has been recovering on its own should stop saying so.
        if share
            .audio_capture
            .as_ref()
            .is_some_and(AudioCapture::is_alive)
        {
            share.audio_error = None;
        }
        if !share.sharer.is_connected() {
            return;
        }
        let metrics = Arc::clone(&share.metrics);
        while let Some(frame) = share
            .audio_capture
            .as_ref()
            .and_then(AudioCapture::try_frame)
        {
            let Some(encoder) = share.audio_encoder.as_mut() else {
                break;
            };
            let packet = match encoder.encode(&frame) {
                Ok(packet) => packet,
                Err(error) => {
                    share.audio_error = Some(error);
                    break;
                }
            };
            let timestamp = share.audio_timestamp;
            share.audio_timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
            let sharer = Arc::clone(&share.sharer);
            if let Err(error) = session::block_on(sharer.send_audio(&packet, timestamp, &metrics)) {
                share.audio_error = Some(error);
                break;
            }
            share.audio_frames_sent += 1;
        }
    }

    fn poll_view(&mut self, ctx: &egui::Context, visible: bool) {
        if self.view.is_none() {
            return;
        }
        let snapshot = self.view.as_mut().and_then(|view| {
            view.stats
                .lock()
                .ok()
                .map(|s| (s.packets, s.decoded, s.bytes, s.lost, s.audio_packets))
        });
        {
            let Some(view) = self.view.as_mut() else {
                return;
            };
            if view.viewer.is_connected() {
                view.was_connected = true;
                view.reconnect = None;
            }
            if let Some((packets, decoded, bytes, lost, audio_packets)) = snapshot {
                let now = Instant::now();
                let elapsed = now.duration_since(view.quality.last).as_secs_f32();
                if elapsed >= 0.5 {
                    let received = packets.saturating_sub(audio_packets);
                    let d_frames = decoded.saturating_sub(view.quality.last_decoded);
                    let d_bytes = bytes.saturating_sub(view.quality.last_bytes);
                    let d_lost = lost.saturating_sub(view.quality.last_lost);
                    let d_received = received.saturating_sub(view.quality.last_received);
                    view.quality.fps = d_frames as f32 / elapsed;
                    view.quality.mbps = d_bytes as f32 * 8.0 / elapsed / 1_000_000.0;
                    let total = d_received + d_lost;
                    view.quality.loss = if total > 0 {
                        d_lost as f32 / total as f32 * 100.0
                    } else {
                        0.0
                    };
                    // Packets arrived but nothing decoded. Sequence gaps count
                    // this stream as intact, so loss-based detection cannot see
                    // it — the decoder is simply waiting for an intra frame.
                    view.quality.starved = d_received > 0 && d_frames == 0;
                    // Baselines for the two rates that are reported to the
                    // sharer rather than shown here: decode errors per second,
                    // and the share of decoded frames the UI never displayed.
                    //
                    // Sampled here rather than read again in `link_report`
                    // because a counter read twice can differ between the two
                    // reads, which would make the delta negative and the rate
                    // nonsense. Same reason the window is closed here.
                    let errors = view.metrics.decode_errors.get();
                    let dropped = view.metrics.presented.dropped();
                    let shown = view.metrics.presented.frames();
                    view.quality.decode_errors =
                        errors.saturating_sub(view.quality.last_errors) as f32 / elapsed;
                    let shown_delta = shown.saturating_sub(view.quality.last_shown);
                    view.quality.render_drops = if shown_delta > 0 {
                        dropped.saturating_sub(view.quality.last_dropped) as f32
                            / shown_delta as f32
                            * 100.0
                    } else {
                        0.0
                    };
                    view.quality.last = now;
                    view.quality.last_decoded = decoded;
                    view.quality.last_bytes = bytes;
                    view.quality.last_lost = lost;
                    view.quality.last_received = received;
                    view.quality.last_errors = errors;
                    view.quality.last_dropped = dropped;
                    view.quality.last_shown = shown;
                    // Warnings are carried forward rather than recomputed from a
                    // single window: a decode error that recovers between two
                    // reports would otherwise blink out of existence and the
                    // sharer would see a viewer whose errors stopped, which is
                    // the opposite of what happened.
                    view.quality.decode_errors_peak = view
                        .quality
                        .decode_errors_peak
                        .max(view.quality.decode_errors);
                    view.quality.render_drops_peak = view
                        .quality
                        .render_drops_peak
                        .max(view.quality.render_drops);
                }
            }
        }
        // Reporting happens before the `visible` early-return below. A viewer
        // whose window is not on screen is still watching, and its loss is the
        // whole input to the sharer's recovery decisions.
        self.report_link_state();
        if !visible {
            return;
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(frame) = view.latest.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        // Fix the display aspect on the first frame. The ladder preserves the
        // source ratio, so every later resolution shares it and the box stays
        // put when the resolution drops.
        view.display_aspect.get_or_insert_with(|| {
            if frame.height > 0 {
                frame.width as f32 / frame.height as f32
            } else {
                16.0 / 9.0
            }
        });
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut view.texture {
            Some(preview) if preview.width == frame.width && preview.height == frame.height => {
                preview.texture.set(image, egui::TextureOptions::LINEAR);
            }
            _ => {
                view.texture = Some(Preview {
                    texture: ctx.load_texture("remote", image, egui::TextureOptions::LINEAR),
                    width: frame.width,
                    height: frame.height,
                });
            }
        }
    }

    /// Tells the sharer what this end is seeing, and asks for a keyframe when
    /// the link has clearly lost one.
    ///
    /// Both messages are best-effort UDP and both are rate limited. The report
    /// matters whether or not anything has gone wrong — it is what lets the
    /// sharer's controller step back up when a link recovers, which it could
    /// never infer on its own.
    fn report_link_state(&mut self) {
        let now = Instant::now();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        // A manual-code session has no LAN peer, so there is nobody to tell. It
        // also gets no keyframe recovery, which is worth saying in the UI.
        let Some(peer) = view.sharer_id.clone() else {
            return;
        };
        let Some(lan) = self.lan.as_ref() else {
            return;
        };

        if now.duration_since(view.quality.last_report) >= REPORT_INTERVAL {
            view.quality.last_report = now;
            lan.send_report(&peer, &view.quality.link_report(&view.metrics));
        }

        // Two independent reasons to ask, because they fail differently:
        // sequence gaps (loss), and an intact stream the decoder refuses to
        // decode because it is waiting for an intra frame (starved).
        let connected = view.viewer.is_connected();
        let needs_keyframe = view.quality.loss >= LOSS_BURST || view.quality.starved;
        if connected
            && needs_keyframe
            && now.duration_since(view.quality.last_keyframe_request) >= KEYFRAME_REQUEST_INTERVAL
        {
            view.quality.last_keyframe_request = now;
            view.quality.requested_at_loss = view.quality.loss;
            view.quality.recoveries += 1;
            lan.send_keyframe(&peer);
        }
    }

    fn poll_reconnect(&mut self) {
        let now = Instant::now();
        let action = {
            let Some(view) = self.view.as_mut() else {
                return;
            };
            if !view.was_connected || view.viewer.is_connected() {
                return;
            }
            let Some(peer) = view.lan_peer.clone() else {
                return;
            };
            if view.reconnect.is_none() {
                view.reconnect = Some(Reconnect {
                    attempts: 0,
                    last_request: now,
                });
            }
            let state = view.reconnect.as_mut().expect("reconnect state");
            if state.attempts >= 5 {
                if view.error.is_none() {
                    view.error = Some("Connection lost — reconnect attempts exhausted".to_string());
                }
                None
            } else if now.duration_since(state.last_request) >= Duration::from_secs(3) {
                state.last_request = now;
                state.attempts += 1;
                Some(peer)
            } else {
                None
            }
        };
        if let Some(peer) = action {
            if let Some(lan) = &self.lan {
                lan.send_request(&peer, &self.config.name);
            }
            self.pending_view = Some(peer);
        }
    }

    /// Draws the decoded frame into a box no larger than `max`, stretched to fill it.
    ///
    /// The box depends only on the available space, the source aspect and `max`,
    /// never on the decoded resolution, and it is drawn at the source aspect so a
    /// non-16:9 monitor is not distorted. A resolution drop therefore stretches
    /// the pixels into the same rectangle instead of shrinking the video, which
    /// is the point: the window no longer jumps size as the sharer's adaptive
    /// controller moves up and down the ladder.
    ///
    /// `max` is how far the picture is allowed to grow, and only fullscreen passes
    /// something larger than the window. Everywhere else the cap is what keeps a
    /// 4K stream from filling a sidebar with a postage stamp of itself.
    fn render_image(ui: &mut egui::Ui, preview: &Preview, aspect: f32, max: egui::Vec2) {
        if !aspect.is_finite() || aspect <= 0.0 {
            return;
        }
        let available = ui.available_size();
        let box_width = max.x.min(available.x.max(1.0));
        let box_height = max.y.min(available.y.max(1.0));
        let width = box_width.min(box_height * aspect);
        let height = width / aspect;
        ui.image((preview.texture.id(), egui::vec2(width, height)));
    }

    fn network_status(&mut self, ui: &mut egui::Ui) {
        match self.lan.as_ref().and_then(|lan| lan.local_address()) {
            Some(ip) => {
                ui.label(RichText::new(format!("This PC on your network: {ip}")).weak());
            }
            None => {
                ui.label(
                    RichText::new("Radmin VPN not detected — friends can't find this PC yet.")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
                if self.radmin_exe.is_some() {
                    if ui.button("Launch Radmin VPN").clicked() {
                        self.launch_radmin();
                    }
                } else {
                    ui.label(
                        RichText::new("Install Radmin VPN to share with friends online.").weak(),
                    );
                }
                if let Some(error) = &self.radmin_error {
                    ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
                }
            }
        }
        if let Some(error) = &self.lan_error {
            ui.label(RichText::new(format!("Network discovery off: {error}")).weak());
        }
    }

    fn launch_radmin(&mut self) {
        match crate::radmin::launch() {
            Ok(()) => self.radmin_error = None,
            Err(error) => self.radmin_error = Some(error),
        }
    }

    fn quality_line(ui: &mut egui::Ui, view: &ViewSession) {
        let quality = &view.quality;
        let color = if quality.mbps <= 0.0 {
            Color32::GRAY
        } else if quality.loss > 5.0 || quality.fps < 15.0 {
            Color32::from_rgb(220, 120, 120)
        } else if quality.loss > 1.0 || quality.fps < 24.0 {
            Color32::from_rgb(220, 200, 120)
        } else {
            Color32::from_rgb(150, 220, 150)
        };
        let resolution = view
            .texture
            .as_ref()
            .map(|preview| format!("{}x{} · ", preview.width, preview.height))
            .unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "{resolution}{:.0} fps · {:.1} Mbps · {:.1}% loss",
                quality.fps, quality.mbps, quality.loss
            ))
            .color(color)
            .size(16.0),
        );
        // Recovery state, because "the fps is bad" and "the app is asking the
        // sharer to fix it" are different situations and only one of them is
        // worth waiting out.
        if let Some(peer) = &view.sharer_id {
            let since = view.quality.last_keyframe_request.elapsed();
            if quality.loss >= LOSS_BURST {
                ui.label(
                    RichText::new(format!(
                        "Requesting keyframes from {peer} — {:.1}% loss",
                        quality.loss
                    ))
                    .color(Color32::from_rgb(220, 200, 120)),
                );
            } else if view.quality.recoveries > 0 && since < Duration::from_secs(10) {
                ui.label(
                    RichText::new(format!("Recovered {:.0} s ago", since.as_secs_f32())).weak(),
                );
            }
        } else {
            ui.label(RichText::new("No LAN peer — no keyframe recovery for this session.").weak());
        }
    }

    fn share_view(&mut self, ui: &mut egui::Ui) {
        let mut do_go_live = false;
        let mut do_stop_live = false;
        let mut do_preview = false;

        if self.monitors.is_empty() {
            self.refresh_monitors();
        }
        if self.monitors.is_empty() {
            ui.heading("Share");
            ui.label(RichText::new("No monitor found").color(Color32::from_rgb(220, 120, 120)));
            return;
        }

        ui.heading(if self.live {
            "You're live"
        } else {
            "Share your screen"
        });
        ui.add_space(4.0);
        if self.live || self.share.is_some() {
            if ui.button("Stop streaming").clicked() {
                do_stop_live = true;
            }
        } else if ui.button("Go Live").clicked() {
            do_go_live = true;
        }

        let selected = self.selected_monitor.min(self.monitors.len() - 1);
        egui::ComboBox::from_label("Monitor")
            .selected_text(Self::monitor_label(&self.monitors[selected]))
            .show_ui(ui, |ui| {
                for (index, monitor) in self.monitors.iter().enumerate() {
                    if ui
                        .selectable_label(
                            self.selected_monitor == index,
                            Self::monitor_label(monitor),
                        )
                        .clicked()
                    {
                        self.selected_monitor = index;
                    }
                }
            });

        // Adaptive resolution is the primary control and stays in the open: it
        // starts at the ceiling and walks down only as far as the link and the
        // encoder actually need. The exact ceiling and the frame rate live
        // under Advanced, because the defaults (native / 60 fps) are what most
        // people want and the controller handles everything below them.
        let mut auto_quality = self.auto_quality;
        if ui
            .checkbox(&mut auto_quality, "Adjust resolution to the viewer's link")
            .changed()
        {
            self.auto_quality = auto_quality;
            if let Some(share) = self.share.as_mut() {
                // Turning it off must restore whatever the user picked, since the
                // controller may have moved the encoder somewhere else.
                share.quality.reset(self.share_height);
                share.quality_note = None;
                share
                    .target_height
                    .store(encode_target(self.share_height), Ordering::Relaxed);
            }
        }
        // Show what the controller is actually doing. A silent resolution change is
        // indistinguishable from a bug to whoever is watching, and knowing it was
        // deliberate is the difference between trusting it and fighting it.
        if let Some(share) = self.share.as_ref() {
            if self.auto_quality && share.quality.is_auto() {
                let height = share
                    .quality
                    .height()
                    .map(|height| format!("{height}p"))
                    .unwrap_or_else(|| "native".to_string());
                ui.label(
                    RichText::new(format!(
                        "Sending {height} · loss {:.1}% · encoder drops {:.1}%",
                        share.quality.smoothed_loss(),
                        share.quality.smoothed_drops()
                    ))
                    .color(Color32::from_rgb(220, 200, 120)),
                );
                if let Some(reason) = &share.quality_note {
                    ui.label(RichText::new(reason).weak());
                }
            }
        }

        let streaming = self.live || self.share.is_some();
        egui::CollapsingHeader::new("Advanced")
            .default_open(false)
            .show(ui, |ui| {
                let mut share_height = self.share_height;
                egui::ComboBox::from_label("Stream quality")
                    .selected_text(Self::quality_label(share_height))
                    .show_ui(ui, |ui| {
                        for (label, height) in [
                            ("Native", None),
                            ("720p", Some(720)),
                            ("540p", Some(540)),
                            ("360p", Some(360)),
                        ] {
                            if ui.selectable_label(share_height == height, label).clicked() {
                                share_height = height;
                            }
                        }
                    });
                if share_height != self.share_height {
                    self.share_height = share_height;
                    if let Some(share) = self.share.as_mut() {
                        // A manual choice wins. Resetting the controller stops it
                        // from undoing this on evidence gathered before the choice
                        // was made.
                        share.quality.reset(share_height);
                        share.quality_note = None;
                        share
                            .target_height
                            .store(encode_target(share_height), Ordering::Relaxed);
                    }
                }

                let mut frame_rate = self.frame_rate;
                ui.add_enabled_ui(!streaming, |ui| {
                    egui::ComboBox::from_label("Frame rate")
                        .selected_text(format!("{frame_rate} fps"))
                        .show_ui(ui, |ui| {
                            for fps in [30u32, 60u32] {
                                if ui
                                    .selectable_label(frame_rate == fps, format!("{fps} fps"))
                                    .clicked()
                                {
                                    frame_rate = fps;
                                }
                            }
                        });
                });
                if streaming {
                    ui.label(RichText::new("Stop streaming to change the frame rate.").weak());
                }
                if frame_rate != self.frame_rate {
                    self.frame_rate = frame_rate;
                    if let Some(capture) = &self.capture {
                        capture.set_interval(self.frame_interval());
                    }
                }
            });

        ui.add_space(6.0);
        let preview_label = if self.preview_active {
            "Stop preview"
        } else {
            "Preview"
        };
        if ui.button(preview_label).clicked() {
            do_preview = true;
        }
        if let Some(error) = &self.preview_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }

        ui.add_space(6.0);
        if let Some(share) = self.share.as_mut() {
            // Presence of the objects only proves the thread started. The state
            // handle is what says whether a device is actually being serviced
            // right now, which is the question the panel is really asking.
            let audio_label = match (&share.audio_capture, &share.audio_encoder) {
                (Some(capture), Some(_)) if capture.is_alive() => "Audio: system sound shared",
                (Some(_), Some(_)) => "Audio: reconnecting to the sound device",
                _ => "Audio: not available",
            };
            ui.label(format!("Status: {}", share.sharer.status()));
            if share
                .audio_capture
                .as_ref()
                .is_some_and(AudioCapture::is_alive)
            {
                ui.label(RichText::new(audio_label).weak());
            } else {
                ui.label(RichText::new(audio_label).color(Color32::from_rgb(220, 200, 120)));
            }
            if let Some(error) = &share.audio_error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            if let Some(error) = &share.error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            // Who is actually watching. With one stream going to several people
            // this is the only place a viewer count exists, and it is what tells
            // the person sharing that the second click was served rather than
            // dropped.
            //
            // The same list the viewers get, rendered by the same code, because
            // they are the same fact: a disagreement between the two lists would
            // be read as a bug in one of them.
            let audience = share.audience();
            viewer_list(ui, &audience, "", false, 1.0);
            let now = Instant::now();
            let source_fps = share.frame_rate;
            for slot in &share.viewers {
                let line = slot.summary(now, source_fps);
                ui.label(line);
            }
        } else if self.live {
            ui.label(
                RichText::new("Waiting for someone to join…")
                    .color(Color32::from_rgb(150, 220, 150)),
            );
        }
        if let Some(error) = &self.share_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }

        ui.add_space(8.0);
        if self.preview_active {
            if let Some(preview) = &self.preview {
                ui.label(format!(
                    "{}x{} @ {:.0} fps",
                    preview.width, preview.height, self.fps
                ));
                ui.add_space(8.0);
                let aspect = preview.width as f32 / preview.height.max(1) as f32;
                Self::render_image(ui, preview, aspect, VIEWER_IMAGE_CAP);
            }
        } else if self.live {
            ui.label(RichText::new("Press Preview to see your screen here.").weak());
        } else {
            ui.label(
                RichText::new(
                    "Press Go Live to start sharing, or Preview to check your screen first.",
                )
                .weak(),
            );
        }

        if do_preview {
            self.toggle_preview();
        }
        if do_go_live {
            self.go_live();
        }
        if do_stop_live {
            self.stop_live();
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let mut do_connect = false;
        let mut do_manual = false;
        let mut do_accept = false;

        let mut peers = self.lan.as_ref().map(|lan| lan.peers()).unwrap_or_default();
        peers.sort_by(|a, b| {
            b.sharing
                .cmp(&a.sharing)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.lan.is_none() {
                    ui.label(RichText::new("Peer discovery is unavailable").weak());
                } else if peers.is_empty() {
                    ui.label(RichText::new("Looking for people…").weak());
                }

                for peer in &peers {
                    ui.horizontal(|ui| {
                        if peer.sharing {
                            let selected =
                                matches!(&self.screen, Screen::Peer(id) if id == &peer.id);
                            if ui
                                .add(egui::Button::selectable(
                                    selected,
                                    RichText::new(&peer.name),
                                ))
                                .clicked()
                            {
                                self.screen = Screen::Peer(peer.id.clone());
                            }
                            ui.label(RichText::new("Live").color(Color32::from_rgb(150, 220, 150)));
                        } else {
                            ui.label(RichText::new(&peer.name).color(Color32::GRAY));
                        }
                    });
                }

                if self.pending_view.is_some() {
                    ui.label(RichText::new("Connecting…").color(Color32::from_rgb(220, 200, 120)));
                }
                if let Some(error) = &self.view_error {
                    ui.label(
                        RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)),
                    );
                }
                // Reported here as well as in the Watching panel: a viewer whose
                // speaker died during setup never reaches that panel, and an
                // unconnected viewer has no other way to find out.
                if let Some(view) = self.view.as_ref() {
                    if let Some(error) = &view.audio_error {
                        ui.label(
                            RichText::new(format!("Audio unavailable: {error}"))
                                .color(Color32::from_rgb(220, 120, 120)),
                        );
                    }
                }

                ui.add_space(8.0);
                self.network_status(ui);
                ui.add_space(6.0);
                egui::CollapsingHeader::new("Advanced")
                    .default_open(false)
                    .show(ui, |ui| {
                        // Only while there is nothing to attach a code to: once a
                        // share exists, the panel above owns this, because a
                        // second code means a second viewer rather than a
                        // second share.
                        if !self.live
                            && self.share.is_none()
                            && ui.button("Create manual connection code").clicked()
                        {
                            do_manual = true;
                        }
                        if let Some(share) = self.share.as_mut() {
                            if let Some((_, code)) = &share.manual_offer {
                                ui.label(RichText::new("Share code").strong());
                                Self::code_widget(ui, code, 3);
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut share.answer_input)
                                            .hint_text("Paste answer code")
                                            .desired_width(140.0),
                                    );
                                    if ui.button("Connect").clicked() {
                                        do_accept = true;
                                    }
                                });
                            }
                            // Another manual viewer is another connection, so
                            // this belongs to a share that is already serving one
                            // rather than to the case where nothing works yet.
                            // Kept outside the block above because a code that
                            // has been answered is gone from the panel, and the
                            // button has to outlive it.
                            if ui.button("New code for another viewer").clicked() {
                                do_manual = true;
                            }
                            let (frames_sent, encode_ms, encode_errors) = share
                                .stats
                                .lock()
                                .map(|stats| {
                                    (stats.frames_sent, stats.encode_ms, stats.encode_errors)
                                })
                                .unwrap_or((0, 0.0, 0));
                            ui.label(format!(
                                "{} frames sent, {:.1} ms/frame, {} encode errors, {} audio frames",
                                frames_sent, encode_ms, encode_errors, share.audio_frames_sent
                            ));
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.code_input)
                                .hint_text("Connection code")
                                .desired_width(140.0),
                        );
                        if ui.button("Connect").clicked() {
                            do_connect = true;
                        }
                        if let Some(code) = self
                            .view
                            .as_ref()
                            .and_then(|view| view.answer_code.as_ref())
                        {
                            ui.label(RichText::new("Your reply code").strong());
                            Self::code_widget(ui, code, 2);
                        }
                    });
            });

        if do_connect {
            self.start_view(self.code_input.trim().to_string(), None);
            if self.view.is_some() {
                self.screen = Screen::View;
            }
        }
        if do_manual {
            self.create_manual_offer();
        }
        if do_accept {
            if let Some(share) = self.share.as_mut() {
                Self::accept_answer(share);
            }
        }
    }

    fn peer_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, id: &str) {
        let name = self
            .lan
            .as_ref()
            .map(|lan| lan.peers())
            .unwrap_or_default()
            .into_iter()
            .find(|peer| peer.id == id)
            .map(|peer| peer.name)
            .unwrap_or_else(|| "Stream".to_string());

        let connected = self
            .view
            .as_ref()
            .and_then(|view| view.sharer_id.as_deref())
            == Some(id);
        if connected {
            self.watch_view(ui, ctx);
            return;
        }

        ui.heading(&name);
        let pending = self.pending_view.as_deref() == Some(id);
        if pending {
            ui.label(RichText::new("Connecting…").color(Color32::from_rgb(220, 200, 120)));
        } else {
            ui.label(RichText::new("Not watching yet.").weak());
        }
        if let Some(error) = &self.view_error {
            ui.label(RichText::new(error.to_string()).color(Color32::from_rgb(220, 120, 120)));
        }
        ui.add_space(8.0);

        let available = ui.available_size();
        let width = available.x.clamp(320.0, 1024.0);
        let height = (width * 9.0 / 16.0).min(available.y.max(200.0));
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(6), Color32::from_gray(28));
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(6),
            egui::Stroke::new(1.0_f32, Color32::from_gray(80)),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if pending {
                "Connecting…"
            } else {
                "Click to watch"
            },
            egui::FontId::proportional(20.0),
            Color32::from_gray(200),
        );
        if response.clicked() && !pending {
            self.request_view(id);
        }
    }

    fn watch_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut request_stop = false;
        let mut toggle_fullscreen = false;
        if let Some(view) = self.view.as_mut() {
            ui.heading("Watching");
            ui.label(format!("Status: {}", view.viewer.status()));
            Self::quality_line(ui, view);
            let muted = view
                .audio_playback
                .as_ref()
                .map(|playback| playback.is_muted())
                .unwrap_or(false);
            if let Some(playback) = &view.audio_playback {
                let label = if muted { "Unmute" } else { "Mute" };
                if ui.button(label).clicked() {
                    playback.set_muted(!muted);
                }
                let mut vol = playback.volume_percent();
                let mut vol_changed = false;
                ui.horizontal(|ui| {
                    ui.label("Volume");
                    if ui
                        .add(egui::Slider::new(&mut vol, 0..=200).suffix("%"))
                        .changed()
                    {
                        vol_changed = true;
                    }
                });
                if vol_changed {
                    playback.set_volume_percent(vol);
                    self.config.volume_percent = vol;
                    let _ = config::save(&self.config);
                    if playback.is_muted() && vol > 0 {
                        playback.set_muted(false);
                    }
                }
                // unmute on change to non-zero? if muted and vol>0, unmute? but button controls mute
                // `Some(playback)` says the thread was started, not that the
                // speaker is still being driven. A worker stuck in its retry
                // backoff leaves the session silent with nothing else to show
                // for it.
                if !playback.is_alive() {
                    ui.label(
                        RichText::new("Audio: reconnecting to the speaker")
                            .color(Color32::from_rgb(220, 200, 120)),
                    );
                }
                if let Some(error) = playback.try_error() {
                    view.audio_error = Some(error);
                } else if playback.is_alive() {
                    view.audio_error = None;
                }
            }
            if view.reconnect.is_some() {
                ui.label(
                    RichText::new("Connection lost — reconnecting…")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
            }
            egui::CollapsingHeader::new("Diagnostics")
                .default_open(false)
                .show(ui, |ui| {
                    if let Ok(stats) = view.stats.lock() {
                        ui.label(format!(
                            "Video: {} packets, {} frames assembled, {} decoded (first {}x{}), {} no-picture, {} decode errors",
                            stats.packets,
                            stats.aus,
                            stats.decoded,
                            stats.first_width,
                            stats.first_height,
                            stats.decode_none,
                            stats.decode_errors
                        ));
                        if let Some(first_error) = &stats.first_error {
                            ui.label(
                                RichText::new(format!("First decode error: {first_error}"))
                                    .color(Color32::from_rgb(220, 120, 120)),
                            );
                        }
                        // "on" must mean the speaker is actually being driven. Presence of the
                        // object only means the thread was started, which is how
                        // a dead audio thread used to look healthy.
                        let playing = view
                            .audio_playback
                            .as_ref()
                            .is_some_and(|playback| playback.is_alive());
                        ui.label(format!(
                            "Audio ({}): {} packets, {} decoded, {} errors",
                            if playing { "on" } else { "off" },
                            stats.audio_packets,
                            stats.audio_decoded,
                            stats.audio_errors
                        ));
                        if let Some(error) = &stats.audio_error {
                            ui.label(
                                RichText::new(format!("First audio error: {error}"))
                                    .color(Color32::from_rgb(220, 120, 120)),
                            );
                        }
                    }
                    if let Some(error) = &view.audio_error {
                        ui.label(
                            RichText::new(format!("Audio unavailable: {error}"))
                                .color(Color32::from_rgb(220, 120, 120)),
                        );
                    }
                });
            if let Some(error) = &view.error {
                ui.label(RichText::new(error).color(Color32::from_rgb(220, 120, 120)));
            }
            ui.add_space(8.0);
            if let Some(preview) = &view.texture {
                ui.label(format!("Stream {}x{}", preview.width, preview.height));
                let aspect = view
                    .display_aspect
                    .unwrap_or(preview.width as f32 / preview.height as f32);
                Self::render_image(ui, preview, aspect, VIEWER_IMAGE_CAP);
            } else if view.stats.lock().map(|s| s.packets).unwrap_or(0) > 0 {
                ui.label(
                    RichText::new("Receiving stream data but no decoded picture yet…")
                        .color(Color32::from_rgb(220, 200, 120)),
                );
            } else {
                ui.label(RichText::new("Waiting for stream data…").color(Color32::GRAY));
            }
            // Who else is watching, and what this end measured. Under the picture
            // rather than above it, because the picture is the point and both of
            // these are context for it.
            //
            // The verdict is computed from the very report that is on its way to
            // the sharer, so the two sides can never disagree about what this
            // machine measured — only about what to do about it.
            ui.add_space(6.0);
            viewer_audience(ui, view, 1.0);
            let mine = view.quality.link_report(&view.metrics);
            let verdict = diagnose(&mine.diag, mine.loss, view.source_fps);
            let verdict_text = format!("Your end: {}", verdict.describe(view.source_fps));
            if verdict == Bottleneck::Ok {
                ui.label(RichText::new(verdict_text).weak());
            } else {
                ui.label(RichText::new(verdict_text).color(Color32::from_rgb(220, 200, 120)));
            }
            ui.add_space(8.0);
            if ui.button("Stop watching").clicked() {
                request_stop = true;
            }
            ui.add_space(6.0);
            if ui
                .button(if view.fullscreen {
                    "Leave fullscreen (Esc)"
                } else {
                    "Fullscreen (F11)"
                })
                .clicked()
            {
                toggle_fullscreen = true;
            }
        }
        if request_stop {
            self.stop_view(ctx);
        }
        if toggle_fullscreen {
            let on = self.view.as_ref().is_some_and(|view| view.fullscreen);
            self.set_fullscreen(ctx, !on);
        }
    }

    /// The picture, alone, filling the window, with the controls floating over it.
    ///
    /// Deliberately not the normal panel layout with the video hidden. A sidebar, a
    /// status line and a scrolling column would all be sitting on top of the thing
    /// the user went fullscreen to look at, and the honest version of fullscreen for
    /// a stream is the stream.
    ///
    /// The controls have to come with it, though. Everything the windowed panel
    /// offers that is still meaningful without the panel — leaving fullscreen,
    /// muting, who else is watching — moves into a HUD that fades. Otherwise the
    /// user has to leave fullscreen to change the volume, which makes the volume
    /// control unreachable exactly when it is wanted.
    fn fullscreen_view(&mut self, ctx: &egui::Context) {
        let mut request_stop = false;
        let mut leave = false;
        let mut set_muted = None;
        let now = Instant::now();
        egui::CentralPanel::default()
            // Letterboxed with black rather than the panel background: the bars are
            // not part of the picture and should not look like they are.
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let Some(view) = self.view.as_ref() else {
                    return;
                };
                if let Some(preview) = &view.texture {
                    let aspect = view
                        .display_aspect
                        .unwrap_or(preview.width as f32 / preview.height.max(1) as f32);
                    // No cap. This is the one place the video is allowed to be as
                    // large as the display can show it.
                    Self::render_image(
                        ui,
                        preview,
                        aspect,
                        egui::vec2(f32::INFINITY, f32::INFINITY),
                    );
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Waiting for the stream…").color(Color32::GRAY));
                    });
                }
            });
        let pointer = ctx.input(|input| input.pointer.latest_pos());
        // Read the HUD state and update the idle clock in one borrow, then let it
        // go: the window below needs `self` again, and holding the session across it
        // would not compile.
        let (alpha, muted, has_audio) = {
            let Some(view) = self.view.as_mut() else {
                return;
            };
            if pointer != view.last_pointer {
                view.hud_idle_since = now;
            }
            view.last_pointer = pointer;
            let playback = view.audio_playback.as_ref();
            (
                view.hud_alpha(view.hud_idle(now)),
                playback.is_some_and(|playback| playback.is_muted()),
                playback.is_some(),
            )
        };
        if alpha > 0.0 {
            egui::Area::new(egui::Id::new("argos.hud"))
                .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(16.0, -16.0))
                .order(egui::Order::Foreground)
                .interactable(true)
                .show(ctx, |ui| {
                    let frame = egui::Frame::popup(ui.style())
                        .fill(with_alpha(ui.style().visuals.panel_fill, alpha))
                        // No shadow: a shadow is a solid shape at the edge of the
                        // picture, and a faded HUD with a solid shadow under it looks
                        // like a bug rather than a fade.
                        .shadow(egui::Shadow::NONE);
                    frame.show(ui, |ui| {
                        ui.set_style(faded_style(ui.style(), alpha));
                        ui.horizontal(|ui| {
                            if ui.button("Leave fullscreen (Esc)").clicked() {
                                leave = true;
                            }
                            // Only when there is a speaker to control. A mute button
                            // that silently does nothing is worse than none at all.
                            if has_audio
                                && ui.button(if muted { "Unmute" } else { "Mute" }).clicked()
                            {
                                set_muted = Some(!muted);
                            }
                            if ui.button("Stop watching").clicked() {
                                request_stop = true;
                            }
                        });
                        ui.separator();
                        if let Some(view) = self.view.as_ref() {
                            viewer_audience(ui, view, alpha);
                        }
                    });
                });
        }
        if leave {
            self.set_fullscreen(ctx, false);
        }
        if let Some(muted) = set_muted {
            if let Some(playback) = self
                .view
                .as_ref()
                .and_then(|view| view.audio_playback.as_ref())
            {
                playback.set_muted(muted);
            }
        }
        if request_stop {
            self.stop_view(ctx);
        }
        // Repaint while the HUD is still fading, and settle to a slow tick once it is
        // gone: nothing else on screen changes, and a fullscreen video that keeps
        // asking for frames at 60 Hz for no reason is the whole battery.
        let settling = self
            .view
            .as_ref()
            .is_some_and(|view| now.duration_since(view.hud_idle_since) < HUD_LINGER);
        ctx.request_repaint_after(if settling {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        });
    }

    fn welcome(&self, ui: &mut egui::Ui) {
        ui.heading("Argos");
        ui.add_space(6.0);
        ui.label("Pick someone to watch, or press Share to stream your screen.");
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Everyone on your Radmin network sees each other. No accounts, no servers.",
            )
            .weak(),
        );
    }

    /// One line of the stage-timing table. The peak column is the important
    /// one: a mean over a long window stays flat through an intermittent stall,
    /// while a single 400 ms hitch is the whole story.
    fn stage_row<S: ReportSink>(sink: &mut S, name: &str, stage: &Ema) {
        sink.line(format!(
            "  {name:<11} {:>7.2} ms avg  {:>8.2} ms peak  {:>7} samples",
            stage.mean_ms(),
            stage.peak_ms(),
            stage.samples()
        ));
    }

    fn counter_row<S: ReportSink>(sink: &mut S, name: &str, value: String) {
        sink.line(format!("  {name:<11} {value}"));
    }

    /// Rows for one audio device worker.
    ///
    /// The drift figure is the one that matters over a long session: a buffer
    /// that is slowly emptying looks fine right up until every period is
    /// silence, and the level itself is what says whether that is happening.
    fn audio_rows<S: ReportSink>(sink: &mut S, name: &str, audio: &AudioMetrics, secs: f32) {
        sink.heading(name);
        Self::stage_row(sink, "  device io", &audio.device_io);
        Self::stage_row(sink, "  convert", &audio.convert);
        Self::stage_row(sink, "  codec", &audio.codec);
        Self::counter_row(
            sink,
            "  periods",
            format!(
                "{} ({:.1}/s)",
                audio.periods.get(),
                audio.periods.get() as f32 / secs
            ),
        );
        Self::counter_row(
            sink,
            "  underruns",
            format!(
                "{} ({} periods silent)",
                audio.underruns.dropped(),
                audio.underruns.dropped()
            ),
        );
        Self::counter_row(
            sink,
            "  overruns",
            format!(
                "{} ({} periods clipped)",
                audio.overruns.dropped(),
                audio.overruns.dropped()
            ),
        );
        Self::counter_row(
            sink,
            "  drift",
            format!(
                "{} samples ({:.1}/s), {} device errors, {} restarts",
                audio.drift_samples.get(),
                audio.drift_samples.get() as f32 / secs,
                audio.device_errors.get(),
                audio.restarts.get()
            ),
        );
    }

    /// The always-visible pipeline readout (Ctrl+D).
    ///
    /// Deliberately a floating window rather than a section in the sidebar: it
    /// has to stay readable while the video is stalling, which is exactly when
    /// the sidebar is not what anyone is looking at.
    fn metrics_window(&mut self, ctx: &egui::Context) {
        let mut reset = false;
        let mut copy = false;
        let mut close = false;
        let window = egui::Window::new("Pipeline")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 40.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!(
                        "window {:.0} s",
                        self.metrics_since.elapsed().as_secs_f32()
                    )));
                    if ui.small_button("Reset").clicked() {
                        reset = true;
                    }
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy every number below to the clipboard")
                        .clicked()
                    {
                        copy = true;
                    }
                    if ui.small_button("Close").clicked() {
                        close = true;
                    }
                });
                ui.separator();

                let secs = self.metrics_since.elapsed().as_secs_f32().max(0.001);
                Self::metrics_body(ui, self.share.as_ref(), self.view.as_ref(), secs);
            });
        if window.is_none() {
            self.show_metrics = false;
        }
        if close {
            self.show_metrics = false;
        }
        if copy {
            let secs = self.metrics_since.elapsed().as_secs_f32().max(0.001);
            let mut report = format!(
                "Argos pipeline report — window {secs:.1} s\n\
                 (rates are per second over that window)\n"
            );
            Self::metrics_body(&mut report, self.share.as_ref(), self.view.as_ref(), secs);
            ctx.copy_text(report);
        }
        if reset {
            self.metrics_since = Instant::now();
            if let Some(metrics) = self.share.as_ref().map(|share| &share.metrics) {
                Self::reset_sender(metrics);
            }
            // The per-viewer send timings are part of the readout, so clearing the
            // readout without clearing them would leave the comparison rows
            // describing a window the rest of the panel has forgotten.
            if let Some(share) = self.share.as_ref() {
                share.sharer.reset_write_timings();
            }
            if let Some(metrics) = self.view.as_ref().map(|view| &view.metrics) {
                Self::reset_receiver(metrics);
            }
        }
    }

    /// The readout rows, written to whichever [`ReportSink`] is passed.
    ///
    /// The floating window and the copied report both call this, so the text on
    /// the clipboard cannot drift from the text on screen.
    fn metrics_body<S: ReportSink>(
        sink: &mut S,
        share: Option<&ShareSession>,
        view: Option<&ViewSession>,
        secs: f32,
    ) {
        if let Some(share) = share {
            let m = &share.metrics;
            if let Some(capture) = share.audio_capture.as_ref() {
                Self::audio_rows(sink, "Capture", capture.state().metrics(), secs);
            }
            sink.heading("Sending");
            Self::stage_row(sink, "acquire", &m.acquire);
            Self::stage_row(sink, "convert", &m.convert);
            Self::stage_row(sink, "encode", &m.encode);
            Self::stage_row(sink, "packetize", &m.packetize);
            Self::stage_row(sink, "write", &m.write);
            sink.separator();
            Self::counter_row(
                sink,
                "captured",
                format!(
                    "{} ({:.1} fps, {} dropped)",
                    m.captured.frames(),
                    m.captured.frames() as f32 / secs,
                    m.captured.dropped()
                ),
            );
            Self::counter_row(
                sink,
                "resolution",
                format!(
                    "{}x{} captured -> {}x{} encoded",
                    m.captured_width.get(),
                    m.captured_height.get(),
                    m.encoded_width.get(),
                    m.encoded_height.get()
                ),
            );
            Self::counter_row(
                sink,
                "queued",
                format!(
                    "{} ({:.1} fps, {} dropped)",
                    m.queued.frames(),
                    m.queued.frames() as f32 / secs,
                    m.queued.dropped()
                ),
            );
            Self::counter_row(
                sink,
                "encoded",
                format!(
                    "{} ({:.1} fps, {} dropped)",
                    m.encoded.frames(),
                    m.encoded.frames() as f32 / secs,
                    m.encoded.dropped()
                ),
            );
            Self::counter_row(
                sink,
                "bitrate",
                format!(
                    "{:.2} Mbps, {} keyframes, last IDR {} KiB",
                    m.encoded_bytes.get() as f32 * 8.0 / secs / 1_000_000.0,
                    m.keyframes.get(),
                    m.last_keyframe_bytes.get() / 1024
                ),
            );
            Self::counter_row(
                sink,
                "errors",
                format!(
                    "{} encode, {} video write, {} audio write",
                    m.encode_errors.get(),
                    m.write_errors.get(),
                    m.audio_errors.get()
                ),
            );
            Self::counter_row(
                sink,
                "audio out",
                format!(
                    "{} frames ({:.1}/s), {} KiB",
                    m.audio_frames.get(),
                    m.audio_frames.get() as f32 / secs,
                    m.audio_bytes.get() / 1024
                ),
            );
            if let Some(quality) = share.quality.last_reason() {
                let rung = share.quality.rung();
                Self::counter_row(
                    sink,
                    "adaptive",
                    format!(
                        "rung {rung} of {}, {:.1}% loss, {:.1}% encoder drops — {quality}",
                        argos_core::quality::LADDER.len() - 1,
                        share.quality.smoothed_loss(),
                        share.quality.smoothed_drops()
                    ),
                );
            }
            Self::viewer_comparison(sink, share);
        }

        if let Some(view) = view {
            if share.is_some() {
                sink.separator();
            }
            let m = &view.metrics;
            if let Some(playback) = view.audio_playback.as_ref() {
                Self::audio_rows(sink, "Speaker", playback.state().metrics(), secs);
            }
            sink.heading("Receiving");
            Self::stage_row(sink, "callback", &m.receive);
            Self::stage_row(sink, "depacketize", &m.depacketize);
            Self::stage_row(sink, "decode", &m.decode);
            Self::stage_row(sink, "present", &m.present);
            sink.separator();
            Self::counter_row(
                sink,
                "packets",
                format!(
                    "{} ({:.0}/s), {} lost, {:.2} Mbps",
                    m.packets.get(),
                    m.packets.get() as f32 / secs,
                    m.sequence_losses.get(),
                    m.bytes.get() as f32 * 8.0 / secs / 1_000_000.0
                ),
            );
            Self::counter_row(
                sink,
                "frames",
                format!(
                    "{} decoded ({:.1} fps), {} replaced before display, {} no-picture",
                    m.decoded.frames(),
                    m.decoded.frames() as f32 / secs,
                    m.presented.dropped(),
                    m.no_picture.get()
                ),
            );
            Self::counter_row(
                sink,
                "errors",
                format!(
                    "{} decode, {} audio",
                    m.decode_errors.get(),
                    m.audio_errors.get()
                ),
            );
            Self::counter_row(
                sink,
                "audio in",
                format!(
                    "{} packets ({:.1}/s), {} decoded, {} dropped by device",
                    m.audio_packets.get(),
                    m.audio_packets.get() as f32 / secs,
                    m.audio_decoded.frames(),
                    m.audio_decoded.dropped()
                ),
            );
            // What the sharer is being told, and how many times recovery has
            // actually been asked for. A count, not a timestamp: "idle" and
            // "never needed" look identical on a clock and mean completely
            // different things.
            if view.sharer_id.is_some() {
                Self::counter_row(
                    sink,
                    "recovery",
                    match view.quality.recoveries {
                        0 => "no keyframes requested".to_string(),
                        count => format!(
                            "{count} keyframes requested, last {:.1} s ago, \
                             {:.1}% loss at the time",
                            view.quality.last_keyframe_request.elapsed().as_secs_f32(),
                            view.quality.requested_at_loss
                        ),
                    },
                );
            } else {
                Self::counter_row(sink, "recovery", "no LAN peer — unavailable".to_string());
            }
        }

        if share.is_none() && view.is_none() {
            sink.note("Idle — nothing is streaming.");
        }
    }

    /// One row per viewer, comparing everyone against everyone.
    ///
    /// This is the whole answer to "why is his stream stuttering when mine is
    /// fine", laid out as numbers rather than prose. The columns are the pipeline in
    /// order — what the sharer spent writing to that socket, what the viewer spent
    /// decoding it, what it spent displaying it — so a row that goes red at one point
    /// names the stage without anyone having to reason about it.
    ///
    /// The send column is per viewer rather than the aggregate the `write` row above
    /// reports, because the aggregate is the sum over every viewer and so hides the
    /// one that matters: five fast viewers and one slow one produce an aggregate that
    /// looks fine.
    ///
    /// Nothing here feeds the adaptive controller. These rows exist to be read, and a
    /// verdict that moved the ladder would lower the resolution for everybody because
    /// of one machine.
    fn viewer_comparison<S: ReportSink>(sink: &mut S, share: &ShareSession) {
        if share.viewers.is_empty() {
            return;
        }
        let now = Instant::now();
        sink.separator();
        sink.heading("Viewers");
        let budget = diagnose::frame_budget_ms(share.frame_rate);
        sink.line(format!(
            "  {:<14} {:>5} {:>6} {:>8} {:>8} {:>7}  {}",
            "viewer", "fps", "loss%", "send ms", "decode ms", "drops%", "verdict"
        ));
        for slot in &share.viewers {
            let fresh = slot
                .reported
                .is_some_and(|at| now.duration_since(at) <= VIEWER_REPORT_TTL);
            let (send_mean, send_peak) = share.sharer.viewer_write_ms(&slot.id);
            if !fresh {
                sink.line(format!(
                    "  {:<14} {:>5} {:>6} {:>8} {:>8} {:>7} {:>7}",
                    Self::truncate(slot.label(), 14),
                    "—",
                    "—",
                    format!("{send_mean:.2}"),
                    "—",
                    "—",
                    if slot.connected { "silent" } else { "joining" }
                ));
                continue;
            }
            let verdict = diagnose(&slot.diag, slot.loss, share.frame_rate);
            sink.line(format!(
                "  {:<14} {:>5.0} {:>6.1} {:>8} {:>8} {:>7} {:>7}",
                Self::truncate(slot.label(), 14),
                slot.fps,
                slot.loss,
                format!("{send_mean:.2}/{send_peak:.2}"),
                format!("{:.1}", slot.diag.decode_ms),
                format!("{:.0}", slot.diag.render_drops),
                verdict.describe(share.frame_rate),
            ));
        }
        if let Some(budget) = budget {
            // The one line that turns a column of decode times into a verdict. The
            // rows above are only meaningful against this, and a reader should not
            // have to do the division to know whether 22 ms is alarming.
            let share_of_budget = diagnose::DECODE_BUDGET_SHARE * budget;
            sink.note(&format!(
                "decode budget at {} fps is {budget:.1} ms per frame, so a viewer \
                 above {share_of_budget:.1} ms is too slow for its own machine",
                share.frame_rate,
            ));
        }
        sink.note(
            "send ms is mean/peak for this viewer's socket. Nothing here changes the \
             stream quality; the ladder runs on loss and encoder drops only.",
        );
    }

    /// Shortened to a column width, so one very long peer name cannot push the
    /// numbers off the side of the readout.
    fn truncate(text: &str, width: usize) -> String {
        if text.chars().count() <= width {
            return text.to_string();
        }
        let kept: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    }

    fn reset_sender(metrics: &SenderMetrics) {
        for stage in [
            &metrics.acquire,
            &metrics.readback,
            &metrics.convert,
            &metrics.encode,
            &metrics.packetize,
            &metrics.write,
        ] {
            stage.reset();
        }
        for frames in [&metrics.captured, &metrics.queued, &metrics.encoded] {
            frames.reset();
        }
        for counter in [
            &metrics.encoded_bytes,
            &metrics.last_keyframe_bytes,
            &metrics.keyframes,
            &metrics.captured_width,
            &metrics.captured_height,
            &metrics.encoded_width,
            &metrics.encoded_height,
            &metrics.encode_errors,
            &metrics.write_errors,
            &metrics.audio_frames,
            &metrics.audio_bytes,
            &metrics.audio_errors,
        ] {
            counter.reset();
        }
    }

    fn reset_receiver(metrics: &ReceiverMetrics) {
        for stage in [
            &metrics.depacketize,
            &metrics.decode,
            &metrics.present,
            &metrics.receive,
        ] {
            stage.reset();
        }
        for frames in [&metrics.audio_decoded, &metrics.decoded, &metrics.presented] {
            frames.reset();
        }
        for counter in [
            &metrics.packets,
            &metrics.bytes,
            &metrics.audio_bytes,
            &metrics.audio_packets,
            &metrics.audio_errors,
            &metrics.sequence_losses,
            &metrics.access_units,
            &metrics.no_picture,
            &metrics.decode_errors,
        ] {
            counter.reset();
        }
    }

    fn content(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.screen.clone() {
                    Screen::Home => self.welcome(ui),
                    Screen::Share => self.share_view(ui),
                    Screen::Peer(id) => self.peer_view(ui, ctx, &id),
                    Screen::View => self.watch_view(ui, ctx),
                });
        });
    }

    fn status_text(&self) -> &str {
        if self
            .share
            .as_ref()
            .map(|share| share.sharer.is_connected())
            .unwrap_or(false)
        {
            "sharing"
        } else if self.view.is_some() {
            "watching"
        } else if self.live {
            "live"
        } else if self.capture.is_some() {
            "previewing"
        } else {
            "offline"
        }
    }
}

/// Where a pipeline readout is written: the floating window, or the clipboard.
///
/// Both render through the same row helpers, so the copied report and the
/// on-screen text cannot drift apart.
trait ReportSink {
    /// A section title, e.g. `Sending`.
    fn heading(&mut self, text: &str);
    /// A plain, de-emphasised line, e.g. the idle message.
    fn note(&mut self, text: &str);
    /// One pre-formatted readout row.
    fn line(&mut self, text: String);
    /// A blank separator between sections.
    fn separator(&mut self);
}

impl ReportSink for egui::Ui {
    fn heading(&mut self, text: &str) {
        self.label(RichText::new(text).strong());
    }

    fn note(&mut self, text: &str) {
        self.label(RichText::new(text).weak());
    }

    fn line(&mut self, text: String) {
        self.monospace(text);
    }

    fn separator(&mut self) {
        egui::Ui::separator(self);
    }
}

impl ReportSink for String {
    fn heading(&mut self, text: &str) {
        if !self.is_empty() {
            self.push('\n');
        }
        self.push_str(text);
        self.push('\n');
    }

    fn note(&mut self, text: &str) {
        self.push_str(text);
        self.push('\n');
    }

    fn line(&mut self, text: String) {
        self.push_str(&text);
        self.push('\n');
    }

    fn separator(&mut self) {}
}

impl eframe::App for ArgosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input_mut(|input| input.key_pressed(egui::Key::D) && input.modifiers.ctrl) {
            self.show_metrics = !self.show_metrics;
        }
        let fullscreen = self.view.as_ref().is_some_and(|view| view.fullscreen);
        if ctx.input(|input| fullscreen_key(input, fullscreen)) {
            self.set_fullscreen(ctx, !fullscreen);
        }
        ctx.set_visuals(egui::Visuals::dark());
        self.poll_lan_events();
        // Read after the events, because an event can finish the session the
        // fullscreen flag belonged to.
        let fullscreen = self.view.as_ref().is_some_and(|view| view.fullscreen);
        if !fullscreen {
            self.top_bar(ctx);
            self.side_bar(ctx);
        }
        if self.capture.is_some() {
            let active = self.update_capture_active();
            self.poll_capture(ctx);
            ctx.request_repaint_after(if active {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(250)
            });
        }
        if self.share.is_some() {
            self.poll_share_audio();
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if self.view.is_some() {
            let visible = self.view_visible();
            self.poll_view(ctx, visible);
            self.poll_reconnect();
            ctx.request_repaint_after(if visible {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(250)
            });
        }
        if fullscreen {
            self.fullscreen_view(ctx);
        } else {
            self.content(ctx);
        }
        if self.show_metrics {
            // The overlay is the only thing that redraws while nothing else
            // would, so a stall that has already stopped the pipeline still
            // refreshes the numbers that explain it.
            self.metrics_window(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.lan.is_some() && self.capture.is_none() && self.view.is_none() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.show_settings && !fullscreen {
            self.settings_window(ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        audience_from, decode_target, encode_target, lan, roster_is_stale, worst_link, ViewerSlot,
        KEYFRAME_REQUEST_FLOOR, ROSTER_STALE, VIEWER_REPORT_TTL,
    };
    use std::time::{Duration, Instant};

    fn slot(name: &str) -> ViewerSlot {
        ViewerSlot::new(name, name, Instant::now())
    }

    /// Marks the slot as having reported `loss`/`fps` as of `at`.
    fn reported(mut slot: ViewerSlot, loss: f32, fps: f32, at: Instant) -> ViewerSlot {
        slot.loss = loss;
        slot.fps = fps;
        slot.reported = Some(at);
        slot
    }

    #[test]
    fn the_height_target_round_trips_through_the_atomic() {
        for height in [None, Some(2), Some(240), Some(540), Some(720), Some(1080)] {
            assert_eq!(decode_target(encode_target(height)), height);
        }
    }

    #[test]
    fn native_is_a_negative_sentinel_not_a_height() {
        assert!(encode_target(None) < 0);
        assert_eq!(decode_target(-1), None);
    }

    /// A share serves one stream to everyone, so the resolution has to be chosen
    /// for the link that needs it most. Picking the average would let one bad
    /// viewer be carried by a good one, which is the failure this exists to
    /// prevent.
    #[test]
    fn the_ladder_answers_to_the_worst_viewer_not_the_average() {
        let now = Instant::now();
        let slots = vec![
            reported(slot("alice"), 0.2, 60.0, now),
            reported(slot("bob"), 7.5, 41.0, now),
            reported(slot("carol"), 0.0, 60.0, now),
        ];
        let (loss, fps) = worst_link(&slots, now).expect("someone reported");
        assert_eq!(loss, 7.5, "the worst link's loss is the one that counts");
        assert_eq!(
            fps, 41.0,
            "the worst link's frame rate is the one that counts"
        );
    }

    /// One viewer leaves and their last measurement stops being evidence. Acting
    /// on it would keep the resolution down for nobody.
    #[test]
    fn a_viewer_who_went_quiet_stops_steering_the_ladder() {
        let now = Instant::now();
        let stale = reported(
            slot("gone"),
            9.0,
            20.0,
            now - VIEWER_REPORT_TTL - Duration::from_millis(1),
        );
        let fresh = reported(slot("here"), 0.5, 60.0, now);
        let (loss, fps) = worst_link(&[stale, fresh], now).expect("someone is still reporting");
        assert_eq!(loss, 0.5);
        assert_eq!(fps, 60.0);

        // With everyone stale there is nothing to act on, which is different from
        // finding a healthy link and climbing on it.
        let all_stale = vec![reported(
            slot("gone"),
            9.0,
            20.0,
            now - VIEWER_REPORT_TTL - Duration::from_millis(1),
        )];
        assert!(worst_link(&all_stale, now).is_none());
    }

    /// A viewer that never got as far as sending a report has no measurement,
    /// and must not be counted as a lossless link.
    #[test]
    fn a_viewer_who_has_not_reported_yet_is_not_a_healthy_link() {
        let now = Instant::now();
        let silent = slot("connecting");
        assert!(silent.reported.is_none());
        assert!(worst_link(&[silent], now).is_none());
    }

    /// The second viewer has to get the offer already waiting for them, not a
    /// slot that is treated as finished because nobody has answered a *first*
    /// one yet.
    #[test]
    fn a_slot_is_live_until_it_connects_or_its_answer_window_lapses() {
        let now = Instant::now();
        let mut fresh = slot("fresh");
        fresh.connected = false;
        assert!(
            fresh.is_live(now),
            "an unanswered offer is still worth waiting on"
        );
        assert!(
            !fresh.is_live(now + super::VIEWER_ANSWER_TIMEOUT + Duration::from_millis(1)),
            "a slot past its answer window can be handed out again"
        );
        let connected = ViewerSlot {
            connected: true,
            ..slot("connected")
        };
        assert!(
            connected.is_live(now + Duration::from_secs(3600)),
            "a served viewer is live whatever the clock says"
        );
    }

    /// The first request a viewer makes has to get through: starting the floor
    /// at zero would make the rate limiter swallow the request that matters.
    #[test]
    fn the_first_keyframe_request_is_not_eaten_by_the_rate_limit() {
        let slot = slot("first");
        assert!(
            Instant::now().duration_since(slot.last_keyframe_request) >= KEYFRAME_REQUEST_FLOOR,
            "a brand new slot is already outside the floor, so its first request counts"
        );
    }

    /// The reported symptom: one viewer says their stream is dropping frames
    /// while the sharer sees nothing wrong at all. The sharer's own line has to
    /// name the cause, or the two sides of the same stream disagree about whether
    /// anything is wrong.
    #[test]
    fn a_slow_viewer_decodes_are_reported_as_a_slow_decoder() {
        let now = Instant::now();
        let mut viewer = reported(slot("bob"), 0.4, 30.0, now);
        viewer.connected = true;
        viewer.diag = lan::Diagnostics {
            // 30 fps leaves 33 ms a frame and the decoder is using almost all of
            // it. The packets arrived; there is nothing for the ladder to send
            // less of.
            decode_ms: 31.0,
            present_ms: 1.0,
            render_drops: 0.0,
            decode_errors: 0.0,
            waiting_keyframe: false,
        };
        let line = viewer.summary(now, 30).text().to_string();
        assert!(
            line.contains("decoder too slow"),
            "expected the decoder to be named, got {line:?}"
        );
    }

    /// The three states a viewer row can be in have to stay distinguishable. In
    /// particular a viewer that has gone quiet must not be shown as healthy: the
    /// whole reason report freshness is tracked is so that silence stops counting
    /// as evidence, and a row reading "ok" for a viewer who is not talking would
    /// undo that.
    #[test]
    fn a_viewer_row_distinguishes_connecting_silent_and_reporting() {
        let now = Instant::now();
        let mut connecting = slot("new");
        connecting.connected = false;
        let line = connecting.summary(now, 30).text().to_string();
        assert!(line.contains("connecting"), "got {line:?}");

        let mut silent = reported(slot("quiet"), 0.0, 60.0, now);
        silent.connected = true;
        silent.reported = Some(now - VIEWER_REPORT_TTL - Duration::from_millis(1));
        let line = silent.summary(now, 30).text().to_string();
        assert!(line.contains("not reporting"), "got {line:?}");

        let mut healthy = reported(slot("fine"), 0.0, 60.0, now);
        healthy.connected = true;
        let line = healthy.summary(now, 30).text().to_string();
        assert!(line.contains("ok"), "got {line:?}");
        assert!(
            !line.contains("not reporting") && !line.contains("connecting"),
            "a reporting viewer read as {line:?}"
        );
    }

    /// The audience list a viewer shows ages, and a headcount that quietly stops
    /// being true is worse than an obviously old one: it reads as fact. Nothing
    /// arrives before the first roster, and that has to read as "not known yet"
    /// rather than as a fresh list of nobody.
    #[test]
    fn an_audience_list_is_believed_until_the_sharer_goes_quiet() {
        let now = Instant::now();
        assert!(
            !roster_is_stale(Some(now), now),
            "a list that just arrived is fresh"
        );
        assert!(
            roster_is_stale(Some(now - ROSTER_STALE - Duration::from_millis(1)), now),
            "a list past the staleness window is not to be believed"
        );
        // Never told anything is not the same as told something and then not told
        // again; only the second one is worth a warning.
        assert!(
            !roster_is_stale(None, now),
            "no list yet is not a stale list"
        );
    }

    /// The shared list is the only reason a viewer can tell that somebody else is
    /// in the room. If the roster arrives it must be shown in full — including
    /// people still connecting, since "two people are watching" and "one is
    /// watching and one is waiting for their code" are different facts.
    #[test]
    fn the_viewers_audience_is_whatever_the_sharer_last_said() {
        let roster = vec![
            lan::RosterEntry {
                id: "a".to_string(),
                name: "alice".to_string(),
                connected: true,
            },
            lan::RosterEntry {
                id: "b".to_string(),
                name: String::new(),
                connected: false,
            },
        ];
        let audience = audience_from(&roster, Some("a"), "me");
        assert_eq!(audience.len(), 2);
        assert_eq!(audience[0].label(), "alice");
        assert_eq!(
            audience[1].label(),
            "Anonymous",
            "a peer with no name must not render as a blank row"
        );
        assert!(
            !audience[1].connected,
            "a connecting viewer is not watching yet"
        );
    }

    /// A manual-code session is never told anything, so the only viewer it can
    /// know about is the one reading the screen. Showing an empty room there would
    /// claim nobody is watching while somebody is.
    #[test]
    fn a_manual_session_knows_of_exactly_itself() {
        let audience = audience_from(&[], Some("aabb"), "alice");
        assert_eq!(audience.len(), 1);
        assert_eq!(audience[0].id, "aabb");
        assert_eq!(audience[0].label(), "alice");
    }
}
