// Node adapter for testing the actual browser worker modules without browser automation.
import { parentPort, workerData } from 'node:worker_threads';
import { readFile } from 'node:fs/promises';
import { openSync, closeSync, readSync, fstatSync } from 'node:fs';

globalThis.self = globalThis;
globalThis.postMessage = (data, transfer) => parentPort.postMessage(data, transfer);
globalThis.FileReaderSync = class {
  readAsArrayBuffer(bytes) {
    if (bytes.fd !== undefined) {
      const buffer = Buffer.allocUnsafe(Math.max(0, bytes.end - bytes.start));
      const length = readSync(bytes.fd, buffer, 0, buffer.length, bytes.start);
      return buffer.buffer.slice(buffer.byteOffset, buffer.byteOffset + length);
    }
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }
};
globalThis.fetch = async url => new Response(await readFile(url), { headers: { 'Content-Type': 'application/wasm' } });
await import(workerData.module);
let fd;
parentPort.on('message', data => {
  if (data.type === 'open' || data.type === 'source') {
    if (fd !== undefined) { closeSync(fd); fd = undefined; }
    if (data.file.path) {
      fd = openSync(data.file.path, 'r');
      const size = fstatSync(fd).size;
      data.file = { size, slice: (start, end) => ({ fd, start, end: Math.min(end, size) }) };
    } else {
      const bytes = data.file.bytes;
      data.file = { size: bytes.length, slice: (start, end) => bytes.subarray(start, end) };
    }
  }
  self.onmessage({ data });
});
