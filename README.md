# nudat

[![CI](https://github.com/opensagadev/nudat/actions/workflows/ci.yml/badge.svg)](https://github.com/opensagadev/nudat/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-2021-orange)
![License: MIT](https://img.shields.io/badge/license-MIT-blue)

A Rust library and command-line tool for Traveller's Tales Nu engine archives. Inspect, extract, edit, and rebuild PC `.DAT`, Android `.dat`, and Android `.obb` files. Payloads are streamed, so inspecting a large archive does not load it into memory.

## Quick start

```sh
cargo build --release

target/release/nudat info GAME.DAT
target/release/nudat list GAME.DAT --long
target/release/nudat tree GAME.DAT --depth 3
target/release/nudat cat GAME.DAT 'STUFF\TEXT\BADWORDS.TXT' > badwords.txt
target/release/nudat unpack GAME.DAT game/ --jobs 8
target/release/nudat pack game/ rebuilt.DAT --format pc --jobs 8
target/release/nudat verify rebuilt.DAT
```

To rebuild an Android expansion file for the game, provide the original OBB as a base. `nudat` compares the unpacked files with it, keeps unchanged compressed payloads byte-for-byte, and stores edited or added files without compression:

```sh
target/release/nudat unpack main.1060.com.wb.lego.tcs.obb obb/
target/release/nudat pack obb/ rebuilt.obb \
  --base main.1060.com.wb.lego.tcs.obb --jobs 8
target/release/nudat verify rebuilt.obb
```

Without `--base`, `pack` writes uncompressed entries. That works for PC archives and small Android archives, but a full uncompressed OBB can exceed the game's 2 GiB seek limit. `--format pc|android|obb` overrides the output-name default. `edit` is useful when only a few files change: it copies unchanged payloads in their original encoded form.

```sh
target/release/nudat edit GAME.DAT modified.DAT \
  --put 'STUFF\TEXT\BADWORDS.TXT=badwords.txt' \
  --remove 'OLD\FILE.BIN'
```

`list` traverses directories in order, while `tree` summarizes them with recursive file counts. Both accept `--filter`; `tree` also accepts `--depth` and `--long`. `cat` emits the decoded file bytes unchanged, including binary files. `unpack` and `pack` run on Rayon workers and show file and byte progress on stderr. Base-backed packing reports comparison and writing separately. `--jobs` sets the worker count.

## Format notes

| Variant | Index version | Header | Packing |
| --- | ---: | --- | --- |
| PC MkDat | `-3` | 1,024-byte `MkDat v4.0` prefix | Uncompressed |
| Android PakDat | `-5` | 512-byte `PakDat (TechRound) v1.1` prefix | Original blocks with `--base` |
| Android OBB | `-5` | 512-byte `PakDat v1.01` prefix | Original blocks with `--base` |

The first eight bytes give the index offset and length as little-endian 32-bit integers. Payloads begin at 256-byte-aligned offsets. The index holds file sizes and compression modes, a directory tree and name table, then hashes sorted by the game's case-insensitive path hash. Tree leaf numbers are **not** file-record positions; `nudat` resolves names through the hash table. The reader supports uncompressed, `LZ2K`, and Nu `DFLT` payloads. Base-backed packing retains the original encoded blocks; newly added or edited files use mode 0 (uncompressed).

The game loader misinterprets a normal index offset above 2 GiB. `nudat` rejects archives that exceed that limit instead of producing an OBB the game cannot open. The supplied 27,493-file OBB rebuild with `--base` retains 19,490 compressed payloads and stays around 1.3 GB. File count, tree-node count, and individual entry sizes are also limited by the format's 16-bit and 32-bit fields.

The library exposes `Archive::open`, `entries`, `read`, `copy_to`, `extract`, `unpack`, `rewrite`, `repack`, `pack`, and their progress variants. Progress callbacks for parallel operations are thread-safe and may arrive out of order. `unpack_with_progress` remains available for sequential callbacks.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Licensed under MIT. Game assets are not included.
