//! Process-wide libpd ownership and the preallocated UI-to-audio command path.

use std::ffi::{c_void, CStr, CString};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use arrayvec::ArrayString;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::error::{CoreError, Result};

use super::{ffi, hooks};

static ENGINE: Mutex<Engine> = Mutex::new(Engine { initialized: false });
static BLOCKSIZE: AtomicUsize = AtomicUsize::new(0);
static INPUT_CHANNELS: AtomicUsize = AtomicUsize::new(0);
static OUTPUT_CHANNELS: AtomicUsize = AtomicUsize::new(0);
static AUDIO_READY: AtomicBool = AtomicBool::new(false);

/// One global instance: hold its lock for setup/open/close while the callback is stopped.
/// The callback uses the free process/drain_into_pd functions and never takes this lock.
pub struct Engine {
    initialized: bool,
}

/// Owned patch handle; explicitly close it under Engine's lock after stopping audio.
/// There is no Drop callback because destruction on an audio thread would violate realtime rules.
#[derive(Debug)]
#[must_use]
pub struct Patch {
    handle: NonNull<c_void>,
}

// SAFETY: moving the opaque handle does not access Pd; close_patch is serialized by Engine's owner.
unsafe impl Send for Patch {}

impl Engine {
    pub fn global() -> &'static Mutex<Engine> {
        &ENGINE
    }

    pub fn init(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        // SAFETY: the global Engine lock excludes setup callers and audio has not started.
        unsafe {
            if ffi::libpd_queued_init() < -1 {
                return Err(CoreError::new("Unable to initialize libpd"));
            }
            hooks::install();
            ffi::libpd_set_verbose(0);
            BLOCKSIZE.store(ffi::libpd_blocksize() as usize, Ordering::Relaxed);
            // libpd_exists interns without dispatching, so fixed command gensyms never allocate in DSP.
            for name in [
                c"#transport",
                c"#micvolume",
                c"#touch",
                c"#accelerate",
                c"play",
                c"scene",
                c"record",
                c"down",
                c"xy",
                c"up",
                c"pd",
                c"dsp",
            ] {
                ffi::libpd_exists(name.as_ptr());
            }
            // Host overlay receivers (spec: Pd->host lists on rj_image_android / rj_text_android).
            // PdUiDispatcher used to subscribe these; bound for the process lifetime, never unbound.
            for recv in [c"rj_image_android", c"rj_text_android"] {
                if ffi::libpd_bind(recv.as_ptr()).is_null() {
                    return Err(CoreError::new("Unable to bind host overlay receiver"));
                }
            }
        }
        self.initialized = true;
        Ok(())
    }

    fn require_initialized(&self) -> Result<()> {
        if self.initialized {
            Ok(())
        } else {
            Err(CoreError::new("Pd engine is not initialized"))
        }
    }

    pub fn set_search_paths(&mut self, paths: &[&Path]) -> Result<()> {
        self.require_initialized()?;
        let paths = paths
            .iter()
            .map(|path| {
                CString::new(path.as_os_str().as_encoded_bytes())
                    .map_err(|_| CoreError::new(format!("Invalid search path: {}", path.display())))
            })
            .collect::<Result<Vec<_>>>()?;
        // SAFETY: all paths are valid C strings, and the stopped engine is held by the caller.
        unsafe {
            ffi::libpd_clear_search_path();
            for path in paths {
                ffi::libpd_add_to_search_path(path.as_ptr());
            }
        }
        Ok(())
    }

    pub fn open_patch(&self, dir: &Path, name: &str) -> Result<Patch> {
        self.require_initialized()?;
        let failure = || CoreError::new(format!("Unable to open {name} in {}", dir.display()));
        let name = CString::new(name).map_err(|_| failure())?;
        let directory = CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| failure())?;
        // SAFETY: paths are NUL-terminated; patch creation only runs under the stopped Engine lock.
        let handle =
            NonNull::new(unsafe { ffi::libpd_openfile(name.as_ptr(), directory.as_ptr()) })
                .ok_or_else(failure)?;
        // Do not query $0: this libpd's getter leaves Pd's lastpopped pointing at the patch after close.
        Ok(Patch { handle })
    }

    pub fn close_patch(&self, patch: Patch) {
        // SAFETY: Patch owns a live handle, consumed exactly once while the caller holds Engine's lock.
        unsafe {
            ffi::libpd_closefile(patch.handle.as_ptr());
        }
    }

    pub fn init_audio(&mut self, in_channels: i32, out_channels: i32, rate: i32) -> Result<()> {
        self.require_initialized()?;
        let blocksize = BLOCKSIZE.load(Ordering::Relaxed);
        let max_channels = i32::MAX as usize / (blocksize * std::mem::size_of::<f32>());
        if in_channels < 0
            || out_channels < 0
            || rate <= 0
            || in_channels as usize > max_channels
            || out_channels as usize > max_channels
        {
            return Err(CoreError::new("Invalid Pd audio configuration"));
        }
        // SAFETY: channel-buffer sizes fit Pd's C integers; setup and DSP graph allocation are stopped.
        unsafe {
            if ffi::libpd_init_audio(in_channels, out_channels, rate) != 0 {
                return Err(CoreError::new("Unable to initialize Pd audio"));
            }
            ffi::libpd_start_message(1);
            ffi::libpd_add_float(1.0);
            if ffi::libpd_finish_message(c"pd".as_ptr(), c"dsp".as_ptr()) != 0 {
                return Err(CoreError::new("Unable to enable Pd DSP"));
            }
        }
        INPUT_CHANNELS.store(in_channels as usize, Ordering::Relaxed);
        OUTPUT_CHANNELS.store(out_channels as usize, Ordering::Relaxed);
        AUDIO_READY.store(true, Ordering::Release);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TouchKind {
    Down = 0,
    Xy = 1,
    Up = 2,
}

// RecordStart carries a fixed 1 KiB path so the queue stays allocation-free (CONTRACT.md).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
    TransportPlay(bool),
    MicVolume(f32),
    Touch {
        kind: TouchKind,
        id: i32,
        x: f32,
        y: f32,
    },
    Accelerate(f32, f32, f32),
    RecordStart(ArrayString<1024>),
    Record(bool),
}

fn float_message(receiver: &CStr, selector: &CStr, values: &[f32]) {
    // SAFETY: called only by the sole Pd owner, using libpd_init's preallocated 32-atom message buffer.
    unsafe {
        ffi::libpd_start_message(values.len() as i32);
        for value in values {
            ffi::libpd_add_float(*value);
        }
        ffi::libpd_finish_message(receiver.as_ptr(), selector.as_ptr());
    }
}

impl Command {
    /// Internal Pd dispatch; only drain_into_pd may call this while the audio callback owns libpd.
    /// Fixed selectors were interned at init. RecordStart still invokes libpd's allocating gensym
    /// for a previously unseen path (and the scene's writesf~ may itself perform recording I/O).
    pub fn apply(&self) {
        match self {
            Self::TransportPlay(on) => {
                float_message(c"#transport", c"play", &[if *on { 1.0 } else { 0.0 }]);
            }
            Self::MicVolume(gain) => {
                // SAFETY: the queue's sole consumer owns Pd; the receiver is a static C string.
                unsafe {
                    ffi::libpd_float(c"#micvolume".as_ptr(), *gain);
                }
            }
            Self::Touch { kind, id, x, y } => {
                let selector = match kind {
                    TouchKind::Down => c"down",
                    TouchKind::Xy => c"xy",
                    TouchKind::Up => c"up",
                };
                float_message(c"#touch", selector, &[*id as f32, *x, *y]);
            }
            Self::Accelerate(x, y, z) => {
                // SAFETY: three floats fit the message buffer allocated by libpd_init.
                unsafe {
                    ffi::libpd_start_message(3);
                    ffi::libpd_add_float(*x);
                    ffi::libpd_add_float(*y);
                    ffi::libpd_add_float(*z);
                    ffi::libpd_finish_list(c"#accelerate".as_ptr());
                }
            }
            Self::RecordStart(path) => {
                // Queue producers validate this before the callback; never silently truncate a path.
                assert!(
                    !path.is_empty() && !path.as_bytes().contains(&0),
                    "Invalid RecordStart path"
                );
                let mut terminated = [0u8; 1025];
                terminated[..path.len()].copy_from_slice(path.as_bytes());
                // SAFETY: the stack buffer is NUL-terminated and remains alive through gensym's copy.
                unsafe {
                    ffi::libpd_start_message(1);
                    ffi::libpd_add_symbol(terminated.as_ptr().cast());
                    ffi::libpd_finish_message(c"#transport".as_ptr(), c"scene".as_ptr());
                }
                float_message(c"#transport", c"record", &[1.0]);
            }
            Self::Record(on) => {
                float_message(c"#transport", c"record", &[if *on { 1.0 } else { 0.0 }]);
            }
        }
    }
}

pub struct CommandQueue {
    producer: Mutex<Producer<Command>>,
    dropped: AtomicU64,
}

impl CommandQueue {
    /// Allocate once on the control thread; hand the consumer to the single audio callback owner.
    pub fn new() -> (Self, Consumer<Command>) {
        let (producer, consumer) = RingBuffer::new(256);
        (
            Self {
                producer: Mutex::new(producer),
                dropped: AtomicU64::new(0),
            },
            consumer,
        )
    }

    /// UI/control thread only. False means full, or an empty/NUL-bearing RecordStart path.
    /// Only full-queue drops increment the overflow counter.
    pub fn push(&self, command: Command) -> bool {
        if matches!(&command, Command::RecordStart(path) if path.is_empty() || path.as_bytes().contains(&0))
        {
            return false;
        }
        let mut producer = self
            .producer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if producer.push(command).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The only host-command entry into Pd: the audio consumer drains FIFO order without Rust locks,
/// heap allocation, sleeping, logging or JNI. RecordStart's native gensym is the documented exception.
pub fn drain_into_pd(consumer: &mut Consumer<Command>) {
    while let Ok(command) = consumer.pop() {
        command.apply();
    }
}

/// Interleaved, single-precision rendering by the sole audio callback owner.
/// init_audio must have completed, and setup/open/close must not overlap this call.
pub fn process(ticks: i32, input: &[f32], output: &mut [f32]) -> i32 {
    assert!(
        AUDIO_READY.load(Ordering::Acquire),
        "Pd audio is not initialized"
    );
    assert!(ticks >= 0, "Pd ticks must be nonnegative");
    let frames = (ticks as usize)
        .checked_mul(BLOCKSIZE.load(Ordering::Relaxed))
        .expect("Pd frame count overflow");
    let input_size = frames
        .checked_mul(INPUT_CHANNELS.load(Ordering::Relaxed))
        .expect("Pd input size overflow");
    let output_size = frames
        .checked_mul(OUTPUT_CHANNELS.load(Ordering::Relaxed))
        .expect("Pd output size overflow");
    assert_eq!(input.len(), input_size, "Pd input buffer size");
    assert_eq!(output.len(), output_size, "Pd output buffer size");
    // SAFETY: slice sizes exactly match ticks * blocksize * channels, and only this owner runs DSP.
    unsafe { ffi::libpd_process_float(ticks, input.as_ptr(), output.as_mut_ptr()) }
}
