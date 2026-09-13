use nudat::{pack, Archive, Format};
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn tempdir() -> std::io::Result<TempDir> {
    tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target"))
}

#[test]
fn tree_and_cat_show_archive_contents() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("input");
    fs::create_dir_all(input.join("levels/episode_i")).unwrap();
    fs::write(input.join("levels/episode_i/one.bin"), b"\0hello\n").unwrap();
    fs::write(input.join("levels/episode_i/two.bin"), b"second").unwrap();
    fs::write(input.join("root.bin"), b"root").unwrap();
    let archive = temp.path().join("test.dat");
    pack(&input, &archive, Format::Pc).unwrap();

    let tree = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args(["tree", archive.to_str().unwrap(), "--filter", "one.bin"])
        .output()
        .unwrap();
    assert!(tree.status.success());
    assert_eq!(
        String::from_utf8(tree.stdout).unwrap(),
        "test.dat (1 file)\n└── levels/ (1 file)\n    └── episode_i/ (1 file)\n"
    );

    let list = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args(["list", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8(list.stdout).unwrap(),
        "levels\\episode_i\\one.bin\nlevels\\episode_i\\two.bin\nroot.bin\n"
    );

    let cat = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args(["cat", archive.to_str().unwrap(), "LEVELS/EPISODE_I/ONE.BIN"])
        .output()
        .unwrap();
    assert!(cat.status.success());
    assert_eq!(cat.stdout, b"\0hello\n");

    let unpacked = temp.path().join("unpacked");
    let unpack = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args([
            "unpack",
            archive.to_str().unwrap(),
            unpacked.to_str().unwrap(),
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();
    assert!(unpack.status.success());
    assert!(String::from_utf8(unpack.stderr)
        .unwrap()
        .contains("unpack: 100%"));
    assert_eq!(unpack.stdout, b"unpacked 3 files\n");

    let bad_jobs = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args([
            "unpack",
            archive.to_str().unwrap(),
            unpacked.to_str().unwrap(),
            "--jobs",
            "0",
        ])
        .output()
        .unwrap();
    assert!(!bad_jobs.status.success());
    assert!(String::from_utf8(bad_jobs.stderr)
        .unwrap()
        .contains("--jobs must be at least 1"));

    let obb = temp.path().join("rebuilt.obb");
    let packed = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args([
            "pack",
            input.to_str().unwrap(),
            obb.to_str().unwrap(),
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();
    assert!(packed.status.success());
    let progress = String::from_utf8(packed.stderr).unwrap();
    assert!(progress.contains("compress: 100%"));
    assert!(progress.contains("pack: 100%"));
    assert_eq!(packed.stdout, b"packed 3 files\n");
    let bytes = fs::read(&obb).unwrap();
    assert!(bytes[8..512]
        .windows(b"PakDat v1.01".len())
        .any(|window| window == b"PakDat v1.01"));
    let obb_archive = Archive::open(&obb).unwrap();
    assert_eq!(obb_archive.version(), -5);
    assert_eq!(obb_archive.read("root.bin").unwrap(), b"root");

    fs::write(input.join("root.bin"), b"changed").unwrap();
    let rebuilt = temp.path().join("rebuilt_again.obb");
    let packed = Command::new(env!("CARGO_BIN_EXE_nudat"))
        .args([
            "pack",
            input.to_str().unwrap(),
            rebuilt.to_str().unwrap(),
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();
    assert!(packed.status.success());
    let stderr = String::from_utf8(packed.stderr).unwrap();
    assert!(stderr.contains("compress: 100%"));
    assert!(stderr.contains("pack: 100%"));
    assert_eq!(
        Archive::open(rebuilt).unwrap().read("root.bin").unwrap(),
        b"changed"
    );
}
