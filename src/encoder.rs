use crate::session::FrameIdentity;
use anyhow::{Result, bail};
use clap::ValueEnum;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum EncoderKind {
    Vaapi,
    Software,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncoderFault {
    #[error("encoder input deadline")]
    InputDeadline,
    #[error("encoder output deadline")]
    OutputDeadline,
    #[error("encoder metadata backlog")]
    MetadataBacklog,
    #[error("encoder output queue")]
    OutputQueue,
    #[error("encoder process exit")]
    ProcessExit,
    #[error("encoder IO")]
    Io,
    #[error("encoder worker limit")]
    WorkerLimit,
    #[error("encoder framing")]
    Framing,
    #[error("encoder operation")]
    Other,
}

pub struct RawFrame {
    pub pixels: Arc<[u8]>,
    pub identity: FrameIdentity,
    pub ticks: u64,
}

#[derive(Clone, Copy, Default)]
pub struct EncodingTimings {
    pub capture_to_submit_ms: u64,
    pub capture_to_selected_ms: u64,
    pub raw_write_ms: u64,
    pub raw_accepted: u64,
    pub raw_replaced: u64,
    pub raw_contended: u64,
    pub submit_to_output_ms: u64,
    pub capture_to_output_ms: u64,
}

pub struct EncodedFrame {
    pub timings: EncodingTimings,
    pub bytes: Arc<[u8]>,
    pub identity: FrameIdentity,
    pub ticks: u64,
}

pub struct Encoder {
    input: Arc<RawMailbox>,
    pub output: Receiver<EncodedFrame>,
    fault: Arc<Mutex<Option<EncoderFault>>>,
    stop: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Accepted,
    Replaced,
    Contended,
}

#[derive(Default)]
struct RawMailbox {
    frame: Mutex<Option<RawFrame>>,
    accepted: AtomicU64,
    replaced: AtomicU64,
    contended: AtomicU64,
}
impl RawMailbox {
    fn submit(
        &self,
        frame: RawFrame,
        stop: &AtomicBool,
    ) -> std::result::Result<SubmitOutcome, EncoderFault> {
        let mut slot = match self.frame.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => {
                self.contended.fetch_add(1, Ordering::Relaxed);
                return Ok(SubmitOutcome::Contended);
            }
            Err(TryLockError::Poisoned(_)) => return Err(EncoderFault::Other),
        };
        if stop.load(Ordering::Acquire) {
            return Err(EncoderFault::Other);
        }
        let displaced = slot.replace(frame);
        drop(slot);
        let outcome = if displaced.is_some() {
            self.replaced.fetch_add(1, Ordering::Relaxed);
            SubmitOutcome::Replaced
        } else {
            self.accepted.fetch_add(1, Ordering::Relaxed);
            SubmitOutcome::Accepted
        };
        drop(displaced);
        Ok(outcome)
    }
    fn take(&self) -> std::result::Result<Option<RawFrame>, EncoderFault> {
        Ok(self.frame.lock().map_err(|_| EncoderFault::Other)?.take())
    }
}

struct PendingRaw {
    frame: RawFrame,
    offset: usize,
    began: Instant,
    selected: Instant,
    first_write: Option<Instant>,
}
struct SubmittedRaw {
    identity: FrameIdentity,
    ticks: u64,
    selected: Instant,
    first_write: Instant,
    submitted: Instant,
}
enum RawWriteStep {
    Idle,
    Progress,
    Complete(SubmittedRaw),
}
fn write_raw(
    frames: &RawMailbox,
    pending: &mut Option<PendingRaw>,
    input: &mut impl Write,
    frame_bytes: usize,
    now: Instant,
) -> std::result::Result<RawWriteStep, EncoderFault> {
    if pending
        .as_ref()
        .is_some_and(|p| now.saturating_duration_since(p.began) > Duration::from_secs(1))
    {
        return Err(EncoderFault::InputDeadline);
    }
    if pending.as_ref().is_none_or(|p| p.offset == 0) {
        if let Some(frame) = frames.take()? {
            if frame.pixels.len() != frame_bytes {
                return Err(EncoderFault::Other);
            }
            let began = pending.as_ref().map_or(now, |p| p.began);
            *pending = Some(PendingRaw {
                frame,
                offset: 0,
                began,
                selected: now,
                first_write: None,
            });
        }
    }
    let Some(current) = pending.as_mut() else {
        return Ok(RawWriteStep::Idle);
    };
    let started = Instant::now();
    match input.write(&current.frame.pixels[current.offset..]) {
        Ok(0) => Err(EncoderFault::ProcessExit),
        Ok(n) => {
            current.first_write.get_or_insert(started);
            current.offset += n;
            if current.offset != current.frame.pixels.len() {
                return Ok(RawWriteStep::Progress);
            }
            let finished = pending.take().unwrap();
            Ok(RawWriteStep::Complete(SubmittedRaw {
                identity: finished.frame.identity,
                ticks: finished.frame.ticks,
                selected: finished.selected,
                first_write: finished.first_write.unwrap(),
                submitted: Instant::now(),
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(RawWriteStep::Idle),
        Err(_) => Err(EncoderFault::Io),
    }
}

#[derive(Clone, Default)]
pub struct EncoderWorkers(Arc<Mutex<Vec<Arc<AtomicBool>>>>);
impl EncoderWorkers {
    fn reserve(&self) -> Result<WorkerCompletion> {
        let mut workers = self.0.lock().unwrap();
        workers.retain(|done| !done.load(Ordering::Acquire));
        if workers.len() >= 4 {
            return Err(EncoderFault::WorkerLimit.into());
        }
        let done = Arc::new(AtomicBool::new(false));
        workers.push(done.clone());
        Ok(WorkerCompletion(done))
    }
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|done| done.load(Ordering::Acquire))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
#[derive(Debug)]
struct WorkerCompletion(Arc<AtomicBool>);
impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Encoder {
    pub fn start(
        kind: EncoderKind,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    ) -> Result<Self> {
        Self::start_tracked(
            kind,
            width,
            height,
            bitrate,
            fps,
            &EncoderWorkers::default(),
        )
    }
    pub fn start_tracked(
        kind: EncoderKind,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        workers: &EncoderWorkers,
    ) -> Result<Self> {
        if width == 0
            || height == 0
            || width > 4096
            || height > 2160
            || width % 2 != 0
            || height % 2 != 0
        {
            bail!("H.264 output must have even dimensions within 4096 by 2160");
        }
        let mut command = Command::new("ffmpeg");
        command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if matches!(kind, EncoderKind::Vaapi) {
            command.args(["-vaapi_device", "/dev/dri/by-path/pci-0000:00:02.0-render"]);
        }
        command.args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &format!("{width}x{height}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-map",
            "0:v",
            "-an",
        ]);
        match kind {
            EncoderKind::Vaapi => {
                command.args([
                    "-vf",
                    "format=nv12,hwupload",
                    "-c:v",
                    "h264_vaapi",
                    "-async_depth",
                    "1",
                    "-profile:v",
                    "constrained_baseline",
                    "-level:v",
                    "40",
                ]);
            }
            EncoderKind::Software => {
                command.args([
                    "-vf",
                    "format=yuv420p",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-profile:v",
                    "baseline",
                    "-level:v",
                    "4.0",
                    "-x264-params",
                    "repeat-headers=1:scenecut=0",
                    "-threads",
                    "2",
                ]);
            }
        }
        command.args([
            "-bf",
            "0",
            "-g",
            &(fps * 30).to_string(),
            "-b:v",
            &format!("{bitrate}k"),
            "-maxrate",
            &format!("{bitrate}k"),
            "-bufsize",
            &format!("{}k", bitrate / 2),
            "-bsf:v",
            "h264_metadata=aud=insert",
            "-flush_packets",
            "1",
            "-f",
            "tee",
            "[f=h264:flush_packets=1]pipe:1|[f=framecrc:flush_packets=1]pipe:2",
        ]);
        let input = Arc::new(RawMailbox::default());
        let frames = input.clone();
        let (encoded, output) = mpsc::sync_channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let worker_stop = stop.clone();
        let worker_fault = fault.clone();
        let completion = workers.reserve()?;
        thread::Builder::new()
            .name("h264-encoder".into())
            .spawn(move || {
                let _completion = completion;
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                let child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();
                let result = child
                    .map_err(|_| EncoderFault::ProcessExit)
                    .and_then(|child| {
                        encode_loop(
                            child,
                            &frames,
                            encoded,
                            &worker_stop,
                            (width as usize) * (height as usize) * 4,
                        )
                    });
                if let Err(fault) = result {
                    *worker_fault.lock().unwrap() = Some(fault);
                }
                worker_stop.store(true, Ordering::Release);
            })?;
        Ok(Self {
            input,
            output,
            fault,
            stop,
        })
    }

    pub fn submit(&self, frame: RawFrame) -> std::result::Result<SubmitOutcome, EncoderFault> {
        if self.stop.load(Ordering::Acquire) {
            return Err(self.fault().unwrap_or(EncoderFault::Other));
        }
        self.input
            .submit(frame, &self.stop)
            .map_err(|fault| self.fault().unwrap_or(fault))
    }

    pub fn fault(&self) -> Option<EncoderFault> {
        *self.fault.lock().unwrap()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn deliver_frame(
    encoded: &SyncSender<EncodedFrame>,
    mut frame: EncodedFrame,
    stop: &AtomicBool,
    timeout: Duration,
) -> std::result::Result<Option<Duration>, EncoderFault> {
    let mut began = None;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        if began.is_some_and(|at: Instant| at.elapsed() >= timeout) {
            return Err(EncoderFault::OutputQueue);
        }
        match encoded.try_send(frame) {
            Ok(()) => return Ok(Some(began.map_or(Duration::ZERO, |at| at.elapsed()))),
            Err(TrySendError::Full(rejected)) => {
                frame = rejected;
                began.get_or_insert_with(Instant::now);
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => return Ok(None),
        }
    }
}

fn nonblocking(fd: &impl std::os::fd::AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

fn encode_loop(
    child: Child,
    frames: &RawMailbox,
    encoded: SyncSender<EncodedFrame>,
    stop: &AtomicBool,
    frame_bytes: usize,
) -> std::result::Result<(), EncoderFault> {
    let mut child = ChildGuard(child);
    let mut input = child.0.stdin.take().ok_or(EncoderFault::Other)?;
    let mut output = child.0.stdout.take().ok_or(EncoderFault::Other)?;
    nonblocking(&input).map_err(|_| EncoderFault::Io)?;
    let _ = rustix::pipe::fcntl_setpipe_size(&input, 1024 * 1024);
    nonblocking(&output).map_err(|_| EncoderFault::Io)?;
    let mut ledger = child.0.stderr.take().ok_or(EncoderFault::Other)?;
    nonblocking(&ledger).map_err(|_| EncoderFault::Io)?;
    let mut parser = PacketFramer::default();
    let mut metadata = VecDeque::new();
    let mut pending: Option<PendingRaw> = None;
    let mut last_progress = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut progressed = false;
        let mut buffer = [0u8; 64 * 1024];
        for (reader, is_ledger) in [
            (&mut output as &mut dyn Read, false),
            (&mut ledger as &mut dyn Read, true),
        ] {
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        return Err(if parser.incomplete() {
                            EncoderFault::Framing
                        } else {
                            EncoderFault::ProcessExit
                        });
                    }
                    Ok(n) => {
                        progressed = true;
                        if is_ledger {
                            parser.push_ledger(&buffer[..n])?;
                        } else {
                            parser.push_payload(&buffer[..n])?;
                        }
                        while let Some(bytes) = parser.take_packet()? {
                            let submitted: SubmittedRaw =
                                metadata.pop_front().ok_or(EncoderFault::Framing)?;
                            let identity = submitted.identity;
                            let ticks = submitted.ticks;
                            let completed = Instant::now();
                            let Some(wait) = deliver_frame(
                                &encoded,
                                EncodedFrame {
                                    bytes: bytes.into(),
                                    identity,
                                    ticks,
                                    timings: EncodingTimings {
                                        capture_to_submit_ms: submitted
                                            .submitted
                                            .saturating_duration_since(identity.captured)
                                            .as_millis()
                                            as u64,
                                        capture_to_selected_ms: submitted
                                            .selected
                                            .saturating_duration_since(identity.captured)
                                            .as_millis()
                                            as u64,
                                        raw_write_ms: submitted
                                            .submitted
                                            .saturating_duration_since(submitted.first_write)
                                            .as_millis()
                                            as u64,
                                        raw_accepted: frames.accepted.load(Ordering::Relaxed),
                                        raw_replaced: frames.replaced.load(Ordering::Relaxed),
                                        raw_contended: frames.contended.load(Ordering::Relaxed),
                                        submit_to_output_ms: completed
                                            .saturating_duration_since(submitted.submitted)
                                            .as_millis()
                                            as u64,
                                        capture_to_output_ms: completed
                                            .saturating_duration_since(identity.captured)
                                            .as_millis()
                                            as u64,
                                    },
                                },
                                stop,
                                crate::session::DEADMAN,
                            )?
                            else {
                                return Ok(());
                            };
                            if let Some(pending) = pending.as_mut() {
                                pending.began += wait;
                            }
                            last_progress = Instant::now();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return Err(EncoderFault::Io),
                }
            }
        }
        match write_raw(
            frames,
            &mut pending,
            &mut input,
            frame_bytes,
            Instant::now(),
        )? {
            RawWriteStep::Idle => {}
            RawWriteStep::Progress => progressed = true,
            RawWriteStep::Complete(submitted) => {
                progressed = true;
                metadata.push_back(submitted);
                if metadata.len() > 4 {
                    return Err(EncoderFault::MetadataBacklog);
                }
            }
        }
        if progressed {
            last_progress = Instant::now();
        }
        if !metadata.is_empty() && last_progress.elapsed() > Duration::from_secs(1) {
            return Err(EncoderFault::OutputDeadline);
        }
        if child.0.try_wait().map_err(|_| EncoderFault::Io)?.is_some() {
            return Err(EncoderFault::ProcessExit);
        }
        if !progressed {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_LEDGER_LINE: usize = 4096;
const MAX_LEDGER_ROWS: usize = 4;

#[derive(Debug, PartialEq, Eq)]
struct PacketBoundary {
    size: usize,
    checksum: u32,
}
#[derive(Default)]
struct PacketFramer {
    payload: Vec<u8>,
    line: Vec<u8>,
    rows: VecDeque<PacketBoundary>,
    next_pts: u64,
}
impl PacketFramer {
    fn push_payload(&mut self, bytes: &[u8]) -> std::result::Result<(), EncoderFault> {
        if bytes.len() > MAX_PACKET_BYTES.saturating_sub(self.payload.len()) {
            return Err(EncoderFault::Framing);
        }
        self.payload.extend_from_slice(bytes);
        Ok(())
    }
    fn push_ledger(&mut self, bytes: &[u8]) -> std::result::Result<(), EncoderFault> {
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::str::from_utf8(&self.line).map_err(|_| EncoderFault::Framing)?;
                if !valid_header(line) {
                    let row = parse_boundary(line, self.next_pts)?;
                    if self.rows.len() >= MAX_LEDGER_ROWS {
                        return Err(EncoderFault::Framing);
                    }
                    self.rows.push_back(row);
                    self.next_pts = self.next_pts.checked_add(1).ok_or(EncoderFault::Framing)?;
                }
                self.line.clear();
            } else {
                if self.line.len() >= MAX_LEDGER_LINE {
                    return Err(EncoderFault::Framing);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }
    fn take_packet(&mut self) -> std::result::Result<Option<Vec<u8>>, EncoderFault> {
        let Some(row) = self.rows.front() else {
            return Ok(None);
        };
        if self.payload.len() < row.size {
            return Ok(None);
        }
        let mut checksum = adler2::Adler32::from_checksum(0);
        checksum.write_slice(&self.payload[..row.size]);
        if checksum.checksum() != row.checksum {
            return Err(EncoderFault::Framing);
        }
        let bytes = self.payload.drain(..row.size).collect();
        self.rows.pop_front();
        Ok(Some(bytes))
    }
    fn incomplete(&self) -> bool {
        !self.payload.is_empty() || !self.rows.is_empty() || !self.line.is_empty()
    }
}
fn decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}
fn hex(value: &str, digits: Option<usize>) -> Option<u32> {
    let value = value.strip_prefix("0x")?;
    if value.is_empty()
        || digits.is_some_and(|n| value.len() != n)
        || !value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    u32::from_str_radix(value, 16).ok()
}
fn valid_header(line: &str) -> bool {
    if let Some(version) = line.strip_prefix("#software: Lavf") {
        return version.split('.').count() == 3
            && version.split('.').all(|part| decimal(part).is_some());
    }
    if let Some(ratio) = line
        .strip_prefix("#tb 0: ")
        .or_else(|| line.strip_prefix("#sar 0: "))
    {
        return ratio
            .split_once('/')
            .is_some_and(|(a, b)| decimal(a).is_some() && decimal(b).is_some_and(|n| n != 0));
    }
    if let Some(size) = line.strip_prefix("#dimensions 0: ") {
        return size.split_once('x').is_some_and(|(a, b)| {
            decimal(a).is_some_and(|n| n != 0) && decimal(b).is_some_and(|n| n != 0)
        });
    }
    matches!(line, "#media_type 0: video" | "#codec_id 0: h264")
}
fn parse_boundary(
    line: &str,
    expected_pts: u64,
) -> std::result::Result<PacketBoundary, EncoderFault> {
    let fields: Vec<_> = line.split(',').map(str::trim).collect();
    if fields.len() < 6
        || decimal(fields[0]) != Some(0)
        || decimal(fields[1]) != Some(expected_pts)
        || decimal(fields[2]) != Some(expected_pts)
        || decimal(fields[3]) != Some(1)
    {
        return Err(EncoderFault::Framing);
    }
    let size = decimal(fields[4])
        .filter(|n| *n != 0 && *n <= MAX_PACKET_BYTES as u64)
        .ok_or(EncoderFault::Framing)? as usize;
    let checksum = hex(fields[5], Some(8)).ok_or(EncoderFault::Framing)?;
    let mut index = 6;
    if let Some(flags) = fields.get(index).and_then(|value| value.strip_prefix("F=")) {
        hex(flags, None).ok_or(EncoderFault::Framing)?;
        index += 1;
    }
    if let Some(count) = fields.get(index).and_then(|value| value.strip_prefix("S=")) {
        let count = decimal(count)
            .filter(|n| *n > 0 && *n <= MAX_LEDGER_LINE as u64 / 3)
            .ok_or(EncoderFault::Framing)? as usize;
        index += 1;
        if fields.len() != index + count * 3 {
            return Err(EncoderFault::Framing);
        }
        for side in fields[index..].chunks_exact(3) {
            if side[0].is_empty()
                || side[0].bytes().any(|b| b.is_ascii_control())
                || decimal(side[1]).is_none()
                || hex(side[2], Some(8)).is_none()
            {
                return Err(EncoderFault::Framing);
            }
        }
        index = fields.len();
    }
    if fields.len() != index {
        return Err(EncoderFault::Framing);
    }
    Ok(PacketBoundary { size, checksum })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{desktop::Viewport, session::ViewportFrame};
    use std::sync::mpsc::TryRecvError;

    fn encoded_frame(timestamp: u32) -> EncodedFrame {
        EncodedFrame {
            timings: EncodingTimings::default(),
            bytes: vec![0, 0, 1, 0x41, timestamp as u8].into(),
            identity: FrameIdentity {
                timestamp,
                generation: 7,
                content: timestamp as u64,
                captured: Instant::now(),
                viewport: None,
            },
            ticks: timestamp as u64 * 3000,
        }
    }

    fn test_encoder() -> Encoder {
        let (_encoded, output) = mpsc::sync_channel(2);
        Encoder {
            input: Arc::new(RawMailbox::default()),
            output,
            fault: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
    fn raw_frame(timestamp: u32) -> RawFrame {
        RawFrame {
            pixels: vec![timestamp as u8; 4].into(),
            identity: FrameIdentity {
                timestamp,
                generation: 7,
                content: timestamp as u64,
                captured: Instant::now(),
                viewport: None,
            },
            ticks: timestamp as u64 * 3000,
        }
    }
    struct ScriptWriter {
        limits: VecDeque<usize>,
        bytes: Vec<u8>,
    }
    impl ScriptWriter {
        fn new(limits: impl IntoIterator<Item = usize>) -> Self {
            Self {
                limits: limits.into_iter().collect(),
                bytes: Vec::new(),
            }
        }
    }
    impl Write for ScriptWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let limit = self.limits.pop_front().expect("write schedule exhausted");
            if limit == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let count = limit.min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn completed(step: RawWriteStep) -> SubmittedRaw {
        match step {
            RawWriteStep::Complete(frame) => frame,
            _ => panic!("expected complete raw frame"),
        }
    }
    #[test]
    fn newest_accepted_raw_identity_is_selected() {
        let encoder = test_encoder();
        assert_eq!(encoder.submit(raw_frame(1)), Ok(SubmitOutcome::Accepted));
        assert_eq!(encoder.submit(raw_frame(2)), Ok(SubmitOutcome::Replaced));
        let mut newest = raw_frame(3);
        newest.identity.viewport = Some(ViewportFrame {
            seq: 2,
            viewport: Viewport::new(640, 360, 1280, 720, 1920, 1080).unwrap(),
        });
        assert_eq!(encoder.submit(newest), Ok(SubmitOutcome::Replaced));
        let mut input = ScriptWriter::new([4]);
        let selected =
            completed(write_raw(&encoder.input, &mut None, &mut input, 4, Instant::now()).unwrap());
        assert_eq!(selected.identity.timestamp, 3);
        assert_eq!(selected.identity.viewport.unwrap().seq, 2);
        assert_eq!(
            selected.identity.viewport.unwrap().viewport.origin(),
            (640, 360)
        );
        assert_eq!(selected.ticks, 9000);
        assert_eq!(input.bytes, [3, 3, 3, 3]);
        assert_eq!(encoder.input.accepted.load(Ordering::Relaxed), 1);
        assert_eq!(encoder.input.replaced.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn partially_written_pixels_and_identity_finish_before_newest_raw_frame() {
        let encoder = test_encoder();
        let first = raw_frame(1);
        let captured = first.identity.captured;
        encoder.submit(first).unwrap();
        let mut pending = None;
        let mut input = ScriptWriter::new([2, 2, 4]);
        assert!(matches!(
            write_raw(&encoder.input, &mut pending, &mut input, 4, Instant::now()).unwrap(),
            RawWriteStep::Progress
        ));
        assert_eq!(encoder.submit(raw_frame(2)), Ok(SubmitOutcome::Accepted));
        assert_eq!(encoder.submit(raw_frame(3)), Ok(SubmitOutcome::Replaced));
        let first = completed(
            write_raw(&encoder.input, &mut pending, &mut input, 4, Instant::now()).unwrap(),
        );
        let newest = completed(
            write_raw(&encoder.input, &mut pending, &mut input, 4, Instant::now()).unwrap(),
        );
        assert_eq!(input.bytes, [1, 1, 1, 1, 3, 3, 3, 3]);
        assert_eq!(
            (
                first.identity.timestamp,
                first.ticks,
                first.identity.captured
            ),
            (1, 3000, captured)
        );
        assert_eq!((newest.identity.timestamp, newest.ticks), (3, 9000));
        assert!(pending.is_none());
    }
    #[test]
    fn zero_byte_pending_frame_can_be_replaced_without_resetting_deadline() {
        let encoder = test_encoder();
        encoder.submit(raw_frame(1)).unwrap();
        let mut pending = None;
        let mut input = ScriptWriter::new([0, 4]);
        let began = Instant::now();
        assert!(matches!(
            write_raw(&encoder.input, &mut pending, &mut input, 4, began).unwrap(),
            RawWriteStep::Idle
        ));
        let replacement = raw_frame(3);
        let captured = replacement.identity.captured;
        encoder.submit(replacement).unwrap();
        let frame = completed(
            write_raw(
                &encoder.input,
                &mut pending,
                &mut input,
                4,
                began + Duration::from_millis(100),
            )
            .unwrap(),
        );
        assert_eq!(input.bytes, [3, 3, 3, 3]);
        assert_eq!(
            (
                frame.identity.timestamp,
                frame.ticks,
                frame.identity.captured
            ),
            (3, 9000, captured)
        );
    }
    #[test]
    fn repeated_zero_byte_replacements_cannot_extend_input_deadline() {
        let encoder = test_encoder();
        let began = Instant::now();
        let mut pending = None;
        let mut input = ScriptWriter::new([0, 0]);
        encoder.submit(raw_frame(1)).unwrap();
        write_raw(&encoder.input, &mut pending, &mut input, 4, began).unwrap();
        encoder.submit(raw_frame(2)).unwrap();
        write_raw(
            &encoder.input,
            &mut pending,
            &mut input,
            4,
            began + Duration::from_millis(600),
        )
        .unwrap();
        assert_eq!(pending.as_ref().unwrap().began, began);
        encoder.submit(raw_frame(3)).unwrap();
        assert!(matches!(
            write_raw(
                &encoder.input,
                &mut pending,
                &mut input,
                4,
                began + Duration::from_millis(1100)
            ),
            Err(EncoderFault::InputDeadline)
        ));
        assert!(input.bytes.is_empty());
    }
    #[test]
    fn caller_returns_contended_without_waiting_for_raw_slot() {
        let encoder = test_encoder();
        let slot = encoder.input.frame.lock().unwrap();
        assert_eq!(encoder.submit(raw_frame(3)), Ok(SubmitOutcome::Contended));
        assert!(slot.is_none());
        assert_eq!(encoder.input.contended.load(Ordering::Relaxed), 1);
        drop(slot);
        encoder.stop.store(true, Ordering::Release);
        assert_eq!(encoder.submit(raw_frame(4)), Err(EncoderFault::Other));
        *encoder.fault.lock().unwrap() = Some(EncoderFault::InputDeadline);
        assert_eq!(
            encoder.submit(raw_frame(5)),
            Err(EncoderFault::InputDeadline)
        );
    }

    #[test]
    fn encoder_worker_capacity_is_bounded_until_cleanup_completes() {
        let workers = EncoderWorkers::default();
        let mut pending = Vec::new();
        for _ in 0..4 {
            pending.push(workers.reserve().unwrap());
        }
        assert!(matches!(
            workers
                .reserve()
                .unwrap_err()
                .downcast_ref::<EncoderFault>(),
            Some(EncoderFault::WorkerLimit)
        ));
        assert!(!workers.wait(Duration::ZERO));
        pending.pop();
        let replacement = workers.reserve().unwrap();
        drop(pending);
        drop(replacement);
        assert!(workers.wait(Duration::ZERO));
    }

    #[test]
    fn cancelled_worker_reaps_its_owned_process_before_reporting_completion() {
        let child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let workers = EncoderWorkers::default();
        let completion = workers.reserve().unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let frames = RawMailbox::default();
        let (encoded, output) = mpsc::sync_channel(2);
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let _completion = completion;
            encode_loop(child, &frames, encoded, &worker_stop, 4).unwrap();
        });
        assert!(workers.wait(Duration::from_secs(1)));
        worker.join().unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        drop(output);
    }

    #[test]
    fn cancellation_reaps_process_when_encoded_output_is_full() {
        let child = Command::new("sh")
            .args(["-c", "dd bs=4 count=1 of=/dev/null 2>/dev/null; printf '\\000\\000\\001\\101\\003'; printf '0,0,0,1,5,0x00880045\\n' >&2; exec sleep 5"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let pid = child.id();
        let workers = EncoderWorkers::default();
        let completion = workers.reserve().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let frames = RawMailbox::default();
        let (encoded, output) = mpsc::sync_channel(2);
        encoded.send(encoded_frame(1)).unwrap();
        encoded.send(encoded_frame(2)).unwrap();
        let frame = encoded_frame(3);
        frames
            .submit(
                RawFrame {
                    pixels: Arc::from([0u8; 4]),
                    identity: frame.identity,
                    ticks: frame.ticks,
                },
                &stop,
            )
            .unwrap();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let _completion = completion;
            encode_loop(child, &frames, encoded, &worker_stop, 4).unwrap();
        });
        thread::sleep(Duration::from_millis(30));
        stop.store(true, Ordering::Release);
        assert!(workers.wait(Duration::from_secs(1)));
        worker.join().unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(output.try_recv().unwrap().identity.timestamp, 1);
        assert_eq!(output.try_recv().unwrap().identity.timestamp, 2);
        assert!(output.try_recv().is_err());
    }

    #[test]
    fn delayed_consumer_receives_every_frame_in_order() {
        let (sender, receiver) = mpsc::sync_channel(2);
        sender
            .try_send(encoded_frame(1))
            .unwrap_or_else(|_| panic!("queue first frame"));
        sender
            .try_send(encoded_frame(2))
            .unwrap_or_else(|_| panic!("queue second frame"));
        let (started, began) = mpsc::channel();
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            let wait = deliver_frame(
                &sender,
                encoded_frame(3),
                &AtomicBool::new(false),
                Duration::from_secs(1),
            )?;
            deliver_frame(
                &sender,
                encoded_frame(4),
                &AtomicBool::new(false),
                Duration::from_secs(1),
            )?;
            Ok::<_, EncoderFault>(wait)
        });
        began.recv().unwrap();
        thread::sleep(Duration::from_millis(40));
        for timestamp in 1..=4 {
            let frame = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(frame.identity.timestamp, timestamp);
            assert_eq!(frame.identity.generation, 7);
            assert_eq!(frame.identity.content, timestamp as u64);
            assert_eq!(frame.ticks, timestamp as u64 * 3000);
            assert_eq!(&*frame.bytes, &[0, 0, 1, 0x41, timestamp as u8]);
        }
        assert!(worker.join().unwrap().unwrap().unwrap() >= Duration::from_millis(20));
    }

    #[test]
    fn full_queue_can_be_cancelled_while_receiver_stays_alive() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(encoded_frame(1))
            .unwrap_or_else(|_| panic!("queue first frame"));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (result, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            result
                .send(deliver_frame(
                    &sender,
                    encoded_frame(2),
                    &worker_stop,
                    crate::session::DEADMAN,
                ))
                .unwrap();
        });
        thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);
        assert_eq!(
            completed.recv_timeout(Duration::from_millis(500)).unwrap(),
            Ok(None)
        );
        worker.join().unwrap();
        assert_eq!(receiver.recv().unwrap().identity.timestamp, 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn full_queue_expires_at_the_output_consumer_deadline() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(encoded_frame(1))
            .unwrap_or_else(|_| panic!("queue first frame"));
        let started = Instant::now();
        assert_eq!(
            deliver_frame(
                &sender,
                encoded_frame(2),
                &AtomicBool::new(false),
                crate::session::DEADMAN
            ),
            Err(EncoderFault::OutputQueue)
        );
        assert!(started.elapsed() >= crate::session::DEADMAN);
        assert!(started.elapsed() < crate::session::DEADMAN + Duration::from_secs(1));
        assert_eq!(receiver.recv().unwrap().identity.timestamp, 1);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn disconnected_output_consumer_stops_without_a_fault() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        assert_eq!(
            deliver_frame(
                &sender,
                encoded_frame(1),
                &AtomicBool::new(false),
                crate::session::DEADMAN
            ),
            Ok(None)
        );
    }

    fn live_encoder_round_trip(kind: EncoderKind) {
        let workers = EncoderWorkers::default();
        let encoder = Encoder::start_tracked(kind, 64, 64, 700, 8, &workers).unwrap();
        let mut bytes = Vec::new();
        for timestamp in [1, 4, 9, 16] {
            let identity = FrameIdentity {
                timestamp,
                generation: 7,
                content: timestamp as u64,
                captured: Instant::now(),
                viewport: None,
            };
            encoder
                .submit(RawFrame {
                    pixels: vec![timestamp as u8; 64 * 64 * 4].into(),
                    identity,
                    ticks: timestamp as u64 * 3000,
                })
                .unwrap();
            let frame = encoder.output.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(frame.identity.timestamp, timestamp);
            assert_eq!(frame.identity.generation, 7);
            assert_eq!(frame.identity.content, timestamp as u64);
            assert_eq!(frame.identity.captured, identity.captured);
            assert_eq!(frame.ticks, timestamp as u64 * 3000);
            assert!(!frame.bytes.is_empty());
            bytes.extend_from_slice(&frame.bytes);
        }
        assert_eq!(encoder.fault(), None);
        drop(encoder);
        assert!(workers.wait(Duration::from_secs(2)));
        let mut decoder = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-f",
                "framemd5",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        decoder.stdin.take().unwrap().write_all(&bytes).unwrap();
        let decoded = decoder.wait_with_output().unwrap();
        assert!(decoded.status.success());
        assert!(decoded.stderr.is_empty());
        let decoded = std::str::from_utf8(&decoded.stdout).unwrap();
        assert_eq!(
            decoded
                .lines()
                .filter(|line| !line.starts_with('#') && !line.is_empty())
                .count(),
            4
        );
    }
    #[test]
    #[ignore = "requires local Intel VAAPI and FFmpeg"]
    fn vaapi_publishes_a_standalone_first_frame_and_preserves_metadata() {
        live_encoder_round_trip(EncoderKind::Vaapi);
    }
    #[test]
    #[ignore = "requires local FFmpeg with libx264"]
    fn software_publishes_a_standalone_first_frame_and_decodes_every_packet() {
        live_encoder_round_trip(EncoderKind::Software);
    }

    #[test]
    fn first_packet_is_ready_without_a_second_frame_or_eof() {
        let mut parser = PacketFramer::default();
        parser.push_payload(&[0, 0, 1, 0x41, 3]).unwrap();
        parser.push_ledger(b"0,0,0,1,5,0x00880045\n").unwrap();
        assert_eq!(parser.take_packet().unwrap(), Some(vec![0, 0, 1, 0x41, 3]));
        assert_eq!(parser.take_packet().unwrap(), None);
        assert!(!parser.incomplete());
    }
    #[test]
    fn independent_pipe_splits_preserve_exact_packet_boundaries() {
        let payload = [0, 0, 1, 0x41, 3, 0, 0, 1, 0x41, 4];
        let ledger =
            b"0,0,0,1,5,0x00880045\n0,1,1,1,5,0x00890046,F=0x0,S=1,Quality stats,8,0x064c00cb\n";
        for payload_split in 0..=payload.len() {
            for ledger_split in 0..=ledger.len() {
                for ledger_first in [false, true] {
                    let mut parser = PacketFramer::default();
                    let mut result = Vec::new();
                    for suffix in [false, true] {
                        let p = if suffix {
                            &payload[payload_split..]
                        } else {
                            &payload[..payload_split]
                        };
                        let l = if suffix {
                            &ledger[ledger_split..]
                        } else {
                            &ledger[..ledger_split]
                        };
                        if ledger_first {
                            parser.push_ledger(l).unwrap();
                            parser.push_payload(p).unwrap();
                        } else {
                            parser.push_payload(p).unwrap();
                            parser.push_ledger(l).unwrap();
                        }
                        while let Some(packet) = parser.take_packet().unwrap() {
                            result.push(packet);
                        }
                    }
                    assert_eq!(result, vec![vec![0, 0, 1, 0x41, 3], vec![0, 0, 1, 0x41, 4]]);
                    assert!(!parser.incomplete());
                }
            }
        }
    }
    #[test]
    fn ledger_accepts_only_documented_headers_flags_and_side_data() {
        let mut parser = PacketFramer::default();
        parser.push_ledger(b"#software: Lavf63.1.101\n#tb 0: 1/8\n#media_type 0: video\n#codec_id 0: h264\n#dimensions 0: 1920x1080\n#sar 0: 1/1\n").unwrap();
        assert!(parser.rows.is_empty());
        for suffix in [
            "",
            ", F=0xAB",
            ", S=1, Quality stats, 8, 0x064c00cb",
            ", F=0x0, S=2, Quality stats, 8, 0x064c00cb, New extradata, 12, 0x064c00cb",
        ] {
            assert_eq!(
                parse_boundary(&format!("0,0,0,1,5,0x00880045{suffix}"), 0).unwrap(),
                PacketBoundary {
                    size: 5,
                    checksum: 0x00880045
                }
            );
        }
        for line in [
            "0,0,1,1,5,0x00880045",
            "0,1,1,1,5,0x00880045",
            "1,0,0,1,5,0x00880045",
            "0,0,0,0,5,0x00880045",
            "0,0,0,1,0,0x00880045",
            "0,0,0,1,16777217,0x00880045",
            "0,0,0,1,5,0x8a0045",
            "0,0,0,1,5,0x00880045,F=oops",
            "0,0,0,1,5,0x00880045,S=1,Quality stats,8",
            "0,0,0,1,5,0x00880045,S=0",
            "0,0,0,1,5,0x00880045,unknown",
        ] {
            assert_eq!(
                parse_boundary(line, 0),
                Err(EncoderFault::Framing),
                "{line}"
            );
        }
        for bad in [
            b"private ffmpeg error\n".as_slice(),
            b"#unknown header\n".as_slice(),
        ] {
            assert_eq!(
                PacketFramer::default().push_ledger(bad),
                Err(EncoderFault::Framing)
            );
        }
    }
    #[test]
    fn corrupt_or_oversized_pipe_data_is_a_typed_framing_fault() {
        let mut parser = PacketFramer::default();
        parser.push_ledger(b"0,0,0,1,5,0x00000000\n").unwrap();
        parser.push_payload(&[0, 0, 1, 0x41, 3]).unwrap();
        assert_eq!(parser.take_packet(), Err(EncoderFault::Framing));
        assert_eq!(
            PacketFramer::default().push_payload(&vec![1; MAX_PACKET_BYTES + 1]),
            Err(EncoderFault::Framing)
        );
        assert_eq!(
            PacketFramer::default().push_ledger(&vec![1; MAX_LEDGER_LINE + 1]),
            Err(EncoderFault::Framing)
        );
        let mut parser = PacketFramer::default();
        for index in 0..4 {
            parser
                .push_ledger(format!("0,{index},{index},1,5,0x00880045\n").as_bytes())
                .unwrap();
        }
        assert_eq!(
            parser.push_ledger(b"0,4,4,1,5,0x00880045\n"),
            Err(EncoderFault::Framing)
        );
    }
}
