// Streaming, uncompressed ZIP writer. Only the central-directory records stay in memory.
const encoder = new TextEncoder();
const table = Uint32Array.from({ length: 256 }, (_, n) => {
  for (let i = 0; i < 8; i++) n = n & 1 ? 0xedb88320 ^ (n >>> 1) : n >>> 1;
  return n >>> 0;
});
const LIMIT = 0xffffffffn;
const u16 = (view, at, value) => view.setUint16(at, value, true);
const u32 = (view, at, value) => view.setUint32(at, Number(value), true);
const u64 = (view, at, value) => view.setBigUint64(at, BigInt(value), true);
const record = length => { const bytes = new Uint8Array(length); return [bytes, new DataView(bytes.buffer)]; };

export function zipLength(entries) {
  let offset = 0n, centralSize = 0n;
  for (const entry of entries) {
    const size = BigInt(entry.size), nameLength = encoder.encode(entry.path).length;
    if (!Number.isSafeInteger(entry.size) || entry.size < 0 || !nameLength || nameLength > 65535) throw new Error('Invalid ZIP entry');
    const large = size >= LIMIT, largeOffset = offset >= LIMIT;
    centralSize += BigInt(46 + nameLength + ((large || largeOffset) ? 4 + (large ? 16 : 0) + (largeOffset ? 8 : 0) : 0));
    offset += BigInt(30 + nameLength + (large ? 20 : 0) + (large ? 24 : 16)) + size;
  }
  const zip64 = entries.length >= 65535 || offset >= LIMIT || centralSize >= LIMIT;
  return offset + centralSize + (zip64 ? 76n : 0n) + 22n;
}

export class ZipWriter {
  constructor(stream) { this.stream = stream; this.offset = 0n; this.files = []; this.active = false; this.closed = false; }

  async put(bytes) {
    await this.stream.write(bytes);
    this.offset += BigInt(bytes.length);
  }

  async open(entry) {
    if (this.active || this.closed) throw new Error('ZIP entry overlap');
    if (!Number.isSafeInteger(entry.size) || entry.size < 0) throw new Error('Invalid ZIP entry size');
    const name = encoder.encode(entry.path);
    if (!name.length || name.length > 65535) throw new Error('ZIP path is too long: ' + entry.path);
    const size = BigInt(entry.size), large = size >= LIMIT;
    const offset = this.offset;
    const extraLength = large ? 20 : 0;
    const [header, view] = record(30 + name.length + extraLength);
    u32(view, 0, 0x04034b50); u16(view, 4, large ? 45 : 20);
    u16(view, 6, 0x0808); // UTF-8 and data descriptor
    u16(view, 8, 0); // Store: no compression
    u32(view, 18, large ? LIMIT : size); u32(view, 22, large ? LIMIT : size);
    u16(view, 26, name.length); u16(view, 28, extraLength);
    header.set(name, 30);
    if (large) {
      u16(view, 30 + name.length, 0x0001); u16(view, 32 + name.length, 16);
      u64(view, 34 + name.length, size); u64(view, 42 + name.length, size);
    }
    this.active = true;
    try { await this.put(header); } catch (error) { this.active = false; throw error; }
    let crc = 0xffffffff, written = 0n, done = false;
    return {
      write: async (position, bytes) => {
        if (done || !this.active || BigInt(position) !== written || written + BigInt(bytes.length) > size) throw new Error('Invalid ZIP entry write: ' + entry.path);
        for (const byte of bytes) crc = table[(crc ^ byte) & 255] ^ (crc >>> 8);
        await this.put(bytes); written += BigInt(bytes.length);
      },
      close: async () => {
        if (done || written !== size) throw new Error('Incomplete ZIP entry: ' + entry.path);
        const [descriptor, descriptorView] = record(large ? 24 : 16);
        u32(descriptorView, 0, 0x08074b50); u32(descriptorView, 4, (crc ^ 0xffffffff) >>> 0);
        if (large) { u64(descriptorView, 8, size); u64(descriptorView, 16, size); }
        else { u32(descriptorView, 8, size); u32(descriptorView, 12, size); }
        await this.put(descriptor);
        this.files.push({ name, size, offset, crc: (crc ^ 0xffffffff) >>> 0, large });
        done = true; this.active = false;
      },
      abort: async () => { done = true; this.active = false; },
    };
  }

  async close() {
    if (this.active || this.closed) throw new Error('ZIP is still being written');
    const centralStart = this.offset;
    for (const file of this.files) {
      const largeOffset = file.offset >= LIMIT;
      const extraLength = (file.large || largeOffset) ? 4 + (file.large ? 16 : 0) + (largeOffset ? 8 : 0) : 0;
      const [header, view] = record(46 + file.name.length + extraLength);
      u32(view, 0, 0x02014b50); u16(view, 4, 45); u16(view, 6, file.large || largeOffset ? 45 : 20);
      u16(view, 8, 0x0808); u16(view, 10, 0);
      u32(view, 16, file.crc); u32(view, 20, file.large ? LIMIT : file.size); u32(view, 24, file.large ? LIMIT : file.size);
      u16(view, 28, file.name.length); u16(view, 30, extraLength);
      u32(view, 42, largeOffset ? LIMIT : file.offset);
      header.set(file.name, 46);
      if (extraLength) {
        const at = 46 + file.name.length;
        u16(view, at, 0x0001); u16(view, at + 2, extraLength - 4);
        let cursor = at + 4;
        if (file.large) { u64(view, cursor, file.size); u64(view, cursor + 8, file.size); cursor += 16; }
        if (largeOffset) u64(view, cursor, file.offset);
      }
      await this.put(header);
    }
    const centralSize = this.offset - centralStart;
    const zip64 = this.files.length >= 65535 || centralStart >= LIMIT || centralSize >= LIMIT;
    if (zip64) {
      const zip64Offset = this.offset;
      const [end64, endView] = record(56);
      u32(endView, 0, 0x06064b50); u64(endView, 4, 44);
      u16(endView, 12, 45); u16(endView, 14, 45);
      u64(endView, 24, this.files.length); u64(endView, 32, this.files.length);
      u64(endView, 40, centralSize); u64(endView, 48, centralStart);
      await this.put(end64);
      const [locator, locatorView] = record(20);
      u32(locatorView, 0, 0x07064b50); u64(locatorView, 8, zip64Offset); u32(locatorView, 16, 1);
      await this.put(locator);
    }
    const [end, endView] = record(22);
    u32(endView, 0, 0x06054b50);
    u16(endView, 8, zip64 ? 65535 : this.files.length); u16(endView, 10, zip64 ? 65535 : this.files.length);
    u32(endView, 12, zip64 ? LIMIT : centralSize); u32(endView, 16, zip64 ? LIMIT : centralStart);
    await this.put(end);
    await this.stream.close(); this.closed = true;
  }

  async abort() { if (!this.closed) { this.closed = true; await this.stream.abort(); } }
}
