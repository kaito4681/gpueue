//! Builds `forkguard/forkguard.c` into the shared library that the helper
//! embeds and preloads into jobs (see `src/forkguard.rs`).

use std::path::PathBuf;

fn main() {
    let src = "forkguard/forkguard.c";
    println!("cargo:rerun-if-changed={src}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("forkguard.so");
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
    let status = compiler
        .to_command()
        .args(["-shared", "-fvisibility=hidden", "-Wl,--no-undefined"])
        .arg("-o")
        .arg(&out)
        .arg(src)
        .arg("-ldl")
        .status()
        .unwrap_or_else(|e| panic!("running the C compiler for {src}: {e}"));
    assert!(status.success(), "compiling {src} failed");
}
