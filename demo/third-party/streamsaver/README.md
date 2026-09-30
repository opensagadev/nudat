StreamSaver.js 2.0.6, MIT license: https://github.com/jimmywarting/StreamSaver.js

Vendored files: `StreamSaver.js`, `mitm.html`, `sw.js`, and `LICENSE`.
`mitm.html` always registers its own scoped worker, because the site may already
have an older service worker at the root, and keeps that worker alive during
transferable-stream downloads for Firefox. `sw.js` waits for the transferred
readable stream before announcing its URL. `StreamSaver.js` forwards the worker's
download-start signal. Firefox uses `firefox-download.js` to register the same
scoped worker directly and start the download with a normal link. Its message
channel allows only one outstanding ZIP chunk.
