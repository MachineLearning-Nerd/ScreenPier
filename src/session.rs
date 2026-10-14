use crate::desktop::{InputPermit, Viewport};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub const MAX_MESSAGE: usize = 72 * 1024;
pub const DEADMAN: Duration = Duration::from_secs(3);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Hello {
        lease: String,
    },
    Heartbeat {},
    Presented {
        epoch: String,
        generation: u64,
        rtp: u32,
        viewport_seq: Option<u64>,
    },
    Viewport {
        revision: String,
        generation: u64,
        seq: u64,
        x: u32,
        y: u32,
    },
    Release {
        revision: String,
    },
    InputResetAck {
        revision: String,
    },
    Pointer {
        revision: String,
        generation: u64,
        rtp: Option<u32>,
        x: f64,
        y: f64,
    },
    Key {
        id: Option<u64>,
        revision: String,
        generation: u64,
        rtp: Option<u32>,
        code: u32,
        down: bool,
    },
    Button {
        id: Option<u64>,
        revision: String,
        generation: u64,
        rtp: Option<u32>,
        x: f64,
        y: f64,
        button: u8,
        down: bool,
    },
    Scroll {
        revision: String,
        generation: u64,
        rtp: Option<u32>,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    ClipboardRead {
        request: u32,
    },
    ClipboardWrite {
        request: u32,
        text: String,
    },
}

#[derive(Clone, Copy)]
pub struct FrameIdentity {
    pub timestamp: u32,
    pub generation: u64,
    pub content: u64,
    pub captured: Instant,
    pub viewport: Option<ViewportFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportFrame {
    pub seq: u64,
    pub viewport: Viewport,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct DisplayEvidence {
    pub generation_matches: bool,
    pub same_content: Option<bool>,
    pub capture_age_us: u64,
    pub presented_capture_age_us: Option<u64>,
    pub accepted_ack_age_us: Option<u64>,
    pub display_deadline_offset_us: Option<i64>,
}
pub(crate) fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}
pub(crate) fn deadline_offset(deadline: Instant, now: Instant) -> i64 {
    if deadline >= now {
        deadline
            .duration_since(now)
            .as_micros()
            .min(i64::MAX as u128) as i64
    } else {
        -(now
            .duration_since(deadline)
            .as_micros()
            .min(i64::MAX as u128) as i64)
    }
}

pub struct DisplayProgress {
    frames: VecDeque<FrameIdentity>,
    generation: u64,
    current_content: u64,
    capture_at: Instant,
    presented: Option<FrameIdentity>,
    presented_at: Option<Instant>,
}

impl DisplayProgress {
    pub fn new(generation: u64, now: Instant) -> Self {
        Self {
            frames: VecDeque::new(),
            generation,
            current_content: 0,
            capture_at: now,
            presented: None,
            presented_at: None,
        }
    }

    pub fn captured(&mut self, content: u64, now: Instant) {
        self.capture_at = now;
        if content != self.current_content {
            self.current_content = content;
        }
    }

    pub fn sent(&mut self, frame: FrameIdentity) {
        self.frames.push_back(frame);
        while self.frames.len() > 180 {
            self.frames.pop_front();
        }
    }

    pub fn acknowledge(&mut self, generation: u64, timestamp: u32, now: Instant) -> bool {
        self.acknowledge_viewport(generation, timestamp, None, now)
    }

    pub fn acknowledge_viewport(
        &mut self,
        generation: u64,
        timestamp: u32,
        viewport_seq: Option<u64>,
        now: Instant,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(frame) = self
            .frames
            .iter()
            .rev()
            .find(|f| f.timestamp == timestamp)
            .copied()
        else {
            return false;
        };
        if frame.generation != generation
            || frame.viewport.map(|viewport| viewport.seq) != viewport_seq
            || self
                .presented
                .is_some_and(|old| frame.captured <= old.captured)
        {
            return false;
        }
        self.presented = Some(frame);
        self.presented_at = Some(now);
        true
    }

    pub(crate) fn evidence(&self, generation: u64, now: Instant) -> DisplayEvidence {
        DisplayEvidence {
            generation_matches: generation == self.generation,
            same_content: self
                .presented
                .map(|frame| frame.content == self.current_content),
            capture_age_us: micros(now.saturating_duration_since(self.capture_at)),
            presented_capture_age_us: self
                .presented
                .map(|frame| micros(now.saturating_duration_since(frame.captured))),
            accepted_ack_age_us: self.accepted_ack_age(now).map(micros),
            display_deadline_offset_us: self
                .input_deadline(generation)
                .map(|deadline| deadline_offset(deadline, now)),
        }
    }

    pub fn input_deadline(&self, generation: u64) -> Option<Instant> {
        if generation != self.generation {
            return None;
        }
        let frame = self.presented?;
        let mut deadline =
            (self.capture_at + Duration::from_millis(750)).min(self.presented_at? + DEADMAN);
        if frame.content != self.current_content {
            deadline = deadline.min(frame.captured + Duration::from_millis(500));
        }
        Some(deadline)
    }

    pub fn input_permit(
        &self,
        generation: u64,
        heartbeat: Instant,
        now: Instant,
    ) -> Option<InputPermit> {
        let expires_at = self.input_deadline(generation)?.min(heartbeat + DEADMAN);
        (now < expires_at).then(|| InputPermit::new(generation, expires_at))
    }

    pub fn input_permit_for_frame(
        &self,
        generation: u64,
        rtp: u32,
        heartbeat: Instant,
        now: Instant,
    ) -> Option<InputPermit> {
        (self.presented?.timestamp == rtp)
            .then(|| self.input_permit(generation, heartbeat, now))
            .flatten()
    }

    pub fn admits(&self, generation: u64, now: Instant) -> bool {
        self.input_deadline(generation)
            .is_some_and(|deadline| now < deadline)
    }

    pub fn accepted_ack_age(&self, now: Instant) -> Option<Duration> {
        self.presented_at
            .map(|at| now.saturating_duration_since(at))
    }

    pub fn acknowledged_rtp(&self) -> Option<u32> {
        self.presented.map(|frame| frame.timestamp)
    }

    pub fn acknowledged_frame(&self) -> Option<FrameIdentity> {
        self.presented
    }

    pub fn presented_age(&self, now: Instant) -> Option<Duration> {
        self.presented
            .map(|frame| now.duration_since(frame.captured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presented(start: Instant) -> DisplayProgress {
        let mut progress = DisplayProgress::new(1, start);
        progress.captured(1, start);
        progress.sent(FrameIdentity {
            timestamp: 42,
            generation: 1,
            content: 1,
            captured: start,
            viewport: None,
        });
        assert!(progress.acknowledge(1, 42, start));
        progress
    }

    #[test]
    fn evidence_reports_changed_content_deadline_without_changing_strict_admission() {
        let start = Instant::now();
        let mut progress = presented(start);
        progress.captured(2, start + Duration::from_millis(400));
        let before = start + Duration::from_micros(499_999);
        assert!(progress.admits(1, before));
        let facts = progress.evidence(1, before);
        assert_eq!(facts.display_deadline_offset_us, Some(1));
        assert_eq!(facts.same_content, Some(false));
        assert_eq!(facts.capture_age_us, 99_999);
        assert_eq!(facts.presented_capture_age_us, Some(499_999));
        assert_eq!(facts.accepted_ack_age_us, Some(499_999));
        let at = start + Duration::from_millis(500);
        assert!(!progress.admits(1, at));
        assert_eq!(progress.evidence(1, at).display_deadline_offset_us, Some(0));
        assert_eq!(
            progress
                .evidence(1, at + Duration::from_micros(1))
                .display_deadline_offset_us,
            Some(-1)
        );
        progress.captured(1, start + Duration::from_millis(700));
        assert!(progress.admits(1, at));
        assert_eq!(progress.evidence(1, at).same_content, Some(true));
        assert!(!progress.evidence(2, at).generation_matches);
        assert_eq!(progress.evidence(2, at).display_deadline_offset_us, None);
        let missing = DisplayProgress::new(1, start).evidence(1, at);
        assert_eq!(missing.presented_capture_age_us, None);
        assert_eq!(missing.accepted_ack_age_us, None);
        assert_eq!(missing.display_deadline_offset_us, None);
    }

    #[test]
    fn evidence_preserves_capture_ack_and_heartbeat_boundaries() {
        let start = Instant::now();
        let mut progress = presented(start);
        assert!(progress.admits(1, start + Duration::from_micros(749_999)));
        assert!(!progress.admits(1, start + Duration::from_millis(750)));
        assert_eq!(
            progress
                .evidence(1, start + Duration::from_millis(750))
                .display_deadline_offset_us,
            Some(0)
        );
        progress.captured(1, start + Duration::from_millis(2800));
        assert!(progress.admits(1, start + Duration::from_micros(2_999_999)));
        assert!(!progress.admits(1, start + DEADMAN));
        assert_eq!(
            progress
                .evidence(1, start + DEADMAN)
                .display_deadline_offset_us,
            Some(0)
        );
        progress.sent(FrameIdentity {
            timestamp: 43,
            generation: 1,
            content: 1,
            captured: start + Duration::from_millis(2800),
            viewport: None,
        });
        assert!(progress.acknowledge(1, 43, start + Duration::from_millis(2800)));
        assert!(
            progress
                .input_permit(1, start, start + Duration::from_micros(2_999_999))
                .is_some()
        );
        assert!(progress.input_permit(1, start, start + DEADMAN).is_none());
        assert_eq!(
            deadline_offset(
                progress.input_deadline(1).unwrap().min(start + DEADMAN),
                start + DEADMAN
            ),
            0
        );
    }

    #[test]
    fn input_deadline_is_the_earliest_capture_presentation_or_changed_content_limit() {
        let start = Instant::now();
        let mut progress = presented(start);
        assert_eq!(
            progress.input_deadline(1),
            Some(start + Duration::from_millis(750))
        );
        assert_eq!(progress.input_deadline(2), None);
        let later = start + Duration::from_millis(2800);
        progress.captured(1, later);
        assert_eq!(progress.input_deadline(1), Some(start + DEADMAN));
        progress.captured(2, later);
        assert_eq!(
            progress.input_deadline(1),
            Some(start + Duration::from_millis(500))
        );
        assert!(DisplayProgress::new(1, start).input_deadline(1).is_none());
    }

    #[test]
    fn permits_reject_exact_deadlines_and_apply_the_static_content_exception() {
        let start = Instant::now();
        let mut progress = presented(start);
        let capture_deadline = start + Duration::from_millis(750);
        assert!(progress.input_permit(1, start, capture_deadline).is_none());
        progress.captured(2, start + Duration::from_millis(400));
        let content_deadline = start + Duration::from_millis(500);
        assert!(progress.input_permit(1, start, content_deadline).is_none());
        let later = start + Duration::from_millis(2800);
        progress.captured(1, later);
        assert!(progress.input_permit(1, later, later).is_some());
        assert!(progress.input_permit(1, later, start + DEADMAN).is_none());
    }

    #[test]
    fn input_proof_requires_the_presented_frame_and_keeps_the_changed_content_deadline() {
        let start = Instant::now();
        let mut progress = presented(start);
        progress.captured(2, start + Duration::from_millis(100));
        let before = start + Duration::from_millis(499);
        assert!(
            progress
                .input_permit_for_frame(1, 42, start, before)
                .is_some()
        );
        assert!(
            progress
                .input_permit_for_frame(1, 43, start, before)
                .is_none()
        );
        assert!(
            progress
                .input_permit_for_frame(2, 42, start, before)
                .is_none()
        );
        assert!(
            progress
                .input_permit_for_frame(1, 42, start, start + Duration::from_millis(500))
                .is_none()
        );
        progress.sent(FrameIdentity {
            timestamp: 43,
            generation: 1,
            content: 2,
            captured: start + Duration::from_millis(100),
            viewport: None,
        });
        assert!(progress.acknowledge(1, 43, before));
        assert!(
            progress
                .input_permit_for_frame(1, 42, start, before)
                .is_none()
        );
        assert!(
            progress
                .input_permit_for_frame(1, 43, start, before)
                .is_some()
        );
    }

    #[test]
    fn heartbeat_bounds_the_permit_even_with_recent_static_presentations() {
        let start = Instant::now();
        let mut progress = presented(start);
        let later = start + Duration::from_millis(2800);
        progress.captured(1, later);
        progress.sent(FrameIdentity {
            timestamp: 43,
            generation: 1,
            content: 1,
            captured: later,
            viewport: None,
        });
        assert!(progress.acknowledge(1, 43, later));
        assert_eq!(
            progress.input_deadline(1),
            Some(later + Duration::from_millis(750))
        );
        assert!(progress.input_permit(1, start, later).is_some());
        assert!(progress.input_permit(1, start, start + DEADMAN).is_none());
        assert!(progress.input_permit(1, later, start + DEADMAN).is_some());
    }

    #[test]
    fn changing_stalled_display_suspends_control_but_static_capture_stays_usable() {
        let start = Instant::now();
        let mut progress = DisplayProgress::new(1, start);
        progress.captured(1, start);
        progress.sent(FrameIdentity {
            timestamp: 42,
            generation: 1,
            content: 1,
            captured: start,
            viewport: None,
        });
        assert!(!progress.admits(1, start));
        assert!(!progress.acknowledge(2, 42, start));
        assert!(!progress.acknowledge(1, 43, start));
        assert!(progress.acknowledge(1, 42, start));
        let later = start + Duration::from_secs(2);
        progress.captured(1, later);
        assert!(progress.admits(1, later));
        progress.captured(2, later);
        progress.captured(2, later + Duration::from_millis(600));
        assert!(!progress.admits(1, later + Duration::from_millis(600)));
        assert!(!progress.admits(2, later));
    }

    #[test]
    fn old_presentations_cannot_regress_progress_and_rtp_wrap_is_supported() {
        let start = Instant::now();
        let mut p = DisplayProgress::new(1, start);
        p.sent(FrameIdentity {
            timestamp: u32::MAX,
            generation: 1,
            content: 1,
            captured: start,
            viewport: None,
        });
        p.sent(FrameIdentity {
            timestamp: 4,
            generation: 1,
            content: 2,
            captured: start + Duration::from_millis(1),
            viewport: None,
        });
        assert!(p.acknowledge(1, 4, start));
        assert!(!p.acknowledge(1, u32::MAX, start));
        let new = DisplayProgress::new(1, start);
        assert!(!new.admits(1, start));
    }

    #[test]
    fn delayed_ack_from_the_old_viewport_cannot_authorize_a_reused_rtp_timestamp() {
        let start = Instant::now();
        let viewport = Viewport::new(0, 0, 1280, 720, 1920, 1080).unwrap();
        let mut progress = DisplayProgress::new(1, start);
        progress.captured(1, start);
        progress.sent(FrameIdentity {
            timestamp: 42,
            generation: 1,
            content: 1,
            captured: start,
            viewport: Some(ViewportFrame { seq: 1, viewport }),
        });
        assert!(!progress.acknowledge_viewport(1, 42, Some(0), start));
        assert!(!progress.acknowledge(1, 42, start));
        assert!(
            progress
                .input_permit_for_frame(1, 42, start, start)
                .is_none()
        );
        assert!(progress.acknowledge_viewport(1, 42, Some(1), start));
        assert!(
            progress
                .input_permit_for_frame(1, 42, start, start)
                .is_some()
        );
    }

    #[test]
    fn viewport_command_requires_an_exact_integer_origin_and_sequence() {
        let pan: Command = serde_json::from_str(
            r#"{"type":"viewport","revision":"8","generation":1,"seq":2,"x":640,"y":360}"#,
        )
        .unwrap();
        assert!(
            matches!(pan, Command::Viewport { revision, generation: 1, seq: 2, x: 640, y: 360 } if revision == "8")
        );
        assert!(
            serde_json::from_str::<Command>(
                r#"{"type":"viewport","revision":"8","generation":1,"seq":2,"x":-1,"y":360}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Command>(
                r#"{"type":"viewport","revision":"8","generation":1,"seq":2,"x":0.5,"y":360}"#
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_commands_are_rejected() {
        let Command::Key {
            revision,
            generation,
            rtp,
            code,
            down,
            ..
        } = serde_json::from_str::<Command>(
            r#"{"type":"key","revision":"1","generation":2,"rtp":43,"code":30,"down":true}"#,
        )
        .unwrap()
        else {
            panic!("key command required");
        };
        assert_eq!(
            (revision.as_str(), generation, rtp, code, down),
            ("1", 2, Some(43), 30, true)
        );
        assert!(
            serde_json::from_str::<Command>(r#"{"type":"key","code":42,"down":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Command>(r#"{"type":"heartbeat","extra":1}"#).is_err());
        assert!(matches!(
            serde_json::from_str::<Command>(
                r#"{"type":"key","revision":"1","generation":1,"code":30,"down":true}"#
            ),
            Ok(Command::Key { rtp: None, .. })
        ));
    }
    #[test]
    fn continuous_changes_and_replayed_acknowledgments_cannot_keep_stale_control_alive() {
        let start = Instant::now();
        let mut p = DisplayProgress::new(1, start);
        p.sent(FrameIdentity {
            timestamp: 1,
            generation: 1,
            content: 1,
            captured: start,
            viewport: None,
        });
        assert!(p.acknowledge(1, 1, start));
        for step in 1..=20 {
            let now = start + Duration::from_millis(step * 50);
            p.captured(step + 1, now);
            assert!(!p.acknowledge(1, 1, now));
            if step == 9 {
                assert!(p.admits(1, start + Duration::from_millis(499)));
            }
            if step == 10 {
                assert!(!p.admits(1, start + Duration::from_millis(500)));
            }
        }
        assert!(!p.admits(1, start + Duration::from_secs(1)));
    }
}
