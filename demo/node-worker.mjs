import { Worker } from 'node:worker_threads';

// Test/benchmark adapter. The website still uses native browser Workers.
export function createWorker(url) {
  const worker = new Worker(new URL('./test-worker.mjs', import.meta.url), { workerData: { module: url.href } });
  const adapter = { postMessage: (data, transfer) => worker.postMessage(data, transfer), terminate: () => worker.terminate() };
  worker.on('message', data => adapter.onmessage?.({ data }));
  worker.on('error', error => adapter.onerror?.(error));
  return adapter;
}
