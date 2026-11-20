// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::marker::PhantomData;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use koidrive_ignore::NameKey;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::{FileId, is_gone, stat_path};

const MAX_GITFILE: u64 = 1 << 20;
const MAX_INDEX: u64 = 1 << 30;
const MAX_PATH: usize = 4096;

/// The index of a work tree (RFC 0033 §3): a folder that holds a `.git` folder, or a `.git` file whose
/// `gitdir:` line names its git directory. Paths are relative to the work tree and compared by `K`'s key.
pub struct GitIndex<K> {
    folder: PathBuf,
    git_dir: Option<PathBuf>,
    read: Option<Stamp>,
    answers: Option<Answers>,
    key: PhantomData<fn() -> K>,
}

#[derive(PartialEq)]
struct Stamp {
    dev: u64,
    id: FileId,
    size: u64,
    mtime_ns: i128,
}

#[derive(PartialEq)]
struct Answers {
    listed: HashSet<String>,
    sparse: HashSet<String>,
}

struct Entry {
    name: Vec<u8>,
    mode: u32,
}

struct Index<'a> {
    entries: Vec<Entry>,
    link: Option<&'a [u8]>,
    checksum: &'a [u8],
}

impl<K: NameKey> GitIndex<K> {
    /// The work tree at `folder`, with no answers until the first [`GitIndex::refresh`].
    pub fn find(folder: &Path) -> Option<GitIndex<K>> {
        match fs::symlink_metadata(folder.join(".git")) {
            Err(err) if is_gone(&err) => None,
            _ => Some(GitIndex {
                folder: folder.to_path_buf(),
                git_dir: None,
                read: None,
                answers: None,
                key: PhantomData,
            }),
        }
    }

    /// Reads the index again when it is unreadable or its size, modification time or identity changed, and
    /// returns whether the answers changed. An index that is missing, being replaced or unreadable keeps the
    /// answers of the last read.
    pub fn refresh(&mut self) -> bool {
        self.git_dir = git_dir(&self.folder);
        let current = self.current();
        if let Some((_, stamp)) = &current
            && self.read.as_ref() == Some(stamp)
        {
            return false;
        }
        let read = current.and_then(|(git_dir, stamp)| Some((stamp, answers::<K>(&git_dir)?)));
        let Some((stamp, answers)) = read else {
            self.read = None;
            return false;
        };
        self.read = Some(stamp);
        let changed = self.answers.as_ref() != Some(&answers);
        self.answers = Some(answers);
        changed
    }

    /// Whether the last [`GitIndex::refresh`] read the index.
    pub fn readable(&self) -> bool {
        self.read.is_some()
    }

    /// Whether the index lists the file at `path`, or `None` before the first read and below a sparse
    /// directory entry.
    pub fn lists(&self, path: &Path) -> Option<bool> {
        let answers = self.answers.as_ref()?;
        let utf8 = path.to_str().is_some();
        let path = key::<K>(&path.as_os_str().to_string_lossy());
        let sparse =
            (path.match_indices('/')).any(|(end, _)| answers.sparse.contains(&path[..end]));
        (!sparse).then(|| utf8 && answers.listed.contains(&path))
    }

    /// The keys of the paths the index lists.
    pub fn listed(&self) -> impl Iterator<Item = &str> {
        (self.answers.iter()).flat_map(|answers| answers.listed.iter().map(String::as_str))
    }

    /// The git directory the last [`GitIndex::refresh`] looked in, whose `index` and `index.lock` a watcher
    /// watches.
    pub fn git_dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }

    fn current(&self) -> Option<(PathBuf, Stamp)> {
        let git_dir = self.git_dir.clone()?;
        if fs::symlink_metadata(git_dir.join("index.lock")).is_ok() {
            return None;
        }
        let stat = stat_path(&git_dir.join("index")).ok()?;
        let stamp = Stamp {
            dev: stat.dev,
            id: stat.id,
            size: stat.size,
            mtime_ns: stat.mtime_ns,
        };
        Some((git_dir, stamp))
    }
}

pub(crate) struct Spelled;

impl NameKey for Spelled {
    fn key(name: &str) -> String {
        name.to_owned()
    }
}

fn git_dir(folder: &Path) -> Option<PathBuf> {
    let dot_git = folder.join(".git");
    if fs::symlink_metadata(&dot_git).ok()?.is_dir() {
        return Some(dot_git);
    }
    let text = read(&dot_git, MAX_GITFILE).ok()?;
    let target = path_line(text.strip_prefix(b"gitdir: ")?);
    (!target.is_empty()).then(|| folder.join(target))
}

fn read(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let file = (fs::File::options().read(true))
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

fn path_line(line: &[u8]) -> &OsStr {
    let end = line.len()
        - (line.iter().rev())
            .take_while(|b| matches!(b, b'\n' | b'\r'))
            .count();
    OsStr::from_bytes(&line[..end])
}

fn hash_size(git_dir: &Path) -> Option<usize> {
    let optional = |path: PathBuf| match read(&path, MAX_GITFILE) {
        Ok(bytes) => Some(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    };
    let common = match optional(git_dir.join("commondir"))? {
        Some(line) => git_dir.join(path_line(&line)),
        None => git_dir.to_path_buf(),
    };
    let config = optional(common.join("config"))?.unwrap_or_default();
    match object_format(&String::from_utf8_lossy(&config)).as_deref() {
        None | Some("sha1") => Some(20),
        Some("sha256") => Some(32),
        Some(_) => None,
    }
}

fn object_format(config: &str) -> Option<String> {
    let config = config.strip_prefix('\u{feff}').unwrap_or(config);
    let mut joined = String::with_capacity(config.len());
    let (mut quoted, mut comment) = (false, false);
    let mut chars = config.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\n' => (quoted, comment) = (false, false),
            _ if comment => continue,
            '"' => {
                quoted = !quoted;
                continue;
            }
            '#' | ';' if !quoted => {
                comment = true;
                continue;
            }
            '\\' => {
                match chars.next() {
                    Some('\n') => {}
                    Some('\r') if chars.next_if_eq(&'\n').is_some() => {}
                    next => joined.extend([Some(char), next].into_iter().flatten()),
                }
                continue;
            }
            _ => {}
        }
        joined.push(char);
    }
    let mut section = String::new();
    let mut format = None;
    for line in joined.lines() {
        let mut line = line.trim();
        if let Some((name, rest)) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
            section = name.trim().to_ascii_lowercase();
            line = rest.trim();
        }
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        if section == "extensions" && key.trim().eq_ignore_ascii_case("objectformat") {
            format = Some(value.trim().to_owned());
        }
    }
    format
}

fn answers<K: NameKey>(git_dir: &Path) -> Option<Answers> {
    let hash = hash_size(git_dir)?;
    let bytes = read(&git_dir.join("index"), MAX_INDEX).ok()?;
    let index = parse(&bytes, hash)?;
    let entries = match index.link {
        None => index.entries,
        Some(link) => {
            let (base, bitmaps) = link.split_at_checked(hash)?;
            if base.iter().all(|byte| *byte == 0) {
                index.entries
            } else {
                let hex: String = base.iter().map(|byte| format!("{byte:02x}")).collect();
                let shared = read(&git_dir.join(format!("sharedindex.{hex}")), MAX_INDEX).ok()?;
                let shared = parse(&shared, hash)?;
                if shared.checksum != base {
                    return None;
                }
                merge(shared.entries, index.entries, bitmaps)?
            }
        }
    };
    let mut answers = Answers {
        listed: HashSet::new(),
        sparse: HashSet::new(),
    };
    for entry in entries {
        match entry.mode {
            0o100644 | 0o100755 => {
                if let Some(name) = verified(&entry.name) {
                    answers.listed.insert(key::<K>(name));
                }
            }
            0o040000 => {
                if let Some(name) = verified(entry.name.strip_suffix(b"/")?) {
                    answers.sparse.insert(key::<K>(name));
                }
            }
            _ => {}
        }
    }
    Some(answers)
}

fn verified(path: &[u8]) -> Option<&str> {
    let path = std::str::from_utf8(path).ok()?;
    (path.split('/'))
        .all(|name| !matches!(name, "" | "." | "..") && !name.eq_ignore_ascii_case(".git"))
        .then_some(path)
}

fn key<K: NameKey>(path: &str) -> String {
    let keys: Vec<String> = path.split('/').map(K::key).collect();
    keys.join("/")
}

fn parse(bytes: &[u8], hash: usize) -> Option<Index<'_>> {
    let (body, checksum) = bytes.split_at(bytes.len().checked_sub(hash)?);
    let digest = match hash {
        20 => Sha1::digest(body).to_vec(),
        _ => Sha256::digest(body).to_vec(),
    };
    if checksum.iter().any(|byte| *byte != 0) && digest != checksum {
        return None;
    }
    let mut r = Reader(body);
    if r.take(4)? != b"DIRC" {
        return None;
    }
    let version = r.u32()?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = r.u32()?;
    let mut entries = Vec::new();
    let mut name = Vec::new();
    let mut decoded = 0u64;
    for _ in 0..count {
        let start = r.0.len();
        let fixed = r.take(40 + hash)?;
        let mode = u32::from_be_bytes(fixed[24..28].try_into().ok()?);
        let flags = r.u16()?;
        if flags & 0x4000 != 0 && (version == 2 || r.u16()? & !0x6000 != 0) {
            return None;
        }
        if version == 4 {
            name.truncate(name.len().checked_sub(r.varint()?)?);
        } else {
            name.clear();
        }
        name.extend_from_slice(r.until_nul()?);
        if flags & 0xfff != 0xfff && usize::from(flags & 0xfff) != name.len() {
            return None;
        }
        decoded += name.len() as u64;
        if name.len() > MAX_PATH || decoded > MAX_INDEX {
            return None;
        }
        if version != 4
            && r.take((8 - (start - r.0.len()) % 8) % 8)?
                .iter()
                .any(|b| *b != 0)
        {
            return None;
        }
        entries.push(Entry {
            name: name.clone(),
            mode,
        });
    }
    let mut link = None;
    while !r.0.is_empty() {
        let signature = r.take(4)?;
        let size = usize::try_from(r.u32()?).ok()?;
        let data = r.take(size)?;
        match signature {
            b"link" => link = Some(data),
            b"sdir" => {}
            [first, ..] if first.is_ascii_uppercase() => {}
            _ => return None,
        }
    }
    Some(Index {
        entries,
        link,
        checksum,
    })
}

fn merge(base: Vec<Entry>, own: Vec<Entry>, bitmaps: &[u8]) -> Option<Vec<Entry>> {
    let mut r = Reader(bitmaps);
    let (deleted, replaced) = if r.0.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (ewah(&mut r, base.len())?, ewah(&mut r, base.len())?)
    };
    if !r.0.is_empty() {
        return None;
    }
    let mut base: Vec<Option<Entry>> = base.into_iter().map(Some).collect();
    let mut own = own.into_iter();
    for at in deleted {
        base[at].take()?;
    }
    for at in replaced {
        let entry = own.next()?;
        if !entry.name.is_empty() {
            return None;
        }
        base[at].as_mut()?.mode = entry.mode;
    }
    let added: Vec<Entry> = own.collect();
    if added.iter().any(|entry| entry.name.is_empty()) {
        return None;
    }
    Some(base.into_iter().flatten().chain(added).collect())
}

fn ewah(r: &mut Reader<'_>, limit: usize) -> Option<Vec<usize>> {
    r.u32()?;
    let words: Vec<u64> = (0..r.u32()?).map(|_| r.u64()).collect::<Option<_>>()?;
    r.u32()?;
    let mut words = words.into_iter();
    let mut bits = Vec::new();
    let mut at = 0usize;
    while let Some(marker) = words.next() {
        let run = usize::try_from((marker >> 1) & 0xffff_ffff)
            .ok()?
            .checked_mul(64)?;
        let end = at.checked_add(run)?;
        if marker & 1 == 1 {
            if end > limit {
                return None;
            }
            bits.extend(at..end);
        }
        at = end;
        for _ in 0..marker >> 33 {
            let word = words.next()?;
            bits.extend(
                (0..64)
                    .filter(|bit| word >> bit & 1 == 1)
                    .map(|bit| at + bit),
            );
            at = at.checked_add(64)?;
        }
    }
    bits.iter().all(|&bit| bit < limit).then_some(bits)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn until_nul(&mut self) -> Option<&'a [u8]> {
        let end = self.0.iter().position(|byte| *byte == 0)?;
        let name = self.take(end)?;
        self.take(1)?;
        Some(name)
    }

    fn varint(&mut self) -> Option<usize> {
        let mut byte = self.take(1)?[0];
        let mut value = usize::from(byte & 0x7f);
        while byte & 0x80 != 0 {
            byte = self.take(1)?[0];
            value = value.checked_add(1)?.checked_mul(128)? | usize::from(byte & 0x7f);
        }
        Some(value)
    }
}
