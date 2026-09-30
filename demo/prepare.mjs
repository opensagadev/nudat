// Render the shared OpenSaga shell and copy its stylesheet for a standalone build.
import { mkdir, copyFile, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('.', import.meta.url));
const saga = process.argv[2];
if (!saga) throw new Error('Usage: node demo/prepare.mjs PATH_TO_SAGA_CHECKOUT');
const site = resolve(saga, 'scripts/site');
const source = path => readFile(resolve(site, path), 'utf8');
let header = await source('header.html');
for (const name of ['logo', 'discord', 'github']) {
  let icon = await source(`assets/${name}.svg`);
  if (name !== 'logo') icon = icon.replace('<svg ', '<svg aria-hidden="true" ');
  header = header.replace(`<!--__${name.toUpperCase()}__-->`, icon);
}
header = header.replaceAll('__ROOT__', 'https://opensaga.dev/');
for (const page of ['home', 'play', 'progress']) header = header.replaceAll(`__${page.toUpperCase()}_CURRENT__`, '');
const head = (await source('head.html')).replaceAll('__ROOT__', './branding/');
const template = await readFile(resolve(root, 'index.template.html'), 'utf8');
const html = template.replace('<!--__HEAD__-->', head)
  .replace('<!--__HEADER__-->', header)
  .replace('<!--__FOOTER__-->', await source('footer.html'));
if (/<!--__[A-Z_]+__-->/.test(html)) throw new Error('Unrendered site template marker');
await mkdir(resolve(root, 'branding'), { recursive: true });
await copyFile(resolve(site, 'site.css'), resolve(root, 'branding/site.css'));
for (const file of ['favicon.svg', 'favicon.png']) {
  await copyFile(resolve(site, 'assets', file), resolve(root, 'branding', file));
}
await writeFile(resolve(root, 'index.html'), html);
console.log('Web assets ready.');
