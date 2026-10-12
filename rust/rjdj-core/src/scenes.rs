//! Startup/import recovery under the same process-wide lock as scene extraction.

use crate::archive::{delete_owned_tree, import_scene_locked, require_owned};
use crate::error::{CoreError, Result};
use crate::plist::read_info;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static IMPORT_LOCK: Mutex<()> = Mutex::new(());

/// Mirrors synchronized(SceneArchive.class); never acquire on the audio thread.
pub struct ImportLock {
    _guard: MutexGuard<'static, ()>,
}

impl ImportLock {
    pub fn acquire() -> Self {
        Self {
            _guard: IMPORT_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

/// Collects valid unregistered folders; use the callback API to retain successes on partial failure.
pub fn recover_scenes(root: &Path, registered: &[String]) -> Result<Vec<PathBuf>> {
    let mut scenes = Vec::new();
    recover_scenes_with(root, registered, |scene| {
        scenes.push(scene.to_path_buf());
        Ok(())
    })?;
    Ok(scenes)
}

/// Registers each valid candidate before returning the first failure, with subsequent failures suppressed.
pub fn recover_scenes_with(
    root: &Path,
    registered: &[String],
    mut register: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let _lock = ImportLock::acquire();
    let folders = fs::read_dir(root).map_err(|_| CoreError::new("Cannot inspect scene storage"))?;
    let mut folders = folders
        .map(|folder| folder.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|_| CoreError::new("Cannot inspect scene storage"))?;
    folders.sort();
    let mut failure: Option<CoreError> = None;
    for folder in folders {
        let result = (|| {
            let name = folder.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with(".import-") {
                delete_owned_tree(root, &folder)?;
            } else if folder.is_dir() && name.ends_with(".rj") {
                require_owned(root, &folder)?;
                if registered
                    .iter()
                    .any(|registered| registered == name.as_ref())
                {
                    return Ok(());
                }
                require_owned(&folder, &folder.join("_main.pd"))?;
                require_owned(&folder, &folder.join("Info.plist"))?;
                if !folder.join("_main.pd").is_file() {
                    return Err(CoreError::new("Scene is missing _main.pd"));
                }
                read_info(&folder)?;
                register(&folder)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(first) = &mut failure {
                first.suppressed.push(error.message);
                first.suppressed.extend(error.suppressed);
            } else {
                failure = Some(error);
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Returns the folder to register; the shell sets defaultSeeded only after registration succeeds.
///
/// An existing Atsuke directory is reused even if it is damaged; addScene performs metadata validation.
/// A persisted seed flag prevents reseeding after the user deletes the default scene.
pub fn seed_default(
    root: &Path,
    atsuke_zip: impl Read,
    already_seeded: bool,
) -> Result<Option<PathBuf>> {
    let _lock = ImportLock::acquire();
    if already_seeded {
        return Ok(None);
    }
    let sample = root.join("Atsuke.rj");
    if sample.is_dir() {
        return Ok(Some(sample));
    }
    import_scene_locked(atsuke_zip, root).map(Some)
}
