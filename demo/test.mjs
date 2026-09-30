import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import init, { BrowserArchive, decode_chunk } from './pkg/nudat_web.js';
import { planArchives, archiveEntries, mergeArchiveEntries } from './batch.js';
import { browse, buildTree, treeRows, filterTree, folderPaths } from './browser-model.js';
import { browserCapabilities } from './capabilities.js';
import { exportEntries, workerCount } from './export.js';
import { createWorker } from './node-worker.mjs';

let source, decodedProgress = [], readCalls = 0;
globalThis.nudatRead = (offset, length) => { readCalls++; return new Uint8Array(source.subarray(offset, offset + length)); };
globalThis.nudatDecoded = count => decodedProgress.push(count);
await init({ module_or_path: readFileSync(new URL('./pkg/nudat_web_bg.wasm', import.meta.url)) });
for (const format of ['pc', 'pc-legacy', 'android', 'obb']) {
  source = readFileSync(new URL(`../tests/fixtures/${format}.dat`, import.meta.url));
  const archive = new BrowserArchive(source.length);
  assert.equal(JSON.parse(archive.metadata()).entries.length, 3);
  assert.equal(Buffer.from(archive.extract('readme.txt')).toString(), 'hello from nudat\n');
  assert.equal(archive.extract('empty.bin').length, 0);
  decodedProgress = [];
  assert.equal(Buffer.from(archive.extract('nested/sample.gsc')).toString(), 'portable archive data\n'.repeat(2400));
  assert.ok(decodedProgress.some(n => n > 0 && n < 52800), 'Decode progress must advance within a file');
  assert.equal(decodedProgress.at(-1), 52800);
  for (const chunk of JSON.parse(archive.chunks('nested/sample.gsc'))) {
    readCalls = 0;
    const decoded = decode_chunk(source.length, chunk.offset, chunk.storedSize, chunk.size, chunk.compression);
    assert.equal(readCalls, 1, 'A batch should import compressed input in a single JS/WASM call');
    assert.equal(Buffer.from(decoded).toString(), 'portable archive data\n'.repeat(2400).slice(chunk.outputOffset, chunk.outputOffset + chunk.size));
  }
  assert.throws(() => archive.extract('missing'));
  archive.free();
  console.log(`${format}: WebAssembly decoding and incremental progress passed`);
}
source = Buffer.from('not an archive');
assert.throws(() => new BrowserArchive(source.length));
const batch = planArchives([
  { name: 'GAME.DAT', webkitRelativePath: 'games/pc/GAME.DAT', fixture: 'pc' },
  { name: 'GAME.DAT', webkitRelativePath: 'games/android/GAME.DAT', fixture: 'android' },
  { name: 'main.obb', fixture: 'obb' },
  { name: 'main.obb', fixture: 'pc-legacy' },
  { name: 'README.txt' },
]);
assert.equal(batch.length, 4);
assert.equal(planArchives([{ name: 'notes.txt' }]).length, 0);
const listing = [];
for (const item of batch) {
  source = readFileSync(new URL(`../tests/fixtures/${item.file.fixture}.dat`, import.meta.url));
  const archive = new BrowserArchive(source.length);
  listing.push(...archiveEntries(item, JSON.parse(archive.metadata()).entries));
  archive.free();
}
const merged = mergeArchiveEntries(listing);
assert.equal(merged.entries.length, 3);
assert.equal(merged.duplicates, 9);
assert.ok(merged.entries.every(e => e.archiveId === 3), 'Later archive wins duplicates');
assert.equal(browse(merged.entries)[0].name, 'nested');
assert.equal(browse(merged.entries, 'nested/')[0].name, 'sample.gsc');
assert.equal(browse(merged.entries, '', 'sample.gsc').length, 1);
assert.equal(browse(merged.entries, '', 'missing').length, 0);
const tree = buildTree(merged.entries);
assert.deepEqual(treeRows(tree, new Set()).map(row => row.name), ['nested', 'empty.bin', 'readme.txt']);
assert.deepEqual(treeRows(tree, new Set(['nested/'])).map(row => [row.name, row.depth]),
  [['nested', 0], ['sample.gsc', 1], ['empty.bin', 0], ['readme.txt', 0]]);
assert.deepEqual(tree.children[0].paths, ['nested/sample.gsc']);
assert.deepEqual(treeRows(filterTree(tree, 'sample'), folderPaths(filterTree(tree, 'sample')))
  .map(row => [row.name, row.depth]), [['nested', 0], ['sample.gsc', 1]]);
assert.deepEqual(treeRows(filterTree(tree, 'NESTED'), folderPaths(filterTree(tree, 'NESTED')))
  .map(row => row.name), ['nested', 'sample.gsc']);
assert.equal(treeRows(filterTree(tree, 'missing'), folderPaths(filterTree(tree, 'missing'))).length, 0);
const casing = mergeArchiveEntries([
  { path: 'A/first.txt', archiveId: 0 }, { path: 'a/second.txt', archiveId: 1 },
  { path: 'a/FIRST.TXT', originalPath: 'a/FIRST.TXT', archiveId: 2 },
]);
assert.deepEqual(casing.entries.map(e => e.path), ['A/first.txt', 'A/second.txt']);
assert.equal(casing.entries[0].originalPath, 'a/FIRST.TXT');
assert.throws(() => mergeArchiveEntries([{ path: 'a' }, { path: 'A/file' }]), /file and a folder/);
assert.throws(() => archiveEntries(batch[0], [{ path: '../escape.txt' }]), /unsafe/);
const mockDocument = { createElement: () => ({ webkitdirectory: false }) };
const supported = { isSecureContext: true, navigator: { serviceWorker: { register() {} } }, streamSaver: { createWriteStream() {} } };
assert.deepEqual(browserCapabilities(supported, mockDocument), { files: true, folders: true });
assert.deepEqual(browserCapabilities({ ...supported, navigator: { ...supported.navigator, userAgent: 'Mozilla/5.0 Firefox/143.0' } }, mockDocument), { files: true, folders: false });
assert.deepEqual(browserCapabilities({ ...supported, navigator: { ...supported.navigator, userAgent: 'FxiOS/143.0' } }, mockDocument), { files: true, folders: false });
assert.deepEqual(browserCapabilities(supported, { createElement: () => ({}) }), { files: true, folders: false });
assert.deepEqual(browserCapabilities({ ...supported, navigator: {} }, mockDocument), { files: false, folders: false });
assert.deepEqual(browserCapabilities({ ...supported, isSecureContext: false }, mockDocument), { files: false, folders: false });
assert.deepEqual(browserCapabilities({ ...supported, streamSaver: undefined }, mockDocument), { files: false, folders: false });
assert.deepEqual(browserCapabilities({ ...supported, navigator: { ...supported.navigator, userAgent: 'Firefox/143.0' }, streamSaver: undefined }, mockDocument), { files: true, folders: false });
assert.equal(workerCount([{ size: 1024 ** 3 }], 16), 6);
assert.equal(workerCount(merged.entries, 1), 1);
const workerArchives = batch.map(a => ({ ...a, file: { bytes: readFileSync(new URL(`../tests/fixtures/${a.file.fixture}.dat`, import.meta.url)) } }));
const events = [], output = new Map();
const memorySink = async entry => {
  const bytes = new Uint8Array(entry.size);
  return {
    async write(offset, chunk) { bytes.set(chunk, offset); },
    async close() { output.set(entry.archiveId + ':' + entry.path, bytes); },
    async abort() {},
  };
};
assert.equal(await exportEntries({ entries: listing, archives: workerArchives,
  signal: new AbortController().signal, createWorker, open: memorySink,
  onProgress: p => events.push(p),
}), 12);
for (const entry of listing) {
  const expected = entry.path === 'empty.bin' ? '' : entry.path === 'readme.txt' ? 'hello from nudat\n' : 'portable archive data\n'.repeat(2400);
  assert.equal(Buffer.from(output.get(entry.archiveId + ':' + entry.path)).toString(), expected);
}
const total = listing.reduce((n, e) => n + e.size, 0);
assert.equal(events.at(-1).decoded, total);
assert.equal(events.at(-1).written, total);
for (let i = 1; i < events.length; i++) {
  assert.ok(events[i].decoded >= events[i - 1].decoded);
  assert.ok(events[i].written >= events[i - 1].written);
}
for (const phase of ['open', 'write', 'close']) {
  let aborted = 0;
  await assert.rejects(exportEntries({ entries: merged.entries, archives: workerArchives,
    signal: new AbortController().signal, createWorker,
    open: async () => {
      if (phase === 'open') throw new Error('disk full');
      return { async write() { if (phase === 'write') throw new Error('disk full'); },
        async close() { if (phase === 'close') throw new Error('disk full'); }, async abort() { aborted++; } };
    },
  }), /disk full/);
  if (phase !== 'open') assert.ok(aborted > 0);
}

// One file larger than the old limit, with deliberately out-of-order decode
// completion. No complete-file allocation; retain at most the queued chunks.
const CHUNK = 4 * 1024 * 1024;
const large = { path: 'large.bin', originalPath: 'large.bin', archiveId: 0, size: 80 * CHUNK };
const chunks = Array.from({ length: 80 }, (_, i) => ({ outputOffset: i * CHUNK, size: CHUNK, offset: i * CHUNK, storedSize: CHUNK, compression: 0 }));
let workerId = 0;
const usedWorkers = new Set();
const decodedOffsets = [];
function fakeWorker() {
  const id = workerId++;
  let stopped = false;
  const worker = {
    terminate() { stopped = true; },
    postMessage(data) {
      const send = value => { if (!stopped) worker.onmessage({ data: { id: data.id, value } }); };
      if (data.type === 'chunk') {
        usedWorkers.add(id);
        setTimeout(() => {
          if (stopped) return;
          const bytes = new Uint8Array(data.chunk.size);
          bytes[0] = data.chunk.outputOffset / CHUNK;
          decodedOffsets.push(data.chunk.outputOffset);
          send(bytes);
        }, data.chunk.outputOffset === 0 ? 50 : 0);
      } else queueMicrotask(() => send(data.type === 'plan' ? chunks : true));
    },
  };
  return worker;
}
let activeWrites = 0, saved = 0, stats;
const positions = [];
await exportEntries({ entries: [large], archives: [{ file: {} }], cores: 5,
  signal: new AbortController().signal, createWorker: fakeWorker,
  open: async () => ({
    async write(offset, bytes) {
      assert.equal(++activeWrites, 1, 'Writes to one file must be serialized');
      assert.equal(bytes[0], offset / CHUNK);
      positions.push(offset);
      await new Promise(resolve => setTimeout(resolve, 2));
      activeWrites--;
    }, async close() { saved++; }, async abort() { assert.fail('Unexpected abort'); },
  }), onMetrics: value => { stats = value; },
});
assert.equal(saved, 1);
assert.equal(new Set(positions).size, 80);
assert.notEqual(decodedOffsets[0], 0, 'Must exercise out-of-order decode completion');
assert.deepEqual(positions, chunks.map(c => c.outputOffset), 'Browser writes must be sequential');
assert.equal(usedWorkers.size, 4, 'Multiple workers must decode the same file');
assert.ok(stats.peakPendingBytes <= 128 * 1024 * 1024);
assert.ok(stats.peakPendingChunks <= 24);
assert.equal(stats.decoded, large.size);
assert.equal(stats.written, large.size);

const controller = new AbortController();
let aborts = 0, closes = 0;
await assert.rejects(exportEntries({ entries: [large], archives: [{ file: {} }], cores: 5,
  signal: controller.signal, createWorker: fakeWorker,
  open: async () => ({ async write() { controller.abort(); }, async close() { closes++; }, async abort() { aborts++; } }),
}), /Cancelled/);
assert.equal(aborts, 1); assert.equal(closes, 0);
console.log('All formats, parallel chunks within one large file, positioned writes, bounded buffers, cancellation and disk errors passed');
