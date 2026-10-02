//! Pure, GPU-free playback transport state machine (PREV-02).
//!
//! # Why this module exists
//!
//! Phase 1's `WorkerCommand::Preview` ran a one-shot decode→render→present job
//! to completion, so a play/pause/seek issued during a preview sat in the
//! command channel until the job ended (RESEARCH Pitfall 2). Phase 2 turns the
//! preview into a paced **session** whose per-tick state is owned here.
//!
//! This is deliberately a plain state machine with **no** engine types, **no**
//! I/O, and **no** GPU: the transport's authority (is it playing? at which
//! frame? is a seek pending? does it wrap at the end?) is a pure function of
//! the commands it received, and is therefore unit-testable without a device.
//! The worker's GPU-touching `tick_session` drives this machine and performs the
//! decode/render/present work; the machine never touches a decoder.
//!
//! # The coalesced-seek contract
//!
//! `FfmpegFileSource::seek` documents that callers handling rapid input (scrub
//! bars, key repeat) must **coalesce seek requests on their side** and call it
//! once with the final target (`crates/reco-io/src/adapters.rs:453-457`). A
//! per-pointer-move seek is forbidden. This machine implements that contract:
//! [`Transport::request_seek`] only stores a pending target, and
//! [`Transport::take_pending_seek`] returns it exactly once so the session tick
//! performs at most one `source.seek` per tick no matter how many scrub events
//! arrived.
//!
//! # Units and arithmetic
//!
//! Frame-step is exactly ±1 frame. The frame duration is derived from
//! [`SourceInfo::fps_rational`](reco_core::source::SourceInfo) when present
//! (exact, e.g. 30000/1001 for 29.97 fps), else from the approximate
//! `SourceInfo.fps`. This mirrors `crates/reco-cli/src/preview.rs:128`.

use std::time::Duration;

/// Where playback is, as the worker's authoritative state (PREV-02).
///
/// The UI mirrors this; it never owns it (UI-SPEC Interaction rule 1: the
/// worker's value wins on every `Transport` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransportState {
    /// Frames are advancing on the session tick.
    Playing,
    /// Frames are held at the current position.
    Paused,
    /// The source is exhausted and playback is not looping.
    Ended,
}

/// The pure playback transport state machine.
///
/// Construct once per preview session from the loaded source's
/// [`SourceInfo`](reco_core::source::SourceInfo); drive it with
/// [`Self::play`] / [`Self::pause`] / [`Self::request_seek`] /
/// [`Self::step`] / [`Self::set_loop`] and advance it with
/// [`Self::on_frame_advanced`]. Every transition is pure.
#[derive(Debug, Clone, PartialEq)]
pub struct Transport {
    state: TransportState,
    loop_enabled: bool,
    frame: u64,
    total_frames: Option<u64>,
    fps_rational: Option<(i32, i32)>,
    fps: f64,
    pending_seek: Option<u64>,
}

impl Transport {
    /// Build a transport for a source with the given timing metadata.
    ///
    /// The default state is [`TransportState::Paused`], loop is off, and no
    /// seek is pending (the session starts still, at frame 0).
    pub fn new(fps: f64, fps_rational: Option<(i32, i32)>, total_frames: Option<u64>) -> Self {
        Self {
            state: TransportState::Paused,
            loop_enabled: false,
            frame: 0,
            total_frames,
            fps_rational,
            fps,
            pending_seek: None,
        }
    }

    /// The current transport state.
    pub fn state(&self) -> TransportState {
        self.state
    }

    /// Whether full-clip looping is enabled (default `false`).
    pub fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    /// The current playback position in frames.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Total frames in the source, if known.
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// The exact frame-rate rational, if the source reported one.
    pub fn fps_rational(&self) -> Option<(i32, i32)> {
        self.fps_rational
    }

    /// Begin playing. From [`TransportState::Ended`], restart at frame 0 so a
    /// play-after-end replays the clip rather than hanging at the last frame.
    pub fn play(&mut self) {
        if self.state == TransportState::Ended {
            self.frame = 0;
            self.pending_seek = Some(0);
        }
        self.state = TransportState::Playing;
    }

    /// Pause playback at the current position.
    ///
    /// A no-op from [`TransportState::Ended`] (there is nothing to pause).
    pub fn pause(&mut self) {
        if self.state != TransportState::Ended {
            self.state = TransportState::Paused;
        }
    }

    /// Toggle full-clip looping.
    pub fn set_loop(&mut self, enabled: bool) {
        self.loop_enabled = enabled;
    }

    /// Record a coalesced seek target.
    ///
    /// Rapid scrub input accumulates here; the session tick calls
    /// [`Self::take_pending_seek`] once per tick and so issues at most one
    /// `source.seek` regardless of how many requests arrived (the
    /// `FfmpegFileSource::seek` caller-coalescing contract). The target is
    /// clamped to `total_frames` when the total is known (T-02-04: an
    /// unbounded seek must not reach the decoder).
    pub fn request_seek(&mut self, frame: u64) {
        let clamped = match self.total_frames {
            Some(total) => frame.min(total),
            None => frame,
        };
        self.pending_seek = Some(clamped);
    }

    /// Take the pending seek target, clearing it.
    ///
    /// Returns `Some(frame)` exactly once per coalesced request burst, then
    /// `None` until [`Self::request_seek`] is called again.
    pub fn take_pending_seek(&mut self) -> Option<u64> {
        self.pending_seek.take()
    }

    /// Record that a seek to `frame` completed: set the current position, clear
    /// the seek-active flag, and re-arm playback if looping was requested while
    /// at the end.
    pub fn mark_seeking_done(&mut self, frame: u64) {
        self.frame = frame;
        if self.state == TransportState::Ended {
            self.state = TransportState::Paused;
        }
    }

    /// Step `direction` frames (expected exactly `-1` or `+1`), saturating.
    ///
    /// The command boundary rejects any other value (T-02-04), so this method
    /// treats `direction` as an i64 delta and saturates at 0 and `total_frames`.
    pub fn step(&mut self, direction: i32) {
        let next = self.frame as i64 + direction as i64;
        let clamped = next.clamp(0, self.total_frames.unwrap_or(u64::MAX) as i64);
        self.frame = clamped as u64;
        // Stepping implies a display update at the new position; pause first so
        // the session does not keep advancing past the stepped frame.
        if self.state == TransportState::Playing {
            self.state = TransportState::Paused;
        }
    }

    /// Advance the frame counter one step, honoring loop/end semantics.
    ///
    /// Call once per frame the session actually presents. On reaching a known
    /// `total_frames`:
    /// * with loop enabled, wrap to frame 0 and stay [`TransportState::Playing`];
    /// * with loop disabled, transition to [`TransportState::Ended`].
    ///
    /// The session marks the source's end via [`Self::mark_ended`] when the
    /// decoder (not the frame count) reports exhaustion.
    pub fn on_frame_advanced(&mut self) {
        self.frame = self.frame.saturating_add(1);
        if let Some(total) = self.total_frames
            && self.frame >= total
        {
            if self.loop_enabled {
                self.frame = 0;
            } else {
                self.frame = total.saturating_sub(1);
                self.state = TransportState::Ended;
            }
        }
    }

    /// Mark playback as ended (the decoder reported end-of-stream).
    ///
    /// With loop enabled the session should instead wrap via
    /// [`Self::request_seek`]; the caller decides. Here we only record reality.
    pub fn mark_ended(&mut self) {
        self.state = TransportState::Ended;
    }

    /// The duration of one frame at the source's frame rate.
    ///
    /// Uses `fps_rational` when present (exact), else `fps`. A non-positive or
    /// non-finite rate yields a zero duration rather than a panic — the session
    /// then advances as fast as the decoder allows.
    pub fn frame_duration(&self) -> Duration {
        let fps = match self.fps_rational {
            Some((num, den)) if num > 0 && den > 0 => num as f64 / den as f64,
            _ => self.fps,
        };
        if fps.is_finite() && fps > 0.0 {
            Duration::from_secs_f64(1.0 / fps)
        } else {
            Duration::ZERO
        }
    }
}

/// Copy the state a USER set from `previous` into a freshly built `next`.
///
/// This is the single named seam for state that must **survive a session
/// boundary**. It exists because `Transport::new` is a constructor, not an
/// inheriting one: it hard-codes `loop_enabled: false`, so a session that was
/// started after the user ticked Loop silently discarded the setting — the flag
/// was stored correctly (on the import-built transport) and then thrown away at
/// `begin_preview`, with no error anywhere (the UAT gap where the clip stopped
/// at its end instead of wrapping).
///
/// Today this carries exactly `loop_enabled`, which is the only field a user
/// sets directly. It deliberately does **not** copy `state`, `frame` or
/// `pending_seek`: a new session starts paused at frame 0, which is the
/// existing documented contract, and carrying the playhead would silently
/// rewind or jump the user.
///
/// **If you add a new user-settable field to `Transport`, add it here.** That
/// sentence is the actual fix for the class of bug: a field set in one place
/// and defaulted in the constructor is invisible until someone is looking for it.
pub fn carry_user_state(previous: Option<&Transport>, next: &mut Transport) {
    let Some(previous) = previous else {
        return;
    };
    next.loop_enabled = previous.loop_enabled;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> Transport {
        // 30 fps exact rational, 300 frames.
        Transport::new(30.0, Some((30, 1)), Some(300))
    }

    #[test]
    fn default_is_paused_no_loop_no_pending_seek() {
        let mut t = transport();
        assert_eq!(t.state(), TransportState::Paused);
        assert!(!t.loop_enabled());
        assert_eq!(t.frame(), 0);
        assert_eq!(t.take_pending_seek(), None);
    }

    #[test]
    fn play_pause_and_end_transitions() {
        let mut t = transport();
        t.play();
        assert_eq!(t.state(), TransportState::Playing);
        t.pause();
        assert_eq!(t.state(), TransportState::Paused);
        t.mark_ended();
        assert_eq!(t.state(), TransportState::Ended);
    }

    #[test]
    fn play_from_ended_restarts_at_frame_zero() {
        let mut t = transport();
        t.mark_ended();
        t.request_seek(250);
        assert_eq!(t.take_pending_seek(), Some(250));
        // Simulate having advanced near the end.
        t.play();
        assert_eq!(t.state(), TransportState::Playing);
        assert_eq!(t.frame(), 0);
        assert_eq!(t.take_pending_seek(), Some(0));
    }

    #[test]
    fn loop_on_wraps_at_total_and_stays_playing() {
        let mut t = transport();
        t.set_loop(true);
        t.play();
        t.frame = 299;
        t.on_frame_advanced();
        // Reached total (300) with loop on: wrap to 0, still playing.
        assert_eq!(t.frame(), 0);
        assert_eq!(t.state(), TransportState::Playing);
    }

    #[test]
    fn loop_off_ends_at_total() {
        let mut t = transport();
        t.set_loop(false);
        t.play();
        t.frame = 299;
        t.on_frame_advanced();
        assert_eq!(t.state(), TransportState::Ended);
        // Held at the last valid frame, not past the end.
        assert_eq!(t.frame(), 299);
    }

    #[test]
    fn rapid_seeks_coalesce_to_the_last_target_then_clear() {
        let mut t = transport();
        t.request_seek(10);
        t.request_seek(20);
        t.request_seek(999);
        // 999 > total (300): clamped to 300 (T-02-04).
        assert_eq!(t.take_pending_seek(), Some(300));
        assert_eq!(t.take_pending_seek(), None);
    }

    #[test]
    fn request_seek_clamps_to_total_when_known() {
        let mut t = transport();
        t.request_seek(100_000);
        assert_eq!(t.take_pending_seek(), Some(300));
    }

    #[test]
    fn request_seek_unclamped_when_total_unknown() {
        let mut t = Transport::new(30.0, None, None);
        t.request_seek(100_000);
        assert_eq!(t.take_pending_seek(), Some(100_000));
    }

    #[test]
    fn step_is_plus_minus_one_and_saturates_at_zero() {
        let mut t = transport();
        t.frame = 5;
        t.step(1);
        assert_eq!(t.frame(), 6);
        t.step(-1);
        assert_eq!(t.frame(), 5);
        // No-op at frame 0 for direction -1.
        t.frame = 0;
        t.step(-1);
        assert_eq!(t.frame(), 0);
    }

    #[test]
    fn step_pauses_playing_transport() {
        let mut t = transport();
        t.play();
        t.step(1);
        assert_eq!(t.state(), TransportState::Paused);
    }

    #[test]
    fn step_saturates_at_total() {
        let mut t = transport();
        t.frame = 299;
        t.step(1);
        assert_eq!(t.frame(), 300);
    }

    #[test]
    fn frame_duration_prefers_rational() {
        // 30000/1001 = 29.97 fps -> ~33.37 ms, not 1/29.97 rounded differently.
        let t = Transport::new(29.97, Some((30000, 1001)), None);
        let expected = Duration::from_secs_f64(1001.0 / 30000.0);
        assert_eq!(t.frame_duration(), expected);
    }

    #[test]
    fn frame_duration_falls_back_to_fps() {
        let t = Transport::new(25.0, None, None);
        assert_eq!(t.frame_duration(), Duration::from_secs_f64(1.0 / 25.0));
    }

    #[test]
    fn frame_duration_is_zero_for_degenerate_rate() {
        let t = Transport::new(0.0, None, None);
        assert_eq!(t.frame_duration(), Duration::ZERO);
    }

    #[test]
    fn carry_user_state_moves_loop_onto_a_fresh_transport() {
        // The UAT gap: `Transport::new` hard-codes `loop_enabled: false`, so a
        // user who ticked Loop before pressing Play had the flag stored
        // correctly and then silently discarded when the session was built.
        let mut previous = transport();
        previous.set_loop(true);
        previous.play();
        previous.step(1);

        let mut next = Transport::new(30.0, Some((30, 1)), Some(300));
        assert!(!next.loop_enabled());

        carry_user_state(Some(&previous), &mut next);
        assert!(next.loop_enabled());
        // Everything else is a NEW session's business: it starts paused at
        // frame 0 with no pending seek. That is the documented contract, and
        // copying position across would silently rewind the user's playhead.
        assert_eq!(next.state(), TransportState::Paused);
        assert_eq!(next.frame(), 0);
        assert_eq!(next.take_pending_seek(), None);
    }

    #[test]
    fn carry_user_state_without_a_previous_transport_keeps_the_new_defaults() {
        // First ever session (no import yet, or the mock's idle placeholder):
        // nothing to carry, so the fresh defaults must survive untouched.
        let mut next = Transport::new(25.0, Some((25, 1)), Some(100));
        carry_user_state(None, &mut next);
        assert!(!next.loop_enabled());
        assert_eq!(next.state(), TransportState::Paused);
        assert_eq!(next.frame(), 0);
        assert_eq!(next.total_frames(), Some(100));
    }
}
