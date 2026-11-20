use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct FrameTimeline {
    started_frames: AtomicU64,
    fps: u32,
}

impl FrameTimeline {
    pub(super) fn new(fps: u32) -> Self {
        Self {
            started_frames: AtomicU64::new(0),
            fps,
        }
    }

    pub(super) fn frame_started(&self) {
        self.started_frames.fetch_add(1, Ordering::Relaxed);
    }

    // A click can first appear in the frame after the one already being sent.
    pub(super) fn next_sample_time(&self) -> f64 {
        self.started_frames.load(Ordering::Relaxed) as f64 / f64::from(self.fps)
    }
}

#[cfg(test)]
mod tests {
    use super::FrameTimeline;

    #[test]
    fn mouse_time_stays_on_the_next_video_frame_when_encoder_is_slow() {
        let timeline = FrameTimeline::new(60);
        assert_eq!(timeline.next_sample_time(), 0.0);
        timeline.frame_started();
        let blocked_frame = timeline.next_sample_time();
        assert_eq!(blocked_frame, 1.0 / 60.0);
        assert_eq!(timeline.next_sample_time(), blocked_frame);
        timeline.frame_started();
        assert_eq!(timeline.next_sample_time(), 2.0 / 60.0);
    }
}
