//! SceneArchive.java's private-storage importer and owned-tree operations.

use crate::error::{CoreError, Result};
use crate::plist::read_info;
use crate::scenes::ImportLock;
use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use zip::read::read_zipfile_from_stream_with_options;
use zip::{ZipArchive, ZipReadOptions};

pub const MAX_ENTRIES: usize = 4096;
pub const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Raw (compressed) input cap for the spool: the 256 MiB decompressed total plus headroom for
/// stored entries and the central directory. Java streamed without a spool, so this is a
/// deliberate hardening against a huge document filling private storage before entry limits run.
pub const MAX_ARCHIVE_BYTES: u64 = 320 * 1024 * 1024;
const ONE_SCENE: &str = "Archive must contain exactly one .rj scene";
const TOO_LARGE: &str = "Archive is too large to import";

/// Imports a document stream without replacing an existing scene.
pub fn import_scene(input: impl Read, root: &Path) -> Result<PathBuf> {
    let _lock = ImportLock::acquire();
    import_scene_locked(input, root)
}

pub(crate) fn import_scene_locked(input: impl Read, root: &Path) -> Result<PathBuf> {
    if !root.is_dir() && fs::create_dir_all(root).is_err() {
        return Err(CoreError::new("Cannot create scene storage"));
    }
    let staging = stage(root)?;
    let result = (|| {
        // zip's streaming reader needs a known compressed size for Java data-descriptor ZIPs.
        let mut spool = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(staging.join("archive.zip"))?;
        // Read one byte past the cap so an oversize document is detected without draining it.
        let copied = io::copy(&mut input.take(MAX_ARCHIVE_BYTES + 1), &mut spool)?;
        if copied > MAX_ARCHIVE_BYTES {
            return Err(CoreError::new(TOO_LARGE));
        }
        let extracted = staging.join("files");
        fs::create_dir(&extracted).map_err(|_| CoreError::new("Cannot stage scene"))?;
        extract(&mut spool, &extracted)?;
        let mut scenes = Vec::new();
        find_scenes(&extracted, &mut scenes)?;
        if scenes.len() != 1 {
            return Err(CoreError::new(ONE_SCENE));
        }
        let scene = &scenes[0];
        if !scene.join("_main.pd").is_file() {
            return Err(CoreError::new("Scene is missing _main.pd"));
        }
        read_info(scene)?;
        let destination = root.join(scene.file_name().ok_or_else(|| CoreError::new(ONE_SCENE))?);
        require_owned(root, &destination)?;
        if destination.exists() {
            return Err(CoreError::new(
                "A scene with this folder name already exists",
            ));
        }
        fs::rename(scene, &destination).map_err(|_| CoreError::new("Cannot publish scene"))?;
        Ok(destination)
    })();
    if let Err(cleanup) = delete_owned_tree(root, &staging) {
        if let Ok(published) = &result {
            delete_owned_tree(root, published)?;
        }
        return Err(cleanup);
    }
    result
}

fn stage(root: &Path) -> Result<PathBuf> {
    let mut random = [0u8; 16];
    let mut entropy =
        File::open("/dev/urandom").map_err(|_| CoreError::new("Cannot stage scene"))?;
    loop {
        entropy
            .read_exact(&mut random)
            .map_err(|_| CoreError::new("Cannot stage scene"))?;
        let mut name = String::from(".import-");
        for byte in random {
            use std::fmt::Write;
            write!(&mut name, "{byte:02x}").map_err(|_| CoreError::new("Cannot stage scene"))?;
        }
        let path = root.join(name);
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(CoreError::new("Cannot stage scene")),
        }
    }
}

fn extract(spool: &mut File, staging: &Path) -> Result<()> {
    spool.seek(SeekFrom::Start(0))?;
    let zip = ZipArchive::new(spool.try_clone()?).map_err(|_| CoreError::new(ONE_SCENE))?;
    check_central_entries(spool, zip.central_directory_start())?;
    let mut entries = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip.by_index_data(index).map_err(zip_error)?;
        entries.push((entry.header_start(), entry.compressed_size(), entry.crc32()));
    }
    entries.sort_unstable_by_key(|entry| entry.0);
    let mut paths = HashSet::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    for (offset, compressed, crc) in entries {
        spool.seek(SeekFrom::Start(offset))?;
        // Read names from the local header, as Java ZipInputStream does, not the central directory.
        let options = ZipReadOptions::new()
            .override_compressed_size(compressed)
            .override_crc(crc);
        let mut entry = read_zipfile_from_stream_with_options(spool, options)
            .map_err(zip_error)?
            .ok_or_else(|| CoreError::new(ONE_SCENE))?;
        let name = entry.name().map_err(zip_error)?;
        validate_name(&name)?;
        let file = staging.join(name.as_ref());
        require_owned(staging, &file)?;
        if !paths.insert(canonical_path(&file)?) {
            return Err(CoreError::new("Duplicate archive entry"));
        }
        if name.ends_with('/') {
            let mut byte = [0];
            if entry.read(&mut byte)? != 0 {
                return Err(CoreError::new("Directory entry contains data"));
            }
            if !file.is_dir() && fs::create_dir_all(&file).is_err() {
                return Err(CoreError::new("Cannot create scene directory"));
            }
        } else {
            let parent = file
                .parent()
                .ok_or_else(|| CoreError::new("Cannot create scene directory"))?;
            if !parent.is_dir() && fs::create_dir_all(parent).is_err() {
                return Err(CoreError::new("Cannot create scene directory"));
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        CoreError::new("Archive path collision")
                    } else {
                        e.into()
                    }
                })?;
            let mut size = 0u64;
            loop {
                let n = entry.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                total += n as u64;
                if size > MAX_ENTRY_BYTES || total > MAX_TOTAL_BYTES {
                    return Err(CoreError::new("Archive exceeds scene size limits"));
                }
                out.write_all(&buffer[..n])?;
            }
        }
    }
    Ok(())
}

fn zip_error(error: zip::result::ZipError) -> CoreError {
    CoreError::new(error.to_string())
}

fn validate_name(name: &str) -> Result<()> {
    // Java String.length counts UTF-16 units; split("/") drops trailing empty components.
    if name.encode_utf16().count() > 1024 || name.trim_end_matches('/').split('/').count() > 32 {
        return Err(CoreError::new("Archive path is too deep"));
    }
    if name.starts_with('/')
        || name.contains(['\\', ':'])
        || name.split('/').any(|p| p == "." || p == "..")
    {
        return Err(CoreError::new("Invalid archive path"));
    }
    Ok(())
}

fn check_central_entries(spool: &mut File, start: u64) -> Result<()> {
    // ZipArchive deduplicates exact names, so check the raw records before using its index.
    spool.seek(SeekFrom::Start(start))?;
    let mut names = HashSet::new();
    let mut count = 0;
    loop {
        let mut magic = [0; 4];
        spool.read_exact(&mut magic)?;
        if magic != *b"PK\x01\x02" {
            break;
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err(CoreError::new("Archive contains too many entries"));
        }
        let mut header = [0; 42];
        spool.read_exact(&mut header)?;
        let name_length = u16::from_le_bytes([header[24], header[25]]) as usize;
        let extra_length = u16::from_le_bytes([header[26], header[27]]) as i64;
        let comment_length = u16::from_le_bytes([header[28], header[29]]) as i64;
        let mut name = vec![0; name_length];
        spool.read_exact(&mut name)?;
        if !names.insert(name) {
            return Err(CoreError::new("Duplicate archive entry"));
        }
        spool.seek(SeekFrom::Current(extra_length + comment_length))?;
    }
    Ok(())
}

fn find_scenes(folder: &Path, scenes: &mut Vec<PathBuf>) -> Result<()> {
    let children =
        fs::read_dir(folder).map_err(|_| CoreError::new("Cannot inspect scene directory"))?;
    for child in children {
        let child = child
            .map_err(|_| CoreError::new("Cannot inspect scene directory"))?
            .path();
        if child.is_dir() {
            if child
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".rj"))
            {
                scenes.push(child.clone());
            }
            find_scenes(&child, scenes)?;
        }
    }
    Ok(())
}

/// Resolves existing ancestors while allowing a not-yet-created tail, like File.getCanonicalPath.
pub(crate) fn canonical_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => resolved.push(component),
            Component::CurDir => (),
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Component::Prefix(_) => return Err(CoreError::new("Path is outside private storage")),
        }
    }
    Ok(resolved)
}

/// Refuses the root itself, paths outside it, and symlink components even when they stay inside.
pub fn require_owned(root: &Path, target: &Path) -> Result<()> {
    let canonical_root = canonical_path(root)?;
    let canonical_target = canonical_path(target)?;
    if canonical_target == canonical_root || !canonical_target.starts_with(&canonical_root) {
        return Err(CoreError::new("Path is outside private storage"));
    }
    let root = std::path::absolute(root)?;
    let mut relative = std::path::absolute(target)?;
    while relative != root {
        match fs::symlink_metadata(&relative) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CoreError::new("Symbolic links are not scene files"));
            }
            Ok(_) => (),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(e.into()),
        }
        if !relative.pop() {
            break;
        }
    }
    Ok(())
}

/// Deletes bottom-up, rejecting links rather than traversing or removing them.
pub fn delete_owned_tree(root: &Path, target: &Path) -> Result<()> {
    require_owned(root, target)?;
    if !target.exists() {
        return Ok(());
    }
    if target.is_dir() {
        let children =
            fs::read_dir(target).map_err(|_| CoreError::new("Cannot inspect private directory"))?;
        for child in children {
            let child = child.map_err(|_| CoreError::new("Cannot inspect private directory"))?;
            delete_owned_tree(root, &child.path())?;
        }
        fs::remove_dir(target).map_err(|_| CoreError::new("Cannot delete private file"))?;
    } else {
        fs::remove_file(target).map_err(|_| CoreError::new("Cannot delete private file"))?;
    }
    Ok(())
}
