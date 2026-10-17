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
//! Some objects CRIU recreates as root are not paths at all, and the
//! helper refuses to restore them (see `check_image_paths` in
//! `crate::engine`), so they are read too:
//!
//! - internet sockets (`inet_sk_entry`, `images/sk-inet.proto`: `family`
//!   = 3, `type` = 4, `proto` = 5, `state` = 6, `src_port` = 7), which
//!   CRIU binds with `SO_REUSEADDR` and `SO_REUSEPORT` set, as root;
//! - inotify and fanotify descriptors (`ify` = 13, `ffy` = 14), whose
//!   marks CRIU re-adds as root through a path;
//! - listening Unix sockets with an abstract name (`unix_sk_entry`
//!   `state` = 4), which CRIU sets up as root, so a client asking for the
//!   listener's credentials (`SO_PEERCRED`) would be told root;
//! - descriptors in flight in `SCM_RIGHTS` messages (`sk-queues.img`:
//!   `sk_packet_entry`, `images/sk-packet.proto`, `scm` = 4), which
//!   CRIU re-opens as root and passes to the job, out of sight of the
//!   restore hook (see [`read_in_flight_fds`]).
//!
//! libcuda's own listener (`cuda-uvmfd-<pid ns inode>-<pid>`, see
//! [`crate::host::parse_cuda_uvmfd_name`]) is told apart from other
//! abstract listeners; the helper restores it when `pstree.img` (the
//! `pid`, field 1, of each `pstree_entry`, `images/pstree.proto`) has a
//! process with that pid and the job has libcuda mapped.
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

/// Name of the image with the queued messages of the job's sockets.
pub const SK_QUEUES_IMAGE: &str = "sk-queues.img";

/// Name of the image with the job's process tree.
pub const PSTREE_IMAGE: &str = "pstree.img";

/// `fd_types` (`images/fdinfo.proto`).
const REG: u64 = 1;
const INETSK: u64 = 4;
const UNIXSK: u64 = 5;
const INOTIFY: u64 = 8;
const FANOTIFY: u64 = 12;

/// `TCP_LISTEN`, the state CRIU records for a listening socket.
const TCP_LISTEN: u64 = 10;

/// An internet socket of the image bound to a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InetPort {
    /// The IP protocol (`IPPROTO_TCP`, `IPPROTO_UDP`, ...).
    pub proto: u8,
    pub port: u16,
}

impl std::fmt::Display for InetPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match crate::host::inet_proto_name(self.proto) {
            Some(name) => write!(f, "{name} port {}", self.port),
            None => write!(f, "protocol {} port {}", self.proto, self.port),
        }
    }
}

/// What `files.img` says about paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImagePaths {
    /// Paths of regular files and directories CRIU opens (or re-creates)
    /// on restore, in the job's mount namespace.
    pub files: Vec<String>,
    /// Paths Unix sockets were bound to (abstract names are left out).
    pub unix_sockets: Vec<String>,
    /// Abstract names (`@name`, NULs shown as `@`) of listening Unix
    /// sockets, but for libcuda's.
    pub abstract_listeners: Vec<String>,
    /// libcuda's listening sockets (`@cuda-uvmfd-<pid ns inode>-<pid>@`),
    /// with the pid in their name.
    pub cuda_uvmfd_listeners: Vec<(String, u32)>,
    /// The ports internet sockets were bound to.
    pub inet_ports: Vec<InetPort>,
    /// How many inotify and fanotify descriptors there are.
    pub notify_fds: usize,
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
        let mut isk = None;
        let mut usk = None;
        for field in Fields::new(entry) {
            match field? {
                (1, Value::Varint(v)) => kind = Some(v),
                (3, Value::Bytes(b)) => reg = Some(b),
                (4, Value::Bytes(b)) => isk = Some(b),
                (16, Value::Bytes(b)) => usk = Some(b),
                _ => {}
            }
        }
        match (kind, reg, isk, usk) {
            (Some(REG), Some(reg), _, _) => {
                if let Some(name) = string_field(reg, 6)? {
                    push_unique(&mut out.files, name);
                }
            }
            (Some(INETSK), _, Some(isk), _) => {
                let port = varint_field(isk, 7)?.unwrap_or(0);
                if port == 0 {
                    continue; // not bound
                }
                let port = InetPort {
                    proto: u8::try_from(varint_field(isk, 5)?.unwrap_or(0))
                        .map_err(|_| "invalid protocol of an inet socket")?,
                    port: u16::try_from(port).map_err(|_| "invalid port of an inet socket")?,
                };
                if !out.inet_ports.contains(&port) {
                    out.inet_ports.push(port);
                }
            }
            // The marks are in the entry, but they are not looked at: any
            // of them is refused.
            (Some(INOTIFY | FANOTIFY), ..) => out.notify_fds += 1,
            (Some(UNIXSK), _, _, Some(usk)) => {
                let name = bytes_field(usk, 11)?.unwrap_or_default();
                if name.is_empty() {
                    continue; // unbound
                }
                if name[0] == 0 {
                    if varint_field(usk, 4)? == Some(TCP_LISTEN) {
                        let shown =
                            format!("@{}", String::from_utf8_lossy(&name[1..])).replace('\0', "@");
                        // libcuda binds the name with a NUL at its end.
                        let cuda = name[1..]
                            .strip_suffix(b"\0")
                            .and_then(|b| std::str::from_utf8(b).ok())
                            .and_then(crate::host::parse_cuda_uvmfd_name);
                        match cuda {
                            Some((_, pid)) => {
                                if !out.cuda_uvmfd_listeners.iter().any(|(n, _)| *n == shown) {
                                    out.cuda_uvmfd_listeners.push((shown, pid));
                                }
                            }
                            None => push_unique(&mut out.abstract_listeners, shown),
                        }
                    }
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

/// The pids (in the job's pid namespace) of the processes in
/// `<images_dir>/pstree.img`; none if there is no such image.
pub fn read_pids(images_dir: &Path) -> io::Result<Vec<u32>> {
    match fs::read(images_dir.join(PSTREE_IMAGE)) {
        Ok(bytes) => parse_pstree_image(&bytes).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{PSTREE_IMAGE}: {e}"))
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The `pid` of each entry of `pstree.img`.
pub fn parse_pstree_image(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for entry in entries(bytes)? {
        let pid = varint_field(entry, 1)?.ok_or("a process without a pid")?;
        out.push(u32::try_from(pid).map_err(|_| "invalid pid of a process")?);
    }
    Ok(out)
}

/// Whether `<images_dir>/sk-queues.img` has a message with descriptors in
/// flight (`SCM_RIGHTS`); `false` if there is no such image.
pub fn read_in_flight_fds(images_dir: &Path) -> io::Result<bool> {
    match fs::read(images_dir.join(SK_QUEUES_IMAGE)) {
        Ok(bytes) => parse_sk_queues_image(&bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{SK_QUEUES_IMAGE}: {e}"),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether a message in `sk-queues.img` carries control messages (CRIU
/// only saves `SCM_RIGHTS`, and refuses others itself). Each entry there
/// is followed by the message's data, `length` (= 2) bytes; a message
/// with control messages is reported before the rest is read.
pub fn parse_sk_queues_image(bytes: &[u8]) -> Result<bool, String> {
    let mut pos = magic_len(bytes)?;
    while pos < bytes.len() {
        let entry = entry_at(bytes, &mut pos)?;
        let mut length = 0;
        for field in Fields::new(entry) {
            match field? {
                (2, Value::Varint(v)) => length = v,
                (4, _) => return Ok(true),
                _ => {}
            }
        }
        let length = usize::try_from(length).map_err(|_| "length overflow")?;
        pos = pos
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| format!("the data of {length} bytes at {pos} is truncated"))?;
    }
    Ok(false)
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at.checked_add(4)?)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The length of an image's magic numbers.
fn magic_len(bytes: &[u8]) -> Result<usize, String> {
    let first = u32_at(bytes, 0).ok_or("too short for a magic number")?;
    let pos = if COMMON_MAGICS.contains(&first) { 8 } else { 4 };
    if pos > bytes.len() {
        return Err("too short for its magic numbers".into());
    }
    Ok(pos)
}

/// The entry at `*pos`, which is moved past it.
fn entry_at<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
    let len = u32_at(bytes, *pos).ok_or("truncated entry size")? as usize;
    *pos += 4;
    let entry = bytes
        .get(*pos..*pos + len)
        .ok_or_else(|| format!("entry of {len} bytes at {pos} is truncated"))?;
    *pos += len;
    Ok(entry)
}

/// The entries of an image (after its magic numbers).
fn entries(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut pos = magic_len(bytes)?;
    let mut out = Vec::new();
    while pos < bytes.len() {
        out.push(entry_at(bytes, &mut pos)?);
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

/// The last varint field `number` of a message.
fn varint_field(msg: &[u8], number: u64) -> Result<Option<u64>, String> {
    let mut out = None;
    for f in Fields::new(msg) {
        if let (n, Value::Varint(v)) = f?
            && n == number
        {
            out = Some(v);
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

    fn file_entry(kind: u64, id: u64, field: u64, inner: &[u8]) -> Vec<u8> {
        let mut fe = Vec::new();
        field_varint(1, kind, &mut fe);
        field_varint(2, id, &mut fe);
        field_bytes(field, inner, &mut fe);
        fe
    }

    #[test]
    fn reads_inet_ports_notify_fds_and_abstract_listeners() {
        let isk = |proto: u64, port: u64| {
            let mut isk = Vec::new();
            field_varint(1, 1, &mut isk);
            field_varint(3, 10, &mut isk); // AF_INET6
            field_varint(5, proto, &mut isk);
            field_varint(6, 10, &mut isk);
            field_varint(7, port, &mut isk);
            file_entry(INETSK, 1, 4, &isk)
        };
        let usk = |name: &[u8], state: u64| {
            let mut usk = Vec::new();
            field_varint(4, state, &mut usk);
            field_bytes(11, name, &mut usk);
            file_entry(UNIXSK, 2, 16, &usk)
        };
        let img = image(&[
            isk(6, 8888),
            isk(17, 8888),
            isk(6, 8888),
            isk(6, 0), // not bound
            file_entry(INOTIFY, 3, 13, &[8, 3]),
            file_entry(FANOTIFY, 4, 14, &[8, 4]),
            usk(b"\0listen", TCP_LISTEN),
            usk(b"\0dgram", 7),
            usk(b"/tmp/l.sock", TCP_LISTEN),
            // libcuda's, and near misses.
            usk(b"\0cuda-uvmfd-4026532358-2\0", TCP_LISTEN),
            usk(b"\0cuda-uvmfd-4026532358-2", TCP_LISTEN),
            usk(b"\0cuda-uvmfd-4026532358-02\0", TCP_LISTEN),
            usk(b"\0cuda-uvmfd-4026532358-3\0", 7),
        ]);
        let p = parse_files_image(&img).unwrap();
        assert_eq!(
            p.inet_ports,
            [
                InetPort {
                    proto: 6,
                    port: 8888
                },
                InetPort {
                    proto: 17,
                    port: 8888
                }
            ]
        );
        assert_eq!(p.inet_ports[0].to_string(), "TCP port 8888");
        assert_eq!(p.notify_fds, 2);
        assert_eq!(
            p.abstract_listeners,
            [
                "@listen",
                "@cuda-uvmfd-4026532358-2",
                "@cuda-uvmfd-4026532358-02@"
            ]
        );
        assert_eq!(
            p.cuda_uvmfd_listeners,
            [("@cuda-uvmfd-4026532358-2@".to_owned(), 2)]
        );
        assert_eq!(p.unix_sockets, ["/tmp/l.sock"]);
    }

    #[test]
    fn reads_pids_of_the_process_tree() {
        let entry = |pid: u64, threads: &[u64]| {
            let mut e = Vec::new();
            field_varint(1, pid, &mut e);
            field_varint(2, 1, &mut e);
            for t in threads {
                field_varint(5, *t, &mut e);
            }
            e
        };
        let img = image(&[entry(1, &[1]), entry(2, &[2, 7]), entry(3, &[3])]);
        assert_eq!(parse_pstree_image(&img).unwrap(), [1, 2, 3]);
        assert!(parse_pstree_image(&image(&[vec![16, 1]])).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pids(dir.path()).unwrap().is_empty());
        fs::write(dir.path().join(PSTREE_IMAGE), &img).unwrap();
        assert_eq!(read_pids(dir.path()).unwrap(), [1, 2, 3]);
    }

    #[test]
    fn finds_descriptors_in_flight() {
        // `sk_packet_entry` followed by its data.
        let packet = |scm: bool, data: &[u8]| {
            let mut pe = Vec::new();
            field_varint(1, 7, &mut pe);
            field_varint(2, data.len() as u64, &mut pe);
            if scm {
                let mut se = Vec::new();
                field_varint(1, 1, &mut se);
                field_varint(2, 12, &mut se);
                field_bytes(4, &se, &mut pe);
            }
            let mut out = (pe.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(&pe);
            out.extend_from_slice(data);
            out
        };
        let mut img = image(&[]);
        img.extend(packet(false, b"hello"));
        img.extend(packet(false, &[0, 0, 0, 0, 0]));
        assert_eq!(parse_sk_queues_image(&img), Ok(false));
        let mut with_scm = img.clone();
        with_scm.extend(packet(true, b"x"));
        assert_eq!(parse_sk_queues_image(&with_scm), Ok(true));
        img.pop();
        assert!(parse_sk_queues_image(&img).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(!read_in_flight_fds(dir.path()).unwrap());
        fs::write(dir.path().join(SK_QUEUES_IMAGE), &with_scm).unwrap();
        assert!(read_in_flight_fds(dir.path()).unwrap());
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
