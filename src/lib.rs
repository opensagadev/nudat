//! Reader and writer for Traveller's Tales Nu engine DAT archives.
//!
//! Versions -3 (MkDat) and -5 (PakDat, including OBB wrappers) are supported.
//! Rewrites retain the original wrapper and copy unchanged stored payloads byte-for-byte.

#![allow(unstable_name_collisions)]

mod dflt;
mod lz2k;

use binrw::{BinRead, BinReaderExt, BinWrite, BinWriterExt};
use flate2::read::DeflateDecoder;
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom, Write};

const ALIGN: u64 = 256;
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
    PcLegacy,
    Android,
    Obb,
}

impl Format {
    pub fn version(self) -> i32 {
        match self {
            Self::Pc => -3,
            Self::PcLegacy => -2,
            Self::Android | Self::Obb => -5,
        }
    }
    pub fn marker(self) -> &'static [u8] {
        match self {
            Self::Pc => b"MkDat v4.0",
            Self::PcLegacy => b"MkDat V3.26",
            Self::Android => b"PakDat (TechRound) v1.1",
            Self::Obb => b"PakDat v1.01",
        }
    }
    pub fn prefix_len(self) -> usize {
        match self {
            Self::Pc => 1024,
            Self::PcLegacy => 2048,
            Self::Android | Self::Obb => 512,
        }
    }
}

/// One file in the archive. Paths use forward slashes; lookups accept either separator.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub offset: u64,
    pub stored_size: u32,
    pub size: u32,
    pub compression: Compression,
}

/// Parsed DAT metadata. Payloads remain in the caller's reader until requested.
pub struct Archive {
    version: i32,
    prefix: Vec<u8>,
    entries: Vec<Entry>,
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

impl Archive {
    pub fn new(file: &mut (impl Read + Seek)) -> Result<Self> {
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
                if hashes[leaf] != path_hash(&path) {
                    return Err(invalid(format!("DAT hash mismatch for {path}")));
                }
                leaf
            } else {
                hashes
                    .binary_search(&path_hash(&path))
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
        let key = normalize_path(path).ok()?;
        self.entries
            .iter()
            .find(|e| e.path.eq_ignore_ascii_case(&key))
    }

    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Decode a single file from the same reader used to parse this index.
    pub fn read(&self, reader: &mut (impl Read + Seek), path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.into()))?;
        if entry.size > 512 * 1024 * 1024 {
            return Err(input("entry is too large for read; use copy_to instead"));
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.copy_to(reader, path, &mut bytes)?;
        Ok(bytes)
    }

    /// Stream decoded bytes from a caller-owned reader to a caller-owned writer.
    pub fn copy_to(
        &self,
        reader: &mut (impl Read + Seek),
        path: &str,
        output: &mut impl Write,
    ) -> Result<u64> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.into()))?;
        reader.seek(SeekFrom::Start(entry.offset))?;
        decode_entry_to(reader, entry, output)
    }

    pub fn verify(&self, reader: &mut (impl Read + Seek)) -> Result<()> {
        for entry in &self.entries {
            reader.seek(SeekFrom::Start(entry.offset))?;
            decode_entry_to(reader, entry, &mut io::sink()).map_err(|source| {
                NudatError::Entry {
                    path: entry.path.clone(),
                    source: Box::new(source),
                }
            })?;
        }
        Ok(())
    }
}

/// A DAT archive backed by a seekable reader rather than a filesystem path.
///
/// Use a `Cursor<Vec<u8>>` in browsers, or a custom reader that fetches byte
/// ranges. Parsing and decompression use the same metadata API.
/// Payloads are read only when requested. The reader must retain its contents.
pub struct ReaderArchive<R> {
    index: Archive,
    reader: R,
}

impl<R: Read + Seek> ReaderArchive<R> {
    pub fn new(mut reader: R) -> Result<Self> {
        let index = Archive::new(&mut reader)?;
        Ok(Self { index, reader })
    }

    pub fn version(&self) -> i32 {
        self.index.version()
    }

    pub fn format(&self) -> Option<Format> {
        self.index.format()
    }

    pub fn entries(&self) -> &[Entry] {
        self.index.entries()
    }

    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.index.entry(path)
    }

    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.into()))?;
        if entry.size > 512 * 1024 * 1024 {
            return Err(input("entry is too large for read; use copy_to instead"));
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.copy_to(path, &mut bytes)?;
        Ok(bytes)
    }

    pub fn copy_to(&mut self, path: &str, output: &mut impl Write) -> Result<u64> {
        let entry = self
            .index
            .entry(path)
            .ok_or_else(|| NudatError::MissingEntry(path.into()))?;
        self.reader.seek(SeekFrom::Start(entry.offset))?;
        decode_entry_to(&mut self.reader, entry, output)
    }

    pub fn verify(&mut self) -> Result<()> {
        for entry in self.index.entries() {
            self.reader.seek(SeekFrom::Start(entry.offset))?;
            decode_entry_to(&mut self.reader, entry, &mut io::sink()).map_err(|source| {
                NudatError::Entry {
                    path: entry.path.clone(),
                    source: Box::new(source),
                }
            })?;
        }
        Ok(())
    }
}
pub fn normalize_path(path: &str) -> Result<String> {
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

pub fn path_hash(path: &str) -> u32 {
    path.bytes().fold(0x811c9dc5u32, |hash, b| {
        let b = if b == b'/' { b'\\' } else { b };
        (hash ^ u32::from(b.to_ascii_uppercase())).wrapping_mul(0x199933)
    })
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

fn build_tree(pending: &[Entry]) -> Result<EncodedTree> {
    let mut root = Node::default();
    for (i, item) in pending.iter().enumerate() {
        let parts = item.path.split('/').collect::<Vec<_>>();
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

/// Write an index after stored payloads and patch the archive header at offset 0.
/// Entries may arrive in any order. Their offsets must be 256-byte aligned.
pub fn write_index(out: &mut (impl Write + Seek), version: i32, entries: &[Entry]) -> Result<()> {
    if ![-2, -3, -5].contains(&version) {
        return Err(input("unsupported DAT version"));
    }
    let mut pending = entries.to_vec();
    pending.sort_by_key(|entry| path_hash(&entry.path));
    if pending.len() > i16::MAX as usize {
        return Err(input("too many DAT files"));
    }
    let mut keys = HashSet::new();
    let mut hashes = HashSet::new();
    for entry in &pending {
        if normalize_path(&entry.path)? != entry.path {
            return Err(input("entry paths must use forward slashes"));
        }
        if !keys.insert(entry.path.to_ascii_uppercase()) {
            return Err(input("duplicate archive path"));
        }
        if !hashes.insert(path_hash(&entry.path)) {
            return Err(input("DAT path hash collision"));
        }
        if entry.offset % ALIGN != 0
            || entry.offset / ALIGN > i32::MAX as u64
            || entry.size > i32::MAX as u32
            || entry.stored_size > i32::MAX as u32
        {
            return Err(input("DAT entry exceeds format limit"));
        }
    }
    let tree = build_tree(&pending)?;
    let infos = pending
        .iter()
        .map(|entry| {
            (
                (entry.offset / ALIGN) as i32,
                entry.stored_size as i32,
                entry.size as i32,
                entry.compression.raw(),
            )
        })
        .collect::<Vec<_>>();
    let check_index = |offset: u64| -> Result<()> {
        let limit = if version == -2 {
            i32::MAX as u64 * ALIGN
        } else {
            i32::MAX as u64
        };
        if offset > limit {
            return Err(input("DAT index exceeds the game's loader limit"));
        }
        Ok(())
    };
    let mut index = out.stream_position()?;
    if version == -2 {
        let aligned = align(index);
        check_index(aligned)?;
        out.write_all(&vec![0; (aligned - index) as usize])?;
        index = aligned;
    }
    check_index(index)?;
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
        out.write_le(&path_hash(&item.path))?;
    }
    out.write_le(&0i32)?;
    out.write_le(&0i32)?;
    let end = out.stream_position()?;
    if end - index > u32::MAX as u64 {
        return Err(input("DAT index exceeds 32-bit length limit"));
    }
    out.seek(SeekFrom::Start(0))?;
    let header_offset = if version == -2 {
        -((index / ALIGN) as i32)
    } else {
        index as i32
    };
    out.write_le(&header_offset)?;
    out.write_le(&((end - index) as u32))?;
    Ok(out.flush()?)
}

/// Whether the game's packer compresses this path in the selected format.
pub fn should_compress(path: &str, format: Format) -> bool {
    match format {
        Format::Pc => lz2k::should_compress(path),
        Format::PcLegacy => lz2k::should_compress_legacy(path),
        Format::Android | Format::Obb => dflt::should_compress(path),
    }
}

/// Encode exactly `size` bytes as game-compatible compressed chunks.
/// Returns stored length. Callers may retain raw bytes if encoding is larger.
/// The callback reports cumulative input bytes after each chunk.
pub fn encode_payload(
    source: &mut impl Read,
    output: &mut impl Write,
    size: u32,
    format: Format,
    mut progress: impl FnMut(u64),
) -> Result<u32> {
    let mut buffer = [0u8; PACK_BLOCK_SIZE];
    let mut left = size as usize;
    let mut stored = 0u64;
    while left != 0 {
        let length = left.min(buffer.len());
        source.read_exact(&mut buffer[..length])?;
        let block = match format {
            Format::Pc | Format::PcLegacy => lz2k::encode_block(&buffer[..length]),
            Format::Android | Format::Obb => dflt::encode_block(&buffer[..length]),
        };
        if matches!(format, Format::Pc | Format::PcLegacy) {
            output.write_all(b"LZ2K")?;
            output.write_all(&(length as u32).to_le_bytes())?;
            output.write_all(&(block.len() as u32).to_le_bytes())?;
        } else {
            output.write_all(b"DFLT")?;
            output.write_all(&(block.len() as u32).to_le_bytes())?;
            output.write_all(&(length as u32).to_le_bytes())?;
        }
        output.write_all(&block)?;
        stored += 12 + block.len() as u64;
        left -= length;
        progress(size as u64 - left as u64);
    }
    u32::try_from(stored).map_err(|_| input("encoded payload exceeds 32-bit size limit"))
}

/// Builds archives on any seekable writer, including an in-memory Cursor.
/// Filesystem discovery, staging and parallel scheduling belong to the caller.
pub struct ArchiveWriter<W> {
    writer: W,
    version: i32,
    entries: Vec<Entry>,
    prefix_len: u64,
}

impl<W: Write + Seek> ArchiveWriter<W> {
    pub fn new(writer: W, format: Format) -> Result<Self> {
        let mut prefix = vec![0; format.prefix_len()];
        let marker = [
            b"BEGIN_APP_ID_STRING".as_slice(),
            format.marker(),
            b"END_APP_ID_STRING\0",
        ]
        .concat();
        prefix[8..8 + marker.len()].copy_from_slice(&marker);
        Self::with_prefix(writer, format.version(), &prefix)
    }

    /// Start a rewrite retaining an existing archive's wrapper and version.
    pub fn from_archive(writer: W, archive: &Archive) -> Result<Self> {
        Self::with_prefix(writer, archive.version(), archive.prefix())
    }

    fn with_prefix(mut writer: W, version: i32, prefix: &[u8]) -> Result<Self> {
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(prefix)?;
        Ok(Self {
            writer,
            version,
            entries: Vec::new(),
            prefix_len: prefix.len() as u64,
        })
    }

    /// Append raw bytes without compression.
    pub fn add_raw(&mut self, path: &str, source: &mut impl Read, size: u32) -> Result<()> {
        self.add_encoded(path, source, size, size, Compression::None)
    }

    /// Append a pre-encoded payload. Use this to preserve original compressed
    /// bytes during editing, or stream the output of `encode_payload`.
    pub fn add_encoded(
        &mut self,
        path: &str,
        source: &mut impl Read,
        stored_size: u32,
        size: u32,
        compression: Compression,
    ) -> Result<()> {
        let path = normalize_path(path)?;
        if size > i32::MAX as u32 || stored_size > i32::MAX as u32 {
            return Err(input("DAT entry exceeds format limit"));
        }
        if compression == Compression::None && size != stored_size {
            return Err(input("uncompressed size mismatch"));
        }
        if self.entries.iter().any(|entry| {
            entry.path.eq_ignore_ascii_case(&path) || path_hash(&entry.path) == path_hash(&path)
        }) {
            return Err(input("duplicate archive path or hash collision"));
        }
        let position = self.writer.stream_position()?;
        let offset = align(position);
        let end = offset
            .checked_add(stored_size as u64)
            .ok_or_else(|| input("DAT offset overflow"))?;
        let limit = if self.version == -2 {
            i32::MAX as u64 * ALIGN
        } else {
            i32::MAX as u64
        };
        if end > limit || offset / ALIGN > i32::MAX as u64 {
            return Err(input("DAT index exceeds the game's loader limit"));
        }
        self.writer
            .write_all(&vec![0; (offset - position) as usize])?;
        let copied = io::copy(&mut source.take(stored_size as u64), &mut self.writer)?;
        if copied != stored_size as u64 {
            return Err(input("source is shorter than declared payload length"));
        }
        self.entries.push(Entry {
            path,
            offset,
            stored_size,
            size,
            compression,
        });
        Ok(())
    }

    /// Finalize the index and return the writer positioned at the archive end.
    /// The destination should be empty or truncated before constructing it.
    pub fn finish(mut self) -> Result<W> {
        if self.entries.is_empty() {
            self.writer.seek(SeekFrom::Start(self.prefix_len))?;
        }
        write_index(&mut self.writer, self.version, &self.entries)?;
        self.writer.seek(SeekFrom::End(0))?;
        Ok(self.writer)
    }
}
