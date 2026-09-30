// DOM/API simulation only: no browser automation or computer-use tools.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const html = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
const template = readFileSync(new URL('./index.template.html', import.meta.url), 'utf8');
assert.doesNotMatch(html, /Try a sample|id="sample"/);
assert.doesNotMatch(html, /Open files or a folder, then download one ZIP|Files from all archives share one tree|Ready to download/);
assert.match(html, /<main class="site-content">/);
assert.match(html, /<div class="site-container"><div class="file-browser">/);
assert.match(html, /<section id="activity" class="site-container"/);
assert.match(html, /Select one or more DAT or OBB archives\./);
assert.match(html, /Find DAT and OBB archives in the folder and its subfolders\./);
assert.doesNotMatch(html, /id="breadcrumbs"/);
assert.match(html, /<header class="opensaga-header">/);
assert.match(html, /<footer class="site-footer">/);
assert.match(template, /<!--__HEAD__-->/);
assert.match(template, /<!--__HEADER__-->/);
assert.match(template, /<!--__FOOTER__-->/);
assert.doesNotMatch(template, /<header|<footer/);
assert.match(html, /third-party\/streamsaver\/StreamSaver\.js/);
const mitm = readFileSync(new URL('./third-party/streamsaver/mitm.html', import.meta.url), 'utf8');
assert.match(mitm, /navigator\.serviceWorker\.register\('sw\.js\?v=3', \{ scope: '\.\/' \}\)/);
assert.doesNotMatch(mitm, /return navigator\.serviceWorker\.getRegistration/, 'A root worker must not be reused for downloads');
class Element {
  constructor(tag = '') {
    this.tag = tag; this.children = []; this.hidden = false; this.value = ''; this.checked = false;
    this.classList = { add() {}, remove() {}, toggle() {} };
    this.style = { setProperty() {} };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  setAttribute(name, value) { this[name] = value; }
  removeAttribute(name) { delete this[name]; }
  addEventListener() {}
  querySelectorAll(selector) {
    return this.children.flatMap(child => child instanceof Element
      ? [...(selector.split(',').includes(child.tag) ? [child] : []), ...child.querySelectorAll(selector)] : []);
  }
}
for (const { name, supported, firefox, folders } of [
  { name: 'chromium', supported: true, firefox: false, folders: true },
  { name: 'firefox', supported: true, firefox: true, folders: false },
  { name: 'unsupported', supported: false, firefox: false, folders: false },
]) {
  const elements = new Map([...html.matchAll(/id="([^"]+)"/g)].map(([, id]) => [id, new Element()]));
  let downloadPort;
  globalThis.document = {
    getElementById(id) { assert.ok(elements.has(id), `Missing element ${id}`); return elements.get(id); },
    createElement(tag) {
      const el = new Element(tag); if (tag === 'input') el.webkitdirectory = false;
      if (tag === 'a') { el.click = () => { downloadPort.postMessage({ debug: 'Download started' }); downloadPort.postMessage({ ready: true }); }; el.remove = () => {}; }
      return el;
    },
    createTextNode(text) { return text; },
    body: { append() {} },
  };
  let saved = 0, aborted = 0, failDownload = false, downloadCalls = 0, downloadName;
  const chunks = [];
  const streamSaver = supported ? { createWriteStream(filename, options) {
    downloadCalls++;
    downloadName = filename;
    if (failDownload) throw new Error('Download blocked');
    queueMicrotask(options.onDownloadStart);
    return new WritableStream({
      write(data) { chunks.push(data.slice()); },
      close() { saved++; },
      abort() { aborted++; },
    });
  } } : undefined;
  const serviceWorker = firefox ? { async register() {
    if (failDownload) throw new Error('Download blocked');
    return { active: { state: 'activated', postMessage(data, ports) {
      if (data === 'ping') return;
      downloadCalls++; downloadName = decodeURIComponent(data.url.split('/').at(-1));
      downloadPort = ports[0];
      downloadPort.onmessage = ({ data }) => {
        if (data === 'end') { saved++; downloadPort.postMessage({ done: true }); }
        else if (data !== 'abort') { chunks.push(data.slice()); downloadPort.postMessage({ ready: true }); }
      };
      queueMicrotask(() => downloadPort.postMessage({ download: data.url }));
    } } };
  } } : { register() {} };
  globalThis.window = { isSecureContext: true, document: globalThis.document,
    navigator: { serviceWorker: supported ? serviceWorker : undefined,
      userAgent: firefox ? 'Mozilla/5.0 Firefox/143.0' : 'Mozilla/5.0 Chrome/142.0' }, streamSaver };
  globalThis.fetch = async () => ({ text: async () => '<svg></svg>' });
  globalThis.Worker = class {
    postMessage(data) {
      queueMicrotask(() => {
        const value = data.type === 'open' ? { entries: [
          { path: 'nested/hello.cfg', size: 5 }, { path: 'empty.dll', size: 0 },
        ] } : data.type === 'plan' ? [{ outputOffset: 0, size: data.path === 'empty.dll' ? 0 : 5 }]
          : data.type === 'source' ? true : new Uint8Array(data.chunk.size);
        if (data.type === 'chunk') this.onmessage({ data: { id: data.id, progress: value.length } });
        this.onmessage({ data: { id: data.id, value } });
      });
    }
    terminate() {}
  };
  await import(`./app.js?test=${name}`);
  assert.equal(elements.get('controls').hidden, !supported);
  assert.equal(elements.get('folder-option').hidden, !folders);
  assert.equal(elements.get('unsupported').hidden, supported);
  for (const id of ['more', 'extract', 'cancel']) assert.equal(typeof elements.get(id).onclick, 'function');
  assert.equal(elements.has('destination'), false);
  assert.equal(elements.has('timings'), false);
  assert.equal(elements.has('progress-detail'), false);
  assert.equal(elements.has('up'), false);
  if (supported) {
    elements.get('file').onchange({ target: { files: [{ name: 'test.dat' }], value: '' } });
    for (let i = 0; i < 100 && elements.get('archive').hidden; i++) await new Promise(r => setTimeout(r, 5));
    assert.equal(elements.get('archive').hidden, false);
    assert.equal(elements.get('status').textContent, '');
    assert.equal(elements.get('activity').hidden, true);
    assert.equal(elements.get('extract').disabled, false);
    const iconRows = elements.get('files').children;
    assert.ok(iconRows.some(row => row.children[0].children[3].innerHTML?.includes('<svg')));
    const folder = iconRows.find(row => row.children[0].children[4].textContent === 'nested');
    assert.equal(folder.children[0].children[1].className, 'archive-tree-toggle');
    assert.equal(folder.children[0].children[1].textContent, undefined);
    assert.equal(folder.children[0].children[1]['aria-expanded'], 'false');
    folder.children[0].children[1].onclick();
    assert.ok(elements.get('files').children.some(row => row.children[0].children[4].textContent === 'hello.cfg'));
    assert.equal(elements.get('files').children.find(row => row.children[0].children[4].textContent === 'nested').children[0].children[1]['aria-expanded'], 'true');
    const nestedFile = elements.get('files').children.find(row => row.children[0].children[4].textContent === 'hello.cfg');
    assert.equal(nestedFile.children[0].children[0].className, 'archive-tree-indent');
    elements.get('search').value = 'hello'; elements.get('search').oninput();
    assert.deepEqual(elements.get('files').children.map(row => row.children[0].children[4].textContent), ['nested', 'hello.cfg']);
    elements.get('search').value = 'missing'; elements.get('search').oninput();
    assert.equal(elements.get('files').children[0].textContent, 'No matching files.');
    elements.get('search').value = ''; elements.get('search').oninput();
    await elements.get('extract').onclick();
    assert.equal(downloadCalls, 1, 'One click should start one browser download');
    assert.match(downloadName, /^nudat-\d{8}T\d{6}Z\.zip$/);
    assert.equal(saved, 1, 'One ZIP is streamed: ' + elements.get('status').textContent);
    assert.equal(elements.get('progress').value, 1);
    assert.equal(elements.get('progress-percent').textContent, '100%');
    const zip = Buffer.concat(chunks.map(chunk => Buffer.from(chunk)));
    assert.ok(zip.includes(Buffer.from('nested/hello.cfg')));
    assert.ok(zip.includes(Buffer.from('empty.dll')));
    failDownload = true;
    await elements.get('extract').onclick();
    assert.match(elements.get('status').textContent, /Download blocked/);
    assert.equal(elements.get('progress').hidden, false);
    assert.notEqual(elements.get('progress-percent').textContent, '100%');
  }
}
console.log('One-click ZIP export, original names, progress, failure and Firefox file-only mode passed');
