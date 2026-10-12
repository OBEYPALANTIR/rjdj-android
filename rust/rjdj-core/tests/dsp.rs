use std::ffi::{c_void, CStr};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, MutexGuard};

use arrayvec::ArrayString;
use rjdjcore::pd::{
    drain_into_pd, drain_queued, ffi, process, set_event_sink, Atom, Command, CommandQueue, Engine,
    Patch, PdEvent, TouchKind,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

// Byte-for-byte the object graph in SceneEngineInstrumentation.java.
const BRIDGE_PATCH: &str = "#N canvas 0 0 400 300 10;\n\
#X obj 20 20 soundinput;\n#X obj 20 80 soundoutput;\n\
#X connect 0 0 1 0;\n#X connect 0 1 1 1;\n";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        loop {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("rjdj-dsp-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("creating DSP fixture: {error}"),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Session {
    engine: MutexGuard<'static, Engine>,
    patch: Option<Patch>,
    bindings: Vec<*mut c_void>,
    events: mpsc::Receiver<PdEvent>,
    directory: TempDir,
    _serial: MutexGuard<'static, ()>,
}

impl Session {
    fn new(rate: i32, bridges: bool, receivers: &[&CStr]) -> Self {
        let serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let mut engine = Engine::global()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        engine.init().unwrap();
        engine.init().unwrap(); // Initialization must be idempotent in this single process.
        drain_queued();
        let (sender, events) = mpsc::channel();
        set_event_sink(sender);
        let directory = TempDir::new();
        if bridges {
            let archive_path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../ScenePlayer/res/raw/abstractions.zip");
            let mut archive = zip::ZipArchive::new(File::open(archive_path).unwrap()).unwrap();
            for name in ["soundinput.pd", "soundoutput.pd"] {
                let mut entry = archive.by_name(name).unwrap();
                let mut output = File::create(directory.0.join(name)).unwrap();
                io::copy(&mut entry, &mut output).unwrap();
            }
        }
        engine.set_search_paths(&[directory.0.as_path()]).unwrap();
        engine.init_audio(1, 2, rate).unwrap();
        let bindings = receivers
            .iter()
            .map(|receiver| {
                // SAFETY: setup is serialized under Engine's lock, and receivers are C strings.
                let binding = unsafe { ffi::libpd_bind(receiver.as_ptr()) };
                assert!(!binding.is_null());
                binding
            })
            .collect();
        Self {
            engine,
            patch: None,
            bindings,
            events,
            directory,
            _serial: serial,
        }
    }

    fn open(&mut self, text: &str) {
        fs::write(self.directory.0.join("_main.pd"), text).unwrap();
        self.patch = Some(
            self.engine
                .open_patch(&self.directory.0, "_main.pd")
                .unwrap(),
        );
    }

    fn take_events(&self) -> Vec<PdEvent> {
        drain_queued();
        self.events.try_iter().collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(patch) = self.patch.take() {
            self.engine.close_patch(patch);
        }
        for binding in self.bindings.drain(..) {
            // SAFETY: every handle came from libpd_bind and DSP is no longer processing.
            unsafe { ffi::libpd_unbind(binding) };
        }
        drain_queued();
    }
}

fn render(rate: i32, ticks: usize, first_frame: usize) -> Vec<f32> {
    let blocksize = unsafe { ffi::libpd_blocksize() } as usize;
    assert_eq!(blocksize, 64);
    let mut input = vec![0.0; blocksize];
    let mut output = vec![0.0; blocksize * 2];
    for block in 0..ticks {
        for (frame, value) in input.iter_mut().enumerate() {
            let position = first_frame + block * blocksize + frame;
            *value = (0.25 * (std::f64::consts::TAU * 440.0 * position as f64 / rate as f64).sin())
                as f32;
        }
        assert_eq!(process(1, &input, &mut output), 0);
        for stereo in output.chunks_exact(2) {
            assert!(
                (stereo[0] - stereo[1]).abs() < 1e-5,
                "stereo parity: {stereo:?}"
            );
        }
    }
    output
}

fn energy(output: &[f32]) -> f64 {
    output
        .iter()
        .map(|value| (*value as f64).powi(2))
        .sum::<f64>()
        / output.len() as f64
}

fn assert_microphone_bridge(rate: i32) {
    let mut session = Session::new(rate, true, &[]);
    session.open(BRIDGE_PATCH);
    let (queue, mut consumer) = CommandQueue::new();
    assert!(queue.push(Command::MicVolume(1.0)));
    assert!(queue.push(Command::TransportPlay(true)));
    drain_into_pd(&mut consumer);
    let output = render(rate, 800, 0);
    assert!(
        energy(&output) > 0.005,
        "microphone energy at {rate} Hz: {}",
        energy(&output)
    );
    assert!(queue.push(Command::MicVolume(0.0)));
    drain_into_pd(&mut consumer);
    let output = render(rate, 800, 800 * 64);
    assert!(
        energy(&output) < 1e-6,
        "muted energy after the 50 ms ramp: {}",
        energy(&output)
    );
}

#[test]
fn microphone_bridge_at_48000_hz() {
    assert_microphone_bridge(48000);
}

#[test]
fn microphone_bridge_at_22050_hz() {
    assert_microphone_bridge(22050);
}

#[test]
fn queued_hooks_deliver_exact_atoms_and_whole_print_lines() {
    // rj_text_android is bound by Engine::init itself (host overlay receiver); binding it again here would double every event.
    let mut session = Session::new(22050, false, &[]);
    session.open(include_str!("fixtures/pd/events.pd"));
    assert!(
        session.events.try_recv().is_err(),
        "hooks must wait for the drain thread"
    );
    assert_eq!(
        session.take_events(),
        vec![
            PdEvent::Bang {
                recv: "rj_text_android".into()
            },
            PdEvent::Float {
                recv: "rj_text_android".into(),
                x: 2.5
            },
            PdEvent::Symbol {
                recv: "rj_text_android".into(),
                sym: "ready".into()
            },
            PdEvent::List {
                recv: "rj_text_android".into(),
                atoms: vec![
                    Atom::Symbol("overlay-key".into()),
                    Atom::Symbol("text".into()),
                    Atom::Symbol("hello".into()),
                    Atom::Float(17.5),
                ]
            },
            PdEvent::Message {
                recv: "rj_text_android".into(),
                msg: "move".into(),
                atoms: vec![Atom::Float(12.0), Atom::Float(34.0),]
            },
            PdEvent::Print("print_fragments: list alpha 7 omega".into()),
        ]
    );
}

#[test]
fn unknown_object_keeps_patch_open_and_reports_exact_creation_error() {
    let mut session = Session::new(22050, false, &[]);
    session.open(include_str!("fixtures/pd/unknown.pd"));
    assert_eq!(
        session.take_events(),
        vec![
            PdEvent::Print("verbose(0): rj_doesnotexist".into()),
            PdEvent::Print("error: ... couldn't create".into()),
        ]
    );
}

#[test]
fn all_libpd_extra_objects_instantiate() {
    let mut session = Session::new(22050, false, &[]);
    session.open(include_str!("fixtures/pd/extra.pd"));
    let events = session.take_events();
    assert!(
        !events.iter().any(|event| {
            matches!(event, PdEvent::Print(line) if line.contains("couldn't create"))
        }),
        "extra object creation failed: {events:?}"
    );
}

#[test]
fn commands_stay_queued_until_audio_drain_and_preserve_payloads() {
    let mut session = Session::new(
        22050,
        false,
        &[
            c"transport_echo",
            c"mic_echo",
            c"touch_echo",
            c"accelerate_echo",
        ],
    );
    session.open(include_str!("fixtures/pd/commands.pd"));
    let (queue, mut consumer) = CommandQueue::new();
    for command in [
        Command::TransportPlay(true),
        Command::TransportPlay(false),
        Command::MicVolume(0.0625),
        Command::Touch {
            kind: TouchKind::Down,
            id: 3,
            x: 0.0,
            y: 319.0,
        },
        Command::Touch {
            kind: TouchKind::Xy,
            id: 3,
            x: 11.5,
            y: 22.5,
        },
        Command::Touch {
            kind: TouchKind::Up,
            id: 3,
            x: 319.0,
            y: 0.0,
        },
        Command::Accelerate(1.0, -2.0, -3.0),
        Command::RecordStart(ArrayString::from("/private/recordings/音 scene.wav").unwrap()),
        Command::Record(false),
    ] {
        assert!(queue.push(command));
    }
    assert!(
        session.take_events().is_empty(),
        "UI producers must not call Pd"
    );
    drain_into_pd(&mut consumer);
    let message = |recv: &str, msg: &str, values: &[f32]| PdEvent::Message {
        recv: recv.into(),
        msg: msg.into(),
        atoms: values.iter().copied().map(Atom::Float).collect(),
    };
    assert_eq!(
        session.take_events(),
        vec![
            message("transport_echo", "play", &[1.0]),
            message("transport_echo", "play", &[0.0]),
            PdEvent::Float {
                recv: "mic_echo".into(),
                x: 0.0625
            },
            message("touch_echo", "down", &[3.0, 0.0, 319.0]),
            message("touch_echo", "xy", &[3.0, 11.5, 22.5]),
            message("touch_echo", "up", &[3.0, 319.0, 0.0]),
            PdEvent::List {
                recv: "accelerate_echo".into(),
                atoms: vec![Atom::Float(1.0), Atom::Float(-2.0), Atom::Float(-3.0),]
            },
            PdEvent::Message {
                recv: "transport_echo".into(),
                msg: "scene".into(),
                atoms: vec![Atom::Symbol("/private/recordings/音 scene.wav".into()),]
            },
            message("transport_echo", "record", &[1.0]),
            message("transport_echo", "record", &[0.0]),
        ]
    );
    assert_eq!(queue.dropped(), 0);
}

#[test]
fn command_queue_drops_new_commands_at_256_without_overwriting() {
    let (queue, mut consumer) = CommandQueue::new();
    for index in 0..256 {
        assert!(queue.push(Command::MicVolume(index as f32)));
    }
    assert!(!queue.push(Command::TransportPlay(false)));
    assert!(!queue.push(Command::Record(false)));
    assert_eq!(queue.dropped(), 2);
    for index in 0..256 {
        assert!(
            matches!(consumer.pop().unwrap(), Command::MicVolume(value) if value == index as f32)
        );
    }
    assert!(consumer.pop().is_err());
    assert!(queue.push(Command::TransportPlay(true)));
    assert!(matches!(
        consumer.pop().unwrap(),
        Command::TransportPlay(true)
    ));
    assert_eq!(queue.dropped(), 2);
}

#[test]
fn record_start_uses_the_last_queue_slot_or_is_rejected_whole() {
    let (queue, mut consumer) = CommandQueue::new();
    for _ in 0..255 {
        assert!(queue.push(Command::MicVolume(1.0)));
    }
    let start = Command::RecordStart(ArrayString::from("/private/record.wav").unwrap());
    assert!(queue.push(start.clone()));
    assert!(!queue.push(start));
    assert_eq!(queue.dropped(), 1);
    for _ in 0..255 {
        assert!(matches!(consumer.pop(), Ok(Command::MicVolume(1.0))));
    }
    assert!(matches!(
        consumer.pop(),
        Ok(Command::RecordStart(path)) if path.as_str() == "/private/record.wav"
    ));
    assert!(consumer.pop().is_err());
}

#[test]
fn patch_open_failure_names_the_main_patch_and_directory() {
    let mut session = Session::new(22050, false, &[]);
    session.open("#N canvas 0 0 400 300 10;\n");
    session.engine.close_patch(session.patch.take().unwrap());
    fs::remove_file(session.directory.0.join("_main.pd")).unwrap();
    let error = session
        .engine
        .open_patch(&session.directory.0, "_main.pd")
        .unwrap_err();
    assert_eq!(
        error.message,
        format!(
            "Unable to open _main.pd in {}",
            session.directory.0.display()
        )
    );
}

#[test]
fn processing_rejects_wrong_buffer_sizes_before_entering_c() {
    let _session = Session::new(22050, false, &[]);
    assert!(std::panic::catch_unwind(|| process(1, &[0.0; 63], &mut [0.0; 128])).is_err());
    assert!(std::panic::catch_unwind(|| process(1, &[0.0; 64], &mut [0.0; 127])).is_err());
    assert!(std::panic::catch_unwind(|| process(-1, &[], &mut [])).is_err());
    assert_eq!(process(0, &[], &mut []), 0);
}

#[test]
fn command_queue_rejects_empty_and_nul_paths_without_a_full_queue_drop() {
    let (queue, mut consumer) = CommandQueue::new();
    assert!(!queue.push(Command::RecordStart(ArrayString::new())));
    assert!(!queue.push(Command::RecordStart(
        ArrayString::from("/private/record\0.wav").unwrap()
    )));
    assert!(consumer.pop().is_err());
    assert_eq!(queue.dropped(), 0);
    assert!(queue.push(Command::RecordStart(
        ArrayString::from("/private/record.wav").unwrap()
    )));
}
