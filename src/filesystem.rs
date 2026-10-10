//! Filesystem adapters for the nudat CLI. Not part of the library.
use nudat::{
    encode_payload, normalize_path as normalize, path_hash as name_hash, should_compress,
    Archive as ArchiveIndex, Compression, Entry, Format, NudatError, Result,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
const ALIGN: u64 = 256;
fn align(value: u64) -> u64 {
    (value + ALIGN - 1) & !(ALIGN - 1)
}
fn input(message: impl Into<String>) -> NudatError {
    NudatError::InvalidInput(message.into())
}

pub struct Archive {
    path: PathBuf,
    index: ArchiveIndex,
}
impl std::ops::Deref for Archive {
    type Target = ArchiveIndex;
    fn deref(&self) -> &Self::Target {
        &self.index
    }
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

#[cfg(test)]
struct ProgressWriter<'a, 'b, W, F> {
    writer: W,
    callback: &'a mut F,
    entry: &'b Entry,
    files_completed: usize,
    total_written: &'a mut u64,
}

#[cfg(test)]
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
        let index = ArchiveIndex::new(&mut File::open(&path)?)?;
        Ok(Self { path, index })
    }
    #[cfg(test)]
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.index.read(&mut File::open(&self.path)?, path)
    }
    pub fn copy_to(&self, path: &str, writer: &mut impl Write) -> Result<u64> {
        self.index
            .copy_to(&mut File::open(&self.path)?, path, writer)
    }
    fn copy_entry_to(&self, entry: &Entry, writer: &mut impl Write) -> Result<u64> {
        self.copy_to(&entry.path, writer)
    }
    pub fn verify(&self) -> Result<()> {
        self.index.verify(&mut File::open(&self.path)?)
    }
    pub fn extract(&self, path: &str, output: impl AsRef<Path>) -> Result<()> {
        if self.entry(path).is_none() {
            return Err(NudatError::MissingEntry(path.to_owned()));
        }
        let mut out = File::create(output)?;
        self.copy_to(path, &mut out)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn unpack(&self, directory: impl AsRef<Path>) -> Result<()> {
        self.unpack_parallel_with_progress(directory, |_, _, _| {})
    }

    /// Unpack all files and report the current entry, completed-file count,
    /// and cumulative decoded bytes after each write and file completion.
    #[cfg(test)]
    pub fn unpack_with_progress(
        &self,
        directory: impl AsRef<Path>,
        mut progress: impl FnMut(&Entry, usize, u64),
    ) -> Result<()> {
        let root = directory.as_ref();
        fs::create_dir_all(root)?;
        let mut total_written = 0;
        for (index, entry) in self.entries().iter().enumerate() {
            progress(entry, index, total_written);
            let path = root.join(&entry.path);
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
            .entries()
            .iter()
            .map(|entry| (entry, root.join(&entry.path)))
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
        for entry in self.entries() {
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
            self.version(),
            self.prefix(),
            pending.into_values().collect(),
            Some(&self.path),
            None,
        )
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
            let child_path = child.path();
            let relative = child_path.strip_prefix(root).unwrap().to_string_lossy();
            let path = normalize(&relative)?;
            let size = child.metadata()?.len();
            if size > i32::MAX as u64 {
                return Err(input("file exceeds DAT entry size limit"));
            }
            pending.push(Pending {
                path,
                source: Source::Disk(child_path),
                size: size as u32,
                stored_size: size as u32,
                compression: Compression::None,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub fn pack(directory: impl AsRef<Path>, output: impl AsRef<Path>, format: Format) -> Result<()> {
    pack_with_progress(directory, output, format, |_, _, _, _, _, _| {})
}

/// Pack a directory in parallel. PC entries use LZ2K and Android DAT/OBB
/// entries use game-compatible fixed-Huffman DFLT when compression helps.
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
    let staging = tempfile::Builder::new()
        .prefix(".nudat-")
        .tempdir_in(output_parent)?;
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
            if let Source::Disk(path) = &item.source {
                let compress = should_compress(&item.path, format);
                if item.size != 0 && compress {
                    let mut source = File::open(path)?;
                    if source.metadata()?.len() != item.size as u64 {
                        return Err(input(format!(
                            "input file changed while packing: {}",
                            path.display()
                        )));
                    }
                    let staged_path = staging.path().join(index.to_string());
                    // Buffer each header and payload together to avoid four
                    // separate writes per 16 KiB chunk.
                    let mut staged = io::BufWriter::new(File::create(&staged_path)?);
                    let mut reported = 0;
                    let stored_size =
                        encode_payload(&mut source, &mut staged, item.size, format, |bytes| {
                            bytes_encoded.fetch_add(bytes - reported, Ordering::Relaxed);
                            reported = bytes;
                            progress(
                                PackPhase::Encode,
                                &item.path,
                                files_completed.load(Ordering::Relaxed),
                                bytes_encoded.load(Ordering::Relaxed),
                                total_files,
                                total_bytes,
                            );
                        })? as u64;
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
                        item.compression = if matches!(format, Format::Pc | Format::PcLegacy) {
                            Compression::Lz2k
                        } else {
                            Compression::Deflate
                        };
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
    let index_limit = if version == -2 {
        i32::MAX as u64 * ALIGN
    } else {
        i32::MAX as u64
    };
    let check_index = |offset| -> Result<()> {
        if offset > index_limit {
            return Err(input(if version == -2 {
                "DAT index exceeds legacy sector offset limit"
            } else {
                "DAT index exceeds the game's 2 GiB loader limit"
            }));
        }
        Ok(())
    };
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

    let mut planned_index = prefix.len() as u64;
    for item in &pending {
        planned_index = align(planned_index) + item.stored_size as u64;
    }
    check_index(if version == -2 {
        align(planned_index)
    } else {
        planned_index
    })?;
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
                item.compression,
            ));
            position += item.stored_size as u64;
        }
        check_index(if version == -2 {
            align(position)
        } else {
            position
        })?;
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
                item.compression,
            ));
        }
    }
    let entries = pending
        .iter()
        .zip(infos)
        .map(|(item, (offset, stored, size, compression))| Entry {
            path: item.path.clone(),
            offset: offset as u64 * ALIGN,
            stored_size: stored as u32,
            size: size as u32,
            compression,
        })
        .collect::<Vec<_>>();
    nudat::write_index(&mut out, version, &entries)
}
