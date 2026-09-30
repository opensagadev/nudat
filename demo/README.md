# Web unpacker

A plain-JavaScript page that opens DATs/OBBs or their surrounding folder and
downloads one uncompressed ZIP through the browser's native Downloads UI. No
uploads or React. The browser must support service-worker streaming on localhost
or HTTPS. Chrome and Edge offer file and folder selection; Firefox offers file
selection.

Archives are combined into one root tree. Paths are case-insensitive, matching
nudat. Later input archives win duplicate paths; the page reports the number of
overrides. Conflicting file/directory paths are rejected. The ZIP preserves the
merged paths and original names, including `.cfg` and `.dll`. ZIP entries use the
Store method (no compression), data descriptors, and ZIP64 where required.
The ZIP streams through a locally hosted copy of StreamSaver 2.0.6 on Chromium.
Firefox registers the same scoped download worker directly and feeds it through
a backpressured message channel. The ZIP is not assembled in memory and uses
the browser's usual download location and progress UI. StreamSaver's MIT
license is included under `third-party/streamsaver/`.

## Build and run

Requires Rust, Node.js, and wasm-bindgen-cli **0.2.129**. From the repo root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --locked -p nudat-web --release --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir demo/pkg target/wasm32-unknown-unknown/release/nudat_web.wasm
node demo/prepare.mjs /path/to/saga
npm --prefix demo test
node demo/server.mjs
```

Open http://127.0.0.1:4173. The server binds to loopback only. OpenSaga styles and
assets are copied from its checkout, not duplicated in the library.
`index.template.html` contains only the unpacker page. `prepare.mjs` renders
the shared head, header, and footer from Saga's `scripts/site/` partials and
copies its single site stylesheet; `style.css` contains only unpacker controls.

## Try it

1. Use **Open DAT/OBB files** or **Open game folder** to select archives. All files start
   selected; expand folders in the directory tree or use the checkboxes to narrow
   the selection. Click **Download ZIP**.
2. **Open game folder** includes archives in subfolders. Unrelated files are ignored.
   Contents appear together at one root.
3. For a large export, the page shows one progress bar and percentage while it
   creates the ZIP. The browser's Downloads UI shows the actual save. Downloads
   use names like `nudat-20260927T120000Z.zip` (UTC).
4. Cancel an export. The incomplete ZIP stream is aborted. You can download
   again without reopening the archives. Browser-side cancellation also stops
   writing the stream.

One planner scans block headers and up to six workers decode independent batches
of at most 4 MiB, including batches from the same file. Pool size depends on CPU
count, not file size. One ZIP entry receives completed batches in byte order and
finishes only after all batches are written. The old
256 MiB per-file export limit no longer applies. Individual compressed blocks
larger than 4 MiB are rejected; blocks are never split through a compression stream.

ZIP entries are written one at a time because a ZIP is a sequential stream;
up to six workers still decode independent chunks of the current entry. There
are at most 24 pending batches and a 128 MiB decoded-data budget (the batch cap
currently tightens this to 96 MiB), excluding worker WASM heaps and browser
buffers. Slow ZIP writes pause dispatch. Input uses local range reads. Export
work is grouped by archive and physical offset. Repacking is not included.
Compressed batches enter WASM in one read; their internal blocks reuse input
and output buffers. Raw batches transfer directly from the file read without
entering WASM. Progress callbacks cross into JavaScript at 256 KiB intervals,
plus the first block and completion; input/index scanning uses a 256 KiB cache.

Tests execute the real WebAssembly and worker modules under Node, checking all
four formats, incremental byte progress, merged paths/overrides, cancellation,
write failures, ZIP structure, and capability checks. No computer-use tools are used.

## Decoder benchmarks

`cargo bench --bench decoder` measures decoding synthetic LZ2K blocks without
filesystem access. To measure the actual WASM decoder on a local archive:

```sh
node demo/bench-decoder.mjs /path/to/GAME.DAT
```

This reads entries in archive offset order, hashes the decoded bytes, and reports
elapsed time and throughput. It does not write extracted files. An optional
second argument selects another generated `nudat_web.js` for before/after
comparisons; its matching WASM must be in the same directory. The hash should
match across versions. This measures reading, decoding and hashing under Node,
not browser filesystem export speed.

To compare worker counts using the actual export scheduler without saving files:

```sh
node demo/bench-parallel.mjs /path/to/EPISODE_I.DAT target/chunk-hashes.json reference
node demo/bench-parallel.mjs /path/to/EPISODE_I.DAT target/chunk-hashes.json 1,2,4,6
```

The first command hashes chunks from the whole-entry decoder (which retains its
256 MiB per-entry limit for this reference check), saving only a small JSON
manifest. The second checks every parallel chunk against those hashes, including
its output offset and size. It includes planning, worker startup, reads, decoding,
transfers and hashing, but not destination filesystem writes.
