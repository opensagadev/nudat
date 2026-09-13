//! Reader and writer for Traveller's Tales Nu engine DAT archives.
//!
//! Versions -3 (MkDat) and -5 (PakDat, including OBB wrappers) are supported.
//! Rewrites retain the original wrapper and copy unchanged stored payloads byte-for-byte.

#![allow(unstable_name_collisions)]

mod dflt;
mod lz2k;

use binrw::{BinRead, BinReaderExt, BinWrite, BinWriterExt};
use flate2::read::DeflateDecoder;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const ALIGN: u64 = 256;

/// Errors returned by DAT operations.
#[derive(Debug, thiserror::Error)]
pub enum NudatError {
    #[error("invalid DAT archive: {0}")]
    InvalidArchive(String),
    #[error("invalid request: {0}")]
    InvalidInput(String),
    #[error("archive entry not found: {0}")]
    MissingEntry(String),
    #[error("failed to decode {path}: {source}")]
    Entry {
        path: String,
        #[source]
        source: Box<NudatError>,
    },
    #[error("binary index error: {0}")]
    Binary(#[from] binrw::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, NudatError>;

fn invalid(message: impl Into<String>) -> NudatError {
    NudatError::InvalidArchive(message.into())
}
fn input(message: impl Into<String>) -> NudatError {
    NudatError::InvalidInput(message.into())
}
fn align(value: u64) -> u64 {
    (value + ALIGN - 1) & !(ALIGN - 1)
}

#[derive(BinRead, BinWrite)]
#[brw(little)]
struct RawFileInfo {
    offset: i32,
    stored_size: i32,
    size: i32,
    compression: i32,
}

#[derive(BinRead, BinWrite)]
#[brw(little)]
struct RawNodeV1 {
    child: i16,
    sibling: i16,
    name_offset: u32,
}

#[derive(BinRead, BinWrite)]
#[brw(little)]
struct RawNodeV2 {
    child: i16,
    sibling: i16,
    name_offset: u32,
    unknown: u16,
    unknown2: u16,
}

/// The payload encoding used by an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    None,
    Lz2k,
    Deflate,
}

impl Compression {
    fn from_raw(raw: i32) -> Result<Self> {
        match raw {
            0 => Ok(Self::None),
            2 => Ok(Self::Lz2k),
            3 => Ok(Self::Deflate),
            _ => Err(invalid(format!("unsupported compression mode {raw}"))),
        }
    }
    fn raw(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Lz2k => 2,
            Self::Deflate => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Pc,
    Android,
    Obb,
}

impl Format {
    fn version(self) -> i32 {
        match self {
            Self::Pc => -3,
            _ => -5,
        }
    }
    fn marker(self) -> &'static [u8] {
        match self {
            Self::Pc => b"MkDat v4.0",
            Self::Android => b"PakDat (TechRound) v1.1",
            Self::Obb => b"PakDat v1.01",
        }
    }
    fn prefix_len(self) -> usize {
        match self {
            Self::Pc => 1024,
            _ => 512,
        }
    }
}

/// One file in the archive. Paths use backslashes, matching the Nu engine.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub offset: u64,
    pub stored_size: u32,
    pub size: u32,
    pub compression: Compression,
}

/// An opened DAT index. Payloads remain on disk until requested.
pub struct Archive {
    path: PathBuf,
    version: i32,
    prefix: Vec<u8>,
    entries: Vec<Entry>,
}

#[derive(Clone)]
enum Source {
    Stored { offset: u64, size: u32 },
    Disk(PathBuf),
}

#[derive(Clone)]
struct Pending {
    path: String,
    source: Source,
    size: u32,
    stored_size: u32,
    compression: Compression,
}

#[derive(Default)]
struct Node {
    name: String,
    children: BTreeMap<String, Node>,
    file: Option<usize>,
}

#[derive(Clone)]
struct FlatNode {
    child: i16,
    sibling: i16,
    name: String,
}

struct EncodedTree {
    nodes: Vec<FlatNode>,
    names: Vec<u8>,
    name_offsets: Vec<u32>,
}

struct ProgressWriter<'a, 'b, W, F> {
    writer: W,
    callback: &'a mut F,
    entry: &'b Entry,
    files_completed: usize,
    total_written: &'a mut u64,
}

impl<W: Write, F: FnMut(&Entry, usize, u64)> Write for ProgressWriter<'_, '_, W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        *self.total_written += written as u64;
        (self.callback)(self.entry, self.files_completed, *self.total_written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct ParallelProgressWriter<'a, W, F> {
    writer: W,
    callback: &'a F,
    entry: &'a Entry,
    files_completed: &'a AtomicUsize,
    total_written: &'a AtomicU64,
    since_report: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PackPhase {
    Encode,
    Write,
}

type PackProgressCallback<'a> = dyn Fn(PackPhase, &str, usize, u64, usize, u64) + Send + Sync + 'a;

struct PackProgressWriter<'a, W> {
    writer: W,
    callback: &'a PackProgressCallback<'a>,
    path: &'a str,
    files_completed: &'a AtomicUsize,
    total_written: &'a AtomicU64,
    total_files: usize,
    total_bytes: u64,
    since_report: u64,
}

impl<W: Write> Write for PackProgressWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        let total = self
            .total_written
            .fetch_add(written as u64, Ordering::Relaxed)
            + written as u64;
        self.since_report += written as u64;
        if self.since_report >= 1024 * 1024 {
            (self.callback)(
                PackPhase::Write,
                self.path,
                self.files_completed.load(Ordering::Relaxed),
                total,
                self.total_files,
                self.total_bytes,
            );
            self.since_report = 0;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write, F: Fn(&Entry, usize, u64) + Sync> Write for ParallelProgressWriter<'_, W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        let total = self
            .total_written
            .fetch_add(written as u64, Ordering::Relaxed)
            + written as u64;
        self.since_report += written as u64;
        if self.since_report >= 1024 * 1024 {
            (self.callback)(
                self.entry,
                self.files_completed.load(Ordering::Relaxed),
                total,
            );
            self.since_report = 0;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        if file_len < 16 {
            return Err(invalid("file is too short to be a DAT archive"));
        }
        let index = file.read_le::<u32>()? as u64;
        if index < 8 || index + 8 > file_len {
            return Err(invalid("DAT index offset is outside the file"));
        }
        file.seek(SeekFrom::Start(index))?;
        let version = file.read_le::<i32>()?;
        if version != -3 && version != -5 {
            return Err(invalid(format!("unsupported DAT version {version}")));
        }
        let count = file.read_le::<i32>()?;
        if count < 0 || count as u64 > (file_len - index) / 16 {
            return Err(invalid("invalid DAT file count"));
        }
        let count = count as usize;
        let mut infos = Vec::with_capacity(count);
        for _ in 0..count {
            let raw = file.read_le::<RawFileInfo>()?;
            let (offset, stored, size) = (raw.offset, raw.stored_size, raw.size);
            let compression = Compression::from_raw(raw.compression)?;
            if offset < 0 || stored < 0 || size < 0 {
                return Err(invalid("negative DAT entry offset or length"));
            }
            let offset = offset as u64 * ALIGN;
            if offset
                .checked_add(stored as u64)
                .is_none_or(|end| end > index)
            {
                return Err(invalid("DAT entry extends into the index"));
            }
            infos.push((offset, stored as u32, size as u32, compression));
        }
        let node_count = file.read_le::<i32>()?;
        if node_count < 1 || node_count > i16::MAX as i32 {
            return Err(invalid("invalid DAT tree node count"));
        }
        let node_count = node_count as usize;
        let node_width = if version == -5 { 12 } else { 8 };
        if file.stream_position()? + (node_count * node_width) as u64 + 4 > file_len {
            return Err(invalid("truncated DAT tree"));
        }
        let mut nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let (child, sibling, name) = if version == -5 {
                let raw = file.read_le::<RawNodeV2>()?;
                (raw.child, raw.sibling, raw.name_offset)
            } else {
                let raw = file.read_le::<RawNodeV1>()?;
                (raw.child, raw.sibling, raw.name_offset)
            };
            nodes.push((child, sibling, name as usize));
        }
        let names_len = file.read_le::<i32>()?;
        if names_len < 0 || file.stream_position()? + names_len as u64 > file_len {
            return Err(invalid("invalid DAT name table length"));
        }
        let mut names = vec![0; names_len as usize];
        file.read_exact(&mut names)?;
        let mut node_names = Vec::with_capacity(node_count);
        for &(_, _, offset) in &nodes {
            let name = names
                .get(offset..)
                .ok_or_else(|| invalid("invalid DAT name offset"))?;
            let end = name
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| invalid("unterminated DAT name"))?;
            let name =
                std::str::from_utf8(&name[..end]).map_err(|_| invalid("non-UTF-8 DAT filename"))?;
            node_names.push(name.to_owned());
        }
        let mut hashes = vec![0u8; count * 4];
        file.read_exact(&mut hashes)?;
        let hash_count = file.read_le::<i32>()?;
        let hash_names_len = file.read_le::<i32>()?;
        if hash_count < 0
            || hash_names_len < 0
            || file.stream_position()? + hash_names_len as u64 > file_len
        {
            return Err(invalid("invalid DAT hash-name table"));
        }
        file.seek(SeekFrom::Current(hash_names_len as i64))?;
        if file.stream_position()? != file_len {
            return Err(invalid("unexpected trailing DAT index data"));
        }
        let mut paths = vec![None; count];
        let mut visited = HashSet::new();
        fn walk(
            idx: i16,
            parent: &str,
            nodes: &[(i16, i16, usize)],
            names: &[String],
            paths: &mut [Option<String>],
            seen: &mut HashSet<i16>,
        ) -> Result<()> {
            let mut at = idx;
            while at != 0 {
                if at < 0 || !seen.insert(at) {
                    return Err(invalid("invalid or cyclic DAT tree"));
                }
                let i = at as usize;
                let &(child, sibling, _) = nodes
                    .get(i)
                    .ok_or_else(|| invalid("DAT tree index out of range"))?;
                let name = &names[i];
                if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
                    return Err(invalid("invalid DAT tree name"));
                }
                let path = if parent.is_empty() {
                    name.clone()
                } else {
                    format!("{parent}\\{name}")
                };
                if child <= 0 {
                    let slot = paths
                        .get_mut((-child) as usize)
                        .ok_or_else(|| invalid("DAT file index out of range"))?;
                    if slot.replace(path).is_some() {
                        return Err(invalid("duplicate DAT file index"));
                    }
                } else {
                    walk(child, &path, nodes, names, paths, seen)?;
                }
                at = sibling;
            }
            Ok(())
        }
        walk(
            nodes[0].0,
            "",
            &nodes,
            &node_names,
            &mut paths,
            &mut visited,
        )?;
        if paths.iter().any(Option::is_none) {
            return Err(invalid("DAT tree does not name every file"));
        }
        // The tree's leaf indexes are not file-info indexes in MkDat -3.
        // Both variants store file-info records in sorted path-hash order.
        let hashes = hashes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        if hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("DAT hashes are unsorted or collide"));
        }
        let mut entries = vec![None; count];
        for path in paths.into_iter().flatten() {
            let position = hashes
                .binary_search(&name_hash(&path))
                .map_err(|_| invalid(format!("DAT hash missing for {path}")))?;
            let (offset, stored_size, size, compression) = infos[position];
            if entries[position]
                .replace(Entry {
                    path,
                    offset,
                    stored_size,
                    size,
                    compression,
                })
                .is_some()
            {
                return Err(invalid("duplicate DAT path hash"));
            }
        }
        if entries.iter().any(Option::is_none) {
            return Err(invalid("DAT hash table does not name every entry"));
        }
        let entries = entries.into_iter().map(Option::unwrap).collect::<Vec<_>>();
        let prefix_len = entries
            .iter()
            .map(|e| e.offset)
            .min()
            .unwrap_or(index)
            .min(index);
        if !(8..=1 << 20).contains(&prefix_len) {
            return Err(invalid("invalid DAT wrapper length"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut prefix = vec![0; prefix_len as usize];
        file.read_exact(&mut prefix)?;
        Ok(Self {
            path,
            version,
            prefix,
            entries,
        })
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn format(&self) -> Option<Format> {
        [Format::Pc, Format::Android, Format::Obb]
            .into_iter()
            .find(|format| {
                self.prefix
                    .windows(format.marker().len())
                    .any(|window| window == format.marker())
            })
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn entry(&self, path: &str) -> Option<&Entry> {
        let key = normalize(path).ok()?;
        self.entries
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case(&key))
    }

    /// Return decoded file bytes. For very large entries, use `copy_to` instead.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.to_owned()))?;
        if entry.size > 512 * 1024 * 1024 {
            return Err(input("entry is too large for read; use copy_to instead"));
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.copy_to(path, &mut bytes)?;
        Ok(bytes)
    }

    /// Stream one decoded file into a writer, returning the number of bytes written.
    pub fn copy_to(&self, path: &str, writer: &mut impl Write) -> Result<u64> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.to_owned()))?;
        self.copy_entry_to(entry, writer)
    }

    fn copy_entry_to(&self, entry: &Entry, writer: &mut impl Write) -> Result<u64> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(entry.offset))?;
        decode_entry_to(&mut file, entry, writer)
    }

    pub fn extract(&self, path: &str, output: impl AsRef<Path>) -> Result<()> {
        if self.entry(path).is_none() {
            return Err(NudatError::MissingEntry(path.to_owned()));
        }
        let mut out = File::create(output)?;
        self.copy_to(path, &mut out)?;
        Ok(())
    }

    pub fn unpack(&self, directory: impl AsRef<Path>) -> Result<()> {
        self.unpack_parallel_with_progress(directory, |_, _, _| {})
    }

    /// Unpack all files and report the current entry, completed-file count,
    /// and cumulative decoded bytes after each write and file completion.
    pub fn unpack_with_progress(
        &self,
        directory: impl AsRef<Path>,
        mut progress: impl FnMut(&Entry, usize, u64),
    ) -> Result<()> {
        let root = directory.as_ref();
        fs::create_dir_all(root)?;
        let mut total_written = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            progress(entry, index, total_written);
            let path = root.join(entry.path.replace('\\', "/"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let output = File::create(path)?;
            let mut writer = ProgressWriter {
                writer: output,
                callback: &mut progress,
                entry,
                files_completed: index,
                total_written: &mut total_written,
            };
            self.copy_entry_to(entry, &mut writer)?;
            drop(writer);
            progress(entry, index + 1, total_written);
        }
        Ok(())
    }

    /// Unpack independent entries concurrently using Rayon's current thread pool.
    /// Progress callbacks may run on any worker thread and can arrive out of order.
    pub fn unpack_parallel_with_progress(
        &self,
        directory: impl AsRef<Path>,
        progress: impl Fn(&Entry, usize, u64) + Send + Sync,
    ) -> Result<()> {
        let root = directory.as_ref();
        fs::create_dir_all(root)?;
        let targets = self
            .entries
            .iter()
            .map(|entry| (entry, root.join(entry.path.replace('\\', "/"))))
            .collect::<Vec<_>>();
        let mut created_dirs = HashSet::new();
        for (_, path) in &targets {
            if let Some(parent) = path.parent() {
                if created_dirs.insert(parent.to_path_buf()) {
                    fs::create_dir_all(parent)?;
                }
            }
        }
        let files_completed = AtomicUsize::new(0);
        let total_written = AtomicU64::new(0);
        targets.par_iter().try_for_each(|(entry, path)| {
            progress(
                entry,
                files_completed.load(Ordering::Relaxed),
                total_written.load(Ordering::Relaxed),
            );
            let output = File::create(path)?;
            let mut writer = ParallelProgressWriter {
                writer: output,
                callback: &progress,
                entry,
                files_completed: &files_completed,
                total_written: &total_written,
                since_report: 0,
            };
            self.copy_entry_to(entry, &mut writer)?;
            let completed = files_completed.fetch_add(1, Ordering::Relaxed) + 1;
            progress(entry, completed, total_written.load(Ordering::Relaxed));
            Ok(())
        })
    }

    /// Save changes to a new archive. Paths are case-insensitive; replacement paths may be new.
    /// An unchanged entry keeps its original compressed bytes and compression mode.
    pub fn rewrite(
        &self,
        output: impl AsRef<Path>,
        replacements: &[(String, PathBuf)],
        removals: &[String],
    ) -> Result<()> {
        if output.as_ref() == self.path
            || (output.as_ref().exists()
                && fs::canonicalize(output.as_ref())? == fs::canonicalize(&self.path)?)
        {
            return Err(input(
                "write to a different output path, then replace the original yourself",
            ));
        }
        let remove = removals
            .iter()
            .map(|s| normalize(s).map(|p| p.to_ascii_uppercase()))
            .collect::<Result<HashSet<_>>>()?;
        let mut pending = BTreeMap::new();
        for entry in &self.entries {
            let key = entry.path.to_ascii_uppercase();
            if !remove.contains(&key) {
                pending.insert(
                    key,
                    Pending {
                        path: entry.path.clone(),
                        source: Source::Stored {
                            offset: entry.offset,
                            size: entry.stored_size,
                        },
                        size: entry.size,
                        stored_size: entry.stored_size,
                        compression: entry.compression,
                    },
                );
            }
        }
        for (name, disk) in replacements {
            let requested_path = normalize(name)?;
            let key = requested_path.to_ascii_uppercase();
            let path = self
                .entry(&requested_path)
                .map_or(requested_path, |entry| entry.path.clone());
            let size = fs::metadata(disk)?.len();
            if size > i32::MAX as u64 {
                return Err(input("replacement file exceeds DAT entry size limit"));
            }
            pending.insert(
                key,
                Pending {
                    path,
                    source: Source::Disk(disk.clone()),
                    size: size as u32,
                    stored_size: size as u32,
                    compression: Compression::None,
                },
            );
        }
        write_archive(
            output.as_ref(),
            self.version,
            &self.prefix,
            pending.into_values().collect(),
            Some(&self.path),
            None,
        )
    }

    pub fn verify(&self) -> Result<()> {
        let mut file = File::open(&self.path)?;
        for entry in &self.entries {
            file.seek(SeekFrom::Start(entry.offset))?;
            decode_entry_to(&mut file, entry, &mut io::sink()).map_err(|source| {
                NudatError::Entry {
                    path: entry.path.clone(),
                    source: Box::new(source),
                }
            })?;
        }
        Ok(())
    }
}

fn collect_directory(root: &Path, dir: &Path, pending: &mut Vec<Pending>) -> Result<()> {
    let mut children = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let kind = child.file_type()?;
        if kind.is_symlink() {
            return Err(input("symlinks are not supported when packing"));
        }
        if kind.is_dir() {
            collect_directory(root, &child.path(), pending)?;
        } else if kind.is_file() {
            let relative = child
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('/', "\\");
            let path = normalize(&relative)?;
            let size = child.metadata()?.len();
            if size > i32::MAX as u64 {
                return Err(input("file exceeds DAT entry size limit"));
            }
            pending.push(Pending {
                path,
                source: Source::Disk(child.path()),
                size: size as u32,
                stored_size: size as u32,
                compression: Compression::None,
            });
        }
    }
    Ok(())
}

pub fn pack(directory: impl AsRef<Path>, output: impl AsRef<Path>, format: Format) -> Result<()> {
    pack_with_progress(directory, output, format, |_, _, _, _, _, _| {})
}

/// Pack a directory in parallel. Android DAT and OBB entries use game-compatible
/// fixed-Huffman DFLT blocks when that reduces their stored size.
/// Progress reports the phase, current path, completed files, cumulative bytes,
/// total files, and total bytes. Callbacks may arrive out of order within a phase.
pub fn pack_with_progress(
    directory: impl AsRef<Path>,
    output: impl AsRef<Path>,
    format: Format,
    progress: impl Fn(PackPhase, &str, usize, u64, usize, u64) + Send + Sync,
) -> Result<()> {
    let root = directory.as_ref();
    if !root.is_dir() {
        return Err(input("input is not a directory"));
    }
    let mut pending = Vec::new();
    collect_directory(root, root, &mut pending)?;
    let mut paths = HashSet::new();
    for item in &pending {
        if !paths.insert(item.path.to_ascii_uppercase()) {
            return Err(input(format!(
                "duplicate case-insensitive input path: {}",
                item.path
            )));
        }
    }
    if let Ok(output_path) = fs::canonicalize(output.as_ref()) {
        for item in &pending {
            if let Source::Disk(source) = &item.source {
                if fs::canonicalize(source)? == output_path {
                    return Err(input(format!(
                        "output archive would overwrite input file: {}",
                        source.display()
                    )));
                }
            }
        }
    }
    let total_files = pending.len();
    let total_bytes = pending.iter().map(|item| item.size as u64).sum();
    let files_completed = AtomicUsize::new(0);
    let bytes_encoded = AtomicU64::new(0);
    let output_parent = output
        .as_ref()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = if format == Format::Pc {
        None
    } else {
        Some(
            tempfile::Builder::new()
                .prefix(".nudat-")
                .tempdir_in(output_parent)?,
        )
    };
    pending
        .par_iter_mut()
        .enumerate()
        .try_for_each(|(index, item)| -> Result<()> {
            progress(
                PackPhase::Encode,
                &item.path,
                files_completed.load(Ordering::Relaxed),
                bytes_encoded.load(Ordering::Relaxed),
                total_files,
                total_bytes,
            );
            if let (Some(staging), Source::Disk(path)) = (&staging, &item.source) {
                if item.size != 0 && dflt::should_compress(&item.path) {
                    let mut source = File::open(path)?;
                    if source.metadata()?.len() != item.size as u64 {
                        return Err(input(format!(
                            "input file changed while packing: {}",
                            path.display()
                        )));
                    }
                    let staged_path = staging.path().join(index.to_string());
                    let mut staged = File::create(&staged_path)?;
                    let mut buffer = [0u8; dflt::BLOCK_SIZE];
                    let mut left = item.size as usize;
                    let mut stored_size = 0u64;
                    let mut since_report = 0usize;
                    while left != 0 {
                        let length = left.min(buffer.len());
                        source.read_exact(&mut buffer[..length])?;
                        let block = dflt::encode_block(&buffer[..length]);
                        staged.write_all(b"DFLT")?;
                        staged.write_all(&(block.len() as u32).to_le_bytes())?;
                        staged.write_all(&(length as u32).to_le_bytes())?;
                        staged.write_all(&block)?;
                        stored_size += 12 + block.len() as u64;
                        left -= length;
                        bytes_encoded.fetch_add(length as u64, Ordering::Relaxed);
                        since_report += length;
                        if since_report >= 1024 * 1024 {
                            progress(
                                PackPhase::Encode,
                                &item.path,
                                files_completed.load(Ordering::Relaxed),
                                bytes_encoded.load(Ordering::Relaxed),
                                total_files,
                                total_bytes,
                            );
                            since_report = 0;
                        }
                    }
                    if source.metadata()?.len() != item.size as u64 {
                        return Err(input(format!(
                            "input file changed while packing: {}",
                            path.display()
                        )));
                    }
                    if stored_size < item.size as u64 {
                        staged.flush()?;
                        item.source = Source::Disk(staged_path);
                        item.stored_size = stored_size as u32;
                        item.compression = Compression::Deflate;
                    }
                } else {
                    bytes_encoded.fetch_add(item.size as u64, Ordering::Relaxed);
                }
            } else {
                bytes_encoded.fetch_add(item.size as u64, Ordering::Relaxed);
            }
            let completed = files_completed.fetch_add(1, Ordering::Relaxed) + 1;
            progress(
                PackPhase::Encode,
                &item.path,
                completed,
                bytes_encoded.load(Ordering::Relaxed),
                total_files,
                total_bytes,
            );
            Ok(())
        })?;
    let mut prefix = vec![0; format.prefix_len()];
    let marker = [
        b"BEGIN_APP_ID_STRING".as_slice(),
        format.marker(),
        b"END_APP_ID_STRING\0",
    ]
    .concat();
    prefix[8..8 + marker.len()].copy_from_slice(&marker);
    write_archive(
        output.as_ref(),
        format.version(),
        &prefix,
        pending,
        None,
        Some(&progress),
    )
}

fn normalize(path: &str) -> Result<String> {
    let path = path.replace('/', "\\");
    if path.is_empty() || path.starts_with('\\') || path.contains(':') || path.contains('\0') {
        return Err(input("invalid archive path"));
    }
    if path
        .split('\\')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(input("invalid archive path component"));
    }
    Ok(path)
}

fn name_hash(path: &str) -> u32 {
    path.bytes().fold(0x811c9dc5u32, |hash, b| {
        (hash ^ u32::from(b.to_ascii_uppercase())).wrapping_mul(0x199933)
    })
}

fn decode_entry_to(file: &mut File, entry: &Entry, output: &mut impl Write) -> Result<u64> {
    let mut written = 0u64;
    match entry.compression {
        Compression::None => {
            if entry.stored_size != entry.size {
                return Err(invalid("uncompressed size mismatch"));
            }
            written = io::copy(&mut file.take(entry.stored_size as u64), output)?;
        }
        Compression::Lz2k | Compression::Deflate => {
            let mut remaining = entry.stored_size as u64;
            while remaining > 0 {
                if remaining < 12 {
                    return Err(invalid("truncated compressed block header"));
                }
                let mut header = [0; 12];
                file.read_exact(&mut header)?;
                remaining -= 12;
                let first = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
                let second = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
                let (decoded, compressed) = if entry.compression == Compression::Lz2k {
                    if &header[..4] != b"LZ2K" {
                        return Err(invalid("invalid LZ2K block magic"));
                    }
                    (first, second)
                } else {
                    if &header[..4] != b"DFLT" {
                        return Err(invalid("invalid DFLT block magic"));
                    }
                    // DFLT stores compressed size before decoded size.
                    (second, first)
                };
                if compressed as u64 > remaining {
                    return Err(invalid("compressed block exceeds entry"));
                }
                if compressed == 0 && decoded != 0 {
                    return Err(invalid("empty compressed block"));
                }
                if written + decoded as u64 > entry.size as u64 {
                    return Err(invalid("decoded entry exceeds declared size"));
                }
                let mut bytes = vec![0; compressed];
                file.read_exact(&mut bytes)?;
                remaining -= compressed as u64;
                let block = if compressed == decoded {
                    bytes
                } else if entry.compression == Compression::Lz2k {
                    lz2k::decode(&bytes, decoded).map_err(|e| invalid(e.to_string()))?
                } else {
                    // Nu's DFLT stream swaps the DEFLATE dynamic and stored
                    // block type tags. Game files use one final dynamic block.
                    if bytes[0] & 0b110 == 0 {
                        bytes[0] |= 0b100;
                    }
                    let decoder = DeflateDecoder::new(bytes.as_slice());
                    let mut out = Vec::with_capacity(decoded);
                    decoder.take(decoded as u64 + 1).read_to_end(&mut out)?;
                    out
                };
                if block.len() != decoded {
                    return Err(invalid("compressed block decoded size mismatch"));
                }
                output.write_all(&block)?;
                written += block.len() as u64;
            }
        }
    }
    if written != entry.size as u64 {
        return Err(invalid("decoded entry size mismatch"));
    }
    Ok(written)
}

fn write_archive(
    output: &Path,
    version: i32,
    prefix: &[u8],
    mut pending: Vec<Pending>,
    original: Option<&Path>,
    progress: Option<&PackProgressCallback<'_>>,
) -> Result<()> {
    if pending.len() > i16::MAX as usize {
        return Err(input("too many DAT files"));
    }
    pending.sort_by(|a, b| {
        name_hash(&a.path)
            .cmp(&name_hash(&b.path))
            .then(a.path.cmp(&b.path))
    });
    let mut keys = HashSet::new();
    for item in &pending {
        normalize(&item.path)?;
        if !keys.insert(item.path.to_ascii_uppercase()) {
            return Err(input(format!("duplicate archive path: {}", item.path)));
        }
    }
    for pair in pending.windows(2) {
        if name_hash(&pair[0].path) == name_hash(&pair[1].path) {
            return Err(input(format!(
                "DAT path hash collision: {} and {}",
                pair[0].path, pair[1].path
            )));
        }
    }
    let tree = build_tree(&pending)?;
    let mut planned_index = prefix.len() as u64;
    for item in &pending {
        planned_index = align(planned_index) + item.stored_size as u64;
    }
    if planned_index > i32::MAX as u64 {
        return Err(input("DAT index exceeds the game's 2 GiB loader limit"));
    }
    if let Ok(output_path) = fs::canonicalize(output) {
        for item in &pending {
            if let Source::Disk(source) = &item.source {
                if fs::canonicalize(source)? == output_path {
                    return Err(input(format!(
                        "output archive would overwrite input file: {}",
                        source.display()
                    )));
                }
            }
        }
    }
    let mut out = File::create(output)?;
    out.write_all(prefix)?;
    let mut infos = Vec::with_capacity(pending.len());
    if let Some(progress) = progress {
        let total_files = pending.len();
        let total_bytes = pending.iter().map(|item| item.stored_size as u64).sum();
        let files_completed = AtomicUsize::new(0);
        let total_written = AtomicU64::new(0);
        let mut position = out.stream_position()?;
        let mut offsets = Vec::with_capacity(pending.len());
        for item in &pending {
            position = align(position);
            let offset = position / ALIGN;
            if offset > i32::MAX as u64 {
                return Err(input("DAT offset exceeds format limit"));
            }
            offsets.push(position);
            infos.push((
                offset as i32,
                item.stored_size as i32,
                item.size as i32,
                item.compression.raw(),
            ));
            position += item.stored_size as u64;
        }
        if position > i32::MAX as u64 {
            return Err(input("DAT index exceeds the game's 2 GiB loader limit"));
        }
        out.set_len(position)?;
        pending.par_iter().zip(offsets.par_iter()).try_for_each(
            |(item, &offset)| -> Result<()> {
                progress(
                    PackPhase::Write,
                    &item.path,
                    files_completed.load(Ordering::Relaxed),
                    total_written.load(Ordering::Relaxed),
                    total_files,
                    total_bytes,
                );
                let source = match &item.source {
                    Source::Disk(path) => {
                        let source = File::open(path)?;
                        if source.metadata()?.len() != item.stored_size as u64 {
                            return Err(input(format!(
                                "input file changed while packing: {}",
                                path.display()
                            )));
                        }
                        source
                    }
                    Source::Stored { offset, size } => {
                        if *size != item.stored_size {
                            return Err(input("stored entry size mismatch"));
                        }
                        let original = original.ok_or_else(|| input("missing source archive"))?;
                        let mut source = File::open(original)?;
                        source.seek(SeekFrom::Start(*offset))?;
                        source
                    }
                };
                let mut target = File::options().write(true).open(output)?;
                target.seek(SeekFrom::Start(offset))?;
                let mut writer = PackProgressWriter {
                    writer: target,
                    callback: progress,
                    path: &item.path,
                    files_completed: &files_completed,
                    total_written: &total_written,
                    total_files,
                    total_bytes,
                    since_report: 0,
                };
                let copied = io::copy(&mut source.take(item.stored_size as u64), &mut writer)?;
                if copied != item.stored_size as u64 {
                    return Err(input(format!(
                        "source file changed while packing: {}",
                        item.path
                    )));
                }
                let completed = files_completed.fetch_add(1, Ordering::Relaxed) + 1;
                progress(
                    PackPhase::Write,
                    &item.path,
                    completed,
                    total_written.load(Ordering::Relaxed),
                    total_files,
                    total_bytes,
                );
                Ok(())
            },
        )?;
        out.seek(SeekFrom::Start(position))?;
    } else {
        let mut source_file = original.map(File::open).transpose()?;
        for item in &pending {
            let position = out.stream_position()?;
            let aligned = align(position);
            out.write_all(&vec![0; (aligned - position) as usize])?;
            let offset = aligned / ALIGN;
            if offset > i32::MAX as u64 {
                return Err(input("DAT offset exceeds format limit"));
            }
            let copied = match &item.source {
                Source::Stored { offset, size } => {
                    let file = source_file
                        .as_mut()
                        .ok_or_else(|| input("missing source archive"))?;
                    file.seek(SeekFrom::Start(*offset))?;
                    io::copy(&mut file.take(*size as u64), &mut out)?
                }
                Source::Disk(path) => io::copy(&mut File::open(path)?, &mut out)?,
            };
            if copied > i32::MAX as u64 {
                return Err(input("DAT entry exceeds format limit"));
            }
            infos.push((
                offset as i32,
                copied as i32,
                item.size as i32,
                item.compression.raw(),
            ));
        }
    }
    let index = out.stream_position()?;
    if index > i32::MAX as u64 {
        return Err(input("DAT index exceeds the game's 2 GiB loader limit"));
    }
    out.write_le(&version)?;
    out.write_le(&(pending.len() as i32))?;
    for (offset, stored, size, mode) in infos {
        out.write_le(&RawFileInfo {
            offset,
            stored_size: stored,
            size,
            compression: mode,
        })?;
    }
    out.write_le(&(tree.nodes.len() as i32))?;
    for (node, name_offset) in tree.nodes.iter().zip(tree.name_offsets.iter()) {
        if version == -5 {
            out.write_le(&RawNodeV2 {
                child: node.child,
                sibling: node.sibling,
                name_offset: *name_offset,
                unknown: 0,
                unknown2: 0,
            })?;
        } else {
            out.write_le(&RawNodeV1 {
                child: node.child,
                sibling: node.sibling,
                name_offset: *name_offset,
            })?;
        }
    }
    out.write_le(&(tree.names.len() as i32))?;
    out.write_all(&tree.names)?;
    for item in &pending {
        out.write_le(&name_hash(&item.path))?;
    }
    out.write_le(&0i32)?;
    out.write_le(&0i32)?;
    let end = out.stream_position()?;
    if end - index > u32::MAX as u64 {
        return Err(input("DAT index exceeds 32-bit length limit"));
    }
    out.seek(SeekFrom::Start(0))?;
    out.write_le(&(index as u32))?;
    out.write_le(&((end - index) as u32))?;
    Ok(out.flush()?)
}

fn build_tree(pending: &[Pending]) -> Result<EncodedTree> {
    let mut root = Node::default();
    for (i, item) in pending.iter().enumerate() {
        let parts = item.path.split('\\').collect::<Vec<_>>();
        let mut at = &mut root;
        for (j, part) in parts.iter().enumerate() {
            at = at
                .children
                .entry(part.to_ascii_uppercase())
                .or_insert_with(|| Node {
                    name: (*part).to_owned(),
                    ..Default::default()
                });
            if j == parts.len() - 1 {
                if at.file.replace(i).is_some() || !at.children.is_empty() {
                    return Err(input("file/directory path conflict"));
                }
            } else if at.file.is_some() {
                return Err(input("file/directory path conflict"));
            }
        }
    }
    let mut flat = vec![FlatNode {
        child: 0,
        sibling: 0,
        name: String::new(),
    }];
    fn add_children(node: &Node, flat: &mut Vec<FlatNode>) -> Result<i16> {
        let mut first = 0;
        let mut previous = 0;
        for child in node.children.values() {
            let idx = i16::try_from(flat.len()).map_err(|_| input("too many DAT tree nodes"))?;
            flat.push(FlatNode {
                child: 0,
                sibling: 0,
                name: child.name.clone(),
            });
            if first == 0 {
                first = idx;
            }
            if previous != 0 {
                flat[previous as usize].sibling = idx;
            }
            previous = idx;
            flat[idx as usize].child = if let Some(file) = child.file {
                -(i16::try_from(file).map_err(|_| input("too many DAT files"))?)
            } else {
                add_children(child, flat)?
            };
        }
        Ok(first)
    }
    flat[0].child = add_children(&root, &mut flat)?;
    let mut bytes = vec![0];
    let mut offsets = Vec::with_capacity(flat.len());
    for (i, node) in flat.iter().enumerate() {
        if i == 0 {
            offsets.push(0);
            continue;
        }
        offsets.push(bytes.len() as u32);
        bytes.extend_from_slice(node.name.as_bytes());
        bytes.push(0);
    }
    Ok(EncodedTree {
        nodes: flat,
        names: bytes,
        name_offsets: offsets,
    })
}
