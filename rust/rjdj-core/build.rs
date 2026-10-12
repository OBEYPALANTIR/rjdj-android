use std::env;
use std::fs;
use std::path::PathBuf;

// PD_SRC_FILES, in order, from the pinned libpd/Android.mk; no JNI/audio backend sources.
const PD_SRC_FILES: &[&str] = &[
    "pure-data/src/d_arithmetic.c",
    "pure-data/src/d_array.c",
    "pure-data/src/d_ctl.c",
    "pure-data/src/d_dac.c",
    "pure-data/src/d_delay.c",
    "pure-data/src/d_fft.c",
    "pure-data/src/d_fft_fftsg.c",
    "pure-data/src/d_filter.c",
    "pure-data/src/d_global.c",
    "pure-data/src/d_math.c",
    "pure-data/src/d_misc.c",
    "pure-data/src/d_osc.c",
    "pure-data/src/d_resample.c",
    "pure-data/src/d_soundfile.c",
    "pure-data/src/d_soundfile_aiff.c",
    "pure-data/src/d_soundfile_caf.c",
    "pure-data/src/d_soundfile_next.c",
    "pure-data/src/d_soundfile_wave.c",
    "pure-data/src/d_ugen.c",
    "pure-data/src/g_all_guis.c",
    "pure-data/src/g_array.c",
    "pure-data/src/g_bang.c",
    "pure-data/src/g_canvas.c",
    "pure-data/src/g_clone.c",
    "pure-data/src/g_editor.c",
    "pure-data/src/g_editor_extras.c",
    "pure-data/src/g_graph.c",
    "pure-data/src/g_guiconnect.c",
    "pure-data/src/g_io.c",
    "pure-data/src/g_mycanvas.c",
    "pure-data/src/g_numbox.c",
    "pure-data/src/g_radio.c",
    "pure-data/src/g_readwrite.c",
    "pure-data/src/g_rtext.c",
    "pure-data/src/g_scalar.c",
    "pure-data/src/g_slider.c",
    "pure-data/src/g_template.c",
    "pure-data/src/g_text.c",
    "pure-data/src/g_toggle.c",
    "pure-data/src/g_traversal.c",
    "pure-data/src/g_undo.c",
    "pure-data/src/g_vumeter.c",
    "pure-data/src/m_atom.c",
    "pure-data/src/m_binbuf.c",
    "pure-data/src/m_class.c",
    "pure-data/src/m_conf.c",
    "pure-data/src/m_glob.c",
    "pure-data/src/m_memory.c",
    "pure-data/src/m_obj.c",
    "pure-data/src/m_pd.c",
    "pure-data/src/m_sched.c",
    "pure-data/src/s_audio.c",
    "pure-data/src/s_audio_dummy.c",
    "pure-data/src/s_inter.c",
    "pure-data/src/s_inter_gui.c",
    "pure-data/src/s_loader.c",
    "pure-data/src/s_main.c",
    "pure-data/src/s_net.c",
    "pure-data/src/s_path.c",
    "pure-data/src/s_print.c",
    "pure-data/src/s_utf8.c",
    "pure-data/src/x_acoustics.c",
    "pure-data/src/x_arithmetic.c",
    "pure-data/src/x_connective.c",
    "pure-data/src/x_file.c",
    "pure-data/src/x_gui.c",
    "pure-data/src/x_list.c",
    "pure-data/src/x_midi.c",
    "pure-data/src/x_misc.c",
    "pure-data/src/x_net.c",
    "pure-data/src/x_array.c",
    "pure-data/src/x_time.c",
    "pure-data/src/x_interface.c",
    "pure-data/src/x_scalar.c",
    "pure-data/src/x_text.c",
    "pure-data/src/x_vexp.c",
    "pure-data/src/x_vexp_if.c",
    "pure-data/src/x_vexp_fun.c",
    "libpd_wrapper/s_libpdmidi.c",
    "libpd_wrapper/x_libpdreceive.c",
    "libpd_wrapper/z_libpd.c",
    "libpd_wrapper/util/z_ringbuffer.c",
    "libpd_wrapper/util/z_queued.c",
    "libpd_wrapper/z_hooks.c",
    "libpd_wrapper/util/z_print_util.c",
];

// PD_EXTRA_FILES from libpd/Makefile; z_libpd.c registers these with LIBPD_EXTRA.
const PD_EXTRA_FILES: &[&str] = &[
    "pure-data/extra/bob~/bob~.c",
    "pure-data/extra/bonk~/bonk~.c",
    "pure-data/extra/choice/choice.c",
    "pure-data/extra/fiddle~/fiddle~.c",
    "pure-data/extra/loop~/loop~.c",
    "pure-data/extra/lrshift~/lrshift~.c",
    "pure-data/extra/pique/pique.c",
    "pure-data/extra/pd~/pdsched.c",
    "pure-data/extra/pd~/pd~.c",
    "pure-data/extra/sigmund~/sigmund~.c",
    "pure-data/extra/stdout/stdout.c",
];

fn main() {
    let libpd = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("../../PdCore/pd-core/src/main/jni/libpd");
    println!("cargo:rerun-if-changed={}", libpd.display());

    let mut build = cc::Build::new();
    build
        .include(libpd.join("libpd_wrapper"))
        .include(libpd.join("libpd_wrapper/util"))
        .include(libpd.join("pure-data/src"))
        .files(
            PD_SRC_FILES
                .iter()
                .chain(PD_EXTRA_FILES)
                .map(|source| libpd.join(source)),
        )
        .define("PD", None)
        .define("USEAPI_DUMMY", None)
        .define("PD_INTERNAL", None)
        .define("HAVE_UNISTD_H", None)
        .define("HAVE_ALLOCA_H", None)
        .define("HAVE_LIBDL", None)
        .define("LIBPD_EXTRA", "1")
        .define("PD_FLOATSIZE", "32")
        .warnings(false)
        .flag("-w")
        .pic(true);

    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("android") => {
            build.define("HAVE_ENDIAN_H", None);
            // Pd's s_loader.c keys the Android external naming (lib<name>.so, ~ -> _tilde) on
            // ANDROID, which NDK clang does not define (it defines __ANDROID__); Android.mk set it.
            build.define("ANDROID", None);
            println!("cargo:rustc-link-lib=dl");
            println!("cargo:rustc-link-lib=log");
            export_pd_api_for_externals();
        }
        // glibc's endian.h satisfies m_private_utils.h on Linux hosts (CI runs the host tests there).
        Ok("linux") => {
            build.define("HAVE_ENDIAN_H", None);
        }
        Ok("macos") => {
            // Pd's endian helpers require the BSD header on macOS, not Linux's endian.h.
            build
                .define("_DARWIN_C_SOURCE", None)
                .define("HAVE_MACHINE_ENDIAN_H", None);
        }
        _ => {}
    }

    // cc emits static link metadata for both the cdylib and Rust's host test binaries.
    build.compile("rjdj_pd");
}

/// Pd C API imported by the five `rj_*` externals (`ScenePlayer/jni/*.c`), which Pd dlopen()s
/// against this cdylib at runtime (EXT-02). rustc's own version script localizes every symbol
/// not exported by Rust (`local: *`); lld gives exact-name patterns priority over wildcards, so a
/// second version script naming these symbols makes them global again. `--export-dynamic-symbol`
/// does not do this for shared objects.
const PD_API_EXPORTS: &[&str] = &[
    "class_new",            // all five
    "class_addmethod",      // all five
    "class_doaddfloat",     // rj_accum (class_addfloat macro)
    "class_domainsignalin", // CLASS_MAINSIGNALIN users
    "pd_new",               // all five
    "gensym",               // all five
    "post",                 // all five
    "outlet_new",           // all five
    "outlet_float",         // all five
    "inlet_new",            // rj_barkflux_accum~
    "dsp_add",              // signal externals
    "sys_getsr",            // rj_accum, rj_barkflux_accum~, rj_centroid~, rj_zcr~
    "s_float",              // outlet type symbol
];

fn export_pd_api_for_externals() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let script = out.join("pd-exports.map");
    let body = format!("{{ global: {}; }};\n", PD_API_EXPORTS.join("; "));
    fs::write(&script, body).expect("write pd-exports.map");
    println!(
        "cargo:rustc-link-arg=-Wl,--version-script={}",
        script.display()
    );
}
