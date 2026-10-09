//! The fork guard: a small preload library that lets jobs with forked,
//! CUDA-less children be checkpointed (`forkguard/forkguard.c`).
//!
//! A child that a CUDA process forks without exec (e.g. a PyTorch
//! DataLoader worker with the default `fork` start method) inherits the
//! parent's mappings and descriptors of `/dev/nvidia*`. It cannot use them,
//! but CRIU cannot dump them, and cuda-checkpoint only releases them in
//! processes with a CUDA context. The library interposes `fork()` and drops
//! them in the child, so such jobs checkpoint without changes.
//!
//! The library is built by `build.rs` and embedded in the helper, which
//! installs it at start-up into a root-owned directory readable by every
//! user (`<data_dir>/lib` by default) and puts it first in each job's
//! `LD_PRELOAD`. Its file name contains a hash of its contents and old
//! versions are never removed: a restored process maps the library by path
//! and CRIU expects the same file, so a checkpoint taken before an upgrade
//! still restores afterwards.
//!
//! Not covered: statically linked programs (no `LD_PRELOAD`) and programs
//! that create processes without the dynamic `fork()` symbol (raw
//! `clone(2)`, `_Fork()`). `vfork()`/`posix_spawn()` children exec right
//! away, which drops the mappings anyway. NVML cannot be initialized in a
//! forked child of a CUDA process with the guard (it reuses the descriptors
//! inherited from libcuda); CUDA itself is unusable in such a child in any
//! case.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The library, compiled from `forkguard/forkguard.c`.
pub const LIBRARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/forkguard.so"));

const NAME_PREFIX: &str = "puzuz-forkguard-";

/// 64-bit FNV-1a: a stable content hash for the file name (not a security
/// measure; the directory is root-owned).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// `puzuz-forkguard-<hash of the contents>.so`.
pub fn file_name() -> String {
    format!("{NAME_PREFIX}{:016x}.so", fnv1a(LIBRARY))
}

/// Writes the library into `dir` (created with mode 0755) unless it is
/// already there with the same contents, and returns its path. The file is
/// replaced atomically, so processes that map an existing one keep it.
pub fn install(dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    let path = dir.join(file_name());
    if fs::read(&path).is_ok_and(|b| b == LIBRARY) {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        return Ok(path);
    }
    let tmp = dir.join(format!(".{}.tmp-{}", file_name(), std::process::id()));
    let res = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        f.write_all(LIBRARY)?;
        f.set_permissions(fs::Permissions::from_mode(0o644))?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.map(|()| path)
}

/// Puts `lib` first in `LD_PRELOAD`, keeping the job's own entries except
/// other fork guards (a job that runs `puzuz add` passes its own
/// `LD_PRELOAD` on).
pub fn add_to_preload(envs: &mut BTreeMap<String, String>, lib: &Path) {
    let mut entries = vec![lib.to_string_lossy().into_owned()];
    if let Some(old) = envs.get("LD_PRELOAD") {
        entries.extend(
            old.split([':', ' '])
                .filter(|e| !e.is_empty())
                .filter(|e| {
                    !Path::new(e)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(NAME_PREFIX))
                })
                .map(str::to_owned),
        );
    }
    envs.insert("LD_PRELOAD".into(), entries.join(":"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn library_is_a_shared_object() {
        assert!(LIBRARY.starts_with(b"\x7fELF"));
        assert!(file_name().starts_with(NAME_PREFIX) && file_name().ends_with(".so"));
    }

    #[test]
    fn install_is_idempotent_and_world_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("lib");
        let a = install(&dir).unwrap();
        assert_eq!(fs::read(&a).unwrap(), LIBRARY);
        assert_eq!(
            fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let ino = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&a).unwrap());
        assert_eq!(install(&dir).unwrap(), a);
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&a).unwrap()),
            ino,
            "an identical library is not rewritten"
        );
        // A damaged file is replaced.
        fs::write(&a, b"junk").unwrap();
        install(&dir).unwrap();
        assert_eq!(fs::read(&a).unwrap(), LIBRARY);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary files left"
        );
    }

    #[test]
    fn preload_keeps_the_jobs_entries() {
        let lib = Path::new("/var/lib/puzuz/lib/puzuz-forkguard-1.so");
        let mut envs = BTreeMap::new();
        add_to_preload(&mut envs, lib);
        assert_eq!(envs["LD_PRELOAD"], lib.to_str().unwrap());
        envs.insert(
            "LD_PRELOAD".into(),
            "/x/a.so /var/lib/puzuz/lib/puzuz-forkguard-0.so:/y/b.so".into(),
        );
        add_to_preload(&mut envs, lib);
        assert_eq!(
            envs["LD_PRELOAD"],
            "/var/lib/puzuz/lib/puzuz-forkguard-1.so:/x/a.so:/y/b.so"
        );
    }

    /// The real thing with a stand-in for `/dev/nvidia`: a forked child
    /// loses the parent's mappings and descriptors of matching files, the
    /// parent keeps them, other files are untouched.
    #[test]
    fn forked_children_drop_device_mappings_and_fds() {
        if Command::new("python3").arg("-V").output().is_err() {
            eprintln!("skipped: no python3");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let lib = install(&tmp.path().join("lib")).unwrap();
        let dev = tmp.path().join("fakedev");
        fs::create_dir(&dev).unwrap();
        for f in ["ctl", "uvm"] {
            fs::write(dev.join(f), vec![0u8; 8192]).unwrap();
        }
        let other = tmp.path().join("other");
        fs::write(&other, vec![0u8; 8192]).unwrap();
        let script = r#"
import mmap, os, sys
dev, other = sys.argv[1], sys.argv[2]
keep = []
def mapped(name):
    return sum(1 for l in open("/proc/self/maps") if name in l)
def fds(name):
    n = 0
    for fd in os.listdir("/proc/self/fd"):
        try:
            n += name in os.readlink(f"/proc/self/fd/{fd}")
        except OSError:
            pass
    return n
def state():
    return (mapped(dev), fds(dev), mapped(other), fds(other))
for path in [dev + "/ctl", dev + "/uvm", other]:
    f = open(path, "r+b")
    keep.append(f)
    for _ in range(150):  # 300 device mappings: more than one pass of the guard
        keep.append(mmap.mmap(f.fileno(), 4096))
before = state()
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.write(w, repr(state()).encode())
    os._exit(0)
os.waitpid(pid, 0)
child = eval(os.read(r, 1000).decode())
after = state()
assert before[0] == 300 and before[1] > 0, before
assert child[:2] == (0, 0), ("device state left in the child", child)
assert child[2:] == before[2:], ("other files changed in the child", before, child)
assert after == before, ("the parent changed", before, after)
print("ok")
"#;
        let out = Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg(script)
            .arg(&dev)
            .arg(&other)
            .env("LD_PRELOAD", &lib)
            .env("PUZUZ_FORKGUARD_TEST_PREFIX", &dev)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.trim(), "ok");
    }
}
