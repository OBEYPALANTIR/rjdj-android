//! Queued hooks run on the drain thread, never inside libpd's audio processing.

use std::ffi::{c_char, c_int, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Mutex};

use super::ffi;

#[derive(Debug, Clone, PartialEq)]
pub enum Atom {
    Float(f32),
    Symbol(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PdEvent {
    Print(String),
    Bang {
        recv: String,
    },
    Float {
        recv: String,
        x: f32,
    },
    Symbol {
        recv: String,
        sym: String,
    },
    List {
        recv: String,
        atoms: Vec<Atom>,
    },
    Message {
        recv: String,
        msg: String,
        atoms: Vec<Atom>,
    },
}

static EVENT_SINK: Mutex<Option<Sender<PdEvent>>> = Mutex::new(None);
static PRINT_FRAGMENT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static DRAIN_LOCK: Mutex<()> = Mutex::new(());
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Replace the process-wide sink. Events are owned and safe to forward to JNI on the drain thread.
pub fn set_event_sink(sender: Sender<PdEvent>) {
    *EVENT_SINK.lock().unwrap_or_else(|error| error.into_inner()) = Some(sender);
}

fn emit(event: PdEvent) {
    if let Some(sender) = EVENT_SINK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        // A dropped listener is normal; queued delivery must not unwind through C.
        let _ = sender.send(event);
    }
}

unsafe fn string(value: *const c_char) -> String {
    // SAFETY: libpd guarantees a live, NUL-terminated string for the duration of a hook.
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}

unsafe fn atoms(argc: c_int, mut argv: *mut ffi::PdAtom) -> Vec<Atom> {
    let mut output = Vec::with_capacity(argc.max(0) as usize);
    for index in 0..argc {
        // SAFETY: queued hooks supply argc valid atoms; accessors avoid assuming t_atom's layout.
        unsafe {
            if ffi::libpd_is_float(argv) != 0 {
                output.push(Atom::Float(ffi::libpd_get_float(argv)));
            } else if ffi::libpd_is_symbol(argv) != 0 {
                output.push(Atom::Symbol(string(ffi::libpd_get_symbol(argv))));
            }
            // Pd pointer atoms have no Java/RjDj counterpart and are deliberately omitted.
            if index + 1 < argc {
                argv = ffi::libpd_next_atom(argv);
            }
        }
    }
    output
}

unsafe extern "C" fn print_hook(value: *const c_char) {
    // SAFETY: libpd supplies a live C string; accumulate bytes before decoding split UTF-8.
    let fragment = unsafe { CStr::from_ptr(value) }.to_bytes();
    let mut pending = PRINT_FRAGMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for part in fragment.split_inclusive(|byte| *byte == b'\n') {
        pending.extend_from_slice(part);
        if part.last() == Some(&b'\n') {
            pending.pop();
            emit(PdEvent::Print(
                String::from_utf8_lossy(&pending).into_owned(),
            ));
            pending.clear();
        }
    }
}

unsafe extern "C" fn bang_hook(recv: *const c_char) {
    // SAFETY: receiver strings are valid during queued dispatch.
    emit(PdEvent::Bang {
        recv: unsafe { string(recv) },
    });
}

unsafe extern "C" fn float_hook(recv: *const c_char, x: f32) {
    // SAFETY: receiver strings are valid during queued dispatch.
    emit(PdEvent::Float {
        recv: unsafe { string(recv) },
        x,
    });
}

unsafe extern "C" fn symbol_hook(recv: *const c_char, sym: *const c_char) {
    // SAFETY: both strings are valid during queued dispatch.
    unsafe {
        emit(PdEvent::Symbol {
            recv: string(recv),
            sym: string(sym),
        });
    }
}

unsafe extern "C" fn list_hook(recv: *const c_char, argc: c_int, argv: *mut ffi::PdAtom) {
    // SAFETY: the receiver and argc atoms are live until this queued hook returns.
    unsafe {
        emit(PdEvent::List {
            recv: string(recv),
            atoms: atoms(argc, argv),
        });
    }
}

unsafe extern "C" fn message_hook(
    recv: *const c_char,
    msg: *const c_char,
    argc: c_int,
    argv: *mut ffi::PdAtom,
) {
    // SAFETY: the receiver, selector and argc atoms are live until this queued hook returns.
    unsafe {
        emit(PdEvent::Message {
            recv: string(recv),
            msg: string(msg),
            atoms: atoms(argc, argv),
        });
    }
}

pub(super) unsafe fn install() {
    // SAFETY: Engine calls this once after queued initialization, while DSP is stopped.
    unsafe {
        ffi::libpd_set_queued_printhook(Some(print_hook));
        ffi::libpd_set_queued_banghook(Some(bang_hook));
        ffi::libpd_set_queued_floathook(Some(float_hook));
        ffi::libpd_set_queued_symbolhook(Some(symbol_hook));
        ffi::libpd_set_queued_listhook(Some(list_hook));
        ffi::libpd_set_queued_messagehook(Some(message_hook));
    }
    INSTALLED.store(true, Ordering::Release);
}

/// Dispatch queued events on the non-realtime drain thread, after Engine::init.
/// libpd's message ring has one reader; this lock serializes drain callers, not audio callbacks.
pub fn drain_queued() {
    assert!(
        INSTALLED.load(Ordering::Acquire),
        "Pd engine is not initialized"
    );
    let _drain = DRAIN_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    // SAFETY: the queued ring remains alive for the process, and this is its only drain reader.
    unsafe {
        ffi::libpd_queued_receive_pd_messages();
    }
}
