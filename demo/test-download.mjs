import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { nativeDownload } from './native-download.js';
import { firefoxDownload } from './firefox-download.js';

const scope = { addEventListener() {}, registration: { scope: 'https://example.test/download/' } };
runInNewContext(readFileSync(new URL('./third-party/streamsaver/sw.js', import.meta.url), 'utf8'),
  { self: scope, ReadableStream, Response, Headers, console });
const messages = [];
const port = { postMessage(message) { messages.push(message); } };
const url = 'https://example.test/download/archive.zip';
scope.onmessage({ data: { url, transferringReadable: true, headers: {} }, ports: [port] });
assert.equal(messages.length, 0, 'Do not start the download before its stream arrives');
const readableStream = new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array([1])); controller.close(); } });
port.onmessage({ data: { readableStream } });
assert.equal(messages[0].download, url);
let response;
scope.onfetch({ request: { url }, respondWith(value) { response = value; } });
assert.equal(response.status, 200);
assert.deepEqual(new Uint8Array(await response.arrayBuffer()), new Uint8Array([1]));

const channelMessages = [];
const channelPort = { postMessage(message) { channelMessages.push(message); } };
const channelUrl = 'https://example.test/download/firefox.zip';
scope.onmessage({ data: { url: channelUrl, backpressure: true, headers: {} }, ports: [channelPort] });
assert.equal(channelMessages[0].download, channelUrl);
let channelResponse;
scope.onfetch({ request: { url: channelUrl }, respondWith(value) { channelResponse = value; } });
const channelReader = channelResponse.body.getReader();
await new Promise(resolve => setTimeout(resolve, 0));
assert.ok(channelMessages.some(message => message.ready), 'Worker must request the next chunk');
const firstRead = channelReader.read();
channelPort.onmessage({ data: new Uint8Array([7]) });
assert.deepEqual((await firstRead).value, new Uint8Array([7]));
channelPort.onmessage({ data: 'end' });
assert.equal((await channelReader.read()).done, true);

let downloaded;
const firefoxWindow = {
  navigator: { serviceWorker: { async register() {
    return { active: { state: 'activated', postMessage(data, ports) { scope.onmessage({ data, ports }); } } };
  } } },
  document: {
    body: { append() {} },
    createElement() { return { click() {
      let result;
      scope.onfetch({ request: { url: this.href }, respondWith(value) { result = value; } });
      downloaded = result.arrayBuffer();
    }, remove() {} }; },
  },
};
const firefoxStream = await firefoxDownload('test.zip', 3n,
  { window: firefoxWindow, startupTimeoutMs: 1000, writeTimeoutMs: 1000 });
await firefoxStream.write(new Uint8Array([1, 2, 3]));
await firefoxStream.close();
assert.deepEqual(new Uint8Array(await downloaded), new Uint8Array([1, 2, 3]));

let aborted = 0;
const stalled = { streamSaver: { createWriteStream() { return new WritableStream({ abort() { aborted++; } }); } } };
await assert.rejects(nativeDownload('test.zip', 22n, { window: stalled, startupTimeoutMs: 5 }), /did not start/);
await new Promise(resolve => setTimeout(resolve, 0));
assert.equal(aborted, 1);
const stalledWrite = { streamSaver: { createWriteStream(_name, options) {
  queueMicrotask(options.onDownloadStart);
  return new WritableStream({ write() { return new Promise(() => {}); } });
} } };
const download = await nativeDownload('test.zip', 22n, { window: stalledWrite, writeTimeoutMs: 5 });
await assert.rejects(download.write(new Uint8Array([1])), /stopped accepting ZIP data/);
console.log('Download stream announcement, Firefox direct download, and stalled-start timeout passed');
