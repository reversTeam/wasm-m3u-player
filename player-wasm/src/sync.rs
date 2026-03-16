/// A/V synchronization using audio clock as master.
///
/// Strategy:
/// - Audio clock (AudioContext.currentTime) is the master reference.
/// - For each video frame, compare its PTS with the audio clock.
/// - If the frame is too early (> threshold ahead), hold it for the next rAF.
/// - If the frame is too late (> threshold behind), drop it.
/// - Otherwise, render it.
/// - Threshold adapts to the detected FPS (60% of one frame interval).
/// - After N consecutive drops, emit SkipToKeyframe to avoid endless frame-by-frame dropping.

/// Default sync threshold for unknown FPS.
const DEFAULT_SYNC_THRESHOLD_MS: f64 = 40.0;

/// Number of consecutive drops before triggering a keyframe skip.
const MAX_CONSECUTIVE_DROPS: u32 = 5;

/// Action to take for a video frame based on A/V sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncAction {
    /// Frame is within tolerance — render it.
    Render,
    /// Frame is too early — hold for next rAF.
    Hold,
    /// Frame is too late — drop it.
    Drop,
    /// Too many consecutive drops — skip to next keyframe instead of
    /// dropping frame-by-frame (avoids long catch-up sequences).
    SkipToKeyframe,
}

/// A/V synchronization engine.
pub struct AVSync {
    /// Start time offset (ms) — set when playback begins.
    start_offset_ms: f64,
    /// Whether we have an audio clock available.
    has_audio: bool,
    /// Adaptive sync threshold in ms (based on FPS).
    threshold_ms: f64,
    /// Detected FPS (0 = unknown).
    fps: f64,
    /// Consecutive drop count — reset on Render or Hold.
    consecutive_drops: u32,
    /// Stats
    frames_rendered: u64,
    frames_dropped: u64,
    frames_held: u64,
    frames_skipped: u64,
}

impl AVSync {
    pub fn new() -> Self {
        Self {
            start_offset_ms: 0.0,
            has_audio: false,
            threshold_ms: DEFAULT_SYNC_THRESHOLD_MS,
            fps: 0.0,
            consecutive_drops: 0,
            frames_rendered: 0,
            frames_dropped: 0,
            frames_held: 0,
            frames_skipped: 0,
        }
    }

    /// Set whether audio clock is available.
    pub fn set_has_audio(&mut self, has_audio: bool) {
        self.has_audio = has_audio;
    }

    /// Set the playback start offset.
    pub fn set_start_offset(&mut self, offset_ms: f64) {
        self.start_offset_ms = offset_ms;
    }

    /// Set the detected FPS and adapt the sync threshold.
    /// Threshold = 60% of one frame interval (e.g. 24fps → 25ms, 30fps → 20ms, 60fps → 10ms).
    pub fn set_fps(&mut self, fps: f64) {
        self.fps = fps;
        if fps > 0.0 {
            self.threshold_ms = (1000.0 / fps) * 0.6;
            // Clamp to reasonable range
            if self.threshold_ms < 8.0 {
                self.threshold_ms = 8.0;
            }
            if self.threshold_ms > 60.0 {
                self.threshold_ms = 60.0;
            }
        } else {
            self.threshold_ms = DEFAULT_SYNC_THRESHOLD_MS;
        }
    }

    /// Get the current threshold in ms (for debugging).
    pub fn threshold_ms(&self) -> f64 {
        self.threshold_ms
    }

    /// Determine what to do with a video frame given the current clock.
    ///
    /// - `frame_pts_ms`: presentation timestamp of the frame in ms
    /// - `clock_ms`: current master clock time in ms (audio or performance)
    pub fn should_render_frame(&mut self, frame_pts_ms: f64, clock_ms: f64) -> SyncAction {
        let adjusted_clock = clock_ms - self.start_offset_ms;
        let diff = frame_pts_ms - adjusted_clock;

        if diff > self.threshold_ms {
            // Frame is too early — hold
            self.frames_held += 1;
            self.consecutive_drops = 0;
            SyncAction::Hold
        } else if diff < -self.threshold_ms {
            // Frame is too late — drop (or skip if too many consecutive drops)
            self.consecutive_drops += 1;
            self.frames_dropped += 1;

            if self.consecutive_drops >= MAX_CONSECUTIVE_DROPS {
                self.frames_skipped += 1;
                self.consecutive_drops = 0;
                SyncAction::SkipToKeyframe
            } else {
                SyncAction::Drop
            }
        } else {
            // Within tolerance — render
            self.frames_rendered += 1;
            self.consecutive_drops = 0;
            SyncAction::Render
        }
    }

    /// Get sync statistics: (rendered, dropped, held, skipped).
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (self.frames_rendered, self.frames_dropped, self.frames_held, self.frames_skipped)
    }

    /// Reset sync state (e.g. after seek).
    pub fn reset(&mut self) {
        self.start_offset_ms = 0.0;
        self.frames_rendered = 0;
        self.frames_dropped = 0;
        self.frames_held = 0;
        self.frames_skipped = 0;
        self.consecutive_drops = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_within_threshold() {
        let mut sync = AVSync::new();
        assert_eq!(sync.should_render_frame(100.0, 100.0), SyncAction::Render);
        assert_eq!(sync.should_render_frame(100.0, 80.0), SyncAction::Render); // 20ms early
        assert_eq!(sync.should_render_frame(100.0, 120.0), SyncAction::Render); // 20ms late
    }

    #[test]
    fn test_hold_when_early() {
        let mut sync = AVSync::new();
        assert_eq!(sync.should_render_frame(200.0, 100.0), SyncAction::Hold); // 100ms early
        assert_eq!(sync.should_render_frame(150.0, 100.0), SyncAction::Hold); // 50ms early
    }

    #[test]
    fn test_drop_when_late() {
        let mut sync = AVSync::new();
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop); // 100ms late
    }

    #[test]
    fn test_skip_after_consecutive_drops() {
        let mut sync = AVSync::new();
        // 4 drops, then the 5th triggers SkipToKeyframe
        for _ in 0..4 {
            assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
        }
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::SkipToKeyframe);
        // After skip, counter resets — next late frame is a normal Drop
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
    }

    #[test]
    fn test_consecutive_drops_reset_on_render() {
        let mut sync = AVSync::new();
        // 3 drops
        for _ in 0..3 {
            sync.should_render_frame(100.0, 200.0);
        }
        // Then a render resets the counter
        assert_eq!(sync.should_render_frame(100.0, 100.0), SyncAction::Render);
        // Only 1 drop after reset — not enough for skip
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
    }

    #[test]
    fn test_stats() {
        let mut sync = AVSync::new();
        sync.should_render_frame(100.0, 100.0); // render
        sync.should_render_frame(200.0, 100.0); // hold
        sync.should_render_frame(100.0, 200.0); // drop

        let (rendered, dropped, held, skipped) = sync.stats();
        assert_eq!(rendered, 1);
        assert_eq!(dropped, 1);
        assert_eq!(held, 1);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_with_start_offset() {
        let mut sync = AVSync::new();
        sync.set_start_offset(50.0);

        // clock=100, offset=50 → adjusted=50, frame=100 → diff=50 → Hold
        assert_eq!(sync.should_render_frame(100.0, 100.0), SyncAction::Hold);

        // clock=150, offset=50 → adjusted=100, frame=100 → diff=0 → Render
        assert_eq!(sync.should_render_frame(100.0, 150.0), SyncAction::Render);
    }

    #[test]
    fn test_adaptive_threshold_24fps() {
        let mut sync = AVSync::new();
        sync.set_fps(24.0);
        // 1000/24 * 0.6 = 25ms threshold
        assert!((sync.threshold_ms() - 25.0).abs() < 0.1);
        // 30ms late with 25ms threshold → Drop
        assert_eq!(sync.should_render_frame(100.0, 130.0), SyncAction::Drop);
        // 20ms late with 25ms threshold → Render
        assert_eq!(sync.should_render_frame(100.0, 120.0), SyncAction::Render);
    }

    #[test]
    fn test_adaptive_threshold_60fps() {
        let mut sync = AVSync::new();
        sync.set_fps(60.0);
        // 1000/60 * 0.6 = 10ms, but clamped to min 8ms
        assert!((sync.threshold_ms() - 10.0).abs() < 0.1);
    }

    // =============================================
    // Additional boundary tests
    // =============================================

    #[test]
    fn test_reset_clears_all_state() {
        let mut sync = AVSync::new();
        sync.set_start_offset(100.0);
        // Generate some activity
        sync.should_render_frame(100.0, 100.0); // render
        sync.should_render_frame(100.0, 200.0); // drop
        sync.should_render_frame(200.0, 100.0); // hold

        sync.reset();

        let (r, d, h, s) = sync.stats();
        assert_eq!((r, d, h, s), (0, 0, 0, 0));
        // After reset, start_offset should be 0 → same PTS/clock → Render
        assert_eq!(sync.should_render_frame(100.0, 100.0), SyncAction::Render);
    }

    #[test]
    fn test_double_reset() {
        let mut sync = AVSync::new();
        sync.should_render_frame(100.0, 100.0);
        sync.reset();
        sync.reset(); // Double reset should be safe
        let (r, d, h, s) = sync.stats();
        assert_eq!((r, d, h, s), (0, 0, 0, 0));
    }

    #[test]
    fn test_set_fps_zero() {
        let mut sync = AVSync::new();
        sync.set_fps(0.0);
        assert_eq!(sync.threshold_ms(), DEFAULT_SYNC_THRESHOLD_MS);
    }

    #[test]
    fn test_set_fps_negative() {
        let mut sync = AVSync::new();
        sync.set_fps(-24.0);
        // Negative FPS → fps <= 0 → fallback to default
        assert_eq!(sync.threshold_ms(), DEFAULT_SYNC_THRESHOLD_MS);
    }

    #[test]
    fn test_threshold_clamp_min_8ms() {
        let mut sync = AVSync::new();
        // 240fps → 1000/240 * 0.6 = 2.5ms → should clamp to 8ms
        sync.set_fps(240.0);
        assert_eq!(sync.threshold_ms(), 8.0);
    }

    #[test]
    fn test_threshold_clamp_max_60ms() {
        let mut sync = AVSync::new();
        // 5fps → 1000/5 * 0.6 = 120ms → should clamp to 60ms
        sync.set_fps(5.0);
        assert_eq!(sync.threshold_ms(), 60.0);
    }

    #[test]
    fn test_threshold_30fps() {
        let mut sync = AVSync::new();
        sync.set_fps(30.0);
        // 1000/30 * 0.6 = 20ms
        assert!((sync.threshold_ms() - 20.0).abs() < 0.1);
    }

    #[test]
    fn test_threshold_120fps() {
        let mut sync = AVSync::new();
        sync.set_fps(120.0);
        // 1000/120 * 0.6 = 5ms → clamp to 8ms
        assert_eq!(sync.threshold_ms(), 8.0);
    }

    #[test]
    fn test_exact_threshold_boundary_render() {
        let mut sync = AVSync::new();
        // Default threshold = 40ms
        // diff = exactly 40ms → should Render (not Hold/Drop)
        assert_eq!(sync.should_render_frame(140.0, 100.0), SyncAction::Render);
        assert_eq!(sync.should_render_frame(60.0, 100.0), SyncAction::Render);
    }

    #[test]
    fn test_just_beyond_threshold_hold() {
        let mut sync = AVSync::new();
        // diff = 40.001 → just beyond threshold → Hold
        assert_eq!(
            sync.should_render_frame(140.001, 100.0),
            SyncAction::Hold
        );
    }

    #[test]
    fn test_just_beyond_threshold_drop() {
        let mut sync = AVSync::new();
        // diff = -40.001 → just beyond threshold → Drop
        assert_eq!(
            sync.should_render_frame(59.999, 100.0),
            SyncAction::Drop
        );
    }

    #[test]
    fn test_consecutive_holds_dont_trigger_skip() {
        let mut sync = AVSync::new();
        // 100 consecutive holds — should never trigger SkipToKeyframe
        for _ in 0..100 {
            assert_eq!(sync.should_render_frame(1000.0, 100.0), SyncAction::Hold);
        }
        let (_, _, _, skipped) = sync.stats();
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_stats_count_skip_once_per_cycle() {
        let mut sync = AVSync::new();
        // 5 drops → 1 skip, then 5 more drops → 1 skip
        for _ in 0..5 {
            sync.should_render_frame(100.0, 200.0);
        }
        for _ in 0..5 {
            sync.should_render_frame(100.0, 200.0);
        }
        let (_, dropped, _, skipped) = sync.stats();
        assert_eq!(skipped, 2);
        // 10 drops counted + 2 skipped events (skip also increments frames_dropped)
        assert_eq!(dropped, 10);
    }

    #[test]
    fn test_skip_resets_consecutive_drops() {
        let mut sync = AVSync::new();
        // After SkipToKeyframe, consecutive_drops resets
        // So the NEXT drop is just a normal Drop, not another SkipToKeyframe
        for _ in 0..4 {
            sync.should_render_frame(100.0, 200.0); // Drop
        }
        assert_eq!(
            sync.should_render_frame(100.0, 200.0),
            SyncAction::SkipToKeyframe
        );
        // Counter reset — next is a normal Drop
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
    }

    #[test]
    fn test_hold_resets_consecutive_drops() {
        let mut sync = AVSync::new();
        // 4 drops, then a hold, then more drops — should NOT skip at 5
        for _ in 0..4 {
            sync.should_render_frame(100.0, 200.0); // Drop
        }
        sync.should_render_frame(1000.0, 100.0); // Hold — resets counter
        // Now need 5 MORE drops for SkipToKeyframe
        for _ in 0..4 {
            assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
        }
        assert_eq!(
            sync.should_render_frame(100.0, 200.0),
            SyncAction::SkipToKeyframe
        );
    }

    #[test]
    fn test_negative_pts() {
        let mut sync = AVSync::new();
        // Negative PTS values (shouldn't happen but must not panic)
        let result = sync.should_render_frame(-100.0, 0.0);
        // diff = -100 - 0 = -100 → Drop
        assert_eq!(result, SyncAction::Drop);
    }

    #[test]
    fn test_large_pts_values() {
        let mut sync = AVSync::new();
        // Very large PTS (2+ hours = 7_200_000 ms)
        let result = sync.should_render_frame(7_200_000.0, 7_200_000.0);
        assert_eq!(result, SyncAction::Render);
    }

    #[test]
    fn test_set_has_audio_toggle() {
        let mut sync = AVSync::new();
        assert!(!sync.has_audio);
        sync.set_has_audio(true);
        assert!(sync.has_audio);
        sync.set_has_audio(false);
        assert!(!sync.has_audio);
    }

    #[test]
    fn test_start_offset_affects_sync() {
        let mut sync = AVSync::new();
        sync.set_start_offset(1000.0);
        // clock=1000, offset=1000 → adjusted=0, frame=0 → diff=0 → Render
        assert_eq!(sync.should_render_frame(0.0, 1000.0), SyncAction::Render);
        // clock=1000, offset=1000 → adjusted=0, frame=100 → diff=100 → Hold
        assert_eq!(sync.should_render_frame(100.0, 1000.0), SyncAction::Hold);
    }

    #[test]
    fn test_fps_change_mid_playback() {
        let mut sync = AVSync::new();
        sync.set_fps(24.0);
        assert!((sync.threshold_ms() - 25.0).abs() < 0.1);

        // Change FPS mid-playback (e.g., variable frame rate content)
        sync.set_fps(60.0);
        assert!((sync.threshold_ms() - 10.0).abs() < 0.1);

        // Reset to 0 → back to default
        sync.set_fps(0.0);
        assert_eq!(sync.threshold_ms(), DEFAULT_SYNC_THRESHOLD_MS);
    }

    #[test]
    fn test_many_renders_stats_accumulate() {
        let mut sync = AVSync::new();
        for i in 0..1000 {
            sync.should_render_frame(i as f64, i as f64); // Render each time
        }
        let (rendered, dropped, held, skipped) = sync.stats();
        assert_eq!(rendered, 1000);
        assert_eq!(dropped, 0);
        assert_eq!(held, 0);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_interleaved_actions() {
        let mut sync = AVSync::new();
        // Render, Hold, Drop, Render, Drop, Drop, Drop, Drop, SkipToKeyframe
        assert_eq!(sync.should_render_frame(100.0, 100.0), SyncAction::Render);
        assert_eq!(sync.should_render_frame(200.0, 100.0), SyncAction::Hold);
        assert_eq!(sync.should_render_frame(100.0, 200.0), SyncAction::Drop);
        assert_eq!(sync.should_render_frame(300.0, 300.0), SyncAction::Render);
        // Now 5 consecutive drops → skip
        for _ in 0..4 {
            sync.should_render_frame(100.0, 300.0);
        }
        assert_eq!(
            sync.should_render_frame(100.0, 300.0),
            SyncAction::SkipToKeyframe
        );

        let (r, d, h, s) = sync.stats();
        assert_eq!(r, 2);
        assert_eq!(d, 6); // 1 + 5
        assert_eq!(h, 1);
        assert_eq!(s, 1);
    }
}
