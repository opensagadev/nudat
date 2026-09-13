use nudat::{pack, pack_with_progress, Archive, Compression, Format, NudatError, PackPhase};
use std::collections::HashSet;
use std::fs;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;

fn tempdir() -> std::io::Result<TempDir> {
    tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target"))
}

#[test]
fn pack_unpack_and_edit_all_wrappers() {
    for format in [Format::Pc, Format::Android, Format::Obb] {
        let temp = tempdir().unwrap();
        let input = temp.path().join("input");
        fs::create_dir_all(input.join("Levels/Episode_I")).unwrap();
        fs::write(input.join("Levels/Episode_I/level.scp"), b"level contents").unwrap();
        fs::write(input.join("empty.bin"), b"").unwrap();
        let archive = temp.path().join("archive.dat");
        pack(&input, &archive, format).unwrap();
        let dat = Archive::open(&archive).unwrap();
        assert_eq!(dat.entries().len(), 2);
        assert_eq!(
            dat.read("levels/episode_i/LEVEL.SCP").unwrap(),
            b"level contents"
        );
        assert_eq!(dat.read("empty.bin").unwrap(), b"");
        assert_eq!(
            dat.entry("LEVELS\\EPISODE_I\\LEVEL.SCP")
                .unwrap()
                .compression,
            Compression::None
        );
        dat.verify().unwrap();
        let unpacked = temp.path().join("unpacked");
        dat.unpack(&unpacked).unwrap();
        assert_eq!(
            fs::read(unpacked.join("Levels/Episode_I/level.scp")).unwrap(),
            b"level contents"
        );
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, b"updated").unwrap();
        let edited = temp.path().join("edited.dat");
        dat.rewrite(
            &edited,
            &[
                ("Levels/Episode_I/level.scp".into(), replacement),
                ("added.bin".into(), unpacked.join("empty.bin")),
            ],
            &["empty.bin".into()],
        )
        .unwrap();
        let changed = Archive::open(&edited).unwrap();
        assert_eq!(changed.entries().len(), 2);
        assert_eq!(
            changed.read("levels/episode_i/level.scp").unwrap(),
            b"updated"
        );
        assert_eq!(changed.read("added.bin").unwrap(), b"");
        assert!(changed.entry("empty.bin").is_none());
        changed.verify().unwrap();
    }
}

#[test]
fn pc_tree_leaf_numbers_do_not_determine_payload_order() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("alpha.txt"), b"alpha content").unwrap();
    fs::write(input.join("beta.txt"), b"beta content").unwrap();
    let archive = temp.path().join("test.dat");
    pack(&input, &archive, Format::Pc).unwrap();

    let mut bytes = fs::read(&archive).unwrap();
    let index = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let nodes_start = index + 8 + 2 * 16 + 4;
    let count = i32::from_le_bytes(
        bytes[index + 8 + 2 * 16..index + 8 + 2 * 16 + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let leaves = (1..count)
        .map(|i| nodes_start + i * 8)
        .filter(|&at| i16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) <= 0)
        .collect::<Vec<_>>();
    assert_eq!(leaves.len(), 2);
    let first = bytes[leaves[0]..leaves[0] + 2].to_vec();
    let second = bytes[leaves[1]..leaves[1] + 2].to_vec();
    bytes[leaves[0]..leaves[0] + 2].copy_from_slice(&second);
    bytes[leaves[1]..leaves[1] + 2].copy_from_slice(&first);
    fs::write(&archive, bytes).unwrap();

    let dat = Archive::open(&archive).unwrap();
    assert_eq!(dat.read("alpha.txt").unwrap(), b"alpha content");
    assert_eq!(dat.read("beta.txt").unwrap(), b"beta content");
}

#[test]
fn errors_distinguish_missing_entries_and_bad_indexes() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("one.txt"), b"one").unwrap();
    let archive = temp.path().join("test.dat");
    pack(&input, &archive, Format::Pc).unwrap();
    let dat = Archive::open(&archive).unwrap();
    assert!(matches!(
        dat.read("missing.txt"),
        Err(NudatError::MissingEntry(_))
    ));

    let mut bytes = fs::read(&archive).unwrap();
    bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&archive, bytes).unwrap();
    assert!(matches!(
        Archive::open(&archive),
        Err(NudatError::InvalidArchive(_))
    ));
}

#[test]
fn unpack_progress_reports_bytes_within_a_file() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("large.bin"), vec![42; 128 * 1024]).unwrap();
    fs::write(input.join("small.bin"), b"small").unwrap();
    let archive = temp.path().join("test.dat");
    pack(&input, &archive, Format::Pc).unwrap();
    let dat = Archive::open(&archive).unwrap();
    let total = dat
        .entries()
        .iter()
        .map(|entry| entry.size as u64)
        .sum::<u64>();
    let mut events = Vec::new();
    dat.unpack_with_progress(temp.path().join("unpacked"), |entry, files, bytes| {
        events.push((entry.path.clone(), files, bytes));
    })
    .unwrap();
    assert_eq!(events.first().unwrap().2, 0);
    assert_eq!(events.last().unwrap().1, 2);
    assert_eq!(events.last().unwrap().2, total);
    assert!(events
        .iter()
        .any(|event| event.1 == 0 && event.2 > 0 && event.2 < total));
}

#[test]
fn parallel_unpack_uses_multiple_workers_and_preserves_files() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    let expected = (0..16)
        .map(|index| {
            let name = format!("file_{index:02}.bin");
            let contents = vec![index as u8; 64 * 1024];
            fs::write(input.join(&name), &contents).unwrap();
            (name, contents)
        })
        .collect::<Vec<_>>();
    let archive = temp.path().join("test.dat");
    pack(&input, &archive, Format::Pc).unwrap();
    let dat = Archive::open(&archive).unwrap();
    let output = temp.path().join("unpacked");
    let workers = Mutex::new(HashSet::new());
    let files_seen = AtomicUsize::new(0);
    let bytes_seen = AtomicU64::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    pool.install(|| {
        dat.unpack_parallel_with_progress(&output, |_, files, bytes| {
            workers.lock().unwrap().insert(std::thread::current().id());
            files_seen.fetch_max(files, Ordering::Relaxed);
            bytes_seen.fetch_max(bytes, Ordering::Relaxed);
            if files == 0 && bytes == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    })
    .unwrap();

    assert!(workers.lock().unwrap().len() > 1);
    assert_eq!(files_seen.load(Ordering::Relaxed), expected.len());
    assert_eq!(bytes_seen.load(Ordering::Relaxed), 16 * 64 * 1024);
    for (name, contents) in expected {
        assert_eq!(fs::read(output.join(name)).unwrap(), contents);
    }
}

#[test]
fn parallel_pack_matches_sequential_writer_and_reports_progress() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    for index in 0..16 {
        fs::write(
            input.join(format!("file_{index:02}.bin")),
            vec![index as u8; 64 * 1024],
        )
        .unwrap();
    }
    fs::write(input.join("large.bin"), vec![42; 3 * 1024 * 1024]).unwrap();
    let archive = temp.path().join("parallel.dat");
    let events = Mutex::new(Vec::new());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    pool.install(|| {
        pack_with_progress(
            &input,
            &archive,
            Format::Pc,
            |phase, path, files, bytes, total_files, total_bytes| {
                events.lock().unwrap().push((
                    phase,
                    path.to_owned(),
                    files,
                    bytes,
                    total_files,
                    total_bytes,
                ));
            },
        )
    })
    .unwrap();
    let dat = Archive::open(&archive).unwrap();
    dat.verify().unwrap();
    let sequential = temp.path().join("sequential.dat");
    dat.rewrite(&sequential, &[], &[]).unwrap();
    assert_eq!(fs::read(&archive).unwrap(), fs::read(sequential).unwrap());

    let events = events.lock().unwrap();
    let total_bytes = 4 * 1024 * 1024;
    assert_eq!(events.iter().map(|event| event.2).max(), Some(17));
    assert_eq!(events.iter().map(|event| event.3).max(), Some(total_bytes));
    assert!(events
        .iter()
        .all(|event| event.4 == 17 && event.5 == total_bytes));
    assert!(events
        .iter()
        .any(|event| event.1 == "large.bin" && event.3 > 0 && event.3 < total_bytes));
}

#[test]
fn obb_repack_preserves_unchanged_files_and_reports_both_phases() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    fs::write(input.join("same.bin"), vec![b'A'; 300 * 1024]).unwrap();
    fs::write(input.join("changed.bin"), b"before").unwrap();
    fs::write(input.join("removed.bin"), b"remove").unwrap();
    let base_path = temp.path().join("base.obb");
    pack(&input, &base_path, Format::Obb).unwrap();
    let base = Archive::open(&base_path).unwrap();
    fs::write(input.join("changed.bin"), b"after").unwrap();
    fs::remove_file(input.join("removed.bin")).unwrap();
    fs::write(input.join("added.bin"), b"added").unwrap();
    let output = temp.path().join("rebuilt.obb");
    let phases = Mutex::new(HashSet::new());
    base.repack_with_progress(&input, &output, |phase, _, files, _, total, _| {
        phases.lock().unwrap().insert(phase);
        assert!(files <= total);
    })
    .unwrap();
    let dat = Archive::open(&output).unwrap();
    dat.verify().unwrap();
    assert_eq!(dat.format(), Some(Format::Obb));
    assert_eq!(dat.read("same.bin").unwrap(), vec![b'A'; 300 * 1024]);
    assert_eq!(dat.read("changed.bin").unwrap(), b"after");
    assert_eq!(dat.read("added.bin").unwrap(), b"added");
    assert!(dat.entry("removed.bin").is_none());
    let old = base.entry("same.bin").unwrap();
    let new = dat.entry("same.bin").unwrap();
    assert_eq!(old.compression, new.compression);
    assert_eq!(old.stored_size, new.stored_size);
    let old_bytes = fs::read(base_path).unwrap();
    let new_bytes = fs::read(output).unwrap();
    assert_eq!(
        &old_bytes[old.offset as usize..old.offset as usize + old.stored_size as usize],
        &new_bytes[new.offset as usize..new.offset as usize + new.stored_size as usize]
    );
    assert_eq!(
        *phases.lock().unwrap(),
        HashSet::from([PackPhase::Compare, PackPhase::Write])
    );
}

#[test]
fn pack_rejects_an_index_past_the_game_loader_limit() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(&input).unwrap();
    for name in ["a.bin", "b.bin"] {
        fs::File::create(input.join(name))
            .unwrap()
            .set_len(1_200_000_000)
            .unwrap();
    }
    let output = temp.path().join("too_large.dat");
    let error = pack(&input, &output, Format::Pc).unwrap_err();
    assert!(error.to_string().contains("2 GiB loader limit"));
}
