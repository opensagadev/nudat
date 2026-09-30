import assert from 'node:assert/strict';
import { exportEntries } from './export.js';
import { exportProgress } from './progress.js';

let previous = 0;
for (const p of [
  { decoded: 0, written: 0, completed: 0, committedBytes: 0 },
  { decoded: 100, written: 0, completed: 0, committedBytes: 0 },
  { decoded: 100, written: 100, completed: 0, committedBytes: 0 },
  { decoded: 100, written: 100, completed: 1, committedBytes: 50 },
  { decoded: 100, written: 100, completed: 2, committedBytes: 100 },
]) {
  const value = exportProgress({ total: 100, ...p }, 2);
  assert.ok(value >= previous); previous = value;
}
assert.equal(previous, 0.99);
assert.equal(exportProgress({ total: 0, completed: 1 }, 2), 0.495);
assert.equal(exportProgress({ total: 0, completed: 2 }, 2), 0.99);
assert.equal(exportProgress({ total: 1000, decoded: 1000, written: 0, completed: 0 }, 100), 0.5);
assert.equal(exportProgress({ total: 1000, decoded: 1000, written: 1000, completed: 0 }, 100), 0.99);

function workerFor(plans) {
  return () => {
    let stopped = false;
    const worker = {
      terminate() { stopped = true; },
      postMessage(data) {
        queueMicrotask(() => {
          if (stopped) return;
          const value = data.type === 'plan' ? plans.get(data.path) : data.type === 'chunk' ? new Uint8Array(data.chunk.size) : true;
          worker.onmessage({ data: { id: data.id, value } });
        });
      },
    };
    return worker;
  };
}
const plan = n => Array.from({ length: n }, (_, i) => ({ outputOffset: i * 4, size: 4 }));
const entry = (path, n) => ({ path, originalPath: path, archiveId: 0, size: n * 4 });

// A stalled large file must not consume every buffer and prevent another file
// from reaching its own writer. The other file is what opens this gate.
let unblock;
const gate = new Promise(resolve => { unblock = resolve; });
const abort = new AbortController();
const timer = setTimeout(() => { abort.abort(); unblock(); }, 5000);
let writes = 0;
try {
  await exportEntries({ entries: [entry('large', 48), entry('other', 2)], archives: [{ file: {} }],
    signal: abort.signal, cores: 5, createWorker: workerFor(new Map([['large', plan(48)], ['other', plan(2)]])),
    open: async e => ({
      async write() { if (e.path === 'large') await gate; else unblock(); writes++; },
      async close() {}, async abort() {},
    }),
  });
} finally { clearTimeout(timer); }
assert.equal(writes, 50);

// Closing files must release decode buffers and active-writer slots. Hold all
// closes until 32 have started: the old 24-file/24-buffer coupling deadlocks here.
let releaseCloses, active = 0, peak = 0, closed = 0, stats;
const closeGate = new Promise(resolve => { releaseCloses = resolve; });
const closeAbort = new AbortController();
const closeTimer = setTimeout(() => { closeAbort.abort(); releaseCloses(); }, 5000);
const entries = Array.from({ length: 60 }, (_, i) => entry('file-' + i, 1));
try {
  assert.equal(await exportEntries({ entries, archives: [{ file: {} }], cores: 5,
    signal: closeAbort.signal, createWorker: workerFor(new Map(entries.map(e => [e.path, plan(1)]))),
    open: async () => ({ async write() {}, async close() {
      peak = Math.max(peak, ++active);
      if (active === 32) releaseCloses();
      await closeGate;
      active--; closed++;
    }, async abort() { assert.fail('Unexpected abort'); } }),
    onMetrics: m => { stats = m; },
  }), 60);
} finally { clearTimeout(closeTimer); }
assert.equal(peak, 32); assert.equal(active, 0); assert.equal(closed, 60);
assert.ok(stats.peakOpenFiles <= 56);
assert.ok(stats.peakPendingChunks <= 24);
assert.equal(stats.peakClosingFiles, 32);
console.log('Saving fairness and 32 independent concurrent closes passed; no early completion');
