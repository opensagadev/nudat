use nudat::{ArchiveIndex, Compression, Format, NudatError};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

const FIXTURES: &[(Format, &[u8])] = &[
    (Format::Pc, include_bytes!("fixtures/pc.dat")),
    (Format::PcLegacy, include_bytes!("fixtures/pc-legacy.dat")),
    (Format::Android, include_bytes!("fixtures/android.dat")),
    (Format::Obb, include_bytes!("fixtures/obb.dat")),
];

#[test]
fn independent_chunks_reconstruct_all_formats_in_reverse_order() {
    for &(_, bytes) in FIXTURES {
        let mut source = Cursor::new(bytes);
        let archive = ArchiveIndex::from_reader(&mut source).unwrap();
        for entry in archive.entries() {
            let chunks = archive.chunks(&mut source, &entry.path, 16384).unwrap();
            let expected = archive.read(&mut source, &entry.path).unwrap();
            let mut actual = vec![0; entry.size as usize];
            let mut next = 0;
            for chunk in &chunks {
                assert_eq!(chunk.output_offset, next);
                next += u64::from(chunk.size);
                assert!(chunk.size <= 16384);
            }
            assert_eq!(next, u64::from(entry.size));
            for chunk in chunks.iter().rev() {
                let mut decoded = Vec::new();
                chunk.copy_to(&mut source, &mut decoded).unwrap();
                let start = chunk.output_offset as usize;
                actual[start..start + decoded.len()].copy_from_slice(&decoded);
            }
            assert_eq!(actual, expected);
        }
        assert!(archive.chunks(&mut source, "readme.txt", 0).is_err());
        let raw = archive.chunks(&mut source, "readme.txt", 5).unwrap();
        assert!(raw.len() > 1);
        let mut raw_output = Vec::new();
        for chunk in raw {
            chunk.copy_to(&mut source, &mut raw_output).unwrap();
        }
        assert_eq!(raw_output, b"hello from nudat\n");
        assert!(archive.chunks(&mut source, "nested/sample.gsc", 1).is_err());
        let offset = archive.entry("nested/sample.gsc").unwrap().offset as usize;
        let mut broken = bytes.to_vec();
        broken[offset..offset + 4].fill(0);
        assert!(archive
            .chunks(&mut Cursor::new(broken), "nested/sample.gsc", 16384)
            .is_err());
    }
}

#[test]
fn decodes_all_formats_without_filesystem_access() {
    for &(format, bytes) in FIXTURES {
        let mut source = Cursor::new(bytes);
        source.set_position(19); // Parsing must not depend on the initial position.
        let archive = ArchiveIndex::from_reader(&mut source).unwrap();
        assert_eq!(archive.format(), Some(format));
        assert_eq!(archive.entries().len(), 3);
        assert_eq!(
            archive.read(&mut source, "README.TXT").unwrap(),
            b"hello from nudat\n"
        );
        assert_eq!(archive.read(&mut source, "empty.bin").unwrap(), b"");
        let compressed = archive.entry("NESTED\\SAMPLE.GSC").unwrap();
        assert_ne!(compressed.compression, Compression::None);
        let expected = b"portable archive data\n".repeat(2400);
        let mut output = Vec::new();
        assert_eq!(
            archive
                .copy_to(&mut source, "nested/sample.gsc", &mut output)
                .unwrap(),
            expected.len() as u64
        );
        assert_eq!(output, expected);
        archive.verify(&mut source).unwrap();
        assert!(matches!(
            archive.read(&mut source, "missing"),
            Err(NudatError::MissingEntry(_))
        ));
    }
}

#[test]
fn rejects_bad_indexes_and_truncated_payloads() {
    assert!(ArchiveIndex::from_reader(&mut Cursor::new(b"bad archive")).is_err());
    let mut bytes = FIXTURES[0].1.to_vec();
    bytes[..4].copy_from_slice(&i32::MAX.to_le_bytes());
    assert!(ArchiveIndex::from_reader(&mut Cursor::new(bytes)).is_err());

    let mut source = Cursor::new(FIXTURES[0].1);
    let archive = ArchiveIndex::from_reader(&mut source).unwrap();
    let entry = archive.entry("readme.txt").unwrap();
    let truncated = &FIXTURES[0].1[..entry.offset as usize + 1];
    assert!(archive
        .read(&mut Cursor::new(truncated), "readme.txt")
        .is_err());
}

// A custom source ensures indexing only touches metadata, not entry payloads.
struct MetadataOnly<'a> {
    inner: Cursor<&'a [u8]>,
    payload: std::ops::Range<u64>,
}

impl Read for MetadataOnly<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let start = self.inner.position();
        let end = start + buffer.len() as u64;
        assert!(end <= self.payload.start || start >= self.payload.end);
        self.inner.read(buffer)
    }
}

impl Seek for MetadataOnly<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[test]
fn index_does_not_read_payloads() {
    let bytes = FIXTURES[0].1;
    let end = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64;
    let mut source = MetadataOnly {
        inner: Cursor::new(bytes),
        payload: 1024..end,
    };
    assert_eq!(
        ArchiveIndex::from_reader(&mut source)
            .unwrap()
            .entries()
            .len(),
        3
    );
}
