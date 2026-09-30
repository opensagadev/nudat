// Decode an archive through the browser WASM adapter without writing output.
// Usage: node demo/bench-decoder.mjs archive.dat [path/to/nudat_web.js]
import { openSync, closeSync, fstatSync, readSync, readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

if (!process.argv[2]) throw new Error('Expected an archive path');
const moduleUrl = process.argv[3] ? pathToFileURL(resolve(process.argv[3])) : new URL('./pkg/nudat_web.js', import.meta.url);
const { default: init, BrowserArchive } = await import(moduleUrl);
const fd = openSync(process.argv[2], 'r');
let cache = Buffer.alloc(0), cacheStart = 0;
globalThis.nudatRead = (offset, length) => {
  if (offset < cacheStart || offset + length > cacheStart + cache.length) {
    cacheStart = offset;
    const buffer = Buffer.allocUnsafe(Math.max(length, 65536));
    cache = buffer.subarray(0, readSync(fd, buffer, 0, buffer.length, offset));
  }
  return cache.subarray(offset - cacheStart, offset - cacheStart + length);
};
globalThis.nudatDecoded = () => {};
let archive;
try {
  await init({ module_or_path: readFileSync(new URL('nudat_web_bg.wasm', moduleUrl)) });
  archive = new BrowserArchive(fstatSync(fd).size);
  const entries = JSON.parse(archive.metadata()).entries.sort((a, b) => a.offset - b.offset);
  const hash = createHash('sha256');
  let bytes = 0;
  const start = performance.now();
  for (const entry of entries) {
    const data = archive.extract(entry.path);
    hash.update(data); bytes += data.length;
  }
  const seconds = (performance.now() - start) / 1000;
  console.log(JSON.stringify({ files: entries.length, bytes, seconds, mibPerSecond: bytes / 1048576 / seconds, sha256: hash.digest('hex') }));
} finally { archive?.free(); closeSync(fd); }
