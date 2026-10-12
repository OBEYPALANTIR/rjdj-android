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
fn append_zip64_footer(bytes: &mut Vec<u8>, classic: &[u8], extension: u64) {
    let record_position = bytes.len() as u64;
    let mut record = [0; 56];
    record[..4].copy_from_slice(b"PK\x06\x06");
    record[4..12].copy_from_slice(&(44u64 + extension).to_le_bytes());
    record[12..14].copy_from_slice(&45u16.to_le_bytes());
    record[14..16].copy_from_slice(&45u16.to_le_bytes());
    let on_disk = u16::from_le_bytes(classic[8..10].try_into().unwrap());
    let total = u16::from_le_bytes(classic[10..12].try_into().unwrap());
    let size = u32::from_le_bytes(classic[12..16].try_into().unwrap());
    let directory = u32::from_le_bytes(classic[16..20].try_into().unwrap());
    record[24..32].copy_from_slice(&u64::from(on_disk).to_le_bytes());
    record[32..40].copy_from_slice(&u64::from(total).to_le_bytes());
    record[40..48].copy_from_slice(&u64::from(size).to_le_bytes());
    record[48..56].copy_from_slice(&u64::from(directory).to_le_bytes());
    bytes.extend_from_slice(&record);
    bytes.resize(bytes.len() + extension as usize, 0);
    let mut locator = [0; 20];
    locator[..4].copy_from_slice(b"PK\x06\x07");
    locator[8..16].copy_from_slice(&record_position.to_le_bytes());
    locator[16..20].copy_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&locator);
}
fn directory_signed_archive(signature: &[u8], zip64: bool) -> Vec<u8> {
    let mut bytes = archive(&[
        ("Test.rj/_main.pd", b"patch"),
        ("Test.rj/Info.plist", INFO.as_bytes()),
        ("Test.rj/sample.wav", b"required sample"),
    ]);
    let mut classic = bytes.split_off(bytes.len() - 22);
    let size = u32::from_le_bytes(classic[12..16].try_into().unwrap());
    bytes.extend_from_slice(b"PK\x05\x05");
    bytes.extend_from_slice(&u16::try_from(signature.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(signature);
    classic[12..16].copy_from_slice(&(size + 6 + signature.len() as u32).to_le_bytes());
    if zip64 {
        append_zip64_footer(&mut bytes, &classic, 0);
        classic[8..20].fill(0xff);
    }
    bytes.extend_from_slice(&classic);
    bytes
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
fn classic_declared_entry_count_is_rejected_before_central_records_are_read() {
    for field in [8, 10] {
        let mut bytes = [0; 26];
        bytes[..4].copy_from_slice(b"PK\x01\x02");
        let footer = &mut bytes[4..];
        footer[..4].copy_from_slice(b"PK\x05\x06");
        footer[field..field + 2]
            .copy_from_slice(&((MAX_ENTRIES + 1) as u16).to_le_bytes());
        // Only the first central signature exists; no entry can be parsed.
        rejected(&bytes, "Archive contains too many entries");
    }
}
#[test]
fn zip64_declared_entry_count_is_rejected_before_central_records_are_read() {
    for classic_count in [0, u16::MAX] {
        let record_position = 46 * (MAX_ENTRIES + 1);
        let mut bytes = vec![0; record_position + 98];
        bytes[..4].copy_from_slice(b"PK\x01\x02");
        let record = &mut bytes[record_position..record_position + 56];
        record[..4].copy_from_slice(b"PK\x06\x06");
        record[4..12].copy_from_slice(&44u64.to_le_bytes());
        record[24..32].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
        record[32..40].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
        let locator = &mut bytes[record_position + 56..record_position + 76];
        locator[..4].copy_from_slice(b"PK\x06\x07");
        locator[8..16].copy_from_slice(&(record_position as u64).to_le_bytes());
        locator[16..20].copy_from_slice(&1u32.to_le_bytes());
        let footer = &mut bytes[record_position + 76..];
        footer[..4].copy_from_slice(b"PK\x05\x06");
        footer[8..10].copy_from_slice(&classic_count.to_le_bytes());
        footer[10..12].copy_from_slice(&classic_count.to_le_bytes());
        footer[12..20].fill(0xff);
        // Consistent ZIP64 counts fit the directory span, but no complete entry exists.
        rejected(&bytes, "Archive contains too many entries");
    }
}
#[test]
fn classic_footer_comment_retries_after_directory_decode_or_boundary_mismatch() {
    for count in [3u16, 2, 1] {
        let work = Work::new();
        let mut bytes = valid();
        let footer = bytes.len() - 22;
        let mut comment = bytes[footer..].to_vec();
        comment[8..10].copy_from_slice(&count.to_le_bytes());
        comment[10..12].copy_from_slice(&count.to_le_bytes());
        bytes[footer + 20..footer + 22].copy_from_slice(&22u16.to_le_bytes());
        bytes.extend_from_slice(&comment);
        let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
        assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
        assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
    }
}
#[test]
fn zip64_footer_comment_count_mismatch_preserves_all_scene_assets() {
    for (on_disk, total) in [(2u64, 1u64), (1, 2), ((MAX_ENTRIES + 1) as u64, 1)] {
        let work = Work::new();
        let mut bytes = archive(&[
            ("Test.rj/_main.pd", b"patch"),
            ("Test.rj/Info.plist", INFO.as_bytes()),
            ("Test.rj/sample.wav", b"required sample"),
        ]);
        let record_position = bytes.len() as u64;
        let footer = bytes.len() - 22;
        let directory = u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap());
        let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
        bytes[footer + 20..footer + 22].copy_from_slice(&98u16.to_le_bytes());
        let mut record = [0; 56];
        record[..4].copy_from_slice(b"PK\x06\x06");
        record[4..12].copy_from_slice(&44u64.to_le_bytes());
        record[24..32].copy_from_slice(&on_disk.to_le_bytes());
        record[32..40].copy_from_slice(&total.to_le_bytes());
        record[40..48].copy_from_slice(&u64::from(size).to_le_bytes());
        record[48..56].copy_from_slice(&u64::from(directory).to_le_bytes());
        bytes.extend_from_slice(&record);
        let mut locator = [0; 20];
        locator[..4].copy_from_slice(b"PK\x06\x07");
        locator[8..16].copy_from_slice(&record_position.to_le_bytes());
        locator[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&locator);
        let mut comment = [0; 22];
        comment[..4].copy_from_slice(b"PK\x05\x06");
        comment[8..20].fill(0xff);
        bytes.extend_from_slice(&comment);
        let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
        assert_eq!(fs::read(imported.join("sample.wav")).unwrap(), b"required sample");
        assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
    }
}
#[test]
fn underdeclared_directory_cannot_hide_the_actual_entry_limit_or_retry_older_footers() {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("Test.rj/_main.pd", options()).unwrap();
    zip.write_all(b"patch").unwrap();
    zip.start_file("Test.rj/Info.plist", options()).unwrap();
    zip.write_all(INFO.as_bytes()).unwrap();
    zip.start_file("Test.rj/sample.wav", options()).unwrap();
    zip.write_all(b"required sample").unwrap();
    for i in 3..=MAX_ENTRIES {
        zip.start_file(format!("Test.rj/{i:08}"), options()).unwrap();
    }
    let archive = zip.finish().unwrap().into_inner();
    for prefix in [Vec::new(), valid()] {
        let mut bytes = archive.clone();
        let footer = bytes.len() - 22;
        let directory = u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap());
        bytes[footer + 8..footer + 10].copy_from_slice(&2u16.to_le_bytes());
        bytes[footer + 10..footer + 12].copy_from_slice(&2u16.to_le_bytes());
        bytes[footer + 16..footer + 20]
            .copy_from_slice(&(directory + prefix.len() as u32).to_le_bytes());
        let mut combined = prefix;
        combined.extend_from_slice(&bytes);
        rejected(&combined, "Archive contains too many entries");
    }
}
#[test]
fn stored_nested_zip_over_outer_entry_limit_is_an_opaque_scene_asset() {
    let work = Work::new();
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut nested = ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..=MAX_ENTRIES {
        nested.start_file(format!("{i:08}"), stored).unwrap();
    }
    let nested = nested.finish().unwrap().into_inner();
    let mut outer = ZipWriter::new(Cursor::new(Vec::new()));
    outer.start_file("Test.rj/_main.pd", options()).unwrap();
    outer.write_all(b"patch").unwrap();
    outer.start_file("Test.rj/Info.plist", options()).unwrap();
    outer.write_all(INFO.as_bytes()).unwrap();
    outer.start_file("Test.rj/assets.zip", stored).unwrap();
    outer.write_all(&nested).unwrap();
    let bytes = outer.finish().unwrap().into_inner();
    let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
    assert_eq!(imported, work.root().join("Test.rj"));
    assert_eq!(fs::read(imported.join("assets.zip")).unwrap(), nested);
}
#[test]
fn zip64_mismatched_large_extensible_sector_is_rejected_before_entry_count() {
    let work = Work::new();
    let root = work.root();
    let kept = scene(&root, "Kept.rj", INFO);
    let extension = 256 * 1024 * 1024;
    let mut record = [0; 56];
    record[..4].copy_from_slice(b"PK\x06\x06");
    record[4..12].copy_from_slice(&(44u64 + extension).to_le_bytes());
    record[24..32].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
    record[32..40].copy_from_slice(&((MAX_ENTRIES + 1) as u64).to_le_bytes());
    let mut trailer = [0; 42];
    trailer[..4].copy_from_slice(b"PK\x06\x07");
    trailer[16..20].copy_from_slice(&1u32.to_le_bytes());
    trailer[20..24].copy_from_slice(b"PK\x05\x06");
    trailer[28..40].fill(0xff);
    // Stream the large gap without allocating it; the locator is one byte past the record end.
    let input = Cursor::new(record)
        .chain(io::repeat(0).take(extension + 1))
        .chain(Cursor::new(trailer));
    assert_eq!(import_scene(input, &root).unwrap_err().message, ONE_SCENE);
    assert_eq!(names(&root), vec!["Kept.rj"]);
    assert_eq!(fs::read(kept.join("_main.pd")).unwrap(), b"patch");
}
#[test]
fn backward_footer_search_retries_invalid_candidates_across_buffer_boundaries() {
    for boundary in 1..=3 {
        let work = Work::new();
        let mut bytes = valid();
        bytes.resize(bytes.len() + 8192 - 44 + boundary, 0);
        let mut later = [0; 22];
        later[..4].copy_from_slice(b"PK\x05\x06");
        later[10..12].copy_from_slice(&((MAX_ENTRIES + 1) as u16).to_le_bytes());
        match boundary {
            1 => later[16..20].copy_from_slice(&u32::MAX.to_le_bytes()),
            2 => later[20..22].copy_from_slice(&u16::MAX.to_le_bytes()),
            _ => later[4..6].copy_from_slice(&1u16.to_le_bytes()),
        }
        bytes.extend_from_slice(&later);
        assert_eq!(
            import_scene(bytes.as_slice(), &work.root()).unwrap(),
            work.root().join("Test.rj")
        );
    }
}
#[test]
fn directory_signatures_preserve_scene_files_before_classic_and_zip64_footers() {
    for zip64 in [false, true] {
        for signature in [&[][..], &b"\0\xffopaque signature"[..]] {
            let work = Work::new();
            let bytes = directory_signed_archive(signature, zip64);
            let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
            assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
            assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
            assert_eq!(fs::read(imported.join("sample.wav")).unwrap(), b"required sample");
        }
    }
}
#[test]
fn directory_signature_lengths_and_single_record_boundary_are_enforced() {
    for length in [1u16, u16::MAX] {
        let mut bytes = directory_signed_archive(b"signature", false);
        let footer = bytes.len() - 22;
        let signature = footer - 6 - b"signature".len();
        bytes[signature + 4..signature + 6].copy_from_slice(&length.to_le_bytes());
        rejected(&bytes, ONE_SCENE);
    }
    let mut bytes = directory_signed_archive(b"", false);
    let footer = bytes.len() - 22;
    let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
    let signature = footer - 6;
    bytes.splice(signature..signature, b"PK\x05\x05\0\0".iter().copied());
    let footer = footer + 6;
    bytes[footer + 12..footer + 16].copy_from_slice(&(size + 6).to_le_bytes());
    rejected(&bytes, ONE_SCENE);
}
#[test]
fn small_zip64_scene_with_fitting_classic_fields_is_accepted() {
    for extension in [0, 256, 65536] {
        let work = Work::new();
        let mut bytes = valid();
        let classic = bytes.split_off(bytes.len() - 22);
        append_zip64_footer(&mut bytes, &classic, extension);
        bytes.extend_from_slice(&classic);
        let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
        assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
        assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
    }
}
#[test]
fn invalid_optional_zip64_locator_in_a_central_comment_keeps_classic_import() {
    let work = Work::new();
    let mut bytes = valid();
    let footer = bytes.len() - 22;
    let directory = u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap()) as usize;
    let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
    let name = u16::from_le_bytes(bytes[directory + 28..directory + 30].try_into().unwrap()) as usize;
    let extra = u16::from_le_bytes(bytes[directory + 30..directory + 32].try_into().unwrap()) as usize;
    let comment = u16::from_le_bytes(bytes[directory + 32..directory + 34].try_into().unwrap()) as usize;
    let last = directory + 46 + name + extra + comment;
    let previous = u16::from_le_bytes(bytes[last + 32..last + 34].try_into().unwrap());
    bytes[last + 32..last + 34].copy_from_slice(&(previous + 20).to_le_bytes());
    let mut locator = [0; 20];
    locator[..4].copy_from_slice(b"PK\x06\x07");
    locator[16..20].copy_from_slice(&1u32.to_le_bytes());
    // Offset zero is a local header, not a ZIP64 record; these are opaque comment bytes.
    bytes.splice(footer..footer, locator);
    let footer = footer + 20;
    bytes[footer + 12..footer + 16].copy_from_slice(&(size + 20).to_le_bytes());
    let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
    assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
    assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
}
#[test]
fn optional_zip64_directory_failure_in_a_central_comment_retries_same_classic_footer() {
    for required in [false, true] {
        let mut bytes = valid();
        let footer = bytes.len() - 22;
        let directory =
            u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
        let name =
            u16::from_le_bytes(bytes[directory + 28..directory + 30].try_into().unwrap()) as usize;
        let extra =
            u16::from_le_bytes(bytes[directory + 30..directory + 32].try_into().unwrap()) as usize;
        let comment =
            u16::from_le_bytes(bytes[directory + 32..directory + 34].try_into().unwrap()) as usize;
        let last = directory + 46 + name + extra + comment;
        let previous = u16::from_le_bytes(bytes[last + 32..last + 34].try_into().unwrap());
        bytes[last + 32..last + 34].copy_from_slice(&(previous + 76).to_le_bytes());
        let mut classic = bytes.split_off(footer);
        // Matching counts/offsets pass fixed checks, but this chain is inside the file comment.
        append_zip64_footer(&mut bytes, &classic, 0);
        classic[12..16].copy_from_slice(&(size + 76).to_le_bytes());
        if required {
            classic[8..20].fill(0xff);
        }
        bytes.extend_from_slice(&classic);
        if required {
            rejected(&bytes, ONE_SCENE);
        } else {
            let work = Work::new();
            let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
            assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
            assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
            assert_eq!(names(&work.root()), vec!["Test.rj"]);
        }
    }
}
#[test]
fn optional_zip64_directory_failure_in_signature_payload_retries_same_classic_footer() {
    let work = Work::new();
    let mut bytes = directory_signed_archive(b"", false);
    let footer = bytes.len() - 22;
    let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
    bytes[footer - 2..footer].copy_from_slice(&76u16.to_le_bytes());
    let mut classic = bytes.split_off(footer);
    append_zip64_footer(&mut bytes, &classic, 0);
    classic[12..16].copy_from_slice(&(size + 76).to_le_bytes());
    bytes.extend_from_slice(&classic);
    let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
    assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
    assert_eq!(fs::read(imported.join("Info.plist")).unwrap(), INFO.as_bytes());
    assert_eq!(fs::read(imported.join("sample.wav")).unwrap(), b"required sample");
    assert_eq!(names(&work.root()), vec!["Test.rj"]);
}
#[test]
fn small_zip64_scene_with_classic_sentinels_and_entry_extras_is_accepted() {
    for extension in [0, 256, 65536] {
        for fields in [&[24, 20, 42][..], &[20][..], &[42][..], &[24][..]] {
            let work = Work::new();
            let original = valid();
            let mut classic = original[original.len() - 22..].to_vec();
            let start = u32::from_le_bytes(classic[16..20].try_into().unwrap()) as usize;
            let mut bytes = original[..start].to_vec();
            let mut position = start;
            for _ in 0..2 {
                let mut header = original[position..position + 46].to_vec();
                let name = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
                let extra = u16::from_le_bytes(header[30..32].try_into().unwrap()) as usize;
                let comment = u16::from_le_bytes(header[32..34].try_into().unwrap()) as usize;
                let mut zip64 = Vec::new();
                for &field in fields {
                    let value = u32::from_le_bytes(header[field..field + 4].try_into().unwrap());
                    zip64.extend_from_slice(&u64::from(value).to_le_bytes());
                    header[field..field + 4].fill(0xff);
                }
                header[6..8].copy_from_slice(&45u16.to_le_bytes());
                header[30..32].copy_from_slice(&((extra + 10 + zip64.len()) as u16).to_le_bytes());
                bytes.extend_from_slice(&header);
                bytes.extend_from_slice(&original[position + 46..position + 46 + name + extra]);
                bytes.extend_from_slice(&[0xfe, 0xca, 2, 0, 0, 0]);
                bytes.extend_from_slice(&1u16.to_le_bytes());
                bytes.extend_from_slice(&(zip64.len() as u16).to_le_bytes());
                bytes.extend_from_slice(&zip64);
                bytes.extend_from_slice(
                    &original[position + 46 + name + extra..position + 46 + name + extra + comment],
                );
                position += 46 + name + extra + comment;
            }
            classic[12..16].copy_from_slice(&((bytes.len() - start) as u32).to_le_bytes());
            append_zip64_footer(&mut bytes, &classic, extension);
            classic[8..20].fill(0xff);
            bytes.extend_from_slice(&classic);
            let imported = import_scene(bytes.as_slice(), &work.root()).unwrap();
            assert_eq!(imported, work.root().join("Test.rj"));
            assert_eq!(fs::read(imported.join("_main.pd")).unwrap(), b"patch");
        }
    }
}
#[test]
fn central_zip64_sentinels_require_complete_extra_fields() {
    for extra in [
        &[][..],
        &[1, 0, 0, 0][..],
        &[1, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0][..],
        &[1, 0, 8, 0][..],
    ] {
        let mut bytes = valid();
        let footer = bytes.len() - 22;
        let start = u32::from_le_bytes(bytes[footer + 16..footer + 20].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(bytes[footer + 12..footer + 16].try_into().unwrap());
        let name = u16::from_le_bytes(bytes[start + 28..start + 30].try_into().unwrap()) as usize;
        let previous = u16::from_le_bytes(bytes[start + 30..start + 32].try_into().unwrap());
        bytes[start + 20..start + 24].fill(0xff);
        bytes[start + 30..start + 32]
            .copy_from_slice(&(previous + extra.len() as u16).to_le_bytes());
        let insertion = start + 46 + name;
        bytes.splice(insertion..insertion, extra.iter().copied());
        let footer = footer + extra.len();
        bytes[footer + 12..footer + 16].copy_from_slice(&(size + extra.len() as u32).to_le_bytes());
        rejected(&bytes, ONE_SCENE);
    }
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
fn java_duplicate_identical_central_names_are_rejected() {
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
