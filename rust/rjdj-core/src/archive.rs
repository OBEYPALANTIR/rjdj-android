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
use zip::ZipReadOptions;

pub const MAX_ENTRIES: usize = 4096;
pub const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Raw (compressed) input cap for the spool: the 256 MiB decompressed total plus headroom for
/// stored entries and the central directory. Java streamed without a spool, so this is a
/// deliberate hardening against a huge document filling private storage before entry limits run.
pub const MAX_ARCHIVE_BYTES: u64 = 320 * 1024 * 1024;
const ONE_SCENE: &str = "Archive must contain exactly one .rj scene";
const TOO_LARGE: &str = "Archive is too large to import";
// Local-header offset, compressed size and CRC32.
type CentralEntry = (u64, u64, u32);

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
    let mut entries = find_central_directory(spool)?;
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

fn find_central_directory(spool: &mut File) -> Result<Vec<CentralEntry>> {
    let length = spool.seek(SeekFrom::End(0))?;
    let mut buffer = [0; 8192];
    let mut end = length;
    while end != 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let size = (end - start) as usize;
        spool.seek(SeekFrom::Start(start))?;
        spool.read_exact(&mut buffer[..size])?;
        for (offset, magic) in buffer[..size].windows(4).enumerate().rev() {
            if magic != b"PK\x05\x06" {
                continue;
            }
            if let Some((directory, count, directory_end)) =
                central_directory_at(spool, start + offset as u64, length)
            {
                if count > MAX_ENTRIES as u64 {
                    return Err(CoreError::new("Archive contains too many entries"));
                }
                if let Some(entries) =
                    check_central_entries(spool, directory, count, directory_end)?
                {
                    return Ok(entries);
                }
            }
        }
        if start == 0 {
            break;
        }
        end = start + 3; // Preserve signatures spanning adjacent scan buffers.
    }
    Err(CoreError::new(ONE_SCENE))
}

fn central_directory_at(
    spool: &mut (impl Read + Seek),
    position: u64,
    length: u64,
) -> Option<(u64, u64, u64)> {
    if length.checked_sub(position)? < 22 {
        return None;
    }
    let mut header = [0; 22];
    spool.seek(SeekFrom::Start(position)).ok()?;
    spool.read_exact(&mut header).ok()?;
    let comment = u16::from_le_bytes([header[20], header[21]]) as u64;
    if header[..4] != *b"PK\x05\x06" || comment > length - position - 22 {
        return None;
    }
    let on_disk = u16::from_le_bytes([header[8], header[9]]);
    let total = u16::from_le_bytes([header[10], header[11]]);
    let may_be_zip64 =
        total == u16::MAX || header[12..16] == [0xff; 4] || header[16..20] == [0xff; 4];
    if let Some(directory) = zip64_directory_at(spool, position) {
        return Some(directory);
    }
    if may_be_zip64 {
        return None;
    }
    let directory = u32::from_le_bytes(header[16..20].try_into().ok()?) as u64;
    if header[4..6] != header[6..8] || directory >= position {
        return None;
    }
    let mut magic = [0; 4];
    spool.seek(SeekFrom::Start(directory)).ok()?;
    spool.read_exact(&mut magic).ok()?;
    (magic == *b"PK\x01\x02").then_some((directory, u64::from(total.max(on_disk)), position))
}

fn zip64_directory_at(
    spool: &mut (impl Read + Seek),
    position: u64,
) -> Option<(u64, u64, u64)> {
    let locator_position = position.checked_sub(20)?;
    let mut locator = [0; 20];
    spool.seek(SeekFrom::Start(locator_position)).ok()?;
    spool.read_exact(&mut locator).ok()?;
    if locator[..4] != *b"PK\x06\x07" {
        return None;
    }
    let record_position = u64::from_le_bytes(locator[8..16].try_into().ok()?);
    let disks = u32::from_le_bytes(locator[16..20].try_into().ok()?);
    if disks > 1 || locator_position.checked_sub(record_position)? < 56 {
        return None;
    }
    let mut record = [0; 56];
    spool.seek(SeekFrom::Start(record_position)).ok()?;
    spool.read_exact(&mut record).ok()?;
    let record_size = u64::from_le_bytes(record[4..12].try_into().ok()?);
    // Check the actual locator boundary without reading the extensible data sector.
    if record[..4] != *b"PK\x06\x06"
        || record_size < 44
        || record_size.checked_add(12)? != locator_position - record_position
        || record[16..20] != record[20..24]
        || record[20..24] != locator[4..8]
    {
        return None;
    }
    let on_disk = u64::from_le_bytes(record[24..32].try_into().ok()?);
    let total = u64::from_le_bytes(record[32..40].try_into().ok()?);
    let directory = u64::from_le_bytes(record[48..56].try_into().ok()?);
    if on_disk != total || record_position < directory.saturating_add(total.saturating_mul(46)) {
        return None;
    }
    Some((directory, total, record_position))
}

fn check_central_entries(
    spool: &mut File,
    start: u64,
    declared_count: u64,
    end: u64,
) -> Result<Option<Vec<CentralEntry>>> {
    if spool.seek(SeekFrom::Start(start)).is_err() {
        return Ok(None);
    }
    let mut names = HashSet::new();
    let mut duplicate = false;
    let mut entries = Vec::with_capacity(declared_count.min(MAX_ENTRIES as u64) as usize);
    let mut position = start;
    loop {
        let mut magic = [0; 4];
        if spool.read_exact(&mut magic).is_err() {
            return Ok(None);
        }
        if magic != *b"PK\x01\x02" {
            if magic == *b"PK\x05\x05" {
                if end.saturating_sub(position) < 6 {
                    return Ok(None);
                }
                let mut length = [0; 2];
                if spool.read_exact(&mut length).is_err() {
                    return Ok(None);
                }
                // Signature bytes are opaque; only their single-record boundary matters.
                position += 6 + u64::from(u16::from_le_bytes(length));
            }
            if position != end || entries.len() as u64 != declared_count {
                return Ok(None);
            }
            if duplicate {
                return Err(CoreError::new("Duplicate archive entry"));
            }
            return Ok(Some(entries));
        }
        if entries.len() == MAX_ENTRIES {
            return Err(CoreError::new("Archive contains too many entries"));
        }
        let Some((name, entry, next)) = central_entry_at(spool, position, end) else {
            return Ok(None);
        };
        // Only report duplicate names once this candidate's count and boundary are valid.
        duplicate |= !names.insert(name);
        entries.push(entry);
        position = next;
    }
}

fn central_entry_at(
    spool: &mut File,
    position: u64,
    end: u64,
) -> Option<(Vec<u8>, CentralEntry, u64)> {
    if end.checked_sub(position)? < 46 {
        return None;
    }
    let mut header = [0; 42];
    spool.read_exact(&mut header).ok()?;
    let name_length = u16::from_le_bytes([header[24], header[25]]) as usize;
    let extra_length = u16::from_le_bytes([header[26], header[27]]) as u64;
    let comment_length = u16::from_le_bytes([header[28], header[29]]) as u64;
    let mut extra_position = position + 46 + name_length as u64;
    let extra_end = extra_position + extra_length;
    let next = extra_end + comment_length;
    if next > end {
        return None;
    }
    let mut name = vec![0; name_length];
    spool.read_exact(&mut name).ok()?;
    let crc = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    let mut fields = [0; 3]; // Uncompressed size, compressed size, local-header offset.
    for (field, offset) in fields.iter_mut().zip([20, 16, 38]) {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(&header[offset..offset + 4]);
        *field = u32::from_le_bytes(bytes) as u64;
    }
    if fields.contains(&(u32::MAX as u64)) {
        let mut found = false;
        while extra_position + 4 <= extra_end {
            let mut extra = [0; 4];
            spool.read_exact(&mut extra).ok()?;
            let size = u16::from_le_bytes([extra[2], extra[3]]) as u64;
            let field_end = extra_position + 4 + size;
            if field_end > extra_end {
                return None;
            }
            if extra[..2] == [1, 0] {
                let mut position = extra_position + 4;
                for field in &mut fields {
                    if *field == u32::MAX as u64 {
                        if position + 8 > field_end {
                            return None;
                        }
                        let mut bytes = [0; 8];
                        spool.read_exact(&mut bytes).ok()?;
                        *field = u64::from_le_bytes(bytes);
                        position += 8;
                    }
                }
                found = true;
                break;
            }
            spool.seek(SeekFrom::Start(field_end)).ok()?;
            extra_position = field_end;
        }
        if !found {
            return None;
        }
    }
    spool.seek(SeekFrom::Start(next)).ok()?;
    Some((name, (fields[2], fields[1], crc), next))
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

#[cfg(test)]
#[test]
fn mismatched_zip64_locator_is_checked_with_fixed_reads_only() {
    struct FooterOnly {
        position: u64,
        locator: u64,
        record: [u8; 56],
        trailer: [u8; 42],
    }
    impl Seek for FooterOnly {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let SeekFrom::Start(position) = from else {
                panic!("footer validation must use absolute offsets");
            };
            self.position = position;
            Ok(position)
        }
    }
    impl Read for FooterOnly {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let bytes: &[u8] = match self.position {
                0 => &self.record,
                p if p == self.locator => &self.trailer[..20],
                p if p == self.locator + 20 => &self.trailer[20..],
                _ => panic!("attempted to read the ZIP64 extensible data sector"),
            };
            assert_eq!(out.len(), bytes.len(), "only fixed-size footer reads");
            out.copy_from_slice(bytes);
            self.position += out.len() as u64;
            Ok(out.len())
        }
    }
    let extension = 256 * 1024 * 1024;
    let mut spool = FooterOnly {
        position: 0,
        locator: 56 + extension + 1,
        record: [0; 56],
        trailer: [0; 42],
    };
    spool.record[..4].copy_from_slice(b"PK\x06\x06");
    spool.record[4..12].copy_from_slice(&(44u64 + extension).to_le_bytes());
    spool.record[24..32].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
    spool.record[32..40].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
    spool.trailer[..4].copy_from_slice(b"PK\x06\x07");
    spool.trailer[16..20].copy_from_slice(&1u32.to_le_bytes());
    spool.trailer[20..24].copy_from_slice(b"PK\x05\x06");
    spool.trailer[28..40].fill(0xff);
    let position = spool.locator + 20;
    let length = position + 22;
    assert_eq!(central_directory_at(&mut spool, position, length), None);
}
