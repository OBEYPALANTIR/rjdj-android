use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rjdjcore::playback::{contained_file, prepare, reclaim_abandoned, release, search_paths};
use rjdjcore::recording::{new_recording_path, validate_recording, wav_sample_rate};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "rjdj-playback-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path.canonicalize().unwrap()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test directory: {e}"),
            }
        }
    }

    fn scene(&self) -> PathBuf {
        let source = self.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("_main.pd"), [1, 2]).unwrap();
        source
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(self.0.join("cache")) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("playback-") {
                    release(&entry.path());
                }
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cache_entries(cache: &Path) -> usize {
    fs::read_dir(cache).unwrap().count()
}

#[test]
fn java_playback_check_preserves_sources_live_copies_and_unrelated_cache() {
    let root = TempDir::new();
    let source = root.scene();
    let nested = source.join("nested");
    fs::create_dir(&nested).unwrap();
    let bridges = [
        "rj_image.pd",
        "rj_text.pd",
        "soundinput.pd",
        "soundoutput.pd",
    ];
    for name in bridges {
        fs::write(source.join(name), [3]).unwrap();
        fs::write(nested.join(name), [4]).unwrap();
    }
    fs::write(nested.join("image.png"), [5, 6]).unwrap();
    fs::write(nested.join("soundinput.pd.bak"), [7]).unwrap();
    fs::create_dir(nested.join("case")).unwrap();
    fs::write(nested.join("case/Soundinput.pd"), [8]).unwrap();
    let cache = root.0.join("cache");
    let abandoned = cache.join("playback-abandoned");
    fs::create_dir_all(&abandoned).unwrap();
    fs::write(abandoned.join("unfinished"), [9]).unwrap();
    let unrelated = cache.join("unrelated");
    fs::create_dir(&unrelated).unwrap();

    let copy = prepare(&source, &cache).unwrap();
    assert!(!abandoned.exists());
    assert!(unrelated.is_dir());
    assert!(copy.starts_with(&cache));
    assert!(copy
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("playback-"));
    assert_eq!(fs::read(copy.join("_main.pd")).unwrap(), [1, 2]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let second = prepare(&source, &cache).unwrap();
    assert_ne!(copy, second);
    assert!(copy.join("_main.pd").is_file());
    assert!(second.join("_main.pd").is_file());
    for name in bridges {
        assert_eq!(fs::read(source.join(name)).unwrap(), [3]);
        assert_eq!(fs::read(nested.join(name)).unwrap(), [4]);
        assert!(!copy.join(name).exists());
        assert!(!copy.join("nested").join(name).exists());
    }
    assert_eq!(
        fs::read(contained_file(&copy, "nested/image.png").unwrap()).unwrap(),
        [5, 6]
    );
    assert_eq!(
        fs::read(copy.join("nested/soundinput.pd.bak")).unwrap(),
        [7]
    );
    assert_eq!(
        fs::read(copy.join("nested/case/Soundinput.pd")).unwrap(),
        [8]
    );
    for bad in ["../source/_main.pd", "/etc/passwd", ".", "nested/..", ""] {
        assert_eq!(
            contained_file(&copy, bad).unwrap_err().message,
            "Path escapes scene"
        );
    }
    // getCanonicalFile does not require the requested image to exist.
    assert_eq!(
        contained_file(&copy, "nested/missing.png").unwrap(),
        copy.join("nested/missing.png")
    );
    assert_eq!(
        contained_file(&copy, "nested/../_main.pd").unwrap(),
        copy.join("_main.pd")
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&root.0, source.join("escape")).unwrap();
        assert_eq!(
            prepare(&source, &cache).unwrap_err().message,
            "Scene exceeds limits or contains links"
        );
        assert_eq!(
            cache_entries(&cache),
            3,
            "failed copy must not leak or remove live copies"
        );
        assert!(source.join("_main.pd").is_file());
        fs::remove_file(source.join("escape")).unwrap();
    }
    let mut deep = source.clone();
    for _ in 0..65 {
        deep = deep.join("d");
        fs::create_dir(&deep).unwrap();
    }
    assert_eq!(
        prepare(&source, &cache).unwrap_err().message,
        "Scene exceeds limits or contains links"
    );
    assert_eq!(cache_entries(&cache), 3);
    release(&copy);
    assert!(!copy.exists());
    assert!(second.join("_main.pd").is_file());
    release(&second);
    assert_eq!(cache_entries(&cache), 1);
}

#[test]
fn depth_64_is_allowed_and_omitted_bridges_are_not_visited() {
    let root = TempDir::new();
    let source = root.scene();
    let mut deep = source.clone();
    for _ in 0..64 {
        deep = deep.join("d");
        fs::create_dir(&deep).unwrap();
    }
    // Java skips a matching name before counting, inspecting links or descending.
    let skipped = deep.join("rj_image.pd");
    fs::create_dir(&skipped).unwrap();
    fs::write(skipped.join("otherwise-too-deep"), [1]).unwrap();
    let copy = prepare(&source, &root.0.join("cache")).unwrap();
    assert!(copy.join(deep.strip_prefix(&source).unwrap()).is_dir());
    assert!(!copy.join(skipped.strip_prefix(&source).unwrap()).exists());
    release(&copy);
}

#[test]
fn counts_root_and_copied_entries_toward_the_10000_entry_limit() {
    let root = TempDir::new();
    let source = root.scene();
    for n in 0..9998 {
        File::create(source.join(format!("asset-{n}"))).unwrap();
    }
    let cache = root.0.join("cache");
    let copy = prepare(&source, &cache).unwrap();
    assert_eq!(fs::read_dir(&copy).unwrap().count(), 9999);
    release(&copy);
    File::create(source.join("one-too-many")).unwrap();
    assert_eq!(
        prepare(&source, &cache).unwrap_err().message,
        "Scene exceeds limits or contains links"
    );
    assert_eq!(cache_entries(&cache), 0);
}

#[test]
fn refuses_over_256_mib_without_leaking_or_changing_source() {
    let root = TempDir::new();
    let source = root.scene();
    let big = source.join("_main.pd");
    let length = 256 * 1024 * 1024 + 1;
    File::create(&big).unwrap().set_len(length).unwrap();
    let cache = root.0.join("cache");
    assert_eq!(
        prepare(&source, &cache).unwrap_err().message,
        "Scene exceeds limits"
    );
    assert_eq!(fs::metadata(&big).unwrap().len(), length);
    assert_eq!(cache_entries(&cache), 0);
}

#[test]
fn missing_scene_and_main_have_java_messages_and_failed_copies_are_removed() {
    let root = TempDir::new();
    let cache = root.0.join("cache");
    assert_eq!(
        prepare(&root.0.join("absent"), &cache).unwrap_err().message,
        "Scene is missing"
    );
    let source = root.scene();
    fs::remove_file(source.join("_main.pd")).unwrap();
    assert_eq!(
        prepare(&source, &cache).unwrap_err().message,
        "Scene has no _main.pd"
    );
    assert_eq!(cache_entries(&cache), 0);
    fs::create_dir(source.join("_main.pd")).unwrap();
    assert_eq!(
        prepare(&source, &cache).unwrap_err().message,
        "Scene has no _main.pd"
    );
    assert_eq!(cache_entries(&cache), 0);
    let file = root.0.join("not-a-directory");
    fs::write(&file, [1]).unwrap();
    assert_eq!(
        prepare(&file, &cache).unwrap_err().message,
        "Scene is missing"
    );
}

#[cfg(unix)]
#[test]
fn refuses_file_directory_and_dangling_symlinks_but_retains_java_hardlink_behavior() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new();
    let source = root.scene();
    let cache = root.0.join("cache");
    for target in [
        source.join("_main.pd"),
        root.0.clone(),
        root.0.join("missing"),
    ] {
        symlink(&target, source.join("link")).unwrap();
        assert_eq!(
            prepare(&source, &cache).unwrap_err().message,
            "Scene exceeds limits or contains links"
        );
        assert_eq!(cache_entries(&cache), 0);
        fs::remove_file(source.join("link")).unwrap();
    }
    fs::hard_link(source.join("_main.pd"), source.join("hard-link")).unwrap();
    let copy = prepare(&source, &cache).unwrap();
    assert_eq!(fs::read(copy.join("hard-link")).unwrap(), [1, 2]);
    release(&copy);
    symlink(&source, root.0.join("source-alias")).unwrap();
    let copy = prepare(&root.0.join("source-alias"), &cache).unwrap();
    assert!(copy.join("_main.pd").is_file());
    release(&copy);
}

#[cfg(unix)]
#[test]
fn contained_paths_resolve_symlinks_before_testing_containment() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new();
    let source = root.scene();
    symlink(&root.0, source.join("outside")).unwrap();
    assert_eq!(
        contained_file(&source, "outside/missing.png")
            .unwrap_err()
            .message,
        "Path escapes scene"
    );
    symlink(source.join("_main.pd"), source.join("inside")).unwrap();
    assert_eq!(
        contained_file(&source, "inside").unwrap(),
        source.join("_main.pd")
    );
    symlink(&source, root.0.join("alias")).unwrap();
    assert_eq!(
        contained_file(&root.0.join("alias"), "_main.pd").unwrap(),
        source.join("_main.pd")
    );
}

#[test]
fn reclaim_removes_only_abandoned_prefixed_entries_and_release_is_idempotent() {
    let root = TempDir::new();
    let source = root.scene();
    let cache = root.0.join("cache");
    let live = prepare(&source, &cache).unwrap();
    let stale = cache.join("playback-stale");
    fs::create_dir_all(stale.join("partial")).unwrap();
    let unrelated = cache.join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    fs::write(cache.join("playback-file"), [1]).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&source, cache.join("playback-link")).unwrap();
    }
    reclaim_abandoned(&cache).unwrap();
    assert!(!stale.exists());
    assert!(!cache.join("playback-file").exists());
    assert!(unrelated.is_dir());
    assert!(live.join("_main.pd").is_file());
    assert!(source.join("_main.pd").is_file());
    assert_eq!(cache_entries(&cache), 2);
    release(&live);
    release(&live);
    assert!(!live.exists());
    assert_eq!(cache_entries(&cache), 1);
    assert_eq!(
        reclaim_abandoned(&root.0.join("missing-cache"))
            .unwrap_err()
            .message,
        "Cannot inspect scene cache"
    );
}

#[test]
fn search_paths_follow_java_global_registration_not_patch_local_rj() {
    let scene = Path::new("/private/cache/playback-example");
    let abstractions = Path::new("/private/files");
    let externals = Path::new("/private/native-libs");
    assert_eq!(
        search_paths(scene, abstractions, externals),
        [abstractions.to_path_buf(), externals.to_path_buf()]
    );
    assert_eq!(
        search_paths(scene, abstractions, abstractions),
        [abstractions.to_path_buf()]
    );
}

#[test]
fn recording_validation_matches_database_semantics_without_validating_wav() {
    let root = TempDir::new();
    let path = new_recording_path(&root.0, 1791700000123);
    assert_eq!(path.file_name().unwrap(), "recording_1791700000123.wav");
    assert!(!validate_recording(&path).unwrap());
    assert!(!path.exists());
    fs::write(&path, []).unwrap();
    assert!(!validate_recording(&path).unwrap());
    assert!(!path.exists());
    fs::write(&path, [1, 2, 3]).unwrap();
    assert!(validate_recording(&path).unwrap());
    assert_eq!(fs::read(&path).unwrap(), [1, 2, 3]);
    assert!(!validate_recording(&root.0).unwrap());
}

#[cfg(unix)]
#[test]
fn recording_symlinks_follow_java_validation_but_empty_links_are_not_deleted() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new();
    let empty = root.0.join("empty.wav");
    fs::write(&empty, []).unwrap();
    let link = root.0.join("link.wav");
    symlink(&empty, &link).unwrap();
    assert!(!validate_recording(&link).unwrap());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(empty.exists());
    fs::write(&empty, [1]).unwrap();
    assert!(validate_recording(&link).unwrap());
    assert_eq!(fs::read(&empty).unwrap(), [1]);
}

fn riff(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    for (name, data) in chunks {
        bytes.extend_from_slice(*name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 != 0 {
            bytes.push(0);
        }
    }
    let length = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes
}

#[test]
fn wav_rate_reads_fmt_and_skips_padded_unknown_chunks_without_hound() {
    let root = TempDir::new();
    let path = root.0.join("rate.wav");
    let mut fmt = [0u8; 16];
    fmt[0..2].copy_from_slice(&1u16.to_le_bytes());
    fmt[2..4].copy_from_slice(&2u16.to_le_bytes());
    fmt[4..8].copy_from_slice(&22050u32.to_le_bytes());
    fmt[8..12].copy_from_slice(&(22050u32 * 4).to_le_bytes());
    fmt[12..14].copy_from_slice(&4u16.to_le_bytes());
    fmt[14..16].copy_from_slice(&16u16.to_le_bytes());
    fs::write(
        &path,
        riff(&[(b"JUNK", &[1, 2, 3]), (b"fmt ", &fmt), (b"data", &[0; 4])]),
    )
    .unwrap();
    assert_eq!(wav_sample_rate(&path), Some(22050));
    fmt[4..8].copy_from_slice(&48000u32.to_le_bytes());
    fs::write(&path, riff(&[(b"fmt ", &fmt)])).unwrap();
    assert_eq!(wav_sample_rate(&path), Some(48000));
    assert_eq!(wav_sample_rate(&root.0.join("absent")), None);
    for bytes in [
        b"not a wave".to_vec(),
        riff(&[(b"data", &[0; 4])]),
        riff(&[(b"fmt ", &[0; 7])]),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(wav_sample_rate(&path), None);
    }
    let mut truncated = riff(&[(b"fmt ", &fmt)]);
    truncated.pop();
    fs::write(&path, truncated).unwrap();
    assert_eq!(wav_sample_rate(&path), None);
    let mut invalid = riff(&[(b"fmt ", &fmt)]);
    invalid[0..4].copy_from_slice(b"RIFX");
    fs::write(&path, invalid).unwrap();
    assert_eq!(wav_sample_rate(&path), None);
}
