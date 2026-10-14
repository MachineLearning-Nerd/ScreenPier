use memmap2::MmapMut;
use rustix::event::{PollFd, PollFlags, poll};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    os::fd::AsFd,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    },
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::{
    screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1},
    virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1},
};
use xkbcommon::xkb;

mod hyprland_pointer;
use hyprland_pointer::{PointerAdapter, Reply as PointerReply};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPreparationFault {
    Unsupported,
    Unavailable,
    Peer,
    ShortWrite,
    InvalidReply,
}

const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStage {
    Synchronize,
    Constraints,
    Ready,
}

#[derive(Debug, Error)]
pub enum DesktopError {
    #[error("Wayland connection failed: {0}")]
    Connection(String),
    #[error("Wayland dispatch failed: {0}")]
    Dispatch(String),
    #[error("required protocol is unavailable: {0}")]
    MissingProtocol(&'static str),
    #[error("selected output is unavailable")]
    OutputUnavailable,
    #[error("operation exceeded its deadline")]
    Timeout,
    #[error("capture exceeded its deadline during {0:?}")]
    CaptureTimeout(CaptureStage),
    #[error("compositor rejected capture")]
    CaptureFailed,
    #[error("capture format is unsupported")]
    UnsupportedFormat,
    #[error("invalid frame layout")]
    InvalidFrame,
    #[error("pointer preparation failed: {0:?}")]
    PointerPreparation(PointerPreparationFault),
    #[error("invalid input")]
    InvalidInput,
    #[error("operation cancelled")]
    Cancelled,
    #[error("input command queue is full")]
    InputBacklog,
    #[error("clipboard transfer already pending")]
    ClipboardBusy,
    #[error("input permission expired")]
    InputExpired,
    #[error("input geometry is obsolete")]
    StaleGeometry,
    #[error("clipboard access requires a confirmed unlocked session")]
    ClipboardLocked,
    #[error("clipboard exceeds the UTF-8 text limit")]
    ClipboardLimit,
    #[error("clipboard helper failed")]
    ClipboardFailed,
    #[error("XKB keymap could not be created")]
    Keymap,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, DesktopError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopGeometry {
    pub output: String,
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    pub transform: u32,
    pub generation: u64,
}
#[derive(Clone, Debug, Default)]
pub struct InputAuthority(Arc<Mutex<AuthorityState>>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorityPhase {
    Open = 0,
    Releasing = 1,
    AwaitingAcknowledgement = 2,
    Suspended = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuthoritySnapshot {
    pub phase: AuthorityPhase,
    pub revision: u64,
    pub lock: LockKnowledge,
    pub lock_generation: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LockKnowledge {
    #[default]
    Unobserved,
    Verified(KnownLock),
    Unavailable {
        last_known: Option<KnownLock>,
    },
    Changed {
        previous: KnownLock,
        current: KnownLock,
    },
}
impl LockKnowledge {
    pub(crate) fn visible_state(self) -> LockState {
        match self {
            Self::Verified(known) => known.into(),
            Self::Changed { current, .. } => current.into(),
            Self::Unobserved | Self::Unavailable { .. } => LockState::Unknown,
        }
    }
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Unobserved => 0,
            Self::Verified(KnownLock::Unlocked) => 1,
            Self::Verified(KnownLock::Locked) => 2,
            Self::Unavailable { .. } => 3,
            Self::Changed { .. } => 4,
        }
    }
    pub(crate) fn last_known(self) -> Option<KnownLock> {
        match self {
            Self::Verified(known) => Some(known),
            Self::Unavailable { last_known } => last_known,
            Self::Changed { previous, .. } => Some(previous),
            Self::Unobserved => None,
        }
    }
    fn verified(self) -> bool {
        matches!(self, Self::Verified(_))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockEffect {
    None,
    Release,
    DesktopResetRequired,
}
#[derive(Debug, Default)]
struct AuthorityState {
    input: InputPhase,
    lock: LockKnowledge,
    lock_generation: u64,
}
impl AuthorityState {
    fn is_open(&self) -> bool {
        self.input.is_open() && self.lock.verified()
    }
}
#[derive(Debug)]
enum InputPhase {
    Open(u64),
    Releasing(u64),
    AwaitingAcknowledgement(u64),
}
impl Default for InputPhase {
    fn default() -> Self {
        Self::Open(0)
    }
}
impl InputPhase {
    fn revision(&self) -> u64 {
        match self {
            Self::Open(revision)
            | Self::Releasing(revision)
            | Self::AwaitingAcknowledgement(revision) => *revision,
        }
    }
    fn is_open(&self) -> bool {
        matches!(self, Self::Open(_))
    }
    fn close(&mut self) -> u64 {
        if self.is_open() {
            *self = Self::Releasing(self.revision().wrapping_add(1));
        }
        self.revision()
    }
}
impl InputAuthority {
    pub(crate) fn snapshot(&self) -> AuthoritySnapshot {
        let state = self.0.lock().unwrap();
        AuthoritySnapshot {
            phase: match state.input {
                InputPhase::Open(_) if state.lock.verified() => AuthorityPhase::Open,
                InputPhase::Open(_) => AuthorityPhase::Suspended,
                InputPhase::Releasing(_) => AuthorityPhase::Releasing,
                InputPhase::AwaitingAcknowledgement(_) => AuthorityPhase::AwaitingAcknowledgement,
            },
            revision: state.input.revision(),
            lock: state.lock,
            lock_generation: state.lock_generation,
        }
    }
    pub fn observe_lock(&self, observation: LockObservation) -> LockEffect {
        let mut state = self.0.lock().unwrap();
        if matches!(state.lock, LockKnowledge::Changed { .. }) {
            return LockEffect::None;
        }
        let previous = state.lock;
        let next = match observation.outcome {
            Ok(current) => match previous.last_known() {
                Some(previous) if previous != current => {
                    LockKnowledge::Changed { previous, current }
                }
                _ => LockKnowledge::Verified(current),
            },
            Err(_) => LockKnowledge::Unavailable {
                last_known: previous.last_known(),
            },
        };
        if previous == next {
            return LockEffect::None;
        }
        state.lock = next;
        state.lock_generation = state.lock_generation.wrapping_add(1);
        let effect = if matches!(next, LockKnowledge::Changed { .. }) {
            LockEffect::DesktopResetRequired
        } else if previous.verified() && !next.verified() {
            LockEffect::Release
        } else {
            LockEffect::None
        };
        if effect != LockEffect::None {
            state.input.close();
        }
        effect
    }
    pub(crate) fn clipboard_guard(&self) -> Option<ClipboardGuard> {
        let state = self.0.lock().unwrap();
        (state.lock == LockKnowledge::Verified(KnownLock::Unlocked)).then(|| ClipboardGuard {
            authority: self.clone(),
            lock_generation: state.lock_generation,
        })
    }
    pub(crate) fn retire(&self) {
        let mut state = self.0.lock().unwrap();
        state.lock = LockKnowledge::Unavailable {
            last_known: state.lock.last_known(),
        };
        state.lock_generation = state.lock_generation.wrapping_add(1);
        state.input.close();
    }
    pub fn revoke(&self) {
        self.close();
    }
    pub fn close(&self) -> u64 {
        self.0.lock().unwrap().input.close()
    }
    pub fn revision(&self) -> u64 {
        self.0.lock().unwrap().input.revision()
    }
    pub fn is_open(&self) -> bool {
        self.0.lock().unwrap().is_open()
    }
    pub fn released(&self) -> Option<u64> {
        let mut state = self.0.lock().unwrap();
        if let InputPhase::Releasing(revision) = state.input {
            state.input = InputPhase::AwaitingAcknowledgement(revision);
            Some(revision)
        } else {
            None
        }
    }
    pub fn acknowledge(&self, revision: u64) -> bool {
        let mut state = self.0.lock().unwrap();
        if matches!(state.input, InputPhase::AwaitingAcknowledgement(current) if current == revision)
        {
            state.input = InputPhase::Open(revision);
            true
        } else {
            false
        }
    }
    pub fn permit(&self, generation: u64, expires_at: Instant) -> InputPermit {
        InputPermit {
            generation,
            expires_at,
            authority: self.clone(),
            revision: self.revision(),
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct ClipboardGuard {
    authority: InputAuthority,
    lock_generation: u64,
}
impl ClipboardGuard {
    pub(crate) fn current(&self) -> bool {
        self.with(|| ()).is_some()
    }
    pub(crate) fn with<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        let state = self.authority.0.lock().unwrap();
        if state.lock_generation != self.lock_generation
            || state.lock != LockKnowledge::Verified(KnownLock::Unlocked)
        {
            return None;
        }
        Some(operation())
    }
}
#[derive(Clone, Debug)]
pub struct InputPermit {
    generation: u64,
    expires_at: Instant,
    authority: InputAuthority,
    revision: u64,
}
impl InputPermit {
    pub fn new(generation: u64, expires_at: Instant) -> Self {
        InputAuthority::default().permit(generation, expires_at)
    }
    pub fn authorized_by(self, authority: &InputAuthority) -> Self {
        self.authorized_by_revision(authority, authority.revision())
    }
    pub fn authorized_by_revision(mut self, authority: &InputAuthority, revision: u64) -> Self {
        self.authority = authority.clone();
        self.revision = revision;
        self
    }
    pub(crate) fn current_for(&self, authority: &InputAuthority) -> bool {
        if !Arc::ptr_eq(&self.authority.0, &authority.0) {
            return false;
        }
        let phase = authority.0.lock().unwrap();
        phase.is_open() && phase.input.revision() == self.revision
    }
    pub(crate) fn emit(&self, generation: u64, emit: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut phase = self.authority.0.lock().unwrap();
        if phase.input.revision() != self.revision {
            return Err(DesktopError::Cancelled);
        }
        if !phase.lock.verified() {
            return Err(DesktopError::Cancelled);
        }
        if Instant::now() >= self.expires_at || !phase.is_open() {
            phase.input.close();
            return Err(DesktopError::InputExpired);
        }
        if generation != self.generation {
            return Err(DesktopError::StaleGeometry);
        }
        emit()
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct Cancellation {
    revision: Arc<AtomicU64>,
    expected: u64,
}
impl Cancellation {
    pub(crate) fn new(revision: Arc<AtomicU64>) -> Self {
        let expected = revision.load(Ordering::Acquire);
        Self { revision, expected }
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.revision.load(Ordering::Acquire) == self.expected {
            Ok(())
        } else {
            Err(DesktopError::Cancelled)
        }
    }
}

fn input_with_permit<T>(
    target: &mut T,
    permit: InputPermit,
    prepare: impl FnOnce(&mut T) -> Result<DesktopGeometry>,
    emit: impl FnOnce(&mut T, DesktopGeometry) -> Result<()>,
    release: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    let result = prepare(target)
        .and_then(|geometry| permit.emit(geometry.generation, || emit(target, geometry)));
    if result.is_err() && !matches!(result, Err(DesktopError::Cancelled)) {
        if permit.authority.revision() == permit.revision {
            permit.authority.close();
        }
        release(target)?;
    }
    result
}

pub(crate) fn flush_until(
    deadline: Instant,
    mut flush: impl FnMut() -> Result<bool>,
    mut writable: impl FnMut(Instant) -> Result<()>,
) -> Result<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(DesktopError::Timeout);
        }
        if flush()? {
            return Ok(());
        }
        writable(deadline)?;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
}
#[derive(Debug)]
pub struct Frame {
    pub ready_at: Instant,
    pub geometry: DesktopGeometry,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub y_inverted: bool,
    pub opaque: bool,
    pub bytes: Vec<u8>,
}
impl Frame {
    pub fn rgba(&self) -> Result<Vec<u8>> {
        let len = frame_size(self.width, self.height, self.stride)?;
        if self.bytes.len() != len {
            return Err(DesktopError::InvalidFrame);
        }
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for y in 0..self.height as usize {
            let src_y = if self.y_inverted {
                self.height as usize - 1 - y
            } else {
                y
            };
            let start = src_y * self.stride as usize;
            for p in self.bytes[start..start + self.width as usize * 4].chunks_exact(4) {
                let alpha = if self.opaque { 255 } else { p[3] };
                match self.format {
                    PixelFormat::Bgra => out.extend_from_slice(&[p[2], p[1], p[0], alpha]),
                    PixelFormat::Rgba => out.extend_from_slice(&[p[0], p[1], p[2], alpha]),
                }
            }
        }
        Ok(out)
    }
}
fn frame_size(width: u32, height: u32, stride: u32) -> Result<usize> {
    let minimum = width.checked_mul(4).ok_or(DesktopError::InvalidFrame)?;
    let len = (stride as usize)
        .checked_mul(height as usize)
        .ok_or(DesktopError::InvalidFrame)?;
    if width == 0
        || height == 0
        || stride < minimum
        || len > MAX_FRAME_BYTES
        || len > i32::MAX as usize
    {
        return Err(DesktopError::InvalidFrame);
    }
    Ok(len)
}
#[derive(Clone, Copy, Debug)]
pub struct Position {
    x: f64,
    y: f64,
}
impl Position {
    pub fn new(x: f64, y: f64) -> Result<Self> {
        if !x.is_finite()
            || !y.is_finite()
            || !(0.0..=1.0).contains(&x)
            || !(0.0..=1.0).contains(&y)
        {
            return Err(DesktopError::InvalidInput);
        }
        Ok(Self { x, y })
    }
    fn pixels(self, width: u32, height: u32) -> Result<(u32, u32)> {
        if width == 0 || height == 0 {
            return Err(DesktopError::InvalidInput);
        }
        Ok((
            (self.x * (width - 1) as f64).round() as u32,
            (self.y * (height - 1) as f64).round() as u32,
        ))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyCode(u32);
impl KeyCode {
    pub fn new(evdev: u32) -> Result<Self> {
        if !(1..=247).contains(&evdev) {
            return Err(DesktopError::InvalidInput);
        }
        Ok(Self(evdev))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
impl Button {
    fn evdev(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Back => 0x116,
            Self::Forward => 0x115,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Press,
    Release,
}
#[derive(Default)]
struct PressedInputs {
    keys: BTreeSet<KeyCode>,
    buttons: BTreeSet<Button>,
}
impl PressedInputs {
    fn key(&mut self, key: KeyCode, transition: Transition) -> bool {
        match transition {
            Transition::Press => self.keys.insert(key),
            Transition::Release => self.keys.remove(&key),
        }
    }
    fn button(&mut self, button: Button, transition: Transition) -> bool {
        match transition {
            Transition::Press => self.buttons.insert(button),
            Transition::Release => self.buttons.remove(&button),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPreparationCapability {
    GenericWayland,
    Hyprland,
    UnsupportedHyprland,
    UnobservedHyprland,
}

#[derive(Debug)]
pub struct Capabilities {
    pub screencopy_version: Option<u32>,
    pub virtual_keyboard: bool,
    pub virtual_pointer_version: Option<u32>,
    pub pointer_preparation: PointerPreparationCapability,
    pub outputs: Vec<DesktopGeometry>,
}
#[derive(Clone)]
struct Output {
    proxy: wl_output::WlOutput,
    pending: DesktopGeometry,
    current: Option<DesktopGeometry>,
}
#[derive(Clone, Copy)]
struct BufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncPurpose {
    Setup,
    Input,
    Capture,
}
struct PendingSync {
    id: wayland_client::backend::ObjectId,
    completed_at: Option<Instant>,
}
struct CaptureFrame {
    proxy: zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    geometry: DesktopGeometry,
    spec: Option<BufferSpec>,
    constraints_at: Option<Instant>,
    inverted: bool,
}
struct CaptureStorage {
    spec: BufferSpec,
    _file: std::fs::File,
    mapping: MmapMut,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}
impl Drop for CaptureStorage {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
impl Drop for CaptureFrame {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}
struct CaptureCopy {
    frame: CaptureFrame,
    storage: CaptureStorage,
}
enum CaptureResources {
    Frame(CaptureFrame),
    Copy(CaptureCopy),
}
enum CapturePhase {
    Synchronizing(PendingSync),
    Constraints(CaptureFrame),
    Copying(CaptureCopy),
    Readable(CaptureCopy, Instant),
    Failed(CaptureResources),
}
impl CapturePhase {
    fn stage(&self) -> CaptureStage {
        match self {
            Self::Synchronizing(_) => CaptureStage::Synchronize,
            Self::Constraints(_) => CaptureStage::Constraints,
            _ => CaptureStage::Ready,
        }
    }
    fn frame_mut(&mut self) -> Option<&mut CaptureFrame> {
        match self {
            Self::Constraints(frame) | Self::Failed(CaptureResources::Frame(frame)) => Some(frame),
            Self::Copying(copy)
            | Self::Readable(copy, _)
            | Self::Failed(CaptureResources::Copy(copy)) => Some(&mut copy.frame),
            _ => None,
        }
    }
}
struct CaptureOperation {
    deadline: Instant,
    phase: CapturePhase,
}
pub(crate) enum CapturePoll {
    Idle,
    Pending,
    Complete(Frame),
}
struct WaylandState {
    outputs: BTreeMap<u32, Output>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    capture_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    keyboard_manager: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    pointer_manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    capture: Option<CaptureOperation>,
    setup_sync: Option<PendingSync>,
    input_sync: Option<PendingSync>,
}
pub enum InputPoll {
    Pending,
    Complete,
}
struct ButtonOperation {
    position: Position,
    button: Button,
    transition: Transition,
    permit: InputPermit,
    refresh: Option<PointerReply>,
    stage: ButtonStage,
}
enum ButtonStage {
    GeometrySync,
    DeviceSync,
    MotionSync,
    RefreshReply,
    Flushing,
}

pub struct Desktop {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
    selected: u32,
    keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    pointer: Option<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1>,
    keymap: xkb::Keymap,
    xkb_state: xkb::State,
    pressed: PressedInputs,
    started: Instant,
    clipboard: Option<Child>,
    cancellation: Cancellation,
    pointer_adapter: PointerAdapter,
    input: Option<ButtonOperation>,
}
impl Desktop {
    pub fn connect(output: &str) -> Result<Self> {
        Self::connect_cancellable(output, Cancellation::default())
    }
    pub(crate) fn connect_cancellable(output: &str, cancellation: Cancellation) -> Result<Self> {
        cancellation.check()?;
        let connection =
            Connection::connect_to_env().map_err(|e| DesktopError::Connection(e.to_string()))?;
        let mut desktop = Self::connect_on(output, cancellation, connection)?;
        desktop.pointer_adapter = PointerAdapter::detect(&desktop.cancellation)?;
        Ok(desktop)
    }
    fn connect_on(
        output: &str,
        cancellation: Cancellation,
        connection: Connection,
    ) -> Result<Self> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let state = WaylandState {
            outputs: BTreeMap::new(),
            shm: None,
            seat: None,
            capture_manager: None,
            keyboard_manager: None,
            pointer_manager: None,
            capture: None,
            setup_sync: None,
            input_sync: None,
        };
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or(DesktopError::Keymap)?;
        let xkb_state = xkb::State::new(&keymap);
        let mut desktop = Self {
            connection,
            queue,
            state,
            selected: 0,
            keyboard: None,
            pointer: None,
            keymap,
            xkb_state,
            pressed: PressedInputs::default(),
            started: Instant::now(),
            clipboard: None,
            cancellation,
            pointer_adapter: PointerAdapter::Generic,
            input: None,
        };
        desktop.sync(deadline.saturating_duration_since(Instant::now()))?;
        desktop.sync(deadline.saturating_duration_since(Instant::now()))?;
        desktop.selected = desktop
            .state
            .outputs
            .iter()
            .find(|(_, o)| o.current.as_ref().is_some_and(|g| g.output == output))
            .map(|(id, _)| *id)
            .ok_or(DesktopError::OutputUnavailable)?;
        Ok(desktop)
    }
    pub(crate) fn set_cancellation(&mut self, cancellation: Cancellation) {
        self.cancellation = cancellation;
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            screencopy_version: self.state.capture_manager.as_ref().map(Proxy::version),
            virtual_keyboard: self.state.keyboard_manager.is_some(),
            virtual_pointer_version: self.state.pointer_manager.as_ref().map(Proxy::version),
            pointer_preparation: match &self.pointer_adapter {
                PointerAdapter::Generic => PointerPreparationCapability::GenericWayland,
                PointerAdapter::Hyprland(_) => PointerPreparationCapability::Hyprland,
                PointerAdapter::Unsupported => PointerPreparationCapability::UnsupportedHyprland,
                PointerAdapter::UnobservedHyprland(_) => {
                    PointerPreparationCapability::UnobservedHyprland
                }
            },
            outputs: self
                .state
                .outputs
                .values()
                .filter_map(|o| o.current.clone())
                .collect(),
        }
    }
    pub fn geometry(&self) -> Result<DesktopGeometry> {
        self.state
            .outputs
            .get(&self.selected)
            .and_then(|o| o.current.clone())
            .filter(|g| g.width > 0 && g.height > 0)
            .ok_or(DesktopError::OutputUnavailable)
    }
    fn dispatch_until(
        &mut self,
        deadline: Instant,
        done: impl Fn(&WaylandState) -> bool,
    ) -> Result<()> {
        loop {
            self.cancellation.check()?;
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| DesktopError::Dispatch(e.to_string()))?;
            if done(&self.state) {
                return Ok(());
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(DesktopError::Timeout)?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let write_pending = match self.connection.flush() {
                Ok(()) => false,
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    true
                }
                Err(e) => return Err(DesktopError::Dispatch(e.to_string())),
            };
            let mut fds = [PollFd::new(
                &self.connection,
                PollFlags::IN
                    | if write_pending {
                        PollFlags::OUT
                    } else {
                        PollFlags::empty()
                    },
            )];
            let timeout =
                rustix::event::Timespec::try_from(remaining.min(Duration::from_millis(5)))
                    .map_err(|_| DesktopError::Timeout)?;
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
            if fds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
            {
                return Err(DesktopError::Connection("socket closed".into()));
            }
            if fds[0].revents().contains(PollFlags::IN) {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(DesktopError::Dispatch(e.to_string())),
                }
            }
        }
    }
    fn sync(&mut self, timeout: Duration) -> Result<()> {
        self.sync_for(SyncPurpose::Setup, timeout)
    }
    fn sync_for(&mut self, purpose: SyncPurpose, timeout: Duration) -> Result<()> {
        let callback = self
            .connection
            .display()
            .sync(&self.queue.handle(), purpose);
        let pending = Some(PendingSync {
            id: callback.id(),
            completed_at: None,
        });
        match purpose {
            SyncPurpose::Setup => self.state.setup_sync = pending,
            SyncPurpose::Input => self.state.input_sync = pending,
            SyncPurpose::Capture => unreachable!(),
        }
        let result = self.dispatch_until(Instant::now() + timeout, |state| {
            let slot = match purpose {
                SyncPurpose::Setup => &state.setup_sync,
                _ => &state.input_sync,
            };
            slot.as_ref()
                .is_some_and(|sync| sync.completed_at.is_some())
        });
        match purpose {
            SyncPurpose::Setup => self.state.setup_sync = None,
            SyncPurpose::Input => self.state.input_sync = None,
            SyncPurpose::Capture => unreachable!(),
        }
        result
    }
    pub fn refresh(&mut self) -> Result<DesktopGeometry> {
        self.sync(IO_TIMEOUT)?;
        self.geometry()
    }
    pub(crate) fn begin_capture(&mut self, timeout: Duration) -> Result<()> {
        self.cancellation.check()?;
        if self.state.capture.is_some() {
            return Err(DesktopError::CaptureFailed);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(DesktopError::CaptureTimeout(CaptureStage::Synchronize))?;
        let callback = self
            .connection
            .display()
            .sync(&self.queue.handle(), SyncPurpose::Capture);
        self.state.capture = Some(CaptureOperation {
            deadline,
            phase: CapturePhase::Synchronizing(PendingSync {
                id: callback.id(),
                completed_at: None,
            }),
        });
        Ok(())
    }
    pub(crate) fn abort_capture(&mut self) {
        self.state.capture = None;
    }
    fn pump_available(&mut self) -> Result<()> {
        self.cancellation.check()?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| DesktopError::Dispatch(e.to_string()))?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let pending = match self.connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(e) => return Err(DesktopError::Dispatch(e.to_string())),
        };
        let mut fds = [PollFd::new(
            &self.connection,
            PollFlags::IN
                | if pending {
                    PollFlags::OUT
                } else {
                    PollFlags::empty()
                },
        )];
        let zero = rustix::event::Timespec::default();
        match poll(&mut fds, Some(&zero)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
        if fds[0]
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
        {
            return Err(DesktopError::Connection("socket closed".into()));
        }
        if fds[0].revents().contains(PollFlags::IN) {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(DesktopError::Dispatch(e.to_string())),
            }
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| DesktopError::Dispatch(e.to_string()))?;
        }
        Ok(())
    }
    pub(crate) fn poll_capture(&mut self) -> Result<CapturePoll> {
        let result = self.advance_capture();
        if result.is_err() {
            self.abort_capture();
        }
        result
    }
    fn advance_capture(&mut self) -> Result<CapturePoll> {
        self.pump_available()?;
        let Some(operation) = self.state.capture.take() else {
            return Ok(CapturePoll::Idle);
        };
        let CaptureOperation { deadline, phase } = operation;
        if let CapturePhase::Readable(copy, ready_at) = phase {
            if ready_at > deadline {
                return Err(DesktopError::CaptureTimeout(CaptureStage::Ready));
            }
            if self.geometry()? != copy.frame.geometry {
                return Err(DesktopError::StaleGeometry);
            }
            let spec = copy.storage.spec;
            let (format, opaque) = match spec.format {
                wl_shm::Format::Argb8888 => (PixelFormat::Bgra, false),
                wl_shm::Format::Xrgb8888 => (PixelFormat::Bgra, true),
                wl_shm::Format::Abgr8888 => (PixelFormat::Rgba, false),
                wl_shm::Format::Xbgr8888 => (PixelFormat::Rgba, true),
                _ => return Err(DesktopError::UnsupportedFormat),
            };
            return Ok(CapturePoll::Complete(Frame {
                ready_at,
                geometry: copy.frame.geometry.clone(),
                width: spec.width,
                height: spec.height,
                stride: spec.stride,
                format,
                y_inverted: copy.frame.inverted,
                opaque,
                bytes: copy.storage.mapping.to_vec(),
            }));
        }
        if matches!(phase, CapturePhase::Failed(_)) {
            return Err(DesktopError::CaptureFailed);
        }
        if Instant::now() >= deadline {
            return Err(DesktopError::CaptureTimeout(phase.stage()));
        }
        let phase = match phase {
            CapturePhase::Synchronizing(sync) if sync.completed_at.is_some() => {
                let geometry = self.geometry()?;
                let manager = self
                    .state
                    .capture_manager
                    .as_ref()
                    .filter(|manager| manager.version() >= 3)
                    .ok_or(DesktopError::MissingProtocol("wlr-screencopy-v3"))?;
                let output = &self
                    .state
                    .outputs
                    .get(&self.selected)
                    .ok_or(DesktopError::OutputUnavailable)?
                    .proxy;
                let proxy = manager.capture_output(1, output, &self.queue.handle(), ());
                CapturePhase::Constraints(CaptureFrame {
                    proxy,
                    geometry,
                    spec: None,
                    constraints_at: None,
                    inverted: false,
                })
            }
            CapturePhase::Constraints(frame) if frame.constraints_at.is_some() => {
                if self.geometry()? != frame.geometry {
                    return Err(DesktopError::StaleGeometry);
                }
                let spec = frame.spec.ok_or(DesktopError::UnsupportedFormat)?;
                let len = frame_size(spec.width, spec.height, spec.stride)?;
                let shm = self
                    .state
                    .shm
                    .as_ref()
                    .ok_or(DesktopError::MissingProtocol("wl_shm"))?;
                let file = tempfile::tempfile()?;
                file.set_len(len as u64)?;
                // The compositor owns writes until Ready. Cancellation discards this allocation.
                let mapping = unsafe { MmapMut::map_mut(&file)? };
                let pool = shm.create_pool(file.as_fd(), len as i32, &self.queue.handle(), ());
                let buffer = pool.create_buffer(
                    0,
                    spec.width as i32,
                    spec.height as i32,
                    spec.stride as i32,
                    spec.format,
                    &self.queue.handle(),
                    (),
                );
                frame.proxy.copy(&buffer);
                CapturePhase::Copying(CaptureCopy {
                    frame,
                    storage: CaptureStorage {
                        spec,
                        _file: file,
                        mapping,
                        pool,
                        buffer,
                    },
                })
            }
            phase => phase,
        };
        self.state.capture = Some(CaptureOperation { deadline, phase });
        Ok(CapturePoll::Pending)
    }
    pub fn capture(&mut self, timeout: Duration) -> Result<Frame> {
        self.begin_capture(timeout)?;
        loop {
            match self.poll_capture()? {
                CapturePoll::Complete(frame) => return Ok(frame),
                CapturePoll::Pending => std::thread::sleep(Duration::from_millis(2)),
                CapturePoll::Idle => return Err(DesktopError::CaptureFailed),
            }
        }
    }
    fn ensure_keyboard(&mut self, permit: &InputPermit) -> Result<()> {
        if self.keyboard.is_some() {
            return Ok(());
        }
        let manager = self
            .state
            .keyboard_manager
            .as_ref()
            .ok_or(DesktopError::MissingProtocol("virtual-keyboard"))?;
        let seat = self
            .state
            .seat
            .as_ref()
            .ok_or(DesktopError::MissingProtocol("wl_seat"))?;
        let mut keymap_file = tempfile::tempfile()?;
        let text = self.keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        keymap_file.write_all(text.as_bytes())?;
        keymap_file.write_all(&[0])?;
        let keyboard = manager.create_virtual_keyboard(seat, &self.queue.handle(), ());
        keyboard.keymap(1, keymap_file.as_fd(), (text.len() + 1) as u32);
        self.keyboard = Some(keyboard);
        self.input_sync(permit)
    }
    fn ensure_pointer(&mut self, permit: &InputPermit) -> Result<()> {
        if self.pointer.is_some() {
            return Ok(());
        }
        let manager = self
            .state
            .pointer_manager
            .as_ref()
            .ok_or(DesktopError::MissingProtocol("virtual-pointer-v2"))?;
        if manager.version() < 2 {
            return Err(DesktopError::MissingProtocol("virtual-pointer-v2"));
        }
        let output = &self
            .state
            .outputs
            .get(&self.selected)
            .ok_or(DesktopError::OutputUnavailable)?
            .proxy;
        self.pointer = Some(manager.create_virtual_pointer_with_output(
            self.state.seat.as_ref(),
            Some(output),
            &self.queue.handle(),
            (),
        ));
        self.input_sync(permit)
    }
    fn input_sync(&mut self, permit: &InputPermit) -> Result<()> {
        let remaining = permit
            .expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(DesktopError::InputExpired)?;
        let result = self.sync_for(SyncPurpose::Input, remaining.min(IO_TIMEOUT));
        if matches!(result, Err(DesktopError::Timeout)) && Instant::now() >= permit.expires_at {
            return Err(DesktopError::InputExpired);
        }
        result
    }
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    fn modifiers(&self) {
        if let Some(keyboard) = &self.keyboard {
            keyboard.modifiers(
                self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED),
                self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED),
                self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            );
        }
    }
    pub fn key(&mut self, key: KeyCode, transition: Transition, permit: InputPermit) -> Result<()> {
        if self.input.is_some() {
            return Err(DesktopError::InputBacklog);
        }
        self.finish_input(
            permit.clone(),
            |desktop| {
                if desktop
                    .keymap
                    .key_get_name(xkb::Keycode::new(key.0 + 8))
                    .is_none()
                {
                    return Err(DesktopError::InvalidInput);
                }
                desktop.input_sync(&permit)?;
                desktop.ensure_keyboard(&permit)?;
                desktop.geometry()
            },
            |desktop, _| {
                if desktop.pressed.key(key, transition) {
                    let down = transition == Transition::Press;
                    desktop
                        .keyboard
                        .as_ref()
                        .unwrap()
                        .key(desktop.time(), key.0, u32::from(down));
                    desktop.xkb_state.update_key(
                        xkb::Keycode::new(key.0 + 8),
                        if down {
                            xkb::KeyDirection::Down
                        } else {
                            xkb::KeyDirection::Up
                        },
                    );
                    desktop.modifiers();
                }
                Ok(())
            },
        )
    }
    fn prepare_pointer(&mut self, permit: &InputPermit) -> Result<DesktopGeometry> {
        self.input_sync(permit)?;
        self.ensure_pointer(permit)?;
        self.geometry()
    }
    fn move_pointer(&mut self, position: Position, geometry: &DesktopGeometry) -> Result<()> {
        let (x, y) = position.pixels(geometry.width, geometry.height)?;
        self.pointer.as_ref().unwrap().motion_absolute(
            self.time(),
            x,
            y,
            geometry.width,
            geometry.height,
        );
        Ok(())
    }
    pub fn pointer_absolute(&mut self, position: Position, permit: InputPermit) -> Result<()> {
        if self.input.is_some() {
            return Err(DesktopError::InputBacklog);
        }
        self.finish_input(
            permit.clone(),
            |desktop| desktop.prepare_pointer(&permit),
            |desktop, geometry| {
                desktop.move_pointer(position, &geometry)?;
                desktop.pointer.as_ref().unwrap().frame();
                Ok(())
            },
        )
    }
    pub fn button(
        &mut self,
        position: Position,
        button: Button,
        transition: Transition,
        permit: InputPermit,
    ) -> Result<()> {
        if self.input.is_some() {
            return Err(DesktopError::InputBacklog);
        }
        let authority = permit.authority.clone();
        let result = (|| {
            self.begin_button(position, button, transition, permit)?;
            loop {
                match self.poll_input()? {
                    InputPoll::Complete => return Ok(()),
                    InputPoll::Pending => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        })();
        if result.is_err() {
            authority.close();
            self.release_all()?;
        }
        result
    }
    pub fn begin_button(
        &mut self,
        position: Position,
        button: Button,
        transition: Transition,
        permit: InputPermit,
    ) -> Result<()> {
        if self.input.is_some() {
            return Err(DesktopError::InputBacklog);
        }
        self.cancellation.check()?;
        let geometry = self.geometry()?;
        position.pixels(geometry.width, geometry.height)?;
        permit.emit(geometry.generation, || Ok(()))?;
        let refresh = if transition == Transition::Press && self.pressed.buttons.is_empty() {
            self.pointer_adapter.prepare()?
        } else {
            None
        };
        self.begin_input_sync();
        self.input = Some(ButtonOperation {
            position,
            button,
            transition,
            permit,
            refresh,
            stage: ButtonStage::GeometrySync,
        });
        Ok(())
    }
    fn begin_input_sync(&mut self) {
        let callback = self
            .connection
            .display()
            .sync(&self.queue.handle(), SyncPurpose::Input);
        self.state.input_sync = Some(PendingSync {
            id: callback.id(),
            completed_at: None,
        });
    }
    pub fn abort_input(&mut self) {
        self.input = None;
        self.state.input_sync = None;
    }
    pub fn poll_input(&mut self) -> Result<InputPoll> {
        let Some(mut operation) = self.input.take() else {
            return Ok(InputPoll::Complete);
        };
        let result = self.poll_button(&mut operation);
        match result {
            Ok(InputPoll::Pending) => self.input = Some(operation),
            Ok(InputPoll::Complete) => self.state.input_sync = None,
            Err(_) => {
                self.state.input_sync = None;
            }
        }
        result
    }
    fn poll_button(&mut self, operation: &mut ButtonOperation) -> Result<InputPoll> {
        self.cancellation.check()?;
        if Instant::now() >= operation.permit.expires_at {
            return Err(DesktopError::InputExpired);
        }
        self.pump_available()?;
        let geometry = self.geometry()?;
        operation.permit.emit(geometry.generation, || Ok(()))?;
        let sync_done = self
            .state
            .input_sync
            .as_ref()
            .is_some_and(|sync| sync.completed_at.is_some());
        match operation.stage {
            ButtonStage::GeometrySync if sync_done => {
                if self.pointer.is_none() {
                    let manager = self
                        .state
                        .pointer_manager
                        .as_ref()
                        .filter(|m| m.version() >= 2)
                        .ok_or(DesktopError::MissingProtocol("virtual-pointer-v2"))?;
                    let output = &self
                        .state
                        .outputs
                        .get(&self.selected)
                        .ok_or(DesktopError::OutputUnavailable)?
                        .proxy;
                    operation.permit.emit(geometry.generation, || {
                        self.pointer = Some(manager.create_virtual_pointer_with_output(
                            self.state.seat.as_ref(),
                            Some(output),
                            &self.queue.handle(),
                            (),
                        ));
                        Ok(())
                    })?;
                    self.begin_input_sync();
                    operation.stage = ButtonStage::DeviceSync;
                } else {
                    self.begin_button_motion(operation, &geometry)?;
                }
            }
            ButtonStage::DeviceSync if sync_done => {
                self.begin_button_motion(operation, &geometry)?
            }
            ButtonStage::MotionSync if sync_done => {
                self.state.input_sync = None;
                if let Some(refresh) = operation.refresh.as_mut() {
                    operation
                        .permit
                        .emit(geometry.generation, || refresh.refresh())?;
                    operation.stage = ButtonStage::RefreshReply;
                } else {
                    self.emit_button(operation, &geometry)?;
                }
            }
            ButtonStage::RefreshReply => {
                if operation.refresh.as_mut().unwrap().ready()? {
                    if let PointerAdapter::UnobservedHyprland(Some(path)) = &self.pointer_adapter {
                        self.pointer_adapter = PointerAdapter::Hyprland(path.clone());
                    }
                    self.emit_button(operation, &geometry)?;
                }
            }
            ButtonStage::Flushing => match self.connection.flush() {
                Ok(()) => return Ok(InputPoll::Complete),
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(DesktopError::Dispatch(error.to_string())),
            },
            _ => {}
        }
        Ok(InputPoll::Pending)
    }
    fn begin_button_motion(
        &mut self,
        operation: &mut ButtonOperation,
        geometry: &DesktopGeometry,
    ) -> Result<()> {
        operation.permit.emit(geometry.generation, || {
            self.move_pointer(operation.position, geometry)?;
            if operation.refresh.is_some() {
                self.pointer.as_ref().unwrap().frame();
            }
            Ok(())
        })?;
        if operation.refresh.is_some() {
            self.begin_input_sync();
            operation.stage = ButtonStage::MotionSync;
        } else {
            self.emit_button(operation, geometry)?;
        }
        Ok(())
    }
    fn emit_button(
        &mut self,
        operation: &mut ButtonOperation,
        geometry: &DesktopGeometry,
    ) -> Result<()> {
        operation.permit.emit(geometry.generation, || {
            if self.pressed.button(operation.button, operation.transition) {
                self.pointer.as_ref().unwrap().button(
                    self.time(),
                    operation.button.evdev(),
                    if operation.transition == Transition::Press {
                        wl_pointer::ButtonState::Pressed
                    } else {
                        wl_pointer::ButtonState::Released
                    },
                );
            }
            self.pointer.as_ref().unwrap().frame();
            Ok(())
        })?;
        operation.stage = ButtonStage::Flushing;
        Ok(())
    }
    pub fn axis(
        &mut self,
        position: Position,
        horizontal: f64,
        vertical: f64,
        permit: InputPermit,
    ) -> Result<()> {
        if self.input.is_some() {
            return Err(DesktopError::InputBacklog);
        }
        self.finish_input(
            permit.clone(),
            |desktop| {
                if !horizontal.is_finite()
                    || !vertical.is_finite()
                    || horizontal.abs() > 10000.0
                    || vertical.abs() > 10000.0
                {
                    return Err(DesktopError::InvalidInput);
                }
                desktop.prepare_pointer(&permit)
            },
            |desktop, geometry| {
                desktop.move_pointer(position, &geometry)?;
                let pointer = desktop.pointer.as_ref().unwrap();
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
                if horizontal != 0.0 {
                    pointer.axis(
                        desktop.time(),
                        wl_pointer::Axis::HorizontalScroll,
                        horizontal,
                    );
                }
                if vertical != 0.0 {
                    pointer.axis(desktop.time(), wl_pointer::Axis::VerticalScroll, vertical);
                }
                pointer.frame();
                Ok(())
            },
        )
    }
    fn flush_input_until(&mut self, deadline: Instant, cancellable: bool) -> Result<()> {
        let cancellation = if cancellable {
            self.cancellation.clone()
        } else {
            Cancellation::default()
        };
        flush_until(
            deadline,
            || {
                if cancellable {
                    self.cancellation.check()?;
                }
                match self.connection.flush() {
                    Ok(()) => Ok(true),
                    Err(wayland_client::backend::WaylandError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        Ok(false)
                    }
                    Err(error) => Err(DesktopError::Dispatch(error.to_string())),
                }
            },
            |deadline| {
                wait_fd_cancellable(&self.connection, PollFlags::OUT, deadline, &cancellation)
            },
        )
    }
    fn finish_input(
        &mut self,
        permit: InputPermit,
        prepare: impl FnOnce(&mut Self) -> Result<DesktopGeometry>,
        emit: impl FnOnce(&mut Self, DesktopGeometry) -> Result<()>,
    ) -> Result<()> {
        let expires_at = permit.expires_at;
        let deadline = expires_at.min(Instant::now() + Duration::from_secs(2));
        let authority = permit.authority.clone();
        input_with_permit(self, permit, prepare, emit, Self::release_all)?;
        if let Err(error) = self.flush_input_until(deadline, true) {
            authority.close();
            self.release_all()?;
            return Err(
                if matches!(error, DesktopError::Timeout) && Instant::now() >= expires_at {
                    DesktopError::InputExpired
                } else {
                    error
                },
            );
        }
        Ok(())
    }
    pub fn release_all(&mut self) -> Result<()> {
        self.abort_input();
        if let Some(keyboard) = &self.keyboard {
            for key in &self.pressed.keys {
                keyboard.key(self.time(), key.0, 0);
            }
        }
        if let Some(pointer) = &self.pointer {
            for button in &self.pressed.buttons {
                pointer.button(
                    self.time(),
                    button.evdev(),
                    wl_pointer::ButtonState::Released,
                );
            }
            pointer.frame();
        }
        self.pressed = PressedInputs::default();
        self.xkb_state = xkb::State::new(&self.keymap);
        self.modifiers();
        self.flush_input_until(Instant::now() + Duration::from_millis(100), false)
    }
    fn ensure_unlocked(&self) -> Result<()> {
        self.cancellation.check()?;
        if lock_state_cancellable(Duration::from_millis(200), &self.cancellation)
            == LockState::Unlocked
        {
            Ok(())
        } else {
            Err(DesktopError::ClipboardLocked)
        }
    }
    pub fn clipboard_read(&mut self) -> Result<String> {
        self.ensure_unlocked()?;
        let child = Command::new("wl-paste")
            .args(["--no-newline", "--type", "text"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let bytes =
            bounded_output_cancellable(child, IO_TIMEOUT, MAX_CLIPBOARD_BYTES, &self.cancellation)?;
        self.ensure_unlocked()?;
        String::from_utf8(bytes).map_err(|_| DesktopError::ClipboardFailed)
    }
    pub fn clipboard_write(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES || text.contains('\0') {
            return Err(DesktopError::ClipboardLimit);
        }
        self.ensure_unlocked()?;
        let mut child = ChildGuard::new(
            Command::new("wl-copy")
                .args(["--foreground", "--type", "text/plain;charset=utf-8"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut stdin = child.stdin.take().ok_or(DesktopError::ClipboardFailed)?;
        nonblocking(&stdin)?;
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut offset = 0;
        while offset < text.len() {
            self.cancellation.check()?;
            match stdin.write(&text.as_bytes()[offset..]) {
                Ok(0) => return Err(DesktopError::ClipboardFailed),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_fd_cancellable(&stdin, PollFlags::OUT, deadline, &self.cancellation)?
                }
                Err(e) => return Err(e.into()),
            }
        }
        drop(stdin);
        loop {
            self.cancellation.check()?;
            if child.try_wait()?.is_some() {
                return Err(DesktopError::ClipboardFailed);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(DesktopError::Timeout)?;
            let reader = Command::new("wl-paste")
                .args(["--no-newline", "--type", "text"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            if bounded_output_cancellable(
                reader,
                remaining,
                MAX_CLIPBOARD_BYTES,
                &self.cancellation,
            )
            .is_ok_and(|bytes| bytes == text.as_bytes())
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(DesktopError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.ensure_unlocked()?;
        if let Some(mut old) = self.clipboard.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        self.clipboard = Some(child.take());
        Ok(())
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.abort_capture();
        let _ = self.release_all();
        if let Some(pointer) = &self.pointer {
            pointer.destroy();
        }
        if let Some(keyboard) = &self.keyboard {
            keyboard.destroy();
        }
        let _ = self.connection.flush();
        if let Some(mut child) = self.clipboard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    Unlocked,
    Locked,
    Unknown,
}

pub fn lock_state(timeout: Duration) -> LockState {
    lock_state_cancellable(timeout, &Cancellation::default())
}
pub(crate) fn lock_state_cancellable(timeout: Duration, cancellation: &Cancellation) -> LockState {
    observe_lock_cancellable(timeout, cancellation).state()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownLock {
    Unlocked = 0,
    Locked = 1,
}
impl From<KnownLock> for LockState {
    fn from(value: KnownLock) -> Self {
        match value {
            KnownLock::Unlocked => Self::Unlocked,
            KnownLock::Locked => Self::Locked,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockQueryFailure {
    Spawn = 1,
    Deadline = 2,
    Cancelled = 3,
    OutputLimit = 4,
    HelperFailure = 5,
    Io = 6,
    InvalidReply = 7,
}
#[derive(Clone, Copy, Debug)]
pub struct LockObservation {
    pub outcome: std::result::Result<KnownLock, LockQueryFailure>,
    pub elapsed_us: u64,
}
impl LockObservation {
    pub fn state(self) -> LockState {
        self.outcome.map(Into::into).unwrap_or(LockState::Unknown)
    }
    pub(crate) fn outcome_code(self) -> u8 {
        self.outcome.err().map_or(0, |failure| failure as u8)
    }
}
pub fn observe_lock(timeout: Duration) -> LockObservation {
    observe_lock_cancellable(timeout, &Cancellation::default())
}
pub(crate) fn observe_lock_cancellable(
    timeout: Duration,
    cancellation: &Cancellation,
) -> LockObservation {
    let began = Instant::now();
    let outcome = (|| {
        cancellation
            .check()
            .map_err(|_| LockQueryFailure::Cancelled)?;
        let child = Command::new("hyprctl")
            .args(["-j", "locked"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| LockQueryFailure::Spawn)?;
        let bytes =
            bounded_output_cancellable(child, timeout, 128, cancellation).map_err(|error| {
                match error {
                    DesktopError::Timeout => LockQueryFailure::Deadline,
                    DesktopError::Cancelled => LockQueryFailure::Cancelled,
                    DesktopError::ClipboardLimit => LockQueryFailure::OutputLimit,
                    DesktopError::Io(_) => LockQueryFailure::Io,
                    _ => LockQueryFailure::HelperFailure,
                }
            })?;
        match parse_lock_state(&bytes) {
            Some(LockState::Unlocked) => Ok(KnownLock::Unlocked),
            Some(LockState::Locked) => Ok(KnownLock::Locked),
            _ => Err(LockQueryFailure::InvalidReply),
        }
    })();
    LockObservation {
        outcome,
        elapsed_us: began.elapsed().as_micros().min(u64::MAX as u128) as u64,
    }
}

fn parse_lock_state(bytes: &[u8]) -> Option<LockState> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        locked: bool,
    }
    serde_json::from_slice::<Reply>(bytes).ok().map(|reply| {
        if reply.locked {
            LockState::Locked
        } else {
            LockState::Unlocked
        }
    })
}

struct ChildGuard(Option<Child>);
impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }
    fn take(mut self) -> Child {
        self.0.take().unwrap()
    }
}
impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn nonblocking(fd: &impl AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)?;
    Ok(())
}
fn wait_fd_cancellable(
    fd: &impl AsFd,
    flags: PollFlags,
    deadline: Instant,
    cancellation: &Cancellation,
) -> Result<()> {
    loop {
        cancellation.check()?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(DesktopError::Timeout)?;
        let timeout = rustix::event::Timespec::try_from(remaining.min(Duration::from_millis(5)))
            .map_err(|_| DesktopError::Timeout)?;
        let mut fds = [PollFd::new(fd, flags)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => continue,
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(std::io::Error::from(e).into()),
        }
    }
}
#[cfg(test)]
fn bounded_output(child: Child, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    bounded_output_cancellable(child, timeout, limit, &Cancellation::default())
}
fn bounded_output_cancellable(
    child: Child,
    timeout: Duration,
    limit: usize,
    cancellation: &Cancellation,
) -> Result<Vec<u8>> {
    let mut guard = ChildGuard::new(child);
    let mut stdout = guard.stdout.take().ok_or(DesktopError::ClipboardFailed)?;
    nonblocking(&stdout)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        cancellation.check()?;
        let mut buf = [0; 4096];
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() + n > limit {
                    return Err(DesktopError::ClipboardLimit);
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_fd_cancellable(&stdout, PollFlags::IN, deadline, cancellation)?
            }
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            return Err(DesktopError::Timeout);
        }
    }
    loop {
        cancellation.check()?;
        if let Some(status) = guard.try_wait()? {
            return if status.success() {
                Ok(out)
            } else {
                Err(DesktopError::ClipboardFailed)
            };
        }
        if Instant::now() >= deadline {
            return Err(DesktopError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                "wl_output" if version >= 4 => {
                    let proxy = registry.bind(name, 4, qh, name);
                    state.outputs.insert(
                        name,
                        Output {
                            proxy,
                            pending: DesktopGeometry {
                                output: String::new(),
                                width: 0,
                                height: 0,
                                scale: 1,
                                transform: 0,
                                generation: 1,
                            },
                            current: None,
                        },
                    );
                }
                "zwlr_screencopy_manager_v1" => {
                    state.capture_manager = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_manager = Some(registry.bind(name, 1, qh, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager = Some(registry.bind(name, version.min(2), qh, ()))
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    output.proxy.release();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => output.pending.output = name,
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                output.pending.width = width.max(0) as u32;
                output.pending.height = height.max(0) as u32;
            }
            wl_output::Event::Scale { factor } => output.pending.scale = factor,
            wl_output::Event::Geometry { transform, .. } => {
                output.pending.transform = match transform {
                    WEnum::Value(value) => value as u32,
                    WEnum::Unknown(value) => value,
                }
            }
            wl_output::Event::Done => {
                if let Some(current) = &output.current {
                    if current != &output.pending {
                        output.pending.generation = current.generation.saturating_add(1);
                    }
                }
                output.current = Some(output.pending.clone());
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, SyncPurpose> for WaylandState {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        _: wl_callback::Event,
        purpose: &SyncPurpose,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let slot = match purpose {
            SyncPurpose::Setup => state.setup_sync.as_mut(),
            SyncPurpose::Input => state.input_sync.as_mut(),
            SyncPurpose::Capture => {
                state
                    .capture
                    .as_mut()
                    .and_then(|capture| match &mut capture.phase {
                        CapturePhase::Synchronizing(sync) => Some(sync),
                        _ => None,
                    })
            }
        };
        if let Some(sync) = slot.filter(|sync| sync.id == callback.id()) {
            sync.completed_at = Some(Instant::now());
        }
    }
}
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(mut capture) = state.capture.take() else {
            return;
        };
        if !capture
            .phase
            .frame_mut()
            .is_some_and(|current| current.proxy.id() == frame.id())
        {
            state.capture = Some(capture);
            return;
        }
        capture.phase = match (capture.phase, event) {
            (
                CapturePhase::Constraints(mut frame),
                zwlr_screencopy_frame_v1::Event::Buffer {
                    format: WEnum::Value(format),
                    width,
                    height,
                    stride,
                },
            ) => {
                if matches!(
                    format,
                    wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Abgr8888
                        | wl_shm::Format::Xbgr8888
                ) {
                    frame.spec = Some(BufferSpec {
                        format,
                        width,
                        height,
                        stride,
                    });
                }
                CapturePhase::Constraints(frame)
            }
            (CapturePhase::Constraints(mut frame), zwlr_screencopy_frame_v1::Event::BufferDone) => {
                frame.constraints_at = Some(Instant::now());
                CapturePhase::Constraints(frame)
            }
            (
                mut phase,
                zwlr_screencopy_frame_v1::Event::Flags {
                    flags: WEnum::Value(flags),
                },
            ) => {
                if let Some(frame) = phase.frame_mut() {
                    frame.inverted = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
                phase
            }
            (CapturePhase::Copying(copy), zwlr_screencopy_frame_v1::Event::Ready { .. }) => {
                CapturePhase::Readable(copy, Instant::now())
            }
            (CapturePhase::Constraints(frame), zwlr_screencopy_frame_v1::Event::Failed) => {
                CapturePhase::Failed(CaptureResources::Frame(frame))
            }
            (CapturePhase::Copying(copy), zwlr_screencopy_frame_v1::Event::Failed) => {
                CapturePhase::Failed(CaptureResources::Copy(copy))
            }
            (phase, _) => phase,
        };
        state.capture = Some(capture);
    }
}
delegate_noop!(WaylandState: ignore wl_shm::WlShm);
delegate_noop!(WaylandState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandState: ignore wl_buffer::WlBuffer);
delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(WaylandState: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(WaylandState: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(WaylandState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(WaylandState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

#[cfg(test)]
mod tests {
    use super::*;
    fn known_authority() -> InputAuthority {
        let authority = InputAuthority::default();
        authority.observe_lock(LockObservation {
            outcome: Ok(KnownLock::Unlocked),
            elapsed_us: 0,
        });
        authority
    }
    fn known_permit(generation: u64, expires_at: Instant) -> InputPermit {
        known_authority().permit(generation, expires_at)
    }

    fn geometry() -> DesktopGeometry {
        DesktopGeometry {
            output: "test".into(),
            width: 1,
            height: 2,
            scale: 1,
            transform: 0,
            generation: 1,
        }
    }
    struct InputFixture {
        geometry: DesktopGeometry,
        pressed: PressedInputs,
        events: Vec<&'static str>,
    }
    impl InputFixture {
        fn held() -> Self {
            let mut pressed = PressedInputs::default();
            pressed.key(KeyCode::new(42).unwrap(), Transition::Press);
            pressed.button(Button::Left, Transition::Press);
            Self {
                geometry: geometry(),
                pressed,
                events: Vec::new(),
            }
        }
        fn release(&mut self) -> Result<()> {
            if !self.pressed.keys.is_empty() {
                self.events.push("release key");
            }
            if !self.pressed.buttons.is_empty() {
                self.events.push("release button");
            }
            self.pressed = PressedInputs::default();
            Ok(())
        }
    }

    fn observation(outcome: std::result::Result<KnownLock, LockQueryFailure>) -> LockObservation {
        LockObservation {
            outcome,
            elapsed_us: 150_000,
        }
    }
    #[test]
    fn initial_unknown_never_emits_and_initial_locked_accepts_fresh_unlock_input() {
        let authority = InputAuthority::default();
        let mut keys = Vec::new();
        assert_eq!(authority.snapshot().phase, AuthorityPhase::Suspended);
        assert!(
            authority
                .permit(1, Instant::now() + Duration::from_secs(1))
                .emit(1, || {
                    keys.push(30);
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(
            authority.observe_lock(observation(Err(LockQueryFailure::Deadline))),
            LockEffect::None
        );
        assert_eq!(
            authority.observe_lock(observation(Err(LockQueryFailure::Io))),
            LockEffect::None
        );
        assert_eq!(authority.revision(), 0);
        assert!(authority.clipboard_guard().is_none());
        authority.observe_lock(observation(Ok(KnownLock::Locked)));
        authority
            .permit(1, Instant::now() + Duration::from_secs(1))
            .emit(1, || {
                keys.push(30);
                Ok(())
            })
            .unwrap();
        assert_eq!(keys, [30]);
        assert!(authority.clipboard_guard().is_none());
    }
    #[test]
    fn exact_ack_while_unknown_stays_suspended_until_same_state_is_verified() {
        let authority = known_authority();
        let old = authority.permit(1, Instant::now() + Duration::from_secs(1));
        assert_eq!(
            authority.observe_lock(observation(Err(LockQueryFailure::Deadline))),
            LockEffect::Release
        );
        assert!(!authority.acknowledge(1));
        assert_eq!(
            authority.observe_lock(observation(Err(LockQueryFailure::Spawn))),
            LockEffect::None
        );
        assert_eq!(authority.revision(), 1);
        assert_eq!(authority.released(), Some(1));
        assert!(!authority.acknowledge(0));
        assert!(authority.acknowledge(1));
        assert!(!authority.acknowledge(1));
        assert_eq!(authority.snapshot().phase, AuthorityPhase::Suspended);
        let mut keys = Vec::new();
        assert!(
            authority
                .permit(1, Instant::now() + Duration::from_secs(1))
                .emit(1, || {
                    keys.push(48);
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(authority.revision(), 1);
        authority.observe_lock(observation(Ok(KnownLock::Unlocked)));
        assert!(
            old.emit(1, || {
                keys.push(42);
                Ok(())
            })
            .is_err()
        );
        authority
            .permit(1, Instant::now() + Duration::from_secs(1))
            .emit(1, || {
                keys.push(30);
                Ok(())
            })
            .unwrap();
        assert_eq!(keys, [30]);
    }
    #[test]
    fn known_recovery_cannot_skip_cleanup_or_exact_ack() {
        let authority = known_authority();
        authority.observe_lock(observation(Err(LockQueryFailure::Deadline)));
        authority.observe_lock(observation(Ok(KnownLock::Unlocked)));
        assert!(!authority.is_open());
        assert!(!authority.acknowledge(1));
        assert_eq!(authority.released(), Some(1));
        assert!(!authority.is_open());
        assert!(authority.acknowledge(1));
        let mut keys = Vec::new();
        authority
            .permit(1, Instant::now() + Duration::from_secs(1))
            .emit(1, || {
                keys.push(30);
                Ok(())
            })
            .unwrap();
        assert_eq!(keys, [30]);
    }
    #[test]
    fn verified_transition_is_absorbing_even_after_ack_and_later_unknown() {
        for (first, second) in [
            (KnownLock::Unlocked, KnownLock::Locked),
            (KnownLock::Locked, KnownLock::Unlocked),
        ] {
            let authority = InputAuthority::default();
            authority.observe_lock(observation(Ok(first)));
            authority.observe_lock(observation(Err(LockQueryFailure::Deadline)));
            assert_eq!(authority.released(), Some(1));
            assert!(authority.acknowledge(1));
            assert_eq!(
                authority.observe_lock(observation(Ok(second))),
                LockEffect::DesktopResetRequired
            );
            assert_eq!(authority.released(), Some(2));
            assert!(authority.acknowledge(2));
            authority.observe_lock(observation(Err(LockQueryFailure::Deadline)));
            authority.observe_lock(observation(Ok(first)));
            assert_eq!(
                authority.snapshot().lock,
                LockKnowledge::Changed {
                    previous: first,
                    current: second
                }
            );
            let mut keys = Vec::new();
            assert!(
                authority
                    .permit(1, Instant::now() + Duration::from_secs(1))
                    .emit(1, || {
                        keys.push(42);
                        Ok(())
                    })
                    .is_err()
            );
            let replacement = InputAuthority::default();
            replacement.observe_lock(observation(Ok(second)));
            replacement
                .permit(1, Instant::now() + Duration::from_secs(1))
                .emit(1, || {
                    keys.push(30);
                    Ok(())
                })
                .unwrap();
            assert_eq!(keys, [30]);
        }
    }
    #[test]
    fn clipboard_guard_survives_focus_release_but_not_uncertainty_or_retirement() {
        let authority = known_authority();
        let queued = authority.clipboard_guard().unwrap();
        authority.close();
        assert_eq!(queued.with(|| "plain text"), Some("plain text"));
        authority.observe_lock(observation(Err(LockQueryFailure::Deadline)));
        authority.observe_lock(observation(Ok(KnownLock::Unlocked)));
        assert_eq!(queued.with(|| "stale text"), None);
        let fresh = authority.clipboard_guard().unwrap();
        assert_eq!(fresh.with(|| "fresh text"), Some("fresh text"));
        authority.retire();
        assert_eq!(fresh.with(|| "retired text"), None);
    }
    #[test]
    fn authority_snapshot_observes_exact_phase_and_revision_without_transitioning() {
        let authority = known_authority();
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::Open,
                revision: 0,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert_eq!(authority.close(), 1);
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::Releasing,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(!authority.acknowledge(1));
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::Releasing,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert_eq!(authority.released(), Some(1));
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::AwaitingAcknowledgement,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(!authority.acknowledge(0));
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::AwaitingAcknowledgement,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(authority.acknowledge(1));
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::Open,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
        assert!(!authority.acknowledge(1));
        assert_eq!(
            authority.snapshot(),
            AuthoritySnapshot {
                phase: AuthorityPhase::Open,
                revision: 1,
                lock: crate::desktop::LockKnowledge::Verified(crate::desktop::KnownLock::Unlocked),
                lock_generation: 1
            }
        );
    }

    #[test]
    fn reset_requires_completed_release_and_exact_ack_before_new_emission() {
        let authority = known_authority();
        let old = authority.permit(1, Instant::now() + Duration::from_secs(1));
        assert_eq!(authority.close(), 1);
        assert_eq!(authority.close(), 1);
        assert!(!authority.acknowledge(1));
        assert_eq!(authority.released(), Some(1));
        assert!(!authority.acknowledge(0));
        assert!(authority.acknowledge(1));
        let mut events = Vec::new();
        assert!(matches!(
            old.emit(1, || {
                events.push("old");
                Ok(())
            }),
            Err(DesktopError::Cancelled)
        ));
        authority
            .permit(1, Instant::now() + Duration::from_secs(1))
            .emit(1, || {
                events.push("new");
                Ok(())
            })
            .unwrap();
        assert_eq!(events, ["new"]);
        assert!(authority.is_open());
        assert!(!authority.acknowledge(0));
        assert!(authority.is_open());
    }

    #[test]
    fn writable_backpressure_retries_before_reporting_completion_and_has_a_deadline() {
        let mut attempts = 0;
        let mut waits = 0;
        flush_until(
            Instant::now() + Duration::from_secs(1),
            || {
                attempts += 1;
                Ok(attempts == 3)
            },
            |_| {
                waits += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((attempts, waits), (3, 2));
        let result = flush_until(
            Instant::now() + Duration::from_millis(10),
            || Ok(false),
            |_| {
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            },
        );
        assert!(matches!(result, Err(DesktopError::Timeout)));
    }

    #[test]
    fn revocation_during_preparation_rejects_without_touching_newer_held_state() {
        let mut target = InputFixture::held();
        let authority = known_authority();
        let permit = authority.permit(1, Instant::now() + Duration::from_secs(1));
        let result = input_with_permit(
            &mut target,
            permit,
            |target| {
                authority.revoke();
                Ok(target.geometry.clone())
            },
            |target, _| {
                target.events.push("new press");
                Ok(())
            },
            InputFixture::release,
        );
        assert!(matches!(result, Err(DesktopError::Cancelled)));
        assert!(target.events.is_empty());
        assert!(!target.pressed.keys.is_empty());
        assert!(!target.pressed.buttons.is_empty());
    }

    #[test]
    fn unresponsive_wayland_setup_can_be_cancelled_before_its_deadline() {
        let (socket, _compositor) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let revision = Arc::new(AtomicU64::new(0));
        let cancellation = Cancellation::new(revision.clone());
        let revoker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            revision.fetch_add(1, Ordering::AcqRel);
        });
        let began = Instant::now();
        let result = Desktop::connect_on("fixture", cancellation, connection);
        revoker.join().unwrap();
        assert!(matches!(result, Err(DesktopError::Cancelled)));
        assert!(began.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn helper_wait_is_interrupted_by_cancellation() {
        let revision = Arc::new(AtomicU64::new(0));
        let cancellation = Cancellation::new(revision.clone());
        let child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let revoker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            revision.fetch_add(1, Ordering::AcqRel);
        });
        let began = Instant::now();
        let result = bounded_output_cancellable(child, Duration::from_secs(2), 128, &cancellation);
        revoker.join().unwrap();
        assert!(matches!(result, Err(DesktopError::Cancelled)));
        assert!(began.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn delayed_preparation_cannot_emit_or_mutate_held_state_after_permission_expires() {
        for operation in ["key", "motion", "button", "axis"] {
            let mut target = InputFixture::held();
            let expires_at = Instant::now() + Duration::from_millis(20);
            let result = input_with_permit(
                &mut target,
                known_permit(1, expires_at),
                |target| {
                    std::thread::sleep(
                        expires_at.saturating_duration_since(Instant::now())
                            + Duration::from_millis(5),
                    );
                    Ok(target.geometry.clone())
                },
                |target, _| {
                    target.events.push(operation);
                    target
                        .pressed
                        .key(KeyCode::new(30).unwrap(), Transition::Press);
                    Ok(())
                },
                InputFixture::release,
            );
            assert!(matches!(result, Err(DesktopError::InputExpired)));
            assert_eq!(target.events, ["release key", "release button"]);
            assert!(target.pressed.keys.is_empty());
            assert!(target.pressed.buttons.is_empty());
        }
    }

    #[test]
    fn initialization_geometry_change_rejects_input_and_releases_held_state() {
        let mut target = InputFixture::held();
        let result = input_with_permit(
            &mut target,
            known_permit(1, Instant::now() + Duration::from_secs(1)),
            |target| {
                target.geometry.generation = 2;
                Ok(target.geometry.clone())
            },
            |target, _| {
                target.events.push("motion");
                Ok(())
            },
            InputFixture::release,
        );
        assert!(matches!(result, Err(DesktopError::StaleGeometry)));
        assert_eq!(target.events, ["release key", "release button"]);
        assert!(target.pressed.keys.is_empty());
        assert!(target.pressed.buttons.is_empty());
    }

    #[test]
    fn unexpired_prepared_input_emits_and_keeps_held_state() {
        let mut target = InputFixture::held();
        input_with_permit(
            &mut target,
            known_permit(1, Instant::now() + Duration::from_secs(1)),
            |target| Ok(target.geometry.clone()),
            |target, _| {
                target.events.push("key");
                target
                    .pressed
                    .key(KeyCode::new(30).unwrap(), Transition::Press);
                Ok(())
            },
            InputFixture::release,
        )
        .unwrap();
        assert_eq!(target.events, ["key"]);
        assert_eq!(target.pressed.keys.len(), 2);
        assert_eq!(target.pressed.buttons.len(), 1);
    }

    #[test]
    fn subprocess_output_has_a_size_limit_and_deadline() {
        let child = Command::new("printf")
            .arg("abc")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(
            bounded_output(child, Duration::from_secs(1), 3).unwrap(),
            b"abc"
        );
        let child = Command::new("printf")
            .arg("abcd")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(matches!(
            bounded_output(child, Duration::from_secs(1), 3),
            Err(DesktopError::ClipboardLimit)
        ));
        let child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(matches!(
            bounded_output(child, Duration::from_millis(20), 3),
            Err(DesktopError::Timeout)
        ));
    }
    #[test]
    fn padded_inverted_bgra_becomes_top_down_opaque_rgba() {
        let frame = Frame {
            ready_at: Instant::now(),
            geometry: geometry(),
            width: 1,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra,
            y_inverted: true,
            opaque: true,
            bytes: vec![10, 20, 30, 0, 9, 9, 9, 9, 40, 50, 60, 0, 9, 9, 9, 9],
        };
        assert_eq!(
            frame.rgba().unwrap(),
            vec![60, 50, 40, 255, 30, 20, 10, 255]
        );
    }
    #[test]
    fn rgba_retains_alpha_and_rejects_truncation() {
        let mut frame = Frame {
            ready_at: Instant::now(),
            geometry: geometry(),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Rgba,
            y_inverted: false,
            opaque: false,
            bytes: vec![1, 2, 3, 4],
        };
        assert_eq!(frame.rgba().unwrap(), vec![1, 2, 3, 4]);
        frame.bytes.pop();
        assert!(matches!(frame.rgba(), Err(DesktopError::InvalidFrame)));
    }
    #[test]
    fn unsafe_allocations_are_rejected() {
        for (w, h, s) in [
            (0, 1, 4),
            (2, 1, 4),
            (1, 0, 4),
            (u32::MAX, 1, u32::MAX),
            (1, u32::MAX, 4),
        ] {
            assert!(frame_size(w, h, s).is_err());
        }
    }
    #[test]
    fn coordinates_preserve_edges_and_reject_invalid_numbers() {
        assert_eq!(
            Position::new(1.0, 1.0).unwrap().pixels(1920, 1080).unwrap(),
            (1919, 1079)
        );
        assert_eq!(
            Position::new(0.0, 0.0).unwrap().pixels(1920, 1080).unwrap(),
            (0, 0)
        );
        for n in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
            assert!(Position::new(n, 0.0).is_err());
        }
    }
    #[test]
    fn unmatched_releases_and_duplicate_presses_do_not_change_remote_state() {
        let key = KeyCode::new(30).unwrap();
        let mut pressed = PressedInputs::default();
        assert!(!pressed.key(key, Transition::Release));
        assert!(pressed.key(key, Transition::Press));
        assert!(!pressed.key(key, Transition::Press));
        assert!(pressed.key(key, Transition::Release));
        assert!(pressed.keys.is_empty());
        assert!(!pressed.button(Button::Left, Transition::Release));
        assert!(pressed.button(Button::Left, Transition::Press));
        assert!(pressed.button(Button::Left, Transition::Release));
        assert!(pressed.buttons.is_empty());
    }

    #[test]
    fn lock_status_parsing_accepts_only_the_authoritative_boolean_object() {
        assert_eq!(
            parse_lock_state(br#"{"locked":false}"#),
            Some(LockState::Unlocked)
        );
        assert_eq!(
            parse_lock_state(br#"{"locked":true}"#),
            Some(LockState::Locked)
        );
        for bytes in [
            b"false".as_slice(),
            b"null",
            br#"{"locked":"false"}"#,
            br#"{"locked":false,"extra":1}"#,
            b"",
        ] {
            assert_eq!(parse_lock_state(bytes), None);
        }
    }
}

#[cfg(test)]
#[path = "desktop/protocol_tests.rs"]
mod protocol_tests;
