export class WorkerClient {
  constructor(url, createWorker = url => new Worker(url, { type: 'module' })) {
    this.worker = createWorker(url);
    this.pending = new Map(); this.sequence = 0;
    this.worker.onmessage = ({ data }) => {
      const request = this.pending.get(data.id);
      if (!request) return;
      if (data.progress !== undefined) { request.onProgress?.(data.progress); return; }
      this.pending.delete(data.id);
      data.error ? request.reject(new Error(data.error)) : request.resolve(data.value);
    };
    this.worker.onerror = event => this.stop(new Error(event.message || 'Export worker failed'));
  }
  call(type, fields = {}, transfer = [], onProgress) {
    if (this.error) return Promise.reject(this.error);
    return new Promise((resolve, reject) => {
      const id = ++this.sequence;
      this.pending.set(id, { resolve, reject, onProgress });
      this.worker.postMessage({ id, type, ...fields }, transfer);
    });
  }
  stop(error = new Error('Cancelled')) {
    if (this.error) return;
    this.error = error;
    this.worker.terminate();
    for (const request of this.pending.values()) request.reject(error);
    this.pending.clear();
  }
}

export function workerCount(entries, cores = 4) {
  return entries.length ? Math.max(1, Math.min(6, (cores || 2) - 1)) : 1;
}

export async function exportEntries({ entries, archives, open, signal, onProgress = () => {}, onMetrics = () => {}, createWorker, cores, maxActiveFiles = 24 }) {
  const started = performance.now();
  const metrics = { archiveOpens: 0, indexingMs: 0, decodingMs: 0, writingMs: 0, closingMs: 0,
    slowestCloseMs: 0, slowestClosePath: '', peakClosingFiles: 0, peakPendingChunks: 0,
    peakPendingBytes: 0, peakOpenFiles: 0 };
  const makeClient = () => new WorkerClient(new URL('./worker.js', import.meta.url), createWorker);
  const planner = makeClient(), clients = Array.from({ length: workerCount(entries, cores) }, makeClient);
  const states = new Set(), saves = new Set(), waiters = new Set();
  const ready = [];
  let producerDone = false, failure, closingFiles = 0, lastDecodedAt = started;
  let pendingChunks = 0, pendingBytes = 0, completed = 0, decoded = 0, written = 0, committedBytes = 0;
  const total = entries.reduce((sum, e) => sum + e.size, 0);
  const wake = () => { for (const resolve of waiters) resolve(); waiters.clear(); };
  const wait = () => new Promise(resolve => waiters.add(resolve));
  const stopped = () => failure || signal.aborted;
  const stop = () => { planner.stop(); clients.forEach(c => c.stop()); wake(); };
  const fail = error => { failure ||= error; stop(); };
  const check = () => { if (stopped()) throw failure || new Error('Cancelled'); };
  const progress = (stage, entry) => onProgress({ completed, decoded, written, committedBytes, total, closingFiles, stage, entry });
  signal.addEventListener('abort', stop, { once: true });

  async function produce() {
    let archiveId;
    try {
      for (const entry of [...entries].sort((a, b) => a.archiveId - b.archiveId || (a.offset ?? 0) - (b.offset ?? 0))) {
        while ((maxActiveFiles === 1 ? states.size : states.size - closingFiles) >= maxActiveFiles && !stopped()) await wait();
        check();
        const at = performance.now();
        progress('Planning blocks', entry);
        if (archiveId !== entry.archiveId) {
          await planner.call('open', { file: archives[entry.archiveId].file, metadata: false });
          archiveId = entry.archiveId; metrics.archiveOpens++;
        }
        const chunks = await planner.call('plan', { path: entry.originalPath });
        metrics.indexingMs += performance.now() - at;
        check();
        let end = 0;
        for (const c of chunks) {
          if (c.outputOffset !== end || !Number.isSafeInteger(c.size) || c.size < 0 || c.size > 4 * 1024 * 1024) throw new Error('Invalid block plan');
          end += c.size;
        }
        if (!chunks.length || end !== entry.size) throw new Error('Block plan size mismatch');
        const state = { entry, chunks, next: 0, pending: 0, remaining: chunks.length,
          nextWrite: 0, nextWriteIndex: 0, ready: new Map(), flushing: false };
        // File creation overlaps both planning and decoding. Its rejection is
        // handled immediately, including when no decode has completed yet.
        state.sink = Promise.resolve().then(() => { check(); return open(entry); });
        state.sink.catch(fail);
        states.add(state);
        metrics.peakOpenFiles = Math.max(metrics.peakOpenFiles, states.size);
        ready.push(state);
        wake();
      }
    } catch (error) { fail(error); }
    finally { producerDone = true; wake(); }
  }

  async function take() {
    for (;;) {
      if (stopped()) return;
      if (!ready.length && producerDone) return;
      // Rotate among files instead of filling the entire buffer budget from
      // one large file whose writes must run serially. A lone file still feeds
      // every decoder; multiple files get independent disk-write pipelines.
      const perFile = Math.max(clients.length, Math.ceil(24 / Math.max(1, states.size - closingFiles)));
      const index = ready.findIndex(state => state.pending < perFile &&
        pendingBytes + state.chunks[state.next].size <= 128 * 1024 * 1024);
      if (index >= 0 && pendingChunks < 24) {
        const [state] = ready.splice(index, 1);
        const indexInFile = state.next++;
        const chunk = state.chunks[indexInFile];
        if (state.next < state.chunks.length) ready.push(state);
        state.pending++;
        pendingChunks++; pendingBytes += chunk.size;
        metrics.peakPendingChunks = Math.max(metrics.peakPendingChunks, pendingChunks);
        metrics.peakPendingBytes = Math.max(metrics.peakPendingBytes, pendingBytes);
        return { state, chunk, indexInFile };
      }
      await wait();
    }
  }
  function release(state, chunk) { state.pending--; pendingChunks--; pendingBytes -= chunk.size; wake(); }

  function finish(state, sink) {
    // Browser commit/scan latency must not reserve a decoded buffer or prevent
    // other files from being opened. Keep these operations separately bounded.
    const closing = (async () => {
      while (closingFiles >= 32 && !stopped()) await wait();
      check();
      closingFiles++;
      metrics.peakClosingFiles = Math.max(metrics.peakClosingFiles, closingFiles);
      progress('Finishing file', state.entry); wake();
      const at = performance.now();
      try {
        await sink.close();
        states.delete(state); completed++; committedBytes += state.entry.size;
      } finally {
        const elapsed = performance.now() - at;
        metrics.closingMs += elapsed;
        if (elapsed > metrics.slowestCloseMs) {
          metrics.slowestCloseMs = elapsed;
          metrics.slowestClosePath = state.entry.path;
        }
        closingFiles--; wake();
      }
      progress('Saved', state.entry);
    })().catch(fail).finally(() => saves.delete(closing));
    saves.add(closing);
  }

  function flush(state) {
    if (state.flushing || !state.ready.has(state.nextWriteIndex) || stopped()) return;
    state.flushing = true;
    const saving = (async () => {
      const sink = await state.sink;
      while (state.ready.has(state.nextWriteIndex)) {
        check();
        const { chunk, bytes } = state.ready.get(state.nextWriteIndex);
        state.ready.delete(state.nextWriteIndex);
        if (chunk.outputOffset !== state.nextWrite) throw new Error('Out-of-order output range');
        const at = performance.now();
        try {
          if (bytes.length) await sink.write(chunk.outputOffset, bytes);
          check();
          written += bytes.length;
          state.nextWrite += bytes.length;
          state.nextWriteIndex++;
          progress('Writing', state.entry);
          if (--state.remaining === 0) finish(state, sink);
        } finally {
          metrics.writingMs += performance.now() - at;
          release(state, chunk);
        }
      }
    })().catch(fail).finally(() => {
      state.flushing = false;
      saves.delete(saving);
      if (!stopped()) flush(state);
    });
    saves.add(saving);
  }

  async function consume(client) {
    let archiveId;
    for (;;) {
      const job = await take();
      if (!job) return;
      const { state, chunk, indexInFile } = job, { entry } = state;
      let handedToWriter = false;
      try {
        check();
        if (archiveId !== entry.archiveId) {
          await client.call('source', { file: archives[entry.archiveId].file });
          archiveId = entry.archiveId;
        }
        let chunkDecoded = 0;
        const at = performance.now();
        const bytes = await client.call('chunk', { chunk }, [], value => {
          const next = Math.max(chunkDecoded, Math.min(chunk.size, value));
          decoded += next - chunkDecoded; chunkDecoded = next;
          progress('Decoding', entry);
        });
        metrics.decodingMs += performance.now() - at;
        lastDecodedAt = performance.now();
        check();
        if (bytes.length !== chunk.size) throw new Error('Decoded chunk size mismatch');
        decoded += chunk.size - chunkDecoded;
        // Decode may finish out of order. Hold later chunks briefly so each
        // browser stream receives only sequential writes from byte zero.
        state.ready.set(indexInFile, { chunk, bytes });
        handedToWriter = true;
        flush(state);
      } catch (error) { fail(error); }
      finally { if (!handedToWriter) release(state, chunk); }
    }
  }
  try {
    check();
    await Promise.all([produce(), ...clients.map(consume)]);
    // A final write can enqueue a close while we're waiting for this set.
    while (saves.size) await Promise.all(saves);
    check();
    return completed;
  } finally {
    signal.removeEventListener('abort', stop); stop();
    // Await any opening handle, then abort each unfinished file exactly once.
    await Promise.all([...states].map(async state => {
      for (const { chunk } of state.ready.values()) release(state, chunk);
      state.ready.clear();
      try { const sink = await state.sink; await sink.abort(); } catch { /* preserve original failure */ }
    }));
    onMetrics({ ...metrics, workers: clients.length, savingTailMs: performance.now() - lastDecodedAt, elapsedMs: performance.now() - started, completed, decoded, written });
  }
}
