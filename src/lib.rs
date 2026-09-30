//! Portable archive indexing and decoding, with optional native filesystem operations.
//!
//! Use [`ArchiveIndex`] with any [`std::io::Read`] + [`std::io::Seek`] source.
//! The default `native` feature also provides `Archive`, packing, and rewriting.

#![allow(unstable_name_collisions)]

#[cfg(feature = "native")]
mod dflt;
mod lz2k;
#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use native::{pack, pack_with_progress, Archive, PackPhase};

use binrw::{BinRead, BinReaderExt, BinWrite};
use flate2::read::DeflateDecoder;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom, Write};

const ALIGN: u64 = 256;
#[cfg(feature = "native")]
const PACK_BLOCK_SIZE: usize = 16 * 1024;

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
#[cfg(feature = "native")]
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
    #[cfg(feature = "native")]
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
    PcLegacy,
    Android,
    Obb,
}

impl Format {
    #[cfg(feature = "native")]
    fn version(self) -> i32 {
        match self {
            Self::Pc => -3,
            Self::PcLegacy => -2,
            Self::Android | Self::Obb => -5,
        }
    }
    fn marker(self) -> &'static [u8] {
        match self {
            Self::Pc => b"MkDat v4.0",
            Self::PcLegacy => b"MkDat V3.26",
            Self::Android => b"PakDat (TechRound) v1.1",
            Self::Obb => b"PakDat v1.01",
        }
    }
    #[cfg(feature = "native")]
    fn prefix_len(self) -> usize {
        match self {
            Self::Pc => 1024,
            Self::PcLegacy => 2048,
            Self::Android | Self::Obb => 512,
        }
    }
}

/// One file in the archive. Paths use forward slashes, preserving original case.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub offset: u64,
    pub stored_size: u32,
    pub size: u32,
    pub compression: Compression,
}

/// Parsed archive metadata, independent of filesystem paths and payload storage.
///
/// Parsing reads the index and wrapper, leaving payloads in the supplied source.
/// Supply the same archive data to `read`, `copy_to`, and `verify` afterwards.
pub struct ArchiveIndex {
    version: i32,
    prefix: Vec<u8>,
    entries: Vec<Entry>,
    lookup: HashMap<String, usize>,
}

impl ArchiveIndex {
    /// Parse a complete archive from a seekable source, starting at offset zero.
    /// The source's position after parsing is unspecified.
    pub fn from_reader(file: &mut (impl Read + Seek)) -> Result<Self> {
        let file_len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        if file_len < 16 {
            return Err(invalid("file is too short to be a DAT archive"));
        }
        let raw_index = file.read_le::<i32>()?;
        let index = if raw_index < 0 {
            (-i64::from(raw_index) as u64) * ALIGN
        } else {
            raw_index as u64
        };
        if index < 8 || index + 8 > file_len {
            return Err(invalid("DAT index offset is outside the file"));
        }
        file.seek(SeekFrom::Start(index))?;
        let version = file.read_le::<i32>()?;
        if version != -2 && version != -3 && version != -5 {
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
                    format!("{parent}/{name}")
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
        // MkDat -2 leaf indexes identify file records directly. Later variants
        // store file records in sorted path-hash order instead.
        let hashes = hashes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        if version != -2 && hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("DAT hashes are unsorted or collide"));
        }
        let mut entries = vec![None; count];
        for (leaf, path) in paths.into_iter().enumerate() {
            let path = path.unwrap();
            let position = if version == -2 {
                if hashes[leaf] != name_hash(&path) {
                    return Err(invalid(format!("DAT hash mismatch for {path}")));
                }
                leaf
            } else {
                hashes
                    .binary_search(&name_hash(&path))
                    .map_err(|_| invalid(format!("DAT hash missing for {path}")))?
            };
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
        let mut lookup = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            lookup
                .entry(entry.path.to_ascii_uppercase())
                .or_insert(index);
        }
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
            version,
            prefix,
            entries,
            lookup,
        })
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn format(&self) -> Option<Format> {
        [Format::Pc, Format::PcLegacy, Format::Android, Format::Obb]
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
        let key = normalize(path).ok()?.to_ascii_uppercase();
        self.lookup.get(&key).map(|&index| &self.entries[index])
    }

    /// Decode one entry into memory. Use `copy_to` for entries larger than 512 MiB.
    pub fn read(&self, source: &mut (impl Read + Seek), path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.to_owned()))?;
        if entry.size > 512 * 1024 * 1024 {
            return Err(input("entry is too large for read; use copy_to instead"));
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.copy_to(source, path, &mut bytes)?;
        Ok(bytes)
    }

    /// Seek to an entry and stream its decoded bytes into a writer.
    pub fn copy_to(
        &self,
        source: &mut (impl Read + Seek),
        path: &str,
        writer: &mut impl Write,
    ) -> Result<u64> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.to_owned()))?;
        source.seek(SeekFrom::Start(entry.offset))?;
        decode_entry_to(source, entry, writer)
    }

    /// Decode every entry, discarding its output and reporting the first failure.
    pub fn verify(&self, source: &mut (impl Read + Seek)) -> Result<()> {
        for entry in &self.entries {
            source.seek(SeekFrom::Start(entry.offset))?;
            decode_entry_to(source, entry, &mut io::sink()).map_err(|source| {
                NudatError::Entry {
                    path: entry.path.clone(),
                    source: Box::new(source),
                }
            })?;
        }
        Ok(())
    }
}

fn normalize(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    if path.is_empty() || path.starts_with('/') || path.contains(':') || path.contains('\0') {
        return Err(input("invalid archive path"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(input("invalid archive path component"));
    }
    Ok(path)
}

fn name_hash(path: &str) -> u32 {
    path.bytes().fold(0x811c9dc5u32, |hash, b| {
        let b = if b == b'/' { b'\\' } else { b };
        (hash ^ u32::from(b.to_ascii_uppercase())).wrapping_mul(0x199933)
    })
}

/// An independently decodable range, suitable for parallel scheduling.
#[derive(Clone, Debug)]
pub struct DecodeChunk {
    pub offset: u64,
    pub stored_size: u32,
    pub size: u32,
    pub output_offset: u64,
    pub compression: Compression,
}

impl DecodeChunk {
    /// Decode this range into a writer. Position its output at `output_offset`.
    pub fn copy_to(&self, source: &mut (impl Read + Seek), output: &mut impl Write) -> Result<u64> {
        source.seek(SeekFrom::Start(self.offset))?;
        decode_entry_to(
            source,
            &Entry {
                path: String::new(),
                offset: self.offset,
                stored_size: self.stored_size,
                size: self.size,
                compression: self.compression,
            },
            output,
        )
    }
}

impl ArchiveIndex {
    /// Scan headers without decoding payloads. Each returned range is at most
    /// `limit` decoded bytes; a compressed block exceeding that limit is rejected.
    /// LZ2K/DFLT blocks are independent; raw entries may split at any byte.
    pub fn chunks(
        &self,
        source: &mut (impl Read + Seek),
        path: &str,
        limit: u32,
    ) -> Result<Vec<DecodeChunk>> {
        if limit == 0 {
            return Err(input("chunk limit must be positive"));
        }
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.to_owned()))?;
        let mut chunks: Vec<DecodeChunk> = Vec::new();
        let mut offset = entry.offset;
        let end = offset + u64::from(entry.stored_size);
        let mut output_offset = 0u64;
        if entry.compression == Compression::None && entry.size != entry.stored_size {
            return Err(invalid("uncompressed size mismatch"));
        }
        while offset < end {
            let (size, stored_size) = if entry.compression == Compression::None {
                let size = (end - offset).min(u64::from(limit)) as u32;
                (size, size)
            } else {
                if end - offset < 12 {
                    return Err(invalid("truncated compressed block header"));
                }
                source.seek(SeekFrom::Start(offset))?;
                let mut header = [0; 12];
                source.read_exact(&mut header)?;
                let (size, compressed) = block_sizes(&header, entry.compression)?;
                if u64::from(compressed) > end - offset - 12 {
                    return Err(invalid("compressed block exceeds entry"));
                }
                if compressed == 0 && size != 0 {
                    return Err(invalid("empty compressed block"));
                }
                (size, compressed + 12)
            };
            if size > limit {
                return Err(input("compressed block exceeds chunk limit"));
            }
            if u64::from(stored_size) > u64::from(limit) + 12 {
                return Err(input("compressed input exceeds chunk limit"));
            }
            if output_offset + u64::from(size) > u64::from(entry.size) {
                return Err(invalid("decoded entry exceeds declared size"));
            }
            // Bound compressed input as well as decoded output. Headers of empty
            // blocks count too, so corrupt files cannot create an unbounded job.
            let merge = chunks.last_mut().filter(|chunk| {
                u64::from(chunk.size) + u64::from(size) <= u64::from(limit)
                    && u64::from(chunk.stored_size) + u64::from(stored_size) <= u64::from(limit)
            });
            if let Some(chunk) = merge {
                chunk.size += size;
                chunk.stored_size += stored_size;
            } else {
                chunks.push(DecodeChunk {
                    offset,
                    stored_size,
                    size,
                    output_offset,
                    compression: entry.compression,
                });
            }
            offset += u64::from(stored_size);
            output_offset += u64::from(size);
        }
        if output_offset != u64::from(entry.size) {
            return Err(invalid("decoded entry size mismatch"));
        }
        if chunks.is_empty() {
            chunks.push(DecodeChunk {
                offset,
                stored_size: 0,
                size: 0,
                output_offset: 0,
                compression: entry.compression,
            });
        }
        Ok(chunks)
    }
}

fn block_sizes(header: &[u8; 12], compression: Compression) -> Result<(u32, u32)> {
    let first = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let second = u32::from_le_bytes(header[8..12].try_into().unwrap());
    match compression {
        Compression::Lz2k if &header[..4] == b"LZ2K" => Ok((first, second)),
        Compression::Deflate if &header[..4] == b"DFLT" => Ok((second, first)),
        _ => Err(invalid("invalid compressed block magic")),
    }
}

fn decode_entry_to(file: &mut impl Read, entry: &Entry, output: &mut impl Write) -> Result<u64> {
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
            let mut bytes = Vec::new();
            let mut decoded_buffer = Vec::new();
            while remaining > 0 {
                if remaining < 12 {
                    return Err(invalid("truncated compressed block header"));
                }
                let mut header = [0; 12];
                file.read_exact(&mut header)?;
                remaining -= 12;
                let (decoded, compressed) = block_sizes(&header, entry.compression)?;
                let (decoded, compressed) = (decoded as usize, compressed as usize);
                if compressed as u64 > remaining {
                    return Err(invalid("compressed block exceeds entry"));
                }
                if compressed == 0 && decoded != 0 {
                    return Err(invalid("empty compressed block"));
                }
                if written + decoded as u64 > entry.size as u64 {
                    return Err(invalid("decoded entry exceeds declared size"));
                }
                bytes.resize(compressed, 0);
                file.read_exact(&mut bytes)?;
                remaining -= compressed as u64;
                let block = if compressed == decoded {
                    bytes.as_slice()
                } else if entry.compression == Compression::Lz2k {
                    lz2k::decode(&bytes, decoded, &mut decoded_buffer)
                        .map_err(|e| invalid(e.to_string()))?;
                    decoded_buffer.as_slice()
                } else {
                    // Nu's DFLT stream swaps the DEFLATE dynamic and stored
                    // block type tags. Game files use one final dynamic block.
                    if bytes[0] & 0b110 == 0 {
                        bytes[0] |= 0b100;
                    }
                    let decoder = DeflateDecoder::new(bytes.as_slice());
                    decoded_buffer.clear();
                    decoded_buffer.reserve(decoded);
                    decoder
                        .take(decoded as u64 + 1)
                        .read_to_end(&mut decoded_buffer)?;
                    decoded_buffer.as_slice()
                };
                if block.len() != decoded {
                    return Err(invalid("compressed block decoded size mismatch"));
                }
                output.write_all(block)?;
                written += block.len() as u64;
            }
        }
    }
    if written != entry.size as u64 {
        return Err(invalid("decoded entry size mismatch"));
    }
    Ok(written)
}
