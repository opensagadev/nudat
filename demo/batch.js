// Preserve input order and provenance; archive contents are merged separately.
export function planArchives(files) {
  return Array.from(files).filter(file => /\.(dat|obb)$/i.test(file.name)).map((file, id) => {
    const name = file.webkitRelativePath || file.name;
    return { id, file, name };
  });
}

export function archiveEntries(archive, entries) {
  return entries.map(entry => {
    if (entry.path.split('/').some(p => !p || p === '.' || p === '..' || /[\\:\0]/.test(p))) {
      throw new Error('Archive contains an unsafe filename.');
    }
    return { ...entry, originalPath: entry.path, archiveId: archive.id,
      path: entry.path };
  });
}

// DAT lookups are case-insensitive. Match that behavior across archives too.
export function mergeArchiveEntries(entries) {
  const files = new Map(), spelling = new Map();
  let duplicates = 0;
  for (const entry of entries) {
    let key = '';
    const parts = entry.path.split('/').map(part => {
      key += '/' + part.toUpperCase();
      if (!spelling.has(key)) spelling.set(key, part);
      return spelling.get(key);
    });
    if (files.has(key)) duplicates++;
    files.set(key, { ...entry, path: parts.join('/') });
  }
  for (const key of files.keys()) {
    const parts = key.split('/'); parts.pop();
    while (parts.length > 1) {
      if (files.has(parts.join('/'))) throw new Error('Cannot merge a file and a folder with the same path: ' + parts.join('/').slice(1));
      parts.pop();
    }
  }
  return { entries: [...files.values()], duplicates };
}
