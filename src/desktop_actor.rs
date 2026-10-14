use crate::desktop::{
    self, AuthoritySnapshot, Button, Cancellation, CaptureStage, ClipboardGuard, Desktop,
    DesktopError, DesktopGeometry, InputAuthority, InputPermit, KeyCode, LockEffect,
    LockObservation, Position, Transition,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const INPUT_CAPACITY: usize = 128;
const CAPTURE_TIMEOUT_LIMIT: u64 = 3;

pub enum InputAction {
    Key(KeyCode, Transition),
    Pointer(Position),
    Button(Position, Button, Transition),
    Axis(Position, f64, f64),
}
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub geometry: DesktopGeometry,
    pub pixels: Arc<[u8]>,
    pub captured: Instant,
    pub content: u64,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct ActorTimings {
    pub capture_ms: u64,
    pub capture_timeout_sync: u64,
    pub capture_timeout_constraints: u64,
    pub capture_timeout_ready: u64,
    pub capture_timeout_consecutive: u64,
    pub capture_failed_ms: u64,
    pub conversion_ms: u64,
    pub comparison_ms: u64,
    pub input_queue_ms: u64,
    pub lock_query_ms: u64,
    pub lock_query_sequence: u64,
    pub lock_query_outcome: u8,
    pub lock_query_counts: [u64; 8],
    pub lock_observed_us: u64,
    pub lock_query_us: u64,
    pub cancellations: u64,
    pub rejected_inputs: u64,
    pub input_completed: u64,
    pub input_expired: u64,
    pub input_cancelled: u64,
    pub input_failed: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupFault {
    NativeDeadline,
    NativeConnection,
    NativeOperation,
}
impl CleanupFault {
    fn from_error(error: &DesktopError) -> Self {
        match error {
            DesktopError::Timeout => Self::NativeDeadline,
            DesktopError::Connection(_) | DesktopError::Dispatch(_) => Self::NativeConnection,
            _ => Self::NativeOperation,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CleanupFailure {
    pub actor_session: u64,
    pub sequence: u64,
    pub reason: CleanupFault,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CleanupHealth {
    pub completions: u64,
    pub failures: u64,
    pub last_failure: Option<CleanupFailure>,
}
impl CleanupHealth {
    fn completed(&mut self, actor_session: u64, error: Option<&DesktopError>) -> ShutdownOutcome {
        self.completions = self.completions.saturating_add(1);
        match error {
            None => ShutdownOutcome::Clean,
            Some(error) => {
                self.failures = self.failures.saturating_add(1);
                let failure = CleanupFailure {
                    actor_session,
                    sequence: self.completions,
                    reason: CleanupFault::from_error(error),
                };
                self.last_failure = Some(failure);
                ShutdownOutcome::Failed(failure)
            }
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", content = "failure", rename_all = "snake_case")]
pub enum ShutdownOutcome {
    Clean,
    Failed(CleanupFailure),
    Deadline,
}

pub(crate) struct LockUpdate {
    pub observation: LockObservation,
    pub observed_at: Instant,
    pub sequence: u64,
    pub authority: AuthoritySnapshot,
}
pub struct ClipboardReply {
    pub request: u32,
    pub text: Option<String>,
    pub ok: bool,
    pub(crate) guard: ClipboardGuard,
}
pub struct ActorUpdate {
    pub frame: Option<CapturedFrame>,
    pub(crate) lock: Option<LockUpdate>,
    pub clipboard: Option<ClipboardReply>,
    pub fault: Option<DesktopError>,
    pub input_reset: Option<u64>,
    pub timings: ActorTimings,
}
#[derive(Clone)]
struct Activation {
    id: u64,
    authority: InputAuthority,
    fps: u32,
    wake: bool,
}
enum Operation {
    Input(InputAction, InputPermit),
    ClipboardRead(u32, ClipboardGuard),
    ClipboardWrite(u32, String, ClipboardGuard),
}
struct QueuedOperation {
    session: u64,
    queued: Instant,
    operation: Operation,
}
#[derive(Default)]
struct Mailbox {
    desired: Option<Activation>,
    sequence: u64,
    release: bool,
    commands: VecDeque<QueuedOperation>,
    frame: Option<(u64, CapturedFrame)>,
    lock: Option<(u64, LockUpdate)>,
    clipboard: Option<(u64, ClipboardReply)>,
    clipboard_pending: bool,
    fault: Option<(u64, DesktopError)>,
    input_reset: Option<(u64, u64)>,
    timings: ActorTimings,
    stop: bool,
    finished: bool,
    cleanup: CleanupHealth,
    shutdown: Option<ShutdownOutcome>,
}
#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    cancellation: Arc<AtomicU64>,
}
#[derive(Clone)]
pub struct DesktopActor(Arc<Shared>);
pub struct DesktopSession {
    shared: Arc<Shared>,
    id: u64,
    authority: InputAuthority,
}
impl DesktopActor {
    pub fn start(output: String) -> std::io::Result<Self> {
        Self::start_logged(output, None)
    }
    pub(crate) fn start_logged(
        output: String,
        log: Option<Arc<crate::diagnostics::DiagnosticsLog>>,
    ) -> std::io::Result<Self> {
        Self::start_with_logger(
            move |cancel| Desktop::connect_cancellable(&output, cancel),
            log,
        )
    }
    #[cfg(test)]
    fn start_with<B: Backend + 'static>(
        connect: impl FnMut(Cancellation) -> desktop::Result<B> + Send + 'static,
    ) -> std::io::Result<Self> {
        Self::start_with_logger(connect, None)
    }
    fn start_with_logger<B: Backend + 'static>(
        mut connect: impl FnMut(Cancellation) -> desktop::Result<B> + Send + 'static,
        log: Option<Arc<crate::diagnostics::DiagnosticsLog>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        thread::Builder::new()
            .name("live-desktop".into())
            .spawn(move || {
                actor_loop(&worker, &mut connect, log.as_deref());
                let mut mailbox = worker.mailbox.lock().unwrap();
                mailbox.finished = true;
                worker.wake.notify_all();
            })?;
        Ok(Self(shared))
    }
    pub fn activate(&self, fps: u32, wake: bool) -> DesktopSession {
        let authority = InputAuthority::default();
        let mut mailbox = self.0.mailbox.lock().unwrap();
        if let Some(old) = &mailbox.desired {
            old.authority.retire();
        }
        mailbox.sequence = mailbox.sequence.wrapping_add(1);
        let id = mailbox.sequence;
        mailbox.desired = Some(Activation {
            id,
            authority: authority.clone(),
            fps,
            wake,
        });
        mailbox.release = true;
        mailbox.commands.clear();
        mailbox.frame = None;
        mailbox.lock = None;
        mailbox.clipboard = None;
        mailbox.clipboard_pending = false;
        mailbox.fault = None;
        mailbox.input_reset = None;
        self.0.cancellation.fetch_add(1, Ordering::AcqRel);
        self.0.wake.notify_one();
        DesktopSession {
            shared: self.0.clone(),
            id,
            authority,
        }
    }
    pub fn cleanup_health(&self) -> CleanupHealth {
        self.0.mailbox.lock().unwrap().cleanup
    }
    pub fn shutdown(&self, timeout: Duration) -> ShutdownOutcome {
        let mut mailbox = self.0.mailbox.lock().unwrap();
        if let Some(active) = &mailbox.desired {
            active.authority.retire();
        }
        mailbox.stop = true;
        mailbox.release = true;
        self.0.cancellation.fetch_add(1, Ordering::AcqRel);
        self.0.wake.notify_one();
        let (mailbox, _) = self
            .0
            .wake
            .wait_timeout_while(mailbox, timeout, |state| !state.finished)
            .unwrap();
        if mailbox.finished {
            mailbox.shutdown.unwrap_or(ShutdownOutcome::Deadline)
        } else {
            ShutdownOutcome::Deadline
        }
    }
}
impl DesktopSession {
    pub(crate) fn authority_snapshot(&self) -> AuthoritySnapshot {
        self.authority.snapshot()
    }
    pub fn revision(&self) -> u64 {
        self.authority.revision()
    }
    pub fn input_open(&self) -> bool {
        self.authority.is_open()
    }
    pub fn acknowledge_reset(&self, revision: u64) -> bool {
        self.authority.acknowledge(revision)
    }
    pub fn authorize(&self, permit: InputPermit) -> InputPermit {
        permit.authorized_by(&self.authority)
    }
    pub fn authorize_revision(&self, permit: InputPermit, revision: u64) -> InputPermit {
        permit.authorized_by_revision(&self.authority, revision)
    }
    pub fn input(&self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
        if !permit.current_for(&self.authority) {
            return Err(DesktopError::Cancelled);
        }
        self.submit(Operation::Input(action, permit))
    }
    pub fn clipboard_read(&self, request: u32) -> desktop::Result<()> {
        self.submit(Operation::ClipboardRead(
            request,
            self.authority
                .clipboard_guard()
                .ok_or(DesktopError::ClipboardLocked)?,
        ))
    }
    pub fn clipboard_write(&self, request: u32, text: String) -> desktop::Result<()> {
        self.submit(Operation::ClipboardWrite(
            request,
            text,
            self.authority
                .clipboard_guard()
                .ok_or(DesktopError::ClipboardLocked)?,
        ))
    }
    fn submit(&self, operation: Operation) -> desktop::Result<()> {
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if mailbox.stop
            || mailbox
                .desired
                .as_ref()
                .is_none_or(|active| active.id != self.id)
        {
            return Err(DesktopError::Cancelled);
        }
        if mailbox.commands.len() >= INPUT_CAPACITY {
            self.authority.revoke();
            mailbox.commands.clear();
            mailbox.release = true;
            self.shared.cancellation.fetch_add(1, Ordering::AcqRel);
            self.shared.wake.notify_one();
            return Err(DesktopError::InputBacklog);
        }
        if matches!(
            operation,
            Operation::ClipboardRead(_, _) | Operation::ClipboardWrite(_, _, _)
        ) {
            if mailbox.clipboard_pending {
                return Err(DesktopError::ClipboardBusy);
            }
            mailbox.clipboard_pending = true;
        }
        mailbox.commands.push_back(QueuedOperation {
            session: self.id,
            queued: Instant::now(),
            operation,
        });
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn release(&self) {
        self.authority.revoke();
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if mailbox
            .desired
            .as_ref()
            .is_some_and(|active| active.id == self.id)
        {
            mailbox
                .commands
                .retain(|command| !matches!(command.operation, Operation::Input(_, _)));
            if !mailbox.release {
                mailbox.release = true;
                self.shared.cancellation.fetch_add(1, Ordering::AcqRel);
            }
            self.shared.wake.notify_one();
        }
    }
    pub fn set_fps(&self, fps: u32) {
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if let Some(active) = &mut mailbox.desired {
            if active.id == self.id {
                active.fps = fps;
            }
        }
    }
    pub fn poll(&self) -> ActorUpdate {
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        let clipboard = take_session(&mut mailbox.clipboard, self.id);
        if clipboard.is_some() {
            mailbox.clipboard_pending = false;
        }
        ActorUpdate {
            frame: take_session(&mut mailbox.frame, self.id),
            lock: take_session(&mut mailbox.lock, self.id),
            clipboard,
            fault: take_session(&mut mailbox.fault, self.id),
            input_reset: take_session(&mut mailbox.input_reset, self.id),
            timings: mailbox.timings,
        }
    }
}
impl Drop for DesktopSession {
    fn drop(&mut self) {
        self.authority.retire();
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if mailbox
            .desired
            .as_ref()
            .is_some_and(|active| active.id == self.id)
        {
            mailbox.desired = None;
            mailbox.commands.clear();
            mailbox.release = true;
            mailbox.frame = None;
            mailbox.clipboard = None;
            mailbox.clipboard_pending = false;
            self.shared.cancellation.fetch_add(1, Ordering::AcqRel);
            self.shared.wake.notify_one();
        }
    }
}
fn take_session<T>(slot: &mut Option<(u64, T)>, session: u64) -> Option<T> {
    if slot.as_ref().is_some_and(|(id, _)| *id == session) {
        slot.take().map(|(_, value)| value)
    } else {
        None
    }
}
trait Backend {
    fn lock(&mut self, cancellation: &Cancellation) -> LockObservation {
        desktop::observe_lock_cancellable(Duration::from_millis(150), cancellation)
    }
    fn cancellation(&mut self, cancellation: Cancellation);
    fn release(&mut self) -> desktop::Result<()>;
    fn begin_capture(&mut self) -> desktop::Result<()>;
    fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll>;
    fn abort_capture(&mut self);
    fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()>;
    fn begin_input(
        &mut self,
        action: InputAction,
        permit: InputPermit,
    ) -> desktop::Result<desktop::InputPoll> {
        self.input(action, permit)
            .map(|()| desktop::InputPoll::Complete)
    }
    fn poll_input(&mut self) -> desktop::Result<desktop::InputPoll> {
        Ok(desktop::InputPoll::Complete)
    }
    fn abort_input(&mut self) {}
    fn read(&mut self) -> desktop::Result<String>;
    fn write(&mut self, text: &str) -> desktop::Result<()>;
}
impl Backend for Desktop {
    fn cancellation(&mut self, cancellation: Cancellation) {
        self.set_cancellation(cancellation);
    }
    fn release(&mut self) -> desktop::Result<()> {
        self.release_all()
    }
    fn begin_capture(&mut self) -> desktop::Result<()> {
        self.begin_capture(Duration::from_millis(120))
    }
    fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
        self.poll_capture()
    }
    fn abort_capture(&mut self) {
        self.abort_capture();
    }
    fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
        match action {
            InputAction::Key(key, transition) => self.key(key, transition, permit),
            InputAction::Pointer(position) => self.pointer_absolute(position, permit),
            InputAction::Button(position, button, transition) => {
                self.button(position, button, transition, permit)
            }
            InputAction::Axis(position, dx, dy) => self.axis(position, dx, dy, permit),
        }
    }
    fn begin_input(
        &mut self,
        action: InputAction,
        permit: InputPermit,
    ) -> desktop::Result<desktop::InputPoll> {
        match action {
            InputAction::Button(position, button, transition) => {
                self.begin_button(position, button, transition, permit)?;
                Ok(desktop::InputPoll::Pending)
            }
            other => self
                .input(other, permit)
                .map(|()| desktop::InputPoll::Complete),
        }
    }
    fn poll_input(&mut self) -> desktop::Result<desktop::InputPoll> {
        Desktop::poll_input(self)
    }
    fn abort_input(&mut self) {
        Desktop::abort_input(self);
    }
    fn read(&mut self) -> desktop::Result<String> {
        self.clipboard_read()
    }
    fn write(&mut self, text: &str) -> desktop::Result<()> {
        self.clipboard_write(text)
    }
}
fn actor_loop<B: Backend>(
    shared: &Shared,
    mut connect: impl FnMut(Cancellation) -> desktop::Result<B>,
    log: Option<&crate::diagnostics::DiagnosticsLog>,
) {
    let mut desktop: Option<B> = None;
    let mut input_pending = false;
    let mut session = 0;
    let mut invalid = false;
    let mut previous: Option<Arc<[u8]>> = None;
    let mut content = 0u64;
    let mut next_capture = Instant::now();
    let mut capture_started: Option<Instant> = None;
    let mut next_lock = Instant::now();
    let mut capture_timeouts = 0u64;
    let started = Instant::now();
    loop {
        let (active, release, stop, cancellation) = {
            let mut mailbox = shared.mailbox.lock().unwrap();
            (
                mailbox.desired.clone(),
                std::mem::take(&mut mailbox.release),
                mailbox.stop,
                Cancellation::new(shared.cancellation.clone()),
            )
        };
        if release {
            if let Some(desktop) = desktop.as_mut() {
                desktop.abort_input();
                input_pending = false;
                desktop.abort_capture();
            }
            capture_started = None;
            let released_session = if desktop.is_some() {
                session
            } else {
                active.as_ref().map_or(session, |active| active.id)
            };
            let result = desktop.as_mut().map_or(Ok(()), Backend::release);
            {
                let mut mailbox = shared.mailbox.lock().unwrap();
                let outcome = mailbox
                    .cleanup
                    .completed(released_session, result.as_ref().err());
                if stop {
                    mailbox.shutdown = Some(outcome);
                }
            }
            match result {
                Ok(()) => {
                    if let Some(active) = &active {
                        if let Some(revision) = active.authority.released() {
                            let mut mailbox = shared.mailbox.lock().unwrap();
                            if current(&mailbox, active.id) {
                                mailbox.input_reset = Some((active.id, revision));
                            }
                        }
                    }
                }
                Err(error) => {
                    let id = active.as_ref().map_or(session, |active| active.id);
                    let mut mailbox = shared.mailbox.lock().unwrap();
                    if current(&mailbox, id)
                        || (mailbox.desired.is_none() && mailbox.sequence == id)
                    {
                        mailbox.fault = Some((id, error));
                        if let Some(active) = &mailbox.desired {
                            active.authority.close();
                        }
                        mailbox.desired = None;
                        mailbox.commands.clear();
                    }
                    drop(mailbox);
                    desktop = None;
                    if !stop {
                        continue;
                    }
                }
            }
        }
        if invalid {
            capture_started = None;
            desktop = None;
            invalid = false;
        }
        if stop {
            break;
        }
        let Some(active) = active else {
            let mailbox = shared.mailbox.lock().unwrap();
            let _ = shared
                .wake
                .wait_timeout(mailbox, Duration::from_millis(10))
                .unwrap();
            continue;
        };
        if session != active.id {
            if let Some(desktop) = desktop.as_mut() {
                desktop.abort_input();
                input_pending = false;
                desktop.abort_capture();
            }
            capture_started = None;
            if active.wake {
                match wake_monitor(&cancellation) {
                    Ok(()) => {}
                    Err(DesktopError::Cancelled) => {
                        record_cancel(shared);
                        continue;
                    }
                    Err(error) => {
                        publish_fault(shared, active.id, error);
                        continue;
                    }
                }
            }
            session = active.id;
            previous = None;
            next_capture = Instant::now();
            next_lock = Instant::now();
            capture_timeouts = 0;
            shared
                .mailbox
                .lock()
                .unwrap()
                .timings
                .capture_timeout_consecutive = 0;
        }
        if desktop.is_none() {
            match connect(cancellation.clone()) {
                Ok(connected) => desktop = Some(connected),
                Err(DesktopError::Cancelled) => {
                    record_cancel(shared);
                    continue;
                }
                Err(error) => {
                    publish_fault(shared, session, error);
                    continue;
                }
            }
        }
        let desktop = desktop.as_mut().unwrap();
        desktop.cancellation(cancellation.clone());
        if capture_started.is_none() && Instant::now() >= next_lock {
            let observation = desktop.lock(&cancellation);
            let observed_at = Instant::now();
            let mut mailbox = shared.mailbox.lock().unwrap();
            let discarded = !current(&mailbox, session) || cancellation.check().is_err();
            let before = active.authority.snapshot();
            let effect = if discarded {
                LockEffect::None
            } else {
                active.authority.observe_lock(observation)
            };
            let authority = active.authority.snapshot();
            let timings = &mut mailbox.timings;
            timings.lock_query_ms = observation.elapsed_us / 1_000;
            timings.lock_query_us = observation.elapsed_us;
            timings.lock_query_sequence = timings.lock_query_sequence.saturating_add(1);
            timings.lock_query_outcome = observation.outcome_code();
            let count = &mut timings.lock_query_counts[observation.outcome_code() as usize];
            *count = count.saturating_add(1);
            timings.lock_observed_us = observed_at
                .saturating_duration_since(started)
                .as_micros()
                .min(u64::MAX as u128) as u64;
            let sequence = timings.lock_query_sequence;
            if let Some(log) = log {
                log.submit_native_lock(crate::diagnostics::NativeLockRecord {
                    version: 1,
                    actor_session: session,
                    lock_sequence: sequence,
                    native_lock_event: if discarded {
                        5
                    } else if effect == LockEffect::DesktopResetRequired {
                        4
                    } else if observation.outcome.is_err() {
                        2
                    } else if matches!(before.lock, desktop::LockKnowledge::Unavailable { .. })
                        && matches!(authority.lock, desktop::LockKnowledge::Verified(_))
                    {
                        3
                    } else {
                        1
                    },
                    lock_outcome: observation.outcome_code(),
                    query_elapsed_us: observation.elapsed_us,
                    observed_elapsed_us: timings.lock_observed_us,
                    authority_phase: authority.phase as u8,
                    input_revision: authority.revision,
                    lock_knowledge: authority.lock.code(),
                    last_known: authority.lock.last_known().map(|known| known as u8),
                    transition: matches!(authority.lock, desktop::LockKnowledge::Changed { .. }),
                    lock_generation: authority.lock_generation,
                    query_counts: timings.lock_query_counts,
                    discarded,
                    log_dropped: 0,
                    log_failed: false,
                });
            }
            if discarded {
                continue;
            }
            mailbox.lock = Some((
                session,
                LockUpdate {
                    observation,
                    observed_at,
                    sequence,
                    authority,
                },
            ));
            next_lock = Instant::now() + Duration::from_millis(250);
            if effect != LockEffect::None {
                let denied = mailbox
                    .commands
                    .iter()
                    .find_map(|command| match &command.operation {
                        Operation::ClipboardRead(request, guard)
                        | Operation::ClipboardWrite(request, _, guard) => Some(ClipboardReply {
                            request: *request,
                            text: None,
                            ok: false,
                            guard: guard.clone(),
                        }),
                        Operation::Input(_, _) => None,
                    });
                if let Some((_, reply)) = mailbox.clipboard.as_mut() {
                    reply.text = None;
                    reply.ok = false;
                } else if let Some(reply) = denied {
                    mailbox.clipboard = Some((session, reply));
                }
                mailbox.commands.clear();
                mailbox.clipboard_pending = mailbox.clipboard.is_some();
                mailbox.release = true;
                shared.cancellation.fetch_add(1, Ordering::AcqRel);
                shared.wake.notify_one();
                continue;
            }
        }
        let input_began = Instant::now();
        for _ in 0..8 {
            if input_began.elapsed() >= Duration::from_millis(8) || cancellation.check().is_err() {
                break;
            }
            let pending_result = if input_pending {
                match desktop.poll_input() {
                    Ok(desktop::InputPoll::Pending) => break,
                    Ok(desktop::InputPoll::Complete) => Some(Ok(())),
                    Err(error) => Some(Err(error)),
                }
            } else {
                None
            };
            let command = if pending_result.is_none() {
                shared.mailbox.lock().unwrap().commands.pop_front()
            } else {
                None
            };
            if pending_result.is_none() && command.is_none() {
                break;
            }
            let result = if let Some(result) = pending_result {
                input_pending = false;
                record_input_result(shared, &result);
                result
            } else {
                let command = command.unwrap();
                if command.session != session {
                    continue;
                }
                shared.mailbox.lock().unwrap().timings.input_queue_ms =
                    command.queued.elapsed().as_millis() as u64;
                match command.operation {
                    Operation::Input(action, permit) => {
                        let result = if permit.current_for(&active.authority) {
                            desktop.begin_input(action, permit)
                        } else {
                            Err(DesktopError::Cancelled)
                        };
                        let result = match result {
                            Ok(desktop::InputPoll::Pending) => {
                                input_pending = true;
                                break;
                            }
                            Ok(desktop::InputPoll::Complete) => Ok(()),
                            Err(error) => Err(error),
                        };
                        record_input_result(shared, &result);
                        result
                    }
                    Operation::ClipboardRead(request, guard) => {
                        let text = guard.current().then(|| desktop.read().ok()).flatten();
                        let text = text.filter(|_| guard.current());
                        publish_clipboard(
                            shared,
                            session,
                            ClipboardReply {
                                request,
                                ok: text.is_some(),
                                text,
                                guard,
                            },
                        );
                        Ok(())
                    }
                    Operation::ClipboardWrite(request, text, guard) => {
                        let ok = guard.current() && desktop.write(&text).is_ok() && guard.current();
                        publish_clipboard(
                            shared,
                            session,
                            ClipboardReply {
                                request,
                                text: None,
                                ok,
                                guard,
                            },
                        );
                        Ok(())
                    }
                }
            };
            match result {
                Ok(()) => {}
                Err(DesktopError::InputExpired | DesktopError::PointerPreparation(_)) => {
                    active.authority.close();
                    let mut mailbox = shared.mailbox.lock().unwrap();
                    mailbox.timings.rejected_inputs += 1;
                    if current(&mailbox, session) {
                        mailbox
                            .commands
                            .retain(|command| !matches!(command.operation, Operation::Input(_, _)));
                        mailbox.release = true;
                        shared.cancellation.fetch_add(1, Ordering::AcqRel);
                        shared.wake.notify_one();
                    }
                    break;
                }
                Err(DesktopError::Cancelled) => record_cancel(shared),
                Err(error) => {
                    publish_fault(shared, session, error);
                    invalid = true;
                    break;
                }
            }
        }
        if invalid {
            continue;
        }
        if cancellation.check().is_err() {
            continue;
        }
        let capture_result = if capture_started.is_none() && Instant::now() >= next_capture {
            capture_started = Some(Instant::now());
            desktop
                .begin_capture()
                .and_then(|()| desktop.poll_capture())
        } else if capture_started.is_some() {
            desktop.poll_capture()
        } else {
            Ok(desktop::CapturePoll::Idle)
        };
        let frame = match capture_result {
            Ok(desktop::CapturePoll::Complete(frame)) => Some(Ok(frame)),
            Err(error) => Some(Err(error)),
            Ok(desktop::CapturePoll::Pending | desktop::CapturePoll::Idle) => None,
        };
        if let Some(frame) = frame {
            let began = capture_started.take().unwrap();
            let capture_ms = began.elapsed().as_millis() as u64;
            if frame.is_err() {
                shared.mailbox.lock().unwrap().timings.capture_failed_ms = capture_ms;
            }
            match frame {
                Ok(frame) => {
                    capture_timeouts = 0;
                    shared
                        .mailbox
                        .lock()
                        .unwrap()
                        .timings
                        .capture_timeout_consecutive = 0;
                    let captured = frame.ready_at;
                    let began = Instant::now();
                    let pixels = frame.rgba().map(Arc::<[u8]>::from);
                    let conversion_ms = began.elapsed().as_millis() as u64;
                    match pixels {
                        Ok(pixels) => {
                            let began = Instant::now();
                            if previous.as_ref().is_none_or(|old| **old != *pixels) {
                                content = content.wrapping_add(1);
                            }
                            let comparison_ms = began.elapsed().as_millis() as u64;
                            previous = Some(pixels.clone());
                            let mut mailbox = shared.mailbox.lock().unwrap();
                            if current(&mailbox, session) && cancellation.check().is_ok() {
                                mailbox.frame = Some((
                                    session,
                                    CapturedFrame {
                                        width: frame.width,
                                        height: frame.height,
                                        geometry: frame.geometry,
                                        pixels,
                                        captured,
                                        content,
                                    },
                                ));
                                mailbox.timings.capture_ms = capture_ms;
                                mailbox.timings.conversion_ms = conversion_ms;
                                mailbox.timings.comparison_ms = comparison_ms;
                            }
                        }
                        Err(error) => {
                            publish_fault(shared, session, error);
                            invalid = true;
                        }
                    }
                }
                Err(DesktopError::Cancelled) => record_cancel(shared),
                Err(DesktopError::CaptureTimeout(stage)) => {
                    capture_timeouts = capture_timeouts.saturating_add(1);
                    {
                        let mut mailbox = shared.mailbox.lock().unwrap();
                        let timings = &mut mailbox.timings;
                        let count = match stage {
                            CaptureStage::Synchronize => &mut timings.capture_timeout_sync,
                            CaptureStage::Constraints => &mut timings.capture_timeout_constraints,
                            CaptureStage::Ready => &mut timings.capture_timeout_ready,
                        };
                        *count = count.saturating_add(1);
                        timings.capture_timeout_consecutive = capture_timeouts;
                    }
                    if capture_timeouts >= CAPTURE_TIMEOUT_LIMIT {
                        publish_fault(shared, session, DesktopError::CaptureTimeout(stage));
                        invalid = true;
                    }
                }
                Err(error) => {
                    publish_fault(shared, session, error);
                    invalid = true;
                }
            }
            next_capture = began + Duration::from_secs_f64(1.0 / active.fps.max(1) as f64);
        }
        let mailbox = shared.mailbox.lock().unwrap();
        let wait = if input_pending {
            Duration::from_millis(2)
        } else if !mailbox.commands.is_empty() {
            Duration::ZERO
        } else if capture_started.is_some() {
            Duration::from_millis(2)
        } else {
            next_capture
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5))
        };
        let _ = shared.wake.wait_timeout(mailbox, wait).unwrap();
    }
}
fn record_input_result(shared: &Shared, result: &desktop::Result<()>) {
    let mut mailbox = shared.mailbox.lock().unwrap();
    let count = match result {
        Ok(()) => &mut mailbox.timings.input_completed,
        Err(DesktopError::InputExpired) => &mut mailbox.timings.input_expired,
        Err(DesktopError::Cancelled) => &mut mailbox.timings.input_cancelled,
        Err(_) => &mut mailbox.timings.input_failed,
    };
    *count = count.saturating_add(1);
}
fn current(mailbox: &Mailbox, session: u64) -> bool {
    !mailbox.stop
        && mailbox
            .desired
            .as_ref()
            .is_some_and(|active| active.id == session)
}
fn record_cancel(shared: &Shared) {
    shared.mailbox.lock().unwrap().timings.cancellations += 1;
}
fn publish_fault(shared: &Shared, session: u64, error: DesktopError) {
    let mut mailbox = shared.mailbox.lock().unwrap();
    if current(&mailbox, session) {
        if let Some(active) = &mailbox.desired {
            active.authority.revoke();
        }
        mailbox.fault = Some((session, error));
        mailbox.desired = None;
        mailbox.commands.clear();
        mailbox.release = true;
        shared.cancellation.fetch_add(1, Ordering::AcqRel);
    }
}
fn publish_clipboard(shared: &Shared, session: u64, reply: ClipboardReply) {
    let mut mailbox = shared.mailbox.lock().unwrap();
    if current(&mailbox, session) {
        let mut reply = reply;
        if !reply.guard.current() {
            reply.text = None;
            reply.ok = false;
        }
        mailbox.clipboard = Some((session, reply));
    }
}
fn wake_monitor(cancellation: &Cancellation) -> desktop::Result<()> {
    let mut child = std::process::Command::new("omarchy")
        .args(["system", "wake"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Err(error) = cancellation.check() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DesktopError::Timeout);
        }
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(DesktopError::Timeout)
            };
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{Frame, PixelFormat};
    use std::sync::{Barrier, atomic::AtomicUsize};

    struct Fake {
        cancel: Cancellation,
        frames: Arc<AtomicUsize>,
        inputs: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
        capture_barrier: Option<Arc<Barrier>>,
        pause_after: usize,
        reject_input: bool,
        fail_capture: bool,
    }
    impl Backend for Fake {
        fn lock(&mut self, _: &Cancellation) -> LockObservation {
            LockObservation {
                outcome: Ok(desktop::KnownLock::Unlocked),
                elapsed_us: 0,
            }
        }
        fn cancellation(&mut self, cancellation: Cancellation) {
            self.cancel = cancellation;
        }
        fn release(&mut self) -> desktop::Result<()> {
            self.releases.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn begin_capture(&mut self) -> desktop::Result<()> {
            Ok(())
        }
        fn abort_capture(&mut self) {}
        fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
            if self.frames.load(Ordering::Acquire) >= self.pause_after {
                if let Some(barrier) = self.capture_barrier.take() {
                    barrier.wait();
                    barrier.wait();
                }
            }
            self.cancel.check()?;
            if self.fail_capture {
                self.fail_capture = false;
                return Err(DesktopError::OutputUnavailable);
            }
            let value = self.frames.fetch_add(1, Ordering::AcqRel) + 1;
            Ok(desktop::CapturePoll::Complete(Frame {
                ready_at: Instant::now(),
                geometry: DesktopGeometry {
                    output: "fixture".into(),
                    width: 1,
                    height: 1,
                    scale: 1,
                    transform: 0,
                    generation: 1,
                },
                width: 1,
                height: 1,
                stride: 4,
                format: PixelFormat::Rgba,
                y_inverted: false,
                opaque: true,
                bytes: vec![value as u8, 0, 0, 255],
            }))
        }
        fn input(&mut self, _: InputAction, _: InputPermit) -> desktop::Result<()> {
            self.cancel.check()?;
            thread::sleep(Duration::from_millis(2));
            if self.reject_input {
                self.reject_input = false;
                return Err(DesktopError::InputExpired);
            }
            self.inputs.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn read(&mut self) -> desktop::Result<String> {
            Ok("fixture".into())
        }
        fn write(&mut self, _: &str) -> desktop::Result<()> {
            Ok(())
        }
    }
    fn fixture(
        barrier: Option<Arc<Barrier>>,
    ) -> (
        DesktopActor,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        fixture_after(barrier, 0)
    }
    fn fixture_after(
        barrier: Option<Arc<Barrier>>,
        pause_after: usize,
    ) -> (
        DesktopActor,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let frames = Arc::new(AtomicUsize::new(0));
        let inputs = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(AtomicUsize::new(0));
        let (worker_frames, worker_inputs, worker_releases) =
            (frames.clone(), inputs.clone(), releases.clone());
        let actor = DesktopActor::start_with(move |cancel| {
            Ok(Fake {
                cancel,
                frames: worker_frames.clone(),
                inputs: worker_inputs.clone(),
                releases: worker_releases.clone(),
                capture_barrier: barrier.clone(),
                pause_after,
                reject_input: false,
                fail_capture: false,
            })
        })
        .unwrap();
        (actor, frames, inputs, releases)
    }
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "actor did not reach expected state"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn press(session: &DesktopSession) -> desktop::Result<()> {
        if session.authority.snapshot().lock == desktop::LockKnowledge::Unobserved {
            session.authority.observe_lock(LockObservation {
                outcome: Ok(desktop::KnownLock::Unlocked),
                elapsed_us: 0,
            });
        }
        let permit =
            session.authorize(InputPermit::new(1, Instant::now() + Duration::from_secs(1)));
        session.input(
            InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
            permit,
        )
    }

    #[test]
    fn saturated_input_mailbox_cannot_prevent_out_of_band_release() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, inputs, releases) = fixture(Some(barrier.clone()));
        let session = actor.activate(30, false);
        barrier.wait();
        for _ in 0..INPUT_CAPACITY {
            press(&session).unwrap();
        }
        assert!(matches!(press(&session), Err(DesktopError::InputBacklog)));
        session.release();
        assert!(actor.0.mailbox.lock().unwrap().commands.is_empty());
        barrier.wait();
        wait_until(|| releases.load(Ordering::Acquire) == 1);
        assert_eq!(inputs.load(Ordering::Acquire), 0);
        drop(session);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn takeover_discards_old_queued_inputs_and_stale_session_drop() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, inputs, releases) = fixture(Some(barrier.clone()));
        let old = actor.activate(30, false);
        barrier.wait();
        press(&old).unwrap();
        let new = actor.activate(30, false);
        assert!(matches!(press(&old), Err(DesktopError::Cancelled)));
        drop(old);
        press(&new).unwrap();
        barrier.wait();
        wait_until(|| inputs.load(Ordering::Acquire) == 1);
        assert_eq!(inputs.load(Ordering::Acquire), 1);
        assert_eq!(releases.load(Ordering::Acquire), 1);
        drop(new);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn latest_frame_slot_has_no_backlog_for_a_delayed_consumer() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, _, _) = fixture_after(Some(barrier.clone()), 4);
        let session = actor.activate(100, false);
        barrier.wait();
        let update = session.poll();
        assert_eq!(update.frame.unwrap().content, 4);
        assert!(session.poll().frame.is_none());
        drop(session);
        barrier.wait();
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn fatal_output_fault_reconnects_before_a_replacement_session_captures() {
        let connections = Arc::new(AtomicUsize::new(0));
        let worker_connections = connections.clone();
        let actor = DesktopActor::start_with(move |cancel| {
            let first = worker_connections.fetch_add(1, Ordering::AcqRel) == 0;
            Ok(Fake {
                cancel,
                frames: Arc::new(AtomicUsize::new(0)),
                inputs: Arc::new(AtomicUsize::new(0)),
                releases: Arc::new(AtomicUsize::new(0)),
                capture_barrier: None,
                pause_after: 0,
                reject_input: false,
                fail_capture: first,
            })
        })
        .unwrap();
        let old = actor.activate(30, false);
        let mut fault = None;
        wait_until(|| {
            fault = old.poll().fault;
            fault.is_some()
        });
        assert!(matches!(fault, Some(DesktopError::OutputUnavailable)));
        drop(old);
        let new = actor.activate(30, false);
        let mut frame = None;
        wait_until(|| {
            frame = new.poll().frame;
            frame.is_some()
        });
        assert_eq!(frame.unwrap().content, 1);
        assert_eq!(connections.load(Ordering::Acquire), 2);
        drop(new);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    struct LockBackend {
        base: Fake,
        outcome: Arc<Mutex<std::result::Result<desktop::KnownLock, desktop::LockQueryFailure>>>,
        held: Arc<Mutex<[bool; 2]>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        cleanup_fail: Arc<std::sync::atomic::AtomicBool>,
        query_barrier: Arc<Mutex<Option<Arc<Barrier>>>>,
    }
    impl Backend for LockBackend {
        fn lock(&mut self, _: &Cancellation) -> LockObservation {
            let outcome = *self.outcome.lock().unwrap();
            let barrier = self.query_barrier.lock().unwrap().take();
            if let Some(barrier) = barrier {
                barrier.wait();
                barrier.wait();
            }
            LockObservation {
                outcome,
                elapsed_us: 150_000,
            }
        }
        fn cancellation(&mut self, cancellation: Cancellation) {
            self.base.cancellation(cancellation);
        }
        fn begin_capture(&mut self) -> desktop::Result<()> {
            self.base.begin_capture()
        }
        fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
            self.base.poll_capture()
        }
        fn abort_capture(&mut self) {
            self.base.abort_capture();
        }
        fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
            permit.emit(1, || {
                match action {
                    InputAction::Key(key, Transition::Press)
                        if key == KeyCode::new(42).unwrap() =>
                    {
                        self.held.lock().unwrap()[0] = true;
                        self.events.lock().unwrap().push("shift down");
                    }
                    InputAction::Button(_, Button::Left, Transition::Press) => {
                        self.held.lock().unwrap()[1] = true;
                        self.events.lock().unwrap().push("button down");
                    }
                    InputAction::Key(key, Transition::Press)
                        if key == KeyCode::new(30).unwrap() =>
                    {
                        self.events.lock().unwrap().push("a down")
                    }
                    _ => {}
                }
                Ok(())
            })
        }
        fn release(&mut self) -> desktop::Result<()> {
            let mut held = self.held.lock().unwrap();
            if held[0] {
                self.events.lock().unwrap().push("shift up");
            }
            if held[1] {
                self.events.lock().unwrap().push("button up");
            }
            *held = [false; 2];
            if self.cleanup_fail.load(Ordering::Acquire) {
                Err(DesktopError::Timeout)
            } else {
                Ok(())
            }
        }
        fn read(&mut self) -> desktop::Result<String> {
            self.events.lock().unwrap().push("clipboard read");
            Ok("fixture text".into())
        }
        fn write(&mut self, _: &str) -> desktop::Result<()> {
            self.events.lock().unwrap().push("clipboard write");
            Ok(())
        }
    }
    struct LockFixture {
        actor: DesktopActor,
        outcome: Arc<Mutex<std::result::Result<desktop::KnownLock, desktop::LockQueryFailure>>>,
        held: Arc<Mutex<[bool; 2]>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        cleanup_fail: Arc<std::sync::atomic::AtomicBool>,
        query_barrier: Arc<Mutex<Option<Arc<Barrier>>>>,
    }
    fn lock_fixture() -> LockFixture {
        let outcome = Arc::new(Mutex::new(Ok(desktop::KnownLock::Unlocked)));
        let held = Arc::new(Mutex::new([false; 2]));
        let events = Arc::new(Mutex::new(Vec::new()));
        let cleanup_fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let query_barrier = Arc::new(Mutex::new(None));
        let (worker_outcome, worker_held, worker_events, worker_fail, worker_barrier) = (
            outcome.clone(),
            held.clone(),
            events.clone(),
            cleanup_fail.clone(),
            query_barrier.clone(),
        );
        let actor = DesktopActor::start_with(move |cancel| {
            Ok(LockBackend {
                base: Fake {
                    cancel,
                    frames: Arc::new(AtomicUsize::new(0)),
                    inputs: Arc::new(AtomicUsize::new(0)),
                    releases: Arc::new(AtomicUsize::new(0)),
                    capture_barrier: None,
                    pause_after: 0,
                    reject_input: false,
                    fail_capture: false,
                },
                outcome: worker_outcome.clone(),
                held: worker_held.clone(),
                events: worker_events.clone(),
                cleanup_fail: worker_fail.clone(),
                query_barrier: worker_barrier.clone(),
            })
        })
        .unwrap();
        LockFixture {
            actor,
            outcome,
            held,
            events,
            cleanup_fail,
            query_barrier,
        }
    }
    fn fresh_native_permit(session: &DesktopSession) -> InputPermit {
        session.authorize(InputPermit::new(1, Instant::now() + Duration::from_secs(2)))
    }
    #[test]
    fn unavailable_native_query_releases_holds_once_and_ack_cannot_emit_until_recovery() {
        let fixture = lock_fixture();
        let session = fixture.actor.activate(30, false);
        wait_until(|| session.input_open());
        let old = fresh_native_permit(&session);
        session
            .input(
                InputAction::Key(KeyCode::new(42).unwrap(), Transition::Press),
                fresh_native_permit(&session),
            )
            .unwrap();
        session
            .input(
                InputAction::Button(
                    Position::new(0.5, 0.5).unwrap(),
                    Button::Left,
                    Transition::Press,
                ),
                fresh_native_permit(&session),
            )
            .unwrap();
        wait_until(|| *fixture.held.lock().unwrap() == [true, true]);
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        let mut revision = None;
        wait_until(|| {
            revision = session.poll().input_reset;
            revision.is_some()
        });
        assert_eq!(revision, Some(1));
        assert_eq!(*fixture.held.lock().unwrap(), [false, false]);
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["shift down", "button down", "shift up", "button up"]
        );
        assert!(matches!(
            session.input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                old
            ),
            Err(DesktopError::Cancelled)
        ));
        assert!(!session.acknowledge_reset(0));
        assert!(session.acknowledge_reset(1));
        assert!(!session.acknowledge_reset(1));
        assert_eq!(
            session.authority_snapshot().phase,
            desktop::AuthorityPhase::Suspended
        );
        assert!(matches!(
            session.input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&session)
            ),
            Err(DesktopError::Cancelled)
        ));
        assert!(matches!(
            session.clipboard_read(11),
            Err(DesktopError::ClipboardLocked)
        ));
        wait_until(|| {
            fixture
                .actor
                .0
                .mailbox
                .lock()
                .unwrap()
                .timings
                .lock_query_counts[desktop::LockQueryFailure::Deadline as usize]
                >= 2
        });
        assert_eq!(session.revision(), 1);
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["shift down", "button down", "shift up", "button up"]
        );
        *fixture.outcome.lock().unwrap() = Ok(desktop::KnownLock::Unlocked);
        wait_until(|| session.input_open());
        session
            .input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&session),
            )
            .unwrap();
        wait_until(|| fixture.events.lock().unwrap().len() == 5);
        assert_eq!(
            *fixture.events.lock().unwrap(),
            [
                "shift down",
                "button down",
                "shift up",
                "button up",
                "a down"
            ]
        );
        session.release();
        wait_until(|| session.poll().input_reset.is_some());
        session.clipboard_read(12).unwrap();
        let mut reply = None;
        wait_until(|| {
            reply = session.poll().clipboard;
            reply.is_some()
        });
        let reply = reply.unwrap();
        assert_eq!(
            (reply.request, reply.ok, reply.text.as_deref()),
            (12, true, Some("fixture text"))
        );
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn actual_transition_after_unknown_is_sticky_before_peer_consumption_and_ack() {
        let fixture = lock_fixture();
        let session = fixture.actor.activate(30, false);
        wait_until(|| session.input_open());
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        wait_until(|| {
            session.authority_snapshot().phase == desktop::AuthorityPhase::AwaitingAcknowledgement
        });
        assert!(session.acknowledge_reset(1));
        *fixture.outcome.lock().unwrap() = Ok(desktop::KnownLock::Locked);
        wait_until(|| {
            matches!(
                session.authority_snapshot().lock,
                desktop::LockKnowledge::Changed { .. }
            )
        });
        wait_until(|| {
            session.authority_snapshot().phase == desktop::AuthorityPhase::AwaitingAcknowledgement
        });
        assert!(session.acknowledge_reset(2));
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        wait_until(|| {
            fixture
                .actor
                .0
                .mailbox
                .lock()
                .unwrap()
                .timings
                .lock_query_counts[desktop::LockQueryFailure::Deadline as usize]
                >= 2
        });
        assert!(matches!(
            session.input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&session)
            ),
            Err(DesktopError::Cancelled)
        ));
        assert_eq!(
            session.authority_snapshot().lock,
            desktop::LockKnowledge::Changed {
                previous: desktop::KnownLock::Unlocked,
                current: desktop::KnownLock::Locked
            }
        );
        let update = session.poll().lock.unwrap();
        assert_eq!(
            update.authority.lock,
            desktop::LockKnowledge::Changed {
                previous: desktop::KnownLock::Unlocked,
                current: desktop::KnownLock::Locked
            }
        );
        drop(session);
        let replacement = fixture.actor.activate(30, false);
        *fixture.outcome.lock().unwrap() = Ok(desktop::KnownLock::Locked);
        wait_until(|| replacement.input_open());
        replacement
            .input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&replacement),
            )
            .unwrap();
        wait_until(|| fixture.events.lock().unwrap().len() == 1);
        assert_eq!(*fixture.events.lock().unwrap(), ["a down"]);
        assert!(matches!(
            replacement.clipboard_read(3),
            Err(DesktopError::ClipboardLocked)
        ));
        drop(replacement);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn queued_key_and_clipboard_are_denied_when_query_finishes_unknown() {
        let fixture = lock_fixture();
        let session = fixture.actor.activate(30, false);
        wait_until(|| session.input_open());
        let barrier = Arc::new(Barrier::new(2));
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        *fixture.query_barrier.lock().unwrap() = Some(barrier.clone());
        barrier.wait();
        session
            .input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&session),
            )
            .unwrap();
        session.clipboard_read(21).unwrap();
        barrier.wait();
        let mut revision = None;
        let mut reply = None;
        wait_until(|| {
            let update = session.poll();
            revision = update.input_reset.or(revision);
            reply = update.clipboard.or(reply.take());
            revision.is_some() && reply.is_some()
        });
        let reply = reply.unwrap();
        assert_eq!((reply.request, reply.ok, reply.text), (21, false, None));
        assert_eq!(revision, Some(1));
        assert!(fixture.events.lock().unwrap().is_empty());
        assert!(session.acknowledge_reset(1));
        *fixture.outcome.lock().unwrap() = Ok(desktop::KnownLock::Unlocked);
        wait_until(|| session.input_open());
        session
            .input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&session),
            )
            .unwrap();
        session.clipboard_read(22).unwrap();
        wait_until(|| fixture.events.lock().unwrap().len() == 2);
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["a down", "clipboard read"]
        );
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn lock_suspension_cleanup_failure_invalidates_native_backend() {
        let fixture = lock_fixture();
        let session = fixture.actor.activate(30, false);
        wait_until(|| session.input_open());
        session
            .input(
                InputAction::Key(KeyCode::new(42).unwrap(), Transition::Press),
                fresh_native_permit(&session),
            )
            .unwrap();
        wait_until(|| fixture.held.lock().unwrap()[0]);
        fixture.cleanup_fail.store(true, Ordering::Release);
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        let mut fault = None;
        wait_until(|| {
            fault = session.poll().fault;
            fault.is_some()
        });
        assert!(matches!(fault, Some(DesktopError::Timeout)));
        assert!(!session.acknowledge_reset(1));
        assert_eq!(*fixture.events.lock().unwrap(), ["shift down", "shift up"]);
        assert_eq!(fixture.actor.cleanup_health().failures, 1);
        drop(session);
        fixture.cleanup_fail.store(false, Ordering::Release);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn obsolete_completed_lock_query_cannot_mutate_replacement_authority() {
        let fixture = lock_fixture();
        let old = fixture.actor.activate(30, false);
        wait_until(|| old.input_open());
        let barrier = Arc::new(Barrier::new(2));
        *fixture.outcome.lock().unwrap() = Err(desktop::LockQueryFailure::Deadline);
        *fixture.query_barrier.lock().unwrap() = Some(barrier.clone());
        barrier.wait();
        let replacement = fixture.actor.activate(30, false);
        *fixture.outcome.lock().unwrap() = Ok(desktop::KnownLock::Unlocked);
        barrier.wait();
        wait_until(|| replacement.input_open());
        assert_eq!(replacement.revision(), 0);
        assert_eq!(
            replacement.authority_snapshot().lock,
            desktop::LockKnowledge::Verified(desktop::KnownLock::Unlocked)
        );
        assert_eq!(
            fixture
                .actor
                .0
                .mailbox
                .lock()
                .unwrap()
                .timings
                .lock_query_counts[desktop::LockQueryFailure::Deadline as usize],
            1
        );
        drop(old);
        replacement
            .input(
                InputAction::Key(KeyCode::new(30).unwrap(), Transition::Press),
                fresh_native_permit(&replacement),
            )
            .unwrap();
        wait_until(|| fixture.events.lock().unwrap().len() == 1);
        assert_eq!(*fixture.events.lock().unwrap(), ["a down"]);
        drop(replacement);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    struct HeldBackend {
        base: Fake,
        held: Arc<Mutex<Vec<u32>>>,
        events: Arc<Mutex<Vec<(u32, bool)>>>,
        prepare: Option<Arc<Barrier>>,
        cleanup: Option<Arc<Barrier>>,
        fail_cleanup: bool,
    }
    impl Backend for HeldBackend {
        fn lock(&mut self, _: &Cancellation) -> LockObservation {
            LockObservation {
                outcome: Ok(desktop::KnownLock::Unlocked),
                elapsed_us: 0,
            }
        }
        fn cancellation(&mut self, cancellation: Cancellation) {
            self.base.cancellation(cancellation);
        }
        fn begin_capture(&mut self) -> desktop::Result<()> {
            self.base.begin_capture()
        }
        fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
            self.base.poll_capture()
        }
        fn abort_capture(&mut self) {
            self.base.abort_capture();
        }
        fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
            if let InputAction::Key(key, _) = &action {
                if key == &KeyCode::new(30).unwrap() {
                    if let Some(barrier) = self.prepare.take() {
                        barrier.wait();
                        barrier.wait();
                    }
                }
            }
            permit.emit(1, || {
                if let InputAction::Key(key, transition) = action {
                    let code = [29, 30, 42, 48]
                        .into_iter()
                        .find(|code| KeyCode::new(*code).unwrap() == key)
                        .unwrap();
                    let down = transition == Transition::Press;
                    let mut held = self.held.lock().unwrap();
                    if down {
                        held.push(code);
                    } else {
                        held.retain(|old| *old != code);
                    }
                    self.events.lock().unwrap().push((code, down));
                }
                Ok(())
            })
        }
        fn release(&mut self) -> desktop::Result<()> {
            for code in self.held.lock().unwrap().drain(..) {
                self.events.lock().unwrap().push((code, false));
            }
            if let Some(barrier) = self.cleanup.take() {
                barrier.wait();
                barrier.wait();
            }
            let mut attempts = 0;
            desktop::flush_until(
                Instant::now() + Duration::from_millis(30),
                || {
                    attempts += 1;
                    Ok(!self.fail_cleanup && attempts >= 3)
                },
                |_| {
                    thread::sleep(Duration::from_millis(2));
                    Ok(())
                },
            )
        }
        fn read(&mut self) -> desktop::Result<String> {
            self.base.read()
        }
        fn write(&mut self, text: &str) -> desktop::Result<()> {
            self.base.write(text)
        }
    }
    struct HeldFixture {
        actor: DesktopActor,
        held: Arc<Mutex<Vec<u32>>>,
        events: Arc<Mutex<Vec<(u32, bool)>>>,
        connections: Arc<AtomicUsize>,
    }
    fn held_actor(
        prepare: Option<Arc<Barrier>>,
        cleanup: Option<Arc<Barrier>>,
        fail_cleanup: bool,
    ) -> HeldFixture {
        let held = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (worker_held, worker_events, worker_connections) =
            (held.clone(), events.clone(), connections.clone());
        let mut prepare = prepare;
        let mut cleanup = cleanup;
        let actor = DesktopActor::start_with(move |cancel| {
            let first = worker_connections.fetch_add(1, Ordering::AcqRel) == 0;
            Ok(HeldBackend {
                base: Fake {
                    cancel,
                    frames: Arc::new(AtomicUsize::new(0)),
                    inputs: Arc::new(AtomicUsize::new(0)),
                    releases: Arc::new(AtomicUsize::new(0)),
                    capture_barrier: None,
                    pause_after: usize::MAX,
                    reject_input: false,
                    fail_capture: false,
                },
                held: worker_held.clone(),
                events: worker_events.clone(),
                prepare: prepare.take(),
                cleanup: cleanup.take(),
                fail_cleanup: fail_cleanup && first,
            })
        })
        .unwrap();
        HeldFixture {
            actor,
            held,
            events,
            connections,
        }
    }
    fn held_press(session: &DesktopSession, code: u32, lifetime: Duration) {
        if session.authority.snapshot().lock == desktop::LockKnowledge::Unobserved {
            session.authority.observe_lock(LockObservation {
                outcome: Ok(desktop::KnownLock::Unlocked),
                elapsed_us: 0,
            });
        }
        session
            .input(
                InputAction::Key(KeyCode::new(code).unwrap(), Transition::Press),
                session.authorize(InputPermit::new(1, Instant::now() + lifetime)),
            )
            .unwrap();
    }
    struct CaptureBackend {
        base: HeldBackend,
        results: std::sync::mpsc::Receiver<desktop::Result<()>>,
        attempts: Arc<AtomicUsize>,
        input_timeout: bool,
        pending: bool,
        lock_queries: Option<Arc<Mutex<Vec<bool>>>>,
    }
    impl Backend for CaptureBackend {
        fn lock(&mut self, cancel: &Cancellation) -> LockObservation {
            if let Some(queries) = &self.lock_queries {
                queries.lock().unwrap().push(self.pending);
            }
            self.base.lock(cancel)
        }
        fn cancellation(&mut self, cancel: Cancellation) {
            self.base.cancellation(cancel);
        }
        fn release(&mut self) -> desktop::Result<()> {
            self.base.release()
        }
        fn begin_capture(&mut self) -> desktop::Result<()> {
            if self.attempts.load(Ordering::Acquire) == 0
                && self.base.held.lock().unwrap().is_empty()
            {
                self.pending = false;
            } else {
                self.pending = true;
                self.attempts.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        }
        fn abort_capture(&mut self) {
            self.pending = false;
        }
        fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
            self.base.base.cancel.check()?;
            if !self.pending {
                return self.base.poll_capture();
            }
            match self.results.try_recv() {
                Ok(result) => {
                    self.pending = false;
                    result?;
                    self.base.poll_capture()
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => Ok(desktop::CapturePoll::Pending),
                Err(_) => Err(DesktopError::Cancelled),
            }
        }
        fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
            if std::mem::take(&mut self.input_timeout) {
                return Err(DesktopError::Timeout);
            }
            self.base.input(action, permit)
        }
        fn read(&mut self) -> desktop::Result<String> {
            self.base.read()
        }
        fn write(&mut self, text: &str) -> desktop::Result<()> {
            self.base.write(text)
        }
    }
    fn capture_fixture(
        input_timeout: bool,
    ) -> (
        HeldFixture,
        std::sync::mpsc::Sender<desktop::Result<()>>,
        Arc<AtomicUsize>,
    ) {
        capture_fixture_with_cleanup(input_timeout, false)
    }
    fn capture_fixture_with_cleanup(
        input_timeout: bool,
        fail_cleanup: bool,
    ) -> (
        HeldFixture,
        std::sync::mpsc::Sender<desktop::Result<()>>,
        Arc<AtomicUsize>,
    ) {
        capture_fixture_with_lock_queries(input_timeout, fail_cleanup, None)
    }
    fn capture_fixture_with_lock_queries(
        input_timeout: bool,
        fail_cleanup: bool,
        lock_queries: Option<Arc<Mutex<Vec<bool>>>>,
    ) -> (
        HeldFixture,
        std::sync::mpsc::Sender<desktop::Result<()>>,
        Arc<AtomicUsize>,
    ) {
        let held = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let (send, receive) = std::sync::mpsc::channel();
        let mut receive = Some(receive);
        let (worker_held, worker_events, worker_connections, worker_attempts) = (
            held.clone(),
            events.clone(),
            connections.clone(),
            attempts.clone(),
        );
        let actor = DesktopActor::start_with(move |cancel| {
            worker_connections.fetch_add(1, Ordering::AcqRel);
            Ok(CaptureBackend {
                base: HeldBackend {
                    base: Fake {
                        cancel,
                        frames: Arc::new(AtomicUsize::new(0)),
                        inputs: Arc::new(AtomicUsize::new(0)),
                        releases: Arc::new(AtomicUsize::new(0)),
                        capture_barrier: None,
                        pause_after: usize::MAX,
                        reject_input: false,
                        fail_capture: false,
                    },
                    held: worker_held.clone(),
                    events: worker_events.clone(),
                    prepare: None,
                    cleanup: None,
                    fail_cleanup,
                },
                results: receive.take().unwrap(),
                attempts: worker_attempts.clone(),
                input_timeout,
                pending: false,
                lock_queries: lock_queries.clone(),
            })
        })
        .unwrap();
        (
            HeldFixture {
                actor,
                held,
                events,
                connections,
            },
            send,
            attempts,
        )
    }
    #[test]
    fn periodic_lock_query_waits_for_capture_completion_or_timeout() {
        for result in [
            Ok(()),
            Err(DesktopError::CaptureTimeout(CaptureStage::Ready)),
        ] {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let (fixture, captures, attempts) =
                capture_fixture_with_lock_queries(false, false, Some(queries.clone()));
            let session = fixture.actor.activate(100, false);
            held_press(&session, 42, Duration::from_secs(1));
            wait_until(|| attempts.load(Ordering::Acquire) == 1);
            thread::sleep(Duration::from_millis(300));
            let pending_queries = queries.lock().unwrap().clone();
            captures.send(result).unwrap();
            wait_until(|| queries.lock().unwrap().len() >= 2);
            let completed_queries = queries.lock().unwrap().clone();
            drop(session);
            assert_eq!(
                fixture.actor.shutdown(Duration::from_secs(1)),
                ShutdownOutcome::Clean
            );
            assert_eq!(
                pending_queries,
                [false],
                "periodic query began during capture"
            );
            assert_eq!(
                completed_queries,
                [false, false],
                "deferred query did not run after capture"
            );
        }
    }
    #[test]
    fn pending_capture_serves_keyup_before_capture_completes() {
        let (fixture, _captures, attempts) = capture_fixture(false);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *fixture.held.lock().unwrap() == [42]);
        wait_until(|| attempts.load(Ordering::Acquire) == 1);
        for (code, transition) in [
            (42, Transition::Release),
            (30, Transition::Press),
            (30, Transition::Release),
        ] {
            session
                .input(
                    InputAction::Key(KeyCode::new(code).unwrap(), transition),
                    session.authorize(InputPermit::new(1, Instant::now() + Duration::from_secs(1))),
                )
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_millis(80);
        while fixture.events.lock().unwrap().len() < 4 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let observed = fixture.events.lock().unwrap().clone();
        let held = fixture.held.lock().unwrap().clone();
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
        assert_eq!(
            observed,
            [(42, true), (42, false), (30, true), (30, false)],
            "queued keys did not complete while capture remained pending"
        );
        assert!(held.is_empty());
    }
    #[test]
    fn pending_capture_does_not_block_shutdown_or_hide_cleanup_failure() {
        for fail_cleanup in [false, true] {
            let (fixture, _captures, attempts) = capture_fixture_with_cleanup(false, fail_cleanup);
            let session = fixture.actor.activate(100, false);
            held_press(&session, 42, Duration::from_secs(1));
            wait_until(|| *fixture.held.lock().unwrap() == [42]);
            wait_until(|| attempts.load(Ordering::Acquire) == 1);
            let result = fixture.actor.shutdown(Duration::from_secs(1));
            if fail_cleanup {
                assert!(matches!(result, ShutdownOutcome::Failed(_)));
                assert_eq!(fixture.actor.cleanup_health().failures, 1);
            } else {
                assert_eq!(result, ShutdownOutcome::Clean);
            }
            assert_eq!(*fixture.events.lock().unwrap(), [(42, true), (42, false)]);
            assert!(fixture.held.lock().unwrap().is_empty());
            drop(session);
        }
    }
    fn missed_capture_keeps_key_transitions(stage: CaptureStage) {
        let (fixture, captures, attempts) = capture_fixture(false);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *fixture.held.lock().unwrap() == [42]);
        wait_until(|| attempts.load(Ordering::Acquire) == 1);
        session.poll();
        for (code, transition) in [
            (42, Transition::Release),
            (30, Transition::Press),
            (30, Transition::Release),
        ] {
            session
                .input(
                    InputAction::Key(KeyCode::new(code).unwrap(), transition),
                    session.authorize(InputPermit::new(1, Instant::now() + Duration::from_secs(1))),
                )
                .unwrap();
        }
        captures
            .send(Err(DesktopError::CaptureTimeout(stage)))
            .unwrap();
        wait_until(|| {
            let update = session.poll();
            assert!(
                update.fault.is_none(),
                "one missed capture destroyed queued keyup"
            );
            assert!(update.frame.is_none(), "failed capture published a frame");
            fixture.events.lock().unwrap().len() == 4
        });
        assert_eq!(
            *fixture.events.lock().unwrap(),
            [(42, true), (42, false), (30, true), (30, false)]
        );
        assert!(fixture.held.lock().unwrap().is_empty());
        assert_eq!(fixture.connections.load(Ordering::Acquire), 1);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn missed_capture_keeps_queued_keyup_and_next_key_pair_on_the_same_backend() {
        for stage in [
            CaptureStage::Synchronize,
            CaptureStage::Constraints,
            CaptureStage::Ready,
        ] {
            missed_capture_keeps_key_transitions(stage);
        }
    }
    #[test]
    fn three_consecutive_capture_timeouts_fault_and_explicitly_release_held_input() {
        let (fixture, captures, attempts) = capture_fixture(false);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        for (index, stage) in [
            CaptureStage::Synchronize,
            CaptureStage::Constraints,
            CaptureStage::Ready,
        ]
        .into_iter()
        .enumerate()
        {
            wait_until(|| attempts.load(Ordering::Acquire) == index + 1);
            captures
                .send(Err(DesktopError::CaptureTimeout(stage)))
                .unwrap();
        }
        let mut update = session.poll();
        wait_until(|| {
            if update.fault.is_none() {
                update = session.poll();
            }
            update.fault.is_some()
        });
        assert!(matches!(
            update.fault,
            Some(DesktopError::CaptureTimeout(CaptureStage::Ready))
        ));
        assert_eq!(
            (
                update.timings.capture_timeout_sync,
                update.timings.capture_timeout_constraints,
                update.timings.capture_timeout_ready,
                update.timings.capture_timeout_consecutive
            ),
            (1, 1, 1, 3)
        );
        assert!(!session.input_open());
        wait_until(|| fixture.actor.cleanup_health().completions >= 2);
        assert_eq!(*fixture.events.lock().unwrap(), [(42, true), (42, false)]);
        assert!(fixture.held.lock().unwrap().is_empty());
        assert_eq!(fixture.actor.cleanup_health().failures, 0);
        assert_eq!(fixture.connections.load(Ordering::Acquire), 1);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn successful_capture_resets_the_consecutive_timeout_budget() {
        let (fixture, captures, attempts) = capture_fixture(false);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        for (index, result) in [
            Err(DesktopError::CaptureTimeout(CaptureStage::Ready)),
            Ok(()),
            Err(DesktopError::CaptureTimeout(CaptureStage::Ready)),
            Err(DesktopError::CaptureTimeout(CaptureStage::Ready)),
        ]
        .into_iter()
        .enumerate()
        {
            wait_until(|| attempts.load(Ordering::Acquire) == index + 1);
            captures.send(result).unwrap();
        }
        wait_until(|| attempts.load(Ordering::Acquire) == 5);
        let update = session.poll();
        assert!(update.fault.is_none());
        assert_eq!(update.timings.capture_timeout_ready, 3);
        assert_eq!(update.timings.capture_timeout_consecutive, 2);
        assert_eq!(update.frame.unwrap().content, 1);
        assert_eq!(*fixture.held.lock().unwrap(), [42]);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
        assert_eq!(*fixture.events.lock().unwrap(), [(42, true), (42, false)]);
    }
    #[test]
    fn generic_capture_timeout_is_fatal_and_not_a_dropped_capture() {
        let (fixture, captures, attempts) = capture_fixture(false);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| attempts.load(Ordering::Acquire) == 1);
        captures.send(Err(DesktopError::Timeout)).unwrap();
        let mut update = session.poll();
        wait_until(|| {
            if update.fault.is_none() {
                update = session.poll();
            }
            update.fault.is_some()
        });
        assert!(matches!(update.fault, Some(DesktopError::Timeout)));
        assert_eq!(update.timings.capture_timeout_consecutive, 0);
        wait_until(|| fixture.actor.cleanup_health().completions >= 2);
        assert_eq!(*fixture.events.lock().unwrap(), [(42, true), (42, false)]);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn native_input_timeout_remains_fatal() {
        let (fixture, _, _) = capture_fixture(true);
        let session = fixture.actor.activate(100, false);
        held_press(&session, 42, Duration::from_secs(1));
        let mut update = session.poll();
        wait_until(|| {
            if update.fault.is_none() {
                update = session.poll();
            }
            update.fault.is_some()
        });
        assert!(matches!(update.fault, Some(DesktopError::Timeout)));
        assert_eq!(update.timings.input_failed, 1);
        assert_eq!(update.timings.capture_timeout_consecutive, 0);
        assert!(fixture.events.lock().unwrap().is_empty());
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn expiry_releases_shift_discards_queued_a_and_waits_for_exact_reset_ack() {
        let barrier = Arc::new(Barrier::new(2));
        let HeldFixture {
            actor,
            held,
            events,
            ..
        } = held_actor(Some(barrier.clone()), None, false);
        let session = actor.activate(30, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        held_press(&session, 30, Duration::from_millis(20));
        barrier.wait();
        held_press(&session, 48, Duration::from_secs(1));
        thread::sleep(Duration::from_millis(25));
        barrier.wait();
        let mut reset = None;
        wait_until(|| {
            reset = session.poll().input_reset;
            reset.is_some()
        });
        assert_eq!(reset, Some(1));
        assert!(held.lock().unwrap().is_empty());
        assert_eq!(*events.lock().unwrap(), [(42, true), (42, false)]);
        assert!(!session.input_open());
        assert_eq!(
            session.authority_snapshot(),
            AuthoritySnapshot {
                phase: desktop::AuthorityPhase::AwaitingAcknowledgement,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(!session.acknowledge_reset(0));
        assert_eq!(
            session.authority_snapshot(),
            AuthoritySnapshot {
                phase: desktop::AuthorityPhase::AwaitingAcknowledgement,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(session.acknowledge_reset(1));
        assert_eq!(
            session.authority_snapshot(),
            AuthoritySnapshot {
                phase: desktop::AuthorityPhase::Open,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        held_press(&session, 42, Duration::from_secs(1));
        held_press(&session, 30, Duration::from_secs(1));
        wait_until(|| events.lock().unwrap().len() == 4);
        assert_eq!(
            *events.lock().unwrap(),
            [(42, true), (42, false), (42, true), (30, true)]
        );
        drop(session);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn urgent_cancel_removes_queued_presses_and_stale_poll_preserves_replacement_reset() {
        let barrier = Arc::new(Barrier::new(2));
        let HeldFixture {
            actor,
            held,
            events,
            ..
        } = held_actor(Some(barrier.clone()), None, false);
        let old = actor.activate(30, false);
        held_press(&old, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        held_press(&old, 30, Duration::from_secs(1));
        barrier.wait();
        held_press(&old, 48, Duration::from_secs(1));
        old.release();
        barrier.wait();
        let mut reset = None;
        wait_until(|| {
            reset = old.poll().input_reset;
            reset.is_some()
        });
        assert_eq!(*events.lock().unwrap(), [(42, true), (42, false)]);
        let new = actor.activate(30, false);
        held_press(&new, 29, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [29]);
        new.release();
        wait_until(|| actor.0.mailbox.lock().unwrap().input_reset.is_some());
        assert!(old.poll().input_reset.is_none());
        assert_eq!(new.poll().input_reset, Some(1));
        assert!(!old.acknowledge_reset(0));
        assert!(!new.input_open());
        assert!(new.acknowledge_reset(1));
        drop(old);
        assert!(new.input_open());
        drop(new);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn reset_is_not_published_before_cleanup_finishes_and_idle_cleanup_still_runs() {
        let barrier = Arc::new(Barrier::new(2));
        let HeldFixture {
            actor,
            held,
            events,
            ..
        } = held_actor(None, Some(barrier.clone()), false);
        let session = actor.activate(30, false);
        held_press(&session, 29, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [29]);
        session.release();
        barrier.wait();
        assert!(!session.input_open());
        assert!(session.poll().input_reset.is_none());
        assert!(!session.acknowledge_reset(1));
        barrier.wait();
        let mut reset = None;
        wait_until(|| {
            reset = session.poll().input_reset;
            reset.is_some()
        });
        assert_eq!(reset, Some(1));
        assert_eq!(*events.lock().unwrap(), [(29, true), (29, false)]);
        assert!(session.acknowledge_reset(1));
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        drop(session);
        wait_until(|| held.lock().unwrap().is_empty());
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn failed_cleanup_stays_closed_and_replacement_uses_a_new_native_connection() {
        let HeldFixture {
            actor,
            held,
            connections,
            ..
        } = held_actor(None, None, true);
        let session = actor.activate(30, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        session.release();
        let mut fault = None;
        wait_until(|| {
            fault = session.poll().fault;
            fault.is_some()
        });
        assert!(matches!(fault, Some(DesktopError::Timeout)));
        assert!(!session.input_open());
        assert!(!session.acknowledge_reset(1));
        let new = actor.activate(30, false);
        held_press(&new, 30, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [30]);
        assert_eq!(connections.load(Ordering::Acquire), 2);
        drop(session);
        assert!(new.input_open());
        drop(new);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn dropped_session_cleanup_failure_survives_replacement_activation() {
        let HeldFixture { actor, held, .. } = held_actor(None, None, true);
        let session = actor.activate(30, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        drop(session);
        wait_until(|| actor.cleanup_health().failures == 1);
        let failure = actor.cleanup_health().last_failure.unwrap();
        assert_eq!(
            failure,
            CleanupFailure {
                actor_session: 1,
                sequence: 2,
                reason: CleanupFault::NativeDeadline
            }
        );
        let replacement = actor.activate(30, false);
        assert_eq!(actor.cleanup_health().last_failure, Some(failure));
        held_press(&replacement, 30, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [30]);
        drop(replacement);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
        assert_eq!(actor.cleanup_health().failures, 1);
        assert_eq!(actor.cleanup_health().last_failure, Some(failure));
    }

    #[test]
    fn completed_shutdown_reports_its_failed_native_cleanup() {
        let HeldFixture { actor, held, .. } = held_actor(None, None, true);
        let session = actor.activate(30, false);
        held_press(&session, 42, Duration::from_secs(1));
        wait_until(|| *held.lock().unwrap() == [42]);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Failed(CleanupFailure {
                actor_session: 1,
                sequence: 2,
                reason: CleanupFault::NativeDeadline,
            })
        );
        assert_eq!(actor.cleanup_health().failures, 1);
        drop(session);
    }

    #[test]
    fn shutdown_deadline_is_distinct_from_completed_cleanup_failure() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, _, _) = fixture(Some(barrier.clone()));
        let session = actor.activate(30, false);
        barrier.wait();
        assert_eq!(
            actor.shutdown(Duration::from_millis(1)),
            ShutdownOutcome::Deadline
        );
        barrier.wait();
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
        drop(session);
    }

    #[test]
    fn cleanup_summary_aggregates_failures_without_erasing_the_latest_typed_identity() {
        let mut health = CleanupHealth::default();
        for sequence in 1..=1000 {
            assert_eq!(
                health.completed(7, Some(&DesktopError::Timeout)),
                ShutdownOutcome::Failed(CleanupFailure {
                    actor_session: 7,
                    sequence,
                    reason: CleanupFault::NativeDeadline,
                })
            );
        }
        assert_eq!(health.completed(8, None), ShutdownOutcome::Clean);
        assert_eq!(health.completions, 1001);
        assert_eq!(health.failures, 1000);
        assert_eq!(
            health.last_failure,
            Some(CleanupFailure {
                actor_session: 7,
                sequence: 1000,
                reason: CleanupFault::NativeDeadline
            })
        );
        assert_eq!(
            CleanupFault::from_error(&DesktopError::Connection("private native detail".into())),
            CleanupFault::NativeConnection
        );
    }

    #[test]
    fn expired_native_input_does_not_stop_the_desktop_session() {
        let actor = DesktopActor::start_with(|cancel| {
            Ok(Fake {
                cancel,
                frames: Arc::new(AtomicUsize::new(0)),
                inputs: Arc::new(AtomicUsize::new(0)),
                releases: Arc::new(AtomicUsize::new(0)),
                capture_barrier: None,
                pause_after: 0,
                reject_input: true,
                fail_capture: false,
            })
        })
        .unwrap();
        let session = actor.activate(30, false);
        press(&session).unwrap();
        let mut reset = None;
        wait_until(|| {
            reset = session.poll().input_reset;
            reset.is_some()
        });
        assert_eq!(reset, Some(1));
        let update = session.poll();
        assert_eq!(update.timings.rejected_inputs, 1);
        assert_eq!(update.timings.input_expired, 1);
        assert_eq!(update.timings.input_completed, 0);
        assert!(update.fault.is_none());
        assert!(matches!(press(&session), Err(DesktopError::Cancelled)));
        assert_eq!(session.poll().timings.input_completed, 0);
        assert!(session.acknowledge_reset(1));
        press(&session).unwrap();
        wait_until(|| session.poll().timings.input_completed == 1);
        session.clipboard_read(9).unwrap();
        let mut reply = None;
        wait_until(|| {
            reply = session.poll().clipboard;
            reply.is_some()
        });
        assert_eq!(reply.unwrap().request, 9);
        drop(session);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn queued_input_cannot_starve_capture() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, inputs, _) = fixture_after(Some(barrier.clone()), 4);
        let session = actor.activate(100, false);
        for _ in 0..64 {
            press(&session).unwrap();
        }
        barrier.wait();
        assert!(inputs.load(Ordering::Acquire) < 64);
        assert_eq!(session.poll().frame.unwrap().content, 4);
        drop(session);
        barrier.wait();
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn delayed_capture_does_not_stop_rtc_timeouts_or_status_polling() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, _, _) = fixture(Some(barrier.clone()));
        let session = actor.activate(30, false);
        barrier.wait();
        let began = Instant::now();
        let mut rtc = str0m::Rtc::builder().build(began);
        for _ in 0..100 {
            rtc.handle_input(str0m::Input::Timeout(Instant::now()))
                .unwrap();
            assert!(session.poll().frame.is_none());
            while !matches!(rtc.poll_output().unwrap(), str0m::Output::Timeout(_)) {}
        }
        assert!(began.elapsed() < Duration::from_millis(500));
        barrier.wait();
        drop(session);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }

    #[test]
    fn only_one_clipboard_request_can_remain_pending() {
        let barrier = Arc::new(Barrier::new(2));
        let (actor, _, _, _) = fixture(Some(barrier.clone()));
        let session = actor.activate(30, false);
        barrier.wait();
        session.clipboard_read(7).unwrap();
        assert!(matches!(
            session.clipboard_read(8),
            Err(DesktopError::ClipboardBusy)
        ));
        barrier.wait();
        wait_until(|| actor.0.mailbox.lock().unwrap().clipboard.is_some());
        assert!(matches!(
            session.clipboard_read(8),
            Err(DesktopError::ClipboardBusy)
        ));
        let reply = session.poll().clipboard.unwrap();
        assert_eq!(reply.request, 7);
        assert_eq!(reply.text.as_deref(), Some("fixture"));
        assert!(reply.ok);
        drop(session);
        assert_eq!(
            actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    struct CooperativeInput {
        fake: Fake,
        events: Arc<Mutex<Vec<(u32, bool)>>>,
        ready: Arc<std::sync::atomic::AtomicBool>,
        started: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
        pending: Option<(u32, InputPermit)>,
        failed: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Backend for CooperativeInput {
        fn lock(&mut self, cancel: &Cancellation) -> LockObservation {
            self.fake.lock(cancel)
        }
        fn cancellation(&mut self, cancel: Cancellation) {
            self.fake.cancellation(cancel);
        }
        fn release(&mut self) -> desktop::Result<()> {
            self.fake.release()
        }
        fn begin_capture(&mut self) -> desktop::Result<()> {
            self.fake.begin_capture()
        }
        fn poll_capture(&mut self) -> desktop::Result<desktop::CapturePoll> {
            self.fake.poll_capture()
        }
        fn abort_capture(&mut self) {
            self.fake.abort_capture();
        }
        fn input(&mut self, action: InputAction, permit: InputPermit) -> desktop::Result<()> {
            match action {
                InputAction::Key(key, transition) => permit.emit(1, || {
                    self.events.lock().unwrap().push((
                        [30, 48]
                            .into_iter()
                            .find(|code| KeyCode::new(*code).unwrap() == key)
                            .unwrap(),
                        transition == Transition::Press,
                    ));
                    Ok(())
                }),
                InputAction::Button(_, button, transition) => permit.emit(1, || {
                    self.events.lock().unwrap().push((
                        match button {
                            Button::Left => 272,
                            Button::Right => 273,
                            _ => panic!("unexpected test button"),
                        },
                        transition == Transition::Press,
                    ));
                    Ok(())
                }),
                _ => Err(DesktopError::InvalidInput),
            }
        }
        fn begin_input(
            &mut self,
            action: InputAction,
            permit: InputPermit,
        ) -> desktop::Result<desktop::InputPoll> {
            if let InputAction::Button(_, button, Transition::Press) = action {
                permit.emit(1, || Ok(()))?;
                self.pending = Some((
                    match button {
                        Button::Left => 272,
                        Button::Right => 273,
                        _ => panic!("unexpected test button"),
                    },
                    permit,
                ));
                self.started.fetch_add(1, Ordering::AcqRel);
                Ok(desktop::InputPoll::Pending)
            } else {
                self.input(action, permit)
                    .map(|()| desktop::InputPoll::Complete)
            }
        }
        fn poll_input(&mut self) -> desktop::Result<desktop::InputPoll> {
            self.fake.cancel.check()?;
            let (button, permit) = self.pending.as_ref().unwrap();
            if self.failed.load(Ordering::Acquire) {
                return Err(DesktopError::PointerPreparation(
                    desktop::PointerPreparationFault::Unavailable,
                ));
            }
            permit.emit(1, || Ok(()))?;
            if !self.ready.load(Ordering::Acquire) {
                return Ok(desktop::InputPoll::Pending);
            }
            permit.emit(1, || {
                self.events.lock().unwrap().push((*button, true));
                Ok(())
            })?;
            self.pending = None;
            Ok(desktop::InputPoll::Complete)
        }
        fn abort_input(&mut self) {
            if self.pending.take().is_some() {
                self.aborted.fetch_add(1, Ordering::AcqRel);
            }
        }
        fn read(&mut self) -> desktop::Result<String> {
            self.fake.read()
        }
        fn write(&mut self, text: &str) -> desktop::Result<()> {
            self.fake.write(text)
        }
    }
    struct CooperativeFixture {
        actor: DesktopActor,
        frames: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<(u32, bool)>>>,
        ready: Arc<std::sync::atomic::AtomicBool>,
        started: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
        failed: Arc<std::sync::atomic::AtomicBool>,
    }
    fn cooperative_fixture() -> CooperativeFixture {
        let frames = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let aborted = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failure = failed.clone();
        let (f, e, r, s, a) = (
            frames.clone(),
            events.clone(),
            ready.clone(),
            started.clone(),
            aborted.clone(),
        );
        let actor = DesktopActor::start_with(move |cancel| {
            Ok(CooperativeInput {
                fake: Fake {
                    cancel,
                    frames: f.clone(),
                    inputs: Arc::new(AtomicUsize::new(0)),
                    releases: Arc::new(AtomicUsize::new(0)),
                    capture_barrier: None,
                    pause_after: 0,
                    reject_input: false,
                    fail_capture: false,
                },
                events: e.clone(),
                ready: r.clone(),
                started: s.clone(),
                aborted: a.clone(),
                pending: None,
                failed: failure.clone(),
            })
        })
        .unwrap();
        CooperativeFixture {
            actor,
            frames,
            events,
            ready,
            started,
            aborted,
            failed,
        }
    }
    fn pending_press(session: &DesktopSession) {
        wait_until(|| session.input_open());
        let permit =
            session.authorize(InputPermit::new(1, Instant::now() + Duration::from_secs(1)));
        session
            .input(
                InputAction::Button(
                    Position::new(0.5, 0.5).unwrap(),
                    Button::Left,
                    Transition::Press,
                ),
                permit,
            )
            .unwrap();
    }
    #[test]
    fn pending_native_button_preserves_fifo_and_capture_progress() {
        let fixture = cooperative_fixture();
        let session = fixture.actor.activate(100, false);
        pending_press(&session);
        wait_until(|| fixture.started.load(Ordering::Acquire) == 1);
        let initial = fixture.frames.load(Ordering::Acquire);
        held_press(&session, 30, Duration::from_secs(1));
        wait_until(|| fixture.frames.load(Ordering::Acquire) >= initial + 3);
        assert_eq!(*fixture.events.lock().unwrap(), []);
        assert_eq!(session.poll().timings.input_completed, 0);
        fixture.ready.store(true, Ordering::Release);
        wait_until(|| fixture.events.lock().unwrap().len() == 2);
        assert_eq!(*fixture.events.lock().unwrap(), [(272, true), (30, true)]);
        wait_until(|| session.poll().timings.input_completed == 2);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn urgent_release_aborts_pending_button_before_cleanup_and_never_replays_it() {
        let fixture = cooperative_fixture();
        let session = fixture.actor.activate(100, false);
        pending_press(&session);
        wait_until(|| fixture.started.load(Ordering::Acquire) == 1);
        held_press(&session, 30, Duration::from_secs(1));
        session.release();
        let mut reset = None;
        wait_until(|| {
            reset = session.poll().input_reset;
            reset.is_some()
        });
        assert_eq!(fixture.aborted.load(Ordering::Acquire), 1);
        assert_eq!(*fixture.events.lock().unwrap(), []);
        fixture.ready.store(true, Ordering::Release);
        assert!(session.acknowledge_reset(reset.unwrap()));
        held_press(&session, 48, Duration::from_secs(1));
        wait_until(|| fixture.events.lock().unwrap().len() == 1);
        assert_eq!(*fixture.events.lock().unwrap(), [(48, true)]);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn replacement_activation_aborts_obsolete_pending_button() {
        let fixture = cooperative_fixture();
        let old = fixture.actor.activate(100, false);
        pending_press(&old);
        wait_until(|| fixture.started.load(Ordering::Acquire) == 1);
        let new = fixture.actor.activate(100, false);
        fixture.ready.store(true, Ordering::Release);
        held_press(&new, 48, Duration::from_secs(1));
        wait_until(|| fixture.events.lock().unwrap().len() == 1);
        assert_eq!(fixture.aborted.load(Ordering::Acquire), 1);
        assert_eq!(*fixture.events.lock().unwrap(), [(48, true)]);
        drop(old);
        drop(new);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
    #[test]
    fn failed_pointer_preparation_resets_input_once_without_stopping_capture() {
        let fixture = cooperative_fixture();
        let session = fixture.actor.activate(100, false);
        pending_press(&session);
        wait_until(|| fixture.started.load(Ordering::Acquire) == 1);
        held_press(&session, 30, Duration::from_secs(1));
        fixture.failed.store(true, Ordering::Release);
        let mut reset = None;
        wait_until(|| {
            reset = session.poll().input_reset;
            reset.is_some()
        });
        let update = session.poll();
        assert!(update.fault.is_none());
        assert_eq!(update.timings.input_failed, 1);
        assert_eq!(update.timings.rejected_inputs, 1);
        assert_eq!(fixture.aborted.load(Ordering::Acquire), 1);
        assert_eq!(*fixture.events.lock().unwrap(), []);
        let initial = fixture.frames.load(Ordering::Acquire);
        wait_until(|| fixture.frames.load(Ordering::Acquire) >= initial + 3);
        fixture.failed.store(false, Ordering::Release);
        fixture.ready.store(true, Ordering::Release);
        assert!(session.acknowledge_reset(reset.unwrap()));
        held_press(&session, 48, Duration::from_secs(1));
        wait_until(|| fixture.events.lock().unwrap().len() == 1);
        assert_eq!(*fixture.events.lock().unwrap(), [(48, true)]);
        assert_eq!(session.poll().timings.input_completed, 1);
        drop(session);
        assert_eq!(
            fixture.actor.shutdown(Duration::from_secs(1)),
            ShutdownOutcome::Clean
        );
    }
}
