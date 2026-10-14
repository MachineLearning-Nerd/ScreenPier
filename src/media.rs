use crate::{
    allocation::{Allocation, AllocationController, BandwidthObservation, EstimateKind, fps_for},
    desktop::{
        AuthorityPhase, AuthoritySnapshot, Button, ClipboardGuard, DesktopGeometry, InputPermit,
        KeyCode, KnownLock, LockKnowledge, LockState, Position, Transition,
    },
    desktop_actor::{
        ActorTimings, CleanupHealth, DesktopActor, DesktopSession, InputAction, ShutdownOutcome,
    },
    diagnostics::{
        CleanupEvent, CleanupRecord, DiagnosticsLog, FaultReason, PeerDiagnostics, PeerSnapshot,
        ReadinessEvent, ReadinessObservation, ReadinessReason, ReadinessRecord,
    },
    encoder::{Encoder, EncoderFault, EncoderWorkers, RawFrame},
    server::{Config, HostStatus},
    session::{
        Command, DEADMAN, DisplayProgress, FrameIdentity, MAX_MESSAGE, deadline_offset, micros,
    },
};
use anyhow::{Context, Result, bail};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    bwe::{Bitrate, BweKind},
    change::SdpOffer,
    channel::{ChannelId, Reliability},
    format::Codec,
    media::{Direction, Frequency, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QualityMode {
    #[default]
    Clear,
    LowData,
    Smooth,
}

impl QualityMode {
    fn bitrate_cap(self, operator_cap: u32) -> u32 {
        let mode_cap = match self {
            Self::Clear => 800,
            Self::LowData => 560,
            Self::Smooth => 2400,
        };
        operator_cap.min(mode_cap)
    }

    fn capture_fps(self, operator_cap: u32) -> u32 {
        let cap = self.bitrate_cap(operator_cap);
        if self == Self::Smooth && cap >= 1500 {
            20
        } else {
            fps_for(cap)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RateMode {
    Fixed {
        fps: u32,
    },
    #[cfg(test)]
    Adaptive,
}

pub enum HostCommand {
    Offer {
        sdp: String,
        takeover: bool,
        quality: QualityMode,
        reply: oneshot::Sender<OfferResult>,
    },
    Stop {
        lease: String,
        reply: oneshot::Sender<bool>,
    },
}
pub enum OfferResult {
    Accepted {
        sdp: String,
        lease: String,
        epoch: String,
    },
    Busy,
    Rejected,
}

fn random_id() -> String {
    let mut bytes = [0; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const RESET_DRAIN: Duration = Duration::from_secs(5);
const RTC_DRAIN_BUDGET: Duration = Duration::from_millis(2);
const RTC_DRAIN_TIMEOUT_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug)]
enum PeerPhase {
    Active,
    Resetting { deadline: Instant },
}

#[derive(Debug, PartialEq, Eq)]
struct ControlAvailability {
    input: bool,
    clipboard: bool,
}
impl ControlAvailability {
    fn new(current: bool, input_open: bool, lock: LockState) -> Self {
        Self {
            input: current && input_open && lock != LockState::Unknown,
            clipboard: current && lock == LockState::Unlocked,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControlState {
    ready: bool,
    clipboard_ready: bool,
    revision: u64,
    generation: u64,
    lock: LockState,
}

struct OutboundMessage {
    bytes: Vec<u8>,
    clipboard: Option<(ClipboardGuard, Vec<u8>)>,
}
impl std::ops::Deref for OutboundMessage {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
struct Peer {
    rtc: Rtc,
    events: VecDeque<Event>,
    deadline: Instant,
    lease: String,
    epoch: String,
    started: Instant,
    heartbeat: Instant,
    connected: bool,
    authenticated_channel: bool,
    channel: Option<ChannelId>,
    mid: Option<Mid>,
    outbound: VecDeque<OutboundMessage>,
    desktop: Option<DesktopSession>,
    actor: Option<DesktopActor>,
    geometry: Option<DesktopGeometry>,
    actor_timings: ActorTimings,
    encoder: Option<Encoder>,
    encoder_workers: EncoderWorkers,
    progress: Option<DisplayProgress>,
    content: u64,
    generation: u64,
    next_status: Instant,
    last_queued_control: Option<ControlState>,
    lock: LockState,
    next_keyframe_at: Instant,
    reset_encoder: bool,
    rate_mode: RateMode,
    allocation: AllocationController,
    frames: u64,
    ready: bool,
    phase: PeerPhase,
    stage: &'static str,
    diagnostics: PeerDiagnostics,
}

impl Peer {
    fn new(
        sdp: String,
        config: &Config,
        local: SocketAddr,
        socket: Option<&UdpSocket>,
        rate_mode: RateMode,
    ) -> Result<(Self, String)> {
        let offer = SdpOffer::from_sdp_string(&sdp)?;
        let now = Instant::now();
        let mut builder = Rtc::builder()
            .clear_codecs()
            .enable_bwe(Some(Bitrate::kbps(config.bitrate_kbps as u64)));
        builder
            .codec_config()
            .add_h264(96.into(), Some(97.into()), true, 0x42e028);
        let rtc = builder.build(now);
        let mut peer = Self {
            rtc,
            events: VecDeque::new(),
            deadline: now,
            lease: random_id(),
            epoch: random_id(),
            started: now,
            heartbeat: now,
            connected: false,
            authenticated_channel: false,
            channel: None,
            mid: None,
            outbound: VecDeque::new(),
            desktop: None,
            actor: None,
            geometry: None,
            actor_timings: ActorTimings::default(),
            encoder: None,
            encoder_workers: EncoderWorkers::default(),
            progress: None,
            content: 0,
            generation: 0,
            next_status: now,
            last_queued_control: None,
            lock: LockState::Unknown,
            next_keyframe_at: now + Duration::from_secs(1),
            reset_encoder: false,
            rate_mode,
            allocation: AllocationController::new(
                config.bitrate_kbps,
                Allocation {
                    kbps: config.bitrate_kbps,
                    fps: match rate_mode {
                        RateMode::Fixed { fps } => fps,
                        #[cfg(test)]
                        RateMode::Adaptive => fps_for(config.bitrate_kbps),
                    },
                },
                now,
            ),
            frames: 0,
            ready: false,
            phase: PeerPhase::Active,
            stage: "Negotiation",
            diagnostics: PeerDiagnostics::new(now),
        };
        let candidate = Candidate::host(local, "udp")?;
        peer.rtc.add_local_candidate(candidate);
        peer.drain_to(socket)?;
        let answer = peer.rtc.sdp_api().accept_offer(offer)?;
        let media_lines: Vec<_> = answer
            .media_lines
            .iter()
            .filter(|line| line.typ.is_media())
            .collect();
        if media_lines.len() != 1 {
            bail!("offer must negotiate one receive-only video track");
        }
        let mid = media_lines[0].mid();
        let media = peer
            .rtc
            .media(mid)
            .context("negotiated media unavailable")?;
        if media.kind() != MediaKind::Video || media.direction() != Direction::SendOnly {
            bail!("offer must negotiate one receive-only video track");
        }
        peer.mid = Some(mid);
        let answer = answer.to_sdp_string();
        peer.drain_to(socket)?;
        let writer = peer.rtc.writer(mid).context("no video writer")?;
        if !writer.payload_params().any(|p| {
            p.spec().codec == Codec::H264
                && p.spec().format.packetization_mode == Some(1)
                && p.spec().format.profile_level_id == Some(0x42e028)
        }) {
            bail!("H.264 constrained baseline level 4.0 packetization mode 1 is required");
        }
        peer.rtc
            .bwe()
            .set_desired_bitrate(Bitrate::kbps(config.bitrate_kbps as u64));
        peer.drain_to(socket)?;
        Ok((peer, answer))
    }

    fn drain(&mut self, socket: &UdpSocket) -> Result<()> {
        self.drain_to(Some(socket))
    }
    fn drain_to(&mut self, socket: Option<&UdpSocket>) -> Result<()> {
        let began = Instant::now();
        let mut serviced_timeouts = 0;
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(deadline) => {
                    self.deadline = deadline;
                    let now = Instant::now();
                    if deadline > now
                        || serviced_timeouts >= RTC_DRAIN_TIMEOUT_LIMIT
                        || now.duration_since(began) >= RTC_DRAIN_BUDGET
                    {
                        return Ok(());
                    }
                    self.diagnostics.timeout(now, deadline);
                    self.rtc.handle_input(Input::Timeout(now))?;
                    serviced_timeouts += 1;
                }
                Output::Transmit(transmit) => {
                    if let Some(socket) = socket {
                        socket.send_to(&transmit.contents, transmit.destination)?;
                    }
                }
                Output::Event(event) => {
                    if self.events.len() >= 256 {
                        bail!("RTC event queue exceeded its bound");
                    }
                    self.events.push_back(event);
                }
            }
        }
    }

    fn queue(&mut self, message: serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(&message)?;
        if bytes.len() > MAX_MESSAGE || self.outbound.len() >= 64 {
            bail!("control output queue exceeded its bound");
        }
        self.outbound.push_back(OutboundMessage {
            bytes,
            clipboard: None,
        });
        Ok(())
    }

    fn queue_clipboard(&mut self, reply: crate::desktop_actor::ClipboardReply) -> Result<()> {
        let bytes = if reply.guard.current() && reply.ok {
            serde_json::to_vec(&if reply.text.is_some() {
                clipboard_reply(reply.request, reply.text)
            } else {
                json!({"type":"clipboard","request":reply.request,"ok":true})
            })?
        } else {
            serde_json::to_vec(&json!({"type":"clipboard","request":reply.request,"ok":false}))?
        };
        let denied =
            serde_json::to_vec(&json!({"type":"clipboard","request":reply.request,"ok":false}))?;
        if bytes.len() > MAX_MESSAGE || self.outbound.len() >= 64 {
            bail!("control output queue exceeded its bound");
        }
        self.outbound.push_back(OutboundMessage {
            bytes,
            clipboard: Some((reply.guard, denied)),
        });
        Ok(())
    }

    fn flush_messages(&mut self, socket: &UdpSocket) -> Result<()> {
        let Some(id) = self.channel else {
            return Ok(());
        };
        self.rtc.handle_input(Input::Timeout(Instant::now()))?;
        self.drain(socket)?;
        let resetting = self.is_resetting();
        for _ in 0..16 {
            let Some(message) = self.outbound.front() else {
                break;
            };
            let Some(mut channel) = self.rtc.channel(id) else {
                bail!("control channel closed");
            };
            if channel.buffered_amount() > 128 * 1024 {
                if resetting {
                    break;
                }
                bail!("control channel is stalled");
            }
            let accepted = if let Some((guard, denied)) = &message.clipboard {
                match guard.with(|| channel.write(false, &message.bytes)) {
                    Some(result) => result?,
                    None => channel.write(false, denied)?,
                }
            } else {
                channel.write(false, &message.bytes)?
            };
            self.drain(socket)?;
            if !accepted {
                break;
            }
            self.outbound.pop_front();
        }
        Ok(())
    }

    fn is_resetting(&self) -> bool {
        matches!(self.phase, PeerPhase::Resetting { .. })
    }

    fn accepts_offer(&self, takeover: bool) -> bool {
        takeover || self.is_resetting()
    }

    fn owns_lease(&self, lease: &str) -> bool {
        self.lease == lease
    }

    fn begin_reset(
        &mut self,
        reason: &str,
        lock: LockState,
        now: Instant,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        if self.is_resetting() {
            return Ok(());
        }
        self.diagnostics.fail(match reason {
            "Desktop lock state changed" => FaultReason::LockChanged,
            "Monitor geometry changed" => FaultReason::GeometryChanged,
            _ => FaultReason::Operation,
        });
        if self.ready {
            self.record_readiness(
                log,
                now,
                ReadinessEvent::PeerFalling,
                match reason {
                    "Desktop lock state changed" => ReadinessReason::LockChanged,
                    "Monitor geometry changed" => ReadinessReason::GeometryChanged,
                    _ => ReadinessReason::Unresolved,
                },
                false,
                self.generation,
            );
        }
        self.revoke_input();
        self.phase = PeerPhase::Resetting {
            deadline: now + RESET_DRAIN,
        };
        self.lock = lock;
        self.encoder = None;
        self.desktop = None;
        self.geometry = None;
        self.progress = None;
        self.outbound.clear();
        self.queue(json!({"type":"reset","reason":reason}))
    }

    fn revoke_input(&mut self) {
        if let Some(desktop) = &self.desktop {
            desktop.release();
        }
        self.ready = false;
    }

    fn current_display(&self, now: Instant) -> bool {
        !self.is_resetting()
            && self.authenticated_channel
            && now.duration_since(self.heartbeat) < DEADMAN
            && self
                .progress
                .as_ref()
                .is_some_and(|p| p.admits(self.generation, now))
    }

    fn control_availability(&self, now: Instant) -> ControlAvailability {
        let state = self.control_state(
            now,
            self.desktop
                .as_ref()
                .map(DesktopSession::authority_snapshot),
        );
        ControlAvailability {
            input: state.ready,
            clipboard: state.clipboard_ready,
        }
    }

    fn control_state(&self, now: Instant, authority: Option<AuthoritySnapshot>) -> ControlState {
        let lock = authority.map_or(self.lock, |snapshot| snapshot.lock.visible_state());
        let availability = ControlAvailability::new(
            self.current_display(now) && authority.is_some(),
            authority.is_some_and(|snapshot| snapshot.phase == AuthorityPhase::Open),
            lock,
        );
        let clipboard_verified = authority
            .is_some_and(|snapshot| snapshot.lock == LockKnowledge::Verified(KnownLock::Unlocked));
        ControlState {
            ready: availability.input,
            clipboard_ready: availability.clipboard && clipboard_verified,
            revision: authority.map_or(0, |snapshot| snapshot.revision),
            generation: self.generation,
            lock,
        }
    }

    fn publish_control_state(
        &mut self,
        state: ControlState,
        now: Instant,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        if self.last_queued_control == Some(state) && now < self.next_status {
            return Ok(());
        }
        self.queue(json!({"type":"state","ready":state.ready,"clipboard_ready":state.clipboard_ready,"generation":state.generation,"input_revision":state.revision.to_string(),"lock":state.lock,"bitrate_kbps":self.allocation.applied().allocation.kbps,"fps":self.allocation.applied().allocation.fps}))?;
        self.last_queued_control = Some(state);
        if now >= self.next_status {
            self.next_status = now + Duration::from_millis(500);
        }
        if let Some(log) = log {
            let mut record = self.readiness_record(
                now,
                ReadinessEvent::StateQueued,
                ReadinessReason::None,
                state.ready,
                state.generation,
            );
            record.generation = Some(state.generation);
            record.message_revision = Some(state.revision);
            log.submit_readiness(record);
        }
        Ok(())
    }

    fn readiness_reason(
        &self,
        now: Instant,
        generation: u64,
        authority: Option<AuthoritySnapshot>,
    ) -> ReadinessReason {
        if authority.is_some_and(|snapshot| snapshot.phase != AuthorityPhase::Open) {
            return ReadinessReason::AuthorityAlreadyClosed;
        }
        if self.is_resetting() {
            return ReadinessReason::PeerResetting;
        }
        if !self.authenticated_channel {
            return ReadinessReason::ChannelUnauthenticated;
        }
        if self.desktop.is_none() {
            return ReadinessReason::DesktopAbsent;
        }
        if self.lock == LockState::Unknown {
            return ReadinessReason::LockUnknown;
        }
        let Some(progress) = &self.progress else {
            return ReadinessReason::ProgressAbsent;
        };
        let evidence = progress.evidence(generation, now);
        if !evidence.generation_matches {
            return ReadinessReason::GenerationMismatch;
        }
        if evidence.presented_capture_age_us.is_none() || evidence.accepted_ack_age_us.is_none() {
            return ReadinessReason::PresentationAbsent;
        }
        let display_expired = progress
            .input_deadline(generation)
            .is_some_and(|deadline| now >= deadline);
        let heartbeat_expired = now.duration_since(self.heartbeat) >= DEADMAN;
        match (display_expired, heartbeat_expired) {
            (true, false) => ReadinessReason::DisplayExpired,
            (false, true) => ReadinessReason::HeartbeatExpired,
            _ => ReadinessReason::Unresolved,
        }
    }

    fn readiness_record(
        &mut self,
        now: Instant,
        event: ReadinessEvent,
        reason: ReadinessReason,
        ready_after: bool,
        generation: u64,
    ) -> ReadinessRecord {
        let authority = self
            .desktop
            .as_ref()
            .map(DesktopSession::authority_snapshot);
        let evidence = self
            .progress
            .as_ref()
            .map(|progress| progress.evidence(generation, now));
        let reason = if matches!(
            event,
            ReadinessEvent::PeerFalling
                | ReadinessEvent::AdmissionDenied
                | ReadinessEvent::HeartbeatRevocation
                | ReadinessEvent::ResetObserved
        ) && authority
            .is_some_and(|snapshot| snapshot.phase != AuthorityPhase::Open)
        {
            ReadinessReason::AuthorityAlreadyClosed
        } else if matches!(reason, ReadinessReason::Unresolved) {
            self.readiness_reason(now, generation, authority)
        } else {
            reason
        };
        let pacer_queue = self.pacer_queue(now);
        let mut record = self.diagnostics.readiness_record(
            now,
            ReadinessObservation {
                event,
                reason,
                ready_before: self.ready,
                ready_after,
                authority,
                evidence,
            },
        );
        record.heartbeat_age_us = micros(now.saturating_duration_since(self.heartbeat));
        record.pacer_queue = pacer_queue;
        record.permit_deadline_offset_us = self
            .progress
            .as_ref()
            .and_then(|progress| progress.input_deadline(generation))
            .map(|deadline| deadline_offset(deadline.min(self.heartbeat + DEADMAN), now));
        record
    }

    fn record_readiness(
        &mut self,
        log: Option<&DiagnosticsLog>,
        now: Instant,
        event: ReadinessEvent,
        reason: ReadinessReason,
        ready_after: bool,
        generation: u64,
    ) {
        if let Some(log) = log {
            let record = self.readiness_record(now, event, reason, ready_after, generation);
            log.submit_readiness(record);
        }
    }

    fn check_ready(
        &mut self,
        now: Instant,
        authority: Option<AuthoritySnapshot>,
        log: Option<&DiagnosticsLog>,
    ) -> ControlState {
        let state = self.control_state(now, authority);
        let ready = state.ready;
        if self.ready != ready {
            self.record_readiness(
                log,
                now,
                if ready {
                    ReadinessEvent::PeerRising
                } else {
                    ReadinessEvent::PeerFalling
                },
                if ready {
                    ReadinessReason::None
                } else {
                    ReadinessReason::Unresolved
                },
                ready,
                self.generation,
            );
        }
        if self.ready && !ready {
            self.revoke_input();
        }
        self.ready = ready;
        state
    }

    fn process_command(&mut self, command: Command, log: Option<&DiagnosticsLog>) -> Result<()> {
        if self.is_resetting() {
            return Ok(());
        }
        self.stage = "Desktop control";
        let now = Instant::now();
        match command {
            Command::Hello { lease } => {
                if self.authenticated_channel || lease != self.lease {
                    bail!("invalid channel lease");
                }
                self.authenticated_channel = true;
                self.heartbeat = now;
                self.queue(json!({"type":"hello","epoch":self.epoch,"input_revision":self.desktop.as_ref().map_or(0, |desktop| desktop.revision()).to_string()}))?;
            }
            _ if !self.authenticated_channel => bail!("channel lease required"),
            Command::Heartbeat {} => self.heartbeat = now,
            Command::Release { revision } => {
                if self
                    .desktop
                    .as_ref()
                    .is_some_and(|desktop| revision == desktop.revision().to_string())
                {
                    self.record_readiness(
                        log,
                        now,
                        ReadinessEvent::ExplicitRelease,
                        ReadinessReason::ExplicitRelease,
                        false,
                        self.generation,
                    );
                    self.revoke_input();
                }
            }
            Command::InputResetAck { revision } => {
                if let (Some(_), Ok(revision)) = (&self.desktop, revision.parse::<u64>()) {
                    let mut record = log.map(|_| {
                        self.readiness_record(
                            now,
                            ReadinessEvent::ResetAcknowledged,
                            ReadinessReason::None,
                            self.ready,
                            self.generation,
                        )
                    });
                    let accepted = self.desktop.as_ref().unwrap().acknowledge_reset(revision);
                    if let (Some(log), Some(mut record)) = (log, record.take()) {
                        let after = self.desktop.as_ref().unwrap().authority_snapshot();
                        record.submitted_revision = Some(revision);
                        record.acknowledgement_accepted = Some(accepted);
                        record.authority_phase_after = Some(after.phase as u8);
                        record.input_revision_after = Some(after.revision);
                        log.submit_readiness(record);
                    }
                }
            }
            Command::Presented {
                epoch,
                generation,
                rtp,
            } => {
                if epoch == self.epoch {
                    if let Some(progress) = &mut self.progress {
                        if progress.acknowledge(generation, rtp, now) {
                            self.diagnostics.received_ack_frame_last_ms = progress
                                .presented_age(now)
                                .map(|age| age.as_millis() as u64);
                        }
                    }
                }
            }
            Command::ClipboardRead { request } => {
                let queued = self.control_availability(now).clipboard
                    && self
                        .desktop
                        .as_ref()
                        .is_some_and(|desktop| desktop.clipboard_read(request).is_ok());
                if !queued {
                    self.queue(clipboard_reply(request, None))?;
                }
            }
            Command::ClipboardWrite { request, text } => {
                let queued = text.len() <= 64 * 1024
                    && !text.contains('\0')
                    && self.control_availability(now).clipboard
                    && self
                        .desktop
                        .as_ref()
                        .is_some_and(|desktop| desktop.clipboard_write(request, text).is_ok());
                if !queued {
                    self.queue(json!({"type":"clipboard","request":request,"ok":false}))?;
                }
            }
            Command::Pointer {
                revision,
                generation,
                x,
                y,
            } => {
                if let Some(permit) = self.admit_input_revision(generation, &revision, log) {
                    self.queue_input(InputAction::Pointer(Position::new(x, y)?), permit)?;
                }
            }
            Command::Key {
                revision,
                generation,
                code,
                down,
            } => {
                if let Some(permit) = self.admit_input_revision(generation, &revision, log) {
                    self.queue_input(
                        InputAction::Key(KeyCode::new(code)?, transition(down)),
                        permit,
                    )?;
                }
            }
            Command::Button {
                revision,
                generation,
                x,
                y,
                button,
                down,
            } => {
                if let Some(permit) = self.admit_input_revision(generation, &revision, log) {
                    let button = match button {
                        0 => Button::Left,
                        1 => Button::Middle,
                        2 => Button::Right,
                        3 => Button::Back,
                        4 => Button::Forward,
                        _ => bail!("unsupported button"),
                    };
                    self.queue_input(
                        InputAction::Button(Position::new(x, y)?, button, transition(down)),
                        permit,
                    )?;
                }
            }
            Command::Scroll {
                revision,
                generation,
                x,
                y,
                dx,
                dy,
            } => {
                if let Some(permit) = self.admit_input_revision(generation, &revision, log) {
                    self.queue_input(InputAction::Axis(Position::new(x, y)?, dx, dy), permit)?;
                }
            }
        }
        Ok(())
    }

    fn queue_input(&self, action: InputAction, permit: InputPermit) -> Result<()> {
        match self.desktop.as_ref().unwrap().input(action, permit) {
            Ok(())
            | Err(
                crate::desktop::DesktopError::Cancelled
                | crate::desktop::DesktopError::InputBacklog,
            ) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn admit_input_revision(
        &mut self,
        generation: u64,
        revision: &str,
        log: Option<&DiagnosticsLog>,
    ) -> Option<InputPermit> {
        if !self.desktop.as_ref().is_some_and(|desktop| {
            desktop.input_open() && revision == desktop.revision().to_string()
        }) {
            self.diagnostics.input_received = self.diagnostics.input_received.saturating_add(1);
            self.diagnostics.input_denied = self.diagnostics.input_denied.saturating_add(1);
            return None;
        }
        self.admit_input(generation, log).map(|permit| {
            self.desktop
                .as_ref()
                .unwrap()
                .authorize_revision(permit, revision.parse().unwrap())
        })
    }

    fn admit_input(
        &mut self,
        generation: u64,
        log: Option<&DiagnosticsLog>,
    ) -> Option<InputPermit> {
        self.diagnostics.input_received = self.diagnostics.input_received.saturating_add(1);
        let now = Instant::now();
        let permit = if self.current_display(now)
            && self.lock != LockState::Unknown
            && generation == self.generation
        {
            self.progress
                .as_ref()
                .and_then(|progress| progress.input_permit(generation, self.heartbeat, now))
        } else {
            None
        };
        if permit.is_none() {
            self.diagnostics.input_denied = self.diagnostics.input_denied.saturating_add(1);
            self.record_readiness(
                log,
                now,
                ReadinessEvent::AdmissionDenied,
                ReadinessReason::Unresolved,
                false,
                generation,
            );
            self.revoke_input();
        } else {
            self.diagnostics.input_admitted = self.diagnostics.input_admitted.saturating_add(1);
        }
        permit.map(|permit| match self.desktop.as_ref() {
            Some(desktop) => desktop.authorize(permit),
            None => permit,
        })
    }

    fn events(
        &mut self,
        config: &Config,
        socket: &UdpSocket,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        let began = Instant::now();
        for _ in 0..64 {
            if !self.is_resetting() && self.connected && self.heartbeat.elapsed() >= DEADMAN {
                self.record_readiness(
                    log,
                    Instant::now(),
                    ReadinessEvent::HeartbeatRevocation,
                    ReadinessReason::Unresolved,
                    false,
                    self.generation,
                );
                self.revoke_input();
                bail!("controller heartbeat expired");
            }
            if began.elapsed() > Duration::from_millis(10) {
                break;
            }
            let Some(event) = self.events.pop_front() else {
                break;
            };
            if self.is_resetting() {
                if matches!(event, Event::ChannelClose(_) | Event::Closed) {
                    bail!("peer closed");
                }
                continue;
            }
            match event {
                Event::Connected => {
                    self.stage = "Wayland connection";
                    self.connected = true;
                    self.heartbeat = Instant::now();
                    self.desktop = Some(
                        self.actor
                            .as_ref()
                            .context("desktop actor unavailable")?
                            .activate(self.allocation.applied().allocation.fps, !config.local_test),
                    );
                }
                Event::ChannelOpen(id, label) => {
                    let channel = self.rtc.channel(id).context("missing opened channel")?;
                    let channel_config =
                        channel.config().context("missing channel configuration")?;
                    if label != "control"
                        || !channel_config.ordered
                        || channel_config.reliability != Reliability::Reliable
                        || self.channel.is_some()
                    {
                        bail!("one reliable ordered control channel required");
                    }
                    self.channel = Some(id);
                }
                Event::ChannelData(data) => {
                    if Some(data.id) != self.channel || data.binary || data.data.len() > MAX_MESSAGE
                    {
                        bail!("invalid control message");
                    }
                    let command: Command = serde_json::from_slice(&data.data)
                        .map_err(|_| anyhow::anyhow!("invalid control command"))?;
                    self.process_command(command, log)?;
                }
                Event::ChannelClose(_) | Event::Closed => bail!("peer closed"),
                Event::KeyframeRequest(_) => self.request_keyframe(Instant::now()),
                Event::EgressBitrateEstimate(estimate) => {
                    self.observe_bandwidth(estimate, Instant::now());
                }
                _ => {}
            }
        }
        self.flush_messages(socket)
    }

    fn observe_bandwidth(&mut self, estimate: BweKind, now: Instant) {
        let (estimate, kind) = match estimate {
            BweKind::Twcc {
                estimate,
                can_probe,
            } => (estimate, EstimateKind::Twcc { can_probe }),
            BweKind::Remb { estimate, .. } => (estimate, EstimateKind::Remb),
            _ => return,
        };
        let observation = BandwidthObservation {
            estimate_bps: estimate.as_u64(),
            observed_at: now,
            kind,
        };
        if matches!(self.rate_mode, RateMode::Fixed { .. }) {
            self.allocation.record(observation);
        } else {
            self.allocation.observe(observation);
        }
    }

    fn request_keyframe(&mut self, now: Instant) {
        self.diagnostics.keyframe_requests += 1;
        if now >= self.next_keyframe_at {
            self.next_keyframe_at = now + Duration::from_secs(1);
            self.reset_encoder = true;
        }
    }

    fn start_encoder(&mut self, config: &Config, width: u32, height: u32) -> Result<()> {
        let allocation = self.allocation.applied().allocation;
        self.encoder = Some(Encoder::start_tracked(
            config.encoder,
            width,
            height,
            allocation.kbps,
            allocation.fps,
            &self.encoder_workers,
        )?);
        self.diagnostics.encoder_starts += 1;
        self.diagnostics.encoder_allocation = Some(allocation);
        Ok(())
    }

    fn capture(
        &mut self,
        config: &Config,
        socket: &UdpSocket,
        _now: Instant,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        if self.is_resetting() || !self.connected || !self.authenticated_channel {
            return Ok(());
        }
        if let Some(fault) = self.encoder.as_ref().and_then(Encoder::fault) {
            self.stage = "H.264 encoder";
            return Err(fault.into());
        }
        let update = self.desktop.as_ref().context("desktop unavailable")?.poll();
        self.actor_timings = update.timings;
        if let Some(revision) = update.input_reset {
            if let Some(log) = log {
                let mut record = self.readiness_record(
                    Instant::now(),
                    ReadinessEvent::ResetObserved,
                    ReadinessReason::ResetObservation,
                    false,
                    self.generation,
                );
                record.message_revision = Some(revision);
                log.submit_readiness(record);
            }
            self.ready = false;
            self.queue(json!({"type":"input_reset","revision":revision.to_string()}))?;
        }
        if let Some(error) = update.fault {
            self.stage = "Desktop actor";
            return Err(error.into());
        }
        if let Some(update) = update.lock {
            let consumed_at = Instant::now();
            let authority = self.desktop.as_ref().unwrap().authority_snapshot();
            if let Some(log) = log {
                log.submit_lock(crate::diagnostics::LockRecord {
                    version: 1,
                    session: self.diagnostics.session,
                    lock_sequence: update.sequence,
                    lock_outcome: update.observation.outcome_code(),
                    query_elapsed_us: update.observation.elapsed_us,
                    observed_elapsed_us: crate::session::micros(
                        update.observed_at.saturating_duration_since(self.started),
                    ),
                    consumed_elapsed_us: crate::session::micros(
                        consumed_at.saturating_duration_since(self.started),
                    ),
                    observation_phase: update.authority.phase as u8,
                    observation_revision: update.authority.revision,
                    authority_phase: authority.phase as u8,
                    input_revision: authority.revision,
                    lock_knowledge: authority.lock.code(),
                    last_known: authority.lock.last_known().map(|known| known as u8),
                    transition: matches!(authority.lock, LockKnowledge::Changed { .. }),
                    lock_generation: authority.lock_generation,
                    query_counts: self.actor_timings.lock_query_counts,
                    log_dropped: 0,
                    log_failed: false,
                });
            }
            if let LockKnowledge::Changed { current, .. } = authority.lock {
                self.begin_reset(
                    "Desktop lock state changed",
                    current.into(),
                    consumed_at,
                    log,
                )?;
                return self.flush_messages(socket);
            }
            self.lock = authority.lock.visible_state();
        }
        if let Some(reply) = update.clipboard {
            self.queue_clipboard(reply)?;
        }
        if let Some(frame) = update.frame {
            self.stage = "Desktop capture";
            if self.generation != 0 && self.generation != frame.geometry.generation {
                self.begin_reset("Monitor geometry changed", self.lock, Instant::now(), log)?;
                return self.flush_messages(socket);
            }
            self.generation = frame.geometry.generation;
            self.geometry = Some(frame.geometry.clone());
            self.content = frame.content;
            let pixels = frame.pixels;
            let captured = frame.captured;
            let progress = self
                .progress
                .get_or_insert_with(|| DisplayProgress::new(self.generation, captured));
            progress.captured(self.content, captured);
            if self.reset_encoder {
                self.encoder = None;
                self.reset_encoder = false;
            }
            if self.encoder.is_none() {
                self.stage = "H.264 encoder startup";
                self.start_encoder(config, frame.width, frame.height)?;
                self.desktop
                    .as_ref()
                    .unwrap()
                    .set_fps(self.allocation.applied().allocation.fps);
            }
            let ticks =
                (captured.duration_since(self.started).as_nanos() * 90_000 / 1_000_000_000) as u64;
            let identity = FrameIdentity {
                timestamp: ticks as u32,
                generation: self.generation,
                content: self.content,
                captured,
            };
            self.stage = "H.264 encoder";
            self.encoder.as_ref().unwrap().submit(RawFrame {
                pixels,
                identity,
                ticks,
            })?;
        }
        for _ in 0..2 {
            self.stage = "H.264 video transport";
            let encoded = self
                .encoder
                .as_ref()
                .and_then(|encoder| encoder.output.try_recv().ok());
            let Some(frame) = encoded else {
                break;
            };
            self.diagnostics.encoding = Some(frame.timings);
            self.diagnostics.capture_to_writer_last_ms = Some(
                Instant::now()
                    .saturating_duration_since(frame.identity.captured)
                    .as_millis() as u64,
            );
            self.rtc.handle_input(Input::Timeout(Instant::now()))?;
            self.drain(socket)?;
            let mid = self.mid.context("video not negotiated")?;
            let writer = self.rtc.writer(mid).context("video writer unavailable")?;
            let pt = writer
                .payload_params()
                .find(|p| {
                    p.spec().codec == Codec::H264 && p.spec().format.packetization_mode == Some(1)
                })
                .context("H.264 not negotiated")?
                .pt();
            writer.write(
                pt,
                frame.identity.captured,
                MediaTime::new(frame.ticks, Frequency::NINETY_KHZ),
                frame.bytes,
            )?;
            self.drain(socket)?;
            self.progress.as_mut().unwrap().sent(frame.identity);
            self.frames += 1;
            let geometry = self.geometry.as_ref().context("geometry unavailable")?;
            self.queue(json!({"type":"frame","epoch":self.epoch,"generation":self.generation,"rtp":frame.identity.timestamp,"width":geometry.width,"height":geometry.height}))?;
        }
        self.stage = "H.264 encoder";
        if let Some(fault) = self.encoder.as_ref().and_then(Encoder::fault) {
            return Err(fault.into());
        }
        let now = Instant::now();
        let state = self.check_ready(
            now,
            self.desktop
                .as_ref()
                .map(DesktopSession::authority_snapshot),
            log,
        );
        self.publish_control_state(state, now, log)?;
        self.flush_messages(socket)
    }

    fn tick(
        &mut self,
        config: &Config,
        socket: &UdpSocket,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        self.tick_at(config, socket, Instant::now(), log)
    }

    fn tick_at(
        &mut self,
        config: &Config,
        socket: &UdpSocket,
        now: Instant,
        log: Option<&DiagnosticsLog>,
    ) -> Result<()> {
        self.diagnostics.tick(now);
        self.stage = "Media transport";
        if now >= self.deadline {
            self.diagnostics.timeout(now, self.deadline);
            self.rtc.handle_input(Input::Timeout(now))?;
            self.drain(socket)?;
        }
        self.events(config, socket, log)?;
        if let PeerPhase::Resetting { deadline } = self.phase {
            if Instant::now() >= deadline {
                bail!("reset drain expired");
            }
            return Ok(());
        }
        if self.started.elapsed() > Duration::from_secs(12 * 60 * 60)
            || (!self.connected && self.started.elapsed() > Duration::from_secs(15))
            || (self.connected && self.heartbeat.elapsed() >= DEADMAN)
        {
            bail!("session expired");
        }
        if self.allocation.take_due(now).is_some() {
            self.reset_encoder = true;
        }
        self.capture(config, socket, Instant::now(), log)
    }
    fn log_snapshot(&mut self, log: Option<&DiagnosticsLog>, now: Instant) {
        if let Some(log) = log {
            let pacer_queue = self.pacer_queue(now);
            log.submit(self.diagnostics.record(
                now,
                PeerSnapshot {
                    bitrate_kbps: self.allocation.applied().allocation.kbps,
                    fps: self.allocation.applied().allocation.fps,
                    allocation: Some(crate::diagnostics::AllocationRecord::new(
                        &self.allocation,
                        now,
                    )),
                    bwe_estimate_bps: self.allocation.latest().map(|o| o.estimate_bps),
                    frames: self.frames,
                    input_ready: self.ready,
                    actor: self.actor_timings,
                    presented_age: self.progress.as_ref().and_then(|p| p.presented_age(now)),
                    ack_age: self.progress.as_ref().and_then(|p| p.accepted_ack_age(now)),
                    pacer_queue,
                },
            ));
        }
    }
    fn pacer_queue(&mut self, now: Instant) -> Option<crate::diagnostics::PacerQueueRecord> {
        self.mid.and_then(|mid| {
            self.rtc
                .direct_api()
                .stream_tx_by_mid(mid, None)
                .and_then(|stream| stream.queue_info())
                .map(|info| crate::diagnostics::PacerQueueRecord::new(info, now))
        })
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.revoke_input();
        self.encoder = None;
    }
}
fn transition(down: bool) -> Transition {
    if down {
        Transition::Press
    } else {
        Transition::Release
    }
}

fn clipboard_reply(request: u32, text: Option<String>) -> serde_json::Value {
    let message = json!({"type":"clipboard","request":request,"ok":text.is_some(),"text":text});
    if serde_json::to_vec(&message).is_ok_and(|bytes| bytes.len() <= MAX_MESSAGE) {
        message
    } else {
        json!({"type":"clipboard","request":request,"ok":false,"error":"Clipboard text exceeds the 72 KiB encoded transfer limit"})
    }
}

pub fn run_host(
    config: Config,
    socket: UdpSocket,
    receiver: Receiver<HostCommand>,
    status: Arc<Mutex<HostStatus>>,
    stop: Arc<AtomicBool>,
) {
    let diagnostics = config
        .diagnostics_log
        .clone()
        .map(|path| Arc::new(DiagnosticsLog::start(path)));
    let host_started = Instant::now();
    let mut observed_cleanup_failures = 0;
    let mut session_ordinal = 0u64;
    let actor = match DesktopActor::start_logged(config.output.clone(), diagnostics.clone()) {
        Ok(actor) => actor,
        Err(_) => {
            status.lock().unwrap().state = "Desktop actor could not start".into();
            return;
        }
    };
    let encoder_workers = EncoderWorkers::default();
    let mut peer: Option<Peer> = None;
    let mut buffer = [0u8; 65536];
    let local = socket.local_addr().expect("bound UDP socket");
    let mut last_failure = String::new();
    while !stop.load(Ordering::Acquire) {
        for _ in 0..8 {
            let Ok(command) = receiver.try_recv() else {
                break;
            };
            match command {
                HostCommand::Offer {
                    sdp,
                    takeover,
                    quality,
                    reply,
                } => {
                    if reply.is_closed() {
                        continue;
                    }
                    if peer.as_ref().is_some_and(|p| !p.accepts_offer(takeover)) {
                        let _ = reply.send(OfferResult::Busy);
                        continue;
                    }
                    if let Some(active) = peer.as_mut() {
                        active.diagnostics.fail(FaultReason::Replaced);
                        active.log_snapshot(diagnostics.as_deref(), Instant::now());
                    }
                    peer = None;
                    let mut session_config = config.clone();
                    session_config.bitrate_kbps = quality.bitrate_cap(config.bitrate_kbps);
                    let rate_mode = RateMode::Fixed {
                        fps: quality.capture_fps(config.bitrate_kbps),
                    };
                    let result = Peer::new(sdp, &session_config, local, Some(&socket), rate_mode);
                    match result {
                        Ok((mut new_peer, sdp)) => {
                            session_ordinal = session_ordinal.saturating_add(1);
                            new_peer.diagnostics.session = session_ordinal;
                            new_peer.actor = Some(actor.clone());
                            new_peer.encoder_workers = encoder_workers.clone();
                            let response = OfferResult::Accepted {
                                sdp,
                                lease: new_peer.lease.clone(),
                                epoch: new_peer.epoch.clone(),
                            };
                            if reply.send(response).is_ok() {
                                peer = Some(new_peer);
                                last_failure.clear();
                            }
                        }
                        Err(_) => {
                            let _ = reply.send(OfferResult::Rejected);
                        }
                    }
                }
                HostCommand::Stop { lease, reply } => {
                    let current = peer.as_ref().is_some_and(|p| p.owns_lease(&lease));
                    if current {
                        if let Some(active) = peer.as_mut() {
                            active.diagnostics.fail(FaultReason::Stopped);
                            active.log_snapshot(diagnostics.as_deref(), Instant::now());
                        }
                        peer = None;
                    }
                    let _ = reply.send(current);
                }
            }
        }
        let mut failure = None;
        if let Some(active) = peer.as_mut() {
            for _ in 0..64 {
                match socket.recv_from(&mut buffer) {
                    Ok((size, source)) => {
                        if let Ok(receive) =
                            Receive::new(Protocol::Udp, source, local, &buffer[..size])
                        {
                            let input = Input::Receive(Instant::now(), receive);
                            if active.rtc.accepts(&input)
                                && (active.rtc.handle_input(input).is_err()
                                    || active.drain(&socket).is_err())
                            {
                                active.diagnostics.fail(FaultReason::Transport);
                                failure = Some("Media transport failed".into());
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        active.diagnostics.fail(FaultReason::Socket);
                        failure = Some("Media socket failed".into());
                        break;
                    }
                }
            }
            if failure.is_none() {
                if let Err(error) = active.tick(&config, &socket, diagnostics.as_deref()) {
                    active
                        .diagnostics
                        .fail(FaultReason::from_safe_reason(failure_reason(&error)));
                    failure = Some(format!(
                        "{} stopped ({}). Reconnect to restore it.",
                        active.stage,
                        failure_reason(&error)
                    ));
                }
            }
            if failure.is_some() {
                active.revoke_input();
            }
            if failure.is_some() || active.diagnostics.due(Instant::now()) {
                active.log_snapshot(diagnostics.as_deref(), Instant::now());
            }
        }
        if let Some(error) = failure {
            peer = None;
            last_failure = error;
        }
        let cleanup = actor.cleanup_health();
        observe_cleanup(
            cleanup,
            &mut observed_cleanup_failures,
            diagnostics.as_deref(),
            host_started,
        );
        let mut snapshot = if let Some(active) = &peer {
            HostStatus {
                active: true,
                connected: active.connected,
                input_ready: active.ready,
                lock: format!("{:?}", active.lock).to_lowercase(),
                state: if active.is_resetting() {
                    "resetting"
                } else if active.ready {
                    "active"
                } else if active.connected {
                    "awaiting_display"
                } else {
                    "negotiating"
                }
                .into(),
                frames: active.frames,
                bitrate_kbps: active.allocation.applied().allocation.kbps,
                diagnostics_dropped: 0,
                diagnostics_failed: false,
                cleanup,
                desktop_shutdown: None,
                presented_age_ms: active.progress.as_ref().and_then(|p| {
                    p.presented_age(Instant::now())
                        .map(|age| age.as_millis() as u64)
                }),
            }
        } else {
            HostStatus {
                state: if cleanup.failures != 0 {
                    "Native input cleanup failed. See cleanup status.".into()
                } else if last_failure.is_empty() {
                    "disconnected".into()
                } else {
                    last_failure.clone()
                },
                cleanup,
                ..HostStatus::default()
            }
        };
        if let Some(log) = &diagnostics {
            snapshot.diagnostics_dropped = log.dropped();
            snapshot.diagnostics_failed = log.failed();
        }
        *status.lock().unwrap() = snapshot;
        let sleep = peer.as_ref().map_or(Duration::from_millis(2), |active| {
            active
                .deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(2))
        });
        std::thread::sleep(sleep);
    }
    if let Some(active) = peer.as_mut() {
        active.diagnostics.fail(FaultReason::Stopped);
        active.log_snapshot(diagnostics.as_deref(), Instant::now());
    }
    drop(peer);
    let shutdown_deadline = Instant::now() + Duration::from_secs(2);
    let desktop_shutdown = actor.shutdown(Duration::from_secs(2));
    let cleanup = actor.cleanup_health();
    observe_cleanup(
        cleanup,
        &mut observed_cleanup_failures,
        diagnostics.as_deref(),
        host_started,
    );
    if let Some(log) = &diagnostics {
        log.submit_cleanup(cleanup_record(
            CleanupEvent::HostShutdown,
            host_started,
            cleanup,
            Some(desktop_shutdown),
        ));
    }
    {
        let mut status = status.lock().unwrap();
        status.cleanup = cleanup;
        status.desktop_shutdown = Some(desktop_shutdown);
    }
    let encoders_finished =
        encoder_workers.wait(shutdown_deadline.saturating_duration_since(Instant::now()));
    let diagnostics_finished = diagnostics
        .as_ref()
        .is_none_or(|log| log.shutdown(Duration::from_millis(250)));
    let mut status = status.lock().unwrap();
    if matches!(desktop_shutdown, ShutdownOutcome::Deadline)
        || !encoders_finished
        || !diagnostics_finished
    {
        status.state = "Shutdown cleanup deadline exceeded".into();
    } else if matches!(desktop_shutdown, ShutdownOutcome::Failed(_)) {
        status.state = "Shutdown native input cleanup failed".into();
    }
    if let Some(log) = &diagnostics {
        status.diagnostics_dropped = log.dropped();
        status.diagnostics_failed = log.failed();
    }
}

fn cleanup_record(
    event: CleanupEvent,
    started: Instant,
    cleanup: CleanupHealth,
    shutdown: Option<ShutdownOutcome>,
) -> CleanupRecord {
    CleanupRecord {
        version: 1,
        event,
        elapsed_ms: started.elapsed().as_millis() as u64,
        cleanup,
        shutdown,
        log_dropped: 0,
        log_failed: false,
    }
}
fn observe_cleanup(
    cleanup: CleanupHealth,
    observed: &mut u64,
    log: Option<&DiagnosticsLog>,
    started: Instant,
) {
    if cleanup.failures > *observed {
        *observed = cleanup.failures;
        if let Some(log) = log {
            log.submit_cleanup(cleanup_record(
                CleanupEvent::NativeReleaseFailure,
                started,
                cleanup,
                None,
            ));
        }
    }
}

fn failure_reason(error: &anyhow::Error) -> &'static str {
    use crate::desktop::{CaptureStage, DesktopError};
    use str0m::RtcError;
    if let Some(fault) = error.downcast_ref::<EncoderFault>() {
        return match fault {
            EncoderFault::InputDeadline => "encoder input deadline",
            EncoderFault::OutputDeadline => "encoder output deadline",
            EncoderFault::MetadataBacklog => "encoder metadata backlog",
            EncoderFault::OutputQueue => "encoder output queue",
            EncoderFault::ProcessExit => "encoder process exit",
            EncoderFault::Io => "encoder IO",
            EncoderFault::WorkerLimit => "encoder worker limit",
            EncoderFault::Framing => "encoder framing",
            EncoderFault::Other => "encoder operation",
        };
    }
    if let Some(error) = error.downcast_ref::<DesktopError>() {
        return match error {
            DesktopError::Timeout => "Wayland deadline",
            DesktopError::CaptureTimeout(CaptureStage::Synchronize) => "capture sync deadline",
            DesktopError::CaptureTimeout(CaptureStage::Constraints) => {
                "capture constraints deadline"
            }
            DesktopError::CaptureTimeout(CaptureStage::Ready) => "capture ready deadline",
            DesktopError::CaptureFailed => "capture rejected",
            DesktopError::OutputUnavailable => "output unavailable",
            DesktopError::StaleGeometry => "geometry changed",
            DesktopError::InputExpired => "input permission expired",
            DesktopError::InputBacklog => "input command backlog",
            DesktopError::Cancelled => "desktop command cancelled",
            DesktopError::Connection(_) | DesktopError::Dispatch(_) => "Wayland connection",
            _ => "desktop operation",
        };
    }
    if let Some(error) = error.downcast_ref::<RtcError>() {
        return match error {
            RtcError::Ice(_) => "ICE",
            RtcError::Dtls(_) => "DTLS",
            RtcError::Sctp(_) => "data channel",
            RtcError::WriteWithoutPoll => "RTC write ordering",
            _ => "RTC operation",
        };
    }
    match error.to_string().as_str() {
        "session expired" | "controller heartbeat expired" => "controller deadline",
        "peer closed" => "peer closed",
        "reset drain expired" => "reset deadline",
        "control channel is stalled" => "control backpressure",
        _ => "operation failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_modes_are_bounded_by_the_operator_cap() {
        assert_eq!(QualityMode::Clear.bitrate_cap(3000), 800);
        assert_eq!(QualityMode::LowData.bitrate_cap(3000), 560);
        assert_eq!(QualityMode::Smooth.bitrate_cap(3000), 2400);
        assert_eq!(QualityMode::Clear.capture_fps(3000), 15);
        assert_eq!(QualityMode::LowData.capture_fps(3000), 8);
        assert_eq!(QualityMode::Smooth.capture_fps(3000), 20);
        for mode in [
            QualityMode::Clear,
            QualityMode::LowData,
            QualityMode::Smooth,
        ] {
            assert_eq!(mode.bitrate_cap(400), 400);
        }
    }

    #[test]
    fn clear_mode_keeps_its_measured_recipe_when_sparse_feedback_falls() {
        let mut config = config();
        config.bitrate_kbps = QualityMode::Clear.bitrate_cap(config.bitrate_kbps);
        let (mut peer, _) = Peer::new(
            include_str!("fixtures/recvonly-h264.sdp").into(),
            &config,
            "127.0.0.1:6081".parse().unwrap(),
            None,
            RateMode::Fixed { fps: 15 },
        )
        .unwrap();
        peer.observe_bandwidth(
            BweKind::Twcc {
                estimate: Bitrate::kbps(100),
                can_probe: true,
            },
            Instant::now(),
        );
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation { kbps: 800, fps: 15 }
        );
        assert_eq!(peer.allocation.pending(), None);
        let (log, receiver) = DiagnosticsLog::stalled();
        peer.log_snapshot(Some(&log), Instant::now());
        let crate::diagnostics::LogRecord::Peer(record) = receiver.try_recv().unwrap() else {
            panic!("peer diagnostic record required");
        };
        assert_eq!(record.bwe_estimate_bps, Some(100_000));
    }
    fn known_authority() -> crate::desktop::InputAuthority {
        let authority = crate::desktop::InputAuthority::default();
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Ok(crate::desktop::KnownLock::Unlocked),
            elapsed_us: 0,
        });
        authority
    }

    fn state_messages(peer: &Peer) -> Vec<serde_json::Value> {
        peer.outbound
            .iter()
            .map(|bytes| serde_json::from_slice(bytes).unwrap())
            .collect()
    }

    #[test]
    fn recovered_control_is_published_before_the_periodic_deadline() {
        let t0 = Instant::now();
        let mut peer = fresh_peer(t0);
        peer.next_status = t0;
        let authority = known_authority();
        let state = peer.control_state(t0, Some(authority.snapshot()));
        peer.publish_control_state(state, t0, None).unwrap();
        assert_eq!(
            state_messages(&peer),
            vec![json!({
                "type":"state", "ready":true, "clipboard_ready":true,
                "generation":1, "input_revision":"0", "lock":"unlocked",
                "bitrate_kbps":3000, "fps":30
            })]
        );
        peer.outbound.clear();
        authority.revoke();
        assert_eq!(authority.released(), Some(1));
        peer.ready = false;
        peer.queue(json!({"type":"input_reset","revision":"1"}))
            .unwrap();
        let state = peer.control_state(t0 + Duration::from_millis(100), Some(authority.snapshot()));
        peer.publish_control_state(state, t0 + Duration::from_millis(100), None)
            .unwrap();
        assert_eq!(
            state_messages(&peer),
            vec![
                json!({"type":"input_reset","revision":"1"}),
                json!({"type":"state","ready":false,"clipboard_ready":true,
                "generation":1,"input_revision":"1","lock":"unlocked",
                "bitrate_kbps":3000,"fps":30})
            ]
        );
        peer.outbound.clear();
        assert!(authority.acknowledge(1));
        let presented = t0 + Duration::from_millis(140);
        peer.progress.as_mut().unwrap().captured(2, presented);
        peer.progress.as_mut().unwrap().sent(FrameIdentity {
            timestamp: 43,
            generation: 1,
            content: 2,
            captured: presented,
        });
        assert!(
            peer.progress
                .as_mut()
                .unwrap()
                .acknowledge(1, 43, presented)
        );
        let now = t0 + Duration::from_millis(141);
        let state = peer.control_state(now, Some(authority.snapshot()));
        peer.publish_control_state(state, now, None).unwrap();
        assert_eq!(
            state_messages(&peer),
            vec![json!({
                "type":"state","ready":true,"clipboard_ready":true,
                "generation":1,"input_revision":"1","lock":"unlocked",
                "bitrate_kbps":3000,"fps":30
            })]
        );
        assert_eq!(peer.next_status, t0 + Duration::from_millis(500));
        peer.publish_control_state(state, now, None).unwrap();
        assert_eq!(peer.outbound.len(), 1);
    }

    #[test]
    fn accepted_reset_and_fresh_display_queue_true_on_the_next_turn() {
        let t0 = Instant::now();
        let mut peer = fresh_peer(t0);
        let authority = known_authority();
        authority.revoke();
        assert_eq!(authority.released(), Some(1));
        let state = peer.control_state(t0 + Duration::from_millis(100), Some(authority.snapshot()));
        peer.last_queued_control = Some(state);
        peer.next_status = t0 + Duration::from_millis(500);
        assert!(authority.acknowledge(1));
        let now = t0 + Duration::from_millis(141);
        let state = peer.check_ready(now, Some(authority.snapshot()), None);
        peer.publish_control_state(state, now, None).unwrap();
        assert_eq!(
            state_messages(&peer),
            vec![json!({
                "type":"state","ready":true,"clipboard_ready":true,
                "generation":1,"input_revision":"1","lock":"unlocked",
                "bitrate_kbps":3000,"fps":30
            })]
        );
    }

    #[test]
    fn closed_authority_and_expired_display_never_publish_ready() {
        let t0 = Instant::now();
        let mut peer = fresh_peer(t0);
        let authority = known_authority();
        authority.revoke();
        assert!(!authority.acknowledge(1));
        assert!(!peer.control_state(t0, Some(authority.snapshot())).ready);
        assert_eq!(authority.released(), Some(1));
        assert!(!authority.acknowledge(0));
        let state = peer.control_state(t0, Some(authority.snapshot()));
        peer.publish_control_state(state, t0, None).unwrap();
        assert_eq!(state_messages(&peer)[0]["ready"], false);
        assert!(authority.acknowledge(1));
        assert!(!authority.acknowledge(1));
        let expired = t0 + DEADMAN;
        let state = peer.control_state(expired, Some(authority.snapshot()));
        peer.publish_control_state(state, expired, None).unwrap();
        assert_eq!(state_messages(&peer)[1]["ready"], false);
        assert!(!state.clipboard_ready);
        peer.heartbeat = expired;
        assert!(
            !peer
                .control_state(expired, Some(authority.snapshot()))
                .ready
        );
        peer.generation = 2;
        assert!(!peer.control_state(t0, Some(authority.snapshot())).ready);
        peer.generation = 1;
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Err(crate::desktop::LockQueryFailure::Deadline),
            elapsed_us: 150_000,
        });
        assert!(!peer.control_state(t0, Some(authority.snapshot())).ready);
        assert!(!peer.control_state(t0, None).ready);
    }

    #[test]
    fn every_control_signature_change_is_published_without_duplicate_messages() {
        let now = Instant::now();
        let mut peer = fresh_peer(now);
        let authority = known_authority();
        let mut state = peer.control_state(now, Some(authority.snapshot()));
        peer.publish_control_state(state, now, None).unwrap();
        for next in [
            ControlState {
                ready: false,
                ..state
            },
            ControlState {
                clipboard_ready: false,
                ..state
            },
            ControlState {
                revision: 7,
                ..state
            },
            ControlState {
                generation: 2,
                ..state
            },
            ControlState {
                lock: LockState::Locked,
                ..state
            },
        ] {
            let prior = peer.outbound.len();
            peer.publish_control_state(next, now, None).unwrap();
            peer.publish_control_state(next, now, None).unwrap();
            assert_eq!(peer.outbound.len(), prior + 1);
            assert_eq!(
                state_messages(&peer).last().unwrap()["input_revision"],
                next.revision.to_string()
            );
            state = next;
        }
        assert_eq!(peer.last_queued_control, Some(state));
    }

    #[test]
    fn failed_state_admission_commits_neither_signature_nor_periodic_deadline() {
        let now = Instant::now();
        let mut peer = fresh_peer(now);
        let state = peer.control_state(now, Some(known_authority().snapshot()));
        for _ in 0..64 {
            peer.queue(json!({"type":"frame","rtp":42})).unwrap();
        }
        let deadline = peer.next_status;
        assert!(
            peer.publish_control_state(state, now + Duration::from_millis(500), None)
                .is_err()
        );
        assert_eq!(peer.last_queued_control, None);
        assert_eq!(peer.next_status, deadline);
        peer.outbound.clear();
        peer.publish_control_state(state, now + Duration::from_millis(500), None)
            .unwrap();
        assert_eq!(peer.last_queued_control, Some(state));
        assert_eq!(peer.outbound.len(), 1);
    }

    #[test]
    fn periodic_state_still_publishes_bitrate_and_fps_when_permissions_do_not_change() {
        let t0 = Instant::now();
        let mut peer = fresh_peer(t0);
        peer.next_status = t0;
        let state = peer.control_state(t0, Some(known_authority().snapshot()));
        peer.publish_control_state(state, t0, None).unwrap();
        peer.outbound.clear();
        peer.allocation = AllocationController::new(3000, Allocation { kbps: 700, fps: 8 }, t0);
        peer.publish_control_state(state, t0 + Duration::from_millis(499), None)
            .unwrap();
        assert!(peer.outbound.is_empty());
        peer.publish_control_state(state, t0 + Duration::from_millis(500), None)
            .unwrap();
        assert_eq!(
            state_messages(&peer),
            vec![json!({
                "type":"state","ready":true,"clipboard_ready":true,
                "generation":1,"input_revision":"0","lock":"unlocked",
                "bitrate_kbps":700,"fps":8
            })]
        );
    }

    #[test]
    fn already_closed_authority_never_attributes_a_later_expired_display() {
        let now = Instant::now();
        let peer = fresh_peer(now - Duration::from_secs(4));
        for phase in [
            AuthorityPhase::Releasing,
            AuthorityPhase::AwaitingAcknowledgement,
        ] {
            assert_eq!(
                peer.readiness_reason(
                    now,
                    1,
                    Some(AuthoritySnapshot {
                        phase,
                        revision: 7,
                        lock: crate::desktop::LockKnowledge::Verified(
                            crate::desktop::KnownLock::Unlocked
                        ),
                        lock_generation: 1
                    })
                ),
                ReadinessReason::AuthorityAlreadyClosed
            );
        }
    }
    #[test]
    fn readiness_fall_is_recorded_once_before_mutation_and_queue_saturation_is_safe() {
        let (mut peer, _) = offline_peer();
        let (log, receiver) = DiagnosticsLog::stalled();
        peer.ready = true;
        let now = Instant::now();
        assert!(!peer.check_ready(now, None, Some(&log)).ready);
        assert!(!peer.ready);
        let crate::diagnostics::LogRecord::Readiness(record) = receiver.try_recv().unwrap() else {
            panic!("readiness record required")
        };
        assert_eq!(record.readiness_event, 1);
        assert_eq!(record.reason, 3);
        assert!(record.ready_before);
        assert!(!record.ready_after);
        assert_eq!(record.authority_phase, None);
        assert!(!peer.check_ready(now, None, Some(&log)).ready);
        assert!(receiver.try_recv().is_err());
        peer.ready = true;
        assert!(!peer.check_ready(now, None, Some(&log)).ready);
        peer.ready = true;
        assert!(!peer.check_ready(now, None, Some(&log)).ready);
        assert_eq!(log.dropped(), 1);
        assert!(!peer.ready);
    }

    #[test]
    fn encoder_fault_survives_context_without_exposing_untyped_errors() {
        let error =
            anyhow::Error::new(EncoderFault::MetadataBacklog).context("capture submission failed");
        assert_eq!(failure_reason(&error), "encoder metadata backlog");
        assert_eq!(
            failure_reason(&anyhow::anyhow!("private diagnostic payload")),
            "operation failed"
        );
    }

    fn config() -> Config {
        Config {
            owner_login: None,
            public_origin: None,
            http_port: 6080,
            media_ip: "127.0.0.1".parse().unwrap(),
            media_port: 6081,
            output: "eDP-1".into(),
            encoder: crate::encoder::EncoderKind::Vaapi,
            bitrate_kbps: 3000,
            local_test: true,
            diagnostics_log: None,
        }
    }

    fn offline_peer() -> (Peer, String) {
        let config = config();
        let result = Peer::new(
            include_str!("fixtures/recvonly-h264.sdp").into(),
            &config,
            "127.0.0.1:6081".parse().unwrap(),
            None,
            RateMode::Adaptive,
        );
        result.unwrap()
    }

    fn tick_socket() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").unwrap()
    }

    #[test]
    fn peer_tick_consumes_retained_bwe_after_cooldown_without_another_event() {
        let (mut peer, _) = offline_peer();
        let start = Instant::now();
        peer.allocation = AllocationController::new(
            3000,
            Allocation {
                kbps: 3000,
                fps: 30,
            },
            start,
        );
        peer.events
            .push_back(Event::EgressBitrateEstimate(BweKind::Twcc {
                estimate: Bitrate::bps(1_000_000),
                can_probe: true,
            }));
        let socket = tick_socket();
        peer.tick_at(&config(), &socket, start + Duration::from_millis(200), None)
            .unwrap();
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation {
                kbps: 3000,
                fps: 30
            }
        );
        assert!(!peer.reset_encoder);
        assert!(peer.events.is_empty());
        peer.tick_at(&config(), &socket, start + Duration::from_secs(1), None)
            .unwrap();
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation { kbps: 800, fps: 15 }
        );
        assert!(peer.reset_encoder);
        assert_eq!(peer.allocation.commits(), 1);
    }

    #[test]
    fn peer_tick_recovers_ordinary_growth_and_preserves_twcc_capability_metadata() {
        let (mut peer, _) = offline_peer();
        let start = Instant::now();
        peer.allocation = AllocationController::new(3000, Allocation { kbps: 700, fps: 30 }, start);
        peer.events
            .push_back(Event::EgressBitrateEstimate(BweKind::Twcc {
                estimate: Bitrate::bps(1_050_000),
                can_probe: false,
            }));
        let socket = tick_socket();
        peer.tick_at(&config(), &socket, start, None).unwrap();
        peer.tick_at(&config(), &socket, start + Duration::from_secs(4), None)
            .unwrap();
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation { kbps: 840, fps: 15 }
        );
        assert_eq!(
            peer.allocation.latest().unwrap().kind,
            EstimateKind::Twcc { can_probe: false }
        );
        assert!(peer.reset_encoder);
    }

    #[test]
    fn peer_headroom_and_remb_mapping_survive_keyframe_requests() {
        let (mut peer, _) = offline_peer();
        let start = Instant::now();
        peer.allocation = AllocationController::new(
            3000,
            Allocation {
                kbps: 3000,
                fps: 30,
            },
            start,
        );
        peer.next_keyframe_at = start;
        peer.request_keyframe(start);
        peer.reset_encoder = false;
        peer.request_keyframe(start + Duration::from_millis(999));
        assert!(!peer.reset_encoder);
        peer.request_keyframe(start + Duration::from_secs(1));
        assert!(peer.reset_encoder);
        assert_eq!(peer.diagnostics.keyframe_requests, 3);
        peer.observe_bandwidth(
            BweKind::Remb {
                estimate: Bitrate::bps(3_000_000),
                mid: peer.mid.unwrap(),
            },
            start + Duration::from_millis(200),
        );
        peer.tick_at(
            &config(),
            &tick_socket(),
            start + Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation {
                kbps: 2400,
                fps: 24
            }
        );
        assert_eq!(peer.allocation.latest().unwrap().kind, EstimateKind::Remb);
    }

    #[test]
    fn peer_overflow_estimate_caps_before_narrowing() {
        let (mut peer, _) = offline_peer();
        peer.observe_bandwidth(
            BweKind::Twcc {
                estimate: Bitrate::bps(u64::MAX),
                can_probe: true,
            },
            Instant::now(),
        );
        assert_eq!(
            peer.allocation.candidate(),
            Some(Allocation {
                kbps: 3000,
                fps: 30
            })
        );
        assert_eq!(peer.allocation.pending(), None);
        peer.observe_bandwidth(
            BweKind::Twcc {
                estimate: Bitrate::bps(3_000_000),
                can_probe: true,
            },
            Instant::now(),
        );
        peer.tick_at(
            &config(),
            &tick_socket(),
            Instant::now() + Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert_eq!(
            peer.allocation.applied().allocation,
            Allocation {
                kbps: 2400,
                fps: 24
            }
        );
    }

    #[test]
    #[ignore = "requires local FFmpeg with libx264"]
    fn peer_encoder_startup_uses_allocation_committed_by_regular_tick() {
        let (mut peer, _) = offline_peer();
        let start = Instant::now();
        peer.observe_bandwidth(
            BweKind::Twcc {
                estimate: Bitrate::bps(1_000_000),
                can_probe: true,
            },
            start,
        );
        let mut config = config();
        config.encoder = crate::encoder::EncoderKind::Software;
        peer.tick_at(
            &config,
            &tick_socket(),
            start + Duration::from_secs(1),
            None,
        )
        .unwrap();
        peer.start_encoder(&config, 64, 64).unwrap();
        assert_eq!(
            peer.allocation.applied().committed_at,
            start + Duration::from_secs(1)
        );
        assert_eq!(
            peer.diagnostics.encoder_allocation,
            Some(Allocation { kbps: 800, fps: 15 })
        );
        let encoder = peer.encoder.as_ref().unwrap();
        let identity = FrameIdentity {
            timestamp: 90,
            generation: 7,
            content: 2,
            captured: Instant::now(),
        };
        encoder
            .submit(RawFrame {
                pixels: vec![20; 64 * 64 * 4].into(),
                identity,
                ticks: 90,
            })
            .unwrap();
        let frame = encoder.output.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (
                frame.identity.timestamp,
                frame.identity.generation,
                frame.identity.content,
                frame.ticks
            ),
            (90, 7, 2, 90)
        );
        assert!(!frame.bytes.is_empty());
        let mut configurations = Vec::new();
        for task in std::fs::read_dir("/proc/self/task").unwrap() {
            let children = std::fs::read_to_string(task.unwrap().path().join("children")).unwrap();
            for child in children.split_whitespace() {
                let Ok(command) = std::fs::read(format!("/proc/{child}/cmdline")) else {
                    continue;
                };
                let args = command
                    .split(|byte| *byte == 0)
                    .filter_map(|arg| std::str::from_utf8(arg).ok())
                    .collect::<Vec<_>>();
                if args
                    .first()
                    .is_none_or(|program| !program.ends_with("ffmpeg"))
                {
                    continue;
                }
                let argument = |flag| {
                    args.windows(2)
                        .find(|pair| pair[0] == flag)
                        .map(|pair| pair[1].to_string())
                };
                if argument("-video_size").as_deref() == Some("64x64")
                    && argument("-b:v").as_deref() == Some("800k")
                {
                    configurations.push((
                        argument("-framerate"),
                        argument("-b:v"),
                        argument("-maxrate"),
                        argument("-g"),
                    ));
                }
            }
        }
        assert_eq!(
            configurations,
            vec![(
                Some("15".into()),
                Some("800k".into()),
                Some("800k".into()),
                Some("450".into())
            )]
        );
        assert_eq!(encoder.fault(), None);
        peer.encoder = None;
        assert!(peer.encoder_workers.wait(Duration::from_secs(2)));
        assert_eq!(peer.diagnostics.encoder_starts, 1);
    }

    #[test]
    fn browser_offer_negotiates_without_capture_or_network_io() {
        let (peer, answer) = offline_peer();
        assert!(answer.contains("a=sendonly"));
        assert!(peer.desktop.is_none());
        assert!(peer.encoder.is_none());
        assert!(!peer.connected);
    }
    #[test]
    fn input_permits_require_channel_authentication_known_lock_and_matching_generation() {
        let (mut peer, _) = offline_peer();
        let now = Instant::now();
        let mut progress = DisplayProgress::new(1, now);
        progress.captured(1, now);
        progress.sent(FrameIdentity {
            timestamp: 42,
            generation: 1,
            content: 1,
            captured: now,
        });
        assert!(progress.acknowledge(1, 42, now));
        peer.progress = Some(progress);
        peer.generation = 1;
        peer.heartbeat = now;
        peer.lock = LockState::Unlocked;
        assert!(peer.admit_input(1, None).is_none());
        peer.authenticated_channel = true;
        assert!(peer.admit_input(1, None).is_some());
        assert!(peer.admit_input(2, None).is_none());
        peer.lock = LockState::Unknown;
        assert!(peer.admit_input(1, None).is_none());
        peer.lock = LockState::Locked;
        assert!(peer.admit_input(1, None).is_some());
        peer.heartbeat = now - DEADMAN;
        assert!(peer.admit_input(1, None).is_none());
    }

    fn fresh_peer(now: Instant) -> Peer {
        let (mut peer, _) = offline_peer();
        let mut progress = DisplayProgress::new(1, now);
        progress.captured(1, now);
        progress.sent(FrameIdentity {
            timestamp: 42,
            generation: 1,
            content: 1,
            captured: now,
        });
        assert!(progress.acknowledge(1, 42, now));
        peer.progress = Some(progress);
        peer.generation = 1;
        peer.authenticated_channel = true;
        peer.heartbeat = now;
        peer.lock = LockState::Unlocked;
        peer
    }
    #[test]
    fn clipboard_capability_requires_fresh_unlocked_display_but_not_native_input_authority() {
        let now = Instant::now();
        let mut peer = fresh_peer(now);
        assert_eq!(
            ControlAvailability::new(peer.current_display(now), false, peer.lock),
            ControlAvailability {
                input: false,
                clipboard: true
            }
        );
        assert_eq!(
            ControlAvailability::new(true, true, LockState::Locked),
            ControlAvailability {
                input: true,
                clipboard: false
            }
        );
        assert_eq!(
            ControlAvailability::new(true, true, LockState::Unknown),
            ControlAvailability {
                input: false,
                clipboard: false
            }
        );
        assert_eq!(
            peer.control_availability(now),
            ControlAvailability {
                input: false,
                clipboard: false
            }
        );
        peer.progress
            .as_mut()
            .unwrap()
            .captured(2, now + Duration::from_millis(100));
        assert_eq!(
            ControlAvailability::new(
                peer.current_display(now + Duration::from_millis(499)),
                false,
                peer.lock
            ),
            ControlAvailability {
                input: false,
                clipboard: true
            }
        );
        assert_eq!(
            ControlAvailability::new(
                peer.current_display(now + Duration::from_millis(500)),
                true,
                peer.lock
            ),
            ControlAvailability {
                input: false,
                clipboard: false
            }
        );
        peer = fresh_peer(now);
        assert!(peer.current_display(now + Duration::from_millis(749)));
        assert!(!peer.current_display(now + Duration::from_millis(750)));
        peer.progress
            .as_mut()
            .unwrap()
            .captured(1, now + Duration::from_millis(2990));
        peer.heartbeat = now + Duration::from_millis(2990);
        assert!(peer.current_display(now + Duration::from_millis(2999)));
        assert!(!peer.current_display(now + DEADMAN));
        peer = fresh_peer(now + DEADMAN);
        peer.heartbeat = now;
        assert!(!peer.current_display(now + DEADMAN));
        peer = fresh_peer(now);
        peer.generation = 2;
        assert!(!peer.current_display(now));
        peer.generation = 1;
        peer.authenticated_channel = false;
        assert!(!peer.current_display(now));
        peer.authenticated_channel = true;
        peer.phase = PeerPhase::Resetting {
            deadline: now + RESET_DRAIN,
        };
        assert!(!peer.current_display(now));
    }
    #[test]
    fn host_cleanup_observer_reports_aggregate_failure_without_a_live_peer_and_deduplicates() {
        use crate::desktop_actor::{CleanupFailure, CleanupFault};
        let (logger, receiver) = DiagnosticsLog::stalled();
        let health = CleanupHealth {
            completions: 4,
            failures: 2,
            last_failure: Some(CleanupFailure {
                actor_session: 3,
                sequence: 4,
                reason: CleanupFault::NativeConnection,
            }),
        };
        let mut observed = 0;
        observe_cleanup(health, &mut observed, Some(&logger), Instant::now());
        assert_eq!(observed, 2);
        observe_cleanup(health, &mut observed, Some(&logger), Instant::now());
        assert_eq!(logger.dropped(), 0);
        let record = serde_json::to_value(receiver.try_recv().unwrap()).unwrap();
        assert_eq!(record["event"], "native_release_failure");
        assert_eq!(record["cleanup"]["failures"], 2);
        assert_eq!(
            record["cleanup"]["last_failure"],
            json!({"actor_session":3,"sequence":4,"reason":"native_connection"})
        );
        assert!(receiver.try_recv().is_err());
        assert!(record.get("session").is_none());
        observe_cleanup(
            CleanupHealth {
                failures: 3,
                ..health
            },
            &mut observed,
            None,
            Instant::now(),
        );
        assert_eq!(observed, 3);
    }

    #[test]
    fn reset_revokes_display_and_only_retains_its_reset_message() {
        let (mut peer, _) = offline_peer();
        let now = Instant::now();
        peer.authenticated_channel = true;
        peer.ready = true;
        peer.lock = LockState::Unlocked;
        peer.generation = 1;
        peer.progress = Some(DisplayProgress::new(1, now));
        peer.queue(json!({"type":"frame","rtp":42})).unwrap();
        peer.begin_reset("Desktop lock state changed", LockState::Locked, now, None)
            .unwrap();
        assert!(!peer.ready);
        assert!(peer.desktop.is_none());
        assert!(peer.encoder.is_none());
        assert!(peer.progress.is_none());
        assert!(peer.geometry.is_none());
        assert_eq!(peer.lock, LockState::Locked);
        assert_eq!(peer.outbound.len(), 1);
        let message: serde_json::Value = serde_json::from_slice(&peer.outbound[0]).unwrap();
        assert_eq!(message["type"], "reset");
        assert_eq!(message["reason"], "Desktop lock state changed");
        assert!(peer.admit_input(1, None).is_none());
    }

    #[test]
    fn reset_ignores_commands_and_does_not_extend_its_deadline() {
        let (mut peer, _) = offline_peer();
        let now = Instant::now();
        peer.heartbeat = now - DEADMAN;
        let heartbeat = peer.heartbeat;
        peer.begin_reset("geometry", LockState::Unlocked, now, None)
            .unwrap();
        peer.process_command(Command::Heartbeat {}, None).unwrap();
        peer.process_command(
            Command::Hello {
                lease: "wrong".into(),
            },
            None,
        )
        .unwrap();
        peer.process_command(
            Command::ClipboardWrite {
                request: 1,
                text: "dummy text".into(),
            },
            None,
        )
        .unwrap();
        peer.process_command(
            Command::Pointer {
                revision: "0".into(),
                generation: 1,
                x: 0.5,
                y: 0.5,
            },
            None,
        )
        .unwrap();
        peer.begin_reset(
            "second reset",
            LockState::Unknown,
            now + Duration::from_secs(1),
            None,
        )
        .unwrap();
        assert_eq!(peer.heartbeat, heartbeat);
        assert!(
            matches!(peer.phase, PeerPhase::Resetting { deadline } if deadline == now + RESET_DRAIN)
        );
        assert_eq!(peer.outbound.len(), 1);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.connected = true;
        peer.capture(&config(), &socket, now, None).unwrap();
        assert!(peer.desktop.is_none());
        assert!(peer.encoder.is_none());
        assert_eq!(peer.frames, 0);
        peer.phase = PeerPhase::Resetting {
            deadline: now - Duration::from_millis(1),
        };
        assert_eq!(
            peer.tick(&config(), &socket, None).unwrap_err().to_string(),
            "reset drain expired"
        );
    }

    #[test]
    fn normal_offer_can_replace_only_a_resetting_peer_and_stale_stop_is_harmless() {
        let (mut peer, _) = offline_peer();
        assert!(!peer.accepts_offer(false));
        assert!(peer.accepts_offer(true));
        let lease = peer.lease.clone();
        assert!(peer.owns_lease(&lease));
        assert!(!peer.owns_lease("old lease"));
        peer.begin_reset("geometry", LockState::Unlocked, Instant::now(), None)
            .unwrap();
        assert!(peer.accepts_offer(false));
        assert!(peer.owns_lease(&lease));
        assert!(!peer.owns_lease("old lease"));
    }

    fn client_outputs(
        rtc: &mut Rtc,
        socket: &UdpSocket,
        messages: &mut Vec<String>,
        opened: &mut bool,
    ) {
        loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(_) => break,
                Output::Transmit(packet) => {
                    socket
                        .send_to(&packet.contents, packet.destination)
                        .unwrap();
                }
                Output::Event(Event::ChannelOpen(_, _)) => *opened = true,
                Output::Event(Event::ChannelData(data)) => {
                    messages.push(String::from_utf8(data.data).unwrap());
                }
                Output::Event(_) => {}
            }
        }
    }

    fn drive_client(
        rtc: &mut Rtc,
        socket: &UdpSocket,
        messages: &mut Vec<String>,
        opened: &mut bool,
    ) {
        rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
        client_outputs(rtc, socket, messages, opened);
        let mut buffer = [0u8; 65536];
        while let Ok((size, source)) = socket.recv_from(&mut buffer) {
            let received = Receive::new(
                Protocol::Udp,
                source,
                socket.local_addr().unwrap(),
                &buffer[..size],
            )
            .unwrap();
            let input = Input::Receive(Instant::now(), received);
            if rtc.accepts(&input) {
                rtc.handle_input(input).unwrap();
                client_outputs(rtc, socket, messages, opened);
            }
        }
    }

    fn drive_peer(peer: &mut Peer, socket: &UdpSocket) {
        let mut buffer = [0u8; 65536];
        while let Ok((size, source)) = socket.recv_from(&mut buffer) {
            let received = Receive::new(
                Protocol::Udp,
                source,
                socket.local_addr().unwrap(),
                &buffer[..size],
            )
            .unwrap();
            let input = Input::Receive(Instant::now(), received);
            if peer.rtc.accepts(&input) {
                peer.rtc.handle_input(input).unwrap();
                peer.drain(socket).unwrap();
            }
        }
    }

    fn loopback_peer() -> (Peer, Rtc, UdpSocket, UdpSocket) {
        use str0m::change::SdpAnswer;
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        client_socket.set_nonblocking(true).unwrap();
        let mut builder = Rtc::builder().clear_codecs();
        builder
            .codec_config()
            .add_h264(96.into(), Some(97.into()), true, 0x42e028);
        let mut client = builder.build(Instant::now());
        client.add_local_candidate(
            Candidate::host(client_socket.local_addr().unwrap(), "udp").unwrap(),
        );
        let mut messages = Vec::new();
        let mut opened = false;
        client_outputs(&mut client, &client_socket, &mut messages, &mut opened);
        let mut change = client.sdp_api();
        change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        change.add_channel("control".into());
        let (offer, pending) = change.apply().unwrap();
        let (mut peer, answer) = Peer::new(
            offer.to_sdp_string(),
            &config(),
            server.local_addr().unwrap(),
            Some(&server),
            RateMode::Adaptive,
        )
        .unwrap();
        client
            .sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap())
            .unwrap();
        let handshake_deadline = Instant::now() + Duration::from_secs(3);
        while peer.channel.is_none() || !opened {
            assert!(
                Instant::now() < handshake_deadline,
                "local RTC handshake exceeded its deadline"
            );
            drive_client(&mut client, &client_socket, &mut messages, &mut opened);
            drive_peer(&mut peer, &server);
            peer.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .unwrap();
            peer.drain(&server).unwrap();
            while let Some(event) = peer.events.pop_front() {
                if let Event::ChannelOpen(id, _) = event {
                    peer.channel = Some(id);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        for _ in 0..5 {
            drive_client(&mut client, &client_socket, &mut messages, &mut opened);
            drive_peer(&mut peer, &server);
        }
        (peer, client, server, client_socket)
    }

    #[test]
    fn guarded_real_channel_write_returns_backpressure_without_reentering_authority() {
        let (mut peer, _client, _server, _client_socket) = loopback_peer();
        let authority = known_authority();
        let guard = authority.clipboard_guard().unwrap();
        let mut channel = peer.rtc.channel(peer.channel.unwrap()).unwrap();
        assert!(channel.write(false, &vec![b'x'; 64 * 1024]).unwrap());
        assert_eq!(
            guard.with(|| channel.write(false, &vec![b't'; 64 * 1024]).unwrap()),
            Some(false)
        );
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Err(crate::desktop::LockQueryFailure::Deadline),
            elapsed_us: 150_000,
        });
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Ok(KnownLock::Unlocked),
            elapsed_us: 0,
        });
        let mut channel = peer.rtc.channel(peer.channel.unwrap()).unwrap();
        assert_eq!(
            guard.with(|| channel.write(false, b"stale text").unwrap()),
            None
        );
        let (mut fresh_peer, _client, _server, _client_socket) = loopback_peer();
        let mut channel = fresh_peer.rtc.channel(fresh_peer.channel.unwrap()).unwrap();
        assert_eq!(
            authority
                .clipboard_guard()
                .unwrap()
                .with(|| channel.write(false, b"fresh text").unwrap()),
            Some(true)
        );
    }
    #[test]
    fn completed_and_queued_clipboard_never_deliver_plaintext_across_unknown() {
        let (mut peer, mut client, server, client_socket) = loopback_peer();
        let authority = known_authority();
        let completed = authority.clipboard_guard().unwrap();
        peer.queue_clipboard(crate::desktop_actor::ClipboardReply {
            request: 71,
            text: Some("stale completed text".into()),
            ok: true,
            guard: completed.clone(),
        })
        .unwrap();
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Err(crate::desktop::LockQueryFailure::Deadline),
            elapsed_us: 150_000,
        });
        authority.observe_lock(crate::desktop::LockObservation {
            outcome: Ok(KnownLock::Unlocked),
            elapsed_us: 0,
        });
        peer.queue_clipboard(crate::desktop_actor::ClipboardReply {
            request: 72,
            text: Some("stale mailbox text".into()),
            ok: true,
            guard: completed,
        })
        .unwrap();
        authority.close();
        peer.queue_clipboard(crate::desktop_actor::ClipboardReply {
            request: 73,
            text: Some("fresh clipboard".into()),
            ok: true,
            guard: authority.clipboard_guard().unwrap(),
        })
        .unwrap();
        peer.flush_messages(&server).unwrap();
        let mut messages = Vec::new();
        let mut opened = true;
        let deadline = Instant::now() + Duration::from_secs(1);
        while messages.len() < 3 {
            assert!(Instant::now() < deadline);
            drive_client(&mut client, &client_socket, &mut messages, &mut opened);
            drive_peer(&mut peer, &server);
            std::thread::sleep(Duration::from_millis(1));
        }
        let received: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| serde_json::from_str(message).unwrap())
            .collect();
        assert_eq!(
            received,
            [
                json!({"type":"clipboard","request":71,"ok":false}),
                json!({"type":"clipboard","request":72,"ok":false}),
                json!({"type":"clipboard","request":73,"ok":true,"text":"fresh clipboard"}),
            ]
        );
    }
    #[test]
    fn stalled_diagnostic_consumer_does_not_stop_the_real_rtc_control_channel() {
        let (mut peer, mut client, server, client_socket) = loopback_peer();
        let (logger, _stalled_consumer) = DiagnosticsLog::stalled();
        for _ in 0..100 {
            peer.log_snapshot(Some(&logger), Instant::now());
        }
        assert_eq!(logger.dropped(), 99);
        peer.queue(json!({"type":"diagnostics_probe"})).unwrap();
        peer.flush_messages(&server).unwrap();
        let mut messages: Vec<String> = Vec::new();
        let mut opened = true;
        let deadline = Instant::now() + Duration::from_secs(1);
        while !messages.iter().any(|message| {
            serde_json::from_str::<serde_json::Value>(message).unwrap()["type"]
                == "diagnostics_probe"
        }) {
            assert!(Instant::now() < deadline);
            peer.log_snapshot(Some(&logger), Instant::now());
            drive_client(&mut client, &client_socket, &mut messages, &mut opened);
            drive_peer(&mut peer, &server);
            peer.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .unwrap();
            peer.drain(&server).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(logger.dropped() >= 100);
    }

    #[test]
    fn overdue_pacer_timer_releases_the_complete_fragmented_video_frame() {
        let (mut peer, _client, server, client_socket) = loopback_peer();
        let mut buffer = [0u8; 65536];
        while client_socket.recv_from(&mut buffer).is_ok() {}
        let now = Instant::now();
        peer.rtc.handle_input(Input::Timeout(now)).unwrap();
        peer.drain(&server).unwrap();
        let writer = peer.rtc.writer(peer.mid.unwrap()).unwrap();
        let pt = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::H264)
            .unwrap()
            .pt();
        let mut frame = vec![0x55; 3000];
        frame[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
        writer
            .write(
                pt,
                now,
                MediaTime::new(90_000, Frequency::NINETY_KHZ),
                frame,
            )
            .unwrap();
        client_outputs(&mut peer.rtc, &server, &mut Vec::new(), &mut false);
        let (log, receiver) = DiagnosticsLog::stalled();
        peer.log_snapshot(Some(&log), Instant::now());
        let crate::diagnostics::LogRecord::Peer(record) = receiver.try_recv().unwrap() else {
            panic!("peer diagnostic record required");
        };
        assert!(record.pacer_queue.is_some());
        let generation = peer.generation;
        let readiness = peer.readiness_record(
            Instant::now(),
            ReadinessEvent::StateQueued,
            ReadinessReason::None,
            false,
            generation,
        );
        assert!(readiness.pacer_queue.is_some());
        std::thread::sleep(Duration::from_millis(40));
        peer.rtc
            .handle_input(Input::Timeout(Instant::now()))
            .unwrap();
        peer.drain(&server).unwrap();
        let receive_deadline = Instant::now() + Duration::from_secs(1);
        let mut video_packets = Vec::new();
        loop {
            while let Ok((size, _)) = client_socket.recv_from(&mut buffer) {
                if size >= 12 && buffer[0] >> 6 == 2 && buffer[1] & 0x7f == *pt {
                    assert_eq!(u32::from_be_bytes(buffer[4..8].try_into().unwrap()), 90_000);
                    video_packets.push((
                        u16::from_be_bytes(buffer[2..4].try_into().unwrap()),
                        buffer[1] & 0x80 != 0,
                    ));
                }
            }
            if video_packets.len() >= 3 && video_packets.last().unwrap().1 {
                break;
            }
            let now = Instant::now();
            assert!(
                now < receive_deadline,
                "fragmented video frame exceeded its receive deadline: {video_packets:?}"
            );
            std::thread::sleep(
                peer.deadline
                    .min(receive_deadline)
                    .saturating_duration_since(now),
            );
            let now = Instant::now();
            if now >= peer.deadline {
                peer.rtc.handle_input(Input::Timeout(now)).unwrap();
                peer.drain(&server).unwrap();
            }
        }
        assert_eq!(video_packets.len(), 3);
        assert_eq!(
            video_packets
                .iter()
                .map(|packet| packet.1)
                .collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(video_packets[1].0, video_packets[0].0.wrapping_add(1));
        assert_eq!(video_packets[2].0, video_packets[1].0.wrapping_add(1));
    }

    #[test]
    fn reset_is_retransmitted_after_its_first_udp_delivery_is_lost() {
        let (mut peer, mut client, server, client_socket) = loopback_peer();
        let config = config();
        let mut messages: Vec<String> = Vec::new();
        let mut opened = true;
        peer.connected = true;
        peer.authenticated_channel = true;
        peer.heartbeat = Instant::now() - DEADMAN;
        peer.begin_reset(
            "dummy geometry change",
            LockState::Unlocked,
            Instant::now(),
            None,
        )
        .unwrap();
        peer.flush_messages(&server).unwrap();
        assert!(
            peer.outbound.is_empty(),
            "SCTP must accept ownership of the reliable reset"
        );
        let mut dropped = 0;
        let mut buffer = [0u8; 65536];
        while client_socket.recv_from(&mut buffer).is_ok() {
            dropped += 1;
        }
        assert!(
            dropped > 0,
            "the first reset transmission must actually be dropped"
        );
        let deadline = Instant::now() + RESET_DRAIN;
        while !messages
            .iter()
            .any(|message| message.contains("dummy geometry change"))
        {
            assert!(
                Instant::now() < deadline,
                "reset was not retransmitted within its drain deadline"
            );
            drive_client(&mut client, &client_socket, &mut messages, &mut opened);
            drive_peer(&mut peer, &server);
            peer.tick(&config, &server, None).unwrap();
            assert!(peer.desktop.is_none());
            assert!(peer.encoder.is_none());
            assert!(!peer.ready);
            std::thread::sleep(Duration::from_millis(2));
        }
        let message: serde_json::Value = serde_json::from_str(messages.last().unwrap()).unwrap();
        assert_eq!(message["type"], "reset");
        assert_eq!(message["reason"], "dummy geometry change");
    }

    #[test]
    fn escaped_clipboard_text_reports_a_transfer_limit_instead_of_stopping_the_session() {
        let reply = clipboard_reply(4, Some("\n".repeat(64 * 1024)));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["request"], 4);
        assert!(reply["error"].as_str().unwrap().contains("transfer limit"));
        let ordinary = clipboard_reply(5, Some("a".repeat(64 * 1024)));
        assert_eq!(ordinary["ok"], true);
        assert_eq!(ordinary["text"].as_str().unwrap().len(), 64 * 1024);
    }
}
