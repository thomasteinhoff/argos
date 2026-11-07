//! Wire-format and dispatch tests for the LAN signal messages.
//!
//! These matter more than they look. Two of the message kinds carry the whole
//! recovery path — a viewer asks for a keyframe and reports its view of the
//! link — and they travel as UDP between two separately built programs, where
//! a renamed string literal is an error nothing reports. So the tests drive the
//! real [`dispatch`] over real serialised bytes rather than restating the
//! mapping next to it.

use super::{
    dispatch, interface_broadcast, select_vpn_or_lan_address, CursorUpdate, Diagnostics, LanEvent,
    Message, RosterEntry,
};
use std::net::Ipv4Addr;

/// The id this process would answer to. Messages must be addressed to it.
const US: &str = "0123456789abcdef";
/// The peer that sent the message.
const THEM: &str = "fedcba9876543210";

fn message(kind: &str) -> Message {
    Message {
        kind: kind.to_string(),
        id: THEM.to_string(),
        name: String::new(),
        to: US.to_string(),
        port: 45893,
        sharing: false,
        sdp: String::new(),
        loss: 0.0,
        fps: 0.0,
        diag: Diagnostics::default(),
        roster: Vec::new(),
        source_fps: 0,
        cursor: None,
    }
}

fn roster_entry(id: &str, name: &str, connected: bool) -> RosterEntry {
    RosterEntry {
        id: id.to_string(),
        name: name.to_string(),
        connected,
    }
}

/// Round-trips through the wire format, so a test cannot pass on a field the
/// serialiser would drop.
fn over_the_wire(kind: &str) -> Option<LanEvent> {
    let bytes = serde_json::to_vec(&message(kind)).expect("serialise");
    dispatch(serde_json::from_slice(&bytes).expect("deserialise"))
}

#[test]
fn every_kind_reaches_its_handler() {
    assert!(matches!(
        over_the_wire("request"),
        Some(LanEvent::Request { .. })
    ));
    assert!(matches!(
        over_the_wire("offer"),
        Some(LanEvent::Offer { .. })
    ));
    assert!(matches!(
        over_the_wire("answer"),
        Some(LanEvent::Answer { .. })
    ));
    // Sent just before a viewer's hard exit, so the sharer frees its slot
    // instead of counting a ghost forever.
    assert!(matches!(over_the_wire("bye"), Some(LanEvent::Bye { .. })));
    // Sent by the sharer when it ends its stream on purpose, so a viewer stops
    // reconnecting into nothing.
    assert!(matches!(over_the_wire("stop"), Some(LanEvent::Stop { .. })));
    // The two that carry recovery. Without these two the viewer is stuck
    // waiting for an intra frame that will only arrive on the 4 s backstop,
    // and the sharer never learns what its link looks like.
    assert!(matches!(
        over_the_wire("keyframe"),
        Some(LanEvent::Keyframe { .. })
    ));
    assert!(matches!(
        over_the_wire("report"),
        Some(LanEvent::Report { .. })
    ));
    // Without this a viewer shows an empty audience forever: it has no way to
    // discover who else is watching this stream, because the sharer is the only
    // party that knows.
    assert!(matches!(
        over_the_wire("roster"),
        Some(LanEvent::Roster { .. })
    ));
}

/// The audience list is the whole point of the message, so it has to survive the
/// wire intact — including who is still negotiating, which is the difference
/// between "two people are watching" and "one person is watching and one is
/// waiting for their code".
#[test]
fn a_roster_carries_every_viewer_and_the_shape_of_the_stream() {
    let mut sent = message("roster");
    sent.roster = vec![
        roster_entry("aaaa", "desk", true),
        roster_entry("bbbb", "", false),
    ];
    sent.source_fps = 30;
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
        Some(LanEvent::Roster {
            viewers,
            source_fps,
            ..
        }) => {
            assert_eq!(viewers.len(), 2);
            assert_eq!(viewers[0].name, "desk");
            assert!(viewers[0].connected);
            // A peer that sent no name is shown as Anonymous rather than as a
            // blank line, which is indistinguishable from a rendering fault.
            assert_eq!(viewers[1].label(), "Anonymous");
            assert!(!viewers[1].connected);
            assert_eq!(source_fps, 30);
        }
        other => panic!("expected a Roster event, got {other:?}"),
    }
}

/// The cursor payload is a picture plus a position, and the viewer draws both:
/// dropping the rgba or mangling the fractions would show a wrong pointer.
#[test]
fn a_cursor_carries_position_shape_and_desktop_anchor() {
    let mut sent = message("cursor");
    sent.cursor = Some(CursorUpdate {
        x: 0.25,
        y: 0.5,
        desktop_w: 1920,
        desktop_h: 1080,
        width: 32,
        height: 32,
        hotspot_x: 2,
        hotspot_y: 2,
        generation: 3,
        visible: true,
        rgba: vec![0u8; 32 * 32 * 4],
    });
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
        Some(LanEvent::Cursor { cursor, .. }) => {
            assert_eq!(cursor.x, 0.25);
            assert_eq!(cursor.y, 0.5);
            assert_eq!(cursor.desktop_w, 1920);
            assert_eq!(cursor.generation, 3);
            assert!(cursor.visible);
            assert_eq!(cursor.rgba.len(), 32 * 32 * 4);
        }
        other => panic!("expected a Cursor event, got {other:?}"),
    }
}

/// A cursor whose position did not survive JSON (a non-finite float serialises
/// as `null` and fails to parse) must be refused, not silently treated as the
/// top-left corner.
#[test]
fn a_cursor_with_nowhere_to_be_is_refused() {
    let mut sent = message("cursor");
    sent.cursor = Some(CursorUpdate {
        x: f32::NAN,
        ..Default::default()
    });
    let bytes: Vec<u8> = serde_json::to_string(&sent) // to_string so NAN becomes null
        .unwrap_or_default()
        .into_bytes();
    let parsed = serde_json::from_slice(&bytes);
    // Serde serialises NAN as null, so the message must fail to parse at all.
    assert!(parsed.is_err() || dispatch(parsed.ok().unwrap()).is_none());
}

/// An older build that knows nothing about the cursor field still exchanges
/// every other message: a message without it deserialises to `cursor: None`.
#[test]
fn a_message_without_the_cursor_field_still_parses() {
    let mut sent = message("request");
    sent.cursor = None;
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    // Strip the field, as a build predating it would send.
    let text = String::from_utf8(bytes).expect("utf8");
    let text = text.replacen(",\"cursor\":null", "", 1);
    assert!(!text.contains("cursor"));
    let parsed: Message = serde_json::from_str(&text).expect("parse without cursor field");
    assert!(parsed.cursor.is_none());
    assert!(matches!(
        dispatch(serde_json::from_slice(text.as_bytes()).expect("deserialise")),
        Some(LanEvent::Request { .. })
    ));
}

/// The sender's id is what the sharer uses to decide whether the request came
/// from the viewer it is actually serving. Losing it would mean a keyframe
/// request that can never match a peer.
#[test]
fn the_senders_id_survives() {
    match over_the_wire("keyframe") {
        Some(LanEvent::Keyframe { id }) => assert_eq!(id, THEM),
        other => panic!("expected a Keyframe event, got {other:?}"),
    }
}

#[test]
fn a_report_carries_its_measurements() {
    let mut sent = message("report");
    sent.loss = 7.5;
    sent.fps = 28.0;
    sent.diag = Diagnostics {
        decode_ms: 38.9,
        present_ms: 1.4,
        render_drops: 41.2,
        decode_errors: 2.1,
        waiting_keyframe: false,
    };
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
        Some(LanEvent::Report {
            loss, fps, diag, ..
        }) => {
            assert!((loss - 7.5).abs() < f32::EPSILON);
            assert!((fps - 28.0).abs() < f32::EPSILON);
            assert!((diag.decode_ms - 38.9).abs() < f32::EPSILON);
            assert!((diag.present_ms - 1.4).abs() < f32::EPSILON);
            assert!((diag.render_drops - 41.2).abs() < f32::EPSILON);
            assert!((diag.decode_errors - 2.1).abs() < f32::EPSILON);
            assert!(!diag.waiting_keyframe);
        }
        other => panic!("expected a Report event, got {other:?}"),
    }
}

/// A report from a build that predates the diagnostics carries no `diag` field at
/// all, and has to keep working: loss and frame rate are what the quality
/// controller runs on, and losing those would silently freeze the ladder for
/// anyone with one end on an older build.
///
/// The absence has to read as "not measured" rather than "measured as perfect",
/// which is why the defaults are all zero *and* the sharer treats a viewer that
/// has never reported diagnostics as undiagnosed instead of healthy.
#[test]
fn a_report_from_an_older_build_still_carries_loss_and_fps() {
    let legacy = br#"{"kind":"report","id":"ccdd","to":"aabb","port":1,"loss":3.0,"fps":12.0}"#;
    let parsed: Message = serde_json::from_slice(legacy).expect("legacy report must parse");
    assert!((parsed.loss - 3.0).abs() < f32::EPSILON);
    assert!((parsed.fps - 12.0).abs() < f32::EPSILON);
    match dispatch(parsed) {
        Some(LanEvent::Report {
            loss, fps, diag, ..
        }) => {
            assert!((loss - 3.0).abs() < f32::EPSILON);
            assert!((fps - 12.0).abs() < f32::EPSILON);
            // Every diagnostic reads as unmeasured, which is a state the sharer
            // can display as "not reporting" rather than as a healthy viewer.
            assert_eq!(diag, Diagnostics::default());
        }
        other => panic!("expected a Report event, got {other:?}"),
    }
}

/// The diagnostics are displayed next to each other as if they were measured on
/// the same machine, so one peer must not be able to report a drop rate outside
/// the range a percentage can take. Clamped for the same reason loss is.
#[test]
fn out_of_range_diagnostics_are_clamped() {
    let mut sent = message("report");
    sent.diag = Diagnostics {
        decode_ms: 12.0,
        present_ms: -3.0,
        render_drops: 480.0,
        decode_errors: -1.0,
        waiting_keyframe: true,
    };
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
        Some(LanEvent::Report { diag, .. }) => {
            assert!((diag.render_drops - 100.0).abs() < f32::EPSILON);
            assert_eq!(diag.present_ms, 0.0);
            assert_eq!(diag.decode_errors, 0.0);
            assert!((diag.decode_ms - 12.0).abs() < f32::EPSILON);
            assert!(diag.waiting_keyframe);
        }
        other => panic!("expected a Report event, got {other:?}"),
    }
}

/// Loss is a percentage. One peer reporting nonsense must not be able to drive
/// the sharer's ladder off either end.
#[test]
fn out_of_range_measurements_are_clamped() {
    for (sent_loss, want_loss) in [(1000.0, 100.0), (-5.0, 0.0)] {
        let mut sent = message("report");
        sent.loss = sent_loss;
        let bytes = serde_json::to_vec(&sent).expect("serialise");
        match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
            Some(LanEvent::Report { loss, .. }) => assert_eq!(loss, want_loss),
            other => panic!("expected a Report event, got {other:?}"),
        }
    }
}

/// A `hello` from an older build has no `loss` or `fps` field at all. It must
/// still parse: the beacon is how peers discover each other, and a parse
/// failure there would make two versions of the app unable to see each other
/// at all.
///
/// This also documents why [`dispatch`] validates rather than trusting the
/// defaults. A datagram that parses but omits the measurements produces exactly
/// the same "no loss, no frames" reading, and that reading is the one the
/// quality controller would climb on.
#[test]
fn a_message_without_the_new_fields_still_parses() {
    let legacy = br#"{"kind":"hello","id":"aabb","name":"desk","to":"","port":1234,"sharing":true,"sdp":""}"#;
    let parsed: Message = serde_json::from_slice(legacy).expect("legacy message must parse");
    assert_eq!(parsed.kind, "hello");
    assert_eq!(parsed.id, "aabb");
    assert!(parsed.sharing);
    assert_eq!(parsed.loss, 0.0);
    assert_eq!(parsed.fps, 0.0);
    assert_eq!(parsed.diag, Diagnostics::default());
    assert!(parsed.roster.is_empty());

    // Same datagram shaped as a report: it parses, and it reads as a perfect
    // link, which is exactly why the sender's id is also what the sharer
    // trusts it to be.
    let forged = br#"{"kind":"report","id":"ccdd","to":"aabb","port":1,"loss":0.0,"fps":0.0}"#;
    let parsed: Message = serde_json::from_slice(forged).expect("parse");
    assert_eq!(parsed.loss, 0.0);
}

/// NaN and infinity have no JSON representation and serialise as `null`, which
/// fails to deserialise into an `f32`. The datagram is dropped before dispatch.
///
/// Verified rather than assumed: if this ever regressed, a peer could hand the
/// quality controller a fabricated reading and the ladder would climb on
/// evidence nobody sent.
#[test]
fn a_non_finite_measurement_is_rejected_at_parse_time() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut sent = message("report");
        sent.loss = bad;
        let bytes = serde_json::to_vec(&sent).expect("serialise");
        assert!(
            serde_json::from_slice::<Message>(&bytes).is_err(),
            "{bad} was not rejected"
        );
    }
}

/// The same rule for the diagnostics: a non-finite measurement would render as
/// `NaN` or `inf` in a column the sharer compares across viewers, where it looks
/// like a real reading.
#[test]
fn a_non_finite_diagnostic_is_rejected_at_parse_time() {
    let mut sent = message("report");
    sent.diag.decode_ms = f32::NAN;
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    assert!(serde_json::from_slice::<Message>(&bytes).is_err());
}

/// A newer version's message kind must be ignored, not misread. This is what
/// lets the two apps be upgraded independently.
#[test]
fn an_unknown_kind_is_ignored_rather_than_misread() {
    assert!(over_the_wire("something-newer").is_none());
}

#[test]
fn zerotier_interface_takes_priority_over_local_ethernet_and_virtual_bridges() {
    let candidates = vec![
        ("docker0".to_string(), Ipv4Addr::new(172, 17, 0, 1)),
        ("enp3s0".to_string(), Ipv4Addr::new(192, 168, 1, 20)),
        ("ztcf123456".to_string(), Ipv4Addr::new(10, 147, 17, 5)),
    ];
    assert_eq!(
        select_vpn_or_lan_address(&candidates),
        Some(Ipv4Addr::new(10, 147, 17, 5))
    );
}

#[test]
fn private_lan_skips_container_bridges_when_no_vpn_is_active() {
    let candidates = vec![
        ("docker0".to_string(), Ipv4Addr::new(172, 17, 0, 1)),
        ("virbr0".to_string(), Ipv4Addr::new(192, 168, 122, 1)),
        ("wlan0".to_string(), Ipv4Addr::new(192, 168, 0, 42)),
    ];
    assert_eq!(
        select_vpn_or_lan_address(&candidates),
        Some(Ipv4Addr::new(192, 168, 0, 42))
    );
}

#[test]
fn directed_broadcast_is_computed_from_netmask_when_os_omits_it() {
    let ip = Ipv4Addr::new(10, 147, 17, 5);
    let mask = Ipv4Addr::new(255, 255, 255, 0);
    assert_eq!(
        interface_broadcast(ip, mask, None),
        Some(Ipv4Addr::new(10, 147, 17, 255))
    );
    assert_eq!(
        interface_broadcast(Ipv4Addr::LOCALHOST, Ipv4Addr::new(255, 0, 0, 0), None),
        None
    );
}
