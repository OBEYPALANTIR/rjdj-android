//! Recording names and post-stop validation; the scene's writesf~ owns WAV creation.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::archive::delete_owned_tree;
use crate::error::Result;

/// Match ScenePlayer's wall-clock name, without creating or truncating the file.
pub fn new_recording_path(recordings_dir: &Path, wall_clock_ms: i64) -> PathBuf {
    recordings_dir.join(format!("recording_{wall_clock_ms}.wav"))
}

/// SceneDataBase.addRecording checks only a regular file's existence and length, not
/// its WAV header. Empty owned files are removed best-effort; a deletion failure does
/// not create a recording row or destroy a nonempty file.
pub fn validate_recording(path: &Path) -> Result<bool> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    if metadata.len() != 0 {
        return Ok(true);
    }
    if let Some(parent) = path.parent() {
        let _ = delete_owned_tree(parent, path);
    }
    Ok(false)
}

/// Read the little-endian sample rate from a bounded RIFF/WAVE fmt chunk. This is a
/// verification helper, deliberately separate from the permissive registration check.
pub fn wav_sample_rate(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return None;
    }
    let end = u32::from_le_bytes(header[4..8].try_into().ok()?) as u64 + 8;
    if end < 12 || end > file.metadata().ok()?.len() {
        return None;
    }
    let mut position = 12u64;
    while position + 8 <= end {
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk).ok()?;
        let length = u32::from_le_bytes(chunk[4..].try_into().ok()?) as u64;
        let data_end = position + 8 + length;
        if data_end > end {
            return None;
        }
        if &chunk[..4] == b"fmt " {
            if length < 16 {
                return None;
            }
            let mut fmt = [0u8; 16];
            file.read_exact(&mut fmt).ok()?;
            let rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
            return (rate != 0).then_some(rate);
        }
        position = data_end + (length & 1);
        file.seek(SeekFrom::Start(position)).ok()?;
    }
    None
}
