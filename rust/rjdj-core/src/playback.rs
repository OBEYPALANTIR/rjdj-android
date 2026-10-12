//! Private playback copies preserve imported scenes while substituting the Android bridges.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::archive::canonical_path;
use crate::error::{CoreError, Result};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const BRIDGES: [&str; 4] = [
    "rj_image.pd",
    "rj_text.pd",
    "soundinput.pd",
    "soundoutput.pd",
];
static ACTIVE_COPIES: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static NEXT_COPY: AtomicU64 = AtomicU64::new(0);

/// Java's canonical containment check: permit missing assets, but not an absolute path,
/// an escape (including through a symlink), or the scene directory itself.
pub fn contained_file(root: &Path, rel: &str) -> Result<PathBuf> {
    let root = canonical_path(root)?;
    let file = canonical_path(&root.join(rel))?;
    if Path::new(rel).is_absolute() || !file.starts_with(&root) || file == root {
        return Err(CoreError::new("Path escapes scene"));
    }
    Ok(file)
}

/// Make a process-owned copy; bridge names are omitted before inspecting or counting
/// those entries, exactly as in ScenePlayback.java. Failed copies are always released.
pub fn prepare(source_scene: &Path, cache: &Path) -> Result<PathBuf> {
    if !source_scene.is_dir() {
        return Err(CoreError::new("Scene is missing"));
    }
    fs::create_dir_all(cache)?;
    let cache = canonical_path(cache)?;
    let copy = {
        let mut active = ACTIVE_COPIES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reclaim(&cache, &active)?;
        let copy = loop {
            let path = cache.join(format!(
                "playback-{}-{}",
                std::process::id(),
                NEXT_COPY.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        // Register before unlocking: another player's preparation cannot reclaim this copy.
        active.insert(copy.clone());
        copy
    };
    let result = (|| {
        let source = canonical_path(source_scene)?;
        copy_tree(&source, &copy, 0, &mut 0, &mut 0)?;
        if !copy.join("_main.pd").is_file() {
            return Err(CoreError::new("Scene has no _main.pd"));
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(copy),
        Err(error) => {
            release(&copy);
            Err(error)
        }
    }
}

fn copy_tree(
    source: &Path,
    target: &Path,
    depth: usize,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    *entries += 1;
    let metadata = fs::symlink_metadata(source)?;
    if depth > 64 || *entries > 10000 || metadata.file_type().is_symlink() {
        return Err(CoreError::new("Scene exceeds limits or contains links"));
    }
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        let children = fs::read_dir(source).map_err(|_| CoreError::new("Cannot read scene"))?;
        for child in children {
            let child = child.map_err(|_| CoreError::new("Cannot read scene"))?;
            let name = child.file_name();
            if BRIDGES.iter().any(|bridge| name == *bridge) {
                continue;
            }
            copy_tree(&child.path(), &target.join(name), depth + 1, entries, bytes)?;
        }
    } else {
        *bytes = bytes.saturating_add(metadata.len());
        if !metadata.is_file() || *bytes > MAX_BYTES {
            return Err(CoreError::new("Scene exceeds limits"));
        }
        fs::copy(source, target)?;
    }
    Ok(())
}

/// Reclaim all unowned `playback-*` cache entries without following links. Ownership
/// is process-local, so copies left by a terminated process are abandoned on restart.
pub fn reclaim_abandoned(cache: &Path) -> Result<()> {
    let cache = canonical_path(cache)?;
    let active = ACTIVE_COPIES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    reclaim(&cache, &active)
}

fn reclaim(cache: &Path, active: &HashSet<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(cache).map_err(|_| CoreError::new("Cannot inspect scene cache"))?;
    for entry in entries {
        let entry = entry.map_err(|_| CoreError::new("Cannot inspect scene cache"))?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(b"playback-")
        {
            continue;
        }
        let path = entry.path();
        if active.contains(&path) {
            continue;
        }
        delete(&path);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(CoreError::new("Cannot reclaim scene cache")),
        }
    }
    Ok(())
}

/// Remove process ownership and best-effort delete a private playback copy. Idempotent.
pub fn release(dir: &Path) {
    let mut active = ACTIVE_COPIES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let key = canonical_path(dir).unwrap_or_else(|_| dir.to_path_buf());
    active.remove(&key);
    delete(dir);
}

fn delete(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

/// Global search paths in Java registration order, with duplicate directories removed.
/// SceneTabs.unpackResources extracts `abstractions.zip` into filesDir and adds it;
/// PdService.onCreate adds filesDir again, then nativeLibraryDir for externals.
/// ScenePlayer.initPdService only prepares the copy; startAudio opens its `_main.pd`.
/// Pd searches the canvas directory and its `declare -path rj` (as in Atsuke) before
/// global paths: the scene's copied `rj/` library is NOT an explicit global entry.
/// Removing the four bridge filenames at every depth lets the filesDir versions win:
/// soundinput/soundoutput implement transport, microphone gain and writesf~ recording;
/// rj_image/rj_text forward overlays to the Android receivers. Other scene assets stay local.
pub fn search_paths(
    _playback_dir: &Path,
    abstractions_dir: &Path,
    native_library_dir: &Path,
) -> Vec<PathBuf> {
    let mut paths = vec![abstractions_dir.to_path_buf()];
    if native_library_dir != abstractions_dir {
        paths.push(native_library_dir.to_path_buf());
    }
    paths
}
