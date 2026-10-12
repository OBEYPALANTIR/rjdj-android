//! Host-only corpus evidence; no Android playback or compatibility certification.
use std::collections::{BTreeSet, VecDeque};
use std::error::Error;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Output};
use std::sync::{mpsc, MutexGuard};

use rjdjcore::pd::{
    drain_into_pd, drain_queued, ffi, process, set_event_sink, Atom, Command, CommandQueue, Engine,
    Patch, PdEvent, TouchKind,
};
use rjdjcore::playback;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
const REV: &str = "e74c696afebe672c5bfed543cdd77e2d53865db6";
const RATE: usize = 22050;
const BLOCK: usize = 64;
const FRAMES: usize = RATE * 12;
const EXTERNALS: [&str; 5] = [
    "rj_accum",
    "rj_barkflux_accum~",
    "rj_centroid~",
    "rj_senergy~",
    "rj_zcr~",
];
const EXTRAS: [&str; 7] = [
    "complex-mod~",
    "hilbert~",
    "rev1-final",
    "rev1-stage",
    "rev1~",
    "rev2~",
    "rev3~",
];
const BRIDGES: [&str; 4] = [
    "rj_image.pd",
    "rj_text.pd",
    "soundinput.pd",
    "soundoutput.pd",
];

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Result<Self> {
        for n in 0..10000 {
            let path =
                std::env::temp_dir().join(format!("rjdj-qualify-{}-{n}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err("Cannot create qualification scratch directory".into())
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}
fn output(cmd: &mut Process) -> Result<Output> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?}: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(out)
}
fn sha(path: &Path) -> Result<String> {
    let out = output(Process::new("shasum").args(["-a", "256"]).arg(path))?;
    Ok(String::from_utf8(out.stdout)?
        .split_whitespace()
        .next()
        .ok_or("Empty shasum output")?
        .to_owned())
}
fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(format!("Input contains a link: {}", entry.path().display()).into());
            }
            if kind.is_dir() {
                walk(&entry.path(), result)?;
            } else if kind.is_file() {
                result.push(entry.path());
            } else {
                return Err(
                    format!("Input is not a regular file: {}", entry.path().display()).into(),
                );
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    walk(dir, &mut result)?;
    result.sort();
    Ok(result)
}
fn unzip(source: &Path, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(source)?)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry
            .enclosed_name()
            .ok_or("ZIP path escapes extraction directory")?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err("ZIP contains a link".into());
        }
        let target = dest.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            fs::create_dir_all(target.parent().ok_or("ZIP entry has no parent")?)?;
            io::copy(&mut entry, &mut File::create(target)?)?;
        }
    }
    Ok(())
}

// Sorted regular-file USTAR, with metadata normalized so extraction timestamps cannot change identity.
fn identity(dir: &Path, tar: &Path) -> Result<(String, usize)> {
    let entries = files(dir)?;
    let mut writer = BufWriter::new(File::create(tar)?);
    fn octal(field: &mut [u8], n: u64) {
        let value = format!("{:0width$o}\0", n, width = field.len() - 1);
        assert_eq!(value.len(), field.len());
        field.copy_from_slice(value.as_bytes());
    }
    for file in &entries {
        let relative = file
            .strip_prefix(dir)?
            .to_str()
            .ok_or("Non-UTF-8 corpus name")?;
        let mut header = [0u8; 512];
        if relative.len() > 100 {
            return Err(format!("USTAR file name exceeds 100 bytes: {relative}").into());
        }
        header[..relative.len()].copy_from_slice(relative.as_bytes());
        octal(&mut header[100..108], 0o644);
        octal(&mut header[108..116], 0);
        octal(&mut header[116..124], 0);
        let length = fs::metadata(file)?.len();
        octal(&mut header[124..136], length);
        octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        writer.write_all(&header)?;
        io::copy(&mut File::open(file)?, &mut writer)?;
        writer.write_all(&[0u8; 512][..((512 - length % 512) % 512) as usize])?;
    }
    writer.write_all(&[0u8; 1024])?;
    writer.flush()?;
    drop(writer);
    Ok((sha(tar)?, entries.len()))
}
fn json(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn strings(values: impl IntoIterator<Item = String>) -> String {
    format!(
        "[{}]",
        values
            .into_iter()
            .map(|s| json(&s))
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn number(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        json(&value.to_string())
    }
}
fn md(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}

// Pd records end at an unescaped semicolon, not necessarily at a physical newline.
fn records(text: &str) -> Vec<(usize, String)> {
    let mut result = Vec::new();
    let mut record = String::new();
    let (mut line, mut start, mut escaped) = (1, 1, false);
    for ch in text.chars() {
        if record.is_empty() && !ch.is_whitespace() {
            start = line;
        }
        if ch == ';' && !escaped {
            result.push((start, record.trim().to_owned()));
            record.clear();
        } else if !record.is_empty() || !ch.is_whitespace() {
            record.push(ch);
        }
        if ch == '\n' {
            line += 1;
        }
        escaped = ch == '\\' && !escaped;
    }
    assert!(
        record.trim().is_empty(),
        "Unterminated Pd record at line {start}"
    );
    result
}
fn tokens(record: &str) -> Vec<String> {
    let mut output = Vec::new();
    let (mut token, mut escaped) = (String::new(), false);
    for ch in record.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !token.is_empty() {
                output.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        output.push(token);
    }
    output
}
struct Scan {
    count: usize,
    classes: BTreeSet<String>,
    dynamic: BTreeSet<String>,
    pd_tilde: usize,
}
fn scan(dir: &Path, target: &Path) -> Result<Scan> {
    let (mut entries, mut classes, mut dynamic, mut count, mut pd_tilde) =
        (Vec::new(), BTreeSet::new(), BTreeSet::new(), 0, 0);
    for path in files(dir)?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "pd"))
    {
        count += 1;
        let name = path.strip_prefix(dir)?.to_string_lossy();
        for (line, record) in records(&fs::read_to_string(&path)?) {
            let words = tokens(&record);
            let class = if words.len() >= 5 && words[0] == "#X" && words[1] == "obj" {
                Some(words[4].as_str())
            } else {
                None
            };
            let mut kinds = Vec::new();
            if let Some(class) = class {
                classes.insert(class.to_owned());
                kinds.push("object");
                if class == "pd~" {
                    pd_tilde += 1;
                }
                if class.contains('$') {
                    dynamic.insert(class.to_owned());
                    kinds.push("dynamic-class");
                }
                if matches!(
                    class,
                    "r" | "receive"
                        | "s"
                        | "send"
                        | "r~"
                        | "receive~"
                        | "s~"
                        | "send~"
                        | "throw~"
                        | "catch~"
                ) && words.get(5).is_some_and(|arg| arg.starts_with('#'))
                {
                    kinds.push("host-receiver-or-sender");
                }
            }
            if class == Some("declare")
                || (words.first().map(String::as_str) == Some("#X")
                    && words.get(1).map(String::as_str) == Some("declare"))
            {
                kinds.push("declare");
            }
            let assets = words
                .iter()
                .filter(|word| {
                    let lower = word.to_ascii_lowercase();
                    lower.ends_with(".wav") || lower.ends_with(".png")
                })
                .cloned()
                .collect::<Vec<_>>();
            if !assets.is_empty() {
                kinds.push("asset-reference");
            }
            if !kinds.is_empty() {
                entries.push(format!("{{\"file\":{},\"line\":{line},\"kinds\":{},\"class\":{},\"tokens\":{},\"assets\":{},\"record\":{}}}",
                    json(&name), strings(kinds.into_iter().map(str::to_owned)), class.map(json).unwrap_or_else(|| "null".into()),
                    strings(words.iter().cloned()), strings(assets), json(&record)));
            }
        }
    }
    fs::write(target, format!("{{\"scope\":\"all source .pd files, including unused/help files\",\"pd_files\":{count},\"records\":[{}]}}\n", entries.join(",\n")))?;
    Ok(Scan {
        count,
        classes,
        dynamic,
        pd_tilde,
    })
}
fn reachable(dir: &Path, bridges: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut seen = BTreeSet::new();
    let mut todo = VecDeque::from([dir.join("_main.pd")]);
    while let Some(file) = todo.pop_front() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let parent = file.parent().ok_or("Patch has no directory")?;
        for (_, record) in records(&fs::read_to_string(&file)?) {
            let words = tokens(&record);
            if words.len() < 5 || words[0] != "#X" || words[1] != "obj" {
                continue;
            }
            let class = &words[4];
            if class.contains('$') {
                continue;
            }
            for candidate in [
                parent.join(format!("{class}.pd")),
                dir.join(format!("{class}.pd")),
                dir.join("rj").join(format!("{class}.pd")),
                bridges.join(format!("{class}.pd")),
            ] {
                if candidate.is_file() {
                    todo.push_back(candidate);
                    break;
                }
            }
        }
    }
    Ok(seen)
}

struct Session {
    engine: MutexGuard<'static, Engine>,
    patch: Option<Patch>,
    receiver: mpsc::Receiver<PdEvent>,
    bindings: Vec<*mut c_void>,
    events: Vec<(usize, PdEvent)>,
}
impl Session {
    fn new(paths: &[PathBuf], bind_value: bool) -> Result<Self> {
        let mut engine = Engine::global().lock().unwrap_or_else(|e| e.into_inner());
        engine.init()?;
        drain_queued();
        let (sender, receiver) = mpsc::channel();
        set_event_sink(sender);
        engine.set_search_paths(&paths.iter().map(PathBuf::as_path).collect::<Vec<_>>())?;
        engine.init_audio(1, 2, RATE as i32)?;
        // SAFETY: this child is Pd's sole owner; binding/unbinding happens under its stopped engine lock.
        let bindings = if bind_value {
            let binding = unsafe { ffi::libpd_bind(c"qualify-value".as_ptr()) };
            if binding.is_null() {
                return Err("Cannot bind external probe receiver".into());
            }
            vec![binding]
        } else {
            Vec::new()
        };
        Ok(Self {
            engine,
            patch: None,
            receiver,
            bindings,
            events: Vec::new(),
        })
    }
    fn open(&mut self, dir: &Path, name: &str) -> bool {
        match self.engine.open_patch(dir, name) {
            Ok(patch) => {
                self.patch = Some(patch);
                true
            }
            Err(error) => {
                self.events
                    .push((0, PdEvent::Print(format!("HARNESS ERROR: {error}"))));
                false
            }
        }
    }
    fn collect(&mut self, frame: usize) {
        drain_queued();
        self.events
            .extend(self.receiver.try_iter().map(|event| (frame, event)));
    }
    fn close(&mut self) {
        if let Some(patch) = self.patch.take() {
            self.engine.close_patch(patch);
        }
        self.collect(0);
    }
    fn dsp_off(&self) {
        // SAFETY: the same offline thread owns libpd, and no process call is in flight.
        unsafe {
            ffi::libpd_start_message(1);
            ffi::libpd_add_float(0.0);
            assert_eq!(
                ffi::libpd_finish_message(c"pd".as_ptr(), c"dsp".as_ptr()),
                0
            );
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
        for binding in self.bindings.drain(..) {
            // SAFETY: each live binding is consumed once, with processing stopped.
            unsafe {
                ffi::libpd_unbind(binding);
            }
        }
    }
}
fn prints(events: &[(usize, PdEvent)]) -> Vec<String> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            PdEvent::Print(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}
fn errors(events: &[(usize, PdEvent)]) -> Vec<String> {
    prints(events)
        .into_iter()
        .filter(|text| {
            let lower = text.to_ascii_lowercase();
            lower.contains("couldn't create")
                || lower.contains("no such file")
                || lower.contains("error")
                || lower.contains("cannot open")
                || lower.contains("can't open")
                || lower.contains("failed")
        })
        .collect()
}
fn atoms(values: &[Atom]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|atom| match atom {
                Atom::Float(value) =>
                    format!("{{\"type\":\"float\",\"value\":{}}}", number(*value as f64)),
                Atom::Symbol(value) => format!("{{\"type\":\"symbol\",\"value\":{}}}", json(value)),
            })
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn event_json(frame: usize, event: &PdEvent) -> String {
    let detail = match event {
        PdEvent::Print(line) => format!("\"kind\":\"print\",\"line\":{}", json(line)),
        PdEvent::Bang { recv } => format!("\"kind\":\"bang\",\"receiver\":{}", json(recv)),
        PdEvent::Float { recv, x } => format!(
            "\"kind\":\"float\",\"receiver\":{},\"value\":{}",
            json(recv),
            number(*x as f64)
        ),
        PdEvent::Symbol { recv, sym } => format!(
            "\"kind\":\"symbol\",\"receiver\":{},\"value\":{}",
            json(recv),
            json(sym)
        ),
        PdEvent::List {
            recv,
            atoms: values,
        } => format!(
            "\"kind\":\"list\",\"receiver\":{},\"atoms\":{}",
            json(recv),
            atoms(values)
        ),
        PdEvent::Message {
            recv,
            msg,
            atoms: values,
        } => format!(
            "\"kind\":\"message\",\"receiver\":{},\"selector\":{},\"atoms\":{}",
            json(recv),
            json(msg),
            atoms(values)
        ),
    };
    format!(
        "{{\"frame\":{frame},\"seconds\":{},{detail}}}",
        frame as f64 / RATE as f64
    )
}
fn save_events(prefix: &Path, events: &[(usize, PdEvent)], script: &[String]) -> Result<()> {
    fs::write(
        format!("{}.console.txt", prefix.display()),
        format!("{}\n", prints(events).join("\n")),
    )?;
    fs::write(format!("{}.events.json", prefix.display()), format!("{{\"rate\":{RATE},\"timestamp\":\"end of owning 64-frame tick (load events at frame 0)\",\"script\":[{}],\"events\":[{}]}}\n", script.join(",\n"),
        events.iter().map(|(frame, event)| event_json(*frame, event)).collect::<Vec<_>>().join(",\n")))?;
    Ok(())
}
fn write_wav(path: &Path, samples: &[f32]) -> Result<()> {
    let bytes = u32::try_from(samples.len() * 4)?;
    let mut file = BufWriter::new(File::create(path)?);
    file.write_all(b"RIFF")?;
    file.write_all(&(bytes + 48).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&(RATE as u32).to_le_bytes())?;
    file.write_all(&(RATE as u32 * 8).to_le_bytes())?;
    file.write_all(&8u16.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"fact")?;
    file.write_all(&4u32.to_le_bytes())?;
    file.write_all(&(samples.len() as u32 / 2).to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    for sample in samples {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}
fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let bytes = fs::read(path)?;
    if bytes.len() < 56
        || &bytes[..4] != b"RIFF"
        || &bytes[48..52] != b"data"
        || bytes[20..22] != 3u16.to_le_bytes()
        || bytes[24..28] != (RATE as u32).to_le_bytes()
    {
        return Err("Not a harness stereo float WAV".into());
    }
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into()?) as usize + 8,
        bytes.len()
    );
    assert_eq!(
        u32::from_le_bytes(bytes[52..56].try_into()?) as usize + 56,
        bytes.len()
    );
    Ok(bytes[56..]
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect())
}

fn atsuke_image_contract(events: &[(usize, PdEvent)]) -> bool {
    let delivered_frame =
        |seconds: f64| ((seconds * RATE as f64).ceil() as usize).div_ceil(BLOCK) * BLOCK + BLOCK;
    ["bg.png", "eyes-full.png", "tail1.png"]
        .iter()
        .all(|asset| {
            let key = events.iter().find_map(|(frame, event)| match event {
                PdEvent::List { recv, atoms } if *frame == 0 && recv == "rj_image_android" => {
                    match atoms.as_slice() {
                        [Atom::Symbol(key), Atom::Symbol(selector), Atom::Symbol(file)]
                            if selector == "load" && file == asset =>
                        {
                            Some(key)
                        }
                        _ => None,
                    }
                }
                _ => None,
            });
            let Some(key) = key else {
                return false;
            };
            let has = |frame: usize, expected: &[Atom]| {
                events.iter().any(|(at, event)| match event {
                    PdEvent::List { recv, atoms }
                        if *at == frame
                            && recv == "rj_image_android"
                            && atoms.len() == expected.len() + 1 =>
                    {
                        matches!(&atoms[0], Atom::Symbol(id) if id == key)
                            && atoms[1..] == *expected
                    }
                    _ => false,
                })
            };
            has(
                0,
                &[
                    Atom::Symbol("move".into()),
                    Atom::Float(160.0),
                    Atom::Float(160.0),
                ],
            ) && has(0, &[Atom::Symbol("visible".into()), Atom::Float(1.0)])
                && (*asset != "eyes-full.png"
                    || (has(
                        delivered_frame(1.0),
                        &[Atom::Symbol("visible".into()), Atom::Float(0.0)],
                    ) && has(
                        delivered_frame(5.1),
                        &[Atom::Symbol("visible".into()), Atom::Float(1.0)],
                    )))
        })
        && !events.iter().any(
            |(_, event)| matches!(event, PdEvent::List { recv, .. } if recv == "rj_text_android"),
        )
}

fn scene_worker(
    source: &Path,
    bridges: &Path,
    ext: &Path,
    out: &Path,
    label: &str,
    mode: &str,
) -> Result<()> {
    let scratch = TempDir::new()?;
    let copy = playback::prepare(source, &scratch.0, &mut || Ok(false))?;
    let paths = playback::search_paths(&copy, bridges, ext);
    let mut session = Session::new(&paths, false)?;
    let opened = session.open(&copy, "_main.pd");
    session.collect(0);
    let mut script = Vec::new();
    let (queue, mut consumer) = CommandQueue::new();
    let mut plan = vec![(0.0, "#micvolume 1".to_owned(), Command::MicVolume(1.0))];
    if mode != "no-transport" {
        plan.push((
            0.0,
            "#transport play 1".into(),
            Command::TransportPlay(true),
        ));
    }
    if mode != "no-touch" {
        plan.push((
            1.0,
            "#touch down 1 160 160".into(),
            Command::Touch {
                kind: TouchKind::Down,
                id: 1,
                x: 160.0,
                y: 160.0,
            },
        ));
        for index in 0..40 {
            let x = 20.0 + 280.0 * index as f32 / 39.0;
            let y = 240.0 - 160.0 * index as f32 / 39.0;
            plan.push((
                1.1 + index as f64 * 0.1,
                format!("#touch xy 1 {x} {y}"),
                Command::Touch {
                    kind: TouchKind::Xy,
                    id: 1,
                    x,
                    y,
                },
            ));
        }
        plan.push((
            5.1,
            "#touch up 1 300 80".into(),
            Command::Touch {
                kind: TouchKind::Up,
                id: 1,
                x: 300.0,
                y: 80.0,
            },
        ));
    }
    if mode != "no-accelerate" {
        for (time, z) in [(6.0, -1.0), (7.0, 0.0), (8.0, 1.0), (9.0, -1.0)] {
            plan.push((
                time,
                format!("#accelerate 0 0 {z}"),
                Command::Accelerate(0.0, 0.0, z),
            ));
        }
    }
    if mode != "no-mute" {
        plan.push((10.0, "#micvolume 0".into(), Command::MicVolume(0.0)));
    }
    plan.sort_by(|left, right| left.0.total_cmp(&right.0));
    let mut next = 0;
    let mut rendered = Vec::with_capacity(FRAMES * 2 + BLOCK * 2);
    let (mut input, mut stereo) = ([0.0f32; BLOCK], [0.0f32; BLOCK * 2]);
    let amplitude = 10.0f64.powf(-12.0 / 20.0);
    if opened {
        for frame in (0..FRAMES).step_by(BLOCK) {
            while next < plan.len() && (plan[next].0 * RATE as f64).ceil() as usize <= frame {
                let (requested, text, command) = &plan[next];
                assert!(
                    queue.push(command.clone()),
                    "Single-owner QA command queue overflow"
                );
                script.push(format!(
                    "{{\"requested_seconds\":{requested},\"frame\":{frame},\"message\":{}}}",
                    json(text)
                ));
                next += 1;
            }
            drain_into_pd(&mut consumer);
            for (offset, sample) in input.iter_mut().enumerate() {
                *sample = (amplitude
                    * (std::f64::consts::TAU * 440.0 * (frame + offset) as f64 / RATE as f64).sin())
                    as f32;
            }
            let status = process(1, &input, &mut stereo);
            if status != 0 {
                return Err(format!("libpd_process_float returned {status}").into());
            }
            rendered.extend_from_slice(&stereo);
            session.collect((frame + BLOCK).min(FRAMES));
        }
        rendered.truncate(FRAMES * 2);
    }
    session.close();
    save_events(&out.join(label), &session.events, &script)?;
    write_wav(&out.join(format!("{label}.wav")), &rendered)?;
    fs::write(out.join(format!("{label}.opened")), opened.to_string())?;
    if source.file_name().is_some_and(|name| name == "Atsuke.rj") {
        fs::write(
            out.join(format!("{label}.overlay-contract")),
            atsuke_image_contract(&session.events).to_string(),
        )?;
    }
    drop(session);
    playback::release(&copy);
    Ok(())
}

struct Spectrum {
    real: Vec<f64>,
    imag: Vec<f64>,
    window: Vec<f64>,
    twiddle: Vec<(f64, f64)>,
}
impl Spectrum {
    fn new() -> Self {
        let n = 1024;
        Self {
            real: vec![0.0; n],
            imag: vec![0.0; n],
            window: (0..n)
                .map(|index| {
                    0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (n - 1) as f64).cos()
                })
                .collect(),
            twiddle: (0..n / 2)
                .map(|index| {
                    let (sin, cos) = (-std::f64::consts::TAU * index as f64 / n as f64).sin_cos();
                    (cos, sin)
                })
                .collect(),
        }
    }
    fn centroid(&mut self, audio: &[f32], channel: usize) -> f64 {
        let (mut weighted, mut total) = (0.0, 0.0);
        for start in (0..audio.len() / 2).step_by(512) {
            for index in 0..1024 {
                self.real[index] = audio
                    .get((start + index) * 2 + channel)
                    .copied()
                    .unwrap_or(0.0) as f64
                    * self.window[index];
                self.imag[index] = 0.0;
            }
            for index in 0usize..1024 {
                let reversed = index.reverse_bits() >> (usize::BITS - 10);
                if index < reversed {
                    self.real.swap(index, reversed);
                    self.imag.swap(index, reversed);
                }
            }
            for width in [2, 4, 8, 16, 32, 64, 128, 256, 512, 1024] {
                for base in (0..1024).step_by(width) {
                    for offset in 0..width / 2 {
                        let left = base + offset;
                        let right = left + width / 2;
                        let (cos, sin) = self.twiddle[offset * 1024 / width];
                        let re = cos * self.real[right] - sin * self.imag[right];
                        let im = sin * self.real[right] + cos * self.imag[right];
                        self.real[right] = self.real[left] - re;
                        self.imag[right] = self.imag[left] - im;
                        self.real[left] += re;
                        self.imag[left] += im;
                    }
                }
            }
            for bin in 0..=512 {
                let magnitude = self.real[bin].hypot(self.imag[bin]);
                total += magnitude;
                weighted += magnitude * bin as f64 * RATE as f64 / 1024.0;
            }
        }
        if total == 0.0 {
            0.0
        } else {
            weighted / total
        }
    }
}
#[derive(Clone)]
struct Stats {
    rms: [f64; 2],
    peak: f64,
    centroid: [f64; 2],
    finite: bool,
    parity: f64,
}
fn stats(audio: &[f32], spectrum: &mut Spectrum) -> Stats {
    let (mut sums, mut peak, mut parity, mut finite) = ([0.0, 0.0], 0.0f64, 0.0f64, true);
    for stereo in audio.chunks_exact(2) {
        for channel in 0..2 {
            let sample = stereo[channel] as f64;
            finite &= sample.is_finite();
            sums[channel] += sample * sample;
            peak = peak.max(sample.abs());
        }
        parity = parity.max((stereo[0] as f64 - stereo[1] as f64).abs());
    }
    Stats {
        rms: sums.map(|sum| {
            if audio.is_empty() {
                0.0
            } else {
                (sum / (audio.len() / 2) as f64).sqrt()
            }
        }),
        peak,
        centroid: [spectrum.centroid(audio, 0), spectrum.centroid(audio, 1)],
        finite,
        parity,
    }
}
fn checks() {
    assert_eq!(
        tokens("#X obj 0 0 \\$1 escaped\\ name"),
        ["#X", "obj", "0", "0", "$1", "escaped name"]
    );
    assert_eq!(
        records("#X msg 0 0 a\\; b\n c;\n#X obj 0 0 osc~;"),
        vec![
            (1, "#X msg 0 0 a\\; b\n c".into()),
            (3, "#X obj 0 0 osc~".into())
        ]
    );
    assert_eq!(json("\"\\\n\t"), "\"\\\"\\\\\\n\\t\"");
    let sine = (0..RATE)
        .flat_map(|frame| {
            let x =
                (0.25 * (std::f64::consts::TAU * 440.0 * frame as f64 / RATE as f64).sin()) as f32;
            [x, x]
        })
        .collect::<Vec<_>>();
    let measured = stats(&sine, &mut Spectrum::new());
    assert!((measured.rms[0] - 0.25 / 2.0f64.sqrt()).abs() < 1e-6);
    assert!(
        (measured.centroid[0] - 440.0).abs() < 15.0,
        "FFT self-check: {}",
        measured.centroid[0]
    );
    assert!(measured.finite && measured.parity == 0.0 && measured.peak <= 0.25);
    assert!(
        !atsuke_image_contract(&[]),
        "Missing image lists cannot satisfy the overlay contract"
    );
}

fn child(args: &[String], out: &Path, label: &str) -> Result<bool> {
    let command = std::env::current_exe()?;
    let output = Process::new(command).args(args).output()?;
    fs::write(
        out.join(format!("{label}.host.txt")),
        format!(
            "status: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(output.status.success())
}
fn port_lines(path: &Path) -> Result<(String, String)> {
    let (mut inlets, mut outlets, mut depth) = (Vec::new(), Vec::new(), 0usize);
    for (line, record) in records(&fs::read_to_string(path)?) {
        let words = tokens(&record);
        if words.first().map(String::as_str) == Some("#N")
            && words.get(1).map(String::as_str) == Some("canvas")
        {
            depth += 1;
        }
        if words.first().map(String::as_str) == Some("#X")
            && words.get(1).map(String::as_str) == Some("restore")
        {
            depth = depth.saturating_sub(1);
        }
        if depth == 1 && words.len() >= 5 && words[0] == "#X" && words[1] == "obj" {
            match words[4].as_str() {
                "inlet" | "inlet~" => inlets.push(format!("{}@{line}", words[4])),
                "outlet" | "outlet~" => outlets.push(format!("{}@{line}", words[4])),
                _ => {}
            }
        }
    }
    Ok((inlets.join(", "), outlets.join(", ")))
}
#[allow(clippy::too_many_arguments)] // QA harness, not product code
fn abstraction_worker(
    source: &Path,
    search: &Path,
    scene_copy: bool,
    bridges: &Path,
    ext: &Path,
    out: &Path,
    label: &str,
    head: &Path,
) -> Result<()> {
    let scratch = TempDir::new()?;
    let host = if scene_copy {
        search.to_path_buf()
    } else {
        scratch.0.clone()
    };
    let paths = if scene_copy {
        playback::search_paths(search, bridges, ext)
    } else {
        vec![
            search.to_path_buf(),
            bridges.to_path_buf(),
            ext.to_path_buf(),
        ]
    };
    let mut session = Session::new(&paths, false)?;
    // COR-06 here is instantiation only, not an inlet/outlet behavior certification.
    session.dsp_off();
    let mut table = String::from("| Abstraction | Creates without creation error? | All-error count | Inlets (root canvas class@line) | Outlets | SHA-256 vs HEAD | Error lines |\n|---|---|---:|---|---|---|---|\n");
    let mut console = String::new();
    let mut hashes = String::from("name\tsource_sha256\thead_sha256\n");
    let patches = files(source)?
        .into_iter()
        .filter(|path| {
            path.parent() == Some(source)
                && path.extension().is_some_and(|ext| ext == "pd")
                && !path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .ends_with("-help")
        })
        .collect::<Vec<_>>();
    for path in &patches {
        let name = path
            .file_stem()
            .ok_or("Abstraction has no name")?
            .to_string_lossy();
        let patch = format!(
            "#N canvas 0 0 400 200 10;\n{}#X obj 20 20 {name};\n",
            if scene_copy {
                "#X declare -path rj;\n"
            } else {
                ""
            }
        );
        let generated = host.join("__qualify_abstraction.pd");
        fs::write(&generated, patch)?;
        session.events.clear();
        let opened = session.open(&host, "__qualify_abstraction.pd");
        session.collect(0);
        session.close();
        let error_lines = errors(&session.events);
        let creates = opened
            && !error_lines
                .iter()
                .any(|line| line.contains("couldn't create"));
        console.push_str(&format!(
            "=== {name} ===\n{}\n",
            prints(&session.events).join("\n")
        ));
        let hash = sha(path)?;
        let head_file = head.join(path.file_name().ok_or("No abstraction filename")?);
        let head_hash = if head_file.is_file() {
            sha(&head_file)?
        } else {
            "absent".into()
        };
        let compare = if label == "HEAD" {
            "HEAD"
        } else if BRIDGES.contains(
            &path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
        ) {
            if hash == head_hash {
                "same; playback replaces bridge"
            } else {
                "different; playback replaces bridge"
            }
        } else if hash == head_hash {
            "same"
        } else if head_hash == "absent" {
            "local-only"
        } else {
            "different"
        };
        let (inlets, outlets) = port_lines(path)?;
        table.push_str(&format!(
            "| {} | {creates} | {} | {} | {} | {compare} | {} |\n",
            md(&name),
            error_lines.len(),
            md(&inlets),
            md(&outlets),
            md(&error_lines.join("; "))
        ));
        hashes.push_str(&format!("{name}\t{hash}\t{head_hash}\n"));
        fs::remove_file(generated)?;
    }
    fs::write(out.join(format!("{label}-abstractions.md")), format!("## {label}: {} non-help abstractions\n\nSource: `{}`. Search root: `{}`. DSP disabled; no creation arguments supplied.\n\n{table}\n", patches.len(), source.display(), search.display()))?;
    fs::write(
        out.join(format!("{label}-abstractions.console.txt")),
        console,
    )?;
    fs::write(out.join(format!("{label}-abstractions.hashes.tsv")), hashes)?;
    Ok(())
}
fn external_patch(name: &str) -> String {
    let graph = if name == "rj_accum" {
        "#X obj 20 20 inlet~;\n#X obj 200 20 block~ 512;\n#X obj 20 60 env~ 512;\n#X obj 20 100 rj_accum 512;\n#X obj 20 140 s qualify-value;\n#X connect 0 0 2 0;\n#X connect 2 0 3 0;\n#X connect 3 0 4 0;\n".to_owned()
    } else if name == "rj_zcr~" {
        "#X obj 20 20 inlet~;\n#X obj 200 20 block~ 1024;\n#X obj 20 60 rj_zcr~;\n#X obj 20 100 s qualify-value;\n#X connect 0 0 2 0;\n#X connect 2 0 3 0;\n".to_owned()
    } else {
        format!("#X obj 20 20 inlet~;\n#X obj 200 20 block~ 1024;\n#X obj 20 60 rfft~;\n#X obj 20 100 *~;\n#X obj 120 100 *~;\n#X obj 20 140 +~;\n#X obj 20 180 sqrt~;\n#X obj 20 220 {name};\n#X obj 20 260 s qualify-value;\n#X connect 0 0 2 0;\n#X connect 2 0 3 0;\n#X connect 2 0 3 1;\n#X connect 2 1 4 0;\n#X connect 2 1 4 1;\n#X connect 3 0 5 0;\n#X connect 4 0 5 1;\n#X connect 5 0 6 0;\n#X connect 6 0 7 0;\n#X connect 7 0 8 0;\n")
    };
    format!("#N canvas 0 0 400 300 10;\n#X obj 20 20 adc~ 1;\n#N canvas 0 0 500 400 isolated 0;\n{graph}#X restore 20 100 pd isolated;\n#X connect 0 0 1 0;\n")
}
fn external_worker(name: &str, bridges: &Path, ext: &Path, out: &Path, head: &Path) -> Result<()> {
    let scratch = TempDir::new()?;
    let patch = external_patch(name);
    fs::write(scratch.0.join("probe.pd"), &patch)?;
    fs::write(out.join(format!("{name}.probe.pd")), &patch)?;
    let mut session = Session::new(&[bridges.to_path_buf(), ext.to_path_buf()], true)?;
    let opened = session.open(&scratch.0, "probe.pd");
    session.collect(0);
    let (mut input, mut stereo, mut random) = ([0.0f32; BLOCK], [0.0f32; BLOCK * 2], 0x12345678u32);
    if opened {
        for frame in (0..RATE * 2).step_by(BLOCK) {
            for (index, value) in input.iter_mut().enumerate() {
                if frame < RATE {
                    *value = (0.25
                        * (std::f64::consts::TAU * 440.0 * (frame + index) as f64 / RATE as f64)
                            .sin()) as f32;
                } else {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    *value = (random as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32 * 0.25;
                }
            }
            assert_eq!(process(1, &input, &mut stereo), 0);
            session.collect(frame + BLOCK);
        }
    }
    session.close();
    save_events(&out.join(format!("{name}.probe")), &session.events, &[])?;
    let values = session
        .events
        .iter()
        .filter_map(|(_, event)| match event {
            PdEvent::Float { recv, x } if recv == "qualify-value" => Some(*x),
            _ => None,
        })
        .collect::<Vec<_>>();
    let finite = !values.is_empty() && values.iter().all(|value| value.is_finite());
    let error_lines = errors(&session.events);
    let min = values.iter().copied().fold(f32::INFINITY, f32::min);
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    fs::write(
        out.join(format!("{name}.probe.result")),
        format!(
            "{opened}\t{}\t{finite}\t{}\t{}\t{}\n",
            error_lines.len(),
            values.len(),
            number(min as f64),
            number(max as f64)
        ),
    )?;
    drop(session);
    let mut help = Session::new(&[bridges.to_path_buf(), ext.to_path_buf()], false)?;
    // The original zcr help contains a naked 64-frame object. Load all help files with DSP OFF.
    help.dsp_off();
    let help_open = help.open(&head.join("src"), &format!("{name}-help.pd"));
    help.collect(0);
    help.close();
    save_events(&out.join(format!("{name}.help")), &help.events, &[])?;
    fs::write(
        out.join(format!("{name}.help.result")),
        format!("{help_open}\t{}\n", errors(&help.events).len()),
    )?;
    if !finite {
        return Err(format!(
            "{name}: finite/nonempty output assertion failed (probe evidence saved)"
        )
        .into());
    }
    Ok(())
}
fn pd_tilde_worker(bridges: &Path, ext: &Path, out: &Path) -> Result<()> {
    let scratch = TempDir::new()?;
    let absent = scratch.0.join("absent/a/b/c");
    for candidate in [
        absent.join("bin/pd"),
        absent.join("../../../bin/pd"),
        absent.join("pd"),
    ] {
        assert!(!candidate.exists(), "pd~ absence precondition");
    }
    let patch = format!("#N canvas 0 0 400 300 10;\n#X obj 20 20 pd~ -pddir {} -ninsig 1 -noutsig 2;\n#X obj 20 60 loadbang;\n#X msg 20 100 pd~ start -nogui;\n#X connect 1 0 2 0;\n#X connect 2 0 0 0;\n", absent.display());
    fs::write(scratch.0.join("pd-tilde.pd"), &patch)?;
    fs::write(out.join("pd-tilde.probe.pd"), &patch)?;
    let mut session = Session::new(&[bridges.to_path_buf(), ext.to_path_buf()], false)?;
    let opened = session.open(&scratch.0, "pd-tilde.pd");
    session.collect(0);
    for frame in 0..16 {
        assert_eq!(process(1, &[0.0; BLOCK], &mut [0.0; BLOCK * 2]), 0);
        session.collect((frame + 1) * BLOCK);
    }
    session.close();
    save_events(&out.join("pd-tilde"), &session.events, &[])?;
    fs::write(out.join("pd-tilde.md"), format!("# UNK-11: pd~ without an external Pd executable\n\nPatch opened: `{opened}`. The host libpd build registers `pd~`; this is not an embedded replacement for its subprocess. The probe explicitly sets `-pddir {}`; all three executable candidates were asserted absent before sending `pd~ start -nogui`. No process was substituted.\n\nExact queued console output:\n\n```text\n{}\n```\n\nAndroid packages no standalone `pd` executable. This host failure documents that prerequisite, not an Android runtime test. Public scene static `pd~` occurrence counts are in summary.md; a zero count is not proof about a personal collection or dynamically created classes.\n", absent.display(), prints(&session.events).join("\n")))?;
    Ok(())
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn console_errors(path: &Path) -> Result<Vec<String>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("couldn't create")
                || lower.contains("no such file")
                || lower.contains("error")
                || lower.contains("cannot open")
                || lower.contains("can't open")
                || lower.contains("failed")
        })
        .map(str::to_owned)
        .collect())
}
fn scene_contract(
    name: &str,
    windows: &[Stats],
    total: &Stats,
    tail: &Stats,
    image_sequence: bool,
) -> (bool, String) {
    let bounds = total.finite
        && total.peak <= 1.00001
        && total
            .centroid
            .iter()
            .all(|centroid| (0.0..=RATE as f64 / 2.0).contains(centroid));
    let nonzero = windows
        .iter()
        .take(10)
        .any(|window| window.rms.iter().any(|rms| *rms > 1e-6));
    let mut text = format!("All samples finite, peak <= 1.00001 (real soundoutput clip), centroid in [0,11025] Hz: **{bounds}**. Active-window RMS > 1e-6: **{nonzero}**.\n");
    let mut pass = bounds && nonzero;
    match name {
        "HelloWorldWorldWorld" => {
            let clean_tail = tail.rms.iter().all(|rms| *rms < 1e-5);
            let tone = (400.0..480.0).contains(&windows[4].centroid[0]);
            let parity = total.parity < 1e-5;
            pass &= clean_tail && tone && parity;
            text.push_str(&format!("Graph: five 200/300/500/700/1100 ms delays sum into one gain, then identical L/R. Second 4 centroid in [400,480] Hz: **{tone}**; L/R max difference < 1e-5: **{parity}**; final 250 ms RMS < 1e-5: **{clean_tail}** (L {:.9}, R {:.9}). The whole second-11 window includes the valid 1100 ms delay tail until about t=11.16; the silence window starts at t=11.75, after the 50 ms mic ramp, maximum delay and output high-pass decay. This corrects an initially over-broad whole-second silence check without changing its 1e-5 threshold.\n", tail.rms[0], tail.rms[1]));
        }
        "OnsetDetection" => {
            // The onset criterion is >25 dB between adjacent high-passed 64-frame RMS windows.
            pass = bounds;
            text.push_str("Graph: two hip~ 3000 stages, 200 ms fade-in and >25 dB onset detector trigger the two sample banks. A steady 440 Hz sine is not an onset guarantee; either silence or bounded nonzero sample playback is allowed. This scenario does NOT certify onset detection against percussive input.\n");
        }
        "StreetBeatz" => {
            let persists = windows[11].rms.iter().any(|rms| *rms > 1e-6);
            let parity = total.parity < 1e-5;
            pass &= persists && parity;
            text.push_str(&format!("Graph: sf_beat/sf_break loops start at loadbang; z-axis <-0.75 and >0.75 select the crossfade. Loop audio must persist after mic mute: **{persists}**; L/R max difference < 1e-5: **{parity}**. Accelerometer response is separately measured against no-accelerate control.\n"));
        }
        "Vocoder" => {
            let clean_tail = windows[11].rms.iter().all(|rms| *rms < 1e-5);
            let parity = total.parity < 1e-5;
            pass &= clean_tail && parity;
            text.push_str(&format!("Graph: microphone modulates the e_vocoder chord carrier; z-axis selects MIDI chords. L/R max difference < 1e-5: **{parity}**; second 11 RMS < 1e-5 after mute/analysis decay: **{clean_tail}**. Accelerometer response is separately measured against no-accelerate control.\n"));
        }
        "Atsuke" => {
            pass &= image_sequence;
            text.push_str(&format!("Graph: synthesis/recording/reverb may sustain after mic mute. Exact typed rj_image_android lists must load bg.png, eyes-full.png and tail1.png at frame 0, move each to (160,160) and make each visible; the eyes key must receive visible 0 at the down tick and visible 1 at the up tick. No rj_text_android lists are expected. This sequence contract: **{image_sequence}**. Exact atoms, keys and event order are retained in events.json; no screen-render claim is made. Touch reactivity is compared with no-touch control.\n"));
        }
        _ => unreachable!("five pinned scenes only"),
    }
    (pass, text)
}

fn run(out: &Path) -> Result<()> {
    if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
        return Err("This host external qualification requires macOS arm64".into());
    }
    fs::create_dir_all(out)?;
    let out = out.canonicalize()?;
    let repo = root();
    let corpus = PathBuf::from("/tmp/rjlib-e74c696");
    if !corpus.exists() {
        output(
            Process::new("git")
                .args(["clone", "https://github.com/rjdj/rjlib.git"])
                .arg(&corpus),
        )?;
        output(
            Process::new("git")
                .arg("-C")
                .arg(&corpus)
                .args(["checkout", REV]),
        )?;
    }
    let revision = String::from_utf8(
        output(
            Process::new("git")
                .arg("-C")
                .arg(&corpus)
                .args(["rev-parse", "HEAD"]),
        )?
        .stdout,
    )?
    .trim()
    .to_owned();
    if revision != REV {
        return Err(format!("rjlib must be {REV}; found {revision}").into());
    }
    let dirty = output(
        Process::new("git")
            .arg("-C")
            .arg(&corpus)
            .args(["status", "--porcelain"]),
    )?;
    if !dirty.stdout.is_empty() {
        return Err("Pinned rjlib input has uncommitted changes; refusing to mutate it".into());
    }
    let scratch = TempDir::new()?;
    let bridges = scratch.0.join("bridges");
    let extracted = scratch.0.join("scenes");
    unzip(&repo.join("ScenePlayer/res/raw/abstractions.zip"), &bridges)?;
    unzip(&repo.join("ScenePlayer/res/raw/atsuke.zip"), &extracted)?;
    let pd = repo.join("PdCore/pd-core/src/main/jni/libpd/pure-data");
    for name in EXTRAS {
        fs::copy(
            pd.join("extra").join(format!("{name}.pd")),
            bridges.join(format!("{name}.pd")),
        )?;
    }
    let ext = PathBuf::from("/tmp/rj-ext");
    fs::create_dir_all(&ext)?;
    let loader = fs::read_to_string(pd.join("src/s_loader.c"))?;
    assert!(
        loader.contains("\".pd_darwin\""),
        "Pinned loader no longer tries .pd_darwin"
    );
    let mut build = String::new();
    for name in EXTERNALS {
        let source = repo.join("ScenePlayer/jni").join(format!("{name}.c"));
        let binary = ext.join(format!("{name}.pd_darwin"));
        let mut command = Process::new("cc");
        command
            .arg("-DPD")
            .arg(format!("-I{}", pd.join("src").display()))
            .args(["-bundle", "-undefined", "dynamic_lookup"])
            .arg(&source)
            .arg("-o")
            .arg(&binary);
        let invocation = format!("{command:?}");
        let result = command.output()?;
        build.push_str(&format!(
            "{invocation}\nstatus: {}\n{}{}\n",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        ));
        if !result.status.success() {
            fs::write(out.join("externals.build.txt"), build)?;
            return Err(format!("Host external compile failed: {name}").into());
        }
        build.push_str(&format!(
            "source SHA-256: {}\nbinary SHA-256: {}\n",
            sha(&source)?,
            sha(&binary)?
        ));
        build.push_str(&String::from_utf8(
            output(Process::new("file").arg(&binary))?.stdout,
        )?);
    }
    fs::write(out.join("externals.build.txt"), build)?;
    fs::write(out.join("inputs.txt"), format!("rjlib: {} {revision}\nclean: true\nrepo: {}\nabstractions.zip SHA-256: {}\natsuke.zip SHA-256: {}\nExtras: {}\n", corpus.display(), repo.display(), sha(&repo.join("ScenePlayer/res/raw/abstractions.zip"))?, sha(&repo.join("ScenePlayer/res/raw/atsuke.zip"))?, EXTRAS.join(", ")))?;
    let mut scenes = fs::read_dir(corpus.join("examplescenes"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.is_dir() && entry.extension().is_some_and(|ext| ext == "rj"))
        .collect::<Vec<_>>();
    scenes.push(extracted.join("Atsuke.rj"));
    scenes.sort_by_key(|scene| scene.file_name().map(|name| name.to_os_string()));
    assert_eq!(scenes.len(), 5, "Expected four public examples plus Atsuke");
    let mut summary = format!("# Public scene offline qualification\n\nDate: {}. Host: macOS arm64. Pd: 22050 Hz, mono input/stereo output, 64-frame host ticks.\n\nCommand: `CARGO_TARGET_DIR=/tmp/rjdj-target-J cargo run --example qualify -- --out {}`\n\nWorking directory: `{}`. rjlib git revision: `{revision}` (clean read-only input). This revision uses `examplescenes/`, not `examples/`. Atsuke and bridges are the checkout's ZIP bytes (hashes in inputs.txt).\n\nSteps 1–5 only. **Step 6 device playback was not run here.** No result is a compatibility or original-app fidelity claim. Instantiation and audio verdicts are independent; both are shown.\n\n| Scene | 1 identity | 2 scan | 3 zero instantiation errors | 4 defined contract | 5 offline contract pass | Verdicts | RMS L/R (12 s) | Peak | Centroid L/R Hz | Console errors |\n|---|---|---|---|---|---|---|---|---:|---|---:|\n", String::from_utf8(output(Process::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"]))?.stdout)?.trim(), out.display(), Path::new(env!("CARGO_MANIFEST_DIR")).display());
    let mut abstraction_sections = Vec::new();
    let mut all_scene_pd_tilde = 0;
    let mut spectrum = Spectrum::new();
    for scene in &scenes {
        let name = scene
            .file_stem()
            .ok_or("Scene has no name")?
            .to_string_lossy()
            .into_owned();
        let (hash, count) = identity(scene, &out.join(format!("{name}.source.tar")))?;
        let scanned = scan(scene, &out.join(format!("{name}.static.json")))?;
        all_scene_pd_tilde += scanned.pd_tilde;
        let args = vec![
            "--worker-scene".into(),
            path(scene),
            path(&bridges),
            path(&ext),
            path(&out),
            name.clone(),
            "scripted".into(),
        ];
        let succeeded = child(&args, &out, &name)?;
        if !succeeded {
            return Err(format!("Scene worker failed: {name}; see host.txt").into());
        }
        let opened = fs::read_to_string(out.join(format!("{name}.opened")))? == "true";
        let audio = read_wav(&out.join(format!("{name}.wav")))?;
        let total = stats(&audio, &mut spectrum);
        let windows = audio
            .chunks(RATE * 2)
            .map(|audio| stats(audio, &mut spectrum))
            .collect::<Vec<_>>();
        if windows.len() != 12 {
            return Err(format!("{name}: did not render twelve seconds").into());
        }
        let console = fs::read_to_string(out.join(format!("{name}.console.txt")))?;
        let error_lines = console_errors(&out.join(format!("{name}.console.txt")))?;
        let events = fs::read_to_string(out.join(format!("{name}.events.json")))?;
        let tail = stats(&audio[(FRAMES - RATE / 4) * 2..], &mut spectrum);
        let image_sequence = name != "Atsuke"
            || fs::read_to_string(out.join(format!("{name}.overlay-contract")))? == "true";
        let (mut passed, contract) = scene_contract(&name, &windows, &total, &tail, image_sequence);
        let copy = playback::prepare(scene, &scratch.0.join("playback"), &mut || Ok(false))?;
        let reachable = reachable(&copy, &bridges)?;
        let reachable_text = reachable
            .iter()
            .map(fs::read_to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        let mut comparisons = String::from("| Control (same input/other messages) | Audio difference RMS | Max absolute audio difference | Evidence interpretation |\n|---|---:|---:|---|\n");
        for mode in ["no-touch", "no-accelerate", "no-mute", "no-transport"] {
            let receiver = match mode {
                "no-touch" => "#touch",
                "no-accelerate" => "#accelerate",
                "no-mute" => "#micvolume",
                _ => "#transport",
            };
            if !reachable_text.contains(receiver) {
                continue;
            }
            let label = format!("{name}-{mode}");
            let args = vec![
                "--worker-scene".into(),
                path(scene),
                path(&bridges),
                path(&ext),
                path(&out),
                label.clone(),
                mode.into(),
            ];
            if !child(&args, &out, &label)? {
                return Err(format!("Control worker failed: {label}").into());
            }
            let control = read_wav(&out.join(format!("{label}.wav")))?;
            assert_eq!(control.len(), audio.len());
            let (squares, delta_peak) =
                audio
                    .iter()
                    .zip(&control)
                    .fold((0.0, 0.0f64), |(sum, peak), (a, b)| {
                        let difference = (*a - *b) as f64;
                        (sum + difference * difference, peak.max(difference.abs()))
                    });
            let delta_rms = (squares / audio.len() as f64).sqrt();
            let mut interpretation = if delta_rms > 1e-7 {
                "audio reacts (difference > 1e-7 RMS)".to_owned()
            } else {
                "no measurable audio response at 1e-7 RMS".to_owned()
            };
            if mode == "no-touch" && name == "Atsuke" {
                let control_events = fs::read_to_string(out.join(format!("{label}.events.json")))?;
                // Keep exact keys in evidence; a source-derived required sequence does not depend on $0 values.
                let visible = events.matches("\"value\":\"visible\"").count();
                let control_visible = control_events.matches("\"value\":\"visible\"").count();
                let overlay_response = visible > control_visible;
                interpretation.push_str(&format!("; image visibility list counts {visible} vs {control_visible}, touch response {overlay_response}"));
                passed &= overlay_response;
            }
            if mode == "no-accelerate" && matches!(name.as_str(), "StreetBeatz" | "Vocoder") {
                passed &= delta_rms > 1e-7;
            }
            if mode == "no-mute" && matches!(name.as_str(), "HelloWorldWorldWorld" | "Vocoder") {
                passed &= delta_rms > 1e-7;
            }
            if mode == "no-transport" {
                let control_stats = stats(&control, &mut spectrum);
                let transport_silent = control_stats.rms.iter().all(|x| *x < 1e-6);
                passed &= transport_silent;
                interpretation.push_str(&format!("; no-play RMS < 1e-6: {transport_silent}"));
            }
            comparisons.push_str(&format!(
                "| {mode} | {delta_rms:.8} | {delta_peak:.8} | {interpretation} |\n"
            ));
        }
        let instantiate = opened && error_lines.is_empty();
        let audio_verdict = if total.rms.iter().any(|rms| *rms > 1e-6) {
            "audio-nonzero"
        } else {
            "audio-silent"
        };
        let instantiate_verdict = if instantiate {
            "instantiates"
        } else {
            "instantiates-with-errors"
        };
        let mut report = format!("# {name}: host offline evidence\n\nVerdicts: **{instantiate_verdict}**, **{audio_verdict}**. Neither verdict certifies this scene for Android.\n\n| Step | Boolean | Evidence |\n|---|---|---|\n| 1 pinned identity | true | source `{}`; {count} files; sorted normalized USTAR SHA-256 `{hash}`; source tar archived |\n| 2 static pre-scan | true | {} .pd files, {} distinct #X obj classes, {} dynamic classes; every occurrence in {name}.static.json |\n| 3 instantiation without errors | {instantiate} | real Engine::global/init_audio(1,2,22050)/open_patch; playback::prepare omits four bridges at all depths; {} console error lines |\n| 4 defined scenario contract | true | source-derived bounded audio/receiver expectations and tolerances below; this is not a universal musical-fidelity contract |\n| 5 offline scenario pass | {passed} | twelve-second float WAV + per-second features + exact queued console/events + ablation controls |\n| 6 device playback | false | not run; orchestrator-owned |\n\n## Identity and search paths\n\nSource tar contains sorted regular files, relative names, mode 0644, uid/gid/mtime zero, no directory entries; it fingerprints bytes/names rather than source permissions.\n\nGlobal paths are bridges+seven Pd extra .pd files first, `/tmp/rj-ext` second, exactly playback::search_paths order. Scene canvas directory and its own declared `rj/` precede global paths; HEAD rjlib is NOT a scene fallback.\n\nStatic dynamic classes: `{}`. Static pd~ occurrences in all source patches: {}. Reachable literal abstraction traversal: {} files (dynamic/message-created objects are not statically certified).\n\n## Scenario and tolerances\n\nInput: continuous mono 440 Hz sine, peak amplitude 10^(-12/20) = {:.9}, 22050 Hz, even after the microphone mute. At t=0, micvolume 1 then transport play 1. At t=1 touch down 1 160 160; 40 xy points at t=1.1…5.0 from (20,240) to (300,80); t=5.1 up. Accelerate z=-1,0,1,-1 at t=6,7,8,9; t=10 micvolume 0; stop capture at exactly t=12. All message dispatches are rounded up to the next 64-frame tick; requested/actual times in events.json.\n\n{contract}\n## Per-second features\n\nCentroid uses magnitude-weighted 1024-point Hann FFTs, hop 512, zero-padded at each second's end, independently L/R. Empty spectrum centroid=0. The harness asserts a 440 Hz reference sine's RMS and centroid before running any case.\n\n| Second | RMS L | RMS R | Peak | Centroid L Hz | Centroid R Hz | Finite |\n|---:|---:|---:|---:|---:|---:|---|\n", scene.display(), scanned.count, scanned.classes.len(), scanned.dynamic.len(), error_lines.len(), scanned.dynamic.iter().cloned().collect::<Vec<_>>().join(", "), scanned.pd_tilde, reachable.len(), 10.0f64.powf(-12.0 / 20.0));
        for (second, stats) in windows.iter().enumerate() {
            report.push_str(&format!(
                "| {second} | {:.9} | {:.9} | {:.9} | {:.3} | {:.3} | {} |\n",
                stats.rms[0],
                stats.rms[1],
                stats.peak,
                stats.centroid[0],
                stats.centroid[1],
                stats.finite
            ));
        }
        report.push_str(&format!("\n## Reactivity controls\n\n{comparisons}\nIndependent children prevent libpd class caches/state leaking across cases. Random/audio-generated musical branches may differ between children; audio A/B differences alone do not prove causal musical parity. Exact overlay counts/order and the deterministic signal-only examples give stronger evidence.\n\n## Exact console\n\n```text\n{console}```\n\n## Outputs\n\n`{name}.wav`, `{name}.console.txt`, `{name}.events.json` (all exact rj_image_android/rj_text_android lists), `{name}.static.json`, `{name}.source.tar`, and same-signal ablation WAVs/events/console.\n"));
        fs::write(out.join(format!("{name}.report.md")), report)?;
        summary.push_str(&format!("| [{name}]({name}.report.md) | true | true | {instantiate} | true | {passed} | {instantiate_verdict}; {audio_verdict} | {:.7} / {:.7} | {:.7} | {:.2} / {:.2} | {} |\n", total.rms[0], total.rms[1], total.peak, total.centroid[0], total.centroid[1], error_lines.len()));
        let args = vec![
            "--worker-abstractions".into(),
            path(&scene.join("rj")),
            path(&copy),
            "scene".into(),
            path(&bridges),
            path(&ext),
            path(&out),
            name.clone(),
            path(&corpus.join("rj")),
        ];
        if !child(&args, &out, &format!("{name}-abstractions"))? {
            return Err(format!("Local abstraction worker failed: {name}").into());
        }
        abstraction_sections.push(fs::read_to_string(
            out.join(format!("{name}-abstractions.md")),
        )?);
        playback::release(&copy);
    }
    let head_args = vec![
        "--worker-abstractions".into(),
        path(&corpus.join("rj")),
        path(&corpus.join("rj")),
        "head".into(),
        path(&bridges),
        path(&ext),
        path(&out),
        "HEAD".into(),
        path(&corpus.join("rj")),
    ];
    if !child(&head_args, &out, "HEAD-abstractions")? {
        return Err("HEAD abstraction worker failed".into());
    }
    fs::write(out.join("abstractions.md"), format!("# COR-06 instantiation evidence\n\nPinned HEAD: `{revision}`. Every root rj/*.pd without a -help suffix is instantiated alone, without invented arguments. `Creates` means the host patch opens with zero couldn't-create lines; other errors remain counted and printed. DSP is disabled. Root-canvas inlet/outlet classes and physical line numbers are observed from the original files, not inferred from a loaded help patch.\n\nScene-local sets are tested separately through each real prepared playback copy, with its own declare -path rj and host bridges; no HEAD fallback. Original local bytes are SHA-256 compared to HEAD in per-library hashes.tsv. Four bridge classes may intentionally resolve to the host's implementation after playback omission.\n\n**This closes only the assigned COR-06 instantiation slice. It does not supply or certify per-abstraction inlet/outlet/audio contracts.** Dynamic $n classes without creation arguments may fail honestly. Full exact console logs are the adjacent *-abstractions.console.txt files.\n\n{}\n{}", fs::read_to_string(out.join("HEAD-abstractions.md"))?, abstraction_sections.join("\n")))?;
    let mut external_report = "# COR-08: host external qualification\n\nHost macOS arm64 only; no Android dlopen, packaging or 16 KB alignment claim. All five separate bundles were compiled from ScenePlayer/jni/*.c; commands, compiler output, Mach-O architecture, source/binary hashes are in externals.build.txt. Pinned s_loader.c lines 83–86 tries .d_arm64, .d_fat and .pd_darwin (after deken extensions); this run uses .pd_darwin and the engine's real loader.\n\nEvery isolated probe uses a top-level adc~ into a reblocked inlet~ subpatch, never a naked unsafe 64-frame object. Fixed input: 1 s 440 Hz 0.25-peak sine, then 1 s xorshift32 noise seed 0x12345678 at 0.25 peak. Accumulator receives env~ 512 floats with hopsize 512; centroid/energy/barkflux receive the magnitude of a 1024-point real FFT; zcr receives waveform blocks. Contract: at least one float outlet message and every observed float finite/non-NaN; exact values retained in *.probe.events.json. This finite-output smoke contract is not algorithm-accuracy certification.\n\nOriginal unmodified help patches are opened/closed with DSP **disabled**, including the unsafe naked rj_zcr~ object at top level. Help loading is not execution certification.\n\n| External | Probe opens | Errors | Finite output contract | Float count | Min | Max | Help opens (DSP off) | Help errors |\n|---|---|---:|---|---:|---:|---:|---|---:|\n".to_string();
    for name in EXTERNALS {
        let args = vec![
            "--worker-external".into(),
            name.into(),
            path(&bridges),
            path(&ext),
            path(&out),
            path(&corpus),
        ];
        let success = child(&args, &out, &format!("{name}.external"))?;
        let probe = fs::read_to_string(out.join(format!("{name}.probe.result")))?;
        let help = fs::read_to_string(out.join(format!("{name}.help.result")))?;
        external_report.push_str(&format!(
            "| {name} | {} | {} |\n",
            probe.trim().split('\t').collect::<Vec<_>>().join(" | "),
            help.trim().split('\t').collect::<Vec<_>>().join(" | ")
        ));
        if !success {
            external_report.push_str(&format!(
                "\n{name}: worker failed its output assertion; consult {name}.external.host.txt.\n"
            ));
        }
    }
    external_report.push_str("\n## Preserved out-of-bounds hazards\n\n- rj_centroid~.c:8,38–50 and rj_senergy~.c:8,38–43 hard-read 513 input samples while ignoring the actual vector size. A 64-frame or 512-frame direct DSP vector is too short; both probes use block~ 1024.\n- rj_zcr~.c:46–54 accumulates into an internal buffer but mistakenly computes crossings from in[0…1023], not that buffer. It can therefore read out of bounds whenever the current vector is <1024, including the naked object in rj_zcr~-help.pd:21. Its 1024-frame subpatches are safe for this memory bound; no engine fix was made.\n- rj_barkflux_accum~ checks its 513-sample minimum but does not guard every filter index against vector size; the probe uses the help patch's 1024-point FFT size. rj_accum also assumes valid hopsize/durations for allocations/divisions; only its documented 512 hop was used.\n\nClean help loading means `opens=true, errors=0` in the table; exact full console output is in each *.help.console.txt. No original help was processed under an unsafe vector size. Licensing remains unstated in these C sources.\n");
    fs::write(out.join("externals.md"), external_report)?;
    if !child(
        &[
            "--worker-pd-tilde".into(),
            path(&bridges),
            path(&ext),
            path(&out),
        ],
        &out,
        "pd-tilde",
    )? {
        return Err("pd~ worker failed".into());
    }
    summary.push_str(&format!("\nStatic pd~ occurrences across all five scene source trees (including unused/help libraries): **{all_scene_pd_tilde}**. See [pd-tilde.md](pd-tilde.md) for real class instantiation and absent-executable start output.\n\n[abstractions.md](abstractions.md): all HEAD and local non-help rj/ instantiations + interfaces/hashes. [externals.md](externals.md): five actual host builds/isolated finite-output probes and DSP-off help loads; OOB hazards preserved.\n\nScope limits: no personal collection, no proprietary original binary, no device playback/overlays/recording, no per-abstraction musical behavior certification. A silent result or console errors are retained, never replaced by fallback audio or stubs. Source scenes and engine src/tests were not edited.\n"));
    fs::write(out.join("summary.md"), summary)?;
    println!("Qualification artifacts: {}", out.display());
    Ok(())
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("--worker-scene") if args.len() == 7 => scene_worker(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
            &args[5],
            &args[6],
        ),
        Some("--worker-abstractions") if args.len() == 9 => abstraction_worker(
            Path::new(&args[1]),
            Path::new(&args[2]),
            args[3] == "scene",
            Path::new(&args[4]),
            Path::new(&args[5]),
            Path::new(&args[6]),
            &args[7],
            Path::new(&args[8]),
        ),
        Some("--worker-external") if args.len() == 6 => external_worker(
            &args[1],
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
            Path::new(&args[5]),
        ),
        Some("--worker-pd-tilde") if args.len() == 4 => pd_tilde_worker(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
        ),
        Some("--out") if args.len() == 2 => {
            checks();
            run(Path::new(&args[1]))
        }
        _ => Err("Usage: cargo run --example qualify -- --out <artifact-directory>".into()),
    }
}
