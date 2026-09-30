import { exportEntries } from './export.js';
import { ZipWriter, zipLength } from './zip.js';
import { nativeDownload } from './native-download.js';
import { exportProgress } from './progress.js';
import { buildTree, treeRows, filterTree, folderPaths } from './browser-model.js';
import { browserCapabilities } from './capabilities.js';
import { planArchives, archiveEntries, mergeArchiveEntries } from './batch.js';

const $ = id => document.getElementById(id);
let worker, sequence = 0, pending = new Map(), entries = [], selected = new Set(), busy = false, cancelled = false;
let archives = [], multiple = false;
let tree = buildTree([]), searchTree = tree, expanded = new Set(), searchExpanded = new Set(), visibleLimit = 250, exportAbort;
const formatSize = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KiB` : `${(n / 1048576).toFixed(1)} MiB`;
const zipName = () => `nudat-${new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d{3}Z$/, 'Z')}.zip`;
const icons = {
  folder: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3.5 8V6.5a2 2 0 0 1 2-2h4.2l2.1 2.3h6.7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2V8Z"/><path d="M3.5 9h17"/></svg>',
  file: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6.5 3.5h7l4 4v12a1.5 1.5 0 0 1-1.5 1.5h-9a1.5 1.5 0 0 1-1.5-1.5V5A1.5 1.5 0 0 1 6.5 3.5Z"/><path d="M13.5 3.5v4h4"/></svg>',
};
const status = (text, error = false) => {
  $('status').textContent = text;
  $('status').classList.toggle('error', error);
  $('activity').hidden = !text && $('progress').hidden && $('errors').hidden;
};

function resetWorker() {
  worker?.terminate();
  for (const request of pending.values()) request.reject(new Error('Cancelled'));
  pending.clear();
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  worker.onmessage = ({ data }) => {
    const request = pending.get(data.id);
    if (!request) return;
    pending.delete(data.id);
    data.error ? request.reject(new Error(data.error)) : request.resolve(data.value);
  };
  worker.onerror = event => {
    for (const request of pending.values()) request.reject(new Error(event.message || 'Archive worker failed. Reopen the archive.'));
    pending.clear();
  };
}
function call(type, fields = {}) {
  return new Promise((resolve, reject) => {
    const id = ++sequence;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, type, ...fields });
  });
}
function setBusy(value) {
  busy = value;
  for (const id of ['file', 'folder', 'search', 'select-all', 'more']) $(id).disabled = value;
  $('files').querySelectorAll('input,button').forEach(el => el.disabled = value);
  $('cancel').hidden = !value;
  updateSelection();
}
function updateSelection() {
  const total = entries.filter(e => selected.has(e.path)).reduce((sum, e) => sum + e.size, 0);
  $('count').textContent = `${selected.size} selected · ${formatSize(total)}`;
  $('select-all').checked = entries.length > 0 && selected.size === entries.length;
  $('select-all').indeterminate = selected.size > 0 && selected.size < entries.length;
  $('extract').disabled = busy || selected.size === 0;
  for (const checkbox of $('files').querySelectorAll('input')) {
    const paths = checkbox.paths;
    const count = paths.filter(p => selected.has(p)).length;
    checkbox.checked = count === paths.length;
    checkbox.indeterminate = count > 0 && count < paths.length;
  }
}
function checkPaths(paths, value) { paths.forEach(path => value ? selected.add(path) : selected.delete(path)); updateSelection(); }
function render() {
  const query = $('search').value.trim();
  const activeExpanded = query ? searchExpanded : expanded;
  const rows = treeRows(query ? searchTree : tree, activeExpanded);
  const container = $('files'); container.replaceChildren();
  for (const row of rows.slice(0, visibleLimit)) {
    const line = document.createElement('div'); line.className = 'browser-row'; line.setAttribute('role', 'row');
    const cell = document.createElement('div'); cell.className = 'browser-name'; cell.setAttribute('role', 'cell');
    const input = document.createElement('input'); input.type = 'checkbox'; input.paths = row.paths;
    input.setAttribute('aria-label', `Select ${row.name}`); input.disabled = busy;
    input.onchange = () => checkPaths(row.paths, input.checked);
    const indent = document.createElement('span'); indent.className = 'archive-tree-indent'; indent.style.setProperty('--depth', row.depth);
    indent.setAttribute('aria-hidden', 'true');
    const toggle = document.createElement(row.folder ? 'button' : 'span');
    toggle.className = row.folder ? 'archive-tree-toggle' : 'archive-tree-spacer';
    if (row.folder) {
      toggle.type = 'button'; toggle.disabled = busy;
      toggle.setAttribute('aria-label', `${activeExpanded.has(row.path) ? 'Collapse' : 'Expand'} ${row.name}`);
      toggle.setAttribute('aria-expanded', String(activeExpanded.has(row.path)));
      toggle.onclick = () => {
        if (activeExpanded.has(row.path)) activeExpanded.delete(row.path); else activeExpanded.add(row.path);
        render();
      };
    }
    const icon = document.createElement('span'); icon.className = `browser-icon ${row.folder ? 'folder' : 'file'}`; icon.setAttribute('aria-hidden', 'true'); icon.innerHTML = row.folder ? icons.folder : icons.file;
    const name = document.createElement(row.folder ? 'button' : 'span'); name.className = 'entry-name'; name.textContent = row.name; name.title = row.name;
    if (row.folder) { name.type = 'button'; name.disabled = busy; name.onclick = toggle.onclick; }
    cell.append(indent, toggle, input, icon, name);
    const size = document.createElement('span'); size.className = 'file-size'; size.setAttribute('role', 'cell'); size.textContent = formatSize(row.size);
    line.append(cell, size); container.append(line);
  }
  $('more').hidden = rows.length <= visibleLimit; $('more').disabled = busy;
  $('listing-count').textContent = `${Math.min(rows.length, visibleLimit)} of ${rows.length} items`;
  if (!rows.length) { const p = document.createElement('p'); p.className = 'empty'; p.textContent = query ? 'No matching files.' : 'No files.'; container.append(p); }
  updateSelection();
}
async function open(files) {
  if (busy) return;
  const planned = planArchives(files);
  if (!planned.length) { status('No DAT or OBB files found.', true); return; }
  expanded = new Set(); searchExpanded = new Set(); tree = buildTree([]); searchTree = tree; visibleLimit = 250; archives = planned; multiple = planned.length > 1;
  entries = []; selected.clear(); $('archive').hidden = true; $('errors').hidden = true;
  $('progress').hidden = true; $('progress-percent').hidden = true;
  resetWorker(); setBusy(true); cancelled = false;
  const failures = []; let opened = 0;
  try {
    for (const archive of archives) {
      if (cancelled) break;
      status(`Opening archives… ${archive.id + 1}/${archives.length}`);
      try {
        const metadata = await call('open', { file: archive.file });
        entries.push(...archiveEntries(archive, metadata.entries));
        opened++;
      } catch (error) {
        if (cancelled) break;
        failures.push(`${archive.name}: ${error.message}`);
        resetWorker();
      }
    }
    if (cancelled) { entries = []; status('Cancelled.'); return; }
    const merged = mergeArchiveEntries(entries); entries = merged.entries; tree = buildTree(entries); searchTree = tree;
    selected = new Set(entries.map(e => e.path));
    $('filename').textContent = multiple ? `${opened} archives` : archives[0].file.name;
    $('summary').textContent = `${entries.length} files · ${formatSize(entries.reduce((sum, e) => sum + e.size, 0))}`;
    $('archive').hidden = opened === 0; $('search').value = ''; render();
    $('errors').textContent = failures.join('\n'); $('errors').hidden = !failures.length;
    status(opened ? '' : 'Could not open these archives.', !opened);
  } catch (error) { status(error.message, true); }
  finally { setBusy(false); }
}

async function extract() {
  const chosen = entries.filter(e => selected.has(e.path));
  if (!chosen.length || busy) return;
  const filename = zipName();
  let output;
  exportAbort = new AbortController();
  const signal = exportAbort.signal;
  cancelled = false; setBusy(true);
  const bar = $('progress');
  bar.hidden = false; bar.max = 1; bar.value = 0;
  $('progress-percent').hidden = false; $('progress-percent').textContent = '0%';
  bar.setAttribute('aria-valuetext', '0%');
  status('Creating ZIP…');
  let lastUpdate = 0;
  try {
    worker?.terminate();
    output = new ZipWriter(await nativeDownload(filename, zipLength(chosen)));
    await exportEntries({
      entries: chosen, archives, signal, cores: navigator.hardwareConcurrency, maxActiveFiles: 1,
      open: async entry => {
        if (signal.aborted) throw new Error('Cancelled');
        return output.open(entry);
      },
      onProgress: progress => {
        const now = performance.now();
        if (now - lastUpdate < 75 && progress.completed !== chosen.length) return;
        lastUpdate = now;
        const ratio = exportProgress(progress, chosen.length);
        bar.value = ratio;
        const percent = Math.floor(ratio * 100);
        $('progress-percent').textContent = percent + '%';
        bar.setAttribute('aria-valuetext', percent + '%');
      },
    });
    status('Finalizing ZIP…');
    await output.close();
    bar.value = 1;
    $('progress-percent').textContent = '100%';
    bar.setAttribute('aria-valuetext', '100%');
    status('ZIP ready: ' + filename);
  } catch (error) {
    if (output) { try { await output.abort(); } catch { /* Preserve the extraction error. */ } }
    status(cancelled ? 'Cancelled.' : error.message, !cancelled);
  } finally {
    exportAbort = null; setBusy(false);
  }
}
for (const id of ['file', 'folder']) $(id).onchange = event => { const files = Array.from(event.target.files); if (files.length) open(files); event.target.value = ''; };
$('search').oninput = () => {
  visibleLimit = 250;
  searchTree = filterTree(tree, $('search').value);
  searchExpanded = folderPaths(searchTree);
  render();
};
$('more').onclick = () => { visibleLimit += 250; render(); };
$('select-all').onchange = () => checkPaths(entries.map(e => e.path), $('select-all').checked);
$('extract').onclick = extract;
$('cancel').onclick = () => { cancelled = true; if (exportAbort) exportAbort.abort(); else { resetWorker(); selected.clear(); entries = []; $('archive').hidden = true; } status('Cancelling…'); };
const drop = $('drop-area');
for (const type of ['dragenter', 'dragover']) drop.addEventListener(type, e => { e.preventDefault(); if (!busy) drop.classList.add('dragging'); });
drop.addEventListener('dragleave', () => drop.classList.remove('dragging'));
drop.addEventListener('drop', e => { e.preventDefault(); drop.classList.remove('dragging'); if (e.dataTransfer.files.length) open(Array.from(e.dataTransfer.files)); });
const capabilities = browserCapabilities(window, document);
$('controls').hidden = !capabilities.files;
$('folder-option').hidden = !capabilities.folders;
$('unsupported').hidden = capabilities.files;
