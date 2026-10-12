//! Registered scene and audio JNI boundary. Release panics abort after logging to logcat;
//! catch_unwind prevents FFI unwinding and returns failure values in unwind-enabled builds.

use std::ffi::{c_char, c_void, CString};
use std::fs::File;
use std::os::fd::FromRawFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};

use arrayvec::{ArrayString, ArrayVec};
use jni::objects::{JClass, JLongArray, JObject, JObjectArray, JString, JThrowable};
use jni::refs::{Global, IntoAuto, Reference};
use jni::strings::{JNIStr, JNIString};
use jni::sys::{
    jboolean, jfloat, jint, jlongArray, jobjectArray, jstring, JNI_ERR, JNI_VERSION_1_6,
};
use jni::vm::{JavaVM, ScopeToken};
use jni::{jni_sig, jni_str};
use jni::{Env, EnvUnowned, JValue, NativeMethod, Outcome};

use crate::audio::{self, AudioEvent};
use crate::error::{CoreError, Result as CoreResult, Status};
use crate::pd::engine::{Command, TouchKind};
use crate::pd::hooks::{Atom, PdEvent};
use crate::{archive, playback, plist, recording, scenes};

pub struct Paths {
    pub files_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub native_library_dir: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();
static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static LISTENER: Mutex<Option<Arc<Global<JObject<'static>>>>> = Mutex::new(None);

pub fn paths() -> Option<&'static Paths> {
    PATHS.get()
}

pub fn java_vm() -> Option<&'static JavaVM> {
    JAVA_VM.get()
}

const NATIVE_METHODS: &[(&JNIStr, &JNIStr, *mut c_void)] = &[
    (
        jni_str!("hello"),
        jni_str!("()Ljava/lang/String;"),
        hello as *mut c_void,
    ),
    (
        jni_str!("nativeInit"),
        jni_str!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"),
        native_init as *mut c_void,
    ),
    (jni_str!("importScene"), jni_str!("(ILjava/lang/String;)Ljava/lang/String;"), import_scene as *mut c_void),
    (jni_str!("recoverScenes"), jni_str!("(Ljava/lang/String;[Ljava/lang/String;Lorg/puredata/android/scenes/RjdjCore$Registrar;)V"), recover_scenes as *mut c_void),
    (jni_str!("seedDefault"), jni_str!("(Ljava/lang/String;Ljava/lang/String;Z)Ljava/lang/String;"), seed_default as *mut c_void),
    (jni_str!("readInfo"), jni_str!("(Ljava/lang/String;)[Ljava/lang/String;"), read_info as *mut c_void),
    (jni_str!("deleteOwnedTree"), jni_str!("(Ljava/lang/String;Ljava/lang/String;)V"), delete_owned_tree as *mut c_void),
    (jni_str!("preparePlayback"), jni_str!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"), prepare_playback as *mut c_void),
    (jni_str!("releasePlayback"), jni_str!("(Ljava/lang/String;)V"), release_playback as *mut c_void),
    (jni_str!("validateRecording"), jni_str!("(Ljava/lang/String;)Z"), validate_recording as *mut c_void),
    (jni_str!("startAudio"), jni_str!("(Ljava/lang/String;F)I"), start_audio as *mut c_void),
    (jni_str!("stopAudio"), jni_str!("()V"), stop_audio as *mut c_void),
    (jni_str!("isRunning"), jni_str!("()Z"), is_running as *mut c_void),
    (jni_str!("sendTransportPlay"), jni_str!("(Z)V"), send_transport_play as *mut c_void),
    (jni_str!("sendTouch"), jni_str!("(IIFF)V"), send_touch as *mut c_void),
    (jni_str!("sendAccelerate"), jni_str!("(FFF)V"), send_accelerate as *mut c_void),
    (jni_str!("sendMicVolume"), jni_str!("(F)V"), send_mic_volume as *mut c_void),
    (jni_str!("sendRecord"), jni_str!("(Ljava/lang/String;Z)Z"), send_record as *mut c_void),
    (jni_str!("audioInfo"), jni_str!("()[J"), audio_info as *mut c_void),
    (jni_str!("setListener"), jni_str!("(Lorg/puredata/android/scenes/RjdjCore$Listener;)V"), set_listener as *mut c_void),
];

#[link(name = "log")]
extern "C" {
    fn __android_log_print(priority: jint, tag: *const c_char, format: *const c_char, ...) -> jint;
}

fn log_error(message: &str) {
    if let Ok(message) = CString::new(message.replace('\0', "\\0")) {
        // SAFETY: all strings are NUL-terminated; %s matches message's pointer.
        unsafe {
            __android_log_print(
                6, // ANDROID_LOG_ERROR
                b"RjdjCore\0".as_ptr().cast(),
                b"%s\0".as_ptr().cast(),
                message.as_ptr(),
            );
        }
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        log_error(&format!("{info}"));
    }));
}

#[no_mangle]
pub extern "system" fn JNI_OnLoad(raw_vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    catch_unwind(AssertUnwindSafe(|| {
        install_panic_hook();
        if raw_vm.is_null() {
            return JNI_ERR;
        }
        // SAFETY: the JVM supplies its live JavaVM pointer to JNI_OnLoad.
        let vm = unsafe { JavaVM::from_raw(raw_vm) };
        let mut scope = ScopeToken::default();
        // SAFETY: JNI_OnLoad runs on an attached thread; the guard stays in this scope.
        let mut attachment = match unsafe { vm.get_env_attachment(&mut scope) } {
            Ok(attachment) => attachment,
            Err(_) => return JNI_ERR,
        };
        let env = attachment.borrow_env_mut();
        let class = match env.find_class(jni_str!("org/puredata/android/scenes/RjdjCore")) {
            Ok(class) => class,
            Err(_) => return JNI_ERR,
        };
        let mut methods = ArrayVec::<NativeMethod<'static>, { NATIVE_METHODS.len() }>::new();
        for &(name, sig, fn_ptr) in NATIVE_METHODS {
            // SAFETY: each table entry matches its static Java method's JNI ABI exactly.
            let method = unsafe { NativeMethod::from_raw_parts(name, sig, fn_ptr) };
            if methods.try_push(method).is_err() {
                return JNI_ERR;
            }
        }
        // SAFETY: the table contains only correctly typed, live extern-system functions.
        if unsafe { env.register_native_methods(&class, &methods) }.is_err() {
            return JNI_ERR;
        }
        let _ = JAVA_VM.set(vm);
        audio::set_event_callback(Box::new(forward_event));
        JNI_VERSION_1_6
    }))
    .unwrap_or(JNI_ERR)
}

extern "system" fn hello(mut unowned: EnvUnowned<'_>, _class: JClass<'_>) -> jstring {
    catch_unwind(AssertUnwindSafe(|| {
        match unowned
            .with_env_no_catch(|env| -> jni::errors::Result<jstring> {
                match env.new_string(format!("rjdjcore {}", crate::VERSION)) {
                    Ok(value) => Ok(value.into_raw()),
                    Err(_) => {
                        env.exception_clear();
                        Ok(ptr::null_mut())
                    }
                }
            })
            .into_outcome()
        {
            Outcome::Ok(value) => value,
            Outcome::Err(_) | Outcome::Panic(_) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

extern "system" fn native_init(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    files_dir: JString<'_>,
    cache_dir: JString<'_>,
    native_library_dir: JString<'_>,
) -> jint {
    catch_unwind(AssertUnwindSafe(|| {
        match unowned
            .with_env_no_catch(|env| -> jni::errors::Result<jint> {
                let status = (|| {
                    let files_dir = directory(env, &files_dir)?;
                    let cache_dir = directory(env, &cache_dir)?;
                    let native_library_dir = directory(env, &native_library_dir)?;
                    Ok::<_, Status>(initialize(
                        &PATHS,
                        Paths {
                            files_dir,
                            cache_dir,
                            native_library_dir,
                        },
                    ))
                })();
                Ok(match status {
                    Ok(status) | Err(status) => status as jint,
                })
            })
            .into_outcome()
        {
            Outcome::Ok(value) => value,
            Outcome::Err(_) | Outcome::Panic(_) => Status::Internal as jint,
        }
    }))
    .unwrap_or(Status::Internal as jint)
}

fn with_native<'local, T: Copy>(
    unowned: &mut EnvUnowned<'local>,
    fallback: T,
    call: impl FnOnce(&mut Env<'local>) -> jni::errors::Result<T>,
) -> T {
    match catch_unwind(AssertUnwindSafe(|| {
        unowned.with_env_no_catch(call).into_outcome()
    })) {
        Ok(Outcome::Ok(value)) => value,
        Ok(Outcome::Err(_)) | Ok(Outcome::Panic(_)) | Err(_) => fallback,
    }
}

fn io_native<'local, T: Copy>(
    unowned: &mut EnvUnowned<'local>,
    fallback: T,
    call: impl FnOnce(&mut Env<'local>) -> CoreResult<T>,
) -> T {
    with_native(unowned, fallback, |env| {
        let result = match catch_unwind(AssertUnwindSafe(|| call(env))) {
            Ok(result) => result,
            Err(_) => Err(CoreError::new("RjdjCore internal failure")),
        };
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                throw_io(env, &error)?;
                Ok(fallback)
            }
        }
    })
}

fn audio_native<'local, T: Copy>(
    unowned: &mut EnvUnowned<'local>,
    fallback: T,
    call: impl FnOnce(&mut Env<'local>) -> CoreResult<T>,
) -> T {
    with_native(unowned, fallback, |env| match call(env) {
        Ok(value) => Ok(value),
        Err(error) => {
            env.exception_clear();
            log_error(&error.message);
            Ok(fallback)
        }
    })
}

fn java_error(env: &mut Env<'_>, error: jni::errors::Error) -> CoreError {
    let mut message = error.to_string();
    if let Some(exception) = env.exception_occurred() {
        let exception = exception.auto();
        env.exception_clear();
        if let Ok(value) = exception.get_message(env) {
            if value.is_null() {
                message = "null".to_owned();
            } else if let Ok(chars) = value.mutf8_chars(env) {
                message = chars.to_str().into_owned();
            }
        }
        env.exception_clear();
    }
    CoreError::new(message)
}

fn checked<T>(result: jni::errors::Result<T>, env: &mut Env<'_>) -> CoreResult<T> {
    result.map_err(|error| java_error(env, error))
}

fn string(env: &mut Env<'_>, value: &JString<'_>) -> CoreResult<String> {
    if value.is_null() {
        return Err(CoreError::new("Invalid argument"));
    }
    let chars = checked(value.mutf8_chars(env), env)?;
    Ok(chars.to_str().into_owned())
}

fn path(env: &mut Env<'_>, value: &JString<'_>) -> CoreResult<PathBuf> {
    Ok(PathBuf::from(string(env, value)?))
}

fn new_io_exception<'local>(
    env: &mut Env<'local>,
    message: &str,
) -> jni::errors::Result<JThrowable<'local>> {
    let message = env.new_string(message)?.auto();
    let exception = env.new_object(
        jni_str!("java/io/IOException"),
        jni_sig!("(Ljava/lang/String;)V"),
        &[JValue::Object(&message)],
    )?;
    JThrowable::cast_local(env, exception)
}

fn throw_io(env: &mut Env<'_>, error: &CoreError) -> jni::errors::Result<()> {
    if error.suppressed.is_empty() {
        return env.throw_new(
            jni_str!("java/io/IOException"),
            JNIString::new(&error.message),
        );
    }
    let exception = new_io_exception(env, &error.message)?;
    for message in &error.suppressed {
        let suppressed = new_io_exception(env, message)?.auto();
        env.call_method(
            &exception,
            jni_str!("addSuppressed"),
            jni_sig!("(Ljava/lang/Throwable;)V"),
            &[JValue::Object(&suppressed)],
        )?;
    }
    env.throw(exception)
}

fn return_path(env: &mut Env<'_>, value: &Path) -> CoreResult<jstring> {
    Ok(checked(env.new_string(value.to_string_lossy()), env)?.into_raw())
}

extern "system" fn import_scene(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    detached_fd: jint,
    scenes_root: JString<'_>,
) -> jstring {
    io_native(&mut unowned, ptr::null_mut(), |env| {
        if detached_fd < 0 {
            return Err(CoreError::new("No archive was received"));
        }
        // SAFETY: ParcelFileDescriptor.detachFd transfers this descriptor exactly once.
        let input = unsafe { File::from_raw_fd(detached_fd) };
        let root = path(env, &scenes_root)?;
        return_path(env, &archive::import_scene(input, &root)?)
    })
}

extern "system" fn recover_scenes(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    scenes_root: JString<'_>,
    registered: JObjectArray<'_, JString<'_>>,
    registrar: JObject<'_>,
) {
    io_native(&mut unowned, (), |env| {
        let root = path(env, &scenes_root)?;
        if registrar.is_null() || registered.is_null() {
            return Err(CoreError::new("Invalid argument"));
        }
        let length = checked(registered.len(env), env)?;
        let mut names = Vec::with_capacity(length);
        for index in 0..length {
            let name = checked(registered.get_element(env, index), env)?.auto();
            names.push(string(env, &name)?);
        }
        scenes::recover_scenes_with(&root, &names, |scene| {
            let scene = checked(env.new_string(scene.to_string_lossy()), env)?.auto();
            checked(
                env.call_method(
                    &registrar,
                    jni_str!("register"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[JValue::Object(&scene)],
                ),
                env,
            )?;
            Ok(())
        })
    });
}

extern "system" fn seed_default(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    scenes_root: JString<'_>,
    atsuke_zip_path: JString<'_>,
    already_seeded: jboolean,
) -> jstring {
    io_native(&mut unowned, ptr::null_mut(), |env| {
        let root = path(env, &scenes_root)?;
        let scene = if already_seeded {
            scenes::seed_default(&root, std::io::empty(), true)?
        } else {
            let zip = path(env, &atsuke_zip_path)?;
            scenes::seed_default(&root, File::open(zip)?, false)?
        };
        match scene {
            Some(scene) => return_path(env, &scene),
            None => Ok(ptr::null_mut()),
        }
    })
}

extern "system" fn read_info(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    scene_dir: JString<'_>,
) -> jobjectArray {
    io_native(&mut unowned, ptr::null_mut(), |env| {
        let scene = path(env, &scene_dir)?;
        let info = plist::read_info(&scene)?;
        let pairs = checked(
            JObjectArray::<JString>::new(env, info.len() * 2, JString::null()),
            env,
        )?;
        for (index, (key, value)) in info.iter().enumerate() {
            let key = checked(env.new_string(key), env)?.auto();
            let value = checked(env.new_string(value), env)?.auto();
            checked(pairs.set_element(env, index * 2, &key), env)?;
            checked(pairs.set_element(env, index * 2 + 1, &value), env)?;
        }
        Ok(pairs.into_raw())
    })
}

extern "system" fn delete_owned_tree(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    root: JString<'_>,
    target: JString<'_>,
) {
    io_native(&mut unowned, (), |env| {
        let root = path(env, &root)?;
        let target = path(env, &target)?;
        archive::delete_owned_tree(&root, &target)
    });
}

extern "system" fn prepare_playback(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    scene_dir: JString<'_>,
    cache_dir: JString<'_>,
) -> jstring {
    io_native(&mut unowned, ptr::null_mut(), |env| {
        let scene = path(env, &scene_dir)?;
        let cache = path(env, &cache_dir)?;
        let thread = checked(
            env.call_static_method(
                jni_str!("java/lang/Thread"),
                jni_str!("currentThread"),
                jni_sig!("()Ljava/lang/Thread;"),
                &[],
            )
            .and_then(|value| value.l()),
            env,
        )?.auto();
        let copy = playback::prepare(&scene, &cache, &mut || {
            checked(
                env.call_method(&thread, jni_str!("isInterrupted"), jni_sig!("()Z"), &[])
                    .and_then(|value| value.z()),
                env,
            )
        })?;
        return_path(env, &copy)
    })
}

extern "system" fn release_playback(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    playback_dir: JString<'_>,
) {
    io_native(&mut unowned, (), |env| {
        if playback_dir.is_null() {
            return Ok(());
        }
        let dir = path(env, &playback_dir)?;
        audio::close_patch_if(&dir);
        playback::release(&dir);
        Ok(())
    });
}

extern "system" fn validate_recording(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    wav_path: JString<'_>,
) -> jboolean {
    io_native(&mut unowned, false, |env| {
        recording::validate_recording(&path(env, &wav_path)?)
    })
}

extern "system" fn start_audio(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    playback_dir: JString<'_>,
    mic_volume: jfloat,
) -> jint {
    audio_native(&mut unowned, Status::Internal as jint, |env| {
        let Some(paths) = paths() else {
            return Ok(Status::NotInitialized as jint);
        };
        let playback_dir = match directory(env, &playback_dir) {
            Ok(dir) => dir,
            Err(status) => return Ok(status as jint),
        };
        let search_paths =
            playback::search_paths(&playback_dir, &paths.files_dir, &paths.native_library_dir);
        Ok(audio::start(&playback_dir, mic_volume, &search_paths) as jint)
    })
}

extern "system" fn stop_audio(mut unowned: EnvUnowned<'_>, _class: JClass<'_>) {
    audio_native(&mut unowned, (), |_| {
        audio::stop();
        Ok(())
    });
}

extern "system" fn is_running(mut unowned: EnvUnowned<'_>, _class: JClass<'_>) -> jboolean {
    audio_native(&mut unowned, false, |_| Ok(audio::is_running()))
}

extern "system" fn send_transport_play(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    on: jboolean,
) {
    audio_native(&mut unowned, (), |_| {
        audio::send(Command::TransportPlay(on));
        Ok(())
    });
}

fn touch_kind(kind: jint) -> Option<TouchKind> {
    match kind {
        0 => Some(TouchKind::Down),
        1 => Some(TouchKind::Xy),
        2 => Some(TouchKind::Up),
        _ => None,
    }
}

extern "system" fn send_touch(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    kind: jint,
    pointer_id: jint,
    x: jfloat,
    y: jfloat,
) {
    audio_native(&mut unowned, (), |_| {
        if let Some(kind) = touch_kind(kind) {
            audio::send(Command::Touch {
                kind,
                id: pointer_id,
                x,
                y,
            });
        }
        Ok(())
    });
}

extern "system" fn send_accelerate(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    x: jfloat,
    y: jfloat,
    z: jfloat,
) {
    audio_native(&mut unowned, (), |_| {
        audio::send(Command::Accelerate(x, y, z));
        Ok(())
    });
}

extern "system" fn send_mic_volume(mut unowned: EnvUnowned<'_>, _class: JClass<'_>, gain: jfloat) {
    audio_native(&mut unowned, (), |_| {
        audio::send(Command::MicVolume(gain));
        Ok(())
    });
}

fn record_scene(path: &str) -> CoreResult<Command> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(CoreError::new("Invalid recording path"));
    }
    let path =
        ArrayString::try_from(path).map_err(|_| CoreError::new("Recording path is too long"))?;
    Ok(Command::RecordScene(path))
}

fn send_record_commands(path: Option<&str>, mut send: impl FnMut(Command) -> bool) -> CoreResult<bool> {
    if let Some(path) = path {
        if !send(record_scene(path)?) {
            return Ok(false);
        }
    }
    Ok(send(Command::Record(path.is_some())))
}

extern "system" fn send_record(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    wav_path: JString<'_>,
    start: jboolean,
) -> jboolean {
    audio_native(&mut unowned, false, |env| {
        let path = if start {
            Some(string(env, &wav_path)?)
        } else {
            None
        };
        send_record_commands(path.as_deref(), audio::send)
    })
}

extern "system" fn audio_info(mut unowned: EnvUnowned<'_>, _class: JClass<'_>) -> jlongArray {
    audio_native(&mut unowned, ptr::null_mut(), |env| {
        let values = audio::info();
        let array = checked(JLongArray::new(env, values.len()), env)?;
        checked(array.set_region(env, 0, &values), env)?;
        Ok(array.into_raw())
    })
}

extern "system" fn set_listener(
    mut unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    listener: JObject<'_>,
) {
    audio_native(&mut unowned, (), |env| {
        let listener = if listener.is_null() {
            None
        } else {
            Some(Arc::new(checked(env.new_global_ref(&listener), env)?))
        };
        *LISTENER.lock().unwrap_or_else(|poison| poison.into_inner()) = listener;
        Ok(())
    });
}

fn forward_event(event: AudioEvent) {
    let Some(vm) = java_vm() else { return };
    let listener = LISTENER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let Some(listener) = listener else { return };
    let result = catch_unwind(AssertUnwindSafe(|| {
        // jni 0.22 attaches permanently; its per-call local frame frees all event references.
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            if let Err(error) = deliver_event(env, &listener, event) {
                log_error(&format!(
                    "Listener callback failed: {}",
                    java_error(env, error)
                ));
            }
            env.exception_clear();
            Ok(())
        })
    }));
    if let Ok(Err(error)) = result {
        log_error(&format!("Unable to attach audio event thread: {error}"));
    }
}

fn deliver_event(
    env: &mut Env<'_>,
    listener: &Global<JObject<'static>>,
    event: AudioEvent,
) -> jni::errors::Result<()> {
    match event {
        AudioEvent::Pd(PdEvent::List { recv, atoms }) => {
            let receiver = env.new_string(recv)?;
            let args = JObjectArray::<JObject>::new(env, atoms.len(), JObject::null())?;
            for (index, atom) in atoms.iter().enumerate() {
                let value = match atom {
                    Atom::Float(value) => env.new_object(
                        jni_str!("java/lang/Float"),
                        jni_sig!("(F)V"),
                        &[JValue::Float(*value)],
                    )?,
                    Atom::Symbol(value) => env.new_string(value)?.into(),
                }
                .auto();
                args.set_element(env, index, &value)?;
            }
            env.call_method(
                listener,
                jni_str!("onList"),
                jni_sig!("(Ljava/lang/String;[Ljava/lang/Object;)V"),
                &[JValue::Object(&receiver), JValue::Object(&args)],
            )?;
        }
        AudioEvent::Pd(PdEvent::Print(line)) => {
            let line = env.new_string(line)?;
            env.call_method(
                listener,
                jni_str!("onPrint"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&line)],
            )?;
        }
        AudioEvent::State { state, status } => {
            env.call_method(
                listener,
                jni_str!("onAudioState"),
                jni_sig!("(II)V"),
                &[JValue::Int(state as jint), JValue::Int(status as jint)],
            )?;
        }
        // The shell subscribes only to overlay lists; it has no bang/float/symbol/message callbacks.
        AudioEvent::Pd(
            PdEvent::Bang { .. }
            | PdEvent::Float { .. }
            | PdEvent::Symbol { .. }
            | PdEvent::Message { .. },
        ) => {}
    }
    Ok(())
}

fn directory(env: &mut Env<'_>, value: &JString<'_>) -> Result<PathBuf, Status> {
    if value.is_null() {
        return Err(Status::InvalidArgument);
    }
    let chars = match value.mutf8_chars(env) {
        Ok(chars) => chars,
        Err(_) => {
            env.exception_clear();
            return Err(Status::Internal);
        }
    };
    Ok(PathBuf::from(chars.to_str().as_ref()))
}

fn initialize(storage: &OnceLock<Paths>, dirs: Paths) -> Status {
    if dirs.files_dir.as_os_str().is_empty()
        || dirs.cache_dir.as_os_str().is_empty()
        || dirs.native_library_dir.as_os_str().is_empty()
    {
        return Status::InvalidArgument;
    }
    // A concurrent or repeated initialization intentionally keeps the first directories.
    let _ = storage.set(dirs);
    Status::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_kinds_match_the_java_protocol_and_reject_unknown_values() {
        assert_eq!(touch_kind(0), Some(TouchKind::Down));
        assert_eq!(touch_kind(1), Some(TouchKind::Xy));
        assert_eq!(touch_kind(2), Some(TouchKind::Up));
        assert_eq!(touch_kind(-1), None);
        assert_eq!(touch_kind(3), None);
    }

    #[test]
    fn recording_paths_are_never_truncated_or_sent_with_nul() {
        assert!(record_scene("").is_err());
        assert!(record_scene("/private/record\0.wav").is_err());
        assert!(record_scene(&"x".repeat(1025)).is_err());
        match record_scene(&"x".repeat(1024)).unwrap() {
            Command::RecordScene(path) => assert_eq!(path.len(), 1024),
            _ => panic!("Wrong recording command"),
        }
        match record_scene("/private/音.wav").unwrap() {
            Command::RecordScene(path) => assert_eq!(path.as_str(), "/private/音.wav"),
            _ => panic!("Wrong recording command"),
        }
    }

    #[test]
    fn saturated_record_stop_reports_failure_without_delivering_stop() {
        let (queue, mut consumer) = crate::pd::engine::CommandQueue::new();
        for _ in 0..256 {
            assert!(queue.push(Command::MicVolume(1.0)));
        }
        assert!(!send_record_commands(None, |command| queue.push(command)).unwrap());
        assert_eq!(queue.dropped(), 1);
        for _ in 0..256 {
            assert!(matches!(consumer.pop(), Ok(Command::MicVolume(1.0))));
        }
        assert!(consumer.pop().is_err());
        assert!(send_record_commands(None, |command| queue.push(command)).unwrap());
        assert!(matches!(consumer.pop(), Ok(Command::Record(false))));
    }

    fn dirs(files: &str, cache: &str, libraries: &str) -> Paths {
        Paths {
            files_dir: files.into(),
            cache_dir: cache.into(),
            native_library_dir: libraries.into(),
        }
    }

    #[test]
    fn initialization_rejects_each_empty_directory_without_publishing() {
        let storage = OnceLock::new();
        for (files, cache, libraries) in [
            ("", "/cache", "/libs"),
            ("/files", "", "/libs"),
            ("/files", "/cache", ""),
        ] {
            assert_eq!(
                initialize(&storage, dirs(files, cache, libraries)),
                Status::InvalidArgument
            );
            assert!(storage.get().is_none());
        }
        assert_eq!(
            initialize(&storage, dirs("/files", "/cache", "/libs")),
            Status::Ok
        );
    }

    #[test]
    fn repeated_initialization_keeps_all_first_directories() {
        let storage = OnceLock::new();
        assert_eq!(
            initialize(&storage, dirs("/files/音", "/cache", "/libs")),
            Status::Ok
        );
        assert_eq!(
            initialize(
                &storage,
                dirs("/other-files", "/other-cache", "/other-libs")
            ),
            Status::Ok
        );
        let first = storage.get().unwrap();
        assert_eq!(first.files_dir, PathBuf::from("/files/音"));
        assert_eq!(first.cache_dir, PathBuf::from("/cache"));
        assert_eq!(first.native_library_dir, PathBuf::from("/libs"));
        assert_eq!(
            initialize(&storage, dirs("", "/cache", "/libs")),
            Status::InvalidArgument
        );
        assert_eq!(storage.get().unwrap().files_dir, first.files_dir);
    }
}
