// StreamSaver 2.0.6 is vendored locally, including its MITM page and service worker.
// Its worker runs in a separate scope, so the unpacker page need not be controlled.
import { firefoxDownload } from './firefox-download.js';
export async function nativeDownload(filename, length, { window = globalThis.window, startupTimeoutMs = 15000, writeTimeoutMs = 30000 } = {}) {
  if (length > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error('ZIP is too large for the browser download API.');
  if (/Firefox\/|FxiOS\//i.test(window.navigator?.userAgent || ''))
    return firefoxDownload(filename, length, { window, startupTimeoutMs, writeTimeoutMs });
  if (!window.streamSaver?.createWriteStream) throw new Error('Download helper did not load. Reload and try again.');
  window.streamSaver.mitm = new URL('./third-party/streamsaver/mitm.html', import.meta.url).href;
  let started;
  const ready = new Promise(resolve => { started = resolve; });
  const stream = window.streamSaver.createWriteStream(filename, {
    size: Number(length), onDownloadStart: started,
  });
  const writer = stream.getWriter();
  let timer;
  try {
    await Promise.race([
      ready,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('The browser did not start the ZIP download. Reload the page and try again.')), startupTimeoutMs); }),
    ]);
  } catch (error) {
    writer.abort().catch(() => {});
    throw error;
  } finally { clearTimeout(timer); }
  const bounded = (operation, timeout, message) => {
    let timer;
    return Promise.race([
      operation,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), timeout); }),
    ]).finally(() => clearTimeout(timer));
  };
  return {
    write: bytes => bounded(writer.write(bytes), writeTimeoutMs, 'The browser stopped accepting ZIP data. Reload the page and try again.'),
    close: () => bounded(writer.close(), writeTimeoutMs, 'The browser did not finish the ZIP download. Reload the page and try again.'),
    abort: () => bounded(writer.abort(), 1000, 'Download abort timed out.'),
  };
}
