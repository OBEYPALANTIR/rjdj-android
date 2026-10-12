//! Error and status types shared across modules (orchestrator-owned; see CONTRACT.md).

use std::fmt;

/// Failure carrying the exact user-visible message (byte-equal to the Java IOException text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreError {
    pub message: String,
    /// Later failures kept alongside the first one, like Java's `Throwable.addSuppressed`.
    pub suppressed: Vec<String>,
}

impl CoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            suppressed: Vec::new(),
        }
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::new(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// JNI status codes (CONTRACT.md "Status").
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    AlreadyRunning = 1,
    NotInitialized = -1,
    PatchOpenFailed = -2,
    OutputOpenFailed = -3,
    InputOpenFailed = -4,
    StartFailed = -5,
    InvalidArgument = -6,
    Disconnected = -7,
    Internal = -8,
}

/// Engine lifecycle state (CONTRACT.md "State").
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped = 0,
    Starting = 1,
    Running = 2,
    Restarting = 3,
    Failed = 4,
}
