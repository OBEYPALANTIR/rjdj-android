//! rjdj-core: one cdylib replacing pd-for-android's JNI glue, PdService/PdAudio hosting and the
//! Oboe engine for the RjDj revival. See CONTRACT.md for the ABI and ownership of each module.

pub mod archive;
pub mod error;
pub mod fifo;
pub mod pd;
pub mod playback;
pub mod plist;
pub mod recording;
pub mod resample;
pub mod scenes;

#[cfg(target_os = "android")]
pub mod audio;
#[cfg(target_os = "android")]
pub mod jni;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
