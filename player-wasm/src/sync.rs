/// A/V synchronization using audio clock as master.
///
/// Strategy:
/// - Audio clock (AudioContext.currentTime) is the master reference.
/// - For each video frame, compare its PTS with the audio clock.
/// - If the frame is too early (>40ms ahead), hold it for the next rAF.
/// - If the frame is too late (>40ms behind), drop it.
/// - Otherwise, render it.

/// Synchronization threshold in milliseconds.
const SYNC_THRESHOLD_MS: f64 = 40.0;

/// Action to take for a video frame based on A/V sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncAction {
    /// Frame is within tolerance — render it.
    Render,
    /// Frame is too early — hold for next rAF.
    Hold,
    /// Frame is too late — drop it.
    Drop,
}

/// A/V synchronization engine.
pub struct AVSync {
    /// Start time offset (ms) — set when playback begins.
    start_offset_ms: f64,
    /// Whether we have an audio clock available.
    has_audio: bool,
    /// Stats
    frames_rendered: u64,
    frames_dropped: u64,
    frames_held: u64,
}

impl AVSync {
    pub fn new() -> Self {
        Self {
            start_offset_ms: 0.0,
            has_audio: false,
            frames_rendered: 0,
            frames_dropped: 0,
            frames_held: 0,
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

    /// Determine what to do with a video frame given the current clock.
    ///
    /// - `frame_pts_ms`: presentation timestamp of the frame in ms
    /// - `clock_ms`: current master clock time in ms (audio or performance)
    pub fn should_render_frame(&mut self, frame_pts_ms: f64, clock_ms: f64) -> SyncAction {
        let adjusted_clock = clock_ms - self.start_offset_ms;
        let diff = frame_pts_ms - adjusted_clock;

        if diff > SYNC_THRESHOLD_MS {
            // Frame is too early — hold
            self.frames_held += 1;
            SyncAction::Hold
        } else if diff < -SYNC_THRESHOLD_MS {
            // Frame is too late — drop
            self.frames_dropped += 1;
            SyncAction::Drop
        } else {
            // Within tolerance — render
            self.frames_rendered += 1;
            SyncAction::Render
        }
    }

    /// Get sync statistics.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.frames_rendered, self.frames_dropped, self.frames_held)
    }

    /// Reset sync state (e.g. after seek).
    pub fn reset(&mut self) {
        self.start_offset_ms = 0.0;
        self.frames_rendered = 0;
        self.frames_dropped = 0;
        self.frames_held = 0;
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
        assert_eq!(sync.should_render_frame(100.0, 150.0), SyncAction::Drop); // 50ms late
    }

    #[test]
    fn test_stats() {
        let mut sync = AVSync::new();
        sync.should_render_frame(100.0, 100.0); // render
        sync.should_render_frame(200.0, 100.0); // hold
        sync.should_render_frame(100.0, 200.0); // drop

        let (rendered, dropped, held) = sync.stats();
        assert_eq!(rendered, 1);
        assert_eq!(dropped, 1);
        assert_eq!(held, 1);
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
}
