use rjdjcore::archive::{
    delete_owned_tree, import_scene, require_owned, MAX_ARCHIVE_BYTES, MAX_ENTRIES,
    MAX_ENTRY_BYTES, MAX_TOTAL_BYTES,
};
use rjdjcore::error::CoreError;
use rjdjcore::plist::{read_info, MAX_INFO_BYTES};
use rjdjcore::scenes::{recover_scenes, recover_scenes_with, seed_default};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const INFO: &str = "<?xml version=\"1.0\"?><!DOCTYPE plist PUBLIC \"Apple\" \"http://127.0.0.1:1/never-load.dtd\"><plist><dict><key>info</key><dict><key>name</key><string>Test &amp; scene</string><key>author</key><string>Artist</string><key>description</key><string>Text</string></dict></dict></plist>";
const ONE_SCENE: &str = "Archive must contain exactly one .rj scene";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Work(PathBuf);
impl Work {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "rjdj-archive-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create isolated test directory: {e}"),
            }
        }
    }
    fn root(&self) -> PathBuf {
        self.0.join("scenes")
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("clean isolated test directory");
    }
}

fn options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
}
fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(*name, options()).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
fn valid() -> Vec<u8> {
    archive(&[
        ("wrapper/Test.rj/_main.pd", b"patch"),
        ("wrapper/Test.rj/Info.plist", INFO.as_bytes()),
    ])
}
fn scene(root: &Path, name: &str, info: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("_main.pd"), b"patch").unwrap();
    fs::write(dir.join("Info.plist"), info).unwrap();
    dir
}
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
fn rejected(bytes: &[u8], expected: &str) {
    let work = Work::new();
    let root = work.root();
    fs::create_dir(&root).unwrap();
    let kept = scene(&root, "Kept.rj", INFO);
    assert_eq!(import_scene(bytes, &root).unwrap_err().message, expected);
    assert_eq!(names(&root), vec!["Kept.rj"]);
    assert_eq!(fs::read(kept.join("_main.pd")).unwrap(), b"patch");
}
fn metadata(xml: &[u8]) -> (Work, PathBuf) {
    let work = Work::new();
    let dir = scene(&work.root(), "Metadata.rj", "");
    fs::write(dir.join("Info.plist"), xml).unwrap();
    (work, dir)
}
fn value(info: &[(String, String)], key: &str) -> String {
    info.iter().find(|(k, _)| k == key).unwrap().1.clone()
}

#[test]
fn java_single_wrapped_scene_is_recognized_and_source_preserved() {
    let work = Work::new();
    let original = work.0.join("source.rjz");
    let bytes = valid();
    fs::write(&original, &bytes).unwrap();
    let root = work.root();
    let imported = import_scene(File::open(&original).unwrap(), &root).unwrap();
    assert_eq!(imported, root.join("Test.rj"));
    assert_eq!(fs::read(original).unwrap(), bytes);
    assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
    assert_eq!(names(&root), vec!["Test.rj"]);
}

#[test]
fn java_nested_metadata_decodes_amp_and_accepts_inert_external_dtd() {
    let (_work, dir) = metadata(INFO.as_bytes());
    let info = read_info(&dir).unwrap();
    assert_eq!(value(&info, "name"), "Test & scene");
    assert_eq!(value(&info, "author"), "Artist");
    assert_eq!(value(&info, "description"), "Text");
}

#[test]
fn java_collision_preserves_existing_scene_and_patch() {
    let work = Work::new();
    let root = work.root();
    let existing = import_scene(valid().as_slice(), &root).unwrap();
    assert_eq!(
        import_scene(valid().as_slice(), &root).unwrap_err().message,
        "A scene with this folder name already exists"
    );
    assert_eq!(fs::read(existing.join("_main.pd")).unwrap(), b"patch");
    assert_eq!(names(&root), vec!["Test.rj"]);
}

#[test]
fn java_unsafe_parent_traversal_paths_are_rejected_and_staging_cleaned() {
    for name in ["../escape", "a/../escape", "a/../../escape"] {
        rejected(&archive(&[(name, b"bad")]), "Invalid archive path");
    }
}
#[test]
fn java_unsafe_absolute_backslash_colon_and_dot_paths_are_rejected() {
    for name in [
        "/absolute",
        "a\\escape",
        "C:/escape",
        "a:escape",
        "a/./escape",
        "./escape",
    ] {
        rejected(&archive(&[(name, b"bad")]), "Invalid archive path");
    }
}
#[test]
fn java_paths_over_1024_utf16_characters_or_32_components_are_rejected() {
    for name in ["a".repeat(1025), "a/".repeat(32) + "file", "😀".repeat(513)] {
        rejected(&archive(&[(&name, b"bad")]), "Archive path is too deep");
    }
}
#[test]
fn java_empty_path_is_outside_private_storage() {
    rejected(&archive(&[("", b"")]), "Path is outside private storage");
}
#[test]
fn java_missing_main_patch_is_rejected() {
    rejected(
        &archive(&[("A.rj/Info.plist", INFO.as_bytes())]),
        "Scene is missing _main.pd",
    );
}
#[test]
fn java_missing_metadata_is_rejected_and_staging_cleaned() {
    let work = Work::new();
    let root = work.root();
    fs::create_dir(&root).unwrap();
    let error =
        import_scene(archive(&[("A.rj/_main.pd", b"patch")]).as_slice(), &root).unwrap_err();
    assert!(
        error
            .message
            .ends_with("/A.rj/Info.plist (No such file or directory)"),
        "{}",
        error.message
    );
    assert!(names(&root).is_empty());
}
#[test]
fn java_zero_multiple_nested_or_case_mismatched_scene_directories_are_rejected() {
    for entries in [
        vec![
            ("plain/_main.pd", &b"patch"[..]),
            ("plain/Info.plist", INFO.as_bytes()),
        ],
        vec![
            ("A.rj/_main.pd", &b"p"[..]),
            ("A.rj/Info.plist", INFO.as_bytes()),
            ("B.rj/_main.pd", &b"p"[..]),
            ("B.rj/Info.plist", INFO.as_bytes()),
        ],
        vec![
            ("A.rj/B.rj/_main.pd", &b"p"[..]),
            ("A.rj/Info.plist", INFO.as_bytes()),
        ],
        vec![
            ("A.RJ/_main.pd", &b"p"[..]),
            ("A.RJ/Info.plist", INFO.as_bytes()),
        ],
    ] {
        rejected(&archive(&entries), ONE_SCENE);
    }
}
#[test]
fn java_internal_external_and_expanding_entity_declarations_are_rejected() {
    for entity in ["SYSTEM 'file:///etc/passwd'", "'expand'"] {
        let xml = format!("<!DOCTYPE plist [<!ENTITY a {entity}>]><plist><key>name</key><string>&a;</string></plist>");
        rejected(
            &archive(&[("A.rj/_main.pd", b"p"), ("A.rj/Info.plist", xml.as_bytes())]),
            "Unsupported scene metadata entities or encoding",
        );
    }
}
#[test]
fn java_non_archive_is_rejected_and_staging_cleaned() {
    rejected(&[1, 2, 3], ONE_SCENE);
}
#[test]
fn java_directory_with_payload_is_rejected() {
    rejected(
        &archive(&[("A.rj/", b"unexpected")]),
        "Directory entry contains data",
    );
}
#[test]
fn java_max_entries_plus_one_is_rejected_and_staging_cleaned() {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..=MAX_ENTRIES {
        zip.start_file(format!("A.rj/{i}"), options()).unwrap();
    }
    rejected(
        &zip.finish().unwrap().into_inner(),
        "Archive contains too many entries",
    );
}
#[test]
fn java_max_entries_is_accepted() {
    let work = Work::new();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("A.rj/_main.pd", options()).unwrap();
    zip.start_file("A.rj/Info.plist", options()).unwrap();
    zip.write_all(INFO.as_bytes()).unwrap();
    for i in 2..MAX_ENTRIES {
        zip.start_file(format!("A.rj/{i}"), options()).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    assert_eq!(
        import_scene(bytes.as_slice(), &work.root()).unwrap(),
        work.root().join("A.rj")
    );
}
fn large_archive(entries: usize, size: u64) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..entries {
        zip.start_file(format!("A.rj/large{i}"), options()).unwrap();
        io::copy(&mut io::repeat(0).take(size), &mut zip).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
#[test]
fn java_per_entry_decompressed_limit_is_enforced() {
    rejected(
        &large_archive(1, MAX_ENTRY_BYTES + 1),
        "Archive exceeds scene size limits",
    );
}
#[test]
fn java_total_decompressed_limit_is_enforced_below_per_entry_limit() {
    rejected(
        &large_archive(5, MAX_TOTAL_BYTES / 5 + 1),
        "Archive exceeds scene size limits",
    );
}
#[test]
fn java_duplicate_canonical_paths_are_rejected() {
    rejected(
        &archive(&[("A.rj/file", b"first"), ("A.rj//file", b"second")]),
        "Duplicate archive entry",
    );
    rejected(
        &archive(&[("A.rj/", b""), ("A.rj//", b"")]),
        "Duplicate archive entry",
    );
}
#[test]
fn java_duplicate_identical_zip_names_are_not_hidden_by_ziparchive() {
    let mut bytes = archive(&[("A.rj/one", b"first"), ("A.rj/two", b"second")]);
    for i in 0..bytes.len() - 8 {
        if &bytes[i..i + 8] == b"A.rj/two" {
            bytes[i..i + 8].copy_from_slice(b"A.rj/one");
        }
    }
    rejected(&bytes, "Duplicate archive entry");
}
#[test]
fn java_file_directory_collisions_are_rejected() {
    rejected(
        &archive(&[("A.rj/file/child", b"p"), ("A.rj/file", b"p")]),
        "Archive path collision",
    );
    rejected(
        &archive(&[("A.rj/file", b"p"), ("A.rj/file/child", b"p")]),
        "Cannot create scene directory",
    );
    rejected(
        &archive(&[("A.rj/file", b"p"), ("A.rj/file/", b"")]),
        "Duplicate archive entry",
    );
}
#[test]
fn java_explicit_directory_after_implicit_directory_is_accepted() {
    let work = Work::new();
    let bytes = archive(&[
        ("A.rj/_main.pd", b"p"),
        ("A.rj/", b""),
        ("A.rj/Info.plist", INFO.as_bytes()),
    ]);
    assert!(import_scene(bytes.as_slice(), &work.root())
        .unwrap()
        .is_dir());
}
#[test]
fn java_outside_and_root_deletion_are_denied_and_owned_deletion_preserves_originals() {
    let work = Work::new();
    let root = work.root();
    let original = work.0.join("source.rjz");
    fs::write(&original, valid()).unwrap();
    let imported = import_scene(File::open(&original).unwrap(), &root).unwrap();
    let outside = work.0.join("outside");
    fs::write(&outside, [7]).unwrap();
    for target in [&outside, &root] {
        assert_eq!(
            require_owned(&root, target).unwrap_err().message,
            "Path is outside private storage"
        );
        assert_eq!(
            delete_owned_tree(&root, target).unwrap_err().message,
            "Path is outside private storage"
        );
        assert!(target.exists());
    }
    delete_owned_tree(&root, &imported).unwrap();
    assert!(!imported.exists());
    assert!(original.exists());
    assert_eq!(fs::read(outside).unwrap(), [7]);
}
#[test]
fn java_owned_missing_child_deletion_is_a_noop() {
    let work = Work::new();
    fs::create_dir(work.root()).unwrap();
    delete_owned_tree(&work.root(), &work.root().join("missing/tree")).unwrap();
    assert!(names(&work.root()).is_empty());
}
#[cfg(unix)]
#[test]
fn java_symlink_deletion_and_symlink_ancestor_paths_are_refused() {
    use std::os::unix::fs::symlink;
    let work = Work::new();
    let root = work.root();
    let imported = scene(&root, "A.rj", INFO);
    let outside = work.0.join("outside");
    fs::write(&outside, b"safe").unwrap();
    let link = imported.join("external");
    symlink(&outside, &link).unwrap();
    assert_eq!(
        delete_owned_tree(&root, &link).unwrap_err().message,
        "Path is outside private storage"
    );
    fs::remove_file(&link).unwrap();
    let internal = root.join("linked");
    symlink(&imported, &internal).unwrap();
    for target in [
        internal.clone(),
        internal.join("_main.pd"),
        internal.join("missing/tail"),
    ] {
        assert_eq!(
            require_owned(&root, &target).unwrap_err().message,
            "Symbolic links are not scene files"
        );
        assert_eq!(
            delete_owned_tree(&root, &target).unwrap_err().message,
            "Symbolic links are not scene files"
        );
    }
    assert_eq!(fs::read(outside).unwrap(), b"safe");
    assert!(imported.join("_main.pd").is_file());
}
#[cfg(unix)]
#[test]
fn java_owned_tree_deletion_never_follows_nested_symlinks() {
    use std::os::unix::fs::symlink;
    let work = Work::new();
    let root = work.root();
    let imported = scene(&root, "A.rj", INFO);
    let outside = work.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"safe").unwrap();
    symlink(&outside, imported.join("link")).unwrap();
    assert_eq!(
        delete_owned_tree(&root, &imported).unwrap_err().message,
        "Path is outside private storage"
    );
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"safe");
    assert!(imported.join("link").is_symlink());
}
#[test]
fn java_bundled_atsuke_imports_with_name_atsuke() {
    let work = Work::new();
    let zip = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ScenePlayer/res/raw/atsuke.zip");
    let imported = import_scene(File::open(zip).unwrap(), &work.root()).unwrap();
    assert_eq!(value(&read_info(&imported).unwrap(), "name"), "Atsuke");
    assert_eq!(imported.file_name().unwrap(), "Atsuke.rj");
}
#[test]
fn java_streamed_zipoutputstream_data_descriptors_are_accepted_from_read_only_input() {
    let work = Work::new();
    let mut zip = ZipWriter::new_stream(Vec::new());
    zip.start_file("A.rj/_main.pd", options()).unwrap();
    zip.write_all(b"patch").unwrap();
    zip.start_file("A.rj/Info.plist", options()).unwrap();
    zip.write_all(INFO.as_bytes()).unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    struct Stream<'a>(&'a [u8]);
    impl Read for Stream<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(7);
            self.0.read(&mut out[..n])
        }
    }
    let imported = import_scene(Stream(&bytes), &work.root()).unwrap();
    assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
}

#[test]
fn java_metadata_last_value_wins_and_non_scalar_start_clears_pending_key() {
    let xml = b"<plist><dict><key>name</key><string>first</string><key>name</key><string> last </string><key>author</key><dict><key>category</key><string>Reality</string><key>sceneId</key><integer> 42 </integer></dict><key>ignored</key><true/><string>not saved</string></dict></plist>";
    let (_work, dir) = metadata(xml);
    let info = read_info(&dir).unwrap();
    assert_eq!(value(&info, "name"), "last");
    assert_eq!(value(&info, "author"), "Unknown artist");
    assert_eq!(value(&info, "description"), "");
    assert_eq!(value(&info, "category"), "Reality");
    assert_eq!(value(&info, "sceneId"), "42");
    assert!(!info.iter().any(|(k, _)| k == "info" || k == "ignored"));
}
#[test]
fn java_metadata_missing_and_empty_name_are_rejected() {
    for xml in [
        "<plist><dict/></plist>",
        "<plist><key>name</key><string> \n\t </string></plist>",
        "<plist><key>name</key><string/></plist>",
    ] {
        let (_work, dir) = metadata(xml.as_bytes());
        assert_eq!(
            read_info(&dir).unwrap_err().message,
            "Scene metadata is missing its name"
        );
    }
}
#[test]
fn java_metadata_present_empty_author_is_not_replaced_and_trim_is_java_trim() {
    let (_work, dir) = metadata(
        "<plist><key>name</key><string>\u{a0}name\u{a0}</string><key>author</key><string/></plist>"
            .as_bytes(),
    );
    let info = read_info(&dir).unwrap();
    assert_eq!(value(&info, "name"), "\u{a0}name\u{a0}");
    assert_eq!(value(&info, "author"), "");
}
#[test]
fn java_metadata_accepts_utf8_cdata_numeric_entities_and_local_names() {
    let (_work, dir) = metadata("<p:plist xmlns:p=\"urn:test\"><p:key>name</p:key><p:string> Café <![CDATA[& raw]]> &#x1f3b5; </p:string></p:plist>".as_bytes());
    assert_eq!(value(&read_info(&dir).unwrap(), "name"), "Café & raw 🎵");
}
#[test]
fn java_metadata_nul_and_entity_declarations_are_rejected_before_parsing() {
    for xml in [
        &b"<plist>\0</plist>"[..],
        &b"<plist><!-- <!ENTITY forbidden> --></plist>"[..],
    ] {
        let (_work, dir) = metadata(xml);
        assert_eq!(
            read_info(&dir).unwrap_err().message,
            "Unsupported scene metadata entities or encoding"
        );
    }
}
#[test]
fn java_metadata_benign_internal_subset_is_accepted_without_loading_external_entities() {
    let (_work, dir) = metadata(b"<!DOCTYPE plist [<!ELEMENT plist ANY><!ATTLIST plist version CDATA '1.0'>]><plist><key>name</key><string>Test</string></plist>");
    assert_eq!(value(&read_info(&dir).unwrap(), "name"), "Test");
}
#[test]
fn java_metadata_malformed_xml_is_rejected() {
    for xml in [
        "<plist><key>name</key><string>Test</plist>",
        "<plist><key>name</key><string>Test</string>",
        "<plist/><plist/>",
        "<plist><key>name</key><string>&undefined;</string></plist>",
    ] {
        let (_work, dir) = metadata(xml.as_bytes());
        assert_eq!(
            read_info(&dir).unwrap_err().message,
            "Invalid scene metadata"
        );
    }
}
#[test]
fn java_metadata_over_one_mib_is_rejected_but_exact_limit_is_accepted() {
    let xml = "<plist><key>name</key><string>Test</string></plist>";
    let mut bytes = xml.as_bytes().to_vec();
    bytes.resize(MAX_INFO_BYTES, b' ');
    let (_work, dir) = metadata(&bytes);
    assert_eq!(value(&read_info(&dir).unwrap(), "name"), "Test");
    bytes.push(b' ');
    fs::write(dir.join("Info.plist"), bytes).unwrap();
    assert_eq!(
        read_info(&dir).unwrap_err().message,
        "Scene metadata is too large"
    );
}
#[test]
fn java_recovery_registers_published_scene_and_is_idempotent() {
    let work = Work::new();
    let root = work.root();
    let dir = scene(&root, "Recovered.rj", INFO);
    assert_eq!(recover_scenes(&root, &[]).unwrap(), vec![dir.clone()]);
    assert!(recover_scenes(&root, &["Recovered.rj".into()])
        .unwrap()
        .is_empty());
    assert_eq!(fs::read(dir.join("_main.pd")).unwrap(), b"patch");
}
#[test]
fn java_recovery_removes_abandoned_import_files_and_directories_only() {
    let work = Work::new();
    let root = work.root();
    fs::create_dir_all(root.join(".import-dead/nested")).unwrap();
    fs::write(root.join(".import-dead/nested/unfinished"), [1]).unwrap();
    fs::write(root.join(".import-file"), [1]).unwrap();
    fs::create_dir(root.join("unrelated")).unwrap();
    fs::write(root.join("keep"), [2]).unwrap();
    assert!(recover_scenes(&root, &[]).unwrap().is_empty());
    assert_eq!(names(&root), vec!["keep", "unrelated"]);
}
#[test]
fn java_damaged_recovery_candidate_is_preserved_and_valid_candidates_still_register() {
    let work = Work::new();
    let root = work.root();
    let damaged = scene(&root, "A-damaged.rj", INFO);
    fs::remove_file(damaged.join("_main.pd")).unwrap();
    let good = scene(&root, "Z-good.rj", INFO);
    let mut registered = Vec::new();
    let error = recover_scenes_with(&root, &[], |path| {
        registered.push(path.to_path_buf());
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.message, "Scene is missing _main.pd");
    assert_eq!(registered, vec![good]);
    assert!(damaged.is_dir());
    assert!(damaged.join("Info.plist").is_file());
}
#[test]
fn java_recovery_returns_first_error_and_suppresses_later_errors_after_continuing() {
    let work = Work::new();
    let root = work.root();
    scene(&root, "A.rj", INFO);
    scene(&root, "B.rj", INFO);
    let good = scene(&root, "C.rj", INFO);
    let mut registered = Vec::new();
    let error = recover_scenes_with(&root, &[], |path| {
        match path.file_name().unwrap().to_str().unwrap() {
            "A.rj" => Err(CoreError::new("Cannot save scene in library")),
            "B.rj" => Err(CoreError::new("Second failure")),
            _ => {
                registered.push(path.to_path_buf());
                Ok(())
            }
        }
    })
    .unwrap_err();
    assert_eq!(error.message, "Cannot save scene in library");
    assert_eq!(error.suppressed, vec!["Second failure"]);
    assert_eq!(registered, vec![good]);
    assert_eq!(names(&root), vec!["A.rj", "B.rj", "C.rj"]);
}
#[test]
fn java_registered_partial_scene_does_not_block_recovery() {
    let work = Work::new();
    let root = work.root();
    let registered = scene(&root, "Registered.rj", INFO);
    fs::remove_file(registered.join("_main.pd")).unwrap();
    fs::remove_file(registered.join("Info.plist")).unwrap();
    let good = scene(&root, "Good.rj", INFO);
    assert_eq!(
        recover_scenes(&root, &["Registered.rj".into()]).unwrap(),
        vec![good]
    );
    assert!(registered.is_dir());
}
#[cfg(unix)]
#[test]
fn java_recovery_refuses_symlink_metadata_and_preserves_damaged_folder() {
    use std::os::unix::fs::symlink;
    let work = Work::new();
    let root = work.root();
    let damaged = scene(&root, "Damaged.rj", INFO);
    fs::rename(damaged.join("Info.plist"), damaged.join("real.plist")).unwrap();
    symlink(damaged.join("real.plist"), damaged.join("Info.plist")).unwrap();
    assert_eq!(
        recover_scenes(&root, &[]).unwrap_err().message,
        "Symbolic links are not scene files"
    );
    assert!(damaged.join("Info.plist").is_symlink());
}
#[test]
fn java_recovery_missing_storage_reports_exact_inspection_error() {
    let work = Work::new();
    assert_eq!(
        recover_scenes(&work.root(), &[]).unwrap_err().message,
        "Cannot inspect scene storage"
    );
}
#[test]
fn java_default_seed_is_once_and_deletion_does_not_reset_the_flag() {
    let work = Work::new();
    let zip = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ScenePlayer/res/raw/atsuke.zip");
    let seeded = seed_default(&work.root(), File::open(&zip).unwrap(), false)
        .unwrap()
        .unwrap();
    assert_eq!(value(&read_info(&seeded).unwrap(), "name"), "Atsuke");
    assert_eq!(
        seed_default(&work.root(), &b"not read"[..], true).unwrap(),
        None
    );
    delete_owned_tree(&work.root(), &seeded).unwrap();
    assert_eq!(
        seed_default(&work.root(), &b"not read"[..], true).unwrap(),
        None
    );
    assert!(names(&work.root()).is_empty());
}
#[test]
fn java_default_seed_reuses_published_atsuke_folder_without_replacing_it() {
    let work = Work::new();
    let root = work.root();
    let existing = scene(&root, "Atsuke.rj", INFO);
    assert_eq!(
        seed_default(&root, &b"not an archive"[..], false).unwrap(),
        Some(existing.clone())
    );
    assert_eq!(fs::read(existing.join("_main.pd")).unwrap(), b"patch");
    assert_eq!(
        value(&read_info(&existing).unwrap(), "name"),
        "Test & scene"
    );
}
#[test]
fn java_default_seed_existing_file_is_not_replaced() {
    let work = Work::new();
    let root = work.root();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("Atsuke.rj"), b"keep").unwrap();
    let zip = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ScenePlayer/res/raw/atsuke.zip");
    assert_eq!(
        seed_default(&root, File::open(zip).unwrap(), false)
            .unwrap_err()
            .message,
        "A scene with this folder name already exists"
    );
    assert_eq!(fs::read(root.join("Atsuke.rj")).unwrap(), b"keep");
}

#[test]
fn java_entry_and_total_size_limits_are_inclusive() {
    let work = Work::new();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("A.rj/_main.pd", options()).unwrap();
    zip.start_file("A.rj/Info.plist", options()).unwrap();
    zip.write_all(INFO.as_bytes()).unwrap();
    for i in 0..4 {
        let size = MAX_ENTRY_BYTES - if i == 3 { INFO.len() as u64 } else { 0 };
        zip.start_file(format!("A.rj/large{i}"), options()).unwrap();
        io::copy(&mut io::repeat(0).take(size), &mut zip).unwrap();
    }
    let bytes = zip.finish().unwrap().into_inner();
    let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
    assert_eq!(
        fs::metadata(imported.join("large0")).unwrap().len(),
        MAX_ENTRY_BYTES
    );
    let total: u64 = fs::read_dir(&imported)
        .unwrap()
        .map(|file| file.unwrap().metadata().unwrap().len())
        .sum();
    assert_eq!(total, MAX_TOTAL_BYTES);
    assert_eq!(names(&work.root()), vec!["A.rj"]);
}

#[test]
fn java_32_path_components_are_accepted_including_trailing_directory_slashes() {
    let work = Work::new();
    let folder = "wrapper/".repeat(30) + "Depth.rj";
    let main = format!("{folder}/_main.pd");
    let info = format!("{folder}/Info.plist");
    let directory = format!("{folder}///");
    let bytes = archive(&[
        (&directory, b""),
        (&main, b"patch"),
        (&info, INFO.as_bytes()),
    ]);
    assert_eq!(
        import_scene(bytes.as_slice(), &work.root()).unwrap(),
        work.root().join("Depth.rj")
    );
}

#[test]
fn java_default_seed_reuses_a_damaged_existing_directory_for_registration_to_validate() {
    let work = Work::new();
    let root = work.root();
    let sample = root.join("Atsuke.rj");
    fs::create_dir_all(&sample).unwrap();
    assert_eq!(
        seed_default(&root, &b"not an archive"[..], false).unwrap(),
        Some(sample.clone())
    );
    assert!(sample.is_dir());
    assert!(!sample.join("Info.plist").exists());
}

#[test]
// macOS PATH_MAX is 1024, so staging prefix + a 1024-char entry cannot exist there; Android/Linux allow 4096.
#[cfg_attr(
    target_os = "macos",
    ignore = "host PATH_MAX 1024 cannot hold the staging prefix plus a 1024-char entry"
)]
fn java_1024_path_characters_are_accepted_without_truncating_components() {
    let work = Work::new();
    let wrapper = "w".repeat(250) + "/";
    let folder = wrapper.repeat(4) + "Limits.rj";
    let main = format!("{folder}/_main.pd");
    let info = format!("{folder}/Info.plist");
    assert_eq!(info.encode_utf16().count(), 1024);
    let bytes = archive(&[(&main, b"patch"), (&info, INFO.as_bytes())]);
    assert_eq!(
        import_scene(bytes.as_slice(), &work.root()).unwrap(),
        work.root().join("Limits.rj")
    );
}

#[test]
fn java_cannot_create_scene_storage_preserves_existing_file() {
    let work = Work::new();
    let root = work.root();
    fs::write(&root, b"keep").unwrap();
    assert_eq!(
        import_scene(valid().as_slice(), &root).unwrap_err().message,
        "Cannot create scene storage"
    );
    assert_eq!(fs::read(root).unwrap(), b"keep");
}

#[test]
fn java_document_provider_read_failure_cleans_staging_and_preserves_existing_scenes() {
    struct BrokenProvider;
    impl Read for BrokenProvider {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("provider read failed"))
        }
    }
    let work = Work::new();
    let root = work.root();
    let kept = scene(&root, "Kept.rj", INFO);
    assert_eq!(
        import_scene(BrokenProvider, &root).unwrap_err().message,
        "provider read failed"
    );
    assert_eq!(names(&root), vec!["Kept.rj"]);
    assert_eq!(fs::read(kept.join("_main.pd")).unwrap(), b"patch");
}

#[cfg(unix)]
#[test]
fn java_recovery_never_follows_abandoned_staging_symlinks_and_keeps_scanning() {
    use std::os::unix::fs::symlink;
    let work = Work::new();
    let root = work.root();
    let good = scene(&root, "Good.rj", INFO);
    let outside = work.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"safe").unwrap();
    let staging = root.join(".import-link");
    symlink(&outside, &staging).unwrap();
    let mut registered = Vec::new();
    let error = recover_scenes_with(&root, &[], |path| {
        registered.push(path.to_path_buf());
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.message, "Path is outside private storage");
    assert_eq!(registered, vec![good]);
    assert!(staging.is_symlink());
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"safe");
}

#[test]
fn java_recovery_preserves_invalid_metadata_and_registers_other_candidates() {
    let work = Work::new();
    let root = work.root();
    let bad = scene(&root, "Bad.rj", "<plist><dict/></plist>");
    let good = scene(&root, "Good.rj", INFO);
    let mut registered = Vec::new();
    let error = recover_scenes_with(&root, &[], |path| {
        registered.push(path.to_path_buf());
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.message, "Scene metadata is missing its name");
    assert_eq!(registered, vec![good]);
    assert_eq!(
        fs::read_to_string(bad.join("Info.plist")).unwrap(),
        "<plist><dict/></plist>"
    );
    assert_eq!(fs::read(bad.join("_main.pd")).unwrap(), b"patch");
}

/// Endless zero stream: counts how many bytes the importer actually pulled.
struct Endless<'a>(&'a AtomicU64);

impl Read for Endless<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        buf.fill(0);
        self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }
}

#[test]
fn oversize_document_stops_reading_at_the_raw_cap_and_removes_staging() {
    let work = Work::new();
    let pulled = AtomicU64::new(0);
    let error = import_scene(Endless(&pulled), &work.root()).unwrap_err();
    assert_eq!(error.message, "Archive is too large to import");
    let pulled = pulled.load(Ordering::Relaxed);
    assert!(
        pulled > MAX_ARCHIVE_BYTES,
        "must read past the cap to detect oversize: {pulled}"
    );
    // io::copy reads in bounded chunks, so the overshoot is at most one buffer.
    assert!(
        pulled <= MAX_ARCHIVE_BYTES + 1 + 1024 * 1024,
        "kept reading after the cap: {pulled}"
    );
    assert!(
        fs::read_dir(work.root()).unwrap().next().is_none(),
        "staging (spool) must be removed after rejection"
    );
}
