//! Binaries built with Nix: copies of them still load their libraries (and the dynamic loader)
//! from the Nix store, so the store path they came from must survive garbage collection.
//! `service install` registers it as a GC root while the installed binaries need it.

use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::receipt::{BINARIES, RECEIPT_PATH};
use crate::system::{System, run_checked};

pub const NIX_STORE: &str = "/nix/store";
/// The garbage-collector root `service install` registers for binaries from the Nix store.
pub const GC_ROOT: &str = "/nix/var/nix/gcroots/puzuz";
/// Where `nix-store` is when it is not in systemd's default `PATH` (sudo's `secure_path` hides
/// the profile of a multi-user Nix installation).
const NIX_STORE_BINARIES: &[&str] = &[
    "/nix/var/nix/profiles/default/bin/nix-store",
    "/run/current-system/sw/bin/nix-store",
];

/// A GC root and the store path it keeps, as recorded in the receipt.
pub type GcRoot = (Option<PathBuf>, Option<PathBuf>);

/// The store path (`/nix/store/<hash>-<name>`) the system path `exe` resolves into, if any.
pub fn store_path_of(sys: &dyn System, exe: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(sys.root()).ok()?;
    let resolved = std::fs::canonicalize(sys.path(exe)).ok()?;
    let system = Path::new("/").join(resolved.strip_prefix(&root).ok()?);
    let entry = system.strip_prefix(NIX_STORE).ok()?.components().next()?;
    Some(Path::new(NIX_STORE).join(entry))
}

/// The program interpreter (`PT_INTERP`, the dynamic loader) of a 64-bit little-endian ELF
/// file, read from `file`; `None` for anything else (scripts, static binaries).
pub fn elf_interpreter(file: &std::fs::File) -> Option<PathBuf> {
    const PT_INTERP: u32 = 3;
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0).ok()?;
    if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return None;
    }
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let u64_at = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let phoff = u64_at(&header, 0x20);
    let phentsize = usize::from(u16_at(&header, 0x36));
    let phnum = usize::from(u16_at(&header, 0x38));
    if phentsize < 56 || phnum == 0 || phnum > 256 {
        return None;
    }
    let mut table = vec![0u8; phentsize * phnum];
    file.read_exact_at(&mut table, phoff).ok()?;
    let interp = table
        .chunks_exact(phentsize)
        .find(|entry| u32_at(entry, 0) == PT_INTERP)?;
    let (offset, size) = (u64_at(interp, 8), u64_at(interp, 32));
    if size == 0 || size > 4096 {
        return None;
    }
    let mut path = vec![0u8; size as usize];
    file.read_exact_at(&mut path, offset).ok()?;
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    Some(PathBuf::from(String::from_utf8(path[..end].to_vec()).ok()?))
}

/// Whether the binaries in `bin_dir` load their dynamic loader from the Nix store (they are
/// built with Nix, wherever they were copied to).
pub fn binaries_need_store(sys: &dyn System, bin_dir: &Path) -> bool {
    BINARIES.iter().any(|name| {
        std::fs::File::open(sys.path(&bin_dir.join(name)))
            .ok()
            .and_then(|file| elf_interpreter(&file))
            .is_some_and(|interp| interp.starts_with(NIX_STORE))
    })
}

fn manual_command(store_path: &str) -> String {
    format!("`sudo nix-store --add-root {GC_ROOT} --realise {store_path}`")
}

/// Registers [`GC_ROOT`] for `store_path` with `nix-store`. Returns whether it worked; a failure
/// is only a warning.
fn add_gc_root(sys: &dyn System, store_path: &Path, out: &mut dyn Write) -> bool {
    let program = crate::system::find_program(sys, Path::new("nix-store")).or_else(|| {
        NIX_STORE_BINARIES
            .iter()
            .map(PathBuf::from)
            .find(|path| sys.path(path).is_file())
    });
    let store_path_text = store_path.to_string_lossy();
    let manual = format!(
        "register it yourself so that garbage collection keeps what the binaries use: {}",
        manual_command(&store_path_text)
    );
    let Some(program) = program else {
        writeln!(
            out,
            "warning: nix-store not found; the binaries come from {store_path_text} but no GC \
             root was registered; {manual}"
        )
        .ok();
        return false;
    };
    match run_checked(
        sys,
        &program.to_string_lossy(),
        &["--add-root", GC_ROOT, "--realise", &store_path_text],
    ) {
        Ok(_) => {
            writeln!(
                out,
                "registered the Nix GC root {GC_ROOT} for {store_path_text}"
            )
            .ok();
            true
        }
        Err(err) => {
            writeln!(out, "warning: {err}; {manual}").ok();
            false
        }
    }
}

/// Removes the GC root `root` (only ever [`GC_ROOT`], and only a symlink).
pub fn remove_gc_root(sys: &dyn System, root: &Path, out: &mut dyn Write) {
    if root != Path::new(GC_ROOT) {
        writeln!(
            out,
            "warning: ignoring the GC root {} in {RECEIPT_PATH} (expected {GC_ROOT})",
            root.display()
        )
        .ok();
        return;
    }
    let local = sys.path(root);
    match std::fs::symlink_metadata(&local) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::remove_file(&local) {
            Ok(()) => {
                writeln!(out, "removed the Nix GC root {GC_ROOT}").ok();
            }
            Err(err) => {
                writeln!(out, "warning: removing {GC_ROOT}: {err}").ok();
            }
        },
        Ok(_) => {
            writeln!(out, "warning: kept {GC_ROOT} (not a symlink)").ok();
        }
        Err(_) => {}
    }
}

/// The GC root of the installation whose binaries are now in `bin_dir`, decided from those
/// binaries (not from where `puzuz` ran):
/// - they do not use the store: the previous root, if any, is removed;
/// - they come from the store path `store_path` (this `puzuz` runs from it): that is
///   registered; if that fails, the previous root is kept (never orphaned);
/// - they use the store but are copies of copies (no store path at hand): the previous root
///   stays, or, without one, a warning says how to register it.
pub fn decide_gc_root(
    sys: &dyn System,
    bin_dir: &Path,
    store_path: Option<&Path>,
    previous: GcRoot,
    out: &mut dyn Write,
) -> GcRoot {
    if !binaries_need_store(sys, bin_dir) {
        if let Some(root) = &previous.0 {
            remove_gc_root(sys, root, out);
        }
        return (None, None);
    }
    match store_path {
        Some(store_path) => {
            if add_gc_root(sys, store_path, out) {
                (Some(PathBuf::from(GC_ROOT)), Some(store_path.to_owned()))
            } else {
                if previous.0.is_some() {
                    writeln!(out, "kept the previous Nix GC root {GC_ROOT}").ok();
                }
                previous
            }
        }
        None if previous.0.is_some() => previous,
        None => {
            writeln!(
                out,
                "warning: the binaries in {} load their libraries from {NIX_STORE}, but the \
                 store path they came from is unknown, so no GC root was registered; register \
                 one yourself: {}",
                bin_dir.display(),
                manual_command("<the store path of puzuz>")
            )
            .ok();
            (None, None)
        }
    }
}
