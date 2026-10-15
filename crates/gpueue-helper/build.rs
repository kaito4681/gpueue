//! Builds `forkguard/forkguard.c` into the shared library that the helper
//! embeds and preloads into jobs (see `src/forkguard.rs`).

use std::path::{Path, PathBuf};

fn main() {
    let src = "forkguard/forkguard.c";
    println!("cargo:rerun-if-changed={src}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("forkguard.so");
    // The library is preloaded into programs linked against any glibc, so it
    // depends on libc.so.6 only, which every such program has loaded
    // already. glibc 2.34 moved dlsym into libc; libdl is linked only for an
    // older one, where `--no-undefined` fails the first attempt.
    if !link(src, &out, &[]) && !link(src, &out, &["-ldl"]) {
        panic!("compiling {src} failed");
    }
}

fn link(src: &str, out: &Path, libs: &[&str]) -> bool {
    // Fixed flags so that a given source gives the same library in debug
    // and release builds (its installed file name derives from its bytes).
    let compiler = cc::Build::new()
        .opt_level(2)
        .debug(false)
        .pic(true)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .get_compiler();
    let mut cmd = compiler.to_command();
    cmd.args(["-shared", "-fvisibility=hidden", "-Wl,--no-undefined"])
        .arg("-o")
        .arg(out)
        .arg(src)
        .args(libs);
    // Nix's linker wrapper adds a RUNPATH with the store paths of the
    // libraries it links (glibc). The dynamic loader would then resolve the
    // library's dependencies from that glibc in jobs that use another one,
    // which fails. Turn that off for the wrapper of the host platform.
    for (key, _) in std::env::vars_os() {
        if let Some(salt) = key
            .to_str()
            .and_then(|k| k.strip_prefix("NIX_BINTOOLS_WRAPPER_TARGET_HOST_"))
        {
            cmd.env(format!("NIX_DONT_SET_RPATH_{salt}"), "1");
        }
    }
    cmd.status()
        .unwrap_or_else(|e| panic!("running the C compiler for {src}: {e}"))
        .success()
}
