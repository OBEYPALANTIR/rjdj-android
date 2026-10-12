//! Single-instance Pd engine facade.
//!
//! Setup/open/close calls hold Engine::global()'s lock while audio is stopped. The audio
//! callback alone calls drain_into_pd/process without that lock. drain_queued has a
//! separate non-realtime reader and is the only place Rust event hooks execute.

pub mod engine;
pub mod ffi;
pub mod hooks;

pub use engine::{drain_into_pd, process, Command, CommandQueue, Engine, Patch, TouchKind};
pub use hooks::{drain_queued, set_event_sink, Atom, PdEvent};
