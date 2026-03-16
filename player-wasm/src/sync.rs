/// A/V synchronization based on FFmpeg/ffplay's algorithm.
///
/// Key concepts from ffplay adapted to requestAnimationFrame:
///
/// 1. **frame_timer**: wall-clock accumulator tracking when the current frame was
///    "logically" displayed. Advances by `delay` (not `now()`), so timing errors
///    don't accumulate across frames.
///
/// 2. **compute_target_delay**: takes the nominal frame duration (PTS difference)
///    and adjusts it based on video↔audio drift:
///    - Video behind audio → shorten delay (catch up)
///    - Video ahead of audio → lengthen delay (slow down)
///    - Within threshold → no correction
///
/// 3. **Dynamic sync threshold**: `clamp(frame_duration, 40ms, 100ms)` — adapts
///    to the actual frame rate, not a fixed constant.
///
/// 4. **Frame dropping**: handled by the caller's catch-up loop, not by decide().
///    In ffplay, drops happen when the queue has more frames AND the next is also due.
///    Our caller loop naturally does this: it keeps the LATEST renderable frame and
///    closes intermediates.

/// FFplay constants (in milliseconds)
const AV_SYNC_THRESHOLD_MIN_MS: f64 = 40.0;
const AV_SYNC_THRESHOLD_MAX_MS: f64 = 100.0;
const AV_SYNC_FRAMEDUP_THRESHOLD_MS: f64 = 100.0;
const AV_NOSYNC_THRESHOLD_MS: f64 = 10_000.0;

/// If decide() returns Render this many times in a row without a Hold in between,
/// the video is massively behind. Emit SkipToKeyframe to break the cascade.
/// This replaces ffplay's "framedrop" heuristic adapted for our caller loop.
const MAX_RENDERS_WITHOUT_HOLD: u32 = 12;

/// Action to take for a video frame based on A/V sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncAction {
    /// Frame is on time — render it now.
    Render,
    /// Frame is too early — hold for next rAF tick.
    Hold,
    /// Frame is too late — drop it (caller should immediately check next frame).
    Drop,
    /// Massive backlog detected — skip to next keyframe.
    SkipToKeyframe,
}

/// A/V synchronization engine modeled after ffplay's algorithm.
///
/// The caller drives a tight loop per rAF tick:
/// ```ignore
/// let mut best_frame = None;
/// loop {
///     let action = sync.decide(peek_pts, clock, now);
///     match action {
///         Render => { best_frame = Some(take_frame()); continue; }
///         Hold | SkipToKeyframe => { break; }
///         Drop => { unreachable in current impl }
///     }
/// }
/// if let Some(f) = best_frame { render(f); }
/// ```
pub struct AVSync {
    /// Wall-clock time (ms) when the last frame was logically displayed.
    frame_timer_ms: Option<f64>,
    /// PTS (ms) of the last displayed/processed frame.
    last_pts_ms: Option<f64>,
    /// Duration (ms) of the last frame.
    last_duration_ms: f64,
    /// Default frame duration when PTS diff is invalid. Set from FPS.
    default_duration_ms: f64,
    /// Maximum plausible frame duration (ms).
    max_frame_duration_ms: f64,

    pub has_audio: bool,
    fps: f64,

    /// Consecutive Render calls without a Hold — detects massive backlog.
    renders_without_hold: u32,

    // Stats
    frames_rendered: u64,
    frames_dropped: u64,
    frames_held: u64,
    frames_skipped: u64,
}

impl AVSync {
    pub fn new() -> Self {
        Self {
            frame_timer_ms: None,
            last_pts_ms: None,
            last_duration_ms: 40.0,
            default_duration_ms: 40.0,
            max_frame_duration_ms: 5_000.0,
            has_audio: false,
            fps: 0.0,
            renders_without_hold: 0,
            frames_rendered: 0,
            frames_dropped: 0,
            frames_held: 0,
            frames_skipped: 0,
        }
    }

    pub fn set_has_audio(&mut self, has_audio: bool) {
        self.has_audio = has_audio;
    }

    /// Kept for API compatibility (no-op in ffplay-style sync).
    pub fn set_start_offset(&mut self, _offset_ms: f64) {}

    /// Set detected FPS. Updates default frame duration.
    pub fn set_fps(&mut self, fps: f64) {
        self.fps = fps;
        if fps > 0.0 {
            self.default_duration_ms = 1000.0 / fps;
        } else {
            self.default_duration_ms = 40.0;
        }
    }

    /// Get current dynamic sync threshold for debugging.
    pub fn threshold_ms(&self) -> f64 {
        self.last_duration_ms
            .clamp(AV_SYNC_THRESHOLD_MIN_MS, AV_SYNC_THRESHOLD_MAX_MS)
    }

    /// Core sync decision — call for each frame in the decoded queue.
    ///
    /// - `frame_pts_ms`: PTS of the candidate frame (ms)
    /// - `clock_ms`: master playback clock (audio-driven, ms)
    /// - `now_ms`: wall-clock time (performance.now(), ms)
    ///
    /// Returns Render, Hold, or SkipToKeyframe (never Drop in current design).
    pub fn decide(&mut self, frame_pts_ms: f64, clock_ms: f64, now_ms: f64) -> SyncAction {
        // First frame: render immediately, initialize timer
        let frame_timer = match self.frame_timer_ms {
            Some(ft) => ft,
            None => {
                self.frame_timer_ms = Some(now_ms);
                self.last_pts_ms = Some(frame_pts_ms);
                self.last_duration_ms = self.default_duration_ms;
                self.frames_rendered += 1;
                self.renders_without_hold = 1;
                return SyncAction::Render;
            }
        };

        // Step 1: Frame duration from PTS difference
        let duration = self.vp_duration(frame_pts_ms);

        // Step 2: Adjust delay for A/V drift (ffplay's compute_target_delay)
        let delay = self.compute_target_delay(duration, clock_ms);

        // Step 3: Is it time?
        if now_ms < frame_timer + delay {
            self.frames_held += 1;
            self.renders_without_hold = 0;
            return SyncAction::Hold;
        }

        // Step 4: Frame is due. Advance frame_timer.
        let mut ft = frame_timer + delay;

        // Step 5: Snap frame_timer if too far behind wall clock.
        // This prevents a catch-up storm after stalls, seeks, or tab-away.
        // Unlike ffplay (delay > 0 guard), we ALWAYS snap — delay=0 during
        // catch-up is when snapping matters most.
        if (now_ms - ft) > AV_SYNC_THRESHOLD_MAX_MS {
            ft = now_ms;
        }
        self.frame_timer_ms = Some(ft);

        // Step 6: Update state
        self.last_duration_ms = duration;
        self.last_pts_ms = Some(frame_pts_ms);
        self.frames_rendered += 1;
        self.renders_without_hold += 1;

        // Step 7: Detect massive backlog.
        // If we've had many Renders without a single Hold, the video queue has
        // a huge backlog. Frame-by-frame catch-up via the caller's best_frame
        // pattern works but can burn through hundreds of frames. After N,
        // signal SkipToKeyframe to jump ahead efficiently.
        if self.renders_without_hold >= MAX_RENDERS_WITHOUT_HOLD {
            self.frames_skipped += 1;
            self.renders_without_hold = 0;
            return SyncAction::SkipToKeyframe;
        }

        SyncAction::Render
    }

    /// Frame duration from PTS difference (ffplay's vp_duration).
    fn vp_duration(&self, frame_pts_ms: f64) -> f64 {
        if let Some(last) = self.last_pts_ms {
            let dur = frame_pts_ms - last;
            if dur > 0.0 && dur <= self.max_frame_duration_ms {
                return dur;
            }
        }
        self.default_duration_ms
    }

    /// FFplay's compute_target_delay: adjust delay based on A/V drift.
    fn compute_target_delay(&self, delay: f64, clock_ms: f64) -> f64 {
        let video_clock = self.last_pts_ms.unwrap_or(0.0);
        let diff = video_clock - clock_ms;

        // Give up sync if drift > 10s
        if diff.is_nan() || diff.abs() >= AV_NOSYNC_THRESHOLD_MS {
            return delay;
        }

        let sync_threshold = delay.clamp(AV_SYNC_THRESHOLD_MIN_MS, AV_SYNC_THRESHOLD_MAX_MS);

        if diff <= -sync_threshold {
            // Video BEHIND audio: shorten delay
            (delay + diff).max(0.0)
        } else if diff >= sync_threshold && delay > AV_SYNC_FRAMEDUP_THRESHOLD_MS {
            // Video AHEAD + long frame: proportional slow-down
            delay + diff
        } else if diff >= sync_threshold {
            // Video AHEAD + short frame: double delay
            2.0 * delay
        } else {
            // Within threshold — no correction
            delay
        }
    }

    /// Get sync statistics: (rendered, dropped, held, skipped).
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (
            self.frames_rendered,
            self.frames_dropped,
            self.frames_held,
            self.frames_skipped,
        )
    }

    /// Reset all sync state (after seek).
    pub fn reset(&mut self) {
        self.frame_timer_ms = None;
        self.last_pts_ms = None;
        self.last_duration_ms = self.default_duration_ms;
        self.renders_without_hold = 0;
        self.frames_rendered = 0;
        self.frames_dropped = 0;
        self.frames_held = 0;
        self.frames_skipped = 0;
    }

    /// Resync frame_timer to wall clock (after seek, stall recovery, or skip).
    pub fn resync_timer(&mut self, now_ms: f64) {
        self.frame_timer_ms = Some(now_ms);
        self.last_pts_ms = None;
        self.last_duration_ms = self.default_duration_ms;
        self.renders_without_hold = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sync_24fps() -> AVSync {
        let mut sync = AVSync::new();
        sync.set_fps(24.0);
        sync
    }

    #[test]
    fn test_first_frame_renders_immediately() {
        let mut sync = make_sync_24fps();
        assert_eq!(sync.decide(0.0, 0.0, 0.0), SyncAction::Render);
        assert_eq!(sync.frames_rendered, 1);
    }

    #[test]
    fn test_hold_when_too_early() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // Frame 1 at PTS=41.67ms, but only 10ms of wall-clock passed.
        // delay ≈ 41.67ms (no correction: diff=0-10≈-10, within threshold 40ms)
        // frame_timer(0) + 41.67 = 41.67 > now(10) → Hold
        let action = sync.decide(41.67, 10.0, 10.0);
        assert_eq!(action, SyncAction::Hold);
    }

    #[test]
    fn test_render_on_time() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // Frame 1: PTS=41.67, clock=41.67, now=42
        // diff = 0 - 41.67 = -41.67, threshold=41.67
        // -41.67 <= -41.67 → delay = max(0, 41.67 - 41.67) = 0
        // frame_timer(0) + 0 = 0, now(42) >= 0 → due
        // snap: 42-0=42 < 100 → no snap, ft=0
        // → Render (frame_timer advances to 0, snaps on next)
        let action = sync.decide(41.67, 41.67, 42.0);
        assert_eq!(action, SyncAction::Render);
    }

    #[test]
    fn test_catch_up_after_stall() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // 500ms stall: now=500, clock=500, PTS=500
        // diff = 0-500=-500, delay=max(0, 41.67-500)=0
        // ft=0+0=0, snap: 500-0=500>100 → ft=500
        let action = sync.decide(500.0, 500.0, 500.0);
        assert_eq!(action, SyncAction::Render);

        // Next frame schedules normally from ft=500
        let action2 = sync.decide(541.67, 541.67, 542.0);
        assert_eq!(action2, SyncAction::Render);
    }

    #[test]
    fn test_catchup_shortens_delay() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // Video 60ms behind audio
        // diff = 0-60=-60, threshold=41.67, -60 <= -41.67 → delay=max(0, 41.67-60)=0
        // now(42) >= ft(0)+0 → Render
        let action = sync.decide(41.67, 60.0, 42.0);
        assert_eq!(action, SyncAction::Render);
    }

    #[test]
    fn test_video_ahead_slows_down() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // Video 80ms ahead: clock=-80
        // diff = 0-(-80) = 80, threshold=41.67
        // 80 >= 41.67 && delay(41.67) <= 100 → double: delay=83.33
        // ft(0)+83.33 = 83.33 > now(42) → Hold
        let action = sync.decide(41.67, -80.0, 42.0);
        assert_eq!(action, SyncAction::Hold);
    }

    #[test]
    fn test_skip_after_massive_backlog() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);

        // Rapidly feed frames — all "due" because frame_timer snaps
        let mut last_action = SyncAction::Render;
        for i in 1..=20 {
            let pts = i as f64 * 41.67;
            last_action = sync.decide(pts, 5000.0, 5000.0);
            if last_action == SyncAction::SkipToKeyframe {
                assert!(
                    i >= MAX_RENDERS_WITHOUT_HOLD as i32 - 1,
                    "skip at frame {} (expected around {})",
                    i,
                    MAX_RENDERS_WITHOUT_HOLD
                );
                return;
            }
        }
        panic!("Expected SkipToKeyframe, last action: {:?}", last_action);
    }

    #[test]
    fn test_renders_reset_after_hold() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);

        // Several renders (video behind audio → delay=0 → all due immediately)
        for i in 1..=5 {
            let pts = i as f64 * 41.67;
            sync.decide(pts, 5000.0, 5000.0);
        }
        assert!(sync.renders_without_hold > 0);

        // A Hold: video is AHEAD of audio → doubled delay → Hold
        // frame_timer ≈ 5000 after snap. delay doubled = 83.33ms.
        // ft(5000) + 83.33 = 5083.33 > now(5001) → Hold
        sync.decide(999999.0, -1000.0, 5001.0);
        assert_eq!(sync.renders_without_hold, 0);
    }

    #[test]
    fn test_stats() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0); // Render
        sync.decide(41.67, 5.0, 5.0); // Hold (too early)

        let (rendered, _dropped, held, _skipped) = sync.stats();
        assert_eq!(rendered, 1);
        assert_eq!(held, 1);
    }

    #[test]
    fn test_reset_clears_all() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        sync.decide(100.0, 100.0, 200.0);
        sync.reset();

        let (r, d, h, s) = sync.stats();
        assert_eq!((r, d, h, s), (0, 0, 0, 0));
        assert!(sync.frame_timer_ms.is_none());
    }

    #[test]
    fn test_resync_timer() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        sync.resync_timer(5000.0);

        // Next frame renders immediately (first after resync)
        let action = sync.decide(5000.0, 5000.0, 5000.0);
        assert_eq!(action, SyncAction::Render);
    }

    #[test]
    fn test_set_fps() {
        let mut sync = AVSync::new();
        sync.set_fps(30.0);
        assert!((sync.default_duration_ms - 33.33).abs() < 0.1);
        sync.set_fps(60.0);
        assert!((sync.default_duration_ms - 16.67).abs() < 0.1);
        sync.set_fps(0.0);
        assert_eq!(sync.default_duration_ms, 40.0);
    }

    #[test]
    fn test_vp_duration_uses_pts_diff() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        sync.decide(50.0, 50.0, 50.0);
        assert!((sync.last_duration_ms - 50.0).abs() < 0.1);
    }

    #[test]
    fn test_negative_pts_diff_uses_default() {
        let mut sync = make_sync_24fps();
        sync.decide(100.0, 100.0, 0.0);
        sync.decide(50.0, 100.0, 50.0);
        assert!((sync.last_duration_ms - 41.67).abs() < 0.1);
    }

    #[test]
    fn test_large_pts_values() {
        let mut sync = make_sync_24fps();
        let t = 7_200_000.0;
        sync.decide(t, t, t);
        let action = sync.decide(t + 41.67, t + 41.67, t + 42.0);
        assert_eq!(action, SyncAction::Render);
    }

    #[test]
    fn test_smooth_24fps_playback() {
        let mut sync = make_sync_24fps();
        let frame_dur = 1000.0 / 24.0;
        let tick_interval = 1000.0 / 60.0; // 60fps display
        let mut now = 0.0;
        let mut next_pts = 0.0;
        let mut rendered = 0u32;
        let mut held = 0u32;
        let total_frames = 120;
        let mut frame_idx = 0;

        while frame_idx < total_frames && now < 6000.0 {
            let clock = now;
            let action = sync.decide(next_pts, clock, now);
            match action {
                SyncAction::Render => {
                    rendered += 1;
                    frame_idx += 1;
                    next_pts += frame_dur;
                }
                SyncAction::Hold => {
                    held += 1;
                }
                SyncAction::Drop => {
                    frame_idx += 1;
                    next_pts += frame_dur;
                }
                SyncAction::SkipToKeyframe => break,
            }
            now += tick_interval;
        }

        assert!(rendered >= 110, "expected ~120 renders, got {}", rendered);
        assert!(held > 0, "expected holds at 60fps display");
        let (_r, _d, _h, s) = sync.stats();
        assert_eq!(s, 0, "no skips in smooth playback");
    }

    #[test]
    fn test_decode_latency_burst() {
        let mut sync = make_sync_24fps();
        let fd = 1000.0 / 24.0;
        // Frame 0 at t=0
        assert_eq!(sync.decide(0.0, 0.0, 0.0), SyncAction::Render);

        // 80ms decode latency burst: frames 1,2 arrive at t=80
        let a1 = sync.decide(fd, 80.0, 80.0);
        assert_eq!(a1, SyncAction::Render);

        // Frame 2 at same wall time
        let a2 = sync.decide(2.0 * fd, 80.0, 80.0);
        // After snap, frame_timer ≈ 80, so next frame might need to wait
        assert!(a2 == SyncAction::Render || a2 == SyncAction::Hold);
    }

    #[test]
    fn test_consecutive_holds_no_skip() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // Video far AHEAD of audio: delay is doubled, frame_timer+delay > now.
        // Keep now small (< frame_timer + doubled_delay).
        for i in 1..=100 {
            // clock is negative → video is "ahead" → delay doubled
            // ft(0) + 83.33 = 83.33 > now (i as f64 * 0.1)
            let action = sync.decide(50000.0, -1000.0, i as f64 * 0.1);
            assert_eq!(action, SyncAction::Hold, "iteration {}", i);
        }
        assert_eq!(sync.frames_skipped, 0);
    }

    #[test]
    fn test_no_sync_correction_beyond_10s() {
        let mut sync = make_sync_24fps();
        sync.decide(0.0, 0.0, 0.0);
        // 15s drift → no correction, use default delay
        let action = sync.decide(41.67, 15_000.0, 42.0);
        assert_eq!(action, SyncAction::Render);
    }
}
