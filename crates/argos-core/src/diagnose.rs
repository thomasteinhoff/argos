//! Turning a viewer's measurements into a verdict about where a stutter comes
//! from.
//!
//! A viewer that is dropping frames has told the sharer only one fact — a low
//! frame rate — and that fact has two completely different causes with opposite
//! fixes. Packets going missing is the network's problem, and the quality ladder
//! can answer it by sending less. Packets arriving intact and then not being
//! decoded, or decoded and then not displayed, is the viewer's machine being too
//! slow, and lowering the resolution would make the stream worse for everybody to
//! compensate for one person. The two look identical in the two numbers that have
//! always crossed the wire, which is why the sharer needed a third source of
//! evidence before the question could be answered at all.
//!
//! Nothing here feeds the ladder. [`crate::quality`] still moves only on loss and
//! dropped frames, and this module is deliberately a separate call: a verdict that
//! quietly changed a setting shared by every viewer would be a policy change
//! wearing instrumentation's clothes. What it produces is a label on numbers
//! somebody has to read.

use crate::lan::Diagnostics;

/// Where the frames are being lost, given what a viewer reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bottleneck {
    /// Nothing measurably wrong.
    Ok,
    /// Packets are going missing between here and the sharer.
    Link,
    /// The decoder cannot keep up with the frame budget.
    Decode,
    /// Frames decode but the UI never displays them.
    Render,
    /// Packets arrive intact and nothing decodes: waiting on an intra frame.
    Keyframe,
    /// No packets are arriving at all, or none yet.
    NoSignal,
}

impl Bottleneck {
    /// One line, for a table cell or a status line.
    pub fn describe(&self, source_fps: u32) -> String {
        match self {
            Bottleneck::Ok => "ok".to_string(),
            Bottleneck::Link => "link is dropping packets".to_string(),
            Bottleneck::Decode => match frame_budget_ms(source_fps) {
                // Without a frame rate there is no budget to be over, so the
                // number is left out rather than invented.
                Some(budget) => format!("decoder too slow for a {budget:.0} ms frame"),
                None => "decoder too slow".to_string(),
            },
            Bottleneck::Render => "frames decoded but never displayed".to_string(),
            Bottleneck::Keyframe => "waiting for a keyframe".to_string(),
            Bottleneck::NoSignal => "no packets arriving".to_string(),
        }
    }
}

/// Loss at or above this makes the link the answer, whatever else is also true.
///
/// The same 2% the viewer itself uses to decide it has lost enough frames to ask
/// the sharer for a keyframe. Reusing the number is deliberate — it is already the
/// threshold the code treats as "this link is in trouble" — but the two decisions
/// are different enough to be worth separating in the reasoning: asking for a
/// keyframe is cheap and worth doing on a single burst, while naming the link as
/// the bottleneck is a claim about what to change and should not be made on one
/// bad window.
pub const LOSS_THRESHOLD: f32 = 2.0;

/// Decode time above this share of the frame budget makes the machine the answer.
///
/// Below the budget rather than at it, because a decoder averaging exactly the
/// budget has already spent the whole frame interval on decoding and has nothing
/// left for the rest of the pipeline — and the rest of the pipeline includes a
/// compositor on a machine that is already busy. Equal is not a margin anyone can
/// rely on.
pub const DECODE_BUDGET_SHARE: f32 = 0.8;

/// Render drops at or above this make the display path the answer.
///
/// A handful of replaced frames is normal, because replacing a frame is what a
/// single-slot display does whenever a repaint arrives late. This only fires on a
/// rate that is visible as a stutter rather than as a measurement.
pub const RENDER_DROP_THRESHOLD: f32 = 5.0;

/// Milliseconds available per frame at the given rate, or `None` when the rate is
/// unknown or nonsensical.
///
/// A budget has to come from the stream rather than a default: deciding a decode
/// is too slow means comparing it against something, and substituting a guessed
/// frame rate would produce a confident verdict out of no evidence.
pub fn frame_budget_ms(source_fps: u32) -> Option<f32> {
    if source_fps == 0 || source_fps > 1000 {
        return None;
    }
    Some(1000.0 / source_fps as f32)
}

/// What the numbers add up to.
///
/// The order is not stylistic. Loss is checked before everything because a lossy
/// link produces decode errors, render drops and starvation as downstream
/// symptoms, and blaming the machine for those would name the wrong end of the
/// pipe. Within that, the cheap causes come first: a viewer waiting on a keyframe
/// has a frame rate identical to a viewer whose decoder cannot keep up, and the
/// one that is fixed by a request the viewer can already send.
///
/// `source_fps` is the stream's rate, which the sharer is the only one who knows,
/// and is what turns a decode time into a verdict.
pub fn diagnose(diag: &Diagnostics, loss: f32, source_fps: u32) -> Bottleneck {
    // Nothing has been measured yet. Distinguishes "this viewer is fine" from
    // "this viewer has not told us anything", which for a peer on a build from
    // before these fields existed is the normal case and not a fault.
    if !loss.is_finite() {
        return Bottleneck::NoSignal;
    }
    if loss >= LOSS_THRESHOLD {
        return Bottleneck::Link;
    }
    if diag.waiting_keyframe {
        return Bottleneck::Keyframe;
    }
    // A decode time cannot be called too slow without a budget to compare it
    // against, so the machine is only ever blamed when the stream told us how
    // fast it is going.
    if let Some(budget) = frame_budget_ms(source_fps) {
        if diag.decode_ms > budget * DECODE_BUDGET_SHARE {
            return Bottleneck::Decode;
        }
    }
    if diag.render_drops >= RENDER_DROP_THRESHOLD {
        return Bottleneck::Render;
    }
    if diag.decode_errors > 0.0 {
        // Errors with no loss to explain them: a frame was damaged in transit
        // without enough missing packets to show as a burst, or the decoder lost
        // sync for another reason. Named as a link problem because that is where
        // the evidence points, not as a machine problem the evidence contradicts.
        return Bottleneck::Link;
    }
    Bottleneck::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A viewer measuring nothing at all: no loss, and every diagnostic at its
    /// default. What a peer on an older build looks like.
    fn silent() -> Diagnostics {
        Diagnostics::default()
    }

    /// A healthy viewer: clean link, decode well inside the budget, no drops.
    fn healthy() -> Diagnostics {
        Diagnostics {
            decode_ms: 3.0,
            present_ms: 0.8,
            render_drops: 0.0,
            decode_errors: 0.0,
            waiting_keyframe: false,
        }
    }

    /// The reported symptom this module exists to explain: heavy frame drops at
    /// 30 fps on the viewer's machine, nothing visible at all on the sharer's.
    /// The packets are arriving — that is the whole point — and the decoder cannot
    /// get through them.
    #[test]
    fn a_viewer_whose_decoder_misses_the_budget_is_not_a_link_problem() {
        let diag = Diagnostics {
            decode_ms: 31.0,
            present_ms: 1.1,
            render_drops: 0.0,
            decode_errors: 0.0,
            waiting_keyframe: false,
        };
        // 30 fps leaves 33.3 ms a frame; 31 ms of it is decode.
        assert_eq!(diagnose(&diag, 0.0, 30), Bottleneck::Decode);
    }

    /// The same decode time can be fast enough or too slow depending only on how
    /// often a frame is due, which is what makes the frame rate necessary rather than
    /// a fixed millisecond threshold. 20 ms of work leaves room in a 30 fps frame
    /// (33 ms) and none at all in a 60 fps one (17 ms), and the machine has not
    /// changed between them.
    #[test]
    fn a_decode_time_is_judged_against_the_streams_frame_rate() {
        let diag = Diagnostics {
            decode_ms: 20.0,
            present_ms: 1.1,
            render_drops: 0.0,
            decode_errors: 0.0,
            waiting_keyframe: false,
        };
        assert_eq!(diagnose(&diag, 0.0, 30), Bottleneck::Ok);
        assert_eq!(diagnose(&diag, 0.0, 60), Bottleneck::Decode);
    }

    /// The verdict cannot name a machine when it does not know how fast the
    /// stream is going. Falling back to a guessed frame rate here would produce a
    /// confident answer about a viewer nobody measured.
    #[test]
    fn an_unknown_frame_rate_cannot_condemn_a_decoder() {
        let diag = Diagnostics {
            decode_ms: 400.0,
            present_ms: 0.0,
            render_drops: 0.0,
            decode_errors: 0.0,
            waiting_keyframe: false,
        };
        assert_eq!(diagnose(&diag, 0.0, 0), Bottleneck::Ok);
        // An impossible rate is treated as no rate at all, rather than
        // producing a budget of a thousandth of a millisecond.
        assert_eq!(diagnose(&diag, 0.0, 5000), Bottleneck::Ok);
    }

    /// Loss explains everything downstream of it. A lossy link also produces
    /// decode errors and dropped frames, and blaming the viewer's machine for
    /// those would point the fix in exactly the wrong direction.
    #[test]
    fn loss_outranks_every_machine_symptom() {
        let diag = Diagnostics {
            decode_ms: 90.0,
            present_ms: 4.0,
            render_drops: 40.0,
            decode_errors: 12.0,
            waiting_keyframe: true,
        };
        assert_eq!(diagnose(&diag, 7.5, 30), Bottleneck::Link);
    }

    /// A viewer waiting for an intra frame has a frame rate indistinguishable
    /// from a viewer whose decoder is too slow, and the fix is a request the
    /// viewer can already make. Checked before the decode time because a decoder
    /// sitting idle waiting for data reports a beautifully fast decode.
    #[test]
    fn waiting_for_a_keyframe_is_not_a_slow_decoder() {
        let mut diag = healthy();
        diag.waiting_keyframe = true;
        assert_eq!(diagnose(&diag, 0.0, 60), Bottleneck::Keyframe);
        // And it still loses to real loss, which is a bigger problem to have.
        assert_eq!(diagnose(&diag, 5.0, 60), Bottleneck::Link);
    }

    /// Frames that decode and are then thrown away are a display problem, not a
    /// network one, and no amount of keyframe recovery touches it.
    #[test]
    fn decoded_frames_the_ui_never_shows_are_a_render_problem() {
        let mut diag = healthy();
        diag.render_drops = 33.0;
        assert_eq!(diagnose(&diag, 0.0, 60), Bottleneck::Render);
    }

    /// A couple of replaced frames is what the single-slot display does on any
    /// machine, and calling it a fault would mean the verdict fires constantly on
    /// healthy viewers.
    #[test]
    fn the_occasional_replaced_frame_is_not_a_fault() {
        let mut diag = healthy();
        diag.render_drops = 1.5;
        assert_eq!(diagnose(&diag, 0.0, 60), Bottleneck::Ok);
    }

    /// Nothing wrong: the answer a healthy viewer must be able to get, or the
    /// verdict is worthless as evidence.
    #[test]
    fn a_healthy_viewer_reads_as_ok() {
        assert_eq!(diagnose(&healthy(), 0.0, 60), Bottleneck::Ok);
        assert_eq!(diagnose(&healthy(), 0.4, 30), Bottleneck::Ok);
    }

    /// A peer that sends nothing but link numbers is not a broken viewer, and
    /// must not be reported as one. Every diagnostic at its default is what a
    /// datagram from an older build parses to.
    #[test]
    fn a_peer_that_measured_nothing_is_not_in_trouble() {
        assert_eq!(diagnose(&silent(), 0.0, 60), Bottleneck::Ok);
    }

    /// Decode errors with an otherwise clean link still point at the link: a
    /// damaged frame in flight is the only thing that produces them without
    /// enough loss to show up as a burst.
    #[test]
    fn decode_errors_without_loss_still_name_the_link() {
        let mut diag = healthy();
        diag.decode_errors = 0.5;
        assert_eq!(diagnose(&diag, 0.1, 60), Bottleneck::Link);
    }

    /// A loss figure that is not a number cannot be compared with the threshold,
    /// and treating it as zero would claim a healthy link that was never
    /// measured.
    #[test]
    fn an_impossible_loss_reads_as_no_signal() {
        assert_eq!(diagnose(&healthy(), f32::NAN, 60), Bottleneck::NoSignal);
        assert_eq!(
            diagnose(&healthy(), f32::INFINITY, 60),
            Bottleneck::NoSignal
        );
    }

    /// Every verdict has to read as a sentence, because that is what goes in a
    /// cell next to a number. The one that quotes a budget is the only one that
    /// depends on the frame rate.
    #[test]
    fn every_verdict_says_something_a_person_can_read() {
        for (bottleneck, fps) in [
            (Bottleneck::Ok, 30),
            (Bottleneck::Link, 30),
            (Bottleneck::Decode, 30),
            (Bottleneck::Decode, 0),
            (Bottleneck::Render, 30),
            (Bottleneck::Keyframe, 30),
            (Bottleneck::NoSignal, 30),
        ] {
            let text = bottleneck.describe(fps);
            assert!(!text.is_empty(), "{bottleneck:?} had no description");
            assert!(
                text.chars().all(|c| !c.is_control()),
                "{bottleneck:?} described with control characters"
            );
        }
    }
}
