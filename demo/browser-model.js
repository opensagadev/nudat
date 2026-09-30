export function browse(entries, directory = '', query = '') {
  if (query) return entries.filter(e => e.path.toLowerCase().includes(query.toLowerCase()))
    .map(e => ({ name: e.path, path: e.path, size: e.size, paths: [e.path], folder: false }));
  const folders = new Map(), files = [];
  for (const entry of entries) {
    if (!entry.path.startsWith(directory)) continue;
    const relative = entry.path.slice(directory.length), slash = relative.indexOf('/');
    if (slash < 0) { files.push({ name: relative, path: entry.path, size: entry.size, paths: [entry.path], folder: false }); continue; }
    const name = relative.slice(0, slash), path = directory + name + '/';
    if (!folders.has(path)) folders.set(path, { name, path, size: 0, paths: [], folder: true });
    const folder = folders.get(path); folder.paths.push(entry.path); folder.size += entry.size;
  }
  return [...folders.values(), ...files].sort((a, b) => Number(b.folder) - Number(a.folder) || a.name.localeCompare(b.name));
}

export function buildTree(entries) {
  const root = { children: new Map() };
  for (const entry of entries) {
    const parts = entry.path.split('/');
    let parent = root, path = '';
    for (let i = 0; i < parts.length; i++) {
      const name = parts[i], folder = i < parts.length - 1;
      path += name + (folder ? '/' : '');
      let node = parent.children.get(name);
      if (!node) {
        node = { name, path, folder, size: 0, paths: [], children: folder ? new Map() : null };
        parent.children.set(name, node);
      }
      node.size += entry.size;
      node.paths.push(entry.path);
      parent = node;
    }
  }
  const sort = node => {
    node.children = [...node.children.values()].sort((a, b) =>
      Number(b.folder) - Number(a.folder) || a.name.localeCompare(b.name));
    for (const child of node.children) if (child.folder) sort(child);
  };
  sort(root);
  return root;
}

export function treeRows(tree, expanded) {
  const rows = [];
  const visit = (nodes, depth) => {
    for (const node of nodes) {
      rows.push({ ...node, depth });
      if (node.folder && expanded.has(node.path)) visit(node.children, depth + 1);
    }
  };
  visit(tree.children, 0);
  return rows;
}

export function filterTree(tree, query) {
  const needle = query.trim().toLowerCase();
  if (!needle) return tree;
  const filter = node => {
    if (!node.folder) return node.path.toLowerCase().includes(needle) ? node : null;
    if (node.name.toLowerCase().includes(needle)) return node;
    const children = node.children.map(filter).filter(Boolean);
    return children.length ? { ...node, children } : null;
  };
  return { children: tree.children.map(filter).filter(Boolean) };
}

export function folderPaths(tree) {
  const paths = new Set();
  const visit = nodes => {
    for (const node of nodes) if (node.folder) { paths.add(node.path); visit(node.children); }
  };
  visit(tree.children);
  return paths;
}
