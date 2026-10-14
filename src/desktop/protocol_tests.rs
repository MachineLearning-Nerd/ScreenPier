use super::*;
use std::{os::unix::net::UnixStream, sync::atomic::AtomicBool, thread};

#[derive(Default)]
struct PeerState {
    pause: Option<CaptureStage>,
    input_sync_count: usize,
    pause_input_at: Option<usize>,
    paused_input: Option<u32>,
    pointer_events: Vec<(u16, Vec<u32>)>,
    sync: Option<u32>,
    late_sync: Option<u32>,
    release_late: bool,
    frame: Option<u32>,
    constraints_sent: bool,
    copied: bool,
    ready_sent: bool,
    keys: Vec<(u32, bool)>,
    ready_on_sync: bool,
    geometry: Option<u32>,
    remove: bool,
    fail: bool,
    close: bool,
    geometry_steps: Vec<u32>,
}
struct Peer {
    state: Arc<Mutex<PeerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}
fn string(value: &str) -> Vec<u8> {
    let mut data = words(&[(value.len() + 1) as u32]);
    data.extend(value.bytes());
    data.push(0);
    while data.len() % 4 != 0 {
        data.push(0);
    }
    data
}
fn send(socket: &mut UnixStream, id: u32, opcode: u16, payload: &[u8]) {
    let mut data = words(&[id, (((payload.len() + 8) as u32) << 16) | opcode as u32]);
    data.extend(payload);
    if let Err(error) = socket.write_all(&data) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
fn done(socket: &mut UnixStream, id: u32) {
    send(socket, id, 0, &words(&[1]));
    send(socket, 1, 1, &words(&[id]));
}
fn constraints(socket: &mut UnixStream, id: u32) {
    send(
        socket,
        id,
        0,
        &words(&[wl_shm::Format::Xrgb8888 as u32, 2, 2, 8]),
    );
    send(socket, id, 6, &[]);
}
fn ready(socket: &mut UnixStream, id: u32) {
    send(socket, id, 2, &words(&[0, 1, 0]));
}
impl Peer {
    fn dispatch_ready_and_output_change(&self, desktop: &mut Desktop, removal: bool) {
        {
            let mut state = self.state.lock().unwrap();
            state.ready_on_sync = true;
            if removal {
                state.remove = true;
            } else {
                state.geometry_steps = vec![3, 2];
            }
        }
        for _ in 0..2 {
            desktop
                .sync_for(SyncPurpose::Input, Duration::from_millis(80))
                .unwrap();
        }
    }
    fn start() -> (Self, Desktop) {
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let state = Arc::new(Mutex::new(PeerState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            let mut objects = BTreeMap::from([(1, "wl_display".to_string())]);
            let mut pending = Vec::new();
            let mut registry = 0;
            let mut output = 0;
            while !stopped.load(Ordering::Acquire) {
                let mut buffer = [0; 4096];
                match server.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => pending.extend_from_slice(&buffer[..count]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(error) => panic!("peer read failed {error}"),
                }
                while pending.len() >= 8 {
                    let id = u32::from_ne_bytes(pending[..4].try_into().unwrap());
                    let header = u32::from_ne_bytes(pending[4..8].try_into().unwrap());
                    let size = (header >> 16) as usize;
                    assert!((8..=4096).contains(&size));
                    if pending.len() < size {
                        break;
                    }
                    let message = pending.drain(..size).collect::<Vec<_>>();
                    let args = &message[8..];
                    let arg = |offset: usize| {
                        u32::from_ne_bytes(args[offset..offset + 4].try_into().unwrap())
                    };
                    let mut state = shared.lock().unwrap();
                    match (objects.get(&id).map(String::as_str), header as u16) {
                        (Some("wl_display"), 1) => {
                            registry = arg(0);
                            objects.insert(registry, "wl_registry".into());
                            for (name, interface, version) in [
                                (1, "wl_output", 4),
                                (2, "wl_shm", 1),
                                (3, "wl_seat", 7),
                                (4, "zwlr_screencopy_manager_v1", 3),
                                (5, "zwp_virtual_keyboard_manager_v1", 1),
                                (6, "zwlr_virtual_pointer_manager_v1", 2),
                            ] {
                                let mut payload = words(&[name]);
                                payload.extend(string(interface));
                                payload.extend(words(&[version]));
                                send(&mut server, registry, 0, &payload);
                            }
                        }
                        (Some("wl_display"), 0) => {
                            let callback = arg(0);
                            state.input_sync_count += 1;
                            if state.pause_input_at == Some(state.input_sync_count) {
                                state.paused_input = Some(callback);
                            } else if state.pause == Some(CaptureStage::Synchronize)
                                && state.sync.is_none()
                            {
                                state.sync = Some(callback);
                            } else {
                                if state.ready_on_sync {
                                    ready(&mut server, state.frame.unwrap());
                                    state.ready_sent = true;
                                    state.ready_on_sync = false;
                                }
                                done(&mut server, callback);
                            }
                        }
                        (Some("wl_registry"), 0) => {
                            let len = arg(4) as usize;
                            let interface =
                                String::from_utf8(args[8..8 + len - 1].to_vec()).unwrap();
                            let new_id = arg(8 + len.div_ceil(4) * 4 + 4);
                            if interface == "wl_output" {
                                output = new_id;
                                send(&mut server, output, 4, &string("fixture"));
                                send(&mut server, output, 1, &words(&[1, 2, 2, 60000]));
                                send(&mut server, output, 2, &[]);
                            }
                            objects.insert(new_id, interface);
                        }
                        (Some("zwlr_screencopy_manager_v1"), 0) => {
                            let frame = arg(0);
                            objects.insert(frame, "frame".into());
                            state.frame = Some(frame);
                            state.constraints_sent = false;
                            state.copied = false;
                            state.ready_sent = false;
                        }
                        (Some("wl_shm"), 0) => {
                            objects.insert(arg(0), "pool".into());
                        }
                        (Some("pool"), 0) => {
                            objects.insert(arg(0), "buffer".into());
                        }
                        (Some("frame"), 0) => state.copied = true,
                        (Some("zwp_virtual_keyboard_manager_v1"), 0) => {
                            objects.insert(arg(4), "keyboard".into());
                        }
                        (Some("keyboard"), 1) => state.keys.push((arg(4), arg(8) == 1)),
                        (Some("zwlr_virtual_pointer_manager_v1"), 2) => {
                            objects.insert(arg(8), "pointer".into());
                        }
                        (Some("pointer"), opcode) => {
                            state.pointer_events.push((
                                opcode,
                                args.chunks_exact(4)
                                    .map(|part| u32::from_ne_bytes(part.try_into().unwrap()))
                                    .collect(),
                            ));
                        }
                        _ => {}
                    }
                }
                let mut state = shared.lock().unwrap();
                if state.pause_input_at.is_none() {
                    if let Some(callback) = state.paused_input.take() {
                        done(&mut server, callback);
                    }
                }
                if std::mem::take(&mut state.release_late) {
                    done(&mut server, state.late_sync.take().unwrap());
                }
                if state.pause != Some(CaptureStage::Synchronize) {
                    if let Some(sync) = state.sync.take() {
                        done(&mut server, sync);
                    }
                }
                if let Some(frame) = state.frame {
                    if !state.constraints_sent && state.pause != Some(CaptureStage::Constraints) {
                        constraints(&mut server, frame);
                        state.constraints_sent = true;
                    }
                    if state.copied && !state.ready_sent && state.pause != Some(CaptureStage::Ready)
                    {
                        ready(&mut server, frame);
                        state.ready_sent = true;
                    }
                    if std::mem::take(&mut state.fail) {
                        send(&mut server, frame, 3, &[]);
                    }
                }
                if state.close {
                    break;
                }
                for width in std::mem::take(&mut state.geometry_steps) {
                    send(&mut server, output, 1, &words(&[1, width, 2, 60000]));
                    send(&mut server, output, 2, &[]);
                }
                if let Some(width) = state.geometry.take() {
                    send(&mut server, output, 1, &words(&[1, width, 2, 60000]));
                    send(&mut server, output, 2, &[]);
                }
                if std::mem::take(&mut state.remove) {
                    send(&mut server, registry, 1, &words(&[1]));
                }
            }
        });
        let desktop = Desktop::connect_on(
            "fixture",
            Cancellation::default(),
            Connection::from_socket(client).unwrap(),
        )
        .unwrap();
        (
            Self {
                state,
                stop,
                worker: Some(worker),
            },
            desktop,
        )
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn permit() -> InputPermit {
    let authority = InputAuthority::default();
    authority.observe_lock(LockObservation {
        outcome: Ok(KnownLock::Unlocked),
        elapsed_us: 0,
    });
    authority.permit(1, Instant::now() + Duration::from_secs(1))
}
fn poll_until(desktop: &mut Desktop, predicate: impl Fn(&Desktop) -> bool) {
    let deadline = Instant::now() + Duration::from_millis(100);
    while !predicate(desktop) {
        assert!(matches!(
            desktop.poll_capture().unwrap(),
            CapturePoll::Pending
        ));
        assert!(
            Instant::now() < deadline,
            "production capture did not reach stage"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn native_keyboard_never_emits_under_unknown_or_absorbing_transition() {
    let (peer, mut desktop) = Peer::start();
    let authority = InputAuthority::default();
    let fresh = || authority.permit(1, Instant::now() + Duration::from_secs(1));
    assert!(matches!(
        desktop.key(KeyCode::new(42).unwrap(), Transition::Press, fresh()),
        Err(DesktopError::Cancelled)
    ));
    authority.observe_lock(LockObservation {
        outcome: Ok(KnownLock::Unlocked),
        elapsed_us: 0,
    });
    desktop
        .key(KeyCode::new(30).unwrap(), Transition::Press, fresh())
        .unwrap();
    desktop
        .key(KeyCode::new(30).unwrap(), Transition::Release, fresh())
        .unwrap();
    authority.observe_lock(LockObservation {
        outcome: Err(LockQueryFailure::Deadline),
        elapsed_us: 150_000,
    });
    desktop.release_all().unwrap();
    assert_eq!(authority.released(), Some(1));
    assert!(authority.acknowledge(1));
    assert!(matches!(
        desktop.key(KeyCode::new(42).unwrap(), Transition::Press, fresh()),
        Err(DesktopError::Cancelled)
    ));
    authority.observe_lock(LockObservation {
        outcome: Ok(KnownLock::Locked),
        elapsed_us: 0,
    });
    desktop.release_all().unwrap();
    assert_eq!(authority.released(), Some(2));
    assert!(authority.acknowledge(2));
    authority.observe_lock(LockObservation {
        outcome: Err(LockQueryFailure::Deadline),
        elapsed_us: 150_000,
    });
    assert!(matches!(
        desktop.key(KeyCode::new(42).unwrap(), Transition::Press, fresh()),
        Err(DesktopError::Cancelled)
    ));
    let replacement = InputAuthority::default();
    replacement.observe_lock(LockObservation {
        outcome: Ok(KnownLock::Locked),
        elapsed_us: 0,
    });
    let fresh = || replacement.permit(1, Instant::now() + Duration::from_secs(1));
    desktop
        .key(KeyCode::new(48).unwrap(), Transition::Press, fresh())
        .unwrap();
    desktop
        .key(KeyCode::new(48).unwrap(), Transition::Release, fresh())
        .unwrap();
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(80))
        .unwrap();
    assert_eq!(
        peer.state.lock().unwrap().keys,
        [(30, true), (30, false), (48, true), (48, false)]
    );
}
#[test]
fn production_capture_yields_to_literal_keyboard_in_every_pending_stage() {
    for stage in [
        CaptureStage::Synchronize,
        CaptureStage::Constraints,
        CaptureStage::Ready,
    ] {
        let (peer, mut desktop) = Peer::start();
        desktop
            .key(KeyCode::new(42).unwrap(), Transition::Press, permit())
            .unwrap();
        peer.state.lock().unwrap().pause = Some(stage);
        desktop.begin_capture(Duration::from_millis(120)).unwrap();
        poll_until(&mut desktop, |desktop| {
            let capture = desktop.state.capture.as_ref().unwrap();
            match stage {
                CaptureStage::Synchronize => peer.state.lock().unwrap().sync.is_some(),
                CaptureStage::Constraints => matches!(capture.phase, CapturePhase::Constraints(_)),
                CaptureStage::Ready => matches!(capture.phase, CapturePhase::Copying(_)),
            }
        });
        for (code, transition) in [
            (42, Transition::Release),
            (30, Transition::Press),
            (30, Transition::Release),
        ] {
            desktop
                .key(KeyCode::new(code).unwrap(), transition, permit())
                .unwrap();
        }
        desktop
            .sync_for(SyncPurpose::Input, Duration::from_millis(80))
            .unwrap();
        assert_eq!(
            peer.state.lock().unwrap().keys,
            [(42, true), (42, false), (30, true), (30, false)]
        );
        assert!(desktop.pressed.keys.is_empty());
        assert!(matches!(
            desktop.poll_capture().unwrap(),
            CapturePoll::Pending
        ));
        desktop.abort_capture();
    }
}
#[test]
fn ready_dispatched_during_input_keeps_its_original_stamp() {
    let (peer, mut desktop) = Peer::start();
    peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
    desktop.begin_capture(Duration::from_millis(120)).unwrap();
    poll_until(&mut desktop, |desktop| {
        matches!(
            desktop.state.capture.as_ref().unwrap().phase,
            CapturePhase::Copying(_)
        )
    });
    peer.state.lock().unwrap().ready_on_sync = true;
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(80))
        .unwrap();
    let original = match desktop.state.capture.as_ref().unwrap().phase {
        CapturePhase::Readable(_, at) => at,
        _ => panic!("Ready was not dispatched during input sync"),
    };
    thread::sleep(Duration::from_millis(5));
    let CapturePoll::Complete(frame) = desktop.poll_capture().unwrap() else {
        panic!("missing completed image")
    };
    assert_eq!(frame.ready_at, original);
    assert_eq!(frame.bytes, [0; 16]);
}
#[test]
fn every_pending_stage_times_out_on_the_original_deadline() {
    for stage in [
        CaptureStage::Synchronize,
        CaptureStage::Constraints,
        CaptureStage::Ready,
    ] {
        let (peer, mut desktop) = Peer::start();
        peer.state.lock().unwrap().pause = Some(stage);
        desktop.begin_capture(Duration::from_millis(25)).unwrap();
        let deadline = desktop.state.capture.as_ref().unwrap().deadline;
        poll_until(&mut desktop, |desktop| match stage {
            CaptureStage::Synchronize => peer.state.lock().unwrap().sync.is_some(),
            CaptureStage::Constraints => matches!(
                desktop.state.capture.as_ref().unwrap().phase,
                CapturePhase::Constraints(_)
            ),
            CaptureStage::Ready => matches!(
                desktop.state.capture.as_ref().unwrap().phase,
                CapturePhase::Copying(_)
            ),
        });
        assert_eq!(desktop.state.capture.as_ref().unwrap().deadline, deadline);
        thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
        );
        assert!(
            matches!(desktop.poll_capture(), Err(DesktopError::CaptureTimeout(found)) if found == stage)
        );
        assert!(desktop.state.capture.is_none());
    }
}

#[test]
fn geometry_round_trip_and_output_removal_never_publish_old_pixels() {
    for removal in [false, true] {
        let (peer, mut desktop) = Peer::start();
        peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
        desktop.begin_capture(Duration::from_millis(120)).unwrap();
        poll_until(&mut desktop, |desktop| {
            matches!(
                desktop.state.capture.as_ref().unwrap().phase,
                CapturePhase::Copying(_)
            )
        });
        peer.dispatch_ready_and_output_change(&mut desktop, removal);
        let result = desktop.poll_capture();
        if removal {
            assert!(matches!(result, Err(DesktopError::OutputUnavailable)));
        } else {
            assert_eq!(desktop.geometry().unwrap().generation, 3);
            assert!(matches!(result, Err(DesktopError::StaleGeometry)));
        }
        assert!(desktop.state.capture.is_none());
    }
}
#[test]
fn cancellation_failure_and_eof_discard_pending_storage() {
    for reason in 0..3 {
        let (peer, mut desktop) = Peer::start();
        peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
        desktop.begin_capture(Duration::from_millis(120)).unwrap();
        poll_until(&mut desktop, |desktop| {
            matches!(
                desktop.state.capture.as_ref().unwrap().phase,
                CapturePhase::Copying(_)
            )
        });
        match reason {
            0 => {
                desktop.cancellation.revision.fetch_add(1, Ordering::AcqRel);
            }
            1 => {
                peer.state.lock().unwrap().fail = true;
                desktop
                    .sync_for(SyncPurpose::Input, Duration::from_millis(80))
                    .unwrap();
                desktop
                    .sync_for(SyncPurpose::Input, Duration::from_millis(80))
                    .unwrap();
            }
            _ => {
                peer.state.lock().unwrap().close = true;
                thread::sleep(Duration::from_millis(5));
            }
        }
        let result = desktop.poll_capture();
        match reason {
            0 => assert!(matches!(result, Err(DesktopError::Cancelled))),
            1 => assert!(matches!(result, Err(DesktopError::CaptureFailed))),
            _ => assert!(matches!(
                result,
                Err(DesktopError::Connection(_) | DesktopError::Dispatch(_))
            )),
        }
        assert!(desktop.state.capture.is_none());
    }
}
#[test]
fn aborted_storage_is_disjoint_from_successor_even_with_late_writes() {
    use std::os::unix::fs::FileExt;
    let (peer, mut desktop) = Peer::start();
    peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
    desktop.begin_capture(Duration::from_millis(120)).unwrap();
    poll_until(&mut desktop, |desktop| {
        matches!(
            desktop.state.capture.as_ref().unwrap().phase,
            CapturePhase::Copying(_)
        )
    });
    let retired_file = match &desktop.state.capture.as_ref().unwrap().phase {
        CapturePhase::Copying(copy) => copy.storage._file.try_clone().unwrap(),
        _ => unreachable!(),
    };
    desktop.abort_capture();
    desktop.begin_capture(Duration::from_millis(120)).unwrap();
    poll_until(&mut desktop, |desktop| {
        matches!(
            desktop.state.capture.as_ref().unwrap().phase,
            CapturePhase::Copying(_)
        )
    });
    retired_file.write_all_at(&[255; 16], 0).unwrap();
    let current = match &desktop.state.capture.as_ref().unwrap().phase {
        CapturePhase::Copying(copy) => &copy.storage.mapping[..],
        _ => unreachable!(),
    };
    assert_eq!(current, [0; 16]);
    peer.state.lock().unwrap().ready_on_sync = true;
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(80))
        .unwrap();
    let CapturePoll::Complete(frame) = desktop.poll_capture().unwrap() else {
        panic!("successor did not complete")
    };
    assert_eq!(frame.bytes, [0; 16]);
}

#[test]
fn obsolete_capture_callback_cannot_satisfy_successor_or_input_fence() {
    let (peer, mut desktop) = Peer::start();
    peer.state.lock().unwrap().pause = Some(CaptureStage::Synchronize);
    desktop.begin_capture(Duration::from_millis(120)).unwrap();
    poll_until(&mut desktop, |_| peer.state.lock().unwrap().sync.is_some());
    let old_id = match &desktop.state.capture.as_ref().unwrap().phase {
        CapturePhase::Synchronizing(sync) => sync.id.clone(),
        _ => unreachable!(),
    };
    desktop.abort_capture();
    {
        let mut state = peer.state.lock().unwrap();
        state.late_sync = state.sync.take();
    }
    desktop.begin_capture(Duration::from_millis(120)).unwrap();
    poll_until(&mut desktop, |_| peer.state.lock().unwrap().sync.is_some());
    peer.state.lock().unwrap().release_late = true;
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(80))
        .unwrap();
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(80))
        .unwrap();
    assert!(matches!(
        desktop.poll_capture().unwrap(),
        CapturePoll::Pending
    ));
    let CapturePhase::Synchronizing(current) = &desktop.state.capture.as_ref().unwrap().phase
    else {
        panic!("obsolete callback advanced successor")
    };
    assert_ne!(current.id, old_id);
    assert!(current.completed_at.is_none());
    assert!(desktop.state.input_sync.is_none());
}
#[test]
fn timely_ready_is_not_restamped_or_rejected_when_materialized_after_deadline() {
    let (peer, mut desktop) = Peer::start();
    peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
    desktop.begin_capture(Duration::from_millis(40)).unwrap();
    poll_until(&mut desktop, |desktop| {
        matches!(
            desktop.state.capture.as_ref().unwrap().phase,
            CapturePhase::Copying(_)
        )
    });
    peer.state.lock().unwrap().ready_on_sync = true;
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(30))
        .unwrap();
    let (original, deadline) = match &desktop.state.capture.as_ref().unwrap() {
        CaptureOperation {
            deadline,
            phase: CapturePhase::Readable(_, at),
        } => (*at, *deadline),
        _ => unreachable!(),
    };
    assert!(original < deadline);
    thread::sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    let CapturePoll::Complete(frame) = desktop.poll_capture().unwrap() else {
        panic!("timely ready was lost")
    };
    assert_eq!(frame.ready_at, original);
    assert!(frame.ready_at < deadline);
}
#[test]
fn late_ready_still_expires_instead_of_becoming_fresh() {
    let (peer, mut desktop) = Peer::start();
    peer.state.lock().unwrap().pause = Some(CaptureStage::Ready);
    desktop.begin_capture(Duration::from_millis(25)).unwrap();
    poll_until(&mut desktop, |desktop| {
        matches!(
            desktop.state.capture.as_ref().unwrap().phase,
            CapturePhase::Copying(_)
        )
    });
    let deadline = desktop.state.capture.as_ref().unwrap().deadline;
    thread::sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    peer.state.lock().unwrap().ready_on_sync = true;
    desktop
        .sync_for(SyncPurpose::Input, Duration::from_millis(30))
        .unwrap();
    assert!(matches!(
        desktop.poll_capture(),
        Err(DesktopError::CaptureTimeout(CaptureStage::Ready))
    ));
    assert!(desktop.state.capture.is_none());
}

struct RefreshPeer {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    request: Arc<Mutex<Vec<u8>>>,
    release: std::sync::mpsc::Sender<Vec<u8>>,
    worker: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    hold_open: Arc<AtomicBool>,
}
impl RefreshPeer {
    fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("refresh.sock");
        Self::start_in(directory, path)
    }
    fn start_at(path: std::path::PathBuf) -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), path)
    }
    fn start_in(directory: tempfile::TempDir, path: std::path::PathBuf) -> Self {
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let request = Arc::new(Mutex::new(Vec::new()));
        let received = request.clone();
        let (release, ready) = std::sync::mpsc::channel::<Vec<u8>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let hold_open = Arc::new(AtomicBool::new(false));
        let hold = hold_open.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut socket = loop {
                if let Ok((socket, _)) = listener.accept() {
                    break socket;
                }
                if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            };
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut bytes = [0; 1024];
            if let Ok(count) = socket.read(&mut bytes) {
                received.lock().unwrap().extend_from_slice(&bytes[..count]);
            }
            if let Ok(reply) = ready.recv_timeout(Duration::from_secs(2)) {
                let _ = socket.write_all(&reply);
                while hold.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
        Self {
            stop,
            hold_open,
            _directory: directory,
            path,
            request,
            release,
            worker: Some(worker),
        }
    }
    fn attach(&self, desktop: &mut Desktop) {
        desktop.pointer_adapter = PointerAdapter::Hyprland(self.path.clone());
    }
    fn complete(&self, bytes: &[u8]) {
        self.release.send(bytes.to_vec()).unwrap();
    }
}
impl Drop for RefreshPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.release.send(Vec::new());
        self.worker.take().unwrap().join().unwrap();
    }
}
fn poll_button_until(desktop: &mut Desktop, predicate: impl Fn(&Desktop) -> bool) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while !predicate(desktop) {
        assert!(matches!(desktop.poll_input().unwrap(), InputPoll::Pending));
        assert!(Instant::now() < deadline, "button stage was not reached");
        thread::sleep(Duration::from_millis(1));
    }
}
fn complete_button(desktop: &mut Desktop) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while matches!(desktop.poll_input().unwrap(), InputPoll::Pending) {
        assert!(Instant::now() < deadline, "button did not complete");
        thread::sleep(Duration::from_millis(1));
    }
}
fn button_pairs(peer: &Peer) -> Vec<(u32, u32)> {
    peer.state
        .lock()
        .unwrap()
        .pointer_events
        .iter()
        .filter(|(opcode, _)| *opcode == 2)
        .map(|(_, args)| (args[1], args[2]))
        .collect()
}
#[test]
fn native_button_waits_for_initialization_motion_and_exact_refresh_reply() {
    let (peer, mut desktop) = Peer::start();
    let refresh = RefreshPeer::start();
    refresh.attach(&mut desktop);
    {
        let mut state = peer.state.lock().unwrap();
        state.pause_input_at = Some(state.input_sync_count + 1);
    }
    desktop
        .begin_button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit(),
        )
        .unwrap();
    for _ in 0..4 {
        assert!(matches!(desktop.poll_input().unwrap(), InputPoll::Pending));
    }
    assert!(desktop.pointer.is_none());
    assert_eq!(button_pairs(&peer), []);
    assert_eq!(*refresh.request.lock().unwrap(), Vec::<u8>::new());
    peer.state.lock().unwrap().pause_input_at = None;
    poll_button_until(&mut desktop, |desktop| {
        matches!(
            desktop.input.as_ref().unwrap().stage,
            ButtonStage::RefreshReply
        )
    });
    let deadline = Instant::now() + Duration::from_millis(100);
    while refresh.request.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let request = refresh.request.lock().unwrap().clone();
    assert_eq!(
        std::str::from_utf8(&request).unwrap(),
        "repl local function refresh() local p=hl.get_cursor_pos(); local r=hl.dispatch(hl.dsp.cursor.move({x=p.x,y=p.y})); if type(r)~=\"table\" or r.ok~=true then error(\"refresh failed\") end; return \"twa_pointer_ready_v1\" end return refresh()"
    );
    assert_eq!(button_pairs(&peer), []);
    refresh.complete(b"twa_pointer_ready_v1");
    complete_button(&mut desktop);
    desktop
        .button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Release,
            permit(),
        )
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
    let events = peer.state.lock().unwrap().pointer_events.clone();
    assert_eq!(
        events
            .iter()
            .filter(|(opcode, _)| *opcode == 1)
            .map(|(_, args)| args[1..].to_vec())
            .collect::<Vec<_>>(),
        [vec![1, 1, 2, 2], vec![1, 1, 2, 2]]
    );
}
#[test]
fn native_rejected_refresh_does_not_press_and_generic_input_remains_operational() {
    for reply in [
        b"ok".as_slice(),
        b"twa_pointer_ready_v1\n",
        b"twa_pointer_ready_v1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    ] {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        poll_button_until(&mut desktop, |desktop| {
            matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::RefreshReply
            )
        });
        refresh.complete(reply);
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            match desktop.poll_input() {
                Err(DesktopError::PointerPreparation(PointerPreparationFault::InvalidReply)) => {
                    break;
                }
                Ok(InputPoll::Pending) => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                _ => panic!("invalid refresh accepted"),
            }
        }
        desktop.release_all().unwrap();
        desktop.pointer_adapter = PointerAdapter::Generic;
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Release,
                permit(),
            )
            .unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
    }
}
#[test]
fn native_pending_button_is_cancelled_at_each_cooperative_stage() {
    for stage in [0, 1, 2, 3] {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        poll_button_until(&mut desktop, |desktop| match stage {
            0 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::GeometrySync
            ),
            1 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::DeviceSync
            ),
            2 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::MotionSync
            ),
            _ => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::RefreshReply
            ),
        });
        desktop.release_all().unwrap();
        refresh.complete(b"twa_pointer_ready_v1");
        assert!(matches!(desktop.poll_input().unwrap(), InputPoll::Complete));
        desktop.pointer_adapter = PointerAdapter::Generic;
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Press,
                permit(),
            )
            .unwrap();
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Release,
                permit(),
            )
            .unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), [(273, 1), (273, 0)]);
    }
}
#[test]
fn native_chord_drag_duplicate_and_release_never_launch_refresh() {
    let (peer, mut desktop) = Peer::start();
    let position = Position::new(0.5, 0.5).unwrap();
    desktop
        .button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    desktop.pointer_adapter = PointerAdapter::Unsupported;
    desktop
        .button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    desktop
        .button(position, Button::Right, Transition::Press, permit())
        .unwrap();
    desktop
        .pointer_absolute(Position::new(0.0, 0.0).unwrap(), permit())
        .unwrap();
    desktop
        .button(position, Button::Right, Transition::Release, permit())
        .unwrap();
    desktop
        .button(position, Button::Left, Transition::Release, permit())
        .unwrap();
    assert!(matches!(
        desktop.button(position, Button::Left, Transition::Press, permit()),
        Err(DesktopError::PointerPreparation(
            PointerPreparationFault::Unsupported
        ))
    ));
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(
        button_pairs(&peer),
        [(272, 1), (273, 1), (273, 0), (272, 0)]
    );
}

#[test]
fn native_incomplete_refresh_write_never_emits_a_press_or_retries_a_suffix() {
    let request = b"repl local function refresh() local p=hl.get_cursor_pos(); local r=hl.dispatch(hl.dsp.cursor.move({x=p.x,y=p.y})); if type(r)~=\"table\" or r.ok~=true then error(\"refresh failed\") end; return \"twa_pointer_ready_v1\" end return refresh()";
    for limit in (0..request.len()).chain(std::iter::once(usize::MAX)) {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        desktop
            .input
            .as_mut()
            .unwrap()
            .refresh
            .as_mut()
            .unwrap()
            .write_limit = Some(limit);
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match desktop.poll_input() {
                Err(DesktopError::PointerPreparation(PointerPreparationFault::ShortWrite))
                    if limit != usize::MAX =>
                {
                    break;
                }
                Err(DesktopError::PointerPreparation(PointerPreparationFault::Unavailable))
                    if limit == usize::MAX =>
                {
                    break;
                }
                Ok(InputPoll::Pending) => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                _ => panic!("partial refresh was accepted"),
            }
        }
        refresh.complete(b"twa_pointer_ready_v1");
        desktop.release_all().unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), []);
        let deadline = Instant::now() + Duration::from_millis(200);
        while limit != 0 && limit != usize::MAX && refresh.request.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            *refresh.request.lock().unwrap(),
            if limit == usize::MAX {
                vec![]
            } else {
                request[..limit].to_vec()
            }
        );
    }
    let (peer, mut desktop) = Peer::start();
    desktop
        .button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit(),
        )
        .unwrap();
    desktop
        .button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Release,
            permit(),
        )
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
}
#[test]
fn native_pending_refresh_rejects_expiry_geometry_authority_and_lock_changes() {
    for change in 0..6 {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        let authority = InputAuthority::default();
        authority.observe_lock(LockObservation {
            outcome: Ok(KnownLock::Unlocked),
            elapsed_us: 0,
        });
        let permit = authority.permit(1, Instant::now() + Duration::from_secs(1));
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit,
            )
            .unwrap();
        poll_button_until(&mut desktop, |desktop| {
            matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::RefreshReply
            )
        });
        match change {
            0 => desktop.input.as_mut().unwrap().permit.expires_at = Instant::now(),
            1 => {
                peer.state.lock().unwrap().geometry = Some(3);
                desktop.sync(Duration::from_millis(100)).unwrap();
            }
            2 => {
                authority.revoke();
            }
            3 => {
                authority.observe_lock(LockObservation {
                    outcome: Err(LockQueryFailure::Deadline),
                    elapsed_us: 150_000,
                });
            }
            4 => {
                authority.retire();
            }
            _ => {
                authority.observe_lock(LockObservation {
                    outcome: Ok(KnownLock::Locked),
                    elapsed_us: 0,
                });
            }
        }
        refresh.complete(b"twa_pointer_ready_v1");
        let result = desktop.poll_input();
        match change {
            0 => assert!(matches!(result, Err(DesktopError::InputExpired))),
            1 => assert!(matches!(result, Err(DesktopError::StaleGeometry))),
            _ => assert!(matches!(result, Err(DesktopError::Cancelled))),
        }
        desktop.release_all().unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), []);
        let other = InputAuthority::default();
        other.observe_lock(LockObservation {
            outcome: Ok(KnownLock::Unlocked),
            elapsed_us: 0,
        });
        desktop.pointer_adapter = PointerAdapter::Generic;
        let fresh = || {
            other.permit(
                desktop.geometry().unwrap().generation,
                Instant::now() + Duration::from_secs(1),
            )
        };
        let next = fresh();
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Press,
                next,
            )
            .unwrap();
        let next = other.permit(
            desktop.geometry().unwrap().generation,
            Instant::now() + Duration::from_secs(1),
        );
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Release,
                next,
            )
            .unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), [(273, 1), (273, 0)]);
    }
}
#[test]
fn native_refused_refresh_releases_previous_keys_and_keeps_literal_press_absent() {
    let (peer, mut desktop) = Peer::start();
    let directory = tempfile::tempdir().unwrap();
    desktop
        .key(KeyCode::new(42).unwrap(), Transition::Press, permit())
        .unwrap();
    desktop.pointer_adapter = PointerAdapter::Hyprland(directory.path().join("absent.sock"));
    assert!(matches!(
        desktop.button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit()
        ),
        Err(DesktopError::PointerPreparation(
            PointerPreparationFault::Unavailable
        ))
    ));
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(peer.state.lock().unwrap().keys, [(42, true), (42, false)]);
    assert_eq!(button_pairs(&peer), []);
}
#[test]
fn native_exact_marker_without_eof_stays_pending_and_expiry_rejects_late_completion() {
    let (peer, mut desktop) = Peer::start();
    let refresh = RefreshPeer::start();
    refresh.attach(&mut desktop);
    refresh.hold_open.store(true, Ordering::Release);
    desktop
        .begin_button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit(),
        )
        .unwrap();
    poll_button_until(&mut desktop, |desktop| {
        matches!(
            desktop.input.as_ref().unwrap().stage,
            ButtonStage::RefreshReply
        )
    });
    refresh.complete(b"twa_pointer_ready_v1");
    for _ in 0..4 {
        assert!(matches!(desktop.poll_input().unwrap(), InputPoll::Pending));
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(button_pairs(&peer), []);
    desktop.input.as_mut().unwrap().permit.expires_at = Instant::now();
    refresh.hold_open.store(false, Ordering::Release);
    assert!(matches!(
        desktop.poll_input(),
        Err(DesktopError::InputExpired)
    ));
    desktop.release_all().unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), []);
    desktop.pointer_adapter = PointerAdapter::Generic;
    desktop
        .button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Right,
            Transition::Press,
            permit(),
        )
        .unwrap();
    desktop
        .button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Right,
            Transition::Release,
            permit(),
        )
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(273, 1), (273, 0)]);
}
#[test]
fn native_pointer_initialization_and_motion_sync_yield_to_capture_and_cleanup() {
    for stage in 0..3 {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        {
            let mut state = peer.state.lock().unwrap();
            state.pause_input_at = Some(state.input_sync_count + stage + 1);
        }
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        poll_button_until(&mut desktop, |desktop| match stage {
            0 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::GeometrySync
            ),
            1 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::DeviceSync
            ),
            _ => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::MotionSync
            ),
        });
        desktop.begin_capture(Duration::from_millis(120)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            assert!(matches!(desktop.poll_input().unwrap(), InputPoll::Pending));
            match desktop.poll_capture().unwrap() {
                CapturePoll::Complete(frame) => {
                    assert_eq!((frame.width, frame.height), (2, 2));
                    break;
                }
                CapturePoll::Pending => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                CapturePoll::Idle => panic!("capture unexpectedly idle"),
            }
        }
        assert_eq!(*refresh.request.lock().unwrap(), Vec::<u8>::new());
        assert_eq!(button_pairs(&peer), []);
        desktop.release_all().unwrap();
        peer.state.lock().unwrap().pause_input_at = None;
        desktop.pointer_adapter = PointerAdapter::Generic;
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Press,
                permit(),
            )
            .unwrap();
        desktop
            .button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Right,
                Transition::Release,
                permit(),
            )
            .unwrap();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), [(273, 1), (273, 0)]);
    }
}

#[test]
fn direct_input_does_not_replace_or_abort_a_pending_button() {
    let (peer, mut desktop) = Peer::start();
    let refresh = RefreshPeer::start();
    refresh.attach(&mut desktop);
    let position = Position::new(0.5, 0.5).unwrap();
    desktop
        .begin_button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    let sync = desktop.state.input_sync.as_ref().unwrap().id.clone();
    assert!(matches!(
        desktop.button(position, Button::Right, Transition::Press, permit()),
        Err(DesktopError::InputBacklog)
    ));
    assert!(matches!(
        desktop.key(KeyCode::new(30).unwrap(), Transition::Press, permit()),
        Err(DesktopError::InputBacklog)
    ));
    assert!(matches!(
        desktop.pointer_absolute(position, permit()),
        Err(DesktopError::InputBacklog)
    ));
    assert!(matches!(
        desktop.axis(position, 0.0, 1.0, permit()),
        Err(DesktopError::InputBacklog)
    ));
    assert_eq!(desktop.state.input_sync.as_ref().unwrap().id, sync);
    assert!(matches!(
        desktop.input.as_ref().unwrap().stage,
        ButtonStage::GeometrySync
    ));
    poll_button_until(&mut desktop, |desktop| {
        matches!(
            desktop.input.as_ref().unwrap().stage,
            ButtonStage::RefreshReply
        )
    });
    refresh.complete(b"twa_pointer_ready_v1");
    complete_button(&mut desktop);
    desktop
        .button(position, Button::Left, Transition::Release, permit())
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
    assert_eq!(peer.state.lock().unwrap().keys, []);
}
#[test]
fn cancellation_invalidates_the_native_button_at_each_pending_stage() {
    for stage in 0..4 {
        let (peer, mut desktop) = Peer::start();
        let refresh = RefreshPeer::start();
        refresh.attach(&mut desktop);
        let generation = Arc::new(AtomicU64::new(0));
        desktop.cancellation = Cancellation::new(generation.clone());
        desktop
            .begin_button(
                Position::new(0.5, 0.5).unwrap(),
                Button::Left,
                Transition::Press,
                permit(),
            )
            .unwrap();
        poll_button_until(&mut desktop, |desktop| match stage {
            0 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::GeometrySync
            ),
            1 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::DeviceSync
            ),
            2 => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::MotionSync
            ),
            _ => matches!(
                desktop.input.as_ref().unwrap().stage,
                ButtonStage::RefreshReply
            ),
        });
        generation.fetch_add(1, Ordering::AcqRel);
        refresh.complete(b"twa_pointer_ready_v1");
        assert!(matches!(desktop.poll_input(), Err(DesktopError::Cancelled)));
        assert!(desktop.input.is_none());
        assert!(desktop.state.input_sync.is_none());
        desktop.release_all().unwrap();
        desktop.cancellation = Cancellation::default();
        desktop.sync(Duration::from_millis(100)).unwrap();
        assert_eq!(button_pairs(&peer), []);
    }
}

#[test]
fn chords_duplicates_and_releases_do_not_add_a_motion_sync() {
    let (peer, mut desktop) = Peer::start();
    let position = Position::new(0.5, 0.5).unwrap();
    desktop
        .button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    desktop.pointer_adapter = PointerAdapter::Unsupported;
    for (button, transition) in [
        (Button::Right, Transition::Press),
        (Button::Left, Transition::Press),
        (Button::Right, Transition::Release),
        (Button::Left, Transition::Release),
    ] {
        let before = peer.state.lock().unwrap().input_sync_count;
        desktop
            .button(position, button, transition, permit())
            .unwrap();
        assert_eq!(peer.state.lock().unwrap().input_sync_count, before + 1);
    }
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(
        button_pairs(&peer),
        [(272, 1), (273, 1), (273, 0), (272, 0)]
    );
}
#[test]
fn full_owned_unix_backlog_rejects_connect_without_a_native_press() {
    use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let (peer, mut desktop) = Peer::start();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("full.sock");
    let address = SocketAddrUnix::new(&path).unwrap();
    let socket = || {
        net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        )
        .unwrap()
    };
    let listener = socket();
    net::bind(&listener, &address).unwrap();
    net::listen(&listener, 0).unwrap();
    let mut queued = Vec::new();
    let mut blocked = false;
    for _ in 0..16 {
        let client = socket();
        match net::connect(&client, &address) {
            Ok(()) => queued.push(client),
            Err(rustix::io::Errno::AGAIN) => {
                blocked = true;
                break;
            }
            other => panic!("unexpected owned backlog result {other:?}"),
        }
    }
    assert!(blocked);
    desktop.pointer_adapter = PointerAdapter::Hyprland(path);
    assert!(matches!(
        desktop.button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit()
        ),
        Err(DesktopError::PointerPreparation(
            PointerPreparationFault::Unavailable
        ))
    ));
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), []);
}

#[test]
fn transient_startup_probe_recovers_on_the_same_desktop() {
    let (peer, mut desktop) = Peer::start();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("refresh.sock");
    desktop.pointer_adapter = PointerAdapter::detect_endpoint(
        path.clone(),
        &Cancellation::default(),
        Instant::now() + Duration::from_millis(30),
    )
    .unwrap();
    assert_eq!(
        desktop.capabilities().pointer_preparation,
        PointerPreparationCapability::UnobservedHyprland
    );
    let position = Position::new(0.5, 0.5).unwrap();
    assert!(
        desktop
            .button(position, Button::Left, Transition::Press, permit())
            .is_err()
    );
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), []);
    let refresh = RefreshPeer::start_at(path);
    refresh.complete(b"twa_pointer_ready_v1");
    desktop
        .button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    desktop
        .button(position, Button::Left, Transition::Release, permit())
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
    assert_eq!(
        desktop.capabilities().pointer_preparation,
        PointerPreparationCapability::Hyprland
    );
    let events = peer.state.lock().unwrap().pointer_events.clone();
    assert_eq!(
        events
            .iter()
            .filter(|(opcode, _)| *opcode == 1)
            .map(|(_, args)| args[1..].to_vec())
            .collect::<Vec<_>>(),
        [vec![1, 1, 2, 2], vec![1, 1, 2, 2], vec![1, 1, 2, 2]]
    );
}

#[test]
fn startup_capability_reply_distinguishes_verified_unsupported_and_unobserved() {
    for (reply, expected) in [
        (
            b"twa_pointer_capable_v1".as_slice(),
            PointerPreparationCapability::Hyprland,
        ),
        (
            b"unsupported".as_slice(),
            PointerPreparationCapability::UnsupportedHyprland,
        ),
        (
            b"ok".as_slice(),
            PointerPreparationCapability::UnobservedHyprland,
        ),
        (
            b"twa_pointer_capable_v".as_slice(),
            PointerPreparationCapability::UnobservedHyprland,
        ),
        (
            b"unsupported\n".as_slice(),
            PointerPreparationCapability::UnobservedHyprland,
        ),
        (
            b"".as_slice(),
            PointerPreparationCapability::UnobservedHyprland,
        ),
    ] {
        let (peer, mut desktop) = Peer::start();
        let probe = RefreshPeer::start();
        probe.complete(reply);
        desktop.pointer_adapter = PointerAdapter::detect_endpoint(
            probe.path.clone(),
            &Cancellation::default(),
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(desktop.capabilities().pointer_preparation, expected);
        if expected == PointerPreparationCapability::UnsupportedHyprland {
            assert!(matches!(
                desktop.begin_button(
                    Position::new(0.5, 0.5).unwrap(),
                    Button::Left,
                    Transition::Press,
                    permit()
                ),
                Err(DesktopError::PointerPreparation(
                    PointerPreparationFault::Unsupported
                ))
            ));
            assert!(desktop.input.is_none());
            desktop.sync(Duration::from_millis(100)).unwrap();
            assert_eq!(button_pairs(&peer), []);
        }
    }
}
#[test]
fn startup_probe_deadline_and_cancellation_are_not_unsupported() {
    let probe = RefreshPeer::start();
    probe.hold_open.store(true, Ordering::Release);
    probe.complete(b"twa_pointer_capable_v1");
    let adapter = PointerAdapter::detect_endpoint(
        probe.path.clone(),
        &Cancellation::default(),
        Instant::now() + Duration::from_millis(30),
    )
    .unwrap();
    assert!(matches!(
        adapter,
        PointerAdapter::UnobservedHyprland(Some(_))
    ));
    let revision = Arc::new(AtomicU64::new(0));
    let cancellation = Cancellation::new(revision.clone());
    revision.store(1, Ordering::Release);
    assert!(matches!(
        PointerAdapter::detect_endpoint(
            probe.path.clone(),
            &cancellation,
            Instant::now() + Duration::from_millis(30)
        ),
        Err(DesktopError::Cancelled)
    ));
}

#[test]
fn unobserved_probe_and_failed_refresh_recover_without_replaying_a_press() {
    let (peer, mut desktop) = Peer::start();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("refresh.sock");
    let probe = RefreshPeer::start_at(path.clone());
    probe.complete(b"twa_pointer_capable_v");
    desktop.pointer_adapter = PointerAdapter::detect_endpoint(
        path.clone(),
        &Cancellation::default(),
        Instant::now() + Duration::from_millis(100),
    )
    .unwrap();
    drop(probe);
    std::fs::remove_file(&path).unwrap();
    let position = Position::new(0.5, 0.5).unwrap();
    let failed = RefreshPeer::start_at(path.clone());
    failed.complete(b"ok");
    assert!(matches!(
        desktop.button(position, Button::Left, Transition::Press, permit()),
        Err(DesktopError::PointerPreparation(
            PointerPreparationFault::InvalidReply
        ))
    ));
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), []);
    assert_eq!(
        desktop.capabilities().pointer_preparation,
        PointerPreparationCapability::UnobservedHyprland
    );
    drop(failed);
    std::fs::remove_file(&path).unwrap();
    let recovered = RefreshPeer::start_at(path);
    recovered.complete(b"twa_pointer_ready_v1");
    desktop
        .button(position, Button::Left, Transition::Press, permit())
        .unwrap();
    desktop
        .button(position, Button::Left, Transition::Release, permit())
        .unwrap();
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), [(272, 1), (272, 0)]);
    assert_eq!(
        desktop.capabilities().pointer_preparation,
        PointerPreparationCapability::Hyprland
    );
}
#[test]
fn unobserved_hyprland_without_validated_endpoint_does_not_use_generic_input() {
    let (peer, mut desktop) = Peer::start();
    desktop.pointer_adapter = PointerAdapter::UnobservedHyprland(None);
    assert_eq!(
        desktop.capabilities().pointer_preparation,
        PointerPreparationCapability::UnobservedHyprland
    );
    assert!(matches!(
        desktop.button(
            Position::new(0.5, 0.5).unwrap(),
            Button::Left,
            Transition::Press,
            permit()
        ),
        Err(DesktopError::PointerPreparation(
            PointerPreparationFault::Unavailable
        ))
    ));
    desktop.sync(Duration::from_millis(100)).unwrap();
    assert_eq!(button_pairs(&peer), []);
}

#[test]
fn startup_probe_propagates_cancellation_while_waiting_for_eof() {
    let probe = RefreshPeer::start();
    probe.hold_open.store(true, Ordering::Release);
    probe.complete(b"twa_pointer_capable_v1");
    let revision = Arc::new(AtomicU64::new(0));
    let cancellation = Cancellation::new(revision.clone());
    let received = probe.request.clone();
    let cancel = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(100);
        while received.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        revision.store(1, Ordering::Release);
    });
    assert!(matches!(
        PointerAdapter::detect_endpoint(
            probe.path.clone(),
            &cancellation,
            Instant::now() + Duration::from_millis(200)
        ),
        Err(DesktopError::Cancelled)
    ));
    cancel.join().unwrap();
}
