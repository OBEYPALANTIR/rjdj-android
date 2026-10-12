//! The single-precision, single-instance surface from the pinned z_libpd.h and z_queued.h.

use std::ffi::{c_char, c_int, c_void};

/// Opaque t_atom: traverse and read only through libpd's accessors.
#[repr(C)]
pub struct PdAtom {
    _private: [u8; 0],
}

pub type PrintHook = unsafe extern "C" fn(*const c_char);
pub type BangHook = unsafe extern "C" fn(*const c_char);
pub type FloatHook = unsafe extern "C" fn(*const c_char, f32);
pub type SymbolHook = unsafe extern "C" fn(*const c_char, *const c_char);
pub type ListHook = unsafe extern "C" fn(*const c_char, c_int, *mut PdAtom);
pub type MessageHook = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *mut PdAtom);

extern "C" {
    pub fn libpd_queued_init() -> c_int;
    pub fn libpd_queued_release();
    pub fn libpd_queued_receive_pd_messages();
    pub fn libpd_clear_search_path();
    pub fn libpd_add_to_search_path(path: *const c_char);
    pub fn libpd_openfile(name: *const c_char, dir: *const c_char) -> *mut c_void;
    pub fn libpd_closefile(patch: *mut c_void);
    pub fn libpd_getdollarzero(patch: *mut c_void) -> c_int;
    pub fn libpd_blocksize() -> c_int;
    pub fn libpd_init_audio(in_channels: c_int, out_channels: c_int, sample_rate: c_int) -> c_int;
    pub fn libpd_process_float(ticks: c_int, input: *const f32, output: *mut f32) -> c_int;
    pub fn libpd_bang(recv: *const c_char) -> c_int;
    pub fn libpd_float(recv: *const c_char, x: f32) -> c_int;
    pub fn libpd_symbol(recv: *const c_char, symbol: *const c_char) -> c_int;
    pub fn libpd_start_message(maxlen: c_int) -> c_int;
    pub fn libpd_add_float(x: f32);
    pub fn libpd_add_symbol(symbol: *const c_char);
    pub fn libpd_finish_list(recv: *const c_char) -> c_int;
    pub fn libpd_finish_message(recv: *const c_char, msg: *const c_char) -> c_int;
    pub fn libpd_bind(recv: *const c_char) -> *mut c_void;
    pub fn libpd_unbind(binding: *mut c_void);
    pub fn libpd_exists(recv: *const c_char) -> c_int;
    pub fn libpd_set_queued_printhook(hook: Option<PrintHook>);
    pub fn libpd_set_queued_banghook(hook: Option<BangHook>);
    pub fn libpd_set_queued_floathook(hook: Option<FloatHook>);
    pub fn libpd_set_queued_symbolhook(hook: Option<SymbolHook>);
    pub fn libpd_set_queued_listhook(hook: Option<ListHook>);
    pub fn libpd_set_queued_messagehook(hook: Option<MessageHook>);
    pub fn libpd_is_float(atom: *mut PdAtom) -> c_int;
    pub fn libpd_is_symbol(atom: *mut PdAtom) -> c_int;
    pub fn libpd_get_float(atom: *mut PdAtom) -> f32;
    pub fn libpd_get_symbol(atom: *mut PdAtom) -> *const c_char;
    pub fn libpd_next_atom(atom: *mut PdAtom) -> *mut PdAtom;
    pub fn libpd_set_verbose(verbose: c_int);
}
