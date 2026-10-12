//! Per-abstraction contracts in the real offline libpd engine (not help-patch certification).
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::{c_void, CString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, MutexGuard};
use std::time::{Duration, Instant};

use rjdjcore::pd::{self, ffi, Atom, Engine, Patch, PdEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
const RATE: usize = 22050;
const BLOCK: usize = 64;
const EXACT_TOLERANCE: f64 = 1e-6;
const EXTRAS: [&str; 7] = [
    "complex-mod~", "hilbert~", "rev1-final", "rev1-stage", "rev1~", "rev2~", "rev3~",
];

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum Argument {
    Number(f64),
    Text(String),
}
impl Argument {
    fn text(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    name: String,
    #[serde(default)]
    args: Vec<Argument>,
    source: String,
    duration_ms: f64,
    stimuli: Vec<Stimulus>,
    expect: Vec<Expectation>,
    notes: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    tables: Vec<Table>,
    #[serde(default)]
    fixtures: Vec<Fixture>,
    #[serde(default)]
    feedback: Vec<Feedback>,
    expected_result: Option<String>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stimulus {
    inlet: Option<usize>,
    receiver: Option<String>,
    #[serde(default)]
    at_ms: f64,
    signal: Option<Signal>,
    message: Option<Vec<Argument>>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    kind: SignalKind,
    freq: Option<f64>,
    amp: Option<f64>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum SignalKind {
    Sine,
    Noise,
    Dc,
    Silence,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Signal,
    Control,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Expectation {
    outlet: usize,
    kind: Kind,
    rms: Option<[f64; 2]>,
    window_ms: Option<[f64; 2]>,
    centroid_hz: Option<[f64; 2]>,
    #[serde(default)]
    silent: bool,
    changes_after_ms: Option<f64>,
    min_messages: Option<usize>,
    #[serde(rename = "type")]
    message_type: Option<String>,
    range: Option<[f64; 2]>,
    exact: Option<Vec<Vec<Argument>>>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Table {
    name: String,
    file: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    object: String,
    signal: Option<Signal>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Feedback {
    outlet: usize,
    inlet: usize,
    object: String,
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Result<Self> {
        for n in 0..10000 {
            let path = std::env::temp_dir().join(format!("rjdj-contracts-{}-{n}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err("Cannot create contract scratch directory".into())
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn repo() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?)
}
fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), found)?;
            } else if entry.file_type()?.is_file() {
                found.push(entry.path());
            }
        }
        Ok(())
    }
    walk(dir, &mut found)?;
    found.sort();
    Ok(found)
}
fn unzip(source: &Path, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(source)?)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.enclosed_name().ok_or("ZIP entry escapes extraction directory")?;
        if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err("ZIP contains a symlink".into());
        }
        let path = dest.join(name);
        if entry.is_dir() {
            fs::create_dir_all(path)?;
        } else {
            fs::create_dir_all(path.parent().ok_or("ZIP entry has no parent")?)?;
            io::copy(&mut entry, &mut File::create(path)?)?;
        }
    }
    Ok(())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "_#~.-".contains(c))
}
fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("Fixture path must stay relative: {}", path.display()).into());
    }
    Ok(path)
}

// Pd records can span lines; only unescaped semicolons terminate them.
fn records(text: &str) -> Result<Vec<(usize, String)>> {
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
    if !record.trim().is_empty() {
        return Err(format!("Unterminated Pd record at line {start}").into());
    }
    Ok(result)
}
fn tokens(record: &str) -> Vec<String> {
    let mut result = Vec::new();
    let (mut token, mut escaped) = (String::new(), false);
    for ch in record.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !token.is_empty() {
                result.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        result.push(token);
    }
    result
}
fn ports(path: &Path) -> Result<(Vec<Kind>, Vec<Kind>)> {
    let (mut ins, mut outs, mut depth) = (Vec::new(), Vec::new(), 0usize);
    for (_, record) in records(&fs::read_to_string(path)?)? {
        let t = tokens(&record);
        if t.get(1).is_some_and(|s| t[0] == "#N" && s == "canvas") {
            depth += 1;
        } else if t.get(1).is_some_and(|s| t[0] == "#X" && s == "restore") {
            depth = depth.checked_sub(1).ok_or("Invalid Pd canvas nesting")?;
        } else if depth == 1 && t.len() >= 5 && t[0] == "#X" && t[1] == "obj" {
            let x: f64 = t[2].parse()?;
            match t[4].as_str() {
                "inlet" => ins.push((x, Kind::Control)),
                "inlet~" => ins.push((x, Kind::Signal)),
                "outlet" => outs.push((x, Kind::Control)),
                "outlet~" => outs.push((x, Kind::Signal)),
                _ => {}
            }
        }
    }
    ins.sort_by(|a, b| a.0.total_cmp(&b.0));
    outs.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok((ins.into_iter().map(|p| p.1).collect(), outs.into_iter().map(|p| p.1).collect()))
}
fn validate_signal(signal: &Signal) -> Result<()> {
    if signal.amp.is_some_and(|n| !n.is_finite() || (n as f32).is_infinite())
        || signal.freq.is_some_and(|n| !n.is_finite() || !(0.0..=RATE as f64 / 2.0).contains(&n))
        || matches!(signal.kind, SignalKind::Sine) && signal.freq.is_none()
    {
        return Err("Signal must have finite amplitude and a sine frequency in [0, Nyquist]".into());
    }
    Ok(())
}
fn validate(contract: &Contract, ins: &[Kind], outs: &[Kind]) -> Result<()> {
    if !valid_name(&contract.name) || contract.source.is_empty() || contract.notes.trim().is_empty() {
        return Err("Contract needs a safe name, source evidence and notes (including inlet omissions)".into());
    }
    if !contract.duration_ms.is_finite() || contract.duration_ms <= 0.0 {
        return Err("duration_ms must be positive and finite".into());
    }
    if contract.expected_result.as_deref().is_some_and(|s| s != "instantiates-with-errors") {
        return Err("Only expected_result=instantiates-with-errors is supported".into());
    }
    for stimulus in &contract.stimuli {
        if stimulus.inlet.is_some() == stimulus.receiver.is_some()
            || stimulus.signal.is_some() == stimulus.message.is_some()
            || !stimulus.at_ms.is_finite()
            || !(0.0..contract.duration_ms).contains(&stimulus.at_ms)
        {
            return Err("Stimulus needs exactly one inlet/receiver and one signal/message, with at_ms inside duration".into());
        }
        if let Some(inlet) = stimulus.inlet {
            if inlet >= ins.len() {
                return Err(format!("Stimulus inlet {inlet} does not exist ({} inlets)", ins.len()).into());
            }
            if stimulus.signal.is_some() && ins[inlet] != Kind::Signal {
                return Err(format!("Signal stimulus targets control inlet {inlet}").into());
            }
        }
        if let Some(signal) = &stimulus.signal {
            if stimulus.receiver.is_some() {
                return Err("Receiver stimuli must be messages, not signals".into());
            }
            validate_signal(signal)?;
        }
        if stimulus.message.as_ref().is_some_and(Vec::is_empty) {
            return Err("Empty stimulus message; use [\"bang\"] or [\"list\"]".into());
        }
    }
    let mut checked = BTreeSet::new();
    for expect in &contract.expect {
        if expect.outlet >= outs.len() || outs[expect.outlet] != expect.kind {
            return Err(format!("Expectation outlet {} does not exist or has wrong kind", expect.outlet).into());
        }
        checked.insert(expect.outlet);
        for bounds in [expect.rms, expect.centroid_hz, expect.range, expect.window_ms].into_iter().flatten() {
            if !bounds.iter().all(|v| v.is_finite()) || bounds[0] > bounds[1] {
                return Err("Expectation bounds must be finite and ordered".into());
            }
        }
        if let Some([lo, hi]) = expect.window_ms {
            if lo < 0.0 || lo >= hi || hi > contract.duration_ms {
                return Err("window_ms must be nonempty and inside duration".into());
            }
        }
        if expect.changes_after_ms.is_some_and(|n| !n.is_finite() || n <= 0.0 || n >= contract.duration_ms) {
            return Err("changes_after_ms must split the render into two nonempty windows".into());
        }
        if expect.message_type.as_deref().is_some_and(|s| !["float", "bang", "symbol", "list", "any"].contains(&s)) {
            return Err("Unsupported control message type".into());
        }
        if expect.kind == Kind::Signal {
            if expect.rms.is_none() && !expect.silent && expect.centroid_hz.is_none() && expect.changes_after_ms.is_none() {
                return Err("Signal expectation needs a measurement assertion".into());
            }
            if expect.min_messages.is_some() || expect.message_type.is_some() || expect.range.is_some() || expect.exact.is_some() {
                return Err("Control assertion attached to a signal outlet".into());
            }
        } else if expect.rms.is_some() || expect.centroid_hz.is_some() || expect.changes_after_ms.is_some() || expect.silent || expect.window_ms.is_some() {
            return Err("Signal assertion attached to a control outlet".into());
        }
    }
    if checked.len() != outs.len() {
        return Err(format!("Every outlet needs an expectation ({} of {} covered)", checked.len(), outs.len()).into());
    }
    for fixture in &contract.fixtures {
        if fixture.object.trim().is_empty() || fixture.object.contains([';', ',', '\n', '\r', '\0']) {
            return Err("Fixture object must be a single Pd object declaration".into());
        }
        if let Some(signal) = &fixture.signal {
            validate_signal(signal)?;
        }
    }
    for feedback in &contract.feedback {
        if feedback.outlet >= outs.len() || feedback.inlet >= ins.len()
            || feedback.object.trim().is_empty() || feedback.object.contains([';', ',', '\n', '\r', '\0'])
        {
            return Err("Feedback needs existing ports and a single Pd operation".into());
        }
    }
    for table in &contract.tables {
        if !valid_name(&table.name) {
            return Err("Fixture table name must be a single safe Pd symbol".into());
        }
    }
    Ok(())
}
fn pd_atom(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        return Err("Pd atom contains a newline or NUL".into());
    }
    let mut escaped = String::new();
    for ch in value.chars() {
        if ch.is_whitespace() || "\\$;,".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    if escaped.is_empty() {
        return Err("Empty Pd creation atom".into());
    }
    Ok(escaped)
}

struct Host {
    objects: Vec<String>,
    connections: Vec<String>,
}
impl Host {
    fn new() -> Self { Self { objects: Vec::new(), connections: Vec::new() } }
    fn add(&mut self, kind: &str, body: &str) -> usize {
        let index = self.objects.len();
        self.objects.push(format!("#X {kind} 20 {} {body};", 20 + index * 20));
        index
    }
    fn object(&mut self, body: &str) -> usize { self.add("obj", body) }
    fn connect(&mut self, from: usize, outlet: usize, to: usize, inlet: usize) {
        self.connections.push(format!("#X connect {from} {outlet} {to} {inlet};"));
    }
    fn signal(&mut self, signal: &Signal, amplitude_receiver: Option<&str>) -> usize {
        let generator = self.object(&match signal.kind {
            SignalKind::Sine => format!("osc~ {}", signal.freq.unwrap_or(440.0)),
            SignalKind::Noise => "noise~".into(),
            SignalKind::Dc => "sig~ 1".into(),
            SignalKind::Silence => "sig~ 0".into(),
        });
        if matches!(signal.kind, SignalKind::Noise) {
            let load = self.object("loadbang");
            let seed = self.add("msg", "seed 1");
            self.connect(load, 0, seed, 0);
            self.connect(seed, 0, generator, 0);
        }
        let amp = if amplitude_receiver.is_some() { 0.0 } else { signal.amp.unwrap_or(1.0) };
        let gain = self.object(&format!("*~ {amp}"));
        self.connect(generator, 0, gain, 0);
        if let Some(receiver) = amplitude_receiver {
            let control = self.object(&format!("r {receiver}"));
            self.connect(control, 0, gain, 1);
        }
        gain
    }
    fn finish(self) -> String {
        format!("#N canvas 0 0 900 700 10;\n{}\n{}\n", self.objects.join("\n"), self.connections.join("\n"))
    }
}
fn file_name(file: &str) -> &str { file.strip_prefix("generated:").unwrap_or(file) }
fn host(contract: &Contract, ins: &[Kind], outs: &[Kind], signal_outlet: Option<usize>) -> Result<String> {
    let mut host = Host::new();
    for table in &contract.tables {
        host.object(&format!("array define {}", pd_atom(&table.name)?));
        let load = host.object("loadbang");
        let read = host.add("msg", &format!("read -resize {} {}", pd_atom(file_name(&table.file))?, pd_atom(&table.name)?));
        let soundfiler = host.object("soundfiler");
        host.connect(load, 0, read, 0);
        host.connect(read, 0, soundfiler, 0);
    }
    for (index, fixture) in contract.fixtures.iter().enumerate() {
        let object = tokens(&fixture.object).iter().map(|s| pd_atom(s)).collect::<Result<Vec<_>>>()?.join(" ");
        let target = host.object(&object);
        let source = match &fixture.signal {
            Some(signal) => host.signal(signal, None),
            None => host.object(&format!("r rjqa_fixture{index}")),
        };
        host.connect(source, 0, target, 0);
    }
    let args = contract.args.iter().map(|arg| pd_atom(&arg.text())).collect::<Result<Vec<_>>>()?.join(" ");
    let abstraction = host.object(&format!("{} {args}", pd_atom(&contract.name)?));
    for feedback in &contract.feedback {
        let object = tokens(&feedback.object).iter().map(|s| pd_atom(s)).collect::<Result<Vec<_>>>()?.join(" ");
        let operation = host.object(&object);
        host.connect(abstraction, feedback.outlet, operation, 0);
        host.connect(operation, 0, abstraction, feedback.inlet);
    }
    for inlet in 0..ins.len() {
        let receiver = host.object(&format!("r rjqa_in{inlet}"));
        host.connect(receiver, 0, abstraction, inlet);
    }
    for (index, stimulus) in contract.stimuli.iter().enumerate() {
        if let (Some(inlet), Some(signal)) = (stimulus.inlet, &stimulus.signal) {
            let source = host.signal(signal, Some(&format!("rjqa_gain{index}")));
            host.connect(source, 0, abstraction, inlet);
        }
    }
    for (outlet, kind) in outs.iter().enumerate() {
        if *kind == Kind::Signal {
            if signal_outlet == Some(outlet) {
                let gain = host.object("*~ 1");
                let dac = host.object("dac~ 1 2");
                host.connect(abstraction, outlet, gain, 0);
                host.connect(gain, 0, dac, 0);
                host.connect(gain, 0, dac, 1);
            }
        } else {
            // Preserve message type before list prepend normalizes bang/float/symbol into lists.
            let route = host.object("route bang float symbol list");
            host.connect(abstraction, outlet, route, 0);
            for (branch, tag) in ["bang", "float", "symbol", "list", "message"].iter().enumerate() {
                let typed = host.object(&format!("list prepend rjqa_{tag}"));
                let labelled = host.object(&format!("list prepend out{outlet}"));
                let trim = host.object("list trim");
                let send = host.object("s rjqa_out");
                host.connect(route, branch, typed, 0);
                host.connect(typed, 0, labelled, 0);
                host.connect(labelled, 0, trim, 0);
                host.connect(trim, 0, send, 0);
            }
        }
    }
    Ok(host.finish())
}
fn write_sine_wav(path: &Path, stereo: bool) -> Result<()> {
    let channels: u16 = if stereo { 2 } else { 1 };
    let bytes = RATE as u32 * u32::from(channels) * 2;
    let mut file = File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&(RATE as u32).to_le_bytes())?;
    file.write_all(&(RATE as u32 * u32::from(channels) * 2).to_le_bytes())?;
    file.write_all(&(channels * 2).to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    let mut data = Vec::with_capacity(bytes as usize);
    for frame in 0..RATE {
        let sample = (0.5 * (std::f64::consts::TAU * 440.0 * frame as f64 / RATE as f64).sin() * i16::MAX as f64).round() as i16;
        for _ in 0..channels { data.extend_from_slice(&sample.to_le_bytes()); }
    }
    file.write_all(&data)?;
    Ok(())
}
fn prepare_files(contract: &Contract, contract_path: &Path, rjlib: &Path, dest: &Path) -> Result<()> {
    let requested: BTreeSet<&str> = contract.files.iter().map(String::as_str)
        .chain(contract.tables.iter().map(|t| t.file.as_str())).collect();
    for file in requested {
        let name = relative(file_name(file))?;
        let target = dest.join(name);
        fs::create_dir_all(target.parent().ok_or("Fixture has no parent")?)?;
        if let Some(generated) = file.strip_prefix("generated:") {
            match generated {
                "sine440_1s.wav" => write_sine_wav(&target, false)?,
                "sine440_1s_stereo.wav" => write_sine_wav(&target, true)?,
                "seq-midi.txt" => fs::write(&target, "0 144 60 100;\n100 128 60 0;\n")?,
                "index.txt" => fs::write(&target, "sine440_1s.wav 0\n")?,
                "index_stereo.txt" => fs::write(&target, "sine440_1s_stereo.wav 0\n")?,
                _ => return Err(format!("Unknown generated fixture: {file}").into()),
            }
        } else {
            let candidates = [
                contract_path.parent().ok_or("Contract has no parent")?.join(name),
                rjlib.join(name),
                rjlib.join("rj").join(name),
            ];
            let source = candidates.iter().find(|p| p.is_file()).ok_or_else(|| format!("Fixture is unavailable: {file}"))?;
            fs::copy(source, target)?;
        }
    }
    Ok(())
}

struct Session {
    engine: MutexGuard<'static, Engine>,
    patch: Option<Patch>,
    binding: *mut c_void,
    receiver: mpsc::Receiver<PdEvent>,
}
impl Session {
    fn new(paths: &[&Path]) -> Result<Self> {
        let mut engine = Engine::global().lock().unwrap_or_else(|e| e.into_inner());
        let (sender, receiver) = mpsc::channel();
        pd::set_event_sink(sender);
        engine.init()?;
        engine.set_search_paths(paths)?;
        engine.init_audio(1, 2, RATE as i32)?;
        // SAFETY: this offline owner holds the engine lock; bind precedes patch loadbang.
        let binding = unsafe { ffi::libpd_bind(c"rjqa_out".as_ptr()) };
        if binding.is_null() { return Err("Cannot bind rjqa_out".into()); }
        Ok(Self { engine, patch: None, binding, receiver })
    }
    fn collect(&self, at_ms: f64, console: &mut Vec<Value>, messages: &mut Vec<Value>) {
        pd::drain_queued();
        for event in self.receiver.try_iter() {
            match event {
                PdEvent::Print(line) => console.push(json!({"at_ms":at_ms,"line":line})),
                other => if let Some(message) = decode_message(other, at_ms) { messages.push(message); },
            }
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(patch) = self.patch.take() { self.engine.close_patch(patch); }
        // SAFETY: processing has stopped and this is the live binding owned by this session.
        unsafe { ffi::libpd_unbind(self.binding); }
        pd::drain_queued();
    }
}
fn decode_message(event: PdEvent, at_ms: f64) -> Option<Value> {
    let (receiver, selector, atoms) = match event {
        PdEvent::Message { recv, msg, atoms } => (recv, Some(msg), atoms),
        PdEvent::List { recv, atoms } => (recv, None, atoms),
        _ => return None,
    };
    if receiver != "rjqa_out" { return None; }
    let mut body: Vec<Value> = Vec::with_capacity(atoms.len() + 1);
    if let Some(selector) = selector { body.push(Value::String(selector)); }
    for atom in atoms {
        body.push(match atom { Atom::Float(f) => json!(f), Atom::Symbol(s) => json!(s) });
    }
    let outlet = body.first()?.as_str()?.strip_prefix("out")?.parse::<usize>().ok()?;
    let tag = body.get(1)?.as_str()?.strip_prefix("rjqa_")?;
    let kind = if tag == "message" { "list" } else { tag };
    let values = if tag == "bang" { vec![json!("bang")] } else { body[2..].to_vec() };
    Some(json!({"at_ms":at_ms,"outlet":outlet,"type":kind,"atoms":values}))
}
fn send(receiver: &str, message: &[Argument]) -> Result<()> {
    let receiver = CString::new(receiver)?;
    let symbols = message.iter().map(|a| match a {
        Argument::Text(s) => Ok(Some(CString::new(s.as_str())?)),
        Argument::Number(n) if n.is_finite() && (*n as f32).is_finite() => Ok(None),
        _ => Err("Message number is outside finite f32 range".into()),
    }).collect::<Result<Vec<_>>>()?;
    // SAFETY: the offline session exclusively owns libpd; all C strings survive their FFI calls.
    let result = unsafe {
        match message {
            [Argument::Number(n)] => ffi::libpd_float(receiver.as_ptr(), *n as f32),
            [Argument::Text(s)] if s == "bang" => ffi::libpd_bang(receiver.as_ptr()),
            [Argument::Text(s), Argument::Text(_)] if s == "symbol" => ffi::libpd_symbol(receiver.as_ptr(), symbols[1].as_ref().ok_or("Missing symbol")?.as_ptr()),
            _ => {
                let selector = match message.first() { Some(Argument::Text(s)) => Some(s.as_str()), _ => None };
                let start = usize::from(selector.is_some());
                if ffi::libpd_start_message((message.len() - start).try_into()?) != 0 {
                    return Err("libpd_start_message failed".into());
                }
                for index in start..message.len() {
                    match &message[index] {
                        Argument::Number(n) => ffi::libpd_add_float(*n as f32),
                        Argument::Text(_) => ffi::libpd_add_symbol(symbols[index].as_ref().ok_or("Missing symbol")?.as_ptr()),
                    }
                }
                if let Some(selector) = selector {
                    if selector == "list" { ffi::libpd_finish_list(receiver.as_ptr()) }
                    else { ffi::libpd_finish_message(receiver.as_ptr(), symbols[0].as_ref().ok_or("Missing selector")?.as_ptr()) }
                } else { ffi::libpd_finish_list(receiver.as_ptr()) }
            }
        }
    };
    // Sending to an absent scene-global receiver is a contract failure, not a silently dropped stimulus.
    if result != 0 { return Err(format!("Pd rejected stimulus to {} (code {result})", receiver.to_string_lossy()).into()); }
    Ok(())
}
fn sample_index(ms: f64) -> usize { (ms * RATE as f64 / 1000.0).floor() as usize }
fn tick_index(ms: f64) -> usize { sample_index(ms) / BLOCK }
fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() { return f64::NAN; }
    (samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}
fn changes_after(samples: &[f32], split: usize) -> (f64, f64, bool) {
    if split == 0 || split >= samples.len() { return (f64::NAN, f64::NAN, false); }
    let before = rms(&samples[..split]);
    let after = rms(&samples[split..]);
    (before, after, before.is_finite() && after.is_finite() && (after - before).abs() > before * 0.2)
}

// Same 1024-point, Hann-windowed magnitude centroid convention as qualify.rs; FFT avoids quadratic DFT work.
struct Spectrum { real: [f64; 1024], imag: [f64; 1024], window: [f64; 1024], twiddle: [(f64, f64); 512] }
impl Spectrum {
    fn new() -> Self {
        Self {
            real: [0.0; 1024], imag: [0.0; 1024],
            window: std::array::from_fn(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / 1023.0).cos()),
            twiddle: std::array::from_fn(|n| { let (sin, cos) = (-std::f64::consts::TAU * n as f64 / 1024.0).sin_cos(); (cos, sin) }),
        }
    }
    fn centroid(&mut self, samples: &[f32]) -> f64 {
        if samples.is_empty() || samples.iter().any(|s| !s.is_finite()) { return f64::NAN; }
        let (mut weighted, mut total) = (0.0, 0.0);
        for start in (0..samples.len()).step_by(512) {
            for n in 0..1024 {
                self.real[n] = f64::from(samples.get(start + n).copied().unwrap_or(0.0)) * self.window[n];
                self.imag[n] = 0.0;
            }
            for n in 0usize..1024 {
                let reverse = n.reverse_bits() >> (usize::BITS - 10);
                if n < reverse { self.real.swap(n, reverse); self.imag.swap(n, reverse); }
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
        if total == 0.0 { 0.0 } else { weighted / total }
    }
}
fn in_range(value: f64, range: [f64; 2]) -> bool { value.is_finite() && value >= range[0] && value <= range[1] }
fn exact_atom(actual: &Value, expected: &Argument) -> bool {
    match expected {
        Argument::Text(text) => actual.as_str() == Some(text),
        Argument::Number(n) => actual.as_f64().is_some_and(|v| v.is_finite() && (v - n).abs() <= EXACT_TOLERANCE * n.abs().max(1.0)),
    }
}
fn evaluate(expect: &Expectation, contract: &Contract, samples: &[f32], messages: &[Value], spectrum: &mut Spectrum) -> Value {
    let mut failures = Vec::new();
    let mut measured = serde_json::Map::new();
    if expect.kind == Kind::Signal {
        let [start, end] = expect.window_ms.unwrap_or([0.0, contract.duration_ms]);
        let window = &samples[sample_index(start).min(samples.len())..sample_index(end).min(samples.len())];
        let value = rms(window);
        measured.insert("window_ms".into(), json!([start,end]));
        measured.insert("samples".into(), json!(window.len()));
        measured.insert("rms".into(), json!(value));
        if window.is_empty() || window.iter().any(|s| !s.is_finite()) { failures.push("empty or non-finite signal window".to_owned()); }
        if let Some(bounds) = expect.rms {
            if !in_range(value, bounds) { failures.push(format!("RMS {value} outside [{}, {}]", bounds[0], bounds[1])); }
        }
        if expect.silent && !(value.is_finite() && value < 1e-4) { failures.push(format!("not silent: RMS {value} >= 0.0001")); }
        if let Some(bounds) = expect.centroid_hz {
            let value = spectrum.centroid(window);
            measured.insert("centroid_hz".into(), json!(value));
            if !in_range(value, bounds) { failures.push(format!("centroid {value} Hz outside [{}, {}]", bounds[0], bounds[1])); }
        }
        if let Some(ms) = expect.changes_after_ms {
            let (before, after, changed) = changes_after(samples, sample_index(ms));
            measured.insert("changes_after".into(), json!({"at_ms":ms,"before_rms":before,"after_rms":after,"changed_gt_20_percent":changed}));
            if !changed { failures.push(format!("RMS change at {ms} ms is not >20% ({before} -> {after})")); }
        }
    } else {
        let observed: Vec<&Value> = messages.iter().filter(|m| m["outlet"].as_u64() == Some(expect.outlet as u64)).collect();
        measured.insert("message_count".into(), json!(observed.len()));
        measured.insert("messages".into(), json!(observed));
        let minimum = expect.min_messages.unwrap_or(0);
        if observed.len() < minimum { failures.push(format!("{} messages, expected at least {minimum}", observed.len())); }
        if expect.min_messages == Some(0) && expect.message_type.is_none() && expect.range.is_none() && expect.exact.is_none() && !observed.is_empty() {
            failures.push(format!("expected a quiet outlet, got {} messages", observed.len()));
        }
        if let Some(kind) = &expect.message_type {
            if kind != "any" && observed.iter().any(|m| m["type"].as_str() != Some(kind.as_str())) {
                failures.push(format!("message type is not consistently {kind}"));
            }
        }
        if let Some(bounds) = expect.range {
            for (index, message) in observed.iter().enumerate() {
                let numbers: Vec<f64> = message["atoms"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
                if numbers.is_empty() || numbers.iter().any(|&n| !in_range(n, bounds)) {
                    failures.push(format!("message {index} has no numeric value or is outside [{}, {}]", bounds[0], bounds[1]));
                }
            }
        }
        if let Some(exact) = &expect.exact {
            if observed.len() < exact.len() { failures.push(format!("exact prefix needs {} messages, got {}", exact.len(), observed.len())); }
            for (index, expected) in exact.iter().enumerate() {
                if let Some(actual) = observed.get(index).and_then(|m| m["atoms"].as_array()) {
                    if actual.len() != expected.len() || !actual.iter().zip(expected).all(|(a,e)| exact_atom(a,e)) {
                        failures.push(format!("message {index}: expected {}, got {}", serde_json::to_string(expected).unwrap_or_default(), json!(actual)));
                    }
                }
            }
        }
    }
    json!({"outlet":expect.outlet,"kind":expect.kind,"assertion":expect,"measured":measured,"pass":failures.is_empty(),"failures":failures})
}
fn instantiation_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("couldn't create") || lower.contains("no such object") || lower.contains("connection failed")
}
fn worker(contract_path: &Path, rjlib: &Path, bridges: &Path) -> Result<Value> {
    let contract: Contract = serde_json::from_slice(&fs::read(contract_path)?)?;
    let (ins, outs) = ports(&rjlib.join("rj").join(format!("{}.pd", contract.name)))?;
    validate(&contract, &ins, &outs)?;
    let scratch = TempDir::new()?;
    prepare_files(&contract, contract_path, rjlib, &scratch.0)?;
    let audio_outlets: Vec<usize> = outs.iter().enumerate().filter_map(|(n,k)| (*k == Kind::Signal).then_some(n)).collect();
    let passes: Vec<Option<usize>> = if audio_outlets.is_empty() { vec![None] } else { audio_outlets.into_iter().map(Some).collect() };
    let mut reports = Vec::new();
    let mut failures = Vec::new();
    let mut has_errors = false;
    let mut spectrum = Spectrum::new();
    let frames = sample_index(contract.duration_ms);
    let ticks = frames.div_ceil(BLOCK);
    let mut schedule: Vec<usize> = (0..contract.stimuli.len()).collect();
    schedule.sort_by_key(|&i| tick_index(contract.stimuli[i].at_ms));
    for signal_outlet in passes {
        let wrapper = host(&contract, &ins, &outs, signal_outlet)?;
        fs::write(scratch.0.join("_main.pd"), &wrapper)?;
        let mut console = Vec::new();
        let mut messages = Vec::new();
        let mut samples = Vec::with_capacity(ticks * BLOCK);
        let rj = rjlib.join("rj");
        let bridge_abs = bridges.join("abs");
        let mut session = Session::new(&[&scratch.0, &rj, bridges, &bridge_abs])?;
        session.patch = Some(session.engine.open_patch(&scratch.0, "_main.pd")?);
        session.collect(0.0, &mut console, &mut messages);
        let mut cursor = 0;
        let mut active_signal: BTreeMap<usize, usize> = BTreeMap::new();
        let input = [0.0f32; BLOCK];
        let mut output = [0.0f32; BLOCK * 2];
        for tick in 0..ticks {
            while cursor < schedule.len() && tick_index(contract.stimuli[schedule[cursor]].at_ms) <= tick {
                let index = schedule[cursor];
                let stimulus = &contract.stimuli[index];
                let sent = if let Some(signal) = &stimulus.signal {
                    let inlet = stimulus.inlet.ok_or("Signal has no inlet")?;
                    if let Some(previous) = active_signal.insert(inlet, index) {
                        send(&format!("rjqa_gain{previous}"), &[Argument::Number(0.0)])?;
                    }
                    send(&format!("rjqa_gain{index}"), &[Argument::Number(signal.amp.unwrap_or(1.0))])
                } else {
                    let receiver = stimulus.receiver.clone().unwrap_or_else(|| format!("rjqa_in{}", stimulus.inlet.unwrap_or(0)));
                    send(&receiver, stimulus.message.as_deref().ok_or("Message is absent")?)
                };
                if let Err(error) = sent { failures.push(format!("pass {signal_outlet:?}, stimulus {index}: {error}")); }
                cursor += 1;
            }
            if pd::process(1, &input, &mut output) != 0 { return Err("libpd_process_float failed".into()); }
            samples.extend(output.chunks_exact(2).map(|frame| frame[0]));
            session.collect((tick * BLOCK) as f64 * 1000.0 / RATE as f64, &mut console, &mut messages);
        }
        samples.truncate(frames);
        if let Some(patch) = session.patch.take() { session.engine.close_patch(patch); }
        session.collect(contract.duration_ms, &mut console, &mut messages);
        has_errors |= console.iter().any(|line| line["line"].as_str().is_some_and(instantiation_error));
        let checks: Vec<Value> = contract.expect.iter()
            .filter(|expect| expect.kind == Kind::Control || signal_outlet == Some(expect.outlet))
            .map(|expect| evaluate(expect, &contract, &samples, &messages, &mut spectrum)).collect();
        for check in &checks {
            for failure in check["failures"].as_array().into_iter().flatten() {
                failures.push(format!("pass {signal_outlet:?}, outlet {}: {}", check["outlet"], failure.as_str().unwrap_or("assertion failed")));
            }
        }
        reports.push(json!({"signal_outlet":signal_outlet,"rendered_frames":frames,"ticks":ticks,"console":console,"messages":messages,"expectations":checks,"host_patch":wrapper}));
    }
    let instantiates = if has_errors { "instantiates-with-errors" } else { "instantiates" };
    if has_errors && contract.expected_result.as_deref() != Some("instantiates-with-errors") {
        failures.push("unexpected unresolved Pd class (see console)".into());
    }
    let omitted: Vec<usize> = (0..ins.len()).filter(|&i| !contract.stimuli.iter().any(|s| s.inlet == Some(i))).collect();
    Ok(json!({"name":contract.name,"contract":contract,"inlets":ins,"outlets":outs,"omitted_inlets":omitted,"instantiates":instantiates,"pass":failures.is_empty(),"failures":failures,"notes":contract.notes,"passes":reports}))
}
fn failed(name: &str, detail: &str) -> Value {
    json!({"name":name,"instantiates":"not-tested","pass":false,"failures":[detail],"notes":detail,"passes":[]})
}
fn run_child(contract_path: &Path, rjlib: &Path, bridges: &Path, scratch: &Path, name: &str, duration_ms: f64) -> Result<Value> {
    let report_path = scratch.join(format!("{name}.json"));
    let stdout_path = scratch.join(format!("{name}.stdout"));
    let stderr_path = scratch.join(format!("{name}.stderr"));
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--worker").arg(contract_path).arg("--rjlib").arg(rjlib)
        .arg("--bridges").arg(bridges).arg("--out").arg(&report_path)
        .stdout(Stdio::from(File::create(&stdout_path)?))
        .stderr(Stdio::from(File::create(&stderr_path)?)).spawn()?;
    let timeout = Duration::try_from_secs_f64(30.0 + duration_ms.max(0.0) / 1000.0 * 10.0)?;
    let deadline = Instant::now().checked_add(timeout).ok_or("Contract duration is unrepresentable")?;
    let status = loop {
        if let Some(status) = child.try_wait()? { break Some(status); }
        if Instant::now() >= deadline { child.kill()?; child.wait()?; break None; }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read_to_string(stdout_path)?;
    let stderr = fs::read_to_string(stderr_path)?;
    let mut report = match status {
        Some(status) if status.success() => serde_json::from_slice(&fs::read(report_path)?)?,
        Some(status) => failed(name, &format!("contract worker exited {status}; stderr: {stderr}")),
        None => failed(name, "contract worker exceeded its render deadline and was killed"),
    };
    report["native_stdout"] = json!(stdout);
    report["native_stderr"] = json!(stderr);
    let native_errors = [report["native_stdout"].as_str(), report["native_stderr"].as_str()]
        .into_iter().flatten().any(|text| text.lines().any(instantiation_error));
    if native_errors && report["instantiates"] == "instantiates" {
        report["instantiates"] = json!("instantiates-with-errors");
        if report["contract"]["expected_result"] != "instantiates-with-errors" {
            report["pass"] = json!(false);
            report["failures"].as_array_mut().ok_or("Invalid worker report")?.push(json!("unresolved class reported outside queued print hooks"));
        }
    }
    if report["instantiates"] == "instantiates" && report["contract"]["expected_result"] == "instantiates-with-errors" {
        report["pass"] = json!(false);
        report["failures"].as_array_mut().ok_or("Invalid worker report")?.push(json!("expected missing object/connection error was not observed"));
    }
    Ok(report)
}
fn md(text: &str) -> String { text.replace('|', "\\|").replace('\n', "<br>") }
fn contract_markdown(report: &Value) -> String {
    let name = report["name"].as_str().unwrap_or("unknown");
    let status = if report["pass"] == true { "PASS" } else { "FAIL" };
    let mut out = format!("# {name}: {status}\n\nInstantiation: **{}**.\n\n{}\n\n", report["instantiates"].as_str().unwrap_or("not-tested"), report["notes"].as_str().unwrap_or(""));
    if !report["contract"].is_null() {
        out.push_str(&format!("Source: `{}`.\n\nOmitted inlets (justification in notes): `{}`.\n\n## Contract and stimuli\n\n```json\n{}\n```\n\n", report["contract"]["source"].as_str().unwrap_or(""), report["omitted_inlets"], serde_json::to_string_pretty(&report["contract"]).unwrap_or_default()));
    }
    for (index, pass) in report["passes"].as_array().into_iter().flatten().enumerate() {
        out.push_str(&format!("## Render pass {index} (signal outlet {})\n\nFrames: {}; ticks: {}.\n\n### Expectations\n\n", pass["signal_outlet"], pass["rendered_frames"], pass["ticks"]));
        for check in pass["expectations"].as_array().into_iter().flatten() {
            out.push_str(&format!("Outlet {} **{}**\n\n```json\n{}\n```\n\n", check["outlet"], if check["pass"] == true { "PASS" } else { "FAIL" }, serde_json::to_string_pretty(check).unwrap_or_default()));
        }
        out.push_str("### Complete queued console\n\n```text\n");
        for line in pass["console"].as_array().into_iter().flatten() {
            out.push_str(&format!("{} ms: {}\n", line["at_ms"], line["line"].as_str().unwrap_or("")));
        }
        out.push_str("```\n\n### Generated host patch\n\n```puredata\n");
        out.push_str(pass["host_patch"].as_str().unwrap_or(""));
        out.push_str("```\n\n");
    }
    for field in ["native_stdout", "native_stderr"] {
        out.push_str(&format!("## {field}\n\n```text\n{}\n```\n\n", report[field].as_str().unwrap_or("")));
    }
    out.push_str("## Failures\n\n");
    for failure in report["failures"].as_array().into_iter().flatten() {
        out.push_str(&format!("- {}\n", failure.as_str().unwrap_or("Unknown failure")));
    }
    out
}

fn resolve_class(class: &str, scope: &Path, rj: &Path, bridges: &Path) -> String {
    if class.parse::<f64>().is_ok() { return format!("resolved: vanilla numeric [float {class}] box"); }
    if class == "seq" { return "UNRESOLVED: cyclone's [seq], not vanilla or Pd extra".into(); }
    if class == "clone" { return "resolved: vanilla [clone]; target is reported separately".into(); }
    for dir in [scope, rj, bridges, &bridges.join("abs")] {
        let file = dir.join(format!("{class}.pd"));
        if file.is_file() { return format!("resolved abstraction: {}", file.display()); }
    }
    let vanilla = ["float", "f", "symbol", "list", "osc~", "noise~", "sig~", "*~", "+~", "line~", "vline~", "delwrite~", "delread~", "vd~", "table", "array", "inlet", "outlet", "inlet~", "outlet~", "pd", "pd~", "soundfiler", "loadbang", "bang", "b", "t", "trigger", "r", "receive", "s", "send", "r~", "receive~", "s~", "send~", "throw~", "catch~", "route", "select", "sel", "pack", "unpack", "until", "delay", "del", "metro", "snapshot~", "tabread~", "tabread4~", "tabwrite~", "adc~", "dac~", "readsf~", "writesf~", "sigmund~", "fiddle~", "bonk~"];
    if vanilla.contains(&class) { "resolved: registered vanilla/Pd-extra class".into() }
    else { format!("UNRESOLVED statically: no local abstraction for class {class}; no runtime claim") }
}
fn referenced_file(file: &str, pd_path: &Path, scope: &Path, scope_files: &[PathBuf]) -> String {
    if file.contains('$') { return "UNRESOLVED: runtime filename needs an explicit value".into(); }
    for path in [pd_path.parent().unwrap_or(scope).join(file), scope.join(file)] {
        if path.is_file() { return format!("present: {}", path.strip_prefix(scope).unwrap_or(&path).display()); }
    }
    if let Some(path) = scope_files.iter().find(|p| p.file_name() == Path::new(file).file_name()) {
        return format!("present elsewhere in scene tree: {}; literal relative path absent", path.strip_prefix(scope).unwrap_or(path).display());
    }
    "ABSENT in this scene/library tree".into()
}
fn unresolved(rjlib: &Path, atsuke: &Path, bridges: &Path, contracts: &BTreeMap<String, Contract>, reports: &[Value]) -> Result<String> {
    let mut scopes = vec![("HEAD rj/".to_owned(), rjlib.join("rj"), rjlib.join("rj"))];
    for entry in fs::read_dir(rjlib.join("examplescenes"))? {
        let path = entry?.path();
        if path.is_dir() && path.extension().is_some_and(|s| s == "rj") {
            scopes.push((path.file_name().ok_or("Scene has no name")?.to_string_lossy().into_owned(), path.join("rj"), path));
        }
    }
    scopes[1..].sort_by(|a,b| a.0.cmp(&b.0));
    scopes.push(("Atsuke.rj".into(), atsuke.join("rj"), atsuke.to_path_buf()));
    let mut out = "# Explicit unresolved-class and scene-file resolution\n\nStatic findings are not engine certification. [seq] belongs to cyclone and is deliberately not supplied. [clone] is vanilla; its cloned class still needs to resolve. Dynamic classes use each abstraction's contract arguments (and report their real-engine result). All six library copies and their containing scene patches are scanned, including help/documentation references. Files marked absent in help text are not silently generated for scenes.\n\n".to_owned();
    if scopes.len() != 6 { out.push_str(&format!("**INCOMPLETE inventory: {} library copies, expected six.**\n\n", scopes.len())); }
    for (label, library, scope) in scopes {
        out.push_str(&format!("## {label}\n\nLibrary: `{}`.\n\n| file:line | construct | resolution |\n|---|---|---|\n", library.display()));
        let all = files(&scope)?;
        let mut count = 0;
        for path in all.iter().filter(|p| p.extension().is_some_and(|s| s == "pd")) {
            let parsed = records(&fs::read_to_string(path)?)?;
            let name = path.file_stem().ok_or("Pd file has no stem")?.to_string_lossy();
            let contract = contracts.get(name.as_ref());
            let engine = reports.iter().find(|r| r["name"].as_str() == Some(name.as_ref()))
                .map(|r| if r["pass"] == true { "HEAD contract PASS" } else { "HEAD contract FAIL" }).unwrap_or("HEAD contract not run");
            let runtime_builder = parsed.iter().any(|(_,r)| {
                let t = tokens(r);
                t.get(1).is_some_and(|s| s == "msg") && t.iter().skip(4).any(|s| s == "obj")
                    || t.get(4).is_some_and(|s| s == "list") && t.get(5).is_some_and(|s| s == "prepend") && t.get(6).is_some_and(|s| s == "obj")
            });
            for (line, record) in parsed {
                let t = tokens(&record);
                let origin = format!("{}:{}", path.strip_prefix(&scope)?.display(), line);
                let mut finding = |construct: String, resolution: String| {
                    count += 1;
                    out.push_str(&format!("| `{}` | {} | {} |\n", md(&origin), md(&construct), md(&resolution)));
                };
                if t.len() >= 5 && t[0] == "#X" && t[1] == "obj" {
                    let class = &t[4];
                    if class == "seq" || class == "clone" {
                        finding(format!("[{class}]"), resolve_class(class, &scope, &library, bridges));
                        if class == "clone" {
                            let mut index = 5;
                            while index < t.len() && t[index].starts_with('-') {
                                index += if t[index] == "-s" { 2 } else { 1 };
                            }
                            if let Some(target) = t.get(index) { finding(format!("clone target [{target}]"), resolve_class(target, &scope, &library, bridges)); }
                        }
                    }
                    if class.contains("$1") || class.contains("$2") {
                        let mut value = class.clone();
                        if let Some(contract) = contract {
                            for (index, argument) in contract.args.iter().enumerate().rev() {
                                value = value.replace(&format!("${}", index + 1), &argument.text());
                            }
                        }
                        let status = if value.contains('$') { "UNRESOLVED: no explicit creation argument for this dynamic class".to_owned() }
                        else { resolve_class(&value, &scope, &library, bridges) };
                        finding(format!("dynamic [{class}] -> [{value}]"), format!("{status}; {engine}; arguments from contract {name}; scene-local copy not independently executed"));
                    }
                    if class == "import" || class == "declare" && t.iter().any(|s| s == "-lib") {
                        finding(format!("runtime library: {}", t[4..].join(" ")), "UNRESOLVED unless built into libpd or explicitly present; no external library was injected".into());
                    }
                    if class == "pd~" { finding("runtime subprocess [pd~]".into(), "Pd extra is registered; runtime start messages/files are listed separately".into()); }
                    if ["s", "send"].contains(&class.as_str()) && t.get(5).is_some_and(|s| s.starts_with("pd-")) {
                        finding(format!("runtime canvas receiver {}", t[5]), format!("Pd canvas destination; concrete $0 is native; {engine} exercises the HEAD builder"));
                    }
                    if class == "list" && t.get(5).is_some_and(|s| s == "prepend") && t.get(6).is_some_and(|s| s == "obj") {
                        finding("message-created object list assembly [list prepend obj]".into(), format!("runtime object declaration; literal/template class candidates are reported separately; {engine}"));
                    }
                    if runtime_builder && class == "list" && t.get(5).is_some_and(|s| s == "append") && t.get(6).is_some_and(|s| s.starts_with('$')) {
                        let template = &t[6];
                        let value = template.strip_prefix('$').and_then(|s| s.parse::<usize>().ok())
                            .and_then(|index| contract.and_then(|c| c.args.get(index.checked_sub(1)?))).map(Argument::text);
                        let resolution = value.as_deref().map(|s| resolve_class(s, &scope, &library, bridges))
                            .unwrap_or_else(|| "UNRESOLVED: dynamic builder creation argument is not supplied".into());
                        finding(format!("runtime class candidate {template} -> {}", value.as_deref().unwrap_or("?")), format!("{resolution}; {engine}; candidate from creation-argument list assembly"));
                    }
                }
                if t.get(1).is_some_and(|s| t[0] == "#X" && s == "msg") {
                    for index in 4..t.len() {
                        if t[index] == "obj" {
                            if let Some(class) = t.get(index + 3) {
                                let destination = t[4..index].iter().rev().find(|s| s.starts_with("pd-"))
                                    .map(String::as_str).unwrap_or("connected canvas sender");
                                let resolution = if class.contains('$') {
                                    format!("UNRESOLVED static message argument class; {engine} exercises HEAD runtime creation")
                                } else { resolve_class(class, &scope, &library, bridges) };
                                finding(format!("message-created [{class}] into {destination}"), resolution);
                            }
                        }
                    }
                }
                for token in &t {
                    let value = token.trim_matches(|c: char| "\"'(),;<>".contains(c));
                    let lower = value.to_ascii_lowercase();
                    if lower.ends_with(".wav") || lower.ends_with(".aif") || lower.ends_with(".aiff") || lower.ends_with(".pd") {
                        let context = if t.get(1).is_some_and(|s| s == "text") { "documentation" } else { "object/message" };
                        finding(format!("{context} file {value}"), referenced_file(value, path, &scope, &all));
                    }
                }
            }
        }
        if count == 0 { out.push_str("| — | none of the requested unresolved constructs | scan complete |\n"); }
        out.push_str(&format!("\n{} Pd files scanned; {count} findings.\n\n", all.iter().filter(|p| p.extension().is_some_and(|s| s == "pd")).count()));
    }
    Ok(out)
}

fn options() -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            println!("cargo run --example contracts -- --rjlib <rjlib> --contracts <directory> --out <directory> [--only name]\nMeasurement tests: cargo test --example contracts");
            std::process::exit(0);
        }
        if !["--rjlib", "--contracts", "--out", "--only", "--worker", "--bridges"].contains(&flag.as_str()) {
            return Err(format!("Unknown argument {flag}").into());
        }
        let value = args.next().ok_or_else(|| format!("Missing value for {flag}"))?;
        if result.insert(flag.clone(), value).is_some() { return Err(format!("Duplicate option {flag}").into()); }
    }
    Ok(result)
}
fn required<'a>(options: &'a BTreeMap<String, String>, name: &str) -> Result<&'a Path> {
    Ok(Path::new(options.get(name).ok_or_else(|| format!("Missing required {name}"))?))
}
fn run(options: &BTreeMap<String, String>) -> Result<bool> {
    let rjlib = required(options, "--rjlib")?.canonicalize()?;
    let directory = required(options, "--contracts")?.canonicalize()?;
    let out = required(options, "--out")?;
    fs::create_dir_all(out)?;
    let only = options.get("--only");
    if only.is_some_and(|s| !valid_name(s)) { return Err("Unsafe --only name".into()); }
    let scratch = TempDir::new()?;
    let root = repo()?;
    let bridges = scratch.0.join("bridges");
    unzip(&root.join("ScenePlayer/res/raw/abstractions.zip"), &bridges)?;
    for extra in EXTRAS {
        fs::copy(root.join("PdCore/pd-core/src/main/jni/libpd/pure-data/extra").join(format!("{extra}.pd")), bridges.join(format!("{extra}.pd")))?;
    }
    let scenes = scratch.0.join("scenes");
    unzip(&root.join("ScenePlayer/res/raw/atsuke.zip"), &scenes)?;
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)?.map(|e| e.map(|e| e.path())).collect::<std::result::Result<_,_>>()?;
    paths.retain(|p| p.is_file() && p.extension().is_some_and(|s| s == "json") && p.file_name().is_some_and(|s| s != "_schema.json"));
    paths.sort();
    let mut contracts = BTreeMap::new();
    let mut reports = Vec::new();
    let mut loaded = BTreeSet::new();
    for path in paths {
        let name = path.file_stem().ok_or("Contract has no filename")?.to_string_lossy().into_owned();
        if !valid_name(&name) { return Err(format!("Unsafe contract filename {name}").into()); }
        loaded.insert(name.clone());
        let parsed = fs::read(&path).map_err(Box::<dyn Error>::from)
            .and_then(|bytes| serde_json::from_slice::<Contract>(&bytes).map_err(Box::<dyn Error>::from));
        let selected = only.is_none_or(|s| s == &name);
        match parsed {
            Ok(contract) => {
                if contract.name != name {
                    if selected { reports.push(failed(&name, "Contract name differs from filename")); }
                } else {
                    if selected {
                        let report = run_child(&path, &rjlib, &bridges, &scratch.0, &name, contract.duration_ms)
                            .unwrap_or_else(|e| failed(&name, &format!("Harness error: {e}")));
                        reports.push(report);
                    }
                    contracts.insert(name, contract);
                }
            }
            Err(error) => if selected { reports.push(failed(&name, &format!("Invalid contract JSON: {error}"))); },
        }
    }
    if let Some(name) = only {
        if !loaded.contains(name) { reports.push(failed(name, "--only contract was not found")); }
    } else {
        for entry in fs::read_dir(rjlib.join("rj"))? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|s| s == "pd") {
                let name = path.file_stem().ok_or("Abstraction has no stem")?.to_string_lossy().into_owned();
                if !name.ends_with("-help") && !loaded.contains(&name) { reports.push(failed(&name, "Missing per-abstraction contract")); }
            }
        }
    }
    reports.sort_by(|a,b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut summary = "# Real-engine abstraction contracts\n\nlibpd at 22050 Hz; 64-frame ticks; one fresh process per contract and one render per signal outlet. Help-patch loading is never certification. Missing contracts fail an unfiltered run.\n\n| name | instantiates | contract pass | failures | notes |\n|---|---|---|---|---|\n".to_owned();
    for report in &reports {
        let name = report["name"].as_str().ok_or("Report has no name")?;
        fs::write(out.join(format!("{name}.md")), contract_markdown(report))?;
        summary.push_str(&format!("| [{name}]({name}.md) | {} | {} | {} | {} |\n", report["instantiates"].as_str().unwrap_or("not-tested"), if report["pass"] == true { "PASS" } else { "FAIL" }, md(&report["failures"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")), md(report["notes"].as_str().unwrap_or(""))));
    }
    let passed = reports.iter().filter(|r| r["pass"] == true).count();
    summary.push_str(&format!("\n{passed}/{} contracts pass.\n", reports.len()));
    fs::write(out.join("contracts.md"), summary)?;
    fs::write(out.join("contracts.json"), serde_json::to_vec_pretty(&json!({"sample_rate":RATE,"block_frames":BLOCK,"exact_numeric_tolerance":EXACT_TOLERANCE,"rjlib":rjlib,"only":only,"passed":passed,"failed":reports.len()-passed,"contracts":reports}))?)?;
    fs::write(out.join("unresolved.md"), unresolved(&rjlib, &scenes.join("Atsuke.rj"), &bridges, &contracts, &reports)?)?;
    println!("{passed}/{} contracts pass; reports: {}", reports.len(), out.display());
    Ok(passed == reports.len() && !reports.is_empty())
}
fn main() -> Result<()> {
    let options = options()?;
    if let Some(path) = options.get("--worker") {
        let report = worker(Path::new(path), required(&options,"--rjlib")?, required(&options,"--bridges")?)?;
        fs::write(required(&options,"--out")?, serde_json::to_vec(&report)?)?;
        return Ok(());
    }
    if !run(&options)? { std::process::exit(1); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rms_measures_dc_silence_and_sine() {
        assert_eq!(rms(&[0.0; 64]), 0.0);
        assert_eq!(rms(&[0.5; 64]), 0.5);
        let sine: Vec<f32> = (0..RATE).map(|n| (0.5 * (std::f64::consts::TAU * 440.0 * n as f64 / RATE as f64).sin()) as f32).collect();
        assert!((rms(&sine) - 0.5 / 2.0f64.sqrt()).abs() < 1e-7);
        assert!(rms(&[]).is_nan());
        assert!(rms(&[f32::NAN]).is_nan());
    }
    #[test]
    fn centroid_measures_sine_and_silence() {
        let mut spectrum = Spectrum::new();
        assert_eq!(spectrum.centroid(&[0.0; 2048]), 0.0);
        let sine: Vec<f32> = (0..RATE).map(|n| (std::f64::consts::TAU * 440.0 * n as f64 / RATE as f64).sin() as f32).collect();
        assert!((spectrum.centroid(&sine) - 440.0).abs() < 20.0);
        assert!(spectrum.centroid(&[]).is_nan());
    }
    #[test]
    fn centroid_is_spectral_not_a_zero_crossing_estimate() {
        let samples: Vec<f32> = (0..32768).map(|n| {
            let phase = std::f64::consts::TAU * n as f64 / 1024.0;
            ((phase * 20.0).sin() + 0.5 * (phase * 60.0).sin()) as f32
        }).collect();
        let expected = (20.0 + 0.5 * 60.0) / 1.5 * RATE as f64 / 1024.0;
        assert!((Spectrum::new().centroid(&samples) - expected).abs() < 20.0);
    }
    #[test]
    fn changes_after_requires_strict_twenty_percent() {
        assert!(!changes_after(&[1.0, 1.0, 1.19, 1.19], 2).2);
        assert!(changes_after(&[1.0, 1.0, 1.21, 1.21], 2).2);
        assert!(changes_after(&[1.0, 1.0, 0.0, 0.0], 2).2);
        assert!(changes_after(&[0.0, 0.0, 0.5, 0.5], 2).2);
        assert!(!changes_after(&[0.0; 4], 2).2);
        assert!(!changes_after(&[1.0; 4], 0).2);
        assert!(!changes_after(&[1.0; 4], 4).2);
    }
    #[test]
    fn pd_records_preserve_escaped_semicolons_and_physical_line_numbers() {
        let text = "#N canvas 0 0 1 1 10;\n#X msg 0 0 \\; pd-x obj 0 0 osc~\n440;\n";
        let records = records(text).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].0, 2);
        assert_eq!(tokens(&records[1].1)[4], ";");
    }
    #[test]
    fn decoding_preserves_control_message_types() {
        let event = PdEvent::Message { recv:"rjqa_out".into(), msg:"out2".into(), atoms:vec![Atom::Symbol("rjqa_float".into()), Atom::Float(3.0)] };
        let message = decode_message(event, 5.0).unwrap();
        assert_eq!(message["outlet"],2);
        assert_eq!(message["type"],"float");
        assert_eq!(message["atoms"],json!([3.0]));
    }
    #[test]
    fn exact_is_a_fixed_tolerance_with_exact_symbols() {
        assert!(exact_atom(&json!(0.5), &Argument::Number(0.5000001)));
        assert!(!exact_atom(&json!(0.5), &Argument::Number(0.501)));
        assert!(!exact_atom(&json!("one"), &Argument::Text("two".into())));
    }
}
