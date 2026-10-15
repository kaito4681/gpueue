//! Free-space precheck for the image directory.
//!
//! A checkpoint image is roughly "host RAM in use + VRAM in use" (plus the
//! contents of the job's private tmpfs mounts and deleted files saved in
//! it) and is not compressed, so a dump is only attempted when at least
//! `(rss + vram) * 1.2` bytes are available on the target filesystem, with
//! those extra bytes counted as RSS.

use std::io;
use std::path::Path;

/// `statfs` magic number of tmpfs.
const TMPFS_MAGIC: i128 = 0x0102_1994;

/// Bytes needed to dump a job with `rss_bytes` of host memory and
/// `vram_bytes` of GPU memory: `ceil((rss + vram) * 1.2)`, saturating.
pub fn required_bytes(rss_bytes: u64, vram_bytes: u64) -> u64 {
    let total = u128::from(rss_bytes) + u128::from(vram_bytes);
    let required = (total * 6).div_ceil(5);
    u64::try_from(required).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not enough free space for the checkpoint: need {required} bytes, {available} available")]
pub struct InsufficientSpace {
    pub required: u64,
    pub available: u64,
}

/// Pure check: `Ok(required)` when `available_bytes` suffices.
pub fn check_free_space(
    rss_bytes: u64,
    vram_bytes: u64,
    available_bytes: u64,
) -> Result<u64, InsufficientSpace> {
    let required = required_bytes(rss_bytes, vram_bytes);
    if available_bytes >= required {
        Ok(required)
    } else {
        Err(InsufficientSpace {
            required,
            available: available_bytes,
        })
    }
}

/// Bytes available to unprivileged users on the filesystem holding `path`
/// (`f_bavail * f_frsize`).
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let st = rustix::fs::statvfs(path)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Bytes in use on the tmpfs mounted at `path` (`(f_blocks - f_bfree) *
/// f_bsize`), or `None` when `path` is not on tmpfs. CRIU saves the
/// contents of a tmpfs that a dumped mount namespace has of its own in the
/// image, so they need room there as well.
pub fn tmpfs_used_bytes(path: &Path) -> io::Result<Option<u64>> {
    let st = rustix::fs::statfs(path)?;
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    let ty = i128::from(st.f_type);
    if ty != TMPFS_MAGIC {
        return Ok(None);
    }
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    let (blocks, free, bsize) = (
        u64::try_from(st.f_blocks).unwrap_or(0),
        u64::try_from(st.f_bfree).unwrap_or(0),
        u64::try_from(st.f_bsize).unwrap_or(0),
    );
    Ok(Some(blocks.saturating_sub(free).saturating_mul(bsize)))
}

/// Whether `path` lives on tmpfs (not allowed as an image location: images
/// would compete with the job for RAM).
pub fn is_tmpfs(path: &Path) -> io::Result<bool> {
    let st = rustix::fs::statfs(path)?;
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    let ty = i128::from(st.f_type);
    Ok(ty == TMPFS_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_is_120_percent_rounded_up() {
        assert_eq!(required_bytes(0, 0), 0);
        assert_eq!(required_bytes(5, 0), 6);
        assert_eq!(required_bytes(1, 0), 2); // 1.2 -> 2
        assert_eq!(required_bytes(10, 0), 12);
        assert_eq!(required_bytes(3, 2), 6);
        assert_eq!(required_bytes(4, 0), 5); // 4.8 -> 5
        assert_eq!(required_bytes(5 << 30, 5 << 30), 12 << 30);
        assert_eq!(required_bytes(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn check() {
        assert_eq!(check_free_space(100, 400, 600), Ok(600));
        assert_eq!(check_free_space(100, 400, 1000), Ok(600));
        assert_eq!(
            check_free_space(100, 400, 599),
            Err(InsufficientSpace {
                required: 600,
                available: 599
            })
        );
        assert_eq!(check_free_space(0, 0, 0), Ok(0));
    }

    #[test]
    fn statvfs_on_real_fs() {
        let dir = std::env::temp_dir();
        // Only check that the wrappers work; values depend on the host.
        available_bytes(&dir).unwrap();
        is_tmpfs(&dir).unwrap();
        assert!(!is_tmpfs(Path::new("/proc")).unwrap());
        assert!(available_bytes(Path::new("/definitely/not/here")).is_err());
        assert_eq!(tmpfs_used_bytes(Path::new("/proc")).unwrap(), None);
        // /dev/shm is a tmpfs on most systems (and in the test container).
        if is_tmpfs(Path::new("/dev/shm")).unwrap_or(false) {
            let before = tmpfs_used_bytes(Path::new("/dev/shm")).unwrap().unwrap();
            let f = Path::new("/dev/shm").join(format!("gpueue-space-{}", std::process::id()));
            std::fs::write(&f, vec![1u8; 1 << 20]).unwrap();
            let after = tmpfs_used_bytes(Path::new("/dev/shm")).unwrap().unwrap();
            std::fs::remove_file(&f).unwrap();
            assert!(after >= before + (1 << 20) - 4096, "{before} -> {after}");
        }
    }
}
