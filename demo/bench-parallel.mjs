// No extracted files are written. A small hash manifest supplies an independent
// whole-entry-decoder reference for checking every positioned parallel chunk.
// node demo/bench-parallel.mjs archive.dat manifest.json reference
// node demo/bench-parallel.mjs archive.dat manifest.json 1,2,4,6
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { WorkerClient, exportEntries } from './export.js';
import { createWorker } from './node-worker.mjs';

const [input, manifestPath, mode = '1,2,4,6'] = process.argv.slice(2);
if (!input || !manifestPath) throw new Error('Expected archive path and hash manifest path');
const file = { path: resolve(input) };
const stat = statSync(file.path);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
if (mode === 'reference') {
  const client = new WorkerClient(new URL('./worker.js', import.meta.url), createWorker);
  try {
    const { entries } = await client.call('open', { file });
    const reference = [];
    for (const entry of entries) {
      const chunks = await client.call('plan', { path: entry.path });
      const bytes = await client.call('extract', { path: entry.path });
      reference.push({ ...entry, chunks: chunks.map(c => ({ offset: c.outputOffset, size: c.size,
        hash: hash(bytes.subarray(c.outputOffset, c.outputOffset + c.size)) })) });
    }
    writeFileSync(manifestPath, JSON.stringify({ size: stat.size, mtimeMs: stat.mtimeMs, entries: reference }));
    console.log(`Reference hashes saved for ${entries.length} files; no extracted files written`);
  } finally { client.stop(); }
} else {
  const reference = JSON.parse(readFileSync(manifestPath));
  assert.equal(reference.size, stat.size); assert.equal(reference.mtimeMs, stat.mtimeMs);
  for (const workers of mode.split(',').map(Number)) {
    if (!Number.isInteger(workers) || workers < 1 || workers > 6) throw new Error('Expected 1–6 workers');
    let checked = 0;
    await exportEntries({ entries: reference.entries.map(e => ({ ...e, archiveId: 0, originalPath: e.path })),
      archives: [{ file }], cores: workers + 1, signal: new AbortController().signal, createWorker,
      open: async entry => {
        const expected = new Map(entry.chunks.filter(c => c.size > 0).map(c => [c.offset, c]));
        return {
          async write(offset, bytes) {
            const c = expected.get(offset);
            assert.ok(c, 'Unexpected/duplicate chunk offset');
            assert.equal(bytes.length, c.size); assert.equal(hash(bytes), c.hash);
            expected.delete(offset); checked++;
          },
          async close() { assert.equal(expected.size, 0, 'Missing chunks'); }, async abort() {},
        };
      }, onMetrics: m => console.log(JSON.stringify({ ...m, seconds: m.elapsedMs / 1000, checked })),
    });
  }
}
