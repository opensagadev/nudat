# nudat

[![CI](https://github.com/opensagadev/nudat/actions/workflows/ci.yml/badge.svg)](https://github.com/opensagadev/nudat/actions/workflows/ci.yml)
[![Rust 2021](https://img.shields.io/badge/Rust-2021-orange)](Cargo.toml)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Inspect, unpack, edit, and repack Traveller's Tales Nu engine archives.**

PC `.DAT`, Android `.dat`, and Android `.obb` are supported.

## Usage

Build the release CLI:

```sh
cargo build --release
```

The executable is `./target/release/nudat`. Commands below use `nudat` for brevity.

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

`pack` selects OBB for a `.obb` output and PC otherwise; `FORMAT` is `pc`, `android`, or `obb`. Use `--format android` for Android `.dat`. `--jobs` defaults to Rayon's available worker count. Archive paths accept `/` or `\` and ignore ASCII letter case.

Run `nudat <command> --help` for every option.

## Rust library and CLI

The `nudat` crate exposes `Archive::open`, `entries`, `read`, `copy_to`, `extract`, `unpack`, `rewrite`, and `pack`, with progress variants for bulk operations. Entries stay on disk until read.

The CLI runs packing and unpacking in parallel, reports progress on stderr, and writes decoded `cat` bytes directly to stdout.

## Archive formats

### Variants

| Variant | Index version | Prefix | New payloads |
| --- | ---: | --- | --- |
| PC MkDat | `-3` | 1,024 bytes; `MkDat v4.0` | Uncompressed |
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
0x00  u32 index offset
0x04  u32 index length
0x08  variant prefix
      file payloads, each starting at a 256-byte boundary
      index:
        i32 version, i32 file count
        file records[file count]       (16 bytes each)
        i32 node count, nodes           (8 bytes for -3; 12 for -5)
        i32 name-table length, names    (NUL-terminated)
        u32 path hashes[file count]     (sorted)
        i32 extra-hash count, i32 extra-hash length, extra data
```

- **File record:** four little-endian `i32` values: offset ÷ 256, stored size, decoded size, compression mode.
- **Tree node:** `i16` child/sibling links and a `u32` name offset; `-5` adds two `u16` fields.
- **Paths:** backslash separators, ASCII case-insensitive lookup, original spelling preserved. Case-only duplicates are rejected.
- **Hash:** start at `0x811c9dc5`; for each ASCII-uppercase path byte, apply `(hash ^ byte) * 0x199933` modulo `2^32`. Records follow sorted hash order.
- **PC `-3`:** tree leaf numbers are not file-record indexes; `nudat` matches names through the hashes.

### Payload encoding

| Mode | Payload |
| ---: | --- |
| `0` | Raw bytes. |
| `2` | `LZ2K` chunks: magic, `u32 decoded_size`, `u32 stored_size`, then data. Decode only. |
| `3` | `DFLT` chunks: magic, `u32 stored_size`, `u32 decoded_size`, then data. |

- Equal stored and decoded chunk sizes mean verbatim bytes.
- Nu `DFLT` swaps the standard DEFLATE dynamic and stored block tags; the fixed-Huffman tag is unchanged.
- Android packing compresses these suffixes when beneficial: `.android_etc1_tex`, `.bsa`, `.cu2`, `.etc1`, `.fpk`, `.ghg`, `.gsc`, `.ios_pcode`, `.ios_vcode`, `.pak`, `.pvrnc`, `.ter`, `.tex`.
- Text, scripts, audio, and other streamed files stay raw. Encoded files use 16 KiB decoded chunks with one final fixed-Huffman block; incompressible chunks stay verbatim.

The game treats the index offset as signed, so `nudat` rejects output whose index starts at or above 2 GiB. File and tree references are signed 16-bit; individual sizes use signed 32-bit fields.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

MIT licensed. Game assets are not included.
