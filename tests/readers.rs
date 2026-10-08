use nudat::{encode_payload, Archive, ArchiveWriter, Compression, Format, ReaderArchive};
use std::io::{Cursor, Seek, SeekFrom};

#[test]
fn memory_read_write_rewrite_all_formats() {
    for format in [Format::Pc, Format::PcLegacy, Format::Android, Format::Obb] {
        let data = b"compressible memory payload\n".repeat(4000);
        let mut encoded = Vec::new();
        let size = encode_payload(
            &mut data.as_slice(),
            &mut encoded,
            data.len() as u32,
            format,
            |_| {},
        )
        .unwrap();
        let compression = match format {
            Format::Pc | Format::PcLegacy => Compression::Lz2k,
            _ => Compression::Deflate,
        };
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()), format).unwrap();
        writer
            .add_encoded(
                "Levels/test.ghg",
                &mut encoded.as_slice(),
                size,
                data.len() as u32,
                compression,
            )
            .unwrap();
        writer.add_raw("empty.bin", &mut &b""[..], 0).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut source = Cursor::new(bytes.clone());
        let index = Archive::new(&mut source).unwrap();
        assert_eq!(index.read(&mut source, "levels\\TEST.GHG").unwrap(), data);
        index.verify(&mut source).unwrap();
        let mut rewrite = ArchiveWriter::from_archive(Cursor::new(Vec::new()), &index).unwrap();
        for entry in index.entries() {
            source.seek(SeekFrom::Start(entry.offset)).unwrap();
            rewrite
                .add_encoded(
                    &entry.path,
                    &mut source,
                    entry.stored_size,
                    entry.size,
                    entry.compression,
                )
                .unwrap();
        }
        rewrite.add_raw("added.txt", &mut &b"new"[..], 3).unwrap();
        let rewritten = rewrite.finish().unwrap().into_inner();
        let mut changed = ReaderArchive::new(Cursor::new(rewritten.clone())).unwrap();
        assert_eq!(changed.read("added.txt").unwrap(), b"new");
        assert_eq!(changed.read("Levels/test.ghg").unwrap(), data);
        changed.verify().unwrap();
        let entry = changed.entry("Levels/test.ghg").unwrap();
        assert_eq!(
            &rewritten[entry.offset as usize..][..entry.stored_size as usize],
            encoded
        );
    }
}

#[test]
fn empty_and_invalid_archives_and_short_sources() {
    for format in [Format::Pc, Format::PcLegacy, Format::Android, Format::Obb] {
        let bytes = ArchiveWriter::new(Cursor::new(Vec::new()), format)
            .unwrap()
            .finish()
            .unwrap()
            .into_inner();
        assert!(ReaderArchive::new(Cursor::new(bytes))
            .unwrap()
            .entries()
            .is_empty());
    }
    assert!(Archive::new(&mut Cursor::new(vec![0; 16])).is_err());
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()), Format::Pc).unwrap();
    assert!(writer.add_raw("../escape", &mut &b"x"[..], 1).is_err());
    assert!(writer.add_raw("short", &mut &b"x"[..], 2).is_err());
}
