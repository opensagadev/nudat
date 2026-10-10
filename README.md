# nudat

[![CI](https://github.com/opensagadev/nudat/actions/workflows/ci.yml/badge.svg)](https://github.com/opensagadev/nudat/actions/workflows/ci.yml)
[![Rust 2021](https://img.shields.io/badge/Rust-2021-orange)](Cargo.toml)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Inspect, unpack, edit, and repack Traveller's Tales Nu engine archives.**

PC `.DAT` (including the older `MkDat V3.26` variant), Android `.dat`, and Android `.obb` are supported.

## Usage

Install the CLI from crates.io, or download a prebuilt binary from
[GitHub Releases](https://github.com/opensagadev/nudat/releases):

```sh
cargo install nudat
```

To build from a checkout, run `cargo build --release`; the executable is
`./target/release/nudat`.

```sh
nudat info GAME.DAT
nudat list GAME.DAT --long
nudat tree GAME.DAT --depth 2
nudat cat GAME.DAT 'STUFF/TEXT/BADWORDS.TXT' > badwords.txt
# After editing badwords.txt:
nudat edit GAME.DAT patched.DAT --put 'STUFF/TEXT/BADWORDS.TXT=badwords.txt'
```

Unpack, change files, and pack them again:

```sh
nudat unpack GAME.DAT game/
nudat pack game/ rebuilt.DAT
nudat verify rebuilt.DAT

nudat unpack HERO1.DAT hero/
nudat pack hero/ rebuilt-hero.DAT --format pc-legacy

nudat unpack main.1060.com.wb.lego.tcs.obb obb/
nudat pack obb/ rebuilt.obb
```

| Command | Purpose |
| --- | --- |
| `nudat info ARCHIVE` | Show version, file count, and sizes. |
| `nudat list ARCHIVE [--filter TEXT] [--long]` | List every file in directory order. |
| `nudat tree ARCHIVE [--filter TEXT] [--depth N]` | Summarize directories and file counts. |
| `nudat cat ARCHIVE PATH` | Write decoded bytes to stdout. |
| `nudat extract ARCHIVE PATH OUTPUT` | Save one decoded file. |
| `nudat unpack ARCHIVE DIR [--jobs N]` | Extract everything. |
| `nudat pack DIR OUTPUT [--format FORMAT] [--jobs N]` | Build an archive. |
| `nudat edit ARCHIVE OUTPUT --put PATH=FILE [--remove PATH]` | Replace, add, or remove files. |
| `nudat verify ARCHIVE` | Decode and check every entry. |

`pack` selects OBB for a `.obb` output and modern PC otherwise; `FORMAT` is `pc`, `pc-legacy`, `android`, or `obb`. Use `--format pc-legacy` for older PC archives. Use `--format android` for Android `.dat`. `--jobs` defaults to Rayon's available worker count. Archive paths display with `/`, accept either separator as input, and ignore ASCII letter case.

Run `nudat <command> --help` for every option.

## Rust library and CLI

The `nudat` crate contains both the library and the `nudat` executable. The
executable and its dependencies (argument parsing, terminal progress, threading,
temporary files) sit behind the default `cli` feature; library users can disable
it with `default-features = false`.

The library itself has no filesystem or threading dependencies. `Archive::new(&mut reader)`
(or `ArchiveIndex::from_reader`) parses a `Read + Seek` source. `ReaderArchive`
owns the reader for repeated reads. `ArchiveWriter` packs or rewrites archives
through any `Write + Seek` destination; `encode_payload` provides compression.
Files, directories, temporary staging, and parallel operations live exclusively
in the CLI's `src/filesystem.rs`. The browser application lives in
[opensaga.dev](https://github.com/opensagadev/opensaga.dev).

The CLI runs packing and unpacking in parallel, reports progress on stderr, and writes decoded `cat` bytes directly to stdout.

### Portable reader

Without the `cli` feature, the library is portable, including WebAssembly:

```toml
[dependencies]
nudat = { version = "0.1", default-features = false }
```

`ArchiveIndex` parses any `Read + Seek` source, including `Cursor<&[u8]>`:

```rust
use nudat::{ArchiveIndex, Result};
use std::io::Cursor;

fn extract(archive_bytes: &[u8], entry_path: &str) -> Result<Vec<u8>> {
    let mut source = Cursor::new(archive_bytes);
    let index = ArchiveIndex::from_reader(&mut source)?;
    index.read(&mut source, entry_path)
}
```

Indexing reads metadata, leaving payloads in the source. `entries()` lists files;
`copy_to(&mut source, path, &mut writer)` streams a decoded entry, and
`verify(&mut source)` checks every entry. Always supply the same archive data used
to build the index. Folder selection can filter entries by their `/`-separated
path prefix.

The portable library builds for `wasm32-unknown-unknown`. It is not yet a JavaScript
package: a browser integration still needs a WebAssembly binding and browser file
I/O. `Cursor` requires the bytes in memory; asynchronous range reads for large
browser files need an adapter or additional API work. Website UI and styling can
stay in the consuming website repository.

```sh
rustup target add wasm32-unknown-unknown
cargo build --no-default-features --target wasm32-unknown-unknown
cargo test --no-default-features
```

## Releases

The release workflow builds Windows x64, Linux x64 (Ubuntu 22.04/glibc),
macOS Intel, and macOS Apple Silicon binaries. Windows downloads are ZIP files;
Linux and macOS downloads are tar.gz files. Each includes `nudat`, this README,
and the MIT license. Releases include a `SHA256SUMS` file for verification.

To release, bump the version in `Cargo.toml` and `Cargo.lock`, merge to `main`,
and run the **Release** workflow manually from `main`. All four builds must pass
their tests and extracted-binary smoke tests; the workflow then publishes the
crate to crates.io and creates a GitHub Release tagged `vX.Y.Z` with generated
notes and the downloads. Enable **dry-run** to build and verify without
publishing. Reruns skip versions already on crates.io and will not overwrite
published releases.

Pushing a `vX.Y.Z` tag instead creates a **draft** GitHub Release for review
without publishing to crates.io; the workflow rejects tags that do not match
`Cargo.toml`. Prerelease versions such as `v0.2.0-rc.1` are marked as
prereleases. Pull requests build downloadable Actions artifacts without
creating releases.

## Archive formats

### Variants

| Variant | Index version | Prefix | New payloads |
| --- | ---: | --- | --- |
| PC MkDat V3.26 | `-2` | 2,048 bytes; `MkDat V3.26` | Selective `LZ2K` |
| PC MkDat | `-3` | 1,024 bytes; `MkDat v4.0` | Selective `LZ2K` |
| Android PakDat | `-5` | 512 bytes; `PakDat (TechRound) v1.1` | Selective `DFLT` |
| Android OBB | `-5` | 512 bytes; `PakDat v1.01` | Selective `DFLT` |

### Android OBB

An OBB is a standalone PakDat archive, not a ZIP container. Its 512-byte prefix is:

```text
0x000  u32 index offset
0x004  u32 index length
0x008  "BEGIN_APP_ID_STRINGPakDat v1.01END_APP_ID_STRING\0"
       zero padding through 0x1ff
0x200  256-byte-aligned file payloads
       -5 index at the recorded offset
```

Android `.dat` has the same index, tree, and payload encoding, but identifies itself as `PakDat (TechRound) v1.1`. An OBB can be repacked from the extracted files alone.

### Layout and index

```text
0x00  i32 index offset (negative: offset in 256-byte sectors)
0x04  u32 index length
0x08  variant prefix
      file payloads, each starting at a 256-byte boundary
      index:
        i32 version, i32 file count
        file records[file count]       (16 bytes each)
        i32 node count, nodes           (8 bytes for -2/-3; 12 for -5)
        i32 name-table length, names    (NUL-terminated)
        u32 path hashes[file count]
        i32 extra-hash count, i32 extra-hash length, extra data
```

- **File record:** four little-endian `i32` values: offset ÷ 256, stored size, decoded size, compression mode.
- **Tree node:** `i16` child/sibling links and a `u32` name offset; `-5` adds two `u16` fields.
- **Paths:** the DAT index hashes backslash-separated paths; `nudat` displays `/` and accepts either separator. Lookup ignores ASCII case, preserves original spelling, and rejects case-only duplicates.
- **Hash:** start at `0x811c9dc5`; for each ASCII-uppercase path byte, apply `(hash ^ byte) * 0x199933` modulo `2^32`.
- **PC `-2`:** the header stores the negative index offset in 256-byte sectors. Hashes follow file-record order, and tree leaf numbers identify those records directly. They need not be sorted.
- **PC `-3` / Android `-5`:** hashes are sorted. Tree leaf numbers are not file-record indexes; `nudat` matches names through the hashes.

### Payload encoding

| Mode | Payload |
| ---: | --- |
| `0` | Raw bytes. |
| `2` | `LZ2K` chunks: magic, `u32 decoded_size`, `u32 stored_size`, then data. |
| `3` | `DFLT` chunks: magic, `u32 stored_size`, `u32 decoded_size`, then data. |

- Equal stored and decoded chunk sizes mean verbatim bytes.
- `LZ2K` uses Huffman-coded literals and lengths with back-references up to 8 KiB. Modern PC packing compresses `.an3`, `.bsa`, `.dds`, `.fpk`, `.ghg`, `.gsc`, `.pak`, and `.ter` when beneficial; legacy PC packing uses `.fpk`, `.ghg`, `.gsc`, and `.pak`.
- Nu `DFLT` swaps the standard DEFLATE dynamic and stored block tags; the fixed-Huffman tag is unchanged.
- Android packing compresses these suffixes when beneficial: `.android_etc1_tex`, `.bsa`, `.cu2`, `.etc1`, `.fpk`, `.ghg`, `.gsc`, `.ios_pcode`, `.ios_vcode`, `.pak`, `.pvrnc`, `.ter`, `.tex`.
- Text, scripts, audio, and other streamed files stay raw. Encoded files use 16 KiB decoded chunks; incompressible chunks stay verbatim. Compressed Android chunks use one final fixed-Huffman block.

Modern PC and Android use a positive signed index offset, limiting the index position to below 2 GiB. Legacy PC stores a negative sector offset instead. File and tree references are signed 16-bit; individual sizes use signed 32-bit fields.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

MIT licensed. Game assets are not included.
