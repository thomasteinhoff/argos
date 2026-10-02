//! Adaptive resolution control.
//!
//! The sharer cannot see its own link. It knows what it encoded; only the
//! receiver knows how much of that survived the wire and how often a frame
//! actually arrived. So the receiver measures and reports, and the sharer
//! decides — over the app's existing LAN channel, since the rtc transport
//! exposes no RTCP feedback path at all.
//!
//! The controller is deliberately a pure state machine with no I/O, so the
//! behaviour that matters can be tested directly: a lossy link steps down
//! quickly and recovers slowly, and neither direction oscillates.

use std::time::Duration;

/// Heights the controller may select, best first.
///
/// Each step is roughly a halving of pixel count below 720p, which is what
/// makes a step actually relieve the link rather than nibble at it.
pub const LADDER: [Option<u32>; 4] = [Some(720), Some(540), Some(360), Some(240)];

/// Loss above this, sustained, justifies stepping down.
const DOWN_LOSS: f32 = 3.0;
/// Loss below this, sustained, justifies stepping back up.
const UP_LOSS: f32 = 0.5;

/// Encoder queue drops above this, sustained, justify stepping down: the
/// encoder is producing frames slower than it is being fed, so the overload is
/// the machine rather than the link. Higher than [`DOWN_LOSS`] because a full
/// queue is a coarser signal — a single slow keyframe can overflow it — and the
/// remedy, a resolution drop, costs a forced intra frame.
const DOWN_DROPS: f32 = 10.0;
/// Encoder drops below this count as headroom.
const UP_DROPS: f32 = 1.0;

/// How long loss must stay bad before a step down. Short enough to react to a
/// genuinely bad link, long enough that one burst is not treated as a trend.
const DOWN_HOLD: Duration = Duration::from_millis(1500);
/// Encoder saturation must persist through warm-up and the first keyframe
/// before it is believed, so it waits longer than a loss burst.
const LOAD_HOLD: Duration = Duration::from_secs(3);
/// Recovery takes much longer than the way down. Stepping up into a link that
/// is still congested reintroduces the loss that caused the step down, and
/// the oscillation is more visible than either end state.
const UP_HOLD: Duration = Duration::from_secs(8);
/// Floor between any two changes.
const MIN_HOLD: Duration = Duration::from_secs(5);

/// A measurement from the receiver, plus what the sender's own pipeline knows.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    /// Fraction of expected packets that never arrived, 0..100.
    pub loss: f32,
    /// Frames actually decoded per second.
    pub fps: f32,
    /// Frames the sender's encoder discarded because its input queue was full,
    /// as a percentage of those offered, 0..100.
    ///
    /// A receiver cannot measure this and sends zero; the sharer fills it in
    /// from its own queue counters. It is the one signal that separates "the
    /// link is dropping packets" from "the machine cannot encode fast enough" —
    /// on a lossless LAN, the difference between a network fault and a CPU one.
    pub drops: f32,
}

/// What the controller decided, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Stay where you are.
    Hold,
    /// Move to this rung of the ladder.
    Step(usize),
}

pub struct Controller {
    /// Current position in [`LADDER`].
    rung: usize,
    /// Smoothed loss. The raw measurement is spiky enough that thresholding it
    /// directly would make the controller jump on single bad windows.
    loss: f32,
    samples: u32,
    /// Smoothed encoder drop percentage. Tracked separately from `loss`: the
    /// two have different causes and, in the reason the UI shows, different
    /// remedies.
    drops: f32,
    drop_samples: u32,
    /// When the current bad-or-good streak began.
    since_bad: Option<std::time::Instant>,
    since_good: Option<std::time::Instant>,
    /// When the current streak of encoder saturation began.
    since_loaded: Option<std::time::Instant>,
    /// When the last change was made, for [`MIN_HOLD`].
    last_change: Option<std::time::Instant>,
    /// Why the last step happened, so the UI can say so.
    last_reason: Option<&'static str>,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    pub fn new() -> Self {
        let rung = LADDER
            .iter()
            .position(|height| *height == Some(720))
            .unwrap_or(0);
        Self {
            rung,
            loss: 0.0,
            samples: 0,
            drops: 0.0,
            drop_samples: 0,
            since_bad: None,
            since_good: None,
            since_loaded: None,
            last_change: None,
            last_reason: None,
        }
    }

    /// Starts at the user's chosen quality rather than assuming 720p.
    ///
    /// "Native" has no rung in the ladder. It is treated as rung 0, so a link
    /// that is fine leaves native alone and a link that is not gets the first
    /// real step rather than a no-op.
    pub fn at_height(height: Option<u32>) -> Self {
        let mut controller = Self::new();
        if let Some(position) = LADDER.iter().position(|candidate| *candidate == height) {
            controller.rung = position;
        } else {
            controller.rung = 0;
        }
        controller
    }

    pub fn height(&self) -> Option<u32> {
        LADDER[self.rung]
    }

    pub fn rung(&self) -> usize {
        self.rung
    }

    /// True when the current height was chosen by the controller, not the user.
    pub fn is_auto(&self) -> bool {
        self.last_reason.is_some()
    }

    pub fn last_reason(&self) -> Option<&'static str> {
        self.last_reason
    }

    pub fn smoothed_loss(&self) -> f32 {
        self.loss
    }

    /// Smoothed encoder queue-drop percentage, 0..100.
    pub fn smoothed_drops(&self) -> f32 {
        self.drops
    }

    /// Feeds one measurement in and reports whether to change height.
    ///
    /// `now` is passed rather than read so the timing behaviour is testable
    /// without sleeping.
    pub fn update(&mut self, report: Report, now: std::time::Instant) -> Decision {
        // Ignore reports from a link that is not actually streaming. A viewer
        // that has not connected yet reports zero loss and zero fps, and
        // treating that as a healthy link would let the ladder climb on
        // evidence that nothing was received.
        if report.fps <= 0.0 {
            return Decision::Hold;
        }

        self.loss = if self.samples < 8 {
            report.loss
        } else {
            self.loss * 0.7 + report.loss * 0.3
        };
        self.samples = self.samples.saturating_add(1);
        self.drops = if self.drop_samples < 8 {
            report.drops
        } else {
            self.drops * 0.7 + report.drops * 0.3
        };
        self.drop_samples = self.drop_samples.saturating_add(1);

        // The two mechanisms deliberately use different inputs. The smoothed
        // value decides whether the link *looks* bad, because a single 500 ms
        // loss estimate is quantised and far too noisy to threshold directly.
        // But a report that is itself healthy cancels the streak outright,
        // however bad the history. Without that, the EMA holds one catastrophic
        // report above the threshold for several seconds and a single lost
        // window becomes a resolution drop — which is the exact failure this
        // is meant to prevent.
        if self.loss > DOWN_LOSS && report.loss > DOWN_LOSS {
            self.since_bad.get_or_insert(now);
        } else {
            self.since_bad = None;
        }

        // Encoder saturation is a second, independent reason to step down. It
        // is not loss and it never shows up as loss: on a healthy LAN the link
        // delivers every packet, and the frames are discarded before they ever
        // reach it.
        if self.drops > DOWN_DROPS && report.drops > DOWN_DROPS {
            self.since_loaded.get_or_insert(now);
        } else {
            self.since_loaded = None;
        }

        // Stepping back up needs both axes clean. The link recovering is not
        // enough while the encoder still cannot keep up: the step would be
        // undone the moment motion returned, at the cost of a forced intra
        // frame in each direction. Arming the streak on the conjunction is what
        // stops that.
        if self.loss < UP_LOSS
            && report.loss < UP_LOSS
            && self.drops < UP_DROPS
            && report.drops < UP_DROPS
        {
            self.since_good.get_or_insert(now);
        } else {
            self.since_good = None;
        }

        if self
            .since_bad
            .is_some_and(|since| now.duration_since(since) >= DOWN_HOLD)
        {
            if let Some(decision) = self.step(
                now,
                true,
                "reduced automatically — the viewer is losing packets",
            ) {
                return decision;
            }
            return Decision::Hold;
        }

        if self
            .since_loaded
            .is_some_and(|since| now.duration_since(since) >= LOAD_HOLD)
        {
            if let Some(decision) = self.step(
                now,
                true,
                "reduced automatically — the encoder cannot keep up",
            ) {
                return decision;
            }
            return Decision::Hold;
        }

        if self
            .since_good
            .is_some_and(|since| now.duration_since(since) >= UP_HOLD)
        {
            if let Some(decision) = self.step(
                now,
                false,
                "raised automatically — the link and encoder both have headroom",
            ) {
                return decision;
            }
            return Decision::Hold;
        }

        Decision::Hold
    }

    /// Moves one rung in the given direction, or returns `None` when already at
    /// the end.
    ///
    /// A held-off or impossible move clears both streaks rather than leaving
    /// them armed. Otherwise a link pinned at the top or the bottom would keep
    /// a fully-satisfied streak on the books and move the instant [`MIN_HOLD`]
    /// expired, on evidence that had long since stopped being current.
    fn step(
        &mut self,
        now: std::time::Instant,
        down: bool,
        reason: &'static str,
    ) -> Option<Decision> {
        if self
            .last_change
            .is_some_and(|last| now.duration_since(last) < MIN_HOLD)
        {
            self.clear_streaks();
            return None;
        }
        let next = if down {
            self.rung + 1
        } else {
            match self.rung.checked_sub(1) {
                Some(rung) => rung,
                None => {
                    self.clear_streaks();
                    return None;
                }
            }
        };
        if next >= LADDER.len() {
            // Already at the floor. Clear the streak so the controller stops
            // re-testing an end state it cannot leave every half second.
            self.clear_streaks();
            return None;
        }
        self.rung = next;
        self.last_change = Some(now);
        // Restart the streak, and discard the smoothed history with it: the
        // evidence that justified this step is spent, and the pressure that
        // matters from here on is that of the new level, not the one just left
        // behind.
        self.clear_streaks();
        self.loss = 0.0;
        self.samples = 0;
        self.drops = 0.0;
        self.drop_samples = 0;
        self.last_reason = Some(reason);
        Some(Decision::Step(self.rung))
    }

    /// Clears every streak. A held-off or impossible move must not leave a
    /// fully-satisfied streak armed for the instant [`MIN_HOLD`] expires.
    fn clear_streaks(&mut self) {
        self.since_bad = None;
        self.since_good = None;
        self.since_loaded = None;
    }

    /// Clears the auto-adjust state, so a user-chosen height is not immediately
    /// undone by evidence gathered before the choice was made.
    pub fn reset(&mut self, height: Option<u32>) {
        *self = Self::at_height(height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn controller() -> Controller {
        Controller::at_height(Some(720))
    }

    fn busy(loss: f32) -> Report {
        Report {
            loss,
            fps: 30.0,
            drops: 0.0,
        }
    }

    /// A clean link whose *encoder* is the limit: no packets lost, but frames
    /// thrown away at the handoff because the worker was already busy.
    fn saturated(drops: f32) -> Report {
        Report {
            loss: 0.0,
            fps: 30.0,
            drops,
        }
    }

    /// Drives the controller for `seconds` of reports arriving every 500 ms,
    /// returning the rung of the first step taken.
    fn run(controller: &mut Controller, loss: f32, seconds: u32) -> Option<usize> {
        let start = Instant::now();
        let mut decision = None;
        for tick in 0..seconds * 2 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(rung) = controller.update(busy(loss), at) {
                decision.get_or_insert(rung);
            }
        }
        decision
    }

    #[test]
    fn a_clean_link_is_left_alone() {
        let mut controller = controller();
        assert_eq!(run(&mut controller, 0.0, 60), None);
        assert_eq!(controller.height(), Some(720));
        assert!(!controller.is_auto());
    }

    /// One rung down first. A link this bad keeps stepping down — the tests
    /// below bound how fast — but the first move must be 720p to 540p, not
    /// past it.
    #[test]
    fn sustained_loss_steps_down() {
        let mut controller = controller();
        assert_eq!(run(&mut controller, 8.0, 10), Some(1));
        assert!(controller.is_auto());
    }

    /// Once off the top rung, a bad link can keep descending — but no faster
    /// than one rung per [`MIN_HOLD`]. This is the descent that would otherwise
    /// collapse 720p to 240p within seconds of a flaky tunnel coming up.
    #[test]
    fn a_bad_link_descends_at_most_one_rung_per_five_seconds() {
        let mut controller = controller();
        let start = Instant::now();
        let mut ticks = Vec::new();
        for tick in 0..120 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(_) = controller.update(busy(9.0), at) {
                ticks.push(tick);
            }
        }
        assert!(ticks.len() >= 2, "expected repeated descent, got {ticks:?}");
        for (index, tick) in ticks.iter().enumerate() {
            if let Some(previous) = index.checked_sub(1).map(|i| ticks[i]) {
                assert!(
                    tick - previous >= 10,
                    "steps {previous} and {tick} are {} ticks apart, floor is 10",
                    tick - previous
                );
            }
        }
        assert_eq!(controller.height(), LADDER[ticks.len()]);
    }

    /// One burst is not a trend. Without the hold this would step down on a
    /// single 500 ms window, which is how a controller ends up oscillating.
    #[test]
    fn a_single_bad_window_does_not_step_down() {
        let mut controller = controller();
        let start = Instant::now();
        // Four clean reports, two bad, then clean again: under DOWN_HOLD.
        let losses = [0.0, 0.0, 9.0, 9.0, 0.0, 0.0];
        for (tick, loss) in losses.iter().enumerate() {
            let at = start + Duration::from_millis(tick as u64 * 500);
            assert_eq!(controller.update(busy(*loss), at), Decision::Hold);
        }
        assert_eq!(controller.height(), Some(720));
    }

    #[test]
    fn a_bad_window_after_the_hold_does_step_down() {
        let mut controller = controller();
        let start = Instant::now();
        // UP_LOSS to DOWN_HOLD is 4 s of bad reports at 500 ms intervals.
        let mut stepped = false;
        for tick in 0..12 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(_) = controller.update(busy(9.0), at) {
                stepped = true;
            }
        }
        assert!(stepped);
        assert_eq!(controller.height(), Some(540));
    }

    #[test]
    fn recovery_steps_back_up() {
        let mut controller = controller();
        run(&mut controller, 8.0, 10);
        let descended = controller.rung();
        assert!(descended > 0, "never stepped down to test recovery from");
        let before = controller.height();
        let stepped = run(&mut controller, 0.0, 40);
        assert_eq!(stepped, Some(descended - 1));
        assert!(controller.height().unwrap() > before.unwrap());
    }

    /// Climbing back one rung at a time is the point: a link that recovered
    /// fully should reach the top of the ladder again, just not quickly.
    #[test]
    fn a_fully_recovered_link_returns_to_the_top() {
        let mut controller = controller();
        run(&mut controller, 9.0, 60);
        assert!(controller.rung() > 0);
        run(&mut controller, 0.0, 300);
        assert_eq!(controller.height(), Some(720));
    }

    /// The asymmetry is the point: 20 s of loss to go down, then a much longer
    /// clean run to come back.
    #[test]
    fn stepping_up_is_slower_than_stepping_down() {
        let mut down = controller();
        let start = Instant::now();
        let mut down_at = None;
        for tick in 0..120 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(_) = down.update(busy(9.0), at) {
                down_at = Some(tick);
                break;
            }
        }
        let down_at = down_at.expect("never stepped down");

        let mut up = controller();
        run(&mut up, 9.0, 10);
        let mut up_at = None;
        for tick in 0..240 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(_) = up.update(busy(0.0), at) {
                up_at = Some(tick);
                break;
            }
        }
        let up_at = up_at.expect("never stepped up");
        assert!(
            up_at > down_at,
            "stepped up after {up_at} ticks but down after {down_at}"
        );
    }

    #[test]
    fn changes_are_floored_at_five_seconds() {
        let mut controller = controller();
        let start = Instant::now();
        let mut steps = Vec::new();
        for tick in 0..200 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            // Alternate the loss so both directions keep qualifying. Without
            // MIN_HOLD this ratchets down the whole ladder in under 20 s.
            let loss = if (tick / 8) % 2 == 0 { 9.0 } else { 0.0 };
            if let Decision::Step(rung) = controller.update(busy(loss), at) {
                steps.push((tick, rung));
            }
        }
        for (index, (tick, _)) in steps.iter().enumerate() {
            if let Some((previous, _)) = steps.get(index.wrapping_sub(1)) {
                assert!(
                    tick - previous >= 10,
                    "steps {previous} and {tick} are {} ticks apart, floor is 10",
                    tick - previous
                );
            }
        }
    }

    #[test]
    fn the_bottom_rung_holds() {
        let mut controller = Controller::at_height(Some(240));
        assert_eq!(run(&mut controller, 50.0, 60), None);
        assert_eq!(controller.height(), Some(240));
    }

    /// A viewer that has not connected yet reports zero fps. That is absence of
    /// evidence, not evidence of a healthy link, and must not climb the ladder.
    #[test]
    fn an_idle_viewer_does_not_climb_the_ladder() {
        let mut controller = Controller::at_height(Some(360));
        let idle = Report {
            loss: 0.0,
            fps: 0.0,
            drops: 0.0,
        };
        let start = Instant::now();
        for tick in 0..200 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            assert_eq!(controller.update(idle, at), Decision::Hold);
        }
        assert_eq!(controller.height(), Some(360));
    }

    /// Alternating bad and good, with a report in the dead band between them.
    /// Each bad streak is 3 ticks — 1 s, short of DOWN_HOLD — and each good
    /// streak is short of UP_HOLD. If the dead band did not cancel both streaks,
    /// one direction would eventually be satisfied by accumulating across
    /// cycles and the ladder would drift on no actual evidence.
    #[test]
    fn the_dead_band_cancels_both_streaks() {
        let mut controller = controller();
        let start = Instant::now();
        for tick in 0..120 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            let loss = match tick % 6 {
                0..=2 => 9.0,
                3 => 2.0,
                _ => 0.0,
            };
            if let Decision::Step(rung) = controller.update(busy(loss), at) {
                panic!("unexpected step to rung {rung} at tick {tick}");
            }
        }
        assert_eq!(controller.height(), Some(720));
    }

    /// Loss wobbling either side of the threshold must not move the ladder.
    /// The smoothed value settles at the mean, which is below the threshold, so
    /// the streak never even starts. Thresholding the raw report instead would
    /// step down on the high half of this cycle.
    #[test]
    fn loss_wobbling_around_the_threshold_does_not_move_it() {
        let mut controller = controller();
        let start = Instant::now();
        for tick in 0..400 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            let loss = 2.4 + (tick % 3) as f32 * 0.5;
            if let Decision::Step(rung) = controller.update(busy(loss), at) {
                panic!("unexpected step to rung {rung} at tick {tick}");
            }
        }
        assert_eq!(controller.height(), Some(720));
    }

    /// A single catastrophic window must not register. With DOWN_HOLD at 1.5 s
    /// and reports every 500 ms, one report starts a streak that the next
    /// healthy report cancels — the hold is what does that work, and this pins
    /// the boundary so shortening it later would be caught here.
    #[test]
    fn a_single_catastrophic_window_does_not_step_down() {
        let mut controller = controller();
        let start = Instant::now();
        for tick in 0..40 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            let loss = if tick == 20 { 100.0 } else { 0.0 };
            if let Decision::Step(rung) = controller.update(busy(loss), at) {
                panic!("unexpected step to rung {rung} at tick {tick}");
            }
        }
        assert_eq!(controller.height(), Some(720));
    }

    /// Smoothing is what keeps the smoothed loss readable rather than jumping to
    /// whatever the last window reported. The value itself is exposed in the
    /// Pipeline overlay for exactly this reason.
    #[test]
    fn the_smoothed_loss_lags_a_step_change_in_the_link() {
        let mut controller = controller();
        let start = Instant::now();
        for tick in 0..40 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            controller.update(busy(0.0), at);
        }
        assert!(controller.smoothed_loss() < 0.01);
        // One report at 30%: the smoothed figure moves, but nowhere near the
        // raw value, and nowhere near DOWN_LOSS.
        let at = start + Duration::from_millis(20_000);
        controller.update(busy(30.0), at);
        assert!(controller.smoothed_loss() > 1.0 && controller.smoothed_loss() < 10.0);
        assert_eq!(controller.height(), Some(720));
    }

    #[test]
    fn reset_returns_to_a_user_choice() {
        let mut controller = controller();
        run(&mut controller, 8.0, 10);
        assert!(controller.is_auto());
        controller.reset(Some(1080));
        assert_eq!(controller.height(), Some(720));
        assert!(!controller.is_auto());
    }

    /// The reason the encoder signal exists: a lossless LAN reports zero loss
    /// forever, so without a second input the ladder can never relieve a sender
    /// whose CPU, not its link, is the limit.
    #[test]
    fn sustained_encoder_drops_step_down() {
        let mut controller = controller();
        let start = Instant::now();
        let mut stepped = None;
        for tick in 0..20 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(rung) = controller.update(saturated(40.0), at) {
                stepped = Some(rung);
                break;
            }
        }
        assert_eq!(stepped, Some(1));
        assert!(controller.is_auto());
        let reason = controller.last_reason().unwrap_or_default();
        assert!(
            reason.contains("encoder"),
            "expected an encoder reason, got {reason:?}"
        );
    }

    /// Warm-up and the first keyframe both overflow the handoff queue. The
    /// longer [`LOAD_HOLD`] is what keeps that from being read as a persistent
    /// overload and dropping resolution before any video has been sent.
    #[test]
    fn a_brief_encoder_overflow_does_not_step_down() {
        let mut controller = controller();
        let start = Instant::now();
        for tick in 0..4 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            assert_eq!(controller.update(saturated(80.0), at), Decision::Hold);
        }
        for tick in 4..40 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            assert_eq!(controller.update(busy(0.0), at), Decision::Hold);
        }
        assert_eq!(controller.height(), Some(720));
    }

    /// Zero loss is not a reason to climb while the encoder is still discarding
    /// frames. Otherwise the recovery path would undo every encoder-driven step
    /// eight seconds later, forcing an intra frame in each direction.
    #[test]
    fn an_encoder_that_stays_saturated_never_climbs_back() {
        let mut controller = controller();
        let start = Instant::now();
        let mut deepest = 0usize;
        for tick in 0..140 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(rung) = controller.update(saturated(40.0), at) {
                assert!(
                    rung > deepest,
                    "climbed from {deepest} to {rung} while the encoder was saturated"
                );
                deepest = rung;
            }
        }
        assert!(deepest > 0, "the encoder never forced a step down");
    }

    /// And when the encoder does catch up, the ladder resumes climbing — the
    /// block on recovery is the drops, not the fact that a local signal once
    /// moved it.
    #[test]
    fn an_encoder_with_headroom_climbs_back() {
        let mut controller = controller();
        let start = Instant::now();
        let mut descended = None;
        for tick in 0..20 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(rung) = controller.update(saturated(40.0), at) {
                descended = Some(rung);
                break;
            }
        }
        let descended = descended.expect("the encoder never forced a step down");
        for tick in 20..120 {
            let at = start + Duration::from_millis(tick as u64 * 500);
            if let Decision::Step(rung) = controller.update(busy(0.0), at) {
                assert!(rung < descended);
                return;
            }
        }
        panic!("never climbed back once the encoder had headroom");
    }
}
