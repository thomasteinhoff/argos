//! Wire-format and dispatch tests for the LAN signal messages.
//!
//! These matter more than they look. Two of the message kinds carry the whole
//! recovery path — a viewer asks for a keyframe and reports its view of the
//! link — and they travel as UDP between two separately built programs, where
//! a renamed string literal is an error nothing reports. So the tests drive the
//! real [`dispatch`] over real serialised bytes rather than restating the
//! mapping next to it.

use super::{dispatch, LanEvent, Message};

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
    let bytes = serde_json::to_vec(&sent).expect("serialise");
    match dispatch(serde_json::from_slice(&bytes).expect("deserialise")) {
        Some(LanEvent::Report { loss, fps, .. }) => {
            assert!((loss - 7.5).abs() < f32::EPSILON);
            assert!((fps - 28.0).abs() < f32::EPSILON);
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

/// A newer version's message kind must be ignored, not misread. This is what
/// lets the two apps be upgraded independently.
#[test]
fn an_unknown_kind_is_ignored_rather_than_misread() {
    assert!(over_the_wire("something-newer").is_none());
}
