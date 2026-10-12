//! The few things the helper reads from CRIU's images: which paths a
//! restore makes CRIU open, create or bind as root.
//!
//! CRIU restores regular files (open file descriptors, working and root
//! directories, the executable, file mappings, FIFOs, deleted files it
//! re-creates or links back) by their dump-time path, and binds Unix
//! sockets to theirs. All of these are entries of `files.img`: a
//! `file_entry` (`images/fdinfo.proto`) with a `reg_file_entry`
//! (`images/regfile.proto`, `name` = 6) or a `unix_sk_entry`
//! (`images/sk-unix.proto`, `name` = 11, `name_dir` = 14).
//!
//! An image file is one or two `u32` magic numbers followed by entries,
//! each a little-endian `u32` size and a protobuf message of that size.
//! Only the fields above are decoded; everything else is skipped by its
//! wire type, so newer CRIU versions with more fields are read fine.

use std::fs;
use std::io;
use std::path::Path;

/// `IMG_COMMON_MAGIC` and `IMG_SERVICE_MAGIC` (`criu/include/magic.h`):
/// when an image starts with one of them, the image's own magic follows.
const COMMON_MAGICS: [u32; 2] = [0x5456_4319, 0x5510_5940];

/// Name of the image with every file entry.
pub const FILES_IMAGE: &str = "files.img";

/// What `files.img` says about paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImagePaths {
    /// Paths of regular files and directories CRIU opens (or re-creates)
    /// on restore, in the job's mount namespace.
    pub files: Vec<String>,
    /// Paths Unix sockets were bound to (abstract names are left out).
    pub unix_sockets: Vec<String>,
}

/// Reads `<images_dir>/files.img`; `Ok(None)` if there is none.
pub fn read_paths(images_dir: &Path) -> io::Result<Option<ImagePaths>> {
    match fs::read(images_dir.join(FILES_IMAGE)) {
        Ok(bytes) => parse_files_image(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{FILES_IMAGE}: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parses the contents of `files.img`.
pub fn parse_files_image(bytes: &[u8]) -> Result<ImagePaths, String> {
    let mut out = ImagePaths::default();
    for entry in entries(bytes)? {
        let mut kind = None;
        let mut reg = None;
        let mut usk = None;
        for field in Fields::new(entry) {
            match field? {
                (1, Value::Varint(v)) => kind = Some(v),
                (3, Value::Bytes(b)) => reg = Some(b),
                (16, Value::Bytes(b)) => usk = Some(b),
                _ => {}
            }
        }
        const REG: u64 = 1;
        const UNIXSK: u64 = 5;
        match (kind, reg, usk) {
            (Some(REG), Some(reg), _) => {
                if let Some(name) = string_field(reg, 6)? {
                    push_unique(&mut out.files, name);
                }
            }
            (Some(UNIXSK), _, Some(usk)) => {
                let name = bytes_field(usk, 11)?.unwrap_or_default();
                // Unbound, or an abstract name (leading NUL).
                if name.is_empty() || name[0] == 0 {
                    continue;
                }
                // The kernel's sun_path, which may end in NULs.
                let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                let name = String::from_utf8_lossy(&name[..end]).into_owned();
                let path = match string_field(usk, 14)? {
                    Some(dir) if !name.starts_with('/') => {
                        format!("{}/{name}", dir.trim_end_matches('/'))
                    }
                    _ => name,
                };
                push_unique(&mut out.unix_sockets, path);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// The entries of an image (after its magic numbers).
fn entries(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    let u32_at = |at: usize| -> Option<u32> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let first = u32_at(0).ok_or("too short for a magic number")?;
    let mut pos = if COMMON_MAGICS.contains(&first) { 8 } else { 4 };
    if pos > bytes.len() {
        return Err("too short for its magic numbers".into());
    }
    let mut out = Vec::new();
    while pos < bytes.len() {
        let len = u32_at(pos).ok_or("truncated entry size")? as usize;
        pos += 4;
        let entry = bytes
            .get(pos..pos + len)
            .ok_or_else(|| format!("entry of {len} bytes at {pos} is truncated"))?;
        out.push(entry);
        pos += len;
    }
    Ok(out)
}

/// A decoded protobuf field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value<'a> {
    Varint(u64),
    Fixed64,
    Bytes(&'a [u8]),
    Fixed32,
}

/// Iterates over the fields of a protobuf message.
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> Fields<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            failed: false,
        }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos).ok_or("truncated varint")?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("length overflow")?;
        let s = self.buf.get(self.pos..end).ok_or("truncated field")?;
        self.pos = end;
        Ok(s)
    }

    fn field(&mut self) -> Result<(u64, Value<'a>), String> {
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Value::Fixed64
            }
            2 => {
                let len = usize::try_from(self.varint()?).map_err(|_| "length overflow")?;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed32
            }
            t => return Err(format!("unsupported wire type {t}")),
        };
        Ok((key >> 3, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u64, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.buf.len() {
            return None;
        }
        let r = self.field();
        self.failed = r.is_err();
        Some(r)
    }
}

/// The last length-delimited field `number` of a message.
fn bytes_field(msg: &[u8], number: u64) -> Result<Option<&[u8]>, String> {
    let mut out = None;
    for f in Fields::new(msg) {
        if let (n, Value::Bytes(b)) = f?
            && n == number
        {
            out = Some(b);
        }
    }
    Ok(out)
}

fn string_field(msg: &[u8], number: u64) -> Result<Option<String>, String> {
    Ok(bytes_field(msg, number)?.map(|b| String::from_utf8_lossy(b).into_owned()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal protobuf encoder for the tests.
    pub(crate) fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }
    pub(crate) fn field_varint(n: u64, v: u64, out: &mut Vec<u8>) {
        varint(n << 3, out);
        varint(v, out);
    }
    pub(crate) fn field_bytes(n: u64, b: &[u8], out: &mut Vec<u8>) {
        varint((n << 3) | 2, out);
        varint(b.len() as u64, out);
        out.extend_from_slice(b);
    }

    pub(crate) fn reg_entry(id: u64, name: &str) -> Vec<u8> {
        let mut reg = Vec::new();
        field_varint(1, id, &mut reg);
        field_varint(2, 0o2, &mut reg);
        reg.extend_from_slice(&[0x19, 0, 0, 0, 0, 0, 0, 0, 0]); // pos: fixed64 (field 3, wt 1)
        field_bytes(5, &[8, 0], &mut reg); // fown
        field_bytes(6, name.as_bytes(), &mut reg);
        field_varint(7, 5, &mut reg);
        let mut fe = Vec::new();
        field_varint(1, 1, &mut fe);
        field_varint(2, id, &mut fe);
        field_bytes(3, &reg, &mut fe);
        fe
    }

    pub(crate) fn usk_entry(id: u64, name: &[u8], dir: Option<&str>) -> Vec<u8> {
        let mut usk = Vec::new();
        field_varint(1, id, &mut usk);
        field_bytes(11, name, &mut usk);
        if let Some(d) = dir {
            field_bytes(14, d.as_bytes(), &mut usk);
        }
        usk.extend_from_slice(&[0x85, 0x01, 1, 2, 3, 4]); // field 16 as fixed32: skipped
        let mut fe = Vec::new();
        field_varint(1, 5, &mut fe);
        field_varint(2, id, &mut fe);
        field_bytes(16, &usk, &mut fe);
        fe
    }

    pub(crate) fn image(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x5456_4319u32.to_le_bytes());
        out.extend_from_slice(&0x5658_4321u32.to_le_bytes());
        for e in entries {
            out.extend_from_slice(&(e.len() as u32).to_le_bytes());
            out.extend_from_slice(e);
        }
        out
    }

    #[test]
    fn reads_regular_files_and_bound_sockets() {
        let mut pipe = Vec::new();
        field_varint(1, 2, &mut pipe);
        field_varint(2, 9, &mut pipe);
        let img = image(&[
            reg_entry(1, "/home/alice/x/passwd"),
            pipe,
            reg_entry(2, "/home/alice"),
            reg_entry(3, "/home/alice/x/passwd"),
            usk_entry(4, b"/home/alice/sock", None),
            usk_entry(5, b"\0abstract", None),
            usk_entry(6, b"rel.sock", Some("/srv/run/")),
            usk_entry(7, b"", None),
        ]);
        let p = parse_files_image(&img).unwrap();
        assert_eq!(p.files, ["/home/alice/x/passwd", "/home/alice"]);
        assert_eq!(p.unix_sockets, ["/home/alice/sock", "/srv/run/rel.sock"]);
    }

    /// `files.img` of CRIU 4.2 dumping a Python process with
    /// `/home/t/x/f` open and a socket bound to `/home/t/sock`.
    #[test]
    fn reads_a_real_criu_image() {
        let p = parse_files_image(include_bytes!("testdata/criu-4.2-files.img")).unwrap();
        assert!(p.files.contains(&"/home/t/x/f".to_owned()), "{p:?}");
        assert!(p.files.contains(&"/home/t".to_owned()), "{p:?}");
        assert!(p.files.iter().any(|f| f.contains("python3")), "{p:?}");
        assert_eq!(p.unix_sockets, ["/home/t/sock"]);
    }

    #[test]
    fn rejects_truncated_images() {
        let mut img = image(&[reg_entry(1, "/a")]);
        img.pop();
        assert!(parse_files_image(&img).is_err());
        assert!(parse_files_image(&[1, 2]).is_err());
        // An image with a single magic number and no entries.
        assert_eq!(
            parse_files_image(&0x1234_5678u32.to_le_bytes()).unwrap(),
            ImagePaths::default()
        );
    }
}
