import assert from 'node:assert/strict';
import { ZipWriter, zipLength } from './zip.js';

const parts = [];
let closed = 0, aborted = 0;
const zip = new ZipWriter({ async write(bytes) { parts.push(bytes.slice()); }, async close() { closed++; }, async abort() { aborted++; } });
const samples = [['config.cfg', 'setting=1'], ['plugin.dll', 'binary'], ['nested/empty', '']];
for (const [path, contents] of samples) {
  const bytes = new TextEncoder().encode(contents);
  const sink = await zip.open({ path, size: bytes.length });
  await sink.write(0, bytes.subarray(0, 2));
  await sink.write(Math.min(2, bytes.length), bytes.subarray(2));
  await sink.close();
}
await zip.close();
assert.equal(closed, 1); assert.equal(aborted, 0);
const bytes = Buffer.concat(parts.map(part => Buffer.from(part)));
assert.equal(BigInt(bytes.length), zipLength(samples.map(([path, contents]) => ({ path, size: new TextEncoder().encode(contents).length }))));
const end = bytes.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
assert.equal(bytes.readUInt16LE(end + 10), 3);
const central = bytes.readUInt32LE(end + 16);
const names = [];
let offset = central;
for (let i = 0; i < 3; i++) {
  assert.equal(bytes.readUInt32LE(offset), 0x02014b50);
  assert.equal(bytes.readUInt16LE(offset + 10), 0, 'ZIP entries must be uncompressed');
  const nameLength = bytes.readUInt16LE(offset + 28), extra = bytes.readUInt16LE(offset + 30);
  names.push(bytes.toString('utf8', offset + 46, offset + 46 + nameLength));
  offset += 46 + nameLength + extra + bytes.readUInt16LE(offset + 32);
}
assert.deepEqual(names, ['config.cfg', 'plugin.dll', 'nested/empty']);
assert.equal(offset, end);
const broken = new ZipWriter({ async write() {}, async close() {}, async abort() { aborted++; } });
const sink = await broken.open({ path: 'incomplete', size: 10 });
await assert.rejects(sink.close(), /Incomplete ZIP entry/);
await broken.abort();
assert.equal(aborted, 1);
console.log('Streaming stored ZIP, original names, central directory and abort passed');
