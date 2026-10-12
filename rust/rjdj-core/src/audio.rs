//! Output-driven AAudio audio (duplex when input opens), with Pd fixed at 22050 Hz.
//!
//! Startup follows Oboe's FullDuplexStream order: drain input for 20 callbacks
//! that actually receive input, discard 30 further callbacks, then render.
//! Both warmup phases output silence; short normal reads are zero-filled.
//! Output opens first at its native rate, then mono input at that rate with
//! twice the output buffer capacity requested. Exclusive opens retry Shared.
//! If both input opens fail, Pd uses zero input channels and output continues with silence-fed input.
//!
//! One stream-owner thread initializes Pd and owns all stream start/stop/close,
//! diagnostics and disconnect recovery (three attempts, 250 ms backoff).
//! Recovery replaces streams and the FIFO, never the patch or command queue.
//! Stop closes streams but retains the patch for same-directory Play/Pause.
//! A different directory replaces it; close_patch_if releases it before playback deletion.
//! Terminal failure closes callbacks and drains commands before clearing CURRENT
//! and publishing Failed, so listener cleanup reaches the retained patch directly.
//! A separate drain thread polls queued Pd hooks every 10 ms and is the only
//! thread invoking the replaceable event callback. Normal stop joins both.
//!
//! The data callback owns a preallocated scratch buffer, FIFO and SPSC consumer.
//! It only does nonblocking input reads, atomic bookkeeping and Pd ticks: no
//! Rust allocation, mutex, sleep, logging, I/O or JNI. Pd's RecordScene gensym
//! and writesf~ are the accepted native allocation/recording-I/O exceptions.
//! Record(false) waits at most 250 ms on the control side for a completed Pd
//! command drain while streams exist, or applies synchronously to a paused patch.
//! This acknowledges delivery, not writesf~'s child-thread file closure; recording
//! validation still checks only nonempty bytes, as before.

use std::cell::{Cell, UnsafeCell};
use std::ffi::{c_char, c_int, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioError, AudioFormat, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioStreamState, AudioUsage, Clockid,
};
use rtrb::Consumer;

use crate::error::{State, Status};
use crate::fifo::BurstTickFifo;
use crate::pd::engine::{Command, CommandQueue, Engine, Patch};
use crate::pd::{self, hooks};

const PD_RATE: i32 = 22050;
const POLL: Duration = Duration::from_millis(10);
const RESTART_BACKOFF: Duration = Duration::from_millis(250);
const RECORD_BARRIER: Duration = Duration::from_millis(250);

pub enum AudioEvent {
    Pd(hooks::PdEvent),
    State { state: State, status: Status },
}

type EventCallback = dyn Fn(AudioEvent) + Send + Sync + 'static;

static CALLBACK: Mutex<Option<Arc<EventCallback>>> = Mutex::new(None);
static STATE: AtomicI32 = AtomicI32::new(State::Stopped as i32);
static OPERATION: Mutex<()> = Mutex::new(());
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static CURRENT: Mutex<Option<(Arc<Shared>, Arc<CommandQueue>)>> = Mutex::new(None);
static PATCH: Mutex<Option<(PathBuf, Patch)>> = Mutex::new(None);

thread_local! {
    static IN_DRAIN: Cell<bool> = const { Cell::new(false) };
}

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_print(
        priority: c_int,
        tag: *const c_char,
        format: *const c_char,
        ...
    ) -> c_int;
}

fn log(priority: c_int, message: &str) {
    if let Ok(message) = CString::new(message) {
        // SAFETY: the format and both strings are NUL-terminated and live through the call.
        unsafe {
            __android_log_print(
                priority,
                c"RjdjCore".as_ptr(),
                c"%s".as_ptr(),
                message.as_ptr(),
            );
        }
    }
}

fn stream_error(operation: &str, error: AudioError) {
    log(5, &format!("{operation}: {error}"));
}

fn decode_state(value: i32) -> State {
    match value {
        0 => State::Stopped,
        1 => State::Starting,
        2 => State::Running,
        3 => State::Restarting,
        _ => State::Failed,
    }
}

fn started_state(has_input: bool) -> (State, Status) {
    (
        State::Running,
        if has_input {
            Status::Ok
        } else {
            Status::InputOpenFailed
        },
    )
}

struct Shared {
    state: AtomicI32,
    status: AtomicI32,
    transitions: Mutex<()>,
    stopping: AtomicBool,
    drain_exit: AtomicBool,
    pd_ready: AtomicBool,
    fault: AtomicI32,
    drain_epoch: AtomicU64,
    events: mpsc::Sender<AudioEvent>,
    // Native rate, burst, output xruns, input xruns, latency in microseconds.
    metrics: [AtomicI64; 5],
    short_input_frames: AtomicU64,
    input_underruns: AtomicU64,
    output_underruns: AtomicU64,
}

impl Shared {
    fn new(events: mpsc::Sender<AudioEvent>) -> Self {
        Self {
            state: AtomicI32::new(State::Stopped as i32),
            status: AtomicI32::new(Status::Ok as i32),
            transitions: Mutex::new(()),
            stopping: AtomicBool::new(false),
            drain_exit: AtomicBool::new(false),
            pd_ready: AtomicBool::new(false),
            fault: AtomicI32::new(0),
            drain_epoch: AtomicU64::new(0),
            events,
            metrics: [0, 0, 0, 0, -1].map(AtomicI64::new),
            short_input_frames: AtomicU64::new(0),
            input_underruns: AtomicU64::new(0),
            output_underruns: AtomicU64::new(0),
        }
    }

    fn transition_locked(&self, next: State, status: Status) {
        if self.state.load(Ordering::Relaxed) == next as i32
            && self.status.load(Ordering::Relaxed) == status as i32
        {
            return;
        }
        self.status.store(status as i32, Ordering::Relaxed);
        self.state.store(next as i32, Ordering::Release);
        STATE.store(next as i32, Ordering::Release);
        let _ = self.events.send(AudioEvent::State {
            state: next,
            status,
        });
    }

    fn transition(&self, next: State, status: Status) {
        let _guard = self
            .transitions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.transition_locked(next, status);
    }

    fn mark_running(&self, has_input: bool) -> bool {
        let _guard = self
            .transitions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) || self.fault.load(Ordering::Acquire) != 0 {
            return false;
        }
        let (state, status) = started_state(has_input);
        self.transition_locked(state, status);
        true
    }

    fn fault_rt(&self, status: Status) -> bool {
        !self.stopping.load(Ordering::Acquire)
            && self
                .fault
                .compare_exchange(0, status as i32, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    fn signal_fault(&self, status: Status) {
        if self.fault_rt(status) && status == Status::Disconnected {
            self.transition(State::Restarting, status);
        }
    }

    fn drain_commands(&self, consumer: &mut Consumer<Command>) {
        // Odd epochs are in progress; a barrier only acknowledges a later, complete drain.
        self.drain_epoch.fetch_add(1, Ordering::AcqRel);
        pd::drain_into_pd(consumer);
        self.drain_epoch.fetch_add(1, Ordering::Release);
    }

    fn stopped(&self) {
        self.transition(State::Stopped, Status::Ok);
        // Published after the event is queued so the drain thread cannot miss the final event.
        self.drain_exit.store(true, Ordering::Release);
    }
}

struct Session {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    drain: JoinHandle<()>,
}

pub fn set_event_callback(cb: Box<EventCallback>) {
    *CALLBACK.lock().unwrap_or_else(|error| error.into_inner()) = Some(Arc::from(cb));
}

fn dispatch(event: AudioEvent) {
    let callback = CALLBACK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(callback) = callback {
        callback(event);
    }
}

fn drain_events(
    shared: Arc<Shared>,
    events: mpsc::Receiver<AudioEvent>,
    pd_events: mpsc::Receiver<hooks::PdEvent>,
) {
    IN_DRAIN.set(true);
    loop {
        let finishing = shared.drain_exit.load(Ordering::Acquire);
        for event in events.try_iter() {
            dispatch(event);
        }
        if shared.pd_ready.load(Ordering::Acquire) {
            hooks::drain_queued();
        }
        for event in pd_events.try_iter() {
            dispatch(AudioEvent::Pd(event));
        }
        if finishing {
            break;
        }
        thread::sleep(POLL);
    }
    IN_DRAIN.set(false);
}

pub fn state() -> State {
    decode_state(STATE.load(Ordering::Acquire))
}

pub fn is_running() -> bool {
    state() == State::Running
}

/// Whether the running session captures microphone input.
pub fn has_input() -> bool {
    let current = CURRENT.lock().unwrap_or_else(|error| error.into_inner());
    current.as_ref().is_some_and(|(shared, _)| {
        decode_state(shared.state.load(Ordering::Acquire)) == State::Running
            && shared.status.load(Ordering::Relaxed) == Status::Ok as i32
            && !shared.stopping.load(Ordering::Acquire)
            && shared.fault.load(Ordering::Acquire) == 0
    })
}

pub fn start(playback_dir: &Path, mic_volume: f32, search_paths: &[PathBuf]) -> Status {
    if IN_DRAIN.get() {
        return if is_running() {
            Status::AlreadyRunning
        } else {
            Status::StartFailed
        };
    }
    let _operation = OPERATION.lock().unwrap_or_else(|error| error.into_inner());
    if matches!(
        state(),
        State::Starting | State::Running | State::Restarting
    ) {
        return Status::AlreadyRunning;
    }
    if !mic_volume.is_finite()
        || mic_volume < 0.0
        || playback_dir.as_os_str().is_empty()
        || playback_dir.as_os_str().as_encoded_bytes().contains(&0)
        || search_paths
            .iter()
            .any(|path| path.as_os_str().as_encoded_bytes().contains(&0))
    {
        return Status::InvalidArgument;
    }
    shutdown_session();
    log(4, "Starting audio at Pd 22050 Hz");
    let (event_tx, event_rx) = mpsc::channel();
    let (pd_tx, pd_rx) = mpsc::channel();
    let shared = Arc::new(Shared::new(event_tx));
    shared.transition(State::Starting, Status::Ok);
    hooks::set_event_sink(pd_tx);
    let drain_shared = Arc::clone(&shared);
    let drain = match thread::Builder::new()
        .name("rjdj-pd-events".into())
        .spawn(move || drain_events(drain_shared, event_rx, pd_rx))
    {
        Ok(thread) => thread,
        Err(error) => {
            log(6, &format!("Unable to start Pd event thread: {error}"));
            shared.transition(State::Failed, Status::Internal);
            return Status::Internal;
        }
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let worker_shared = Arc::clone(&shared);
    let playback_dir = playback_dir.to_owned();
    let search_paths = search_paths.to_owned();
    let worker = match thread::Builder::new()
        .name("rjdj-audio-owner".into())
        .spawn(move || {
            run_audio(
                worker_shared,
                playback_dir,
                mic_volume,
                search_paths,
                ready_tx,
            )
        }) {
        Ok(thread) => Some(thread),
        Err(error) => {
            log(6, &format!("Unable to start audio owner: {error}"));
            shared.transition(State::Failed, Status::Internal);
            None
        }
    };
    let spawned = worker.is_some();
    *SESSION.lock().unwrap_or_else(|error| error.into_inner()) = Some(Session {
        shared,
        worker,
        drain,
    });
    if !spawned {
        return Status::Internal;
    }
    ready_rx.recv().unwrap_or_else(|error| {
        log(6, &format!("Audio owner failed during startup: {error}"));
        if let Some(session) = SESSION
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            if let Some(worker) = session.worker.take() {
                let _ = worker.join();
            }
            fail_stopped(&session.shared, Status::Internal, None);
        }
        Status::Internal
    })
}

/// Closes streams synchronously and retains the patch; a listener cannot join its own drain thread.
pub fn stop() {
    if IN_DRAIN.get() {
        if let Some(session) = SESSION
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            session.shared.stopping.store(true, Ordering::Release);
        }
    }
    let Some(_operation) = lifecycle_lock() else {
        return;
    };
    shutdown_session();
}

fn lifecycle_lock() -> Option<std::sync::MutexGuard<'static, ()>> {
    loop {
        match OPERATION.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if IN_DRAIN.get() => {
                // An external shutdown may hold OPERATION while joining this listener.
                if state() == State::Stopped
                    && SESSION
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_none()
                {
                    return None;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                return Some(OPERATION.lock().unwrap_or_else(|error| error.into_inner()));
            }
        }
    }
}

/// Release only this playback directory's patch; unrelated active playback is untouched.
pub fn close_patch_if(dir: &Path) {
    let Some(_operation) = lifecycle_lock() else {
        close_retained_patch(dir);
        return;
    };
    let matches = PATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|(open_dir, _)| open_dir == dir);
    if matches {
        shutdown_session();
        close_retained_patch(dir);
    }
}

fn shutdown_session() {
    let session = SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let Some(session) = session else {
        *CURRENT.lock().unwrap_or_else(|error| error.into_inner()) = None;
        STATE.store(State::Stopped as i32, Ordering::Release);
        return;
    };
    log(4, "Stopping audio");
    session.shared.stopping.store(true, Ordering::Release);
    if let Some(worker) = session.worker {
        if worker.join().is_err() {
            log(6, "Audio owner panicked during shutdown");
            fail_stopped(&session.shared, Status::Internal, None);
            session.shared.drain_exit.store(true, Ordering::Release);
        } else if !session.shared.drain_exit.load(Ordering::Acquire) {
            // Initial startup failures have already closed their patch and streams.
            session.shared.stopped();
        }
    } else {
        session.shared.stopped();
    }
    *CURRENT.lock().unwrap_or_else(|error| error.into_inner()) = None;
    if session.drain.thread().id() == thread::current().id() {
        // Keep the handle so the next control operation joins this thread before a new drain starts.
        *SESSION.lock().unwrap_or_else(|error| error.into_inner()) = Some(Session {
            shared: session.shared,
            worker: None,
            drain: session.drain,
        });
    } else if session.drain.join().is_err() {
        log(6, "Pd event thread panicked during shutdown");
    }
}

fn valid_command(command: &Command) -> bool {
    match command {
        Command::MicVolume(gain) => gain.is_finite() && *gain >= 0.0,
        Command::Touch { id, x, y, .. } => *id > 0 && x.is_finite() && y.is_finite(),
        Command::Accelerate(x, y, z) => x.is_finite() && y.is_finite() && z.is_finite(),
        Command::RecordScene(path) => !path.is_empty() && !path.as_bytes().contains(&0),
        Command::TransportPlay(_) | Command::Record(_) => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendPath {
    Queued,
    Direct,
    NoPatch,
}

fn send_path(has_session: bool, has_patch: bool) -> SendPath {
    if has_session {
        SendPath::Queued
    } else if has_patch {
        SendPath::Direct
    } else {
        SendPath::NoPatch
    }
}

/// Queues with a stream session; applies synchronously to a retained, paused patch.
/// With no session, only an absent patch rejects a valid command (Record(false) needs no barrier).
pub fn send(cmd: Command) -> bool {
    if !valid_command(&cmd) {
        return false;
    }
    let barrier = matches!(cmd, Command::Record(false));
    let (shared, epoch) = {
        let current = CURRENT.lock().unwrap_or_else(|error| error.into_inner());
        // CURRENT is cleared only after callbacks close; holding it also excludes a new start.
        let _engine = current.is_none().then(|| {
            Engine::global()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        });
        let has_patch = current.is_some()
            || PATCH
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some();
        match send_path(current.is_some(), has_patch) {
            SendPath::Direct => {
                cmd.apply();
                return true;
            }
            SendPath::NoPatch => return false,
            SendPath::Queued => {}
        }
        let Some((shared, queue)) = current.as_ref() else {
            return false;
        };
        if shared.stopping.load(Ordering::Acquire)
            || !matches!(
                decode_state(shared.state.load(Ordering::Acquire)),
                State::Starting | State::Running | State::Restarting
            )
            || !queue.push(cmd)
        {
            return false;
        }
        (
            Arc::clone(shared),
            shared.drain_epoch.load(Ordering::Acquire),
        )
    };
    if barrier {
        let target = barrier_epoch(epoch);
        let deadline = Instant::now() + RECORD_BARRIER;
        while shared.drain_epoch.load(Ordering::Acquire) < target {
            if Instant::now() >= deadline || shared.drain_exit.load(Ordering::Acquire) {
                log(5, "Record stop command drain timed out");
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    true
}

fn barrier_epoch(epoch: u64) -> u64 {
    epoch.saturating_add(if epoch & 1 == 0 { 2 } else { 3 })
}

/// Stream-owner snapshots avoid native getter/close races; refreshed every 10 ms.
pub fn info() -> [i64; 7] {
    let current = CURRENT.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((shared, _)) = current.as_ref() {
        let values = shared
            .metrics
            .each_ref()
            .map(|value| value.load(Ordering::Acquire));
        [
            values[0],
            i64::from(PD_RATE),
            values[1],
            values[2],
            values[3],
            values[4],
            state() as i64,
        ]
    } else {
        [0, i64::from(PD_RATE), 0, 0, 0, -1, state() as i64]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Drain,
    Discard,
    Process,
}

struct Warmup {
    drain: u32,
    discard: u32,
}

impl Warmup {
    fn new(has_input: bool) -> Self {
        Self {
            drain: if has_input { 20 } else { 0 },
            discard: if has_input { 30 } else { 0 },
        }
    }

    fn phase(&self) -> Phase {
        if self.drain > 0 {
            Phase::Drain
        } else if self.discard > 0 {
            Phase::Discard
        } else {
            Phase::Process
        }
    }

    fn observed(&mut self, frames: usize) {
        if self.drain > 0 {
            if frames > 0 {
                self.drain -= 1;
            }
        } else if self.discard > 0 {
            self.discard -= 1;
        }
    }
}

struct InputStream(AudioStream);

// SAFETY: only immutable AAudio input reads/getters and owner start/stop are shared.
// Output close completes before either input reference is dropped; input has no data callback.
unsafe impl Send for InputStream {}
unsafe impl Sync for InputStream {}

struct Render {
    input: Option<Arc<InputStream>>,
    consumer: Consumer<Command>,
    fifo: BurstTickFifo,
    scratch: Box<[f32]>,
    warmup: Warmup,
    burst: usize,
    input_capacity: usize,
}

struct RenderSlot(UnsafeCell<Option<Render>>);

// SAFETY: initialized before request_start; the sole output callback then has exclusive
// mutable access. The owner can reclaim it only after synchronous output close returns.
unsafe impl Send for RenderSlot {}
unsafe impl Sync for RenderSlot {}

fn read_count(result: ndk::audio::Result<u32>, requested: usize) -> ndk::audio::Result<usize> {
    match result {
        Ok(frames) if frames as usize <= requested => Ok(frames as usize),
        Ok(_) => Err(AudioError::Internal),
        Err(error) => {
            // ndk 0.9 misclassifies positive AAudioStream_read results as __Unknown errors.
            let value: i32 = error.into();
            if value > 0 && value as usize <= requested {
                Ok(value as usize)
            } else if matches!(error, AudioError::WouldBlock | AudioError::Timeout) {
                Ok(0)
            } else {
                Err(error)
            }
        }
    }
}

fn read_input(input: &InputStream, buffer: &mut [f32]) -> ndk::audio::Result<usize> {
    // SAFETY: input is mono PCM_Float, the buffer holds exactly the requested frames,
    // and the owner keeps input open until the output callback has been closed.
    read_count(
        unsafe {
            input
                .0
                .read(buffer.as_mut_ptr().cast(), buffer.len() as i32, 0)
        },
        buffer.len(),
    )
}

fn fault_status(error: AudioError) -> Status {
    if error == AudioError::Disconnected {
        Status::Disconnected
    } else {
        Status::Internal
    }
}

impl Render {
    fn render(&mut self, output: &mut [f32], shared: &Shared) -> AudioCallbackResult {
        output.fill(0.0);
        let has_input = self.input.is_some();
        let phase = self.warmup.phase();
        if let Some(input) = self.input.as_ref().filter(|_| phase != Phase::Process) {
            shared.drain_commands(&mut self.consumer);
            let mut total = 0;
            let limit = if phase == Phase::Drain {
                self.input_capacity
            } else {
                output.len() / 2
            };
            while total < limit {
                let count = (limit - total)
                    .min(self.scratch.len())
                    .min(output.len() / 2);
                match read_input(input, &mut self.scratch[..count]) {
                    Ok(0) => break,
                    Ok(frames) => total += frames,
                    Err(error) => {
                        if phase != Phase::Drain || error == AudioError::Disconnected {
                            shared.fault_rt(fault_status(error));
                            return AudioCallbackResult::Stop;
                        }
                        break;
                    }
                }
            }
            self.warmup.observed(total);
            return AudioCallbackResult::Continue;
        }
        for output in output.chunks_mut(self.burst * 2) {
            let frames = output.len() / 2;
            let input = &mut self.scratch[..frames];
            let read = if let Some(stream) = self.input.as_ref() {
                match read_input(stream, input) {
                    Ok(read) => read,
                    Err(error) => {
                        shared.fault_rt(fault_status(error));
                        return AudioCallbackResult::Stop;
                    }
                }
            } else {
                0
            };
            input[read..].fill(0.0);
            if has_input {
                shared
                    .short_input_frames
                    .fetch_add((frames - read) as u64, Ordering::Relaxed);
            }
            self.fifo.push_input(input);
            self.fifo.pull_output(output, |in64, out128| {
                if shared.fault.load(Ordering::Acquire) == 0 {
                    shared.drain_commands(&mut self.consumer);
                    if pd::process(1, if has_input { in64 } else { &[] }, out128) != 0 {
                        out128.fill(0.0);
                        shared.fault_rt(Status::Internal);
                    }
                }
            });
            if shared.fault.load(Ordering::Acquire) != 0 {
                output.fill(0.0);
                return AudioCallbackResult::Stop;
            }
        }
        shared
            .input_underruns
            .store(self.fifo.input_underruns, Ordering::Relaxed);
        shared
            .output_underruns
            .store(self.fifo.output_underruns, Ordering::Relaxed);
        AudioCallbackResult::Continue
    }
}

struct Streams {
    output: AudioStream,
    input: Option<Arc<InputStream>>,
    render: Arc<RenderSlot>,
    active: Arc<AtomicBool>,
}

impl Streams {
    fn close(self) -> Consumer<Command> {
        self.active.store(false, Ordering::Release);
        if let Err(error) = self.output.request_stop() {
            stream_error("Stop output", error);
        }
        if let Some(input) = self.input.as_ref() {
            if let Err(error) = input.0.request_stop() {
                stream_error("Stop input", error);
            }
        }
        // AAudio close synchronously quiesces callbacks; always close output before input.
        drop(self.output);
        // SAFETY: no callback can access this slot after output close.
        let render = unsafe { &mut *self.render.0.get() }.take();
        let Some(render) = render else {
            panic!("Closed output has no initialized render context")
        };
        let consumer = render.consumer;
        drop(render.input);
        drop(self.input);
        consumer
    }

    fn update_info(&self, shared: &Shared) {
        let latency = if self.output.state() == AudioStreamState::Started {
            self.output
                .timestamp(Clockid::Monotonic)
                .ok()
                .and_then(|stamp| {
                    let mut now = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                    // SAFETY: now is writable and CLOCK_MONOTONIC matches AAudio's timestamp clock.
                    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
                        return None;
                    }
                    Some(latency_micros(
                        stamp.frame_position,
                        stamp.time_nanoseconds,
                        self.output.frames_written(),
                        now.tv_sec
                            .saturating_mul(1_000_000_000)
                            .saturating_add(now.tv_nsec),
                        self.output.sample_rate(),
                    ))
                })
                .unwrap_or(-1)
        } else {
            -1
        };
        for (slot, value) in shared.metrics.iter().zip([
            i64::from(self.output.sample_rate()),
            i64::from(self.output.frames_per_burst()),
            i64::from(self.output.x_run_count()).max(-1),
            self.input
                .as_ref()
                .map_or(-1, |input| i64::from(input.0.x_run_count()).max(-1)),
            latency,
        ]) {
            slot.store(value, Ordering::Release);
        }
    }
}

fn latency_micros(frame: i64, timestamp: i64, written: i64, now: i64, rate: i32) -> i64 {
    if rate <= 0 || frame < 0 || written < frame || timestamp < 0 || now < 0 {
        return -1;
    }
    let presentation =
        i128::from(timestamp) + i128::from(written - frame) * 1_000_000_000 / i128::from(rate);
    let latency = presentation - i128::from(now);
    if latency < 0 {
        -1
    } else {
        i64::try_from(latency / 1000).unwrap_or(-1)
    }
}

fn error_callback(
    shared: Arc<Shared>,
    active: Arc<AtomicBool>,
) -> ndk::audio::AudioStreamErrorCallback {
    Box::new(move |_, error| {
        if active.load(Ordering::Acquire) && !shared.stopping.load(Ordering::Acquire) {
            stream_error("AAudio stream error", error);
            // Never stop, close, query or reopen the erroring stream here.
            shared.signal_fault(fault_status(error));
        }
    })
}

fn open_output(
    shared: &Arc<Shared>,
    slot: &Arc<RenderSlot>,
    active: &Arc<AtomicBool>,
    sharing: AudioSharingMode,
) -> ndk::audio::Result<AudioStream> {
    let callback_shared = Arc::clone(shared);
    let callback_slot = Arc::clone(slot);
    let callback_active = Arc::clone(active);
    AudioStreamBuilder::new()?
        .direction(AudioDirection::Output)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(sharing)
        .format(AudioFormat::PCM_Float)
        .channel_count(2)
        .usage(AudioUsage::Media)
        // Deliberately leave sample rate and frames_per_data_callback unspecified.
        .data_callback(Box::new(move |_, data, frames| {
            if frames <= 0 || data.is_null() {
                callback_shared.fault_rt(Status::Internal);
                return AudioCallbackResult::Stop;
            }
            // SAFETY: validated stereo PCM_Float stream supplies frames * 2 live samples.
            let output =
                unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), frames as usize * 2) };
            if !callback_active.load(Ordering::Acquire)
                || callback_shared.stopping.load(Ordering::Acquire)
                || callback_shared.fault.load(Ordering::Acquire) != 0
            {
                output.fill(0.0);
                return AudioCallbackResult::Stop;
            }
            // SAFETY: owner initialized before start and does not access until output close.
            if let Some(render) = unsafe { &mut *callback_slot.0.get() }.as_mut() {
                render.render(output, &callback_shared)
            } else {
                output.fill(0.0);
                callback_shared.fault_rt(Status::Internal);
                AudioCallbackResult::Stop
            }
        }))
        .error_callback(error_callback(Arc::clone(shared), Arc::clone(active)))
        .open_stream()
}

fn open_input(
    shared: &Arc<Shared>,
    active: &Arc<AtomicBool>,
    rate: i32,
    capacity: i32,
    sharing: AudioSharingMode,
) -> ndk::audio::Result<AudioStream> {
    AudioStreamBuilder::new()?
        .direction(AudioDirection::Input)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(sharing)
        .format(AudioFormat::PCM_Float)
        .channel_count(1)
        .sample_rate(rate)
        .buffer_capacity_in_frames(capacity)
        .error_callback(error_callback(Arc::clone(shared), Arc::clone(active)))
        .open_stream()
}

fn wait_started(stream: &AudioStream, shared: &Shared) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if shared.stopping.load(Ordering::Acquire) || shared.fault.load(Ordering::Acquire) != 0 {
            return false;
        }
        let state = stream.state();
        if state == AudioStreamState::Started {
            return true;
        }
        if state != AudioStreamState::Starting || Instant::now() >= deadline {
            return false;
        }
        if let Err(error) = stream.wait_for_state_change(state, 100_000_000) {
            if error != AudioError::Timeout {
                stream_error("Wait for stream start", error);
                return false;
            }
        }
    }
}

fn open_streams(
    consumer: Consumer<Command>,
    shared: &Arc<Shared>,
) -> Result<Streams, (Status, Consumer<Command>)> {
    let slot = Arc::new(RenderSlot(UnsafeCell::new(None)));
    let active = Arc::new(AtomicBool::new(false));
    let output =
        match open_output(shared, &slot, &active, AudioSharingMode::Exclusive).or_else(|error| {
            stream_error("Exclusive output open failed; retrying Shared", error);
            open_output(shared, &slot, &active, AudioSharingMode::Shared)
        }) {
            Ok(stream) => stream,
            Err(error) => {
                stream_error("Output open failed", error);
                return Err((Status::OutputOpenFailed, consumer));
            }
        };
    let rate = output.sample_rate();
    let burst = output.frames_per_burst();
    let capacity = output.buffer_capacity_in_frames();
    let input_capacity = capacity.checked_mul(2).filter(|value| *value > 0);
    if !(8000..=192000).contains(&rate)
        || burst <= 0
        || input_capacity.is_none()
        || output.channel_count() != 2
        || output.format() != AudioFormat::PCM_Float
    {
        log(6, "Unsupported negotiated output configuration");
        drop(output);
        return Err((Status::OutputOpenFailed, consumer));
    }
    let Some(input_capacity) = input_capacity else {
        unreachable!()
    };
    let input = match open_input(
        shared,
        &active,
        rate,
        input_capacity,
        AudioSharingMode::Exclusive,
    )
    .or_else(|error| {
        stream_error("Exclusive input open failed; retrying Shared", error);
        open_input(
            shared,
            &active,
            rate,
            input_capacity,
            AudioSharingMode::Shared,
        )
    }) {
        Ok(stream) => Some(stream),
        Err(error) => {
            stream_error("Input open failed; continuing simplex output", error);
            None
        }
    };
    if let Some(stream) = input.as_ref() {
        if stream.sample_rate() != rate
            || stream.channel_count() != 1
            || stream.format() != AudioFormat::PCM_Float
            || stream.buffer_capacity_in_frames() <= 0
        {
            log(
                6,
                "Unsupported negotiated input configuration; closing both streams",
            );
            drop(output);
            drop(input);
            return Err((Status::InputOpenFailed, consumer));
        }
        if stream.buffer_capacity_in_frames() < input_capacity {
            log(
                5,
                "Input buffer capacity is below requested double-output capacity",
            );
        }
        log(4, &format!("Opened duplex: stream {rate} Hz, Pd {PD_RATE} Hz, burst {burst}, output capacity {capacity}, input capacity {}",
            stream.buffer_capacity_in_frames()));
    } else {
        log(4, &format!("Opened simplex: stream {rate} Hz, Pd {PD_RATE} Hz, burst {burst}, output capacity {capacity}, no input"));
    }
    // Reconfigure each stopped session, including a duplex recovery after simplex.
    let initialized = Engine::global()
        .lock()
        .map_err(|_| Status::Internal)
        .and_then(|mut engine| {
            engine
                .init_audio(i32::from(input.is_some()), 2, PD_RATE)
                .map_err(|error| {
                    log(6, &error.message);
                    Status::StartFailed
                })
        });
    if let Err(status) = initialized {
        drop(output);
        drop(input);
        return Err((status, consumer));
    }
    let input = input.map(|stream| Arc::new(InputStream(stream)));
    let render = Render {
        input: input.as_ref().map(Arc::clone),
        consumer,
        fifo: BurstTickFifo::new(rate as u32, PD_RATE as u32, burst as usize, 1, 2),
        scratch: vec![0.0; capacity.max(burst) as usize].into_boxed_slice(),
        warmup: Warmup::new(input.is_some()),
        burst: burst as usize,
        input_capacity: input
            .as_ref()
            .map_or(0, |input| input.0.buffer_capacity_in_frames() as usize),
    };
    // SAFETY: streams are not started, so the output callback cannot yet access the slot.
    unsafe {
        *slot.0.get() = Some(render);
    }
    let streams = Streams {
        output,
        input,
        render: slot,
        active,
    };
    streams.active.store(true, Ordering::Release);
    let started = streams
        .input
        .as_ref()
        .map_or(true, |input| match input.0.request_start() {
            Ok(()) => wait_started(&input.0, shared),
            Err(error) => {
                stream_error("Start input failed", error);
                false
            }
        })
        && match streams.output.request_start() {
            Ok(()) => wait_started(&streams.output, shared),
            Err(error) => {
                stream_error("Start output failed", error);
                false
            }
        };
    if !started {
        log(6, "Audio start failed; stopping and closing streams");
        return Err((Status::StartFailed, streams.close()));
    }
    streams.update_info(shared);
    Ok(streams)
}

fn ensure_patch(shared: &Shared, dir: &Path, paths: &[PathBuf]) -> Result<bool, Status> {
    let mut engine = Engine::global().lock().map_err(|_| Status::Internal)?;
    engine.init().map_err(|error| {
        log(6, &error.message);
        Status::NotInitialized
    })?;
    shared.pd_ready.store(true, Ordering::Release);
    let mut retained = PATCH.lock().unwrap_or_else(|error| error.into_inner());
    let reused = retained
        .as_ref()
        .is_some_and(|(open_dir, _)| open_dir == dir);
    if !reused {
        if let Some((_, patch)) = retained.take() {
            engine.close_patch(patch);
        }
    }
    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    engine.set_search_paths(&paths).map_err(|error| {
        log(6, &error.message);
        Status::InvalidArgument
    })?;
    if reused {
        log(4, "Reusing retained scene patch");
        return Ok(false);
    }
    engine.init_audio(1, 2, PD_RATE).map_err(|error| {
        log(6, &error.message);
        Status::StartFailed
    })?;
    let patch = engine.open_patch(dir, "_main.pd").map_err(|error| {
        log(6, &error.message);
        Status::PatchOpenFailed
    })?;
    *retained = Some((dir.to_owned(), patch));
    Ok(true)
}

fn flush_stopped(shared: &Shared, consumer: &mut Consumer<Command>) {
    let _engine = Engine::global()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    shared.drain_commands(consumer);
}

fn finish_terminal<T>(current: &Mutex<Option<T>>, quiesce: impl FnOnce(), publish: impl FnOnce()) {
    let mut current = current.lock().unwrap_or_else(|error| error.into_inner());
    // Exclude queue producers until the final drain switches sends to the direct path.
    quiesce();
    *current = None;
    drop(current);
    publish();
}

/// Called only after streams have closed and their callbacks have quiesced.
fn fail_stopped(shared: &Shared, status: Status, consumer: Option<&mut Consumer<Command>>) {
    finish_terminal(
        &CURRENT,
        || {
            if let Some(consumer) = consumer {
                flush_stopped(shared, consumer);
            }
        },
        || shared.transition(State::Failed, status),
    );
}

fn close_retained_patch(dir: &Path) {
    let engine = Engine::global()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut retained = PATCH.lock().unwrap_or_else(|error| error.into_inner());
    if retained
        .as_ref()
        .is_some_and(|(open_dir, _)| open_dir == dir)
    {
        if let Some((_, patch)) = retained.take() {
            engine.close_patch(patch);
            log(4, "Closed retained scene patch");
        }
    }
}

fn run_audio(
    shared: Arc<Shared>,
    dir: PathBuf,
    mic_volume: f32,
    paths: Vec<PathBuf>,
    ready: mpsc::Sender<Status>,
) {
    let opened = match ensure_patch(&shared, &dir, &paths) {
        Ok(opened) => opened,
        Err(status) => {
            shared.transition(State::Failed, status);
            let _ = ready.send(status);
            return;
        }
    };
    let (queue, mut consumer) = CommandQueue::new();
    if !queue.push(Command::MicVolume(mic_volume)) {
        flush_stopped(&shared, &mut consumer);
        if opened {
            close_retained_patch(&dir);
        }
        shared.transition(State::Failed, Status::Internal);
        let _ = ready.send(Status::Internal);
        return;
    }
    let queue = Arc::new(queue);
    *CURRENT.lock().unwrap_or_else(|error| error.into_inner()) =
        Some((Arc::clone(&shared), Arc::clone(&queue)));
    let initial = open_streams(consumer, &shared);
    let mut streams = match initial {
        Ok(streams) if shared.mark_running(streams.input.is_some()) => Some(streams),
        result => {
            let (status, mut consumer) = match result {
                Ok(streams) => (Status::StartFailed, streams.close()),
                Err(failure) => failure,
            };
            finish_terminal(
                &CURRENT,
                || {
                    flush_stopped(&shared, &mut consumer);
                    if opened {
                        close_retained_patch(&dir);
                    }
                },
                || shared.transition(State::Failed, status),
            );
            let _ = ready.send(status);
            return;
        }
    };
    log(4, "Audio running");
    let _ = ready.send(Status::Ok);
    let mut pending = None;
    while !shared.stopping.load(Ordering::Acquire) {
        if let Some(current) = streams.as_ref() {
            if current
                .input
                .as_ref()
                .is_some_and(|input| input.0.state() == AudioStreamState::Disconnected)
                || current.output.state() == AudioStreamState::Disconnected
            {
                shared.signal_fault(Status::Disconnected);
            }
        }
        let fault = shared.fault.swap(0, Ordering::AcqRel);
        if fault != 0 {
            let status = if fault == Status::Disconnected as i32 {
                Status::Disconnected
            } else {
                Status::Internal
            };
            if status == Status::Disconnected {
                shared.transition(State::Restarting, status);
            }
            shared.metrics[4].store(-1, Ordering::Release);
            log(
                5,
                if status == Status::Disconnected {
                    "Disconnected; closing audio streams"
                } else {
                    "Audio failure; closing audio streams"
                },
            );
            if let Some(current) = streams.take() {
                pending = Some(current.close());
            }
            if status == Status::Disconnected {
                for attempt in 1..=3 {
                    if shared.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    if attempt > 1 {
                        thread::sleep(RESTART_BACKOFF);
                    }
                    if shared.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    shared.fault.store(0, Ordering::Release);
                    let Some(consumer) = pending.take() else {
                        break;
                    };
                    log(
                        4,
                        &format!("Restarting audio streams, attempt {attempt}/3; patch retained"),
                    );
                    match open_streams(consumer, &shared) {
                        Ok(current) if shared.mark_running(current.input.is_some()) => {
                            streams = Some(current);
                            log(4, "Audio restart succeeded; patch retained");
                            break;
                        }
                        result => {
                            let (status, consumer) = match result {
                                Ok(current) => (Status::Disconnected, current.close()),
                                Err(failure) => failure,
                            };
                            pending = Some(consumer);
                            log(
                                5,
                                &format!(
                                    "Audio restart attempt {attempt} failed: status {}",
                                    status as i32
                                ),
                            );
                        }
                    }
                }
            }
            if streams.is_none() && !shared.stopping.load(Ordering::Acquire) {
                shared.fault.store(0, Ordering::Release);
                fail_stopped(&shared, status, pending.as_mut());
                log(
                    6,
                    if status == Status::Disconnected {
                        "Audio restart exhausted; audio failed"
                    } else {
                        "Audio failed; awaiting stop"
                    },
                );
            }
        }
        if let Some(current) = streams.as_ref() {
            current.update_info(&shared);
        }
        thread::sleep(POLL);
    }
    if let Some(current) = streams {
        pending = Some(current.close());
    }
    if let Some(mut consumer) = pending {
        flush_stopped(&shared, &mut consumer);
    }
    *CURRENT.lock().unwrap_or_else(|error| error.into_inner()) = None;
    log(4, &format!("Audio stopped: short input frames {}, FIFO underruns in/out {}/{}, dropped commands {}",
        shared.short_input_frames.load(Ordering::Relaxed),
        shared.input_underruns.load(Ordering::Relaxed),
        shared.output_underruns.load(Ordering::Relaxed), queue.dropped()));
    shared.stopped();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_output_with_failed_input_is_running() {
        assert_eq!(
            started_state(false),
            (State::Running, Status::InputOpenFailed)
        );
        assert_eq!(started_state(true), (State::Running, Status::Ok));
    }

    #[test]
    fn simplex_running_event_changes_when_recovery_restores_input() {
        let (tx, events) = mpsc::channel();
        let shared = Shared::new(tx);
        assert!(shared.mark_running(false));
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::State {
                state: State::Running,
                status: Status::InputOpenFailed
            })
        ));
        shared.transition(State::Restarting, Status::Disconnected);
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::State {
                state: State::Restarting,
                status: Status::Disconnected
            })
        ));
        assert!(shared.mark_running(true));
        assert!(matches!(
            events.try_recv(),
            Ok(AudioEvent::State {
                state: State::Running,
                status: Status::Ok
            })
        ));
    }

    #[test]
    fn retained_patch_without_a_session_dispatches_directly() {
        assert_eq!(send_path(false, true), SendPath::Direct);
        assert_eq!(send_path(false, false), SendPath::NoPatch);
        assert_eq!(send_path(true, true), SendPath::Queued);
        assert_eq!(send_path(true, false), SendPath::Queued);
    }

    #[test]
    fn terminal_failure_quiesces_then_clears_before_publishing() {
        let current = Mutex::new(Some(()));
        let quiesced = Cell::new(false);
        let published = Cell::new(false);
        finish_terminal(
            &current,
            || quiesced.set(true),
            || {
                assert!(quiesced.get());
                let Ok(current) = current.lock() else {
                    panic!("terminal session lock poisoned");
                };
                assert!(current.is_none());
                assert_eq!(send_path(current.is_some(), true), SendPath::Direct);
                published.set(true);
            },
        );
        assert!(published.get());
    }

    #[test]
    fn fault_signal_never_publishes_failed_before_owner_cleanup() {
        for status in [Status::Internal, Status::Disconnected] {
            let (tx, events) = mpsc::channel();
            let shared = Shared::new(tx);
            shared.signal_fault(status);
            assert_eq!(shared.fault.load(Ordering::Acquire), status as i32);
            if status == Status::Disconnected {
                assert!(matches!(
                    events.try_recv(),
                    Ok(AudioEvent::State {
                        state: State::Restarting,
                        status: Status::Disconnected
                    })
                ));
            }
            assert!(events.try_recv().is_err());
        }
    }

    #[test]
    fn drains_twenty_input_bearing_callbacks_before_discarding_thirty() {
        let mut warmup = Warmup::new(true);
        warmup.observed(0);
        assert!(warmup.phase() == Phase::Drain);
        for _ in 0..20 {
            assert!(warmup.phase() == Phase::Drain);
            warmup.observed(192);
        }
        for _ in 0..30 {
            assert!(warmup.phase() == Phase::Discard);
            warmup.observed(0);
        }
        assert!(warmup.phase() == Phase::Process);
        assert!(Warmup::new(true).phase() == Phase::Drain);
        assert!(Warmup::new(false).phase() == Phase::Process);
    }

    #[test]
    fn positive_ndk_read_errors_are_actual_frame_counts() {
        assert_eq!(read_count(Err(AudioError::from(192)), 192), Ok(192));
        assert_eq!(read_count(Ok(64), 192), Ok(64));
        assert_eq!(read_count(Ok(0), 192), Ok(0));
        assert_eq!(read_count(Err(AudioError::WouldBlock), 192), Ok(0));
        assert_eq!(
            read_count(Err(AudioError::Disconnected), 192),
            Err(AudioError::Disconnected)
        );
        assert!(read_count(Err(AudioError::from(193)), 192).is_err());
    }

    #[test]
    fn barrier_never_acknowledges_a_drain_already_in_progress() {
        assert_eq!(barrier_epoch(0), 2);
        assert_eq!(barrier_epoch(1), 4);
        assert_eq!(barrier_epoch(2), 4);
        assert_eq!(barrier_epoch(3), 6);
    }

    #[test]
    fn latency_uses_monotonic_presentation_time_and_marks_unavailable() {
        assert_eq!(
            latency_micros(480, 1_000_000_000, 960, 1_002_000_000, 48000),
            8000
        );
        assert_eq!(
            latency_micros(480, 1_000_000_000, 960, 1_010_000_000, 48000),
            0
        );
        assert_eq!(
            latency_micros(480, 1_000_000_000, 960, 1_011_000_000, 48000),
            -1
        );
        assert_eq!(
            latency_micros(480, 1_000_000_000, 479, 1_002_000_000, 48000),
            -1
        );
        assert_eq!(
            latency_micros(480, 1_000_000_000, 960, 1_002_000_000, 0),
            -1
        );
    }

    #[test]
    fn rejects_nonfinite_commands_and_invalid_pointer_ids() {
        assert!(!valid_command(&Command::MicVolume(f32::NAN)));
        assert!(!valid_command(&Command::MicVolume(-1.0)));
        assert!(valid_command(&Command::MicVolume(0.0)));
        assert!(!valid_command(&Command::Accelerate(
            0.0,
            f32::INFINITY,
            0.0
        )));
        assert!(!valid_command(&Command::Touch {
            kind: crate::pd::engine::TouchKind::Down,
            id: 0,
            x: 0.0,
            y: 0.0,
        }));
        assert!(valid_command(&Command::Record(false)));
    }
}
