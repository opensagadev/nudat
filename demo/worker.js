import init, { BrowserArchive, decode_chunk } from './pkg/nudat_web.js';

let file, archive, cache = new Uint8Array(), cacheStart = 0;
const reader = new FileReaderSync();
let requestId, lastProgress = 0;
globalThis.nudatDecoded = bytes => {
  const now = performance.now();
  if (now - lastProgress >= 60 || lastProgress === 0) {
    lastProgress = now;
    self.postMessage({ id: requestId, progress: bytes });
  }
};
globalThis.nudatRead = (offset, length) => {
  if (offset < cacheStart || offset + length > cacheStart + cache.length) {
    cacheStart = offset;
    cache = new Uint8Array(reader.readAsArrayBuffer(file.slice(offset, offset + Math.max(length, 262144))));
  }
  // wasm-bindgen copies this view into WASM memory; avoid a redundant JS copy.
  return cache.subarray(offset - cacheStart, offset - cacheStart + length);
};
const ready = init();
self.onmessage = async ({ data }) => {
  try {
    await ready;
    if (data.type === 'open' || data.type === 'source') {
      archive?.free();
      archive = null;
      file = data.file;
      cache = new Uint8Array();
      if (data.type === 'open') archive = new BrowserArchive(file.size);
      self.postMessage({ id: data.id, value: !archive || data.metadata === false ? true : JSON.parse(archive.metadata()) });
    } else if (data.type === 'plan') {
      self.postMessage({ id: data.id, value: JSON.parse(archive.chunks(data.path)) });
    } else if (data.type === 'extract' || data.type === 'chunk') {
      requestId = data.id; lastProgress = 0;
      const c = data.chunk;
      let bytes;
      if (c?.compression === 0) {
        if (!Number.isSafeInteger(c.offset) || c.offset < 0 || !Number.isSafeInteger(c.size) || c.size < 0 ||
            c.size > 4 * 1024 * 1024 || c.storedSize !== c.size || c.offset + c.size > file.size) throw new Error('Invalid raw chunk');
        // Raw entries need no decoder or WASM heap copy: transfer the file read.
        bytes = new Uint8Array(reader.readAsArrayBuffer(file.slice(c.offset, c.offset + c.size)));
        if (bytes.length !== c.size) throw new Error('Truncated raw chunk');
      } else {
        bytes = c ? decode_chunk(file.size, c.offset, c.storedSize, c.size, c.compression) : archive.extract(data.path);
      }
      self.postMessage({ id: data.id, progress: bytes.length });
      self.postMessage({ id: data.id, value: bytes }, [bytes.buffer]);
    }
  } catch (error) {
    self.postMessage({ id: data.id, error: String(error) });
  }
};
