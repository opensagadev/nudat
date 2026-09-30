// Firefox starts the service-worker download through a normal same-origin link.
// One chunk is sent only when the worker's response stream asks for it.
export async function firefoxDownload(filename, length, { window, startupTimeoutMs, writeTimeoutMs }) {
  const scope = new URL('./third-party/streamsaver/', import.meta.url).href;
  const registration = await window.navigator.serviceWorker.register(new URL('sw.js?v=4', scope), { scope });
  const worker = registration.installing || registration.waiting || registration.active;
  if (!worker) throw new Error('Download worker did not start. Reload and try again.');
  if (worker.state !== 'activated') await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { worker.removeEventListener('statechange', changed); reject(new Error('Download worker did not activate. Reload and try again.')); }, startupTimeoutMs);
    const changed = () => {
      if (worker.state === 'activated') { clearTimeout(timer); worker.removeEventListener('statechange', changed); resolve(); }
      else if (worker.state === 'redundant') { clearTimeout(timer); worker.removeEventListener('statechange', changed); reject(new Error('Download worker failed to activate.')); }
    };
    worker.addEventListener('statechange', changed);
  });

  const channel = new MessageChannel();
  const port = channel.port1;
  let start, failStart, finish, ready = false, waiting, closed = false;
  const started = new Promise((resolve, reject) => { start = resolve; failStart = reject; });
  const finished = new Promise(resolve => { finish = resolve; });
  const timeout = (promise, ms, message) => {
    let timer;
    return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), ms); })])
      .finally(() => clearTimeout(timer));
  };
  const cleanup = () => { clearInterval(heartbeat); port.close(); closed = true; };
  port.onmessage = ({ data }) => {
    if (data.download) {
      try {
        const link = window.document.createElement('a');
        link.href = data.download; link.download = filename; link.hidden = true;
        window.document.body.append(link); link.click(); link.remove();
      } catch (error) { failStart(error); }
    } else if (data.debug === 'Download started') start();
    else if (data.ready) { ready = true; if (waiting) { waiting(); waiting = null; } }
    else if (data.done) finish();
    else if (data.abort) failStart(new Error('The browser cancelled the download.'));
  };
  port.start?.();
  const heartbeat = setInterval(() => worker.postMessage('ping'), 10000);
  const url = new URL(`${Math.random().toString(36).slice(2)}/${encodeURIComponent(filename)}`, scope).href;
  try {
    worker.postMessage({ url, backpressure: true, headers: {
      'Content-Length': String(length), 'Content-Disposition': `attachment; filename*=UTF-8''${encodeURIComponent(filename)}`,
    } }, [channel.port2]);
  } catch (error) { cleanup(); throw error; }
  try {
    await timeout(started, startupTimeoutMs, 'Firefox did not start the ZIP download. Reload the page and try again.');
  } catch (error) { port.postMessage('abort'); cleanup(); throw error; }
  return {
    async write(bytes) {
      if (closed) throw new Error('Download stream is closed.');
      if (!ready) await timeout(new Promise(resolve => { waiting = resolve; }), writeTimeoutMs,
        'Firefox stopped accepting ZIP data. Reload the page and try again.');
      ready = false;
      port.postMessage(bytes);
    },
    async close() {
      if (closed) return;
      port.postMessage('end');
      try { await timeout(finished, writeTimeoutMs, 'Firefox did not finish the ZIP download.'); }
      finally { cleanup(); }
    },
    async abort() { if (!closed) { port.postMessage('abort'); cleanup(); } },
  };
}
