use crate::{
    desktop::AuthoritySnapshot,
    desktop_actor::{ActorTimings, CleanupHealth, ShutdownOutcome},
    encoder::EncodingTimings,
    session::DisplayEvidence,
};
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

const QUEUE_CAPACITY: usize = 32;
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultReason {
    EncoderInputDeadline,
    EncoderOutputDeadline,
    EncoderMetadataBacklog,
    EncoderOutputQueue,
    EncoderProcessExit,
    EncoderIo,
    EncoderWorkerLimit,
    EncoderFraming,
    EncoderOperation,
    DesktopDeadline,
    CaptureSyncDeadline,
    CaptureConstraintsDeadline,
    CaptureReadyDeadline,
    CaptureRejected,
    OutputUnavailable,
    GeometryChanged,
    InputExpired,
    InputBacklog,
    DesktopCancelled,
    DesktopConnection,
    DesktopOperation,
    Ice,
    Dtls,
    DataChannel,
    RtcOrdering,
    RtcOperation,
    ControllerDeadline,
    PeerClosed,
    ResetDeadline,
    ControlBackpressure,
    Transport,
    Socket,
    LockChanged,
    Stopped,
    Replaced,
    Operation,
}
impl FaultReason {
    pub fn from_safe_reason(reason: &'static str) -> Self {
        match reason {
            "encoder input deadline" => Self::EncoderInputDeadline,
            "encoder output deadline" => Self::EncoderOutputDeadline,
            "encoder metadata backlog" => Self::EncoderMetadataBacklog,
            "encoder output queue" => Self::EncoderOutputQueue,
            "encoder process exit" => Self::EncoderProcessExit,
            "encoder IO" => Self::EncoderIo,
            "encoder worker limit" => Self::EncoderWorkerLimit,
            "encoder framing" => Self::EncoderFraming,
            "encoder operation" => Self::EncoderOperation,
            "Wayland deadline" => Self::DesktopDeadline,
            "capture sync deadline" => Self::CaptureSyncDeadline,
            "capture constraints deadline" => Self::CaptureConstraintsDeadline,
            "capture ready deadline" => Self::CaptureReadyDeadline,
            "capture rejected" => Self::CaptureRejected,
            "output unavailable" => Self::OutputUnavailable,
            "geometry changed" => Self::GeometryChanged,
            "input permission expired" => Self::InputExpired,
            "input command backlog" => Self::InputBacklog,
            "desktop command cancelled" => Self::DesktopCancelled,
            "Wayland connection" => Self::DesktopConnection,
            "desktop operation" => Self::DesktopOperation,
            "ICE" => Self::Ice,
            "DTLS" => Self::Dtls,
            "data channel" => Self::DataChannel,
            "RTC write ordering" => Self::RtcOrdering,
            "RTC operation" => Self::RtcOperation,
            "controller deadline" => Self::ControllerDeadline,
            "peer closed" => Self::PeerClosed,
            "reset deadline" => Self::ResetDeadline,
            "control backpressure" => Self::ControlBackpressure,
            _ => Self::Operation,
        }
    }
}

#[derive(Default, Serialize)]
pub struct ActorRecord {
    capture_last_ms: u64,
    capture_timeout_sync: u64,
    capture_timeout_constraints: u64,
    capture_timeout_ready: u64,
    capture_timeout_consecutive: u64,
    capture_failed_last_ms: u64,
    conversion_last_ms: u64,
    comparison_last_ms: u64,
    input_queue_last_ms: u64,
    lock_query_last_ms: u64,
    lock_query_sequence: u64,
    lock_query_outcome: u8,
    lock_query_counts: [u64; 8],
    lock_observed_us: u64,
    lock_query_us: u64,
    cancellations: u64,
    rejected_inputs: u64,
    input_completed: u64,
    input_expired: u64,
    input_cancelled: u64,
    input_failed: u64,
}
impl From<ActorTimings> for ActorRecord {
    fn from(t: ActorTimings) -> Self {
        Self {
            capture_last_ms: t.capture_ms,
            capture_timeout_sync: t.capture_timeout_sync,
            capture_timeout_constraints: t.capture_timeout_constraints,
            capture_timeout_ready: t.capture_timeout_ready,
            capture_timeout_consecutive: t.capture_timeout_consecutive,
            capture_failed_last_ms: t.capture_failed_ms,
            conversion_last_ms: t.conversion_ms,
            comparison_last_ms: t.comparison_ms,
            input_queue_last_ms: t.input_queue_ms,
            lock_query_last_ms: t.lock_query_ms,
            lock_query_sequence: t.lock_query_sequence,
            lock_query_outcome: t.lock_query_outcome,
            lock_query_counts: t.lock_query_counts,
            lock_observed_us: t.lock_observed_us,
            lock_query_us: t.lock_query_us,
            cancellations: t.cancellations,
            rejected_inputs: t.rejected_inputs,
            input_completed: t.input_completed,
            input_expired: t.input_expired,
            input_cancelled: t.input_cancelled,
            input_failed: t.input_failed,
        }
    }
}
#[derive(Default, Serialize)]
pub struct EncoderRecord {
    capture_to_submit_last_ms: u64,
    capture_to_selected_last_ms: u64,
    raw_write_last_ms: u64,
    raw_accepted: u64,
    raw_replaced: u64,
    raw_contended: u64,
    submit_to_output_last_ms: u64,
    capture_to_output_last_ms: u64,
}
impl From<EncodingTimings> for EncoderRecord {
    fn from(t: EncodingTimings) -> Self {
        Self {
            capture_to_submit_last_ms: t.capture_to_submit_ms,
            capture_to_selected_last_ms: t.capture_to_selected_ms,
            raw_write_last_ms: t.raw_write_ms,
            raw_accepted: t.raw_accepted,
            raw_replaced: t.raw_replaced,
            raw_contended: t.raw_contended,
            submit_to_output_last_ms: t.submit_to_output_ms,
            capture_to_output_last_ms: t.capture_to_output_ms,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AllocationPendingReason {
    ReductionCooldown,
    IncreaseConfirmation,
}
#[derive(Serialize)]
pub struct AllocationRecord {
    estimate_age_ms: Option<u64>,
    can_probe: Option<bool>,
    candidate_kbps: Option<u32>,
    pending_kbps: Option<u32>,
    pending_reason: Option<AllocationPendingReason>,
    pending_due_in_ms: Option<u64>,
    commits: u64,
}
impl AllocationRecord {
    pub(crate) fn new(controller: &crate::allocation::AllocationController, now: Instant) -> Self {
        use crate::allocation::{EstimateKind, PendingChange};
        let latest = controller.latest();
        let pending = controller.pending();
        let (pending_kbps, pending_reason) = match pending {
            Some(PendingChange::Reduction { allocation, .. }) => (
                Some(allocation.kbps),
                Some(AllocationPendingReason::ReductionCooldown),
            ),
            Some(PendingChange::Increase { minimum, .. }) => (
                Some(minimum.kbps),
                Some(AllocationPendingReason::IncreaseConfirmation),
            ),
            None => (None, None),
        };
        Self {
            estimate_age_ms: latest
                .map(|o| now.saturating_duration_since(o.observed_at).as_millis() as u64),
            can_probe: latest.and_then(|o| match o.kind {
                EstimateKind::Twcc { can_probe } => Some(can_probe),
                EstimateKind::Remb => None,
            }),
            candidate_kbps: controller.candidate().map(|a| a.kbps),
            pending_kbps,
            pending_reason,
            pending_due_in_ms: pending
                .map(|pending| pending.due_at().saturating_duration_since(now).as_millis() as u64),
            commits: controller.commits(),
        }
    }
}

#[derive(Serialize)]
pub struct PacerQueueRecord {
    pub sample_age_ms: u64,
    pub first_unsent_age_ms: Option<u64>,
    pub bytes: usize,
    pub packets: usize,
}

impl PacerQueueRecord {
    pub fn new(info: &dyn str0m::rtp::StreamTxQueueInfo, now: Instant) -> Self {
        Self {
            sample_age_ms: now.saturating_duration_since(info.created_at()).as_millis() as u64,
            first_unsent_age_ms: info
                .first_unsent()
                .map(|first| now.saturating_duration_since(first).as_millis() as u64),
            bytes: info.byte_size(),
            packets: info.packet_count(),
        }
    }
}

#[derive(Serialize)]
pub struct DiagRecord {
    pub version: u8,
    pub session: u64,
    pub elapsed_ms: u64,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub bwe_estimate_bps: Option<u64>,
    pub allocation: Option<AllocationRecord>,
    pub encoder_starts: u64,
    pub encoder_restarts: u64,
    pub keyframe_requests: u64,
    pub encoder_kbps: Option<u32>,
    pub encoder_fps: Option<u32>,
    pub frames: u64,
    pub input_ready: bool,
    pub input_received: u64,
    pub input_admitted: u64,
    pub input_denied: u64,
    pub actor: ActorRecord,
    pub encoder: Option<EncoderRecord>,
    pub capture_to_writer_last_ms: Option<u64>,
    pub pacer_queue: Option<PacerQueueRecord>,
    pub received_ack_frame_last_ms: Option<u64>,
    pub presented_frame_age_ms: Option<u64>,
    pub time_since_accepted_ack_ms: Option<u64>,
    pub rtc_tick_gap_max_ms: u64,
    pub due_timeout_lateness_max_ms: u64,
    pub fault: Option<FaultReason>,
    pub log_dropped: u64,
    pub log_failed: bool,
}

pub struct PeerSnapshot {
    pub allocation: Option<AllocationRecord>,
    pub bwe_estimate_bps: Option<u64>,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub frames: u64,
    pub input_ready: bool,
    pub actor: ActorTimings,
    pub presented_age: Option<Duration>,
    pub ack_age: Option<Duration>,
    pub pacer_queue: Option<PacerQueueRecord>,
}

pub struct PeerDiagnostics {
    pub input_received: u64,
    pub input_admitted: u64,
    pub input_denied: u64,
    pub session: u64,
    pub encoder_starts: u64,
    pub keyframe_requests: u64,
    pub(crate) encoder_allocation: Option<crate::allocation::Allocation>,
    pub encoding: Option<EncodingTimings>,
    pub capture_to_writer_last_ms: Option<u64>,
    pub received_ack_frame_last_ms: Option<u64>,
    pub fault: Option<FaultReason>,
    started: Instant,
    last_tick: Instant,
    next_record: Instant,
    tick_gap_max: Duration,
    timeout_lateness_max: Duration,
    force_record: bool,
    readiness_sequence: u64,
}
impl PeerDiagnostics {
    pub fn new(now: Instant) -> Self {
        Self {
            input_received: 0,
            input_admitted: 0,
            input_denied: 0,
            session: 0,
            encoder_starts: 0,
            keyframe_requests: 0,
            encoder_allocation: None,
            encoding: None,
            capture_to_writer_last_ms: None,
            received_ack_frame_last_ms: None,
            fault: None,
            started: now,
            last_tick: now,
            next_record: now,
            tick_gap_max: Duration::ZERO,
            timeout_lateness_max: Duration::ZERO,
            force_record: false,
            readiness_sequence: 0,
        }
    }
    pub(crate) fn readiness_record(
        &mut self,
        now: Instant,
        observation: ReadinessObservation,
    ) -> ReadinessRecord {
        let ReadinessObservation {
            event,
            reason,
            ready_before,
            ready_after,
            authority,
            evidence,
        } = observation;
        self.readiness_sequence = self.readiness_sequence.saturating_add(1);
        ReadinessRecord {
            version: 2,
            session: self.session,
            sequence: self.readiness_sequence,
            elapsed_us: crate::session::micros(now.saturating_duration_since(self.started)),
            readiness_event: event as u8,
            reason: reason as u8,
            ready_before,
            ready_after,
            authority_phase: authority.map(|snapshot| snapshot.phase as u8),
            input_revision: authority.map(|snapshot| snapshot.revision),
            authority_phase_after: None,
            input_revision_after: None,
            submitted_revision: None,
            message_revision: None,
            acknowledgement_accepted: None,
            generation: None,
            heartbeat_age_us: 0,
            permit_deadline_offset_us: None,
            display: evidence,
            pacer_queue: None,
            log_dropped: 0,
            log_failed: false,
        }
    }
    pub fn tick(&mut self, now: Instant) {
        self.tick_gap_max = self
            .tick_gap_max
            .max(now.saturating_duration_since(self.last_tick));
        self.last_tick = now;
    }
    pub fn timeout(&mut self, now: Instant, deadline: Instant) {
        self.timeout_lateness_max = self
            .timeout_lateness_max
            .max(now.saturating_duration_since(deadline));
    }
    pub fn fail(&mut self, reason: FaultReason) {
        self.fault = Some(reason);
        self.force_record = true;
    }
    pub fn due(&mut self, now: Instant) -> bool {
        if !self.force_record && now < self.next_record {
            return false;
        }
        self.force_record = false;
        self.next_record = now + Duration::from_secs(1);
        true
    }
    pub fn record(&self, now: Instant, snapshot: PeerSnapshot) -> DiagRecord {
        let PeerSnapshot {
            allocation,
            bwe_estimate_bps,
            bitrate_kbps,
            fps,
            frames,
            input_ready,
            actor,
            presented_age,
            ack_age,
            pacer_queue,
        } = snapshot;
        DiagRecord {
            version: 2,
            session: self.session,
            elapsed_ms: now.saturating_duration_since(self.started).as_millis() as u64,
            bitrate_kbps,
            fps,
            bwe_estimate_bps,
            allocation,
            encoder_starts: self.encoder_starts,
            encoder_restarts: self.encoder_starts.saturating_sub(1),
            keyframe_requests: self.keyframe_requests,
            encoder_kbps: self.encoder_allocation.map(|a| a.kbps),
            encoder_fps: self.encoder_allocation.map(|a| a.fps),
            frames,
            input_ready,
            input_received: self.input_received,
            input_admitted: self.input_admitted,
            input_denied: self.input_denied,
            actor: actor.into(),
            encoder: self.encoding.map(Into::into),
            capture_to_writer_last_ms: self.capture_to_writer_last_ms,
            pacer_queue,
            received_ack_frame_last_ms: self.received_ack_frame_last_ms,
            presented_frame_age_ms: presented_age.map(|d| d.as_millis() as u64),
            time_since_accepted_ack_ms: ack_age.map(|d| d.as_millis() as u64),
            rtc_tick_gap_max_ms: self.tick_gap_max.as_millis() as u64,
            due_timeout_lateness_max_ms: self.timeout_lateness_max.as_millis() as u64,
            fault: self.fault,
            log_dropped: 0,
            log_failed: false,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupEvent {
    NativeReleaseFailure,
    HostShutdown,
}
#[derive(Serialize)]
pub struct CleanupRecord {
    pub version: u8,
    pub event: CleanupEvent,
    pub elapsed_ms: u64,
    pub cleanup: CleanupHealth,
    pub shutdown: Option<ShutdownOutcome>,
    pub log_dropped: u64,
    pub log_failed: bool,
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReadinessEvent {
    PeerFalling = 1,
    PeerRising = 2,
    AdmissionDenied = 3,
    ExplicitRelease = 4,
    ResetObserved = 5,
    ResetAcknowledged = 6,
    StateQueued = 7,
    HeartbeatRevocation = 8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadinessReason {
    None = 0,
    AuthorityAlreadyClosed = 1,
    PeerResetting = 2,
    ChannelUnauthenticated = 3,
    DesktopAbsent = 4,
    LockUnknown = 5,
    ProgressAbsent = 6,
    GenerationMismatch = 7,
    PresentationAbsent = 8,
    DisplayExpired = 9,
    HeartbeatExpired = 10,
    ExplicitRelease = 11,
    ResetObservation = 12,
    Unresolved = 13,
    LockChanged = 14,
    GeometryChanged = 15,
}
pub(crate) struct ReadinessObservation {
    pub event: ReadinessEvent,
    pub reason: ReadinessReason,
    pub ready_before: bool,
    pub ready_after: bool,
    pub authority: Option<AuthoritySnapshot>,
    pub evidence: Option<DisplayEvidence>,
}
#[derive(Serialize)]
pub(crate) struct ReadinessRecord {
    pub version: u8,
    pub session: u64,
    pub sequence: u64,
    pub elapsed_us: u64,
    pub readiness_event: u8,
    pub reason: u8,
    pub ready_before: bool,
    pub ready_after: bool,
    pub authority_phase: Option<u8>,
    pub input_revision: Option<u64>,
    pub authority_phase_after: Option<u8>,
    pub input_revision_after: Option<u64>,
    pub submitted_revision: Option<u64>,
    pub message_revision: Option<u64>,
    pub acknowledgement_accepted: Option<bool>,
    pub generation: Option<u64>,
    pub heartbeat_age_us: u64,
    pub permit_deadline_offset_us: Option<i64>,
    pub display: Option<DisplayEvidence>,
    pub pacer_queue: Option<PacerQueueRecord>,
    pub log_dropped: u64,
    pub log_failed: bool,
}

#[derive(Serialize)]
pub(crate) struct NativeLockRecord {
    pub version: u8,
    pub actor_session: u64,
    pub lock_sequence: u64,
    pub native_lock_event: u8,
    pub lock_outcome: u8,
    pub query_elapsed_us: u64,
    pub observed_elapsed_us: u64,
    pub authority_phase: u8,
    pub input_revision: u64,
    pub lock_knowledge: u8,
    pub last_known: Option<u8>,
    pub transition: bool,
    pub lock_generation: u64,
    pub query_counts: [u64; 8],
    pub discarded: bool,
    pub log_dropped: u64,
    pub log_failed: bool,
}

#[derive(Serialize)]
pub(crate) struct LockRecord {
    pub version: u8,
    pub session: u64,
    pub lock_sequence: u64,
    pub lock_outcome: u8,
    pub query_elapsed_us: u64,
    pub observed_elapsed_us: u64,
    pub consumed_elapsed_us: u64,
    pub observation_phase: u8,
    pub observation_revision: u64,
    pub authority_phase: u8,
    pub input_revision: u64,
    pub lock_knowledge: u8,
    pub last_known: Option<u8>,
    pub transition: bool,
    pub lock_generation: u64,
    pub query_counts: [u64; 8],
    pub log_dropped: u64,
    pub log_failed: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum LogRecord {
    Peer(Box<DiagRecord>),
    Cleanup(CleanupRecord),
    Readiness(ReadinessRecord),
    Lock(LockRecord),
    NativeLock(NativeLockRecord),
}

pub struct DiagnosticsLog {
    sender: SyncSender<LogRecord>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    finished: Arc<(Mutex<bool>, Condvar)>,
}
impl DiagnosticsLog {
    #[cfg(test)]
    pub(crate) fn stalled() -> (Self, mpsc::Receiver<LogRecord>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        (
            Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
                failed: Arc::new(AtomicBool::new(false)),
                closing: Arc::new(AtomicBool::new(false)),
                finished: Arc::new((Mutex::new(false), Condvar::new())),
            },
            receiver,
        )
    }
    pub fn start(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let worker_failed = failed.clone();
        let closing = Arc::new(AtomicBool::new(false));
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_closing = closing.clone();
        let worker_finished = finished.clone();
        let worker_dropped = dropped.clone();
        let spawn = thread::Builder::new()
            .name("diagnostics-log".into())
            .spawn(move || {
                let mut inflight = false;
                let result = (|| -> io::Result<()> {
                    let mut file = open_log(&path)?;
                    let mut size = file.metadata()?.len();
                    loop {
                        let record = match receiver.recv_timeout(Duration::from_millis(10)) {
                            Ok(record) => record,
                            Err(mpsc::RecvTimeoutError::Timeout)
                                if !worker_closing.load(Ordering::Acquire) =>
                            {
                                continue;
                            }
                            Err(_) => break,
                        };
                        inflight = true;
                        let mut bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
                        bytes.push(b'\n');
                        if bytes.len() > MAX_RECORD_BYTES {
                            return Err(io::Error::other("diagnostic record limit"));
                        }
                        if size + bytes.len() as u64 > MAX_LOG_BYTES {
                            file.set_len(0)?;
                            file.seek(SeekFrom::Start(0))?;
                            size = 0;
                        }
                        file.write_all(&bytes)?;
                        size += bytes.len() as u64;
                        inflight = false;
                    }
                    Ok(())
                })();
                if result.is_err() {
                    worker_failed.store(true, Ordering::Release);
                    worker_dropped.fetch_add(
                        u64::from(inflight) + receiver.try_iter().count() as u64,
                        Ordering::Relaxed,
                    );
                }
                let (state, wake) = &*worker_finished;
                *state.lock().unwrap() = true;
                wake.notify_all();
            });
        if spawn.is_err() {
            failed.store(true, Ordering::Release);
            *finished.0.lock().unwrap() = true;
        }
        Self {
            sender,
            dropped,
            failed,
            closing,
            finished,
        }
    }
    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.closing.store(true, Ordering::Release);
        let (state, wake) = &*self.finished;
        let (finished, _) = wake
            .wait_timeout_while(state.lock().unwrap(), timeout, |done| !*done)
            .unwrap();
        if !*finished {
            self.failed.store(true, Ordering::Release);
        }
        *finished
    }
    pub fn submit(&self, mut record: DiagRecord) {
        record.log_dropped = self.dropped();
        record.log_failed = self.failed();
        self.enqueue(LogRecord::Peer(Box::new(record)));
    }
    pub fn submit_cleanup(&self, mut record: CleanupRecord) {
        record.log_dropped = self.dropped();
        record.log_failed = self.failed();
        self.enqueue(LogRecord::Cleanup(record));
    }
    pub(crate) fn submit_readiness(&self, mut record: ReadinessRecord) {
        record.log_dropped = self.dropped();
        record.log_failed = self.failed();
        self.enqueue(LogRecord::Readiness(record));
    }
    pub(crate) fn submit_native_lock(&self, mut record: NativeLockRecord) {
        record.log_dropped = self.dropped();
        record.log_failed = self.failed();
        self.enqueue(LogRecord::NativeLock(record));
    }
    pub(crate) fn submit_lock(&self, mut record: LockRecord) {
        record.log_dropped = self.dropped();
        record.log_failed = self.failed();
        self.enqueue(LogRecord::Lock(record));
    }
    fn enqueue(&self, record: LogRecord) {
        if self.closing.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(record)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}
fn open_log(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != std::fs::metadata("/proc/self")?.uid()
    {
        return Err(io::Error::other("diagnostic log ownership"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct QueueInfo {
        sampled: Instant,
        first: Option<Instant>,
    }
    impl str0m::rtp::StreamTxQueueInfo for QueueInfo {
        fn created_at(&self) -> Instant {
            self.sampled
        }
        fn byte_size(&self) -> usize {
            2048
        }
        fn packet_count(&self) -> usize {
            3
        }
        fn first_unsent(&self) -> Option<Instant> {
            self.first
        }
    }

    #[test]
    fn pacer_queue_record_distinguishes_stale_sample_from_unsent_packet_age() {
        let now = Instant::now();
        let info = QueueInfo {
            sampled: now - Duration::from_millis(25),
            first: Some(now - Duration::from_millis(180)),
        };
        let queue = PacerQueueRecord::new(&info, now);
        assert_eq!(queue.sample_age_ms, 25);
        assert_eq!(queue.first_unsent_age_ms, Some(180));
        assert_eq!((queue.bytes, queue.packets), (2048, 3));
    }

    fn known_authority() -> crate::desktop::InputAuthority {
        let authority = crate::desktop::InputAuthority::default();
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Ok(crate::desktop::KnownLock::Unlocked),
            elapsed_us: 0,
        });
        authority
    }

    use serde_json::{Value, json};
    fn record(now: Instant) -> DiagRecord {
        let mut diagnostics = PeerDiagnostics::new(now);
        diagnostics.session = 3;
        diagnostics.received_ack_frame_last_ms = Some(120);
        diagnostics.encoding = Some(EncodingTimings {
            capture_to_submit_ms: 15,
            capture_to_selected_ms: 10,
            raw_write_ms: 5,
            raw_accepted: 4,
            raw_replaced: 2,
            raw_contended: 1,
            submit_to_output_ms: 9,
            capture_to_output_ms: 24,
        });
        diagnostics.fail(FaultReason::EncoderFraming);
        diagnostics.record(
            now + Duration::from_millis(10),
            PeerSnapshot {
                allocation: None,
                bwe_estimate_bps: Some(900_000),
                bitrate_kbps: 700,
                fps: 8,
                frames: 6,
                input_ready: false,
                actor: ActorTimings {
                    capture_timeout_sync: 2,
                    capture_timeout_constraints: 3,
                    capture_timeout_ready: 5,
                    capture_timeout_consecutive: 1,
                    capture_failed_ms: 120,
                    ..ActorTimings::default()
                },
                presented_age: Some(Duration::from_millis(130)),
                ack_age: Some(Duration::from_millis(10)),
                pacer_queue: Some(PacerQueueRecord {
                    sample_age_ms: 4,
                    first_unsent_age_ms: Some(80),
                    bytes: 2048,
                    packets: 3,
                }),
            },
        )
    }
    fn wait(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn readiness(now: Instant) -> ReadinessRecord {
        let mut diagnostics = PeerDiagnostics::new(now);
        diagnostics.session = 3;
        diagnostics.readiness_record(
            now + Duration::from_micros(12),
            ReadinessObservation {
                event: ReadinessEvent::PeerFalling,
                reason: ReadinessReason::DisplayExpired,
                ready_before: true,
                ready_after: false,
                authority: Some(AuthoritySnapshot {
                    phase: crate::desktop::AuthorityPhase::Open,
                    revision: 4,
                    lock: crate::desktop::LockKnowledge::Verified(
                        crate::desktop::KnownLock::Unlocked,
                    ),
                    lock_generation: 1,
                }),
                evidence: Some(DisplayEvidence {
                    generation_matches: true,
                    same_content: Some(false),
                    capture_age_us: 7,
                    presented_capture_age_us: Some(500_001),
                    accepted_ack_age_us: Some(20),
                    display_deadline_offset_us: Some(-1),
                }),
            },
        )
    }
    #[test]
    fn readiness_schema_is_literal_numeric_private_and_bounded() {
        let bytes = serde_json::to_vec(&readiness(Instant::now())).unwrap();
        assert!(bytes.len() + 1 < MAX_RECORD_BYTES);
        let actual: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            actual,
            json!({
                "version":2,"session":3,"sequence":1,"elapsed_us":12,"readiness_event":1,"reason":9,
                "ready_before":true,"ready_after":false,"authority_phase":0,"input_revision":4,
                "authority_phase_after":null,"input_revision_after":null,"submitted_revision":null,"message_revision":null,
                "acknowledgement_accepted":null,"generation":null,"heartbeat_age_us":0,
                "permit_deadline_offset_us":null,
                "display":{"generation_matches":true,"same_content":false,"capture_age_us":7,
                    "presented_capture_age_us":500001,"accepted_ack_age_us":20,"display_deadline_offset_us":-1},
                "pacer_queue":null,
                "log_dropped":0,"log_failed":false
            })
        );
        fn numeric(value: &Value) -> bool {
            match value {
                Value::Null | Value::Bool(_) | Value::Number(_) => true,
                Value::Object(fields) => fields.values().all(numeric),
                _ => false,
            }
        }
        assert!(numeric(&actual));
        assert!(std::mem::size_of::<ReadinessRecord>() <= 320);
        let mut largest = readiness(Instant::now());
        largest.session = u64::MAX;
        largest.sequence = u64::MAX;
        largest.elapsed_us = u64::MAX;
        largest.input_revision = Some(u64::MAX);
        largest.input_revision_after = Some(u64::MAX);
        largest.submitted_revision = Some(u64::MAX);
        largest.message_revision = Some(u64::MAX);
        largest.generation = Some(u64::MAX);
        largest.heartbeat_age_us = u64::MAX;
        largest.permit_deadline_offset_us = Some(i64::MIN);
        largest.log_dropped = u64::MAX;
        largest.display = Some(DisplayEvidence {
            generation_matches: false,
            same_content: Some(false),
            capture_age_us: u64::MAX,
            presented_capture_age_us: Some(u64::MAX),
            accepted_ack_age_us: Some(u64::MAX),
            display_deadline_offset_us: Some(i64::MIN),
        });
        assert!(serde_json::to_vec(&largest).unwrap().len() + 1 < MAX_RECORD_BYTES);
    }
    #[test]
    fn stalled_readiness_logger_drops_without_blocking_authority_release() {
        let (log, receiver) = DiagnosticsLog::stalled();
        let start = Instant::now();
        let authority = known_authority();
        log.submit_readiness(readiness(start));
        authority.revoke();
        for _ in 0..100 {
            log.submit_readiness(readiness(start));
        }
        assert_eq!(authority.released(), Some(1));
        assert!(authority.acknowledge(1));
        assert_eq!(log.dropped(), 100);
        assert!(start.elapsed() < Duration::from_millis(100));
        let LogRecord::Readiness(record) = receiver.try_recv().unwrap() else {
            panic!("readiness record required")
        };
        assert_eq!(record.sequence, 1);
        assert_eq!(record.log_dropped, 0);
        assert!(!record.log_failed);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn allocation_log_separates_observation_age_capability_pending_and_commits() {
        use crate::allocation::{
            Allocation, AllocationController, BandwidthObservation, EstimateKind,
        };
        let start = Instant::now();
        let mut controller =
            AllocationController::new(3000, Allocation { kbps: 700, fps: 30 }, start);
        controller.observe(BandwidthObservation {
            estimate_bps: 1_050_000,
            observed_at: start,
            kind: EstimateKind::Twcc { can_probe: false },
        });
        let waiting = serde_json::to_value(AllocationRecord::new(
            &controller,
            start + Duration::from_secs(2),
        ))
        .unwrap();
        assert_eq!(
            waiting,
            json!({
                "estimate_age_ms":2000,"can_probe":false,"candidate_kbps":840,
                "pending_kbps":840,"pending_reason":"increase_confirmation","pending_due_in_ms":1000,"commits":0
            })
        );
        assert_eq!(
            controller.take_due(start + Duration::from_secs(3)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
        let committed = serde_json::to_value(AllocationRecord::new(
            &controller,
            start + Duration::from_secs(60),
        ))
        .unwrap();
        assert_eq!(
            committed,
            json!({
                "estimate_age_ms":60000,"can_probe":false,"candidate_kbps":840,
                "pending_kbps":null,"pending_reason":null,"pending_due_in_ms":null,"commits":1
            })
        );
        let mut record = record(start);
        record.allocation = Some(AllocationRecord::new(
            &controller,
            start + Duration::from_secs(60),
        ));
        assert!(serde_json::to_vec(&record).unwrap().len() < MAX_RECORD_BYTES);
    }

    #[test]
    fn native_lock_observation_is_numeric_bounded_and_nonblocking() {
        let (logger, receiver) = DiagnosticsLog::stalled();
        for _ in 0..2 {
            logger.submit_native_lock(NativeLockRecord {
                version: 1,
                actor_session: 4,
                lock_sequence: 9,
                native_lock_event: 2,
                lock_outcome: 2,
                query_elapsed_us: 150_321,
                observed_elapsed_us: 950_000,
                authority_phase: 1,
                input_revision: 7,
                lock_knowledge: 3,
                last_known: Some(0),
                transition: false,
                lock_generation: 2,
                query_counts: [8, 0, 1, 0, 0, 0, 0, 0],
                discarded: false,
                log_dropped: 0,
                log_failed: false,
            });
        }
        assert_eq!(logger.dropped(), 1);
        let LogRecord::NativeLock(record) = receiver.try_recv().unwrap() else {
            panic!("native observation record absent")
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(bytes.len() < MAX_RECORD_BYTES);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({
                "version":1,"actor_session":4,"lock_sequence":9,"native_lock_event":2,"lock_outcome":2,
                "query_elapsed_us":150321,"observed_elapsed_us":950000,
                "authority_phase":1,"input_revision":7,"lock_knowledge":3,"last_known":0,"transition":false,
                "lock_generation":2,"query_counts":[8,0,1,0,0,0,0,0],"discarded":false,"log_dropped":0,"log_failed":false,
            })
        );
    }
    #[test]
    fn diagnostic_schema_contains_only_closed_safe_fields_and_distinct_ages() {
        let bytes = serde_json::to_vec(&record(Instant::now())).unwrap();
        assert!(bytes.len() < MAX_RECORD_BYTES);
        let actual: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            actual,
            json!({
                "version":2,"session":3,"elapsed_ms":10,"bitrate_kbps":700,"fps":8,"bwe_estimate_bps":900000,"allocation":null,"encoder_starts":0,"encoder_restarts":0,"keyframe_requests":0,"encoder_kbps":null,"encoder_fps":null,"frames":6,"input_ready":false,
                "input_received":0,"input_admitted":0,"input_denied":0,
                "actor":{"capture_last_ms":0,"capture_timeout_sync":2,"capture_timeout_constraints":3,"capture_timeout_ready":5,"capture_timeout_consecutive":1,"capture_failed_last_ms":120,"conversion_last_ms":0,"comparison_last_ms":0,"input_queue_last_ms":0,"lock_query_last_ms":0,"lock_query_sequence":0,"lock_query_outcome":0,"lock_query_counts":[0,0,0,0,0,0,0,0],"lock_observed_us":0,"lock_query_us":0,"cancellations":0,"rejected_inputs":0,"input_completed":0,"input_expired":0,"input_cancelled":0,"input_failed":0},
                "encoder":{"capture_to_submit_last_ms":15,"capture_to_selected_last_ms":10,"raw_write_last_ms":5,"raw_accepted":4,"raw_replaced":2,"raw_contended":1,"submit_to_output_last_ms":9,"capture_to_output_last_ms":24},
                "capture_to_writer_last_ms":null,"pacer_queue":{"sample_age_ms":4,"first_unsent_age_ms":80,"bytes":2048,"packets":3},"received_ack_frame_last_ms":120,"presented_frame_age_ms":130,"time_since_accepted_ack_ms":10,"rtc_tick_gap_max_ms":0,"due_timeout_lateness_max_ms":0,"fault":"encoder_framing","log_dropped":0,"log_failed":false
            })
        );
        assert!(matches!(
            FaultReason::from_safe_reason("unknown private detail"),
            FaultReason::Operation
        ));
    }
    #[test]
    fn host_cleanup_failure_and_final_outcome_drain_before_writer_shutdown() {
        use crate::desktop_actor::{CleanupFailure, CleanupFault};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleanup.jsonl");
        let logger = DiagnosticsLog::start(path.clone());
        let failure = CleanupFailure {
            actor_session: 7,
            sequence: 9,
            reason: CleanupFault::NativeDeadline,
        };
        let health = CleanupHealth {
            completions: 9,
            failures: 3,
            last_failure: Some(failure),
        };
        logger.submit_cleanup(CleanupRecord {
            version: 1,
            event: CleanupEvent::NativeReleaseFailure,
            elapsed_ms: 42,
            cleanup: health,
            shutdown: None,
            log_dropped: 0,
            log_failed: false,
        });
        logger.submit_cleanup(CleanupRecord {
            version: 1,
            event: CleanupEvent::HostShutdown,
            elapsed_ms: 43,
            cleanup: health,
            shutdown: Some(ShutdownOutcome::Failed(failure)),
            log_dropped: 0,
            log_failed: false,
        });
        assert!(logger.shutdown(Duration::from_secs(1)));
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records,
            [
                json!({"version":1,"event":"native_release_failure","elapsed_ms":42,"cleanup":{"completions":9,"failures":3,"last_failure":{"actor_session":7,"sequence":9,"reason":"native_deadline"}},"shutdown":null,"log_dropped":0,"log_failed":false}),
                json!({"version":1,"event":"host_shutdown","elapsed_ms":43,"cleanup":{"completions":9,"failures":3,"last_failure":{"actor_session":7,"sequence":9,"reason":"native_deadline"}},"shutdown":{"outcome":"failed","failure":{"actor_session":7,"sequence":9,"reason":"native_deadline"}},"log_dropped":0,"log_failed":false}),
            ]
        );
        assert!(records.iter().all(|record| record.get("session").is_none()));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!logger.failed());
    }

    #[test]
    fn full_logger_queue_drops_records_without_waiting_for_the_consumer() {
        let (logger, receiver) = DiagnosticsLog::stalled();
        for _ in 0..1000 {
            logger.submit(record(Instant::now()));
        }
        assert_eq!(logger.dropped(), 999);
        assert!(!logger.failed());
        let LogRecord::Peer(record) = receiver.try_recv().unwrap() else {
            panic!("expected peer record")
        };
        assert_eq!(record.session, 3);
    }
    #[test]
    fn owner_only_log_rollover_replaces_the_configured_file_with_complete_jsonl() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("diagnostics.jsonl");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_LOG_BYTES - 1).unwrap();
        drop(file);
        let logger = DiagnosticsLog::start(path.clone());
        logger.submit(record(Instant::now()));
        let mut value = None;
        wait(|| {
            let Ok(bytes) = std::fs::read(&path) else {
                return false;
            };
            if bytes.len() > MAX_RECORD_BYTES || !bytes.ends_with(b"\n") {
                return false;
            }
            value = serde_json::from_slice::<Value>(&bytes).ok();
            value.is_some()
        });
        assert_eq!(value.unwrap()["fault"], "encoder_framing");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!logger.failed());
    }
    #[test]
    fn shutdown_drains_accepted_records_and_rejects_new_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drain.jsonl");
        let logger = DiagnosticsLog::start(path.clone());
        for session in 1..=3 {
            let mut value = record(Instant::now());
            value.session = session;
            logger.submit(value);
        }
        assert!(logger.shutdown(Duration::from_secs(1)));
        let text = std::fs::read_to_string(path).unwrap();
        let sessions: Vec<_> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["session"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(sessions, [1, 2, 3]);
        assert!(!logger.failed());
        logger.submit(record(Instant::now()));
        assert_eq!(logger.dropped(), 1);
    }
    #[test]
    fn shutdown_timeout_is_bounded_and_reports_an_unfinished_writer() {
        let (logger, _receiver) = DiagnosticsLog::stalled();
        logger.submit(record(Instant::now()));
        assert!(!logger.shutdown(Duration::from_millis(5)));
        assert!(logger.failed());
    }

    #[test]
    fn symlink_and_writer_errors_are_safe_failures_without_replacing_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"preserve").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let logger = DiagnosticsLog::start(link);
        wait(|| logger.failed());
        logger.submit(record(Instant::now()));
        assert_eq!(logger.dropped(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        let logger = DiagnosticsLog::start(directory.path().join("missing").join("log"));
        wait(|| logger.failed());
        logger.submit(record(Instant::now()));
        assert_eq!(logger.dropped(), 1);
    }
    #[test]
    fn metrics_measure_maxima_and_rate_limit_normal_records_but_keep_faults() {
        let now = Instant::now();
        let mut diagnostics = PeerDiagnostics::new(now);
        assert!(diagnostics.due(now));
        assert!(!diagnostics.due(now + Duration::from_millis(999)));
        assert!(diagnostics.due(now + Duration::from_secs(1)));
        diagnostics.tick(now + Duration::from_millis(17));
        diagnostics.tick(now + Duration::from_millis(22));
        diagnostics.timeout(
            now + Duration::from_millis(13),
            now + Duration::from_millis(4),
        );
        diagnostics.timeout(
            now + Duration::from_millis(14),
            now + Duration::from_millis(13),
        );
        assert_eq!(diagnostics.tick_gap_max, Duration::from_millis(17));
        assert_eq!(diagnostics.timeout_lateness_max, Duration::from_millis(9));
        diagnostics.fail(FaultReason::ControllerDeadline);
        assert!(diagnostics.due(now + Duration::from_millis(1100)));
        assert!(!diagnostics.due(now + Duration::from_millis(1101)));
    }
}
